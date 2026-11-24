#!/usr/bin/env bash
# newm-river end to end on a headless river: starts river (wlroots' headless backend, pixman, one
# 1280x720 output) with newm-river as its window manager and a test config, opens foot windows,
# drives actions over `newm-river msg action ...` and checks the tiles `newm-river msg --json
# windows` reports, drives the Super key and the pointer through a virtual keyboard and pointer
# (`examples/vinput.rs`), then takes a screenshot with grim and quits through IPC.
#
# Run from the repository root inside its dev shell, after `zig build` and `cargo build
# --release`, with jq on PATH:
#   nix shell nixpkgs#jq -c newm/e2e/headless.sh
# RIVER, NEWM_RIVER and OUT override the river binary, the newm-river binary and the output dir.
# With dbus-daemon and busctl on PATH, a private session bus carries the ScreenShot2 checks.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
# The usage log checks need newm-river built with the `telemetry` feature: unless NEWM_RIVER names
# a binary, this builds one first.
[ -n "${NEWM_RIVER:-}" ] \
    || (cd "$root/newm" && cargo build -q --release -p newm-river --bins --features telemetry)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
out=${OUT:-/tmp/newm-river-e2e}
mkdir -p "$out"
for tool in jq foot grim; do
    command -v "$tool" >/dev/null || { echo "e2e: $tool is not on PATH" >&2; exit 2; }
done

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET
# The usage log (newm-river's model/usage.rs) goes to $out/state/newm-river/usage.jsonl.
export XDG_STATE_HOME=$out/state
rm -rf "$XDG_STATE_HOME"
usage=$XDG_STATE_HOME/newm-river/usage.jsonl
unset DBUS_SESSION_BUS_ADDRESS
dbus_pid=
if command -v dbus-daemon >/dev/null && command -v busctl >/dev/null \
    && bus=$(dbus-daemon --session --fork --print-address=1 --print-pid=1 2>/dev/null); then
    DBUS_SESSION_BUS_ADDRESS=$(sed -n 1p <<<"$bus")
    dbus_pid=$(sed -n 2p <<<"$bus")
    export DBUS_SESSION_BUS_ADDRESS
fi

cat > "$out/config.kdl" <<'KDL'
prefer-no-csd
layout {
    gaps 10
    border { width 1; active-color "#82FB9C8C"; inactive-color "transparent"; }
    shadow { on; softness 26; spread 0; color "#82FB9C36"; }
    canvas {
        tile-width 119
        tile-height 113
        zoom-step 0.8
        zoom-max 7
        overview-dim 0.4
        center-on-switch
        default-width { fixed 960; }
        default-height { proportion 0.666700; }
        super-tap-overview
    }
}
window-rule {
    match app-id="^e2e-float$"
    open-floating true
}
window-rule {
    match app-id="^e2e-2$"
    default-column-width { fixed 228; }
}
binds {
    Mod+Return { spawn "foot"; }
    Mod+h { focus-column-left; }
    Mod+WheelScrollDown { zoom-out; }
    Mod+WheelScrollUp { zoom-in; }
}
KDL
export NIRI_CONFIG=$out/config.kdl
"$nrv" validate

pass=0
fail() {
    echo "FAIL: $*" >&2
    tail -n 40 "$out/river.log" >&2
    echo "windows:" >&2
    "$nrv" msg --json windows 2>/dev/null | jq -c '.[] | {app_id, layout}' >&2 || true
    echo "usage log, last ops:" >&2
    grep -v '"op":"motion"' "$usage" 2>/dev/null | tail -n 6 | jq -c '{op, app, window, before, after, size_source, home_moved, during_drag}' >&2 || true
    exit 1
}
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }

"$river" -c "RUST_LOG=newm_river=debug $nrv" > "$out/river.log" 2>&1 &
river_pid=$!
trap 'kill $river_pid $dbus_pid 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT

# Waits up to 10 s for a shell condition.
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
windows() { msg --json windows; }
win() { windows | jq -c ".[] | select(.app_id == \"$1\")"; }
# One field of a window; numbers as integers (jq 1.7 prints 942.0 as it came).
field() { win "$1" | jq -r "$2 | if type == \"number\" then floor else . end"; }

until_true '[ "$(msg --json outputs | jq length)" -ge 1 ]'
read -r ow oh < <(msg --json outputs | jq -r '[.[]][0].logical | "\(.width) \(.height)"')
ok "river runs newm-river; output ${ow}x${oh}"

# foot picks 930x430 px where newm-river leaves the size to it (a first configure of 0): with the
# border, the gaps and a pixel, 8x4 tiles of 119x113.
open() { foot --app-id "$1" --window-size-pixels=930x430 sh -c 'sleep 600' >/dev/null 2>&1 & }
# The usage log's last `open` line for an app.
open_line() { grep '"op":"open"' "$usage" 2>/dev/null | jq -c "select(.app == \"$1\")" | tail -n 1; }
# IPC reports where windows go; the camera takes 250 ms to get there on screen, so a screenshot
# waits for it.
settle() { sleep 0.4; }

open e2e-1
until_true '[ "$(field e2e-1 .layout.window_size[0])" -gt 0 ] 2>/dev/null'
until_true '[ "$(field e2e-1 .layout.tile_size[0])" = 942 ] 2>/dev/null'
[ "$(field e2e-1 .layout.tile_size[1])" = 442 ] || fail "e2e-1 is not 4 tiles tall: $(win e2e-1)"
[ "$(field e2e-1 .layout.tile_pos_in_workspace_view[0])" = 10 ] || fail "e2e-1 not at the gap: $(win e2e-1)"
[ "$(field e2e-1 .is_focused)" = true ] || fail "e2e-1 not focused"
until_true '[ -n "$(open_line e2e-1)" ]'
open_line e2e-1 | jq -e '.size_source == "app" and .app_px == [930, 430] and .span == [8, 4]' \
    >/dev/null || fail "e2e-1's open line: $(open_line e2e-1)"
