//! Input settings from the config, through river's input protocols: the keymap and numlock
//! (`river-xkb-config-v1`), key repeat and scroll factors (`river-input-management-v1`) and the
//! touchpad and mouse settings (`river-libinput-config-v1`, middle-button scrolling included).

use std::collections::HashMap;
use std::os::fd::AsFd;

use niri_config::{Config, Xkb};
use tracing::{debug, info, warn};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use xkbcommon::xkb;

use crate::config::{self, Setting, Support};
use crate::protocols::input_management::river_input_device_v1::{self, RiverInputDeviceV1};
use crate::protocols::input_management::river_input_manager_v1::{self, RiverInputManagerV1};
use crate::protocols::libinput_config::river_libinput_config_v1::{self, RiverLibinputConfigV1};
use crate::protocols::libinput_config::river_libinput_device_v1::{self, RiverLibinputDeviceV1};
use crate::protocols::libinput_config::river_libinput_result_v1::{self, RiverLibinputResultV1};
use crate::protocols::xkb_config::river_xkb_config_v1::{self, RiverXkbConfigV1};
use crate::protocols::xkb_config::river_xkb_keyboard_v1::{self, RiverXkbKeyboardV1};
use crate::protocols::xkb_config::river_xkb_keymap_v1::{self, RiverXkbKeymapV1};
use crate::river::{memfd, State};

struct Device {
    proxy: RiverInputDeviceV1,
    keyboard: bool,
    configured: bool,
}

struct LibinputDevice {
    proxy: RiverLibinputDeviceV1,
    input: Option<ObjectId>,
    support: Support,
    configured: bool,
}

#[derive(Default)]
pub struct InputState {
    manager: Option<RiverInputManagerV1>,
    libinput: Option<RiverLibinputConfigV1>,
    xkb: Option<RiverXkbConfigV1>,
    devices: HashMap<ObjectId, Device>,
    libinput_devices: HashMap<ObjectId, LibinputDevice>,
    keyboards: HashMap<ObjectId, RiverXkbKeyboardV1>,
    keymap: Option<RiverXkbKeymapV1>,
    keymap_ok: bool,
}

/// The keymap text for the config's `xkb`, when it sets one (else river keeps its own).
fn keymap_text(x: &Xkb) -> Option<String> {
    if *x == Xkb::default() {
        return None;
    }
    if x.file.is_some() {
        info!("config: xkb file is not applied by newm-river; using rules/layout/variant/options");
    }
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        &x.rules,
        &x.model,
        &x.layout,
        &x.variant,
        x.options.clone(),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    );
    match keymap {
        Some(k) => Some(k.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)),
        None => {
            warn!("xkb: no keymap for {x:?}");
            None
        }
    }
}

impl InputState {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<State>, config: &Config) -> Self {
        let mut s = InputState {
            manager: globals.bind(qh, 1..=2, ()).ok(),
            libinput: globals.bind(qh, 1..=2, ()).ok(),
            xkb: globals.bind(qh, 1..=3, ()).ok(),
            ..InputState::default()
        };
        if let (Some(xkbc), Some(text)) = (&s.xkb, keymap_text(&config.input.keyboard.xkb)) {
            let mut bytes = text.into_bytes();
            bytes.push(0);
            match memfd(c"newm-river-keymap", &bytes) {
                Ok(file) => {
                    s.keymap = Some(xkbc.create_keymap(
                        file.as_fd(),
                        river_xkb_config_v1::KeymapFormat::TextV1,
                        qh,
                        (),
                    ));
                }
                Err(e) => warn!("keymap: {e}"),
            }
        }
        s
    }

    /// A reloaded config: a new keymap (applied once river takes it), key repeat and every
    /// libinput device's settings again.
    pub fn reconfigure(&mut self, config: &Config, qh: &QueueHandle<State>) {
        if let (Some(xkbc), Some(text)) = (&self.xkb, keymap_text(&config.input.keyboard.xkb)) {
            let mut bytes = text.into_bytes();
            bytes.push(0);
            match memfd(c"newm-river-keymap", &bytes) {
                Ok(file) => {
                    self.keymap_ok = false;
                    let old = self.keymap.replace(xkbc.create_keymap(
                        file.as_fd(),
                        river_xkb_config_v1::KeymapFormat::TextV1,
                        qh,
                        (),
                    ));
                    if let Some(old) = old {
                        old.destroy();
                    }
                }
                Err(e) => warn!("keymap: {e}"),
            }
        }
        let kb = &config.input.keyboard;
        let (rate, delay) = (i32::from(kb.repeat_rate), i32::from(kb.repeat_delay));
        for d in self.devices.values().filter(|d| d.keyboard) {
            d.proxy.set_repeat_info(rate, delay);
        }
        for dev in self.libinput_devices.values().filter(|d| d.configured) {
            let settings = if dev.support.is_touchpad() {
                config::touchpad_settings(&config.input.touchpad, &dev.support)
            } else {
                config::mouse_settings(&config.input.mouse, &dev.support)
            };
            let input = dev
                .input
                .as_ref()
                .and_then(|i| self.devices.get(i))
                .map(|d| &d.proxy);
            for setting in settings {
                apply(&dev.proxy, input, setting, qh);
            }
        }
    }

    fn setup_keyboard(&self, kb: &RiverXkbKeyboardV1, config: &Config) {
        if self.keymap_ok
            && let Some(k) = &self.keymap
        {
            kb.set_keymap(k);
        }
        if config.input.keyboard.numlock {
            kb.numlock_enable();
        }
    }
}

