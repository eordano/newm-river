#!/usr/bin/env bash
# keyboard-shortcuts-inhibit on a headless river (river/NewmShortcuts.zig): a client that
# inhibits shortcuts for its surface (examples/inhibit_probe.rs, as a VM viewer does) gets the keys
# of the window manager's bindings; a bind with allow-inhibiting=false still fires; Super+Escape
# (toggle-keyboard-shortcuts-inhibit, never inhibitable) turns the inhibitor off and on again.
# Keys come from a virtual keyboard (examples/vinput.rs); binds touch files, so the test sees them.
#
#   nix shell nixpkgs#jq -c newm/e2e/inhibit.sh
#
# RIVER, NEWM_RIVER and OUT override the river binary, the newm-river binary and the output dir.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
out=${OUT:-/tmp/newm-river-e2e-inhibit}
mkdir -p "$out"
rm -f "$out"/fired-* "$out/probe.log"

(cd "$root/newm" && cargo build -q --release --example vinput --example inhibit_probe) \
    || { echo "e2e: cannot build the examples" >&2; exit 2; }
vinput=$root/newm/target/release/examples/vinput
probe_bin=$root/newm/target/release/examples/inhibit_probe

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
export XDG_STATE_HOME=$out/state
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET DBUS_SESSION_BUS_ADDRESS

cat > "$out/config.kdl" <<KDL
prefer-no-csd
binds {
    Mod+T { spawn-sh "touch $out/fired-t"; }
    Mod+Y allow-inhibiting=false { spawn-sh "touch $out/fired-y"; }
    Mod+Escape { toggle-keyboard-shortcuts-inhibit; }
}
KDL
export NIRI_CONFIG=$out/config.kdl
"$nrv" validate

pass=0
fail() {
    echo "FAIL: $*" >&2
    echo "probe:" >&2
    cat "$out/probe.log" >&2 || true
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

"$river" -log-level debug -c "RUST_LOG=newm_river=debug $nrv" > "$out/river.log" 2>&1 &
river_pid=$!
probe_pid=
trap 'kill $probe_pid $river_pid 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT

until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
export NIRI_SOCKET WAYLAND_DISPLAY
until_true '[ "$("$nrv" msg --json outputs | jq length)" -ge 1 ]'
ok "river runs newm-river"

"$probe_bin" > "$out/probe.log" 2>&1 &
probe_pid=$!
until_true 'grep -q "^inhibitor active$" "$out/probe.log"'
ok "river activates the probe's inhibitor"

# A combo: Super down, the key down and up, Super up. The headless river has a keyboard only
# while vinput's virtual keyboard exists, so vinput first waits for the probe to take it.
combo() {
    "$vinput" sleep 400 super down key "$1" down key "$1" up super up sleep 300 || fail "vinput $1"
}
count() { grep -c "^$1$" "$out/probe.log" || true; }

combo 20
until_true 'grep -q "^focus in$" "$out/probe.log"'
until_true '[ "$(count "key 20 down")" = 1 ]'
[ ! -e "$out/fired-t" ] || fail "Super+T fired its bind while the probe inhibited shortcuts"
ok "inhibited: Super+T reaches the probe, its bind does not fire"

combo 21
until_true '[ -e "$out/fired-y" ]'
[ "$(count "key 21 down")" = 0 ] || fail "the probe got Super+Y, an allow-inhibiting=false bind"
ok "inhibited: an allow-inhibiting=false bind (Super+Y) still fires and the probe gets no key"

combo 1
until_true 'grep -q "^inhibitor inactive$" "$out/probe.log"'
[ "$(count "key 1 down")" = 0 ] || fail "the probe got Super+Escape, the toggle bind"
combo 20
until_true '[ -e "$out/fired-t" ]'
[ "$(count "key 20 down")" = 1 ] || fail "the probe got Super+T after the toggle"
ok "Super+Escape turns the inhibitor off: Super+T fires its bind, the probe gets nothing"

combo 1
until_true '[ "$(count "inhibitor active")" = 2 ]'
rm -f "$out/fired-t"
combo 20
until_true '[ "$(count "key 20 down")" = 2 ]'
[ ! -e "$out/fired-t" ] || fail "Super+T fired after the inhibitor came back"
ok "Super+Escape again: the inhibitor is back and Super+T reaches the probe"

echo "inhibit e2e: $pass ok"
