
# ---- vm-lock: newm-river-lock, PAM, swayidle and wlopm in a TTY session (nix/vm/lock.py) ---------
start_all()
wait_session()
open_window("w1", "c03030")
time.sleep(0.5)


def session_id():
    return sh("loginctl show-user alice -p Display --value").strip()


def locked():
    return sh(f"loginctl show-session {session_id()} -p LockedHint --value").strip() == "yes"


def locker_running():
    return machine.execute("pgrep -u alice -x newm-river-lock")[0] == 0


def screenshot2():
    return alice_try("busctl --user call org.kde.KWin.ScreenShot2 /org/kde/KWin/ScreenShot2 "
                     "org.kde.KWin.ScreenShot2 CaptureActiveScreen 'a{sv}h' 0 3 3>/tmp/shot.raw 2>&1")


def power():
    return alice("wlopm")


def unlock():
    uin(f"type {PASSWORD}; tap enter")
    wait(lambda: not locked() and not locker_running(), "the right password to unlock", 20)


def t_screenshot2_unlocked():
    st, out = screenshot2()
    expect(st == 0, f"ScreenShot2 refused while unlocked: {out}")


def t_lock_session():
    expect(not locked(), "LockedHint set before the lock")
    sh("loginctl lock-sessions")
    wait(locker_running, "swayidle's lock handler to start newm-river-lock", 20)
    wait(locked, "newm-river-lock to set LockedHint", 20)
    time.sleep(1)
    img = shot("locked")
    drm_shot("locked-drm")
    cx, cy = img.size[0] // 2, img.size[1] // 2
    expect(img.getpixel((40, img.size[1] - 40)) != (0xc0, 0x30, 0x30), "w1 still shows when locked")
    print(f"lock screen centre pixel {img.getpixel((cx, cy))}")


def t_screenshot2_refused_locked():
    st, out = screenshot2()
    expect(st != 0 and ("NoAuthorized" in out or "locked" in out), f"ScreenShot2 while locked answered {st}: {out}")


def t_wrong_password():
    uin("type wrongpass; tap enter")
    time.sleep(3)
    expect(locked() and locker_running(), "a wrong password unlocked")
    shot("locked-wrong-password")


def t_unlock_password():
    unlock()
    st, out = screenshot2()
    expect(st == 0, f"ScreenShot2 still refused after the unlock: {out}")


def t_super_escape_locks():
    uin("key leftmeta down; sleep 30; tap esc; sleep 30; key leftmeta up")
    wait(locked, "Super+Escape (loginctl lock-session) to lock", 20)
    unlock()


def t_idle_lock_and_monitors():
    spawn("swayidle -w timeout 4 'newm-river-lock --daemonize' "
          "timeout 8 \"wlopm --off '*'\" resume \"wlopm --on '*'\" > /tmp/swayidle-test.log 2>&1")
    wait(locked, "swayidle's 4 s idle timeout to lock", 25)
    wait(lambda: " off" in power(), "swayidle's 8 s idle timeout to turn the monitors off", 25)
    drm_shot("monitors-off-drm")
    uin("tap leftshift")
    wait(lambda: " on" in power(), "input to turn the monitors back on", 15)
    shot("idle-locked")
    unlock()
    sh("pkill -u alice -f 'swayidle -w timeout 4' || true")


def tiles():
    """Every window's tile spot and size, by id."""
    return {w["id"]: (w["layout"]["pos_in_scrolling_layout"], w["layout"]["tile_size"])
            for w in windows()}


