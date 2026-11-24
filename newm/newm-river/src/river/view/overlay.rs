//! The textless overlays: the exit confirmation (a dark 320x112 box framed 8 px in red over a
//! 40% backdrop; Enter quits, any other key cancels and reaches no app) and the config error bar
//! (a red 512x24 bar 16 px below the top for 4 s). Each is
//! a few solid rectangles: single-pixel buffers stretched by a viewport, above everything.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_buffer;
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::{State, Surface};
use crate::model::{Effect, Rect};
use crate::protocols::window_management::river_seat_v1::Modifiers;
use crate::protocols::xkb_bindings::river_xkb_binding_v1::{self, RiverXkbBindingV1};

const XK_RETURN: u32 = 0xff0d;
const XK_KP_ENTER: u32 = 0xff8d;

const DIALOG: (i32, i32) = (320, 112);
const BORDER: i32 = 8;
const BAR: (i32, i32) = (512, 24);
const BAR_TOP: i32 = 16;
pub(in crate::river) const ERROR_SHOWN: Duration = Duration::from_secs(4);

const BACKDROP: [f64; 4] = [0., 0., 0., 0.4];
const RED: [f64; 4] = [1., 0.3, 0.3, 1.];
const DARK: [f64; 4] = [0.1, 0.1, 0.1, 1.];

/// A key the exit confirmation took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::river) enum DialogKey {
    Confirm,
    Cancel,
}

#[derive(Default)]
pub(in crate::river) struct Overlay {
    /// The exit confirmation shows.
    pub exit: bool,
    /// The config error bar shows until then.
    pub error_until: Option<Instant>,
    pub keys: Vec<DialogKey>,
    bindings: HashMap<ObjectId, Vec<RiverXkbBindingV1>>,
    bindings_on: bool,
    /// The next key is to be eaten for the open dialog (asked in the next manage sequence).
    pub eat: bool,
    rects: Vec<Surface>,
    colours: HashMap<[u32; 4], wl_buffer::WlBuffer>,
}

fn centred(area: Rect, size: (i32, i32), top: Option<i32>) -> Rect {
    let x = area.x + (area.w - size.0) / 2;
    let y = top.map_or(area.y + (area.h - size.1) / 2, |t| area.y + t);
    Rect::new(x, y, size.0, size.1)
}

/// What the overlays draw, bottom to top: each rectangle and its colour (straight RGBA).
/// `outputs` are the output areas, `active` the one the dialog and the bar go on.
pub fn rects(
    outputs: &[Rect],
    active: Option<Rect>,
    exit: bool,
    error: bool,
) -> Vec<(Rect, [f64; 4])> {
    let mut out = Vec::new();
    if exit {
        out.extend(outputs.iter().map(|&a| (a, BACKDROP)));
        if let Some(a) = active {
            let frame = centred(a, DIALOG, None);
            let inner = Rect::new(
                frame.x + BORDER,
                frame.y + BORDER,
                frame.w - 2 * BORDER,
                frame.h - 2 * BORDER,
            );
            out.push((frame, RED));
            out.push((inner, DARK));
        }
    }
    if error && let Some(a) = active {
        out.push((centred(a, BAR, Some(BAR_TOP)), RED));
    }
    out
}

fn premultiplied(c: [f64; 4]) -> [u32; 4] {
    let a = c[3].clamp(0., 1.);
    let ch = |v: f64| (v.clamp(0., 1.) * a * f64::from(u32::MAX)) as u32;
    [
        ch(c[0]),
        ch(c[1]),
        ch(c[2]),
        (a * f64::from(u32::MAX)) as u32,
    ]
}

impl State {
    /// `quit` without `skip-confirmation`: the exit confirmation opens.
    pub(in crate::river) fn confirm_quit(&mut self) {
        let o = &mut self.view.overlay;
        if !o.exit {
            o.exit = true;
            o.eat = true;
        }
    }

    /// The config did not parse: the error bar shows.
    pub(in crate::river) fn show_config_error(&mut self, now: Instant) {
        self.view.overlay.error_until = Some(now + ERROR_SHOWN);
    }

