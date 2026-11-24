#!/usr/bin/env bash
# newm-river end to end, beyond headless.sh: launch placeholders, window rules (short-lived
# windows, dialogs, Picture-in-Picture, the gauge HUD), a layer-shell exclusive zone,
# app-requested fullscreen, two outputs (a click on an empty one, monitor actions, the config's
# output blocks read back with wlr-randr), and session save and restore (a bare Chromium told to
# restore its last session; windows of one app back on their own saved spots once their titles
# arrive, on one output and across two). Each part runs its own headless river (pixman) with
# newm-river and a test config; windows come from the e2e_client example (solid colours),
# screenshots from grim.
#
# Run from the repository root inside its dev shell, after `zig build` and
# `cargo build --release -p newm-river --examples`, with jq on PATH:
#   nix shell nixpkgs#jq -c newm/e2e/session.sh
# RIVER, NEWM_RIVER, E2E_CLIENT and OUT override the binaries and the output dir.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
# The usage log checks need newm-river built with the `telemetry` feature: unless NEWM_RIVER names
# a binary, this builds one first.
[ -n "${NEWM_RIVER:-}" ] \
    || (cd "$root/newm" && cargo build -q --release -p newm-river --bins --features telemetry)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
client=${E2E_CLIENT:-$root/newm/target/release/examples/e2e_client}
vinput=${VINPUT:-$root/newm/target/release/examples/vinput}
out=${OUT:-/tmp/newm-river-e2e-session}
rm -rf "$out"
mkdir -p "$out"
for tool in jq grim wlr-randr; do
    command -v "$tool" >/dev/null || { echo "e2e: $tool is not on PATH" >&2; exit 2; }
done

export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET NEWM_RIVER_SESSION

pass=0
river_pid=
log=
fail() {
    echo "FAIL: $*" >&2
    [ -n "$log" ] && tail -n 40 "$log" >&2
    echo "windows:" >&2
    "$nrv" msg --json windows 2>/dev/null | jq -c '.[] | {app_id, is_focused, tiles: .layout.tiles}' >&2 || true
    exit 1
}
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }
cleanup() {
    [ -n "$river_pid" ] && kill "$river_pid" 2>/dev/null || true
    pkill -f "$client" 2>/dev/null || true
}
trap cleanup EXIT

# Waits up to 10 s (or $2 tenths of a second) for a shell condition.
until_true() {
    local i
    for i in $(seq "${2:-100}"); do
        if eval "$1"; then return 0; fi
        sleep 0.1
    done
    fail "timed out waiting for: $1"
}

config() { # extra KDL appended to the base config
    cat > "$out/config.kdl" <<KDL
prefer-no-csd
layout {
    gaps 10
    border { width 1; active-color "#82FB9C8C"; inactive-color "transparent"; }
    shadow { on; softness 26; spread 0; color "#82FB9C36"; }
    canvas {
        tile-width 119
        tile-height 113
        default-width { fixed 960; }
        default-height { proportion 0.666700; }
        $1
    }
}
window-rule {
    match app-id="(?i)ksshaskpass"
    match app-id="(?i)krunner"
    match app-id="(?i)gauge"
    open-floating true
}
window-rule {
    match app-id="(?i)ksshaskpass"
    open-focused false
}
window-rule {
    match app-id="^o-2$"
    default-column-width { fixed 228; }
}
window-rule {
    match title="^Picture.in.[Pp]icture$"
    open-floating true
    open-focused false
    default-floating-position x=10 y=10 relative-to="bottom-right"
}
window-rule {
    match app-id="(?i)gauge" title="^Gauge HUD"
    open-focused false
    draw-border-with-background false
    border { off; }
    default-floating-position x=36 y=82 relative-to="bottom-right"
}
KDL
    NIRI_CONFIG=$out/config.kdl "$nrv" validate >/dev/null
}

