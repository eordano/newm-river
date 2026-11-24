#!/usr/bin/env bash
# The synth background's GPU path on a headless river: it is chosen and named; still at t = 0 it
# draws the look at 1440x960 and, on a scale-2 output, at 2880x1920 physical pixels (shots for
# comparing renders); animated it moves at the output's refresh rate in even scene
# time steps (from wp_presentation feedback); a GPU that fails mid-animation
# (NEWM_SYNTH_GPU_FAIL_AFTER) hands over to the CPU painter once, with one warning, and drawing
# goes on at the CPU painter's 4 frames a second with no spin.
#
# Needs a GPU river can render with and the window manager can make an EGL context on (river's
# headless backend on an NVIDIA GPU does). Run from the repository root inside its dev shell,
# after `zig build` and `cargo build --release -p newm-river --bins --examples`:
#   nix shell nixpkgs#jq nixpkgs#grim nixpkgs#wlr-randr -c newm/e2e/synth-gpu.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
out=${OUT:-/tmp/newm-river-e2e-synth-gpu}
mkdir -p "$out"
rm -f "$out"/*.log "$out"/*.ppm

pass=0
fail() {
    echo "FAIL: $*" >&2
    tail -n 30 "$out/river.log" >&2
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
frames() { grep -c 'synth frame committed' "$out/river.log" || true; }
frames_over() {
    local a b
    a=$(frames)
    sleep "$1"
    b=$(frames)
    echo $((b - a))
}

river_pid=
start() {
    [ -n "$river_pid" ] && kill "$river_pid" 2>/dev/null && wait "$river_pid" 2>/dev/null || true
    XDG_RUNTIME_DIR=$(mktemp -d)
    export XDG_RUNTIME_DIR
    export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
    export XDG_STATE_HOME=$out/state
    unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET DBUS_SESSION_BUS_ADDRESS
    printf 'prefer-no-csd\nsynth-background {\n    mode "%s"\n}\n' "$1" > "$out/config.kdl"
    export NIRI_CONFIG=$out/config.kdl
    : > "$out/river.log"
    "$river" -c "$2 RUST_LOG=newm_river=info,newm_river::river::synth=debug $nrv" \
        > "$out/river.log" 2>&1 &
    river_pid=$!
    until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
    NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
    WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
    export NIRI_SOCKET WAYLAND_DISPLAY
    until_true '[ "$("$nrv" msg --json outputs | jq length)" -ge 1 ]'
    name=$(wlr-randr --json | jq -r '.[0].name')
}
trap 'kill $river_pid 2>/dev/null || true' EXIT

# ---- the GPU draws the still look at t = 0, at scale 1 and at scale 2 ------------------------
start still ""
until_true 'grep -q "synth background: drawing on the" "$out/river.log"'
grep -q "synth background: drawing on the GPU" "$out/river.log" \
    || fail "not on the GPU: $(grep -o 'drawing on the .*' "$out/river.log" | head -n 1)"
ok "$(grep -o 'drawing on the GPU: [^,]*' "$out/river.log" | head -n 1)"

wlr-randr --output "$name" --custom-mode 1440x960 --scale 1 || fail "wlr-randr 1440x960"
until_true 'grep -q "1440x960" <(grep "synth paint\|drawing on the GPU" "$out/river.log") || [ "$(frames)" -ge 2 ]'
frames_over 1 >/dev/null
[ "$(frames_over 2)" -eq 0 ] || fail "still kept drawing"
grim -t ppm "$out/gpu-1440x960-t0.ppm"
ok "still at 1440x960 (scale 1): shot gpu-1440x960-t0.ppm"

before=$(frames)
wlr-randr --output "$name" --custom-mode 2880x1920 --scale 2 || fail "wlr-randr scale 2"
until_true '[ "$(frames)" -gt "$before" ]'
frames_over 1 >/dev/null
[ "$(frames_over 2)" -eq 0 ] || fail "still kept drawing at scale 2"
read -r lw lh < <("$nrv" msg --json outputs | jq -r '[.[]][0].logical | "\(.width) \(.height)"')
[ "$lw $lh" = "1440 960" ] || fail "logical size $lw x $lh"
grim -t ppm "$out/gpu-scale2-2880x1920-t0.ppm"
ok "still on a scale-2 output (1440x960 logical): shot at 2880x1920 physical"

# ---- animated: the display's rate, in even steps ----------------------------------------------
start animated ""
until_true '[ "$(frames)" -ge 30 ]'
sleep 1
refresh=$(wlr-randr --json | jq -r '[.[0].modes[] | select(.current)][0].refresh | if . > 0 then . else 60 end')
a=$(frames)
sleep 5
b=$(frames)
n=$((b - a))
expected=$(awk -v r="$refresh" 'BEGIN { printf "%d", r * 5 }')
[ "$n" -ge $((expected * 3 / 4)) ] && [ "$n" -le $((expected * 11 / 10)) ] \
    || fail "animated: $n frames in 5 s on a $refresh Hz output, expected about $expected"
# The scene time of each frame committed in those 5 s: every step within 10% of the median, so
# none doubled or missing (zero).
steps=$(grep -o 'synth frame committed t=[0-9.]*' "$out/river.log" | sed -n "$((a + 1)),${b}p" \
    | sed 's/.*t=//' | awk 'NR > 1 { printf "%.6f\n", $1 - prev } { prev = $1 }')
read -r median bad < <(echo "$steps" | sort -g | awk '
    { s[NR] = $1 } END { m = s[int((NR + 1) / 2)]; bad = 0
      for (i = 1; i <= NR; i++) if (s[i] < m * 0.9 || s[i] > m * 1.1) bad++
      printf "%.6f %d\n", m, bad }')
[ "$bad" -eq 0 ] && awk -v m="$median" 'BEGIN { exit !(m > 0) }' \
    || fail "uneven steps: median $median s, $bad steps off by more than 10%: $(echo "$steps" | sort -g | uniq -c | head)"
wm=$(pgrep -n -f "^$nrv" || pgrep -n newm-river)
tck=$(getconf CLK_TCK)
a=$(awk '{print $14 + $15}' "/proc/$wm/stat")
sleep 5
b=$(awk '{print $14 + $15}' "/proc/$wm/stat")
cpu=$(((b - a) * 1000 / tck / 5))
draw=$(grep -o 'synth frame committed t=[0-9.]* draw=[0-9]*' "$out/river.log" | tail -n 200 \
    | sed 's/.*draw=//' | sort -n | awk '{ s[NR] = $1 } END { print s[int((NR + 1) / 2)] }')
ok "animated on a $refresh Hz output: $n frames in 5 s, every scene step $median s (none off by 10%), CPU $cpu ms/s, median draw call $draw us"

# ---- a GPU that fails hands over to the CPU, once, without spinning --------------------------
start animated "NEWM_SYNTH_GPU_FAIL_AFTER=8"
until_true 'grep -q "the GPU failed" "$out/river.log"'
sleep 1
[ "$(grep -c 'the GPU failed' "$out/river.log")" -eq 1 ] || fail "more than one GPU failure logged"
n=$(frames_over 5)
[ "$n" -ge 16 ] && [ "$n" -le 22 ] || fail "after the GPU failed: $n frames in 5 s, expected the CPU painter's 4 a second"
wm=$(pgrep -n -f "^$nrv" || pgrep -n newm-river)
tck=$(getconf CLK_TCK)
a=$(awk '{print $14 + $15}' "/proc/$wm/stat")
sleep 5
b=$(awk '{print $14 + $15}' "/proc/$wm/stat")
cpu=$(((b - a) * 1000 / tck / 5))
[ "$cpu" -lt 500 ] || fail "after the GPU failed the window manager spends $cpu ms/s"
grep -q 'synth paint' "$out/river.log" || fail "the CPU painter did not take over"
ok "a failed swap after 8 frames: one warning, the CPU painter takes over, $n frames in 5 s, CPU $cpu ms/s (no spin)"

"$nrv" msg action quit --skip-confirmation >/dev/null 2>&1 || true
echo "all $pass synth GPU checks passed"
