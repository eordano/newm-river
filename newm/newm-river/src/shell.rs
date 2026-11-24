//! The window manager's boxes drawn as river shell surfaces, each showing the part of it on its
//! output: launch placeholders, one surface each below the windows (see
//! [`crate::placeholder::paint`]), a resize drag's box above them (`model/resize.rs`) and the
//! minimap above that (`model/minimap.rs`).

use std::collections::HashMap;

use newm_canvas::Id;
use niri_config::Config;
use tracing::warn;
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_shm, wl_surface};
use wayland_client::{delegate_noop, QueueHandle};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::draw::Pixmap;
use crate::glow;
use crate::model::minimap::{self, MinimapFrame};
use crate::model::{PlaceholderFrame, Rect, ResizeBoxFrame};
use crate::placeholder::{self, Icon};
use crate::protocols::window_management::river_node_v1::RiverNodeV1;
use crate::protocols::window_management::river_shell_surface_v1::RiverShellSurfaceV1;
use crate::protocols::window_management::river_window_manager_v1::RiverWindowManagerV1;
use crate::river::{shm_buffer, State};

delegate_noop!(State: ignore RiverShellSurfaceV1);

/// A resize box's outline width, in logical pixels at 1:1.
const RESIZE_EDGE: f64 = 2.;
/// How opaque a resize box's fill is, over the border colour's own alpha.
const RESIZE_FILL_ALPHA: f32 = 0.3;
/// A resize box's corner radius, in logical pixels at 1:1.
const RESIZE_RADIUS: f64 = 12.;

/// The box's size and the part of it on its output: x, y, width, height.
type Key = ((i32, i32), (i32, i32, i32, i32));

struct Drawn {
    surface: wl_surface::WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    buffer: Option<wl_buffer::WlBuffer>,
    /// What the buffer shows: the box's size and the part of it on its output.
    key: Option<Key>,
    icon: Option<Icon>,
}

impl Drawn {
    fn new(
        compositor: &wl_compositor::WlCompositor,
        wm: &RiverWindowManagerV1,
        qh: &QueueHandle<State>,
        icon: Option<Icon>,
    ) -> Self {
        let surface = compositor.create_surface(qh, ());
        let shell = wm.get_shell_surface(&surface, qh, ());
        let node = shell.get_node(qh, ());
        Drawn {
            surface,
            shell,
            node,
            buffer: None,
            key: None,
            icon,
        }
    }

    /// Shows the part of `rect` on `area`, the box `paint` gives for its size, unless it shows
    /// that already. Returns where the part goes on screen.
    fn show(
        &mut self,
        rect: Rect,
        area: Rect,
        paint: impl FnOnce(&Self, (i32, i32)) -> Vec<u8>,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
    ) -> Option<Rect> {
        let shown = rect.intersect(area);
        let key = shown.map(|v| ((rect.w, rect.h), (v.x - rect.x, v.y - rect.y, v.w, v.h)));
        if self.key != key {
            self.key = key;
            let old = self.buffer.take();
            self.shell.sync_next_commit();
            match key {
                Some((size, part)) => {
                    let pixels = paint(self, size);
                    let b = glow::Buffer {
                        width: part.2,
                        height: part.3,
                        offset: (0, 0),
                        pixels: placeholder::crop(&pixels, size.0, part),
                    };
                    match shm_buffer(shm, &b, qh) {
                        Ok(buffer) => {
                            self.surface.attach(Some(&buffer), 0, 0);
                            self.surface.damage(0, 0, b.width, b.height);
                            self.buffer = Some(buffer);
                        }
                        Err(e) => {
                            warn!("shell surface buffer: {e}");
                            self.surface.attach(None, 0, 0);
                        }
                    }
                }
                None => self.surface.attach(None, 0, 0),
            }
            self.surface.commit();
            if let Some(old) = old {
                old.destroy();
            }
        }
        if let Some(v) = shown {
            self.node.set_position(v.x, v.y);
        }
        shown
    }

    fn destroy(self) {
        self.node.destroy();
        self.shell.destroy();
        self.surface.destroy();
        if let Some(b) = self.buffer {
            b.destroy();
        }
    }
}

