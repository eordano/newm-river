//! The part of KWin's own D-Bus interface (`org.kde.KWin` at `/KWin`) Spectacle calls to pick a
//! window to record: `queryWindowInfo` and `getWindowInfo`. A window's uuid is its
//! ext-foreign-toplevel-list identifier, which `zkde_screencast_unstable_v1.stream_window` takes
//! back (`screencast.rs`).
//!
//! `queryWindowInfo` picks with a pick grab: the window manager takes the
//! pointer, the next click picks the window under it (or, on no window, its output, enough for a
//! screen recording) and Escape cancels (`UserCancel`). Without the window manager's loop (tests)
//! it answers the window under the pointer, else the focused one. Both refuse while the session
//! is locked.
//! `crate::screenshot::serve` serves it beside ScreenShot2 on one connection, which owns the
//! `org.kde.KWin` name; ScreenShot2's `CaptureWindow` takes the same uuid.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use zbus::interface;
use zbus::{OwnedVariant, Variant};

use crate::screenshot::{self, Picked, Plan, Scene, SceneOutput, SceneWindow, Target};

/// How long `queryWindowInfo` waits for the pick's click.
const PICK_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.kde.KWin.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    UserCancel(String),
    InvalidWindow(String),
}

type Info = HashMap<String, OwnedVariant>;

/// KWin's window info map for a window: its uuid is the ext-foreign-toplevel identifier (else
/// its newm-river id), with what Spectacle reads to name it.
pub fn info(w: &SceneWindow) -> Info {
    let mut map = Info::new();
    let mut insert = |key: &str, value: Variant| {
        if let Ok(v) = value.try_to_owned() {
            map.insert(key.to_owned(), v);
        }
    };
    let uuid = w.identifier.clone().unwrap_or_else(|| w.id.to_string());
    let app = w.app_id.clone().unwrap_or_default();
    let rect = w.rect;
    insert("uuid", Variant::from(uuid));
    insert(
        "caption",
        Variant::from(w.title.clone().unwrap_or_default()),
    );
    insert("resourceClass", Variant::from(app.clone()));
    insert("resourceName", Variant::from(app.clone()));
    insert("desktopFile", Variant::from(app));
    insert("x", Variant::from(f64::from(rect.x)));
    insert("y", Variant::from(f64::from(rect.y)));
    insert("width", Variant::from(f64::from(rect.w)));
    insert("height", Variant::from(f64::from(rect.h)));
    map
}

/// The window `queryWindowInfo` answers: under the pointer, else focused.
pub fn query(scene: &Scene) -> Result<Info, Error> {
    if scene.locked {
        return Err(Error::UserCancel("the screen is locked".to_owned()));
    }
    let picked = screenshot::plan(&Target::WindowUnderCursor, scene)
        .or_else(|_| screenshot::plan(&Target::ActiveWindow, scene));
    match picked {
        Ok(Plan::Window { identifier, .. }) => screenshot::window_by_uuid(scene, &identifier)
            .map(info)
            .ok_or_else(|| Error::UserCancel("no window was picked".to_owned())),
        _ => Err(Error::UserCancel("no window was picked".to_owned())),
    }
}

/// KWin's info map for a click on no window: the output there, with no uuid.
fn output_info(o: &SceneOutput) -> Info {
    let mut map = Info::new();
    let mut insert = |key: &str, value: Variant| {
        if let Ok(v) = value.try_to_owned() {
            map.insert(key.to_owned(), v);
        }
    };
    insert("caption", Variant::from(o.name.clone()));
    insert("x", Variant::from(f64::from(o.area.x)));
    insert("y", Variant::from(f64::from(o.area.y)));
    insert("width", Variant::from(f64::from(o.area.w)));
    insert("height", Variant::from(f64::from(o.area.h)));
    map
}

/// What a pick's click gives `queryWindowInfo`.
pub fn picked(scene: &Scene, p: &Picked) -> Result<Info, Error> {
    let gone = || Error::UserCancel("the picked window or output is gone".to_owned());
    match p {
        Picked::Window(id) => scene.windows.iter().find(|w| w.id == *id).map(info),
        Picked::Output(name) => scene
            .outputs
            .iter()
            .find(|o| &o.name == name)
            .map(output_info),
        Picked::Nothing => return Err(Error::UserCancel("the pick was cancelled".to_owned())),
    }
    .ok_or_else(gone)
}