ok "a new window opens at the size its app picks (930x430 px: 8x4 tiles of 119x113) at the origin"

open e2e-2
until_true '[ "$(field e2e-2 .layout.window_size[0])" -gt 0 ] 2>/dev/null'
x1=$(field e2e-1 .layout.tile_pos_in_workspace_view[0])
x2=$(field e2e-2 .layout.tile_pos_in_workspace_view[0])
[ $((x2 - x1)) = 952 ] || fail "e2e-2 is not beside e2e-1: $x1 $x2"
[ "$(field e2e-2 .is_focused)" = true ] || fail "e2e-2 not focused"
w2=$(field e2e-2 .layout.tile_size[0])
[ "$x2" -ge 0 ] && [ $((x2 + w2)) -le "$ow" ] || fail "e2e-2 not shown whole: $x2 $w2"
until_true '[ -n "$(open_line e2e-2)" ]'
open_line e2e-2 | jq -e '.size_source == {"w": "rule", "h": "app"} and .span == [2, 4]' \
    >/dev/null || fail "e2e-2's open line: $(open_line e2e-2)"
ok "a 2-tile window opens beside the focus, where the view scrolls least (no scroll), shown whole"
ok "its rule sets its width, its app its height (the usage log's size_source)"

msg action focus-column-left
until_true '[ "$(field e2e-1 .is_focused)" = true ]'
x1=$(field e2e-1 .layout.tile_pos_in_workspace_view[0])
[ "$x1" -ge 0 ] || fail "e2e-1 not shown after focusing it: $x1"
ok "focus-column-left focuses and shows the left window"

msg action resize-edge right -2
until_true '[ "$(field e2e-1 .layout.tile_size[0])" = 704 ]'
until_true '[ $(( $(field e2e-2 .layout.tile_pos_in_workspace_view[0]) - $(field e2e-1 .layout.tile_pos_in_workspace_view[0]) )) = 714 ]'
ok "resize-edge right -2 shrinks to 6 tiles and the neighbour closes the gap"

msg action swap-neighbour right
until_true '[ "$(field e2e-1 .layout.tile_pos_in_workspace_view[0])" -gt "$(field e2e-2 .layout.tile_pos_in_workspace_view[0])" ]'
ok "swap-neighbour right exchanges the two windows' tiles"

tile1=$(field e2e-1 .layout.tile_size[0])
size1=$(field e2e-1 .layout.window_size[0])
msg action zoom-out
sleep 0.3
[ "$(field e2e-1 .layout.tile_size[0])" = "$tile1" ] || fail "zoom-out changed the tiles: $(win e2e-1)"
[ "$(field e2e-1 .layout.window_size[0])" = "$size1" ] || fail "zoom-out resized: $(win e2e-1)"
grim "$out/zoomed-out.png" || fail "grim"
msg action zoom-in
ok "zoom-out is a camera: tiles and window sizes stay ($out/zoomed-out.png)"

open e2e-float
until_true '[ "$(field e2e-float .is_floating)" = true ] 2>/dev/null'
until_true '[ "$(field e2e-float .is_focused)" = true ]'
ok "an open-floating rule floats the window and focuses it"

msg action close-window
until_true '[ -z "$(win e2e-float)" ]'
until_true '[ "$(field e2e-1 .is_focused)" = true ]'
ok "close-window closes the focused window and the most recent one takes the focus"

msg action toggle-window-floating
until_true '[ "$(field e2e-1 .is_floating)" = true ]'
msg action toggle-window-floating
until_true '[ "$(field e2e-1 .is_floating)" = false ]'
ok "toggle-window-floating floats and re-tiles"

msg --json focused-window | jq -e '.app_id == "e2e-1"' >/dev/null || fail "focused-window"
msg --json workspaces | jq -e 'length == 1 and .[0].is_focused' >/dev/null || fail "workspaces"
msg version >/dev/null || fail "version"
ok "focused-window, workspaces and version answer"

# The pointer and Super (and its tap) through a virtual keyboard and pointer (examples/vinput.rs):
# headless river has no input devices, and touchpad gestures have no virtual device at all.
(cd "$root/newm" && cargo build -q --release --example vinput) || fail "cannot build vinput"
vin() { "$root/newm/target/release/examples/vinput" "$@" || fail "vinput $*"; }
count() { grep -c "$1" "$out/river.log" || true; }

vin super down sleep 40 super up
until_true '[ "$(count "pointer Act(ToggleOverview)")" = 1 ]'
vin super down sleep 40 super up
until_true '[ "$(count "pointer Act(ToggleOverview)")" = 2 ]'
ok "a tap of Super alone toggles the overview (twice: open, close)"

zooms=$(count "pointer Act(ZoomOut)")
vin super down sleep 40 wheel 1 sleep 40 super up
until_true '[ "$(count "pointer Act(ZoomOut)")" = $((zooms + 1)) ]'
msg action zoom-in
[ "$(count "pointer Act(ToggleOverview)")" = 2 ] || fail "Super with a wheel notch was a tap"
ok "Super+wheel down zooms out one step through Mod+WheelScrollDown, and is no tap"

vin abs 640 360 "$ow" "$oh" super down sleep 60 move 20 0 sleep 60 move 60 0 sleep 60 super up
until_true '[ "$(count "pointer PanEnd")" = 1 ]'
[ "$(count "pointer OpStart")" = 1 ] || fail "the pan ran no pointer operation"
ok "Super with plain motion pans past 8 px and settles the focus on release"

