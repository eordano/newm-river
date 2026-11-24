use std::time::{Duration, Instant};

use niri_config::{Config, SynthBackground, SynthOnBattery};
use niri_ipc::SynthMode;

use super::*;

const TICK: Duration = Duration::from_millis(250);

/// The caller's present in the pacing checks: a fixed instant, so "due now" is exact.
static T0: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn config(mode: SynthMode) -> SynthBackground {
    SynthBackground {
        mode,
        ..SynthBackground::default()
    }
}

/// The tuned look with every time-dependent term off.
fn frozen(mode: SynthMode) -> SynthBackground {
    SynthBackground {
        mode,
        stars: Some(0.),
        grid_speed: Some(0.),
        ridge_speed: Some(0.),
        reflection: Some(0.),
        cycle_speed: Some(0.),
        ..SynthBackground::default()
    }
}

/// A surface through one draw: configured, submitted at `t`, painted and committed.
fn drawn_once(animated: bool, t: Instant) -> Pacing {
    let mut p = Pacing::default();
    p.configured();
    assert_eq!(
        p.due(*T0, animated, TICK),
        Some(*T0),
        "a new surface draws at once"
    );
    p.submitted(t);
    p.committed(animated);
    p
}

#[test]
fn the_defaults_draw_the_tuned_look() {
    assert_eq!(look(&config(SynthMode::Animated)), Look::default());
}

#[test]
fn every_knob_reaches_the_look() {
    let mut text = String::from("synth-background {\n seed 9\n");
    for knob in niri_config::SYNTH_KNOBS {
        let kdl = knob.replace('_', "-");
        if knob.ends_with("_color") {
            text += &format!(" {kdl} \"#102030\"\n");
        } else {
            text += &format!(" {kdl} 1\n");
        }
    }
    text += "}\n";
    let config = Config::parse_mem(&text).unwrap();
    let l = look(&config.synth_background);
    let debug = format!("{l:?}");
    assert_eq!(l.seed, 9);
    for knob in niri_config::SYNTH_KNOBS {
        let want = if knob.ends_with("_color") {
            format!("{knob}: {:?}", [16f32 / 255., 32. / 255., 48. / 255.])
        } else {
            format!("{knob}: 1.0")
        };
        assert!(debug.contains(&want), "{want} in {debug}");
    }
}

#[test]
fn off_and_still_want_no_frame_after_the_first() {
    let t = Instant::now();
    // Still: one frame, then nothing however long it waits.
    let p = drawn_once(false, t);
    assert_eq!(p.phase, Phase::Idle);
    assert_eq!(p.due(*T0, false, TICK), None);
    // Off makes no surfaces at all (Synth::is_allocated, synth_tick); the scene says so too.
    let off = Scene::new(&config(SynthMode::Off));
    assert!(!off.animated());
    let still = Scene::new(&config(SynthMode::Still));
    assert!(!still.animated());
}

#[test]
fn animated_waits_for_the_frame_callback_then_one_interval() {
    let t = Instant::now();
    let mut p = drawn_once(true, t);
    // Committed and not yet shown: no deadline at all. A hidden surface (covered, locked,
    // blanked) never gets its callback, so it stays here with no wakeup.
    assert_eq!(p.phase, Phase::Shown);
    assert_eq!(p.due(*T0, true, TICK), None);
    p.frame_done();
    assert_eq!(p.due(*T0, true, TICK), Some(t + TICK), "max-fps paces it");
}

#[test]
fn a_look_that_cannot_move_draws_once_even_when_animated() {
    let scene = Scene::new(&frozen(SynthMode::Animated));
    assert_eq!(scene.mode(), SynthMode::Animated);
    assert!(!scene.animated());
    let p = drawn_once(scene.animated(), Instant::now());
    assert_eq!(p.due(*T0, scene.animated(), scene.interval()), None);
}

