//! The window manager's side of the KDE services (`crate::kde`): the keys KDE apps hold become
//! river xkb bindings whose presses go out as kglobalaccel signals, D-Bus inhibitions become an
//! idle inhibitor on a shell surface of ours, and KRunner gets the windows and focuses the one
//! it runs.

use newm_kde::kglobalaccel as reg;
use newm_kde::{krunner, qtkey};
use niri_config::{Action, Config, Modifiers, Trigger};
use tracing::warn;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1;
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1;

use super::State;
use crate::config;
use crate::kde::{Bridge, ToWm};
use crate::protocols::window_management::river_seat_v1;
use crate::protocols::window_management::river_shell_surface_v1::RiverShellSurfaceV1;
use crate::protocols::xkb_bindings::river_xkb_binding_v1::{self, RiverXkbBindingV1};

/// A KDE app's key binding: the index of its component and action.
pub(super) struct KdeKey(usize);

#[derive(Default)]
pub(super) struct KdeWm {
    pub(super) bridge: Option<Bridge>,
    idle: Option<ZwpIdleInhibitManagerV1>,
    /// Our own shell surface the idle inhibitor sits on (river honours inhibitors of shell
    /// surfaces once it can find their node, river patch in ShellSurface.zig).
    surface: Option<(WlSurface, RiverShellSurfaceV1)>,
    inhibitor: Option<ZwpIdleInhibitorV1>,
    want_inhibit: bool,
    bindings: Vec<RiverXkbBindingV1>,
    actions: Vec<(String, String)>,
    /// New bindings waiting for a manage sequence (river enables bindings only in one).
    unbound: Option<Vec<reg::Binding>>,
}

impl KdeWm {
    pub(super) fn new(globals: &GlobalList, qh: &QueueHandle<State>) -> Self {
        let bridge = Bridge::new()
            .inspect_err(|e| warn!("KDE services: {e}"))
            .ok();
        let idle = globals.bind(qh, 1..=1, ()).ok();
        if idle.is_none() {
            warn!("zwp_idle_inhibit_manager_v1 missing: D-Bus inhibitions cannot hold idle off");
        }
        Self {
            bridge,
            idle,
            ..Self::default()
        }
    }

    /// The fd that wakes the loop for the services' messages; -1 (poll skips it) without them.
    pub(super) fn wake_fd(&self) -> i32 {
        use std::os::fd::AsRawFd as _;
        self.bridge.as_ref().map_or(-1, |b| b.wake.as_raw_fd())
    }
}

/// The Qt keys the config's own key bindings hold (Mod is Super under river).
pub(super) fn reserved_keys(config: &Config) -> Vec<i32> {
    config
        .binds
        .0
        .iter()
        .filter_map(|bind| {
            let Trigger::Keysym(sym) = bind.key.trigger else {
                return None;
            };
            let mut mods = bind.key.modifiers;
            if mods.contains(Modifiers::COMPOSITOR) {
                mods.remove(Modifiers::COMPOSITOR);
                mods |= Modifiers::SUPER;
            }
            let plain = Modifiers::CTRL | Modifiers::SHIFT | Modifiers::ALT | Modifiers::SUPER;
            if !plain.contains(mods) {
                return None;
            }
            qtkey::to_qt(sym.raw(), mods.bits())
        })
        .collect()
}

impl State {
    /// Acts on what the KDE services sent.
    pub(super) fn kde_messages(&mut self) {
        let Some(bridge) = &self.kde.bridge else {
            return;
        };
        bridge.drain_wake();
        let msgs: Vec<ToWm> = bridge.rx.try_iter().collect();
        for msg in msgs {
            match msg {
                ToWm::Bindings(bindings) => {
                    self.kde.unbound = Some(bindings);
                    self.wm.manage_dirty();
                }
                ToWm::Inhibit(on) => {
                    self.kde.want_inhibit = on;
                    self.wm.manage_dirty();
                }
                ToWm::Windows(reply) => {
                    let windows = self
                        .objs
                        .keys()
                        .filter_map(|&id| {
                            let w = self.desktop.window(id)?;
                            Some(krunner::Window {
                                id,
                                title: w.title.clone().unwrap_or_default(),
                                app_id: w.app_id.clone().unwrap_or_default(),
                            })
                        })
                        .collect();
                    let _ = reply.send(windows);
                }
                ToWm::Activate(id) => {
                    if !self.locked && self.objs.contains_key(&id) {
                        self.pending.push((Action::FocusWindow(id), None));
                        self.wm.manage_dirty();
                    }
                }
            }
        }
    }

