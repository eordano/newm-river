//! Screencasts for Spectacle's recordings: river serves KDE's zkde_screencast_unstable_v1
//! and relays each stream over `newm_river_screencast_v1`; here a cast is planned against the
//! scene, captured with ext-image-copy-capture on a thread of its own (a session kept open, so
//! frames come as the source changes) and fed to a PipeWire stream (`pw.rs`) whose node id goes
//! back to river as `created`.
//!
//! Outputs are found by name, windows by the uuid `org.kde.KWin` gave out (`kwin.rs`: their
//! ext-foreign-toplevel-list identifier), regions by the output showing most of them, cropped.
//! While the session is locked new casts fail and running ones send no frames.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_shm;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

use crate::capture::{self, Capturer, Source};
use crate::model::Rect;
use crate::protocols::newm_screencast::{
    newm_river_screencast_cast_v1::{self, NewmRiverScreencastCastV1},
    newm_river_screencast_v1::{self, NewmRiverScreencastV1},
};
use crate::pw::{self, Cmd, Event, Layout, PipeWire};
use crate::river::State;
use crate::screenshot::Scene;

/// What a client asked river for.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Output {
        name: String,
        pointer: u32,
    },
    Window {
        uuid: String,
        pointer: u32,
    },
    Region {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pointer: u32,
    },
}

/// What a cast captures.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub source: PlanSource,
    /// The region, and the area of the output it is cut from, in logical pixels.
    pub crop: Option<(Rect, Rect)>,
    /// Offer BGRA: windows can be translucent, screens cannot.
    pub alpha: bool,
    pub cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanSource {
    Output(String),
    Toplevel(String),
}

/// zkde_screencast_unstable_v1.pointer: 1 hidden, 2 embedded, 4 metadata. Metadata is drawn in:
/// there is no cursor metadata here, and a recording with the pointer asked shows it.
pub fn paints_cursor(pointer: u32) -> bool {
    pointer & (2 | 4) != 0
}

/// A window uuid as KWin writes it (`{...}`) or bare.
pub fn parse_uuid(uuid: &str) -> &str {
    uuid.trim_matches(['{', '}'])
}

fn overlap(a: Rect, b: Rect) -> i64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0 || h <= 0 {
        0
    } else {
        i64::from(w) * i64::from(h)
    }
}

/// Decides what a request casts in a scene, or why it cannot.
pub fn plan(request: &Request, scene: &Scene) -> Result<Plan, String> {
    if scene.locked {
        return Err("the screen is locked".to_owned());
    }
    match request {
        Request::Output { name, pointer } => {
            if !scene.outputs.iter().any(|o| &o.name == name) {
                return Err(format!("no output {name}"));
            }
            Ok(Plan {
                source: PlanSource::Output(name.clone()),
                crop: None,
                alpha: false,
                cursor: paints_cursor(*pointer),
            })
        }
        Request::Window { uuid, pointer } => {
            let uuid = parse_uuid(uuid);
            scene
                .windows
                .iter()
                .find(|w| w.identifier.as_deref() == Some(uuid))
                .map(|_| Plan {
                    source: PlanSource::Toplevel(uuid.to_owned()),
                    crop: None,
                    alpha: true,
                    cursor: paints_cursor(*pointer),
                })
                .ok_or_else(|| "no window has this uuid".to_owned())
        }
        Request::Region {
            x,
            y,
            width,
            height,
            pointer,
        } => {
            let side = |n: u32| n.min(REGION_SIDE) as i32;
            let reach = |n: i32| n.clamp(-REGION_REACH, REGION_REACH);
            let region = Rect {
                x: reach(*x),
                y: reach(*y),
                w: side(*width),
                h: side(*height),
            };
            if region.w <= 0 || region.h <= 0 {
                return Err("the region is empty".to_owned());
            }
            let output = scene
                .outputs
                .iter()
                .map(|o| (overlap(region, o.area), o))
                .filter(|(n, _)| *n > 0)
                .max_by_key(|(n, _)| *n)
                .map(|(_, o)| o)
                .ok_or_else(|| "no output shows this region".to_owned())?;
            Ok(Plan {
                source: PlanSource::Output(output.name.clone()),
                crop: Some((region, output.area)),
                alpha: false,
                cursor: paints_cursor(*pointer),
            })
        }
    }
}

