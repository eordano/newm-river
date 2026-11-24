//! Rendering state river's own protocol lacks, from the fork's `newm-river-render-v1`: a window
//! drawn scaled (the canvas camera) and at an opacity (unfocused windows at 90%); since version 2
//! (a river built with scenefx) rounded corners, a shadow drawn by river in place of the software
//! glow, and [`Snapshot`]s of closing windows. Without the global (an upstream river) the model's
//! values are kept and logged once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use newm_canvas::Id;
use tracing::info;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use crate::model::GlowStyle;
use crate::protocols::newm_render::newm_river_render_v1::NewmRiverRenderV1;
use crate::protocols::newm_render::newm_river_snapshot_v1::{self, NewmRiverSnapshotV1};
use crate::protocols::newm_render::newm_river_window_render_v1::NewmRiverWindowRenderV1;
use crate::protocols::window_management::river_window_v1::RiverWindowV1;
use crate::river::State;

struct WindowRender {
    proxy: NewmRiverWindowRenderV1,
    scale: f64,
    opacity: f32,
    effects: Effects,
}

/// A window's corners and shadow, drawn by river (render protocol version 2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Effects {
    /// Logical pixels; 0 for square corners.
    pub corner_radius: u32,
    pub shadow: Option<Shadow>,
}

/// A shadow below a window, in logical pixels (see `set_shadow` in the protocol).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shadow {
    /// 32-bit RGBA channels, premultiplied (`set_borders`' convention).
    pub color: [u32; 4],
    pub softness: u32,
    pub spread: i32,
    pub offset: (i32, i32),
}

impl Shadow {
    /// The config's `shadow` (the glow's style) as river draws it.
    pub fn from_glow(g: &GlowStyle) -> Self {
        let channel = |c: f32| (f64::from(c.clamp(0., 1.)) * f64::from(u32::MAX)).round() as u32;
        Self {
            color: g.color.map(channel),
            softness: g.softness.max(0.).round() as u32,
            spread: g.spread.round() as i32,
            offset: (g.offset.0.round() as i32, g.offset.1.round() as i32),
        }
    }
}

/// A copy of a closed window's last frame that river keeps drawing (render protocol version 2):
/// the close animation scales, moves and fades it, then drops it. Dropping it removes the copy
/// at once; river removes it by itself after the lifetime given to [`RenderExt::snapshot`].
///
/// The setters are rendering state: call them in a manage or render sequence (river applies them
/// with the next `render_finish`). Its origin starts at the window's top-left (its
/// `river_node_v1` position) and it is drawn at the size the window was drawn at, which already
/// includes the window's camera scale.
pub struct Snapshot {
    proxy: NewmRiverSnapshotV1,
    expired: Arc<AtomicBool>,
}

impl Snapshot {
    /// Draws the copy's origin at `(x, y)`, in `river_node_v1.set_position` coordinates.
    pub fn set_position(&self, x: i32, y: i32) {
        self.proxy.set_position(x, y);
    }

    /// Draws the copy scaled about its origin by `scale` (> 0) of the size it was copied at.
    pub fn set_scale(&self, scale: f64) {
        if scale > 0. {
            self.proxy.set_scale(scale);
        }
    }

    /// Multiplies the copy's alpha by `opacity` (clamped to [0, 1]).
    pub fn set_opacity(&self, opacity: f64) {
        self.proxy.set_opacity(opacity.clamp(0., 1.));
    }