#[test]
fn nothing_is_ever_due_while_a_frame_is_in_flight() {
    // No state but Idle has a deadline, so the loop never spins on a past one.
    let t = Instant::now();
    let mut p = Pacing::default();
    assert_eq!(p.due(*T0, true, TICK), None, "unconfigured");
    p.configured();
    p.submitted(t);
    assert_eq!(p.due(*T0, true, TICK), None, "painting");
    p.no_buffer();
    assert_eq!(p.due(*T0, true, TICK), None, "both buffers river's");
    p.released();
    assert!(p.due(*T0, true, TICK).is_some());
    // A draw that could not start waits an interval, not a spin.
    p.skipped(t);
    assert_eq!(p.due(*T0, true, TICK), Some(t + TICK));
    assert_eq!(p.due(*T0, false, TICK), None);
}

#[test]
fn a_config_change_redraws_a_still_surface_once() {
    let t = Instant::now();
    let mut p = drawn_once(false, t);
    p.dirty = true;
    assert!(p.due(*T0, false, TICK).is_some());
    p.submitted(t + TICK);
    p.committed(false);
    assert_eq!(p.due(*T0, false, TICK), None);
}

#[test]
fn the_toggle_follows_the_configured_mode() {
    for (configured, rest) in [
        (SynthMode::Off, SynthMode::Off),
        (SynthMode::Still, SynthMode::Still),
        (SynthMode::Animated, SynthMode::Still),
    ] {
        let c = config(configured);
        let mut s = Scene::new(&c);
        if configured != SynthMode::Animated {
            s.toggle(&c);
            assert_eq!(s.mode(), SynthMode::Animated, "{configured:?}");
        }
        s.toggle(&c);
        assert_eq!(s.mode(), rest, "{configured:?}");
        s.toggle(&c);
        assert_eq!(s.mode(), SynthMode::Animated, "{configured:?}");
    }
}

#[test]
fn a_set_mode_holds_until_the_config_changes_the_mode() {
    let mut s = Scene::new(&config(SynthMode::Off));
    s.set_mode(SynthMode::Still, &config(SynthMode::Off));
    assert_eq!(s.mode(), SynthMode::Still);
    let other = SynthBackground {
        brightness: Some(0.3),
        ..config(SynthMode::Off)
    };
    s.apply_config(&other);
    assert_eq!(
        s.mode(),
        SynthMode::Still,
        "another knob keeps the set mode"
    );
    s.config_mode_changed();
    s.apply_config(&config(SynthMode::Animated));
    assert_eq!(
        s.mode(),
        SynthMode::Animated,
        "a new mode in the config wins"
    );
}

const HZ60: Duration = Duration::from_nanos(16_666_667);

#[test]
fn the_clock_stands_still_while_hidden() {
    let mut c = SceneClock::default();
    let t = Duration::from_secs(100);
    c.advance(t, Some(HZ60), Duration::ZERO);
    c.advance(t + HZ60, Some(HZ60), Duration::ZERO);
    assert_eq!(c.time, HZ60);
    // An hour hidden (no frames drawn) moves it on by one refresh, not by the hour.
    c.advance(
        t + HZ60 + Duration::from_secs(3600),
        Some(HZ60),
        Duration::ZERO,
    );
    assert_eq!(c.time, HZ60 * 2);
}

#[test]
fn the_display_rate_waits_for_the_frame_callback_and_nothing_more() {
    let scene = Scene::new(&config(SynthMode::Animated));
    assert!(scene.animated());
    assert_eq!(scene.cap(), None, "no max-fps: the display's rate");
    assert_eq!(scene.interval(), Duration::ZERO);
    let t = *T0;
    let mut p = drawn_once(true, t);
    assert_eq!(
        p.due(t, true, scene.interval()),
        None,
        "the callback has not come"
    );
    p.frame_done();
    assert_eq!(
        p.due(t + HZ60, true, scene.interval()),
        Some(t),
        "due the moment the callback comes"
    );
}

