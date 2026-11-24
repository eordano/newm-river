//! The minimap: a small plan of the canvas in the bottom-left
//! corner of the screen, shown for a moment whenever the camera, the focus or a window moves.
//!
//! The canvas has no edges, so the plan has none either: it is a window onto the canvas around
//! what the screen shows. The screen's outline sits in the middle at one size whatever the zoom,
//! and the canvas slides under it as the camera pans, its tile corners drawn as dots. The plan
//! has no frame: it fades out toward its sides, and what lies outside the screen's outline is
//! dimmed. Every window is drawn on its tiles with as many letters of its name as fit: its app's
//! name, or for a terminal connected to another machine with ssh, that machine's. A window too
//! far away to show leaves a mark on the side it lies toward.
//!
//! It is painted here, pixel by pixel, with a 3 by 5 pixel font of its own; `shell.rs` shows the
//! pixels on a shell surface above the windows.
//!
//! Two accents only, on near black: the border's active colour (the theme's accent) for the
//! focused window, Nord's frost for the others.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use newm_canvas::{Id, Rect as Tiles};

use super::{Desktop, OutputId, Rect};

/// The plan's size, in logical pixels.
pub const SIZE: (f64, f64) = (240., 160.);
/// Its distance from the screen's left and bottom edges.
pub const MARGIN: f64 = 12.;
/// The most of the plan's width and height the screen's outline takes.
const VIEW_SHARE: (f64, f64) = (0.4, 0.45);
/// How far from each side the plan fades out.
const FADE: f64 = 18.;
/// The least distance between tile dots: on a camera pulled far back they are left out.
const DOTS_MIN: f64 = 5.;
/// How far inside the plan the mark of a window beyond it sits.
const MARK_INSET: f64 = 7.;
/// The label font: letters 3 by [`FONT_SIZE`] logical pixels, [`LETTER`] apart.
const FONT_SIZE: f64 = 5.;
const LETTER: f64 = 4.;
/// The room between a window's edge and its label.
const LABEL_PAD: f64 = 2.;
/// How long the plan stays after the last move.
pub const SHOWN: Duration = Duration::from_millis(750);

/// Nord's frost, nord8 (#88C0D0).
const FROST: [f64; 3] = [0x88 as f64 / 255., 0xc0 as f64 / 255., 0xd0 as f64 / 255.];
/// The plan's near black (#0B0C16) and the outline's near white (#DDF7FF).
const DARK: [f64; 3] = [0x0b as f64 / 255., 0x0c as f64 / 255., 0x16 as f64 / 255.];
const LIGHT: [f64; 3] = [0xdd as f64 / 255., 0xf7 as f64 / 255., 1.];

/// A rectangle in logical pixels: x, y, width, height.
pub type Area = (f64, f64, f64, f64);

/// A window on the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanWindow {
    /// Where it is drawn, from the plan's top-left corner; it may reach past the plan.
    pub rect: Area,
    pub focused: bool,
    /// The letters of its app's name that fit in it.
    pub label: Option<String>,
}

/// What the minimap draws, from the plan's top-left corner.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The plan's top-left corner on the screen.
    pub loc: (f64, f64),
    /// The outline of what the screen shows.
    pub view: Area,
    /// The windows that reach into the plan, the focused one last.
    pub windows: Vec<PlanWindow>,
    /// A mark for every window beyond the plan, on the side it lies toward, and whether it is
    /// the focused one.
    pub marks: Vec<((f64, f64), bool)>,
    /// The first tile corner (at or after the plan's top-left) and the distance between them;
    /// `None` when the corners are closer than [`DOTS_MIN`].
    pub dots: Option<((f64, f64), (f64, f64))>,
}

/// What the minimap last drew from: when any of it changes, something moved.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    out: OutputId,
    view: (i32, i32, i32, i32),
    focus: Option<Id>,
    windows: Vec<(Id, Tiles)>,
}

/// What the minimap draws from.
struct Scene {
    out: OutputId,
    /// The output's tiled windows on the tiles they are shown on, by id.
    windows: Vec<(Id, Tiles)>,
    /// The part of its canvas the screen shows, in the canvas's logical pixels.
    view: Area,
}

