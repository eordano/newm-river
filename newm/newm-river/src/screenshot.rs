//! KWin's screenshot interface, `org.kde.KWin.ScreenShot2`: the only way Spectacle captures on
//! Wayland, served here over ext-image-copy-capture.
//!
//! The window manager publishes a [`Scene`] (outputs, windows top first, focus, pointer) after
//! every render sequence; a D-Bus call picks its target from that scene ([`plan`]), captures it on
//! a Wayland connection of its own ([`crate::capture`]) and replies with the metadata, while the
//! pixels follow through the pipe the caller passed, as `QImage::Format_RGBA8888_Premultiplied`
//! rows without padding.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use zbus::interface;
use zbus::{OwnedVariant, Variant};

use crate::capture::{Capturer, Image, Source};
use crate::model::Rect;

/// `QImage::Format_RGBA8888_Premultiplied`.
const QIMAGE_FORMAT_RGBA8888_PREMULTIPLIED: u32 = 18;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

/// What the window manager shows now, for picking a capture's target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub outputs: Vec<SceneOutput>,
    /// Shown windows, the topmost first.
    pub windows: Vec<SceneWindow>,
    pub focus: Option<u64>,
    pub active_output: Option<String>,
    pub pointer: Option<(i32, i32)>,
    /// The session is locked: every capture is refused, as KWin refuses it.
    pub locked: bool,
    /// The last fresh pointer position a capture asked for (the service counts up).
    pub pointer_asked: u64,
    /// The last ask `pointer` answers: river sent it in a manage sequence after the ask.
    pub pointer_seq: u64,
    /// The last pick `queryWindowInfo` asked for (the service counts up).
    pub pick_asked: u64,
    /// The last pick that ended and what it picked: a window, an output, or nothing.
    pub picked: Option<(u64, Picked)>,
}

