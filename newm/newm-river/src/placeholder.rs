//! Launch placeholders and the canvas's reservations:
//! a spot reserved on a canvas where a launching app's window will open, drawn as the app's icon
//! centred on a translucent rounded box (no text), until a window with that app ID maps there
//! (it takes the tiles) or [`LIFETIME`] passes. A restored session puts its windows back through
//! placeholders on their saved tiles, which wait [`crate::session::RESTORE_LIFETIME`].
//!
//! Wayland-free: the model reserves and claims, `shell.rs` draws what [`paint`] gives.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use newm_canvas::{Canvas, Id, Rect as Tiles};
use newm_kde::apps;
use niri_config::window_rule::{Match, WindowRule};
use niri_config::PresetSize;

use crate::model::{title_score, OutputId};

/// How long a placeholder waits for its window.
pub const LIFETIME: Duration = Duration::from_secs(10);

/// Placeholder ids start far above river's window ids, so both share a canvas.
pub const FIRST_ID: Id = 1 << 48;

/// The box's corner radius, in logical pixels at 1:1.
pub const RADIUS: f64 = 12.;

/// The default width and height rules give a window (`Some(None)`: no size asked).
pub type RuleSize = (Option<Option<PresetSize>>, Option<Option<PresetSize>>);

#[derive(Clone, Debug, PartialEq)]
pub struct Placeholder {
    pub id: Id,
    pub output: OutputId,
    pub app_id: String,
    /// The title of the window a saved session puts back here: of the windows of its app, the
    /// one whose title fits it best takes it ([`title_score`]).
    pub title: Option<String>,
    pub icon: Option<PathBuf>,
    pub deadline: Instant,
    /// When it was put down, for the usage log's wait until a window took it.
    pub created: Instant,
}

/// A saved canvas being put back.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreCanvas {
    pub windows: Vec<RestoreWindow>,
    pub zoom: i32,
    /// The camera's view at 1:1.
    pub view: (f64, f64),
    /// Index of the focused window in `windows`.
    pub focus: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestoreWindow {
    pub app_id: String,
    pub title: Option<String>,
    pub rect: Tiles,
}

/// A canvas being put back: the placeholders still waiting, and the focus and camera it takes
/// once none waits.
#[derive(Clone, Debug, PartialEq)]
struct Restore {
    output: OutputId,
    waiting: Vec<Id>,
    focus: Option<Id>,
    zoom: i32,
    view: (f64, f64),
    deadline: Instant,
}

/// A restore that ended: the focus and camera its canvas takes now.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub output: OutputId,
    pub focus: Option<Id>,
    pub zoom: i32,
    pub view: (f64, f64),
}

#[derive(Debug)]
pub struct Placeholders {
    list: Vec<Placeholder>,
    restores: Vec<Restore>,
    next: Id,
    /// Until when restored windows may trade spots as their titles change (`model/rematch.rs`).
    /// One for every output: a session puts all its canvases back at once, one launch may open
    /// windows on several of them, and a trade between two outputs is open as long as the
    /// restore is, not as long as one canvas's.
    rematch_until: Option<Instant>,
}

impl Default for Placeholders {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            restores: Vec::new(),
            next: FIRST_ID,
            rematch_until: None,
        }
    }
}

impl Placeholders {
    pub fn iter(&self) -> impl Iterator<Item = &Placeholder> {
        self.list.iter()
    }

    /// Whether a saved canvas is being put back: some of its placeholders wait for windows.
    pub fn is_restoring(&self) -> bool {
        !self.restores.is_empty()
    }

    pub fn alloc(&mut self) -> Id {
        let id = self.next;
        self.next += 1;
        id
    }

    pub fn push(&mut self, p: Placeholder) {
        self.list.push(p);
    }

