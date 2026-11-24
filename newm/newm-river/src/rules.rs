//! Window rules: which of the config's `window-rule`s apply to a window, what they decide when it
//! opens, and how a window opens by its own hints when no rule decides.

use niri_config::window_rule::{FloatingPosition, Match, OnXdgActivate, RelativeTo, WindowRule};
use niri_config::PresetSize;

/// What a rule can match a window by.
#[derive(Clone, Copy, Debug, Default)]
pub struct RuleInfo<'a> {
    pub app_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub is_focused: bool,
    pub is_floating: bool,
    pub at_startup: bool,
    pub is_urgent: bool,
}

/// A border or focus colour scheme: width in logical pixels (0: none) and premultiplied colours.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Style {
    pub width: i32,
    pub active: [f32; 4],
    pub inactive: [f32; 4],
    /// A window that rang the bell while unfocused (xdg-system-bell-v1), until it takes the focus.
    pub urgent: [f32; 4],
}

/// Where along one axis a floating window sits in its output's working area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    Middle,
    End,
}

/// A `default-floating-position`: offsets from a corner, edge middle or the centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Corner {
    pub x: f64,
    pub y: f64,
    pub h: Align,
    pub v: Align,
}

impl Corner {
    pub fn from_config(p: &FloatingPosition) -> Self {
        use Align::{End, Middle, Start};
        let (h, v) = match p.relative_to {
            RelativeTo::TopLeft => (Start, Start),
            RelativeTo::TopRight => (End, Start),
            RelativeTo::BottomLeft => (Start, End),
            RelativeTo::BottomRight => (End, End),
            RelativeTo::Top => (Middle, Start),
            RelativeTo::Bottom => (Middle, End),
            RelativeTo::Left => (Start, Middle),
            RelativeTo::Right => (End, Middle),
        };
        Self {
            x: p.x.0,
            y: p.y.0,
            h,
            v,
        }
    }

    /// The overlay corner: bottom right, `gap` from the working area's edges.
    pub fn overlay(gap: f64) -> Self {
        Self {
            x: gap,
            y: gap,
            h: Align::End,
            v: Align::End,
        }
    }

    /// Where a window of `size` starts along one axis of an area from `start` of `len`.
    pub fn place(align: Align, offset: f64, start: i32, len: i32, size: i32) -> i32 {
        let (start, len, size) = (f64::from(start), f64::from(len), f64::from(size));
        let at = match align {
            Align::Start => start + offset,
            Align::Middle => start + (len - size) / 2. + offset,
            Align::End => start + len - size - offset,
        };
        at.round() as i32
    }
}

/// What the rules that apply to a window decide when it opens.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolved {
    pub open_floating: Option<bool>,
    pub open_focused: Option<bool>,
    /// `on-xdg-activate`: what an xdg-activation request does to the window.
    pub on_xdg_activate: Option<OnXdgActivate>,
    pub floating_position: Option<Corner>,
    /// `default-column-width`: `Some(None)` asks for no size.
    pub default_width: Option<Option<PresetSize>>,
    pub default_height: Option<Option<PresetSize>>,
    pub border: Style,
    /// Whether anything is drawn behind the window (the focus glow): not with
    /// `draw-border-with-background false`, which a translucent HUD window sets.
    pub behind: bool,
}

fn matches(m: &Match, w: &RuleInfo) -> bool {
    let regex = |re: &Option<niri_config::utils::RegexEq>, s: Option<&str>| {
        re.as_ref()
            .is_none_or(|re| s.is_some_and(|s| re.0.is_match(s)))
    };
    let flag = |want: Option<bool>, have: bool| want.is_none_or(|want| want == have);
    regex(&m.app_id, w.app_id)
        && regex(&m.title, w.title)
        && flag(m.is_focused, w.is_focused)
        && flag(m.is_active, w.is_focused)
        && flag(m.is_floating, w.is_floating)
        && flag(m.at_startup, w.at_startup)
        && flag(m.is_urgent, w.is_urgent)
        && flag(m.is_window_cast_target, false)
        && flag(m.is_active_in_column, true)
}

/// niri's rule semantics: any `match` (or none given) and no `exclude`.
pub fn applies(rule: &WindowRule, w: &RuleInfo) -> bool {
    (rule.matches.is_empty() || rule.matches.iter().any(|m| matches(m, w)))
        && !rule.excludes.iter().any(|m| matches(m, w))
}

