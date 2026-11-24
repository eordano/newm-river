//! The config's `output` blocks, applied as a wlr-output-management client: river leaves output
//! configuration to such clients. The plan is made from
//! the heads river reports at start, when a head appears and when the config changes; it sets
//! what a block names (off, mode or modeline, scale, transform, position, variable refresh rate)
//! and places the outputs a block gives no position in a row to the right, as niri does.

use std::collections::HashMap;

use niri_config::live::{Display, DISPLAYS};
use niri_config::{Config, FloatOrInt, OutputName};
use niri_ipc::{ConfiguredMode, Transform};
use tracing::{debug, info, warn};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::river::State;

/// One mode a head offers (refresh in mHz).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh: i32,
    pub preferred: bool,
}

/// What river reports of a head (a connected output).
#[derive(Clone, Debug, PartialEq)]
pub struct HeadInfo {
    pub name: String,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub physical: (i32, i32),
    pub modes: Vec<ModeInfo>,
    /// Index into `modes`; None when off or on a custom mode.
    pub current: Option<usize>,
    pub enabled: bool,
    pub pos: (i32, i32),
    pub transform: Transform,
    pub scale: f64,
    pub vrr: bool,
}

impl Default for HeadInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            make: None,
            model: None,
            serial: None,
            physical: (0, 0),
            modes: Vec::new(),
            current: None,
            enabled: false,
            pos: (0, 0),
            transform: Transform::Normal,
            scale: 1.,
            vrr: false,
        }
    }
}

impl HeadInfo {
    fn output_name(&self) -> OutputName {
        OutputName {
            connector: self.name.clone(),
            make: self.make.clone(),
            model: self.model.clone(),
            serial: self.serial.clone(),
        }
    }
}

/// The mode a plan sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ModeChoice {
    Keep,
    /// Index into the head's modes.
    Mode(usize),
    /// Width, height, refresh in mHz (0: the compositor picks).
    Custom(i32, i32, i32),
}

/// What a plan does with one head.
#[derive(Clone, Debug, PartialEq)]
pub enum HeadPlan {
    Off,
    On {
        mode: ModeChoice,
        pos: (i32, i32),
        scale: f64,
        transform: Transform,
        /// None: leave adaptive sync as it is.
        vrr: Option<bool>,
    },
}

fn swaps_axes(t: Transform) -> bool {
    matches!(
        t,
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
    )
}

/// Width and height in the logical space, as river computes them (truncated).
fn logical_size(w: i32, h: i32, scale: f64, t: Transform) -> (i32, i32) {
    let (w, h) = if swaps_axes(t) { (h, w) } else { (w, h) };
    let s = if scale > 0. { scale } else { 1. };
    ((f64::from(w) / s) as i32, (f64::from(h) / s) as i32)
}

/// The mode a config `mode` asks of a head: the exact refresh when given (closest within
/// 1 Hz), else the preferred mode at that size, else its highest refresh; a custom mode when
/// none fits and the block allows it.
fn pick_mode(head: &HeadInfo, want: ConfiguredMode, custom: bool) -> Option<ModeChoice> {
    let (w, h) = (i32::from(want.width), i32::from(want.height));
    let sized = || {
        head.modes
            .iter()
            .enumerate()
            .filter(move |(_, m)| m.width == w && m.height == h)
    };
    if custom {
        let refresh = want.refresh.map_or(0, |r| (r * 1000.).round() as i32);
        return Some(ModeChoice::Custom(w, h, refresh));
    }
    let found = match want.refresh {
        Some(r) => {
            let mhz = (r * 1000.).round() as i32;
            sized()
                .map(|(i, m)| (i, (m.refresh - mhz).abs()))
                .filter(|&(_, d)| d <= 1000)
                .min_by_key(|&(_, d)| d)
                .map(|(i, _)| i)
        }
        None => sized()
            .find(|(_, m)| m.preferred)
            .or_else(|| sized().max_by_key(|(_, m)| m.refresh))
            .map(|(i, _)| i),
    };
    found.map(ModeChoice::Mode)
}

