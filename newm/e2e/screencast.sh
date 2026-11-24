#!/usr/bin/env bash
# Screencasts on a headless river: a private PipeWire and WirePlumber (devices off) in their
# own XDG_RUNTIME_DIR, a private session bus, river (pixman, one 1280x720 output) with newm-river.
# examples/zkde_probe.rs asks river for zkde_screencast_unstable_v1 streams as Spectacle's
# KPipeWire does (an output, a window by the uuid org.kde.KWin hands out, a region; a virtual
# output and an unknown window fail), consumes each PipeWire node and checks the frames: their
# count, size, format, strictly increasing pts and the colour in the middle. Then
# xdg-desktop-portal-wlr's ScreenCast (what the session routes the portal to) is driven over D-Bus
# and its node consumed the same way.
#
# Run from the repository root inside its dev shell, after `zig build` and
# `cargo build --release -p newm-river --bins --examples`, with jq on PATH:
#   nix shell nixpkgs#jq -c newm/e2e/screencast.sh
# RIVER, NEWM_RIVER, PROBE, E2E_CLIENT, XDPW and OUT override the binaries and the output dir.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
probe=${PROBE:-$root/newm/target/release/examples/zkde_probe}
client=${E2E_CLIENT:-$root/newm/target/release/examples/e2e_client}
xdpw=${XDPW:-}
out=${OUT:-/tmp/newm-river-cast-e2e}
mkdir -p "$out"
for tool in jq pipewire wireplumber dbus-daemon busctl; do
    command -v "$tool" >/dev/null || { echo "e2e: $tool is not on PATH" >&2; exit 2; }
done

XDG_RUNTIME_DIR=$(mktemp -d)
XDG_CONFIG_HOME=$XDG_RUNTIME_DIR/config
XDG_STATE_HOME=$XDG_RUNTIME_DIR/state
export XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_STATE_HOME
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR
bus=$(dbus-daemon --session --fork --print-address=1 --print-pid=1)
DBUS_SESSION_BUS_ADDRESS=$(sed -n 1p <<<"$bus")
dbus_pid=$(sed -n 2p <<<"$bus")
export DBUS_SESSION_BUS_ADDRESS

# WirePlumber links streams; it must not open this machine's sound or video devices.
mkdir -p "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d"
cat > "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d/90-e2e.conf" <<'CONF'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
CONF

cat > "$out/config.kdl" <<'KDL'
prefer-no-csd
layout {
    gaps 10
    border { width 1; active-color "#82FB9C8C"; inactive-color "transparent"; }
    canvas {
        tile-width 119
        tile-height 113
        default-width { fixed 476; }
        default-height { fixed 339; }
    }
    animations { off; }
}
KDL
export NIRI_CONFIG=$out/config.kdl

pass=0
pids=()
cleanup() {
    kill "${pids[@]}" "$dbus_pid" 2>/dev/null || true
    rm -rf "$XDG_RUNTIME_DIR"
}
trap cleanup EXIT
fail() {
    echo "FAIL: $*" >&2
    tail -n 30 "$out/river.log" >&2 || true
    exit 1
}
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }
until_true() {
    local i
    for i in $(seq 100); do
        if eval "$1"; then return 0; fi
        sleep 0.1
    done
    fail "timed out waiting for: $1"
}

pipewire > "$out/pipewire.log" 2>&1 &
pids+=($!)
until_true '[ -S "$XDG_RUNTIME_DIR/pipewire-0" ]'
wireplumber > "$out/wireplumber.log" 2>&1 &
pids+=($!)
ok "private pipewire and wireplumber run"

"$river" -c "RUST_LOG=newm_river=debug $nrv" > "$out/river.log" 2>&1 &
pids+=($!)
until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
export NIRI_SOCKET WAYLAND_DISPLAY
msg() { "$nrv" msg "$@"; }
win() { msg --json windows | jq -c ".[] | select(.app_id == \"$1\")"; }
until_true '[ "$(msg --json outputs | jq length)" -ge 1 ]'
read -r oname ow oh < <(msg --json outputs | jq -r '[.[]][0] | "\(.name) \(.logical.width) \(.logical.height)"')
ok "river runs newm-river; output $oname ${ow}x${oh}"

open() { # app-id colour
    "$client" window --app-id "$1" --color "$2" >/dev/null 2>&1 &
    pids+=($!)
    until_true "[ -n \"\$(win $1)\" ]"
}
open cast-red ff0000
sleep 0.5
read -r ww wh < <(win cast-red | jq -r '.layout.window_size | "\(.[0] | floor) \(.[1] | floor)"')
ok "a red window ${ww}x${wh}"