row() { field "$1" .layout.pos_in_scrolling_layout[1]; }
span() { echo "$(field "$1" .layout.tile_size[0])x$(field "$1" .layout.tile_size[1])"; }
msg action focus-window --id "$(field e2e-2 .id)"
until_true '[ "$(field e2e-2 .layout.tile_pos_in_workspace_view[0])" -ge 0 ]'
# IPC reports where the camera goes; a Super drag picks the window drawn under the pointer
# (`Desktop::window_at`, as drawn), so the camera must get there first.
settle
dj=$(( $(row e2e-2) - $(row e2e-1) ))
span2=$(span e2e-2)
x=$(field e2e-2 .layout.tile_pos_in_workspace_view[0])
y=$(field e2e-2 .layout.tile_pos_in_workspace_view[1])
vin abs $((x + 40)) $((y + 40)) "$ow" "$oh" super down sleep 60 button 272 down sleep 60 \
    move 125 0 sleep 60 move 125 120 sleep 60 button 272 up sleep 60 super up
until_true '[ $(( $(row e2e-2) - $(row e2e-1) )) = $((dj + 1)) ]'
[ "$(span e2e-2)" = "$span2" ] || fail "the drag changed e2e-2's span: $(win e2e-2)"
[ "$(field e2e-2 .is_focused)" = true ] || fail "the dragged window is not focused"
ok "Super+left drag drops e2e-2 on the tiles nearest where it is drawn (a row down), span kept"

taps=$(count "pointer Act(ToggleOverview)")
vin super down sleep 30 key 21 down sleep 10 key 21 up sleep 30 super up
settle
[ "$(count "pointer Act(ToggleOverview)")" = "$taps" ] || fail "Super with an unbound key was a tap"
ok "an unbound key pressed with Super cancels the tap (stop_repeat on Super's own binding)"

dj=$(( $(row e2e-2) - $(row e2e-1) ))
x=$(field e2e-2 .layout.tile_pos_in_workspace_view[0])
y=$(field e2e-2 .layout.tile_pos_in_workspace_view[1])
vin abs $((x + 40)) $((y + 40)) "$ow" "$oh" super down sleep 60 button 272 down sleep 60 \
    move 125 0 sleep 60 move 125 120 sleep 60 key 1 down sleep 30 key 1 up sleep 60 \
    button 272 up sleep 60 super up
until_true '[ "$(count "pointer CancelDrag")" = 1 ]'
settle
[ $(( $(row e2e-2) - $(row e2e-1) )) = "$dj" ] || fail "Escape did not cancel the drop: $(win e2e-2)"
[ "$(count "pointer Drop")" = 1 ] || fail "the cancelled drag dropped"
ok "Escape during a drag cancels it: nothing drops, the window goes home"

