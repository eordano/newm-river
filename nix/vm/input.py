
# ---- vm-input: real devices through libinput (nix/vm/input.py) -------------------------------
start_all()
wait_session()
o = output()
W, H = o["logical"]["width"], o["logical"]["height"]
print(sh("cat /sys/class/drm/card*-*/status; libinput list-devices | grep -A1 '^Device:.*uin'"))

# The app picks its size since open-size: 690x430 makes 6x4 tiles, so the three fit side by side
# on 2560 px and each opens beside the last (placement: where the view scrolls least).
for i, color in enumerate(["c03030", "30c030", "3030c0"]):
    open_window(f"w{i + 1}", color, "--size 690x430")
time.sleep(0.5)


def on_screen(app):
    """The on-screen rectangle (x, y, w, h) of a window at zoom 1, logical px."""
    w = win(app)
    x, y = w["layout"]["tile_pos_in_workspace_view"]
    tw, th = w["layout"]["window_size"]
    return int(x), int(y), int(tw), int(th)


def t_session_on_drm():
    pid = sh("pgrep -u alice -x river").split()[0]
    expect("/dev/dri/card" in sh(f"ls -l /proc/{pid}/fd"), "river holds no DRM card open")
    expect(o["name"].startswith("Virtual-"), f"output {o['name']} is not virtio's")
    expect("not restoring nor saving" not in journal(), "newm-river took the TTY for nested")
    shot("desktop")
    drm_shot("desktop-drm")


def t_super_tap():
    expect(not overview_open(), "the overview is open before the tap")
    uin("key leftmeta down; sleep 50; key leftmeta up")
    wait(overview_open, "a Super tap to open the overview")
    shot("overview-super-tap")
    uin("key leftmeta down; sleep 50; key leftmeta up")
    wait(lambda: not overview_open(), "a second tap to close it")


def t_super_tap_ctrl():
    uin("key leftctrl down; key leftmeta down; sleep 50; key leftmeta up; key leftctrl up")
    time.sleep(0.8)
    expect(not overview_open(), "Ctrl+Super tapped the overview open")


def t_super_tap_unbound_key():
    uin("key leftmeta down; sleep 30; tap z; sleep 30; key leftmeta up")
    time.sleep(0.8)
    expect(not overview_open(), "Super with an unbound key (z) still tapped the overview open")


def t_swipe3_focus():
    # Trackpad: one swipe moves the focus one window, however far the fingers go.
    focus("w3")
    uin("swipe 3 -30 0")
    wait(lambda: focused() == "w2", "a three-finger swipe left to move the focus to w2")
    time.sleep(0.5)
    expect(focused() == "w2", f"30 mm of one swipe moved the focus on to {focused()}")
    uin("swipe 3 30 0")
    wait(lambda: focused() == "w3", "a swipe right to move it back")


def t_swipe4_focus():
    focus("w3")
    uin("swipe 4 -30 0")
    wait(lambda: focused() == "w2", "a four-finger swipe left to move the focus to w2")
    time.sleep(0.5)
    expect(focused() == "w2", f"30 mm of one swipe moved the focus on to {focused()}")
    expect(not overview_open(), "a sideways four-finger swipe opened the overview")
    expect(not zooms(), "a sideways four-finger swipe zoomed")


def zooms():
    """The zoom steps four-finger swipes made, in order: positive pulls the camera back."""
    import re
    return [int(n) for n in re.findall(r"pointer Zoom\((-?\d+)\)", journal())]


def t_swipe4_zoom():
    # Trackpad: four fingers up pull the camera back, down bring it closer, a step for
    # each 100 of motion; the overview stays shut.
    n = len(zooms())
    uin("swipe 4 0 -25")
    wait(lambda: len(zooms()) > n, "a four-finger swipe up to zoom")
    time.sleep(0.5)
    up = zooms()[n:]
    print(f"four fingers 25 mm up: zoom steps {up}")
    expect(all(z > 0 for z in up) and sum(up) >= 2, f"25 mm up made the zoom steps {up}")
    expect(not overview_open(), "a four-finger swipe up opened the overview")
    shot("swipe4-zoomed-out")
    m = len(zooms())
    uin("swipe 4 0 25")
    wait(lambda: len(zooms()) > m, "a four-finger swipe down to zoom")
    time.sleep(0.5)
    down = zooms()[m:]
    print(f"four fingers 25 mm down: zoom steps {down}")
    expect(all(z < 0 for z in down) and sum(down) <= -2, f"25 mm down made the zoom steps {down}")
    expect(not overview_open(), "a four-finger swipe down opened the overview")
    msg("action", "zoom-reset")
    time.sleep(0.5)


def t_super_swipe3_pan():
    # Trackpad: Super and three fingers pan the camera after the fingers on both axes;
    # no window swaps.
    focus("w2")
    time.sleep(0.5)
    cols = [col(w) for w in ("w1", "w2", "w3")]
    x0, y0, _, _ = on_screen("w2")
    pans, ends = count("pointer SwipePan {"), count("pointer SwipePanEnd")
    taps = count("pointer Act(ToggleOverview)")
    sh("(uin 'key leftmeta down; sleep 100; hold 3 10 5 1500; sleep 30; key leftmeta up' "
       ">/dev/null 2>&1 &)")
    wait(lambda: count("pointer SwipePan {") > pans, "Super+3 to pan the camera", 10)
    time.sleep(0.6)
    x1, y1, _, _ = on_screen("w2")
    print(f"Super+3 10 mm right, 5 mm down: w2 went from {x0},{y0} to {x1},{y1}")
    expect(x1 - x0 > 100 and y1 - y0 > 50,
           f"the canvas did not go after the fingers: w2 from {x0},{y0} to {x1},{y1}")
    wait(lambda: count("pointer SwipePanEnd") > ends, "the fingers' lift to end the pan")
    time.sleep(0.5)
    expect([col(w) for w in ("w1", "w2", "w3")] == cols, "Super+3 moved a window")
    expect(count("pointer Act(ToggleOverview)") == taps, "Super with a swipe was a tap")
    focus("w2")
    time.sleep(0.5)


