# ---- vm-reattach: Ghostty windows come back in their tmux session or ssh (nix/vm/reattach.py) --
# Four Ghostty windows, opened and typed into as a user does: (a) local tmux session work, (b) ssh
# far attaching tmux far, (b2) ssh far attaching tmux far2 in /tmp, (c) a plain shell. The session
# saves, the bar's logout ends it (Ghostty and its shells die with it; the tmux servers live on),
# and every login must bring each window back on its saved spot, attached to its session. The ssh
# master outlives the logout in Ghostty's surface scope, so the first login asks for no key; before
# the second the master goes, as after a reboot, and that login must ask once for both windows to
# far. Twice also proves the second restore saves what the first relaunched. A foot window (a
# process per window, relaunched as `foot --working-directory=DIR CMD`) attached to local tmux
# session ft in /tmp comes back the same way.
start_all()
far.wait_for_unit("sshd.service")
wait_session()
STATE = "/home/alice/.local/state/newm-river/session.json"
GHOSTTY = "com.mitchellh.ghostty"
TMUX = "TMUX_TMPDIR=/run/user/1000 tmux"
HOME = "/home/alice"
# The windows by the title tmux and fish_title give them, and what each runs.
WINDOWS = {
    "machine: ~": "tmux new-session -A -s work;",
    "far: ~": "ssh -t far tmux new-session -A -s far;",
    "far: /tmp": "ssh -t far tmux new-session -A -s far2;",
    "~": None,
}
FOOT = "foot"
# The foot window's title (tmux in /tmp) and what it runs.
FOOT_TITLE = "machine: /tmp"
FOOT_RUNS = "tmux new-session -A -s ft;"
spots = {}
before = {}


def ghostties():
    return [w for w in windows() if w.get("app_id") == GHOSTTY]


def titled(title):
    return next((w for w in ghostties() if w.get("title") == title), None)


def foot_window():
    return next((w for w in windows() if w.get("app_id") == FOOT), None)


def focused_ghostty():
    return next((w for w in ghostties() if w.get("is_focused")), None)


def askpass():
    # One ask is two lines: the prompt names the key, then its fingerprint.
    return int(sh("if [ -f /tmp/askpass.log ]; then grep -c 'Allow use of key' /tmp/askpass.log "
                  "|| true; else echo 0; fi"))


def far_tmux(cmd):
    return far.succeed(f"runuser -u alice -- env TMUX_TMPDIR=/run/user/1000 tmux {cmd}")


def far_clients():
    out = far.execute("runuser -u alice -- env TMUX_TMPDIR=/run/user/1000 "
                      "tmux list-clients -F '#{session_name}'")[1]
    return sorted(out.split())


def local_clients():
    """Local tmux clients as (session, pid of the terminal process above the client)."""
    out = alice_try(f"{TMUX} list-clients -F '#{{session_name}} #{{client_pid}}'")[1]
    got = []
    for line in out.splitlines():
        session, pid = line.split()
        got.append((session, terminal_above(pid)))
    return sorted(got)


def terminal_above(pid):
    # By ps: tmux names its client "tmux: client", which splits /proc/<pid>/stat's fields.
    chain = sh(f"p={pid}; while [ \"$p\" -gt 1 ]; do echo $p $(cat /proc/$p/comm); "
               "p=$(ps -o ppid= -p $p | tr -d ' '); done")
    # Ghostty's comm can be its wrapper's (.ghostty-wrappe), foot's is foot.
    return next((line.split()[0] for line in chain.splitlines()
                 if "ghostty" in line or line.split()[1:2] == ["foot"]), None)


def new_terminal():
    """A plain Ghostty window, focused, its fish prompt up."""
    old = {w["id"] for w in ghostties()}
    spawn("ghostty +new-window")
    w = wait(lambda: next((w for w in ghostties() if w["id"] not in old and w.get("is_focused")
                           and w.get("title") == "~"), None),
             "a new Ghostty window with fish's prompt", 90)
    print("fish's environment: " + sh(
        f"for p in $(pgrep -P {w['pid']}); do tr '\\0' '\\n' < /proc/$p/environ "
        "| grep -E '^(PATH|TMUX_TMPDIR|TERM|TERMINFO|__NIXOS_SET_ENVIRONMENT_DONE|SHELL)='; done; true"))
    return w


def type_line(text):
    uin(f"type {text}; tap enter")


