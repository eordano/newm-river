#!/usr/bin/env bash
# Headless proof of the river fork's newm-river-render-v1: river (headless backend; scenefx's
# GLES2 renderer, or pixman in a -Dscenefx=false build) draws a red foot window at (100, 100),
# 400x300, at scale 1 / opacity 1, at scale 0.5 / opacity 0.5, and at 0.5 / 0.5 again after the
# app redrew at 300x200 while scaled; grim shots are then checked pixel by pixel. With render v2
# (scenefx) it also checks rounded corners, the shadow, a snapshot drawn after its window closed
# and gone once released, and a pointer op that keeps reporting motion past the output's edge. Run from the repository
# root inside the dev shell, after `zig build` has produced zig-out/bin/river.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
river=$root/zig-out/bin/river
[ -x "$river" ] || { echo "no $river: run zig build first" >&2; exit 1; }
(cd "$root/newm" && cargo build -q --example render_probe --example vinput)
probe=$root/newm/target/debug/examples/render_probe
vinput=$root/newm/target/debug/examples/vinput
work=$(mktemp -d /tmp/render_probe.XXXXXX)

probe_run() { # scale opacity shot [probe flags]
  local runtime
  runtime=$(mktemp -d -p "$work")
  XDG_RUNTIME_DIR=$runtime WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    WLR_HEADLESS_OUTPUTS=1 timeout 60 "$river" \
    -c "$probe run --scale $1 --opacity $2 --x 100 --y 100 --width 400 --height 300 --shot $3 ${*:4}" \
    > "$3.log" 2>&1 || true
}

shoot() { # scale opacity shot [probe flags]
  probe_run "$@"
  [ -s "$3" ] || { echo "no screenshot $3; logs kept in $work" >&2; exit 1; }
}

shoot 1 1 "$work/full.ppm"
shoot 0.5 0.5 "$work/half.ppm"
shoot 0.5 0.5 "$work/resized.ppm" --resize 300 200

# foot snaps 400x300 to its cell grid: the probe records the size it drew.
read -r w h < "$work/full.ppm.size"
read -r w2 h2 < "$work/half.ppm.size"
[ "$w $h" = "$w2 $h2" ] || { echo "foot drew $w x $h, then $w2 x $h2" >&2; exit 1; }
hw=$((w / 2)) hh=$((h / 2))
echo "render_probe: foot drew ${w}x${h}; scaled box ${hw}x${hh}"
# Unscaled: the whole window is opaque red.
"$probe" check "$work/full.ppm" 100 100 "$w" "$h" 255 0 0
# Scaled about the top-left: a half-size box at the same corner, red at half opacity over black.
"$probe" check "$work/half.ppm" 100 100 "$hw" "$hh" 128 0 0
# And nothing of the window outside it: right of and below the scaled box is the black background.
"$probe" check "$work/half.ppm" $((100 + hw + 2)) 100 $((w - hw)) "$h" 0 0 0
"$probe" check "$work/half.ppm" 100 $((100 + hh + 2)) "$hw" $((h - hh)) 0 0 0
# Resized while scaled: the app's new, smaller buffer is scaled too, and the old area is cleared.
read -r w3 h3 < "$work/resized.ppm.size"
rw=$((w3 / 2)) rh=$((h3 / 2))
echo "render_probe: resized while scaled to ${w3}x${h3}; scaled box ${rw}x${rh}"
"$probe" check "$work/resized.ppm" 100 100 "$rw" "$rh" 128 0 0
"$probe" check "$work/resized.ppm" $((100 + rw + 2)) 100 $((w - rw)) "$h" 0 0 0
"$probe" check "$work/resized.ppm" 100 $((100 + rh + 2)) "$rw" $((h - rh)) 0 0 0
echo "render_probe: scale and opacity OK"

# RENDER_PROBE_V2=0 for a river built with -Dscenefx=false (render v1 only).
if [ "${RENDER_PROBE_V2:-1}" = 0 ]; then
  rm -rf "$work"
  exit 0
fi

# Rounded corners (24 px): the corners outside the radius are the black background, the edges'
# middles and the inside past the radius the red window.
shoot 1 1 "$work/round.ppm" --radius 24
"$probe" check "$work/round.ppm" 100 100 5 5 0 0 0
"$probe" check "$work/round.ppm" $((100 + w - 5)) 100 5 5 0 0 0
"$probe" check "$work/round.ppm" 100 $((100 + h - 5)) 5 5 0 0 0
"$probe" check "$work/round.ppm" $((100 + w - 5)) $((100 + h - 5)) 5 5 0 0 0
"$probe" check "$work/round.ppm" $((100 + w / 2 - 10)) 100 20 6 255 0 0
"$probe" check "$work/round.ppm" 100 $((100 + h / 2 - 10)) 6 20 255 0 0
"$probe" check "$work/round.ppm" 110 110 20 20 255 0 0
echo "render_probe: rounded corners OK"

# The shadow (green, spread 24, softness 4) at the window's opacity 0.5: 11-14 px above the
# window green at half strength, under the half-transparent window none (red at half).
shoot 1 0.5 "$work/shadow.ppm" --shadow
"$probe" check "$work/shadow.ppm" $((100 + w / 2 - 10)) 84 20 8 0 128 0
"$probe" check "$work/shadow.ppm" $((100 + w + 10)) $((100 + h / 2 - 10)) 6 20 0 128 0
"$probe" check "$work/shadow.ppm" $((100 + w / 2 - 10)) $((100 + h / 2 - 10)) 20 20 128 0 0
echo "render_probe: shadow OK"

# A snapshot: foot closed, its last frame still drawn (at the snapshot's half scale), then gone
# once released.
shoot 1 1 "$work/snap.ppm" --snapshot
grep -q 'window closed, snapshot kept' "$work/snap.ppm.log" || { echo "foot did not close" >&2; exit 1; }
"$probe" check "$work/snap.ppm" 100 100 "$hw" "$hh" 255 0 0
"$probe" check "$work/snap.ppm" $((100 + hw + 2)) 100 $((w - hw)) "$h" 0 0 0
"$probe" check "$work/snap.ppm.released" 100 100 "$w" "$h" 0 0 0
echo "render_probe: snapshot kept after close and released OK"

# A pointer op started 10 px from the right edge of the 1920 px output: 400 px of motion to the
# right still reach op_delta although the cursor stops at the edge.
probe_run 1 1 "$work/op.ppm" --op "$vinput"
[ -s "$work/op.ppm.op" ] || { echo "no op_delta recorded; logs kept in $work" >&2; exit 1; }
read -r dx dy < "$work/op.ppm.op"
echo "render_probe: op_delta after 400 px right at the edge: $dx $dy"
[ "$dx" -ge 390 ] && [ "$dy" -eq 0 ] || { echo "op_delta stopped at the edge; logs in $work" >&2; exit 1; }
rm -rf "$work"
echo "render_probe: render v2 and pointer op OK"