    /// Records a restore of `ids` (placeholders already pushed) on `output`, its windows trading
    /// spots by title until `rematch_until`. A second saved canvas on the same output keeps the
    /// first one's camera.
    pub fn start_restore(
        &mut self,
        output: OutputId,
        ids: Vec<Id>,
        saved: &RestoreCanvas,
        deadline: Instant,
        rematch_until: Instant,
    ) {
        self.rematch_until = self.rematch_until.max(Some(rematch_until));
        let focus = saved.focus.and_then(|k| ids.get(k).copied());
        if let Some(r) = self.restores.iter_mut().find(|r| r.output == output) {
            r.waiting.extend(ids);
            r.focus = r.focus.or(focus);
            r.deadline = r.deadline.max(deadline);
        } else {
            self.restores.push(Restore {
                output,
                waiting: ids,
                focus,
                zoom: saved.zoom,
                view: saved.view,
                deadline,
            });
        }
    }

    /// The placeholder a new window with this app ID and title takes, removed, with how well the
    /// titles fit: of its app's (as [`apps::canonical`] names it), the one whose saved title fits
    /// its title best, the oldest of equals. A restore waiting for it waits no more, and focuses
    /// `window` where it would have focused the placeholder.
    pub fn claim(
        &mut self,
        app_id: &str,
        title: Option<&str>,
        window: Id,
    ) -> Option<(Placeholder, f64)> {
        let (id, score) = self
            .list
            .iter()
            .filter(|p| apps::same(&p.app_id, app_id))
            .map(|p| (p.id, title_score(p.title.as_deref(), title)))
            .fold(None, |best: Option<(Id, f64)>, (id, s)| match best {
                Some((_, b)) if b >= s => best,
                _ => Some((id, s)),
            })?;
        Some((self.take(id, window)?, score))
    }

    /// Placeholder `id`, removed: a restore waiting for it waits no more, and focuses `window`
    /// where it would have focused the placeholder.
    pub fn take(&mut self, id: Id, window: Id) -> Option<Placeholder> {
        let k = self.list.iter().position(|p| p.id == id)?;
        let p = self.list.remove(k);
        for r in &mut self.restores {
            r.waiting.retain(|&w| w != p.id);
            if r.focus == Some(p.id) {
                r.focus = Some(window);
            }
        }
        Some(p)
    }

    pub fn get_mut(&mut self, id: Id) -> Option<&mut Placeholder> {
        self.list.iter_mut().find(|p| p.id == id)
    }

    /// Placeholder `id` went to `output`'s canvas (a trade across outputs): the restore of the
    /// canvas it left waits for it no more, and the one of the canvas it joined, if that still
    /// runs, waits for it instead.
    pub fn rehome(&mut self, id: Id, output: OutputId) {
        let Some(p) = self.get_mut(id).filter(|p| p.output != output) else {
            return;
        };
        p.output = output;
        let mut waited = false;
        for r in &mut self.restores {
            let n = r.waiting.len();
            r.waiting.retain(|&w| w != id);
            waited |= r.waiting.len() != n;
        }
        if waited && let Some(r) = self.restores.iter_mut().find(|r| r.output == output) {
            r.waiting.push(id);
        }
    }

    /// Whether restored windows may still trade spots as their titles change; once that time
    /// passed, never again until the next restore.
    pub fn rematching(&mut self, now: Instant) -> bool {
        if self.rematch_until.is_none_or(|until| now > until) {
            self.rematch_until = None;
            return false;
        }
        true
    }

    /// The saved focus follows the saved spot: when `a` and `b` traded spots, a restore that
    /// focuses one focuses the other.
    pub fn retarget_focus(&mut self, a: Id, b: Id) {
        for r in &mut self.restores {
            if r.focus == Some(a) {
                r.focus = Some(b);
            } else if r.focus == Some(b) {
                r.focus = Some(a);
            }
        }
    }

