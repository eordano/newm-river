#!/usr/bin/env bash
# The session contract the packaged newm-river-init relies on: with NIRI_SOCKET exported before river
# starts, newm-river serves its IPC at exactly that path, and `msg action quit` ends river (through
# river_window_manager_v1.exit_session), which is how the bar's logout ends the session.
#
# Run from the repository root inside its dev shell, after `zig build` and `cargo build --release`:
#   newm/e2e/session-socket.sh
# RIVER, NEWM_RIVER and OUT override the river binary, the newm-river binary and the output dir.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
out=${OUT:-/tmp/newm-river-session-socket}
mkdir -p "$out"

XDG_RUNTIME_DIR=$(mktemp -d)
export XDG_RUNTIME_DIR
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
unset WAYLAND_DISPLAY DISPLAY
export NIRI_SOCKET=$XDG_RUNTIME_DIR/newm-river.session.sock
printf 'layout {\n    gaps 10\n}\n' > "$out/config.kdl"
export NIRI_CONFIG=$out/config.kdl

fail() { echo "FAIL: $*" >&2; tail -n 30 "$out/river.log" >&2; exit 1; }

"$river" -c "$nrv" > "$out/river.log" 2>&1 &
river_pid=$!
trap 'kill $river_pid 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT

for _ in $(seq 100); do [ -S "$NIRI_SOCKET" ] && break; sleep 0.1; done
[ -S "$NIRI_SOCKET" ] || fail "no socket at the exported NIRI_SOCKET"
echo "ok 1 - the socket is at the exported NIRI_SOCKET"

"$nrv" msg version > "$out/version.txt" || fail "msg version through NIRI_SOCKET"
echo "ok 2 - msg version answers: $(head -n 1 "$out/version.txt")"

"$nrv" msg action quit --skip-confirmation || fail "msg action quit"
for _ in $(seq 100); do kill -0 "$river_pid" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$river_pid" 2>/dev/null; then fail "river still runs 10 s after quit"; fi
echo "ok 3 - quit over IPC ended river"