    /// The overlays' part of a manage sequence: Enter bound while the dialog shows, the next
    /// key eaten, and the keys it took acted on.
    pub(in crate::river) fn overlay_manage(&mut self, qh: &QueueHandle<State>) {
        for key in std::mem::take(&mut self.view.overlay.keys) {
            if !self.view.overlay.exit {
                continue;
            }
            self.view.overlay.exit = false;
            if key == DialogKey::Confirm {
                self.effect(Effect::Quit);
            }
        }
        let gone: Vec<ObjectId> = self
            .view
            .overlay
            .bindings
            .keys()
            .filter(|k| !self.seats.contains_key(k))
            .cloned()
            .collect();
        for k in gone {
            for b in self.view.overlay.bindings.remove(&k).unwrap_or_default() {
                b.destroy();
            }
        }
        let on = self.view.overlay.exit;
        if on && let Some(xkb) = &self.xkb_bindings {
            for (key, seat) in &self.seats {
                self.view
                    .overlay
                    .bindings
                    .entry(key.clone())
                    .or_insert_with(|| {
                        [XK_RETURN, XK_KP_ENTER]
                            .map(|sym| {
                                let k = DialogKey::Confirm;
                                xkb.get_xkb_binding(&seat.proxy, sym, Modifiers::empty(), qh, k)
                            })
                            .to_vec()
                    });
            }
        }
        if self.view.overlay.bindings_on != on {
            self.view.overlay.bindings_on = on;
            for b in self.view.overlay.bindings.values().flatten() {
                if on {
                    b.enable();
                } else {
                    b.disable();
                }
            }
        }
        if std::mem::take(&mut self.view.overlay.eat) {
            self.pointer.eat_next_key();
        }
    }

    /// Draws the overlays on top of everything, in a render sequence.
    pub(in crate::river) fn render_overlays(&mut self, qh: &QueueHandle<State>, now: Instant) {
        let o = &mut self.view.overlay;
        if o.error_until.is_some_and(|t| now >= t) {
            o.error_until = None;
        }
        let areas: Vec<Rect> = self.desktop.outputs().iter().map(|o| o.area).collect();
        let active = self
            .desktop
            .active_output()
            .and_then(|id| self.desktop.output(id))
            .map(|o| o.area)
            .or_else(|| areas.first().copied());
        let want = rects(&areas, active, o.exit, o.error_until.is_some());
        if !want.is_empty() && self.view.pixels.is_some() && self.view.viewporter.is_some() {
            while self.view.overlay.rects.len() < want.len() {
                let Some(s) = self.new_surface(qh, true) else {
                    break;
                };
                self.view.overlay.rects.push(s);
            }
        }
        let pixels = self.view.pixels.clone();
        let o = &mut self.view.overlay;
        for (k, s) in o.rects.iter_mut().enumerate() {
            let (Some(&(r, colour)), Some(pixels)) = (want.get(k), &pixels) else {
                s.unmap();
                continue;
            };
            let c = premultiplied(colour);
            let buffer = o
                .colours
                .entry(c)
                .or_insert_with(|| pixels.create_u32_rgba_buffer(c[0], c[1], c[2], c[3], qh, ()))
                .clone();
            s.node.set_position(r.x, r.y);
            s.node.place_top();
            if let Some(v) = &s.viewport {
                v.set_destination(r.w.max(1), r.h.max(1));
            }
            s.shell.sync_next_commit();
            s.wl.attach(Some(&buffer), 0, 0);
            s.wl.damage_buffer(0, 0, 1, 1);
            s.wl.commit();
            s.mapped = true;
            s.size = (r.w, r.h);
        }
    }

    /// A key the open exit confirmation asked to eat was not Enter: it cancels.
    pub(crate) fn ate_unbound_key(&mut self) {
        self.view.overlay.keys.push(DialogKey::Cancel);
    }

    /// When the error bar goes.
    pub(in crate::river) fn overlay_deadline(&self) -> Option<Instant> {
        self.view.overlay.error_until
    }
}

impl Dispatch<RiverXkbBindingV1, DialogKey> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        key: &DialogKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let river_xkb_binding_v1::Event::Pressed = event {
            state.view.overlay.keys.push(*key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exit_dialog_is_a_framed_box_over_a_backdrop_on_every_output() {
        let a = Rect::new(0, 0, 1280, 720);
        let b = Rect::new(1280, 0, 1920, 1080);
        let r = rects(&[a, b], Some(a), true, false);
        assert_eq!(r[0], (a, BACKDROP));
        assert_eq!(r[1], (b, BACKDROP));
        assert_eq!(r[2], (Rect::new(480, 304, 320, 112), RED));
        assert_eq!(r[3], (Rect::new(488, 312, 304, 96), DARK));
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn the_error_bar_is_red_near_the_top_of_the_active_output_only() {
        let a = Rect::new(0, 0, 1280, 720);
        let b = Rect::new(1280, 0, 1920, 1080);
        assert_eq!(
            rects(&[a, b], Some(b), false, true),
            vec![(Rect::new(1280 + 704, 16, 512, 24), RED)]
        );
        assert!(rects(&[a], Some(a), false, false).is_empty());
    }

    #[test]
    fn colours_are_premultiplied() {
        let c = premultiplied(BACKDROP);
        assert_eq!(c[0], 0);
        assert_eq!(c[3], (0.4 * f64::from(u32::MAX)) as u32);
        assert_eq!(premultiplied(RED)[3], u32::MAX);
    }
}