/// The farthest a region's corner may lie from the origin, and the largest side it may have, in
/// logical pixels: far past any layout of outputs, and far from i32's limits, which the overlap
/// and crop math (corner plus side, overflow-checked) would otherwise cross.
const REGION_REACH: i32 = 1 << 20;
const REGION_SIDE: u32 = 1 << 16;

/// The part of an output's buffer (`buf` pixels for logical `area`) a logical region covers:
/// x, y, width, height in buffer pixels, clipped to the buffer. None when nothing is left.
pub fn crop_px(region: Rect, area: Rect, (bw, bh): (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    if area.w <= 0 || area.h <= 0 {
        return None;
    }
    let sx = f64::from(bw) / f64::from(area.w);
    let sy = f64::from(bh) / f64::from(area.h);
    let x0 = (f64::from(region.x - area.x) * sx)
        .round()
        .clamp(0., f64::from(bw));
    let y0 = (f64::from(region.y - area.y) * sy)
        .round()
        .clamp(0., f64::from(bh));
    let x1 = (f64::from(region.x + region.w - area.x) * sx)
        .round()
        .clamp(0., f64::from(bw));
    let y1 = (f64::from(region.y + region.h - area.y) * sy)
        .round()
        .clamp(0., f64::from(bh));
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    (w > 0 && h > 0).then_some((x0 as u32, y0 as u32, w, h))
}

/// Cuts rows out of a `width * 4` a row image.
pub fn cut(pixels: &[u8], width: u32, (x, y, w, h): (u32, u32, u32, u32)) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for row in y..y + h {
        let start = (row as usize * width as usize + x as usize) * 4;
        out.extend_from_slice(&pixels[start..start + w as usize * 4]);
    }
    out
}

/// A captured frame as the PipeWire stream takes it, or None when the crop leaves nothing.
fn to_pw(frame: capture::Frame, plan: &Plan) -> Option<pw::Frame> {
    let pts = frame
        .presented
        .and_then(|t| i64::try_from(t.as_nanos()).ok())
        .unwrap_or_else(pw::monotonic_ns);
    // The 8-bit 4-byte formats go to PipeWire as they came (the stream converts them once);
    // any other shm format (3 bytes, 10 bits) becomes RGBA first.
    let (layout, raw) = match frame.format {
        wl_shm::Format::Argb8888 => (Layout::Bgra, frame.pixels),
        wl_shm::Format::Xrgb8888 => (Layout::Bgrx, frame.pixels),
        wl_shm::Format::Abgr8888 => (Layout::Rgba, frame.pixels),
        wl_shm::Format::Xbgr8888 => (Layout::Rgbx, frame.pixels),
        other => (Layout::Rgba, capture::to_rgba(other, &frame.pixels)),
    };
    let (width, height, pixels) = match plan.crop {
        Some((region, area)) => {
            let c = crop_px(region, area, (frame.width, frame.height))?;
            (c.2, c.3, cut(&raw, frame.width, c))
        }
        None => (frame.width, frame.height, raw),
    };
    Some(pw::Frame {
        width,
        height,
        layout,
        pixels,
        pts,
    })
}

/// An eventfd that wakes the window manager's poll loop.
#[derive(Clone)]
struct Wake(Arc<File>);

impl Wake {
    fn new() -> Result<Self> {
        // SAFETY: eventfd returns a new fd or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        Ok(Self(Arc::new(unsafe { File::from_raw_fd(fd) })))
    }

    fn wake(&self) {
        let _ = (&*self.0).write(&1u64.to_ne_bytes());
    }

    fn drain(&self) {
        let mut b = [0; 8];
        let _ = (&*self.0).read(&mut b);
    }
}

/// Events from the PipeWire and capture threads to the window manager's thread.
#[derive(Clone)]
struct Sink {
    tx: mpsc::Sender<Event>,
    wake: Wake,
}

impl Sink {
    fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.wake.wake();
    }
}

struct Cast {
    id: u64,
    proxy: NewmRiverScreencastCastV1,
    stop: Arc<AtomicBool>,
    created: bool,
}

/// The window manager's casts.
pub struct Casts {
    _relay: Option<NewmRiverScreencastV1>,
    scene: Arc<Mutex<Scene>>,
    casts: HashMap<ObjectId, Cast>,
    pw: Option<PipeWire>,
    rx: mpsc::Receiver<Event>,
    sink: Sink,
    locked: Arc<AtomicBool>,
    next_id: u64,
}

