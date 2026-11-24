//! PipeWire video streams for the screencasts (`screencast.rs`), on a thread of their own.
//!
//! The thread owns PipeWire's main loop, context and core, and one output stream per cast. Casts
//! hand it [`Cmd`]s (start, a frame, stop); it answers with [`Event`]s: the node id once a stream
//! is ready for consumers, and the end of a stream. Frames are shm (`MemFd`/`MemPtr` buffers
//! PipeWire allocates and maps), BGRx or BGRA, and carry their CLOCK_MONOTONIC time in the
//! header's pts, as KWin's do: KPipeWire places frames by it and drops one that is not later than
//! the one before. A stream whose source shows nothing new sends its last frame again every
//! [`KEEPALIVE`], newly stamped, so a recording of a still screen lasts as long as it ran.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use pipewire::context::ContextRc;
use pipewire::core::PW_ID_CORE;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, Property};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{
    Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
};
use pipewire::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use tracing::{debug, info, warn};

/// How a frame's 4 bytes a pixel lie in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

/// Pixels for a stream: `width * 4` bytes a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
    pub pixels: Vec<u8>,
    /// CLOCK_MONOTONIC nanoseconds.
    pub pts: i64,
}

pub enum Cmd {
    /// A stream offering BGRx (and BGRA first when `alpha`) at this size.
    Start {
        id: u64,
        width: u32,
        height: u32,
        alpha: bool,
    },
    Frame {
        id: u64,
        frame: Frame,
    },
    Stop {
        id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The stream's node is ready for a consumer.
    Node { id: u64, node: u32 },
    /// The stream ended: PipeWire failed it, or the PipeWire connection broke (every stream).
    Ended { id: u64, error: String },
}

/// The most frames a second a stream offers.
const MAX_FPS: u32 = 60;

/// How long a stream goes without a frame before it sends its last one again.
pub const KEEPALIVE: Duration = Duration::from_millis(100);

/// The PipeWire thread, as the casts see it.
pub struct PipeWire {
    tx: pipewire::channel::Sender<Cmd>,
    dead: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PipeWire {
    /// Connects to PipeWire on a new thread; `notify` is called with each event, from that thread.
    pub fn spawn(notify: impl Fn(Event) + Send + 'static) -> Result<Self> {
        let (tx, rx) = pipewire::channel::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let dead = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dead_ = dead.clone();
        thread::Builder::new()
            .name("pipewire".to_owned())
            .spawn(move || {
                if let Err(e) = run(rx, notify, &ready_tx) {
                    let _ = ready_tx.send(Err(anyhow!("{e:#}")));
                    warn!("pipewire: {e:#}");
                }
                dead_.store(true, std::sync::atomic::Ordering::SeqCst);
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("PipeWire did not answer")??;
        Ok(Self { tx, dead })
    }

    /// False once the thread ended (PipeWire went away): spawn another.
    pub fn alive(&self) -> bool {
        !self.dead.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn sender(&self) -> pipewire::channel::Sender<Cmd> {
        self.tx.clone()
    }
}

/// What a stream negotiated, and where it is.
#[derive(Default)]
struct Inner {
    node: Option<u32>,
    streaming: bool,
    /// The size the stream offers now.
    size: (u32, u32),
    /// The format and size the consumer took.
    format: Option<(VideoFormat, u32, u32)>,
    seq: u64,
    last_pts: i64,
    /// The newest frame the stream could not take yet: it goes out once the stream streams, so a
    /// source that does not change still shows (the first frame comes before the consumer).
    pending: Option<Frame>,
    /// The last frame queued, and when.
    last: Option<Frame>,
    last_push: Option<Instant>,
}

/// The listener comes first: it is dropped (unhooked) while the stream still exists.
struct Stream {
    _listener: StreamListener<()>,
    stream: StreamRc,
    inner: Rc<RefCell<Inner>>,
    alpha: bool,
}

fn run(
    rx: pipewire::channel::Receiver<Cmd>,
    notify: impl Fn(Event) + 'static,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    pipewire::init();
    let main_loop = MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = ContextRc::new(&main_loop, None).context("PipeWire context")?;
    let core = context
        .connect_rc(None)
        .context("PipeWire is not running")?;
    let notify = Rc::new(notify);
    let streams: Rc<RefCell<HashMap<u64, Stream>>> = Rc::default();

    let _core_listener = core
        .add_listener_local()
        .error({
            let main_loop = main_loop.downgrade();
            let streams = streams.clone();
            let notify = notify.clone();
            move |id, _seq, res, message| {
                warn!("pipewire error on {id}: {res} {message}");
                // EPIPE on the core: the daemon went away, every stream with it.
                if id == PW_ID_CORE && res == -32 {
                    for id in streams.borrow().keys() {
                        notify(Event::Ended {
                            id: *id,
                            error: "PipeWire went away".to_owned(),
                        });
                    }
                    if let Some(l) = main_loop.upgrade() {
                        l.quit();
                    }
                }
            }
        })
        .register();

    let _attached = rx.attach(main_loop.loop_(), {
        let streams = streams.clone();
        let core = core.clone();
        let notify = notify.clone();
        move |cmd| match cmd {
            Cmd::Start {
                id,
                width,
                height,
                alpha,
            } => match start(&core, id, (width, height), alpha, notify.clone()) {
                Ok(s) => {
                    streams.borrow_mut().insert(id, s);
                }
                Err(e) => notify(Event::Ended {
                    id,
                    error: format!("{e:#}"),
                }),
            },
            Cmd::Frame { id, frame } => {
                if let Some(s) = streams.borrow().get(&id) {
                    s.frame(frame);
                }
            }
            Cmd::Stop { id } => {
                if let Some(s) = streams.borrow_mut().remove(&id) {
                    let _ = s.stream.disconnect();
                }
            }
        }
    });
    let keepalive = main_loop.loop_().add_timer({
        let streams = streams.clone();
        move |_| {
            for s in streams.borrow().values() {
                s.keepalive();
            }
        }
    });
    let _ = keepalive.update_timer(Some(KEEPALIVE), Some(KEEPALIVE));
    let _ = ready.send(Ok(()));
    info!("connected to PipeWire");
    main_loop.run();
    Ok(())
}

fn start(
    core: &pipewire::core::CoreRc,
    id: u64,
    size: (u32, u32),
    alpha: bool,
    notify: Rc<dyn Fn(Event)>,
) -> Result<Stream> {
    let mut props = PropertiesBox::new();
    props.insert("media.class", "Video/Source");
    props.insert("media.name", "newm-river screencast");
    let stream = StreamRc::new(core.clone(), "newm-river-screencast", props)?;
    let inner = Rc::new(RefCell::new(Inner {
        size,
        ..Inner::default()
    }));
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let inner = inner.clone();
            let notify = notify.clone();
            move |stream, (), old, new| {
                debug!("cast {id}: pipewire {old:?} -> {new:?}");
                let mut inner = inner.borrow_mut();
                match new {
                    StreamState::Paused => {
                        inner.streaming = false;
                        if inner.node.is_none() {
                            let node = stream.node_id();
                            inner.node = Some(node);
                            notify(Event::Node { id, node });
                        }
                    }
                    StreamState::Streaming => {
                        inner.streaming = true;
                        if let Some(f) = inner.pending.take() {
                            inner.pending = push(stream, &mut inner, f);
                        }
                    }
                    StreamState::Error(e) => {
                        inner.streaming = false;
                        notify(Event::Ended { id, error: e });
                    }
                    StreamState::Unconnected | StreamState::Connecting => inner.streaming = false,
                }
            }
        })
        .param_changed({
            let inner = inner.clone();
            move |stream, (), param, pod| {
                if param != ParamType::Format.as_raw() {
                    return;
                }
                let Some(pod) = pod else { return };
                match parse_format(pod) {
                    Ok((t, s)) if t == MediaType::Video && s == MediaSubtype::Raw => {}
                    _ => return,
                }
                let mut info = VideoInfoRaw::new();
                if info.parse(pod).is_err() {
                    return;
                }
                let (w, h) = (info.size().width, info.size().height);
                debug!("cast {id}: consumer took {:?} {w}x{h}", info.format());
                inner.borrow_mut().format = Some((info.format(), w, h));
                let mut b1 = Vec::new();
                let mut b2 = Vec::new();
                let mut params = [
                    make_pod(&mut b1, buffers_param(w, h)),
                    make_pod(&mut b2, header_meta_param()),
                ];
                if let Err(e) = stream.update_params(&mut params) {
                    warn!("cast {id}: buffer params: {e:?}");
                }
            }
        })
        .register()?;

    let mut bufs: Vec<Vec<u8>> = Vec::new();
    let objects = format_params(size, alpha);
    let mut params: Vec<&Pod> = Vec::new();
    for _ in &objects {
        bufs.push(Vec::new());
    }
    for (b, o) in bufs.iter_mut().zip(objects) {
        params.push(make_pod(b, o));
    }
    stream.connect(
        Direction::Output,
        None,
        StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    Ok(Stream {
        stream,
        _listener: listener,
        inner,
        alpha,
    })
}

impl Stream {
    fn frame(&self, frame: Frame) {
        let mut inner = self.inner.borrow_mut();
        if (frame.width, frame.height) != inner.size {
            // The source resized: offer the new size; the frame waits for the consumer to take it.
            inner.size = (frame.width, frame.height);
            let objects = format_params(inner.size, self.alpha);
            let mut bufs: Vec<Vec<u8>> = objects.iter().map(|_| Vec::new()).collect();
            let mut params: Vec<&Pod> = bufs
                .iter_mut()
                .zip(objects)
                .map(|(b, o)| make_pod(b, o))
                .collect();
            if let Err(e) = self.stream.update_params(&mut params) {
                warn!("renegotiating a cast's size: {e:?}");
            }
            inner.pending = Some(frame);
            return;
        }
        inner.pending = push(&self.stream, &mut inner, frame);
    }

    /// Sends the last frame again, stamped now, when the source showed nothing new for a while.
    fn keepalive(&self) {
        let mut inner = self.inner.borrow_mut();
        if !inner.streaming || inner.last_push.is_none_or(|t| t.elapsed() < KEEPALIVE) {
            return;
        }
        if let Some(mut f) = inner.last.take() {
            f.pts = monotonic_ns();
            if let Some(f) = push(&self.stream, &mut inner, f) {
                inner.last = Some(f);
            }
        }
    }
}

/// Writes a frame into a free buffer of the stream and queues it, keeping it as the last frame;
/// gives it back when the stream cannot take it now (not streaming, another size negotiated,
/// every buffer with the consumer).
fn push(stream: &pipewire::stream::Stream, inner: &mut Inner, frame: Frame) -> Option<Frame> {
    let Some((format, w, h)) = inner.format else {
        return Some(frame);
    };
    if !inner.streaming || (w, h) != (frame.width, frame.height) {
        return Some(frame);
    }
    let Some(dst) = Dst::of(format) else {
        return Some(frame);
    };
    // SAFETY: the buffer comes from this stream, is used only here and queued back.
    unsafe {
        let pw_buffer = stream.dequeue_raw_buffer();
        if pw_buffer.is_null() {
            return Some(frame);
        }
        let spa_buffer = (*pw_buffer).buffer;
        if (*spa_buffer).n_datas < 1 {
            stream.queue_raw_buffer(pw_buffer);
            return Some(frame);
        }
        let data = &mut *(*spa_buffer).datas;
        let stride = w * 4;
        let len = stride as usize * h as usize;
        let chunk = data.chunk;
        if data.data.is_null() || (data.maxsize as usize) < len {
            (*chunk).size = 0;
            (*chunk).flags = SPA_CHUNK_FLAG_CORRUPTED as i32;
            stream.queue_raw_buffer(pw_buffer);
            return Some(frame);
        }
        let out = std::slice::from_raw_parts_mut(data.data.cast::<u8>(), len);
        convert(frame.layout, &frame.pixels, dst, out);
        (*chunk).offset = 0;
        (*chunk).size = len as u32;
        (*chunk).stride = stride as i32;
        (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;
        inner.seq = inner.seq.wrapping_add(1);
        // A pending frame keeps its own time: KPipeWire clamps one before the recording to 0.
        inner.last_pts = next_pts(inner.last_pts, frame.pts);
        let header: *mut spa_meta_header =
            spa_buffer_find_meta_data(spa_buffer, SPA_META_Header, size_of::<spa_meta_header>())
                .cast();
        if let Some(header) = header.as_mut() {
            header.flags = 0;
            header.offset = 0;
            header.seq = inner.seq;
            header.pts = inner.last_pts;
            header.dts_offset = 0;
        }
        stream.queue_raw_buffer(pw_buffer);
    }
    inner.last = Some(frame);
    inner.last_push = Some(Instant::now());
    None
}

/// The negotiated layout a frame is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dst {
    Bgrx,
    Bgra,
}

impl Dst {
    fn of(format: VideoFormat) -> Option<Self> {
        if format == VideoFormat::BGRx {
            Some(Self::Bgrx)
        } else if format == VideoFormat::BGRA {
            Some(Self::Bgra)
        } else {
            None
        }
    }
}

/// Copies `src` (one of the shm layouts) into `dst` as BGRx/BGRA. Lengths are equal or the
/// shorter one bounds the copy. An X source becomes opaque in BGRA.
pub fn convert(src_layout: Layout, src: &[u8], dst_layout: Dst, dst: &mut [u8]) {
    let n = src.len().min(dst.len());
    let (src, dst) = (&src[..n], &mut dst[..n]);
    match src_layout {
        Layout::Bgra | Layout::Bgrx => {
            dst.copy_from_slice(src);
            if src_layout == Layout::Bgrx && dst_layout == Dst::Bgra {
                for p in dst.as_chunks_mut::<4>().0 {
                    p[3] = 255;
                }
            }
        }
        Layout::Rgba | Layout::Rgbx => {
            let opaque = src_layout == Layout::Rgbx && dst_layout == Dst::Bgra;
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<4>().0)
            {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = if opaque { 255 } else { s[3] };
            }
        }
    }
}

/// A frame's pts: its own time, but always later than the one before (KPipeWire drops a frame
/// that is not).
pub fn next_pts(last: i64, now: i64) -> i64 {
    now.max(last.saturating_add(1))
}

fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object))
        .expect("serializing a pod");
    Pod::from_bytes(buffer).expect("a pod just serialized")
}