#[test]
fn a_zero_interval_never_spins() {
    // A draw that could not start retries a refresh later, not at once.
    let t = *T0;
    let mut p = drawn_once(true, t);
    p.frame_done();
    p.skipped(t);
    assert_eq!(p.due(t, true, Duration::ZERO), Some(t + REFRESH_FLOOR));
    p.submitted(t + REFRESH_FLOOR);
    p.committed(true);
    p.frame_done();
    assert_eq!(
        p.due(t, true, Duration::ZERO),
        Some(t + REFRESH_FLOOR),
        "a draw that started clears the retry"
    );
    // A listening still look ticks its level a refresh apart at the least.
    let mut s = listening_scene(sound_on(SynthMode::Animated));
    s.hear(0.5);
    let _ = s.level_moved(t, 960);
    assert_eq!(s.level_due(t), t + REFRESH_FLOOR);
}

#[test]
fn presentation_targets_give_exact_even_steps() {
    // Present times that wander by up to 3 ms around a steady 60 Hz (a headless output reports
    // when it committed): the scene still steps by exactly one refresh.
    let mut c = SceneClock::default();
    let base = Duration::from_secs(1000);
    let jitter = [0u64, 3, 1, 2, 0, 3, 2, 1, 0, 2];
    let p = |i: u32| base + HZ60 * i + Duration::from_millis(jitter[i as usize % 10]);
    let mut times = vec![];
    for i in 0..60u32 {
        // The callback comes just after frame i showed, and the next frame is drawn then; the
        // feedback for frame i came before it (even i) or comes after it (odd i).
        let now = p(i) + Duration::from_micros(300);
        let presented = match i {
            0 => None,
            _ if i % 2 == 0 => Some(p(i)),
            _ => Some(p(i - 1)),
        };
        let target = target(now, presented, Some(HZ60));
        times.push(c.advance(target, Some(HZ60), Duration::ZERO));
    }
    for w in times.windows(2).skip(1) {
        assert_eq!(w[1] - w[0], HZ60, "{times:?}");
    }
    // Without a refresh from river nor the output, the target is the monotonic now.
    assert_eq!(target(base, Some(base - HZ60), None), base);
    assert_eq!(target(base, None, Some(HZ60)), base + HZ60);
    // The refresh after the one nearest now, on the grid of the last presented time.
    assert_eq!(
        target(
            base + HZ60 + Duration::from_millis(1),
            Some(base),
            Some(HZ60)
        ),
        base + HZ60 * 2
    );
    assert_eq!(target(base, Some(base), Some(HZ60)), base + HZ60);
}

#[test]
fn a_steady_refresh_of_another_rate_steps_by_that_refresh() {
    let hz144 = Duration::from_nanos(6_944_444);
    let mut c = SceneClock::default();
    let base = Duration::from_secs(5);
    let mut last = None;
    for i in 0..20u32 {
        let t = target(base + hz144 * i, last, Some(hz144));
        let before = c.time;
        c.advance(t, Some(hz144), Duration::ZERO);
        if i > 0 {
            assert_eq!(c.time - before, hz144);
        }
        last = Some(base + hz144 * i);
    }
}

#[test]
fn it_resumes_from_the_picture_that_showed() {
    let mut c = SceneClock::default();
    let t = Duration::from_secs(50);
    for i in 0..10u32 {
        c.advance(t + HZ60 * i, Some(HZ60), Duration::ZERO);
    }
    let shown = c.time;
    // Stopped (battery, idle, still) for a minute: the clock forgets its last target.
    c.stop();
    assert_eq!(
        c.advance(t + Duration::from_secs(60), Some(HZ60), Duration::ZERO),
        shown
    );
    assert_eq!(
        c.advance(
            t + Duration::from_secs(60) + HZ60,
            Some(HZ60),
            Duration::ZERO
        ),
        shown + HZ60,
        "then one refresh a frame again"
    );
    // Hidden without a stop (no callback came for a while): at most one refresh.
    let before = c.time;
    c.advance(t + Duration::from_secs(90), Some(HZ60), Duration::ZERO);
    assert_eq!(c.time - before, HZ60);
}