def t_super_swipe4_move():
    focus("w2")
    before = col("w2")
    fits, back = count("pointer ZoomToFit"), count("pointer ZoomBack")
    sh("(uin 'key leftmeta down; sleep 30; swipe 4 25 0 500; sleep 600' >/dev/null 2>&1 &)")
    wait(lambda: count("pointer ZoomToFit") > fits, "Super+4 to zoom to fit", 10)
    time.sleep(0.5)
    shot("super-swipe4-zoomed")
    uin("key leftmeta up")
    wait(lambda: count("pointer ZoomBack") > back, "Super's release to zoom back")
    wait(lambda: col("w2") > before, f"Super+4 right to move w2 right of column {before}")


def t_super_wheel_zoom():
    outs, ins = count("pointer Act(ZoomOut)"), count("pointer Act(ZoomIn)")
    taps = count("pointer Act(ToggleOverview)")
    x, y, _, _ = on_screen(focused())
    uin(f"abs {x + 50} {y + 50} {W} {H}; key leftmeta down; sleep 40; wheel 1; sleep 40; key leftmeta up")
    wait(lambda: count("pointer Act(ZoomOut)") == outs + 1, "Super+wheel down to zoom out once")
    shot("super-wheel-zoomed-out")
    uin("key leftmeta down; sleep 40; wheel -1; sleep 40; key leftmeta up")
    wait(lambda: count("pointer Act(ZoomIn)") == ins + 1, "Super+wheel up to zoom in once")
    expect(count("pointer Act(ToggleOverview)") == taps, "Super with the wheel was a tap")


def wev_lines(pattern):
    return int(sh(f"grep -c {shlex.quote(pattern)} /tmp/wev.log || true").strip() or 0)


def t_super_2fg_scroll_no_zoom():
    outs = count("pointer Act(ZoomOut)")
    uin("key leftmeta down; sleep 30; swipe 2 0 20; sleep 30; key leftmeta up")
    time.sleep(0.8)
    expect(count("pointer Act(ZoomOut)") == outs, "Super+two-finger scroll zoomed like the wheel")


def t_2fg_scroll_reaches_app():
    focus("wev")
    time.sleep(0.6)
    x, y, _, _ = on_screen("wev")
    uin(f"abs {x + 80} {y + 80} {W} {H}")
    before = wev_lines("axis: time")
    uin("swipe 2 0 20")
    wait(lambda: wev_lines("axis: time") > before, "two-finger scroll to reach wev")


def t_super_2fg_scroll_reaches_app():
    x, y, _, _ = on_screen("wev")
    uin(f"abs {x + 80} {y + 80} {W} {H}")
    before = wev_lines("axis: time")
    uin("key leftmeta down; sleep 30; swipe 2 0 20; sleep 30; key leftmeta up")
    wait(lambda: wev_lines("axis: time") > before, "Super+two-finger scroll to reach wev", 5)


def t_minimap():
    # Trackpad: a plan of the canvas around what the screen shows appears in the
    # bottom-left corner for 750 ms after the focus, the camera or a window moved: 240x160, 12 px
    # from the left edge, the screen's near-white outline in its middle, 0.4 of it wide.
    focus("w1")
    time.sleep(2.5)
    # The plan stays 750 ms: the shot is taken by the command that moves the focus, 200 ms
    # after the move (the action runs in river's next manage sequence, after the reply).
    alice(f"newm-river msg action focus-window --id {win('w2')['id']} && sleep 0.2 && grim /tmp/shots/minimap-shown.png")
    machine.copy_from_vm("/tmp/shots/minimap-shown.png", "shots")
    shown = Image.open(os.path.join(OUT, "shots", "minimap-shown.png")).convert("RGB")
    time.sleep(2.5)
    gone = shot("minimap-gone")
    s = shown.size[0] // W
    box = (0, (H - 230) * s, 330 * s, H * s)
    share = changed(shown, gone, box)
    print(f"minimap: {share:.3f} of the bottom-left corner differs while it is shown")
    expect(share > 0.03, f"no minimap in the bottom-left corner after a focus move ({share:.3f})")

    def outline_sides(img):
        """The columns of the corner holding a tall run of the outline's near white."""
        crop = img.crop(box).convert("RGB")
        px = crop.load()
        light = lambda c: all(abs(v - t) <= 40 for v, t in zip(c, (0xDD, 0xF7, 0xFF)))
        return [x for x in range(crop.width)
                if sum(light(px[x, y]) for y in range(crop.height)) >= 25 * s]

    sides = outline_sides(shown)
    expect(sides, "no outline of the screen in the corner")
    left, right = sides[0], sides[-1]
    print(f"minimap: the screen's outline spans columns {left} to {right} at scale {s}")
    expect(abs((left + right) / 2 - (12 + 120) * s) <= 4 * s,
           f"the outline is centred at column {(left + right) / 2}, the plan's middle is {(12 + 120) * s}")
    expect(abs((right - left) - 96 * s) <= 4 * s,
           f"the outline is {right - left} px wide, not 0.4 of the plan ({96 * s})")
    expect(not outline_sides(gone), "the minimap stayed past 750 ms")
    time.sleep(0.5)
    still = shot("minimap-still-gone")
    expect(changed(gone, still, box) < 0.005, "the corner still changes 3 s after the move")