/// The colour of a placeholder's box: the focus ring colour at half strength.
pub fn color(config: &Config) -> [f32; 4] {
    config
        .layout
        .focus_ring
        .active_color
        .to_array_premul()
        .map(|c| c * 0.5)
}

#[derive(Default)]
pub struct Placeholders {
    drawn: HashMap<Id, Drawn>,
}

impl Placeholders {
    /// In a render sequence: draws each placeholder where its frame says and drops the
    /// surfaces of placeholders gone.
    pub fn render(
        &mut self,
        frames: &[PlaceholderFrame],
        color: [f32; 4],
        wm: &RiverWindowManagerV1,
        globals: (Option<&wl_compositor::WlCompositor>, Option<&wl_shm::WlShm>),
        qh: &QueueHandle<State>,
    ) {
        let gone: Vec<Id> = self
            .drawn
            .keys()
            .copied()
            .filter(|id| !frames.iter().any(|f| f.id == *id))
            .collect();
        for id in gone {
            if let Some(d) = self.drawn.remove(&id) {
                d.destroy();
            }
        }
        let (Some(compositor), Some(shm)) = globals else {
            return;
        };
        for f in frames {
            let d = self.drawn.entry(f.id).or_insert_with(|| {
                let icon = f.icon.as_deref().and_then(|path| {
                    placeholder::load_png(path)
                        .inspect_err(|e| warn!("placeholder icon {}: {e}", path.display()))
                        .ok()
                });
                Drawn::new(compositor, wm, qh, icon)
            });
            let radius = placeholder::RADIUS * f.scale;
            let paint = |d: &Drawn, size| placeholder::paint(size, color, radius, d.icon.as_ref());
            d.show(f.rect, f.area, paint, shm, qh);
            d.node.place_bottom();
        }
    }
}

/// The pixels of a resize drag's box over a cell of `size`, its outline of `edge` pixels around
/// the cell: the border's active colour `color` (premultiplied), the fill at
/// [`RESIZE_FILL_ALPHA`] of it.
pub fn paint_resize_box(size: (i32, i32), edge: i32, color: [f32; 4], radius: f64) -> Vec<u8> {
    let (w, h) = (size.0 + 2 * edge, size.1 + 2 * edge);
    let mut pm = Pixmap::new(w, h);
    let e = f64::from(edge);
    let cell = (e, e, f64::from(size.0), f64::from(size.1));
    pm.fill_rounded(cell, radius, color.map(|c| c * RESIZE_FILL_ALPHA));
    let outer = (0., 0., f64::from(w), f64::from(h));
    pm.fill_ring(outer, radius + e, cell, radius, color);
    pm.to_bytes()
}

/// A resize drag's box, above the windows, while the drag would change its window's tiles.
#[derive(Default)]
pub struct ResizeBox {
    drawn: Option<Drawn>,
}

impl ResizeBox {
    /// In a render sequence: draws the box where its frame says, or takes it down.
    pub fn render(
        &mut self,
        frame: Option<ResizeBoxFrame>,
        wm: &RiverWindowManagerV1,
        globals: (Option<&wl_compositor::WlCompositor>, Option<&wl_shm::WlShm>),
        qh: &QueueHandle<State>,
    ) {
        let (Some(f), (Some(compositor), Some(shm))) = (frame, globals) else {
            if let Some(d) = self.drawn.take() {
                d.destroy();
            }
            return;
        };
        let edge = (RESIZE_EDGE * f.scale).round().max(1.) as i32;
        let rect = Rect::new(
            f.rect.x - edge,
            f.rect.y - edge,
            f.rect.w + 2 * edge,
            f.rect.h + 2 * edge,
        );
        let d = self
            .drawn
            .get_or_insert_with(|| Drawn::new(compositor, wm, qh, None));
        let radius = RESIZE_RADIUS * f.scale;
        let paint = |_: &Drawn, (w, h): (i32, i32)| {
            paint_resize_box((w - 2 * edge, h - 2 * edge), edge, f.color, radius)
        };
        d.show(rect, f.area, paint, shm, qh);
        d.node.place_top();
    }
}