/// One EnumFormat per offered format: BGRA first when the source has alpha, then BGRx.
fn format_params((w, h): (u32, u32), alpha: bool) -> Vec<pod::Object> {
    let mut formats = Vec::new();
    if alpha {
        formats.push(VideoFormat::BGRA);
    }
    formats.push(VideoFormat::BGRx);
    formats
        .into_iter()
        .map(|format| pod::Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: vec![
                pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
                pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
                pod::property!(FormatProperties::VideoFormat, Id, format),
                pod::property!(
                    FormatProperties::VideoSize,
                    Rectangle,
                    Rectangle {
                        width: w,
                        height: h
                    }
                ),
                pod::property!(
                    FormatProperties::VideoFramerate,
                    Fraction,
                    Fraction { num: 0, denom: 1 }
                ),
                pod::property!(
                    FormatProperties::VideoMaxFramerate,
                    Choice,
                    Range,
                    Fraction,
                    Fraction {
                        num: MAX_FPS,
                        denom: 1
                    },
                    Fraction { num: 1, denom: 1 },
                    Fraction {
                        num: MAX_FPS,
                        denom: 1
                    }
                ),
            ],
        })
        .collect()
}

fn buffers_param(w: u32, h: u32) -> pod::Object {
    let stride = (w * 4) as i32;
    pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                SPA_PARAM_BUFFERS_buffers,
                pod::Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 16,
                    },
                ))),
            ),
            Property::new(SPA_PARAM_BUFFERS_blocks, pod::Value::Int(1)),
            Property::new(SPA_PARAM_BUFFERS_size, pod::Value::Int(stride * h as i32)),
            Property::new(SPA_PARAM_BUFFERS_stride, pod::Value::Int(stride)),
            Property::new(
                SPA_PARAM_BUFFERS_dataType,
                pod::Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: 1 << spa::buffer::DataType::MemFd.as_raw(),
                        flags: vec![
                            1 << spa::buffer::DataType::MemFd.as_raw(),
                            1 << spa::buffer::DataType::MemPtr.as_raw(),
                        ],
                    },
                ))),
            ),
        ],
    }
}

