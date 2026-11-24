//! Pixels of an output or a window through river's ext-image-copy-capture-v1, on a Wayland
//! connection of its own, so any thread can capture without touching the window manager's queue.
//!
//! A [`Capturer`] connects, learns the outputs (by their `wl_output` name) and the toplevels (by
//! their ext-foreign-toplevel-list identifier, the string `river_window_v1.identifier` reports
//! for the same window), and copies one frame of a source into a shm buffer. Pixels come back as
//! RGBA8888 premultiplied rows without padding (QImage's `Format_RGBA8888_Premultiplied`, what
//! KWin's ScreenShot2 sends).

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

/// Captured pixels: RGBA8888 premultiplied, `width * 4` bytes a row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source<'a> {
    /// An output by its `wl_output` name (the connector, e.g. `DP-1`).
    Output(&'a str),
    /// A window by its ext-foreign-toplevel-list identifier.
    Toplevel(&'a str),
}

/// The shm formats a capture is read in, the most wanted first. A GLES-rendered output (river
/// with scenefx) may offer only the renderer's read format: 24-bit BGR888 on NVIDIA, or a 10-bit
/// one.
const FORMATS: [wl_shm::Format; 10] = [
    wl_shm::Format::Argb8888,
    wl_shm::Format::Xrgb8888,
    wl_shm::Format::Abgr8888,
    wl_shm::Format::Xbgr8888,
    wl_shm::Format::Argb2101010,
    wl_shm::Format::Xrgb2101010,
    wl_shm::Format::Abgr2101010,
    wl_shm::Format::Xbgr2101010,
    wl_shm::Format::Bgr888,
    wl_shm::Format::Rgb888,
];

/// Bytes a pixel of one of [`FORMATS`] takes.
pub fn bytes_per_pixel(format: wl_shm::Format) -> usize {
    match format {
        wl_shm::Format::Bgr888 | wl_shm::Format::Rgb888 => 3,
        _ => 4,
    }
}

/// The first of [`FORMATS`] the source offers.
pub fn pick_format(offered: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    FORMATS.into_iter().find(|f| offered.contains(f))
}

/// One of [`FORMATS`] in memory (little endian) to RGBA bytes; X formats become opaque.
pub fn to_rgba(format: wl_shm::Format, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    // A 10-bit channel at `shift` of the little-endian word, as its top 8 bits.
    let ten = |v: u32, shift: u32| (((v >> shift) & 0x3ff) >> 2) as u8;
    let two = |v: u32| ((v >> 30) * 85) as u8;
    for p in raw.chunks_exact(bytes_per_pixel(format)) {
        if p.len() == 3 {
            // BGR888 is R, G, B in memory; RGB888 is B, G, R.
            let px = match format {
                wl_shm::Format::Bgr888 => [p[0], p[1], p[2], 255],
                _ => [p[2], p[1], p[0], 255],
            };
            out.extend_from_slice(&px);
            continue;
        }
        let v = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        let px = match format {
            wl_shm::Format::Argb8888 => [p[2], p[1], p[0], p[3]],
            wl_shm::Format::Xrgb8888 => [p[2], p[1], p[0], 255],
            wl_shm::Format::Abgr8888 => [p[0], p[1], p[2], p[3]],
            wl_shm::Format::Argb2101010 => [ten(v, 20), ten(v, 10), ten(v, 0), two(v)],
            wl_shm::Format::Xrgb2101010 => [ten(v, 20), ten(v, 10), ten(v, 0), 255],
            wl_shm::Format::Abgr2101010 => [ten(v, 0), ten(v, 10), ten(v, 20), two(v)],
            wl_shm::Format::Xbgr2101010 => [ten(v, 0), ten(v, 10), ten(v, 20), 255],
            _ => [p[0], p[1], p[2], 255],
        };
        out.extend_from_slice(&px);
    }
    out
}

#[derive(Default)]
struct SessionState {
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    done: bool,
    stopped: bool,
}

#[derive(Default, PartialEq)]
enum FrameState {
    #[default]
    Pending,
    Ready,
    Failed(String),
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, Option<String>)>,
    session: SessionState,
    frame: FrameState,
    /// The transform river applied to the frame's buffer (`wl_output.transform`).
    transform: u32,
    /// The frame's presentation time (CLOCK_MONOTONIC), when the compositor sent one.
    presented: Option<Duration>,
}