#[test]
fn the_cap_paces_it() {
    let capped = |max_fps: Option<f64>, battery: SynthOnBattery, on: bool, cpu: bool| {
        let mut s = Scene::new(&SynthBackground {
            max_fps,
            on_battery: battery,
            on_battery_max_fps: 20.,
            ..config(SynthMode::Animated)
        });
        s.on_battery = on;
        s.cpu = cpu;
        s.cap()
    };
    use SynthOnBattery::*;
    assert_eq!(capped(None, Still, false, false), None);
    assert_eq!(capped(Some(30.), Still, false, false), Some(30.));
    assert_eq!(capped(None, Capped, true, false), Some(20.));
    assert_eq!(
        capped(Some(10.), Capped, true, false),
        Some(10.),
        "the smaller"
    );
    assert_eq!(
        capped(Some(30.), Capped, false, false),
        Some(30.),
        "on mains"
    );
    assert_eq!(capped(None, Animated, true, false), None);
    // The CPU painter stays at 4 frames a second, or max-fps if lower.
    assert_eq!(capped(None, Still, false, true), Some(CPU_FPS));
    assert_eq!(capped(Some(2.), Still, false, true), Some(2.));
    // A capped picture waits the cap after the last frame, callback or not.
    let s = Scene::new(&SynthBackground {
        max_fps: Some(30.),
        ..config(SynthMode::Animated)
    });
    let t = *T0;
    let mut p = drawn_once(true, t);
    p.frame_done();
    assert_eq!(p.due(t, true, s.interval()), Some(t + s.interval()));
    assert_eq!(s.interval(), Duration::from_secs_f64(1. / 30.));
    // Steps of a capped picture are whole refreshes: two at 30 on a 60 Hz output.
    let mut c = SceneClock::default();
    let base = Duration::from_secs(9);
    c.advance(base, Some(HZ60), s.interval());
    c.advance(base + HZ60 * 2, Some(HZ60), s.interval());
    assert_eq!(c.time, HZ60 * 2);
}

#[test]
fn battery_and_idle_stand_it_still() {
    let mut s = Scene::new(&sound_on(SynthMode::Animated));
    assert!(s.animated());
    assert!(
        s.wants_power(),
        "on-battery still by default: watch the supplies"
    );
    s.on_battery = true;
    assert!(!s.animated(), "on battery: still");
    assert!(!s.wants_sound(true), "and not listening");
    s.on_battery = false;
    assert!(s.animated());
    assert_eq!(s.idle_timeout_ms(), Some(300_000));
    s.idle = true;
    assert!(!s.animated(), "idle: still");
    s.idle = false;
    assert!(s.animated());
    // A still picture asks for no frames once drawn.
    s.idle = true;
    let p = drawn_once(s.animated(), *T0);
    assert_eq!(p.phase, Phase::Idle);
    assert_eq!(p.due(*T0, s.animated(), s.interval()), None);

    let animated_on_battery = Scene::new(&SynthBackground {
        on_battery: SynthOnBattery::Animated,
        idle_still_after: 0.,
        ..config(SynthMode::Animated)
    });
    assert!(!animated_on_battery.wants_power(), "nothing to watch");
    assert_eq!(
        animated_on_battery.idle_timeout_ms(),
        None,
        "0 never stops it"
    );
    assert!(!Scene::new(&config(SynthMode::Still)).wants_power());
    assert_eq!(
        Scene::new(&config(SynthMode::Still)).idle_timeout_ms(),
        None
    );
}

