//! The Wayland side of `crate::pointer`: per seat, the Super+left pointer binding, the swipe and
//! axis bindings of `newm-river-gestures-v1`, and a watch on Super, Ctrl, Alt and Shift. Their
//! events and the seat's pointer events queue up as [`In`]; the manage sequence feeds them to the
//! seat's [`Pointer`] and carries out its [`Cmd`]s on the model and the seat.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::Instant;

use niri_config::Config;
use niri_ipc::Reply;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_pointer;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

use crate::config;
use crate::model::Desktop;
use crate::pointer::{self, AxisBind, Cmd, In, Pointer, POLL};
use crate::protocols::newm_gestures::newm_river_axis_binding_v1::{self, NewmRiverAxisBindingV1};
use crate::protocols::newm_gestures::newm_river_gesture_binding_v1::{
    self, NewmRiverGestureBindingV1,
};
use crate::protocols::newm_gestures::newm_river_gestures_seat_v1::{
    self, Kind, NewmRiverGesturesSeatV1,
};
use crate::protocols::newm_gestures::newm_river_gestures_v1::NewmRiverGesturesV1;
use crate::protocols::window_management::river_pointer_binding_v1::{self, RiverPointerBindingV1};
use crate::protocols::window_management::river_seat_v1::{self, Modifiers, RiverSeatV1};
use crate::protocols::xkb_bindings::river_xkb_binding_v1::{self, RiverXkbBindingV1};
use crate::protocols::xkb_bindings::river_xkb_bindings_seat_v1::{self, RiverXkbBindingsSeatV1};
use crate::protocols::xkb_bindings::river_xkb_bindings_v1::RiverXkbBindingsV1;
use crate::river::State;

/// `BTN_LEFT` and `BTN_RIGHT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// Keysyms bound for the pointer: Escape cancels a drag; Super's own keys learn, through
/// `stop_repeat`, of any key pressed while Super is held, bound or not, so it is no tap.
const XK_ESCAPE: u32 = 0xff1b;
const XK_SUPER_L: u32 = 0xffeb;
const XK_SUPER_R: u32 = 0xffec;

/// What a key binding of the pointer is for.
#[derive(Clone, Copy, Debug)]
pub struct KeyData {
    seat: SeatKey,
    escape: bool,
}

/// The swipes bound: finger count and whether Super is held.
const SWIPES: [(u32, bool); 4] = [(3, false), (4, false), (3, true), (4, true)];

/// Which Super+button binding a pointer binding is: left moves, right (secondary) resizes.
#[derive(Clone, Copy, Debug)]
pub struct ButtonData {
    seat: SeatKey,
    secondary: bool,
}

/// The binding objects of one seat.
struct Objects {
    drag: RiverPointerBindingV1,
    resize: RiverPointerBindingV1,
    gestures: Option<NewmRiverGesturesSeatV1>,
    bindings: Vec<NewmRiverGestureBindingV1>,
    axes: Vec<NewmRiverAxisBindingV1>,
    xkb: Option<RiverXkbBindingsSeatV1>,
    /// Escape without and with Super, enabled while a drag runs.
    escape: Vec<RiverXkbBindingV1>,
    escape_on: bool,
    /// Super_L and Super_R without modifiers, while `super-tap-overview` is on.
    supers: Vec<RiverXkbBindingV1>,
}

impl Objects {
    fn destroy(self) {
        self.drag.destroy();
        self.resize.destroy();
        for b in self.bindings {
            b.destroy();
        }
        for a in self.axes {
            a.destroy();
        }
        if let Some(g) = self.gestures {
            g.destroy();
        }
        if let Some(x) = self.xkb {
            x.destroy();
        }
        for b in self.escape.into_iter().chain(self.supers) {
            b.destroy();
        }
    }
}

