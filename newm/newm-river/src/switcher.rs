//! The Alt+Tab switcher: which window is chosen, and the panel that shows the choice as
//! thumbnails with app icons and no text, in most-recently-used order.
//!
//! Alt+Tab opens it on the previous window and steps forward, Alt+Shift+Tab opens it on the
//! least recent one and steps back, releasing Alt confirms and Alt+Escape cancels. The panel
//! shows only after a short delay, so a quick Alt+Tab switches without flashing it.

use std::time::{Duration, Instant};

use newm_canvas::Id;

use crate::draw::{fit, Colour, Pixmap};

/// How long Alt must be held before the panel shows (niri's `open-delay-ms` default).
pub const OPEN_DELAY: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Next,
    Prev,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Switcher {
    list: Vec<Id>,
    selected: usize,
    opened: Instant,
}

impl Switcher {
    /// Opens on the windows in MRU order; None when there are none. A forward open selects the
    /// previous window (the second), a backward one the least recent.
    pub fn open(mru: Vec<Id>, step: Step, now: Instant) -> Option<Self> {
        if mru.is_empty() {
            return None;
        }
        let selected = match step {
            Step::Next => usize::from(mru.len() > 1),
            Step::Prev => mru.len() - 1,
        };
        Some(Self {
            list: mru,
            selected,
            opened: now,
        })
    }

    pub fn list(&self) -> &[Id] {
        &self.list
    }

    pub fn index(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> Id {
        self.list[self.selected]
    }

    /// Moves the choice one window, wrapping around.
    pub fn step(&mut self, step: Step) {
        let n = self.list.len();
        self.selected = match step {
            Step::Next => (self.selected + 1) % n,
            Step::Prev => (self.selected + n - 1) % n,
        };
    }

    /// A window closed while the switcher is open. Returns false when none is left.
    pub fn remove(&mut self, id: Id) -> bool {
        if let Some(k) = self.list.iter().position(|&w| w == id) {
            self.list.remove(k);
            if k < self.selected || self.selected >= self.list.len() {
                self.selected = self.selected.saturating_sub(1);
            }
        }
        !self.list.is_empty()
    }

    /// When the panel shows.
    pub fn shows_at(&self, delay: Duration) -> Instant {
        self.opened + delay
    }
}

/// Where each window's thumbnail goes in the panel, and the panel's size.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub size: (i32, i32),
    /// `(x, y, w, h)` of each cell, in the switcher's order.
    pub cells: Vec<(i32, i32, i32, i32)>,
}

pub const PAD: i32 = 24;
pub const GAP: i32 = 20;
/// The highlight around the chosen cell.
pub const RING: i32 = 6;