# A resize drag: Super+right on a window's outer third drags that edge; a box
# in the border's active colour (#82FB9C) shows the window's tiles after the release, Escape
# cancels and the release applies it. The edge is the side of e2e-2 with room on screen for a
# tile, wherever it opened; the sample point lies in that tile, 20 px in from its far side, where
# everything under the box is grey (a foot window, the background) until the box's green tints it.
green() { set -- $(grim -g "$1 1x1" -t ppm - | tail -c 3 | od -An -tu1 | xargs); echo $(($2 - $1)); }
cancels() { grep -c '"op":"cancel"' "$usage" || true; }
settle
read -r x y w h < <(win e2e-2 | jq -r '.layout | [.tile_pos_in_workspace_view[], .tile_size[]]
    | map(floor | tostring) | join(" ")')
if [ $((x + w + 140)) -lt "$ow" ]; then
    gx=$((x + w - 10)) dx=130 sx=$((x + w + 119 - 20)) side=right
elif [ $((x - 140)) -gt 0 ]; then
    gx=$((x + 10)) dx=-130 sx=$((x - 119 + 20)) side=left
else
    fail "no room beside e2e-2 for a tile: $(win e2e-2)"
fi
gy=$((y + h / 2))
at="$sx,$gy"
[ "$(green "$at")" -lt 8 ] || fail "the sample point $at is not grey before the drag: $(green "$at")"
c0=$(cancels)
vin abs "$gx" "$gy" "$ow" "$oh" super down sleep 60 button 273 down sleep 60 \
    move "$dx" 0 sleep 2000 key 1 down sleep 30 key 1 up sleep 60 button 273 up sleep 60 \
    super up &
vin_pid=$!
until_true '[ "$(count "pointer ResizeTo")" -ge 1 ]'
sleep 0.5
tint=$(green "$at")
grim "$out/resize-preview.png" || fail "grim"
[ "$(field e2e-2 .layout.tile_size[0])" = "$w" ] || fail "the drag resized before the release"
[ "$tint" -ge 12 ] || fail "no resize box at $at during the drag: green-red $tint"
ok "Super+right drags e2e-2's $side edge: a box previews the tile it gains (green-red $tint, $out/resize-preview.png)"
wait $vin_pid || fail "vinput (resize drag, Escape)"
until_true '[ "$(count "pointer CancelResize")" = 1 ]'
settle
[ "$(field e2e-2 .layout.tile_size[0])" = "$w" ] || fail "Escape did not cancel the resize: $(win e2e-2)"
[ "$(green "$at")" -lt 8 ] || fail "the box outlived the cancel: $(green "$at")"
[ "$(cancels)" = $((c0 + 1)) ] || fail "the cancel is not logged"
[ "$(count "pointer ResizeEnd")" = 0 ] || fail "the cancelled resize drag ended"
ok "Escape during a resize drag cancels it: the box goes, the window keeps its size, a cancel logged"

vin abs "$gx" "$gy" "$ow" "$oh" super down sleep 60 button 273 down sleep 60 \
    move "$dx" 0 sleep 400 button 273 up sleep 60 super up
until_true '[ "$(field e2e-2 .layout.tile_size[0])" = $((w + 119)) ]'
settle
grep '"op":"resize"' "$usage" | tail -n 1 | jq -e '.app == "e2e-2"' >/dev/null \
    || fail "no resize line for e2e-2: $(grep '"op":"resize"' "$usage" | tail -n 1)"
[ "$(green "$at")" -lt 8 ] || fail "the box outlived the release: $(green "$at")"
ok "the release applies the previewed tile (e2e-2 $w -> $((w + 119)) px) and takes the box down"
msg action undo
until_true '[ "$(field e2e-2 .layout.tile_size[0])" = "$w" ]'

# A double Super+right click on a tiled window (niri's double resize click, 400 ms): on its right third it drags nothing and spans the tiles the screen holds
# across, and again goes back to the fewest tiles; both are resizes that undo takes back.
dclick() {
    vin abs "$1" "$2" "$ow" "$oh" super down sleep 60 button 273 down sleep 30 button 273 up \
        sleep 100 button 273 down sleep 30 button 273 up sleep 60 super up
}
tbox() {
    win "$1" | jq -r '.layout | [.tile_pos_in_workspace_view[], .tile_size[]]
        | map(floor | tostring) | join(" ")'
}
settle
read -r x y w h < <(tbox e2e-2)
dclick $((x + w - 10)) $((y + h / 2))
until_true '[ "$(field e2e-2 .layout.tile_size[0])" -gt $((ow - 130)) ]'
full=$(field e2e-2 .layout.tile_size[0])
[ $(((full + 10) % 119)) = 0 ] || fail "full width is no whole tiles: $(win e2e-2)"
[ "$(field e2e-2 .layout.tile_size[1])" = "$h" ] || fail "full width changed the height: $(win e2e-2)"
[ "$(field e2e-2 .is_focused)" = true ] || fail "the double click did not focus e2e-2"
ok "a double Super+right click on e2e-2's right third toggles full width ($w -> $full px)"
settle
read -r x y w2 h < <(tbox e2e-2)
dclick $((x + w2 - 10)) $((y + h / 2))
until_true '[ "$(field e2e-2 .layout.tile_size[0])" -lt "$full" ]'
ok "a second double click takes it back to its fewest tiles ($(field e2e-2 .layout.tile_size[0]) px)"
msg action undo
msg action undo

# pick-window (niri's request, which a Ctrl+Super+Escape kill-window bind sends): the next click
# answers the window under it, Escape answers nothing, and a newer pick answers the older one with
# nothing. A click or Escape before the pick starts goes to the app, so each
# repeats until the pick answers.
pick() { msg --json pick-window >"$out/$1.json" 2>&1; }
until_answered() {
    for _ in $(seq 20); do
        sleep 0.3
        kill -0 "$1" 2>/dev/null || return 0
        vin "${@:2}"
    done
    fail "pick-window got no answer"
}
settle
read -r x y w h < <(tbox e2e-2)
pick pick-click &
p=$!
until_answered "$p" abs $((x + w / 2)) $((y + h / 2)) "$ow" "$oh" button 272 down sleep 30 button 272 up
wait "$p" || fail "pick-window: $(cat "$out/pick-click.json")"
jq -e '.app_id == "e2e-2" and .pid > 1' "$out/pick-click.json" >/dev/null \
    || fail "the click picked $(cat "$out/pick-click.json")"
pick pick-old &
p1=$!
sleep 0.5
pick pick-new &
p2=$!
until_true '! kill -0 "$p1" 2>/dev/null'
[ "$(cat "$out/pick-old.json")" = null ] || fail "the older pick answered $(cat "$out/pick-old.json")"
until_answered "$p2" key 1 down sleep 30 key 1 up
wait "$p2" || fail "pick-window: $(cat "$out/pick-new.json")"
[ "$(cat "$out/pick-new.json")" = null ] || fail "Escape picked $(cat "$out/pick-new.json")"
ok "pick-window answers the clicked window (e2e-2), a newer pick and Escape answer nothing"
until_true '[ "$(field e2e-2 .layout.tile_size[0])" = "$w" ]'

# A floating window resizes by the pixel (niri's floating interactive resize, Mod+right
# on a floating window): from a corner both edges move, from the left edge the window moves and
# its right edge stays, the app is asked as the drag moves, Escape puts size and place back. The
# drags go inward, so nothing depends on where the window opened. foot keeps to the cell grid
# unless told not to.
foot --app-id e2e-float -o resize-by-cells=no --window-size-pixels=930x430 sh -c 'sleep 600' \
    >/dev/null 2>&1 &
until_true '[ "$(field e2e-float .is_floating)" = true ] 2>/dev/null'
until_true '[ "$(field e2e-float .layout.window_size[0])" = 930 ]'
settle
fbox() {
    win e2e-float | jq -r '.layout | [(.tile_pos_in_workspace_view | map(. + 1))[], .window_size[]]
        | map(floor | tostring) | join(" ")'
}
read -r fx fy fw fh < <(fbox)
vin abs $((fx + fw - 15)) $((fy + fh - 15)) "$ow" "$oh" super down sleep 60 button 273 down \
    sleep 60 move -30 -20 sleep 100 move -30 -20 sleep 300 button 273 up sleep 60 super up
until_true '[ "$(fbox)" = "$fx $fy $((fw - 60)) $((fh - 40))" ]'
ok "Super+right on a floating window's corner resizes it by the pixel, top-left in place (${fw}x$fh -> $((fw - 60))x$((fh - 40)))"

read -r fx fy fw fh < <(fbox)
vin abs $((fx + 15)) $((fy + fh / 2)) "$ow" "$oh" super down sleep 60 button 273 down sleep 60 \
    move 50 0 sleep 300 button 273 up sleep 60 super up
until_true '[ "$(fbox)" = "$((fx + 50)) $fy $((fw - 50)) $fh" ]'
ok "from its left edge the window moves with the edge and its right edge stays at $((fx + fw))"

read -r fx fy fw fh < <(fbox)
c0=$(count "pointer CancelResize")
vin abs $((fx + 15)) $((fy + fh / 2)) "$ow" "$oh" super down sleep 60 button 273 down sleep 60 \
    move 70 0 sleep 2000 key 1 down sleep 30 key 1 up sleep 60 button 273 up sleep 60 super up &
vin_pid=$!
until_true '[ "$(fbox)" = "$((fx + 70)) $fy $((fw - 70)) $fh" ]'
grim "$out/floating-resize.png" || fail "grim"
ok "the app resizes as the drag moves, before the release ($out/floating-resize.png)"
wait $vin_pid || fail "vinput (floating resize, Escape)"
until_true '[ "$(count "pointer CancelResize")" = $((c0 + 1)) ]'
until_true '[ "$(fbox)" = "$fx $fy $fw $fh" ]'
ok "Escape puts the floating window's size and place back"
msg action close-window
until_true '[ -z "$(win e2e-float)" ]'

# The overview: every window drawn on the output, what lies outside the screen area dimmed,
# focus-window (what a click sends) closes it centred on the window.
red() { grim -g "$1 1x1" -t ppm - | tail -c 3 | od -An -tu1 | awk '{print $1}'; }
xy() { field "$1" '.layout.tile_pos_in_workspace_view | map(floor + 30 | tostring) | join(",")'; }
open e2e-3
until_true '[ "$(field e2e-3 .is_focused)" = true ]'
open e2e-4
until_true '[ "$(field e2e-4 .is_focused)" = true ]'
settle
# A window whose sampled point (its top-left + 30 px, `xy`) lies off screen at home; where e2e-3
# and e2e-4 open follows the camera (beside the focus where the view scrolls least), so it may be
# only partly off screen.
hidden=$(windows | jq -r '[.[] | select(.layout.tile_pos_in_workspace_view | map(. + 30) as $p
    | $p[0] >= 1280 or $p[0] < 0 or $p[1] >= 720 or $p[1] < 0) | .app_id] | first // empty')