struct SeatInput {
    proxy: RiverSeatV1,
    pointer: Pointer,
    /// The pointer's latest position this sequence: state at its start, handled first.
    position: Option<(i32, i32)>,
    /// The pointer's motion this sequence, accelerated and not (gestures v4), handled right
    /// after the position: it happened before what the sequence reports, so the sequence that
    /// arms a Super pan drops the motion before Super went down.
    motion: Option<((f64, f64), (f64, f64))>,
    events: Vec<In>,
    objects: Option<Objects>,
}

/// Whether a swipe binding holds Super.
#[derive(Clone, Copy, Debug)]
pub struct SwipeData {
    seat: SeatKey,
    with_mod: bool,
}

/// A pinch binding's seat and whether Super is part of it.
#[derive(Clone, Copy, Debug)]
pub struct PinchData {
    seat: SeatKey,
    with_mod: bool,
}

/// An axis binding's orientation.
#[derive(Clone, Debug)]
pub struct AxisData {
    seat: SeatKey,
    horizontal: bool,
}

/// The seat a binding belongs to: the protocol id of its `river_seat_v1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeatKey(u32);

impl SeatKey {
    fn of(seat: &RiverSeatV1) -> Self {
        Self(seat.id().protocol_id())
    }
}

/// Every seat's pointer state and binding objects.
pub struct PointerIo {
    gestures: Option<NewmRiverGesturesV1>,
    /// river reports the pointer's unaccelerated motion (gestures v4), so a pan can take less
    /// than all of the acceleration.
    unaccel: bool,
    template: Pointer,
    seats: HashMap<SeatKey, SeatInput>,
    next_poll: Option<Instant>,
}

/// The config's scroll binds, for axis bindings.
fn axis_binds(c: &Config) -> Vec<AxisBind> {
    c.binds
        .0
        .iter()
        .filter(|b| pointer::is_axis_trigger(b.key.trigger))
        .filter_map(|b| {
            Some(AxisBind {
                trigger: b.key.trigger,
                mods: config::river_modifiers(b.key.modifiers),
                action: b.press_action()?.clone(),
            })
        })
        .collect()
}

/// A seat's pointer as the config sets it up; `pan-acceleration` needs river's unaccelerated
/// motion (gestures v4), without which a pan follows the cursor.
fn template(c: &Config, unaccel: bool) -> Pointer {
    let share = if unaccel {
        c.layout.canvas.pan_acceleration
    } else {
        1.
    };
    Pointer::new(c.layout.canvas.super_tap_overview, axis_binds(c))
        .with_pan_acceleration(share)
        .with_tap_zoom(c.layout.canvas.super_tap_zoom)
}

fn bits(m: WEnum<Modifiers>) -> u32 {
    match m {
        WEnum::Value(m) => m.bits(),
        WEnum::Unknown(u) => u,
    }
}