def t_agent():
    sh("install -m 600 -o alice -g users /etc/alice-far-key /tmp/alice-key")
    alice("SSH_ASKPASS=/etc/askpass-count SSH_ASKPASS_REQUIRE=force "
          "ssh-agent -a /tmp/alice-agent.sock >/dev/null 2>&1")
    alice("SSH_AUTH_SOCK=/tmp/alice-agent.sock ssh-add -c /tmp/alice-key && rm /tmp/alice-key")
    expect(machine.execute("as-alice 'systemctl --user is-active tmux.service'")[0] == 0,
           "the tmux user service is not running")
    print("the user manager's environment: " + alice(
        "systemctl --user show-environment | grep -E '^(PATH|TMUX_TMPDIR|__NIXOS)' || true"))
    print("tmux: " + sh("ls -l /run/current-system/sw/bin/tmux"))


def t_open_windows():
    new_terminal()
    type_line("tmux new -s work")
    wait(lambda: titled("machine: ~"), "tmux work to retitle its window", 30)
    alice(f"{TMUX} new-window -d -t work:")
    alice(f"{TMUX} split-window -d -t work:1")

    new_terminal()
    type_line("ssh far")
    wait(lambda: (focused_ghostty() or {}).get("title") == "far: ~", "the ssh prompt on far", 60)
    type_line("tmux new -s far")
    wait(lambda: "far" in far_clients(), "a client on far's session far", 30)
    wait(lambda: (focused_ghostty() or {}).get("title") == "far: ~", "far's tmux title", 30)
    expect(askpass() == 1, f"the first ssh asked {askpass()} times for the key")

    new_terminal()
    type_line("ssh far")
    wait(lambda: (focused_ghostty() or {}).get("title") == "far: ~", "the second ssh prompt", 60)
    type_line("cd /tmp")
    type_line("tmux new -s far2")
    wait(lambda: "far2" in far_clients(), "a client on far's session far2", 30)
    expect(askpass() == 1, f"the second ssh asked for the key: {askpass()} asks")

    spawn("foot")
    wait(lambda: (foot_window() or {}).get("is_focused") and foot_window().get("title") == "~",
         "a foot window with fish's prompt", 90)
    type_line("cd /tmp")
    type_line("tmux new -s ft")
    wait(lambda: foot_window().get("title") == FOOT_TITLE, "tmux ft to retitle foot's window", 30)

    new_terminal()
    wait(lambda: sorted(w.get("title") for w in ghostties()) == sorted(WINDOWS),
         "the four titles", 30)
    shot("opened")


def launches(app_id=GHOSTTY):
    """The saved command per window title of app_id."""
    s = json.loads(sh(f"cat {STATE}"))
    out = {}
    for c in s["canvases"]:
        for w in c["windows"] + c["floating"]:
            if w["app_id"] == app_id and w.get("launch") is not None:
                out[w.get("title")] = s["launches"][w["launch"]]["command"]
    return out


login_at = [0.0]


def saved_right():
    # After a login, only what the saver wrote since: the file from before holds until then.
    if float(sh(f"stat -c %.9Y {STATE}")) <= login_at[0]:
        return False
    got = launches()
    for title, runs in WINDOWS.items():
        cmd = got.get(title)
        if cmd is None or cmd[:2] != ["ghostty", "+new-window"]:
            return False
        if runs is None and "-e" in cmd:
            return False
        if runs is not None and not ("-e" in cmd and runs in cmd[-1]):
            return False
    foot = launches(FOOT).get(FOOT_TITLE)
    if foot is None or foot[:4] != [FOOT, f"--working-directory={HOME}", "sh", "-c"] \
            or FOOT_RUNS not in foot[-1]:
        return False
    got[FOOT_TITLE] = foot
    return got


def t_saved():
    got = wait(saved_right, "session.json to save every window's tmux or ssh", 60)
    for title, cmd in got.items():
        print(f"saved {title!r}: {cmd}")
    expect(sh(f"stat -c %a {STATE}").strip() == "600", "session.json is not 0600")
    expect("flock 9" in got["far: ~"][-1], "ssh to far takes no lock for its master")
    expect(got["~"][2] == f"--working-directory={HOME}", f"the plain shell's launch {got['~']}")
    for title in WINDOWS:
        spots[title] = titled(title)["layout"]["pos_in_scrolling_layout"]
    spots[FOOT_TITLE] = foot_window()["layout"]["pos_in_scrolling_layout"]
    before["work"] = alice(f"{TMUX} list-panes -s -t work -F '#{{window_id}} #{{pane_id}}'")
    before["far"] = far_tmux("list-sessions -F '#{session_name} #{session_created}'")
    print(f"spots {spots}, before {before}")


def interactive_ssh():
    return machine.execute("pgrep -u alice -f '^ssh (-t )?far'")[1].split()


def master():
    return alice_try("ssh -O check far")[0] == 0