/// The minimap's surface: the plan's pixels at its output's scale, shown at the plan's logical
/// size through a viewport.
struct Plan {
    surface: wl_surface::WlSurface,
    shell: RiverShellSurfaceV1,
    node: RiverNodeV1,
    viewport: Option<WpViewport>,
    buffer: Option<wl_buffer::WlBuffer>,
}

impl Plan {
    fn destroy(self) {
        if let Some(v) = self.viewport {
            v.destroy();
        }
        self.node.destroy();
        self.shell.destroy();
        self.surface.destroy();
        if let Some(b) = self.buffer {
            b.destroy();
        }
    }
}

/// The minimap (`model/minimap.rs`), above the windows, while the model shows it.
#[derive(Default)]
pub struct Minimap {
    plan: Option<Plan>,
    /// What the surface shows, and the scale it was painted at.
    shown: Option<(MinimapFrame, f64)>,
}

impl Minimap {
    /// In a render sequence: shows the plan its frame gives, painted again when the plan, the
    /// accent or its output's `scale` changed, or takes it down. Without a viewporter it is
    /// painted at 1:1 and the compositor scales it.
    pub fn render(
        &mut self,
        frame: Option<(MinimapFrame, f64)>,
        wm: &RiverWindowManagerV1,
        globals: (Option<&wl_compositor::WlCompositor>, Option<&wl_shm::WlShm>),
        viewporter: Option<&WpViewporter>,
        qh: &QueueHandle<State>,
    ) {
        let (Some((f, scale)), (Some(compositor), Some(shm))) = (frame, globals) else {
            if let Some(p) = self.plan.take() {
                p.destroy();
            }
            self.shown = None;
            return;
        };
        let p = self.plan.get_or_insert_with(|| {
            let surface = compositor.create_surface(qh, ());
            let shell = wm.get_shell_surface(&surface, qh, ());
            let node = shell.get_node(qh, ());
            // It takes no input: it is only drawn, and a click on it goes to the window under it.
            let nowhere = compositor.create_region(qh, ());
            surface.set_input_region(Some(&nowhere));
            nowhere.destroy();
            let viewport = viewporter.map(|v| v.get_viewport(&surface, qh, ()));
            Plan {
                surface,
                shell,
                node,
                viewport,
                buffer: None,
            }
        });
        let scale = if p.viewport.is_some() && scale > 0. {
            scale
        } else {
            1.
        };
        let painted = (f, scale);
        if self.shown.as_ref() != Some(&painted) {
            let px = minimap::paint(&painted.0.plan, painted.0.accent, scale);
            let b = glow::Buffer {
                width: px.w as i32,
                height: px.h as i32,
                offset: (0, 0),
                pixels: px.bytes(),
            };
            let old = p.buffer.take();
            p.shell.sync_next_commit();
            match shm_buffer(shm, &b, qh) {
                Ok(buffer) => {
                    if let Some(v) = &p.viewport {
                        // The whole buffer, which holds physical pixels, at the plan's logical
                        // size.
                        v.set_destination(minimap::SIZE.0 as i32, minimap::SIZE.1 as i32);
                    }
                    p.surface.attach(Some(&buffer), 0, 0);
                    p.surface.damage_buffer(0, 0, b.width, b.height);
                    p.buffer = Some(buffer);
                }
                Err(e) => {
                    warn!("minimap buffer: {e}");
                    p.surface.attach(None, 0, 0);
                }
            }
            p.surface.commit();
            if let Some(old) = old {
                old.destroy();
            }
        }
        p.node.set_position(painted.0.rect.x, painted.0.rect.y);
        p.node.place_top();
        self.shown = Some(painted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_box_is_outlined_in_the_colour_and_filled_with_a_third_of_it() {
        let color = [0.5, 0.25, 0., 0.5];
        let px = paint_resize_box((40, 30), 2, color, 0.);
        let at = |x: usize, y: usize| {
            let k = (y * 44 + x) * 4;
            u32::from_le_bytes([px[k], px[k + 1], px[k + 2], px[k + 3]])
        };
        assert_eq!(px.len(), 44 * 34 * 4, "the outline lies around the cell");
        assert_eq!(at(0, 17), 0x80804000, "the outline in the colour");
        assert_eq!(at(22, 17), 0x26261300, "the fill at 0.3 of it");
    }
}
