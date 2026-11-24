//! newm's synth background: the tuned synthwave look drawn behind
//! everything, on the GPU (`synth/gpu.rs`) or, without one, on the CPU.
//!
//! Each output gets one `zwlr_layer_surface_v1` in the **background** layer, below bottom-layer
//! bars and windows, with an empty input region. The GPU draws the shader at the output's
//! physical size through an EGL window surface; the CPU fallback paints the look at the logical
//! size with newm-synth's port on one worker thread, into one of two buffers of a shm pool.
//!
//! Pacing: animated, a surface draws its next frame as soon as river answers the last frame's
//! `wl_surface.frame` callback, so on the GPU it moves at the output's own refresh rate.
//! `max-fps`, or `on-battery "capped"` on battery (the smaller of the two), caps
//! it: the next frame then waits `1 / cap` after the last. The CPU painter stays capped at 4
//! frames a second (or `max-fps` if lower): at the display's rate it would take most of a core.
//! wlroots answers the callback only while the surface shows: a fully covered surface (an opaque
//! fullscreen window over it), a disabled one (river disables its normal tree while the session
//! is locked) or one on an output that stops drawing (blanked) gets no callback, so the surface
//! waits with no deadline and no wakeup. Still draws one frame; off holds nothing.
//!
//! Scene time: each animated commit asks `wp_presentation` for feedback. A frame's target is the
//! refresh after the one nearest now on the grid of the last presented time ([`target`]), or the
//! monotonic now without feedback or a known refresh (the feedback's, else the output's mode,
//! else the one measured between presentations).
//! Each output keeps its own scene time ([`SceneClock`]), which steps by the time between targets
//! rounded to whole refreshes, so at a steady refresh every step is exactly one refresh. When
//! the picture stops (hidden, locked, still, battery, idle) the clock forgets its last target,
//! and the next frame continues from the picture that showed, at most one refresh on. Outputs
//! with different refresh rates each step by their own refresh; their pictures can drift apart
//! by the steps one of them missed: each output has its own scene time.
//!
//! Stopping elsewhere: on battery with `on-battery "still"` (read from the kernel's power
//! supplies at start and on each `power_supply` uevent, `synth/uevent.rs`), and after
//! `idle-still-after` seconds without input (`ext_idle_notifier_v1`, which honours idle
//! inhibitors), the picture stands still and asks for no frames. newm-river cannot see clients'
//! opaque regions, so under opaque windows it stops only when river stops answering (fully
//! covered).

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, FromRawFd, RawFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use newm_synth::{quantise, Frame, Level, Look, Painter, Tables};
use niri_config::{SynthBackground, SynthOnBattery};
use niri_ipc::SynthMode;
use tracing::{debug, info, warn};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy as _, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, ZwlrLayerSurfaceV1,
};

use super::State;
use crate::model::OutputId;
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

/// The look a config block asks for: the user's tuned look with the block's knobs on top.
pub fn look(c: &SynthBackground) -> Look {
    let mut l = Look::default();
    if let Some(seed) = c.seed {
        l.seed = seed;
    }
    macro_rules! numbers {
        ($($f:ident),* $(,)?) => {$(
            if let Some(x) = c.$f {
                l.$f = x as f32;
            }
        )*};
    }
    numbers!(
        horizon,
        sky_fall,
        stars,
        sun_size,
        sun_y,
        sun_x,
        stripes,
        stripe_width,
        halo,
        reflection,
        layers,
        ridge_height,
        roughness,
        ridge_frequency,
        ridge_speed,
        haze,
        rim,
        perspective,
        grid_x,
        grid_z,
        line_width,
        grid_glow,
        grid_speed,
        curve,
        cycle_speed,
        cycle_sun,
        cycle_hue,
        hue,
        saturation,
        brightness,
        scanlines,
        bits,
        audio_sun,
        audio_grid,
        audio_ridge,
    );
    macro_rules! colours {
        ($($f:ident),* $(,)?) => {$(
            if let Some(x) = c.$f {
                l.$f = [x.r, x.g, x.b];
            }
        )*};
    }
    colours!(
        sky_color,
        glow_color,
        grid_color,
        sun_top_color,
        sun_bottom_color,
        ridge_color,
    );
    l
}

/// The mode, look and clock, apart from Wayland: the `Synth` state machine.
#[derive(Debug)]
pub struct Scene {
    /// The mode an action set, which holds until the config's mode changes.
    override_mode: Option<SynthMode>,
    mode: SynthMode,
    look: Look,
    /// `max-fps`: a cap, or none (the display's rate).
    max_fps: Option<f64>,
    on_battery_mode: SynthOnBattery,
    on_battery_max_fps: f64,
    /// `idle-still-after` in seconds; 0 never.
    idle_still_after: f64,
    /// The machine runs on battery (the kernel's power supplies, while watched).
    pub on_battery: bool,
    /// The seat has been idle for `idle-still-after` (ext-idle-notify, inhibitors honoured).
    pub idle: bool,
    /// The CPU painter draws: capped at [`CPU_FPS`].
    pub cpu: bool,
    /// `react-to-sound`, from the config.
    react_to_sound: bool,
    /// The capture is open.
    listening: bool,
    level: Level,
    /// When the level last moved on.
    level_at: Option<Instant>,
    /// The sound level the drawn picture shows: a step of the look's level quantum, and the
    /// level it draws as.
    drawn_step: u32,
    drawn_level: f32,
}

/// A frame river has not answered for this long is not showing: wlroots answers only frames
/// that show, within a refresh or two.
pub const HIDDEN_AFTER: Duration = Duration::from_secs(1);

/// One refresh at 60 Hz: the shortest wait for a retry or a sound level tick, so a zero interval
/// (the display's rate) never makes a busy loop.
pub const REFRESH_FLOOR: Duration = Duration::from_nanos(16_666_667);

/// The CPU painter's most frames a second: painting at the display's rate would take most of a
/// core (about 40 ms a frame at 1440x960), so it keeps the old 4.
pub const CPU_FPS: f64 = 4.;