#[derive(Debug, Default, Clone)]
pub struct Minimap {
    seen: Option<Seen>,
    /// When it goes, while it is shown.
    until: Option<Instant>,
    /// The host each terminal window is connected to with ssh, as the session saver last found
    /// it.
    hosts: HashMap<Id, String>,
}

/// The minimap in a render sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct MinimapFrame {
    /// Where the plan goes on screen, [`SIZE`] large.
    pub rect: Rect,
    /// Its output's name: the plan is painted at that output's scale.
    pub output: String,
    pub plan: Plan,
    /// The border's active colour, without its alpha: the focused window's.
    pub accent: [f64; 3],
}

/// The name a window's app is known by: the last part of its app ID, in lower case
/// (`com.mitchellh.ghostty` is `ghostty`). The font draws it in capitals.
pub fn app_name(app_id: &str) -> String {
    let name = app_id.rsplit('.').find(|part| !part.is_empty());
    name.unwrap_or(app_id).to_lowercase()
}

/// The name a remote machine is known by: the first part of the host the user gave ssh
/// (`lab.example` is `lab`), or all of an address.
pub fn host_name(host: &str) -> String {
    let numeric = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    let first = host.split('.').next().unwrap_or(host);
    let name = if host.contains(':') || numeric(first) {
        host
    } else {
        first
    };
    name.to_lowercase()
}

/// The letters of `name` that fit in a window drawn `size` large: none when it is lower than a
/// line or narrower than a letter.
pub fn label_fit(name: &str, size: (f64, f64)) -> Option<String> {
    if size.1 < FONT_SIZE + LABEL_PAD * 2. {
        return None;
    }
    // The last letter needs no gap after it.
    let letters = ((size.0 - LABEL_PAD * 2. + 1.) / LETTER).floor();
    if letters < 1. || name.is_empty() {
        return None;
    }
    Some(name.chars().take(letters as usize).collect())
}

fn overlaps(a: Area, b: Area) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// The minimap on a screen of `screen` size. `windows` are the canvas's windows, each with
/// whether it has the focus and its app's name, and `view` is the part of the canvas the screen
/// shows, all in the canvas's logical pixels; `tile` is the size of one tile there.
///
/// `view` is drawn in the middle, [`VIEW_SHARE`] of the plan, so one scale maps the canvas
/// around it: a camera pulled back keeps the outline's size and draws the windows smaller.
/// Nothing without a window.
pub fn plan(
    windows: &[(Area, bool, String)],
    view: Area,
    tile: (f64, f64),
    screen: (f64, f64),
) -> Option<Plan> {
    if windows.is_empty() || view.2 <= 0. || view.3 <= 0. {
        return None;
    }
    let k = f64::min(
        SIZE.0 * VIEW_SHARE.0 / view.2,
        SIZE.1 * VIEW_SHARE.1 / view.3,
    );
    let middle = (view.0 + view.2 / 2., view.1 + view.3 / 2.);
    let centre = (SIZE.0 / 2., SIZE.1 / 2.);
    let place = |r: Area| -> Area {
        (
            (r.0 - middle.0) * k + centre.0,
            (r.1 - middle.1) * k + centre.1,
            r.2 * k,
            r.3 * k,
        )
    };
    let whole = (0., 0., SIZE.0, SIZE.1);

    let mut shown = Vec::new();
    let mut marks = Vec::new();
    for (rect, focused, name) in windows {
        let rect = place(*rect);
        if overlaps(rect, whole) {
            shown.push(PlanWindow {
                rect,
                focused: *focused,
                label: label_fit(name, (rect.2, rect.3)),
            });
        } else {
            let at = (rect.0 + rect.2 / 2., rect.1 + rect.3 / 2.);
            let mark = (
                at.0.clamp(MARK_INSET, SIZE.0 - MARK_INSET),
                at.1.clamp(MARK_INSET, SIZE.1 - MARK_INSET),
            );
            marks.push((mark, *focused));
        }
    }
    shown.sort_by_key(|w| w.focused);

    let step = (tile.0 * k, tile.1 * k);
    let dots = (step.0 >= DOTS_MIN && step.1 >= DOTS_MIN).then(|| {
        let origin = place((0., 0., 0., 0.));
        let first = (origin.0.rem_euclid(step.0), origin.1.rem_euclid(step.1));
        (first, step)
    });

    Some(Plan {
        loc: (MARGIN, screen.1 - MARGIN - SIZE.1),
        view: place(view),
        windows: shown,
        marks,
        dots,
    })
}

