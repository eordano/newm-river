//! Motion in the usage log: how far, how fast and how hard
//! the camera and the windows move once an operation or a gesture sets them moving, so the
//! disruption of rearrangements and camera moves can be judged on real use.
//!
//! A motion episode begins on an output when a logged operation changes its canvas or moves its
//! camera, a pan begins, or a drag's preview moves the other windows. Each render samples what is
//! drawn: the camera's view point and scale, and every tiled window that glides. The episode ends
//! once nothing glides, the camera rests and no pan or drag preview is on, or when the next
//! operation begins one of its own (`cut` when it had not settled); its summary is one
//! `{"op":"motion"}` line of the usage log, carrying the sequence number of the operation that
//! started it. Positions are canvas pixels at 1:1; `camera_zoomed` is the camera's view point at
//! the scale it draws at. A window counts as moved, and an episode is logged at all, from half a
//! pixel of path. Nothing is written per frame. Without the `telemetry` feature no episode
//! begins and no frame is sampled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use newm_canvas::Id;
use serde_json::{json, Value};

use super::usage::TELEMETRY;
use super::{Desktop, OutputId};

static SEQ: AtomicU64 = AtomicU64::new(1);

/// The episode of a drag's preview moving the other windows, which lasts while the drag is on.
pub(super) const DRAG: &str = "drag";

/// The episode of a pan by the pointer (Super and motion).
pub(super) const PAN: &str = "pan-pointer";

/// The least path, in 1:1 canvas pixels, that counts as moving: less does not show.
const MOVED_PX: f64 = 0.5;

/// The next number in this session's sequence of operations and motion episodes.
pub(super) fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Rounds to a tenth, to keep the log short.
pub(super) fn r(x: f64) -> f64 {
    (x * 10.).round() / 10.
}

/// The path, the top speed and acceleration, and the largest single-frame jump of one point from
/// its samples. Speed is in px/s over each frame; acceleration is the change between two frames'
/// speeds over the time between their midpoints.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MotionStats {
    last: Option<(Duration, (f64, f64))>,
    /// The last frame's velocity and its length in seconds.
    velocity: Option<((f64, f64), f64)>,
    pub path: f64,
    pub max_speed: f64,
    pub max_accel: f64,
    pub max_jump: f64,
}

impl MotionStats {
    /// A point at rest at `p` at time `t`: its first frame accelerates from a standstill.
    pub fn at_rest(t: Duration, p: (f64, f64)) -> Self {
        Self {
            last: Some((t, p)),
            velocity: Some(((0., 0.), 0.)),
            ..Self::default()
        }
    }

    /// The point drawn at `p` at time `t`. A move in no time is a jump with no speed.
    pub fn sample(&mut self, t: Duration, p: (f64, f64)) {
        let Some((t0, p0)) = self.last.replace((t, p)) else {
            return;
        };
        let (dx, dy) = (p.0 - p0.0, p.1 - p0.1);
        let d = dx.hypot(dy);
        self.path += d;
        self.max_jump = self.max_jump.max(d);
        let dt = t.saturating_sub(t0).as_secs_f64();
        if dt <= 0. {
            return;
        }
        let v = (dx / dt, dy / dt);
        self.max_speed = self.max_speed.max(v.0.hypot(v.1));
        if let Some((v0, dt0)) = self.velocity {
            let accel = (v.0 - v0.0).hypot(v.1 - v0.1) / ((dt0 + dt) / 2.);
            self.max_accel = self.max_accel.max(accel);
        }
        self.velocity = Some((v, dt));
    }

    fn json(&self) -> Value {
        json!({
            "path": r(self.path),
            "max_speed": r(self.max_speed),
            "max_accel": r(self.max_accel),
            "max_jump": r(self.max_jump),
        })
    }
}

/// What sets an output's canvas moving.
#[derive(Clone, Debug)]
pub(super) struct Cause {
    kind: String,
    seq: Option<u64>,
    /// Carries on an episode of the same kind under way instead of beginning one.
    carry: bool,
}