def t_zoomed_click():
    focus("wev")
    time.sleep(0.6)
    x, y, _, _ = on_screen("wev")
    before = wev_lines("button: ")
    uin(f"abs {x + 80} {y + 80} {W} {H}; click")
    wait(lambda: wev_lines("button: ") > before, "a click at zoom 1 to reach wev")
    msg("action", "zoom-out")
    msg("action", "zoom-out")
    time.sleep(0.7)
    shot("zoomed-out-click")
    zx, zy, _, _ = on_screen("wev")
    seen = wev_lines("button: ")
    uin(f"abs {zx + 15} {zy + 15} {W} {H}; click")
    time.sleep(0.6)
    reached = wev_lines("button: ") - seen
    other = next((a for a in ("w3", "w2", "w1") if 0 <= on_screen(a)[0] < W - 30), None)
    if other:
        ox, oy, _, _ = on_screen(other)
        uin(f"abs {ox + 15} {max(oy, 0) + 15} {W} {H}; click")
        wait(lambda: focused() == other, f"a click on the zoomed-out {other} to focus it")
    msg("action", "zoom-in")
    msg("action", "zoom-in")
    time.sleep(0.5)
    expect(reached == 0, f"the click on the zoomed-out wev reached it ({reached} button events)")


def t_alt_tab():
    focus("w1")
    focus("w3")
    time.sleep(0.5)
    base = shot("alt-tab-before")
    uin("key leftalt down; sleep 40; tap tab; sleep 700")
    panel = shot("alt-tab-switcher")
    uin("key leftalt up")
    pw, ph = base.size
    box = (pw // 4, ph // 3, 3 * pw // 4, 2 * ph // 3)
    frac = changed(base, panel, box)
    wait(lambda: focused() == "w1", "Alt+Tab to switch to the previous window (w1)")
    expect(frac > 0.1, f"no switcher panel drawn: {frac:.2f} of the middle changed")
    uin("key leftalt down; sleep 40; tap tab; sleep 40; tap tab; sleep 300; key leftalt up")
    wait(lambda: focused() not in ("w1", "w3"), "Alt+Tab+Tab to reach the third window")
    third = focused()
    uin("key leftalt down; sleep 40; tap tab; sleep 300; tap esc; sleep 100; key leftalt up")
    time.sleep(0.5)
    expect(focused() == third, "Alt+Escape did not cancel the switch")


def t_middle_button_scroll():
    focus("wev")
    time.sleep(0.6)
    x, y, _, _ = on_screen("wev")
    uin(f"abs {x + 200} {y + 200} {W} {H}; rel 2 0")
    before, buttons = wev_lines("axis: time"), wev_lines("button: 274")
    # libinput drops motion in the first 200 ms of the button (its button-scroll timeout); hold it
    # well past that, since uinput events can arrive bunched under a loaded host.
    uin("rbutton middle down; sleep 600; rel 0 60; sleep 30; rbutton middle up")
    wait(lambda: wev_lines("axis: time") > before, "middle button + motion to scroll wev")
    uin("rbutton middle down; sleep 60; rbutton middle up")
    wait(lambda: wev_lines("button: 274") > buttons, "a still middle click to reach wev")


def t_pan_past_edge():
    focus("w1")
    x0 = win("w1")["layout"]["tile_pos_in_workspace_view"][0]
    uin(f"abs {W - 2} {H // 2} {W} {H}; key leftmeta down; sleep 120; rel 300 0; sleep 60; key leftmeta up")
    time.sleep(0.6)
    x1 = win("w1")["layout"]["tile_pos_in_workspace_view"][0]
    expect(abs(x1 - x0) >= 100, f"a Super pan at the screen edge moved the view {x1 - x0} px")


KDL = "/home/alice/.config/newm-river/config.kdl"


def reload_config(sed):
    """Edits the session's config with a sed script and waits until newm-river reloaded it."""
    before = count("config reloaded")
    sh(f"sed -i {shlex.quote(sed)} {KDL}")
    wait(lambda: count("config reloaded") > before, "newm-river to reload the config")


def super_flick(x, dx):
    """How far the view moves under a fast Super pan of dx counts from (x, middle), read while
    Super is still held: after the pan, before its fling or the focus settling."""
    def view():
        return win("w1")["layout"]["tile_pos_in_workspace_view"][0]

    uin(f"abs {x} {H // 2} {W} {H}; sleep 200")
    x0 = view()
    uin(f"key leftmeta down; sleep 120; rel {dx} 0 60 4; sleep 400")
    x1 = view()
    uin("key leftmeta up")
    time.sleep(0.6)
    return x1 - x0


def t_pan_acceleration():
    # Through libinput: a flick of 600 counts (10 events of 60, 4 ms apart).
    # The config's mouse is flat, where accelerated == unaccelerated; flat at accel-speed 1 is
    # libinput's constant factor 1 + 1, so the cursor moves 1200 px for the hand's 600 (1000
    # dpi). pan-acceleration 1 (the default) pans by the cursor, 0.5 by 600 + 0.5 * 600, 0 by
    # the hand's 600 (river's gestures v4 pointer_motion). Every step is a hot reload.
    reload_config('s|^\\( *\\)accel-profile "flat"$|&\\n\\1accel-speed 1.0|')
    try:
        cursor = super_flick(300, 600)
        reload_config("s|^\\( *\\)canvas {$|&\\n\\1    pan-acceleration 0.5|")
        half = super_flick(W - 300, -600)
        reload_config("s|pan-acceleration 0.5$|pan-acceleration 0|")
        hand = super_flick(300, 600)
    finally:
        reload_config("/pan-acceleration 0\\|accel-speed 1.0/d")
    print(f"newm-vm: a 600-count flick at 2x panned {cursor} px at pan-acceleration 1, "
          f"{half} px at 0.5, {hand} px at 0")
    for got, want, share in [(cursor, 1200, 1), (half, -900, 0.5), (hand, 600, 0)]:
        expect(abs(got - want) <= 0.05 * abs(want),
               f"pan-acceleration {share} panned {got} px, not about {want}")


def super_fling(dx, inset):
    """How far the view moves under a Super pan of dx counts (20-count steps 10 ms apart) from
    inset px off the screen's edge that lets go of Super while the hand still moves, read once
    the view settled: the pan and the share of its fling pan-momentum keeps. Each call takes
    its own inset: evdev drops an absolute move to the value it last sent."""
    def view():
        return win("w1")["layout"]["tile_pos_in_workspace_view"][0]

    uin(f"abs {inset if dx > 0 else W - inset} {H // 2} {W} {H}; sleep 200")
    x0 = view()
    uin(f"key leftmeta down; sleep 120; rel {dx} 0 20 10; key leftmeta up")
    time.sleep(1.5)
    return view() - x0


def toward_middle():
    """1 when the windows lie left of the screen's middle (a pan right brings them in), else -1."""
    lay = win("w2")["layout"]
    return 1 if lay["tile_pos_in_workspace_view"][0] + lay["window_size"][0] / 2 < W / 2 else -1


def t_pan_momentum():
    # Hot-reloaded: at pan-momentum 1 (the default) a 400 px Super pan let go
    # of mid-motion glides on (a fling); at 0 the view stops where the hand
    # stopped. Each pan heads for the windows, so the focus settling in the middle pulls nothing
    # back. Touchpad swipes move the focus here (no fling to keep) and still do at 0.
    focus("w1")
    d = toward_middle()
    glide = super_fling(400 * d, 300)
    reload_config("s|^\\( *\\)canvas {$|&\\n\\1    pan-momentum 0|")
    try:
        d = toward_middle()
        stop = super_fling(400 * d, 320)
        focus("w3")
        uin("swipe 3 -30 0")
        wait(lambda: focused() in ("w1", "w2"), "a three-finger swipe at pan-momentum 0")
    finally:
        reload_config("/pan-momentum 0/d")
    print(f"newm-vm: a 400 px Super pan let go of mid-motion moved the view {glide} px at "
          f"pan-momentum 1, {stop} px at 0")
    expect(abs(glide) > 600, f"pan-momentum 1: the view moved {glide} px, no fling past the hand's 400")
    expect(abs(abs(stop) - 400) <= 20, f"pan-momentum 0: the view moved {stop} px, not the hand's 400")


def usage():
    """newm-river's usage log, one JSON object per line."""
    text = sh("cat /home/alice/.local/state/newm-river/usage.jsonl 2>/dev/null || true")
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def visible(names, margin=200):
    """The first window of names whose top-left shows with margin px of it on screen."""
    for a in names:
        x, y, _, _ = on_screen(a)
        if 0 <= x < W - margin and 40 <= y < H - margin:
            return a
    return None


def t_escape_cancels_drag():
    focus("wev")
    time.sleep(0.6)
    x, y, _, _ = on_screen("wev")
    home = col("wev")
    esc, cancels, drops = wev_lines("sym: Escape"), count("pointer CancelDrag"), count("pointer Drop")
    logged = len(usage())
    uin(f"abs {x + 60} {y + 60} {W} {H}; key leftmeta down; sleep 40; button left down; sleep 40; "
        "rel 300 0; sleep 150")
    dragged = on_screen("wev")
    uin("tap esc; sleep 40; button left up; sleep 40; key leftmeta up")
    wait(lambda: count("pointer CancelDrag") > cancels, "Escape to cancel the drag")
    time.sleep(0.8)
    expect(dragged[0] - x >= 250, f"the drag moved wev {dragged[0] - x} px")
    expect(on_screen("wev")[:2] == (x, y), f"wev at {on_screen('wev')[:2]}, not home at {(x, y)}")
    expect(col("wev") == home, "the cancelled drag dropped wev")
    expect(count("pointer Drop") == drops, "a drop after Escape")
    expect(wev_lines("sym: Escape") == esc, "Escape reached wev")
    lines = usage()[logged:]
    cancel = next((l for l in lines if l.get("op") == "cancel"), None)
    expect(cancel is not None, f"no cancel in the usage log: {lines}")
    glide = next((l for l in lines if l.get("op") == "motion" and l.get("seq") == cancel["seq"]), None)
    expect(glide is not None, f"no motion line for the cancel: {lines}")
    w = glide["windows"]
    expect(w["max_path"]["v"] >= 250 and glide["frames"] >= 3 and w["max_jump"]["v"] < w["max_path"]["v"] / 2,
           f"wev did not glide home over several frames: {glide}")


def t_drag_preview():
    focus("w1")
    time.sleep(0.6)
    x, y, _, _ = on_screen("w1")
    other = visible(["w2", "w3", "wev"])
    expect(other is not None, "no other window on screen to drag onto")
    ox, oy, _, _ = on_screen(other)
    home = col("w1")
    uin(f"abs {x + 60} {y + 60} {W} {H}; key leftmeta down; sleep 40; button left down; sleep 40; "
        f"rel {ox - x} {oy - y}; sleep 100")
    try:
        wait(lambda: on_screen(other)[:2] != (ox, oy), f"the preview to move {other} out of w1's way", 5)
        shot("drag-preview")
        expect(col("w1") == home, "the tiles waited for the drop")
    finally:
        uin("tap esc; sleep 40; button left up; sleep 40; key leftmeta up")
    wait(lambda: on_screen(other)[:2] == (ox, oy), f"{other} back once the drag is cancelled")


def t_floating_resize():
    """Super+right on a floating window resizes it by the pixel (niri's floating interactive
    resize, Mod+right): from a corner its top-left stays, from its left edge its right
    edge stays and the app takes each size as the drag moves; Escape puts size and place back. The
    absolute mouse moves the cursor exactly; the drags go inward, so where it floats is no matter."""
    spawn("e2e_client window --app-id krunner --color c0c030 --size 600x400")
    wait(lambda: win("krunner") and win("krunner")["layout"]["window_size"][0] > 0, "krunner mapped")
    wait(lambda: win("krunner")["is_floating"], "krunner to float (open-floating)")
    time.sleep(0.6)
    try:
        x, y, w, h = on_screen("krunner")
        uin(f"abs {x + w - 15} {y + h - 15} {W} {H}; key leftmeta down; sleep 40; button right down; "
            f"sleep 40; abs {x + w - 45} {y + h - 35} {W} {H}; sleep 60; abs {x + w - 75} {y + h - 55} {W} {H}; "
            "sleep 200; button right up; sleep 40; key leftmeta up")
        wait(lambda: on_screen("krunner") == (x, y, w - 60, h - 40),
             f"krunner {on_screen('krunner')}: {(x, y, w, h)} resized by -60 -40 at its corner")
        x, y, w, h = on_screen("krunner")
        cancels = count("pointer CancelResize")
        uin(f"abs {x + 15} {y + h // 2} {W} {H}; key leftmeta down; sleep 40; button right down; "
            f"sleep 40; abs {x + 65} {y + h // 2} {W} {H}; sleep 300")
        try:
            wait(lambda: on_screen("krunner") == (x + 50, y, w - 50, h),
                 f"krunner {on_screen('krunner')}: its left edge 50 px in, its right edge staying", 5)
            shot("floating-resize")
        finally:
            uin("tap esc; sleep 40; button right up; sleep 40; key leftmeta up")
        wait(lambda: count("pointer CancelResize") > cancels, "Escape to cancel the resize")
        wait(lambda: on_screen("krunner") == (x, y, w, h),
             f"krunner {on_screen('krunner')}: Escape to put {(x, y, w, h)} back")
    finally:
        msg("action", "close-window", "--id", win("krunner")["id"])
        wait(lambda: not win("krunner"), "krunner closed")


def t_super_resize_in_the_first_moments():
    """Super down, the hand settling 20 px, and a right press inside the first 100 ms: the drag
    resizes the window from where the pointer settled, and the camera does not move."""
    spawn("e2e_client window --app-id krunner --color c0c030 --size 600x400")
    wait(lambda: win("krunner") and win("krunner")["layout"]["window_size"][0] > 0, "krunner mapped")
    wait(lambda: win("krunner")["is_floating"], "krunner to float (open-floating)")
    time.sleep(0.6)
    try:
        x, y, w, h = on_screen("krunner")
        pans, resizes = count("pointer Pan {"), count("pointer ResizeBegin")
        view = win("w1")["layout"]["tile_pos_in_workspace_view"]
        uin(f"abs {x + w - 15} {y + h - 15} {W} {H}; sleep 200; key leftmeta down; sleep 20; "
            f"abs {x + w - 35} {y + h - 15} {W} {H}; sleep 20; button right down; sleep 40; "
            f"abs {x + w - 65} {y + h - 35} {W} {H}; sleep 60; abs {x + w - 95} {y + h - 55} {W} {H}; "
            "sleep 200; button right up; sleep 40; key leftmeta up")
        wait(lambda: count("pointer ResizeBegin") > resizes, "Super+right to start a resize")
        wait(lambda: on_screen("krunner") == (x, y, w - 60, h - 40),
             f"krunner {on_screen('krunner')}: {(x, y, w, h)} resized by -60 -40 from where the hand settled")
        expect(count("pointer Pan {") == pans, "the hand settling panned the camera")
        expect(win("w1")["layout"]["tile_pos_in_workspace_view"] == view,
               f"the camera moved: w1 at {win('w1')['layout']['tile_pos_in_workspace_view']}, was {view}")
        shot("super-resize-first-moments")
    finally:
        msg("action", "close-window", "--id", win("krunner")["id"])
        wait(lambda: not win("krunner"), "krunner closed")


def t_double_resize_click():
    """A double Super+right click on a tiled window's right third (niri's double resize click,
    400 ms) spans the tiles the screen holds across, and again goes back to the fewest; undo takes
    both back."""
    focus("w2")
    time.sleep(0.6)

    def span():
        return win("w2")["layout"]["tile_size"][0]

    def dclick():
        x, y, w, h = on_screen("w2")
        uin(f"abs {x + w - 10} {y + h // 2} {W} {H}; key leftmeta down; sleep 40; "
            "button right down; sleep 30; button right up; sleep 100; "
            "button right down; sleep 30; button right up; sleep 40; key leftmeta up")

    w0 = span()
    dclick()
    wait(lambda: span() > W - 130, f"w2 {span()} px to span the screen ({W} px)")
    full = span()
    expect((full + 10) % 119 == 0, f"full width {full} px is no whole tiles")
    time.sleep(0.6)
    dclick()
    wait(lambda: span() < full, f"w2 {span()} px back from full width")
    msg("action", "undo")
    msg("action", "undo")
    wait(lambda: span() == w0, f"undo to take w2 {span()} px back to {w0}")


def magenta(p):
    r, g, b = p
    return r - g > 60 and b - g > 60


def share(img, box, pred):
    """The fraction of the pixels in box (physical px) for which pred holds."""
    x0, y0, x1, y1 = box
    pts = [(x, y) for y in range(max(y0, 0), min(y1, img.size[1]), 6)
           for x in range(max(x0, 0), min(x1, img.size[0]), 6)]
    return sum(1 for p in pts if pred(img.getpixel(p))) / max(len(pts), 1)


def t_open_close_animations():
    cfg = "/home/alice/.config/newm-river/config.kdl"
    sh(f"cp {cfg} /tmp/config.kdl.bak && printf 'animations {{\\n    slowdown 8\\n}}\\n' >> {cfg}")
    try:
        time.sleep(2)
        spawn("e2e_client window --app-id anim --color ff00ff")
        wait(lambda: win("anim") and win("anim")["layout"]["window_size"][0] > 0, "anim mapped", 10)
        opening = shot("anim-opening")
        time.sleep(2.5)
        x, y, w, h = on_screen("anim")
        s = opening.size[0] // W
        box = (x * s, y * s, (x + w) * s, (y + h) * s)
        opened = shot("anim-open")
        expect(share(opened, box, magenta) > 0.8, "the settled anim window is not drawn magenta")
        expect(changed(opening, opened, box) > 0.1, "no open animation: the shot taken while it opened "
               "is the settled frame")
        msg("action", "close-window", "--id", win("anim")["id"])
        closing = shot("anim-closing")
        time.sleep(2.5)
        closed = shot("anim-closed")
        expect(share(closed, box, magenta) < 0.02, "the closed window is still drawn")
        expect(share(closing, box, magenta) > 0.1, "no close animation: nothing of the window drawn while "
               "it closed")
    finally:
        sh(f"cp /tmp/config.kdl.bak {cfg}")
        time.sleep(2)


def ff_file():
    return alice('cat "$NIRI_SOCKET.floating-follow"').strip()


def t_floating_follow():
    expect(ff_file() == "screen", f"the floating-follow file says {ff_file()!r}, not screen")
    open_window("ffp", "3060c0", "--dialog ffd --dialog-size 300x200")
    wait(lambda: win("ffd") and win("ffd")["layout"]["window_size"][0] > 0, "the dialog mapped")
    try:
        msg("action", "cycle-floating-follow")
        wait(lambda: ff_file() == "parent", "cycle-floating-follow to write parent")
        focus("ffp")
        time.sleep(0.6)
        p0, d0 = on_screen("ffp"), on_screen("ffd")
        uin(f"abs {W // 2} {H // 2} {W} {H}; key leftmeta down; sleep 120; rel -200 0; sleep 300; "
            "key leftmeta up")
        time.sleep(1.5)
        p1, d1 = on_screen("ffp"), on_screen("ffd")
        expect(abs(p1[0] - p0[0]) >= 100, f"the pan moved the parent {p1[0] - p0[0]} px")
        expect(abs((d1[0] - d0[0]) - (p1[0] - p0[0])) <= 2 and abs((d1[1] - d0[1]) - (p1[1] - p0[1])) <= 2,
               f"parent mode: the dialog moved {(d1[0] - d0[0], d1[1] - d0[1])}, its parent "
               f"{(p1[0] - p0[0], p1[1] - p0[1])}")
        shot("floating-follow-parent")
        msg("action", "set-floating-follow", "screen")
        wait(lambda: ff_file() == "screen", "set-floating-follow to write screen")
        expect(on_screen("ffd")[:2] == d1[:2], "the dialog left where it was drawn")
        focus("ffp")
        uin(f"abs {W // 2} {H // 2} {W} {H}; key leftmeta down; sleep 120; rel 200 0; sleep 300; "
            "key leftmeta up")
        time.sleep(1.5)
        p2 = on_screen("ffp")
        expect(abs(p2[0] - p1[0]) >= 100, f"the second pan moved the parent {p2[0] - p1[0]} px")
        expect(on_screen("ffd")[:2] == d1[:2], "screen mode: the dialog followed the pan")
        msg("action", "set-floating-follow", "canvas")
        wait(lambda: ff_file() == "canvas", "set-floating-follow to write canvas")
        msg("action", "zoom-out")
        time.sleep(0.8)
        px, py, pw, ph = on_screen("ffp")
        dx, dy, dw, dh = on_screen("ffd")
        s = 0.8
        pc = (px + pw * s / 2, py + ph * s / 2)
        dc = (dx + dw * s / 2, dy + dh * s / 2)
        before = (d1[0] + d1[2] / 2 - (p2[0] + p2[2] / 2), d1[1] + d1[3] / 2 - (p2[1] + p2[3] / 2))
        after = (dc[0] - pc[0], dc[1] - pc[1])
        expect(abs(after[0] - before[0] * s) <= 3 and abs(after[1] - before[1] * s) <= 3,
               f"canvas mode: the dialog's offset to its parent went {before} -> {after}, not x{s}")
        shot("floating-follow-canvas")
        msg("action", "zoom-in")
    finally:
        msg("action", "set-floating-follow", "screen")
        sh("pkill -f 'app-id [f]fp' || true")
        time.sleep(0.5)


def pick_query():
    """Starts Spectacle's queryWindowInfo in the background; its reply lands in /tmp/pick.*."""
    sh("rm -f /tmp/pick.json /tmp/pick.rc")
    q = ("busctl --user --json=short --timeout=20 call org.kde.KWin /KWin org.kde.KWin "
         "queryWindowInfo > /tmp/pick.json 2>&1; echo $? > /tmp/pick.rc")
    alice(f"({q}) >/dev/null 2>&1 &")
    time.sleep(1)
    expect(sh("test -e /tmp/pick.rc && echo done || echo waiting").strip() == "waiting",
           "queryWindowInfo answered before any click: no pick step")


def t_query_window_pick():
    focus("w1")
    time.sleep(0.4)
    other = visible(["w2", "w3", "wev"])
    expect(other is not None, "no other window on screen to pick")
    x, y, _, _ = on_screen(other)
    pick_query()
    clicks = wev_lines("button: ")
    uin(f"abs {x + 40} {y + 40} {W} {H}; click")
    wait(lambda: sh("cat /tmp/pick.rc 2>/dev/null || true").strip() != "", "the click to answer the pick")
    reply = sh("cat /tmp/pick.json")
    expect(sh("cat /tmp/pick.rc").strip() == "0", f"queryWindowInfo failed: {reply}")
    app = json.loads(reply)["data"][0]["resourceClass"]["data"]
    expect(app == other, f"the pick answered {app}, not the clicked {other}")
    expect(wev_lines("button: ") == clicks, "the pick's click reached wev")
    pick_query()
    uin("tap esc")
    wait(lambda: sh("cat /tmp/pick.rc 2>/dev/null || true").strip() != "", "Escape to end the pick")
    reply = sh("cat /tmp/pick.json")
    # busctl prints the error's message, not its name (org.kde.KWin.Error.UserCancel).
    expect(sh("cat /tmp/pick.rc").strip() != "0" and "cancelled" in reply,
           f"Escape did not cancel the pick: {reply}")


def t_rounded_corners():
    focus("w1")
    time.sleep(0.4)
    x, y, w, h = on_screen("w1")
    img = shot("corners")
    s = img.size[0] // W
    cx, cy = (x + w // 2) * s, (y + h // 2) * s
    inner = img.getpixel((cx, cy))
    # The drawn edges, walked out from the middle: the IPC gives the tiles, and an app that picked
    # a size smaller than its tiles (open-size) is not drawn at their corner.
    left, top = cx, cy
    while left > 0 and img.getpixel((left - 1, cy)) == inner:
        left -= 1
    while top > 0 and img.getpixel((cx, top - 1)) == inner:
        top -= 1
    corner = img.getpixel((left + 1, top + 1))
    edge = img.getpixel((left + 1, cy))
    expect(edge == inner, f"the window's left edge at {left} is not its colour {inner}: {edge}")
    expect(corner != inner, f"the corner pixel {corner} at ({left + 1}, {top + 1}) is the window's own {inner}: no 8 px radius")


def t_config_reload():
    sh("sed -i 's|^binds {$|binds {\\n    Mod+z { spawn-sh \"touch /tmp/reloaded\"; }|' "
       "/home/alice/.config/newm-river/config.kdl")
    expect("Mod+z" in sh("cat /home/alice/.config/newm-river/config.kdl"), "the bind went in")
    time.sleep(2)
    uin("key leftmeta down; sleep 30; tap z; sleep 30; key leftmeta up")
    wait(lambda: machine.execute("test -e /tmp/reloaded")[0] == 0, "a bind added to the config", 5)


def pinches():
    """Each pinch the window manager zoomed with, as (zoom it began at, zoom it reached, the last
    scale river reported for it)."""
    import re
    log = journal()
    ends = [(int(a), int(b)) for a, b in re.findall(r"pinch zoomed (-?\d+) to (-?\d+)", log)]
    scales = [float(x) for x in re.findall(r"pointer PinchScale\(([-0-9.e]+)\)", log)]
    return [(a, b, scales[-1] if scales else 1.0) for a, b in ends]


def pinch_target(start, scale):
    """The zoom a pinch reaches (model::moves::pinch_zoom) with this config's zoom-step 0.8,
    zoom-min -2 and zoom-max 7."""
    import math
    return max(-2, min(7, start + round(math.log(scale) / math.log(0.8))))


def pinch_log(pattern):
    return int(sh(f"grep -c {shlex.quote(pattern)} /tmp/pinch.log || true").strip() or 0)


def t_pinch3_zoom():
    msg("action", "zoom-reset")
    time.sleep(0.5)
    n = len(pinches())
    uin("pinch 3 20 12")
    wait(lambda: len(pinches()) > n, "a three-finger pinch in to zoom")
    start, end, scale = pinches()[-1]
    print(f"pinch in: zoom {start} to {end} at scale {scale}")
    expect(start == 0, f"the pinch began at zoom {start}, not 0")
    expect(end == pinch_target(0, scale), f"scale {scale} reached zoom {end}, not {pinch_target(0, scale)}")
    expect(end >= 1, f"pinching in to 0.6 of the spread pulled the camera back {end} steps")
    shot("pinch3-zoomed-out")
    uin("pinch 3 10 20")
    wait(lambda: len(pinches()) > n + 1, "a three-finger spread to zoom")
    start2, end2, scale2 = pinches()[-1]
    print(f"spread: zoom {start2} to {end2} at scale {scale2}")
    expect(start2 == end, f"the spread began at zoom {start2}, not where the pinch left it ({end})")
    expect(end2 == pinch_target(start2, scale2), f"scale {scale2} reached zoom {end2}, not {pinch_target(start2, scale2)}")
    expect(end2 < start2, f"spreading the fingers twice as far apart did not zoom in: {start2} to {end2}")
    msg("action", "zoom-reset")
    time.sleep(0.5)


def t_pinch2_reaches_app():
    msg("action", "zoom-reset")
    open_window("pinchy", "c0c030", "--size 690x430 --log-pinch > /tmp/pinch.log 2>&1")
    focus("pinchy")
    time.sleep(0.6)
    x, y, w, h = on_screen("pinchy")
    uin(f"abs {x + w // 2} {y + h // 2} {W} {H}")
    n, begins = len(pinches()), count("pointer PinchBegin")
    uin("pinch 2 15 8")
    wait(lambda: pinch_log("pinch-begin 2") > 0, "a plain two-finger pinch to reach the app")
    wait(lambda: pinch_log("pinch-end") > 0, "the app to see the pinch end")
    expect(pinch_log("pinch-update") > 0, "the app saw no pinch updates")
    expect(len(pinches()) == n and count("pointer PinchBegin") == begins,
           "a plain two-finger pinch zoomed the camera")


def t_super_pinch2_zoom():
    n, seen = len(pinches()), pinch_log("pinch-begin")
    taps = count("pointer Act(ToggleOverview)")
    uin("key leftmeta down; sleep 30; pinch 2 15 9; sleep 30; key leftmeta up")
    wait(lambda: len(pinches()) > n, "Super and a two-finger pinch to zoom")
    start, end, scale = pinches()[-1]
    print(f"Super pinch: zoom {start} to {end} at scale {scale}")
    expect(end == pinch_target(start, scale) and end > start,
           f"Super and a pinch in to {scale} went from zoom {start} to {end}")
    expect(pinch_log("pinch-begin") == seen, "the app saw the pinch Super held")
    expect(count("pointer Act(ToggleOverview)") == taps, "Super with a pinch was a tap")
    msg("action", "zoom-reset")


def t_display_scale():
    # Each connected display's scale is a live setting. A set draws the
    # display at the new scale, a reset gives it its own back.
    key = f"output.{o['name']}.scale"
    scale = lambda: output()["logical"]["scale"]
    width = lambda: output()["logical"]["width"]
    own = scale()
    listed = [x for x in js("newm-options") if x["key"] == key]
    expect(listed and listed[0]["category"] == "Displays" and listed[0]["value"] == own
           and not listed[0]["overridden"], f"{key} is not listed at scale {own}: {listed}")
    try:
        msg("newm-set-option", key, own * 2)
        wait(lambda: scale() == own * 2 and width() == W // 2,
             f"the display to be drawn at scale {own * 2}, {W // 2} px wide")
        listed = [x for x in js("newm-options") if x["key"] == key][0]
        expect(listed["overridden"] and listed["value"] == own * 2 and listed["config_value"] == own,
               f"the overridden scale is listed as {listed}")
        shot("display-scale-doubled")
    finally:
        msg("newm-reset-option", key)
    wait(lambda: scale() == own and width() == W, f"a reset to give the display its scale {own} back")
    listed = [x for x in js("newm-options") if x["key"] == key][0]
    expect(not listed["overridden"] and listed["value"] == own, f"after the reset: {listed}")


spawn("stdbuf -oL wev -f wl_pointer -f wl_keyboard > /tmp/wev.log 2>&1")
wait(lambda: win("wev"), "wev mapped")

for name, fn in [
    ("session-on-drm", t_session_on_drm),
    ("super-tap", t_super_tap),
    ("super-tap-ctrl", t_super_tap_ctrl),
    ("super-tap-unbound-key", t_super_tap_unbound_key),
    ("swipe3-focus", t_swipe3_focus),
    ("swipe4-focus", t_swipe4_focus),
    ("swipe4-zoom", t_swipe4_zoom),
    ("super-swipe3-pan", t_super_swipe3_pan),
    ("super-swipe4-move", t_super_swipe4_move),
    ("super-wheel-zoom", t_super_wheel_zoom),
    ("super-2fg-scroll-no-zoom", t_super_2fg_scroll_no_zoom),
    ("2fg-scroll-reaches-app", t_2fg_scroll_reaches_app),
    ("super-2fg-scroll-reaches-app", t_super_2fg_scroll_reaches_app),
    ("minimap", t_minimap),
    ("zoomed-click", t_zoomed_click),
    ("alt-tab", t_alt_tab),
    ("middle-button-scroll", t_middle_button_scroll),
    ("pan-past-edge", t_pan_past_edge),
    ("pan-acceleration", t_pan_acceleration),
    ("pan-momentum", t_pan_momentum),
    ("rounded-corners", t_rounded_corners),
    ("escape-cancels-drag", t_escape_cancels_drag),
    ("drag-preview", t_drag_preview),
    ("floating-resize", t_floating_resize),
    ("super-resize-first-moments", t_super_resize_in_the_first_moments),
    ("double-resize-click", t_double_resize_click),
    ("open-close-animations", t_open_close_animations),
    ("floating-follow", t_floating_follow),
    ("query-window-pick", t_query_window_pick),
    ("config-reload", t_config_reload),
    ("pinch3-zoom", t_pinch3_zoom),
    ("pinch2-reaches-app", t_pinch2_reaches_app),
    ("super-pinch2-zoom", t_super_pinch2_zoom),
    ("display-scale", t_display_scale),
]:
    case(name, fn)

shot("input-end")
print(journal()[-6000:])
finish()