fn apply(
    dev: &RiverLibinputDeviceV1,
    input: Option<&RiverInputDeviceV1>,
    setting: Setting,
    qh: &QueueHandle<State>,
) {
    use river_libinput_device_v1 as d;
    match setting {
        Setting::Tap(on) => {
            let s = if on {
                d::TapState::Enabled
            } else {
                d::TapState::Disabled
            };
            dev.set_tap(s, qh, ());
        }
        Setting::Drag(on) => {
            let s = if on {
                d::DragState::Enabled
            } else {
                d::DragState::Disabled
            };
            dev.set_drag(s, qh, ());
        }
        Setting::DragLock(on) => {
            let s = if on {
                d::DragLockState::EnabledTimeout
            } else {
                d::DragLockState::Disabled
            };
            dev.set_drag_lock(s, qh, ());
        }
        Setting::NaturalScroll(on) => {
            let s = if on {
                d::NaturalScrollState::Enabled
            } else {
                d::NaturalScrollState::Disabled
            };
            dev.set_natural_scroll(s, qh, ());
        }
        Setting::LeftHanded(on) => {
            let s = if on {
                d::LeftHandedState::Enabled
            } else {
                d::LeftHandedState::Disabled
            };
            dev.set_left_handed(s, qh, ());
        }
        Setting::MiddleEmulation(on) => {
            let s = if on {
                d::MiddleEmulationState::Enabled
            } else {
                d::MiddleEmulationState::Disabled
            };
            dev.set_middle_emulation(s, qh, ());
        }
        Setting::Dwt(on) => {
            let s = if on {
                d::DwtState::Enabled
            } else {
                d::DwtState::Disabled
            };
            dev.set_dwt(s, qh, ());
        }
        Setting::Dwtp(on) => {
            let s = if on {
                d::DwtpState::Enabled
            } else {
                d::DwtpState::Disabled
            };
            dev.set_dwtp(s, qh, ());
        }
        Setting::ScrollMethod(m) => {
            use niri_config::ScrollMethod as M;
            let s = match m {
                M::NoScroll => d::ScrollMethod::NoScroll,
                M::TwoFinger => d::ScrollMethod::TwoFinger,
                M::Edge => d::ScrollMethod::Edge,
                M::OnButtonDown => d::ScrollMethod::OnButtonDown,
            };
            dev.set_scroll_method(s, qh, ());
        }
        Setting::ScrollButton(b) => {
            dev.set_scroll_button(b, qh, ());
        }
        Setting::ScrollButtonLock(on) => {
            let s = if on {
                d::ScrollButtonLockState::Enabled
            } else {
                d::ScrollButtonLockState::Disabled
            };
            dev.set_scroll_button_lock(s, qh, ());
        }
        Setting::Clickfinger(finger) => {
            let m = if finger {
                d::ClickMethod::Clickfinger
            } else {
                d::ClickMethod::ButtonAreas
            };
            dev.set_click_method(m, qh, ());
        }
        Setting::FlatAccel(flat) => {
            let p = if flat {
                d::AccelProfile::Flat
            } else {
                d::AccelProfile::Adaptive
            };
            dev.set_accel_profile(p, qh, ());
        }
        Setting::AccelSpeed(speed) => {
            dev.set_accel_speed(speed.clamp(-1., 1.).to_ne_bytes().to_vec(), qh, ());
        }
        Setting::ScrollFactor(f) => {
            if let Some(input) = input {
                input.set_scroll_factor(f.max(0.));
            }
        }
    }
}

impl Dispatch<RiverInputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &RiverInputManagerV1,
        event: river_input_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            river_input_manager_v1::Event::InputDevice { id } => {
                state.input.devices.insert(
                    id.id(),
                    Device {
                        proxy: id,
                        keyboard: false,
                        configured: false,
                    },
                );
            }
            river_input_manager_v1::Event::Finished => {
                if let Some(g) = state.input.manager.take() {
                    g.destroy();
                }
            }
        }
    }

    event_created_child!(State, RiverInputManagerV1, [
        river_input_manager_v1::EVT_INPUT_DEVICE_OPCODE => (RiverInputDeviceV1, ()),
    ]);
}