start() { # name outputs
    log=$out/$1.log
    XDG_RUNTIME_DIR=$(mktemp -d -p "$out")
    export XDG_RUNTIME_DIR NIRI_CONFIG=$out/config.kdl WLR_HEADLESS_OUTPUTS=$2
    # A NIRI_SOCKET river inherits names the socket newm-river binds: the last run's is gone.
    unset NIRI_SOCKET WAYLAND_DISPLAY
    "$river" -c "RUST_LOG=newm_river=debug $nrv" > "$log" 2>&1 &
    river_pid=$!
    until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
    NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
    WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
    export NIRI_SOCKET WAYLAND_DISPLAY
    until_true "[ \"\$(msg --json outputs | jq length)\" -ge $2 ]"
    read -r ow oh < <(msg --json outputs | jq -r '[.[]][0].logical | "\(.width) \(.height)"')
}

stop() {
    # The window manager may exit before its reply is written; river ending is the check.
    msg action quit --skip-confirmation >/dev/null 2>&1 || true
    for _ in $(seq 100); do kill -0 "$river_pid" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$river_pid" 2>/dev/null; then fail "river still runs after quit"; fi
    # A quit ends the window manager cleanly: the supervisor restarts none (supervise.rs).
    if grep -q 'window manager crashed (exit status' "$log"; then
        fail "the window manager failed at the quit"
    fi
    river_pid=
}

msg() { "$nrv" msg "$@"; }
windows() { msg --json windows; }
win() { windows | jq -c ".[] | select(.app_id == \"$1\")"; }
# One field of a window; numbers as integers (jq 1.7 prints 942.0 as it came).
field() { win "$1" | jq -r "$2 | if type == \"number\" then floor else . end"; }
x() { field "$1" .layout.tile_pos_in_workspace_view[0]; }
y() { field "$1" .layout.tile_pos_in_workspace_view[1]; }
mapped() { until_true "[ \"\$(field $1 .layout.window_size[0])\" -gt 0 ] 2>/dev/null"; }
open() { # app-id [client args...]
    local app=$1
    shift
    "$client" window --app-id "$app" "$@" >> "$out/clients.log" 2>&1 &
}
close_all() {
    while [ "$(windows | jq length)" -gt 0 ]; do
        msg action close-window --id "$(windows | jq '.[0].id')" >/dev/null
        sleep 0.2
    done
}
pixel() { "$client" pixel "$@"; }
vin() { "$vinput" "$@" >> "$out/clients.log" 2>&1 || fail "vinput $*"; }
# The output a window's canvas is on (its workspace is its output's).
output_of() { msg --json workspaces | jq -r ".[] | select(.id == $(field "$1" .workspace_id)) | .output"; }
randr() { wlr-randr --json | jq -r ".[] | select(.name == \"$1\") | $2"; }
near() { [ $(($1 - $2)) -le 2 ] && [ $(($2 - $1)) -le 2 ]; }

# ---- one output: placeholders, rules, layer shell, fullscreen -------------------------------
config ""
start clients 1
ok "river runs newm-river; output ${ow}x${oh}"

msg action launch-placeholder --app-id never >/dev/null
sleep "${PLACEHOLDER_WAIT:-10.5}"
open t-1
mapped t-1
[ "$(x t-1)" = 10 ] || fail "a placeholder still holds the origin after 10 s: $(win t-1)"
ok "a placeholder no window took frees its tiles after 10 s"
close_all

"$client" png "$out/icon.png" 64x64 0000ff
msg action launch-placeholder --app-id ph-app --icon "$out/icon.png" >/dev/null
sleep 0.5
grim -t ppm "$out/placeholder.ppm" || fail "grim"
# The empty canvas's first spot: tiles (0, 0, 8, 4), the box at (10, 10) 942x442.
[ "$(pixel "$out/placeholder.ppm" 481 231)" = "0 0 255" ] \
    || fail "no icon at the placeholder's centre: $(pixel "$out/placeholder.ppm" 481 231)"
box=$(pixel "$out/placeholder.ppm" 30 231)
[ "$box" != "0 0 0" ] && [ "$box" != "0 0 255" ] || fail "no box around the icon: $box"
[ "$(pixel "$out/placeholder.ppm" 481 600)" = "0 0 0" ] || fail "the box spills below its tiles"
ok "launch-placeholder draws the app's icon centred on a box on the tiles it holds"

