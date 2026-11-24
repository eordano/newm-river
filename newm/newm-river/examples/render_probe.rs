//! Headless check of the river fork's `newm-river-render-v1` (driven by `render_probe.sh`).
//!
//! `render_probe run --scale S --opacity O --x X --y Y --width W --height H --shot FILE`
//! runs inside river as its window manager: it starts a red `foot`, places it at (X, Y), proposes
//! W x H, applies the scale and opacity, and once the window has drawn a buffer takes a `grim`
//! screenshot (PPM) and ends the session. foot snaps to its cell grid, so the size it drew is
//! written to FILE.size as "width height". With `--resize W2 H2` the probe proposes W2 x H2 once
//! the window has drawn, and shoots only after the app committed a buffer of the new size while
//! scaled: the path where river must rescale what wlroots rewrote on commit.
//!
//! Render protocol version 2 (a river built with scenefx): `--radius R` rounds the window's
//! corners; `--shadow` draws an opaque green shadow (softness 4, spread 24); `--snapshot` closes
//! the window once drawn, keeps a snapshot of it (drawn at half scale) and shoots, then releases
//! the snapshot and shoots FILE.released. `--op VINPUT` instead starts a pointer op with the
//! pointer 10 px from the output's right edge, moves it 400 px right with the virtual pointer and
//! writes the last op_delta to FILE.op ("dx dy").
//!
//! `render_probe check FILE X Y W H R G B` fails unless the box (2 px inset) is filled with the
//! colour, within 8 per channel.

#[path = "../src/protocols.rs"]
mod protocols;

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use protocols::newm_render::newm_river_render_v1::NewmRiverRenderV1;
use protocols::newm_render::newm_river_snapshot_v1::{self, NewmRiverSnapshotV1};
use protocols::newm_render::newm_river_window_render_v1::NewmRiverWindowRenderV1;
use protocols::window_management::river_node_v1::{self, RiverNodeV1};
use protocols::window_management::river_output_v1::{self, RiverOutputV1};
use protocols::window_management::river_seat_v1::{self, RiverSeatV1};
use protocols::window_management::river_window_manager_v1::{self, RiverWindowManagerV1};
use protocols::window_management::river_window_v1::{self, RiverWindowV1};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

#[derive(Clone)]
struct Args {
    scale: f64,
    opacity: f64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    shot: String,
    resize: Option<(i32, i32)>,
    radius: Option<u32>,
    shadow: bool,
    snapshot: bool,
    op: Option<String>,
}

struct Win {
    proxy: RiverWindowV1,
    node: RiverNodeV1,
    render: NewmRiverWindowRenderV1,
    proposed: bool,
    dimensions: (i32, i32),
    /// The size drawn before the resize was proposed.
    before_resize: Option<(i32, i32)>,
}

struct Probe {
    conn: Connection,
    wm: RiverWindowManagerV1,
    render: NewmRiverRenderV1,
    args: Args,
    window: Option<Win>,
    shooting: bool,
    running: bool,
    seat: Option<RiverSeatV1>,
    /// The last op_delta's dx and dy.
    op_delta: Arc<(AtomicI32, AtomicI32)>,
    op_started: bool,
    /// The virtual pointer moved to the edge (set from its thread).
    at_edge: Arc<AtomicBool>,
    edge_asked: bool,
    closing: bool,
    snapshot: Option<NewmRiverSnapshotV1>,
}

impl Probe {
    fn manage(&mut self) {
        let drawn = self.window.as_ref().is_some_and(|w| w.dimensions.0 > 0);
        if self.args.snapshot && drawn && !self.closing {
            self.closing = true;
            if let Some(w) = &self.window {
                w.proxy.close();
            }
        }
        // The op starts once the pointer is at the edge: river handles input queued during a
        // manage sequence after it, so the move to the edge runs outside one (see render).
        if let Some(seat) = &self.seat
            && self.at_edge.load(Ordering::Relaxed)
            && !self.op_started
        {
            self.op_started = true;
            seat.op_start_pointer();
        }
        if let Some(w) = &mut self.window
            && !w.proposed
        {
            w.proxy.use_ssd();
            w.proxy
                .propose_dimensions(self.args.width, self.args.height);
            w.proposed = true;
        } else if let Some(w) = &mut self.window
            && let Some((rw, rh)) = self.args.resize
            && w.before_resize.is_none()
            && w.dimensions.0 > 0
        {
            w.before_resize = Some(w.dimensions);
            w.proxy.propose_dimensions(rw, rh);
        }
        self.wm.manage_finish();
    }