impl Casts {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<State>, scene: Arc<Mutex<Scene>>) -> Self {
        let relay = globals.bind(qh, 1..=1, ()).ok();
        if relay.is_none() {
            warn!("newm_river_screencast_v1 missing: no screencasts for Spectacle");
        }
        let (tx, rx) = mpsc::channel();
        let wake = Wake::new().expect("eventfd");
        Self {
            _relay: relay,
            scene,
            casts: HashMap::new(),
            pw: None,
            rx,
            sink: Sink { tx, wake },
            locked: Arc::new(AtomicBool::new(false)),
            next_id: 1,
        }
    }

    /// The fd the poll loop watches for cast events.
    pub fn wake_fd(&self) -> RawFd {
        self.sink.wake.0.as_raw_fd()
    }

    pub fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::SeqCst);
    }

    /// Answers river for what the threads reported.
    pub fn dispatch(&mut self) {
        self.sink.wake.drain();
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Node { id, node } => {
                    if let Some(c) = self.casts.values_mut().find(|c| c.id == id) {
                        info!("cast {id}: PipeWire node {node}");
                        c.created = true;
                        c.proxy.created(node);
                    }
                }
                Event::Ended { id, error } => {
                    let key = self
                        .casts
                        .iter()
                        .find(|(_, c)| c.id == id)
                        .map(|(k, _)| k.clone());
                    if let Some(c) = key.and_then(|k| self.casts.remove(&k)) {
                        info!("cast {id} ended: {error}");
                        c.stop.store(true, Ordering::SeqCst);
                        if c.created {
                            c.proxy.closed();
                        } else {
                            c.proxy.failed(error);
                        }
                        c.proxy.destroy();
                    }
                }
            }
        }
    }

    fn start(&mut self, proxy: NewmRiverScreencastCastV1, request: Request) {
        let scene = self.scene.lock().map(|s| s.clone()).unwrap_or_default();
        let plan = match plan(&request, &scene) {
            Ok(p) => p,
            Err(e) => {
                info!("screencast {request:?} refused: {e}");
                proxy.failed(e);
                proxy.destroy();
                return;
            }
        };
        if self.pw.as_ref().is_none_or(|p| !p.alive()) {
            let sink = self.sink.clone();
            self.pw = match PipeWire::spawn(move |e| sink.send(e)) {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!("screencast: {e:#}");
                    proxy.failed(format!("{e:#}"));
                    proxy.destroy();
                    return;
                }
            };
        }
        let Some(pw) = &self.pw else { return };
        let id = self.next_id;
        self.next_id += 1;
        info!("cast {id}: {request:?} -> {plan:?}");
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = CastThread {
            id,
            plan,
            stop: stop.clone(),
            locked: self.locked.clone(),
            pw: pw.sender(),
            sink: self.sink.clone(),
        };
        if let Err(e) = thread::Builder::new()
            .name(format!("cast {id}"))
            .spawn(move || ctx.run())
        {
            proxy.failed(format!("{e}"));
            proxy.destroy();
            return;
        }
        self.casts.insert(
            proxy.id(),
            Cast {
                id,
                proxy,
                stop,
                created: false,
            },
        );
    }

    /// The client closed its stream.
    fn stopped(&mut self, proxy: &NewmRiverScreencastCastV1) {
        if let Some(c) = self.casts.remove(&proxy.id()) {
            info!("cast {}: the client closed it", c.id);
            c.stop.store(true, Ordering::SeqCst);
            if let Some(pw) = &self.pw {
                pw.send(Cmd::Stop { id: c.id });
            }
        }
        proxy.destroy();
    }
}

/// One cast's capture loop.
struct CastThread {
    id: u64,
    plan: Plan,
    stop: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    pw: pipewire::channel::Sender<Cmd>,
    sink: Sink,
}

impl CastThread {
    fn run(self) {
        let res = self.capture();
        let _ = self.pw.send(Cmd::Stop { id: self.id });
        if let Err(e) = res
            && !self.stop.load(Ordering::SeqCst)
        {
            self.sink.send(Event::Ended {
                id: self.id,
                error: format!("{e:#}"),
            });
        }
    }

