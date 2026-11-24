#!/usr/bin/env bash
# The synth background on a headless river (wlroots' headless backend, pixman, one output
# set to 1440x960) with newm-river as its window manager: the background draws behind a window and a
# bottom-layer bar, at the output's refresh rate while animated on the GPU (4 frames a second on
# the CPU painter), not at all while a fullscreen window covers it or the output is off, once
# when still and never when off (no worker thread left), it stands still after idle-still-after,
# and it follows a config reload and the two actions. Prints the window manager's CPU time per
# mode.
#
# Run from the repository root inside its dev shell, after `zig build` and `cargo build
# --release -p newm-river --bins --examples`, with jq, grim and wlr-randr on PATH:
#   nix shell nixpkgs#jq nixpkgs#grim nixpkgs#wlr-randr -c newm/e2e/synth.sh
# RIVER, NEWM_RIVER and OUT override the river binary, the newm-river binary and the output dir;
# NEWM_SYNTH_RENDERER=cpu runs the CPU painter instead of the GPU path.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
client=$root/newm/target/release/examples/e2e_client
out=${OUT:-/tmp/newm-river-e2e-synth}
mkdir -p "$out"
rm -f "$out"/*.log "$out"/*.ppm
for tool in jq grim wlr-randr; do
    command -v "$tool" >/dev/null || { echo "e2e: $tool is not on PATH" >&2; exit 2; }
done

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
export XDG_STATE_HOME=$out/state
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET DBUS_SESSION_BUS_ADDRESS

config() {
    cat > "$out/config.kdl.new" <<KDL
prefer-no-csd
synth-background {
    $1
}
KDL
    mv "$out/config.kdl.new" "$out/config.kdl"
}
config 'mode "still"'
export NIRI_CONFIG=$out/config.kdl
"$nrv" validate

pass=0
fail() {
    echo "FAIL: $*" >&2
    tail -n 30 "$out/river.log" >&2
    exit 1
}
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }

"$river" -c "${NEWM_SYNTH_RENDERER:+NEWM_SYNTH_RENDERER=$NEWM_SYNTH_RENDERER }RUST_LOG=newm_river=info,newm_river::river::synth=debug $nrv" \
    > "$out/river.log" 2>&1 &
river_pid=$!
pids=()
trap 'kill $river_pid "${pids[@]}" 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT

until_true() {
    local i
    for i in $(seq 100); do
        if eval "$1"; then return 0; fi
        sleep 0.1
    done
    fail "timed out waiting for: $1"
}

until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
export NIRI_SOCKET WAYLAND_DISPLAY
msg() { "$nrv" msg "$@"; }
until_true '[ "$(msg --json outputs | jq length)" -ge 1 ]'
wm=$(pgrep -n -f "^$nrv" || pgrep -n newm-river)

frames() { grep -c 'synth frame committed' "$out/river.log" || true; }
# Frames committed over $1 seconds.
frames_over() {
    local a b
    a=$(frames)
    sleep "$1"
    b=$(frames)
    echo $((b - a))
}
# The window manager's CPU time over $1 seconds, in milliseconds per second.
cpu_over() {
    local a b tck
    tck=$(getconf CLK_TCK)
    a=$(awk '{print $14 + $15}' "/proc/$wm/stat")
    sleep "$1"
    b=$(awk '{print $14 + $15}' "/proc/$wm/stat")
    echo $(((b - a) * 1000 / tck / $1))
}
pixel() { "$client" pixel "$out/$1.ppm" "$2" "$3"; }
shot() { grim -t ppm "$out/$1.ppm" || fail "grim"; }
synth_thread() { grep -lx synth /proc/$wm/task/*/comm 2>/dev/null | wc -l; }