    /// The config (re)loaded: its keys are reserved, and apps' keys rebound around them.
    pub(super) fn kde_reserve(&mut self) {
        let Some(bridge) = &self.kde.bridge else {
            return;
        };
        let keys = reserved_keys(&self.config);
        let bindings = {
            let mut r = bridge.registry.lock().unwrap();
            r.set_reserved(keys);
            r.bindings()
        };
        self.kde.unbound = Some(bindings);
        self.wm.manage_dirty();
    }

    /// In a manage sequence: binds KDE apps' keys on every seat, holds idle off or lets go.
    pub(super) fn kde_manage(&mut self, qh: &QueueHandle<Self>) {
        if let Some(bindings) = self.kde.unbound.take() {
            self.kde_bind(bindings, qh);
        }
        self.kde_inhibit(qh);
    }

    fn kde_bind(&mut self, bindings: Vec<reg::Binding>, qh: &QueueHandle<Self>) {
        for b in self.kde.bindings.drain(..) {
            b.destroy();
        }
        self.kde.actions.clear();
        let Some(xkb) = &self.xkb_bindings else {
            return;
        };
        for binding in bindings {
            let Some((keysym, mods)) = qtkey::from_qt(binding.key) else {
                continue;
            };
            let mods = river_seat_v1::Modifiers::from_bits_truncate(config::river_modifiers(
                Modifiers::from_bits_truncate(mods),
            ));
            let index = self.kde.actions.len();
            self.kde.actions.push((binding.component, binding.action));
            for seat in self.seats.values() {
                let b = xkb.get_xkb_binding(&seat.proxy, keysym, mods, qh, KdeKey(index));
                b.enable();
                self.kde.bindings.push(b);
            }
        }
    }

    fn kde_inhibit(&mut self, qh: &QueueHandle<Self>) {
        if !self.kde.want_inhibit {
            if let Some(inhibitor) = self.kde.inhibitor.take() {
                inhibitor.destroy();
            }
            return;
        }
        if self.kde.inhibitor.is_some() {
            return;
        }
        let (Some(idle), Some(compositor)) = (&self.kde.idle, &self.compositor) else {
            return;
        };
        let (surface, _) = self.kde.surface.get_or_insert_with(|| {
            let surface = compositor.create_surface(qh, ());
            let shell = self.wm.get_shell_surface(&surface, qh, ());
            (surface, shell)
        });
        self.kde.inhibitor = Some(idle.create_inhibitor(surface, qh, ()));
    }
}

impl Dispatch<RiverXkbBindingV1, KdeKey> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        key: &KdeKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let pressed = match event {
            river_xkb_binding_v1::Event::Pressed => true,
            river_xkb_binding_v1::Event::Released => false,
            _ => return,
        };
        let Some((component, action)) = state.kde.actions.get(key.0) else {
            return;
        };
        let Some(bridge) = state.kde.bridge.as_ref() else {
            return;
        };
        let Some(conn) = bridge.conn.get() else {
            return;
        };
        // A desktop-file component's action is started on the press, never signalled.
        let launch = bridge
            .registry
            .lock()
            .unwrap()
            .service_launch(component, action);
        if let Some(launch) = launch {
            if pressed {
                crate::kde::start_service(conn, launch);
                state.pointer.key_pressed();
            }
            return;
        }
        crate::kde::emit_shortcut(conn, component, action, pressed);
        if pressed {
            state.pointer.key_pressed();
        }
    }
}

delegate_noop!(State: ignore ZwpIdleInhibitManagerV1);
delegate_noop!(State: ignore ZwpIdleInhibitorV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configs_mod_binds_are_reserved_as_super() {
        let config = Config::parse_mem(
            r#"
            binds {
                Mod+Shift+S { screenshot; }
                Print { screenshot-screen; }
                Mod+WheelScrollDown { focus-workspace-down; }
            }
            "#,
        )
        .unwrap();
        let mut keys = reserved_keys(&config);
        keys.sort_unstable();
        assert_eq!(keys, vec![0x0100_0009, 0x53 | 0x1000_0000 | 0x0200_0000]);
    }
}