def t_wlopm_off_on():
    before = tiles()
    expect(before, "no window to watch")
    alice("wlopm --off '*'")
    wait(lambda: " off" in power(), "wlopm --off")
    expect(tiles() == before, f"windows changed while the monitors were off: {tiles()} vs {before}")
    alice("wlopm --on '*'")
    wait(lambda: " on" in power(), "wlopm --on")
    time.sleep(0.5)
    img = shot("after-wlopm-on")
    expect(img.getpixel((img.size[0] // 4, img.size[1] // 2)) != (0, 0, 0), "a black screen after --on")
    expect(tiles() == before, f"windows moved across the monitors' off and on: {tiles()} vs {before}")


def t_randr_off_on():
    """The only output turned off with wlr-output-management (gone, not just dark, as a
    DisplayPort monitor that unplugs itself when it sleeps): the windows wait on their tiles and
    come back with it."""
    before = tiles()
    out = js("outputs")
    name = next(iter(out))
    alice(f"wlr-randr --output {name} --off")
    wait(lambda: not js("outputs").get(name, {}).get("logical"), f"{name} off", 15)
    time.sleep(1)
    gone = {w["id"] for w in windows()}
    expect(gone == set(before), f"windows left the list while {name} was off: {gone} vs {set(before)}")
    alice(f"wlr-randr --output {name} --on")
    wait(lambda: js("outputs").get(name, {}).get("logical"), f"{name} on", 15)
    time.sleep(1)
    expect(tiles() == before, f"windows moved across {name}'s off and on: {tiles()} vs {before}")
    img = shot("after-randr-on")
    expect(img.getpixel((img.size[0] // 4, img.size[1] // 2)) != (0, 0, 0), "a black screen after --on")


KDL = "/home/alice/.config/newm-river/config.kdl"


def river_pid():
    return sh("pgrep -x river || true").strip()


def t_randr_off_on_block():
    """The only output off and on (wlr-randr) three times while the config's output block moves
    and scales it, as a desktop's DP-1 block does (position away from the origin, scale 1.25), which
    newm-river applies again each time the output returns. river must survive every round (a
    headless river aborted here in wlroots' head_send_state) and the windows keep their tiles."""
    name = next(iter(js("outputs")))
    pid = river_pid()
    expect(pid, "no river process")
    before_reload = count("config reloaded")
    sh(f"cp {KDL} /tmp/config.kdl.block.bak")
    block = f'output "{name}" {{\n    position x=3968 y=0\n    scale 1.25\n}}\n'
    sh(f"cat >> {KDL} << 'NEWMEOF'\n{block}NEWMEOF")
    try:
        wait(lambda: count("config reloaded") > before_reload, "newm-river to reload the config")
        wait(lambda: abs(js("outputs")[name]["logical"]["scale"] - 1.25) < 1e-6, f"{name} at scale 1.25")
        time.sleep(1)
        before = tiles()
        for r in range(1, 4):
            alice(f"wlr-randr --output {name} --off")
            wait(lambda: not js("outputs").get(name, {}).get("logical"), f"round {r}: {name} off", 15)
            time.sleep(0.5)
            alice(f"wlr-randr --output {name} --on")
            wait(lambda: js("outputs").get(name, {}).get("logical"), f"round {r}: {name} on", 15)
            time.sleep(1)
            expect(river_pid() == pid, f"round {r}: river restarted or died ({pid} -> {river_pid()})")
            expect(count("Assertion") == 0, f"round {r}: an assertion in the journal")
            expect(tiles() == before, f"round {r}: windows moved: {tiles()} vs {before}")
            logical = js("outputs")[name]["logical"]
            expect(abs(logical["scale"] - 1.25) < 1e-6, f"round {r}: scale {logical['scale']} after --on")
        img = shot("after-block-off-on", name)
        expect(img.getpixel((img.size[0] // 4, img.size[1] // 2)) != (0, 0, 0), "a black screen after the last --on")
    finally:
        before_reload = count("config reloaded")
        sh(f"cp /tmp/config.kdl.block.bak {KDL}")
        wait(lambda: count("config reloaded") > before_reload, "newm-river to reload the restored config")


for name, fn in [
    ("screenshot2-unlocked", t_screenshot2_unlocked),
    ("lock-session", t_lock_session),
    ("screenshot2-refused-locked", t_screenshot2_refused_locked),
    ("wrong-password", t_wrong_password),
    ("unlock-password", t_unlock_password),
    ("super-escape-locks", t_super_escape_locks),
    ("wlopm-off-on", t_wlopm_off_on),
    ("randr-off-on", t_randr_off_on),
    ("randr-off-on-block", t_randr_off_on_block),
    ("idle-lock-and-monitors", t_idle_lock_and_monitors),
]:
    case(name, fn)

print(journal()[-4000:])
finish()