/// A letter of the 3 by 5 font: five rows, top first, the left pixel in bit 2. Letters are
/// capitals; what the font lacks is a low dot.
fn glyph(c: char) -> [u8; 5] {
    let rows: u16 = match c.to_ascii_uppercase() {
        'A' => 0o25755,
        'B' => 0o65656,
        'C' => 0o34443,
        'D' => 0o65556,
        'E' => 0o74647,
        'F' => 0o74644,
        'G' => 0o34553,
        'H' => 0o55755,
        'I' => 0o72227,
        'J' => 0o11152,
        'K' => 0o55655,
        'L' => 0o44447,
        'M' => 0o57755,
        'N' => 0o75555,
        'O' => 0o25552,
        'P' => 0o65644,
        'Q' => 0o25573,
        'R' => 0o65655,
        'S' => 0o34216,
        'T' => 0o72222,
        'U' => 0o55557,
        'V' => 0o55552,
        'W' => 0o55775,
        'X' => 0o55255,
        'Y' => 0o55222,
        'Z' => 0o71247,
        '0' => 0o75557,
        '1' => 0o26227,
        '2' => 0o61247,
        '3' => 0o61216,
        '4' => 0o55711,
        '5' => 0o74616,
        '6' => 0o34652,
        '7' => 0o71222,
        '8' => 0o25252,
        '9' => 0o25316,
        '-' | '_' => 0o00700,
        ':' => 0o02020,
        ' ' => 0,
        _ => 0o00002,
    };
    [12, 9, 6, 3, 0].map(|shift| ((rows >> shift) & 7) as u8)
}

/// The plan's pixels: premultiplied RGBA while it is painted, `scale` pixels to a logical one.
pub struct Pixels {
    pub w: usize,
    pub h: usize,
    scale: f64,
    data: Vec<[f32; 4]>,
}

impl Pixels {
    fn new(scale: f64) -> Self {
        let (w, h) = pixels(scale);
        Self {
            w: w as usize,
            h: h as usize,
            scale,
            data: vec![[0.; 4]; (w * h) as usize],
        }
    }

    /// Paints `colour` at `alpha` over the logical rectangle from `(x0, y0)` to `(x1, y1)`, cut
    /// to the plan; a rectangle thinner than a pixel still takes one.
    fn fill(&mut self, (x0, y0, x1, y1): (f64, f64, f64, f64), colour: [f64; 3], alpha: f64) {
        let px = |v: f64, most: usize| (v * self.scale).round().clamp(0., most as f64) as usize;
        let (left, top) = (px(x0, self.w), px(y0, self.h));
        let right = px(x1, self.w).max(left + 1).min(self.w);
        let bottom = px(y1, self.h).max(top + 1).min(self.h);
        if left >= right {
            return;
        }
        let a = alpha as f32;
        let src = [
            colour[0] as f32 * a,
            colour[1] as f32 * a,
            colour[2] as f32 * a,
            a,
        ];
        for y in top..bottom {
            for dst in &mut self.data[y * self.w + left..y * self.w + right] {
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = s + *d * (1. - a);
                }
            }
        }
    }

    fn rect(&mut self, r: Area, colour: [f64; 3], alpha: f64) {
        self.fill((r.0, r.1, r.0 + r.2, r.1 + r.3), colour, alpha);
    }

    /// The outline of `r`, `width` thick, inside it.
    fn outline(&mut self, r: Area, width: f64, colour: [f64; 3]) {
        let (x0, y0) = (r.0, r.1);
        let (x1, y1) = (x0 + r.2, y0 + r.3);
        self.fill((x0, y0, x1, y0 + width), colour, 1.);
        self.fill((x0, y1 - width, x1, y1), colour, 1.);
        self.fill((x0, y0 + width, x0 + width, y1 - width), colour, 1.);
        self.fill((x1 - width, y0 + width, x1, y1 - width), colour, 1.);
    }

    /// `text` in the 3 by 5 font, its top-left corner at `at`.
    fn text(&mut self, text: &str, at: (f64, f64), colour: [f64; 3]) {
        for (n, c) in text.chars().enumerate() {
            let x = at.0 + n as f64 * LETTER;
            for (row, bits) in glyph(c).into_iter().enumerate() {
                for col in 0..3 {
                    if bits & (4 >> col) != 0 {
                        let (x, y) = (x + f64::from(col), at.1 + row as f64);
                        self.fill((x, y, x + 1., y + 1.), colour, 1.);
                    }
                }
            }
        }
    }

    /// Fades the plan out toward its four sides, over [`FADE`] from each.
    fn fade(&mut self) {
        let fade = FADE * self.scale;
        let share = |at: usize, length: usize| {
            let from_side = f64::min(at as f64 + 0.5, length as f64 - at as f64 - 0.5);
            (from_side / fade).clamp(0., 1.) as f32
        };
        for y in 0..self.h {
            for x in 0..self.w {
                let f = share(x, self.w) * share(y, self.h);
                for v in &mut self.data[y * self.w + x] {
                    *v *= f;
                }
            }
        }
    }

    /// The pixels as premultiplied ARGB8888 bytes.
    pub fn bytes(&self) -> Vec<u8> {
        let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
        let bgra = |p: &[f32; 4]| [byte(p[2]), byte(p[1]), byte(p[0]), byte(p[3])];
        self.data.iter().flat_map(bgra).collect()
    }
}

