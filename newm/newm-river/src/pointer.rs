//! The pointer, the Super key and touchpad gestures, free of Wayland and
//! of the model. river's seat and binding events go in as [`In`]; what they do comes out as
//! [`Cmd`]s, which `pointer_wl` carries out on the model and the seat in the manage sequence. The
//! thresholds and the swipe state machine live here too.
//!
//! - Super+left drags the window under the pointer through river's pointer operation, 1:1 from
//!   the start; the drop puts it on the tiles nearest where it is drawn.
//! - Pointer motion with Super held and no button pans the camera 1:1 once past
//!   [`PAN_THRESHOLD`], counted from [`PAN_DELAY`] after Super went down; Super's release
//!   settles the focus on the window nearest the middle.
//!   Below 1, `pan-acceleration` makes the pan follow the hand's motion plus that share of the
//!   pointer's acceleration (river's `pointer_motion`).
//! - A tap of Super alone toggles the overview.
//! - Bound axis motion (Mod+WheelScroll*, Mod+TouchpadScroll* binds) runs the bind's action once
//!   per notch.
//! - A three-finger swipe moves the focus one window; four fingers move it sideways and zoom the
//!   camera up or down. With Super, three fingers pan the camera and four move the window.

use std::time::{Duration, Instant};

use newm_canvas::Dir;
use niri_config::{Action, Trigger};

/// `river_seat_v1.modifiers` bits.
pub const SHIFT: u32 = 1;
pub const CTRL: u32 = 4;
pub const ALT: u32 = 8;
/// The mod key: Super (Mod4), which `Mod` binds map to.
pub const SUPER: u32 = 64;
/// Caps Lock and Num Lock, which no bind names.
const LOCKS: u32 = 2 | 16;

/// Longest press of Super alone that counts as a tap.
pub const SUPER_TAP: Duration = Duration::from_millis(300);

/// How far the pointer moves with Super held before it pans the canvas, in logical pixels, so
/// jitter while pressing a Super shortcut never pans.
pub const PAN_THRESHOLD: f64 = 8.;

/// How long Super is held before pointer motion counts toward a pan: the hand settling as it
/// presses Super, on a mouse button above all, pans nothing, and a Super+click or
/// Super+right-drag that follows starts where the pointer is.
pub const PAN_DELAY: Duration = Duration::from_millis(100);

/// Touchpad motion past which a swipe takes its axis: GNOME's swipe threshold.
pub const SWIPE_THRESHOLD: f64 = 16.;

/// The least touchpad motion one step of a swipe takes: 1/12 of the 1200 a pan takes to scroll
/// across the working area, so small tiles on a wide screen do not step for every fraction of a
/// millimetre the fingers go.
pub const STEP_MIN: f64 = 100.;

/// The least touchpad motion that moves a window one tile on the camera pulled back to show the
/// whole canvas: half of [`STEP_MIN`], still three times [`SWIPE_THRESHOLD`].
pub const ZOOMED_STEP_MIN: f64 = 50.;

/// The touchpad motion one zoom step of a four-finger swipe takes.
pub const ZOOM_STEP_MOTION: f64 = 100.;

/// The touchpad pinches the window manager binds, by finger count and whether Super is held:
/// each that [`pinch_zooms`]. A pinch it binds reaches no app.
pub const PINCHES: [(u32, bool); 7] = [
    (3, false),
    (4, false),
    (5, false),
    (2, true),
    (3, true),
    (4, true),
    (5, true),
];

/// Whether a touchpad pinch with `fingers` zooms the camera: three or more fingers do, and any
/// pinch with Super held does. A plain two-finger pinch belongs to the app under the pointer, as
/// browsers and image viewers zoom with it.
pub fn pinch_zooms(fingers: u32, with_mod: bool) -> bool {
    fingers >= 3 || with_mod
}

/// How often the pointer's position is asked for while Super is held without a pointer operation:
/// river starts no manage sequence for motion alone.
pub const POLL: Duration = Duration::from_millis(16);

/// `value120` of one wheel notch.
const WHEEL_TICK: f64 = 120.;

/// Touchpad scroll one step of a `TouchpadScroll*` bind takes (niri's finger scroll tracker).
const FINGER_TICK: f64 = 10.;

/// What river reports, in the order it reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum In {
    /// The seat's modifiers changed.
    Mods {
        old: u32,
        new: u32,
    },
    /// A key binding was pressed, or any other key while Super is held (river's `stop_repeat`
    /// on the Super binding).
    Key,
    /// Escape, bound while a drag runs.
    Escape,
    /// A button went down on a window without a binding.
    Interaction,
    /// Where the pointer is, in the global space.
    Position(i32, i32),
    /// Whether the pointer is now within the pan threshold of an edge of the output layout,
    /// where the cursor stops while the motion goes on.
    Edge(bool),
    /// Super+left went down or up.
    Button(bool),
    /// Super+right (the secondary button) went down or up.
    Secondary(bool),
    /// The pointer operation's motion since it started.
    OpDelta(i32, i32),
    /// The pointer's relative motion since the previous one, as the cursor moved (`accel`) and
    /// without pointer acceleration (`unaccel`): gestures v4's `pointer_motion`.
    Motion {
        accel: (f64, f64),
        unaccel: (f64, f64),
    },
    /// Every button of the pointer operation is up.
    OpRelease,
    /// `queryWindowInfo` asks for a window: the next click picks what is under the pointer,
    /// Escape cancels.
    Pick,
    SwipeBegin {
        fingers: u32,
        with_mod: bool,
    },
    SwipeUpdate {
        dx: f64,
        dy: f64,
    },
    SwipeEnd,
    /// A bound touchpad pinch began (see [`PINCHES`]).
    PinchBegin {
        fingers: u32,
        with_mod: bool,
    },
    /// The pinch's fingers are `scale` times as far apart as when it began.
    PinchUpdate {
        scale: f64,
    },
    PinchEnd,
    /// Bound axis motion; `wheel` for a wheel's notches, else touchpad or continuous scrolling.
    Axis {
        horizontal: bool,
        wheel: bool,
        value: f64,
        value120: i32,
    },
}

/// What the river layer does about it.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// A bind's action.
    Act(Action),
    /// Starts or ends river's pointer operation.
    OpStart,
    OpEnd,
    /// Pans the camera of the output under `at` so the content follows the pointer's `by`.
    Pan {
        at: (i32, i32),
        by: (f64, f64),
    },
    /// The pan ended: the window nearest the middle takes the focus.
    PanEnd,
    /// Picks the window under the pointer for a drag.
    DragBegin(i32, i32),
    /// The dragged window is drawn this far from its place, in on-screen pixels.
    DragTo(f64, f64),
    /// Drops the dragged window on the tiles nearest where it is drawn.
    Drop,
    /// Cancels the drag: nothing drops, the window glides home.
    CancelDrag,
    /// Picks the window under the pointer and its nearest edges for a resize drag.
    ResizeBegin(i32, i32),
    /// The pointer moved this far since the resize drag began, in on-screen pixels.
    ResizeTo(f64, f64),
    /// Applies the resize the drag previews.
    ResizeEnd,
    /// Cancels the resize drag: the window keeps its size.
    CancelResize,
    /// Zoom steps of a four-finger swipe: positive pulls the camera back, as `zoom-out` does.
    Zoom(i32),
    /// Super and three fingers pan the camera of the output under `at` after the fingers'
    /// motion `by`, in touchpad units.
    SwipePan {
        at: (i32, i32),
        by: (f64, f64),
    },
    /// The fingers of the pan lifted, or Super went up: the camera coasts on with the touchpad's
    /// momentum.
    SwipePanEnd,
    /// In the overview, a vertical four-finger swipe takes hold of it.
    OverviewBegin,
    /// The fingers moved the overview this much further open (negative: closed).
    OverviewMove(f64),
    /// The fingers left, moving the overview this fast (progress per second).
    OverviewEnd(f64),
    /// One step of a swipe.
    Step(SwipeAction, Dir),
    /// Pulls the camera back until the whole canvas fits; remembers the zoom.
    ZoomToFit,
    /// Returns the camera to the zoom [`Cmd::ZoomToFit`] remembered, if any.
    ZoomBack,
    /// A pinch that zooms the active output's camera began.
    PinchBegin,
    /// The pinch's fingers are this many times as far apart as when it began: the camera takes
    /// the whole zoom step nearest to it (`model::moves::pinch_zoom`).
    PinchScale(f64),
    /// The pinch ended; the camera keeps its zoom.
    PinchEnd,
    /// A Super tap with `super-tap-zoom`: at 1:1 the camera zooms out to the whole step nearest
    /// this scale; at any other zoom it returns to 1:1.
    TapZoom(f64),
    /// A pick ended: clicked at this point of the global space, or cancelled.
    Picked(Option<(i32, i32)>),
}

/// What a touchpad swipe does, chosen when it begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwipeAction {
    /// Three fingers: the focus moves once to the window on the side the fingers go, as one
    /// press of Super+H/J/K/L moves it.
    Focus,
    /// Four fingers: sideways, the focus moves once as Super+H/L move it; up pulls the camera
    /// back and down brings it closer, a zoom step at a time, as `zoom-out` and `zoom-in` do.
    FocusOrZoom,
    /// Super and three fingers: the camera pans after the fingers, as Super with pointer motion
    /// pans it.
    Pan,
    /// Four fingers in the overview, Super or not: up or down, the overview follows the
    /// fingers; sideways, the focus steps.
    Overview,
    /// Super and four fingers: the focused window moves one tile at a time, pushing what is in
    /// the way, while the camera shows the whole canvas.
    Move,
}

impl SwipeAction {
    fn new(fingers: u32, with_mod: bool) -> Option<Self> {
        match (fingers, with_mod) {
            (3, false) => Some(Self::Focus),
            (4, false) => Some(Self::FocusOrZoom),
            (3, true) => Some(Self::Pan),
            (4, true) => Some(Self::Move),
            _ => None,
        }
    }

    fn with_mod(self) -> bool {
        matches!(self, Self::Pan | Self::Move)
    }

    fn step_min(self) -> f64 {
        if self == Self::Move {
            ZOOMED_STEP_MIN
        } else {
            STEP_MIN
        }
    }
}