impl PointerIo {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<State>, c: &Config) -> Self {
        let gestures: Option<NewmRiverGesturesV1> = globals.bind(qh, 1..=4, ()).ok();
        if gestures.is_none() {
            tracing::warn!("newm_river_gestures_v1 missing: no touchpad gestures or Mod+wheel");
        }
        let unaccel = gestures.as_ref().is_some_and(|g| g.version() >= 4);
        if !unaccel && c.layout.canvas.pan_acceleration < 1. {
            tracing::warn!("newm_river_gestures_v1 v4 missing: pan-acceleration has no effect");
        }
        Self {
            template: template(c, unaccel),
            gestures,
            unaccel,
            seats: HashMap::new(),
            next_poll: None,
        }
    }

    /// A reloaded config: the Super tap, the scroll binds and `pan-acceleration`, every seat's
    /// bindings made again in the next manage sequence; the cursor's position is kept.
    pub fn reconfigure(&mut self, c: &Config) {
        self.template = template(c, self.unaccel);
        for s in self.seats.values_mut() {
            if let Some(o) = s.objects.take() {
                o.destroy();
            }
            s.pointer = s.pointer.reconfigured(&self.template);
        }
    }

    pub fn add_seat(&mut self, proxy: &RiverSeatV1) {
        self.seats.insert(
            SeatKey::of(proxy),
            SeatInput {
                proxy: proxy.clone(),
                pointer: self.template.clone(),
                position: None,
                motion: None,
                events: Vec::new(),
                objects: None,
            },
        );
    }

    fn push(&mut self, seat: SeatKey, ev: In) {
        if let Some(s) = self.seats.get_mut(&seat) {
            s.events.push(ev);
        }
    }

    /// `queryWindowInfo` asks for a pick: the first seat's next click makes it.
    pub fn start_pick(&mut self) {
        if let Some(s) = self.seats.values_mut().next() {
            s.events.push(In::Pick);
        }
    }

    /// A key binding was pressed: no Super tap on any seat.
    /// The next non-modifier key reaches no app: the exit confirmation takes it.
    pub fn eat_next_key(&self) {
        for s in self.seats.values() {
            if let Some(x) = s.objects.as_ref().and_then(|o| o.xkb.as_ref()) {
                x.ensure_next_key_eaten();
            }
        }
    }

    pub fn key_pressed(&mut self) {
        for s in self.seats.values_mut() {
            s.events.push(In::Key);
        }
    }

    /// When the pointer's position must next be asked for.
    pub fn deadline(&self) -> Option<Instant> {
        self.next_poll
    }

    /// Whether a manage sequence is due to ask for the pointer's position.
    pub fn tick(&mut self, now: Instant) -> bool {
        match self.next_poll {
            Some(t) if now >= t => {
                self.next_poll = Some(now + POLL);
                true
            }
            _ => false,
        }
    }
}

impl SeatInput {
    fn create(
        &mut self,
        gestures: Option<&NewmRiverGesturesV1>,
        xkb: Option<&RiverXkbBindingsV1>,
        qh: &QueueHandle<State>,
    ) {
        let key = SeatKey::of(&self.proxy);
        let super_mod = Modifiers::from_bits_truncate(pointer::SUPER);
        let button = |secondary| ButtonData {
            seat: key,
            secondary,
        };
        let drag = self
            .proxy
            .get_pointer_binding(BTN_LEFT, super_mod, qh, button(false));
        drag.enable();
        let resize = self
            .proxy
            .get_pointer_binding(BTN_RIGHT, super_mod, qh, button(true));
        resize.enable();
        let gs = gestures.map(|g| g.get_seat(&self.proxy, qh, key));
        let mut bindings = Vec::new();
        let mut axes = Vec::new();
        if let Some(gs) = &gs {
            for (fingers, with_mod) in SWIPES {
                let mods = if with_mod {
                    super_mod
                } else {
                    Modifiers::empty()
                };
                let data = SwipeData {
                    seat: key,
                    with_mod,
                };
                let b = gs.get_gesture_binding(Kind::Swipe, fingers, mods, qh, data);
                b.enable();
                bindings.push(b);
            }
            for (fingers, with_mod) in pointer::PINCHES {
                let mods = if with_mod {
                    super_mod
                } else {
                    Modifiers::empty()
                };
                let data = PinchData {
                    seat: key,
                    with_mod,
                };
                let b = gs.get_gesture_binding(Kind::Pinch, fingers, mods, qh, data);
                b.enable();
                bindings.push(b);
            }
            for spec in self.pointer.axis_bindings() {
                let (horizontal, mods) = (spec.horizontal, spec.mods);
                let axis = if horizontal {
                    wl_pointer::Axis::HorizontalScroll
                } else {
                    wl_pointer::Axis::VerticalScroll
                };
                let data = AxisData {
                    seat: key,
                    horizontal,
                };
                let a = gs.get_axis_binding(axis, Modifiers::from_bits_truncate(mods), qh, data);
                if a.version() >= 2 {
                    // Only the sources some bind uses: Mod+two-finger scrolling reaches the app
                    // unless a TouchpadScroll* bind takes it.
                    let mut sources = newm_river_axis_binding_v1::Sources::empty();
                    if spec.wheel {
                        sources |= newm_river_axis_binding_v1::Sources::Wheel
                            | newm_river_axis_binding_v1::Sources::WheelTilt;
                    }
                    if spec.finger {
                        sources |= newm_river_axis_binding_v1::Sources::Finger
                            | newm_river_axis_binding_v1::Sources::Continuous;
                    }
                    a.set_sources(sources);
                }
                a.enable();
                axes.push(a);
            }
        }
        let xkb_global = xkb;
        let xkb = xkb.filter(|x| x.version() >= 3).map(|x| {
            let s = x.get_seat(&self.proxy, qh, key);
            s.modifiers_watch(Modifiers::from_bits_truncate(
                pointer::SUPER | pointer::SHIFT | pointer::CTRL | pointer::ALT,
            ));
            s
        });
        if xkb.is_none() {
            tracing::warn!("river_xkb_bindings_v1 v3 missing: no Super tap or Super pan");
        }
        let (mut escape, mut supers) = (Vec::new(), Vec::new());
        if let Some(x) = xkb_global {
            for mods in [Modifiers::empty(), super_mod] {
                let data = KeyData {
                    seat: key,
                    escape: true,
                };
                escape.push(x.get_xkb_binding(&self.proxy, XK_ESCAPE, mods, qh, data));
            }
            if self.pointer.taps() && x.version() >= 2 {
                for sym in [XK_SUPER_L, XK_SUPER_R] {
                    let data = KeyData {
                        seat: key,
                        escape: false,
                    };
                    let b = x.get_xkb_binding(&self.proxy, sym, Modifiers::empty(), qh, data);
                    b.enable();
                    supers.push(b);
                }
            }
        }
        self.objects = Some(Objects {
            drag,
            resize,
            gestures: gs,
            bindings,
            axes,
            xkb,
            escape,
            escape_on: false,
            supers,
        });
    }
}

