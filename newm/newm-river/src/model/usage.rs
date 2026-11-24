//! The usage log: one JSON line per rearrangement of a
//! canvas's windows, appended to `$XDG_STATE_HOME/newm-river/usage.jsonl`
//! (`~/.local/state/newm-river/usage.jsonl`), so the placement weights can be tuned on real use
//! later. A line holds the time, the operation, the app it acted on, every window's tiles before
//! and after by app ID, and the cost the placement rules give the change. Never a window title.
//! It also holds the op's number in this session (`seq`), the camera's view before the op and
//! where it goes, at 1:1 and at the zoom, the camera's jump, and how far each window that moved
//! went, in tiles and in 1:1 pixels; the motion the op then draws is a later
//! `{"op":"motion"}` line with the same `seq` (see `motion.rs`).
//!
//! Every op line has `op_us`, the time the canvas took; an `open`
//! line says where the span came from (`size_source`), the size the app had committed
//! (`app_px`), the span, whether it took the focus, the time since the last open and, for a
//! placeholder, how it matched and how long it waited (`restore`); `app-grow` lines follow an app
//! that grows its window (`model.rs` `sync_mins`); `settle`/`unsettled` lines follow a new
//! window to its cell's size (`open.rs`); `float` lines say why a new window floats or tiles;
//! `restore-expired` lines name an app whose placeholder timed out; a `rematch` op and its
//! `restore-rematch` line follow a restored window trading spots by title (`rematch.rs`;
//! across outputs, a `rematch` op per canvas and the line names the window's `output`
//! and the one it went to, `with_output`); lines written during a drag
//! carry `during_drag` and a drop says whether the dragged window's home moved (`home_moved`).
//! Lines go to one writer thread through a bounded queue; a full queue drops lines and the next
//! line queued counts them in `dropped`.
//!
//! Only a build with the `telemetry` feature writes the log; without it no
//! line is made, timed or written, and no writer thread starts.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use newm_canvas::{place, Canvas, Id, Rect, Window};
use newm_kde::apps;
use serde_json::{json, Value};

use super::motion::{next_seq, r, Cause};
use super::{Desktop, OutputId};
use crate::anim::Camera;

/// Whether this build writes the usage log: the `telemetry` feature.
pub(super) const TELEMETRY: bool = cfg!(feature = "telemetry");

impl Desktop {
    /// The app of window `id`, as [`apps::canonical`] names it.
    pub(super) fn app_of(&self, id: Id) -> Option<String> {
        self.windows
            .get(&id)
            .and_then(|w| w.app_id.as_deref())
            .map(apps::canonical)
    }

    /// Runs `op` through [`Self::apply`] on `out`'s canvas and logs it as `name` when it moved,
    /// opened or closed a window, or is a cancelled drag; `mover` is the window the user acted
    /// on.
    pub(super) fn logged<R>(
        &mut self,
        name: &str,
        out: Option<OutputId>,
        mover: Option<Id>,
        op: impl FnOnce(&mut Canvas) -> R,
    ) -> Option<R> {
        self.logged_with(name, out, mover, op, |_, _| Value::Null)
    }