/// What a pick's click landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum Picked {
    Window(u64),
    Output(String),
    Nothing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneOutput {
    pub name: String,
    pub area: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneWindow {
    pub id: u64,
    /// The ext-foreign-toplevel-list identifier, once river sent it.
    pub identifier: Option<String>,
    /// On screen, logical.
    pub rect: Rect,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// An output by its connector name.
    Screen(String),
    ActiveScreen,
    ScreenUnderCursor,
    ActiveWindow,
    WindowUnderCursor,
    /// A window by its newm-river id.
    Window(u64),
    /// A rectangle in global logical coordinates.
    Area {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// All outputs.
    Workspace,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub screen: Option<String>,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    Locked,
    Cancelled,
    InvalidScreen,
    InvalidWindow,
    InvalidArea,
    Failed,
}

/// One output's pixels at its place in the global logical space.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// What a target comes to: one window, or outputs joined one way.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Window {
        identifier: String,
        rect: Rect,
    },
    Screens {
        outputs: Vec<SceneOutput>,
        join: Join,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// One output, named in the reply.
    One,
    /// Every output side by side.
    All,
    /// A logical rectangle cut out of every output.
    Area {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
}

fn contains(r: Rect, (x, y): (i32, i32)) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w && y < r.y + r.h
}

/// Picks what a target captures in a scene.
pub fn plan(target: &Target, scene: &Scene) -> Result<Plan, CaptureError> {
    if scene.locked {
        return Err(CaptureError::Locked);
    }
    let window = |w: &SceneWindow| match &w.identifier {
        Some(identifier) => Ok(Plan::Window {
            identifier: identifier.clone(),
            rect: w.rect,
        }),
        None => Err(CaptureError::InvalidWindow),
    };
    let screen = |o: &SceneOutput| Plan::Screens {
        outputs: vec![o.clone()],
        join: Join::One,
    };
    match target {
        Target::Window(id) => scene
            .windows
            .iter()
            .find(|w| w.id == *id)
            .ok_or(CaptureError::InvalidWindow)
            .and_then(window),
        Target::ActiveWindow => {
            let focus = scene.focus.ok_or(CaptureError::Cancelled)?;
            scene
                .windows
                .iter()
                .find(|w| w.id == focus)
                .ok_or(CaptureError::Cancelled)
                .and_then(window)
        }
        Target::WindowUnderCursor => {
            let p = scene.pointer.ok_or(CaptureError::Cancelled)?;
            scene
                .windows
                .iter()
                .find(|w| contains(w.rect, p))
                .ok_or(CaptureError::Cancelled)
                .and_then(window)
        }
        Target::Screen(name) => scene
            .outputs
            .iter()
            .find(|o| &o.name == name)
            .map(screen)
            .ok_or(CaptureError::InvalidScreen),
        Target::ActiveScreen => scene
            .active_output
            .as_ref()
            .and_then(|n| scene.outputs.iter().find(|o| &o.name == n))
            .or_else(|| scene.outputs.first())
            .map(screen)
            .ok_or(CaptureError::InvalidScreen),
        Target::ScreenUnderCursor => {
            let p = scene.pointer.ok_or(CaptureError::Cancelled)?;
            scene
                .outputs
                .iter()
                .find(|o| contains(o.area, p))
                .map(screen)
                .ok_or(CaptureError::Cancelled)
        }
        Target::Workspace | Target::Area { .. } => {
            if scene.outputs.is_empty() {
                return Err(CaptureError::InvalidScreen);
            }
            let join = match *target {
                Target::Area {
                    x,
                    y,
                    width,
                    height,
                } => Join::Area {
                    x,
                    y,
                    width,
                    height,
                },
                _ => Join::All,
            };
            Ok(Plan::Screens {
                outputs: scene.outputs.clone(),
                join,
            })
        }
    }
}

/// The scale an image of a logical width was captured at, to two decimals.
fn scale_of(pixels: u32, logical: i32) -> f64 {
    if logical <= 0 {
        return 1.;
    }
    (f64::from(pixels) / f64::from(logical) * 100.).round() / 100.
}

/// Captures what a plan names.
pub fn run(plan: &Plan, cursor: bool, cap: &mut Capturer) -> Result<Capture, CaptureError> {
    let grab = |cap: &mut Capturer, source: Source| -> Result<Image, CaptureError> {
        cap.capture(source, cursor, CAPTURE_TIMEOUT).map_err(|e| {
            warn!("screenshot capture of {source:?}: {e:#}");
            CaptureError::Failed
        })
    };
    match plan {
        Plan::Window { identifier, rect } => {
            let img = grab(cap, Source::Toplevel(identifier))?;
            Ok(Capture {
                width: img.width,
                height: img.height,
                scale: scale_of(img.width, rect.w),
                screen: None,
                pixels: img.pixels,
            })
        }
        Plan::Screens { outputs, join } => {
            let mut parts = Vec::new();
            for o in outputs {
                let img = grab(cap, Source::Output(&o.name))?;
                parts.push(Part {
                    x: o.area.x,
                    y: o.area.y,
                    scale: scale_of(img.width, o.area.w),
                    width: img.width,
                    height: img.height,
                    pixels: img.pixels,
                });
            }
            match *join {
                Join::One => {
                    let part = parts.pop().ok_or(CaptureError::Failed)?;
                    Ok(Capture {
                        width: part.width,
                        height: part.height,
                        scale: part.scale,
                        screen: outputs.first().map(|o| o.name.clone()),
                        pixels: part.pixels,
                    })
                }
                Join::All => composite(parts)
                    .map(|(_, _, c)| c)
                    .ok_or(CaptureError::Failed),
                Join::Area {
                    x,
                    y,
                    width,
                    height,
                } => {
                    let (ox, oy, c) = composite(parts).ok_or(CaptureError::Failed)?;
                    let (dx, dy) = (i64::from(x) - i64::from(ox), i64::from(y) - i64::from(oy));
                    crop(c, dx, dy, width, height).ok_or(CaptureError::InvalidArea)
                }
            }
        }
    }
}

/// Joins outputs into one image at the largest of their scales.
///
/// Returns the image with the logical position of its top left corner.
pub fn composite(mut parts: Vec<Part>) -> Option<(i32, i32, Capture)> {
    if parts.len() == 1 {
        let part = parts.pop()?;
        let capture = Capture {
            width: part.width,
            height: part.height,
            scale: part.scale,
            screen: None,
            pixels: part.pixels,
        };
        return Some((part.x, part.y, capture));
    }

    let scale = parts.iter().map(|part| part.scale).reduce(f64::max)?;
    let min_x = parts.iter().map(|part| part.x).min()?;
    let min_y = parts.iter().map(|part| part.y).min()?;

    // Where each part lands in the image: left, top, width, height.
    let rects: Vec<[usize; 4]> = parts
        .iter()
        .map(|part| {
            let px = |logical: f64| (logical * scale).round() as usize;
            [
                px(f64::from(part.x - min_x)),
                px(f64::from(part.y - min_y)),
                px(f64::from(part.width) / part.scale),
                px(f64::from(part.height) / part.scale),
            ]
        })
        .collect();
    let width = rects.iter().map(|[x, _, w, _]| x + w).max()?;
    let height = rects.iter().map(|[_, y, _, h]| y + h).max()?;

    let mut pixels = vec![0; width * height * 4];
    for (part, [left, top, w, h]) in parts.iter().zip(rects) {
        if part.width == 0 || part.height == 0 {
            continue;
        }
        let (src_w, src_h) = (part.width as usize, part.height as usize);
        for row in 0..h {
            let src_row = (row * src_h / h).min(src_h - 1);
            let src = &part.pixels[src_row * src_w * 4..][..src_w * 4];
            let dst = &mut pixels[((top + row) * width + left) * 4..][..w * 4];
            if w == src_w {
                dst.copy_from_slice(src);
                continue;
            }
            for (col, px) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let src_col = (col * src_w / w).min(src_w - 1);
                px.copy_from_slice(&src[src_col * 4..][..4]);
            }
        }
    }

    let capture = Capture {
        width: width as u32,
        height: height as u32,
        scale,
        screen: None,
        pixels,
    };
    Some((min_x, min_y, capture))
}