/// The size a head has under a mode choice.
fn mode_size(head: &HeadInfo, mode: ModeChoice) -> (i32, i32) {
    let current = |i: Option<usize>| {
        i.and_then(|i| head.modes.get(i))
            .map_or((0, 0), |m| (m.width, m.height))
    };
    match mode {
        ModeChoice::Keep => current(head.current),
        ModeChoice::Mode(i) => current(Some(i)),
        ModeChoice::Custom(w, h, _) => (w, h),
    }
}

fn preferred(head: &HeadInfo) -> Option<usize> {
    head.modes
        .iter()
        .position(|m| m.preferred)
        .or_else(|| (!head.modes.is_empty()).then_some(0))
}

/// What to configure, by head index; None when no block names a head, or every head already is
/// as planned.
/// The config's node for the output `name`, as the live settings name outputs.
fn named<'a>(config: &'a Config, name: &str) -> Option<&'a niri_config::Output> {
    let mut outputs = config.outputs.0.iter();
    outputs.find(|o| o.name.eq_ignore_ascii_case(name))
}

/// The connected displays for the live settings: every head
/// that is on, with the scale it is drawn at. river applies a new scale a moment after it is
/// asked for one, so a head whose node in the applied `config` asks for a scale is listed at
/// that scale already.
pub fn displays(heads: &[HeadInfo], config: &Config) -> Vec<Display> {
    let on = heads.iter().filter(|h| h.enabled);
    on.map(|h| {
        let asked = named(config, &h.name).and_then(|o| o.scale);
        let model = h.model.clone().filter(|m| m != "Unknown");
        Display {
            name: h.name.clone(),
            model: model.unwrap_or_default(),
            scale: asked.map_or(h.scale, |s| s.0),
        }
    })
    .collect()
}

/// `config` asking, for each display whose scale no live override and no node of the file sets,
/// for the scale it had by itself (the live `options`' `config_value`). river keeps an output's
/// scale until it is asked for another, so without this a reset would leave the overridden
/// scale on, where an output should go back to its own scale when its node goes.
pub fn with_own_scales(mut config: Config, options: &[niri_ipc::NewmOption]) -> Config {
    for option in options {
        if option.category != DISPLAYS || option.overridden {
            continue;
        }
        let name = option.key.strip_prefix("output.");
        let Some(name) = name.and_then(|k| k.strip_suffix(".scale")) else {
            continue;
        };
        let Some(own) = option.config_value.as_f64() else {
            continue;
        };
        let outputs = &mut config.outputs.0;
        match outputs
            .iter_mut()
            .find(|o| o.name.eq_ignore_ascii_case(name))
        {
            Some(output) => {
                output.scale.get_or_insert(FloatOrInt(own));
            }
            None => outputs.push(niri_config::Output {
                name: name.to_owned(),
                scale: Some(FloatOrInt(own)),
                ..Default::default()
            }),
        }
    }
    config
}

