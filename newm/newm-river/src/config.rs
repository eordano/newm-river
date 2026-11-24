//! The `config.kdl`, read with niri-config, and what newm-river
//! takes from it: layout settings for the model, modifiers for river, the spawn environment and
//! the input settings river's libinput protocol applies.

use std::path::{Path, PathBuf};

use niri_config::input::{Mouse, Touchpad};
use niri_config::{Config, Modifiers, ScrollMethod};
use tracing::{info, warn};

use crate::model::{GlowStyle, Settings};
use crate::rules::Style;

/// `-c`, else `$NIRI_CONFIG`, else `~/.config/newm-river/config.kdl`.
pub fn path(cli: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli {
        return p;
    }
    if let Some(p) = std::env::var_os("NIRI_CONFIG").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/etc"));
    base.join("newm-river/config.kdl")
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    Config::load(path)
        .config
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// The config at `path`, else niri's default one: a broken config never stops the session.
pub fn load_or_default(path: &Path) -> Config {
    if !path.exists() {
        warn!("no config at {}: using the default one", path.display());
        return Config::load_default();
    }
    match load(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "config {} does not load, using the default one:\n{e}",
                path.display()
            );
            Config::load_default()
        }
    }
}

/// The live settings newm-river does not apply: the focus ring (it draws the border instead),
/// hide-when-typing (river shows the cursor), and magnified-input (river's render patch gives a
/// scaled window no pointer input, and its protocol has no request to opt a window in).
pub const LIVE_SKIP: &[&str] = &[
    "layout.focus-ring.width",
    "layout.focus-ring.active-color",
    "cursor.hide-when-typing",
    "layout.canvas.magnified-input",
];

/// Says once which parts of the config newm-river does not apply.
pub fn log_unsupported(c: &Config) {
    let mut ignored = Vec::new();
    for o in &c.outputs.0 {
        let parts = [
            ("max-bpc", o.max_bpc.is_some()),
            ("background-color", o.background_color.is_some()),
            ("backdrop-color", o.backdrop_color.is_some()),
            ("hot-corners", o.hot_corners.is_some()),
            ("layout", o.layout.is_some()),
        ];
        for (part, set) in parts {
            if set {
                info!(
                    "config: output \"{}\": {part} is not applied by newm-river",
                    o.name
                );
            }
        }
    }
    if !c.layer_rules.is_empty() {
        ignored.push("layer-rule");
    }
    if !c.workspaces.is_empty() {
        ignored.push("workspace (one canvas per output)");
    }
    if !c.layout.focus_ring.off {
        ignored.push("layout.focus-ring (the border is drawn instead)");
    }
    if c.layout.border.active_gradient.is_some() || c.layout.border.inactive_gradient.is_some() {
        ignored.push("border gradients");
    }
    for what in ignored {
        info!("config: {what} is not applied by newm-river");
    }
}

/// What the model takes from the config.
pub fn settings(c: &Config) -> Settings {
    let canvas = &c.layout.canvas;
    let border = &c.layout.border;
    let shadow = &c.layout.shadow;
    let corner_radius = c
        .window_rules
        .iter()
        .filter(|r| r.matches.is_empty() && r.excludes.is_empty())
        .filter_map(|r| r.geometry_corner_radius)
        .next_back()
        .map_or(0., |r| f64::from(r.top_left));
    Settings {
        canvas: newm_canvas::Config {
            tile_w: canvas.tile_width,
            tile_h: canvas.tile_height,
            zoom_step: canvas.zoom_step,
            zoom_min: canvas.zoom_min,
            zoom_max: canvas.zoom_max,
            distance_weight: canvas.distance_weight,
            diagonal_weight: canvas.diagonal_weight,
        },
        gaps: c.layout.gaps,
        border: Style {
            width: if border.off {
                0
            } else {
                border.width.round() as i32
            },
            active: border.active_color.to_array_premul(),
            inactive: border.inactive_color.to_array_premul(),
            urgent: border.urgent_color.to_array_premul(),
        },
        // A fully transparent shadow would change no pixel: river is asked for none.
        glow: shadow
            .on
            .then(|| GlowStyle {
                softness: shadow.softness,
                spread: shadow.spread,
                offset: (shadow.offset.x.0, shadow.offset.y.0),
                color: shadow.color.to_array_premul(),
            })
            .filter(|g| g.color[3] > 0.),
        default_width: canvas.default_width,
        default_height: canvas.default_height,
        corner_radius,
        floating_follow: canvas.floating_follow,
        pan_momentum: canvas.pan_momentum,
    }
}

/// river's modifier bits (`river_seat_v1.modifiers`) for niri's: Mod is Super, as on a TTY
/// session.
pub fn river_modifiers(m: Modifiers) -> u32 {
    let mut bits = 0;
    for (niri, river) in [
        (Modifiers::SHIFT, 1),
        (Modifiers::CTRL, 4),
        (Modifiers::ALT, 8),
        (Modifiers::ISO_LEVEL5_SHIFT, 32),
        (Modifiers::SUPER, 64),
        (Modifiers::COMPOSITOR, 64),
        (Modifiers::ISO_LEVEL3_SHIFT, 128),
    ] {
        if m.contains(niri) {
            bits |= river;
        }
    }
    bits
}