/// Cuts a logical rectangle, relative to the image's top left corner, out of an image.
///
/// Returns `None` when the rectangle misses the image.
pub fn crop(capture: Capture, x: i64, y: i64, width: u32, height: u32) -> Option<Capture> {
    let px = |logical: i64, limit: u32| {
        let px = (logical as f64 * capture.scale).round();
        px.clamp(0., f64::from(limit)) as usize
    };
    let left = px(x, capture.width);
    let right = px(x + i64::from(width), capture.width);
    let top = px(y, capture.height);
    let bottom = px(y + i64::from(height), capture.height);
    if left >= right || top >= bottom {
        return None;
    }

    let stride = capture.width as usize * 4;
    let mut pixels = Vec::with_capacity((right - left) * (bottom - top) * 4);
    for row in top..bottom {
        let row = &capture.pixels[row * stride..][..stride];
        pixels.extend_from_slice(&row[left * 4..right * 4]);
    }

    Some(Capture {
        width: (right - left) as u32,
        height: (bottom - top) as u32,
        scale: capture.scale,
        screen: None,
        pixels,
    })
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.kde.KWin.ScreenShot2.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    Cancelled(String),
    InvalidScreen(String),
    InvalidWindow(String),
    InvalidArea(String),
    Failed(String),
    NoAuthorized(String),
}

impl From<CaptureError> for Error {
    fn from(err: CaptureError) -> Self {
        match err {
            CaptureError::Locked => Self::NoAuthorized("the screen is locked".to_owned()),
            CaptureError::Cancelled => Self::Cancelled("nothing to capture".to_owned()),
            CaptureError::InvalidScreen => Self::InvalidScreen("no such screen".to_owned()),
            CaptureError::InvalidWindow => Self::InvalidWindow("no such window".to_owned()),
            CaptureError::InvalidArea => Self::InvalidArea("the area is off screen".to_owned()),
            CaptureError::Failed => Self::Failed("internal error".to_owned()),
        }
    }
}

type Options = HashMap<String, OwnedVariant>;
type Results = HashMap<String, OwnedVariant>;

/// The D-Bus object; the scene is what the window manager last published.
pub struct ScreenShot2 {
    scene: Arc<Mutex<Scene>>,
    waker: Option<UnixStream>,
}

#[interface(name = "org.kde.KWin.ScreenShot2")]
impl ScreenShot2 {
    #[zbus(property)]
    fn version(&self) -> u32 {
        4
    }