/// The monotonic clock's now, the clock wp_presentation reports on.
pub fn monotonic_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec to write.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// The presentation time a frame drawn at `now` aims for: on the grid of refreshes from the last
/// presented time, the one after the refresh nearest `now`. A frame is drawn just after a
/// refresh (its callback came), so this is the next one whether the feedback for the frame that
/// just showed came before the callback or after it. Without feedback it is a refresh after
/// `now`, and without a known refresh `now` itself.
pub fn target(now: Duration, presented: Option<Duration>, refresh: Option<Duration>) -> Duration {
    match (presented, refresh) {
        (Some(p), Some(r)) if !r.is_zero() => {
            let (since, r) = (now.saturating_sub(p).as_nanos(), r.as_nanos());
            let k = (since + r / 2) / r + 1;
            p + Duration::from_nanos((r * k) as u64)
        }
        (None, Some(r)) => now + r,
        _ => now,
    }
}

/// Folds the time between two presentations into the measured refresh: a running average of
/// the intervals, leaving out gaps (a capped or hidden picture, a missed refresh) of more than
/// one and a half of it, and anything over 100 ms.
pub fn measure_refresh(measured: Option<Duration>, dt: Duration) -> Option<Duration> {
    if dt.is_zero() || dt > Duration::from_millis(100) {
        return measured;
    }
    Some(match measured {
        None => dt,
        Some(m) if dt > m * 3 / 2 => m,
        Some(m) => (m * 15 + dt) / 16,
    })
}

/// One output's scene time: the time its picture shows, moved on by the presentation targets
/// of its frames.
#[derive(Debug, Clone, Default)]
pub struct SceneClock {
    pub time: Duration,
    /// The target of the last frame that moved, while the picture moves.
    last_target: Option<Duration>,
}

impl SceneClock {
    /// Moves the picture to a frame presented at `target`. `refresh` is the output's refresh,
    /// if known, and `interval` the cap's interval (zero: every refresh). Steps are the time
    /// between targets in whole refreshes, so they are even at a steady refresh. A gap of more
    /// than two and a half expected steps (the surface was hidden) moves it by one refresh, not
    /// by the gap.
    pub fn advance(
        &mut self,
        target: Duration,
        refresh: Option<Duration>,
        interval: Duration,
    ) -> Duration {
        if let Some(last) = self.last_target {
            let dt = target.saturating_sub(last);
            let refresh = refresh.filter(|r| !r.is_zero());
            let one = refresh.unwrap_or(REFRESH_FLOOR);
            let expected = one.max(interval);
            self.time += if dt > expected * 5 / 2 {
                one
            } else if let Some(r) = refresh {
                let k = (dt.as_nanos() + r.as_nanos() / 2) / r.as_nanos();
                Duration::from_nanos((r.as_nanos() * k) as u64)
            } else {
                dt
            };
        }
        self.last_target = Some(target);
        self.time
    }

    /// The picture stopped: the next move continues from the picture it shows.
    pub fn stop(&mut self) {
        self.last_target = None;
    }
}

impl Scene {
    pub fn new(config: &SynthBackground) -> Self {
        let mut s = Scene {
            override_mode: None,
            mode: SynthMode::Off,
            look: Look::default(),
            max_fps: None,
            on_battery_mode: SynthOnBattery::Still,
            on_battery_max_fps: 30.,
            idle_still_after: 300.,
            on_battery: false,
            idle: false,
            cpu: false,
            react_to_sound: false,
            listening: false,
            level: Level::new(),
            level_at: None,
            drawn_step: 0,
            drawn_level: 0.,
        };
        s.apply_config(config);
        s
    }

    pub fn mode(&self) -> SynthMode {
        self.mode
    }

    /// Animated, with a look that moves, and not held by the battery or idleness: frames keep
    /// coming while shown.
    pub fn animated(&self) -> bool {
        self.mode == SynthMode::Animated && self.look.moves() && !self.held()
    }

    /// Whether the battery (`on-battery "still"`) or idleness stands the picture still.
    pub fn held(&self) -> bool {
        (self.on_battery && self.on_battery_mode == SynthOnBattery::Still) || self.idle
    }

    /// The cap on frames a second in effect, if any: `max-fps`, and on battery with
    /// `on-battery "capped"` the smaller of it and `on-battery-max-fps`.
    pub fn cap(&self) -> Option<f64> {
        let battery = (self.on_battery && self.on_battery_mode == SynthOnBattery::Capped)
            .then_some(self.on_battery_max_fps);
        let cap = match (self.max_fps, battery) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let cap = if self.cpu {
            Some(cap.map_or(CPU_FPS, |c| c.min(CPU_FPS)))
        } else {
            cap
        };
        cap.filter(|c| c.is_finite()).map(|c| c.clamp(1., 1000.))
    }

    /// The least time between two frames' starts: zero (the next frame as soon as the last is
    /// answered, the display's rate) unless a cap applies.
    pub fn interval(&self) -> Duration {
        self.cap()
            .map_or(Duration::ZERO, |fps| Duration::from_secs_f64(1. / fps))
    }

    /// The interval with a floor of one refresh, for timers that must not spin.
    pub fn tick(&self) -> Duration {
        self.interval().max(REFRESH_FLOOR)
    }

    /// Whether the power supplies need watching: animated and `on-battery` not "animated".
    pub fn wants_power(&self) -> bool {
        self.mode == SynthMode::Animated && self.on_battery_mode != SynthOnBattery::Animated
    }

    /// The idle timeout to ask for, in milliseconds: animated with `idle-still-after` not 0.
    pub fn idle_timeout_ms(&self) -> Option<u32> {
        (self.mode == SynthMode::Animated && self.idle_still_after > 0.).then(|| {
            (self.idle_still_after * 1000.)
                .round()
                .min(f64::from(u32::MAX)) as u32
        })
    }

    /// Applies the config's block. Returns whether the picture or the mode changed.
    pub fn apply_config(&mut self, config: &SynthBackground) -> bool {
        self.max_fps = config.max_fps;
        self.on_battery_mode = config.on_battery;
        self.on_battery_max_fps = config.on_battery_max_fps;
        self.idle_still_after = config.idle_still_after;
        self.react_to_sound = config.react_to_sound;
        let mode = self.override_mode.unwrap_or(config.mode);
        let look = look(config);
        let changed = mode != self.mode || look != self.look;
        self.mode = mode;
        self.look = look;
        changed
    }

    /// The config's mode changed: an action's mode no longer holds.
    pub fn config_mode_changed(&mut self) {
        self.override_mode = None;
    }