/// The plan's size in physical pixels at `scale`.
pub fn pixels(scale: f64) -> (i32, i32) {
    (
        (SIZE.0 * scale).ceil() as i32,
        (SIZE.1 * scale).ceil() as i32,
    )
}

/// Paints the plan: the tile dots, the windows with their labels, the dimming of what the screen
/// does not show, the screen's outline, the marks, scanlines, and the fade toward the sides.
pub fn paint(plan: &Plan, accent: [f64; 3], scale: f64) -> Pixels {
    let mut px = Pixels::new(scale);
    px.fill((0., 0., SIZE.0, SIZE.1), DARK, 0.8);

    if let Some((first, step)) = plan.dots {
        let mut y = first.1;
        while y < SIZE.1 {
            let mut x = first.0;
            while x < SIZE.0 {
                px.fill((x - 0.5, y - 0.5, x + 0.5, y + 0.5), FROST, 0.45);
                x += step.0;
            }
            y += step.1;
        }
    }

    for window in &plan.windows {
        let colour = if window.focused { accent } else { FROST };
        px.rect(window.rect, colour, if window.focused { 0.75 } else { 0.2 });
        px.outline(window.rect, 1., colour);
        if let Some(label) = &window.label {
            let at = (window.rect.0 + LABEL_PAD, window.rect.1 + LABEL_PAD);
            px.text(label, at, if window.focused { DARK } else { LIGHT });
        }
    }

    // What the screen does not show is dimmed, so the eye lands on what it does.
    let view = plan.view;
    let (vx0, vy0) = (view.0, view.1);
    let (vx1, vy1) = (vx0 + view.2, vy0 + view.3);
    px.fill((0., 0., SIZE.0, vy0), DARK, 0.5);
    px.fill((0., vy1, SIZE.0, SIZE.1), DARK, 0.5);
    px.fill((0., vy0, vx0, vy1), DARK, 0.5);
    px.fill((vx1, vy0, SIZE.0, vy1), DARK, 0.5);
    px.outline(view, 1.5, LIGHT);

    for (at, focused) in &plan.marks {
        let colour = if *focused { accent } else { FROST };
        px.fill((at.0 - 2., at.1 - 2., at.0 + 2., at.1 + 2.), colour, 1.);
    }

    let mut y = 2.;
    while y < SIZE.1 {
        px.fill((0., y, SIZE.0, y + 1.), [0.; 3], 0.14);
        y += 3.;
    }
    px.fade();
    px
}

impl Desktop {
    /// What the minimap draws: the active output's canvas.
    fn minimap_scene(&self) -> Option<Scene> {
        let o = self.output(self.active_canvas_output()?)?;
        let mut windows: Vec<(Id, Tiles)> = o
            .canvas
            .windows()
            .iter()
            .filter_map(|w| Some((w.id, self.shown_tiles(o, w.id)?)))
            .collect();
        windows.sort_by_key(|(id, _)| *id);
        let cam = self.camera(o);
        let (s, screen) = (cam.scale.max(1e-9), o.canvas.screen());
        let view = (cam.view.0 / s, cam.view.1 / s, screen.0 / s, screen.1 / s);
        Some(Scene {
            out: o.id,
            windows,
            view,
        })
    }