    fn capture(&self) -> Result<()> {
        let mut cap = Capturer::connect()?;
        let source = match &self.plan.source {
            PlanSource::Output(name) => Source::Output(name),
            PlanSource::Toplevel(id) => Source::Toplevel(id),
        };
        let mut session = cap.open(source, self.plan.cursor)?;
        let mut started = false;
        let res = loop {
            if self.stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            let frame = match cap.next_frame(&mut session, Instant::now() + POLL) {
                Ok(Some(f)) => f,
                Ok(None) => continue,
                Err(e) => break Err(e),
            };
            // Locked: the lock screen is no one's to record.
            if self.locked.load(Ordering::SeqCst) {
                continue;
            }
            let Some(frame) = to_pw(frame, &self.plan) else {
                continue;
            };
            if !started {
                started = true;
                let _ = self.pw.send(Cmd::Start {
                    id: self.id,
                    width: frame.width,
                    height: frame.height,
                    alpha: self.plan.alpha,
                });
            }
            let _ = self.pw.send(Cmd::Frame { id: self.id, frame });
        };
        cap.close(session);
        res
    }
}

/// How often a capture loop looks at its stop flag while the source shows nothing new.
const POLL: Duration = Duration::from_millis(200);

impl Dispatch<NewmRiverScreencastV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &NewmRiverScreencastV1,
        event: newm_river_screencast_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use newm_river_screencast_v1::Event;
        let (cast, request) = match event {
            Event::StreamOutput {
                cast,
                output_name,
                pointer,
            } => (
                cast,
                Request::Output {
                    name: output_name,
                    pointer,
                },
            ),
            Event::StreamWindow {
                cast,
                window_uuid,
                pointer,
            } => (
                cast,
                Request::Window {
                    uuid: window_uuid,
                    pointer,
                },
            ),
            Event::StreamRegion {
                cast,
                x,
                y,
                width,
                height,
                scale: _,
                pointer,
            } => (
                cast,
                Request::Region {
                    x,
                    y,
                    width,
                    height,
                    pointer,
                },
            ),
        };
        state.casts.start(cast, request);
    }

    event_created_child!(State, NewmRiverScreencastV1, [
        newm_river_screencast_v1::EVT_STREAM_OUTPUT_OPCODE => (NewmRiverScreencastCastV1, ()),
        newm_river_screencast_v1::EVT_STREAM_WINDOW_OPCODE => (NewmRiverScreencastCastV1, ()),
        newm_river_screencast_v1::EVT_STREAM_REGION_OPCODE => (NewmRiverScreencastCastV1, ()),
    ]);
}