/// Carries out one command of a seat's pointer.
fn run(
    cmd: Cmd,
    desktop: &mut Desktop,
    pending: &mut Vec<(niri_config::Action, Option<Sender<Reply>>)>,
    seat: &RiverSeatV1,
    now: Instant,
) {
    match cmd {
        Cmd::Act(a) => pending.push((a, None)),
        Cmd::OpStart => seat.op_start_pointer(),
        Cmd::Picked(at) => desktop.picked = Some(at),
        Cmd::OpEnd => seat.op_end(),
        Cmd::Pan { at, by } => desktop.pan(at, by, now),
        Cmd::PanEnd => desktop.pan_end(now),
        Cmd::DragBegin(x, y) => {
            desktop.drag_begin(x, y, now);
        }
        Cmd::DragTo(dx, dy) => desktop.drag_to(dx, dy),
        Cmd::Drop => desktop.drop_drag(now),
        Cmd::CancelDrag => {
            desktop.cancel_drag();
        }
        Cmd::ResizeBegin(x, y) => {
            desktop.resize_begin(x, y, now);
        }
        Cmd::ResizeTo(dx, dy) => desktop.resize_to(dx, dy),
        Cmd::ResizeEnd => desktop.resize_end(now),
        Cmd::CancelResize => {
            desktop.cancel_resize();
        }
        Cmd::OverviewBegin => desktop.overview_gesture_begin(),
        Cmd::OverviewMove(d) => desktop.overview_gesture_update(d),
        Cmd::OverviewEnd(v) => desktop.overview_gesture_release(v),
        Cmd::Zoom(steps) => desktop.swipe_zoom(steps, now),
        Cmd::SwipePan { at, by } => desktop.swipe_pan(at, by, now),
        Cmd::SwipePanEnd => desktop.swipe_pan_end(now),
        Cmd::Step(action, d) => desktop.swipe_step(action, d, now),
        Cmd::ZoomToFit => desktop.zoom_to_fit(),
        Cmd::ZoomBack => {
            desktop.zoom_back();
        }
        Cmd::PinchBegin => desktop.pinch_begin(),
        Cmd::PinchScale(scale) => desktop.pinch_scale(scale),
        Cmd::PinchEnd => desktop.pinch_end(),
        Cmd::TapZoom(scale) => desktop.tap_zoom(scale),
    }
}