/// The environment every spawned command gets: the config's `environment`, the IPC socket and
/// the cursor theme. `None` removes a variable.
pub fn environment(c: &Config, socket: &Path) -> Vec<(String, Option<String>)> {
    let mut env: Vec<(String, Option<String>)> = vec![(
        niri_ipc::socket::SOCKET_PATH_ENV.to_owned(),
        Some(socket.display().to_string()),
    )];
    if !c.cursor.xcursor_theme.is_empty() {
        env.push(("XCURSOR_THEME".into(), Some(c.cursor.xcursor_theme.clone())));
        env.push((
            "XCURSOR_SIZE".into(),
            Some(c.cursor.xcursor_size.to_string()),
        ));
    }
    env.extend(
        c.environment
            .0
            .iter()
            .map(|v| (v.name.clone(), v.value.clone())),
    );
    env
}

/// One libinput setting newm-river asks river for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Setting {
    Tap(bool),
    Drag(bool),
    DragLock(bool),
    NaturalScroll(bool),
    LeftHanded(bool),
    MiddleEmulation(bool),
    Dwt(bool),
    Dwtp(bool),
    /// `two-finger`, `edge`, `on-button-down` or none.
    ScrollMethod(ScrollMethod),
    ScrollButton(u32),
    ScrollButtonLock(bool),
    /// `true`: clickfinger, `false`: button areas.
    Clickfinger(bool),
    /// `true`: flat, `false`: adaptive.
    FlatAccel(bool),
    AccelSpeed(f64),
    /// The input device's scroll factor.
    ScrollFactor(f64),
}

/// What a device supports, from river's libinput events.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Support {
    pub tap_fingers: i32,
    pub natural_scroll: bool,
    pub left_handed: bool,
    pub middle_emulation: bool,
    pub dwt: bool,
    pub dwtp: bool,
    /// river's `scroll_methods` bits.
    pub scroll_methods: u32,
    pub click_methods: u32,
    pub accel_profiles: u32,
}

impl Support {
    pub fn is_touchpad(&self) -> bool {
        self.tap_fingers > 0
    }
}

fn scroll_method_bit(m: ScrollMethod) -> u32 {
    match m {
        ScrollMethod::NoScroll => 0,
        ScrollMethod::TwoFinger => 1,
        ScrollMethod::Edge => 2,
        ScrollMethod::OnButtonDown => 4,
    }
}

fn scroll_settings(
    out: &mut Vec<Setting>,
    s: &Support,
    method: Option<ScrollMethod>,
    button: Option<u32>,
    lock: bool,
) {
    if let Some(m) = method
        && (m == ScrollMethod::NoScroll || s.scroll_methods & scroll_method_bit(m) != 0)
    {
        out.push(Setting::ScrollMethod(m));
        if m == ScrollMethod::OnButtonDown {
            if let Some(b) = button {
                out.push(Setting::ScrollButton(b));
            }
            out.push(Setting::ScrollButtonLock(lock));
        }
    }
}

fn accel_settings(
    out: &mut Vec<Setting>,
    s: &Support,
    speed: f64,
    profile: Option<niri_config::input::AccelProfile>,
) {
    out.push(Setting::AccelSpeed(speed));
    if let Some(p) = profile {
        let flat = p == niri_config::input::AccelProfile::Flat;
        if s.accel_profiles & if flat { 1 } else { 2 } != 0 {
            out.push(Setting::FlatAccel(flat));
        }
    }
}

/// The settings a touchpad gets from `input.touchpad`.
pub fn touchpad_settings(t: &Touchpad, s: &Support) -> Vec<Setting> {
    let mut out = vec![Setting::Tap(t.tap)];
    if let Some(d) = t.drag {
        out.push(Setting::Drag(d));
    }
    out.push(Setting::DragLock(t.drag_lock));
    if s.natural_scroll {
        out.push(Setting::NaturalScroll(t.natural_scroll));
    }
    if s.left_handed {
        out.push(Setting::LeftHanded(t.left_handed));
    }
    if s.middle_emulation {
        out.push(Setting::MiddleEmulation(t.middle_emulation));
    }
    if s.dwt {
        out.push(Setting::Dwt(t.dwt));
    }
    if s.dwtp {
        out.push(Setting::Dwtp(t.dwtp));
    }
    scroll_settings(
        &mut out,
        s,
        t.scroll_method,
        t.scroll_button,
        t.scroll_button_lock,
    );
    if let Some(m) = t.click_method {
        let finger = m == niri_config::input::ClickMethod::Clickfinger;
        if s.click_methods & if finger { 2 } else { 1 } != 0 {
            out.push(Setting::Clickfinger(finger));
        }
    }
    accel_settings(&mut out, s, t.accel_speed.0, t.accel_profile);
    if let Some(f) = t.scroll_factor.as_ref().and_then(|f| f.base) {
        out.push(Setting::ScrollFactor(f.0));
    }
    out
}