impl Dispatch<RiverInputDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverInputDeviceV1,
        event: river_input_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_input_device_v1::Event;
        let key = proxy.id();
        let kb = &state.config.input.keyboard;
        let (rate, delay) = (i32::from(kb.repeat_rate), i32::from(kb.repeat_delay));
        match event {
            Event::Removed => {
                state.input.devices.remove(&key);
                proxy.destroy();
                return;
            }
            Event::Type { _type } => {
                if let Some(d) = state.input.devices.get_mut(&key) {
                    d.keyboard = _type == WEnum::Value(river_input_device_v1::Type::Keyboard);
                }
                if proxy.version() >= 2 {
                    return;
                }
            }
            Event::Done => {}
            _ => return,
        }
        if let Some(d) = state.input.devices.get_mut(&key)
            && d.keyboard
            && !d.configured
        {
            d.configured = true;
            d.proxy.set_repeat_info(rate, delay);
            debug!("keyboard repeat {rate}/s after {delay} ms");
        }
    }
}

impl Dispatch<RiverLibinputConfigV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &RiverLibinputConfigV1,
        event: river_libinput_config_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            river_libinput_config_v1::Event::LibinputDevice { id } => {
                state.input.libinput_devices.insert(
                    id.id(),
                    LibinputDevice {
                        proxy: id,
                        input: None,
                        support: Support::default(),
                        configured: false,
                    },
                );
            }
            river_libinput_config_v1::Event::Finished => {
                if let Some(g) = state.input.libinput.take() {
                    g.destroy();
                }
            }
        }
    }

    event_created_child!(State, RiverLibinputConfigV1, [
        river_libinput_config_v1::EVT_LIBINPUT_DEVICE_OPCODE => (RiverLibinputDeviceV1, ()),
    ]);
}

fn bits<T>(v: WEnum<T>, f: impl Fn(T) -> u32) -> u32 {
    match v {
        WEnum::Value(v) => f(v),
        WEnum::Unknown(raw) => raw,
    }
}

impl Dispatch<RiverLibinputDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverLibinputDeviceV1,
        event: river_libinput_device_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_libinput_device_v1::Event;
        let key = proxy.id();
        let Some(dev) = state.input.libinput_devices.get_mut(&key) else {
            return;
        };
        let s = &mut dev.support;
        match event {
            Event::Removed => {
                state.input.libinput_devices.remove(&key);
                proxy.destroy();
            }
            Event::InputDevice { device } => dev.input = Some(device.id()),
            Event::TapSupport { finger_count } => s.tap_fingers = finger_count,
            Event::NaturalScrollSupport { supported } => s.natural_scroll = supported != 0,
            Event::LeftHandedSupport { supported } => s.left_handed = supported != 0,
            Event::MiddleEmulationSupport { supported } => s.middle_emulation = supported != 0,
            Event::DwtSupport { supported } => s.dwt = supported != 0,
            Event::DwtpSupport { supported } => s.dwtp = supported != 0,
            Event::ScrollMethodSupport { methods } => {
                s.scroll_methods = bits(methods, |m| m.bits())
            }
            Event::ClickMethodSupport { methods } => s.click_methods = bits(methods, |m| m.bits()),
            Event::AccelProfilesSupport { profiles } => {
                s.accel_profiles = bits(profiles, |p| p.bits());
            }
            Event::Done => {
                if dev.configured {
                    return;
                }
                dev.configured = true;
                let support = dev.support;
                let settings = if support.is_touchpad() {
                    config::touchpad_settings(&state.config.input.touchpad, &support)
                } else {
                    config::mouse_settings(&state.config.input.mouse, &support)
                };
                let input = dev
                    .input
                    .as_ref()
                    .and_then(|i| state.input.devices.get(i))
                    .map(|d| &d.proxy);
                for setting in settings {
                    apply(&dev.proxy, input, setting, qh);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverLibinputResultV1, ()> for State {
    fn event(
        _: &mut Self,
        proxy: &RiverLibinputResultV1,
        event: river_libinput_result_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, river_libinput_result_v1::Event::Success) {
            debug!("libinput setting {}: {event:?}", proxy.id());
        }
    }
}

impl Dispatch<RiverXkbConfigV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbConfigV1,
        event: river_xkb_config_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            river_xkb_config_v1::Event::XkbKeyboard { id } => {
                state.input.setup_keyboard(&id, &state.config);
                state.input.keyboards.insert(id.id(), id);
            }
            river_xkb_config_v1::Event::Finished => {
                if let Some(g) = state.input.xkb.take() {
                    g.destroy();
                }
            }
        }
    }

    event_created_child!(State, RiverXkbConfigV1, [
        river_xkb_config_v1::EVT_XKB_KEYBOARD_OPCODE => (RiverXkbKeyboardV1, ()),
    ]);
}

impl Dispatch<RiverXkbKeyboardV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &RiverXkbKeyboardV1,
        event: river_xkb_keyboard_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let river_xkb_keyboard_v1::Event::Removed = event {
            state.input.keyboards.remove(&proxy.id());
            proxy.destroy();
        }
    }
}

impl Dispatch<RiverXkbKeymapV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbKeymapV1,
        event: river_xkb_keymap_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            river_xkb_keymap_v1::Event::Success => {
                state.input.keymap_ok = true;
                for kb in state.input.keyboards.values() {
                    state.input.setup_keyboard(kb, &state.config);
                }
            }
            river_xkb_keymap_v1::Event::Failure { error_msg } => {
                warn!("river refused the keymap: {error_msg}");
            }
        }
    }
}
