//! Client bindings for river's protocols, generated from the XML in the repository's `protocol/`.

#![allow(dead_code, non_camel_case_types, non_upper_case_globals, clippy::all)]

pub mod window_management {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/river-window-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/river-window-management-v1.xml");
}

pub mod xkb_bindings {
    use wayland_client;

    use super::window_management::*;

    pub mod __interfaces {
        use super::super::window_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/river-xkb-bindings-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/river-xkb-bindings-v1.xml");
}

pub mod layer_shell {
    use wayland_client;

    use super::window_management::*;

    pub mod __interfaces {
        use super::super::window_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/river-layer-shell-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/river-layer-shell-v1.xml");
}

pub mod input_management {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/river-input-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/river-input-management-v1.xml");
}

pub mod libinput_config {
    use wayland_client;

    use super::input_management::*;

    pub mod __interfaces {
        use super::super::input_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/river-libinput-config-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/river-libinput-config-v1.xml");
}

pub mod xkb_config {
    use wayland_client;

    use super::input_management::*;

    pub mod __interfaces {
        use super::super::input_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/river-xkb-config-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/river-xkb-config-v1.xml");
}

/// newm-river fork-only: per-window scale and opacity (`protocol/newm-river-render-v1.xml`).
pub mod newm_render {
    use wayland_client;

    use super::window_management::*;

    pub mod __interfaces {
        use super::super::window_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/newm-river-render-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/newm-river-render-v1.xml");
}

/// newm-river fork-only: the window manager's side of keyboard-shortcuts-inhibit
/// (`protocol/newm-river-shortcuts-v1.xml`).
pub mod newm_shortcuts {
    use wayland_client;

    use super::window_management::*;
    use super::xkb_bindings::*;

    pub mod __interfaces {
        use super::super::window_management::__interfaces::*;
        use super::super::xkb_bindings::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/newm-river-shortcuts-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/newm-river-shortcuts-v1.xml");
}

/// newm-river fork-only: client hints the window manager acts on, system bell rings and modal
/// dialogs (`protocol/newm-river-hints-v1.xml`).
pub mod newm_hints {
    use wayland_client;

    use super::window_management::*;

    pub mod __interfaces {
        use super::super::window_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/newm-river-hints-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/newm-river-hints-v1.xml");
}

/// newm-river fork-only: touchpad gesture and pointer axis bindings
/// (`protocol/newm-river-gestures-v1.xml`).
pub mod newm_gestures {
    use wayland_client;
    use wayland_client::protocol::*;

    use super::window_management::*;

    pub mod __interfaces {
        use super::super::window_management::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/newm-river-gestures-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/newm-river-gestures-v1.xml");
}

/// newm-river fork-only: zkde_screencast_unstable_v1 streams relayed to the window manager
/// (`protocol/newm-river-screencast-v1.xml`).
pub mod newm_screencast {
    use wayland_client;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("../../protocol/newm-river-screencast-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../protocol/newm-river-screencast-v1.xml");
}