# Still from the start: the scene's clock is 0, so this is the look at t = 0, drawn
# at 1440x960, a laptop's logical size.
name=$(wlr-randr --json | jq -r '.[0].name')
wlr-randr --output "$name" --custom-mode 1440x960 || fail "wlr-randr --custom-mode"
until_true '[ "$(msg --json outputs | jq -r "[.[]][0].logical.width")" = 1440 ]'
until_true '[ "$(frames)" -ge 1 ]'
# One frame per size it was given (the first output size, then each resize's configure), then
# none.
frames_over 1 >/dev/null
n=$(frames_over 2)
[ "$n" -eq 0 ] || fail "still: $n more frames in 2 s once settled"
shot still-1440x960-t0
ok "still at 1440x960: $(frames) frames for its sizes, then none; shot as still-1440x960-t0.ppm"

config 'mode "animated"'
until_true '[ "$(frames)" -ge 4 ]'
renderer=$(grep -o 'synth background: drawing on the [A-Z]*' "$out/river.log" | head -n 1 | awk '{print $NF}')
n=$(frames_over 5)
if [ "$renderer" = GPU ]; then
    refresh=$(wlr-randr --json | jq -r '[.[0].modes[] | select(.current)][0].refresh | if . > 0 then . else 60 end')
    expected=$(awk -v r="$refresh" 'BEGIN { printf "%d", r * 5 }')
    [ "$n" -ge $((expected * 3 / 4)) ] && [ "$n" -le $((expected * 11 / 10)) ] \
        || fail "animated on the GPU: $n frames in 5 s on a $refresh Hz output, expected about $expected"
    ok "animated on the GPU draws at the display's rate ($refresh Hz): $n frames in 5 s"
else
    [ "$n" -ge 16 ] && [ "$n" -le 22 ] || fail "animated on the CPU: $n frames in 5 s, expected about 20"
    ok "animated on the CPU draws at its cap of 4: $n frames in 5 s"
fi
cpu_animated=$(cpu_over 10)
ok "window manager CPU while animated: $cpu_animated ms/s"

shot sky
read -r r g b < <(pixel sky 720 4)
[ "$r$g$b" != 000 ] && [ "$r" -lt 40 ] && [ "$g" -lt 40 ] && [ "$b" -lt 50 ] \
    || fail "the sky's top is ($r $g $b), not the look's dark sky"
# The floor, above where the bottom-layer bar goes later.
read -r fr fg fb < <(pixel sky 720 880)
[ "$fg" -gt "$fr" ] || fail "the floor at the bottom is ($fr $fg $fb), not the look's green-tinted ground"
ok "the background shows the look: sky ($(pixel sky 720 4)), floor ($fr $fg $fb)"

# Placement: a window and a bottom-layer bar draw over it.
"$client" layer --namespace bar --layer bottom --anchor bottom --thickness 30 --exclusive 0 \
    --color 0000ff > "$out/bar.log" 2>&1 &
pids+=($!)
"$client" window --app-id e2e-win --color ff0000 > "$out/win.log" 2>&1 &
win_pid=$!
pids+=($win_pid)
until_true '[ "$(msg --json windows | jq length)" -ge 1 ]'
sleep 0.6
shot over
read -r r g b < <(pixel over 720 952)
[ "$r $g $b" = "0 0 255" ] || fail "the bottom-layer bar is ($r $g $b) over the background"
ok "a bottom-layer bar draws over the background"
geo=$(msg --json windows | jq -r '.[0].layout | "\(.tile_pos_in_workspace_view[0] + 20) \(.tile_pos_in_workspace_view[1] + 20)"')
read -r wx wy <<<"$geo"
read -r r g b < <(pixel over "${wx%.*}" "${wy%.*}")
[ "$r $g $b" = "255 0 0" ] || fail "the window is ($r $g $b) at $wx,$wy"
ok "a window draws over the background"
kill "$win_pid"
until_true '[ "$(msg --json windows | jq length)" -eq 0 ]'