open ph-other --color 00ff00
mapped ph-other
# pos_in_scrolling_layout counts tiles from the top-left one held, from 1: the placeholder holds
# columns 1-8 and rows 1-4, and on this 1280x720 output the view scrolls least below it (the
# placement rules: "the placeholder holds (1, 0), so 1 opens below 0").
[ "$(field ph-other .layout.pos_in_scrolling_layout[0])" = 1 ] \
    && [ "$(field ph-other .layout.pos_in_scrolling_layout[1])" = 5 ] || fail "ph-other: $(win ph-other)"
open ph-app
mapped ph-app
[ "$(field ph-app .layout.pos_in_scrolling_layout[0])" = 1 ] \
    || fail "ph-app did not take the placeholder's tiles: $(win ph-app)"
[ "$(x ph-app)" = 10 ] && [ "$(field ph-app .is_focused)" = true ] || fail "ph-app: $(win ph-app)"
ok "another app opens below the placeholder; the app's window takes its tiles and the focus"
close_all

"$client" layer --anchor top --thickness 40 >> "$out/clients.log" 2>&1 &
layer_pid=$!
open c-1
mapped c-1
until_true '[ "$(y c-1)" = 50 ]'
kill $layer_pid
until_true '[ "$(y c-1)" = 10 ]'
ok "a layer surface's exclusive zone shrinks the canvas (the window moves 40 px down and back)"

open dlg --size 600x400 --dialog dlg-child --dialog-size 300x200
mapped dlg-child
[ "$(field dlg-child .is_floating)" = true ] && [ "$(field dlg-child .is_focused)" = true ] \
    || fail "dialog: $(win dlg-child)"
pc=$(( $(x dlg) + $(field dlg .layout.tile_size[0]) / 2 ))
dc=$(( $(x dlg-child) + $(field dlg-child .layout.tile_size[0]) / 2 ))
near "$pc" "$dc" || fail "dialog not centred on its parent: $pc $dc"
[ "$(field dlg-child .layout.window_size[0])" = 300 ] || fail "dialog resized: $(win dlg-child)"
ok "a dialog floats focused at its own size, centred on its parent"

open pip --size 320x180 --min 160x90 --max 640x360
mapped pip
[ "$(field pip .is_floating)" = true ] && [ "$(field pip .is_focused)" = false ] || fail "pip: $(win pip)"
near $(( $(x pip) + $(field pip .layout.tile_size[0]) )) $((ow - 10)) || fail "pip not right: $(win pip)"
near $(( $(y pip) + $(field pip .layout.tile_size[1]) )) $((oh - 10)) || fail "pip not low: $(win pip)"
ok "a window capping its size on both axes (Picture-in-Picture) floats bottom-right unfocused"

open firefox --title Picture-in-Picture --size 320x180
mapped firefox
[ "$(field firefox .is_floating)" = true ] && [ "$(field firefox .is_focused)" = false ] \
    || fail "titled pip: $(win firefox)"
ok "Firefox's Picture-in-Picture floats unfocused through its title rule"

open org.kde.krunner --size 500x60
mapped org.kde.krunner
[ "$(field org.kde.krunner .is_floating)" = true ] && [ "$(field org.kde.krunner .is_focused)" = true ] \
    || fail "krunner: $(win org.kde.krunner)"
open ksshaskpass --size 400x150
mapped ksshaskpass
[ "$(field ksshaskpass .is_floating)" = true ] && [ "$(field ksshaskpass .is_focused)" = false ] \
    || fail "ksshaskpass: $(win ksshaskpass)"
[ "$(field org.kde.krunner .is_focused)" = true ] || fail "ksshaskpass took the focus"
ok "short-lived windows float by the config's open-floating rules; ksshaskpass opens unfocused"

open gauge --title "Gauge HUD" --size 300x120 --color 202020
mapped gauge
[ "$(field gauge .is_floating)" = true ] && [ "$(field gauge .is_focused)" = false ] || fail "hud: $(win gauge)"
[ "$(field gauge .layout.tile_size[0])" = 300 ] || fail "the HUD has a border: $(win gauge)"
near $(( $(x gauge) + 300 )) $((ow - 36)) && near $(( $(y gauge) + 120 )) $((oh - 82)) \
    || fail "the HUD is not parked bottom-right: $(win gauge)"
ok "the gauge HUD floats unfocused, borderless, parked bottom-right (36, 82)"
close_all

