//! Session save and restore: newm-river remembers which windows are
//! open and where, and at every start relaunches their apps and puts each window back on its
//! saved tiles.
//!
//! Wayland clients die with the compositor, so the windows are saved to
//! `$XDG_STATE_HOME/newm-river/session.json` (`~/.local/state` without it; readable by the user
//! only: titles and command lines are private) once a change has held for [`SAVE_INTERVAL`], and
//! at a quit the user asks for. A teardown (logout, reboot) that closes windows faster than that
//! is never saved, so the last settled session is what comes back after it, as after a crash.
//!
//! At start, after the config's `spawn-at-startup` programs, once the outputs are known, each
//! saved canvas gets a placeholder on every window's saved tiles, every saved process is launched
//! once (a Chromium-family browser told to restore its last session, see [`restoring`]), and once
//! the windows took their placeholders (or [`RESTORE_LIFETIME`] passed) each canvas takes its
//! saved focus, zoom and view. A Ghostty window comes back running the tmux session or ssh
//! command it showed (see [`terminals`]).
//!
//! Only the session on a TTY is saved and restored: a nested newm-river (a window of another
//! session) would relaunch the outer session's apps there and overwrite its saved session. river
//! on a TTY runs its DRM backend and holds a DRM primary node (`/dev/dri/card*`) open; nested
//! (Wayland or X11 backend) or headless, it does not. `NEWM_RIVER_SESSION=tty` or `nested`
//! overrides the test (the e2e uses it).
//!
//! A window manager started again after a crash (`supervise.rs`) takes over a running session:
//! it launches nothing, and the windows river hands it take the spots the session file saved
//! ([`Saver::adopt`]).
//!
//! The file is written by a thread of its own: the write ends in an fsync, which a busy disk can
//! hold for a second, and river waits on the window manager for every manage sequence.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use newm_canvas::Rect as Tiles;
use newm_kde::{apps, exec};
use niri_config::utils::RegexEq;
use niri_config::{Config, SpawnAtStartup, SpawnShAtStartup};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::model::{Desktop, Effect, Place, Window};
use crate::placeholder::{RestoreCanvas, RestoreWindow};

mod terminals;
use terminals::{Ask, Terminals};

/// How long a change must hold before it is saved; the session is written at most this often.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(2);

/// How long a restore waits for its windows before it drops the placeholders left.
pub const RESTORE_LIFETIME: Duration = Duration::from_secs(20);

const OUTPUT_WAIT: Duration = Duration::from_millis(500);
const OUTPUT_TRIES: u32 = 20;

/// Parts of app IDs (any case) of prompts, agents and lockers, which are never saved nor
/// relaunched.
const NEVER: &[&str] = &[
    "polkit",
    "pinentry",
    "askpass",
    "xdg-desktop-portal",
    "org.freedesktop.impl.portal",
    "kwallet",
    "ksecretd",
    "org.kde.kded",
    "swaylock",
    "gtklock",
    "hyprlock",
    "newm-river-lock",
];

/// Programs whose windows belong to other processes: their command lines bring back nothing.
const HELPERS: &[&str] = &["xwayland-satellite", "Xwayland"];

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub canvases: Vec<SavedCanvas>,
    /// What brings the windows back: one launch per process, or per window for a terminal.
    pub launches: Vec<Launch>,
}

/// One output's canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedCanvas {
    /// The output's connector name.
    pub output: Option<String>,
    pub zoom: i32,
    /// The camera's view, in logical pixels at 1:1.
    pub view: (f64, f64),
    /// Index of the focused window in `windows`.
    pub focus: Option<usize>,
    pub windows: Vec<SavedWindow>,
    pub floating: Vec<SavedFloating>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub app_id: String,
    pub title: Option<String>,
    /// Its tiles: column, row, width and height.
    pub rect: (i32, i32, i32, i32),
    /// Index in [`Session::launches`] of what brings it back.
    pub launch: Option<usize>,
}

/// A floating window: saved, and back when a tiled window's launch brings its process back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFloating {
    pub app_id: String,
    pub title: Option<String>,
    /// Its position on the output and its size, in logical pixels.
    pub pos: (f64, f64),
    pub size: (f64, f64),
    pub launch: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Launch {
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// How a window comes back: with its process, launched once for all its windows, or on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relaunch {
    Process(Launch),
    Window(Launch),
}

/// A window as the saver finds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The window's ID.
    pub id: u64,
    pub app_id: String,
    pub title: Option<String>,
    pub pid: Option<i32>,
}

/// Where a found window was.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Spot {
    Tiled(Tiles),
    Floating((f64, f64), (f64, f64)),
}

/// Saves the session and puts it back at start (see the module docs).
#[derive(Debug)]
pub struct Saver {
    path: Option<PathBuf>,
    /// Program names the config starts itself: their launches are left to it.
    startup: Vec<String>,
    started: Instant,
    next_tick: Instant,
    /// Whether the saved session was put back (or there was none): before that, saving would
    /// overwrite it.
    ready: bool,
    /// The session last found, written once it holds for an interval.
    seen: Option<String>,
    written: Option<String>,
    resolver: Resolver,
    terminals: Terminals,
    /// This window manager took over a running session ([`Self::adopt`]): no launches.
    adopting: bool,
    writer: Option<mpsc::Sender<Job>>,
}

/// A session file to write, and who waits for it.
struct Job {
    path: PathBuf,
    json: String,
    done: Option<mpsc::Sender<io::Result<()>>>,
}

/// How long a quit waits for the session file to be written.
const QUIT_WRITE_WAIT: Duration = Duration::from_secs(3);

/// The thread that writes session files, in the order they come.
fn writer() -> Option<mpsc::Sender<Job>> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("session-save".into())
        .spawn(move || {
            for job in rx {
                let res = write_private(&job.path, &job.json);
                if let Err(e) = &res {
                    warn!("saving the session to {}: {e}", job.path.display());
                }
                if let Some(done) = job.done {
                    let _ = done.send(res);
                }
            }
        })
        .inspect_err(|e| warn!("session writer thread: {e}"))
        .ok()?;
    Some(tx)
}

impl Saver {
    pub fn new(path: Option<PathBuf>, startup: Vec<String>, now: Instant) -> Self {
        Self {
            path,
            startup,
            started: now,
            next_tick: now,
            ready: false,
            seen: None,
            written: None,
            resolver: Resolver::default(),
            terminals: Terminals::default(),
            adopting: false,
            writer: None,
        }
    }

    /// A saver for a window manager that takes over a running session after a crash: it puts
    /// the saved spots back for the windows already open ([`Self::adopt`]) and launches nothing.
    pub fn adopting(mut self) -> Self {
        self.adopting = true;
        self
    }