    /// [`Self::logged`], adding to the line the fields of the object `extra` gives from the
    /// desktop and the op's result once the op ran.
    pub(super) fn logged_with<R>(
        &mut self,
        name: &str,
        out: Option<OutputId>,
        mover: Option<Id>,
        op: impl FnOnce(&mut Canvas) -> R,
        extra: impl FnOnce(&Self, &R) -> Value,
    ) -> Option<R> {
        if !TELEMETRY {
            return self.apply(out, op);
        }
        let before: Vec<Window> = out
            .and_then(|o| self.output(o))
            .map(|o| o.canvas.windows().to_vec())
            .unwrap_or_default();
        let apps: Vec<(Id, Option<String>)> =
            before.iter().map(|w| (w.id, self.app_of(w.id))).collect();
        let app = mover.and_then(|m| self.app_of(m));
        let seq = next_seq();
        let cam = out
            .and_then(|o| self.output(o))
            .map(|o| (self.camera(o), o.canvas.zoom()));
        let drawn = out.and_then(|o| self.drawn_now(o));
        let (rv, took) = self.apply(out, |c| {
            let start = Instant::now();
            let rv = op(c);
            (rv, start.elapsed())
        })?;
        let o = self.output(out?)?;
        let after = o.canvas.windows();
        let changed = before.len() != after.len()
            || before.iter().any(|w| {
                after
                    .iter()
                    .find(|a| a.id == w.id)
                    .is_none_or(|a| a.rect != w.rect)
            });
        let target = Camera {
            scale: o.canvas.scale(),
            view: o.canvas.view(),
        };
        let zoom = o.canvas.zoom();
        let jumped = cam.is_some_and(|(c, _)| c != target);
        if changed || name == "cancel" {
            let mut line = self.usage_line(name, out, mover, app, &before, &apps);
            if let Value::Object(m) = &mut line {
                m.insert("seq".into(), json!(seq));
                if let Some((c, z)) = cam {
                    m.insert("camera".into(), camera_json(c, z, target, zoom));
                }
                m.insert("moved".into(), self.moved_json(out, &before, &apps));
                m.insert("op_us".into(), json!(took.as_micros() as u64));
                if let Value::Object(extra) = extra(self, &rv) {
                    m.extend(extra);
                }
            }
            self.emit(line);
        }
        if (changed || jumped || name == "cancel")
            && let (Some(o), Some(drawn)) = (out, drawn)
        {
            self.begin_motion(o, Cause::op(name, seq), drawn);
        }
        Some(rv)
    }

    /// How far each window that stayed moved: in tiles and in 1:1 canvas pixels.
    fn moved_json(
        &self,
        out: Option<OutputId>,
        before: &[Window],
        apps: &[(Id, Option<String>)],
    ) -> Value {
        let Some(o) = out.and_then(|o| self.output(o)) else {
            return json!([]);
        };
        let moved: Vec<Value> = before
            .iter()
            .filter_map(|w| {
                let (from, to) = (w.rect, o.canvas.rect(w.id)?);
                let (a, b) = (self.cell(&o.canvas, from), self.cell(&o.canvas, to));
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                if dx == 0. && dy == 0. {
                    return None;
                }
                let (di, dj) = (to.i - from.i, to.j - from.j);
                let app = apps.iter().find(|a| a.0 == w.id).and_then(|a| a.1.clone());
                Some(json!({
                    "app": app,
                    "id": w.id,
                    "d_tiles": [di, dj, r(f64::from(di).hypot(f64::from(dj)))],
                    "d_px": [r(dx), r(dy), r(dx.hypot(dy))],
                }))
            })
            .collect();
        Value::Array(moved)
    }

    fn usage_line(
        &self,
        name: &str,
        out: Option<OutputId>,
        mover: Option<Id>,
        app: Option<String>,
        before: &[Window],
        apps: &[(Id, Option<String>)],
    ) -> Value {
        let Some(canvas) = out.and_then(|o| self.output(o)).map(|o| &o.canvas) else {
            return Value::Null;
        };
        usage_line(canvas, name, mover, app, before, |id| {
            apps.iter()
                .find(|a| a.0 == id)
                .and_then(|a| a.1.clone())
                .or_else(|| self.app_of(id))
        })
    }
}

/// One line of the log: `canvas` is after the change, `before` its windows before it.
pub fn usage_line(
    canvas: &Canvas,
    name: &str,
    mover: Option<Id>,
    app: Option<String>,
    before: &[Window],
    app_of: impl Fn(Id) -> Option<String>,
) -> Value {
    let after = canvas.windows();
    let tiles = |r: Rect| json!([r.i, r.j, r.w, r.h]);
    let list = |ws: &[Window]| -> Vec<Value> {
        ws.iter()
            .map(|w| json!({ "app": app_of(w.id), "id": w.id, "tiles": tiles(w.rect) }))
            .collect()
    };
    let kept: Vec<Window> = before
        .iter()
        .filter(|w| after.iter().any(|a| a.id == w.id))
        .copied()
        .collect();
    let rects: Vec<Rect> = kept
        .iter()
        .filter_map(|w| after.iter().find(|a| a.id == w.id).map(|a| a.rect))
        .collect();
    let config = canvas.config();
    let movers: Vec<Id> = mover.into_iter().collect();
    let cost = place::cost(
        &kept,
        &rects,
        &movers,
        config.distance_weight,
        config.diagonal_weight,
    );
    let islands = |ws: &[Window]| {
        place::islands(ws, &[])
            .values()
            .collect::<HashSet<_>>()
            .len()
    };
    json!({
        "t": now_ms(),
        "op": name,
        "app": app.as_deref().map(apps::canonical),
        "window": mover,
        "zoom": canvas.zoom(),
        "before": list(before),
        "after": list(after),
        "tiles": cost.tiles,
        "distance": cost.distance,
        "diagonal": cost.diagonal,
        "islands": [islands(before), islands(after)],
        "weights": [config.distance_weight, config.diagonal_weight],
    })
}