/// The window a uuid names.
pub fn get(scene: &Scene, uuid: &str) -> Result<Info, Error> {
    if scene.locked {
        return Err(Error::InvalidWindow("the screen is locked".to_owned()));
    }
    screenshot::window_by_uuid(scene, uuid)
        .map(info)
        .ok_or_else(|| Error::InvalidWindow(format!("no window {uuid}")))
}

pub(crate) struct KWin {
    pub(crate) scene: Arc<Mutex<Scene>>,
    /// Wakes the window manager's loop to start a pick.
    pub(crate) waker: Option<UnixStream>,
}

impl KWin {
    fn scene(&self) -> Scene {
        self.scene.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Asks the window manager for a pick and waits for its click (at most [`PICK_WAIT`]).
    fn pick(&self, waker: &UnixStream) -> Result<Info, Error> {
        let seq = {
            let mut s = self
                .scene
                .lock()
                .map_err(|_| Error::UserCancel("no scene".to_owned()))?;
            s.pick_asked += 1;
            s.pick_asked
        };
        let _ = (&*waker).write(&[1]);
        let deadline = Instant::now() + PICK_WAIT;
        while Instant::now() < deadline {
            if let Ok(mut s) = self.scene.lock()
                && s.picked.as_ref().is_some_and(|(n, _)| *n >= seq)
                && let Some((_, p)) = s.picked.take()
            {
                return picked(&s, &p);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(Error::UserCancel("no window was picked in time".to_owned()))
    }
}

#[interface(name = "org.kde.KWin")]
impl KWin {
    #[zbus(name = "queryWindowInfo")]
    fn query_window_info(&self) -> Result<Info, Error> {
        let scene = self.scene();
        match &self.waker {
            Some(waker) if !scene.locked => self.pick(waker),
            _ => query(&scene),
        }
    }

    #[zbus(name = "getWindowInfo")]
    fn get_window_info(&self, uuid: String) -> Result<Info, Error> {
        get(&self.scene(), &uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rect;
    use crate::screenshot::SceneWindow;

    #[test]
    fn a_pick_answers_the_window_clicked_or_its_output_and_a_cancel_refuses() {
        let mut s = scene();
        s.outputs.push(SceneOutput {
            name: "HEADLESS-1".into(),
            area: Rect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
        });
        let info = picked(&s, &Picked::Window(2)).unwrap();
        let uuid = String::try_from(info["uuid"].try_clone().unwrap()).unwrap();
        assert_eq!(uuid, "focused");
        let info = picked(&s, &Picked::Output("HEADLESS-1".into())).unwrap();
        let caption = String::try_from(info["caption"].try_clone().unwrap()).unwrap();
        assert_eq!(caption, "HEADLESS-1");
        assert!(!info.contains_key("uuid"));
        assert!(matches!(
            picked(&s, &Picked::Nothing),
            Err(Error::UserCancel(_))
        ));
    }

    fn scene() -> Scene {
        Scene {
            windows: vec![
                SceneWindow {
                    id: 1,
                    identifier: Some("top".into()),
                    rect: Rect {
                        x: 0,
                        y: 0,
                        w: 10,
                        h: 10,
                    },
                    title: None,
                    app_id: None,
                },
                SceneWindow {
                    id: 2,
                    identifier: Some("focused".into()),
                    rect: Rect {
                        x: 50,
                        y: 0,
                        w: 10,
                        h: 10,
                    },
                    title: None,
                    app_id: None,
                },
            ],
            focus: Some(2),
            pointer: Some((5, 5)),
            ..Scene::default()
        }
    }

    fn uuid(info: &Info) -> String {
        String::try_from(info["uuid"].try_clone().unwrap()).unwrap()
    }

    #[test]
    fn a_query_answers_the_window_under_the_pointer_else_the_focused_one() {
        assert_eq!(uuid(&query(&scene()).unwrap()), "top");
        let mut s = scene();
        s.pointer = Some((500, 500));
        assert_eq!(uuid(&query(&s).unwrap()), "focused");
        s.focus = None;
        assert!(query(&s).is_err());
    }

    #[test]
    fn window_info_is_found_by_uuid_and_refused_while_locked() {
        assert_eq!(uuid(&get(&scene(), "{focused}").unwrap()), "focused");
        assert!(get(&scene(), "nope").is_err());
        let mut s = scene();
        s.locked = true;
        assert!(get(&s, "top").is_err());
        assert!(query(&s).is_err());
    }
}
