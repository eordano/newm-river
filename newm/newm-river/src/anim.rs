//! The camera and how it moves: every zoom and pan animates about the middle of the screen.
//!
//! A [`Camera`] is a scale and a view (the top-left of the screen in on-screen pixels at that
//! scale, newm-canvas's convention). Between two cameras the scale moves geometrically and the
//! canvas point under the middle of the screen moves linearly, so a pure zoom keeps the middle
//! still and a pure pan keeps the scale.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub scale: f64,
    pub view: (f64, f64),
}

impl Camera {
    /// The canvas point (1:1 canvas pixels) under the middle of a screen of this size.
    pub fn centre(&self, screen: (f64, f64)) -> (f64, f64) {
        (
            (self.view.0 + screen.0 / 2.) / self.scale,
            (self.view.1 + screen.1 / 2.) / self.scale,
        )
    }

    /// The camera at `scale` with `centre` under the middle of the screen.
    pub fn at_centre(scale: f64, centre: (f64, f64), screen: (f64, f64)) -> Self {
        Self {
            scale,
            view: (
                centre.0 * scale - screen.0 / 2.,
                centre.1 * scale - screen.1 / 2.,
            ),
        }
    }

    /// The camera a fraction `t` of the way from `a` to `b`.
    pub fn between(a: Camera, b: Camera, t: f64, screen: (f64, f64)) -> Self {
        let t = t.clamp(0., 1.);
        let scale = a.scale * (b.scale / a.scale).powf(t);
        let (ca, cb) = (a.centre(screen), b.centre(screen));
        let centre = (ca.0 + (cb.0 - ca.0) * t, ca.1 + (cb.1 - ca.1) * t);
        Self::at_centre(scale, centre, screen)
    }

    fn near(&self, o: &Camera) -> bool {
        (self.scale - o.scale).abs() < 1e-9
            && (self.view.0 - o.view.0).abs() < 1e-6
            && (self.view.1 - o.view.1).abs() < 1e-6
    }
}

/// Fast start, gentle stop.
pub fn ease_out_cubic(t: f64) -> f64 {
    let u = 1. - t.clamp(0., 1.);
    1. - u * u * u
}

/// How a tween moves the camera.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Curve {
    /// [`ease_out_cubic`] over the view settings' duration.
    Ease,
    /// A fling's glide: the rest of the way shrinks by `rate` every millisecond, so it starts
    /// at `-1000 ln rate` times the distance per second and ends after `duration`, when less
    /// than 1.5 px is left.
    Decelerate { rate: f64, duration: Duration },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Tween {
    from: Camera,
    to: Camera,
    start: Instant,
    curve: Curve,
}

/// One output's camera following its target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Follow {
    shown: Option<Camera>,
    target: Option<Camera>,
    tween: Option<Tween>,
    /// The next tween is a fling's glide at this rate (see [`Curve::Decelerate`]).
    fling: Option<f64>,
}

impl Follow {
    /// Aims at `target` and returns the camera to draw with at `now`. A new target starts a
    /// tween from where the camera is; the first target, or a zero duration, is taken at once.
    pub fn step(
        &mut self,
        target: Camera,
        now: Instant,
        screen: (f64, f64),
        duration: Duration,
    ) -> Camera {
        let at = self.at(now, screen, duration);
        let fling = self.fling.take();
        if self.target.is_none_or(|t| !t.near(&target)) {
            self.target = Some(target);
            self.tween = match at {
                Some(from) if !duration.is_zero() && !from.near(&target) => Some(Tween {
                    from,
                    to: target,
                    start: now,
                    curve: fling.map_or(Curve::Ease, |rate| {
                        let len = (target.view.0 - from.view.0).hypot(target.view.1 - from.view.1);
                        let secs = (1.5 / len.max(1.5)).ln() / (1000. * rate.ln());
                        Curve::Decelerate {
                            rate,
                            duration: Duration::from_secs_f64(secs.max(0.)),
                        }
                    }),
                }),
                _ => None,
            };
        }
        let cam = self.at(now, screen, duration).unwrap_or(target);
        self.shown = Some(cam);
        cam
    }

    /// The camera at `now`, ending the tween once its time is up.
    fn at(&mut self, now: Instant, screen: (f64, f64), duration: Duration) -> Option<Camera> {
        let Some(tw) = self.tween else {
            return self.target.or(self.shown);
        };
        let elapsed = now.saturating_duration_since(tw.start);
        let (t, eased) = match tw.curve {
            Curve::Ease if duration.is_zero() => (1., 1.),
            Curve::Ease => {
                let t = elapsed.as_secs_f64() / duration.as_secs_f64();
                (t, ease_out_cubic(t))
            }
            Curve::Decelerate { duration, .. } if duration.is_zero() => (1., 1.),
            Curve::Decelerate { rate, duration } => (
                elapsed.as_secs_f64() / duration.as_secs_f64(),
                1. - rate.powf(1000. * elapsed.as_secs_f64()),
            ),
        };
        if t >= 1. {
            self.tween = None;
            return Some(tw.to);
        }
        Some(Camera::between(tw.from, tw.to, eased, screen))
    }