/// The camera's view before an op and where it goes, at 1:1 and at the zoom, and its jump.
fn camera_json(before: Camera, zoom_before: i32, after: Camera, zoom_after: i32) -> Value {
    let one = |c: Camera| (c.view.0 / c.scale, c.view.1 / c.scale);
    let (b, a) = (one(before), one(after));
    let (jx, jy) = (a.0 - b.0, a.1 - b.1);
    json!({
        "before": [r(b.0), r(b.1)],
        "after": [r(a.0), r(a.1)],
        "before_zoomed": [r(before.view.0), r(before.view.1)],
        "after_zoomed": [r(after.view.0), r(after.view.1)],
        "jump": [r(jx), r(jy), r(jx.hypot(jy))],
        "zoom": [zoom_before, zoom_after],
        "scale": [before.scale, after.scale],
    })
}

pub(super) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// When the last window opened, in ms since the epoch; 0 before the first.
static LAST_OPEN: AtomicU64 = AtomicU64::new(0);

/// The ms since the last window opened (`None` for the first), and now as the last.
pub(super) fn since_last_open() -> Option<u64> {
    if !TELEMETRY {
        return None;
    }
    let now = now_ms() as u64;
    let last = LAST_OPEN.swap(now, Ordering::Relaxed);
    (last != 0).then(|| now.saturating_sub(last))
}

/// The `float` line for a new window: whether it floats or tiles and why (`decision`: `tiled`,
/// `dialog` for a child, `popup-like` for a fixed-size or rule-floated window, `overlay`), with
/// its app (as [`apps::canonical`] names it; `app_id` is the window's own), minimum and maximum
/// size, parent, the logical size of the output it opens on and whether a rule decided.
/// `max_huge` flags a maximum of 32767 or more on an axis, which X11 toolkits write on the axis
/// they do not cap; `max_fits` whether the maximum fits within the output, which an overlay's
/// must; `x11` whether Xwayland serves it.
pub(super) fn float_line(
    w: &super::Window,
    decision: &str,
    by_rule: bool,
    output: Option<(i32, i32)>,
) -> Value {
    let x11 = w.pid.is_some_and(|pid| {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .is_ok_and(|exe| exe.to_string_lossy().to_lowercase().contains("xwayland"))
    });
    json!({
        "t": now_ms(),
        "op": "float",
        "app": w.app_id.as_deref().map(apps::canonical),
        "app_id": w.app_id,
        "decision": decision,
        "by_rule": by_rule,
        "min": [w.min.0, w.min.1],
        "max": [w.max.0, w.max.1],
        "has_parent": w.parent.is_some(),
        "max_huge": w.max.0 >= 32767 || w.max.1 >= 32767,
        "output": output.map(|o| [o.0, o.1]),
        "max_fits": crate::rules::cap_fits(w.max, output),
        "x11": x11,
    })
}

/// Without the `telemetry` feature the usage log is compiled out: nothing is written.
#[cfg(not(feature = "telemetry"))]
pub(super) fn append(_line: &Value) {}

#[cfg(feature = "telemetry")]
pub(super) use file::append;

/// The usage log file and its writer thread, built with the `telemetry` feature only.
#[cfg(feature = "telemetry")]
mod file {
    use std::fs::{File, OpenOptions};
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::OnceLock;

    use serde_json::{json, Value};