open fs --fullscreen
mapped fs
until_true "[ \"\$(field fs .layout.window_size[0])\" = $ow ] && [ \"\$(field fs .layout.window_size[1])\" = $oh ]"
ok "an app asking for fullscreen gets the whole output"
close_all
stop
ok "quit"

# ---- two outputs: a click on the empty one activates it; monitor actions --------------------
start monitors 2
x2=$(msg --json outputs | jq '[.[].logical.x] | max')
open o-1
mapped o-1
[ "$(msg --json focused-output | jq .logical.x)" = 0 ] || fail "new windows open on the first output"
second=$(msg --json outputs | jq -r '[.[]] | max_by(.logical.x) | .name')
first=$(msg --json outputs | jq -r '[.[]] | min_by(.logical.x) | .name')
lw=$(msg --json outputs | jq '[.[].logical | .x + .width] | max')
lh=$(msg --json outputs | jq '[.[].logical | .y + .height] | max')
vin abs $((x2 + 100)) 100 "$lw" "$lh" button 272 down button 272 up
until_true "[ \"\$(msg --json focused-output | jq -r .name)\" = $second ]"
[ "$(msg --json focused-window)" = null ] || fail "the empty output kept a focused window"
open o-3 --color 00ff00
mapped o-3
[ "$(output_of o-3)" = "$second" ] || fail "o-3 did not open on the clicked output: $(win o-3)"
ok "a click on an empty output makes it the active one: the next window opens there"
msg action focus-monitor-left >/dev/null
until_true '[ "$(field o-1 .is_focused)" = true ]'
size=$(field o-1 '.layout.tile_size | map(floor)')
msg action move-window-to-monitor-right >/dev/null
until_true "[ \"\$(output_of o-1)\" = $second ]"
[ "$(field o-1 .is_focused)" = true ] || fail "the focus did not follow o-1"
[ "$(field o-1 '.layout.tile_size | map(floor)')" = "$size" ] || fail "o-1 left its span: $(win o-1)"
msg action move-column-to-monitor-left >/dev/null
until_true "[ \"\$(output_of o-1)\" = $first ]"
msg action focus-monitor-next >/dev/null
until_true '[ "$(field o-3 .is_focused)" = true ]'
ok "focus-monitor and move-window/column-to-monitor between the canvases, at the window's span"
stop

# ---- output blocks: applied over wlr-output-management, read back with wlr-randr -------------
config ''
cat >> "$out/config.kdl" <<'KDL'
output "HEADLESS-2" {
    scale 2
    transform "90"
    position x=0 y=0
}
KDL
start output-blocks 2
until_true '[ "$(randr HEADLESS-2 ".scale == 2")" = true ]'
[ "$(randr HEADLESS-2 .transform)" = 90 ] || fail "HEADLESS-2 not rotated: $(wlr-randr --json)"
read -r mw mh < <(randr HEADLESS-2 '.modes[] | select(.current) | "\(.width) \(.height)"')
[ "$(randr HEADLESS-2 .position.x)" = 0 ] || fail "HEADLESS-2 not at x 0"
[ "$(randr HEADLESS-1 .position.x)" = $((mh / 2)) ] || fail "HEADLESS-1 not right of it: $(wlr-randr --json)"
# jq 1.7 prints a number as it came (2.0): arithmetic gives a canonical one.
logical=$(msg --json outputs | jq -c '."HEADLESS-2".logical | [.x, .width, .height, (.scale * 100 | round), .transform]')
[ "$logical" = "[0,$((mh / 2)),$((mw / 2)),200,\"90\"]" ] || fail "IPC reports HEADLESS-2 as $logical"
msg action focus-monitor HEADLESS-2 >/dev/null
open o-4
mapped o-4
[ "$(output_of o-4)" = HEADLESS-2 ] || fail "o-4 not on HEADLESS-2: $(win o-4)"
[ "$(field o-4 .layout.window_size[0])" -le $((mh / 2)) ] || fail "o-4 wider than the scaled output"
ok "the config's output block sets scale, transform and position; the rest sit to its right"
stop
config ''