    /// `set-synth-background`: the mode until the config's mode changes.
    pub fn set_mode(&mut self, mode: SynthMode, config: &SynthBackground) -> bool {
        self.override_mode = Some(mode);
        self.apply_config(config)
    }

    /// `toggle-synth-background`: from animated back to the configured mode when that is off
    /// or still (to still when the config says animated), from anything else to animated.
    pub fn toggle(&mut self, config: &SynthBackground) -> bool {
        let mode = match (self.mode, config.mode) {
            (SynthMode::Animated, SynthMode::Animated) => SynthMode::Still,
            (SynthMode::Animated, resting) => resting,
            _ => SynthMode::Animated,
        };
        self.set_mode(mode, config)
    }

    /// The frame to draw at `now` for a picture `size` pixels large, at scene time `time` (the
    /// output's [`SceneClock`]).
    fn frame(&mut self, now: Instant, time: Duration, size: (i32, i32)) -> Frame {
        if self.listening {
            (self.drawn_step, self.drawn_level) = self.sound_step(now, size.1.max(1) as u32);
        }
        Frame::new(
            &self.look,
            size.0 as usize,
            size.1 as usize,
            time.as_secs_f64(),
            self.drawn_level,
        )
    }

    /// Whether the background should listen: `react-to-sound` on, animated, showing on some
    /// output, and some audio amount not zero. Nothing else opens the capture.
    pub fn wants_sound(&self, showing: bool) -> bool {
        self.react_to_sound
            && showing
            && self.mode == SynthMode::Animated
            && !self.held()
            && self.look.hears()
    }

    /// Whether the level is still moving: sound came in (`pending`), or it has not settled at
    /// zero where the picture shows it.
    pub fn sound_busy(&self, pending: bool) -> bool {
        self.listening && (pending || self.level.level() > 0. || self.drawn_step != 0)
    }

    /// A sound block came in, as its level.
    pub fn hear(&mut self, level: f32) {
        self.level.feed(level);
    }

    /// Moves the level on to `now` and gives its step at a picture `height` pixels tall.
    fn sound_step(&mut self, now: Instant, height: u32) -> (u32, f32) {
        // The first step after the capture opened counts as one tick.
        let dt = self
            .level_at
            .replace(now)
            .map_or(self.tick(), |t| now.saturating_duration_since(t));
        let level = self.level.advance(dt);
        self.look
            .level_quantum(height)
            .map_or((0, 0.), |q| quantise(level, q))
    }

    /// For a look that does not move by itself: moves the level on and says whether the drawn
    /// picture is a step off, so a redraw would show something.
    pub fn level_moved(&mut self, now: Instant, height: u32) -> bool {
        self.sound_step(now, height).0 != self.drawn_step
    }

    /// When the next level tick is due while a still look listens: one interval after the last,
    /// and never sooner than a refresh.
    pub fn level_due(&self, now: Instant) -> Instant {
        self.level_at.map_or(now, |t| t + self.tick())
    }

    /// The capture opened or closed. Closed, the level goes to zero, and returns whether the
    /// picture showed a level (so it is drawn again without).
    pub fn set_listening(&mut self, on: bool) -> bool {
        self.listening = on;
        if on {
            return false;
        }
        self.level = Level::new();
        self.level_at = None;
        let showed = self.drawn_step != 0;
        self.drawn_step = 0;
        self.drawn_level = 0.;
        showed
    }
}

/// Where one surface is in its drawing cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the layer surface's first configure.
    Unconfigured,
    /// Nothing in flight: draws when `dirty`, or when animated and an interval has passed.
    Idle,
    /// The worker is painting a buffer.
    Painting,
    /// The frame is committed; waiting for river's frame callback, which only comes while the
    /// surface shows.
    Shown,
    /// Both buffers are still river's; waiting for one's release.
    NoBuffer,
}

/// A surface's pacing, apart from Wayland so its guarantees can be tested.
#[derive(Debug, Clone)]
pub struct Pacing {
    pub phase: Phase,
    /// The picture must be drawn again (first frame, a new size, a config change).
    pub dirty: bool,
    pub last_submit: Option<Instant>,
    /// When the last frame was committed: unanswered for [`HIDDEN_AFTER`], it is not showing.
    pub committed_at: Option<Instant>,
    /// The last draw could not start: the retry waits at least a refresh.
    pub retry: bool,
}

impl Default for Pacing {
    fn default() -> Self {
        Pacing {
            phase: Phase::Unconfigured,
            dirty: true,
            last_submit: None,
            committed_at: None,
            retry: false,
        }
    }
}

impl Pacing {
    /// When this surface next wants to draw, `now` being the caller's present: never while
    /// anything is in flight, while hidden or while still with nothing new.
    pub fn due(&self, now: Instant, animated: bool, interval: Duration) -> Option<Instant> {
        if self.phase != Phase::Idle {
            return None;
        }
        match self.last_submit {
            _ if self.dirty => Some(now),
            Some(last) if animated && self.retry => Some(last + interval.max(REFRESH_FLOOR)),
            Some(last) if animated => Some(last + interval),
            None if animated => Some(now),
            _ => None,
        }
    }

    pub fn configured(&mut self) {
        if self.phase == Phase::Unconfigured {
            self.phase = Phase::Idle;
        }
        self.dirty = true;
    }

    pub fn submitted(&mut self, now: Instant) {
        self.phase = Phase::Painting;
        self.dirty = false;
        self.retry = false;
        self.last_submit = Some(now);
    }

    /// The paint is committed. Animated, the next waits for the frame callback; still, the
    /// surface is idle at once (it asked for no callback).
    pub fn committed(&mut self, animated: bool) {
        self.phase = if animated { Phase::Shown } else { Phase::Idle };
        self.committed_at = self.last_submit;
    }

    /// Whether this surface's last frame shows: anything but a frame river has left unanswered
    /// for [`HIDDEN_AFTER`] (and a surface not yet configured).
    pub fn showing(&self, now: Instant) -> bool {
        match self.phase {
            Phase::Unconfigured => false,
            Phase::Shown => self.committed_at.is_none_or(|t| now < t + HIDDEN_AFTER),
            _ => true,
        }
    }

    pub fn frame_done(&mut self) {
        if self.phase == Phase::Shown {
            self.phase = Phase::Idle;
        }
    }