/// In a manage sequence: binds new seats and plays every seat's queued input on the model.
pub fn manage(state: &mut State, qh: &QueueHandle<State>) {
    let now = Instant::now();
    let xkb = state.xkb_bindings.clone();
    let State {
        desktop,
        pending,
        pointer,
        ..
    } = state;
    let gestures = pointer.gestures.clone();
    for seat in pointer.seats.values_mut() {
        if seat.objects.is_none() {
            seat.create(gestures.as_ref(), xkb.as_ref(), qh);
        }
        let mut events = std::mem::take(&mut seat.events);
        if let Some((accel, unaccel)) = seat.motion.take() {
            events.insert(0, In::Motion { accel, unaccel });
        }
        if let Some((x, y)) = seat.position.take() {
            let edge = desktop.near_layout_edge(x, y, crate::pointer::PAN_THRESHOLD);
            events.splice(0..0, [In::Position(x, y), In::Edge(edge)]);
        }
        seat.pointer.overview = desktop.overview_open();
        for ev in events {
            let step = desktop.touchpad_tile_motion();
            for cmd in seat.pointer.handle(ev, now, step) {
                tracing::debug!("pointer {cmd:?}");
                run(cmd, desktop, pending, &seat.proxy, now);
            }
        }
        let dragging = seat.pointer.escape_bound();
        if let Some(o) = &mut seat.objects
            && o.escape_on != dragging
        {
            o.escape_on = dragging;
            for b in &o.escape {
                if dragging {
                    b.enable();
                } else {
                    b.disable();
                }
            }
        }
    }
    let polling = pointer.seats.values().any(|s| s.pointer.polling());
    pointer.next_poll = polling.then(|| pointer.next_poll.unwrap_or(now + POLL));
}

/// A seat event: what the pointer needs of it. The river layer handles the rest.
pub fn seat_event(state: &mut State, proxy: &RiverSeatV1, event: &river_seat_v1::Event) {
    use river_seat_v1::Event;
    let key = SeatKey::of(proxy);
    let io = &mut state.pointer;
    match event {
        Event::PointerPosition { x, y } => {
            if let Some(s) = io.seats.get_mut(&key) {
                s.position = Some((*x, *y));
            }
        }
        Event::OpDelta { dx, dy } => io.push(key, In::OpDelta(*dx, *dy)),
        Event::OpRelease => io.push(key, In::OpRelease),
        Event::WindowInteraction { .. } => io.push(key, In::Interaction),
        Event::Removed => {
            if let Some(o) = io.seats.remove(&key).and_then(|s| s.objects) {
                o.destroy();
            }
        }
        _ => {}
    }
}

impl Dispatch<RiverPointerBindingV1, ButtonData> for State {
    fn event(
        state: &mut Self,
        _: &RiverPointerBindingV1,
        event: river_pointer_binding_v1::Event,
        data: &ButtonData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_pointer_binding_v1::Event;
        let down = matches!(event, Event::Pressed);
        let ev = if data.secondary {
            In::Secondary(down)
        } else {
            In::Button(down)
        };
        state.pointer.push(data.seat, ev);
    }
}

