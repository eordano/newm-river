# Chromium's windows and tabs over DevTools on 127.0.0.1:9222, for nix/vm/browser.py.
#   cdp-tabs                    each window's tab URLs: [[url, ...], ...], sorted
#   cdp-tabs new URL [window]   URL in a new tab of the last active window, or in a new window
import json
import sys
import urllib.request

from websockets.sync.client import connect

url = "http://127.0.0.1:9222/json/version"
version = json.load(urllib.request.urlopen(url, timeout=5))
ws = connect(version["webSocketDebuggerUrl"], max_size=None, open_timeout=5)
seq = 0


def call(method, **params):
    global seq
    seq += 1
    ws.send(json.dumps({"id": seq, "method": method, "params": params}))
    while True:
        reply = json.loads(ws.recv(timeout=10))
        if reply.get("id") == seq:
            if "error" in reply:
                sys.exit(f"{method}: {reply['error']}")
            return reply["result"]


if sys.argv[1:2] == ["new"]:
    new_window = sys.argv[3:4] == ["window"]
    call("Target.createTarget", url=sys.argv[2], newWindow=new_window)
else:
    windows = {}
    for t in call("Target.getTargets")["targetInfos"]:
        if t["type"] == "page":
            got = call("Browser.getWindowForTarget", targetId=t["targetId"])
            windows.setdefault(got["windowId"], []).append(t["url"])
    print(json.dumps(sorted(sorted(urls) for urls in windows.values())))
ws.close()
