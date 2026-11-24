#!/usr/bin/env bash
# newm-river end to end: the only output going off and coming back, again and again (what
# swayidle's `wlopm --off`/`wlr-randr --off` and a DisplayPort monitor that unplugs itself when
# it sleeps do). Every round the window must be drawn on its tile again, not only reachable.
# Runs a headless river with newm-river and the e2e_client example; screenshots from grim.
#
# Run from the repository root inside its dev shell, after `zig build` and
# `cargo build --release -p newm-river --examples`, with jq on PATH:
#   nix shell nixpkgs#jq -c newm/e2e/outputs.sh
# RIVER, NEWM_RIVER, E2E_CLIENT, OUT and ROUNDS override the binaries, the output dir and the
# number of off/on rounds (20).
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
client=${E2E_CLIENT:-$root/newm/target/release/examples/e2e_client}
out=${OUT:-/tmp/newm-river-e2e-outputs}
rounds=${ROUNDS:-20}
case "$out" in /tmp/newm-river-e2e-*) ;; *) echo "e2e: OUT must be under /tmp/newm-river-e2e-" >&2; exit 2 ;; esac
rm -rf "$out"
mkdir -p "$out"
for tool in jq grim wlr-randr; do
    command -v "$tool" >/dev/null || { echo "e2e: $tool is not on PATH" >&2; exit 2; }
done

export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET NEWM_RIVER_SESSION

pass=0
river_pid=
log=$out/river.log
fail() { echo "FAIL: $*" >&2; tail -n 60 "$log" >&2; exit 1; }
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }
cleanup() {
    [ -n "$river_pid" ] && kill "$river_pid" 2>/dev/null || true
    pkill -f "$client" 2>/dev/null || true
}
trap cleanup EXIT

until_true() {
    local i
    for i in $(seq "${2:-100}"); do
        if eval "$1"; then return 0; fi
        sleep 0.1
    done
    fail "timed out waiting for: $1"
}

# As a desktop sets DP-1: an output block with a position away from the origin and scale 1.25, which
# newm-river applies again each time the output comes back.
cat > "$out/config.kdl" <<KDL
prefer-no-csd
output "HEADLESS-1" {
    scale 1.25
    position x=3968 y=0
}
layout {
    gaps 10
    border { width 1; active-color "#82FB9C8C"; inactive-color "transparent"; }
    canvas {
        tile-width 119
        tile-height 113
        default-width { fixed 960; }
        default-height { proportion 0.666700; }
    }
}
KDL

msg() { "$nrv" msg "$@"; }
windows() { msg --json windows; }
field() { windows | jq -r ".[] | select(.app_id == \"$1\") | $2 | if type == \"number\" then floor else . end"; }
outputs_on() { wlr-randr --json | jq '[.[] | select(.enabled)] | length'; }
pixel() { "$client" pixel "$@"; }

XDG_RUNTIME_DIR=$(mktemp -d -p "$out")
export XDG_RUNTIME_DIR NIRI_CONFIG=$out/config.kdl WLR_HEADLESS_OUTPUTS=1
"$river" -c "RUST_LOG=newm_river=debug $nrv" > "$log" 2>&1 &
river_pid=$!
until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
export NIRI_SOCKET WAYLAND_DISPLAY
until_true '[ "$(msg --json outputs | jq length)" -ge 1 ]'
name=$(wlr-randr --json | jq -r '.[0].name')

"$client" window --app-id off-1 --color 00ff00 --size 400x300 >> "$out/clients.log" 2>&1 &
until_true '[ "$(field off-1 .layout.window_size[0])" -gt 0 ] 2>/dev/null'
sleep 0.5

# The window's middle, in screen pixels, from what newm-river reports.
drawn() { # tag
    local x y w h
    x=$(field off-1 .layout.tile_pos_in_workspace_view[0])
    y=$(field off-1 .layout.tile_pos_in_workspace_view[1])
    w=$(field off-1 .layout.window_size[0])
    h=$(field off-1 .layout.window_size[1])
    grim -t ppm "$out/$1.ppm" || fail "grim ($1)"
    [ "$(pixel "$out/$1.ppm" $((x + w / 2)) $((y + h / 2)))" = "0 255 0" ]
}
drawn before || fail "the window is not drawn before the first round"
ok "the window is drawn"

for r in $(seq "$rounds"); do
    wlr-randr --output "$name" --off || fail "round $r: wlr-randr --off"
    until_true '[ "$(outputs_on)" -eq 0 ]'
    [ -n "$(field off-1 .id)" ] || fail "round $r: the window left the window list while the output was off"
    wlr-randr --output "$name" --on || fail "round $r: wlr-randr --on"
    until_true '[ "$(outputs_on)" -eq 1 ] && [ "$(msg --json outputs | jq length)" -ge 1 ]'
    # The window must be drawn again without anything else happening: no input, no other
    # window, nothing that would schedule another render by chance.
    for _ in $(seq 20); do
        drawn "round-$r" && break
        sleep 0.1
    done
    drawn "round-$r" || fail "round $r: the window is not drawn after the output came back"
done
ok "$rounds rounds of the only output off and on, the window drawn after each"

msg action quit --skip-confirmation >/dev/null
for _ in $(seq 100); do kill -0 "$river_pid" 2>/dev/null || break; sleep 0.1; done
river_pid=
echo "e2e outputs: $pass ok"