pub fn plan(heads: &[HeadInfo], config: &Config) -> Option<Vec<HeadPlan>> {
    let blocks: Vec<_> = heads
        .iter()
        .map(|h| config.outputs.find(&h.output_name()))
        .collect();
    if blocks.iter().all(Option::is_none) {
        return None;
    }
    let mut order: Vec<usize> = (0..heads.len()).collect();
    order.sort_by(|&a, &b| heads[a].name.cmp(&heads[b].name));
    let mut plans: Vec<Option<HeadPlan>> = vec![None; heads.len()];
    let mut sizes = vec![(0, 0); heads.len()];
    for &i in &order {
        let head = &heads[i];
        let block = blocks[i];
        if block.is_some_and(|b| b.off) {
            plans[i] = Some(HeadPlan::Off);
            continue;
        }
        if block.is_none() && !head.enabled {
            continue;
        }
        let mut mode = ModeChoice::Keep;
        if let Some(b) = block {
            if let Some(m) = b.mode {
                mode = pick_mode(head, m.mode, m.custom).unwrap_or_else(|| {
                    warn!(
                        "output {}: no mode {}x{}{}; keeping the preferred one",
                        head.name,
                        m.mode.width,
                        m.mode.height,
                        m.mode.refresh.map(|r| format!("@{r}")).unwrap_or_default()
                    );
                    ModeChoice::Keep
                });
            } else if let Some(ml) = b.modeline {
                let total = f64::from(ml.htotal) * f64::from(ml.vtotal);
                let mhz = if total > 0. {
                    (ml.clock * 1e9 / total).round() as i32
                } else {
                    0
                };
                info!(
                    "output {}: the modeline is set as a custom mode {}x{} at {mhz} mHz",
                    head.name, ml.hdisplay, ml.vdisplay
                );
                mode = ModeChoice::Custom(i32::from(ml.hdisplay), i32::from(ml.vdisplay), mhz);
            }
        }
        if mode == ModeChoice::Keep && !head.enabled {
            mode = preferred(head).map_or(ModeChoice::Keep, ModeChoice::Mode);
        }
        let scale = block
            .and_then(|b| b.scale)
            .map(|s| s.0)
            .filter(|&s| s > 0.)
            .unwrap_or(if head.scale > 0. { head.scale } else { 1. });
        let transform = block.map_or(head.transform, |b| b.transform);
        let vrr = block.map(|b| {
            if b.is_vrr_on_demand() {
                info!(
                    "output {}: on-demand variable refresh rate stays off",
                    head.name
                );
            }
            b.is_vrr_always_on()
        });
        let (w, h) = mode_size(head, mode);
        sizes[i] = logical_size(w, h, scale, transform);
        plans[i] = Some(HeadPlan::On {
            mode,
            pos: block
                .and_then(|b| b.position)
                .map_or((0, 0), |p| (p.x, p.y)),
            scale,
            transform,
            vrr,
        });
    }
    // Outputs without a position go in a row right of the rightmost positioned one.
    let positioned = |i: usize| blocks[i].is_some_and(|b| b.position.is_some());
    let mut x = 0;
    for &i in &order {
        if let Some(HeadPlan::On { pos, .. }) = &plans[i]
            && positioned(i)
        {
            x = x.max(pos.0 + sizes[i].0);
        }
    }
    for &i in &order {
        if let Some(HeadPlan::On { pos, .. }) = &mut plans[i]
            && !positioned(i)
        {
            *pos = (x, 0);
            x += sizes[i].0;
        }
    }
    let changed = plans.iter().zip(heads).any(|(p, h)| match p {
        None => false,
        Some(HeadPlan::Off) => h.enabled,
        Some(HeadPlan::On {
            mode,
            pos,
            scale,
            transform,
            vrr,
        }) => {
            !h.enabled
                || match *mode {
                    ModeChoice::Keep => false,
                    ModeChoice::Mode(i) => h.current != Some(i),
                    // A custom mode is not listed: it is set while a listed one is current.
                    ModeChoice::Custom(..) => h.current.is_some(),
                }
                || *pos != h.pos
                || (scale - h.scale).abs() > 1e-3
                || *transform != h.transform
                || vrr.is_some_and(|v| v != h.vrr)
        }
    });
    // A head the plan leaves alone (off and named by no block) stays off.
    changed.then(|| {
        plans
            .into_iter()
            .map(|p| p.unwrap_or(HeadPlan::Off))
            .collect()
    })
}

fn wl_transform(t: Transform) -> wl_output::Transform {
    match t {
        Transform::Normal => wl_output::Transform::Normal,
        Transform::_90 => wl_output::Transform::_90,
        Transform::_180 => wl_output::Transform::_180,
        Transform::_270 => wl_output::Transform::_270,
        Transform::Flipped => wl_output::Transform::Flipped,
        Transform::Flipped90 => wl_output::Transform::Flipped90,
        Transform::Flipped180 => wl_output::Transform::Flipped180,
        Transform::Flipped270 => wl_output::Transform::Flipped270,
    }
}

