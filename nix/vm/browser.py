# ---- vm-browser: a restored Chromium reopens its last session (nix/vm/browser.py) -------------
# Chromium (nix/vm/browser.nix) started bare from its desktop entry opens its startup page; the
# test makes two windows of two tabs each from local pages over DevTools, A (Start, Alpha) and B
# (Beta one, Beta two), and swaps them so the saved spots are not the order they opened in. The
# session saves the entry's bare launch, the bar's logout kills Chromium with river, and the next
# login must bring back both windows with their tabs on their saved spots and nothing else:
# newm-river relaunches the entry with --restore-last-session. The control
# then shows what that flag fixes: the same bare launch without it opens only the startup page.
start_all()
wait_session()
STATE = "/home/alice/.local/state/newm-river/session.json"
ENTRY = "/home/alice/.local/share/applications/chromium-browser.desktop"
PAGES = "file:///etc/newm-pages"
TITLES = ("Alpha", "Beta two")
TABS = sorted([sorted([f"{PAGES}/start.html", f"{PAGES}/a1.html"]),
               sorted([f"{PAGES}/b1.html", f"{PAGES}/b2.html"])])
spots = {}
changed_at = [0.0]


def exec_words():
    line = sh(f"sed -n 's/^Exec=//p' {ENTRY}").strip()
    return [w for w in line.split() if not w.startswith("%")]


def browsers():
    return [w for w in windows() if "chromium" in (w.get("app_id") or "").lower()]


def titled(prefix):
    return next((w for w in browsers() if (w.get("title") or "").startswith(prefix + " ")), None)


def tabs():
    return json.loads(alice("cdp-tabs"))


def browser_processes():
    out = machine.execute("pgrep -u alice -a -f '^/nix/store/[^ ]*/libexec/chromium/chromium' "
                          "| grep -v -- --type= || true")[1]
    return [line for line in out.splitlines() if line.strip()]


def t_open():
    spawn(" ".join(exec_words()))
    first = wait(lambda: browsers() or None, "Chromium's first window", 120)[0]
    print(f"first window: app_id {first.get('app_id')!r}, title {first.get('title')!r}")
    wait(lambda: alice_try("cdp-tabs")[0] == 0, "DevTools on 127.0.0.1:9222", 60)
    wait(lambda: titled("Start"), "the startup page in the first window", 60)
    alice(f"cdp-tabs new {PAGES}/a1.html")
    wait(lambda: titled("Alpha"), "Alpha in a new tab of the first window", 60)
    alice(f"cdp-tabs new {PAGES}/b1.html window")
    b = wait(lambda: titled("Beta one"), "a second window", 60)
    wait(lambda: (titled("Beta one") or {}).get("is_focused"), "the second window focused", 30)
    time.sleep(1)
    alice(f"cdp-tabs new {PAGES}/b2.html")
    wait(lambda: titled("Beta two"), "Beta two in a new tab of the second window", 60)
    got = wait(lambda: tabs() if tabs() == TABS else None, f"the tabs {TABS}", 30)
    print(f"tabs {got}, window ids {[w['id'] for w in browsers()]} (B {b['id']})")
    expect(len(browsers()) == 2, f"{len(browsers())} Chromium windows, not 2")
    # Beta two's window opened right of Alpha's: swap them, B left.
    msg("action", "focus-window", "--id", titled("Beta two")["id"])
    wait(lambda: (titled("Beta two") or {}).get("is_focused"), "Beta two focused", 15)
    a0 = titled("Alpha")["layout"]["pos_in_scrolling_layout"]
    b0 = titled("Beta two")["layout"]["pos_in_scrolling_layout"]
    if b0[0] > a0[0]:
        msg("action", "swap-neighbour", "left")
        wait(lambda: titled("Beta two")["layout"]["pos_in_scrolling_layout"][0]
             < titled("Alpha")["layout"]["pos_in_scrolling_layout"][0], "B swapped left of A")
        changed_at[0] = float(sh("date +%s.%N"))
    shot("opened")


