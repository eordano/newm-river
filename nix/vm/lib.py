
# ---- shared helpers of the newm-river VM tests (nix/vm/lib.py) -------------------------------
# TEST, XFAIL, STRICT and PASSWORD come from nix/vm/default.nix.
import json
import os
import shlex
import time

from PIL import Image

OUT = os.environ.get("out", ".")
results = []


class Fail(Exception):
    pass


def sh(cmd, timeout=60):
    return machine.succeed(cmd, timeout=timeout)


def alice(cmd, timeout=60):
    """A shell command as alice inside her session's environment."""
    return machine.succeed("as-alice " + shlex.quote(cmd), timeout=timeout)


def alice_try(cmd, timeout=60):
    return machine.execute("as-alice " + shlex.quote(cmd), timeout=timeout)


def msg(*args):
    return alice("newm-river msg " + " ".join(shlex.quote(str(a)) for a in args))


def js(what):
    return json.loads(alice("newm-river msg --json " + what))


def windows():
    return js("windows")


def win(app):
    for w in windows():
        if w.get("app_id") == app:
            return w
    return None


def focused():
    for w in windows():
        if w.get("is_focused"):
            return w.get("app_id")
    return None


def col(app):
    """The window's tile column on the canvas."""
    return win(app)["layout"]["pos_in_scrolling_layout"][0]


def overview_open():
    return js("overview-state").get("is_open")


def uin(script, timeout=60):
    sh("uin " + shlex.quote(script), timeout=timeout)


def spawn(cmd):
    msg("action", "spawn-sh", "--", cmd)


def wait(pred, what, timeout=15):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            last = pred()
            if last:
                return last
        except Exception as e:  # a window not there yet
            last = e
        time.sleep(0.25)
    raise Fail(f"timed out waiting for {what} (last: {last!r})")


def expect(cond, what):
    if not cond:
        raise Fail(what)


def journal():
    """river's and newm-river's log: the unit's output stream, identified as river."""
    return sh("journalctl -b -q -o cat --no-pager -t river")


def count(pattern):
    return journal().count(pattern)


def open_window(app, color="c03030", extra=""):
    spawn(f"e2e_client window --app-id {app} --color {color} {extra}")
    wait(lambda: win(app) and win(app)["layout"]["window_size"][0] > 0, f"{app} mapped")


def focus(app):
    msg("action", "focus-window", "--id", win(app)["id"])
    wait(lambda: focused() == app, f"{app} focused")


def output():
    return list(js("outputs").values())[0]


def shot(name, output_name=None):
    """A grim screenshot (physical pixels) kept as shots/NAME.png; returns it as a PIL image."""
    o = f"-o {output_name} " if output_name else ""
    alice(f"grim {o}/tmp/shots/{name}.png")
    machine.copy_from_vm(f"/tmp/shots/{name}.png", "shots")
    return Image.open(os.path.join(OUT, "shots", f"{name}.png")).convert("RGB")


def drm_shot(name):
    """What QEMU's virtio GPU scans out: the DRM side of the screen, kept as NAME.png."""
    machine.screenshot(name)


def greenish(p):
    r, g, b = p
    return g > r + 30 and g > b + 20 and g > 80


def changed(a, b, box, tolerance=24):
    """The fraction of the pixels in box (x0, y0, x1, y1) that differ between two shots."""
    x0, y0, x1, y1 = box
    n = diff = 0
    for y in range(y0, y1, 4):
        for x in range(x0, x1, 4):
            n += 1
            pa, pb = a.getpixel((x, y)), b.getpixel((x, y))
            if max(abs(pa[i] - pb[i]) for i in range(3)) > tolerance:
                diff += 1
    return diff / max(n, 1)


def debug():
    """What a failure leaves to read: the windows, the last river lines, the wev log."""
    try:
        ws = [(w.get("app_id"), w.get("is_focused"), w["layout"]["pos_in_scrolling_layout"],
               w["layout"]["tile_pos_in_workspace_view"], w["layout"]["window_size"]) for w in windows()]
        print(f"newm-vm debug: windows {ws}")
        print("newm-vm debug: " + sh("journalctl -b -q -o cat --no-pager -t river | tail -n 25"))
        print("newm-vm debug: " + sh("tail -n 8 /tmp/wev.log /tmp/xterm.log 2>/dev/null || true"))
    except Exception as e:
        print(f"newm-vm debug: {e}")


def release_all():
    uin("key leftmeta up; key leftalt up; key leftctrl up; key leftshift up")


def case(name, fn):
    """Runs one test; an expected failure (xfail.nix) names the branch it waits on."""
    t0 = time.time()
    err = ""
    try:
        with subtest(name):
            fn()
        status = "ok"
    except Exception as e:
        status = "fail"
        err = str(e).replace("\n", " ")[:600]
        try:
            debug()
            release_all()
            if overview_open():
                msg("action", "toggle-overview")
        except Exception:
            pass
    branch = XFAIL.get(name)
    if branch:
        status = "xpass" if status == "ok" else "xfail"
    results.append({"test": TEST, "case": name, "status": status, "branch": branch or "",
                    "secs": round(time.time() - t0, 1), "error": err})
    note = f" (waits on {branch})" if branch else ""
    print(f"newm-vm: {status.upper()} {name}{note}{': ' + err if err else ''}", flush=True)


def wait_session(timeout=120):
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("uin.service")
    sh("test -S /run/uin.sock || sleep 1")
    wait(lambda: alice_try("newm-river msg version")[0] == 0, "newm-river's IPC", timeout)
    wait(lambda: len(js("outputs")) >= 1, "an output", 30)


def river_pid():
    out = machine.execute("pgrep -u alice -x river")[1].split()
    return out[0] if out else None


def logout(hold=True):
    """The bar's logout: IPC quit ends river, the unit, the login shell and the logind session."""
    if hold:
        sh("touch /run/newm-hold")
    pid = river_pid()
    msg("action", "quit", "--skip-confirmation")
    wait(lambda: machine.execute(f"kill -0 {pid}")[0] != 0, "river to exit after IPC quit", 30)
    wait(lambda: machine.execute("as-alice 'systemctl --user is-active newm-river.service'")[0] != 0,
         "newm-river.service to stop", 30)


def login():
    sh("rm -f /run/newm-hold")
    wait(lambda: river_pid() is not None, "the next autologin to start river", 60)
    wait_session()


def finish():
    with open(os.path.join(OUT, "results.json"), "w") as f:
        json.dump(results, f, indent=1)
    lines = [f"{r['status']:5} {r['test']}/{r['case']}"
             + (f" [{r['branch']}]" if r["branch"] else "")
             + (f": {r['error']}" if r["error"] else "") for r in results]
    with open(os.path.join(OUT, "results.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    print("\n".join(["newm-vm summary:"] + lines), flush=True)
    try:
        machine.copy_from_vm("/tmp/shots", "")
    except Exception:
        pass
    failed = [r["case"] for r in results if r["status"] == "fail"]
    if failed and STRICT:
        raise Exception(f"failed: {', '.join(failed)}")