    /// The next move toward a new target is a fling's glide slowing at `rate` per millisecond.
    pub fn fling(&mut self, rate: f64) {
        self.fling = Some(rate);
    }

    /// The camera last drawn with.
    pub fn shown(&self) -> Option<Camera> {
        self.shown
    }

    /// Whether a tween runs: more frames are due.
    pub fn moving(&self) -> bool {
        self.tween.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: (f64, f64) = (1000., 500.);
    const D: Duration = Duration::from_millis(200);

    fn cam(scale: f64, x: f64, y: f64) -> Camera {
        Camera {
            scale,
            view: (x, y),
        }
    }

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn a_zoom_keeps_the_middle_of_the_screen_still() {
        let a = cam(1., 300., 100.);
        let b = Camera::at_centre(0.5, a.centre(SCREEN), SCREEN);
        for t in [0., 0.25, 0.5, 0.9, 1.] {
            let c = Camera::between(a, b, t, SCREEN);
            assert!(close(c.centre(SCREEN), a.centre(SCREEN)), "t={t}: {c:?}");
        }
        let mid = Camera::between(a, b, 0.5, SCREEN);
        assert!((mid.scale - 0.5f64.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn a_pan_keeps_the_scale() {
        let (a, b) = (cam(0.8, 0., 0.), cam(0.8, 400., -200.));
        let c = Camera::between(a, b, 0.5, SCREEN);
        assert!((c.scale - 0.8).abs() < 1e-12);
        assert!(close(c.view, (200., -100.)));
    }

    #[test]
    fn the_first_target_is_taken_at_once() {
        let mut f = Follow::default();
        let now = Instant::now();
        assert_eq!(f.step(cam(1., 5., 5.), now, SCREEN, D), cam(1., 5., 5.));
        assert!(!f.moving());
    }

    #[test]
    fn a_new_target_animates_and_ends_on_it() {
        let mut f = Follow::default();
        let now = Instant::now();
        let (a, b) = (cam(1., 0., 0.), cam(0.5, -250., -125.));
        f.step(a, now, SCREEN, D);
        let start = f.step(b, now, SCREEN, D);
        assert_eq!(start, a);
        assert!(f.moving());
        let half = f.step(b, now + D / 2, SCREEN, D);
        assert!(half.scale < 1. && half.scale > 0.5, "{half:?}");
        assert!(close(half.centre(SCREEN), a.centre(SCREEN)));
        assert_eq!(f.step(b, now + D, SCREEN, D), b);
        assert!(!f.moving());
    }

    #[test]
    fn retargeting_mid_way_starts_from_where_the_camera_is() {
        let mut f = Follow::default();
        let now = Instant::now();
        f.step(cam(1., 0., 0.), now, SCREEN, D);
        f.step(cam(1., 1000., 0.), now, SCREEN, D);
        let mid = f.step(cam(1., 1000., 0.), now + D / 2, SCREEN, D);
        let back = f.step(cam(1., 0., 0.), now + D / 2, SCREEN, D);
        assert_eq!(mid, back);
        assert!(f.moving());
    }

    #[test]
    fn a_fling_glides_from_its_start_speed_and_ends_on_the_target() {
        let mut f = Follow::default();
        let now = Instant::now();
        f.step(cam(1., 0., 0.), now, SCREEN, D);
        f.fling(0.997);
        f.step(cam(1., 300., 0.), now, SCREEN, D);
        let ms = Duration::from_millis;
        let x1 = f.step(cam(1., 300., 0.), now + ms(1), SCREEN, D).view.0;
        let speed = x1 / 0.001;
        let expected = 300. * -1000. * 0.997f64.ln();
        assert!(
            (speed - expected).abs() < expected * 0.01,
            "{speed} vs {expected}"
        );
        assert!(
            f.step(cam(1., 300., 0.), now + D, SCREEN, D).view.0 < 300.,
            "longer than D"
        );
        assert!(f.moving());
        assert_eq!(
            f.step(cam(1., 300., 0.), now + ms(3000), SCREEN, D),
            cam(1., 300., 0.)
        );
        assert!(!f.moving());
        f.step(cam(1., 0., 0.), now + ms(3000), SCREEN, D);
        let half = f.step(cam(1., 0., 0.), now + ms(3000) + D / 2, SCREEN, D);
        assert!(
            (half.view.0 - 300. * (1. - ease_out_cubic(0.5))).abs() < 1e-6,
            "the fling was for one move only: {half:?}"
        );
    }

    #[test]
    fn without_animations_targets_are_taken_at_once() {
        let mut f = Follow::default();
        let now = Instant::now();
        f.step(cam(1., 0., 0.), now, SCREEN, Duration::ZERO);
        assert_eq!(
            f.step(cam(0.5, 9., 9.), now, SCREEN, Duration::ZERO),
            cam(0.5, 9., 9.)
        );
        assert!(!f.moving());
    }
}