/// How far a swipe went.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SwipeState {
    /// Moved this far, short of the threshold.
    Pending(f64, f64),
    /// Stepping along one axis; `rest` is the motion along it that makes no step yet.
    Stepping { horizontal: bool, rest: f64 },
    /// Holding the overview: the fingers' last update and their velocity, progress per second.
    Overview { at: Instant, velocity: f64 },
    /// Zooming the camera; `rest` is the vertical motion that makes no zoom step yet.
    Zooming { rest: f64 },
    /// Panning the camera.
    Panning,
    /// Done: the focus made its one move, or the mod key it began with was released.
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Swipe {
    action: SwipeAction,
    state: SwipeState,
}

/// What one update of a swipe does.
#[derive(Debug, PartialEq)]
enum SwipeEffect {
    Steps(Vec<Dir>),
    /// A vertical four-finger swipe took its axis, with this much progress already.
    OverviewBegin(f64),
    /// The overview follows the fingers this much further.
    OverviewMove(f64),
    /// Zoom steps: positive pulls the camera back, as `zoom-out` does.
    Zoom(i32),
    /// Starts a camera pan with the fingers' motion so far.
    PanBegin(f64, f64),
    /// The fingers' motion, for the camera pan.
    Pan(f64, f64),
}

/// The finger motion that opens the overview all the way (niri's `OVERVIEW_GESTURE_MOVEMENT`).
const OVERVIEW_GESTURE_MOVEMENT: f64 = 300.;

/// One update of a swipe by the fingers' motion `(dx, dy)`. The focus or the window goes where
/// the fingers go, whatever the scroll direction setting: the deltas are the fingers' own motion.
/// Once past [`SWIPE_THRESHOLD`] a swipe with Super and three fingers pans the camera with all of
/// that motion and every motion after it. Any other swipe takes the axis it went furthest along,
/// with all of that motion. A vertical four-finger swipe without Super then zooms the camera, one
/// step for each [`ZOOM_STEP_MOTION`] the fingers go, up pulling it back. Otherwise each `step` of
/// motion along the axis (at least the action's least step) is one step, and what is left carries
/// over; a focus swipe is done after its first step, so one swipe moves the focus one window.
fn swipe_update(
    swipe: Swipe,
    dx: f64,
    dy: f64,
    step: (f64, f64),
    now: Instant,
) -> (Swipe, SwipeEffect) {
    let with = |state| Swipe { state, ..swipe };
    let none = SwipeEffect::Steps(Vec::new());
    let (horizontal, mut rest) = match swipe.state {
        SwipeState::Ignored => return (swipe, none),
        SwipeState::Panning => return (swipe, SwipeEffect::Pan(dx, dy)),
        SwipeState::Zooming { rest } => return zoom_steps(swipe, rest + dy),
        SwipeState::Overview { at, velocity } => {
            // Up opens: the fingers' upward motion is progress.
            let d = -dy / OVERVIEW_GESTURE_MOVEMENT;
            let dt = now.saturating_duration_since(at).as_secs_f64();
            let velocity = if dt > 0. {
                (velocity + d / dt) / 2.
            } else {
                velocity
            };
            let state = SwipeState::Overview { at: now, velocity };
            return (with(state), SwipeEffect::OverviewMove(d));
        }
        SwipeState::Pending(x, y) => {
            let (x, y) = (x + dx, y + dy);
            if x.hypot(y) < SWIPE_THRESHOLD {
                return (with(SwipeState::Pending(x, y)), none);
            }
            if swipe.action == SwipeAction::Pan {
                return (with(SwipeState::Panning), SwipeEffect::PanBegin(x, y));
            }
            let horizontal = x.abs() >= y.abs();
            if !horizontal && swipe.action == SwipeAction::FocusOrZoom {
                return zoom_steps(swipe, y);
            }
            if !horizontal && swipe.action == SwipeAction::Overview {
                let state = SwipeState::Overview {
                    at: now,
                    velocity: 0.,
                };
                return (
                    with(state),
                    SwipeEffect::OverviewBegin(-y / OVERVIEW_GESTURE_MOVEMENT),
                );
            }
            (horizontal, if horizontal { x } else { y })
        }
        SwipeState::Stepping { horizontal, rest } => {
            (horizontal, rest + if horizontal { dx } else { dy })
        }
    };
    let (step, back, forth) = if horizontal {
        (step.0, Dir::Left, Dir::Right)
    } else {
        (step.1, Dir::Up, Dir::Down)
    };
    let step = step.max(swipe.action.step_min());
    let mut steps = Vec::new();
    while rest.abs() >= step {
        steps.push(if rest > 0. { forth } else { back });
        rest -= step.copysign(rest);
    }
    let once = matches!(swipe.action, SwipeAction::Focus | SwipeAction::FocusOrZoom);
    if once && !steps.is_empty() {
        steps.truncate(1);
        return (with(SwipeState::Ignored), SwipeEffect::Steps(steps));
    }
    (
        with(SwipeState::Stepping { horizontal, rest }),
        SwipeEffect::Steps(steps),
    )
}

/// A zooming swipe whose vertical motion short of a step is `rest`: the zoom steps in it, one
/// for each [`ZOOM_STEP_MOTION`], fingers going up (negative) pulling the camera back, and the
/// swipe with what is left.
fn zoom_steps(swipe: Swipe, rest: f64) -> (Swipe, SwipeEffect) {
    let steps = (rest / ZOOM_STEP_MOTION).trunc();
    let swipe = Swipe {
        state: SwipeState::Zooming {
            rest: rest - steps * ZOOM_STEP_MOTION,
        },
        ..swipe
    };
    (swipe, SwipeEffect::Zoom(-steps as i32))
}

/// Turns scroll motion into whole steps; a change of direction starts over (niri's
/// `ScrollTracker`).
#[derive(Clone, Copy, Debug, Default)]
struct Tracker {
    acc: f64,
    last: f64,
}

impl Tracker {
    fn accumulate(&mut self, amount: f64, tick: f64) -> i32 {
        if self.last * amount < 0. {
            self.acc = 0.;
        }
        self.last = amount;
        self.acc += amount;
        let ticks = (self.acc / tick).trunc();
        self.acc -= ticks * tick;
        ticks as i32
    }
}

/// A scroll bind: its trigger, its modifiers in river's bits, its action.
#[derive(Clone, Debug, PartialEq)]
pub struct AxisBind {
    pub trigger: Trigger,
    pub mods: u32,
    pub action: Action,
}

/// The scroll triggers an axis binding serves.
pub fn is_axis_trigger(t: Trigger) -> bool {
    matches!(
        t,
        Trigger::WheelScrollDown
            | Trigger::WheelScrollUp
            | Trigger::WheelScrollLeft
            | Trigger::WheelScrollRight
            | Trigger::TouchpadScrollDown
            | Trigger::TouchpadScrollUp
            | Trigger::TouchpadScrollLeft
            | Trigger::TouchpadScrollRight
    )
}

/// One axis binding river is asked for: an orientation and modifiers, and the sources the binds
/// on it use (`newm-river-gestures-v1` version 2 filters by source, so an
/// unbound source still reaches the application).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisBinding {
    pub horizontal: bool,
    pub mods: u32,
    /// Some `WheelScroll*` bind uses it: wheel and wheel-tilt events.
    pub wheel: bool,
    /// Some `TouchpadScroll*` bind uses it: finger and continuous events.
    pub finger: bool,
}

fn is_wheel(t: Trigger) -> bool {
    matches!(
        t,
        Trigger::WheelScrollDown
            | Trigger::WheelScrollUp
            | Trigger::WheelScrollLeft
            | Trigger::WheelScrollRight
    )
}

fn is_horizontal(t: Trigger) -> bool {
    matches!(
        t,
        Trigger::WheelScrollLeft
            | Trigger::WheelScrollRight
            | Trigger::TouchpadScrollLeft
            | Trigger::TouchpadScrollRight
    )
}

/// The trigger a step of scrolling fires.
fn axis_trigger(horizontal: bool, wheel: bool, positive: bool) -> Trigger {
    match (horizontal, wheel, positive) {
        (false, true, true) => Trigger::WheelScrollDown,
        (false, true, false) => Trigger::WheelScrollUp,
        (true, true, true) => Trigger::WheelScrollRight,
        (true, true, false) => Trigger::WheelScrollLeft,
        (false, false, true) => Trigger::TouchpadScrollDown,
        (false, false, false) => Trigger::TouchpadScrollUp,
        (true, false, true) => Trigger::TouchpadScrollRight,
        (true, false, false) => Trigger::TouchpadScrollLeft,
    }
}

/// The motion a Super pan takes from one pointer motion: the unaccelerated motion plus `share`
/// of what the pointer's acceleration adds to it (0: the hand's motion, 1: the cursor's).
fn pan_delta(accel: (f64, f64), unaccel: (f64, f64), share: f64) -> (f64, f64) {
    (
        unaccel.0 + (accel.0 - unaccel.0) * share,
        unaccel.1 + (accel.1 - unaccel.1) * share,
    )
}

/// Where a move or resize drag that began at operation delta `base` is at delta `(dx, dy)`: it
/// follows the cursor whatever share of the acceleration a pan takes.
fn drag_to(base: (i32, i32), resize: bool, (dx, dy): (i32, i32)) -> Cmd {
    let by = (f64::from(dx - base.0), f64::from(dy - base.1));
    if resize {
        Cmd::ResizeTo(by.0, by.1)
    } else {
        Cmd::DragTo(by.0, by.1)
    }
}

/// What the pointer is doing with Super.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Idle,
    /// Super is held without a pointer operation: motion from `from` past the threshold pans.
    /// `moved` is the pan's motion since Super went down, which counts instead of the cursor's
    /// when the pan takes less than all of the acceleration.
    Armed {
        from: (i32, i32),
        moved: (f64, f64),
    },
    /// Super is held near an edge of the output layout, where the cursor stops: the pointer
    /// operation runs already, begun at `at`, so motion past the edge still counts toward the
    /// threshold; `last` is its last delta.
    ArmedOp {
        from: (i32, i32),
        at: (i32, i32),
        last: (i32, i32),
        moved: (f64, f64),
    },
    /// Panning through the pointer operation; `last` is its last delta.
    Panning {
        last: (i32, i32),
    },
    /// Dragging a window through the pointer operation, which had moved `base` when it began:
    /// moving it with Super+left, or resizing it with Super+right when `resize`.
    Dragging {
        base: (i32, i32),
        resize: bool,
    },
    /// Picking a window for `queryWindowInfo` through the pointer operation: clients get no
    /// pointer input until the click.
    Picking,
}