/// Merges the rules that apply, later ones winning, over the layout's border.
pub fn resolve(rules: &[WindowRule], w: &RuleInfo, border: Style) -> Resolved {
    let mut r = Resolved {
        border,
        behind: true,
        ..Resolved::default()
    };
    for rule in rules.iter().filter(|rule| applies(rule, w)) {
        if let Some(x) = rule.open_floating {
            r.open_floating = Some(x);
        }
        if let Some(x) = rule.open_focused {
            r.open_focused = Some(x);
        }
        if let Some(x) = rule.on_xdg_activate {
            r.on_xdg_activate = Some(x);
        }
        if let Some(x) = rule.draw_border_with_background {
            r.behind = x;
        }
        if let Some(p) = &rule.default_floating_position {
            r.floating_position = Some(Corner::from_config(p));
        }
        if let Some(x) = &rule.default_column_width {
            r.default_width = Some(x.0);
        }
        if let Some(x) = &rule.default_window_height {
            r.default_height = Some(x.0);
        }
        let b = &rule.border;
        if b.off {
            r.border.width = 0;
        } else if b.on && r.border.width == 0 {
            r.border.width = 1;
        }
        if let Some(width) = &b.width
            && !b.off
        {
            r.border.width = width.0.round() as i32;
        }
        if let Some(c) = b.active_color {
            r.border.active = c.to_array_premul();
        }
        if let Some(c) = b.inactive_color {
            r.border.inactive = c.to_array_premul();
        }
        if let Some(c) = b.urgent_color {
            r.border.urgent = c.to_array_premul();
        }
    }
    r
}

/// The opacity the rules give a window now (`opacity 0.9` for unfocused windows): applied by
/// the render patch, see `render::RenderExt`.
pub fn opacity(rules: &[WindowRule], w: &RuleInfo) -> f32 {
    rules
        .iter()
        .filter(|rule| applies(rule, w))
        .filter_map(|rule| rule.opacity)
        .next_back()
        .unwrap_or(1.)
}

/// How a window opens by its own hints, when no `open-floating` rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenHint {
    Tiled,
    /// A dialog of a parent window, or a fixed-height window.
    Floating,
    /// A window with no parent that caps its size on both axes within the output it opens on,
    /// its height not fixed, as Chromium's picture-in-picture windows do (at 80% of the display):
    /// it floats in a corner at its own size and takes the focus only when asked. A cap larger
    /// than the output, such as Firefox's (8102 or 13017 px on each axis), limits nothing and the
    /// window tiles.
    Overlay,
}

impl OpenHint {
    /// The hint for a window with or without a parent, its minimum and maximum size, opening on
    /// an output of logical size `output`.
    pub fn compute(
        has_parent: bool,
        min: (i32, i32),
        max: (i32, i32),
        output: Option<(i32, i32)>,
    ) -> Self {
        if has_parent || (min.1 > 0 && min.1 == max.1) {
            Self::Floating
        } else if cap_fits(max, output) {
            Self::Overlay
        } else {
            Self::Tiled
        }
    }
}

/// Whether maximum size `max` caps both axes within `output`, the logical size of the output a
/// window opens on. Without a known output no cap counts.
pub fn cap_fits(max: (i32, i32), output: Option<(i32, i32)>) -> bool {
    max.0 > 0 && max.1 > 0 && output.is_some_and(|o| max.0 <= o.0 && max.1 <= o.1)
}

#[cfg(test)]
mod tests {
    use niri_config::Config;

    use super::*;

    fn rules(text: &str) -> Vec<WindowRule> {
        Config::parse_mem(text).unwrap().window_rules
    }

    fn info<'a>(app_id: &'a str, title: &'a str) -> RuleInfo<'a> {
        RuleInfo {
            app_id: Some(app_id),
            title: Some(title),
            ..RuleInfo::default()
        }
    }