    /// Shows the minimap for [`SHOWN`] from `now` when the camera, the focus or a window moved
    /// since the last call; the first call only looks. Called once a render sequence.
    pub(super) fn minimap_watch(&mut self, now: Instant) {
        if self.minimap.until.is_some_and(|until| until <= now) {
            self.minimap.until = None;
        }
        let Some(Scene { out, windows, view }) = self.minimap_scene() else {
            return;
        };
        let round = |v: f64| v.round() as i32;
        let seen = Seen {
            out,
            view: (round(view.0), round(view.1), round(view.2), round(view.3)),
            focus: self.focus,
            windows,
        };
        if self.minimap.seen.as_ref() == Some(&seen) {
            return;
        }
        if self.minimap.seen.replace(seen).is_some() {
            self.minimap.until = Some(now + SHOWN);
        }
    }

    /// The session saver's answer after its tick: the host `host` gives is the one that terminal
    /// window is connected to with ssh, and the minimap names the window by it.
    pub fn set_remote_hosts(&mut self, host: impl Fn(Id) -> Option<String>) {
        self.minimap.hosts = self
            .windows
            .keys()
            .filter_map(|&id| Some((id, host(id)?)))
            .collect();
    }

    /// When the minimap goes, while it is shown.
    pub(super) fn minimap_deadline(&self) -> Option<Instant> {
        self.minimap.until
    }

    /// Takes the minimap down once its time is over. Returns whether it did.
    pub(super) fn tick_minimap(&mut self, now: Instant) -> bool {
        let over = self.minimap.until.is_some_and(|until| until <= now);
        if over {
            self.minimap.until = None;
        }
        over
    }