    /// River removed the copy (its lifetime passed): setters do nothing any more.
    pub fn expired(&self) -> bool {
        self.expired.load(Ordering::Relaxed)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

/// User data of a `newm_river_snapshot_v1`: set when river sends `expired`.
pub struct SnapshotData(Arc<AtomicBool>);

impl Dispatch<NewmRiverSnapshotV1, SnapshotData> for State {
    fn event(
        _: &mut Self,
        _: &NewmRiverSnapshotV1,
        event: newm_river_snapshot_v1::Event,
        data: &SnapshotData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let newm_river_snapshot_v1::Event::Expired = event;
        data.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct RenderExt {
    global: Option<NewmRiverRenderV1>,
    windows: HashMap<Id, WindowRender>,
    said: bool,
}

impl RenderExt {
    pub fn new(global: Option<NewmRiverRenderV1>) -> Self {
        if global.is_none() {
            info!("newm_river_render_v1 missing: no camera scale, no window opacity");
        }
        Self {
            global,
            ..Self::default()
        }
    }

    /// Draws window `id` at `scale` and `opacity` (rendering state: call in a manage or render
    /// sequence).
    pub fn window(
        &mut self,
        id: Id,
        window: &RiverWindowV1,
        scale: f64,
        opacity: f32,
        qh: &QueueHandle<State>,
    ) {
        let Some(global) = &self.global else {
            if (scale - 1.).abs() > 1e-9 && !self.said {
                self.said = true;
                info!("window {id}: drawn at 1:1, river lacks newm-river-render-v1");
            }
            return;
        };
        let r = self.windows.entry(id).or_insert_with(|| WindowRender {
            proxy: global.get_window_render(window, qh, ()),
            scale: 1.,
            opacity: 1.,
            effects: Effects::default(),
        });
        if (r.scale - scale).abs() > 1e-9 {
            r.scale = scale;
            r.proxy.set_scale(scale);
        }
        if (r.opacity - opacity).abs() > 1e-6 {
            r.opacity = opacity;
            r.proxy.set_opacity(f64::from(opacity.clamp(0., 1.)));
        }
    }

    /// Whether river draws corners and shadows ([`RenderExt::effects`]) and takes snapshots:
    /// render protocol version 2 (a river built with scenefx).
    pub fn supports_effects(&self) -> bool {
        self.global.as_ref().is_some_and(|g| g.version() >= 2)
    }

    /// Draws window `id` with `effects` (rendering state: call in a manage or render sequence,
    /// after [`RenderExt::window`] for the same window). Without version 2 it does nothing.
    pub fn effects(&mut self, id: Id, effects: Effects) {
        if !self.supports_effects() {
            return;
        }
        let Some(r) = self.windows.get_mut(&id) else {
            return;
        };
        if r.effects.corner_radius != effects.corner_radius {
            r.proxy.set_corner_radius(effects.corner_radius);
        }
        if r.effects.shadow != effects.shadow {
            match effects.shadow {
                Some(s) => r.proxy.set_shadow(
                    s.color[0], s.color[1], s.color[2], s.color[3], s.softness, s.spread,
                    s.offset.0, s.offset.1,
                ),
                None => r.proxy.clear_shadow(),
            }
        }
        r.effects = effects;
    }

    /// Keeps window `id`'s current frame drawn after it closes, for at most `lifetime` (river
    /// caps it at 10 s). Call it when `river_window_v1.closed` arrives, before
    /// [`RenderExt::remove`] and before destroying the `river_window_v1`. None without render
    /// protocol version 2 or when the window was never drawn through [`RenderExt::window`].
    pub fn snapshot(
        &mut self,
        id: Id,
        lifetime: Duration,
        qh: &QueueHandle<State>,
    ) -> Option<Snapshot> {
        if !self.supports_effects() {
            return None;
        }
        let r = self.windows.get(&id)?;
        let expired = Arc::new(AtomicBool::new(false));
        let ms = u32::try_from(lifetime.as_millis())
            .unwrap_or(u32::MAX)
            .max(1);
        let proxy = r.proxy.snapshot(ms, qh, SnapshotData(expired.clone()));
        Some(Snapshot { proxy, expired })
    }

    /// The window closed.
    pub fn remove(&mut self, id: Id) {
        if let Some(r) = self.windows.remove(&id) {
            r.proxy.destroy();
        }
    }
}