#[test]
fn only_power_supply_uevents_count() {
    let msg = |fields: &[&str]| {
        let mut m = b"change@/devices/LNXSYSTM:00/ACPI0003:00/power_supply/AC\0".to_vec();
        for f in fields {
            m.extend_from_slice(f.as_bytes());
            m.push(0);
        }
        m
    };
    assert!(uevent::is_power_supply(&msg(&[
        "ACTION=change",
        "SUBSYSTEM=power_supply",
        "POWER_SUPPLY_ONLINE=0",
    ])));
    assert!(!uevent::is_power_supply(&msg(&[
        "ACTION=add",
        "SUBSYSTEM=usb"
    ])));
    assert!(!uevent::is_power_supply(&msg(&[
        "SUBSYSTEM=power_supply_x"
    ])));
    // The header names the device path, not the subsystem.
    assert!(!uevent::is_power_supply(
        b"add@SUBSYSTEM=power_supply\0ACTION=add\0"
    ));
}

#[test]
fn xrgb_swaps_red_and_blue() {
    let mut px = [newm_synth::pack(0x11, 0x22, 0x33)];
    to_xrgb(&mut px);
    assert_eq!(px[0].to_le_bytes(), [0x33, 0x22, 0x11, 0xff]);
}

#[test]
fn a_due_surface_is_due_at_the_ticks_own_now() {
    // The tick compares `due(now)` with that same `now`: a surface that wants to draw at once
    // must be due exactly then, or the loop polls again and again without drawing.
    let mut p = Pacing::default();
    p.configured();
    let now = Instant::now() + Duration::from_secs(5);
    assert!(p.due(now, true, TICK).is_some_and(|d| d <= now));
    assert!(p.due(now, false, TICK).is_some_and(|d| d <= now));
}

fn listening_scene(config: SynthBackground) -> Scene {
    let mut s = Scene::new(&config);
    s.set_listening(true);
    s
}

fn sound_on(mode: SynthMode) -> SynthBackground {
    SynthBackground {
        react_to_sound: true,
        ..config(mode)
    }
}

#[test]
fn only_the_switch_listens_and_only_while_animated_and_showing() {
    // The tuned audio amounts alone listen to nothing.
    assert!(!Scene::new(&config(SynthMode::Animated)).wants_sound(true));
    let on = Scene::new(&sound_on(SynthMode::Animated));
    assert!(on.wants_sound(true));
    assert!(!on.wants_sound(false), "hidden: not listening");
    assert!(!Scene::new(&sound_on(SynthMode::Still)).wants_sound(true));
    assert!(!Scene::new(&sound_on(SynthMode::Off)).wants_sound(true));
    let deaf = SynthBackground {
        audio_grid: Some(0.),
        audio_ridge: Some(0.),
        audio_sun: Some(0.),
        ..sound_on(SynthMode::Animated)
    };
    assert!(!Scene::new(&deaf).wants_sound(true));
}

#[test]
fn a_frame_left_unanswered_counts_as_hidden() {
    let t = *T0;
    let mut p = Pacing::default();
    assert!(!p.showing(t), "not configured yet");
    p.configured();
    p.submitted(t);
    p.committed(true);
    assert!(p.showing(t + HIDDEN_AFTER / 2));
    assert!(
        !p.showing(t + HIDDEN_AFTER),
        "river did not answer: covered, locked or blank"
    );
    p.frame_done();
    assert!(p.showing(t + HIDDEN_AFTER * 5));
}