[ -n "$hidden" ] || fail "no window's sampled point off screen before the overview: $(windows)"
msg action toggle-overview
settle
msg --json overview-state | jq -e '.is_open' >/dev/null || fail "overview-state is not open"
windows | jq -e 'all(.[]; .layout.tile_pos_in_workspace_view as $p
    | $p[0] >= 0 and $p[1] >= 0 and $p[0] < 1280 and $p[1] < 720)' >/dev/null \
    || fail "a window is off the output in the overview: $(windows)"
grim "$out/overview.png" || fail "grim"
ok "toggle-overview draws every window on the output ($out/overview.png)"

dimmed=$(red "$(xy "$hidden")")
bright=$(red "$(xy e2e-4)")
[ "$bright" -gt 0 ] && [ $((dimmed * 100)) -le $((bright * 75)) ] \
    || fail "dim: $hidden (off screen at home) $dimmed, e2e-4 (on screen) $bright"
ok "the overview dims what lies outside the screen area ($hidden $dimmed, e2e-4 $bright)"

msg action focus-window --id "$(field e2e-3 .id)"
settle
msg --json overview-state | jq -e '.is_open == false' >/dev/null || fail "the pick left it open"
[ "$(field e2e-3 .is_focused)" = true ] || fail "the pick did not focus e2e-3"
cx=$(win e2e-3 | jq '.layout.tile_pos_in_workspace_view[0] + .layout.tile_size[0] / 2 | floor')
[ "$cx" -gt 570 ] && [ "$cx" -lt 710 ] || fail "e2e-3 is not centred: $(win e2e-3)"
grim "$out/picked.png" || fail "grim"
ok "focus-window in the overview closes it centred on the window ($out/picked.png)"

# ScreenShot2 on the private session bus: the reply's size, then the pixels in the pipe.
if [ -n "$dbus_pid" ]; then
    shot() {
        rm -f "$out/shot.raw"
        busctl --user call org.kde.KWin.ScreenShot2 /org/kde/KWin/ScreenShot2 \
            org.kde.KWin.ScreenShot2 "$@" 3>"$out/shot.raw"
    }
    num() { grep -o "\"$1\" [ud] [0-9.]*" <<<"$reply" | awk '{print $3}'; }
    until_true 'busctl --user status org.kde.KWin.ScreenShot2 >/dev/null 2>&1'
    reply=$(shot CaptureActiveScreen 'a{sv}h' 0 3) || fail "CaptureActiveScreen: $reply"
    [ "$(num width)x$(num height)" = 1280x720 ] || fail "CaptureActiveScreen replied $reply"
    until_true '[ "$(stat -c %s "$out/shot.raw")" -ge $((1280 * 720 * 4)) ]'
    [ "$(tr -d '\0' <"$out/shot.raw" | wc -c)" -gt 0 ] || fail "the screen capture is blank"
    ok "ScreenShot2 CaptureActiveScreen: $(num width)x$(num height) through the pipe"

    reply=$(shot CaptureActiveWindow 'a{sv}h' 0 3) || fail "CaptureActiveWindow: $reply"
    want=$(field e2e-3 '.layout.window_size[0]')
    [ "$(num width)" -ge $((want - 4)) ] && [ "$(num width)" -le $((want + 4)) ] \
        || fail "CaptureActiveWindow replied $reply for e2e-3 $want wide"
    size=$(($(num width) * $(num height) * 4))
    until_true '[ "$(stat -c %s "$out/shot.raw")" -ge '"$size"' ]'
    ok "ScreenShot2 CaptureActiveWindow: $(num width)x$(num height), e2e-3 is $want wide"

    reply=$(shot CaptureArea 'iiuua{sv}h' 100 50 200 100 0 3) || fail "CaptureArea: $reply"
    [ "$(num width)x$(num height)" = 200x100 ] || fail "CaptureArea replied $reply"
    ok "ScreenShot2 CaptureArea: 200x100"