    /// Whether [`Self::adopt`] can put the saved canvases on their outputs now.
    pub fn adopt_ready(&self, desktop: &Desktop) -> bool {
        outputs_known(desktop)
    }

    /// Puts each saved canvas's placeholders back, for the windows of the running session to
    /// take as they are placed; launches nothing. Once only, then saving resumes.
    pub fn adopt(&mut self, now: Instant, desktop: &mut Desktop, config: &Config) {
        if self.ready {
            return;
        }
        self.ready = true;
        let canvas = &config.layout.canvas;
        if !canvas.restore_session {
            return;
        }
        let Some(session) = self.load() else {
            return;
        };
        let (canvases, _) = plan(session, &canvas.restore_exclude, &self.startup);
        info!(
            "taking over the running session: {} saved canvases, no launches",
            canvases.len()
        );
        for (output, saved) in canvases {
            desktop.restore(output.as_deref(), saved, RESTORE_LIFETIME, now);
        }
    }

    /// The host the terminal window `id` is connected to with ssh, from the terminal finder's
    /// last answer; `None` for a window that shows something local, or that it has not answered
    /// for yet.
    pub fn remote_host(&self, id: u64) -> Option<String> {
        terminals::ssh_host(&self.terminals.command(id)?)
    }

    /// When the loop must call [`Self::poll`] next.
    pub fn deadline(&self) -> Instant {
        self.next_tick
    }

    /// Puts the saved session back once the outputs are known (or waited for long enough), then
    /// saves the windows as they change. Returns the launches to run.
    pub fn poll(&mut self, now: Instant, desktop: &mut Desktop, config: &Config) -> Vec<Effect> {
        if now < self.next_tick {
            return Vec::new();
        }
        self.next_tick = now + SAVE_INTERVAL;
        if self.ready {
            self.tick(desktop, config);
            // The minimap names an ssh window by its host.
            desktop.set_remote_hosts(|id| self.remote_host(id));
            return Vec::new();
        }
        if self.adopting {
            // The manage sequence adopts the running session's windows (river.rs).
            return Vec::new();
        }
        let waited = now.duration_since(self.started) >= OUTPUT_WAIT * OUTPUT_TRIES;
        if !outputs_known(desktop) && !waited {
            self.next_tick = now + OUTPUT_WAIT;
            return Vec::new();
        }
        self.ready = true;
        self.restore(now, desktop, config)
    }

    /// Saves the session now, the user quitting; skipped before the saved one is back. Waits
    /// (up to [`QUIT_WRITE_WAIT`]) for the file, and for the writes queued before it.
    pub fn save_now(&mut self, desktop: &Desktop, config: &Config) {
        let Some(json) = self.current(desktop, config) else {
            return;
        };
        let Some(path) = self.path.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let job = Job {
            path: path.clone(),
            json: json.clone(),
            done: Some(tx),
        };
        if self.send(job) {
            match rx.recv_timeout(QUIT_WRITE_WAIT) {
                Ok(Ok(())) => self.written = Some(json),
                Ok(Err(_)) => {}
                Err(_) => warn!("saving the session to {}: timed out", path.display()),
            }
            return;
        }
        match write_private(&path, &json) {
            Ok(()) => self.written = Some(json),
            Err(e) => warn!("saving the session to {}: {e}", path.display()),
        }
    }

    /// Queues a job on the writer thread (started on first use); false if there is none.
    fn send(&mut self, job: Job) -> bool {
        if self.writer.is_none() {
            self.writer = writer();
        }
        match &self.writer {
            Some(tx) => tx.send(job).is_ok(),
            None => false,
        }
    }

    /// Saves the session when it changed and the change held since the last tick.
    fn tick(&mut self, desktop: &Desktop, config: &Config) {
        let Some(json) = self.current(desktop, config) else {
            return;
        };
        if self.seen.as_ref() == Some(&json) {
            if self.written.as_ref() != Some(&json) {
                self.write(json);
            }
        } else {
            self.seen = Some(json);
        }
    }

    fn current(&mut self, desktop: &Desktop, config: &Config) -> Option<String> {
        let canvas = &config.layout.canvas;
        // No output (a VT switch away, the only monitor off) holds the windows nowhere a
        // snapshot sees: saving then would write an empty session.
        if !self.ready
            || !canvas.restore_session
            || desktop.placeholders.is_restoring()
            || !desktop.has_outputs()
        {
            return None;
        }
        let home = home();
        self.resolver.processes.clear();
        let mut ask = Ask::default();
        let terminals = &self.terminals;
        let session = snapshot(desktop, &canvas.restore_exclude, |w| {
            if let Some(terminal) = terminals::find(&w.app_id) {
                ask.windows.push(terminals::Window {
                    id: w.id,
                    pid: w.pid,
                    terminal,
                    title: w
                        .title
                        .as_deref()
                        .map(|t| terminal.title(t).to_owned())
                        .unwrap_or_default(),
                });
            }
            self.resolver.relaunch(w, &home, terminals)
        });
        self.terminals.ask(ask);
        serde_json::to_string_pretty(&session)
            .inspect_err(|e| warn!("serializing the session: {e}"))
            .ok()
    }

    /// Writes the session on the writer thread (a failure is logged there), else right here.
    fn write(&mut self, json: String) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let job = Job {
            path: path.clone(),
            json: json.clone(),
            done: None,
        };
        if self.send(job) {
            self.written = Some(json);
            return;
        }
        match write_private(&path, &json) {
            Ok(()) => self.written = Some(json),
            Err(e) => warn!("saving the session to {}: {e}", path.display()),
        }
    }

    fn load(&self) -> Option<Session> {
        let path = self.path.as_ref()?;
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn!("reading the session {}: {e}", path.display());
                return None;
            }
        };
        serde_json::from_str(&text)
            .inspect_err(|e| warn!("parsing the session {}: {e}", path.display()))
            .ok()
    }

    fn restore(&mut self, now: Instant, desktop: &mut Desktop, config: &Config) -> Vec<Effect> {
        let canvas = &config.layout.canvas;
        if !canvas.restore_session {
            return Vec::new();
        }
        let Some(session) = self.load() else {
            return Vec::new();
        };
        let (canvases, launches) = plan(session, &canvas.restore_exclude, &self.startup);
        info!(
            "restoring the session: {} canvases, {} launches",
            canvases.len(),
            launches.len()
        );
        for (output, saved) in canvases {
            desktop.restore(output.as_deref(), saved, RESTORE_LIFETIME, now);
        }
        launches.iter().map(|l| launch(l, Vec::new())).collect()
    }
}

/// Every output has its area and its connector name.
fn outputs_known(desktop: &Desktop) -> bool {
    !desktop.outputs().is_empty()
        && desktop
            .outputs()
            .iter()
            .all(|o| o.area.w > 0 && !o.name.starts_with("OUT-"))
}