impl Dispatch<NewmRiverGestureBindingV1, SwipeData> for State {
    fn event(
        state: &mut Self,
        _: &NewmRiverGestureBindingV1,
        event: newm_river_gesture_binding_v1::Event,
        data: &SwipeData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use newm_river_gesture_binding_v1::Event;
        let ev = match event {
            Event::Begin { finger_count } => In::SwipeBegin {
                fingers: finger_count,
                with_mod: data.with_mod,
            },
            Event::Update { dx, dy, .. } => In::SwipeUpdate { dx, dy },
            Event::End { .. } => In::SwipeEnd,
        };
        state.pointer.push(data.seat, ev);
    }
}

impl Dispatch<NewmRiverGestureBindingV1, PinchData> for State {
    fn event(
        state: &mut Self,
        _: &NewmRiverGestureBindingV1,
        event: newm_river_gesture_binding_v1::Event,
        data: &PinchData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use newm_river_gesture_binding_v1::Event;
        let ev = match event {
            Event::Begin { finger_count } => In::PinchBegin {
                fingers: finger_count,
                with_mod: data.with_mod,
            },
            Event::Update { scale, .. } => In::PinchUpdate { scale },
            Event::End { .. } => In::PinchEnd,
        };
        state.pointer.push(data.seat, ev);
    }
}

impl Dispatch<NewmRiverAxisBindingV1, AxisData> for State {
    fn event(
        state: &mut Self,
        _: &NewmRiverAxisBindingV1,
        event: newm_river_axis_binding_v1::Event,
        data: &AxisData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let newm_river_axis_binding_v1::Event::Axis {
            source,
            value,
            value120,
        } = event;
        let wheel = matches!(
            source,
            WEnum::Value(wl_pointer::AxisSource::Wheel | wl_pointer::AxisSource::WheelTilt)
        );
        state.pointer.push(
            data.seat,
            In::Axis {
                horizontal: data.horizontal,
                wheel,
                value,
                value120,
            },
        );
    }
}

impl Dispatch<RiverXkbBindingV1, KeyData> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        data: &KeyData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            river_xkb_binding_v1::Event::Pressed if data.escape => {
                state.pointer.push(data.seat, In::Escape);
            }
            river_xkb_binding_v1::Event::StopRepeat if !data.escape => {
                state.pointer.push(data.seat, In::Key);
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverXkbBindingsSeatV1, SeatKey> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbBindingsSeatV1,
        event: river_xkb_bindings_seat_v1::Event,
        seat: &SeatKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let river_xkb_bindings_seat_v1::Event::AteUnboundKey = event {
            state.ate_unbound_key();
        }
        if let river_xkb_bindings_seat_v1::Event::ModifiersUpdate { old, new } = event {
            if bits(old) & pointer::ALT != 0 && bits(new) & pointer::ALT == 0 {
                state.alt_released();
            }
            state.pointer.push(
                *seat,
                In::Mods {
                    old: bits(old),
                    new: bits(new),
                },
            );
        }
    }
}

wayland_client::delegate_noop!(State: NewmRiverGesturesV1);
impl Dispatch<NewmRiverGesturesSeatV1, SeatKey> for State {
    /// A press on the desktop (gestures v3) activates the output it lands on, as a click on an
    /// empty output does in niri: new windows open there. The pointer's motion (v4) is summed
    /// for the seat's next manage sequence.
    fn event(
        state: &mut Self,
        _: &NewmRiverGesturesSeatV1,
        event: newm_river_gestures_seat_v1::Event,
        seat: &SeatKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use newm_river_gestures_seat_v1::Event;
        match event {
            Event::DesktopPress { x, y } => {
                if state.desktop.press_on_desktop(x, y, Instant::now()) {
                    state.wm.manage_dirty();
                }
            }
            Event::PointerMotion {
                dx,
                dy,
                unaccel_dx,
                unaccel_dy,
            } => {
                if let Some(s) = state.pointer.seats.get_mut(seat) {
                    let ((ax, ay), (ux, uy)) = s.motion.unwrap_or_default();
                    s.motion = Some(((ax + dx, ay + dy), (ux + unaccel_dx, uy + unaccel_dy)));
                }
            }
        }
    }
}
