#!/usr/bin/env bash
# Focus requests on a headless river: a layer surface asking for exclusive keyboard
# interactivity takes the keyboard while mapped and gives it back when it goes away; an on-demand
# one takes it when mapped and again on a click; a window activated with a fresh xdg-activation
# token (made from another client's keyboard enter, as a link click hands a browser) takes the
# focus; a stale token (made before the focus moved on) changes nothing; a taskbar's
# foreign-toplevel activate (wlrctl) focuses a window. Clients are examples/e2e_client.rs with
# --log-keys; input comes from examples/vinput.rs.
#
#   nix shell nixpkgs#jq -c newm/e2e/focus.sh
#
# RIVER, NEWM_RIVER, WLRCTL and OUT override the river, newm-river and wlrctl binaries and the
# output dir.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
out=${OUT:-/tmp/newm-river-e2e-focus}
mkdir -p "$out"
rm -f "$out"/*.log "$out"/tok-*

(cd "$root/newm" && cargo build -q --release --example vinput --example e2e_client) \
    || { echo "e2e: cannot build the examples" >&2; exit 2; }
vinput=$root/newm/target/release/examples/vinput
client=$root/newm/target/release/examples/e2e_client
if [ -n "${WLRCTL:-}" ]; then
    wlrctl=$WLRCTL
elif command -v wlrctl >/dev/null; then
    wlrctl=$(command -v wlrctl)
else
    wlrctl=$(nix build --no-link --print-out-paths --inputs-from "$root" nixpkgs#wlrctl)/bin/wlrctl
fi

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
export XDG_STATE_HOME=$out/state
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET DBUS_SESSION_BUS_ADDRESS

cat > "$out/config.kdl" <<KDL
prefer-no-csd
KDL
export NIRI_CONFIG=$out/config.kdl
"$nrv" validate

pass=0
pids=()
fail() {
    echo "FAIL: $*" >&2
    for f in "$out"/*.log; do
        [ "$f" = "$out/river.log" ] && continue
        echo "== $f" >&2
        cat "$f" >&2 || true
    done
    tail -n 40 "$out/river.log" >&2
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
count() { grep -c "^$2" "$out/$1.log" 2>/dev/null || true; }
focused() { "$nrv" msg --json focused-window | jq -r '.app_id // "none"'; }
start() {
    local log=$1
    shift
    "$client" "$@" > "$out/$log.log" 2>&1 &
    pids+=($!)
    echo $!
}

"$river" -log-level debug -c "RUST_LOG=newm_river=debug $nrv" > "$out/river.log" 2>&1 &
river_pid=$!
trap 'kill "${pids[@]}" $river_pid 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT

until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
export NIRI_SOCKET WAYLAND_DISPLAY
until_true '[ "$("$nrv" msg --json outputs | jq length)" -ge 1 ]'
ok "river runs newm-river"

# The headless river has a keyboard only while a virtual keyboard exists: hold one throughout.
"$vinput" sleep 900000 > /dev/null 2>&1 &
pids+=($!)

start a window --app-id e2e-a --log-keys --color 2060a0 > /dev/null
until_true '[ "$(count a kb-enter)" -ge 1 ]'
until_true '[ "$(focused)" = e2e-a ]'
ok "a new window takes the keyboard"

# ---- layer shell: exclusive
x=$(start x layer --namespace e2e-x --keyboard exclusive --log-keys --color c03030)
until_true '[ "$(count x kb-enter)" -ge 1 ]'
until_true '[ "$(count a kb-leave)" -ge 1 ]'
"$vinput" sleep 200 key 30 down key 30 up sleep 200 || fail "vinput"
until_true '[ "$(count x "kb-key 30")" -ge 1 ]'
[ "$(count a kb-key)" = 0 ] || fail "the window got a key while an exclusive layer surface was up"
ok "an exclusive layer surface takes the keyboard and its keys"

"$nrv" msg action focus-window --id "$("$nrv" msg --json windows | jq '.[] | select(.app_id=="e2e-a") | .id')" \
    > /dev/null || true
sleep 0.5
[ "$(count a kb-enter)" = 1 ] || fail "the window manager took the keyboard from an exclusive layer surface"
ok "focus requests are ignored while it is up"

kill "$x"
until_true '[ "$(count a kb-enter)" -ge 2 ]'
ok "the keyboard goes back to the window when it goes away"

# ---- layer shell: on demand
d=$(start d layer --namespace e2e-d --keyboard on-demand --log-keys --anchor bottom --color 30c030)
until_true '[ "$(count d kb-enter)" -ge 1 ]'
ok "an on-demand layer surface takes the keyboard when it maps"
a_id=$("$nrv" msg --json windows | jq '.[] | select(.app_id=="e2e-a") | .id')
"$nrv" msg action focus-window --id "$a_id" > /dev/null
until_true '[ "$(count d kb-leave)" -ge 1 ]'
until_true '[ "$(count a kb-enter)" -ge 3 ]'
ok "the window manager can take the keyboard back from it"
# The headless output is 1920x1080; the bar is 40 px along the bottom.
"$vinput" abs 960 1060 1920 1080 sleep 100 button 272 down button 272 up sleep 300 || fail "vinput"
until_true '[ "$(count d kb-enter)" -ge 2 ]'
"$vinput" sleep 200 key 31 down key 31 up sleep 200 || fail "vinput"
until_true '[ "$(count d "kb-key 31")" -ge 1 ]'
ok "a click on it gives it the keyboard and its keys"
kill "$d"
"$nrv" msg action focus-window --id "$a_id" > /dev/null
until_true '[ "$(focused)" = e2e-a ]'

# ---- xdg-activation: a fresh token from the focused client activates another client's window
start p window --app-id e2e-p --log-keys --activate-from "$out/tok-fresh" --color a0a020 > /dev/null
until_true '[ "$(focused)" = e2e-p ]'
start b window --app-id e2e-b --token-out "$out/tok-fresh" --color 20a0a0 > /dev/null
until_true '[ -s "$out/tok-fresh" ]'
until_true 'grep -q "^activated" "$out/p.log"'
until_true '[ "$(focused)" = e2e-p ]'
until_true '[ "$(count p kb-enter)" -ge 2 ]'
ok "a window activated with a fresh token takes the focus"

# ---- xdg-activation: a token asked for after the keyboard and pointer moved on changes nothing
start q window --app-id e2e-q --log-keys --activate-from "$out/tok-stale" --color a020a0 > /dev/null
until_true '[ "$(focused)" = e2e-q ]'
start c window --app-id e2e-c --token-out "$out/tok-made" --token-after "$out/tok-trigger" \
    --color 606060 > /dev/null
until_true '[ "$(focused)" = e2e-c ]'
start e window --app-id e2e-e --color 909090 > /dev/null
until_true '[ "$(focused)" = e2e-e ]'
# The user moved on with the keyboard and the pointer: a token is accepted whose serial is
# no older than the last keyboard enter or the last pointer enter, so the pointer moves onto the
# new window too.
read -r ex ey < <("$nrv" msg --json focused-window \
    | jq -r '.layout | "\((.tile_pos_in_workspace_view[0] + .window_size[0] / 2) | floor) \((.tile_pos_in_workspace_view[1] + .window_size[1] / 2) | floor)"')
"$vinput" abs 1 1 1920 1080 sleep 100 abs "$ex" "$ey" 1920 1080 sleep 200 || fail "vinput"
# Only now does e2e-c ask for its token, with the serial of its keyboard enter from before.
touch "$out/tok-trigger"
until_true '[ -s "$out/tok-made" ]'
mv "$out/tok-made" "$out/tok-stale"
until_true 'grep -q "^activated" "$out/q.log"'
sleep 0.5
[ "$(focused)" = e2e-e ] || fail "a stale token moved the focus to $(focused)"
q_urgent=$("$nrv" msg --json windows | jq '.[] | select(.app_id=="e2e-q") | .is_urgent')
[ "$q_urgent" = false ] || fail "a stale token marked the window urgent"
ok "a stale token moves no focus and marks nothing urgent (it is rejected)"

# ---- foreign-toplevel activate (a taskbar)
"$wlrctl" toplevel focus app_id:e2e-a || fail "wlrctl toplevel focus"
until_true '[ "$(focused)" = e2e-a ]'
ok "a taskbar's foreign-toplevel activate focuses the window"

echo "focus e2e: $pass ok"