    /// The placeholders whose deadline passed, removed (their tiles are freed by the caller).
    pub fn expire(&mut self, now: Instant) -> Vec<Placeholder> {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.list)
            .into_iter()
            .partition(|p| p.deadline <= now);
        self.list = kept;
        for r in &mut self.restores {
            r.waiting.retain(|id| !gone.iter().any(|p| p.id == *id));
        }
        gone
    }

    /// The placeholders of an output that left, removed.
    pub fn drop_output(&mut self, output: OutputId) -> Vec<Placeholder> {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.list)
            .into_iter()
            .partition(|p| p.output == output);
        self.list = kept;
        self.restores.retain(|r| r.output != output);
        gone
    }

    /// Ends the restores no placeholder waits for any more, or whose deadline passed.
    pub fn finish(&mut self, now: Instant) -> Vec<Finished> {
        let (done, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.restores)
            .into_iter()
            .partition(|r| r.waiting.is_empty() || r.deadline <= now);
        self.restores = kept;
        done.into_iter()
            .map(|r| Finished {
                output: r.output,
                focus: r.focus,
                zoom: r.zoom,
                view: r.view,
            })
            .collect()
    }

    /// The next placeholder or restore deadline.
    pub fn deadline(&self) -> Option<Instant> {
        let p = self.list.iter().map(|p| p.deadline);
        let r = self.restores.iter().map(|r| r.deadline);
        p.chain(r).min()
    }
}

/// Reserves the spot the next window would take at span `want`; the canvas keeps its focus.
pub fn reserve(c: &mut Canvas, id: Id, want: Option<(i32, i32)>) {
    let prev = c.focus();
    c.add(id, 0., 0., want);
    if let Some(prev) = prev {
        c.focus_id(prev);
    }
}

/// Reserves each saved window's tiles, or the free ones nearest them; the canvas keeps its focus.
pub fn reserve_saved(c: &mut Canvas, ids: &[Id], windows: &[RestoreWindow]) {
    let prev = c.focus();
    for (&id, w) in ids.iter().zip(windows) {
        c.add_at(id, 0., 0., w.rect);
    }
    if let Some(prev) = prev {
        c.focus_id(prev);
    }
}

/// The default width and height the window rules give a window with this app ID, from the rules
/// a placeholder can tell apply: those whose match tests nothing but the app ID.
pub fn rule_size(rules: &[WindowRule], app_id: &str) -> RuleSize {
    let matches = |m: &Match| {
        m.app_id.as_ref().is_some_and(|re| re.0.is_match(app_id))
            && Match {
                app_id: None,
                at_startup: None,
                ..m.clone()
            } == Match::default()
    };
    let (mut width, mut height) = (None, None);
    for rule in rules {
        if (rule.matches.is_empty() || rule.matches.iter().any(matches))
            && !rule.excludes.iter().any(matches)
        {
            if let Some(x) = rule.default_column_width {
                width = Some(x.0);
            }
            if let Some(x) = rule.default_window_height {
                height = Some(x.0);
            }
        }
    }
    (width, height)
}

/// An icon's pixels: RGBA, straight alpha.
#[derive(Clone, Debug, PartialEq)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn load_png(path: &Path) -> Result<Icon> {
    decode_png(BufReader::new(File::open(path)?))
}

pub fn decode_png(r: impl std::io::BufRead + std::io::Seek) -> Result<Icon> {
    let mut decoder = png::Decoder::new(r);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let len = reader.output_buffer_size().context("image too large")?;
    let mut buf = vec![0; len];
    let info = reader.next_frame(&mut buf)?;
    let pixels = (info.width * info.height) as usize;
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..pixels * 4].to_vec(),
        png::ColorType::Rgb => buf[..pixels * 3]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..pixels * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf[..pixels].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => bail!("indexed image was not expanded"),
    };
    if info.width == 0 || info.height == 0 {
        bail!("empty image");
    }
    Ok(Icon {
        width: info.width,
        height: info.height,
        rgba,
    })
}

/// Where the icon goes in a box of `size`: centred, its longer side a third of the box's
/// shorter side.
pub fn icon_rect(size: (i32, i32), icon: (u32, u32)) -> (i32, i32, i32, i32) {
    let side = f64::from(size.0.min(size.1)) / 3.;
    let fit = side / f64::from(icon.0.max(icon.1));
    let w = (f64::from(icon.0) * fit).round().max(1.) as i32;
    let h = (f64::from(icon.1) * fit).round().max(1.) as i32;
    ((size.0 - w) / 2, (size.1 - h) / 2, w, h)
}