impl Cause {
    /// Operation `kind` numbered `seq` in the usage log.
    pub fn op(kind: &str, seq: u64) -> Self {
        Self {
            kind: kind.into(),
            seq: Some(seq),
            carry: false,
        }
    }

    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            seq: None,
            carry: false,
        }
    }

    /// Carries on the episode of `kind` under way: a drag's preview moving again.
    pub fn carry(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            seq: None,
            carry: true,
        }
    }
}

/// What is drawn on an output at one moment: the camera's 1:1 view point and scale, and where
/// each tiled window is drawn, in canvas pixels at 1:1.
#[derive(Clone, Debug)]
pub(super) struct Drawn {
    t: Duration,
    view: (f64, f64),
    scale: f64,
    windows: HashMap<Id, (f64, f64)>,
}

#[derive(Debug)]
struct Track {
    stats: MotionStats,
    moving: bool,
}

#[derive(Debug)]
struct Episode {
    kind: String,
    seq: u64,
    start: Duration,
    /// The last sample's time: windows that begin to glide later were at rest then.
    last: Duration,
    frames: u32,
    /// Whether all was at rest when the episode began, and not cut out of another.
    from_rest: bool,
    /// Whether nothing moved at the last sample.
    still: bool,
    camera: MotionStats,
    zoomed: MotionStats,
    scale: (f64, f64),
    rest: HashMap<Id, (f64, f64)>,
    windows: HashMap<Id, Track>,
}

impl Episode {
    fn new(kind: String, seq: u64, from_rest: bool, drawn: Drawn) -> Self {
        let start = |p: (f64, f64)| {
            if from_rest {
                MotionStats::at_rest(drawn.t, p)
            } else {
                let mut s = MotionStats::default();
                s.sample(drawn.t, p);
                s
            }
        };
        let (v, s) = (drawn.view, drawn.scale);
        Self {
            kind,
            seq,
            start: drawn.t,
            last: drawn.t,
            frames: 0,
            from_rest,
            still: false,
            camera: start(v),
            zoomed: start((v.0 * s, v.1 * s)),
            scale: (s, s),
            rest: drawn.windows,
            windows: HashMap::new(),
        }
    }

    fn moved(&self) -> bool {
        self.camera.path >= MOVED_PX || self.windows.values().any(|w| w.stats.path >= MOVED_PX)
    }
}

/// Each output's motion episode under way, and when the last frame was drawn.
#[derive(Debug, Default)]
pub struct Motion {
    episodes: HashMap<OutputId, Episode>,
    last_frame: Option<Instant>,
    /// Every usage line, kept in tests, where nothing is written.
    #[cfg(all(test, feature = "telemetry"))]
    pub lines: Vec<Value>,
}

impl Desktop {
    /// Writes `line` to the usage log, marked `during_drag` while a drag is on (the drop that
    /// ends it is not); tests keep it instead.
    pub(super) fn emit(&mut self, mut line: Value) {
        if !TELEMETRY {
            return;
        }
        if self.dragged().is_some()
            && line["op"] != "drop"
            && let Value::Object(m) = &mut line
        {
            m.insert("during_drag".into(), Value::Bool(true));
        }
        super::usage::append(&line);
        #[cfg(all(test, feature = "telemetry"))]
        self.motion.lines.push(line);
    }

    /// What output `out` drew at its last frame (now, before the first). A window being dragged
    /// is left out: it follows the pointer, so where it was is no rest for a later glide. `None`
    /// without telemetry, so no episode begins.
    pub(super) fn drawn_now(&self, out: OutputId) -> Option<Drawn> {
        if !TELEMETRY {
            return None;
        }
        let o = self.output(out)?;
        let cam = self.camera(o);
        let at = self.motion.last_frame.unwrap_or(self.started);
        let windows = o
            .canvas
            .windows()
            .iter()
            .filter(|w| self.dragged() != Some(w.id))
            .filter_map(|w| Some((w.id, self.drawn_1to1(o, w.id)?)))
            .collect();
        Some(Drawn {
            t: at.saturating_duration_since(self.started),
            view: (cam.view.0 / cam.scale, cam.view.1 / cam.scale),
            scale: cam.scale,
            windows,
        })
    }

