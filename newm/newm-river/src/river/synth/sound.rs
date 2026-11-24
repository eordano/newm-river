//! Listening to the sound playing: one passive PipeWire capture of
//! the default sink's monitor, mono F32, on a thread of its own as `pw.rs` runs PipeWire, made
//! only while the background listens.
//!
//! The stream is passive (`node.passive = true`): its link never makes the sink run, so in
//! silence the sink goes idle, WirePlumber suspends it, and this thread gets no callbacks and
//! sleeps in poll. Each block's level goes into an atomic; the window manager takes it when it
//! draws, and asks to be woken by the next block only while it has nothing else scheduled.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use newm_synth::level_of_rms;
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa;
use pipewire::spa::pod::Pod;
use pipewire::stream::{StreamFlags, StreamRc};

/// The stream's properties: a passive capture of the default sink's monitor.
pub fn properties() -> [(&'static str, &'static str); 8] {
    [
        ("media.type", "Audio"),
        ("media.category", "Capture"),
        ("media.role", "Music"),
        ("stream.capture.sink", "true"),
        ("node.passive", "true"),
        ("node.latency", "2048/48000"),
        ("node.name", "newm-synth"),
        ("application.name", "newm synth background"),
    ]
}

/// What the thread and the window manager share.
#[derive(Default)]
struct Shared {
    /// The loudest block's level since the window manager last took it, as f32 bits.
    peak: AtomicU32,
    /// The window manager waits for sound with nothing scheduled: wake it at the next block.
    wake_wanted: AtomicBool,
}

/// The capture thread. Dropping it stops the thread and leaves PipeWire.
pub struct Sound {
    quit: pipewire::channel::Sender<()>,
    shared: Arc<Shared>,
    wake: Arc<File>,
    thread: Option<JoinHandle<()>>,
}

impl Sound {
    pub fn spawn() -> Result<Self> {
        // SAFETY: eventfd returns a new fd or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            bail!("eventfd: {}", std::io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        let wake = Arc::new(unsafe { File::from_raw_fd(fd) });
        let shared = Arc::new(Shared::default());
        let (quit, quit_rx) = pipewire::channel::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (shared_, wake_) = (shared.clone(), wake.clone());
        let thread = thread::Builder::new()
            .name("synth-sound".into())
            .spawn(move || {
                if let Err(e) = run(quit_rx, shared_, wake_, &ready_tx) {
                    let _ = ready_tx.send(Err(anyhow!("{e:#}")));
                }
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("PipeWire did not answer")??;
        Ok(Sound {
            quit,
            shared,
            wake,
            thread: Some(thread),
        })
    }

    /// The loudest block's level since the last call, 0 when none came.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.shared.peak.swap(0, Ordering::AcqRel))
    }

    /// Whether a block came in since the last [`Sound::take_peak`].
    pub fn pending(&self) -> bool {
        self.shared.peak.load(Ordering::Acquire) != 0
    }

    /// Asks the thread to wake the window manager at the next block.
    pub fn wake_on_sound(&self) {
        self.shared.wake_wanted.store(true, Ordering::Release);
    }

    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    pub fn drain(&self) {
        let mut b = [0u8; 8];
        let _ = (&*self.wake).read(&mut b);
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    quit: pipewire::channel::Receiver<()>,
    shared: Arc<Shared>,
    wake: Arc<File>,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    pipewire::init();
    let main_loop = MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = ContextRc::new(&main_loop, None).context("PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("PipeWire is not running")?;
    let mut props = PropertiesBox::new();
    for (k, v) in properties() {
        props.insert(k, v);
    }
    let stream = StreamRc::new(core, "newm-synth", props).context("PipeWire stream")?;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, ()| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data().and_then(|b| b.get(offset..offset + size)) else {
                return;
            };
            let (mut sum, mut n) = (0f32, 0usize);
            for s in bytes.as_chunks::<4>().0 {
                let v = f32::from_le_bytes(*s);
                sum += v * v;
                n += 1;
            }
            if n == 0 {
                return;
            }
            let level = level_of_rms((sum / n as f32).sqrt());
            if level <= 0. {
                return;
            }
            // Keep the loudest block until the window manager takes it.
            let mut old = shared.peak.load(Ordering::Acquire);
            while level > f32::from_bits(old) {
                match shared.peak.compare_exchange_weak(
                    old,
                    level.to_bits(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(now) => old = now,
                }
            }
            if shared.wake_wanted.swap(false, Ordering::AcqRel) {
                let _ = (&*wake).write(&1u64.to_ne_bytes());
            }
        })
        .register()
        .context("PipeWire stream listener")?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_channels(1);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .context("audio format")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).context("audio format")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("PipeWire stream connect")?;

    let _quit = quit.attach(main_loop.loop_(), {
        let main_loop = main_loop.downgrade();
        move |()| {
            if let Some(l) = main_loop.upgrade() {
                l.quit();
            }
        }
    });
    let _ = ready.send(Ok(()));
    main_loop.run();
    Ok(())
}