def t_logout(close_master):
    def check():
        logout()
        wait(lambda: not interactive_ssh(), "the windows' ssh to end", 30)
        wait(lambda: far_clients() == [], "far's tmux clients to detach", 30)
        print("after logout: " + sh("pgrep -u alice -a 'ghostty|ssh|tmux' || true"))
        expect(machine.execute(f"as-alice '{TMUX} has-session -t work'")[0] == 0,
               "the local tmux session work died at logout")
        if close_master:
            alice_try("ssh -O exit far")
            wait(lambda: not master(), "far's ssh master to close", 15)
        expected_asks[0] = 0 if master() else 1
        print(f"far's ssh master open at login: {master()}")
    return check


expected_asks = [1]


def t_restored(n):
    def check():
        asks = askpass()
        login_at[0] = float(sh("date +%s.%N"))
        login()
        wait(lambda: count("restoring the session") >= n, "the restore at login", 30)
        wait(lambda: all(titled(t) for t in WINDOWS), "the four Ghostty windows back", 120)
        wait(lambda: (foot_window() or {}).get("title") == FOOT_TITLE,
             "the foot window back in tmux ft", 60)
        wait(lambda: far_clients() == ["far", "far2"], "far's sessions far and far2 attached", 60)
        local = wait(lambda: local_clients() if len(local_clients()) == 2 else None,
                     "two local tmux clients", 30)
        time.sleep(3)
        shot(f"restored-{n}")
        got = {t: titled(t)["layout"]["pos_in_scrolling_layout"] for t in WINDOWS}
        got[FOOT_TITLE] = foot_window()["layout"]["pos_in_scrolling_layout"]
        print(f"restore {n}: spots {got}, local clients {local}, far {far_clients()}")
        g = str(titled("machine: ~")["pid"])
        f = str(foot_window()["pid"])
        expect(local == [("ft", f), ("work", g)],
               f"the local tmux clients are {local}, Ghostty is {g}, foot is {f}")
        ssh = interactive_ssh()
        expect(len(ssh) == 2, f"{len(ssh)} windows run ssh to far, not 2: the plain shell is plain")
        panes = alice(f"{TMUX} list-panes -s -t work -F '#{{window_id}} #{{pane_id}}'")
        expect(panes == before["work"], f"work's windows changed: {before['work']!r} -> {panes!r}")
        sessions = far_tmux("list-sessions -F '#{session_name} #{session_created}'")
        expect(sessions == before["far"], f"far's sessions changed: {sessions!r}")
        expect(askpass() - asks == expected_asks[0],
               f"the login asked {askpass() - asks} times for far's key, not {expected_asks[0]}")
        ssh = sh("pgrep -u alice -a -x ssh || true")
        print(f"ssh processes:\n{ssh}")
        expect(got == spots, f"windows moved: saved {spots}, restored {got}")
    return check


def dbg():
    print("reattach debug: titles " + repr([(w.get("app_id"), w.get("title"), w.get("pid"),
                                             w.get("is_focused")) for w in windows()]))
    print("reattach debug: local " + alice_try(
        f"{TMUX} list-clients -F '#{{client_pid}} #{{session_name}} #{{T:set-titles-string}}'")[1])
    print("reattach debug: far " + far.execute(
        "runuser -u alice -- env TMUX_TMPDIR=/run/user/1000 tmux list-clients "
        "-F '#{session_name} #{T:set-titles-string}'")[1])
    print("reattach debug: " + sh("ps -u alice -o pid,ppid,args --forest | cut -c1-200"))
    print("reattach debug: askpass " + sh("cat /tmp/askpass.log 2>/dev/null || true"))
    print("reattach debug: " + sh("journalctl -b -q -o cat --no-pager --user-unit "
                                  "app-com.mitchellh.ghostty.service -M alice@ 2>/dev/null "
                                  "| grep -v 'info(' | tail -n 20 || true"))


def debugged(fn):
    def run():
        try:
            fn()
        except Exception:
            try:
                dbg()
                shot(f"fail-{len(results)}")
            except Exception as e:
                print(f"reattach debug: {e}")
            raise
    return run


for name, fn in [
    ("agent", t_agent),
    ("open-windows", t_open_windows),
    ("saved", t_saved),
    ("logout", t_logout(False)),
    ("restored", t_restored(1)),
    ("saved-again", t_saved),
    ("logout-again", t_logout(True)),
    ("restored-again", t_restored(2)),
]:
    case(name, debugged(fn))
    if results[-1]["status"] == "fail" and name in ("agent", "open-windows"):
        break

print(journal()[-6000:])
finish()