/// The saver of the session on a TTY; None for a nested or headless river.
pub fn saver(river: BorrowedFd, config: &Config, now: Instant) -> Option<Saver> {
    let tty = match std::env::var("NEWM_RIVER_SESSION").as_deref() {
        Ok("tty") => true,
        Ok("nested") => false,
        _ => river_on_tty(river),
    };
    if !tty {
        info!("not restoring nor saving the session of a nested or headless river");
        return None;
    }
    let startup = startup_programs(&config.spawn_at_startup, &config.spawn_sh_at_startup);
    Some(Saver::new(session_path(), startup, now))
}

/// Whether the river at the other end of the Wayland socket runs on a TTY: it holds a DRM
/// primary node open.
fn river_on_tty(fd: BorrowedFd) -> bool {
    let Some(pid) = peer_pid(fd) else {
        warn!("no peer credentials on the Wayland socket");
        return false;
    };
    match fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(dir) => holds_drm_card(dir.flatten().filter_map(|e| fs::read_link(e.path()).ok())),
        Err(e) => {
            warn!("reading river's open files (pid {pid}): {e}");
            false
        }
    }
}

pub fn holds_drm_card(links: impl IntoIterator<Item = PathBuf>) -> bool {
    links
        .into_iter()
        .any(|l| l.to_str().is_some_and(|s| s.starts_with("/dev/dri/card")))
}

fn peer_pid(fd: BorrowedFd) -> Option<i32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred and len are valid for writes of a ucred, the size given.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid)
}