    /// Nothing could be drawn (no buffers, a superseded paint): try again after an interval, at
    /// least a refresh, when animated, or at the next change when still, never in a tight loop.
    pub fn skipped(&mut self, now: Instant) {
        self.phase = Phase::Idle;
        self.dirty = false;
        self.retry = true;
        self.last_submit = Some(now);
    }

    pub fn no_buffer(&mut self) {
        self.phase = Phase::NoBuffer;
        self.dirty = true;
    }

    pub fn released(&mut self) {
        if self.phase == Phase::NoBuffer {
            self.phase = Phase::Idle;
        }
    }
}

/// A shared memory file mapped into this process, written by the worker.
struct Shm {
    file: File,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory; the buffer protocol (one job per slot, a slot reused only
// after river releases it) keeps any two writers or a writer and river apart.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

impl Shm {
    fn new(len: usize) -> Result<Self> {
        // SAFETY: memfd_create takes a NUL-terminated name and returns a new fd or -1.
        let fd = unsafe { libc::memfd_create(c"newm-synth".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64)?;
        // SAFETY: a shared read-write mapping of the whole file, checked for failure.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        Ok(Shm {
            file,
            ptr: ptr.cast(),
            len,
        })
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: ptr and len are this mapping's, unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Converts newm-synth's pixels (`[r, g, b, 0xff]` in memory) to `XRGB8888` (`[b, g, r, x]`),
/// the format every wl_shm has.
pub fn to_xrgb(px: &mut [u32]) {
    for p in px {
        let v = *p;
        *p = (v & 0xff00_ff00) | (v & 0xff) << 16 | (v >> 16) & 0xff;
    }
}

struct Job {
    key: u64,
    generation: u32,
    slot: usize,
    frame: Frame,
    seed: u32,
    shm: Arc<Shm>,
    size: (usize, usize),
}

struct Done {
    key: u64,
    generation: u32,
    slot: usize,
}

/// The painting thread. It stops when this is dropped (its job channel closes).
struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Done>,
    wake: Arc<File>,
}

impl Worker {
    fn start() -> Result<Self> {
        // SAFETY: eventfd returns a new fd or -1.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            bail!("eventfd: {}", std::io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        let wake = Arc::new(unsafe { File::from_raw_fd(fd) });
        let (jobs, rx) = channel::<Job>();
        let (tx, done) = channel();
        let waker = wake.clone();
        thread::Builder::new().name("synth".into()).spawn(move || {
            let mut painter = Painter::new();
            let mut tables: Option<Tables> = None;
            for job in rx {
                if tables.as_ref().is_none_or(|t| t.seed() != job.seed) {
                    tables = Some(Tables::new(job.seed));
                }
                let (w, h) = job.size;
                let offset = job.slot * w * h * 4;
                assert!(offset + w * h * 4 <= job.shm.len);
                // SAFETY: the slot lies inside the mapping (checked), is page-aligned plus a
                // multiple of 4, and nothing else touches it until this job is done.
                let px = unsafe {
                    std::slice::from_raw_parts_mut(job.shm.ptr.add(offset).cast::<u32>(), w * h)
                };
                painter.paint_rows(tables.as_ref().unwrap(), &job.frame, px, w, 0..h, w);
                to_xrgb(px);
                let done = Done {
                    key: job.key,
                    generation: job.generation,
                    slot: job.slot,
                };
                if tx.send(done).is_err() {
                    break;
                }
                use std::io::Write as _;
                let _ = (&*waker).write(&1u64.to_ne_bytes());
            }
        })?;
        Ok(Worker { jobs, done, wake })
    }
}

/// Which buffer a release event is for.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    key: u64,
    generation: u32,
    slot: usize,
}

/// Which surface a configure or frame callback is for.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceKey(u64);

struct Pool {
    shm: Arc<Shm>,
    pool: wl_shm_pool::WlShmPool,
    buffers: [wl_buffer::WlBuffer; 2],
    busy: [bool; 2],
}

impl Drop for Pool {
    fn drop(&mut self) {
        for b in &self.buffers {
            b.destroy();
        }
        self.pool.destroy();
    }
}

struct Background {
    key: u64,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// Shows the buffer at the logical size, whatever size it is drawn at.
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// The output's scale, in 120ths (wp-fractional-scale-v1); 120 until river says.
    scale120: u32,
    /// The EGL surface on `surface`, while the GPU draws.
    gpu: Option<gpu::GpuSurface>,
    /// The logical size.
    size: (i32, i32),
    /// Bumped with every new pool, so a finished job for an old one is ignored.
    generation: u32,
    pool: Option<Pool>,
    pacing: Pacing,
    /// The output's `wl_output`, for its refresh.
    output: wayland_client::backend::ObjectId,
    /// This output's scene time.
    clock: SceneClock,
    /// The last presentation feedback: when a frame showed (CLOCK_MONOTONIC) and the refresh
    /// river reported (none when 0, as on VRR or a headless output).
    presented: Option<Duration>,
    refresh: Option<Duration>,
    /// The refresh as measured between presentations, for an output that reports none (a
    /// headless one).
    measured: Option<Duration>,
}

impl Drop for Background {
    fn drop(&mut self) {
        // The EGL surface goes before the wl_surface under it.
        self.gpu = None;
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
        self.layer.destroy();
        self.surface.destroy();
    }
}

impl Background {
    /// The physical size: the logical size at the output's scale.
    fn physical(&self) -> (i32, i32) {
        physical(self.size, self.scale120)
    }
}

/// A logical size at a scale in 120ths (wp-fractional-scale-v1), rounded to whole pixels as
/// river rounds a scaled output's size.
pub fn physical(size: (i32, i32), scale120: u32) -> (i32, i32) {
    let f = |v: i32| ((i64::from(v) * i64::from(scale120) + 60) / 120) as i32;
    (f(size.0), f(size.1))
}

/// What draws the background, chosen once each time the mode turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Gpu,
    Cpu,
}