    fn capture_screen(
        &self,
        name: String,
        options: Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        self.capture(Target::Screen(name), &options, pipe)
    }

    fn capture_active_screen(
        &self,
        options: Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        self.capture(Target::ActiveScreen, &options, pipe)
    }

    fn capture_active_window(
        &self,
        options: Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        self.capture(Target::ActiveWindow, &options, pipe)
    }

    fn capture_window(
        &self,
        handle: String,
        options: Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        let id = {
            let scene = self.scene.lock().map(|s| s.clone()).unwrap_or_default();
            window_by_uuid(&scene, &handle)
                .map(|w| w.id)
                .ok_or(Error::from(CaptureError::InvalidWindow))?
        };
        self.capture(Target::Window(id), &options, pipe)
    }

    /// Captures what is under the pointer: there is no picking step, and `kind` is 0 for a window
    /// and 1 for a screen.
    fn capture_interactive(
        &self,
        kind: u32,
        options: Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        let target = match kind {
            0 => Target::WindowUnderCursor,
            _ => Target::ScreenUnderCursor,
        };
        self.capture(target, &options, pipe)
    }

    fn capture_area(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        options: Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        let target = Target::Area {
            x,
            y,
            width,
            height,
        };
        self.capture(target, &options, pipe)
    }

    fn capture_workspace(&self, options: Options, pipe: zbus::OwnedFd) -> Result<Results, Error> {
        self.capture(Target::Workspace, &options, pipe)
    }
}

impl ScreenShot2 {
    /// river reports the pointer only in manage sequences: asks the window manager for one and
    /// waits (at most [`POINTER_WAIT`]) until the scene holds the position it brought.
    fn fresh_pointer(&self) {
        let Some(waker) = &self.waker else {
            return;
        };
        let seq = match self.scene.lock() {
            Ok(mut s) => {
                s.pointer_asked += 1;
                s.pointer_asked
            }
            Err(_) => return,
        };
        let _ = (&*waker).write(&[1]);
        let deadline = Instant::now() + POINTER_WAIT;
        while Instant::now() < deadline {
            if self.scene.lock().is_ok_and(|s| s.pointer_seq >= seq) {
                return;
            }
            thread::sleep(Duration::from_millis(4));
        }
        debug!("screenshot: no fresh pointer position in {POINTER_WAIT:?}");
    }

    fn capture(
        &self,
        target: Target,
        options: &Options,
        pipe: zbus::OwnedFd,
    ) -> Result<Results, Error> {
        let include_cursor = options
            .get("include-cursor")
            .is_some_and(|value| matches!(value.downcast_ref::<bool>(), Ok(true)));
        if matches!(
            target,
            Target::WindowUnderCursor | Target::ScreenUnderCursor
        ) {
            self.fresh_pointer();
        }
        let scene = self.scene.lock().map(|s| s.clone()).unwrap_or_default();
        let plan = plan(&target, &scene)?;
        let mut cap = Capturer::connect().map_err(|e| {
            warn!("screenshot: {e:#}");
            Error::from(CaptureError::Failed)
        })?;
        let capture = run(&plan, include_cursor, &mut cap)?;
        info!(
            "screenshot {target:?}: {}x{}",
            capture.width, capture.height
        );

        let mut results = HashMap::new();
        let mut insert = |key: &str, value: Variant<'_>| {
            if let Ok(v) = value.try_to_owned() {
                results.insert(key.to_owned(), v);
            }
        };
        insert("type", Variant::from("raw"));
        insert("width", Variant::from(capture.width));
        insert("height", Variant::from(capture.height));
        insert("stride", Variant::from(capture.width * 4));
        insert(
            "format",
            Variant::from(QIMAGE_FORMAT_RGBA8888_PREMULTIPLIED),
        );
        insert("scale", Variant::from(capture.scale));
        if let Some(screen) = &capture.screen {
            insert("screen", Variant::from(screen.as_str()));
        }