    fn render(&mut self) {
        let mut drawn = false;
        if let Some(s) = &self.snapshot {
            s.set_scale(0.5);
        }
        if let Some(w) = &self.window {
            w.node.set_position(self.args.x, self.args.y);
            w.render.set_scale(self.args.scale);
            w.render.set_opacity(self.args.opacity);
            if let Some(r) = self.args.radius {
                w.render.set_corner_radius(r);
            }
            if self.args.shadow {
                w.render.set_shadow(0, u32::MAX, 0, u32::MAX, 4, 24, 0, 0);
            }
            drawn = w.dimensions.0 > 0
                && w.dimensions.1 > 0
                && (self.args.resize.is_none()
                    || w.before_resize.is_some_and(|before| before != w.dimensions));
        }
        if self.args.snapshot {
            drawn = self.snapshot.is_some();
        }
        if let Some(vinput) = self.args.op.clone()
            && drawn
            && !self.edge_asked
        {
            self.edge_asked = true;
            let (wm, conn, at_edge) = (self.wm.clone(), self.conn.clone(), self.at_edge.clone());
            std::thread::spawn(move || {
                let status = Command::new(&vinput)
                    .args(["abs", "1910", "540", "1920", "1080", "sleep", "100"])
                    .status();
                eprintln!("render_probe: pointer to (1910, 540): {status:?}");
                at_edge.store(true, Ordering::Relaxed);
                wm.manage_dirty();
                let _ = conn.flush();
            });
        }
        if self.args.op.is_some() {
            drawn = false;
            if self.op_started && !self.shooting {
                self.shooting = true;
                let (wm, conn, shot) = (self.wm.clone(), self.conn.clone(), self.args.shot.clone());
                let vinput = self.args.op.clone().unwrap_or_default();
                let delta = self.op_delta.clone();
                std::thread::spawn(move || {
                    let status = Command::new(&vinput)
                        .args([
                            "move", "200", "0", "sleep", "50", "move", "200", "0", "sleep", "300",
                        ])
                        .status();
                    eprintln!("render_probe: moved 400 px right: {status:?}");
                    std::thread::sleep(Duration::from_millis(300));
                    let (dx, dy) = (
                        delta.0.load(Ordering::Relaxed),
                        delta.1.load(Ordering::Relaxed),
                    );
                    eprintln!("render_probe: op_delta {dx} {dy}");
                    if let Err(err) = std::fs::write(format!("{shot}.op"), format!("{dx} {dy}\n")) {
                        eprintln!("render_probe: writing {shot}.op: {err}");
                    }
                    wm.exit_session();
                    let _ = conn.flush();
                });
            }
        }
        self.wm.render_finish();
        if drawn && !self.shooting {
            self.shooting = true;
            let (wm, conn, shot) = (self.wm.clone(), self.conn.clone(), self.args.shot.clone());
            let size = self.window.as_ref().map_or((0, 0), |w| w.dimensions);
            if let Err(err) =
                std::fs::write(format!("{shot}.size"), format!("{} {}\n", size.0, size.1))
            {
                eprintln!("render_probe: writing {shot}.size: {err}");
            }
            let snapshot = self.snapshot.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                let grim =
                    |file: &str| match Command::new("grim").args(["-t", "ppm", file]).status() {
                        Ok(status) if status.success() => eprintln!("render_probe: shot {file}"),
                        other => eprintln!("render_probe: grim failed: {other:?}"),
                    };
                grim(&shot);
                if let Some(s) = snapshot {
                    s.destroy();
                    let _ = conn.flush();
                    std::thread::sleep(Duration::from_millis(300));
                    grim(&format!("{shot}.released"));
                }
                wm.exit_session();
                let _ = conn.flush();
            });
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
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

impl Dispatch<RiverWindowManagerV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &RiverWindowManagerV1,
        event: river_window_manager_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_manager_v1::Event;
        match event {
            Event::Unavailable => {
                eprintln!("render_probe: another window manager runs");
                state.running = false;
            }
            Event::Finished => state.running = false,
            Event::ManageStart => state.manage(),
            Event::RenderStart => state.render(),
            Event::Seat { id } => state.seat = Some(id),
            Event::Window { id } if state.window.is_none() && !state.closing => {
                let node = id.get_node(qh, ());
                let render = state.render.get_window_render(&id, qh, ());
                state.window = Some(Win {
                    proxy: id,
                    node,
                    render,
                    proposed: false,
                    dimensions: (0, 0),
                    before_resize: None,
                });
            }
            _ => {}
        }
    }

    event_created_child!(Probe, RiverWindowManagerV1, [
        river_window_manager_v1::EVT_WINDOW_OPCODE => (RiverWindowV1, ()),
        river_window_manager_v1::EVT_OUTPUT_OPCODE => (RiverOutputV1, ()),
        river_window_manager_v1::EVT_SEAT_OPCODE => (RiverSeatV1, ()),
    ]);
}