    /// Where tiled window `id` is drawn on its canvas at 1:1: its shown tiles' cell plus its glide.
    fn drawn_1to1(&self, o: &super::Output, id: Id) -> Option<(f64, f64)> {
        let (x, y, _, _) = self.cell(&o.canvas, self.shown_tiles(o, id)?);
        let g = self.glide_offset(id);
        Some((x + g.0, y + g.1))
    }

    /// Begins an episode on `out` for `cause` from what was drawn `before`, ending the one under
    /// way, or carries on the one under way when `cause` says so and it is of the same kind.
    pub(super) fn begin_motion(&mut self, out: OutputId, cause: Cause, before: Drawn) {
        if !TELEMETRY {
            return;
        }
        if cause.carry
            && let Some(ep) = self.motion.episodes.get_mut(&out)
            && ep.kind == cause.kind
        {
            ep.rest = before.windows;
            return;
        }
        let cut = self.end_motion(out);
        let seq = cause.seq.unwrap_or_else(next_seq);
        let ep = Episode::new(cause.kind, seq, !cut, before);
        self.motion.episodes.insert(out, ep);
    }

    /// Samples every output's episode at the frame drawn at `now`, ending those that settled.
    pub(super) fn sample_motion(&mut self, now: Instant) {
        if !TELEMETRY {
            return;
        }
        self.motion.last_frame = Some(now);
        let t = now.saturating_duration_since(self.started);
        let outs: Vec<OutputId> = self.motion.episodes.keys().copied().collect();
        for out in outs {
            let Some(mut ep) = self.motion.episodes.remove(&out) else {
                continue;
            };
            let Some(o) = self.output(out) else {
                continue;
            };
            let cam = self.camera(o);
            let view = (cam.view.0 / cam.scale, cam.view.1 / cam.scale);
            ep.camera.sample(t, view);
            ep.zoomed.sample(t, cam.view);
            ep.scale = (ep.scale.0.min(cam.scale), ep.scale.1.max(cam.scale));
            let mut moving = false;
            for w in o.canvas.windows() {
                let animating = self.glides_on(w.id);
                moving |= animating;
                match ep.windows.get(&w.id).map(|t| t.moving) {
                    Some(was) if !animating && !was => continue,
                    None if !animating => continue,
                    Some(_) => {}
                    None => {
                        let stats = match ep.rest.get(&w.id) {
                            Some(&p) if ep.from_rest => MotionStats::at_rest(ep.last, p),
                            Some(&p) => {
                                let mut s = MotionStats::default();
                                s.sample(ep.last, p);
                                s
                            }
                            None => MotionStats::default(),
                        };
                        ep.windows.insert(
                            w.id,
                            Track {
                                stats,
                                moving: true,
                            },
                        );
                    }
                }
                let p = self.drawn_1to1(o, w.id);
                if let (Some(track), Some(p)) = (ep.windows.get_mut(&w.id), p) {
                    track.stats.sample(t, p);
                    track.moving = animating;
                }
            }
            if t > ep.last {
                ep.frames += 1;
            }
            ep.last = t;
            ep.still = !moving && !self.camera_moving(out) && self.panning() != Some(out);
            if ep.still && !(ep.kind == DRAG && self.previewing()) {
                self.emit_motion(&ep, false);
            } else {
                self.motion.episodes.insert(out, ep);
            }
        }
    }

    /// Ends `out`'s episode under way, if any, as another begins: `cut` when it had not
    /// settled. Returns whether there was one.
    fn end_motion(&mut self, out: OutputId) -> bool {
        let Some(ep) = self.motion.episodes.remove(&out) else {
            return false;
        };
        self.emit_motion(&ep, !ep.still);
        true
    }

    fn emit_motion(&mut self, ep: &Episode, cut: bool) {
        if ep.moved() {
            let line = self.motion_line(ep, cut);
            self.emit(line);
        }
    }