else
    echo "skip - ScreenShot2: no dbus-daemon or busctl"
fi

# The KDE services on the private bus, in the shapes KDE's clients send.
if [ -n "$dbus_pid" ] && command -v dbus-monitor >/dev/null; then
    accel() { busctl --user call org.kde.kglobalaccel /kglobalaccel org.kde.KGlobalAccel "$@"; }
    id=(4 org.kde.spectacle.desktop FullScreenScreenShot Spectacle Capture)
    f10=16777273 # Qt::Key_F10
    until_true 'busctl --user status org.kde.kglobalaccel >/dev/null 2>&1'
    accel doRegister as "${id[@]}" || fail "doRegister"
    accel setShortcutKeys 'asa(ai)u' "${id[@]}" 1 4 "$f10" 0 0 0 8 >/dev/null || fail "default keys"
    reply=$(accel setShortcutKeys 'asa(ai)u' "${id[@]}" 1 4 "$f10" 0 0 0 2) || fail "setShortcutKeys"
    [ "$reply" = "a(ai) 1 4 $f10 0 0 0" ] || fail "setShortcutKeys answered $reply"
    path=$(accel getComponent s org.kde.spectacle.desktop)
    [ "$path" = 'o "/component/org_kde_spectacle_desktop"' ] || fail "getComponent answered $path"
    comp() { busctl --user call org.kde.kglobalaccel /component/org_kde_spectacle_desktop org.kde.kglobalaccel.Component "$@"; }
    for args in "shortcutNames s default" "shortcutNames" "allShortcutInfos s default" "allShortcutInfos"; do
        # shellcheck disable=SC2086 # the method and its arguments, split on purpose
        comp $args >/dev/null || fail "Component.$args failed (System Settings calls the no-context form)"
    done
    accel doRegister as 4 kate find Kate Find || fail "doRegister kate"
    reply=$(accel setShortcutKeys 'asa(ai)u' 4 kate find Kate Find 1 4 "$f10" 0 0 0 2)
    [ "$reply" = "a(ai) 1 4 0 0 0 0" ] || fail "a key Spectacle holds went to Kate: $reply"
    ok "kglobalaccel: Spectacle registers F10 as KGlobalAccel does; Kate is refused the same key"

    dbus-monitor --session "type='signal',interface='org.kde.kglobalaccel.Component'" \
        > "$out/kglobalaccel.log" 2>&1 &
    monitor_pid=$!
    settle
    vin key 68 down sleep 30 key 68 up
    until_true 'grep -q "member=globalShortcutReleased" "$out/kglobalaccel.log"'
    grep -A3 'member=globalShortcutPressed' "$out/kglobalaccel.log" | grep -q FullScreenScreenShot \
        || fail "no press for Spectacle: $(cat "$out/kglobalaccel.log")"
    kill "$monitor_pid" 2>/dev/null
    ok "a press of F10 reaches Spectacle's component as globalShortcutPressed, then Released"

    reply=$(busctl --user call org.kde.KWin /WindowsRunner org.kde.krunner1 Match s e2e-1) \
        || fail "KRunner Match"
    id1=$(field e2e-1 .id)
    grep -q "\"window-$id1\"" <<<"$reply" || fail "KRunner found no e2e-1 (id $id1): $reply"
    busctl --user call org.kde.KWin /WindowsRunner org.kde.krunner1 Run ss "window-$id1" "" \
        || fail "KRunner Run"
    until_true '[ "$(field e2e-1 .is_focused)" = true ]'
    ok "KRunner's window search finds e2e-1 by its app id, and running it focuses it"

    saver() { busctl --user call org.freedesktop.ScreenSaver /org/freedesktop/ScreenSaver \
        org.freedesktop.ScreenSaver "$@"; }
    [ "$(saver GetActive)" = "b false" ] || fail "GetActive: $(saver GetActive)"
    cookie=$(saver Inhibit ss vlc video | awk '{print $2}')
    [ "${cookie:-0}" -gt 0 ] || fail "Inhibit answered no cookie"
    # Each busctl call is its own D-Bus client: a release drops only the caller's own cookie
    # (72d8121b), and a client leaving the bus takes its inhibitions along.
    if saver UnInhibit u "$cookie" >/dev/null 2>&1; then
        fail "UnInhibit from another client released the cookie"
    fi
    # xdg-desktop-portal-kde calls PowerDevil's main name (inhibit.cpp), not the PolicyAgent name.
    agent=(org.kde.Solid.PowerManagement /org/kde/Solid/PowerManagement/PolicyAgent
        org.kde.Solid.PowerManagement.PolicyAgent)
    cookie=$(busctl --user call "${agent[@]}" AddInhibition uss 4 org.gnome.Totem Playing \
        | awk '{print $2}')
    [ "${cookie:-0}" -gt 0 ] || fail "AddInhibition answered no cookie"
    # ReleaseInhibition answers nothing; from another client it releases nothing.
    busctl --user call "${agent[@]}" ReleaseInhibition u "$cookie" || fail "ReleaseInhibition"
    until_true '[ "$(busctl --user call "${agent[@]}" HasInhibition u 4)" = "b false" ]'
    ok "ScreenSaver GetActive/Inhibit/UnInhibit and the Inhibit portal's PolicyAgent calls answer, UnInhibit only from the holder"