/// The synth background's Wayland side: one background per output while on.
pub struct Synth {
    pub scene: Scene,
    layer_shell: Option<ZwlrLayerShellV1>,
    backgrounds: HashMap<OutputId, Background>,
    worker: Option<Worker>,
    next_key: u64,
    /// The capture, only while the background listens.
    sound: Option<sound::Sound>,
    /// Opening the capture failed (no PipeWire): not tried again until the config changes.
    sound_failed: bool,
    renderer: Option<Renderer>,
    /// The EGL context, while the GPU draws.
    gpu: Option<gpu::Gpu>,
    /// The GPU has drawn a frame (the log names its renderer once).
    gpu_announced: bool,
    viewporter: Option<WpViewporter>,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    presentation: Option<WpPresentation>,
    /// wp_presentation reports on CLOCK_MONOTONIC (its `clock_id`); otherwise feedback is
    /// ignored and the scene time follows the monotonic now.
    presentation_monotonic: bool,
    /// The current mode's refresh of each `wl_output`, for outputs whose feedback has none.
    output_refresh: HashMap<wayland_client::backend::ObjectId, Duration>,
    /// The kernel's power supply events, while `on-battery` matters.
    uevents: Option<uevent::Uevents>,
    /// Opening the uevent socket failed: not on battery, not tried again until the config
    /// changes.
    uevents_failed: bool,
    seat: Option<WlSeat>,
    idle_notifier: Option<ExtIdleNotifierV1>,
    /// river offers no ext_idle_notifier_v1 (logged once).
    idle_missing: bool,
    /// The idle notification and the timeout it was made for.
    idle_notification: Option<(ExtIdleNotificationV1, u32)>,
}

impl Synth {
    pub fn new(config: &SynthBackground) -> Self {
        Synth {
            scene: Scene::new(config),
            layer_shell: None,
            backgrounds: HashMap::new(),
            worker: None,
            next_key: 0,
            sound: None,
            sound_failed: false,
            renderer: None,
            gpu: None,
            gpu_announced: false,
            viewporter: None,
            fractional_manager: None,
            presentation: None,
            presentation_monotonic: false,
            output_refresh: HashMap::new(),
            uevents: None,
            uevents_failed: false,
            seat: None,
            idle_notifier: None,
            idle_missing: false,
            idle_notification: None,
        }
    }

    /// Whether anything of the background exists: a surface, a buffer or the worker.
    pub fn is_allocated(&self) -> bool {
        !self.backgrounds.is_empty() || self.worker.is_some()
    }

    /// The worker's eventfd for the poll loop, or -1 (which poll skips) while off.
    pub fn wake_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd as _;
        self.worker.as_ref().map_or(-1, |w| w.wake.as_raw_fd())
    }

    /// The earliest moment a surface wants to draw.
    pub fn deadline(&self) -> Option<Instant> {
        let (animated, interval) = (self.scene.animated(), self.scene.interval());
        let now = Instant::now();
        let frames = self
            .backgrounds
            .values()
            .filter_map(|b| b.pacing.due(now, animated, interval))
            .min();
        let Some(sound) = &self.sound else {
            return frames;
        };
        // While listening: the next level tick of a still look that hears something, and the
        // moment an unanswered frame counts as hidden (so listening stops). Only future ones:
        // a past one has been acted on.
        let level = (!animated && self.scene.sound_busy(sound.pending()))
            .then(|| self.scene.level_due(now));
        let hidden = self
            .backgrounds
            .values()
            .filter(|b| b.pacing.phase == Phase::Shown)
            .filter_map(|b| b.pacing.committed_at.map(|t| t + HIDDEN_AFTER))
            .filter(|&t| t > now)
            .min();
        [frames, level, hidden].into_iter().flatten().min()
    }

    /// Whether some surface's picture shows.
    pub fn showing(&self, now: Instant) -> bool {
        self.backgrounds.values().any(|b| b.pacing.showing(now))
    }

    /// The capture thread's eventfd for the poll loop, or -1 (which poll skips).
    pub fn sound_fd(&self) -> RawFd {
        self.sound.as_ref().map_or(-1, |s| s.wake_fd())
    }

    /// The uevent socket for the poll loop, or -1 (which poll skips) unless `on-battery`
    /// matters.
    pub fn power_fd(&self) -> RawFd {
        self.uevents.as_ref().map_or(-1, |u| u.fd())
    }

    /// The uevent socket is readable: drains it and re-reads the supplies after a power supply
    /// event.
    pub fn power_event(&mut self) {
        let Some(u) = &self.uevents else {
            return;
        };
        if u.drain() {
            self.read_power();
        }
    }

    fn read_power(&mut self) {
        let on = newm_synth::power::on_battery(std::path::Path::new(newm_synth::power::SYSFS));
        if on != self.scene.on_battery {
            info!("synth background: on battery {on}");
            self.scene.on_battery = on;
        }
    }

    /// A `wl_output`'s current mode, `refresh` in mHz.
    pub fn output_mode(&mut self, output: wayland_client::backend::ObjectId, refresh: i32) {
        if refresh > 0 {
            let r = Duration::from_nanos(1_000_000_000_000 / refresh as u64);
            self.output_refresh.insert(output, r);
        } else {
            self.output_refresh.remove(&output);
        }
    }

    /// The picture changed (config, action): every surface draws again.
    pub fn mark_dirty(&mut self) {
        for b in self.backgrounds.values_mut() {
            b.pacing.dirty = true;
        }
    }
}

impl State {
    /// The config's block changed (or loaded).
    pub(crate) fn synth_config(&mut self, config: &SynthBackground, mode_changed: bool) {
        self.synth.sound_failed = false;
        if mode_changed {
            self.synth.scene.config_mode_changed();
        }
        if self.synth.scene.apply_config(config) {
            self.synth.mark_dirty();
        }
    }

    pub(crate) fn synth_toggle(&mut self) {
        let config = self.config.synth_background.clone();
        if self.synth.scene.toggle(&config) {
            self.synth.mark_dirty();
        }
    }

    pub(crate) fn synth_set_mode(&mut self, mode: SynthMode) {
        let config = self.config.synth_background.clone();
        if self.synth.scene.set_mode(mode, &config) {
            self.synth.mark_dirty();
        }
    }