        // The caller reads the pipe only after this reply, and a screen does not fit in the pipe's
        // buffer, so the pixels are written from a thread of their own.
        let mut pipe = File::from(std::os::fd::OwnedFd::from(pipe));
        let pixels = capture.pixels;
        thread::Builder::new()
            .name("screenshot writer".to_owned())
            .spawn(move || {
                if let Err(err) = pipe.write_all(&pixels) {
                    warn!("error writing a screenshot to the pipe: {err:?}");
                }
            })
            .map_err(|e| {
                warn!("screenshot writer thread: {e:?}");
                Error::from(CaptureError::Failed)
            })?;
        Ok(results)
    }
}

/// The shown window a KWin uuid names: Spectacle hands out the ext-foreign-toplevel identifier
/// `org.kde.KWin.getWindowInfo` gave it (the one zkde-screencast's `stream_window` takes too),
/// braces or not; a bare newm-river id is accepted as well.
pub fn window_by_uuid<'a>(scene: &'a Scene, uuid: &str) -> Option<&'a SceneWindow> {
    let uuid = uuid.trim_matches(['{', '}']);
    scene
        .windows
        .iter()
        .find(|w| w.identifier.as_deref() == Some(uuid))
        .or_else(|| {
            let id = uuid.parse::<u64>().ok()?;
            scene.windows.iter().find(|w| w.id == id)
        })
}

/// How long a capture under the pointer waits for its position.
const POINTER_WAIT: Duration = Duration::from_millis(200);