/// The placeholder's pixels at `size`: a rounded box of `color` (premultiplied) with the icon
/// centred on it. ARGB8888, little-endian (B, G, R, A), premultiplied.
pub fn paint(size: (i32, i32), color: [f32; 4], radius: f64, icon: Option<&Icon>) -> Vec<u8> {
    let (w, h) = (size.0.max(1), size.1.max(1));
    let mut px = vec![0u8; (w * h * 4) as usize];
    let half = (f64::from(w) / 2., f64::from(h) / 2.);
    let r = radius.min(half.0).min(half.1).max(0.);
    for y in 0..h {
        for x in 0..w {
            let (cx, cy) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let qx = (cx - half.0).abs() - (half.0 - r);
            let qy = (cy - half.1).abs() - (half.1 - r);
            let d = (qx.max(0.).powi(2) + qy.max(0.).powi(2)).sqrt() + qx.max(qy).min(0.) - r;
            let cover = (0.5 - d).clamp(0., 1.) as f32;
            let k = ((y * w + x) * 4) as usize;
            px[k] = to_u8(color[2] * cover);
            px[k + 1] = to_u8(color[1] * cover);
            px[k + 2] = to_u8(color[0] * cover);
            px[k + 3] = to_u8(color[3] * cover);
        }
    }
    if let Some(icon) = icon {
        let (ix, iy, iw, ih) = icon_rect((w, h), (icon.width, icon.height));
        for y in 0..ih {
            for x in 0..iw {
                let (dx, dy) = (ix + x, iy + y);
                if dx < 0 || dy < 0 || dx >= w || dy >= h {
                    continue;
                }
                let sx = ((x as u64 * u64::from(icon.width)) / iw as u64) as u32;
                let sy = ((y as u64 * u64::from(icon.height)) / ih as u64) as u32;
                let s = ((sy * icon.width + sx) * 4) as usize;
                let a = f32::from(icon.rgba[s + 3]) / 255.;
                let k = ((dy * w + dx) * 4) as usize;
                let over = |src: u8, dst: u8| {
                    to_u8(f32::from(src) / 255. * a + f32::from(dst) / 255. * (1. - a))
                };
                px[k] = over(icon.rgba[s + 2], px[k]);
                px[k + 1] = over(icon.rgba[s + 1], px[k + 1]);
                px[k + 2] = over(icon.rgba[s], px[k + 2]);
                px[k + 3] = to_u8(a + f32::from(px[k + 3]) / 255. * (1. - a));
            }
        }
    }
    px
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0., 1.) * 255.).round() as u8
}

