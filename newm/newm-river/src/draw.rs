//! Software drawing for the window manager's own surfaces: premultiplied ARGB pixmaps (wl_shm's
//! ARGB8888 in memory), antialiased rounded rectangles and images scaled into a rectangle with an
//! area-averaging filter (thumbnails, icons).

use crate::capture::Image;

/// Premultiplied ARGB pixels, `0xAARRGGBB`, rows without padding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub px: Vec<u32>,
}

/// A premultiplied colour, each channel 0..1.
pub type Colour = [f32; 4];

fn pack(c: [f32; 4]) -> u32 {
    let ch = |v: f32| (v.clamp(0., 1.) * 255. + 0.5) as u32;
    (ch(c[3]) << 24) | (ch(c[0]) << 16) | (ch(c[1]) << 8) | ch(c[2])
}

fn unpack(p: u32) -> [f32; 4] {
    let ch = |s: u32| ((p >> s) & 0xff) as f32 / 255.;
    [ch(16), ch(8), ch(0), ch(24)]
}

/// `src` over `dst`, both premultiplied.
fn over(dst: u32, src: [f32; 4]) -> u32 {
    if src[3] <= 0. && src[0] <= 0. && src[1] <= 0. && src[2] <= 0. {
        return dst;
    }
    let d = unpack(dst);
    let k = 1. - src[3];
    pack([
        src[0] + d[0] * k,
        src[1] + d[1] * k,
        src[2] + d[2] * k,
        src[3] + d[3] * k,
    ])
}

/// How much of the pixel centred at `(px, py)` a rounded rectangle covers, 0..1.
fn coverage(px: f64, py: f64, (x, y, w, h): (f64, f64, f64, f64), radius: f64) -> f32 {
    let r = radius.min(w / 2.).min(h / 2.).max(0.);
    let (cx, cy) = (x + w / 2., y + h / 2.);
    let qx = (px - cx).abs() - (w / 2. - r);
    let qy = (py - cy).abs() - (h / 2. - r);
    let outside = qx.max(0.).hypot(qy.max(0.));
    let d = outside + qx.max(qy).min(0.) - r;
    (0.5 - d).clamp(0., 1.) as f32
}

/// The size an image of `size` takes to fit in `room`, keeping its shape.
pub fn fit(size: (i32, i32), room: (i32, i32)) -> (i32, i32) {
    if size.0 <= 0 || size.1 <= 0 {
        return (0, 0);
    }
    let k = (f64::from(room.0) / f64::from(size.0)).min(f64::from(room.1) / f64::from(size.1));
    (
        ((f64::from(size.0) * k).round() as i32).max(1),
        ((f64::from(size.1) * k).round() as i32).max(1),
    )
}

impl Pixmap {
    pub fn new(width: i32, height: i32) -> Self {
        let (width, height) = (width.max(0), height.max(0));
        Self {
            width,
            height,
            px: vec![0; width as usize * height as usize],
        }
    }