#[test]
fn a_still_look_redraws_only_when_the_level_moves_a_step() {
    let mut s = listening_scene(SynthBackground {
        stars: Some(0.),
        grid_speed: Some(0.),
        ridge_speed: Some(0.),
        reflection: Some(0.),
        cycle_speed: Some(0.),
        ..sound_on(SynthMode::Animated)
    });
    assert!(!s.animated());
    assert!(
        !s.sound_busy(false),
        "silent: nothing to draw, nothing scheduled"
    );
    let t = *T0;
    s.hear(0.8);
    assert!(s.sound_busy(true));
    assert!(
        s.level_moved(t, 960),
        "the first loud block moves the picture"
    );
    let _ = s.frame(t, Duration::ZERO, (1440, 960));
    // The same loudness every tick: after the level settles, no tick asks for a redraw.
    let mut redraws = 0;
    for i in 1..12 {
        s.hear(0.8);
        let now = t + TICK * i;
        if s.level_moved(now, 960) {
            redraws += 1;
            let _ = s.frame(now, Duration::ZERO, (1440, 960));
        }
    }
    assert!(redraws <= 1, "{redraws} redraws for a steady sound");
    // Silence: at most one redraw a tick until the level is drawn at zero, then nothing.
    let mut ticks = 0;
    for i in 12..80 {
        if !s.sound_busy(false) {
            break;
        }
        ticks += 1;
        let now = t + TICK * i;
        if s.level_moved(now, 960) {
            let _ = s.frame(now, Duration::ZERO, (1440, 960));
        }
    }
    assert!(ticks > 0 && ticks < 60, "{ticks} ticks to settle");
    assert!(!s.sound_busy(false));
}

#[test]
fn level_ticks_come_once_an_interval_at_most() {
    let mut s = listening_scene(SynthBackground {
        max_fps: Some(4.),
        ..sound_on(SynthMode::Animated)
    });
    let t = *T0;
    s.hear(0.5);
    let _ = s.level_moved(t, 960);
    assert_eq!(
        s.level_due(t),
        t + TICK,
        "max-fps 4: the next level tick a quarter second on"
    );
}

#[test]
fn closing_the_capture_draws_the_picture_without_its_level() {
    let mut s = listening_scene(sound_on(SynthMode::Animated));
    s.hear(1.);
    let _ = s.frame(*T0 + TICK, Duration::ZERO, (1440, 960));
    assert!(s.drawn_step > 0);
    assert!(s.set_listening(false), "it showed a level: draw again");
    assert_eq!((s.drawn_step, s.drawn_level), (0, 0.));
    assert!(!s.set_listening(false));
}

#[test]
fn the_capture_is_a_passive_one_of_the_sink_monitor() {
    let props: std::collections::HashMap<_, _> = sound::properties().into_iter().collect();
    assert_eq!(props["node.passive"], "true");
    assert_eq!(props["stream.capture.sink"], "true");
    assert_eq!(props["media.category"], "Capture");
}

#[test]
fn the_gpu_draws_at_the_outputs_physical_size() {
    // The GPU path shades every physical pixel, through the viewport.
    assert_eq!(physical((1440, 960), 120), (1440, 960));
    assert_eq!(physical((1440, 960), 240), (2880, 1920));
    assert_eq!(physical((2560, 720), 240), (5120, 1440));
    assert_eq!(physical((1536, 864), 150), (1920, 1080), "scale 1.25");
}

#[test]
fn a_refresh_nobody_reports_is_measured() {
    // A headless output reports no refresh and presents every 15 to 18 ms.
    let mut m = None;
    for i in 0..200u64 {
        m = measure_refresh(m, Duration::from_micros(15_000 + (i * 7919) % 3_000));
    }
    let m = m.unwrap();
    assert!(
        m > Duration::from_micros(16_000) && m < Duration::from_micros(17_000),
        "{m:?}"
    );
    // A missed refresh, a capped or a hidden picture do not move it.
    assert_eq!(measure_refresh(Some(m), m * 2), Some(m));
    assert_eq!(measure_refresh(Some(m), Duration::from_secs(3)), Some(m));
    assert_eq!(measure_refresh(None, Duration::from_millis(250)), None);
    // Steps by that measure are even: the jitter rounds away.
    let mut c = SceneClock::default();
    let mut t = Duration::from_secs(1);
    c.advance(t, Some(m), Duration::ZERO);
    for i in 0..50u64 {
        t += Duration::from_micros(14_500 + (i * 104_729) % 4_000);
        let before = c.time;
        c.advance(t, Some(m), Duration::ZERO);
        assert_eq!(c.time - before, m);
    }
}