fn niri_transform(t: wl_output::Transform) -> Transform {
    match t {
        wl_output::Transform::_90 => Transform::_90,
        wl_output::Transform::_180 => Transform::_180,
        wl_output::Transform::_270 => Transform::_270,
        wl_output::Transform::Flipped => Transform::Flipped,
        wl_output::Transform::Flipped90 => Transform::Flipped90,
        wl_output::Transform::Flipped180 => Transform::Flipped180,
        wl_output::Transform::Flipped270 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

struct Head {
    proxy: ZwlrOutputHeadV1,
    info: HeadInfo,
    modes: Vec<ZwlrOutputModeV1>,
    current: Option<ObjectId>,
}

/// The wlr-output-management client.
pub struct OutputConfig {
    manager: Option<ZwlrOutputManagerV1>,
    heads: Vec<Head>,
    modes: HashMap<ObjectId, ModeInfo>,
    serial: Option<u32>,
    /// A plan is due at the next `done`.
    dirty: bool,
    /// A configuration was applied and has no answer yet.
    in_flight: bool,
    /// Heads the configuration in flight turns on while river has them off.
    enabling: Vec<String>,
    /// Heads river refused to turn on (a second GPU that cannot show river's buffers): left out
    /// of the plans until the heads change, so the rest of the config still applies (the whole
    /// configuration used to fail, scale 1.25 included).
    refused: Vec<String>,
}

impl OutputConfig {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<State>) -> Self {
        let manager = globals.bind(qh, 1..=4, ()).ok();
        if manager.is_none() {
            warn!("zwlr_output_manager_v1 missing: the config's outputs are not applied");
        }
        Self {
            manager,
            heads: Vec::new(),
            modes: HashMap::new(),
            serial: None,
            dirty: true,
            in_flight: false,
            enabling: Vec::new(),
            refused: Vec::new(),
        }
    }

    /// The heads as river last reported them.
    pub fn heads(&self) -> Vec<HeadInfo> {
        self.heads.iter().map(|h| self.info(h)).collect()
    }

    fn info(&self, h: &Head) -> HeadInfo {
        let mut info = h.info.clone();
        info.modes = h
            .modes
            .iter()
            .map(|m| self.modes.get(&m.id()).copied().unwrap_or_default())
            .collect();
        info.current = h
            .current
            .as_ref()
            .and_then(|c| h.modes.iter().position(|m| &m.id() == c));
        info
    }

    /// Fills in what the IPC reports of an output from its head.
    pub fn fill_ipc(&self, out: &mut niri_ipc::Output) {
        let Some(head) = self.heads.iter().find(|h| h.info.name == out.name) else {
            return;
        };
        let info = self.info(head);
        out.make = info.make.clone().unwrap_or_default();
        out.model = info.model.clone().unwrap_or_default();
        out.serial = info.serial.clone();
        let (pw, ph) = info.physical;
        out.physical_size = (pw > 0).then_some((pw as u32, ph.max(0) as u32));
        out.modes = info
            .modes
            .iter()
            .map(|m| niri_ipc::Mode {
                width: m.width.clamp(0, i32::from(u16::MAX)) as u16,
                height: m.height.clamp(0, i32::from(u16::MAX)) as u16,
                refresh_rate: m.refresh.max(0) as u32,
                is_preferred: m.preferred,
            })
            .collect();
        out.current_mode = info.current;
        out.is_custom_mode = info.enabled && info.current.is_none();
        out.vrr_enabled = info.vrr;
        if let Some(l) = &mut out.logical {
            l.scale = info.scale;
            l.transform = info.transform;
        }
    }

    /// The config was reloaded: plan the heads against it again, at once when river has already
    /// described them, else at its next `done`.
    pub fn reconfigure(&mut self, qh: &QueueHandle<State>, config: &Config) {
        self.dirty = true;
        if let Some(serial) = self.serial {
            self.done(serial, qh, config);
        }
    }

    fn done(&mut self, serial: u32, qh: &QueueHandle<State>, config: &Config) {
        self.serial = Some(serial);
        if !self.dirty || self.in_flight {
            return;
        }
        self.dirty = false;
        let heads = self.heads();
        let Some(plan) = plan(&heads, config) else {
            debug!("outputs: nothing to configure");
            return;
        };
        let Some(manager) = &self.manager else {
            return;
        };
        let cfg = manager.create_configuration(serial, qh, ());
        self.enabling.clear();
        for ((head, info), p) in self.heads.iter().zip(&heads).zip(&plan) {
            if self.refused.contains(&info.name) {
                // wlr-output-management leaves a head the configuration does not name as it is.
                continue;
            }
            if matches!(p, HeadPlan::On { .. }) && !info.enabled {
                self.enabling.push(info.name.clone());
            }
            match p {
                HeadPlan::Off if info.enabled => cfg.disable_head(&head.proxy),
                HeadPlan::Off => {}
                HeadPlan::On {
                    mode,
                    pos,
                    scale,
                    transform,
                    vrr,
                } => {
                    let ch = cfg.enable_head(&head.proxy, qh, ());
                    // enable_head starts from the head's current state, so Keep sets nothing.
                    match *mode {
                        ModeChoice::Keep => {}
                        ModeChoice::Mode(i) => ch.set_mode(&head.modes[i]),
                        ModeChoice::Custom(w, h, r) => ch.set_custom_mode(w, h, r),
                    }
                    ch.set_position(pos.0, pos.1);
                    ch.set_scale(*scale);
                    ch.set_transform(wl_transform(*transform));
                    if let Some(v) = vrr
                        && ch.version() >= 4
                    {
                        ch.set_adaptive_sync(if *v {
                            zwlr_output_head_v1::AdaptiveSyncState::Enabled
                        } else {
                            zwlr_output_head_v1::AdaptiveSyncState::Disabled
                        });
                    }
                    info!(
                        "output {}: mode {:?} at {:?}, scale {scale}, {transform:?}",
                        info.name, mode, pos
                    );
                }
            }
        }
        cfg.apply();
        self.in_flight = true;
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_output_manager_v1::Event;
        match event {
            Event::Head { head } => {
                let oc = &mut state.output_config;
                oc.heads.push(Head {
                    proxy: head,
                    info: HeadInfo::default(),
                    modes: Vec::new(),
                    current: None,
                });
                oc.dirty = true;
                // New heads (a hotplug, or every head again after a VT switch back): river may
                // drive a refused one now.
                oc.refused.clear();
            }
            Event::Done { serial } => {
                state.output_config.done(serial, qh, &state.config);
                state.displays_changed();
            }
            Event::Finished => state.output_config.manager = None,
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_head_v1::Event;
        let oc = &mut state.output_config;
        let Some(k) = oc.heads.iter().position(|h| &h.proxy == proxy) else {
            return;
        };
        let head = &mut oc.heads[k];
        match event {
            Event::Name { name } => head.info.name = name,
            Event::Make { make } => head.info.make = Some(make),
            Event::Model { model } => head.info.model = Some(model),
            Event::SerialNumber { serial_number } => head.info.serial = Some(serial_number),
            Event::PhysicalSize { width, height } => head.info.physical = (width, height),
            Event::Mode { mode } => {
                oc.modes.insert(mode.id(), ModeInfo::default());
                head.modes.push(mode);
            }
            Event::Enabled { enabled } => {
                head.info.enabled = enabled != 0;
                if enabled == 0 {
                    head.current = None;
                }
            }
            Event::CurrentMode { mode } => head.current = Some(mode.id()),
            Event::Position { x, y } => head.info.pos = (x, y),
            Event::Transform {
                transform: WEnum::Value(t),
            } => head.info.transform = niri_transform(t),
            Event::Scale { scale } => head.info.scale = scale,
            Event::AdaptiveSync {
                state: WEnum::Value(s),
            } => head.info.vrr = s == zwlr_output_head_v1::AdaptiveSyncState::Enabled,
            Event::Finished => {
                let head = oc.heads.remove(k);
                for m in &head.modes {
                    oc.modes.remove(&m.id());
                    if m.version() >= 3 {
                        m.release();
                    }
                }
                if head.proxy.version() >= 3 {
                    head.proxy.release();
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_mode_v1::Event;
        let oc = &mut state.output_config;
        let key = proxy.id();
        match event {
            Event::Finished => {
                oc.modes.remove(&key);
                for h in &mut oc.heads {
                    h.modes.retain(|m| m.id() != key);
                    if h.current.as_ref() == Some(&key) {
                        h.current = None;
                    }
                }
                if proxy.version() >= 3 {
                    proxy.release();
                }
            }
            event => {
                let Some(m) = oc.modes.get_mut(&key) else {
                    return;
                };
                match event {
                    Event::Size { width, height } => (m.width, m.height) = (width, height),
                    Event::Refresh { refresh } => m.refresh = refresh,
                    Event::Preferred => m.preferred = true,
                    _ => {}
                }
            }
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_output_configuration_v1::Event;
        let oc = &mut state.output_config;
        oc.in_flight = false;
        let mut retry = false;
        match event {
            Event::Succeeded => info!("outputs: the config's outputs are applied"),
            Event::Failed if !oc.enabling.is_empty() => {
                warn!(
                    "outputs: river refused the config's outputs; applying them again without \
                     turning on {}",
                    oc.enabling.join(", ")
                );
                oc.refused.append(&mut oc.enabling);
                oc.dirty = true;
                retry = true;
            }
            Event::Failed => warn!("outputs: river refused the config's outputs"),
            Event::Cancelled => {
                debug!("outputs: the configuration was outdated; planning again");
                oc.dirty = true;
            }
            _ => {}
        }
        oc.enabling.clear();
        proxy.destroy();
        if retry && let Some(serial) = state.output_config.serial {
            state.output_config.done(serial, qh, &state.config);
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: zwlr_output_configuration_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use niri_config::live::LiveOptions;
    use serde_json::Value;

    use super::*;

    /// A display's scale is listed, set and reset live, as far as river's side goes:
    /// the config asked of river after each request, and what the settings list while river
    /// has not applied it yet.
    #[test]
    fn a_displays_scale_is_listed_set_and_reset_live() {
        let glasses = |scale: f64| HeadInfo {
            name: "DP-2".into(),
            model: Some("Air 2 Pro".into()),
            enabled: true,
            scale,
            ..HeadInfo::default()
        };
        let off = HeadInfo {
            name: "DP-3".into(),
            model: Some("Unknown".into()),
            scale: 1.,
            ..HeadInfo::default()
        };
        let key = "output.DP-2.scale";
        let listed = |live: &LiveOptions| {
            let options = live.options();
            let found = options.into_iter().find(|o| o.key == key);
            found.expect("the display's scale is listed")
        };
        let asked = |config: &Config| named(config, "DP-2").and_then(|o| o.scale).map(|s| s.0);

        let mut live = LiveOptions::new(Config::default(), &[]);
        let mut config = Config::default();
        let shown = displays(&[glasses(2.25), off.clone()], &config);
        assert_eq!(shown.len(), 1, "a head that is off is no display");
        live.set_displays(shown);
        let own = listed(&live);
        assert_eq!(own.category, DISPLAYS);
        assert_eq!(own.label, "Scale of Air 2 Pro (DP-2)");
        assert_eq!(own.value, Value::from(2.25));
        assert!(!own.overridden);
        config = with_own_scales(live.effective(), &live.options());
        assert_eq!(
            asked(&config),
            Some(2.25),
            "its own scale, which it has already"
        );

        live.set(key, Value::from(1.5)).unwrap();
        config = with_own_scales(live.effective(), &live.options());
        assert_eq!(
            asked(&config),
            Some(1.5),
            "river is asked for the new scale"
        );
        // river has not applied it yet: the head still reports 2.25.
        live.set_displays(displays(&[glasses(2.25)], &config));
        let set = listed(&live);
        assert_eq!(set.value, Value::from(1.5));
        assert_eq!(set.config_value, Value::from(2.25), "what it had by itself");
        assert!(set.overridden);
        assert!(live.set(key, Value::from(9)).is_err());
        assert!(live.set("output.nowhere.scale", Value::from(1)).is_err());

        live.set_displays(displays(&[glasses(1.5)], &config));
        live.reset(key).unwrap();
        config = with_own_scales(live.effective(), &live.options());
        assert_eq!(
            asked(&config),
            Some(2.25),
            "a reset asks river for the display's own scale back"
        );
        // Until river has applied that, the head reports 1.5: its own scale stays what it was.
        live.set_displays(displays(&[glasses(1.5)], &config));
        let reset = listed(&live);
        assert_eq!(reset.value, Value::from(2.25));
        assert_eq!(reset.config_value, Value::from(2.25));
        assert!(!reset.overridden);

        let file = Config::parse_mem("output \"DP-2\" { scale 2; }").unwrap();
        let live = {
            let mut live = LiveOptions::new(file, &[]);
            live.set_displays(displays(&[glasses(2.)], &live.effective()));
            live
        };
        let config = with_own_scales(live.effective(), &live.options());
        assert_eq!(
            asked(&config),
            Some(2.),
            "the file's scale stays the file's"
        );
        let nameless = displays(&[off], &config);
        assert!(nameless.is_empty());
    }

    fn cfg(kdl: &str) -> Config {
        Config::parse_mem(kdl).unwrap()
    }

    fn mode(width: i32, height: i32, refresh: i32, preferred: bool) -> ModeInfo {
        ModeInfo {
            width,
            height,
            refresh,
            preferred,
        }
    }

    fn head(name: &str, pos: (i32, i32)) -> HeadInfo {
        HeadInfo {
            name: name.into(),
            make: Some("Dell Inc.".into()),
            model: Some("U3415W".into()),
            modes: vec![
                mode(3440, 1440, 59973, true),
                mode(3440, 1440, 29990, false),
                mode(2560, 1080, 60000, false),
                mode(2560, 1080, 75000, false),
            ],
            current: Some(0),
            enabled: true,
            pos,
            scale: 1.,
            ..HeadInfo::default()
        }
    }

    #[test]
    fn no_output_block_leaves_the_outputs_alone() {
        let heads = [head("DP-1", (0, 0)), head("HDMI-A-1", (3440, 0))];
        assert_eq!(plan(&heads, &cfg("")), None);
        assert_eq!(plan(&heads, &cfg("output \"DP-9\" { scale 2; }")), None);
    }

    #[test]
    fn a_scale_moves_the_unpositioned_outputs_beside_it() {
        let heads = [head("DP-1", (0, 0)), head("HDMI-A-1", (3440, 0))];
        let p = plan(&heads, &cfg("output \"DP-1\" { scale 1.25; }")).unwrap();
        let on = |scale: f64, x: i32, vrr: Option<bool>| HeadPlan::On {
            mode: ModeChoice::Keep,
            pos: (x, 0),
            scale,
            transform: Transform::Normal,
            vrr,
        };
        assert_eq!(p, vec![on(1.25, 0, Some(false)), on(1., 2752, None)]);
    }

    #[test]
    fn a_config_already_applied_plans_nothing() {
        let mut dp = head("DP-1", (0, 0));
        dp.scale = 1.25;
        let heads = [dp, head("HDMI-A-1", (2752, 0))];
        assert_eq!(plan(&heads, &cfg("output \"DP-1\" { scale 1.25; }")), None);
    }

    #[test]
    fn modes_follow_niri() {
        let h = head("DP-1", (0, 0));
        let pick = |w, ht, r: Option<f64>, custom| {
            pick_mode(
                &h,
                ConfiguredMode {
                    width: w,
                    height: ht,
                    refresh: r,
                },
                custom,
            )
        };
        assert_eq!(
            pick(2560, 1080, Some(75.), false),
            Some(ModeChoice::Mode(3))
        );
        assert_eq!(
            pick(3440, 1440, Some(30.), false),
            Some(ModeChoice::Mode(1))
        );
        assert_eq!(pick(2560, 1080, None, false), Some(ModeChoice::Mode(3)));
        assert_eq!(pick(3440, 1440, None, false), Some(ModeChoice::Mode(0)));
        assert_eq!(pick(1920, 1080, None, false), None);
        assert_eq!(
            pick(1920, 1080, Some(60.), true),
            Some(ModeChoice::Custom(1920, 1080, 60000))
        );
    }

    #[test]
    fn off_position_transform_and_vrr_are_planned() {
        let heads = [head("DP-1", (0, 0)), head("HDMI-A-1", (3440, 0))];
        let c = cfg(
            "output \"DP-1\" { mode \"2560x1080@75\"; transform \"90\"; position x=100 y=50; \
             variable-refresh-rate; }\noutput \"HDMI-A-1\" { off; }",
        );
        let p = plan(&heads, &c).unwrap();
        assert_eq!(
            p[0],
            HeadPlan::On {
                mode: ModeChoice::Mode(3),
                pos: (100, 50),
                scale: 1.,
                transform: Transform::_90,
                vrr: Some(true),
            }
        );
        assert_eq!(p[1], HeadPlan::Off);
    }

    #[test]
    fn outputs_match_by_make_and_model() {
        let heads = [head("DP-1", (0, 0))];
        let p = plan(
            &heads,
            &cfg("output \"Dell Inc. U3415W Unknown\" { scale 2; }"),
        )
        .unwrap();
        assert!(matches!(p[0], HeadPlan::On { scale, .. } if scale == 2.));
    }

    #[test]
    fn rotated_and_scaled_sizes_are_logical() {
        assert_eq!(
            logical_size(2560, 1080, 1.25, Transform::Normal),
            (2048, 864)
        );
        assert_eq!(logical_size(2560, 1080, 2., Transform::_90), (540, 1280));
    }
}