/// One seat's pointer, Super key and gestures.
#[derive(Clone, Debug)]
pub struct Pointer {
    /// The overview is open: swipes are its gesture there.
    pub overview: bool,
    tap_overview: bool,
    /// The scale a Super tap zooms out to (`super-tap-zoom`); set, it takes the tap from the
    /// overview.
    tap_zoom: Option<f64>,
    binds: Vec<AxisBind>,
    mods: u32,
    /// The pointer is near an edge of the output layout (see [`In::Edge`]).
    edge: bool,
    /// When Super went down alone, while it can still be a tap.
    tap: Option<Instant>,
    pos: (i32, i32),
    mode: Mode,
    /// When Super went down, while it is held: motion counts toward a pan [`PAN_DELAY`] after.
    super_down: Option<Instant>,
    /// The share of the pointer's acceleration a Super pan takes (`pan-acceleration`): at 1 it
    /// follows the cursor, through its position and the operation's deltas; below, the
    /// [`In::Motion`]s.
    pan_share: f64,
    swipe: Option<Swipe>,
    /// A pinch zooms the camera.
    pinch: bool,
    /// Vertical and horizontal, for wheels and for fingers.
    wheel: [Tracker; 2],
    finger: [Tracker; 2],
}

impl Pointer {
    /// `tap_overview`: the config's `super-tap-overview`; `binds`: its scroll binds.
    pub fn new(tap_overview: bool, binds: Vec<AxisBind>) -> Self {
        Self {
            overview: false,
            tap_overview,
            tap_zoom: None,
            binds,
            mods: 0,
            edge: false,
            tap: None,
            pos: (0, 0),
            mode: Mode::Idle,
            super_down: None,
            pan_share: 1.,
            swipe: None,
            pinch: false,
            wheel: [Tracker::default(); 2],
            finger: [Tracker::default(); 2],
        }
    }

    /// A Super tap zooms the camera out to `scale` from 1:1 and back to 1:1 from any other zoom
    /// (`super-tap-zoom`), in place of the overview.
    pub fn with_tap_zoom(mut self, scale: Option<f64>) -> Self {
        self.tap_zoom = scale;
        self
    }

    /// The share of the acceleration a Super pan takes, from 0 (the hand's motion) to 1 (the
    /// cursor's, the default); below 1 it pans by [`In::Motion`]s.
    pub fn with_pan_acceleration(mut self, share: f64) -> Self {
        self.pan_share = share.clamp(0., 1.);
        self
    }

    /// This pointer under a reloaded config's `template`: the cursor's last known position
    /// carries over, since river reports it again only once the cursor moves. A Super pan begun
    /// before it moved again would otherwise start from (0, 0) and jump by the whole position.
    pub fn reconfigured(&self, template: &Pointer) -> Pointer {
        Pointer {
            pos: self.pos,
            edge: self.edge,
            ..template.clone()
        }
    }

    /// Whether a Super pan follows the cursor: `pan-acceleration` 1.
    fn pans_with_cursor(&self) -> bool {
        self.pan_share >= 1.
    }

    /// The axis bindings the scroll binds need: orientation (horizontal or not) and modifiers.
    pub fn axis_bindings(&self) -> Vec<AxisBinding> {
        let mut out: Vec<AxisBinding> = Vec::new();
        for b in &self.binds {
            let (horizontal, mods) = (is_horizontal(b.trigger), b.mods);
            let wheel = is_wheel(b.trigger);
            match out
                .iter_mut()
                .find(|a| a.horizontal == horizontal && a.mods == mods)
            {
                Some(a) => {
                    a.wheel |= wheel;
                    a.finger |= !wheel;
                }
                None => out.push(AxisBinding {
                    horizontal,
                    mods,
                    wheel,
                    finger: !wheel,
                }),
            }
        }
        out
    }

    /// Whether Super is held without a pointer operation, so the pointer's position must be
    /// asked for every [`POLL`].
    pub fn polling(&self) -> bool {
        matches!(self.mode, Mode::Armed { .. })
    }

    /// Whether a Super tap does anything: toggles the overview (`super-tap-overview`) or zooms
    /// (`super-tap-zoom`).
    pub fn taps(&self) -> bool {
        self.tap_overview || self.tap_zoom.is_some()
    }

    /// Whether Escape is bound: while a drag, a resize drag or a pick runs.
    pub fn escape_bound(&self) -> bool {
        matches!(self.mode, Mode::Dragging { .. } | Mode::Picking)
    }

    fn super_held(&self) -> bool {
        self.mods & SUPER != 0
    }

    /// Whether Super has been held [`PAN_DELAY`] by `now`: motion before then counts toward no
    /// pan, and the threshold counts from where the pointer is when it ends.
    fn settled(&self, now: Instant) -> bool {
        self.super_down
            .is_none_or(|down| now.saturating_duration_since(down) >= PAN_DELAY)
    }

    /// One event; `step` is the touchpad motion that scrolls the camera one tile now.
    pub fn handle(&mut self, ev: In, now: Instant, step: (f64, f64)) -> Vec<Cmd> {
        let mut out = Vec::new();
        match ev {
            In::Mods { old, new } => self.mods_changed(old, new, now, &mut out),
            In::Key | In::Interaction => self.tap = None,
            In::Escape if self.mode == Mode::Picking => {
                out.push(Cmd::Picked(None));
                out.push(Cmd::OpEnd);
                self.mode = self.rest_mode();
            }
            In::Pick => match self.mode {
                Mode::Idle | Mode::Armed { .. } => {
                    out.push(Cmd::OpStart);
                    self.mode = Mode::Picking;
                }
                Mode::ArmedOp { .. } => self.mode = Mode::Picking,
                Mode::Picking => {}
                Mode::Panning { .. } | Mode::Dragging { .. } => out.push(Cmd::Picked(None)),
            },
            In::Button(_) | In::Secondary(_) | In::OpRelease if self.mode == Mode::Picking => {
                if !matches!(ev, In::Button(true) | In::Secondary(true)) {
                    out.push(Cmd::Picked(Some(self.pos)));
                    out.push(Cmd::OpEnd);
                    self.mode = self.rest_mode();
                }
            }
            In::Escape => {
                if let Mode::Dragging { resize, .. } = self.mode {
                    out.push(if resize {
                        Cmd::CancelResize
                    } else {
                        Cmd::CancelDrag
                    });
                    out.push(Cmd::OpEnd);
                    self.mode = self.rest_mode();
                }
            }
            In::Position(x, y) => {
                self.pos = (x, y);
                if let Mode::Armed { from, moved } = self.mode {
                    let from = if self.settled(now) { from } else { self.pos };
                    self.mode = Mode::Armed { from, moved };
                    let (dx, dy) = (f64::from(x - from.0), f64::from(y - from.1));
                    if self.pans_with_cursor() && dx.hypot(dy) >= PAN_THRESHOLD {
                        self.tap = None;
                        self.mode = Mode::Panning { last: (0, 0) };
                        out.push(Cmd::OpStart);
                        out.push(Cmd::Pan {
                            at: from,
                            by: (dx, dy),
                        });
                    } else {
                        self.arm_at_edge(&mut out);
                    }
                }
            }
            In::Edge(edge) => {
                self.edge = edge;
                self.arm_at_edge(&mut out);
            }
            In::Button(true) | In::Secondary(true) => {
                self.tap = None;
                let resize = ev == In::Secondary(true);
                let begin = if resize {
                    Cmd::ResizeBegin(self.pos.0, self.pos.1)
                } else {
                    Cmd::DragBegin(self.pos.0, self.pos.1)
                };
                match self.mode {
                    Mode::Dragging { .. } | Mode::Picking => {}
                    Mode::ArmedOp { last, .. } => {
                        out.push(begin);
                        self.mode = Mode::Dragging { base: last, resize };
                    }
                    Mode::Panning { last } => {
                        out.push(Cmd::PanEnd);
                        out.push(begin);
                        self.mode = Mode::Dragging { base: last, resize };
                    }
                    Mode::Idle | Mode::Armed { .. } => {
                        out.push(Cmd::OpStart);
                        out.push(begin);
                        self.mode = Mode::Dragging {
                            base: (0, 0),
                            resize,
                        };
                    }
                }
            }
            In::Button(false) | In::Secondary(false) | In::OpRelease => {
                // Only the button that began the drag, or the last one up, ends it.
                if let Mode::Dragging { resize, .. } = self.mode
                    && (ev == In::OpRelease || (ev == In::Secondary(false)) == resize)
                {
                    out.push(if resize { Cmd::ResizeEnd } else { Cmd::Drop });
                    out.push(Cmd::OpEnd);
                    self.mode = self.rest_mode();
                }
            }
            In::OpDelta(dx, dy) if !self.pans_with_cursor() => match self.mode {
                // The pan goes by the motion events; a drag still follows the cursor.
                Mode::Panning { .. } => self.mode = Mode::Panning { last: (dx, dy) },
                Mode::ArmedOp {
                    from, at, moved, ..
                } => {
                    self.mode = Mode::ArmedOp {
                        from,
                        at,
                        last: (dx, dy),
                        moved,
                    }
                }
                Mode::Dragging { base, resize } => out.push(drag_to(base, resize, (dx, dy))),
                Mode::Idle | Mode::Armed { .. } | Mode::Picking => {}
            },
            In::OpDelta(dx, dy) => match self.mode {
                Mode::Panning { last } => {
                    if (dx, dy) != last {
                        out.push(Cmd::Pan {
                            at: self.pos,
                            by: (f64::from(dx - last.0), f64::from(dy - last.1)),
                        });
                        self.mode = Mode::Panning { last: (dx, dy) };
                    }
                }
                Mode::Dragging { base, resize } => out.push(drag_to(base, resize, (dx, dy))),
                Mode::ArmedOp {
                    from, at, moved, ..
                } => {
                    let at = if self.settled(now) {
                        at
                    } else {
                        (from.0 - dx, from.1 - dy)
                    };
                    let by = (at.0 - from.0 + dx, at.1 - from.1 + dy);
                    if f64::from(by.0).hypot(f64::from(by.1)) >= PAN_THRESHOLD {
                        self.tap = None;
                        self.mode = Mode::Panning { last: (dx, dy) };
                        out.push(Cmd::Pan {
                            at: from,
                            by: (f64::from(by.0), f64::from(by.1)),
                        });
                    } else {
                        self.mode = Mode::ArmedOp {
                            from,
                            at,
                            last: (dx, dy),
                            moved,
                        };
                    }
                }
                Mode::Idle | Mode::Armed { .. } | Mode::Picking => {}
            },
            In::Motion { accel, unaccel } if !self.pans_with_cursor() => {
                let settled = self.settled(now);
                self.pan_motion(pan_delta(accel, unaccel, self.pan_share), settled, &mut out);
            }
            In::Motion { .. } => {}
            In::SwipeBegin { fingers, with_mod } => {
                if !with_mod {
                    self.mod_released(&mut out);
                }
                // In the overview only four fingers act, as its own gesture, Super or not.
                let action = if self.overview {
                    (fingers == 4).then_some(SwipeAction::Overview)
                } else {
                    SwipeAction::new(fingers, with_mod)
                };
                self.swipe = action.map(|action| Swipe {
                    action,
                    state: SwipeState::Pending(0., 0.),
                });
                if with_mod {
                    self.tap = None;
                }
                if self.swipe.is_some_and(|s| s.action == SwipeAction::Move) {
                    out.push(Cmd::ZoomToFit);
                }
            }
            In::SwipeUpdate { dx, dy } => self.swipe_update(dx, dy, step, now, &mut out),
            In::SwipeEnd => match self.swipe.take().map(|s| s.state) {
                Some(SwipeState::Overview { velocity, .. }) => {
                    out.push(Cmd::OverviewEnd(velocity));
                }
                Some(SwipeState::Panning) => out.push(Cmd::SwipePanEnd),
                _ => {}
            },
            In::PinchBegin { fingers, with_mod } => {
                // Off in the overview; with Super it is no tap.
                self.pinch = pinch_zooms(fingers, with_mod) && !self.overview;
                if with_mod {
                    self.tap = None;
                }
                if self.pinch {
                    out.push(Cmd::PinchBegin);
                }
            }
            In::PinchUpdate { scale } => {
                if self.pinch {
                    out.push(Cmd::PinchScale(scale));
                }
            }
            In::PinchEnd => {
                if std::mem::take(&mut self.pinch) {
                    out.push(Cmd::PinchEnd);
                }
            }
            In::Axis {
                horizontal,
                wheel,
                value,
                value120,
            } => self.axis(horizontal, wheel, value, value120, &mut out),
        }
        out
    }