    #[test]
    fn open_hints() {
        let hint = |parent, min, max| OpenHint::compute(parent, min, max, Some((1920, 1080)));
        assert_eq!(hint(false, (0, 0), (0, 0)), OpenHint::Tiled);
        assert_eq!(hint(true, (0, 0), (0, 0)), OpenHint::Floating);
        assert_eq!(hint(false, (100, 50), (0, 50)), OpenHint::Floating);
        assert_eq!(hint(false, (0, 0), (1536, 864)), OpenHint::Overlay);
        assert_eq!(hint(false, (200, 0), (1536, 0)), OpenHint::Tiled);
    }

    /// A cap counts only within the output: Chromium's picture-in-picture caps
    /// itself at 80% of the display and stays an overlay; Firefox's cap is far larger than any
    /// output and limits nothing, so it tiles.
    #[test]
    fn a_cap_counts_only_within_the_output() {
        let fhd = Some((1920, 1080));
        let wide = Some((4096, 1152));
        let hint = OpenHint::compute;
        assert_eq!(
            hint(false, (500, 120), (8102, 8102), fhd),
            OpenHint::Tiled,
            "Firefox"
        );
        assert_eq!(
            hint(false, (500, 120), (13017, 13017), wide),
            OpenHint::Tiled
        );
        assert_eq!(
            hint(false, (240, 52), (1536, 864), fhd),
            OpenHint::Overlay,
            "PiP, 80%"
        );
        assert_eq!(hint(false, (248, 90), (3277, 922), wide), OpenHint::Overlay);
        assert_eq!(
            hint(false, (240, 52), (1920, 1080), fhd),
            OpenHint::Overlay,
            "whole"
        );
        assert_eq!(
            hint(false, (240, 52), (1536, 1081), fhd),
            OpenHint::Tiled,
            "one axis past"
        );
        assert_eq!(
            hint(false, (240, 52), (1536, 864), None),
            OpenHint::Tiled,
            "no output"
        );
        assert_eq!(
            hint(false, (812, 124), (812, 124), fhd),
            OpenHint::Floating,
            "fixed height"
        );
        assert_eq!(
            hint(false, (400, 300), (9000, 300), fhd),
            OpenHint::Floating
        );
        assert_eq!(
            hint(true, (0, 0), (8102, 8102), fhd),
            OpenHint::Floating,
            "a child"
        );
        assert!(cap_fits((1536, 864), fhd));
        assert!(!cap_fits((1536, 0), fhd));
        assert!(!cap_fits((8102, 8102), fhd));
    }

    #[test]
    fn later_rules_win_and_excludes_apply() {
        let rules = rules(
            r#"
            window-rule {
                match app-id="(?i)polkit"
                match app-id="(?i)gauge"
                open-floating true
            }
            window-rule {
                match app-id="(?i)gauge" title="^Gauge HUD"
                open-focused false
                draw-border-with-background false
                border { off; }
                default-floating-position x=36 y=82 relative-to="bottom-right"
            }
            window-rule {
                match is-focused=false
                exclude title="^Picture.in.[Pp]icture$"
                opacity 0.9
            }
            "#,
        );
        let base = Style {
            width: 1,
            ..Style::default()
        };
        let hud = resolve(&rules, &info("gauge", "Gauge HUD"), base);
        assert_eq!(hud.open_floating, Some(true));
        assert_eq!(hud.open_focused, Some(false));
        assert_eq!(hud.border.width, 0);
        assert!(!hud.behind, "nothing drawn behind the HUD");
        assert_eq!(
            hud.floating_position,
            Some(Corner {
                x: 36.,
                y: 82.,
                h: Align::End,
                v: Align::End
            })
        );
        let term = resolve(&rules, &info("foot", "fish"), base);
        assert_eq!(term.open_floating, None);
        assert_eq!(term.border.width, 1);
        assert!(term.behind);
        assert_eq!(opacity(&rules, &info("foot", "fish")), 0.9);
        assert_eq!(opacity(&rules, &info("chromium", "Picture in picture")), 1.);
        let focused = RuleInfo {
            is_focused: true,
            ..info("foot", "fish")
        };
        assert_eq!(opacity(&rules, &focused), 1.);
    }

    #[test]
    fn corner_places() {
        assert_eq!(Corner::place(Align::End, 10., 0, 1920, 400), 1510);
        assert_eq!(Corner::place(Align::Start, 10., 100, 1920, 400), 110);
        assert_eq!(Corner::place(Align::Middle, 0., 0, 1000, 400), 300);
    }
}