impl Dispatch<RiverSeatV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &RiverSeatV1,
        event: river_seat_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let river_seat_v1::Event::OpDelta { dx, dy } = event {
            state.op_delta.0.store(dx, Ordering::Relaxed);
            state.op_delta.1.store(dy, Ordering::Relaxed);
        }
    }
}

impl Dispatch<NewmRiverSnapshotV1, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &NewmRiverSnapshotV1,
        event: newm_river_snapshot_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let newm_river_snapshot_v1::Event::Expired = event;
        eprintln!("render_probe: snapshot expired");
    }
}

impl Dispatch<RiverWindowV1, ()> for Probe {
    fn event(
        state: &mut Self,
        proxy: &RiverWindowV1,
        event: river_window_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let river_window_v1::Event::Closed = event
            && let Some(w) = state.window.take()
        {
            // The close animation's start: keep the last frame, then let the window go.
            state.snapshot = Some(w.render.snapshot(5000, qh, ()));
            w.render.destroy();
            w.node.destroy();
            w.proxy.destroy();
            eprintln!("render_probe: window closed, snapshot kept");
            return;
        }
        if let river_window_v1::Event::Dimensions { width, height } = event
            && let Some(w) = &mut state.window
            && &w.proxy == proxy
        {
            eprintln!("render_probe: window drawn at {width}x{height}");
            w.dimensions = (width, height);
        }
    }
}