field() { sed -n "s/.* $1=\([^ ]*\).*/\1/p" <<<"$2"; }

# An output: content changes while it records (a second window opens), so frames keep coming.
( sleep 1.2; "$client" window --app-id cast-blue --color 0000ff >/dev/null 2>&1 ) &
blue=$!
pids+=($blue)
r=$("$probe" output "$oname" --secs 3) || fail "output stream: $r"
echo "  $r"
[ "$(field size "$r")" = "${ow}x${oh}" ] || fail "output stream size: $r"
[ "$(field format "$r")" = BGRx ] || fail "output stream format: $r"
[ "$(field frames "$r")" -ge 2 ] || fail "output stream: fewer than 2 frames: $r"
[ "$(field pts_increasing "$r")" = true ] || fail "output stream pts: $r"
mid=$(field center "$r")
ok "an output stream: frames as the screen changes, BGRx at the output's size, pts increasing"
until_true 'grep -q "the client closed it" "$out/river.log"'
ok "closing the stream stops the cast"

# A window, by the uuid queryWindowInfo answers once a click picks it (a virtual
# pointer clicks the focused window) and by its foreign-toplevel identifier.
until_true 'busctl --user status org.kde.KWin >/dev/null 2>&1'
read -r fx fy fw fh < <(msg --json focused-window | jq -r \
    '"\(.layout.tile_pos_in_workspace_view[0] | floor) \(.layout.tile_pos_in_workspace_view[1] | floor) \(.layout.window_size[0]) \(.layout.window_size[1])"')
busctl --user --json=short --timeout=30 call org.kde.KWin /KWin org.kde.KWin queryWindowInfo \
    > "$out/query.json" 2> "$out/query.err" &
qpid=$!
# The click only picks once the window manager has taken the pointer: click until it answers.
for _ in $(seq 20); do
    sleep 0.5
    kill -0 "$qpid" 2>/dev/null || break
    "$client" click $((fx + fw / 2)) $((fy + fh / 2)) "$ow" "$oh" || fail "virtual pointer click"
done
wait "$qpid" || fail "queryWindowInfo: $(cat "$out/query.err")"
uuid=$(jq -r '.data[0].uuid.data' "$out/query.json")
[ -n "$uuid" ] && [ "$uuid" != null ] || fail "queryWindowInfo gave no uuid: $(cat "$out/query.json")"
info=$(busctl --user --json=short call org.kde.KWin /KWin org.kde.KWin getWindowInfo s "$uuid")
[ "$(jq -r '.data[0].uuid.data' <<<"$info")" = "$uuid" ] || fail "getWindowInfo: $info"
ok "org.kde.KWin queryWindowInfo/getWindowInfo give a window uuid"
r=$("$probe" window-uuid "$uuid" --secs 1.5) || fail "window stream by uuid: $r"
echo "  $r"
[ "$(field format "$r")" = BGRA ] || fail "window stream format: $r"
r=$("$probe" window cast-red --secs 1.5) || fail "window stream: $r"
echo "  $r"
[ "$(field center "$r")" = ff0000 ] || fail "window stream colour: $r"
[ "$(field size "$r")" = "${ww}x${wh}" ] || fail "window stream size: $r"
ok "a window stream: the red window's own pixels, BGRA"

# A region in the middle of the screen: the colour the output stream had there.
r=$("$probe" region $((ow / 2 - 50)) $((oh / 2 - 30)) 100 60 --secs 1.5) || fail "region stream: $r"
echo "  $r"
[ "$(field size "$r")" = 100x60 ] || fail "region stream size: $r"
[ "$(field center "$r")" = "$mid" ] || fail "region stream colour (want $mid): $r"
r=$("$probe" region -500 0 100 60) && fail "a region off every output was served: $r"
[ "$r" = "failed: no output shows this region" ] || fail "region off screen: $r"
ok "a region stream: the region's size and pixels, cut from the output; one off screen fails"

r=$("$probe" virtual) && fail "a virtual output was served: $r"
[ "$r" = "failed: newm-river has no virtual outputs" ] || fail "virtual output: $r"
r=$("$probe" window-uuid '{no-such-window}') && fail "an unknown window was served: $r"
[ "$r" = "failed: no window has this uuid" ] || fail "unknown window: $r"
ok "a virtual output and an unknown window fail"