/// The settings a mouse gets from `input.mouse`.
pub fn mouse_settings(m: &Mouse, s: &Support) -> Vec<Setting> {
    let mut out = Vec::new();
    if s.natural_scroll {
        out.push(Setting::NaturalScroll(m.natural_scroll));
    }
    if s.left_handed {
        out.push(Setting::LeftHanded(m.left_handed));
    }
    if s.middle_emulation {
        out.push(Setting::MiddleEmulation(m.middle_emulation));
    }
    scroll_settings(
        &mut out,
        s,
        m.scroll_method,
        m.scroll_button,
        m.scroll_button_lock,
    );
    accel_settings(&mut out, s, m.accel_speed.0, m.accel_profile);
    if let Some(f) = m.scroll_factor.as_ref().and_then(|f| f.base) {
        out.push(Setting::ScrollFactor(f.0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r##"
        input {
            keyboard {
                xkb { layout "us"; variant "altgr-intl"; options "caps:escape"; }
                repeat-delay 200
                repeat-rate 60
            }
            touchpad { tap; scroll-factor 0.5; }
            mouse { scroll-method "on-button-down"; scroll-button 274; }
        }
        layout {
            gaps 10
            focus-ring { off; }
            border { width 1; active-color "#82FB9C8C"; inactive-color "transparent"; }
            shadow { on; softness 26; spread 0; offset x=0 y=0; color "#82FB9C36"; }
            canvas {
                tile-width 119
                tile-height 113
                zoom-max 7
                default-width { fixed 960; }
                default-height { proportion 0.666700; }
            }
        }
        window-rule { geometry-corner-radius 8; clip-to-geometry true; }
        binds {
            Mod+Shift+q { resize-edge "left" 1; }
            Mod+Ctrl+H { swap-neighbour "left"; }
            Alt+q { close-window; }
        }
    "##;

    #[test]
    fn layout_settings_come_from_the_config() {
        let c = Config::parse_mem(LIVE).unwrap();
        let s = settings(&c);
        assert_eq!(
            (s.canvas.tile_w, s.canvas.tile_h, s.canvas.zoom_max),
            (119., 113., 7)
        );
        assert_eq!(s.gaps, 10.);
        assert_eq!(s.border.width, 1);
        assert_eq!(s.border.inactive, [0.; 4]);
        let glow = s.glow.unwrap();
        assert_eq!(glow.softness, 26.);
        assert!((glow.color[3] - 0x36 as f32 / 255.).abs() < 1e-3);
        assert_eq!(s.default_width, Some(niri_config::PresetSize::Fixed(960)));
        assert_eq!(s.corner_radius, 8.);
    }

    #[test]
    fn pan_momentum_comes_from_the_config_and_a_pan_glides_on_by_default() {
        let c = Config::parse_mem(LIVE).unwrap();
        assert_eq!(settings(&c).pan_momentum, 1.);
        let live = LIVE.replace("zoom-max 7", "zoom-max 7\n                pan-momentum 0");
        assert_eq!(
            settings(&Config::parse_mem(&live).unwrap()).pan_momentum,
            0.
        );
    }

    #[test]
    fn a_transparent_shadow_is_no_shadow() {
        let c = Config::parse_mem(&LIVE.replace("#82FB9C36", "transparent")).unwrap();
        assert_eq!(settings(&c).glow, None);
    }

    #[test]
    fn modifiers_map_to_river() {
        let c = Config::parse_mem(LIVE).unwrap();
        let mods: Vec<u32> = c
            .binds
            .0
            .iter()
            .map(|b| river_modifiers(b.key.modifiers))
            .collect();
        assert_eq!(mods, vec![64 | 1, 64 | 4, 8]);
    }

    #[test]
    fn mouse_scrolls_on_middle_button_down() {
        let c = Config::parse_mem(LIVE).unwrap();
        let support = Support {
            scroll_methods: 1 | 2 | 4,
            accel_profiles: 3,
            ..Support::default()
        };
        let s = mouse_settings(&c.input.mouse, &support);
        assert!(s.contains(&Setting::ScrollMethod(ScrollMethod::OnButtonDown)));
        assert!(s.contains(&Setting::ScrollButton(274)));
        let pad = Support {
            tap_fingers: 3,
            ..support
        };
        let t = touchpad_settings(&c.input.touchpad, &pad);
        assert!(t.contains(&Setting::Tap(true)));
        assert!(t.contains(&Setting::ScrollFactor(0.5)));
    }

    #[test]
    fn the_live_skip_list_names_live_settings() {
        for key in LIVE_SKIP {
            assert!(niri_config::live::spec(key).is_some(), "{key}");
        }
    }

    #[test]
    fn the_live_config_validates() {
        let live = Path::new("/etc/newm-river/config.kdl");
        if live.exists() {
            load(live).unwrap();
        }
    }
}