fn header_meta_param() -> pod::Object {
    pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                SPA_PARAM_META_type,
                pod::Value::Id(spa::utils::Id(SPA_META_Header)),
            ),
            Property::new(
                SPA_PARAM_META_size,
                pod::Value::Int(size_of::<spa_meta_header>() as i32),
            ),
        ],
    }
}

/// CLOCK_MONOTONIC now, in nanoseconds.
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec to write.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgr_sources_copy_and_x_becomes_opaque_in_bgra() {
        let src = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [0; 8];
        convert(Layout::Bgra, &src, Dst::Bgra, &mut dst);
        assert_eq!(dst, src);
        convert(Layout::Bgrx, &src, Dst::Bgrx, &mut dst);
        assert_eq!(dst, src);
        convert(Layout::Bgrx, &src, Dst::Bgra, &mut dst);
        assert_eq!(dst, [1, 2, 3, 255, 5, 6, 7, 255]);
    }

    #[test]
    fn rgb_sources_swap_red_and_blue() {
        let src = [1, 2, 3, 4];
        let mut dst = [0; 4];
        convert(Layout::Rgba, &src, Dst::Bgra, &mut dst);
        assert_eq!(dst, [3, 2, 1, 4]);
        convert(Layout::Rgbx, &src, Dst::Bgra, &mut dst);
        assert_eq!(dst, [3, 2, 1, 255]);
        convert(Layout::Rgbx, &src, Dst::Bgrx, &mut dst);
        assert_eq!(dst, [3, 2, 1, 4]);
    }

    #[test]
    fn a_short_destination_bounds_the_copy() {
        let mut dst = [0; 4];
        convert(Layout::Bgra, &[9; 8], Dst::Bgra, &mut dst);
        assert_eq!(dst, [9; 4]);
    }

    #[test]
    fn pts_always_grows() {
        assert_eq!(next_pts(0, 100), 100);
        assert_eq!(next_pts(100, 100), 101);
        assert_eq!(next_pts(500, 100), 501);
        assert_eq!(next_pts(i64::MAX, 0), i64::MAX);
    }

    #[test]
    fn alpha_sources_offer_bgra_first() {
        let f = format_params((8, 4), true);
        assert_eq!(f.len(), 2);
        assert_eq!(format_params((8, 4), false).len(), 1);
        assert_eq!(buffers_param(8, 4).properties.len(), 5);
    }
}