/// Serves `org.kde.KWin.ScreenShot2` and the part of `org.kde.KWin` Spectacle reads to name and
/// pick windows (`crate::kwin`) on one session-bus connection, from a thread of its own; logs and
/// gives up when there is no session bus or the names are taken. `waker` wakes the window
/// manager's loop to fetch a fresh pointer.
pub fn serve(
    scene: Arc<Mutex<Scene>>,
    waker: Option<UnixStream>,
    runner: Option<crate::kde::WindowsRunner>,
) {
    let res = thread::Builder::new()
        .name("screenshot2".to_owned())
        .spawn(move || {
            let kwin_waker = waker.as_ref().and_then(|w| w.try_clone().ok());
            // zbus 6 is async only: this thread drives the connection inside zbus::block_on
            // for as long as the window manager runs.
            let served = zbus::block_on(async move {
                let builder = zbus::connection::Builder::session()
                    .serve_at(
                        "/org/kde/KWin/ScreenShot2",
                        ScreenShot2 {
                            scene: scene.clone(),
                            waker,
                        },
                    )
                    .serve_at(
                        "/KWin",
                        crate::kwin::KWin {
                            scene,
                            waker: kwin_waker,
                        },
                    )
                    .name("org.kde.KWin.ScreenShot2")
                    .name("org.kde.KWin");
                // KRunner's window search sits on org.kde.KWin, as KWin serves it (crate::kde).
                let builder = match runner {
                    Some(runner) => builder.serve_at("/WindowsRunner", runner),
                    None => builder,
                };
                let _conn = builder.build().await?;
                info!("serving org.kde.KWin.ScreenShot2");
                std::future::pending::<()>().await;
                Ok::<(), zbus::Error>(())
            });
            if let Err(e) = served {
                warn!("org.kde.KWin.ScreenShot2 not served: {e}");
            }
        });
    if let Err(e) = res {
        warn!("screenshot2 thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(x: i32, y: i32, scale: f64, width: u32, height: u32, fill: u8) -> Part {
        Part {
            x,
            y,
            scale,
            width,
            height,
            pixels: vec![fill; (width * height * 4) as usize],
        }
    }

    fn pixel(capture: &Capture, x: u32, y: u32) -> u8 {
        capture.pixels[((y * capture.width + x) * 4) as usize]
    }

    fn scene() -> Scene {
        let out = |name: &str, x| SceneOutput {
            name: name.into(),
            area: Rect::new(x, 0, 100, 50),
        };
        let win = |id, identifier: Option<&str>, rect| SceneWindow {
            id,
            identifier: identifier.map(Into::into),
            rect,
            title: Some(format!("t{id}")),
            app_id: Some(format!("app{id}")),
        };
        Scene {
            outputs: vec![out("A", 0), out("B", 100)],
            windows: vec![
                win(3, Some("top"), Rect::new(10, 10, 20, 20)),
                win(2, Some("below"), Rect::new(0, 0, 60, 40)),
                win(1, None, Rect::new(120, 0, 10, 10)),
            ],
            focus: Some(2),
            active_output: Some("B".into()),
            pointer: Some((15, 15)),
            locked: false,
            pointer_asked: 0,
            pointer_seq: 0,
            pick_asked: 0,
            picked: None,
        }
    }

    fn window(identifier: &str, rect: Rect) -> Plan {
        Plan::Window {
            identifier: identifier.into(),
            rect,
        }
    }

    #[test]
    fn a_locked_session_refuses_every_capture() {
        let s = Scene {
            locked: true,
            ..scene()
        };
        for target in [
            Target::Screen("A".into()),
            Target::ActiveScreen,
            Target::ScreenUnderCursor,
            Target::ActiveWindow,
            Target::Window(3),
        ] {
            assert_eq!(plan(&target, &s), Err(CaptureError::Locked), "{target:?}");
        }
    }

    #[test]
    fn windows_resolve_to_their_identifier() {
        let s = scene();
        assert_eq!(
            plan(&Target::ActiveWindow, &s),
            Ok(window("below", Rect::new(0, 0, 60, 40)))
        );
        assert_eq!(
            plan(&Target::Window(3), &s),
            Ok(window("top", Rect::new(10, 10, 20, 20)))
        );
        assert_eq!(
            plan(&Target::Window(9), &s),
            Err(CaptureError::InvalidWindow)
        );
        assert_eq!(
            plan(&Target::Window(1), &s),
            Err(CaptureError::InvalidWindow)
        );
    }

    #[test]
    fn under_the_pointer_takes_the_topmost_window_and_its_screen() {
        let mut s = scene();
        assert_eq!(
            plan(&Target::WindowUnderCursor, &s),
            Ok(window("top", Rect::new(10, 10, 20, 20)))
        );
        s.pointer = Some((150, 40));
        let Ok(Plan::Screens { outputs, join }) = plan(&Target::ScreenUnderCursor, &s) else {
            panic!("no screen");
        };
        assert_eq!((outputs[0].name.as_str(), join), ("B", Join::One));
        assert_eq!(
            plan(&Target::WindowUnderCursor, &s),
            Err(CaptureError::Cancelled)
        );
        s.pointer = None;
        assert_eq!(
            plan(&Target::ScreenUnderCursor, &s),
            Err(CaptureError::Cancelled)
        );
    }

    #[test]
    fn screens_resolve_by_name_activity_or_all() {
        let s = scene();
        let names = |p: Result<Plan, CaptureError>| match p {
            Ok(Plan::Screens { outputs, join }) => (
                outputs.into_iter().map(|o| o.name).collect::<Vec<_>>(),
                join,
            ),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            names(plan(&Target::Screen("A".into()), &s)),
            (vec!["A".into()], Join::One)
        );
        assert_eq!(
            names(plan(&Target::ActiveScreen, &s)),
            (vec!["B".into()], Join::One)
        );
        assert_eq!(
            names(plan(&Target::Workspace, &s)),
            (vec!["A".into(), "B".into()], Join::All)
        );
        let area = Target::Area {
            x: 90,
            y: 5,
            width: 20,
            height: 10,
        };
        assert_eq!(
            names(plan(&area, &s)).1,
            Join::Area {
                x: 90,
                y: 5,
                width: 20,
                height: 10
            }
        );
        assert_eq!(
            plan(&Target::Screen("C".into()), &s),
            Err(CaptureError::InvalidScreen)
        );
    }

    #[test]
    fn scales_come_from_pixels_over_logical_size() {
        assert_eq!(scale_of(2000, 1000), 2.);
        assert_eq!(scale_of(1500, 1000), 1.5);
        assert_eq!(scale_of(100, 0), 1.);
    }

    #[test]
    fn one_output_is_returned_as_is() {
        let (x, y, capture) = composite(vec![part(10, 20, 1.5, 3, 2, 7)]).unwrap();
        assert_eq!((x, y), (10, 20));
        assert_eq!((capture.width, capture.height, capture.scale), (3, 2, 1.5));
        assert_eq!(capture.pixels, vec![7; 24]);
    }

    #[test]
    fn outputs_side_by_side_keep_their_places() {
        let parts = vec![part(0, 0, 1., 4, 2, 1), part(4, 0, 1., 2, 3, 2)];
        let (x, y, capture) = composite(parts).unwrap();
        assert_eq!((x, y), (0, 0));
        assert_eq!((capture.width, capture.height), (6, 3));
        assert_eq!(pixel(&capture, 3, 1), 1);
        assert_eq!(pixel(&capture, 4, 2), 2);
        // Below the shorter output nothing is drawn.
        assert_eq!(pixel(&capture, 0, 2), 0);
    }

    #[test]
    fn a_lower_scale_output_is_stretched_to_the_largest_scale() {
        // 2x2 logical at scale 2 to the left of 2x2 logical at scale 1.
        let parts = vec![part(-2, 0, 2., 4, 4, 1), part(0, 0, 1., 2, 2, 2)];
        let (x, y, capture) = composite(parts).unwrap();
        assert_eq!((x, y), (-2, 0));
        assert_eq!((capture.width, capture.height, capture.scale), (8, 4, 2.));
        assert_eq!(pixel(&capture, 3, 3), 1);
        assert_eq!(pixel(&capture, 4, 0), 2);
        assert_eq!(pixel(&capture, 7, 3), 2);
    }

    #[test]
    fn crop_cuts_a_logical_rectangle_in_pixels() {
        let (_, _, capture) = composite(vec![part(0, 0, 2., 8, 8, 0)]).unwrap();
        let mut capture = capture;
        capture.pixels[(2 * 8 + 4) * 4] = 9;
        let cut = crop(capture, 2, 1, 2, 2).unwrap();
        assert_eq!((cut.width, cut.height, cut.scale), (4, 4, 2.));
        assert_eq!(pixel(&cut, 0, 0), 9);
    }

    #[test]
    fn crop_takes_any_area_a_caller_names() {
        let (_, _, capture) = composite(vec![part(100, 0, 1., 4, 4, 3)]).unwrap();
        let x = i64::from(i32::MIN) - 100;
        assert_eq!(
            crop(capture.clone(), x, 0, u32::MAX, 2).map(|c| c.width),
            Some(4)
        );
        assert_eq!(
            crop(capture, i64::from(i32::MAX) - 100, 0, u32::MAX, 2),
            None
        );
    }

    #[test]
    fn crop_clips_to_the_image_and_refuses_a_miss() {
        let (_, _, capture) = composite(vec![part(0, 0, 1., 4, 4, 3)]).unwrap();
        let cut = crop(capture.clone(), -2, 2, 4, 10).unwrap();
        assert_eq!((cut.width, cut.height), (2, 2));
        assert_eq!(crop(capture.clone(), 4, 0, 2, 2), None);
        assert_eq!(crop(capture, 0, 0, 0, 2), None);
    }

    #[test]
    fn a_window_is_found_by_its_identifier_or_its_id() {
        let s = scene();
        let top = &s.windows[0];
        assert_eq!(window_by_uuid(&s, "{top}").map(|w| w.id), Some(top.id));
        assert_eq!(
            window_by_uuid(&s, &top.id.to_string()).map(|w| w.id),
            Some(top.id)
        );
        assert!(window_by_uuid(&s, "{not-a-window}").is_none());
        assert!(window_by_uuid(&s, "999").is_none());
    }

    #[test]
    fn window_info_carries_what_spectacle_reads() {
        let s = scene();
        let id = s.windows[0].id;
        let info = crate::kwin::get(&s, "{top}").unwrap();
        let string = |k: &str| String::try_from(info[k].try_clone().unwrap()).unwrap();
        assert_eq!(string("uuid"), "top");
        assert_eq!(string("caption"), format!("t{id}"));
        assert_eq!(string("desktopFile"), format!("app{id}"));
        assert_eq!(
            f64::try_from(info["width"].try_clone().unwrap()).unwrap(),
            f64::from(s.windows[0].rect.w)
        );
        assert!(crate::kwin::get(&s, "{not-a-window}").is_err());
    }
}
