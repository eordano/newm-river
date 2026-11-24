//! The model's side of launch placeholders and session restore: reserving tiles for a launching
//! app or a saved window, a new window taking them, and expiry. The decisions live in
//! [`crate::placeholder`]; this applies them to the canvases.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use newm_canvas::Id;
use newm_kde::apps;
use serde_json::{json, Value};

use super::usage::now_ms;
use super::{Desktop, OutputId, Rect, Slot, Window, REMATCH_FOR, TITLE_MATCH};
use crate::placeholder::{self, Placeholder, RestoreCanvas};
use crate::rules::Resolved;

/// A placeholder in a render sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaceholderFrame {
    pub id: Id,
    /// Its box on screen, through the camera.
    pub rect: Rect,
    /// Its output's area: the box is drawn clipped to it.
    pub area: Rect,
    pub icon: Option<PathBuf>,
    /// The camera scale it is drawn at: its corners round with it.
    pub scale: f64,
}

impl Desktop {
    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
    }

    /// `launch-placeholder`: reserves the spot the next window would take on the active canvas,
    /// at the span the default size the app ID's rules give asks for.
    pub(super) fn launch_placeholder(
        &mut self,
        app_id: String,
        icon: Option<String>,
        now: Instant,
    ) {
        self.sync_screens();
        let Some(out) = self.active_canvas_output() else {
            return;
        };
        let (default_width, default_height) = placeholder::rule_size(&self.rules, &app_id);
        let resolved = Resolved {
            default_width,
            default_height,
            ..Resolved::default()
        };
        let Some(o) = self.output(out) else {
            return;
        };
        let want = self.wanted_span(&o.canvas, &resolved);
        let id = self.placeholders.alloc();
        self.apply(Some(out), |c| placeholder::reserve(c, id, want));
        self.placeholders.push(Placeholder {
            id,
            output: out,
            app_id,
            title: None,
            icon: icon.map(PathBuf::from),
            deadline: now + placeholder::LIFETIME,
            created: now,
        });
    }

    /// Puts a saved canvas back on the output with this name (else the active one): a
    /// placeholder on each window's saved tiles, or the free ones nearest them. Once none waits,
    /// because windows took them or `lifetime` passed, the canvas takes the saved focus, zoom and
    /// view. Until [`REMATCH_FOR`] passed, a retitled window may still trade spots
    /// (`model/rematch.rs`).
    pub fn restore(
        &mut self,
        output: Option<&str>,
        saved: RestoreCanvas,
        lifetime: Duration,
        now: Instant,
    ) {
        // Outside a manage sequence the canvases may not have their screens yet, and a span is
        // cut to what the screen holds.
        self.sync_screens();
        let named = self
            .outputs
            .iter()
            .find(|o| output.is_some_and(|n| n == o.name))
            .map(|o| o.id);
        let Some(out) = named.or_else(|| self.active_canvas_output()) else {
            return;
        };
        let ids: Vec<Id> = saved
            .windows
            .iter()
            .map(|_| self.placeholders.alloc())
            .collect();
        self.apply(Some(out), |c| {
            placeholder::reserve_saved(c, &ids, &saved.windows);
        });
        let deadline = now + lifetime;
        for (&id, w) in ids.iter().zip(&saved.windows) {
            self.placeholders.push(Placeholder {
                id,
                output: out,
                app_id: w.app_id.clone(),
                title: w.title.clone(),
                icon: None,
                deadline,
                created: now,
            });
        }
        let rematch_until = now + lifetime.max(REMATCH_FOR);
        self.placeholders
            .start_restore(out, ids, &saved, deadline, rematch_until);
        self.finish_restores(now);
    }

    /// The placeholder a new window takes, if one waits for its app: its id, its output and,
    /// for the usage log, whether the title or only the app matched and how long it waited. The
    /// window keeps the spot's saved app and title while its own title may still move it.
    pub(super) fn claim_placeholder(
        &mut self,
        id: Id,
        now: Instant,
    ) -> Option<(Id, OutputId, Value)> {
        let w = self.windows.get(&id)?;
        let app_id = w.app_id.as_deref()?;
        let title = w.title.clone();
        let (p, score) = self.placeholders.claim(app_id, title.as_deref(), id)?;
        self.output(p.output)?;
        let matched_by = if score >= TITLE_MATCH { "title" } else { "app" };
        if let Some(w) = self.windows.get_mut(&id) {
            w.slot = Some(Slot {
                app_id: p.app_id.clone(),
                title: p.title.clone(),
            });
        }
        let waited_ms = now.saturating_duration_since(p.created).as_millis() as u64;
        let restore = json!({ "matched_by": matched_by, "waited_ms": waited_ms });
        Some((p.id, p.output, restore))
    }

    /// Frees the tiles of placeholders whose time passed and ends finished restores. Returns
    /// whether anything changed.
    pub(super) fn tick_placeholders(&mut self, now: Instant) -> bool {
        let expired = self.placeholders.expire(now);
        for p in &expired {
            self.emit(json!({ "t": now_ms(), "op": "restore-expired", "app": apps::canonical(&p.app_id) }));
            self.apply(Some(p.output), |c| c.remove(p.id));
        }
        let ids: Vec<Id> = expired.iter().map(|p| p.id).collect();
        self.block_slots_expired(&ids);
        let settled = self.settle_blocks(now);
        self.finish_restores(now) || !expired.is_empty() || settled
    }

    /// The canvases whose restore ended take the saved focus, zoom and view (at 1:1), moved the
    /// least they must to show the focus.
    pub(super) fn finish_restores(&mut self, now: Instant) -> bool {
        let done = self.placeholders.finish(now);
        for f in &done {
            self.apply(Some(f.output), |c| {
                if let Some(focus) = f.focus {
                    c.focus_id(focus);
                }
                c.zoom_by(f.zoom - c.zoom());
                let s = c.scale();
                c.set_view(f.view.0 * s, f.view.1 * s);
                c.ensure_focus_visible();
            });
            if let Some(focus) = f.focus.filter(|f| self.windows.contains_key(f)) {
                self.focus_window(focus, now);
            }
        }
        !done.is_empty()
    }

    /// An output left: its placeholders go with it.
    pub(super) fn drop_placeholders(&mut self, output: OutputId) {
        self.placeholders.drop_output(output);
    }

    /// Where each placeholder is drawn: its cell through its canvas's camera.
    pub fn placeholder_frames(&self) -> Vec<PlaceholderFrame> {
        self.placeholders
            .iter()
            .filter_map(|p| {
                let o = self.output(p.output)?;
                let (rect, scale) = self.cell_on_screen(o, o.canvas.rect(p.id)?);
                Some(PlaceholderFrame {
                    id: p.id,
                    rect,
                    area: o.area,
                    icon: p.icon.clone(),
                    scale,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use newm_canvas::{Config as CanvasConfig, Rect as Tiles};
    use niri_config::{Action, PresetSize};

    use super::super::Settings;
    use super::*;
    use crate::placeholder::RestoreWindow;

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            default_width: Some(PresetSize::Fixed(960)),
            default_height: Some(PresetSize::Proportion(0.6667)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        (d, now)
    }

    /// A window of `app_id` opens as river would: a manage sequence, the size it was asked for
    /// committed.
    fn open(d: &mut Desktop, id: Id, app_id: &str, title: &str, now: Instant) -> Tiles {
        d.add_window(id);
        let w = d.window_mut(id).unwrap();
        w.app_id = Some(app_id.into());
        w.title = Some(title.into());
        d.manage(now);
        for (id, size) in d.proposals() {
            d.set_asked(id, size);
            d.on_dimensions(id, size.0, size.1);
        }
        d.output(1).unwrap().canvas.rect(id).unwrap()
    }

    fn launch(d: &mut Desktop, app_id: &str, icon: Option<&str>, now: Instant) {
        let action = Action::LaunchPlaceholder {
            app_id: app_id.into(),
            icon: icon.map(Into::into),
        };
        assert!(d.act(action, now).is_empty());
    }

    #[test]
    fn the_next_window_of_the_app_takes_the_placeholder() {
        let (mut d, now) = desktop();
        launch(&mut d, "foot", None, now);
        let frames = d.placeholder_frames();
        assert_eq!(frames.len(), 1);
        let held = d.outputs()[0].canvas.rect(frames[0].id).unwrap();
        assert_eq!((held.w, held.h), (8, 6), "the default size's span");
        assert_eq!(frames[0].rect, Rect::new(10, 10, 942, 668));

        assert_ne!(
            open(&mut d, 1, "kitty", "", now),
            held,
            "another app opens beside it"
        );
        assert_eq!(
            open(&mut d, 2, "foot", "", now),
            held,
            "foot takes its tiles"
        );
        assert!(d.placeholder_frames().is_empty());
        assert_eq!(d.focus(), Some(2));
    }

    #[test]
    fn a_placeholder_frees_its_tiles_after_its_lifetime() {
        let (mut d, now) = desktop();
        launch(&mut d, "never", Some("/icon.png"), now);
        assert_eq!(
            d.placeholder_frames()[0].icon,
            Some(PathBuf::from("/icon.png"))
        );
        assert_eq!(d.deadline(), Some(now + placeholder::LIFETIME));
        assert!(!d.tick(now + Duration::from_secs(9)));
        assert!(d.tick(now + placeholder::LIFETIME));
        assert!(d.outputs()[0].canvas.is_empty());
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn a_saved_canvas_comes_back_on_its_tiles_with_its_focus() {
        let (mut d, now) = desktop();
        let saved = RestoreCanvas {
            windows: vec![
                RestoreWindow {
                    app_id: "foot".into(),
                    title: Some("a".into()),
                    rect: Tiles::new(3, 1, 4, 2),
                },
                RestoreWindow {
                    app_id: "foot".into(),
                    title: Some("b".into()),
                    rect: Tiles::new(7, 1, 4, 2),
                },
            ],
            zoom: 0,
            view: (0., 0.),
            focus: Some(1),
        };
        d.restore(Some("OUT-1"), saved, Duration::from_secs(20), now);
        assert!(d.placeholders.is_restoring());
        assert_eq!(d.focus(), None, "nothing to focus before the windows come");
        let b = open(&mut d, 1, "foot", "b", now);
        let a = open(&mut d, 2, "foot", "a", now);
        assert_eq!(b, Tiles::new(7, 1, 4, 2), "the title picks the tiles");
        assert_eq!(a, Tiles::new(3, 1, 4, 2));
        assert!(!d.placeholders.is_restoring());
        assert_eq!(d.focus(), Some(1), "the saved focus");
    }

    /// A restore matches an app whatever the case of its id: a session saved
    /// where an app runs under X11 restores where it runs on Wayland.
    #[test]
    fn a_restore_matches_an_app_whatever_the_case_of_its_id() {
        let (mut d, now) = desktop();
        let saved = RestoreCanvas {
            windows: vec![
                RestoreWindow {
                    app_id: "Chromium-browser".into(),
                    title: Some("news".into()),
                    rect: Tiles::new(3, 1, 4, 2),
                },
                RestoreWindow {
                    app_id: "com.mitchellh.ghostty".into(),
                    title: Some("a".into()),
                    rect: Tiles::new(7, 1, 4, 2),
                },
            ],
            zoom: 0,
            view: (0., 0.),
            focus: Some(0),
        };
        d.restore(Some("OUT-1"), saved, Duration::from_secs(20), now);
        let browser = open(&mut d, 1, "chromium-browser", "news", now);
        let term = open(&mut d, 2, "COM.MITCHELLH.GHOSTTY", "a", now);
        assert_eq!(
            browser,
            Tiles::new(3, 1, 4, 2),
            "the Wayland Chromium takes the X11 spot"
        );
        assert_eq!(term, Tiles::new(7, 1, 4, 2));
        assert!(!d.placeholders.is_restoring(), "no placeholder waits");
    }

    /// A window manager started after a crash is handed the running session's windows at once
    /// (supervise.rs): held back, they wait for the saved canvas, then take their saved spots by
    /// title, the saved focus with them.
    #[test]
    fn held_windows_of_a_running_session_take_their_saved_spots() {
        let (mut d, now) = desktop();
        d.hold_new_windows(now + Duration::from_secs(3));
        for (id, title) in [(1, "b"), (2, "a")] {
            d.add_window(id);
            let w = d.window_mut(id).unwrap();
            w.app_id = Some("foot".into());
            w.title = Some(title.into());
        }
        // A window of the session that was never saved (an excluded app, one opened just before
        // the crash).
        d.add_window(3);
        d.window_mut(3).unwrap().app_id = Some("unsaved".into());
        d.manage(now);
        assert!(d.window(1).unwrap().place.is_none(), "held back");
        assert!(d.outputs()[0].canvas.is_empty());
        assert_eq!(d.deadline(), Some(now + Duration::from_secs(3)));
        let saved = RestoreCanvas {
            windows: vec![
                RestoreWindow {
                    app_id: "foot".into(),
                    title: Some("a".into()),
                    rect: Tiles::new(3, 1, 4, 2),
                },
                RestoreWindow {
                    app_id: "foot".into(),
                    title: Some("b".into()),
                    rect: Tiles::new(7, 1, 4, 2),
                },
            ],
            zoom: 0,
            view: (0., 0.),
            focus: Some(1),
        };
        d.restore(Some("OUT-1"), saved, Duration::from_secs(20), now);
        d.release_new_windows();
        d.manage(now);
        for (id, size) in d.proposals() {
            d.set_asked(id, size);
            d.on_dimensions(id, size.0, size.1);
        }
        let rect = |d: &Desktop, id| d.output(1).unwrap().canvas.rect(id).unwrap();
        assert_eq!(rect(&d, 1), Tiles::new(7, 1, 4, 2));
        assert_eq!(rect(&d, 2), Tiles::new(3, 1, 4, 2));
        assert!(!d.placeholders.is_restoring());
        assert!(
            d.output(1).unwrap().canvas.rect(3).is_some(),
            "the unsaved window opens"
        );
        assert_eq!(
            d.focus(),
            Some(1),
            "the saved focus, not the unsaved window's"
        );
    }

    /// Without a saved canvas the hold ends on its own and the windows open as new ones.
    #[test]
    fn the_hold_ends_at_its_deadline() {
        let (mut d, now) = desktop();
        let until = now + Duration::from_secs(3);
        d.hold_new_windows(until);
        d.add_window(1);
        d.window_mut(1).unwrap().app_id = Some("foot".into());
        d.manage(now);
        assert!(!d.tick(now + Duration::from_secs(1)));
        assert!(d.window(1).unwrap().place.is_none());
        assert!(d.tick(until), "the hold's end asks for a manage sequence");
        d.manage(until);
        assert!(d.window(1).unwrap().place.is_some());
    }
}