/// `$XDG_STATE_HOME/newm-river/session.json`, `~/.local/state` without it.
pub fn session_path() -> Option<PathBuf> {
    let env = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let state = env("XDG_STATE_HOME").or_else(|| env("HOME").map(|h| h.join(".local/state")))?;
    Some(state.join("newm-river").join("session.json"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// Writes `text` to `path` through a temporary file and a rename, both mode 0600 in a directory
/// only the user can enter, so a reader never sees half a session.
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Whether windows with `app_id` are never saved nor relaunched: prompts, agents and lockers, and
/// what the config's `restore-exclude` patterns match.
pub fn excluded(app_id: &str, patterns: &[RegexEq]) -> bool {
    let lower = app_id.to_lowercase();
    NEVER.iter().any(|never| lower.contains(never)) || patterns.iter().any(|p| p.0.is_match(app_id))
}

/// The window's app ID, title and process, when it is saved: a toplevel with an app ID, not a
/// dialog (those come back with their parent's app, or not at all), and not excluded, by its
/// app or by a client's `NewmRestoreExclude` request.
fn found(w: &Window, exclude: &[RegexEq]) -> Option<Found> {
    if w.parent.is_some() || w.restore_excluded {
        return None;
    }
    let app_id = w.app_id.clone().filter(|id| !id.is_empty())?;
    if excluded(&app_id, exclude) {
        return None;
    }
    Some(Found {
        id: w.id,
        app_id,
        title: w.title.clone(),
        pid: w.pid,
    })
}

/// The session the desktop shows now: one canvas per output.
fn snapshot(
    desktop: &Desktop,
    exclude: &[RegexEq],
    relaunch: impl FnMut(&Found) -> Option<Relaunch>,
) -> Session {
    let mut canvases = Vec::new();
    let mut windows: Vec<(usize, Spot, bool, Found)> = Vec::new();
    for o in desktop.outputs() {
        let k = canvases.len();
        let canvas = &o.canvas;
        let focused = canvas.focus();
        for cw in canvas.windows() {
            if let Some(w) = desktop.window(cw.id).and_then(|w| found(w, exclude)) {
                windows.push((k, Spot::Tiled(cw.rect), focused == Some(cw.id), w));
            }
        }
        for w in desktop.windows() {
            if w.output == Some(o.id)
                && matches!(w.place, Some(Place::Floating { .. }))
                && let Some(r) = desktop.window_rect(w.id)
                && let Some(f) = found(w, exclude)
            {
                let pos = (f64::from(r.x - o.area.x), f64::from(r.y - o.area.y));
                let size = (f64::from(r.w), f64::from(r.h));
                windows.push((k, Spot::Floating(pos, size), false, f));
            }
        }
        let scale = canvas.scale();
        let (x, y) = canvas.view();
        canvases.push(SavedCanvas {
            output: Some(o.name.clone()),
            zoom: canvas.zoom(),
            view: (x / scale, y / scale),
            focus: None,
            windows: Vec::new(),
            floating: Vec::new(),
        });
    }

    let found: Vec<Found> = windows.iter().map(|w| w.3.clone()).collect();
    let (launch_of, launches) = group(&found, relaunch);
    for ((k, spot, focused, w), launch) in windows.into_iter().zip(launch_of) {
        let canvas = &mut canvases[k];
        match spot {
            Spot::Tiled(r) => {
                if focused {
                    canvas.focus = Some(canvas.windows.len());
                }
                canvas.windows.push(SavedWindow {
                    app_id: w.app_id,
                    title: w.title,
                    rect: (r.i, r.j, r.w, r.h),
                    launch,
                });
            }
            Spot::Floating(pos, size) => canvas.floating.push(SavedFloating {
                app_id: w.app_id,
                title: w.title,
                pos,
                size,
                launch,
            }),
        }
    }
    canvases.retain(|c| !c.windows.is_empty() || !c.floating.is_empty());
    Session { canvases, launches }
}

/// Gives each window the index of the launch that brings it back: the windows of one process
/// share one launch (one per command: a browser's web apps have their own), a window relaunched
/// on its own has its own.
pub fn group(
    found: &[Found],
    mut relaunch: impl FnMut(&Found) -> Option<Relaunch>,
) -> (Vec<Option<usize>>, Vec<Launch>) {
    let mut launches = Vec::new();
    let mut by_process: HashMap<(i32, Launch), usize> = HashMap::new();
    let push = |launches: &mut Vec<Launch>, launch: Launch| {
        launches.push(launch);
        launches.len() - 1
    };
    let launch_of = found
        .iter()
        .map(|w| match relaunch(w)? {
            Relaunch::Window(launch) => Some(push(&mut launches, launch)),
            Relaunch::Process(launch) => Some(match w.pid {
                Some(pid) => *by_process
                    .entry((pid, launch.clone()))
                    .or_insert_with(|| push(&mut launches, launch)),
                None => push(&mut launches, launch),
            }),
        })
        .collect();
    (launch_of, launches)
}

/// Finds how windows are relaunched, caching what it reads.
#[derive(Debug, Default)]
struct Resolver {
    /// The desktop entry's launch per app ID.
    entries: HashMap<String, Option<Launch>>,
    /// The desktop files, read on the first app ID that is not a terminal's.
    apps: Option<apps::Index>,
    processes: HashMap<i32, Option<Launch>>,
}

impl Resolver {
    /// A terminal's window, on its own: a window of that terminal running what it showed (see
    /// [`terminals`]), else in the directory the title names, else home. Others: the desktop
    /// entry of the app the app ID names ([`apps::Index::path`]: the file named after it in any
    /// case, else one naming it as `StartupWMClass` or program), else the process's command line
    /// and directory.
    fn relaunch(&mut self, w: &Found, home: &Path, terminals: &Terminals) -> Option<Relaunch> {
        if let Some(terminal) = terminals::find(&w.app_id) {
            let instance = match (&terminal.single_instance, w.pid) {
                (Some(_), Some(pid)) => self
                    .process(pid)
                    .map(|l| terminal.instance(&l.command))
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let title = w.title.as_deref().map(|t| terminal.title(t));
            if let Some(command) = terminals.command(w.id) {
                return Some(Relaunch::Window(terminal.running(home, &instance, command)));
            }
            let dir = title.and_then(|t| terminals.dir(t, home));
            return Some(Relaunch::Window(
                terminal.window(dir.as_deref().unwrap_or(home), &instance),
            ));
        }
        if let Some(launch) = self.desktop_entry(&w.app_id) {
            return Some(Relaunch::Process(launch));
        }
        self.process(w.pid?).map(Relaunch::Process)
    }

    fn process(&mut self, pid: i32) -> Option<Launch> {
        self.processes
            .entry(pid)
            .or_insert_with(|| read_process(pid))
            .clone()
    }

    fn desktop_entry(&mut self, app_id: &str) -> Option<Launch> {
        if let Some(launch) = self.entries.get(app_id) {
            return launch.clone();
        }
        let launch = self
            .apps
            .get_or_insert_with(|| apps::Index::read(&apps::data_dirs()))
            .path(app_id)
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| desktop_launch(&text));
        self.entries.insert(app_id.to_owned(), launch.clone());
        launch
    }
}

/// The directory a terminal's title names: the whole title, or its last word, as an absolute or
/// `~` path to an existing directory.
pub fn title_dir(title: &str, home: &Path) -> Option<PathBuf> {
    let last = title.rsplit([' ', ':']).next().unwrap_or(title);
    [title.trim(), last.trim()].into_iter().find_map(|t| {
        let path = if t == "~" {
            home.to_path_buf()
        } else if let Some(rest) = t.strip_prefix("~/") {
            home.join(rest)
        } else if t.starts_with('/') {
            PathBuf::from(t)
        } else {
            return None;
        };
        path.is_dir().then_some(path)
    })
}

/// A desktop file's `Exec` as arguments ([`exec::argv`]), in its `Path`.
pub fn desktop_launch(text: &str) -> Option<Launch> {
    let icon = apps::entry_key(text, "Icon").map(exec::unescape);
    let name = apps::entry_key(text, "Name").map(exec::unescape);
    let codes = exec::Fields {
        icon: icon.as_deref(),
        name: name.as_deref(),
        location: None,
    };
    let command = exec::argv(apps::entry_key(text, "Exec")?, &codes);
    (!command.is_empty()).then(|| Launch {
        command,
        cwd: apps::entry_key(text, "Path")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
    })
}

/// The process's command line (`/proc/<pid>/cmdline`, the pid river reports as
/// `unreliable_pid`) and directory, unless it is a helper serving other programs' windows.
fn read_process(pid: i32) -> Option<Launch> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let command = cmdline_args(&raw);
    let argv0 = command.first().filter(|a| !a.is_empty())?;
    if HELPERS.contains(&basename(argv0)) || pid == std::process::id() as i32 {
        return None;
    }
    Some(Launch {
        command,
        cwd: fs::read_link(format!("/proc/{pid}/cwd")).ok(),
    })
}

/// The arguments of a NUL-separated (and NUL-terminated) command line.
pub fn cmdline_args(raw: &[u8]) -> Vec<String> {
    let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
    raw.split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The program names the config starts at startup: launches of these are left to the config.
pub fn startup_programs(spawn: &[SpawnAtStartup], spawn_sh: &[SpawnShAtStartup]) -> Vec<String> {
    let mut names: Vec<String> = spawn
        .iter()
        .filter_map(|s| s.command.first())
        .map(|c| basename(c).to_owned())
        .collect();
    for s in spawn_sh {
        let words = s
            .command
            .split(|c: char| c.is_whitespace() || ";&|()'\"`".contains(c))
            .filter(|w| !w.is_empty() && !w.starts_with('-') && !w.contains('='));
        names.extend(words.map(|w| basename(w).to_owned()));
    }
    names
}

/// A saved canvas with its output's name.
pub type OnOutput = (Option<String>, RestoreCanvas);

/// Whether a saved window's tiles are ones newm-river could have saved: a hand-edited or damaged
/// session file must not put a window where the canvas's tile math leaves i32.
fn saved_rect_ok((i, j, w, h): (i32, i32, i32, i32)) -> bool {
    const REACH: i32 = 1 << 20;
    const SPAN: i32 = 1024;
    (-REACH..=REACH).contains(&i)
        && (-REACH..=REACH).contains(&j)
        && (1..=SPAN).contains(&w)
        && (1..=SPAN).contains(&h)
}

/// What a saved session puts back: each canvas's placeholders and camera, and the launches its
/// tiled windows need, less excluded apps and programs the config starts at startup. A launch only
/// floating windows need is left out: those are mostly prompts.
pub fn plan(
    session: Session,
    exclude: &[RegexEq],
    startup: &[String],
) -> (Vec<OnOutput>, Vec<Launch>) {
    let mut needed = BTreeSet::new();
    let mut canvases = Vec::new();
    for canvas in session.canvases {
        let mut windows = Vec::new();
        let mut focus = None;
        for (k, w) in canvas.windows.into_iter().enumerate() {
            if excluded(&w.app_id, exclude) || !saved_rect_ok(w.rect) {
                continue;
            }
            if canvas.focus == Some(k) {
                focus = Some(windows.len());
            }
            needed.extend(w.launch);
            let (i, j, width, height) = w.rect;
            windows.push(RestoreWindow {
                app_id: w.app_id,
                title: w.title,
                rect: Tiles::new(i, j, width, height),
            });
        }
        if !windows.is_empty() {
            let saved = RestoreCanvas {
                windows,
                zoom: canvas.zoom,
                view: canvas.view,
                focus,
            };
            canvases.push((canvas.output, saved));
        }
    }
    let launches = needed
        .into_iter()
        .filter_map(|k| session.launches.get(k))
        .filter(|l| {
            l.command
                .first()
                .is_some_and(|argv0| !startup.iter().any(|s| s == basename(argv0)))
        })
        .cloned()
        .map(restoring)
        .collect();
    (canvases, launches)
}

/// Chromium and the browsers built on it. Started bare, each profile opens its own startup pages
/// (a new tab, a fixed page) instead of the windows it had.
const CHROMIUM_BROWSERS: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "google-chrome-beta",
    "google-chrome-unstable",
    "brave",
    "brave-browser",
    "microsoft-edge",
    "microsoft-edge-stable",
    "vivaldi",
    "vivaldi-stable",
];

const RESTORE_LAST_SESSION: &str = "--restore-last-session";

/// `launch` as a restore runs it: a bare Chromium-family browser reopens its last session (every
/// profile that had windows, with their tabs) whatever each profile's startup setting says. A
/// launch that opens something itself (a URL, a file, an `--app` window) is left as it is: with
/// the browser already running, the flag would open the whole session a second time. Added when
/// restoring, never saved, so a session saved without it gets it too.
fn restoring(mut launch: Launch) -> Launch {
    let Some((argv0, args)) = launch.command.split_first() else {
        return launch;
    };
    let bare = !args
        .iter()
        .any(|a| !a.starts_with('-') || a.starts_with("--app") || a == RESTORE_LAST_SESSION);
    if bare && CHROMIUM_BROWSERS.contains(&basename(argv0)) {
        launch.command.push(String::from(RESTORE_LAST_SESSION));
    }
    launch
}

/// Runs a launch, by its program's name when its path is gone (an old Nix store path), in its
/// directory when that still exists, with the variables `env` set on top of newm-river's.
pub fn launch(launch: &Launch, env: Vec<(String, String)>) -> Effect {
    let mut command = launch.command.clone();
    if let Some(argv0) = command.first_mut()
        && argv0.starts_with('/')
        && !Path::new(argv0).exists()
    {
        *argv0 = basename(argv0).to_owned();
    }
    let cwd = launch.cwd.clone().filter(|dir| dir.is_dir());
    Effect::SpawnIn { command, cwd, env }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use niri_config::PresetSize;

    use super::*;
    use crate::model::{Rect, Settings};

    const GHOSTTY: &str = "com.mitchellh.ghostty";

    /// The Ghostty window launch from before the terminal table, word for word.
    fn ghostty_window(dir: &Path) -> Launch {
        Launch {
            command: vec![
                String::from("ghostty"),
                String::from("+new-window"),
                format!("--working-directory={}", dir.display()),
            ],
            cwd: None,
        }
    }

    /// `ghostty +new-window` from home running `command`, from before the terminal table.
    fn ghostty_running(home: &Path, command: Vec<String>) -> Launch {
        let mut launch = ghostty_window(home);
        launch.command.push(String::from("-e"));
        launch.command.extend(command);
        launch
    }

    fn found(app_id: &str, title: &str, pid: i32) -> Found {
        Found {
            id: 0,
            app_id: app_id.into(),
            title: Some(title.into()),
            pid: Some(pid),
        }
    }

    fn command(args: &[&str]) -> Launch {
        Launch {
            command: args.iter().map(|a| a.to_string()).collect(),
            cwd: None,
        }
    }

    fn regex(re: &str) -> RegexEq {
        re.parse().unwrap()
    }

    #[test]
    fn session_files_are_written_off_the_loop_in_order() {
        let dir = std::env::temp_dir().join(format!("newm-river-saver-{}", std::process::id()));
        let path = dir.join("session.json");
        let mut saver = Saver::new(None, Vec::new(), Instant::now());
        saver.path = Some(path.clone());
        saver.write("first".into());
        saver.write("second".into());
        assert_eq!(saver.written.as_deref(), Some("second"));
        let (tx, rx) = mpsc::channel();
        let job = Job {
            path: path.clone(),
            json: "third".into(),
            done: Some(tx),
        };
        assert!(saver.send(job));
        let res = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        res.unwrap();
        assert_eq!(
            text, "third",
            "the writer thread keeps the order of the writes"
        );
    }

    #[test]
    fn a_session_survives_a_round_trip_through_a_private_file() {
        let dir = std::env::temp_dir().join(format!("newm-river-session-{}", std::process::id()));
        let path = dir.join("state").join("session.json");
        let session = Session {
            canvases: vec![SavedCanvas {
                output: Some("eDP-1".into()),
                zoom: 1,
                view: (12.5, 0.),
                focus: Some(0),
                windows: vec![SavedWindow {
                    app_id: "foot".into(),
                    title: Some("~".into()),
                    rect: (0, 0, 8, 6),
                    launch: Some(0),
                }],
                floating: Vec::new(),
            }],
            launches: vec![command(&["foot"])],
        };
        write_private(&path, &serde_json::to_string(&session).unwrap()).unwrap();
        let saver = Saver::new(Some(path.clone()), Vec::new(), Instant::now());
        assert_eq!(saver.load(), Some(session));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_session_writer_lands_writes_in_order_and_a_quit_waits_for_it() {
        let dir = std::env::temp_dir().join(format!("newm-river-writer-{}", std::process::id()));
        let path = dir.join("session.json");
        let mut saver = Saver::new(Some(path.clone()), Vec::new(), Instant::now());
        for k in 0..50 {
            saver.write(format!("queued {k}"));
        }
        assert!(saver.writer.is_some());
        // What save_now does at a quit: the last write waits for the 50 queued before it.
        let (tx, rx) = mpsc::channel();
        let job = Job {
            path: path.clone(),
            json: String::from("at quit"),
            done: Some(tx),
        };
        assert!(saver.send(job));
        rx.recv_timeout(QUIT_WRITE_WAIT).unwrap().unwrap();
        saver.written = Some(String::from("at quit"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "at quit");
        assert_eq!(saver.written.as_deref(), Some("at quit"));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn windows_of_one_process_share_a_launch() {
        let windows = [
            found("firefox", "a", 10),
            found("firefox", "b", 10),
            found("foot", "c", 11),
            found("foot", "d", 12),
            found("helper", "e", 13),
        ];
        let (of, launches) = group(&windows, |w| match w.app_id.as_str() {
            "helper" => None,
            "foot" if w.pid == Some(12) => Some(Relaunch::Window(command(&["foot", "-D", "/"]))),
            app => Some(Relaunch::Process(command(&[app]))),
        });
        assert_eq!(of, vec![Some(0), Some(0), Some(1), Some(2), None]);
        assert_eq!(launches.len(), 3);
    }

    #[test]
    fn terminals_with_one_title_save_their_own_tmux_sessions() {
        let home = Path::new("/h");
        let attach = |n: u64| terminals::tmux_command(&[], &format!("work{n}"));
        let terminals = Terminals::with((1..=3).map(|n| (n, attach(n))).collect());
        let found: Vec<Found> = [100, 200, 300]
            .into_iter()
            .zip(1..)
            .map(|(pid, id)| Found {
                id,
                ..found("foot", "desk: me@desk: ~", pid)
            })
            .collect();
        let mut resolver = Resolver::default();
        let (launch_of, launches) = group(&found, |w| resolver.relaunch(w, home, &terminals));
        assert_eq!(launch_of, [Some(0), Some(1), Some(2)]);
        let running = |n| {
            let mut launch = command(&["foot", "--working-directory=/h"]);
            launch.command.extend(attach(n));
            launch
        };
        assert_eq!(launches, [running(1), running(2), running(3)]);
    }

    #[test]
    fn ghostty_windows_reopen_in_the_directory_their_title_names() {
        let home = std::env::temp_dir();
        assert_eq!(title_dir("~", &home), Some(home.clone()));
        assert_eq!(title_dir("me@desk: /", &home), Some(PathBuf::from("/")));
        assert_eq!(title_dir("vim notes.txt", &home), None);
        let mut r = Resolver::default();
        let terminals = Terminals::with(HashMap::new());
        let w = Found {
            id: 0,
            app_id: GHOSTTY.into(),
            title: Some("/".into()),
            pid: None,
        };
        assert_eq!(
            r.relaunch(&w, &home, &terminals),
            Some(Relaunch::Window(ghostty_window(Path::new("/"))))
        );
    }

    #[test]
    fn ghostty_windows_rerun_the_tmux_session_or_ssh_they_showed() {
        let home = std::env::temp_dir();
        let attach = terminals::tmux_command(&[], "5");
        let terminals = Terminals::with(HashMap::from([(3, attach.clone())]));
        let mut r = Resolver::default();
        let shown = Found {
            id: 3,
            ..found(GHOSTTY, "desk: ✳ notes", 1)
        };
        let launch = r.relaunch(&shown, &home, &terminals);
        assert_eq!(
            launch,
            Some(Relaunch::Window(ghostty_running(&home, attach)))
        );
        let Some(Relaunch::Window(launch)) = launch else {
            unreachable!()
        };
        assert_eq!(
            launch.command[..4],
            [
                String::from("ghostty"),
                String::from("+new-window"),
                format!("--working-directory={}", home.display()),
                String::from("-e"),
            ]
        );
        assert_eq!(
            r.relaunch(&found(GHOSTTY, "~", 1), &home, &terminals),
            Some(Relaunch::Window(ghostty_window(&home))),
            "a window no terminal was found for: a shell as before"
        );
    }

    #[test]
    fn other_terminals_rerun_what_they_showed_with_their_own_flags() {
        let home = std::env::temp_dir();
        let attach = terminals::tmux_command(&[], "5");
        let terminals = Terminals::with(HashMap::from([(3, attach.clone())]));
        let mut resolver = Resolver::default();
        resolver.processes.insert(1, None);
        // Window 3 shows tmux session 5; the others show a shell.
        let mut relaunch = |app_id: &str, title: &str| {
            let id = if title.starts_with("desk: ✳ notes") {
                3
            } else {
                0
            };
            resolver.relaunch(
                &Found {
                    id,
                    ..found(app_id, title, 1)
                },
                &home,
                &terminals,
            )
        };
        let words = |w: &[&str]| -> Vec<String> { w.iter().map(|&a| a.to_owned()).collect() };
        let running = |w: &[&str]| Launch {
            command: [words(w), attach.clone()].concat(),
            cwd: None,
        };
        let h = home.display().to_string();
        assert_eq!(
            relaunch("Alacritty", "desk: ✳ notes"),
            Some(Relaunch::Window(running(&[
                "alacritty",
                "--working-directory",
                &h,
                "-e"
            ])))
        );
        assert_eq!(
            relaunch("org.kde.konsole", "desk: ✳ notes \u{2014} Konsole"),
            Some(Relaunch::Window(running(&[
                "konsole",
                "--workdir",
                &h,
                "-e"
            ]))),
            "the window found, whatever Konsole appends to its title"
        );
        assert_eq!(
            relaunch("org.gnome.Terminal", "desk: ✳ notes"),
            Some(Relaunch::Window(running(&[
                "gnome-terminal",
                &format!("--working-directory={h}"),
                "--"
            ])))
        );
        assert_eq!(
            relaunch("XTerm", "desk: ✳ notes"),
            Some(Relaunch::Window(Launch {
                command: [words(&["xterm", "-e"]), attach.clone()].concat(),
                cwd: Some(home.clone()),
            }))
        );
        assert_eq!(
            relaunch("foot", "me@desk: /"),
            Some(Relaunch::Window(command(&[
                "foot",
                "--working-directory=/"
            ]))),
            "a plain shell: in the directory its title names"
        );
        assert_eq!(
            relaunch("org.kde.konsole", "/ \u{2014} Konsole"),
            Some(Relaunch::Window(command(&["konsole", "--workdir", "/"])))
        );
        assert_eq!(
            relaunch("kitty", "vim"),
            Some(Relaunch::Window(command(&[
                "kitty",
                &format!("--directory={h}")
            ]))),
            "no directory in the title: home"
        );
    }

    #[test]
    fn kitty_windows_go_back_to_the_single_instance_they_ran_in() {
        let home = Path::new("/h");
        let terminals = Terminals::with(HashMap::new());
        let mut resolver = Resolver::default();
        resolver.processes.insert(
            7,
            Some(command(&[
                "/nix/store/x/bin/kitty",
                "-1",
                "--instance-group",
                "w",
            ])),
        );
        resolver.processes.insert(8, Some(command(&["kitty"])));
        assert_eq!(
            resolver.relaunch(&found("kitty", "~", 7), home, &terminals),
            Some(Relaunch::Window(command(&[
                "kitty",
                "--single-instance",
                "--instance-group=w",
                "--directory=/h"
            ])))
        );
        assert_eq!(
            resolver.relaunch(&found("kitty", "~", 8), home, &terminals),
            Some(Relaunch::Window(command(&["kitty", "--directory=/h"])))
        );
    }

    #[test]
    fn an_unknown_terminal_relaunches_as_before() {
        let home = std::env::temp_dir();
        let terminals = Terminals::with(HashMap::from([(9, terminals::tmux_command(&[], "5"))]));
        let mut resolver = Resolver::default();
        let process = command(&["/nix/store/x/bin/unknownterm", "--hold"]);
        resolver.processes.insert(9, Some(process.clone()));
        assert_eq!(terminals::find("org.example.UnknownTerm"), None);
        assert_eq!(
            resolver.relaunch(
                &Found {
                    id: 9,
                    ..found("org.example.UnknownTerm", "desk: ✳ notes", 9)
                },
                &home,
                &terminals
            ),
            Some(Relaunch::Process(process)),
            "its process's command line, shared by its windows, not a tmux attach"
        );
    }

    #[test]
    fn a_ghostty_session_saved_before_the_terminal_table_restores_the_same() {
        let json = r#"{
  "canvases": [
    {
      "output": "DP-1",
      "zoom": 1,
      "view": [0.0, 0.0],
      "focus": 0,
      "windows": [
        {"app_id": "com.mitchellh.ghostty", "title": "desk: ✳ newm",
         "rect": [0, 0, 1, 1], "launch": 0},
        {"app_id": "com.mitchellh.ghostty", "title": "far: ~",
         "rect": [1, 0, 1, 1], "launch": 1},
        {"app_id": "com.mitchellh.ghostty", "title": "~",
         "rect": [2, 0, 1, 1], "launch": 2}
      ],
      "floating": []
    }
  ],
  "launches": [
    {"command": ["ghostty", "+new-window", "--working-directory=/home/u", "-e", "sh", "-c",
      "tmux new-session -A -s 0; exec \"${SHELL:-sh}\""], "cwd": null},
    {"command": ["ghostty", "+new-window", "--working-directory=/home/u", "-e", "sh", "-c",
      "ssh -t far tmux new-session -A -s 3; exec \"${SHELL:-sh}\""], "cwd": null},
    {"command": ["ghostty", "+new-window", "--working-directory=/home/u"], "cwd": null}
  ]
}"#;
        let session: Session = serde_json::from_str(json).unwrap();
        let saved = session.launches.clone();
        let (canvases, launches) = plan(session, &[], &[]);
        assert_eq!(launches, saved, "every launch runs as it was saved");
        let windows = &canvases[0].1.windows;
        assert_eq!(windows.len(), 3);
        assert!(windows.iter().all(|w| w.app_id == GHOSTTY));
        let home = Path::new("/home/u");
        assert_eq!(saved[2], ghostty_window(home));
        assert_eq!(
            saved[0],
            ghostty_running(home, terminals::tmux_command(&[], "0"))
        );
        let resolver_home = std::env::temp_dir();
        let mut resolver = Resolver::default();
        assert_eq!(
            resolver.relaunch(
                &found(GHOSTTY, "~", 1),
                &resolver_home,
                &Terminals::with(HashMap::new())
            ),
            Some(Relaunch::Window(ghostty_window(&resolver_home))),
            "and the saver writes the same launch again"
        );
    }

    #[test]
    fn desktop_entries_launch_their_exec_without_field_codes() {
        let text = "[Desktop Action new]\nExec=nope\n[Desktop Entry]\nName=A\nExec=\"/bin/my app\" --x=%%1 %U\nPath=/tmp\n";
        assert_eq!(
            desktop_launch(text),
            Some(Launch {
                command: vec!["/bin/my app".into(), "--x=%1".into()],
                cwd: Some("/tmp".into()),
            })
        );
        let none = exec::Fields {
            icon: None,
            name: None,
            location: None,
        };
        assert_eq!(exec::argv(r"a\sb  c%f", &none), vec!["a", "b", "c"]);
        assert_eq!(exec::argv(r#""a\sb"  c%f"#, &none), vec!["a b", "c"]);
        assert_eq!(cmdline_args(b"foot\0-e\0sh\0"), vec!["foot", "-e", "sh"]);
    }

    #[test]
    fn prompts_lockers_and_configured_apps_are_excluded() {
        let patterns = [regex("^mpv$")];
        assert!(excluded(
            "org.kde.polkit-kde-authentication-agent-1",
            &patterns
        ));
        assert!(excluded("mpv", &patterns));
        assert!(excluded("org.kde.ksshaskpass", &patterns));
        assert!(excluded(
            "org.freedesktop.impl.portal.desktop.kde",
            &patterns
        ));
        assert!(!excluded("mpv2", &patterns));
        assert!(!excluded("foot", &patterns));
    }

    #[test]
    fn startup_programs_are_named_by_their_basename() {
        let config = Config::parse_mem(
            r#"
            spawn-at-startup "/nix/store/x/bin/waybar" "-c" "y"
            spawn-sh-at-startup "FOO=1 swaybg -i /a.png; exec mako"
            "#,
        )
        .unwrap();
        let names = startup_programs(&config.spawn_at_startup, &config.spawn_sh_at_startup);
        for name in ["waybar", "swaybg", "mako"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
    }

    #[test]
    fn a_restore_skips_windows_no_save_could_have_written() {
        let window = |app_id: &str, rect| SavedWindow {
            app_id: app_id.into(),
            title: None,
            rect,
            launch: None,
        };
        let session = Session {
            canvases: vec![SavedCanvas {
                output: Some("DP-1".into()),
                zoom: 0,
                view: (0., 0.),
                focus: None,
                windows: vec![
                    window("good", (0, 0, 2, 2)),
                    window("bad", (i32::MAX, 0, 1, 1)),
                    window("bad", (0, i32::MIN, 1, 1)),
                    window("bad", (0, 0, 0, 1)),
                    window("bad", (0, 0, 1, i32::MAX)),
                ],
                floating: Vec::new(),
            }],
            launches: Vec::new(),
        };
        let (canvases, _) = plan(session, &[], &[]);
        let apps: Vec<&str> = canvases[0]
            .1
            .windows
            .iter()
            .map(|w| w.app_id.as_str())
            .collect();
        assert_eq!(apps, ["good"]);
    }

    #[test]
    fn a_restore_plans_tiled_windows_and_their_launches_once() {
        let window = |app_id: &str, launch| SavedWindow {
            app_id: app_id.into(),
            title: None,
            rect: (0, 0, 2, 2),
            launch,
        };
        let session = Session {
            canvases: vec![SavedCanvas {
                output: Some("DP-1".into()),
                zoom: 0,
                view: (0., 0.),
                focus: Some(2),
                windows: vec![
                    window("mpv", Some(0)),
                    window("chromium-browser", Some(1)),
                    window("chromium-browser", Some(1)),
                    window("waybar", Some(2)),
                ],
                floating: vec![SavedFloating {
                    app_id: "prompt".into(),
                    title: None,
                    pos: (0., 0.),
                    size: (1., 1.),
                    launch: Some(3),
                }],
            }],
            launches: vec![
                command(&["mpv"]),
                command(&["chromium"]),
                command(&["/bin/waybar"]),
                command(&["prompt"]),
            ],
        };
        let (canvases, launches) = plan(session, &[regex("^mpv$")], &["waybar".into()]);
        assert_eq!(
            launches,
            vec![command(&["chromium", "--restore-last-session"])],
            "Chromium once for its two windows, restoring its last session; no excluded, \
             startup or floating-only launch"
        );
        assert_eq!(canvases.len(), 1);
        let (output, saved) = &canvases[0];
        assert_eq!(output.as_deref(), Some("DP-1"));
        assert_eq!(saved.windows.len(), 3, "mpv excluded");
        assert_eq!(
            saved.focus,
            Some(1),
            "the focus index follows the exclusion"
        );
    }

    #[test]
    fn chromium_browsers_restore_their_last_session_unless_they_open_something() {
        let restored = |args: &[&str]| restoring(command(args)).command;
        let words = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let chrome = [
            "/nix/store/x/bin/google-chrome-stable",
            "--ozone-platform=x11",
        ];
        assert_eq!(
            restored(&chrome),
            words(&[chrome[0], chrome[1], "--restore-last-session"])
        );
        let twice = ["chromium", "--restore-last-session"];
        assert_eq!(restored(&twice), words(&twice), "never twice");
        for kept in [
            &["chromium", "https://example.org"][..],
            &["chromium", "--profile-directory=Default", "--app-id=abc"],
            &["chromium", "--app=https://example.org"],
            &["firefox"],
            &["kcalc"],
        ] {
            assert_eq!(restored(kept), words(kept), "{kept:?}");
        }
    }

    #[test]
    fn a_launch_runs_in_its_directory_by_name_when_its_path_is_gone() {
        let gone = Launch {
            command: vec!["/nix/store/gone/bin/foot".into(), "it's".into()],
            cwd: Some("/".into()),
        };
        assert_eq!(
            launch(&gone, vec![("SOME_VAR".into(), "1".into())]),
            Effect::SpawnIn {
                command: vec!["foot".into(), "it's".into()],
                cwd: Some("/".into()),
                env: vec![("SOME_VAR".into(), "1".into())],
            }
        );
        assert_eq!(
            launch(&command(&["foot"]), Vec::new()),
            Effect::SpawnIn {
                command: vec!["foot".into()],
                cwd: None,
                env: Vec::new(),
            }
        );
    }

    #[test]
    fn river_on_a_tty_holds_a_drm_card() {
        let links = |l: &[&str]| l.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert!(holds_drm_card(links(&["/dev/null", "/dev/dri/card1"])));
        assert!(!holds_drm_card(links(&[
            "/dev/dri/renderD128",
            "socket:[1]"
        ])));
    }

    #[test]
    fn the_snapshot_saves_tiles_focus_and_camera_per_output() {
        let now = Instant::now();
        let settings = Settings {
            default_width: Some(PresetSize::Fixed(960)),
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_name(1, "DP-1".into());
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        assert!(outputs_known(&d));
        for (id, app) in [(1, "pinentry-qt"), (2, "foot"), (3, "firefox")] {
            d.add_window(id);
            let w = d.window_mut(id).unwrap();
            w.app_id = Some(app.into());
            w.pid = Some(100 + id as i32);
            d.manage(now);
            // A tiled window takes its tiles at its app's first commit (here: no size picked).
            for (id, size) in d.proposals() {
                d.set_asked(id, size);
                d.on_dimensions(id, size.0, size.1);
            }
        }
        d.add_window(4);
        let w = d.window_mut(4).unwrap();
        w.app_id = Some("firefox".into());
        w.parent = Some(3);
        d.manage(now);
        let session = snapshot(&d, &[], |w| Some(Relaunch::Process(command(&[&w.app_id]))));
        assert_eq!(session.canvases.len(), 1);
        let c = &session.canvases[0];
        assert_eq!(c.output.as_deref(), Some("DP-1"));
        let apps: Vec<&str> = c.windows.iter().map(|w| w.app_id.as_str()).collect();
        assert_eq!(apps, ["foot", "firefox"], "no prompt, no dialog");
        assert_eq!(c.focus, Some(1), "the newest window has the focus");
        let r = d.outputs()[0].canvas.rect(2).unwrap();
        assert_eq!(c.windows[0].rect, (r.i, r.j, r.w, r.h));
        assert_eq!(session.launches.len(), 2);
    }

    #[test]
    fn the_snapshot_saves_a_floating_windows_size_and_place_after_a_resize() {
        let now = Instant::now();
        let settle = |d: &mut Desktop| {
            for _ in 0..4 {
                d.manage(now);
                let asked = d.proposals();
                for &(id, size) in &asked {
                    d.set_asked(id, size);
                    d.on_dimensions(id, size.0.max(300), size.1.max(200));
                }
                if asked.is_empty() {
                    break;
                }
            }
        };
        let mut d = Desktop::new(Settings::default(), Vec::new(), now);
        d.add_output(1);
        d.set_output_name(1, "DP-1".into());
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        d.add_window(1);
        let w = d.window_mut(1).unwrap();
        w.app_id = Some("foot".into());
        w.pid = Some(101);
        settle(&mut d);
        d.act(niri_config::Action::ToggleWindowFloating, now);
        settle(&mut d);
        d.render(now);
        let r = d.window_rect(1).unwrap();
        assert!(d.resize_begin(r.x + r.w / 10, r.y + r.h / 2, now));
        d.resize_to(-40., 0.);
        settle(&mut d);
        d.resize_end(now);
        let session = snapshot(&d, &[], |w| Some(Relaunch::Process(command(&[&w.app_id]))));
        let f = &session.canvases[0].floating;
        assert_eq!(f.len(), 1);
        assert_eq!(
            f[0].pos,
            (f64::from(r.x - 40), f64::from(r.y)),
            "the left edge moved"
        );
        assert_eq!(
            f[0].size,
            (f64::from(r.w + 40), f64::from(r.h)),
            "and the size grew"
        );
    }

    #[test]
    fn a_window_a_client_excludes_is_not_saved() {
        let now = Instant::now();
        let mut d = Desktop::new(Settings::default(), Vec::new(), now);
        d.add_output(1);
        d.set_output_name(1, "DP-1".into());
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        for (id, app) in [(1, "foot"), (2, "mpv")] {
            d.add_window(id);
            d.window_mut(id).unwrap().app_id = Some(app.into());
            d.manage(now);
            for (id, size) in d.proposals() {
                d.set_asked(id, size);
                d.on_dimensions(id, size.0.max(300), size.1.max(200));
            }
        }
        let saved = |d: &Desktop| {
            let session = snapshot(d, &[], |w| Some(Relaunch::Process(command(&[&w.app_id]))));
            let canvas = &session.canvases[0];
            let mut apps: Vec<String> = canvas.windows.iter().map(|w| w.app_id.clone()).collect();
            apps.extend(canvas.floating.iter().map(|w| w.app_id.clone()));
            apps.sort_unstable();
            (apps, session.launches.len())
        };
        assert_eq!(saved(&d), (vec!["foot".into(), "mpv".into()], 2));
        d.window_mut(1).unwrap().restore_excluded = true;
        assert_eq!(
            saved(&d),
            (vec!["mpv".into()], 1),
            "neither the window nor its launch is saved"
        );
        d.window_mut(1).unwrap().restore_excluded = false;
        assert_eq!(saved(&d).0, ["foot", "mpv"], "and the mark comes off");
    }
}