# Covered: an opaque fullscreen window over the whole output.
"$client" window --app-id e2e-fs --fullscreen --color 00ff00 > "$out/fs.log" 2>&1 &
fs_pid=$!
pids+=($fs_pid)
until_true '[ "$(msg --json windows | jq length)" -ge 1 ]'
sleep 1
n=$(frames_over 3)
[ "$n" -eq 0 ] || fail "covered by a fullscreen window: $n frames in 3 s"
cpu_covered=$(cpu_over 5)
ok "covered by an opaque fullscreen window: no frames, CPU $cpu_covered ms/s"
kill "$fs_pid"
until_true '[ "$(msg --json windows | jq length)" -eq 0 ]'
sleep 0.5
n=$(frames_over 3)
[ "$n" -ge 8 ] || fail "uncovered again: $n frames in 3 s"
ok "uncovered, it draws again: $n frames in 3 s"

# Blanked: the output off.
wlr-randr --output "$name" --off || fail "wlr-randr --off"
sleep 1
n=$(frames_over 3)
[ "$n" -eq 0 ] || fail "output off: $n frames in 3 s"
ok "with the output off: no frames"
wlr-randr --output "$name" --on || fail "wlr-randr --on"
sleep 1
n=$(frames_over 3)
[ "$n" -ge 8 ] || fail "output on again: $n frames in 3 s"
ok "with the output on again, it draws again: $n frames in 3 s"

# Still, over IPC: at most the frame in flight, then nothing.
msg action set-synth-background still
sleep 0.5
n=$(frames_over 3)
[ "$n" -eq 0 ] || fail "still: $n frames in 3 s"
cpu_still=$(cpu_over 5)
ok "set-synth-background still: no frames, CPU $cpu_still ms/s"

# The toggle from still (the config says animated, so its resting mode is still) animates.
msg action toggle-synth-background
sleep 0.5
n=$(frames_over 2)
[ "$n" -ge 5 ] || fail "toggled to animated: $n frames in 2 s"
ok "toggle-synth-background animates again: $n frames in 2 s"
msg action toggle-synth-background
sleep 0.5
n=$(frames_over 2)
[ "$n" -eq 0 ] || fail "toggled back to still: $n frames in 2 s"
ok "toggled again it is still"

# Off over IPC: the surface and the worker go.
msg action set-synth-background off
sleep 0.5
[ "$(synth_thread)" -eq 0 ] || fail "the synth worker thread is still there"
shot off
read -r r g b < <(pixel off 720 4)
[ "$r $g $b" = "0 0 0" ] || fail "off, the top pixel is ($r $g $b)"
cpu_off=$(cpu_over 5)
ok "set-synth-background off: no surface, no worker, CPU $cpu_off ms/s"

# A config reload with a new mode wins over the action, and the knobs apply.
before=$(frames)
config 'mode "still"
    brightness 0.2'
until_true '[ "$(frames)" -gt "$before" ]'
sleep 0.5
shot reload
read -r r2 g2 b2 < <(pixel reload 720 880)
[ "$g2" -lt "$fg" ] || fail "brightness 0.2: the floor is ($r2 $g2 $b2), not darker than ($fr $fg $fb)"
n=$(frames_over 2)
[ "$n" -eq 0 ] || fail "still after reload: $n frames"
ok "a reload to still with brightness 0.2 draws once, darker: floor ($r2 $g2 $b2)"

# Idle: with no input for idle-still-after seconds it stands still (no input device here, so it
# stays still).
config 'mode "animated"
    idle-still-after 2'
until_true '[ "$(frames_over 1)" -gt 0 ]'
until_true 'grep -q "synth background: idle true" "$out/river.log"'
sleep 0.5
n=$(frames_over 3)
[ "$n" -eq 0 ] || fail "idle past idle-still-after: $n frames in 3 s"
ok "idle past idle-still-after 2: no frames"

echo "renderer: $renderer"
echo "CPU ms/s: animated $cpu_animated, covered $cpu_covered, still $cpu_still, off $cpu_off"
msg action quit --skip-confirmation >/dev/null 2>&1 || true
echo "all $pass synth checks passed"