# ---- two outputs: each window is clipped to its output --------------------------------------
start outputs 2
[ "$(msg --json outputs | jq length)" = 2 ] || fail "two outputs"
x2=$(msg --json outputs | jq '[.[].logical.x] | max')
[ "$x2" -ge "$ow" ] || fail "the second output is not right of the first: $x2"
open o-1
mapped o-1
[ "$(msg --json focused-output | jq .logical.x)" = 0 ] || fail "new windows open on the right output"
# o-2 opens 2 tiles wide, which fits beside o-1 (placement opens a window where the view
# scrolls least, so an 8-tile one would open below), then grows to 8 tiles past the output's edge.
open o-2
mapped o-2
msg action resize-edge right 6 >/dev/null
until_true '[ "$(field o-2 .layout.tile_size[0])" = 942 ]'
msg action focus-column-left >/dev/null
until_true '[ "$(x o-1)" = 10 ]'
[ "$(x o-2)" = 962 ] || fail "o-2 not beside o-1: $(win o-2)"
sleep 0.3
grim -t ppm "$out/outputs.ppm" || fail "grim"
[ "$(pixel "$out/outputs.ppm" $((ow - 10)) 100)" = "255 0 0" ] || fail "o-2 not drawn up to the edge"
[ "$(pixel "$out/outputs.ppm" $((x2 + 20)) 100)" = "0 0 0" ] || fail "o-2 spills onto the second output"
ok "a window past its output's edge is drawn up to it and clipped there, not on the next output"
stop


# ---- session save and restore ----------------------------------------------------------------
export XDG_STATE_HOME=$out/state NEWM_RIVER_SESSION=tty
session=$XDG_STATE_HOME/newm-river/session.json
config 'restore-exclude app-id="^r-skip$"'
printf 'spawn-sh-at-startup "echo started >> %s/startup.log"\n' "$out" >> "$out/config.kdl"
start restore-1 1
open r-1
mapped r-1
open r-2 --color 00ff00
mapped r-2
open r-skip --color 0000ff
mapped r-skip
# Swap r-1 with r-2 so the saved layout is not the one new windows open in. Where r-2 opened
# follows the placement rules (beside r-1 where the view scrolls least: below it on 1280 px).
msg action focus-window --id "$(field r-1 .id)" >/dev/null
until_true '[ "$(field r-1 .is_focused)" = true ]'
if [ "$(x r-2)" -gt "$(x r-1)" ]; then
    msg action swap-neighbour right >/dev/null
    until_true '[ "$(x r-1)" -gt "$(x r-2)" ]'
else
    msg action swap-neighbour down >/dev/null
    until_true '[ "$(y r-1)" -gt "$(y r-2)" ]'
fi
until_true "[ -f $session ] && jq -e '[.canvases[].windows[].app_id] | sort == [\"r-1\", \"r-2\"]' $session >/dev/null" 100
[ "$(stat -c %a "$session")" = 600 ] || fail "the session file is not private"
before=$(windows | jq -c '[.[] | select(.app_id != "r-skip") | [.app_id, .layout.tile_pos_in_workspace_view, .layout.tile_size, .is_focused]] | sort')
ok "the session is saved (0600) once it held, without restore-exclude apps"