def saved_right():
    # Only what the saver wrote after the last change.
    if float(sh(f"stat -c %.9Y {STATE} 2>/dev/null || echo 0")) <= changed_at[0]:
        return None
    s = json.loads(machine.execute(f"cat {STATE} 2>/dev/null || echo '{{}}'")[1])
    got = {}
    for c in s.get("canvases", []):
        for w in c["windows"]:
            for t in TITLES:
                if (w.get("title") or "").startswith(t + " ") and w.get("launch") is not None:
                    got[t] = (s["launches"][w["launch"]]["command"], tuple(w["rect"][:2]))
    return got if len(got) == len(TITLES) else None


def t_saved():
    # Chromium writes its session a few seconds after a change: give it them before the logout.
    got = wait(saved_right, "session.json to save both Chromium windows", 60)
    for title in TITLES:
        spots[title] = titled(title)["layout"]["pos_in_scrolling_layout"]
    print(f"saved {got}, spots {spots}")
    for title, (command, _) in got.items():
        expect(command == exec_words(), f"{title} saved {command}, not the entry's {exec_words()}")
    expect("--restore-last-session" not in sh(f"cat {STATE}"), "the flag was saved")
    time.sleep(5)


def t_logout():
    logout()
    wait(lambda: not browser_processes(), "Chromium to end with the session", 30)


def t_restored():
    login()
    wait(lambda: count("restoring the session") >= 1, "the restore at login", 30)
    wait(lambda: all(titled(t) for t in TITLES), "both Chromium windows back with their pages", 120)
    procs = browser_processes()
    print(f"browser processes: {procs}")
    expect(len(procs) == 1 and "--restore-last-session" in procs[0].split(),
           f"Chromium was not relaunched once with --restore-last-session: {procs}")
    got = wait(lambda: tabs() if tabs() == TABS else None, f"the tabs {TABS}", 60)
    print(f"tabs {got}")
    wait(lambda: {t: titled(t)["layout"]["pos_in_scrolling_layout"] for t in TITLES} == spots,
         f"the windows on their saved spots {spots}", 30)
    time.sleep(5)
    shot("restored")
    titles = sorted(w.get("title") for w in browsers())
    expect(len(titles) == 2, f"Chromium windows {titles}: not just the two restored")
    expect(tabs() == TABS, f"the tabs changed: {tabs()}")


def t_control():
    # A clean exit (SIGTERM) and the same bare launch without the flag: only the startup page.
    pid = titled("Alpha")["pid"]
    sh(f"kill -TERM {pid}")
    wait(lambda: not browser_processes(), "Chromium to exit", 30)
    wait(lambda: not browsers(), "its windows to close", 15)
    spawn(" ".join(exec_words()))
    wait(lambda: titled("Start"), "the startup page", 120)
    time.sleep(8)
    titles = sorted(w.get("title") for w in browsers())
    print(f"without the flag: windows {titles}, tabs {tabs()}")
    expect(titles == [titled("Start")["title"]], f"a bare start opened {titles}")
    shot("control")


def dbg():
    print("browser debug: windows " + repr([(w.get("app_id"), w.get("title"), w.get("pid"),
                                             w.get("is_focused")) for w in windows()]))
    print("browser debug: " + sh("pgrep -u alice -a chromium | cut -c1-300 || true"))
    print("browser debug: " + sh(f"cat {STATE} 2>/dev/null | head -c 3000 || true"))
    print("browser debug: tabs " + alice_try("cdp-tabs")[1])


def debugged(fn):
    def run():
        try:
            fn()
        except Exception:
            try:
                dbg()
                shot(f"fail-{len(results)}")
            except Exception as e:
                print(f"browser debug: {e}")
            raise
    return run


for name, fn in [
    ("open", t_open),
    ("saved", t_saved),
    ("logout", t_logout),
    ("restored", t_restored),
    ("control", t_control),
]:
    case(name, debugged(fn))
    if results[-1]["status"] == "fail" and name in ("open", "saved", "logout"):
        break

print(journal()[-6000:])
finish()