    /// Makes the surfaces match the outputs and the mode, and hands due frames to the worker.
    /// Off, it returns at once once everything is gone.
    pub(crate) fn synth_tick(&mut self, qh: &QueueHandle<State>, now: Instant) {
        if self.synth.scene.mode() == SynthMode::Off {
            if self.synth.is_allocated()
                || self.synth.sound.is_some()
                || self.synth.gpu.is_some()
                || self.synth.uevents.is_some()
                || self.synth.idle_notification.is_some()
            {
                self.synth.sound = None;
                self.synth.uevents = None;
                self.synth.scene.on_battery = false;
                if let Some((n, _)) = self.synth.idle_notification.take() {
                    n.destroy();
                }
                self.synth.scene.idle = false;
                self.synth.scene.set_listening(false);
                self.synth.backgrounds.clear();
                self.synth.worker = None;
                // No GL driver stays loaded while off; the next start chooses again.
                self.synth.gpu = None;
                self.synth.renderer = None;
                self.synth.gpu_announced = false;
            }
            return;
        }
        if self.synth.renderer.is_none() {
            self.synth_choose_renderer();
        }
        if self.synth.renderer == Some(Renderer::Cpu) && self.synth.worker.is_none() {
            match Worker::start() {
                Ok(w) => self.synth.worker = Some(w),
                Err(e) => {
                    warn!("synth background: {e}");
                    return;
                }
            }
        }
        if self.synth.viewporter.is_none() {
            self.synth.viewporter = self.globals.bind(qh, 1..=1, ()).ok();
            self.synth.fractional_manager = self.globals.bind(qh, 1..=1, ()).ok();
        }
        if self.synth.presentation.is_none() {
            self.synth.presentation = self.globals.bind(qh, 1..=1, ()).ok();
        }
        if self.synth.layer_shell.is_none() {
            self.synth.layer_shell = self.globals.bind(qh, 1..=4, ()).ok();
            if self.synth.layer_shell.is_none() {
                warn!("synth background: no zwlr_layer_shell_v1");
                return;
            }
        }
        let outputs: Vec<(OutputId, wl_output::WlOutput)> = self.wl_outputs_by_id();
        self.synth
            .backgrounds
            .retain(|id, _| outputs.iter().any(|(o, _)| o == id));
        for (id, wl) in outputs {
            if !self.synth.backgrounds.contains_key(&id) {
                let bg = self.synth_surface(&wl, qh);
                self.synth.backgrounds.insert(id, bg);
            }
        }

        self.synth_power();
        self.synth_idle(qh);
        self.synth_listen(now);

        let (animated, interval) = (self.synth.scene.animated(), self.synth.scene.interval());
        if !animated {
            // Stopped (still, held, a look that cannot move): the next move continues from the
            // picture that shows.
            for bg in self.synth.backgrounds.values_mut() {
                bg.clock.stop();
            }
        }
        let ids: Vec<OutputId> = self.synth.backgrounds.keys().copied().collect();
        for id in ids {
            let due = self.synth.backgrounds[&id]
                .pacing
                .due(now, animated, interval);
            if due.is_some_and(|d| d <= now) {
                self.synth_submit(id, qh, now);
            }
        }
    }

    /// Opens or closes the uevent socket: open only while `on-battery` matters. Opening reads
    /// the supplies once; after that only power supply events do.
    fn synth_power(&mut self) {
        let synth = &mut self.synth;
        let want = synth.scene.wants_power() && !synth.uevents_failed;
        if want && synth.uevents.is_none() {
            match uevent::Uevents::open() {
                Ok(u) => {
                    synth.uevents = Some(u);
                    synth.read_power();
                }
                Err(e) => {
                    warn!("synth background: not watching the battery: {e:#}");
                    synth.uevents_failed = true;
                }
            }
        } else if !want && synth.uevents.is_some() {
            synth.uevents = None;
            synth.scene.on_battery = false;
        }
    }

    /// Makes the idle notification match `idle-still-after`: none when 0 or not animated, a new
    /// one when the timeout changes.
    fn synth_idle(&mut self, qh: &QueueHandle<State>) {
        let want = self.synth.scene.idle_timeout_ms();
        if self.synth.idle_notification.as_ref().map(|n| n.1) == want {
            return;
        }
        if let Some((n, _)) = self.synth.idle_notification.take() {
            n.destroy();
        }
        self.synth.scene.idle = false;
        let Some(timeout) = want else {
            return;
        };
        if self.synth.idle_missing {
            return;
        }
        if self.synth.idle_notifier.is_none() {
            self.synth.idle_notifier = self.globals.bind(qh, 1..=2, ()).ok();
            self.synth.seat = self.globals.bind(qh, 1..=1, ()).ok();
        }
        let (Some(notifier), Some(seat)) = (&self.synth.idle_notifier, &self.synth.seat) else {
            info!("synth background: no ext_idle_notifier_v1 or wl_seat: idle-still-after does nothing");
            self.synth.idle_missing = true;
            return;
        };
        let n = notifier.get_idle_notification(timeout, seat, qh, ());
        self.synth.idle_notification = Some((n, timeout));
    }

    /// Opens or closes the capture, takes the sound that came in, and for a look that does not
    /// move by itself, redraws when the level moved a step (at most once an interval).
    fn synth_listen(&mut self, now: Instant) {
        let showing = self.synth.showing(now);
        let want = self.synth.scene.wants_sound(showing) && !self.synth.sound_failed;
        if want && self.synth.sound.is_none() {
            match sound::Sound::spawn() {
                Ok(s) => {
                    info!("synth background: listening to sound");
                    self.synth.sound = Some(s);
                    self.synth.scene.set_listening(true);
                }
                Err(e) => {
                    warn!("synth background: not listening to sound: {e:#}");
                    self.synth.sound_failed = true;
                }
            }
        } else if !want && self.synth.sound.is_some() {
            info!("synth background: not listening to sound");
            self.synth.sound = None;
            if self.synth.scene.set_listening(false) {
                self.synth.mark_dirty();
            }
        }
        let Some(sound) = &self.synth.sound else {
            return;
        };
        sound.drain();
        let pending = sound.pending();
        self.synth.scene.hear(sound.take_peak());
        if self.synth.scene.animated() {
            return;
        }
        let scene = &mut self.synth.scene;
        if !scene.sound_busy(pending) {
            sound.wake_on_sound();
            return;
        }
        if now >= scene.level_due(now) {
            let height = self
                .synth
                .backgrounds
                .values()
                .map(|b| b.size.1.max(0) as u32)
                .max()
                .unwrap_or(0);
            if self.synth.scene.level_moved(now, height) {
                self.synth.mark_dirty();
            }
        }
    }