impl Dispatch<NewmRiverScreencastCastV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &NewmRiverScreencastCastV1,
        event: newm_river_screencast_cast_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            newm_river_screencast_cast_v1::Event::Stopped => state.casts.stopped(proxy),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screenshot::{SceneOutput, SceneWindow};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    fn scene() -> Scene {
        Scene {
            outputs: vec![
                SceneOutput {
                    name: "A".into(),
                    area: rect(0, 0, 100, 50),
                },
                SceneOutput {
                    name: "B".into(),
                    area: rect(100, 0, 100, 50),
                },
            ],
            windows: vec![SceneWindow {
                id: 7,
                identifier: Some("abc".into()),
                rect: rect(10, 10, 20, 20),
                title: None,
                app_id: None,
            }],
            ..Scene::default()
        }
    }

    #[test]
    fn outputs_are_cast_by_name_without_alpha() {
        let p = plan(
            &Request::Output {
                name: "B".into(),
                pointer: 2,
            },
            &scene(),
        )
        .unwrap();
        assert_eq!(p.source, PlanSource::Output("B".into()));
        assert!(!p.alpha && p.cursor && p.crop.is_none());
        let missing = Request::Output {
            name: "C".into(),
            pointer: 1,
        };
        assert!(plan(&missing, &scene()).is_err());
    }

    #[test]
    fn windows_are_cast_by_uuid_with_or_without_braces() {
        for uuid in ["abc", "{abc}"] {
            let p = plan(
                &Request::Window {
                    uuid: uuid.into(),
                    pointer: 1,
                },
                &scene(),
            )
            .unwrap();
            assert_eq!(p.source, PlanSource::Toplevel("abc".into()));
            assert!(p.alpha && !p.cursor);
        }
        let missing = Request::Window {
            uuid: "{zzz}".into(),
            pointer: 1,
        };
        assert!(plan(&missing, &scene()).is_err());
    }

    #[test]
    fn a_region_is_cut_from_the_output_showing_most_of_it() {
        let req = |x, width| Request::Region {
            x,
            y: 10,
            width,
            height: 20,
            pointer: 1,
        };
        let p = plan(&req(90, 40), &scene()).unwrap();
        assert_eq!(p.source, PlanSource::Output("B".into()));
        assert_eq!(p.crop, Some((rect(90, 10, 40, 20), rect(100, 0, 100, 50))));
        assert!(plan(&req(300, 10), &scene()).is_err());
        assert!(plan(&req(0, 0), &scene()).is_err());
    }

    #[test]
    fn a_region_stays_far_from_integer_limits() {
        let req = |x, width| Request::Region {
            x,
            y: 10,
            width,
            height: 20,
            pointer: 1,
        };
        assert!(plan(&req(i32::MAX - 5, 100), &scene()).is_err());
        assert!(plan(&req(i32::MIN, u32::MAX), &scene()).is_err());
        let p = plan(&req(0, u32::MAX), &scene()).unwrap();
        let (region, _) = p.crop.unwrap();
        assert_eq!((region.x, region.w), (0, REGION_SIDE as i32));
    }

    #[test]
    fn a_locked_session_refuses_every_cast() {
        let mut s = scene();
        s.locked = true;
        let req = Request::Output {
            name: "A".into(),
            pointer: 1,
        };
        assert_eq!(plan(&req, &s), Err("the screen is locked".to_owned()));
    }

    #[test]
    fn the_pointer_is_drawn_when_embedded_or_metadata_is_asked() {
        assert!(!paints_cursor(1));
        assert!(paints_cursor(2));
        assert!(paints_cursor(4));
    }

    #[test]
    fn crops_scale_to_buffer_pixels_and_clip() {
        let area = rect(100, 0, 100, 50);
        assert_eq!(
            crop_px(rect(110, 10, 20, 20), area, (200, 100)),
            Some((20, 20, 40, 40))
        );
        assert_eq!(
            crop_px(rect(90, 10, 40, 20), area, (100, 50)),
            Some((0, 10, 30, 20))
        );
        assert_eq!(crop_px(rect(0, 0, 10, 10), area, (100, 50)), None);
    }

    #[test]
    fn cut_takes_the_rows_of_a_rectangle() {
        // 3x2 image, pixel value = index.
        let px: Vec<u8> = (0..6u8).flat_map(|i| [i; 4]).collect();
        assert_eq!(
            cut(&px, 3, (1, 0, 2, 2)),
            [1, 1, 1, 1, 2, 2, 2, 2, 4, 4, 4, 4, 5, 5, 5, 5]
        );
    }

    #[test]
    fn three_byte_frames_become_four_byte_rgba() {
        let f = capture::Frame {
            width: 2,
            height: 1,
            format: wl_shm::Format::Bgr888,
            pixels: vec![1, 2, 3, 4, 5, 6],
            presented: None,
        };
        let plan = Plan {
            source: PlanSource::Output("A".into()),
            crop: None,
            alpha: false,
            cursor: false,
        };
        let p = to_pw(f, &plan).unwrap();
        assert_eq!(p.pixels.len(), 8);
        assert_eq!(
            p.pixels,
            capture::to_rgba(wl_shm::Format::Bgr888, &[1, 2, 3, 4, 5, 6])
        );
    }

    #[test]
    fn frames_keep_the_presentation_time() {
        let f = capture::Frame {
            width: 2,
            height: 1,
            format: wl_shm::Format::Xrgb8888,
            pixels: vec![0; 8],
            presented: Some(Duration::from_nanos(42)),
        };
        let plan = Plan {
            source: PlanSource::Output("A".into()),
            crop: None,
            alpha: false,
            cursor: false,
        };
        let p = to_pw(f, &plan).unwrap();
        assert_eq!((p.pts, p.layout, p.width), (42, Layout::Bgrx, 2));
        assert_eq!(
            p.pixels, [0; 8],
            "a 4-byte format passes through for the stream to convert"
        );
    }
}
