
# ---- vm-session: TTY login, Xwayland, restore, HiDPI, two outputs, logout (nix/vm/session.py) --
start_all()
wait_session()
STATE = "/home/alice/.local/state/newm-river/session.json"
CONFIG = "/home/alice/.config/newm-river/config.kdl"
saved = {}
xapp = {}


def by_title(prefix):
    for w in windows():
        if (w.get("title") or "").startswith(prefix):
            return w
    return None


def t_tty_session():
    pid = river_pid()
    expect("/dev/dri/card" in sh(f"ls -l /proc/{pid}/fd"), "river holds no DRM card")
    sid = sh("loginctl show-user alice -p Display --value").strip()
    info = sh(f"loginctl show-session {sid} -p TTY -p Type -p Seat")
    expect("TTY=tty1" in info, f"the session is not tty1's: {info}")
    expect("not restoring nor saving" not in journal(), "newm-river took a TTY river for nested")
    print(sh("for c in /sys/class/drm/card*-*; do echo $c $(cat $c/status); done"))


def t_dmabuf_feedback():
    # examples/feedback_probe.rs, a window committing a new colour every 16 ms: one linux-dmabuf
    # feedback when it maps, then none per commit or frame: at rest, while the camera zooms and
    # around the overview (at most 3 in each 5 s step).
    sh("rm -f /tmp/fb.json /tmp/fb.log /tmp/fb.rc")
    spawn("feedback_probe --app-id fbprobe --counts /tmp/fb.json > /tmp/fb.log 2>&1; echo $? > /tmp/fb.rc")
    wait(lambda: machine.execute("test -e /tmp/fb.rc")[0] == 0
         or (win("fbprobe") and win("fbprobe")["layout"]["window_size"][0] > 0), "fbprobe mapped")
    try:
        if machine.execute("test -e /tmp/fb.rc")[0] == 0:
            raise Fail(f"feedback_probe ended: {sh('cat /tmp/fb.rc /tmp/fb.log')}")

        def counts():
            return json.loads(sh("cat /tmp/fb.json"))

        time.sleep(1)
        mapped = counts()
        expect(mapped["done"] >= 1, f"no dmabuf feedback after the map: {mapped}")

        def step(name, act):
            b = counts()
            act()
            a = counts()
            d, lv, c = (a[k] - b[k] for k in ("done", "leave", "commits"))
            print(f"newm-vm: feedback {name}: {d} feedbacks, {lv} leaves, {c} commits", flush=True)
            expect(c >= 100, f"feedback_probe committed {c} frames {name}")
            expect(d <= 3, f"{d} dmabuf feedbacks {name} (at most 3): {a}")
            expect(lv == 0, f"{lv} wl_surface.leave {name}")

        def zoom():
            for _ in range(5):
                msg("action", "zoom-out")
                time.sleep(0.5)
                msg("action", "zoom-in")
                time.sleep(0.5)

        def overview():
            msg("action", "toggle-overview")
            time.sleep(2.5)
            msg("action", "toggle-overview")
            time.sleep(2.5)

        step("at rest", lambda: time.sleep(5))
        step("while the camera zooms", zoom)
        step("around the overview", overview)
        print(f"newm-vm: feedback totals {counts()}", flush=True)
    finally:
        sh("pkill -x feedback_probe || true")
        wait(lambda: not win("fbprobe"), "fbprobe closed")


def t_xwayland_xterm():
    # -e keeps bash's prompt from retitling it.
    spawn("xterm -title rs-xterm -e sleep 100000 > /tmp/xterm.log 2>&1")
    w = wait(lambda: next((w for w in windows() if "xterm" in (w.get("app_id") or "").lower()), None),
             "the X11 xterm to map", 30)
    print(f"xterm window {w}")
    xapp.update(id=w.get("app_id"), title=w.get("title"))
    expect(machine.execute("pgrep -u alice Xwayland")[0] == 0, "no Xwayland")
    spawn("foot --app-id rs-foot")
    wait(lambda: win("rs-foot"), "foot to map", 30)
    time.sleep(0.5)
    shot("xterm-and-foot")


def t_xwayland_title():
    expect(xapp.get("title") == "rs-xterm", f"the X11 window's title is {xapp.get('title')!r}")