    fn synth_surface(&mut self, wl: &wl_output::WlOutput, qh: &QueueHandle<State>) -> Background {
        let key = self.synth.next_key;
        self.synth.next_key += 1;
        let compositor = self.compositor.as_ref().expect("wl_compositor");
        let surface = compositor.create_surface(qh, ());
        // No input: clicks on the desktop go where they went without a background.
        let region = compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let layer = self.synth.layer_shell.as_ref().unwrap().get_layer_surface(
            &surface,
            Some(wl),
            zwlr_layer_shell_v1::Layer::Background,
            "newm-synth".into(),
            qh,
            SurfaceKey(key),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
        layer.set_size(0, 0);
        let viewport = self
            .synth
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ()));
        let fractional = self
            .synth
            .fractional_manager
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, qh, SurfaceKey(key)));
        surface.commit();
        info!("synth background: a background-layer surface for {wl:?}");
        Background {
            key,
            surface,
            layer,
            viewport,
            fractional,
            scale120: 120,
            gpu: None,
            size: (0, 0),
            generation: 0,
            pool: None,
            pacing: Pacing::default(),
            output: wl.id(),
            clock: SceneClock::default(),
            presented: None,
            refresh: None,
            measured: None,
        }
    }

    /// GPU if an EGL context can be made on this connection, else the CPU painter; one log
    /// line says which and why. `NEWM_SYNTH_RENDERER=cpu` asks for the CPU painter.
    fn synth_choose_renderer(&mut self) {
        if std::env::var("NEWM_SYNTH_RENDERER").is_ok_and(|v| v == "cpu") {
            info!("synth background: drawing on the CPU: NEWM_SYNTH_RENDERER=cpu");
            self.synth.renderer = Some(Renderer::Cpu);
            self.synth.scene.cpu = true;
            return;
        }
        let display = self
            .globals
            .registry()
            .backend()
            .upgrade()
            .map_or(std::ptr::null_mut(), |b| b.display_ptr().cast());
        match gpu::Gpu::new(display) {
            Ok(g) => {
                self.synth.gpu = Some(g);
                self.synth.renderer = Some(Renderer::Gpu);
            }
            Err(e) => {
                info!("synth background: drawing on the CPU: no GPU context: {e:#}");
                self.synth.renderer = Some(Renderer::Cpu);
            }
        }
        self.synth.scene.cpu = self.synth.renderer == Some(Renderer::Cpu);
    }

    /// The GPU failed (a lost context, a failed swap): every background draws on the CPU from
    /// now on, until the mode turns off and on again. Never retried in a loop.
    fn synth_gpu_failed(&mut self, e: &anyhow::Error) {
        warn!("synth background: drawing on the CPU: the GPU failed: {e:#}");
        for bg in self.synth.backgrounds.values_mut() {
            bg.gpu = None;
            bg.pacing.phase = Phase::Idle;
            bg.pacing.dirty = true;
        }
        self.synth.gpu = None;
        self.synth.renderer = Some(Renderer::Cpu);
        self.synth.scene.cpu = true;
    }

    fn synth_submit(&mut self, id: OutputId, qh: &QueueHandle<State>, now: Instant) {
        if self.synth.renderer == Some(Renderer::Gpu) {
            self.synth_draw_gpu(id, qh, now);
            return;
        }
        let time = self.synth_scene_time(id);
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let seed = self.synth.scene.look.seed;
        let Synth {
            scene,
            backgrounds,
            worker,
            ..
        } = &mut self.synth;
        let (Some(bg), Some(worker)) = (backgrounds.get_mut(&id), worker.as_ref()) else {
            return;
        };
        let (w, h) = bg.size;
        if w <= 0 || h <= 0 {
            bg.pacing.skipped(now);
            return;
        }
        if bg.pool.is_none() {
            match make_pool(&shm, bg, qh) {
                Ok(p) => bg.pool = Some(p),
                Err(e) => {
                    warn!("synth background buffers: {e}");
                    bg.pacing.skipped(now);
                    return;
                }
            }
        }
        let pool = bg.pool.as_mut().unwrap();
        let Some(slot) = pool.busy.iter().position(|b| !b) else {
            bg.pacing.no_buffer();
            return;
        };
        pool.busy[slot] = true;
        let frame = scene.frame(now, time, bg.size);
        let job = Job {
            key: bg.key,
            generation: bg.generation,
            slot,
            frame,
            seed,
            shm: pool.shm.clone(),
            size: (w as usize, h as usize),
        };
        debug!("synth paint {w}x{h} slot {slot}");
        if worker.jobs.send(job).is_ok() {
            bg.pacing.submitted(now);
        } else {
            bg.pacing.skipped(now);
        }
    }

    /// Moves output `id`'s scene clock to the frame about to be drawn and gives its time:
    /// animated, to the frame's presentation target; otherwise where it stands.
    fn synth_scene_time(&mut self, id: OutputId) -> Duration {
        let animated = self.synth.scene.animated();
        let interval = self.synth.scene.interval();
        let Synth {
            backgrounds,
            output_refresh,
            presentation_monotonic,
            ..
        } = &mut self.synth;
        let Some(bg) = backgrounds.get_mut(&id) else {
            return Duration::ZERO;
        };
        if !animated {
            bg.clock.stop();
            return bg.clock.time;
        }
        let refresh = bg
            .refresh
            .or_else(|| output_refresh.get(&bg.output).copied())
            .or(bg.measured);
        let presented = bg.presented.filter(|_| *presentation_monotonic);
        let target = target(monotonic_now(), presented, refresh);
        bg.clock.advance(target, refresh, interval)
    }

    /// Draws one frame on the GPU, at the output's physical size, and commits it.
    fn synth_draw_gpu(&mut self, id: OutputId, qh: &QueueHandle<State>, now: Instant) {
        if self
            .synth
            .backgrounds
            .get(&id)
            .is_none_or(|bg| bg.size.0 <= 0 || bg.size.1 <= 0)
            || self.synth.gpu.is_none()
        {
            if let Some(bg) = self.synth.backgrounds.get_mut(&id) {
                bg.pacing.skipped(now);
            }
            return;
        }
        let time = self.synth_scene_time(id);
        let seed = self.synth.scene.look.seed;
        let animated = self.synth.scene.animated();
        let Synth {
            scene,
            backgrounds,
            gpu,
            gpu_announced,
            presentation,
            ..
        } = &mut self.synth;
        let (Some(bg), Some(gpu)) = (backgrounds.get_mut(&id), gpu.as_mut()) else {
            return;
        };
        let size = bg.physical();
        let started = Instant::now();
        let result = (|| {
            match &mut bg.gpu {
                Some(s) => s.resize(size),
                None => bg.gpu = Some(gpu.surface(&bg.surface, size)?),
            }
            let frame = scene.frame(now, time, size);
            if animated {
                bg.surface.frame(qh, SurfaceKey(bg.key));
                if let Some(p) = presentation {
                    p.feedback(&bg.surface, qh, SurfaceKey(bg.key));
                }
            }
            gpu.draw(bg.gpu.as_ref().unwrap(), &frame, seed)
        })();
        match result {
            Ok(()) => {
                bg.pacing.submitted(now);
                bg.pacing.committed(animated);
                // The scene time and the draw call's time on this thread (the GL work and the
                // swap), which e2e/synth-gpu.sh reads.
                debug!(
                    "synth frame committed t={:.6} draw={}",
                    time.as_secs_f64(),
                    started.elapsed().as_micros()
                );
                if !*gpu_announced {
                    *gpu_announced = true;
                    info!(
                        "synth background: drawing on the GPU: {}, {}x{}",
                        gpu.renderer, size.0, size.1
                    );
                }
            }
            Err(e) => self.synth_gpu_failed(&e),
        }
    }

    /// The worker finished paints: attach and commit them.
    pub(crate) fn synth_painted(&mut self, qh: &QueueHandle<State>) {
        let Some(worker) = &self.synth.worker else {
            return;
        };
        let mut b = [0u8; 8];
        use std::io::Read as _;
        let _ = (&*worker.wake).read(&mut b);
        let done: Vec<Done> = worker.done.try_iter().collect();
        let animated = self.synth.scene.animated();
        for d in done {
            let Some(bg) = self
                .synth
                .backgrounds
                .values_mut()
                .find(|bg| bg.key == d.key)
            else {
                continue;
            };
            if bg.generation != d.generation {
                // Painted for a size the surface no longer has; `configured` left it dirty.
                bg.pacing.phase = Phase::Idle;
                bg.pacing.dirty = true;
                continue;
            }
            let Some(pool) = &bg.pool else {
                continue;
            };
            bg.surface.attach(Some(&pool.buffers[d.slot]), 0, 0);
            bg.surface.damage_buffer(0, 0, bg.size.0, bg.size.1);
            if animated {
                bg.surface.frame(qh, SurfaceKey(bg.key));
                if let Some(p) = &self.synth.presentation {
                    p.feedback(&bg.surface, qh, SurfaceKey(bg.key));
                }
            }
            bg.surface.commit();
            bg.pacing.committed(animated);
            debug!("synth frame committed");
        }
    }

    fn synth_background(&mut self, key: u64) -> Option<&mut Background> {
        self.synth.backgrounds.values_mut().find(|b| b.key == key)
    }
}