/// Lays out cells for windows of these sizes in rows centred in a panel no larger than nine
/// tenths of `area`; cells shrink until every row fits.
pub fn layout(sizes: &[(i32, i32)], area: (i32, i32)) -> Layout {
    let max_w = (f64::from(area.0) * 0.9) as i32 - 2 * PAD;
    let max_h = (f64::from(area.1) * 0.9) as i32 - 2 * PAD;
    let mut h = (f64::from(area.1) * 0.18).clamp(60., 220.) as i32;
    loop {
        let widths: Vec<i32> = sizes
            .iter()
            .map(|&(w, wh)| {
                let aspect = if w > 0 && wh > 0 {
                    f64::from(w) / f64::from(wh)
                } else {
                    16. / 10.
                };
                (f64::from(h) * aspect.clamp(0.6, 2.2)).round() as i32
            })
            .collect();
        let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
        let mut row_w = 0;
        for (k, &w) in widths.iter().enumerate() {
            let need = if row_w == 0 { w } else { row_w + GAP + w };
            if need > max_w && row_w > 0 {
                rows.push(vec![k]);
                row_w = w;
            } else {
                rows.last_mut().unwrap().push(k);
                row_w = need;
            }
        }
        let n = rows.len() as i32;
        let total_h = n * h + (n - 1) * GAP;
        let fits = total_h <= max_h && widths.iter().all(|&w| w <= max_w);
        if fits || h <= 24 {
            let row_widths: Vec<i32> = rows
                .iter()
                .map(|r| r.iter().map(|&k| widths[k]).sum::<i32>() + GAP * (r.len() as i32 - 1))
                .collect();
            let inner_w = row_widths.iter().copied().max().unwrap_or(0).max(h);
            let mut cells = vec![(0, 0, 0, 0); sizes.len()];
            for (r, row) in rows.iter().enumerate() {
                let mut x = PAD + (inner_w - row_widths[r]) / 2;
                let y = PAD + r as i32 * (h + GAP);
                for &k in row {
                    cells[k] = (x, y, widths[k], h);
                    x += widths[k] + GAP;
                }
            }
            return Layout {
                size: (inner_w + 2 * PAD, total_h.max(0) + 2 * PAD),
                cells,
            };
        }
        h = (f64::from(h) * 0.85) as i32;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub backdrop: Colour,
    pub highlight: Colour,
    pub placeholder: Colour,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            backdrop: [0., 0., 0., 0.8],
            highlight: [0.51, 0.98, 0.61, 1.],
            placeholder: [0.16, 0.16, 0.18, 1.],
        }
    }
}

/// The icon's side for a cell height.
pub fn icon_side(cell_h: i32) -> i32 {
    (cell_h / 3).clamp(16, 64)
}

