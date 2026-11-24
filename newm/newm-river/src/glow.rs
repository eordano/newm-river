//! The soft glow around the focused window (a shadow in the accent colour): a buffer for
//! a decoration surface below the window, its alpha a Gaussian blur of the window's rounded
//! rectangle, clear under the window itself.

use crate::model::GlowStyle;

/// An ARGB8888 buffer (little-endian: B, G, R, A, premultiplied) and where it sits relative to
/// the window's top-left.
#[derive(Clone, Debug, PartialEq)]
pub struct Buffer {
    pub width: i32,
    pub height: i32,
    pub offset: (i32, i32),
    pub pixels: Vec<u8>,
}

/// How far the glow reaches past the window's edges: three standard deviations of its blur.
pub fn reach(g: &GlowStyle) -> i32 {
    (g.softness * 1.5 + g.spread.max(0.) + g.offset.0.abs().max(g.offset.1.abs())).ceil() as i32 + 1
}

/// The complementary error function (Abramowitz and Stegun 7.1.26, error below 1.5e-7).
fn erfc(x: f64) -> f64 {
    if x < 0. {
        return 2. - erfc(-x);
    }
    let t = 1. / (1. + 0.327_591_1 * x);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    poly * (-x * x).exp()
}

/// Signed distance from `(px, py)` to a rounded rectangle centred at `(cx, cy)`.
fn rounded_rect_distance(p: (f64, f64), c: (f64, f64), half: (f64, f64), radius: f64) -> f64 {
    let r = radius.min(half.0).min(half.1).max(0.);
    let qx = (p.0 - c.0).abs() - (half.0 - r);
    let qy = (p.1 - c.1).abs() - (half.1 - r);
    let outside = (qx.max(0.).powi(2) + qy.max(0.).powi(2)).sqrt();
    outside + qx.max(qy).min(0.) - r
}

/// The glow for a window of `size` with a border of `border` px and corners of `radius`.
pub fn render(g: &GlowStyle, size: (i32, i32), border: i32, radius: f64) -> Buffer {
    let pad = reach(g) + border;
    let (w, h) = (size.0 + 2 * pad, size.1 + 2 * pad);
    let sigma = (g.softness / 2.).max(0.5);
    let half = (
        f64::from(size.0) / 2. + f64::from(border) + g.spread,
        f64::from(size.1) / 2. + f64::from(border) + g.spread,
    );
    let centre = (
        f64::from(pad) + f64::from(size.0) / 2. + g.offset.0,
        f64::from(pad) + f64::from(size.1) / 2. + g.offset.1,
    );
    let window = (
        pad - border,
        pad - border,
        pad + size.0 + border,
        pad + size.1 + border,
    );
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            if x >= window.0 && x < window.2 && y >= window.1 && y < window.3 {
                continue;
            }
            let p = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let d = rounded_rect_distance(p, centre, half, radius + f64::from(border));
            let cover = 0.5 * erfc(d / (sigma * std::f64::consts::SQRT_2));
            if cover < 1. / 512. {
                continue;
            }
            let k = ((y * w + x) * 4) as usize;
            let c = |v: f32| (f64::from(v) * cover * 255.).round().clamp(0., 255.) as u8;
            pixels[k] = c(g.color[2]);
            pixels[k + 1] = c(g.color[1]);
            pixels[k + 2] = c(g.color[0]);
            pixels[k + 3] = c(g.color[3]);
        }
    }
    Buffer {
        width: w,
        height: h,
        offset: (-pad, -pad),
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> GlowStyle {
        GlowStyle {
            softness: 26.,
            spread: 0.,
            offset: (0., 0.),
            color: [0.5, 0.98, 0.61, 1.],
        }
    }

    fn alpha(b: &Buffer, x: i32, y: i32) -> u8 {
        b.pixels[((y * b.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_glow_fades_out_from_the_window_edge() {
        let b = render(&style(), (200, 100), 1, 8.);
        let pad = -b.offset.0;
        assert_eq!((b.width, b.height), (200 + 2 * pad, 100 + 2 * pad));
        // Under the window and its border: clear.
        assert_eq!(alpha(&b, pad + 100, pad + 50), 0);
        assert_eq!(alpha(&b, pad - 1, pad + 50), 0);
        // Just outside the border, about half; fading to nothing at the buffer's edge.
        let near = alpha(&b, pad - 2, pad + 50);
        assert!((110..=135).contains(&near), "{near}");
        let mid = alpha(&b, pad - 14, pad + 50);
        assert!(mid < near && mid > 0);
        assert_eq!(alpha(&b, 0, pad + 50), 0);
    }

    #[test]
    fn erfc_is_accurate() {
        assert!((erfc(0.) - 1.).abs() < 1e-6);
        assert!((erfc(1.) - 0.157_299_2).abs() < 1e-6);
        assert!((erfc(-1.) - 1.842_700_8).abs() < 1e-6);
    }
}