# ---- the window manager is restarted after a crash (supervise.rs) -----------------------------
# A SIGKILL to the window manager (newm-river's child): river keeps the session and newm-river
# starts it again, which puts the windows back on the spots the session file saved, launches
# nothing and runs no startup program twice.
nrv_pids() { # newm-river's processes under this river, the supervisor first
    local p kids=$river_pid all=
    while [ -n "$kids" ]; do
        all="$all $kids"
        kids=$(for p in $kids; do pgrep -P "$p" || true; done)
    done
    for p in $all; do
        [ "$(tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null)" = "$nrv " ] && echo "$p"
    done
    return 0
}
# Canvas tiles, not view-relative spots: the unsaved r-skip opens again as a new window, unfocused,
# and the camera may scroll to show it.
spots() { windows | jq -c '[.[] | select(.app_id != "r-skip") | [.app_id, .layout.pos_in_scrolling_layout, .layout.tile_size, .is_focused]] | sort'; }
before_spots=$(spots)
mapfile -t pids < <(nrv_pids)
[ "${#pids[@]}" = 2 ] || fail "expected newm-river's supervisor and window manager, got: ${pids[*]}"
wm=${pids[1]}
t0=$(date +%s.%N)
kill -9 "$wm"
until_true "mapfile -t pids < <(nrv_pids); [ \${#pids[@]} = 2 ] && [ \${pids[1]} != $wm ]"
kill -0 "$river_pid" 2>/dev/null || fail "river ended with its window manager"
until_true 'msg --json windows >/dev/null 2>&1'
until_true '[ "$(spots)" = "$before_spots" ]' 100
[ "$(windows | jq length)" = 3 ] || fail "windows were launched again: $(windows | jq -c '[.[].app_id]')"
[ "$(wc -l < "$out/startup.log")" = 1 ] || fail "the startup programs ran again"
grep -q 'taking over the running session' "$log" || fail "the restarted window manager did not take over"
ok "a crashed window manager is restarted: the windows take their saved spots, nothing launched again"
# The restarted window manager saves the session once it holds (its first write); the camera may
# have moved to show r-skip, so the next part compares with what it saved.
until_true "awk -v m=\"\$(stat -c %.9Y $session)\" -v t=$t0 'BEGIN { exit !(m > t) }'" 100
before=$(windows | jq -c '[.[] | select(.app_id != "r-skip") | [.app_id, .layout.tile_pos_in_workspace_view, .layout.tile_size, .is_focused]] | sort')
stop

start restore-2 1
until_true '[ -n "$(win r-1)" ] && [ -n "$(win r-2)" ]' 150
until_true "[ \"\$(windows | jq -c '[.[] | [.app_id, .layout.tile_pos_in_workspace_view, .layout.tile_size, .is_focused]] | sort')\" = '$before' ]" 250
[ -z "$(win r-skip)" ] || fail "an excluded app came back"
ok "at start the saved windows are relaunched onto their saved tiles with the saved focus"
stop

# ---- a restored Chromium-family browser reopens its last session ----------------------------
# The saved session (written without the flag, as every save is) has r-1 launched by a bare
# `chromium` (flags only) and r-2 by a `chromium` that opens a URL. The stand-in `chromium`
# records its arguments and maps the window: the bare one is relaunched with
# --restore-last-session added, the one opening a URL as saved, both onto their saved tiles.
mkdir -p "$out/bin"
cat > "$out/bin/chromium" <<CHROMIUM
#!/bin/sh
printf '%s\\n' "\$*" >> "$out/chromium.args"
app=r-1
for a; do case \$a in http*) app=r-2 ;; esac; done
color=ff0000
[ \$app = r-2 ] && color=00ff00
exec "$client" window --app-id \$app --color \$color
CHROMIUM
chmod +x "$out/bin/chromium"
jq --arg c "$out/bin/chromium" '
    def launch_of(app): [.canvases[].windows[] | select(.app_id == app) | .launch][0];
    .launches[launch_of("r-1")].command = [$c, "--ozone-platform=wayland"]
    | .launches[launch_of("r-2")].command = [$c, "https://example.org"]' "$session" > "$session.new"
mv "$session.new" "$session"
chmod 600 "$session"
grep -q -- --restore-last-session "$session" && fail "the saved session already has the flag"
start restore-chromium 1
until_true '[ -n "$(win r-1)" ] && [ -n "$(win r-2)" ]' 150
until_true "[ \"\$(windows | jq -c '[.[] | [.app_id, .layout.tile_pos_in_workspace_view, .layout.tile_size, .is_focused]] | sort')\" = '$before' ]" 250
[ "$(LC_ALL=C sort "$out/chromium.args")" = "$(printf '%s\n' '--ozone-platform=wayland --restore-last-session' 'https://example.org')" ] \
    || fail "chromium relaunched with: $(cat "$out/chromium.args")"
ok "a bare Chromium is relaunched with --restore-last-session, one opening a URL as saved"
stop