fn make_pool(shm: &wl_shm::WlShm, bg: &mut Background, qh: &QueueHandle<State>) -> Result<Pool> {
    let (w, h) = bg.size;
    let one = (w * h * 4) as usize;
    let file = Shm::new(one * 2)?;
    bg.generation += 1;
    let pool = shm.create_pool(file.file.as_fd(), (one * 2) as i32, qh, ());
    let buffers = [0, 1].map(|slot| {
        pool.create_buffer(
            (slot * one) as i32,
            w,
            h,
            w * 4,
            wl_shm::Format::Xrgb8888,
            qh,
            Slot {
                key: bg.key,
                generation: bg.generation,
                slot,
            },
        )
    });
    Ok(Pool {
        shm: Arc::new(file),
        pool,
        buffers,
        busy: [false; 2],
    })
}

delegate_noop!(State: ignore ZwlrLayerShellV1);

impl Dispatch<ZwlrLayerSurfaceV1, SurfaceKey> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        key: &SurfaceKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                debug!("synth background configured {width}x{height}");
                if let Some(bg) = state.synth_background(key.0) {
                    let size = (width as i32, height as i32);
                    if size != bg.size {
                        bg.size = size;
                        bg.pool = None;
                        if let Some(v) = &bg.viewport {
                            v.set_destination(size.0, size.1);
                        }
                    }
                    bg.pacing.configured();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                debug!("synth background closed");
                // river closed it (its output went away); the next tick makes a new one if the
                // output is still there.
                state.synth.backgrounds.retain(|_, b| b.key != key.0);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, SurfaceKey> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        key: &SurfaceKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(bg) = state.synth_background(key.0)
            && bg.scale120 != scale
        {
            bg.scale120 = scale.max(1);
            bg.pacing.dirty = true;
        }
    }
}

delegate_noop!(State: ignore WpFractionalScaleManagerV1);

impl Dispatch<wl_callback::WlCallback, SurfaceKey> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        key: &SurfaceKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && let Some(bg) = state.synth_background(key.0)
        {
            bg.pacing.frame_done();
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, Slot> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        slot: &Slot,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(bg) = state.synth_background(slot.key)
            && bg.generation == slot.generation
            && let Some(pool) = &mut bg.pool
        {
            pool.busy[slot.slot] = false;
            bg.pacing.released();
        }
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.synth.presentation_monotonic = clk_id == libc::CLOCK_MONOTONIC as u32;
            if !state.synth.presentation_monotonic {
                info!("synth background: wp_presentation clock {clk_id} is not CLOCK_MONOTONIC");
            }
        }
    }
}

impl Dispatch<WpPresentationFeedback, SurfaceKey> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        key: &SurfaceKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation_feedback::Event::Presented {
            tv_sec_hi,
            tv_sec_lo,
            tv_nsec,
            refresh,
            ..
        } = event
            && let Some(bg) = state.synth_background(key.0)
        {
            let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
            let at = Duration::new(secs, tv_nsec);
            if let Some(last) = bg.presented {
                bg.measured = measure_refresh(bg.measured, at.saturating_sub(last));
            }
            bg.presented = Some(at);
            bg.refresh = (refresh > 0).then(|| Duration::from_nanos(u64::from(refresh)));
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = match event {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        if state.synth.scene.idle != idle {
            info!("synth background: idle {idle}");
            state.synth.scene.idle = idle;
        }
    }
}

delegate_noop!(State: ignore ExtIdleNotifierV1);
delegate_noop!(State: ignore WlSeat);

mod gpu;
mod sound;
#[cfg(test)]
mod tests;
mod uevent;
