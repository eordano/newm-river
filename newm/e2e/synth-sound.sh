#!/usr/bin/env bash
# The synth background's react-to-sound on a headless river with a private PipeWire and
# WirePlumber (no sound devices of this machine: their monitors are off) and one null sink as the
# default output. The look's own motion is switched off, so every frame drawn is the sound's.
# Checks: the capture opens; in silence the sink suspends and nothing is drawn; a tone wakes the
# sink and draws frames, never more than max-fps; after it the sink suspends again and drawing
# stops; still closes the capture. Prints the window manager's CPU while silent and while a tone
# plays.
#
# Run from the repository root inside its dev shell, after `zig build` and `cargo build
# --release -p newm-river --bins --examples`:
#   nix shell nixpkgs#jq nixpkgs#pipewire nixpkgs#wireplumber nixpkgs#dbus -c newm/e2e/synth-sound.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
river=${RIVER:-$root/zig-out/bin/river}
nrv=${NEWM_RIVER:-$root/newm/target/release/newm-river}
out=${OUT:-/tmp/newm-river-e2e-synth-sound}
mkdir -p "$out"
rm -f "$out"/*.log
for tool in jq pipewire wireplumber pw-dump pw-play dbus-daemon; do
    command -v "$tool" >/dev/null || { echo "e2e: $tool is not on PATH" >&2; exit 2; }
done

XDG_RUNTIME_DIR=$(mktemp -d)
XDG_CONFIG_HOME=$XDG_RUNTIME_DIR/config
XDG_STATE_HOME=$out/state
export XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_STATE_HOME
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
unset WAYLAND_DISPLAY DISPLAY NIRI_SOCKET PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS
bus=$(dbus-daemon --session --fork --print-address=1 --print-pid=1)
DBUS_SESSION_BUS_ADDRESS=$(sed -n 1p <<<"$bus")
dbus_pid=$(sed -n 2p <<<"$bus")
export DBUS_SESSION_BUS_ADDRESS

mkdir -p "$XDG_CONFIG_HOME/wireplumber/wireplumber.conf.d" "$XDG_CONFIG_HOME/pipewire/pipewire.conf.d"
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
cat > "$XDG_CONFIG_HOME/pipewire/pipewire.conf.d/90-sink.conf" <<'CONF'
context.objects = [
  { factory = adapter
    args = { factory.name = support.null-audio-sink node.name = e2e-sink
             node.description = "e2e sink" media.class = Audio/Sink audio.position = [ FL FR ] } }
]
CONF

cat > "$out/config.kdl" <<'KDL'
prefer-no-csd
synth-background {
    mode "animated"
    max-fps 4
    react-to-sound
    stars 0
    grid-speed 0
    ridge-speed 0
    reflection 0
    cycle-speed 0
}
KDL
export NIRI_CONFIG=$out/config.kdl
"$nrv" validate

pass=0
pids=()
fail() {
    echo "FAIL: $*" >&2
    tail -n 30 "$out/river.log" >&2
    exit 1
}
ok() { pass=$((pass + 1)); echo "ok $pass - $*"; }
trap 'kill "${pids[@]}" $dbus_pid 2>/dev/null || true; rm -rf "$XDG_RUNTIME_DIR"' EXIT
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
state() {
    pw-dump 2>/dev/null | jq -r --arg n "$1" \
        '[.[] | select(.type == "PipeWire:Interface:Node" and .info.props["node.name"] == $n) | .info.state][0] // "absent"'
}
until_true '[ "$(state e2e-sink)" != absent ]'
ok "private pipewire and wireplumber run, with a null sink"

"$river" -c "RUST_LOG=newm_river=info,newm_river::river::synth=debug $nrv" \
    > "$out/river.log" 2>&1 &
river_pid=$!
pids+=($river_pid)
until_true 'ls "$XDG_RUNTIME_DIR"/newm-river.*.sock >/dev/null 2>&1'
NIRI_SOCKET=$(ls "$XDG_RUNTIME_DIR"/newm-river.*.sock)
WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -n 1)")
export NIRI_SOCKET WAYLAND_DISPLAY
wm=$(pgrep -n -f "^$nrv" || pgrep -n newm-river)

frames() { grep -c 'synth frame committed' "$out/river.log" || true; }
frames_over() {
    local a b
    a=$(frames)
    sleep "$1"
    b=$(frames)
    echo $((b - a))
}
cpu_over() {
    local a b tck
    tck=$(getconf CLK_TCK)
    a=$(awk '{print $14 + $15}' "/proc/$wm/stat")
    sleep "$1"
    b=$(awk '{print $14 + $15}' "/proc/$wm/stat")
    echo $(((b - a) * 1000 / tck / $1))
}

until_true 'grep -q "listening to sound" "$out/river.log"'
until_true '[ "$(state newm-synth)" != absent ]'
ok "react-to-sound opens one capture (node newm-synth, $(state newm-synth))"

sleep 7
[ "$(state e2e-sink)" = suspended ] || fail "silence: the sink is $(state e2e-sink), not suspended"
n=$(frames_over 3)
[ "$n" -eq 0 ] || fail "silence: $n frames in 3 s"
cpu_silent=$(cpu_over 5)
ok "in silence the sink suspends (capture $(state newm-synth)) and nothing is drawn; CPU $cpu_silent ms/s"

python3 -I -c 'import wave,math,struct,sys
w=wave.open(sys.argv[1],"wb");w.setnchannels(1);w.setsampwidth(2);w.setframerate(48000)
# 6 s: 2 s loud, 2 s quiet, 2 s loud, so the level moves.
f=[(16000 if (i//96000)%2==0 else 1500)*math.sin(i*0.0575) for i in range(48000*6)]
w.writeframes(b"".join(struct.pack("<h",int(v)) for v in f));w.close()' "$out/tone.wav"
a=$(frames)
pw-play "$out/tone.wav" &
play=$!
sleep 1
sink_playing=$(state e2e-sink)
cpu_tone=$(cpu_over 4)
wait $play || true
b=$(frames)
n=$((b - a))
[ "$sink_playing" = running ] || fail "a tone playing: the sink is $sink_playing"
[ "$n" -ge 3 ] || fail "a 6 s tone moving between loud and quiet drew $n frames"
[ "$n" -le 26 ] || fail "a 6 s tone drew $n frames: more than max-fps 4 allows"
ok "a tone runs the sink and draws $n frames in about 6 s (max-fps 4 allows 24); CPU $cpu_tone ms/s"

sleep 8
[ "$(state e2e-sink)" = suspended ] || fail "after the tone: the sink is $(state e2e-sink)"
n=$(frames_over 3)
[ "$n" -eq 0 ] || fail "after the tone settled: $n frames in 3 s"
ok "after the tone the level settles, drawing stops and the sink suspends again"

"$nrv" msg action set-synth-background still
until_true 'grep -q "not listening to sound" "$out/river.log"'
until_true '[ "$(state newm-synth)" = absent ]'
ok "still closes the capture: no newm-synth node left"

echo "CPU ms/s: silent $cpu_silent, tone $cpu_tone"
echo "all $pass synth sound checks passed"