else
    echo "skip - KDE services: no dbus-daemon, busctl or dbus-monitor"
fi

# linux-dmabuf feedback (examples/feedback_probe.rs, a window committing a new colour every 16 ms):
# its surface gets one feedback when it maps, then another only when its output, renderer or
# scanout candidacy changes, not one per commit or per frame: at rest, while the camera zooms and
# around the overview. FEEDBACK_MAX (3) bounds the feedbacks (`done`) one 5 s step may bring.
(cd "$root/newm" && cargo build -q --release --example feedback_probe) || fail "cannot build feedback_probe"
fbmax=${FEEDBACK_MAX:-3}
fbjson=$out/feedback.json
fb() { jq -r ".$1" "$fbjson"; }
fbnow() { echo "$(fb done) $(fb leave) $(fb commits)"; }
fbstep() { # name, the `fbnow` the step began with
    local d0 l0 c0 d l c
    read -r d0 l0 c0 <<<"$2"
    d=$(($(fb done) - d0)) l=$(($(fb leave) - l0)) c=$(($(fb commits) - c0))
    echo "feedback $1: $d feedbacks, $l leaves, $c commits in 5 s"
    [ "$c" -ge 100 ] || fail "feedback_probe committed $c frames in 5 s $1"
    [ "$d" -le "$fbmax" ] || fail "$d dmabuf feedbacks in 5 s $1 (at most $fbmax): $(cat "$fbjson")"
    [ "$l" = 0 ] || fail "$l wl_surface.leave in 5 s $1"
}
"$root/newm/target/release/examples/feedback_probe" --app-id e2e-feedback --counts "$fbjson" \
    > "$out/feedback.log" 2>&1 &
fb_pid=$!
until_true '[ "$(field e2e-feedback .layout.window_size[0])" -gt 0 ] 2>/dev/null || ! kill -0 $fb_pid 2>/dev/null'
fb_rc=0
kill -0 $fb_pid 2>/dev/null || wait $fb_pid || fb_rc=$?
if [ "$fb_rc" = 77 ]; then
    echo "skip - dmabuf feedback: river offers no linux-dmabuf v4 (pixman)"
else
    [ "$fb_rc" = 0 ] || fail "feedback_probe exited $fb_rc: $(cat "$out/feedback.log")"
    sleep 1
    mapped=$(fb done)
    [ "$mapped" -ge 1 ] || fail "no dmabuf feedback after the map: $(cat "$fbjson")"
    b=$(fbnow); sleep 5; fbstep "at rest" "$b"
    b=$(fbnow)
    for _ in 1 2 3 4 5; do msg action zoom-out; sleep 0.5; msg action zoom-in; sleep 0.5; done
    fbstep "while the camera zooms" "$b"
    b=$(fbnow)
    msg action toggle-overview; sleep 2.5; msg action toggle-overview; sleep 2.5
    fbstep "around the overview" "$b"
    ok "dmabuf feedback: $mapped at the map, then none per commit or frame ($(cat "$fbjson"))"
    kill $fb_pid 2>/dev/null || true
    wait $fb_pid 2>/dev/null || true
    until_true '[ -z "$(win e2e-feedback)" ]'
fi

windows > "$out/windows.json"
grim "$out/e2e.png" || fail "grim"
ok "screenshot $out/e2e.png"

# Config hot reload (niri's watcher): a change applies live, a parse error shows the red bar and
# keeps the config running.
rgb() { grim -g "$1 1x1" -t ppm - | tail -c 3 | od -An -tu1 | xargs; }
reddish() { set -- $(rgb "$1"); [ "$1" -gt 200 ] && [ "$2" -lt 120 ] && [ "$3" -lt 120 ]; }
cp "$out/config.kdl" "$out/config.good"
w0=$(field e2e-1 .layout.tile_size[0])
sed -i 's/gaps 10/gaps 20/' "$out/config.kdl"
until_true '[ "$(field e2e-1 .layout.tile_size[0])" = $((w0 - 10)) ]'
ok "an edited config applies live: gaps 20 take 10 px more of e2e-1's tiles ($w0 -> $((w0 - 10)))"
echo 'layout {' > "$out/config.kdl"
until_true 'grep -q "config /.*config.kdl: " "$out/river.log"'
settle
reddish "$((ow / 2)),28" || fail "no error bar: $(rgb "$((ow / 2)),28")"
grim "$out/config-error.png" || fail "grim"
[ "$(field e2e-1 .layout.tile_size[0])" = $((w0 - 10)) ] || fail "the broken config changed the layout"
ok "a config that does not parse shows the red bar and keeps the old one ($out/config-error.png)"
cp "$out/config.good" "$out/config.kdl"
until_true '[ "$(field e2e-1 .layout.tile_size[0])" = "$w0" ]'
ok "the fixed config applies again"