    fn motion_line(&self, ep: &Episode, cut: bool) -> Value {
        let moved: Vec<(&Id, &Track)> = ep
            .windows
            .iter()
            .filter(|(_, w)| w.stats.path >= MOVED_PX)
            .collect();
        let top = |of: fn(&MotionStats) -> f64| -> Value {
            moved
                .iter()
                .max_by(|a, b| of(&a.1.stats).total_cmp(&of(&b.1.stats)))
                .map_or(
                    Value::Null,
                    |&(&id, w)| json!({ "v": r(of(&w.stats)), "app": self.app_of(id), "id": id }),
                )
        };
        let path = moved.iter().fold(0., |sum, (_, w)| sum + w.stats.path);
        json!({
            "t": super::usage::now_ms(),
            "op": "motion",
            "seq": ep.seq,
            "kind": ep.kind,
            "cut": cut,
            "ms": r(ep.last.saturating_sub(ep.start).as_secs_f64() * 1000.),
            "frames": ep.frames,
            "scale": [r(ep.scale.0 * 1000.) / 1000., r(ep.scale.1 * 1000.) / 1000.],
            "camera": ep.camera.json(),
            "camera_zoomed": ep.zoomed.json(),
            "windows": {
                "moved": moved.len(),
                "path": r(path),
                "max_path": top(|s| s.path),
                "max_speed": top(|s| s.max_speed),
                "max_accel": top(|s| s.max_accel),
                "max_jump": top(|s| s.max_jump),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6 * b.abs().max(1.)
    }

    #[test]
    fn constant_speed_has_no_acceleration() {
        let mut s = MotionStats::default();
        for k in 0..=10 {
            s.sample(ms(16 * k), (3. * (16 * k) as f64, 4. * (16 * k) as f64));
        }
        assert!(close(s.max_speed, 5000.), "{s:?}");
        assert!(close(s.path, 5. * 160.), "{s:?}");
        assert!(close(s.max_jump, 5. * 16.), "{s:?}");
        assert!(s.max_accel < 1e-6, "{s:?}");
    }

    #[test]
    fn a_jump_is_the_largest_single_frame_move() {
        let mut s = MotionStats::at_rest(ms(0), (0., 0.));
        s.sample(ms(16), (0., 0.));
        s.sample(ms(16), (300., 400.));
        s.sample(ms(32), (300., 400.));
        assert!(close(s.max_jump, 500.), "{s:?}");
        assert!(close(s.path, 500.), "{s:?}");
        assert_eq!(s.max_speed, 0., "a move in no time has no speed: {s:?}");
        let mut s = MotionStats::at_rest(ms(0), (0., 0.));
        s.sample(ms(10), (100., 0.));
        s.sample(ms(20), (100., 0.));
        assert!(close(s.max_jump, 100.), "{s:?}");
        assert!(close(s.max_speed, 10_000.), "{s:?}");
    }

    #[test]
    fn steady_acceleration_from_rest() {
        let a = 2000.;
        let mut s = MotionStats::at_rest(ms(0), (0., 0.));
        for k in 1..=20 {
            let t = (8 * k) as f64 / 1000.;
            s.sample(ms(8 * k), (a * t * t / 2., 0.));
        }
        assert!(close(s.max_accel, a), "{s:?}");
        let t = 0.160;
        assert!(close(s.path, a * t * t / 2.), "{s:?}");
        assert!(
            close(s.max_speed, a * (t - 0.004)),
            "the last frame's mean speed: {s:?}"
        );
    }

    #[test]
    fn unknown_start_speed_counts_no_acceleration_on_the_first_frame() {
        let mut s = MotionStats::default();
        s.sample(ms(0), (0., 0.));
        s.sample(ms(10), (10., 0.));
        assert_eq!(s.max_accel, 0.);
        let mut s = MotionStats::at_rest(ms(0), (0., 0.));
        s.sample(ms(10), (10., 0.));
        assert!(close(s.max_accel, 1000. / 0.005), "{s:?}");
    }
}

#[cfg(all(test, feature = "telemetry"))]
mod desktop_tests {
    use std::time::{Duration, Instant};

    use newm_canvas::Config as CanvasConfig;
    use niri_config::{Action, PresetSize};
    use serde_json::Value;

    use super::super::view::ViewSettings;
    use super::super::{Desktop, Rect, Settings};

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            gaps: 10.,
            default_width: Some(PresetSize::Fixed(600)),
            default_height: Some(PresetSize::Proportion(0.5)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.set_view_settings(ViewSettings {
            duration: Duration::from_millis(200),
            movement: Duration::from_millis(200),
            ..ViewSettings::default()
        });
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        for id in [1, 2] {
            d.add_window(id);
            // The first configure leaves the size to the app, which picks none (0): the window
            // opens at the canvas's default size; the second asks for its cell.
            for _ in 0..2 {
                d.manage(now);
                for (id, size) in d.proposals() {
                    d.set_asked(id, size);
                    d.on_dimensions(id, size.0, size.1);
                }
            }
        }
        (d, now)
    }

    fn frames(d: &mut Desktop, from: Instant, ms: u64) {
        for k in 0..=ms / 16 {
            d.render(from + Duration::from_millis(16 * k));
        }
    }

    #[test]
    fn an_op_logs_its_camera_and_moves_then_one_motion_line() {
        let (mut d, now) = desktop();
        frames(&mut d, now, 1000);
        // Glides start on the clock: from here on the frames follow it.
        d.motion.lines.clear();
        d.motion.last_frame = None;
        let t = Instant::now();
        d.act(Action::MoveWindowDown, t);
        frames(&mut d, t, 1000);
        let lines = &d.motion.lines;
        let op = lines
            .iter()
            .find(|l| l["op"] != "motion")
            .expect("the op is logged");
        let seq = op["seq"].as_u64().unwrap();
        assert!(op["camera"]["jump"].is_array(), "{op}");
        let moved = op["moved"].as_array().unwrap();
        assert!(!moved.is_empty(), "the moved window is listed: {op}");
        assert!(moved[0]["d_px"][2].as_f64().unwrap() > 0.);
        let motion: Vec<&Value> = lines.iter().filter(|l| l["op"] == "motion").collect();
        assert_eq!(motion.len(), 1, "{lines:?}");
        let m = motion[0];
        assert_eq!(m["seq"].as_u64(), Some(seq));
        assert_eq!(m["cut"], false);
        assert!(m["windows"]["moved"].as_u64().unwrap() >= 1, "{m}");
        assert!(m["frames"].as_u64().unwrap() > 3, "{m}");
        let jump = m["windows"]["max_jump"]["v"].as_f64().unwrap();
        let path = m["windows"]["max_path"]["v"].as_f64().unwrap();
        assert!(jump < path, "the move glides, no one-frame jump: {m}");
    }

    #[test]
    fn a_pan_is_one_motion_episode_through_its_fling() {
        let (mut d, now) = desktop();
        frames(&mut d, now, 1000);
        d.motion.lines.clear();
        let t = now + Duration::from_millis(1000);
        for k in 1..=10u64 {
            let at = t + Duration::from_millis(10 * k);
            d.pan((500, 500), (-10., 0.), at);
            d.render(at);
        }
        d.pan_end(t + Duration::from_millis(110));
        frames(&mut d, t + Duration::from_millis(110), 4000);
        let motion: Vec<&Value> = d
            .motion
            .lines
            .iter()
            .filter(|l| l["op"] == "motion")
            .collect();
        assert_eq!(motion.len(), 1, "{:?}", d.motion.lines);
        assert_eq!(motion[0]["kind"], "pan-pointer");
        let path = motion[0]["camera"]["path"].as_f64().unwrap();
        assert!(path > 150., "the pan and its fling: {}", motion[0]);
    }

    #[test]
    fn pan_momentum_zero_stops_where_the_hand_stopped() {
        let (mut d, now) = desktop();
        d.settings.pan_momentum = 0.;
        frames(&mut d, now, 1000);
        d.motion.lines.clear();
        let t = now + Duration::from_millis(1000);
        for k in 1..=10u64 {
            let at = t + Duration::from_millis(10 * k);
            d.pan((500, 500), (-10., 0.), at);
            d.render(at);
        }
        d.pan_end(t + Duration::from_millis(110));
        frames(&mut d, t + Duration::from_millis(110), 4000);
        let motion: Vec<&Value> = d
            .motion
            .lines
            .iter()
            .filter(|l| l["op"] == "motion")
            .collect();
        assert_eq!(motion.len(), 1, "{:?}", d.motion.lines);
        let path = motion[0]["camera"]["path"].as_f64().unwrap();
        assert!(path >= 99., "the hand's 100 px: {}", motion[0]);
        assert!(
            path < 110.,
            "letting go does not carry the view on: {}",
            motion[0]
        );
    }
}
