//! The overview's geometry: the camera that fits the whole canvas on the screen, and the dim drawn
//! over everything outside the screen area the desktop shows. Rectangles here are `(x, y, w, h)` in
//! 1:1 canvas pixels unless said otherwise.

use crate::anim::Camera;
use crate::model::Rect;

pub type Area = (f64, f64, f64, f64);

/// The share of the screen the fitted canvas may take, leaving a margin around it.
pub const FIT_MARGIN: f64 = 0.94;

/// The smallest rectangle holding every one given.
pub fn union(rects: impl IntoIterator<Item = Area>) -> Option<Area> {
    rects.into_iter().fold(None, |acc, (x, y, w, h)| {
        Some(match acc {
            None => (x, y, w, h),
            Some((ax, ay, aw, ah)) => {
                let (x0, y0) = (ax.min(x), ay.min(y));
                let (x1, y1) = ((ax + aw).max(x + w), (ay + ah).max(y + h));
                (x0, y0, x1 - x0, y1 - y0)
            }
        })
    })
}

/// The canvas area a camera shows on a screen of this size.
pub fn shown_area(cam: Camera, screen: (f64, f64)) -> Area {
    (
        cam.view.0 / cam.scale,
        cam.view.1 / cam.scale,
        screen.0 / cam.scale,
        screen.1 / cam.scale,
    )
}

/// The camera that shows all of `bounds` centred on the screen, at a scale between `min_scale`
/// (the furthest `zoom-max` allows) and `max_scale` (the overview never brings the camera
/// closer than it was).
pub fn fit(bounds: Area, screen: (f64, f64), min_scale: f64, max_scale: f64) -> Camera {
    let (x, y, w, h) = bounds;
    let s = (screen.0 / w.max(1.))
        .min(screen.1 / h.max(1.))
        .mul_add(FIT_MARGIN, 0.)
        .min(max_scale)
        .max(min_scale);
    Camera::at_centre(s, (x + w / 2., y + h / 2.), screen)
}

/// A canvas area on screen through a camera, `origin` being the screen's top-left in the global
/// space.
pub fn on_screen(area: Area, cam: Camera, origin: (f64, f64)) -> Rect {
    let x0 = origin.0 + area.0 * cam.scale - cam.view.0;
    let y0 = origin.1 + area.1 * cam.scale - cam.view.1;
    let x1 = x0 + area.2 * cam.scale;
    let y1 = y0 + area.3 * cam.scale;
    Rect::new(
        x0.round() as i32,
        y0.round() as i32,
        (x1.round() - x0.round()) as i32,
        (y1.round() - y0.round()) as i32,
    )
}

/// What of `output` lies outside `display`: up to four bands (above, below, left, right), with
/// no box drawn around the display area itself.
pub fn bands(output: Rect, display: Rect) -> Vec<Rect> {
    let Some(d) = display.intersect(output) else {
        return vec![output];
    };
    let (ox1, oy1) = (output.x + output.w, output.y + output.h);
    let (dx1, dy1) = (d.x + d.w, d.y + d.h);
    [
        Rect::new(output.x, output.y, output.w, d.y - output.y),
        Rect::new(output.x, dy1, output.w, oy1 - dy1),
        Rect::new(output.x, d.y, d.x - output.x, d.h),
        Rect::new(dx1, d.y, ox1 - dx1, d.h),
    ]
    .into_iter()
    .filter(|r| r.w > 0 && r.h > 0)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: (f64, f64) = (1000., 500.);

    #[test]
    fn the_union_holds_every_rectangle() {
        assert_eq!(union([]), None);
        assert_eq!(
            union([(0., 0., 10., 10.), (-5., 20., 10., 5.)]),
            Some((-5., 0., 15., 25.))
        );
    }

    #[test]
    fn the_fit_shows_the_whole_canvas_centred() {
        let bounds = (-1000., 0., 4000., 1000.);
        let cam = fit(bounds, SCREEN, 0.01, 1.);
        assert!((cam.scale - 0.25 * FIT_MARGIN).abs() < 1e-9, "{cam:?}");
        let r = on_screen(bounds, cam, (0., 0.));
        assert!(r.x >= 0 && r.y >= 0, "{r:?}");
        assert!(r.x + r.w <= 1000 && r.y + r.h <= 500, "{r:?}");
        assert!(((r.x + r.w / 2) - 500).abs() <= 1 && ((r.y + r.h / 2) - 250).abs() <= 1);
    }

    #[test]
    fn the_fit_respects_zoom_max_and_never_comes_closer() {
        let huge = (0., 0., 100_000., 100.);
        assert_eq!(fit(huge, SCREEN, 0.2, 1.).scale, 0.2);
        let small = (0., 0., 100., 100.);
        assert_eq!(fit(small, SCREEN, 0.2, 0.8).scale, 0.8);
    }

    #[test]
    fn the_display_area_maps_through_the_camera() {
        let home = Camera {
            scale: 1.,
            view: (200., 0.),
        };
        let shown = shown_area(home, SCREEN);
        assert_eq!(shown, (200., 0., 1000., 500.));
        let half = Camera {
            scale: 0.5,
            view: (-150., -125.),
        };
        assert_eq!(
            on_screen(shown, half, (10., 20.)),
            Rect::new(260, 145, 500, 250)
        );
    }

    #[test]
    fn bands_cover_the_output_outside_the_display_and_nothing_else() {
        let out = Rect::new(0, 0, 100, 50);
        let disp = Rect::new(20, 10, 30, 20);
        let b = bands(out, disp);
        assert_eq!(b.len(), 4);
        let area: i32 = b.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, 100 * 50 - 30 * 20);
        for r in &b {
            assert!(r.intersect(disp).is_none(), "{r:?} overlaps the display");
            assert_eq!(r.intersect(out), Some(*r));
        }
        for i in 0..b.len() {
            for j in i + 1..b.len() {
                assert!(b[i].intersect(b[j]).is_none());
            }
        }
    }

    #[test]
    fn a_display_at_an_edge_or_off_the_output_gives_fewer_or_one_band() {
        let out = Rect::new(0, 0, 100, 50);
        assert_eq!(bands(out, Rect::new(0, 0, 100, 50)), vec![]);
        assert_eq!(
            bands(out, Rect::new(0, 0, 60, 50)),
            vec![Rect::new(60, 0, 40, 50)]
        );
        assert_eq!(bands(out, Rect::new(200, 0, 10, 10)), vec![out]);
    }
}