macro_rules! ignore_events {
    ($($proxy:ty => $event:ty),* $(,)?) => {$(
        impl Dispatch<$proxy, ()> for Probe {
            fn event(_: &mut Self, _: &$proxy, _: $event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    RiverNodeV1 => river_node_v1::Event,
    RiverOutputV1 => river_output_v1::Event,
    NewmRiverRenderV1 => protocols::newm_render::newm_river_render_v1::Event,
    NewmRiverWindowRenderV1 => protocols::newm_render::newm_river_window_render_v1::Event,
);

fn flag<T: std::str::FromStr>(args: &[String], name: &str) -> Result<T> {
    let i = args
        .iter()
        .position(|a| a == name)
        .with_context(|| format!("missing {name}"))?;
    let value = args
        .get(i + 1)
        .with_context(|| format!("{name} needs a value"))?;
    value
        .parse()
        .ok()
        .with_context(|| format!("bad value for {name}: {value}"))
}

fn run(args: &[String]) -> Result<()> {
    let args = Args {
        scale: flag(args, "--scale")?,
        opacity: flag(args, "--opacity")?,
        x: flag(args, "--x")?,
        y: flag(args, "--y")?,
        width: flag(args, "--width")?,
        height: flag(args, "--height")?,
        shot: flag(args, "--shot")?,
        resize: match args.iter().position(|a| a == "--resize") {
            Some(i) => {
                let n = |j: usize| -> Result<i32> {
                    let v = args.get(i + j).context("--resize needs W H")?;
                    v.parse().with_context(|| format!("bad --resize value {v}"))
                };
                Some((n(1)?, n(2)?))
            }
            None => None,
        },
        radius: flag(args, "--radius").ok(),
        shadow: args.iter().any(|a| a == "--shadow"),
        snapshot: args.iter().any(|a| a == "--snapshot"),
        op: flag(args, "--op").ok(),
    };
    let conn = Connection::connect_to_env().context("no Wayland display: run inside river")?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn)?;
    let qh = queue.handle();
    let wm: RiverWindowManagerV1 = globals
        .bind(&qh, 4..=6, ())
        .context("river_window_manager_v1")?;
    let render: NewmRiverRenderV1 = globals
        .bind(&qh, 1..=2, ())
        .context("newm_river_render_v1: not the newm-river fork?")?;
    if (args.radius.is_some() || args.shadow || args.snapshot) && render.version() < 2 {
        bail!("newm_river_render_v1 version 2 needed: river built without scenefx?");
    }

    // A red window with no cursor, so every pixel of it is the background colour.
    Command::new("foot")
        .args([
            "-o",
            "colors-dark.background=ff0000",
            "-o",
            "csd.preferred=none",
            "sh",
            "-c",
            "printf '\\033[?25l'; sleep 60",
        ])
        .spawn()
        .context("starting foot")?;

    let mut probe = Probe {
        conn: conn.clone(),
        wm,
        render,
        args,
        window: None,
        shooting: false,
        running: true,
        seat: None,
        op_delta: Arc::new((AtomicI32::new(0), AtomicI32::new(0))),
        op_started: false,
        at_edge: Arc::new(AtomicBool::new(false)),
        edge_asked: false,
        closing: false,
        snapshot: None,
    };
    while probe.running {
        queue.blocking_dispatch(&mut probe)?;
    }
    Ok(())
}

fn check(args: &[String]) -> Result<()> {
    let [file, x, y, w, h, r, g, b] = args else {
        bail!("usage: render_probe check FILE X Y W H R G B");
    };
    let n = |s: &String| -> Result<i64> { s.parse().with_context(|| format!("not a number: {s}")) };
    let (x, y, w, h) = (n(x)?, n(y)?, n(w)?, n(h)?);
    let want = [n(r)?, n(g)?, n(b)?];

    let data = std::fs::read(file).with_context(|| format!("reading {file}"))?;
    // Binary PPM: "P6" width height maxval, whitespace separated, then RGB bytes.
    let mut fields = Vec::new();
    let mut pos = 0;
    while fields.len() < 4 {
        while data.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        let start = pos;
        while data.get(pos).is_some_and(|c| !c.is_ascii_whitespace()) {
            pos += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..pos]).into_owned());
    }
    pos += 1;
    if fields[0] != "P6" || fields[3] != "255" {
        bail!("{file}: not an 8-bit binary PPM");
    }
    let (iw, ih): (i64, i64) = (fields[1].parse()?, fields[2].parse()?);
    let pixels = &data[pos..];

    let (mut total, mut good) = (0u64, 0u64);
    for py in (y + 2)..(y + h - 2) {
        for px in (x + 2)..(x + w - 2) {
            if px < 0 || py < 0 || px >= iw || py >= ih {
                bail!("box {x},{y} {w}x{h} leaves the {iw}x{ih} image");
            }
            let i = ((py * iw + px) * 3) as usize;
            total += 1;
            if (0..3).all(|c| (i64::from(pixels[i + c]) - want[c]).abs() <= 8) {
                good += 1;
            }
        }
    }
    let ratio = good as f64 / total.max(1) as f64;
    println!(
        "{file}: box {x},{y} {w}x{h}: {good}/{total} pixels are {want:?} ({:.2}%)",
        ratio * 100.0
    );
    if ratio < 0.99 {
        bail!("box not filled with {want:?}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run(&args[1..]),
        Some("check") => check(&args[1..]),
        _ => bail!("usage: render_probe run ... | render_probe check ..."),
    }
}