    /// Armed near an edge of the output layout, where the cursor stops before the motion does:
    /// the pointer operation starts at once, so its deltas count the motion past the edge.
    fn arm_at_edge(&mut self, out: &mut Vec<Cmd>) {
        if let Mode::Armed { from, moved } = self.mode
            && self.edge
        {
            out.push(Cmd::OpStart);
            self.mode = Mode::ArmedOp {
                from,
                at: self.pos,
                last: (0, 0),
                moved,
            };
        }
    }

    /// The motion `d` a pan taking less than all of the acceleration takes from one
    /// [`In::Motion`]: it counts toward the threshold while armed, from the moment Super has
    /// been held [`PAN_DELAY`] (`settled`), then pans.
    fn pan_motion(&mut self, d: (f64, f64), settled: bool, out: &mut Vec<Cmd>) {
        match &mut self.mode {
            Mode::Panning { .. } => {
                if d != (0., 0.) {
                    out.push(Cmd::Pan {
                        at: self.pos,
                        by: d,
                    });
                }
            }
            Mode::Armed { moved, .. } | Mode::ArmedOp { moved, .. } if !settled => {
                *moved = (0., 0.);
            }
            Mode::Armed { from, moved } | Mode::ArmedOp { from, moved, .. } => {
                *moved = (moved.0 + d.0, moved.1 + d.1);
                if moved.0.hypot(moved.1) >= PAN_THRESHOLD {
                    let (at, by) = (*from, *moved);
                    let last = match self.mode {
                        Mode::ArmedOp { last, .. } => last,
                        _ => {
                            out.push(Cmd::OpStart);
                            (0, 0)
                        }
                    };
                    self.tap = None;
                    self.mode = Mode::Panning { last };
                    out.push(Cmd::Pan { at, by });
                }
            }
            Mode::Idle | Mode::Dragging { .. } | Mode::Picking => {}
        }
    }

    /// Where the pointer goes back to after a drag or a pan: armed while Super is still held.
    fn rest_mode(&self) -> Mode {
        if self.super_held() {
            Mode::Armed {
                from: self.pos,
                moved: (0., 0.),
            }
        } else {
            Mode::Idle
        }
    }

    fn mods_changed(&mut self, old: u32, new: u32, now: Instant, out: &mut Vec<Cmd>) {
        self.mods = new;
        let others = SHIFT | CTRL | ALT;
        if new & SUPER != 0 && old & SUPER == 0 {
            self.tap = (new & others == 0).then_some(now);
            self.super_down = Some(now);
            if self.mode == Mode::Idle {
                self.mode = Mode::Armed {
                    from: self.pos,
                    moved: (0., 0.),
                };
                self.arm_at_edge(out);
            }
        } else if old & SUPER != 0 && new & SUPER == 0 {
            self.super_down = None;
            match self.mode {
                Mode::Panning { .. } => {
                    out.push(Cmd::OpEnd);
                    out.push(Cmd::PanEnd);
                    self.mode = Mode::Idle;
                }
                Mode::Armed { .. } => self.mode = Mode::Idle,
                Mode::ArmedOp { .. } => {
                    out.push(Cmd::OpEnd);
                    self.mode = Mode::Idle;
                }
                Mode::Idle | Mode::Dragging { .. } | Mode::Picking => {}
            }
            self.mod_released(out);
            let tapped = self
                .tap
                .take()
                .is_some_and(|t| now.saturating_duration_since(t) <= SUPER_TAP);
            if tapped {
                if let Some(scale) = self.tap_zoom {
                    out.push(Cmd::TapZoom(scale));
                } else if self.tap_overview {
                    out.push(Cmd::Act(Action::ToggleOverview));
                }
            }
        } else {
            self.tap = None;
        }
    }

    /// Super went up: a swipe begun with it does nothing more (its pan ends), and a camera a
    /// four-finger swipe with it pulled back returns.
    fn mod_released(&mut self, out: &mut Vec<Cmd>) {
        if let Some(s) = self.swipe.as_mut().filter(|s| s.action.with_mod()) {
            if s.state == SwipeState::Panning {
                out.push(Cmd::SwipePanEnd);
            }
            s.state = SwipeState::Ignored;
        }
        out.push(Cmd::ZoomBack);
    }

    fn swipe_update(
        &mut self,
        dx: f64,
        dy: f64,
        step: (f64, f64),
        now: Instant,
        out: &mut Vec<Cmd>,
    ) {
        let Some(swipe) = self.swipe else {
            return;
        };
        if swipe.action.with_mod() && !self.super_held() {
            if swipe.state != SwipeState::Ignored {
                self.mod_released(out);
            }
            return;
        }
        let (swipe, effect) = swipe_update(swipe, dx, dy, step, now);
        self.swipe = Some(swipe);
        match effect {
            SwipeEffect::Steps(steps) => {
                out.extend(steps.into_iter().map(|d| Cmd::Step(swipe.action, d)));
            }
            SwipeEffect::OverviewBegin(d) => {
                out.push(Cmd::OverviewBegin);
                out.push(Cmd::OverviewMove(d));
            }
            SwipeEffect::OverviewMove(d) => out.push(Cmd::OverviewMove(d)),
            SwipeEffect::Zoom(0) => {}
            SwipeEffect::Zoom(steps) => out.push(Cmd::Zoom(steps)),
            SwipeEffect::PanBegin(x, y) | SwipeEffect::Pan(x, y) => out.push(Cmd::SwipePan {
                at: self.pos,
                by: (x, y),
            }),
        }
    }