# Spectacle itself, when SPECTACLE names its binary: a background screen recording, stopped by
# a second `spectacle --record`, must leave a video longer than a second.
if [ -n "${SPECTACLE:-}" ]; then
    rec=$out/spectacle.webm
    rm -f "$rec"
    casts=$(grep -c "PipeWire node" "$out/river.log" || true)
    QT_QPA_PLATFORM=wayland "$SPECTACLE" -b -n -R s -o "$rec" > "$out/spectacle.log" 2>&1 &
    sp=$!
    pids+=($sp)
    # As on Plasma, Spectacle asks which screen to record through queryWindowInfo's pick: a click
    # where no window is (the output's bottom-right corner) answers with the output.
    for _ in $(seq 40); do
        [ "$(grep -c "PipeWire node" "$out/river.log")" -gt "$casts" ] && break
        sleep 0.5
        "$client" click $((ow - 3)) $((oh - 3)) "$ow" "$oh" || fail "virtual pointer click"
    done
    until_true '[ "$(grep -c "PipeWire node" "$out/river.log")" -gt "$casts" ]'
    # Something moves on screen while it records.
    "$client" window --app-id cast-green --color 00ff00 >/dev/null 2>&1 &
    pids+=($!)
    sleep 3
    QT_QPA_PLATFORM=wayland "$SPECTACLE" -b -n -R s >> "$out/spectacle.log" 2>&1 || true
    for _ in $(seq 200); do kill -0 "$sp" 2>/dev/null || break; sleep 0.1; done
    [ -s "$rec" ] || { tail -n 20 "$out/spectacle.log" >&2; fail "spectacle left no recording"; }
    if command -v ffprobe >/dev/null; then
        secs=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$rec")
        echo "  spectacle recording: $(stat -c %s "$rec") bytes, ${secs}s"
        awk -v s="$secs" 'BEGIN { exit !(s > 1) }' || fail "spectacle's recording is ${secs}s long"
    fi
    ok "Spectacle records the screen through zkde_screencast"
else
    echo "skip - Spectacle: SPECTACLE is not set"
fi

# xdg-desktop-portal-wlr's ScreenCast against river, over its impl interface.
# wlroots offers capture clients one shm format, the renderer's preferred read format. On some
# GPUs (NVIDIA, headless) that is a 3-byte one portal-wlr cannot use, so it offers
# DMA-BUF only, which the probe cannot map: that environment says so and skips. Intel and AMD
# (and the VMs' Mesa) read back 4-byte XRGB.
shm=$(grep -o 'capture: shm formats \[[^]]*\]' "$out/river.log" | head -n 1 || true)
if [ -n "$xdpw" ] && [ -x "$xdpw" ] && [[ "$shm" =~ \[(Bgr888|Rgb888)\] ]]; then
    echo "skip - xdg-desktop-portal-wlr: river's renderer reads back only ${BASH_REMATCH[1]} here ($shm)"
elif [ -n "$xdpw" ] && [ -x "$xdpw" ]; then
    cat > "$out/xdpw.conf" <<CONF
[screencast]
chooser_type=none
output_name=$oname
max_fps=30
CONF
    "$xdpw" -l DEBUG -c "$out/xdpw.conf" > "$out/xdpw.log" 2>&1 &
    pids+=($!)
    until_true 'busctl --user status org.freedesktop.impl.portal.desktop.wlr >/dev/null 2>&1'
    dest=(org.freedesktop.impl.portal.desktop.wlr /org/freedesktop/portal/desktop
        org.freedesktop.impl.portal.ScreenCast)
    req=/org/freedesktop/portal/desktop/request/1_1/e2e
    ses=/org/freedesktop/portal/desktop/session/1_1/e2e
    busctl --user call "${dest[@]}" CreateSession oosa{sv} "$req"1 "$ses" e2e 0 > /dev/null \
        || fail "portal CreateSession"
    busctl --user call "${dest[@]}" SelectSources oosa{sv} "$req"2 "$ses" e2e 2 \
        types u 1 cursor_mode u 1 > /dev/null || fail "portal SelectSources"
    started=$(busctl --user --json=short call "${dest[@]}" Start oossa{sv} "$req"3 "$ses" e2e "" 0) \
        || fail "portal Start"
    node=$(jq -r '.data[1].streams.data[0][0]' <<<"$started")
    [[ "$node" =~ ^[0-9]+$ ]] || fail "portal Start gave no stream: $started"
    r=$("$probe" node "$node" --secs 2) || { tail -n 20 "$out/xdpw.log" >&2; fail "portal stream: $r"; }
    echo "  $r"
    [ "$(field size "$r")" = "${ow}x${oh}" ] || fail "portal stream size: $r"
    ok "xdg-desktop-portal-wlr casts the output through river (node $node)"
else
    echo "skip - xdg-desktop-portal-wlr: XDPW is not set"
fi

echo "screencast e2e: $pass passed"