    fn path() -> Option<PathBuf> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        Some(state.join("newm-river/usage.jsonl"))
    }

    /// Lines the writer thread may have queued before new ones are dropped.
    const QUEUE: usize = 4096;

    /// Lines dropped since the last one queued.
    static DROPPED: AtomicU64 = AtomicU64::new(0);

    /// The writer thread's queue; `None` when the thread could not start.
    static WRITER: OnceLock<Option<SyncSender<String>>> = OnceLock::new();

    /// Queues one line for the writer thread; a full queue drops it, counted in the next line
    /// queued as `dropped`. A log that cannot be written is skipped, never an error. Tests write
    /// nothing.
    pub(crate) fn append(line: &Value) {
        if cfg!(test) || line.is_null() {
            return;
        }
        let Some(tx) = WRITER.get_or_init(spawn_writer) else {
            return;
        };
        let mut line = line.clone();
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            line["dropped"] = json!(dropped);
        }
        if tx.try_send(line.to_string()).is_err() {
            DROPPED.fetch_add(dropped + 1, Ordering::Relaxed);
        }
    }

    /// The writer thread: it opens the log once, again after a write fails, and a fresh one once the
    /// log reaches [`ROTATE_AT`].
    fn spawn_writer() -> Option<SyncSender<String>> {
        let (tx, rx) = sync_channel::<String>(QUEUE);
        std::thread::Builder::new()
            .name("usage-log".into())
            .spawn(move || {
                let Some(path) = path() else {
                    return;
                };
                let mut log = Log::new(path, ROTATE_AT);
                for line in rx {
                    log.write(&line);
                }
            })
            .ok()?;
        Some(tx)
    }

    /// Bytes the log holds before it moves to `usage.jsonl.1` (replacing the one there) and a fresh
    /// one starts: motion lines alone add megabytes a day, and the log is never read back.
    const ROTATE_AT: u64 = 64 << 20;

    /// The open usage log and how many bytes it holds.
    struct Log {
        path: PathBuf,
        rotate_at: u64,
        file: Option<File>,
        len: u64,
    }

    impl Log {
        fn new(path: PathBuf, rotate_at: u64) -> Self {
            Self {
                path,
                rotate_at,
                file: None,
                len: 0,
            }
        }

        fn write(&mut self, line: &str) {
            if self.file.is_none() {
                if let Some(dir) = self.path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                self.file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .ok();
                self.len = self
                    .file
                    .as_ref()
                    .and_then(|f| f.metadata().ok())
                    .map_or(0, |m| m.len());
            }
            let Some(f) = &mut self.file else {
                return;
            };
            if writeln!(f, "{line}").is_err() {
                self.file = None;
                return;
            }
            self.len += line.len() as u64 + 1;
            if self.len >= self.rotate_at {
                self.file = None;
                let mut old = self.path.clone().into_os_string();
                old.push(".1");
                let _ = std::fs::rename(&self.path, old);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_log_moves_aside_once_it_is_full() {
            let dir = std::env::temp_dir().join(format!("newm-river-usage-{}", std::process::id()));
            let path = dir.join("usage.jsonl");
            let mut log = Log::new(path.clone(), 30);
            for k in 0..5 {
                log.write(&format!("{{\"k\":{k}}}"));
            }
            let old = std::fs::read_to_string(dir.join("usage.jsonl.1")).unwrap();
            let new = std::fs::read_to_string(&path).unwrap();
            assert_eq!(old, "{\"k\":0}\n{\"k\":1}\n{\"k\":2}\n{\"k\":3}\n");
            assert_eq!(new, "{\"k\":4}\n");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}

#[cfg(all(test, feature = "telemetry"))]
mod tests {
    use newm_canvas::Config;

    use super::*;

    #[test]
    fn a_line_names_the_op_the_apps_and_the_tiles_never_a_title() {
        let mut c = Canvas::new(Config::default(), 1280., 720.);
        c.add(1, 10., 10., Some((2, 2)));
        let before = c.windows().to_vec();
        c.add(2, 10., 10., Some((2, 2)));
        let line = usage_line(&c, "open", Some(2), Some("foot".into()), &before, |id| {
            Some(format!("app{id}"))
        });
        assert_eq!(line["op"], "open");
        assert_eq!(line["app"], "foot");
        assert_eq!(line["window"], 2);
        assert_eq!(line["before"].as_array().unwrap().len(), 1);
        assert_eq!(line["after"].as_array().unwrap().len(), 2);
        assert_eq!(line["after"][1]["app"], "app2");
        assert_eq!(line["after"][1]["tiles"].as_array().unwrap().len(), 4);
        assert!(!line.to_string().contains("title"));
    }
}