/// Draws the panel: the backdrop, a highlight around the chosen cell, each window's thumbnail
/// (or a plain tile until it arrives) and its app's icon at the bottom of the cell. No text.
pub fn draw(
    layout: &Layout,
    selected: usize,
    thumbs: &[Option<&Pixmap>],
    icons: &[Option<&Pixmap>],
    style: &Style,
) -> Pixmap {
    let (pw, ph) = layout.size;
    let mut p = Pixmap::new(pw, ph);
    p.fill_rounded((0., 0., f64::from(pw), f64::from(ph)), 16., style.backdrop);
    for (k, &(x, y, w, h)) in layout.cells.iter().enumerate() {
        if k == selected {
            p.fill_rounded(
                (
                    f64::from(x - RING),
                    f64::from(y - RING),
                    f64::from(w + 2 * RING),
                    f64::from(h + 2 * RING),
                ),
                12.,
                style.highlight,
            );
        }
        match thumbs.get(k).copied().flatten() {
            Some(t) => {
                let (tw, th) = fit((t.width, t.height), (w, h));
                p.fill_rounded(
                    (f64::from(x), f64::from(y), f64::from(w), f64::from(h)),
                    8.,
                    style.backdrop,
                );
                p.draw(t, (x + (w - tw) / 2, y + (h - th) / 2, tw, th), 6.);
            }
            None => p.fill_rounded(
                (f64::from(x), f64::from(y), f64::from(w), f64::from(h)),
                8.,
                style.placeholder,
            ),
        }
        if let Some(icon) = icons.get(k).copied().flatten() {
            let side = icon_side(h);
            let (iw, ih) = fit((icon.width, icon.height), (side, side));
            let (ix, iy) = (x + (w - iw) / 2, y + h - ih - 6);
            p.draw(icon, (ix, iy, iw, ih), 0.);
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(n: u64, step: Step) -> Switcher {
        Switcher::open((1..=n).collect(), step, Instant::now()).unwrap()
    }

    #[test]
    fn alt_tab_opens_on_the_previous_window_and_steps_forward() {
        let mut s = open(3, Step::Next);
        assert_eq!(s.selected(), 2);
        s.step(Step::Next);
        assert_eq!(s.selected(), 3);
        s.step(Step::Next);
        assert_eq!(s.selected(), 1);
        s.step(Step::Prev);
        assert_eq!(s.selected(), 3);
    }

    #[test]
    fn alt_shift_tab_opens_on_the_least_recent_window() {
        let mut s = open(3, Step::Prev);
        assert_eq!(s.selected(), 3);
        s.step(Step::Prev);
        assert_eq!(s.selected(), 2);
    }

    #[test]
    fn one_window_selects_itself_and_none_opens_nothing() {
        assert_eq!(open(1, Step::Next).selected(), 1);
        assert_eq!(Switcher::open(vec![], Step::Next, Instant::now()), None);
    }

    #[test]
    fn a_window_closing_keeps_the_choice_on_a_window_still_there() {
        let mut s = open(4, Step::Next);
        s.step(Step::Next);
        assert_eq!(s.selected(), 3);
        assert!(s.remove(1));
        assert_eq!(s.selected(), 3);
        assert!(s.remove(3));
        assert!(s.list().contains(&s.selected()));
        assert!(s.remove(2));
        assert!(!s.remove(4));
    }

    #[test]
    fn the_panel_shows_after_the_delay() {
        let now = Instant::now();
        let s = Switcher::open(vec![1, 2], Step::Next, now).unwrap();
        assert_eq!(s.shows_at(OPEN_DELAY), now + OPEN_DELAY);
    }

    fn disjoint(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
        a.0 + a.2 + GAP <= b.0 || b.0 + b.2 + GAP <= a.0 || a.1 + a.3 <= b.1 || b.1 + b.3 <= a.1
    }

    #[test]
    fn cells_fit_the_panel_without_touching() {
        let sizes = vec![(1280, 720); 14];
        let l = layout(&sizes, (1280, 720));
        assert!(l.size.0 <= 1152 && l.size.1 <= 648, "{:?}", l.size);
        for (k, &c) in l.cells.iter().enumerate() {
            assert!(c.0 >= PAD && c.1 >= PAD, "{c:?}");
            assert!(
                c.0 + c.2 <= l.size.0 - PAD && c.1 + c.3 <= l.size.1 - PAD,
                "{c:?}"
            );
            for &d in &l.cells[k + 1..] {
                assert!(disjoint(c, d), "{c:?} touches {d:?}");
            }
        }
    }

    #[test]
    fn a_few_windows_make_one_centred_row() {
        let l = layout(&[(800, 600), (1600, 900)], (1920, 1080));
        assert_eq!(l.cells[0].1, l.cells[1].1);
        let right = l.size.0 - (l.cells[1].0 + l.cells[1].2);
        assert!((l.cells[0].0 - right).abs() <= 1);
    }

    #[test]
    fn the_chosen_cell_is_ringed_and_nothing_else_is() {
        let l = layout(&[(400, 300), (400, 300)], (1280, 720));
        let style = Style::default();
        let p = draw(&l, 1, &[None, None], &[None, None], &style);
        let ring = |c: (i32, i32, i32, i32)| p.get(c.0 + c.2 / 2, c.1 - RING / 2);
        let green = ring(l.cells[1]);
        assert!((green >> 8) & 0xff > 0xc0, "{green:08x}");
        let dark = ring(l.cells[0]);
        assert!((dark >> 8) & 0xff < 0x20, "{dark:08x}");
    }

    #[test]
    fn thumbnails_and_icons_are_drawn_in_their_cell() {
        let l = layout(&[(400, 300)], (1280, 720));
        let red = Pixmap {
            width: 4,
            height: 3,
            px: vec![0xffff0000; 12],
        };
        let blue = Pixmap {
            width: 2,
            height: 2,
            px: vec![0xff0000ff; 4],
        };
        let p = draw(&l, 0, &[Some(&red)], &[Some(&blue)], &Style::default());
        let (x, y, w, h) = l.cells[0];
        assert_eq!(p.get(x + w / 2, y + h / 4), 0xffff0000);
        assert_eq!(p.get(x + w / 2, y + h - 6 - icon_side(h) / 2), 0xff0000ff);
    }
}