def t_session_saved():
    def has_both():
        out = machine.execute(f"cat {STATE}")[1]
        return "rs-foot" in out and (xapp.get("id") or "rs-xterm") in out
    wait(has_both, "session.json to list foot and the xterm", 20)
    expect(sh(f"stat -c %a {STATE}").strip() == "600", "session.json is not 0600")
    for w in windows():
        saved[w["app_id"]] = w["layout"]["pos_in_scrolling_layout"]
    print(f"saved {saved}")


def t_wm_crash_restart():
    """A SIGKILL to the window manager (newm-river's child, supervise.rs): the unit, river and
    the apps stay; the restarted window manager puts every window back on its spot."""
    pairs = sh("for p in $(pgrep -u alice -x newm-river); do echo $p $(awk '{print $4}' /proc/$p/stat); done")
    parents = dict(tuple(map(int, line.split())) for line in pairs.splitlines() if line.strip())
    children = [p for p, pp in parents.items() if pp in parents]
    expect(len(children) == 1, f"no single window manager child of the supervisor: {parents}")
    before = {w["app_id"]: w["layout"]["pos_in_scrolling_layout"] for w in windows()}
    rpid = river_pid()
    sh(f"kill -9 {children[0]}")
    wait(lambda: count("taking over the running session") >= 1, "the restarted window manager", 30)
    wait(lambda: alice_try("newm-river msg version")[0] == 0, "newm-river's IPC after the restart", 30)
    expect(river_pid() == rpid, "river did not keep running")
    expect(machine.execute("as-alice 'systemctl --user is-active newm-river.service'")[0] == 0,
           "newm-river.service stopped with its window manager")
    wait(lambda: {w["app_id"]: w["layout"]["pos_in_scrolling_layout"] for w in windows()} == before,
         f"every window back on its spot {before}", 15)
    shot("wm-restarted")


def t_logout_quit():
    old = sh("loginctl show-user alice -p Display --value").strip()
    logout()
    # The next login reads this: the window manager applies an output block's scale.
    sh(f"printf 'output \"Virtual-1\" {{\\n    scale 2\\n}}\\n' >> {CONFIG}")
    wait(lambda: sh(f"loginctl show-session {old} -p State --value 2>/dev/null || echo gone").strip()
         != "active", f"the tty1 session {old} to end", 20)


def t_restore():
    login()
    wait(lambda: count("restoring the session") >= 1, "the restore at login", 30)
    want = [a for a in saved if a in ("rs-foot", xapp.get("id"))]
    expect(want, f"nothing to compare: {saved}")
    for app in want:
        wait(lambda: win(app), f"{app} relaunched", 40)
    time.sleep(2.5)
    got = {a: win(a)["layout"]["pos_in_scrolling_layout"] for a in want}
    shot("restored")
    expect(all(got[a] == saved[a] for a in want), f"tiles moved: saved {saved}, restored {got}")


def t_config_output_scale():
    scale = output()["logical"]["scale"]
    expect(scale == 2, f"the config's output block (scale 2) left scale {scale}")


def t_hidpi_scale2():
    alice(f"wlr-randr --output {output()['name']} --scale 2")
    wait(lambda: output()["logical"]["width"] == 1280, "scale 2: a 1280 px wide logical output")
    time.sleep(0.5)
    shot("hidpi")


def t_hidpi_border():
    open_window("hb", "3030c0")
    time.sleep(0.6)
    w = win("hb")
    x, y = (int(v) for v in w["layout"]["tile_pos_in_workspace_view"])
    img = shot("hidpi-border")
    s = img.size[0] // output()["logical"]["width"]
    expect(s == 2, f"the shot is {img.size}: not 2 px per logical px")
    my = (y + 60) * s
    row = [img.getpixel((px, my)) for px in range(max(x * s - 8, 0), x * s + 12)]
    first = next((i for i, p in enumerate(row) if p == (0x30, 0x30, 0xC0)), None)
    print(f"hb at {x},{y}: pixels across its left edge {row}")
    expect(first is not None, "no window pixels near its left edge")
    border = 0
    while first - border - 1 >= 0 and row[first - border - 1][1] >= 120:
        border += 1
    expect(border == 2, f"the border is {border} physical px wide at scale 2: {row}")