    /// From captured RGBA, premultiplied already.
    pub fn from_rgba(img: &Image) -> Self {
        let px = img
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                (u32::from(p[3]) << 24)
                    | (u32::from(p[0]) << 16)
                    | (u32::from(p[1]) << 8)
                    | u32::from(p[2])
            })
            .collect();
        Self {
            width: img.width as i32,
            height: img.height as i32,
            px,
        }
    }

    /// From straight (not premultiplied) RGBA, as PNG files hold it.
    pub fn from_straight_rgba(width: u32, height: u32, rgba: &[u8]) -> Self {
        let px = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                let a = u32::from(p[3]);
                let m = |c: u8| (u32::from(c) * a + 127) / 255;
                (a << 24) | (m(p[0]) << 16) | (m(p[1]) << 8) | m(p[2])
            })
            .collect();
        Self {
            width: width as i32,
            height: height as i32,
            px,
        }
    }

    #[cfg(test)]
    pub fn get(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return 0;
        }
        self.px[(y * self.width + x) as usize]
    }

    /// The bytes of a wl_shm ARGB8888 buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.px.iter().flat_map(|p| p.to_le_bytes()).collect()
    }

    /// The pixels a rectangle touches, clipped to the pixmap.
    fn span(&self, (x, y, w, h): (f64, f64, f64, f64)) -> (i32, i32, i32, i32) {
        (
            (x.floor() as i32).max(0),
            (y.floor() as i32).max(0),
            ((x + w).ceil() as i32).min(self.width),
            ((y + h).ceil() as i32).min(self.height),
        )
    }

    /// Fills a rounded rectangle, antialiased.
    pub fn fill_rounded(&mut self, rect: (f64, f64, f64, f64), radius: f64, colour: Colour) {
        let (x0, y0, x1, y1) = self.span(rect);
        for y in y0..y1 {
            for x in x0..x1 {
                let c = coverage(f64::from(x) + 0.5, f64::from(y) + 0.5, rect, radius);
                if c > 0. {
                    let i = (y * self.width + x) as usize;
                    self.px[i] = over(self.px[i], colour.map(|v| v * c));
                }
            }
        }
    }

    /// Fills the ring between two rounded rectangles, `inner` within `outer`, antialiased.
    pub fn fill_ring(
        &mut self,
        outer: (f64, f64, f64, f64),
        outer_radius: f64,
        inner: (f64, f64, f64, f64),
        inner_radius: f64,
        colour: Colour,
    ) {
        let (x0, y0, x1, y1) = self.span(outer);
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                let c =
                    coverage(px, py, outer, outer_radius) - coverage(px, py, inner, inner_radius);
                if c > 0. {
                    let i = (y * self.width + x) as usize;
                    self.px[i] = over(self.px[i], colour.map(|v| v * c));
                }
            }
        }
    }

    /// Draws `src` scaled into a rectangle with rounded corners: each pixel the average of the
    /// source pixels under it.
    pub fn draw(&mut self, src: &Pixmap, rect: (i32, i32, i32, i32), radius: f64) {
        let (rx, ry, rw, rh) = rect;
        if src.width <= 0 || src.height <= 0 || rw <= 0 || rh <= 0 {
            return;
        }
        let area = (f64::from(rx), f64::from(ry), f64::from(rw), f64::from(rh));
        let (x0, y0, x1, y1) = self.span(area);
        // The source pixels under destination pixel `k` of `len` (k >= 0, both lengths > 0).
        let edges = |k: i32, len: i32, src_len: i32| {
            let (k, len, src) = (k.max(0) as u64, len as u64, src_len as u64);
            let a = ((k * src / len) as i32).min(src_len - 1);
            let b = ((k + 1) * src).div_ceil(len) as i32;
            (a, b.clamp(a + 1, src_len))
        };
        for y in y0..y1 {
            let (sy0, sy1) = edges(y - ry, rh, src.height);
            for x in x0..x1 {
                let c = coverage(f64::from(x) + 0.5, f64::from(y) + 0.5, area, radius);
                if c <= 0. {
                    continue;
                }
                let (sx0, sx1) = edges(x - rx, rw, src.width);
                let mut sum = [0f32; 4];
                for sy in sy0..sy1 {
                    for sx in sx0..sx1 {
                        let p = unpack(src.px[(sy * src.width + sx) as usize]);
                        for (s, v) in sum.iter_mut().zip(p) {
                            *s += v;
                        }
                    }
                }
                let n = ((sy1 - sy0) * (sx1 - sx0)) as f32;
                let i = (y * self.width + x) as usize;
                self.px[i] = over(self.px[i], sum.map(|v| v / n * c));
            }
        }
    }

    /// A copy scaled to fit in `room`, keeping its shape; never larger than it is.
    pub fn scaled_to_fit(&self, room: (i32, i32)) -> Pixmap {
        let (w, h) = fit((self.width, self.height), room);
        if w >= self.width || h >= self.height {
            return self.clone();
        }
        let mut out = Pixmap::new(w, h);
        out.draw(self, (0, 0, w, h), 0.);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_pack_as_argb() {
        assert_eq!(pack([1., 0., 0., 1.]), 0xffff0000);
        assert_eq!(pack([0., 0., 0.5, 0.5]), 0x80000080);
        let p = Pixmap {
            width: 1,
            height: 1,
            px: vec![0x11223344],
        };
        assert_eq!(p.to_bytes(), vec![0x44, 0x33, 0x22, 0x11]);
    }

    #[test]
    fn captured_and_png_pixels_convert() {
        let img = Image {
            width: 1,
            height: 1,
            pixels: vec![10, 20, 30, 40],
        };
        assert_eq!(Pixmap::from_rgba(&img).px, vec![0x280a141e]);
        let png = Pixmap::from_straight_rgba(1, 1, &[255, 0, 0, 128]);
        assert_eq!(png.px, vec![0x80800000]);
    }

    #[test]
    fn a_rounded_fill_covers_the_middle_and_spares_the_corner() {
        let mut p = Pixmap::new(20, 20);
        p.fill_rounded((0., 0., 20., 20.), 8., [0., 0., 0., 1.]);
        assert_eq!(p.get(10, 10), 0xff000000);
        assert_eq!(p.get(0, 10), 0xff000000);
        assert_eq!(p.get(0, 0), 0);
        assert!(p.get(1, 1) >> 24 < 0x80);
    }

    #[test]
    fn a_ring_covers_its_band_and_spares_the_middle() {
        let mut p = Pixmap::new(20, 20);
        let c = [1., 0., 0., 1.];
        p.fill_ring((0., 0., 20., 20.), 0., (2., 2., 16., 16.), 0., c);
        assert_eq!(p.get(0, 10), 0xffff0000);
        assert_eq!(p.get(1, 1), 0xffff0000);
        assert_eq!(p.get(10, 10), 0);
        assert_eq!(p.get(2, 10), 0);
    }

    #[test]
    fn translucent_fills_blend() {
        let mut p = Pixmap::new(1, 1);
        p.fill_rounded((0., 0., 1., 1.), 0., [1., 1., 1., 1.]);
        p.fill_rounded((0., 0., 1., 1.), 0., [0., 0., 0., 0.4]);
        assert_eq!(p.get(0, 0), pack([0.6, 0.6, 0.6, 1.]));
    }

    #[test]
    fn downscaling_averages_the_pixels_under_each() {
        let src = Pixmap {
            width: 2,
            height: 2,
            px: vec![0xff000000, 0xffffffff, 0xff000000, 0xffffffff],
        };
        let small = src.scaled_to_fit((1, 1));
        assert_eq!((small.width, small.height), (1, 1));
        let [r, g, b, a] = unpack(small.px[0]);
        assert!((r - 0.5).abs() < 0.01 && (g - 0.5).abs() < 0.01 && (b - 0.5).abs() < 0.01);
        assert_eq!(a, 1.);
    }

    #[test]
    fn upscaling_repeats_pixels_and_fit_keeps_the_shape() {
        let src = Pixmap {
            width: 2,
            height: 1,
            px: vec![0xffff0000, 0xff0000ff],
        };
        let mut dst = Pixmap::new(4, 2);
        dst.draw(&src, (0, 0, 4, 2), 0.);
        assert_eq!(
            dst.px,
            [0xffff0000, 0xffff0000, 0xff0000ff, 0xff0000ff].repeat(2)
        );
        assert_eq!(fit((1920, 1080), (320, 320)), (320, 180));
        assert_eq!(fit((100, 400), (320, 200)), (50, 200));
        assert_eq!(src.scaled_to_fit((100, 100)), src);
    }
}