# Live settings, as a settings app drives them over IPC: the list, a set that applies live (the
# floating-follow file beside the socket), the event every change sends, a refused value, the
# export (it validates as a config), and the reset back to the config file.
opt() { msg --json newm-options | jq -c ".[] | select(.key == \"$1\")"; }
[ "$(opt layout.gaps | jq -r .kind)" = float ] || fail "layout.gaps is not listed: $(opt layout.gaps)"
[ -z "$(opt layout.canvas.magnified-input)" ] || fail "newm-river lists magnified-input"
"$nrv" msg event-stream > "$out/live-events.jsonl" 2>/dev/null &
events_pid=$!
until_true 'grep -q NewmOptionsChanged "$out/live-events.jsonl"'
follow_file="$NIRI_SOCKET.floating-follow"
follow0=$(cat "$follow_file")
follow1=parent
[ "$follow0" = parent ] && follow1=canvas
msg newm-set-option layout.canvas.floating-follow "\"$follow1\""
[ "$(opt layout.canvas.floating-follow | jq -r .overridden)" = true ] || fail "the set is not an override"
until_true '[ "$(cat "$follow_file")" = "$follow1" ]'
until_true '[ "$(grep -c NewmOptionsChanged "$out/live-events.jsonl")" -ge 2 ]'
if refused=$(msg newm-set-option layout.gaps 500 2>&1); then fail "gaps 500 was taken"; fi
echo "$refused" | grep -q "value 500 is above the maximum 200" || fail "gaps 500: $refused"
msg --json newm-export | jq -r .kdl > "$out/live.kdl"
grep -q "floating-follow \"$follow1\"" "$out/live.kdl" || fail "the export lacks the override: $(cat "$out/live.kdl")"
"$nrv" --config "$out/live.kdl" validate >/dev/null || fail "the export does not validate: $(cat "$out/live.kdl")"
msg newm-reset-all
[ "$(msg --json newm-options | jq '[.[] | select(.overridden)] | length')" = 0 ] || fail "an override is left"
until_true '[ "$(cat "$follow_file")" = "$follow0" ]'
kill $events_pid 2>/dev/null || true
ok "live settings: list, set applied live ($follow0 -> $follow1), event, refused value, export validates, reset"

# A block over IPC: two windows launched and placed as one unit on the tiles asked, the second 4
# tiles right of the first; the status names both once they mapped, and the window of slot 1
# takes the focus. Parking moves every window just left of the view, the camera showing the
# nearest at its left edge; one undo brings them back.
block=$(jq -cn '{windows: [
    {command: ["foot", "--app-id", "e2e-b1", "sh", "-c", "sleep 600"], cwd: "/", env: {},
     app_id: "e2e-b1", rect: [0, 0, 4, 2]},
    {command: ["foot", "--app-id", "e2e-b2", "sh", "-c", "sleep 600"], cwd: null,
     env: {E2E_BLOCK: "1"}, app_id: "e2e-b2", rect: [4, 0, 4, 2]}],
  anchor: {Tiles: [60, 0]}, focus: 1, fly: true}')
placed=$(msg --json newm-place-block "$block") || fail "newm-place-block: $placed"
bid=$(jq -r .id <<<"$placed")
[ "$(jq -c .origin <<<"$placed")" = "[60,0]" ] || fail "the block's origin: $placed"
until_true '[ "$(msg --json newm-block-status "$bid" | jq .done)" = true ]'
status=$(msg --json newm-block-status "$bid")
[ "$(jq -c .windows <<<"$status")" = "[$(field e2e-b1 .id),$(field e2e-b2 .id)]" ] \
    || fail "the block's status: $status"
settle
xb1=$(field e2e-b1 .layout.tile_pos_in_workspace_view[0])
xb2=$(field e2e-b2 .layout.tile_pos_in_workspace_view[0])
yb1=$(field e2e-b1 .layout.tile_pos_in_workspace_view[1])
yb2=$(field e2e-b2 .layout.tile_pos_in_workspace_view[1])
[ $((xb2 - xb1)) = 476 ] && [ "$yb1" = "$yb2" ] || fail "the block is not one unit: $xb1,$yb1 $xb2,$yb2"
[ "$xb1" -ge 0 ] && [ $((xb2 + $(field e2e-b2 .layout.tile_size[0]))) -le "$ow" ] \
    || fail "the camera does not show the block: $xb1 $xb2"
[ "$(field e2e-b2 .is_focused)" = true ] || fail "slot 1's window is not focused"
msg newm-park-windows
settle
pb1=$(field e2e-b1 .layout.tile_pos_in_workspace_view[0])
pb2=$(field e2e-b2 .layout.tile_pos_in_workspace_view[0])
[ $((pb2 - pb1)) = 476 ] || fail "the park broke the block: $pb1 $pb2"
[ $((pb1 + $(field e2e-b1 .layout.tile_size[0]))) -le 0 ] || fail "the park left e2e-b1 in view: $pb1"
[ "$pb2" -ge 0 ] && [ "$pb2" -le 20 ] || fail "the camera does not show e2e-b2 at its left edge: $pb2"
msg action undo
until_true '[ "$(field e2e-b1 .layout.tile_pos_in_workspace_view[0])" = "$xb1" ]'
ok "a block of two windows lands as one unit, reports both, focuses slot 1; the park undoes in one step"

# The exit confirmation: quit without skip-confirmation asks first, a framed
# box with no text; Escape cancels, Enter quits.
frame="$((ow / 2 - 156)),$((oh / 2 - 52))"
msg action quit
settle
kill -0 $river_pid 2>/dev/null || fail "quit without skip-confirmation quit at once"
reddish "$frame" || fail "no red frame at $frame: $(rgb "$frame")"
grim "$out/exit-confirm.png" || fail "grim"
vin key 1 down sleep 30 key 1 up
settle
reddish "$frame" && fail "Escape left the dialog up"
kill -0 $river_pid 2>/dev/null || fail "Escape quit"
ok "quit asks first with a framed box, Escape cancels ($out/exit-confirm.png)"

msg action quit
settle
"$root/newm/target/release/examples/vinput" key 28 down sleep 30 key 28 up >/dev/null 2>&1 || true
for _ in $(seq 100); do kill -0 $river_pid 2>/dev/null || break; sleep 0.1; done
if kill -0 $river_pid 2>/dev/null; then fail "river still runs after Enter confirmed the quit"; fi
ok "Enter in the exit confirmation ends the river session"
echo "e2e: $pass passed"