def t_hidpi_placeholder():
    for w in windows():
        msg("action", "close-window", "--id", w["id"])
    wait(lambda: not windows(), "every window closed", 20)
    alice("e2e_client png /tmp/icon.png 64x64 0000ff")
    msg("action", "launch-placeholder", "--app-id", "ph-app", "--icon", "/tmp/icon.png")
    time.sleep(0.6)
    img = shot("hidpi-placeholder")
    xs, ys = [], []
    for y in range(0, img.size[1], 2):
        for x in range(0, img.size[0], 2):
            if img.getpixel((x, y)) == (0, 0, 255):
                xs.append(x)
                ys.append(y)
    expect(xs, "no icon pixels at all")
    bw, bh = max(xs) - min(xs) + 2, max(ys) - min(ys) + 2
    print(f"placeholder icon {bw}x{bh} at {min(xs)},{min(ys)}")
    expect(abs(bw - bh) <= 4 and bw >= 80, f"the icon is {bw}x{bh} physical px")


def t_two_outputs():
    logout()
    if sh("cat /sys/class/drm/card0-Virtual-2/status || true").strip() != "connected":
        # QEMU enables a virtio head only when a display UI asks for it, and tests run without
        # one: the second output is wlroots' headless one, beside the DRM one.
        sh("echo 'export WLR_BACKENDS=drm,libinput,headless WLR_HEADLESS_OUTPUTS=1' "
           "> /home/alice/.config/newm-vm/env")
    login()
    wait(lambda: len(js("outputs")) == 2, "two outputs", 30)
    outs = sorted(js("outputs").values(), key=lambda o: o["logical"]["x"])
    print(f"outputs {[(o['name'], o['logical']) for o in outs]}")
    expect(outs[1]["logical"]["x"] >= outs[0]["logical"]["width"], f"overlapping outputs {outs}")
    lw = max(o["logical"]["x"] + o["logical"]["width"] for o in outs)
    lh = max(o["logical"]["y"] + o["logical"]["height"] for o in outs)
    second = outs[1]
    sx = second["logical"]["x"] + second["logical"]["width"] // 2
    sy = second["logical"]["y"] + second["logical"]["height"] // 2
    img = shot("second-output", second["name"])
    px = img.getpixel((img.size[0] // 4, img.size[1] // 2))
    expect(px == (0x20, 0x30, 0x40), f"{second['name']} does not show the wallpaper: {px}")
    shot("two-outputs")
    two_outputs.update(second=second, lw=lw, lh=lh)


two_outputs = {}


def t_click_empty_output():
    """niri focuses the output a click lands on, so the next window opens there."""
    second, lw, lh = two_outputs["second"], two_outputs["lw"], two_outputs["lh"]
    sx = second["logical"]["x"] + second["logical"]["width"] // 2
    sy = second["logical"]["y"] + second["logical"]["height"] // 2
    uin(f"abs {sx} {sy} {lw} {lh}; click")
    time.sleep(0.4)
    open_window("o2", "30c030")
    time.sleep(0.6)
    img = shot("second-output-window", second["name"])
    px = img.getpixel((img.size[0] // 4, img.size[1] // 2))
    expect(px == (0x30, 0xC0, 0x30), f"o2 did not open on {second['name']}: {px}")


def t_logout_ends_session():
    logout()
    expect(machine.execute("pgrep -u alice -x river")[0] != 0, "river still runs")
    st = machine.execute("as-alice 'systemctl --user is-active graphical-session.target'")[1]
    expect(st.strip() != "active", f"graphical-session.target is {st.strip()}")


for name, fn in [
    ("tty-session", t_tty_session),
    ("dmabuf-feedback", t_dmabuf_feedback),
    ("xwayland-xterm", t_xwayland_xterm),
    ("xwayland-title", t_xwayland_title),
    ("session-saved", t_session_saved),
    ("wm-crash-restart", t_wm_crash_restart),
    ("logout-quit", t_logout_quit),
    ("restore", t_restore),
    ("config-output-scale", t_config_output_scale),
    ("hidpi-scale2", t_hidpi_scale2),
    ("hidpi-border", t_hidpi_border),
    ("hidpi-placeholder", t_hidpi_placeholder),
    ("two-outputs", t_two_outputs),
    ("click-empty-output", t_click_empty_output),
    ("logout-ends-session", t_logout_ends_session),
]:
    case(name, fn)

print(journal()[-6000:])
finish()