    fn axis(&mut self, horizontal: bool, wheel: bool, value: f64, v120: i32, out: &mut Vec<Cmd>) {
        if self.super_held() {
            self.tap = None;
        }
        let k = usize::from(horizontal);
        let ticks = if wheel {
            let amount = if v120 != 0 {
                f64::from(v120)
            } else {
                value.signum() * WHEEL_TICK
            };
            self.wheel[k].accumulate(amount, WHEEL_TICK)
        } else {
            self.finger[k].accumulate(value, FINGER_TICK)
        };
        if ticks == 0 {
            return;
        }
        let trigger = axis_trigger(horizontal, wheel, ticks > 0);
        let mods = self.mods & !LOCKS;
        if let Some(b) = self
            .binds
            .iter()
            .find(|b| b.trigger == trigger && b.mods == mods)
        {
            for _ in 0..ticks.unsigned_abs() {
                out.push(Cmd::Act(b.action.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoom_binds() -> Vec<AxisBind> {
        vec![
            AxisBind {
                trigger: Trigger::WheelScrollUp,
                mods: SUPER,
                action: Action::ZoomIn,
            },
            AxisBind {
                trigger: Trigger::WheelScrollDown,
                mods: SUPER,
                action: Action::ZoomOut,
            },
        ]
    }

    fn pointer() -> (Pointer, Instant) {
        (Pointer::new(true, zoom_binds()), Instant::now())
    }

    fn run(p: &mut Pointer, now: Instant, evs: &[In]) -> Vec<Cmd> {
        evs.iter()
            .flat_map(|e| p.handle(e.clone(), now, (0., 0.)))
            .collect()
    }

    const DOWN: In = In::Mods { old: 0, new: SUPER };
    const UP: In = In::Mods { old: SUPER, new: 0 };

    fn acts(cmds: &[Cmd]) -> Vec<Action> {
        cmds.iter()
            .filter_map(|c| match c {
                Cmd::Act(a) => Some(a.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_super_tap_toggles_the_overview() {
        let (mut p, now) = pointer();
        let cmds = run(&mut p, now, &[DOWN, UP]);
        assert_eq!(acts(&cmds), vec![Action::ToggleOverview]);
    }

    #[test]
    fn with_super_tap_zoom_a_tap_zooms_instead_of_the_overview() {
        let (p, now) = pointer();
        let mut p = p.with_tap_zoom(Some(0.33));
        assert!(p.taps());
        let cmds = run(&mut p, now, &[DOWN, UP]);
        assert!(cmds.contains(&Cmd::TapZoom(0.33)), "{cmds:?}");
        assert!(acts(&cmds).is_empty());
        let mut p = Pointer::new(false, Vec::new()).with_tap_zoom(Some(0.33));
        assert!(p.taps());
        assert!(run(&mut p, now, &[DOWN, UP]).contains(&Cmd::TapZoom(0.33)));
    }

    #[test]
    fn a_long_press_is_no_tap() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN]);
        let later = now + SUPER_TAP + Duration::from_millis(1);
        assert!(acts(&p.handle(UP, later, (0., 0.))).is_empty());
    }

    #[test]
    fn the_tap_is_off_without_super_tap_overview() {
        let mut p = Pointer::new(false, Vec::new());
        let now = Instant::now();
        assert!(acts(&run(&mut p, now, &[DOWN, UP])).is_empty());
    }

    #[test]
    fn another_key_button_or_modifier_cancels_the_tap() {
        let (mut p, now) = pointer();
        for between in [
            In::Key,
            In::Interaction,
            In::Button(true),
            In::Mods {
                old: SUPER,
                new: SUPER | SHIFT,
            },
        ] {
            let cmds = run(&mut p, now, &[DOWN, between, UP]);
            assert!(acts(&cmds).is_empty(), "{cmds:?}");
            run(&mut p, now, &[In::Button(false)]);
        }
    }

    #[test]
    fn super_pressed_with_ctrl_alt_or_shift_is_no_tap() {
        for held in [CTRL, ALT, SHIFT] {
            let (mut p, now) = pointer();
            let cmds = run(
                &mut p,
                now,
                &[
                    In::Mods {
                        old: held,
                        new: held | SUPER,
                    },
                    In::Mods {
                        old: held | SUPER,
                        new: held,
                    },
                ],
            );
            assert!(acts(&cmds).is_empty());
        }
    }

    #[test]
    fn super_motion_pans_past_the_threshold_and_cancels_the_tap() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(100, 100), DOWN]);
        let now = now + PAN_DELAY;
        assert!(p.polling());
        assert!(run(&mut p, now, &[In::Position(105, 104)]).is_empty());
        let cmds = run(&mut p, now, &[In::Position(106, 106)]);
        assert_eq!(
            cmds,
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (100, 100),
                    by: (6., 6.)
                }
            ]
        );
        assert!(!p.polling());
        let cmds = run(&mut p, now, &[In::OpDelta(10, -4), In::OpDelta(10, -4)]);
        assert_eq!(
            cmds,
            vec![Cmd::Pan {
                at: (106, 106),
                by: (10., -4.)
            }]
        );
        let cmds = run(&mut p, now, &[UP]);
        assert_eq!(cmds, vec![Cmd::OpEnd, Cmd::PanEnd, Cmd::ZoomBack]);
        assert!(!p.polling());
    }

    #[test]
    fn motion_in_the_first_moments_of_super_counts_for_nothing() {
        let (mut p, now) = pointer();
        let soon = now + PAN_DELAY / 2;
        run(&mut p, now, &[In::Position(100, 100), DOWN]);
        let cmds = run(&mut p, soon, &[In::Position(130, 100), UP]);
        assert_eq!(
            acts(&cmds),
            vec![Action::ToggleOverview],
            "it cancels no tap"
        );

        let (mut p, now) = pointer();
        let soon = now + PAN_DELAY / 2;
        run(&mut p, now, &[In::Position(100, 100), DOWN]);
        assert!(
            run(&mut p, soon, &[In::Position(130, 100)]).is_empty(),
            "the hand settling"
        );
        let now = now + PAN_DELAY;
        assert!(
            run(&mut p, now, &[In::Position(135, 100)]).is_empty(),
            "the threshold counts from where the pointer was when the delay ended"
        );
        assert_eq!(
            run(&mut p, now, &[In::Position(138, 100)]),
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (130, 100),
                    by: (8., 0.)
                }
            ]
        );
    }

    #[test]
    fn the_delay_holds_below_full_acceleration_and_at_the_edge() {
        let mut p = Pointer::new(true, zoom_binds()).with_pan_acceleration(0.);
        let now = Instant::now();
        run(&mut p, now, &[In::Position(100, 100), DOWN]);
        let soon = now + PAN_DELAY / 2;
        assert!(run(&mut p, soon, &[motion((60., 0.), (20., 0.))]).is_empty());
        let later = now + PAN_DELAY;
        assert!(run(&mut p, later, &[motion((15., 0.), (5., 0.))]).is_empty());
        assert_eq!(
            run(&mut p, later, &[motion((9., 0.), (3., 0.))]),
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (100, 100),
                    by: (8., 0.)
                }
            ]
        );

        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(1278, 300), In::Edge(true)]);
        assert_eq!(run(&mut p, now, &[DOWN]), vec![Cmd::OpStart]);
        assert!(run(&mut p, now + PAN_DELAY / 2, &[In::OpDelta(30, 0)]).is_empty());
        let now = now + PAN_DELAY;
        assert!(run(&mut p, now, &[In::OpDelta(35, 0)]).is_empty());
        assert_eq!(
            run(&mut p, now, &[In::OpDelta(38, 0)]),
            vec![Cmd::Pan {
                at: (1278, 300),
                by: (8., 0.)
            }]
        );
    }

    fn pans(cmds: &[Cmd]) -> bool {
        cmds.iter()
            .any(|c| matches!(c, Cmd::Pan { .. } | Cmd::PanEnd))
    }

    #[test]
    fn a_super_click_in_the_first_moments_resizes_or_moves_and_pans_nothing() {
        for resize in [true, false] {
            // The cursor path (full acceleration) and the motion path (below it).
            for share in [1., 0.] {
                let mut p = Pointer::new(true, zoom_binds()).with_pan_acceleration(share);
                let now = Instant::now();
                run(&mut p, now, &[In::Position(100, 100), DOWN]);
                let soon = now + PAN_DELAY / 2;
                let settle = run(
                    &mut p,
                    soon,
                    &[motion((60., 12.), (20., 4.)), In::Position(120, 104)],
                );
                assert!(settle.is_empty(), "the hand settling: {settle:?}");
                let (press, begin) = if resize {
                    (In::Secondary(true), Cmd::ResizeBegin(120, 104))
                } else {
                    (In::Button(true), Cmd::DragBegin(120, 104))
                };
                assert_eq!(run(&mut p, soon, &[press]), vec![Cmd::OpStart, begin]);
                let drag = run(
                    &mut p,
                    now + PAN_DELAY * 2,
                    &[
                        motion((30., 6.), (10., 2.)),
                        In::Position(150, 110),
                        In::OpDelta(30, 6),
                    ],
                );
                assert!(!pans(&drag), "{drag:?}");
                assert_eq!(
                    drag.last(),
                    Some(&if resize {
                        Cmd::ResizeTo(30., 6.)
                    } else {
                        Cmd::DragTo(30., 6.)
                    })
                );
                let release = if resize {
                    In::Secondary(false)
                } else {
                    In::Button(false)
                };
                let end = run(&mut p, now + PAN_DELAY * 3, &[release, UP]);
                assert!(!pans(&end), "{end:?}");
            }
        }
    }

    #[test]
    fn past_the_first_moments_motion_pans_and_a_pan_under_way_keeps_on() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(100, 100), DOWN]);
        let later = now + PAN_DELAY;
        assert_eq!(
            run(&mut p, later, &[In::Position(110, 100)]),
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (100, 100),
                    by: (10., 0.)
                }
            ]
        );
        // Another modifier joining Super starts no new delay.
        let cmds = run(
            &mut p,
            later,
            &[
                In::Mods {
                    old: SUPER,
                    new: SUPER | SHIFT,
                },
                In::OpDelta(4, 0),
            ],
        );
        assert_eq!(
            cmds,
            vec![Cmd::Pan {
                at: (110, 100),
                by: (4., 0.)
            }]
        );
    }

    #[test]
    fn a_reloaded_config_keeps_the_cursor_position_for_the_next_pan() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(1000, 500)]);
        let mut p = p.reconfigured(&pointer().0);
        run(&mut p, now, &[DOWN]);
        let now = now + PAN_DELAY;
        let cmds = run(&mut p, now, &[In::Position(980, 500)]);
        assert_eq!(
            cmds,
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (1000, 500),
                    by: (-20., 0.)
                }
            ],
            "the pan starts where the cursor was, not at (0, 0)"
        );
    }

    #[test]
    fn at_the_edge_super_starts_the_operation_and_its_deltas_pan() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(1278, 300), In::Edge(true)]);
        assert_eq!(run(&mut p, now, &[DOWN]), vec![Cmd::OpStart]);
        let now = now + PAN_DELAY;
        assert!(
            !p.polling(),
            "the operation's deltas count the motion, no polling"
        );
        assert!(run(&mut p, now, &[In::OpDelta(5, 0)]).is_empty());
        let cmds = run(&mut p, now, &[In::OpDelta(12, 0), In::OpDelta(40, 0)]);
        assert_eq!(
            cmds,
            vec![
                Cmd::Pan {
                    at: (1278, 300),
                    by: (12., 0.)
                },
                Cmd::Pan {
                    at: (1278, 300),
                    by: (28., 0.)
                },
            ]
        );
        let cmds = run(&mut p, now, &[UP]);
        assert_eq!(&cmds[..2], &[Cmd::OpEnd, Cmd::PanEnd]);
        assert!(acts(&cmds).is_empty(), "a pan is no tap: {cmds:?}");
    }

    #[test]
    fn at_the_edge_a_tap_and_a_drag_still_work() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(0, 300), In::Edge(true)]);
        let cmds = run(&mut p, now, &[DOWN, UP]);
        assert_eq!(cmds[..2], [Cmd::OpStart, Cmd::OpEnd]);
        assert_eq!(acts(&cmds), vec![Action::ToggleOverview]);
        run(&mut p, now, &[DOWN, In::OpDelta(2, 1)]);
        let cmds = run(&mut p, now, &[In::Button(true), In::OpDelta(30, 1)]);
        assert_eq!(cmds, vec![Cmd::DragBegin(0, 300), Cmd::DragTo(28., 0.)]);
        assert_eq!(
            run(&mut p, now, &[In::Button(false)]),
            vec![Cmd::Drop, Cmd::OpEnd]
        );
    }

    #[test]
    fn reaching_the_edge_while_armed_starts_the_operation() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(1270, 300), DOWN]);
        let now = now + PAN_DELAY;
        assert!(p.polling());
        let cmds = run(&mut p, now, &[In::Position(1274, 300), In::Edge(true)]);
        assert_eq!(cmds, vec![Cmd::OpStart]);
        let cmds = run(&mut p, now, &[In::OpDelta(4, 0)]);
        assert_eq!(
            cmds,
            vec![Cmd::Pan {
                at: (1270, 300),
                by: (8., 0.)
            }]
        );
    }

    #[test]
    fn a_pan_takes_its_share_of_the_acceleration() {
        let (accel, unaccel) = ((30., -12.), (10., -4.));
        assert_eq!(pan_delta(accel, unaccel, 1.), accel);
        assert_eq!(pan_delta(accel, unaccel, 0.), unaccel);
        assert_eq!(pan_delta(accel, unaccel, 0.25), (15., -6.));
    }

    fn motion(accel: (f64, f64), unaccel: (f64, f64)) -> In {
        In::Motion { accel, unaccel }
    }

    #[test]
    fn at_full_acceleration_a_pan_follows_the_cursor_not_the_motion() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(100, 100), DOWN]);
        let now = now + PAN_DELAY;
        assert!(run(&mut p, now, &[motion((40., 0.), (10., 0.))]).is_empty());
        let cmds = run(&mut p, now, &[In::Position(140, 100)]);
        assert_eq!(
            cmds,
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (100, 100),
                    by: (40., 0.)
                }
            ]
        );
        let cmds = run(
            &mut p,
            now,
            &[motion((30., 0.), (10., 0.)), In::OpDelta(30, 0)],
        );
        assert_eq!(
            cmds,
            vec![Cmd::Pan {
                at: (140, 100),
                by: (30., 0.)
            }]
        );
    }

    #[test]
    fn below_full_acceleration_a_pan_takes_its_share_of_each_motion() {
        let mut p = Pointer::new(true, zoom_binds()).with_pan_acceleration(0.25);
        let now = Instant::now();
        // Motion handled before Super goes down in the same sequence counts for nothing.
        let cmds = run(
            &mut p,
            now,
            &[In::Position(100, 100), motion((300., 0.), (100., 0.)), DOWN],
        );
        assert!(cmds.is_empty(), "{cmds:?}");
        let now = now + PAN_DELAY;
        let step = motion((6., 0.), (2., 0.));
        assert!(run(&mut p, now, &[In::Position(106, 100), step.clone()]).is_empty());
        assert!(
            run(&mut p, now, &[In::Position(112, 100), step.clone()]).is_empty(),
            "the cursor's 12 px are past the threshold, the pan's 6 are not"
        );
        let cmds = run(&mut p, now, &[In::Position(118, 100), step]);
        assert_eq!(
            cmds,
            vec![
                Cmd::OpStart,
                Cmd::Pan {
                    at: (100, 100),
                    by: (9., 0.)
                }
            ]
        );
        let evs = [
            In::Position(148, 88),
            motion((30., -12.), (10., -4.)),
            In::OpDelta(30, -12),
        ];
        assert_eq!(
            run(&mut p, now, &evs),
            vec![Cmd::Pan {
                at: (148, 88),
                by: (15., -6.)
            }]
        );
        let cmds = run(&mut p, now, &[UP]);
        assert_eq!(cmds, vec![Cmd::OpEnd, Cmd::PanEnd, Cmd::ZoomBack]);
        assert!(acts(&cmds).is_empty(), "a pan is no tap: {cmds:?}");
    }

    #[test]
    fn below_full_acceleration_the_edge_operation_still_drags_by_its_deltas() {
        let mut p = Pointer::new(true, zoom_binds()).with_pan_acceleration(0.);
        let now = Instant::now();
        run(&mut p, now, &[In::Position(1278, 300), In::Edge(true)]);
        assert_eq!(run(&mut p, now, &[DOWN]), vec![Cmd::OpStart]);
        let now = now + PAN_DELAY;
        assert!(
            run(
                &mut p,
                now,
                &[motion((20., 0.), (5., 0.)), In::OpDelta(20, 0)]
            )
            .is_empty(),
            "the hand moved 5 px, short of the threshold"
        );
        let cmds = run(
            &mut p,
            now,
            &[motion((20., 0.), (5., 0.)), In::OpDelta(40, 0)],
        );
        assert_eq!(
            cmds,
            vec![Cmd::Pan {
                at: (1278, 300),
                by: (10., 0.)
            }]
        );
        let cmds = run(&mut p, now, &[In::OpDelta(45, 0), In::Button(true)]);
        assert_eq!(cmds, vec![Cmd::PanEnd, Cmd::DragBegin(1278, 300)]);
        let cmds = run(
            &mut p,
            now,
            &[motion((9., 0.), (3., 0.)), In::OpDelta(54, 0)],
        );
        assert_eq!(cmds, vec![Cmd::DragTo(9., 0.)], "a drag follows the cursor");
    }

    #[test]
    fn below_full_acceleration_a_resize_drag_still_follows_the_cursor() {
        let mut p = Pointer::new(true, zoom_binds()).with_pan_acceleration(0.);
        let now = Instant::now();
        run(&mut p, now, &[In::Position(300, 200), DOWN]);
        assert_eq!(
            run(&mut p, now, &[In::Secondary(true)]),
            vec![Cmd::OpStart, Cmd::ResizeBegin(300, 200)]
        );
        let cmds = run(
            &mut p,
            now,
            &[motion((30., 6.), (10., 2.)), In::OpDelta(30, 6)],
        );
        assert_eq!(cmds, vec![Cmd::ResizeTo(30., 6.)]);
        assert_eq!(
            run(&mut p, now, &[In::Secondary(false)]),
            vec![Cmd::ResizeEnd, Cmd::OpEnd]
        );
    }

    #[test]
    fn a_pick_takes_the_next_click_and_escape_cancels_it() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(300, 200)]);
        assert_eq!(run(&mut p, now, &[In::Pick]), vec![Cmd::OpStart]);
        assert!(p.escape_bound());
        let cmds = run(&mut p, now, &[In::Position(310, 220), In::OpRelease]);
        assert_eq!(cmds, vec![Cmd::Picked(Some((310, 220))), Cmd::OpEnd]);
        assert!(!p.escape_bound());
        run(&mut p, now, &[In::Pick]);
        let cmds = run(&mut p, now, &[In::Escape, In::OpRelease]);
        assert_eq!(cmds, vec![Cmd::Picked(None), Cmd::OpEnd]);
        run(&mut p, now, &[DOWN, In::Pick]);
        let cmds = run(
            &mut p,
            now,
            &[In::Button(true), In::Button(false), In::OpRelease],
        );
        assert_eq!(cmds, vec![Cmd::Picked(Some((310, 220))), Cmd::OpEnd]);
    }

    #[test]
    fn motion_without_super_never_pans() {
        let (mut p, now) = pointer();
        assert!(run(&mut p, now, &[In::Position(0, 0), In::Position(500, 500)]).is_empty());
        assert!(!p.polling());
    }

    #[test]
    fn super_left_drags_one_to_one_and_drops() {
        let (mut p, now) = pointer();
        let cmds = run(&mut p, now, &[In::Position(40, 50), DOWN, In::Button(true)]);
        assert_eq!(cmds, vec![Cmd::OpStart, Cmd::DragBegin(40, 50)]);
        let cmds = run(&mut p, now, &[In::OpDelta(3, 1), In::OpDelta(130, -20)]);
        assert_eq!(cmds, vec![Cmd::DragTo(3., 1.), Cmd::DragTo(130., -20.)]);
        let cmds = run(&mut p, now, &[In::OpRelease, In::Button(false)]);
        assert_eq!(cmds, vec![Cmd::Drop, Cmd::OpEnd]);
        assert!(p.polling(), "Super still held: armed for a pan again");
    }

    #[test]
    fn a_drag_outlives_super_until_the_button_is_up() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN, In::Button(true)]);
        let cmds = run(&mut p, now, &[UP, In::OpDelta(5, 5)]);
        assert_eq!(cmds, vec![Cmd::ZoomBack, Cmd::DragTo(5., 5.)]);
        assert_eq!(
            run(&mut p, now, &[In::OpRelease]),
            vec![Cmd::Drop, Cmd::OpEnd]
        );
        assert!(!p.polling());
    }

    #[test]
    fn a_button_during_a_pan_ends_it_and_drags_from_there() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(0, 0), DOWN]);
        let now = now + PAN_DELAY;
        run(&mut p, now, &[In::Position(20, 0)]);
        run(&mut p, now, &[In::OpDelta(30, 0)]);
        let cmds = run(&mut p, now, &[In::Button(true), In::OpDelta(40, 2)]);
        assert_eq!(
            cmds,
            vec![Cmd::PanEnd, Cmd::DragBegin(20, 0), Cmd::DragTo(10., 2.)]
        );
    }

    #[test]
    fn super_wheel_zooms_one_step_per_notch() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN]);
        let notch = |v120| In::Axis {
            horizontal: false,
            wheel: true,
            value: 0.,
            value120: v120,
        };
        assert_eq!(
            acts(&run(&mut p, now, &[notch(120)])),
            vec![Action::ZoomOut]
        );
        assert_eq!(
            acts(&run(&mut p, now, &[notch(-240)])),
            vec![Action::ZoomIn, Action::ZoomIn]
        );
        assert!(acts(&run(&mut p, now, &[notch(-60)])).is_empty());
        assert_eq!(acts(&run(&mut p, now, &[notch(-60)])), vec![Action::ZoomIn]);
        let cmds = run(&mut p, now, &[UP]);
        assert!(acts(&cmds).is_empty(), "a wheel with Super is no tap");
    }

    fn pinch(
        p: &mut Pointer,
        now: Instant,
        fingers: u32,
        with_mod: bool,
        scales: &[f64],
    ) -> Vec<Cmd> {
        let mut evs = vec![In::PinchBegin { fingers, with_mod }];
        evs.extend(scales.iter().map(|&scale| In::PinchUpdate { scale }));
        evs.push(In::PinchEnd);
        run(p, now, &evs)
    }

    #[test]
    fn the_bound_pinches_are_those_that_zoom() {
        for (fingers, with_mod) in PINCHES {
            assert!(
                pinch_zooms(fingers, with_mod),
                "{fingers} fingers, Super {with_mod}"
            );
        }
        assert!(
            !pinch_zooms(2, false),
            "a plain two-finger pinch is the app's"
        );
        assert!(!PINCHES.contains(&(2, false)));
        for fingers in 3..=5 {
            assert!(PINCHES.contains(&(fingers, false)) && PINCHES.contains(&(fingers, true)));
        }
        assert!(PINCHES.contains(&(2, true)));
    }

    #[test]
    fn a_pinch_zooms_with_the_fingers_and_ends() {
        let (mut p, now) = pointer();
        assert_eq!(
            pinch(&mut p, now, 3, false, &[0.8, 0.6]),
            vec![
                Cmd::PinchBegin,
                Cmd::PinchScale(0.8),
                Cmd::PinchScale(0.6),
                Cmd::PinchEnd
            ]
        );
    }

    #[test]
    fn super_and_two_fingers_zoom_and_are_no_tap() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN]);
        let cmds = pinch(&mut p, now, 2, true, &[1.25]);
        assert_eq!(cmds.first(), Some(&Cmd::PinchBegin));
        assert!(cmds.contains(&Cmd::PinchScale(1.25)));
        let cmds = run(&mut p, now, &[UP]);
        assert!(acts(&cmds).is_empty(), "a pinch with Super is no tap");
    }

    #[test]
    fn in_the_overview_a_pinch_zooms_nothing() {
        let (mut p, now) = pointer();
        p.overview = true;
        assert!(pinch(&mut p, now, 3, false, &[0.5]).is_empty());
    }

    #[test]
    fn axis_bindings_follow_the_binds() {
        let (p, _) = pointer();
        assert_eq!(
            p.axis_bindings(),
            vec![AxisBinding {
                horizontal: false,
                mods: SUPER,
                wheel: true,
                finger: false
            }]
        );
    }

    fn swipe(fingers: u32, with_mod: bool, motion: &[(f64, f64)], step: (f64, f64)) -> Vec<Cmd> {
        let (mut p, now) = pointer();
        if with_mod {
            run(&mut p, now, &[DOWN]);
        }
        let mut out = p.handle(In::SwipeBegin { fingers, with_mod }, now, step);
        for &(dx, dy) in motion {
            out.extend(p.handle(In::SwipeUpdate { dx, dy }, now, step));
        }
        out.extend(p.handle(In::SwipeEnd, now, step));
        out
    }

    fn steps(cmds: &[Cmd]) -> Vec<(SwipeAction, Dir)> {
        cmds.iter()
            .filter_map(|c| match c {
                Cmd::Step(a, d) => Some((*a, *d)),
                _ => None,
            })
            .collect()
    }

    fn swipe_begin(fingers: u32, with_mod: bool) -> Option<Swipe> {
        SwipeAction::new(fingers, with_mod).map(|action| Swipe {
            action,
            state: SwipeState::Pending(0., 0.),
        })
    }

    /// What a swipe does over the fingers' motion: its steps, its zoom steps and its camera pan.
    #[derive(Debug, Default, PartialEq)]
    struct Swiped {
        steps: Vec<Dir>,
        zoom: Vec<i32>,
        pan: Option<Vec<(f64, f64)>>,
    }

    /// What a swipe with `fingers`, Super held or not, does over the fingers' `motion`, one
    /// event at a time.
    fn run_swipe(fingers: u32, with_mod: bool, motion: &[(f64, f64)], step: (f64, f64)) -> Swiped {
        let now = Instant::now();
        let mut swipe = swipe_begin(fingers, with_mod).unwrap();
        let mut swiped = Swiped::default();
        for &(dx, dy) in motion {
            let (next, effect) = swipe_update(swipe, dx, dy, step, now);
            swipe = next;
            match effect {
                SwipeEffect::Steps(made) => swiped.steps.extend(made),
                SwipeEffect::Zoom(0) => {}
                SwipeEffect::Zoom(delta) => swiped.zoom.push(delta),
                SwipeEffect::PanBegin(dx, dy) => {
                    assert!(swiped.pan.is_none(), "the pan begins once");
                    swiped.pan = Some(vec![(dx, dy)]);
                }
                SwipeEffect::Pan(dx, dy) => swiped.pan.as_mut().unwrap().push((dx, dy)),
                SwipeEffect::OverviewBegin(_) | SwipeEffect::OverviewMove(_) => {
                    panic!("outside the overview no swipe is its gesture")
                }
            }
        }
        swiped
    }

    fn dirs(fingers: u32, with_mod: bool, motion: &[(f64, f64)], step: (f64, f64)) -> Vec<Dir> {
        let swiped = run_swipe(fingers, with_mod, motion, step);
        assert_eq!((&swiped.zoom, &swiped.pan), (&Vec::new(), &None));
        swiped.steps
    }

    #[test]
    fn swipe_actions_by_fingers_and_mod_key() {
        let action = |fingers, with_mod| swipe_begin(fingers, with_mod).map(|s| s.action);
        assert_eq!(action(3, false), Some(SwipeAction::Focus));
        assert_eq!(action(4, false), Some(SwipeAction::FocusOrZoom));
        assert_eq!(action(3, true), Some(SwipeAction::Pan));
        assert_eq!(action(4, true), Some(SwipeAction::Move));
        assert_eq!(action(2, false), None);
        assert_eq!(action(5, true), None);
    }

    #[test]
    fn swipe_below_the_threshold_does_nothing() {
        let begun = swipe_begin(3, false).unwrap();
        let (begun, effect) = swipe_update(begun, 10., 10., (1., 1.), Instant::now());
        assert_eq!(begun.state, SwipeState::Pending(10., 10.));
        assert_eq!(effect, SwipeEffect::Steps(Vec::new()));
        for (fingers, with_mod) in [(3, false), (4, false), (3, true), (4, true)] {
            assert_eq!(
                run_swipe(fingers, with_mod, &[(10., 0.), (-4., 5.)], (1., 1.)),
                Swiped::default()
            );
        }
        assert!(steps(&swipe(3, false, &[(10., 5.)], (1., 1.))).is_empty());
    }

    #[test]
    fn three_fingers_move_the_focus_once_on_either_axis() {
        let right = dirs(3, false, &[(20., 0.); 30], (150., 150.));
        assert_eq!(right, [Dir::Right], "600 of motion is still one window");
        let down = dirs(3, false, &[(0., 25.); 20], (120., 125.));
        assert_eq!(down, [Dir::Down]);
        let up = dirs(3, false, &[(0., -20.); 30], (100., 100.));
        assert_eq!(up, [Dir::Up]);
        let back = dirs(
            3,
            false,
            &[(20., 0.), (200., 0.), (-400., 0.)],
            (100., 100.),
        );
        assert_eq!(
            back,
            [Dir::Right],
            "going back in the same swipe moves nothing"
        );
        let fast = dirs(3, false, &[(450., 0.)], (100., 100.));
        assert_eq!(fast, [Dir::Right], "one fast event is one window too");
        let short = dirs(3, false, &[(20., 0.); 4], (35., 35.));
        assert!(short.is_empty(), "a step is never below STEP_MIN, 100");
        let cmds = swipe(
            3,
            false,
            &[(-120., 10.), (-100., 0.), (-300., 0.)],
            (0., 0.),
        );
        assert_eq!(steps(&cmds), vec![(SwipeAction::Focus, Dir::Left)]);
    }

    #[test]
    fn a_step_is_the_tile_motion_but_never_below_the_least() {
        let cmds = swipe(4, true, &[(450., 0.)], (200., 200.));
        assert_eq!(steps(&cmds).len(), 2);
        let cmds = swipe(4, true, &[(450., 0.)], (10., 10.));
        assert_eq!(steps(&cmds).len(), 9, "ZOOMED_STEP_MIN, 50, a step");
    }

    #[test]
    fn swipe_follows_the_dominant_axis() {
        for (fingers, with_mod, n) in [(3, false, 1), (4, true, 6)] {
            let moves = dirs(fingers, with_mod, &[(-20., 12.); 30], (100., 100.));
            assert_eq!(moves, vec![Dir::Left; n]);
            let moves = dirs(fingers, with_mod, &[(5., -30.); 20], (100., 100.));
            assert_eq!(moves, vec![Dir::Up; n]);
        }
        let cmds = swipe(4, true, &[(20., 15.), (0., 200.)], (0., 0.));
        assert!(steps(&cmds).is_empty(), "sideways took the axis: {cmds:?}");
    }

    #[test]
    fn four_fingers_sideways_move_the_focus_once() {
        let swiped = run_swipe(4, false, &[(-20., 12.); 30], (100., 100.));
        assert_eq!(swiped.steps, [Dir::Left]);
        assert!(swiped.zoom.is_empty());
        let swiped = run_swipe(4, false, &[(20., 0.), (0., 400.)], (100., 100.));
        assert_eq!(swiped, Swiped::default(), "a sideways swipe stays sideways");
        let side = swipe(4, false, &[(-30., 2.), (-90., 0.), (-200., 0.)], (0., 0.));
        assert_eq!(steps(&side), vec![(SwipeAction::FocusOrZoom, Dir::Left)]);
    }

    #[test]
    fn four_fingers_up_pull_the_camera_back_and_down_bring_it_closer() {
        let swiped = run_swipe(4, false, &[(3., -10.), (2., -10.), (40., -85.)], (1., 1.));
        assert!(swiped.steps.is_empty());
        assert_eq!(
            swiped.zoom,
            [1],
            "105 up is one step back, whatever the tile"
        );
        let swiped = run_swipe(4, false, &[(0., -50.); 9], (1., 1.));
        assert_eq!(swiped.zoom, [1; 4], "450 up: a step for each 100");
        let swiped = run_swipe(4, false, &[(0., 30.), (100., 280.)], (1., 1.));
        assert_eq!(
            swiped.zoom,
            [-3],
            "310 down in one event is three steps closer"
        );
        let swiped = run_swipe(4, false, &[(0., -120.), (0., 240.)], (1., 1.));
        assert_eq!(swiped.zoom, [1, -2], "going back zooms back, and on");
        let up = swipe(4, false, &[(0., -30.), (0., -300.)], (0., 0.));
        assert_eq!(
            up[1..],
            [Cmd::Zoom(3)],
            "no overview gesture outside the overview"
        );
        let down = swipe(4, false, &[(2., 30.), (0., 80.)], (0., 0.));
        assert_eq!(down[1..], [Cmd::Zoom(-1)]);
    }

    #[test]
    fn mod_key_and_three_fingers_pan_the_camera() {
        let motion = [(6., 8.), (9., 12.), (-5., 30.), (0., -2.)];
        let swiped = run_swipe(3, true, &motion, (1., 1.));
        assert!(swiped.steps.is_empty() && swiped.zoom.is_empty());
        assert_eq!(
            swiped.pan,
            Some(vec![(15., 20.), (-5., 30.), (0., -2.)]),
            "it begins with all the motion so far, then takes every delta on both axes"
        );
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(70, 80), DOWN]);
        let mut evs = vec![In::SwipeBegin {
            fingers: 3,
            with_mod: true,
        }];
        evs.extend(motion.iter().map(|&(dx, dy)| In::SwipeUpdate { dx, dy }));
        evs.push(In::SwipeEnd);
        let at = (70, 80);
        assert_eq!(
            run(&mut p, now, &evs),
            vec![
                Cmd::SwipePan { at, by: (15., 20.) },
                Cmd::SwipePan { at, by: (-5., 30.) },
                Cmd::SwipePan { at, by: (0., -2.) },
                Cmd::SwipePanEnd
            ],
            "the lift ends the pan"
        );
        let cmds = run(&mut p, now, &[UP]);
        assert!(acts(&cmds).is_empty(), "a swipe with Super is no tap");
    }

    #[test]
    fn releasing_super_ends_the_swipe_pan_once() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN]);
        let begin = In::SwipeBegin {
            fingers: 3,
            with_mod: true,
        };
        let cmds = run(&mut p, now, &[begin, In::SwipeUpdate { dx: 20., dy: 0. }]);
        assert!(matches!(cmds[..], [Cmd::SwipePan { .. }]));
        let cmds = run(&mut p, now, &[UP]);
        assert_eq!(
            cmds.iter().filter(|c| **c == Cmd::SwipePanEnd).count(),
            1,
            "{cmds:?}"
        );
        let more = [In::SwipeUpdate { dx: 90., dy: 9. }, In::SwipeEnd];
        assert!(run(&mut p, now, &more).is_empty());
    }

    #[test]
    fn released_swipe_does_nothing_more() {
        for fingers in [3, 4] {
            let mut swipe = swipe_begin(fingers, true).unwrap();
            swipe.state = SwipeState::Ignored;
            let (next, effect) = swipe_update(swipe, 500., 0., (100., 100.), Instant::now());
            assert_eq!(next, swipe);
            assert_eq!(effect, SwipeEffect::Steps(Vec::new()));
        }
    }

    #[test]
    fn in_the_overview_four_fingers_up_or_down_open_or_close_it() {
        let in_overview = |motion: &[(f64, f64)]| {
            let (mut p, now) = pointer();
            p.overview = true;
            let mut evs = vec![In::SwipeBegin {
                fingers: 4,
                with_mod: false,
            }];
            evs.extend(motion.iter().map(|&(dx, dy)| In::SwipeUpdate { dx, dy }));
            evs.push(In::SwipeEnd);
            run(&mut p, now, &evs)
        };
        let up = in_overview(&[(0., -30.), (0., -300.)]);
        assert_eq!(
            up[1..],
            [
                Cmd::OverviewBegin,
                Cmd::OverviewMove(0.1),
                Cmd::OverviewMove(1.),
                Cmd::OverviewEnd(0.)
            ]
        );
        assert!(steps(&up).is_empty() && acts(&up).is_empty());
        let down = in_overview(&[(2., 30.)]);
        assert_eq!(
            down[1..],
            [
                Cmd::OverviewBegin,
                Cmd::OverviewMove(-0.1),
                Cmd::OverviewEnd(0.)
            ]
        );
    }

    #[test]
    fn in_the_overview_swipes_are_its_gesture_and_still_no_tap() {
        let (mut p, now) = pointer();
        p.overview = true;
        run(&mut p, now, &[DOWN]);
        let step = (0., 0.);
        let mut out = p.handle(
            In::SwipeBegin {
                fingers: 4,
                with_mod: true,
            },
            now,
            step,
        );
        out.extend(p.handle(In::SwipeUpdate { dx: 0., dy: 40. }, now, step));
        out.extend(p.handle(In::SwipeEnd, now, step));
        assert!(!out.contains(&Cmd::ZoomToFit));
        assert!(out.contains(&Cmd::OverviewBegin) && out.contains(&Cmd::OverviewEnd(0.)));
        let mut three = p.handle(
            In::SwipeBegin {
                fingers: 3,
                with_mod: true,
            },
            now,
            step,
        );
        three.extend(p.handle(In::SwipeUpdate { dx: 90., dy: 0. }, now, step));
        three.extend(p.handle(In::SwipeEnd, now, step));
        assert!(steps(&three).is_empty() && acts(&three).is_empty());
        assert!(!three.iter().any(|c| matches!(c, Cmd::SwipePan { .. })));
        let up = p.handle(In::Mods { old: SUPER, new: 0 }, now, step);
        assert!(!acts(&up).contains(&Action::ToggleOverview));
    }

    #[test]
    fn mod_key_and_four_fingers_zoom_to_fit_and_move_in_smaller_steps() {
        let cmds = swipe(4, true, &[(60., 0.)], (0., 0.));
        assert_eq!(cmds.first(), Some(&Cmd::ZoomToFit));
        assert_eq!(steps(&cmds), vec![(SwipeAction::Move, Dir::Right)]);
    }

    #[test]
    fn releasing_super_mid_swipe_zooms_back_and_steps_nothing_more() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN]);
        let begin = In::SwipeBegin {
            fingers: 4,
            with_mod: true,
        };
        assert_eq!(run(&mut p, now, &[begin]), vec![Cmd::ZoomToFit]);
        let cmds = run(&mut p, now, &[UP]);
        assert!(cmds.contains(&Cmd::ZoomBack));
        assert!(acts(&cmds).is_empty(), "a swipe with Super is no tap");
        let cmds = run(&mut p, now, &[In::SwipeUpdate { dx: 500., dy: 0. }]);
        assert!(steps(&cmds).is_empty());
    }

    #[test]
    fn super_released_before_the_update_zooms_back_once() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN]);
        run(
            &mut p,
            now,
            &[In::SwipeBegin {
                fingers: 3,
                with_mod: true,
            }],
        );
        p.mods = 0;
        let cmds = run(
            &mut p,
            now,
            &[
                In::SwipeUpdate { dx: 500., dy: 0. },
                In::SwipeUpdate { dx: 500., dy: 0. },
            ],
        );
        assert_eq!(cmds, vec![Cmd::ZoomBack]);
    }

    #[test]
    fn scroll_trackers_restart_on_a_change_of_direction() {
        let mut t = Tracker::default();
        assert_eq!(t.accumulate(90., 120.), 0);
        assert_eq!(t.accumulate(-60., 120.), 0);
        assert_eq!(t.accumulate(-60., 120.), -1);
        assert_eq!(t.accumulate(250., 120.), 2);
    }

    #[test]
    fn the_overview_follows_the_fingers_with_their_velocity() {
        let (mut p, now) = pointer();
        p.overview = true;
        let step = (0., 0.);
        p.handle(
            In::SwipeBegin {
                fingers: 4,
                with_mod: false,
            },
            now,
            step,
        );
        p.handle(In::SwipeUpdate { dx: 0., dy: -30. }, now, step);
        let later = now + std::time::Duration::from_millis(100);
        let out = p.handle(In::SwipeUpdate { dx: 0., dy: -60. }, later, step);
        assert_eq!(out, vec![Cmd::OverviewMove(0.2)]);
        let Some(Cmd::OverviewEnd(v)) = p.handle(In::SwipeEnd, later, step).pop() else {
            panic!("no end");
        };
        assert!((v - 1.).abs() < 1e-9, "half of 2/s smoothed from rest: {v}");
    }

    #[test]
    fn escape_cancels_a_drag_only() {
        let (mut p, now) = pointer();
        assert!(run(&mut p, now, &[In::Escape]).is_empty());
        run(&mut p, now, &[DOWN, In::Button(true)]);
        assert!(p.escape_bound());
        let out = run(&mut p, now, &[In::Escape]);
        assert_eq!(out, vec![Cmd::CancelDrag, Cmd::OpEnd]);
        assert!(!p.escape_bound());
        assert!(
            run(&mut p, now, &[In::Button(false)]).is_empty(),
            "nothing drops"
        );
    }

    #[test]
    fn super_right_drags_a_resize_that_its_own_button_ends() {
        let (mut p, now) = pointer();
        let cmds = run(
            &mut p,
            now,
            &[In::Position(40, 50), DOWN, In::Secondary(true)],
        );
        assert_eq!(cmds, vec![Cmd::OpStart, Cmd::ResizeBegin(40, 50)]);
        assert!(p.escape_bound());
        let cmds = run(&mut p, now, &[In::OpDelta(3, 1), In::OpDelta(130, -20)]);
        assert_eq!(cmds, vec![Cmd::ResizeTo(3., 1.), Cmd::ResizeTo(130., -20.)]);
        assert!(
            run(&mut p, now, &[In::Button(true), In::Button(false)]).is_empty(),
            "the primary button neither drags nor ends the resize"
        );
        let cmds = run(&mut p, now, &[In::Secondary(false)]);
        assert_eq!(cmds, vec![Cmd::ResizeEnd, Cmd::OpEnd]);
        assert!(p.polling(), "Super still held: armed for a pan again");
    }

    #[test]
    fn the_secondary_button_neither_ends_a_move_drag_nor_resizes() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN, In::Button(true)]);
        assert!(run(&mut p, now, &[In::Secondary(true), In::Secondary(false)]).is_empty());
        assert_eq!(
            run(&mut p, now, &[In::Button(false)]),
            vec![Cmd::Drop, Cmd::OpEnd]
        );
    }

    #[test]
    fn each_super_right_click_is_a_resize_op_of_its_own() {
        // The model tells a double click from two resizes (`model/resize.rs`): a press it takes
        // for one begins no drag, and its op ends with the release like any other.
        let (mut p, now) = pointer();
        run(&mut p, now, &[In::Position(40, 50), DOWN]);
        let click = [In::Secondary(true), In::Secondary(false)];
        let one = vec![
            Cmd::OpStart,
            Cmd::ResizeBegin(40, 50),
            Cmd::ResizeEnd,
            Cmd::OpEnd,
        ];
        for n in 1..=3 {
            assert_eq!(run(&mut p, now, &click), one, "click {n}");
        }
        let cmds = run(
            &mut p,
            now,
            &[
                In::Secondary(true),
                In::OpDelta(-50, 30),
                In::OpDelta(-70, 30),
            ],
        );
        assert_eq!(
            cmds,
            vec![
                Cmd::OpStart,
                Cmd::ResizeBegin(40, 50),
                Cmd::ResizeTo(-50., 30.),
                Cmd::ResizeTo(-70., 30.)
            ],
            "a floating window's resize follows the pointer by the pixel"
        );
        assert_eq!(
            run(&mut p, now, &[In::Escape]),
            vec![Cmd::CancelResize, Cmd::OpEnd]
        );
    }

    #[test]
    fn escape_cancels_a_resize_drag() {
        let (mut p, now) = pointer();
        run(&mut p, now, &[DOWN, In::Secondary(true)]);
        let out = run(&mut p, now, &[In::Escape]);
        assert_eq!(out, vec![Cmd::CancelResize, Cmd::OpEnd]);
        assert!(!p.escape_bound());
        assert!(
            run(&mut p, now, &[In::Secondary(false), In::OpRelease]).is_empty(),
            "nothing resizes"
        );
    }
}