    /// The minimap in a render sequence, while it is shown: in the bottom-left corner of the
    /// active output's working area.
    pub fn minimap_frame(&self) -> Option<MinimapFrame> {
        self.minimap.until?;
        let Scene { out, windows, view } = self.minimap_scene()?;
        let o = self.output(out)?;
        let wa = o.work_area();
        let cells: Vec<(Area, bool, String)> = windows
            .iter()
            .map(|&(id, r)| {
                let app = self.windows.get(&id).and_then(|w| w.app_id.as_deref());
                let name = match self.minimap.hosts.get(&id) {
                    Some(host) => host_name(host),
                    None => app_name(app.unwrap_or_default()),
                };
                (self.cell(&o.canvas, r), Some(id) == self.focus, name)
            })
            .collect();
        let screen = (f64::from(wa.w), f64::from(wa.h));
        let plan = plan(&cells, view, o.canvas.base_tile(), screen)?;
        let rect = Rect::new(
            wa.x + plan.loc.0.round() as i32,
            wa.y + plan.loc.1.round() as i32,
            SIZE.0 as i32,
            SIZE.1 as i32,
        );
        let [r, g, b, a] = self.settings.border.active.map(f64::from);
        let accent = if a > 0. {
            [r / a, g / a, b / a]
        } else {
            [r, g, b]
        };
        Some(MinimapFrame {
            rect,
            output: o.name.clone(),
            plan,
            accent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(r: Area, focused: bool) -> (Area, bool, String) {
        (r, focused, "ghostty".to_owned())
    }

    const SCREEN: (f64, f64) = (1440., 960.);
    const TILE: (f64, f64) = (119., 113.);

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn plan_of(windows: &[(Area, bool, String)], view: Area) -> Plan {
        plan(windows, view, TILE, SCREEN).unwrap()
    }

    #[test]
    fn no_windows_no_minimap() {
        let view = (0., 0., 1440., 960.);
        assert_eq!(plan(&[], view, TILE, SCREEN), None);
    }

    #[test]
    fn sits_in_the_bottom_left_corner() {
        let windows = [win((0., 0., 1000., 800.), true)];
        let plan = plan_of(&windows, (0., 0., 1440., 960.));
        assert_eq!(plan.loc, (MARGIN, SCREEN.1 - MARGIN - SIZE.1));
    }

    #[test]
    fn the_screen_stays_in_the_middle_at_one_size_wherever_the_camera_is() {
        let windows = [win((0., 0., 1000., 800.), true)];
        let views = [
            (0., 0., 1440., 960.),
            (-5000., 9000., 1440., 960.),
            (300., 300., 2880., 1920.),
        ]
        .map(|view| plan_of(&windows, view).view);
        let near = |a: Area, b: Area| {
            close(a.0, b.0) && close(a.1, b.1) && close(a.2, b.2) && close(a.3, b.3)
        };
        assert!(
            near(views[0], views[1]),
            "a pan moves the canvas, not the outline"
        );
        assert!(
            near(views[0], views[2]),
            "a camera pulled back keeps the outline's size"
        );
        let view = views[0];
        assert!(close(view.2, SIZE.0 * VIEW_SHARE.0));
        assert!(close(view.0 + view.2 / 2., SIZE.0 / 2.));
        assert!(close(view.1 + view.3 / 2., SIZE.1 / 2.));
        assert!(view.3 <= SIZE.1 * VIEW_SHARE.1 + 1e-9);
    }

    #[test]
    fn windows_keep_their_place_and_proportions_around_the_screen() {
        let view = (1000., 1000., 1440., 960.);
        let windows = [
            win((1000., 1000., 720., 960.), true),
            win((1720., 1000., 720., 480.), false),
        ];
        let plan = plan_of(&windows, view);
        let [other, focused] = &plan.windows[..] else {
            panic!("two windows, the focused one last, drawn over the others");
        };
        assert!(focused.focused && !other.focused);
        assert!(close(focused.rect.0, plan.view.0));
        assert!(close(focused.rect.1, plan.view.1));
        assert!(close(focused.rect.2, plan.view.2 / 2.));
        assert!(close(focused.rect.3, plan.view.3));
        assert!(close(other.rect.0, focused.rect.0 + focused.rect.2));
        assert!(close(other.rect.3, focused.rect.3 / 2.));
        assert!(plan.marks.is_empty());
    }

    #[test]
    fn a_camera_pulled_back_draws_the_windows_smaller() {
        let windows = [win((0., 0., 1440., 960.), true)];
        let near = plan_of(&windows, (0., 0., 1440., 960.));
        let far = plan_of(&windows, (-720., -480., 2880., 1920.));
        assert!(close(far.windows[0].rect.2, near.windows[0].rect.2 / 2.));
    }

    #[test]
    fn a_window_beyond_the_plan_leaves_a_mark_on_its_side() {
        let view = (0., 0., 1440., 960.);
        let windows = [
            win((0., 0., 1000., 800.), false),
            win((20000., 200., 1000., 800.), true),
            win((200., -30000., 1000., 800.), false),
        ];
        let plan = plan_of(&windows, view);
        assert_eq!(plan.windows.len(), 1, "only the near window is drawn");
        let [(right, right_focused), (up, up_focused)] = plan.marks[..] else {
            panic!("a mark for each far window");
        };
        assert!(right_focused && !up_focused);
        assert!(
            close(right.0, SIZE.0 - MARK_INSET),
            "far to the right: on the right side"
        );
        assert!(right.1 > MARK_INSET && right.1 < SIZE.1 - MARK_INSET);
        assert!(close(up.1, MARK_INSET), "far above: on the top side");
    }

    #[test]
    fn tile_dots_slide_with_the_canvas_and_go_when_too_close() {
        let windows = [win((0., 0., 1000., 800.), true)];
        let at = |x: f64| plan_of(&windows, (x, 0., 1440., 960.)).dots.unwrap();
        let (first, step) = at(0.);
        assert!(close(step.0, TILE.0 * SIZE.0 * VIEW_SHARE.0 / 1440.));
        assert!(first.0 >= 0. && first.0 < step.0 && first.1 >= 0. && first.1 < step.1);
        let (moved, _) = at(TILE.0 / 2.);
        let slid = (first.0 - moved.0).rem_euclid(step.0);
        assert!(
            close(slid, step.0 / 2.),
            "half a tile of pan slides the dots half a step"
        );
        let (whole, _) = at(TILE.0);
        assert!(
            (whole.0 - first.0).abs() < 1e-6,
            "a whole tile brings them back"
        );
        let far = (0., 0., 14400., 9600.);
        assert_eq!(plan_of(&windows, far).dots, None);
    }

    #[test]
    fn app_names_are_the_last_part_of_the_app_id() {
        assert_eq!(app_name("com.mitchellh.ghostty"), "ghostty");
        assert_eq!(app_name("chromium-browser"), "chromium-browser");
        assert_eq!(app_name("Alacritty"), "alacritty");
        assert_eq!(app_name("org.kde.dolphin."), "dolphin");
        assert_eq!(app_name(""), "");
    }

    #[test]
    fn host_names_are_the_first_part_or_the_whole_address() {
        assert_eq!(host_name("buildhost"), "buildhost");
        assert_eq!(host_name("Lab.Example.org"), "lab");
        assert_eq!(host_name("100.64.0.24"), "100.64.0.24");
        assert_eq!(host_name("fd7a::1"), "fd7a::1");
    }

    #[test]
    fn labels_take_the_letters_that_fit() {
        let fit = |w: f64, h: f64| label_fit("ghostty", (w, h));
        assert_eq!(fit(100., 40.).as_deref(), Some("ghostty"));
        let three = LABEL_PAD * 2. + LETTER * 3. - 1.;
        assert_eq!(
            fit(three, 40.).as_deref(),
            Some("gho"),
            "the last needs no gap"
        );
        assert_eq!(fit(three - 0.1, 40.).as_deref(), Some("gh"));
        assert_eq!(fit(LABEL_PAD * 2. + 3., 40.).as_deref(), Some("g"));
        assert_eq!(
            fit(LABEL_PAD * 2. + 2.9, 40.),
            None,
            "narrower than a letter"
        );
        assert_eq!(fit(100., FONT_SIZE + LABEL_PAD), None, "lower than a line");
        assert_eq!(label_fit("", (100., 40.)), None);
    }

    #[test]
    fn the_font_draws_capitals_digits_and_a_dot_for_the_rest() {
        assert_eq!(glyph('a'), glyph('A'));
        assert_eq!(glyph('A'), [0b010, 0b101, 0b111, 0b101, 0b101]);
        assert_eq!(glyph('L'), [0b100, 0b100, 0b100, 0b100, 0b111]);
        assert_eq!(glyph('1'), [0b010, 0b110, 0b010, 0b010, 0b111]);
        assert_eq!(glyph(' '), [0; 5]);
        assert_eq!(glyph('~'), [0, 0, 0, 0, 0b010]);
        let mut seen = std::collections::HashSet::new();
        for c in ('a'..='z').chain('0'..='9') {
            assert!(seen.insert(glyph(c)), "{c} looks like another letter");
        }
    }

    #[test]
    fn text_paints_the_letters_pixels() {
        let mut px = Pixels::new(1.);
        px.text("L", (10., 20.), [1., 1., 1.]);
        let lit = |x: usize, y: usize| px.data[y * px.w + x][3] > 0.5;
        assert!(
            lit(10, 20) && lit(10, 24) && lit(12, 24),
            "the L's stem and foot"
        );
        assert!(!lit(11, 20) && !lit(12, 22) && !lit(13, 24));
        let mut twice = Pixels::new(2.);
        twice.text("L", (10., 20.), [1., 1., 1.]);
        let lit = |x: usize, y: usize| twice.data[y * twice.w + x][3] > 0.5;
        assert!(
            lit(20, 40) && lit(21, 41) && lit(25, 49),
            "each font pixel is two across"
        );
        assert!(!lit(22, 40));
    }

    #[test]
    fn a_plan_paints_at_any_scale_and_fades_out_at_its_sides() {
        let view = (0., 0., 1440., 960.);
        let windows = [
            win((0., 0., 720., 960.), true),
            win((720., 0., 720., 480.), false),
            win((20000., 0., 720., 480.), false),
        ];
        let plan = plan_of(&windows, view);
        for scale in [1., 1.25, 2.] {
            let px = paint(&plan, [0.5, 1., 0.6], scale);
            assert_eq!((px.w as i32, px.h as i32), pixels(scale));
            let bytes = px.bytes();
            assert_eq!(bytes.len(), px.w * px.h * 4);
            let alpha = |x: usize, y: usize| bytes[(y * px.w + x) * 4 + 3];
            assert!(alpha(0, px.h / 2) < 16, "faded out at the side");
            assert!(alpha(px.w / 2, 0) < 16, "and at the top");
            assert!(alpha(px.w / 2, px.h / 2) > 200, "solid in the middle");
            for p in bytes.as_chunks::<4>().0 {
                assert!(
                    p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3],
                    "premultiplied"
                );
            }
        }
    }
}