/// One frame of a kept [`Session`]: the source's shm bytes as they came,
/// `width * bytes_per_pixel(format)` a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: wl_shm::Format,
    pub pixels: Vec<u8>,
    /// When the compositor presented it (CLOCK_MONOTONIC), if it said.
    pub presented: Option<Duration>,
}

/// A capture session kept open for a screencast: frames come as the source changes.
pub struct Session {
    session: ExtImageCopyCaptureSessionV1,
    source: ExtImageCaptureSourceV1,
    /// The shm buffer frames are copied into, its file, size and format, while they fit.
    buffer: Option<(wl_buffer::WlBuffer, File, (u32, u32), wl_shm::Format)>,
    /// The frame asked for and not answered yet.
    pending: Option<ExtImageCopyCaptureFrameV1>,
}

/// A connection to river for capturing.
pub struct Capturer {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    shm: wl_shm::WlShm,
    copy: ExtImageCopyCaptureManagerV1,
    output_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    toplevel_sources: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    list: Option<ExtForeignToplevelListV1>,
}

impl Capturer {
    /// Connects to `WAYLAND_DISPLAY` and learns the outputs and toplevels there now.
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("no Wayland display")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let shm = globals.bind(&qh, 1..=1, ()).context("wl_shm missing")?;
        let copy = globals
            .bind(&qh, 1..=1, ())
            .context("ext_image_copy_capture_manager_v1 missing")?;
        let output_sources = globals.bind(&qh, 1..=1, ()).ok();
        let toplevel_sources = globals.bind(&qh, 1..=1, ()).ok();
        let list = globals.bind(&qh, 1..=1, ()).ok();
        let mut state = State::default();
        for g in globals.contents().clone_list() {
            if g.interface == wl_output::WlOutput::interface().name && g.version >= 4 {
                let o = globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, _>(g.name, 4, &qh, ());
                state.outputs.push((o, None));
            }
        }
        // The first roundtrip brings the toplevels, the second their identifiers.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        Ok(Self {
            conn,
            queue,
            state,
            shm,
            copy,
            output_sources,
            toplevel_sources,
            list,
        })
    }

    /// Copies one frame of a source, waiting at most `timeout`.
    pub fn capture(&mut self, source: Source, cursor: bool, timeout: Duration) -> Result<Image> {
        let qh = self.queue.handle();
        let src = self.source(source)?;
        let options = if cursor {
            ext_image_copy_capture_manager_v1::Options::PaintCursors
        } else {
            ext_image_copy_capture_manager_v1::Options::empty()
        };
        self.state.session = SessionState::default();
        let session = self.copy.create_session(&src, options, &qh, ());
        let res = self.copy_frame(&session, &qh, Instant::now() + timeout);
        session.destroy();
        src.destroy();
        let _ = self.conn.flush();
        res
    }

    /// Opens a session on a source that stays open for [`Self::next_frame`].
    pub fn open(&mut self, source: Source, cursor: bool) -> Result<Session> {
        let qh = self.queue.handle();
        let src = self.source(source)?;
        let options = if cursor {
            ext_image_copy_capture_manager_v1::Options::PaintCursors
        } else {
            ext_image_copy_capture_manager_v1::Options::empty()
        };
        self.state.session = SessionState::default();
        let session = self.copy.create_session(&src, options, &qh, ());
        self.conn.flush()?;
        Ok(Session {
            session,
            source: src,
            buffer: None,
            pending: None,
        })
    }

    /// The source's next frame with new content, or `None` when nothing changed before the
    /// deadline (the frame asked for stays asked). Fails when the source goes away.
    pub fn next_frame(&mut self, s: &mut Session, deadline: Instant) -> Result<Option<Frame>> {
        let qh = self.queue.handle();
        if s.pending.is_none() {
            if !self.wait_for(deadline, |st| st.session.done || st.session.stopped)? {
                return Ok(None);
            }
            if self.state.session.stopped {
                bail!("the capture source went away");
            }
            let size = self.state.session.size.context("no buffer size")?;
            let format = pick_format(&self.state.session.formats)
                .context("no shm format newm-river reads")?;
            if s.buffer
                .as_ref()
                .is_none_or(|b| b.2 != size || b.3 != format)
            {
                if let Some((b, ..)) = s.buffer.take() {
                    b.destroy();
                }
                let (w, h) = size;
                let bpp = bytes_per_pixel(format);
                let len = w as usize * h as usize * bpp;
                let file = memfd(len)?;
                let pool = self.shm.create_pool(file.as_fd(), len as i32, &qh, ());
                let stride = (w as usize * bpp) as i32;
                let buffer = pool.create_buffer(0, w as i32, h as i32, stride, format, &qh, ());
                pool.destroy();
                s.buffer = Some((buffer, file, size, format));
            }
            let (buffer, _, (w, h), _) = s.buffer.as_ref().context("no buffer")?;
            self.state.frame = FrameState::Pending;
            self.state.presented = None;
            let frame = s.session.create_frame(&qh, ());
            frame.attach_buffer(buffer);
            frame.damage_buffer(0, 0, *w as i32, *h as i32);
            frame.capture();
            s.pending = Some(frame);
        }
        if !self.wait_for(deadline, |st| {
            st.frame != FrameState::Pending || st.session.stopped
        })? {
            return Ok(None);
        }
        if let Some(frame) = s.pending.take() {
            frame.destroy();
        }
        if self.state.session.stopped {
            bail!("the capture source went away");
        }
        match std::mem::take(&mut self.state.frame) {
            FrameState::Ready => {}
            FrameState::Failed(reason) if reason.contains("Stopped") => {
                bail!("the capture source went away")
            }
            // New buffer constraints (a resize) or a passing failure: the next call asks again.
            FrameState::Failed(_) | FrameState::Pending => return Ok(None),
        }
        let (_, file, (w, h), format) = s.buffer.as_ref().context("no buffer")?;
        let mut pixels = vec![0; *w as usize * *h as usize * bytes_per_pixel(*format)];
        file.read_exact_at(&mut pixels, 0)?;
        Ok(Some(Frame {
            width: *w,
            height: *h,
            format: *format,
            pixels,
            presented: self.state.presented,
        }))
    }

    /// Ends a session from [`Self::open`].
    pub fn close(&mut self, s: Session) {
        if let Some(frame) = s.pending {
            frame.destroy();
        }
        if let Some((b, ..)) = s.buffer {
            b.destroy();
        }
        s.session.destroy();
        s.source.destroy();
        let _ = self.conn.flush();
    }

    fn source(&mut self, source: Source) -> Result<ExtImageCaptureSourceV1> {
        let qh = self.queue.handle();
        Ok(match source {
            Source::Output(name) => {
                let mgr = self
                    .output_sources
                    .as_ref()
                    .context("ext_output_image_capture_source_manager_v1 missing")?;
                let (o, _) = self
                    .state
                    .outputs
                    .iter()
                    .find(|(_, n)| n.as_deref() == Some(name))
                    .with_context(|| format!("no output {name}"))?;
                mgr.create_source(o, &qh, ())
            }
            Source::Toplevel(id) => {
                let mgr = self
                    .toplevel_sources
                    .as_ref()
                    .context("ext_foreign_toplevel_image_capture_source_manager_v1 missing")?;
                let (h, _) = self
                    .state
                    .toplevels
                    .iter()
                    .find(|(_, i)| i.as_deref() == Some(id))
                    .with_context(|| format!("no toplevel {id}"))?;
                mgr.create_source(h, &qh, ())
            }
        })
    }

    fn copy_frame(
        &mut self,
        session: &ExtImageCopyCaptureSessionV1,
        qh: &QueueHandle<State>,
        deadline: Instant,
    ) -> Result<Image> {
        self.wait(deadline, |s| s.session.done || s.session.stopped)?;
        if self.state.session.stopped {
            bail!("the capture source went away");
        }
        let (w, h) = self.state.session.size.context("no buffer size")?;
        let offered = &self.state.session.formats;
        let format = pick_format(offered)
            .with_context(|| format!("no shm format newm-river reads among {offered:?}"))?;
        let bpp = bytes_per_pixel(format);
        let len = w as usize * h as usize * bpp;
        let file = memfd(len)?;
        let pool = self.shm.create_pool(file.as_fd(), len as i32, qh, ());
        let stride = w as i32 * bpp as i32;
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride, format, qh, ());
        pool.destroy();
        self.state.frame = FrameState::Pending;
        self.state.transform = 0;
        let frame = session.create_frame(qh, ());
        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, w as i32, h as i32);
        frame.capture();
        let res = self.wait(deadline, |s| s.frame != FrameState::Pending);
        frame.destroy();
        buffer.destroy();
        res?;
        if let FrameState::Failed(reason) = &self.state.frame {
            bail!("capture failed: {reason}");
        }
        let mut raw = vec![0; len];
        file.read_exact_at(&mut raw, 0)?;
        Ok(upright(
            Image {
                width: w,
                height: h,
                pixels: to_rgba(format, &raw),
            },
            self.state.transform,
        ))
    }

    /// Dispatches events until `done` holds or the deadline passes.
    fn wait(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> Result<()> {
        if !self.wait_for(deadline, done)? {
            bail!("capture timed out");
        }
        Ok(())
    }

    /// Dispatches events until `done` holds (true) or the deadline passes (false).
    fn wait_for(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> Result<bool> {
        loop {
            self.queue.dispatch_pending(&mut self.state)?;
            if done(&self.state) {
                return Ok(true);
            }
            self.conn.flush()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = left.as_millis().min(i32::MAX as u128 - 1) as i32 + 1;
            // SAFETY: fd is one valid pollfd.
            let n = unsafe { libc::poll(&mut fd, 1, ms) };
            if n < 0 {
                drop(guard);
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if n == 0 {
                drop(guard);
                continue;
            }
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        if let Some(list) = &self.list {
            list.stop();
        }
        for (h, _) in &self.state.toplevels {
            h.destroy();
        }
        let _ = self.conn.flush();
    }
}

/// An anonymous file of `len` bytes for a shm pool.
fn memfd(len: usize) -> Result<File> {
    // SAFETY: the name is a valid C string; the fd returned is owned by the File below.
    let fd = unsafe { libc::memfd_create(c"newm-river-capture".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fd is a fresh descriptor nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(len as u64)?;
    Ok(file)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(o) = state.outputs.iter_mut().find(|(o, _)| o == proxy)
        {
            o.1 = Some(name);
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push((toplevel, None));
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                if let Some(t) = state.toplevels.iter_mut().find(|(h, _)| h == proxy) {
                    t.1 = Some(identifier);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.retain(|(h, _)| h != proxy);
                proxy.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                // A second set of constraints (the source resized) replaces the first.
                if state.session.done {
                    state.session.done = false;
                    state.session.formats.clear();
                }
                state.session.size = Some((width, height));
            }
            Event::ShmFormat {
                format: WEnum::Value(f),
            } => state.session.formats.push(f),
            Event::Done => {
                // Which shm formats river offers says whether other capture clients can read
                // the screen in shared memory: wlroots offers only the renderer's preferred read
                // format, a 3-byte one on some GPUs (newm/e2e/screencast.sh reads this line).
                tracing::debug!("capture: shm formats {:?}", state.session.formats);
                state.session.done = true;
            }
            Event::Stopped => state.session.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => state.frame = FrameState::Ready,
            Event::Transform { transform } => {
                state.transform = match transform {
                    wayland_client::WEnum::Value(t) => t.into(),
                    wayland_client::WEnum::Unknown(u) => u,
                };
            }
            Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                state.presented = Some(Duration::new(secs, tv_nsec));
            }
            Event::Failed { reason } => state.frame = FrameState::Failed(format!("{reason:?}")),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtForeignToplevelImageCaptureSourceManagerV1);

/// Undoes the transform river applied to a captured buffer (a rotated or flipped output), so
/// the image is the way the output shows it. `transform` is a `wl_output.transform`: rotations
/// counter-clockwise, the flipped ones flipped about the vertical axis first.
pub fn upright(img: Image, transform: u32) -> Image {
    let (bw, bh) = (img.width as usize, img.height as usize);
    let turned = matches!(transform, 1 | 3 | 5 | 7);
    let (w, h) = if turned { (bh, bw) } else { (bw, bh) };
    // Where the upright pixel (x, y) is in the buffer.
    let at = |x: usize, y: usize| -> (usize, usize) {
        match transform {
            1 => (y, w - 1 - x),
            2 => (w - 1 - x, h - 1 - y),
            3 => (h - 1 - y, x),
            4 => (w - 1 - x, y),
            5 => (y, x),
            6 => (x, h - 1 - y),
            7 => (h - 1 - y, w - 1 - x),
            _ => (x, y),
        }
    };
    if transform == 0 || transform > 7 || img.pixels.len() < bw * bh * 4 {
        return img;
    }
    let mut pixels = vec![0; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (bx, by) = at(x, y);
            let src = (by * bw + bx) * 4;
            let dst = (y * w + x) * 4;
            pixels[dst..dst + 4].copy_from_slice(&img.pixels[src..src + 4]);
        }
    }
    Image {
        width: w as u32,
        height: h as u32,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_rotated_or_flipped_buffer_comes_back_upright() {
        // Upright 3x2: pixel values 1..6, row by row.
        let upright_px: Vec<u8> = (1..=6).flat_map(|v| [v, 0, 0, 255]).collect();
        let img = |w: u32, h: u32, order: &[u8]| Image {
            width: w,
            height: h,
            pixels: order.iter().flat_map(|&v| [v, 0, 0, 255]).collect(),
        };
        // The buffer river hands over is the upright image under each transform.
        let cases: [(u32, u32, u32, &[u8]); 8] = [
            (0, 3, 2, &[1, 2, 3, 4, 5, 6]),
            (1, 2, 3, &[3, 6, 2, 5, 1, 4]),
            (2, 3, 2, &[6, 5, 4, 3, 2, 1]),
            (3, 2, 3, &[4, 1, 5, 2, 6, 3]),
            (4, 3, 2, &[3, 2, 1, 6, 5, 4]),
            (5, 2, 3, &[1, 4, 2, 5, 3, 6]),
            (6, 3, 2, &[4, 5, 6, 1, 2, 3]),
            (7, 2, 3, &[6, 3, 5, 2, 4, 1]),
        ];
        for (t, w, h, order) in cases {
            let back = upright(img(w, h, order), t);
            assert_eq!((back.width, back.height), (3, 2), "transform {t}");
            assert_eq!(back.pixels, upright_px, "transform {t}");
        }
    }

    use super::*;

    #[test]
    fn argb_is_preferred_and_x_formats_follow() {
        use wl_shm::Format::*;
        assert_eq!(pick_format(&[Xbgr8888, Argb8888]), Some(Argb8888));
        assert_eq!(pick_format(&[Xbgr8888, Xrgb8888]), Some(Xrgb8888));
        assert_eq!(pick_format(&[Rgb565]), None);
    }

    #[test]
    fn shm_formats_become_rgba() {
        use wl_shm::Format::*;
        let raw = [1, 2, 3, 4];
        assert_eq!(to_rgba(Argb8888, &raw), vec![3, 2, 1, 4]);
        assert_eq!(to_rgba(Xrgb8888, &raw), vec![3, 2, 1, 255]);
        assert_eq!(to_rgba(Abgr8888, &raw), vec![1, 2, 3, 4]);
        assert_eq!(to_rgba(Xbgr8888, &raw), vec![1, 2, 3, 255]);
        // 10 bits: red 0x3ff, green 0x200, blue 0 and alpha 3 (XRGB: bits 29-20, 19-10, 9-0).
        let v: u32 = (3 << 30) | (0x3ff << 20) | (0x200 << 10);
        let raw10 = v.to_le_bytes();
        assert_eq!(to_rgba(Argb2101010, &raw10), vec![255, 128, 0, 255]);
        assert_eq!(to_rgba(Xbgr2101010, &raw10), vec![0, 128, 255, 255]);
        assert_eq!(
            to_rgba(Bgr888, &[1, 2, 3, 4, 5, 6]),
            vec![1, 2, 3, 255, 4, 5, 6, 255]
        );
        assert_eq!(to_rgba(Rgb888, &[1, 2, 3]), vec![3, 2, 1, 255]);
    }
}