# ---- session restore by title ---------------------------------------------------------------
# Two windows of one app map titled "term" and take their real titles 300 ms later, as terminals
# do once tmux attaches. The window saved first is relaunched 1.5 s late, so the other maps
# first and takes the first saved spot: it must trade spots once its title arrives.
export XDG_STATE_HOME=$out/state-titles
session=$XDG_STATE_HOME/newm-river/session.json
titled() { windows | jq -c '[.[] | select(.app_id == "t-a") | [.title, .layout.tile_pos_in_workspace_view, .layout.tile_size]] | sort'; }
start restore-3 1
open t-a --title term --retitle alpha --color 00ffff
open t-a --title term --retitle beta --color ff00ff
until_true '[ "$(windows | jq -c "[.[] | select(.app_id == \"t-a\") | .title] | sort")" = "[\"alpha\",\"beta\"]" ]'
until_true "[ -f $session ] && jq -e '[.canvases[].windows[].title] | sort == [\"alpha\", \"beta\"]' $session >/dev/null" 100
before=$(titled)
[ "$(echo "$before" | jq '[.[][1]] | unique | length')" = 2 ] || fail "the two windows share a spot: $before"
stop
first=$(jq -r '.canvases[0].windows[0].title' "$session")
jq '.launches[.canvases[0].windows[0].launch].command += ["--map-delay", "1500"]' "$session" > "$session.new"
mv "$session.new" "$session"
chmod 600 "$session"

start restore-4 1
until_true '[ "$(windows | jq "[.[] | select(.app_id == \"t-a\")] | length")" = 2 ]' 150
until_true "[ \"\$(titled)\" = '$before' ]" 250
usage=$XDG_STATE_HOME/newm-river/usage.jsonl
until_true "grep -q '\"op\":\"restore-rematch\"' $usage" 50
ok "windows of one app come back on their own saved spots once their titles arrive ($first mapped last)"
stop

# ---- session restore by title across two outputs ---------------------------------------------
# One window of an app on each output, titled after they map. The one saved on the first saved
# canvas is relaunched 1.5 s late, so the other maps first and takes that canvas's spot by its
# app alone: once its title arrives it must go back to its own output and tiles, and the late
# one takes the spot it left. Compared: each window's output and tiles as saved, never where the
# placement rules would open it.
export XDG_STATE_HOME=$out/state-outputs
session=$XDG_STATE_HOME/newm-river/session.json
# Each t-b window's title, output, tiles in its output's view and size.
spots() {
    local ws
    ws=$(msg --json workspaces | jq -c 'map({key: (.id | tostring), value: .output}) | from_entries')
    windows | jq -c --argjson ws "$ws" '[.[] | select(.app_id == "t-b")
        | [.title, $ws[.workspace_id | tostring], .layout.tile_pos_in_workspace_view, .layout.tile_size]] | sort'
}
start restore-5 2
second=$(msg --json outputs | jq -r '[.[]] | max_by(.logical.x) | .name')
open t-b --title term --retitle alpha --color 00ffff
until_true '[ "$(field t-b .title)" = alpha ]'
msg action focus-monitor-right >/dev/null
until_true "[ \"\$(msg --json focused-output | jq -r .name)\" = $second ]"
open t-b --title term --retitle beta --color ff00ff
until_true '[ "$(windows | jq -c "[.[] | select(.app_id == \"t-b\") | .title] | sort")" = "[\"alpha\",\"beta\"]" ]'
until_true "[ -f $session ] && jq -e '[.canvases[] | select(.windows | length == 1) | .windows[0].title] | sort == [\"alpha\", \"beta\"]' $session >/dev/null" 100
before=$(spots)
[ "$(echo "$before" | jq '[.[][1]] | unique | length')" = 2 ] || fail "not one window per output: $before"
stop
late=$(jq -r '[.canvases[] | select(.windows != [])][0].windows[0].title' "$session")
jq '.launches[[.canvases[] | select(.windows != [])][0].windows[0].launch].command += ["--map-delay", "1500"]' "$session" > "$session.new"
mv "$session.new" "$session"
chmod 600 "$session"

start restore-6 2
until_true '[ "$(windows | jq "[.[] | select(.app_id == \"t-b\")] | length")" = 2 ]' 150
until_true "[ \"\$(spots)\" = '$before' ]" 250
usage=$XDG_STATE_HOME/newm-river/usage.jsonl
until_true "jq -e -s 'any(.[]; .op == \"restore-rematch\" and .output != null and .output != .with_output)' $usage >/dev/null" 50
ok "windows of one app on two outputs come back on their own saved outputs and tiles ($late mapped last)"
stop

echo "e2e session: $pass passed"