/// The part `(x, y, cw, ch)` of a `w`-wide ARGB8888 image.
pub fn crop(px: &[u8], w: i32, (x, y, cw, ch): (i32, i32, i32, i32)) -> Vec<u8> {
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for row in y..y + ch {
        let start = ((row * w + x) * 4) as usize;
        out.extend_from_slice(&px[start..start + (cw * 4) as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{REMATCH_FOR, TITLE_MATCH};
    use niri_config::Config;

    fn at(d: &mut Placeholders, out: OutputId, app: &str, title: Option<&str>, now: Instant) -> Id {
        let id = d.alloc();
        d.push(Placeholder {
            id,
            output: out,
            app_id: app.into(),
            title: title.map(Into::into),
            icon: None,
            deadline: now + LIFETIME,
            created: now,
        });
        id
    }

    #[test]
    fn a_window_takes_its_apps_placeholder_its_title_first() {
        let now = Instant::now();
        let mut d = Placeholders::default();
        let a = at(&mut d, 1, "foot", Some("one"), now);
        let b = at(&mut d, 1, "foot", Some("two"), now);
        assert!(a >= FIRST_ID && b == a + 1);
        assert_eq!(d.claim("kitty", None, 1), None, "another app takes none");
        assert_eq!(
            d.claim("foot", Some("two"), 1)
                .map(|(p, score)| (p.id, score)),
            Some((b, 1.))
        );
        assert_eq!(
            d.claim("foot", Some("zzz"), 2).map(|(p, _)| p.id),
            Some(a),
            "else the oldest"
        );
        assert!(d.claim("foot", None, 3).is_none());
    }

    /// Titles change once tmux attaches or a tab loads, by a glyph or a count.
    #[test]
    fn a_window_takes_the_placeholder_whose_title_fits_best() {
        let now = Instant::now();
        let mut d = Placeholders::default();
        let slack = at(&mut d, 1, "chromium", Some("* Slack - Chromium"), now);
        let pr = at(&mut d, 1, "chromium", Some("PR 1 - Chromium"), now);
        let (p, score) = d.claim("chromium", Some("PR 1 - Chromium (2)"), 1).unwrap();
        assert_eq!(p.id, pr, "not the oldest: the closest title");
        assert!((TITLE_MATCH..1.).contains(&score), "{score}");
        let (p, score) = d.claim("chromium", Some("New Tab - Chromium"), 2).unwrap();
        assert_eq!(p.id, slack);
        assert!(score < TITLE_MATCH, "matched by its app: {score}");
    }

    #[test]
    fn restored_windows_trade_spots_only_for_a_while() {
        let now = Instant::now();
        let mut d = Placeholders::default();
        assert!(!d.rematching(now), "no restore, no trades");
        let a = at(&mut d, 1, "foot", None, now);
        let saved = RestoreCanvas {
            windows: Vec::new(),
            zoom: 0,
            view: (0., 0.),
            focus: Some(0),
        };
        let until = now + REMATCH_FOR;
        d.start_restore(1, vec![a], &saved, now + Duration::from_secs(20), until);
        assert!(d.rematching(until));
        d.retarget_focus(a, 9);
        assert_eq!(d.take(9, 7), None, "a window is no placeholder");
        assert_eq!(d.take(a, 7).map(|p| p.id), Some(a));
        assert_eq!(
            d.finish(now)[0].focus,
            Some(9),
            "the focus followed the spot"
        );
        assert!(!d.rematching(until + Duration::from_millis(1)));
        assert!(!d.rematching(now), "and stays off");
    }

    #[test]
    fn a_placeholder_moved_to_another_output_is_waited_for_there() {
        let now = Instant::now();
        let mut d = Placeholders::default();
        let a = at(&mut d, 1, "foot", None, now);
        let b = at(&mut d, 2, "foot", None, now);
        let saved = RestoreCanvas {
            windows: Vec::new(),
            zoom: 0,
            view: (0., 0.),
            focus: None,
        };
        let end = now + Duration::from_secs(20);
        d.start_restore(1, vec![a], &saved, end, end);
        d.start_restore(2, vec![b], &saved, end, end);
        d.take(a, 7);
        d.rehome(b, 1);
        assert_eq!(d.iter().map(|p| p.output).collect::<Vec<_>>(), vec![1]);
        let done: Vec<_> = d.finish(now).into_iter().map(|f| f.output).collect();
        assert_eq!(done, vec![2], "output 2 waits no more, output 1 does");
        d.take(b, 8);
        assert_eq!(d.finish(now)[0].output, 1);
    }

    #[test]
    fn placeholders_expire_after_their_lifetime() {
        let now = Instant::now();
        let mut d = Placeholders::default();
        at(&mut d, 1, "foot", None, now);
        assert_eq!(d.deadline(), Some(now + LIFETIME));
        assert!(d
            .expire(now + LIFETIME - Duration::from_millis(1))
            .is_empty());
        assert_eq!(d.expire(now + LIFETIME).len(), 1);
        assert_eq!(d.iter().count(), 0);
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn a_restore_ends_when_its_windows_came_or_its_time_passed() {
        let now = Instant::now();
        let mut d = Placeholders::default();
        let a = at(&mut d, 1, "foot", None, now);
        let b = at(&mut d, 1, "kitty", None, now);
        let saved = RestoreCanvas {
            windows: Vec::new(),
            zoom: 2,
            view: (10., 20.),
            focus: Some(1),
        };
        let end = now + Duration::from_secs(20);
        d.start_restore(1, vec![a, b], &saved, end, end);
        assert!(d.is_restoring());
        d.claim("kitty", None, 7);
        assert!(d.finish(now).is_empty(), "foot still waits");
        d.claim("foot", None, 8);
        let done = d.finish(now);
        assert_eq!(
            done,
            vec![Finished {
                output: 1,
                focus: Some(7),
                zoom: 2,
                view: (10., 20.)
            }],
            "the saved focus moves to the window that took its placeholder"
        );
        assert!(!d.is_restoring());

        let c = at(&mut d, 2, "mpv", None, now);
        d.start_restore(2, vec![c], &saved, end, end);
        assert!(d.finish(now + Duration::from_secs(19)).is_empty());
        assert_eq!(d.finish(now + Duration::from_secs(20)).len(), 1);
    }

    #[test]
    fn a_placeholder_keeps_the_canvas_focus_and_holds_its_tiles() {
        let config = newm_canvas::Config {
            tile_w: 119.,
            tile_h: 113.,
            ..newm_canvas::Config::default()
        };
        let mut c = Canvas::new(config, 1280., 720.);
        c.add(1, 10., 10., Some((4, 4)));
        reserve(&mut c, FIRST_ID, Some((4, 4)));
        assert_eq!(c.focus(), Some(1));
        let held = c.rect(FIRST_ID).unwrap();
        assert!(c.rename(FIRST_ID, 2));
        assert_eq!(
            c.rect(2),
            Some(held),
            "the window takes the placeholder's tiles"
        );
        let saved = [RestoreWindow {
            app_id: "foot".into(),
            title: None,
            rect: Tiles::new(20, 0, 3, 2),
        }];
        reserve_saved(&mut c, &[FIRST_ID + 1], &saved);
        assert_eq!(c.rect(FIRST_ID + 1), Some(Tiles::new(20, 0, 3, 2)));
        assert_eq!(c.focus(), Some(1));
        c.check().unwrap();
    }

    #[test]
    fn placeholder_size_follows_app_id_rules() {
        let config = Config::parse_mem(
            r#"
            window-rule {
                match app-id="^foot$"
                default-column-width { fixed 500; }
            }
            window-rule {
                match app-id="^foot$" title="x"
                default-window-height { fixed 300; }
            }
            "#,
        )
        .unwrap();
        let size = |app| rule_size(&config.window_rules, app);
        assert_eq!(size("foot"), (Some(Some(PresetSize::Fixed(500))), None));
        assert_eq!(size("kitty"), (None, None), "a placeholder has no title");
    }

    #[test]
    fn the_icon_sits_centred_at_a_third_of_the_shorter_side() {
        assert_eq!(icon_rect((300, 600), (64, 64)), (100, 250, 100, 100));
        assert_eq!(icon_rect((600, 300), (128, 64)), (250, 125, 100, 50));
        let icon = Icon {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
        };
        let (w, h) = (90, 60);
        let px = paint((w, h), [0., 0., 0.5, 0.5], RADIUS, Some(&icon));
        let pixel = |x: i32, y: i32| {
            let k = ((y * w + x) * 4) as usize;
            [px[k], px[k + 1], px[k + 2], px[k + 3]]
        };
        assert_eq!(
            pixel(45, 30),
            [0, 0, 255, 255],
            "the icon, opaque red, centred"
        );
        assert_eq!(pixel(10, 30), [128, 0, 0, 128], "the box, translucent blue");
        assert_eq!(pixel(0, 0), [0, 0, 0, 0], "outside the rounded corner");
        assert_eq!(crop(&px, w, (45, 30, 1, 1)), vec![0, 0, 255, 255]);
    }

    #[test]
    fn pngs_decode_to_rgba() {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        let icon = decode_png(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!((icon.width, icon.height), (2, 1));
        assert_eq!(icon.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
