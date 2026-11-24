# newm-river

newm's strict tile canvas desktop on [river](https://codeberg.org/river/river) main. river
composites and speaks to the apps; every policy (tiles, sizes, focus, keys, overview, shell
graphics) lives in a separate window-manager client over `river-window-management-v1`. This
repository is a fork of river's `main` (Zig, at the root, tracking upstream with small patches)
plus a Rust workspace under `newm/` with that window manager.

| Path | What |
|---|---|
| `river/`, `protocol/`, `build.zig` | river, fork of `main`; our patches are listed below |
| `newm/newm-river` | the window manager (binary `newm-river`) |
| `newm/newm-canvas` | the strict tile canvas and its Rocq proof |
| `newm/newm-river-lock` | the ext-session-lock screen locker |
| `newm/niri-config`, `newm/niri-ipc` | the KDL config and IPC types, from niri's |
| `newm/newm-kde`, `newm/newm-synth` | KDE session services, the synthwave background |
| `newm/compat` | the application compatibility VM suite |

The window manager reads niri's KDL configuration (`niri-config`) and answers niri's IPC
(`niri-ipc`) on a niri-compatible socket; the river fork carries only what
the protocol lacks.

## Build

Run builds one at a time inside this flake's dev shell (`nix develop`), on a machine with room
for a full Rust and Zig build.

```
zig build -Doptimize=ReleaseSafe -Dxwayland          # river -> zig-out/bin/river
cd newm && cargo build --release                       # newm-river, newm-river-lock
cd newm && cargo clippy --workspace --all-targets -- -D warnings
cd newm && cargo test --workspace --exclude niri-config && cargo test -p niri-config --lib
cd newm && cargo test -p newm-river --features telemetry   # the usage log and its tests
nix shell nixpkgs#jq -c newm/e2e/headless.sh           # headless river + foot, driven over IPC
cd newm && cargo build --release -p newm-river --example e2e_client
nix shell nixpkgs#jq -c newm/e2e/session.sh            # placeholders, rules, layer shell, outputs, restore
nix shell nixpkgs#jq -c newm/e2e/screencast.sh         # zkde streams, org.kde.KWin, xdg-desktop-portal-wlr
```

`e2e/screencast.sh` starts a private PipeWire and WirePlumber (devices off) in its own
`XDG_RUNTIME_DIR` and a private session bus; `examples/zkde_probe.rs` asks for streams as
KPipeWire does and consumes the nodes. With `SPECTACLE=<spectacle binary>` (and `ffprobe` on
PATH) it also records the screen with Spectacle itself; the dev shell's `XDPW` names
xdg-desktop-portal-wlr's binary for the portal part.

`newm-river` runs the window manager as its child and starts it again after a crash (`supervise.rs`:
a non-zero exit or a signal while river answers; at once, then backing off, at most 5 a minute).
river keeps the windows, outputs and lock for the next window manager; a restarted one runs no
startup program and, on a TTY, puts the windows back on the spots session.json saved, launching
nothing. `e2e/session.sh` SIGKILLs it mid-session.

`e2e/session.sh` drives windows of the `e2e_client` example (solid-colour xdg toplevels with size
caps, a parent or fullscreen asked, layer-shell surfaces) and checks pixels of grim shots. Its
restore part sets `NEWM_RIVER_SESSION=tty`: a headless river is not the TTY session.

### Live tests in a VM (`nix/vm`)

What the headless e2e cannot do runs in NixOS VMs (`pkgs.testers.runNixOSTest`, KVM, 3 GB, 4
vCPUs): river on a 2560x1080 virtio GPU through its DRM backend, started by a tty1 autologin the way
a NixOS session starts it (`newm-river-session` -> `newm-river.service` -> `river -c
newm-river-init`, `nix/package.nix`), with a desktop's config values (`nix/vm/config.nix`:
binds, canvas, swayidle + `newm-river-lock` with PAM service `newm-lock`, an `e2e_client` layer
surface for the bar, swaybg). Input comes from uinput devices libinput takes for hardware
(`nix/vm/uin.py`, python3Packages.evdev: a USB keyboard, an absolute and a relative mouse with
wheels, a five-slot multitouch clickpad). Assertions read `newm-river msg --json`, the
`newm-river.service` journal (`RUST_LOG=newm_river=debug`), wev's event log, the e2e client's
pinch log, logind and pixels of
grim shots (Pillow in the test driver); QEMU's own screendump shows what the DRM side scans out.

```
newm/e2e/vm.sh                                   # all five, one VM at a time (on a KVM host)
newm/e2e/vm.sh vm-lock                           # one; OUT=/tmp/newm-river-vm gets shots + results
nix build .#checks.x86_64-linux.vm-input -L      # strict: fails on any unexpected failure
```

| Test | Cases |
|---|---|
| `vm-input` | session on DRM (river holds `/dev/dri/card0`, no nested fallback); Super tap opens and closes the overview, not with Ctrl, not with an unbound key; 3 fingers move the focus one window a swipe, however far they go; 4 fingers left/right move it once, up/down zoom the camera out and in by whole steps with the overview shut; Super+3 pans the camera after the fingers on both axes and moves no window; Super+4 zooms to fit, moves the window and zooms back on Super's release; Super+wheel zooms one step per notch and is no tap; a three-finger pinch in and a three-finger spread zoom the camera by the steps river's reported scale gives (`pinch zoomed A to B` in the journal); a plain two-finger pinch reaches the app (`e2e_client --log-pinch`) and zooms nothing; Super and a two-finger pinch zoom, reach no app and are no tap; Super+two-finger scroll does not zoom and reaches the app; plain two-finger scroll reaches the app; the minimap shows in the bottom-left corner after a focus move, the screen's near-white outline centred in the plan, and goes again; a click on a zoomed-out window never reaches it (wev) and picks a window; Alt+Tab draws the switcher panel (pixel diff), Alt release switches, Tab twice reaches the third, Alt+Escape cancels; middle-button scrolling and a still middle click; a Super pan at the screen edge; `pan-acceleration` (hot-reloaded, with the mouse reloaded to flat at `accel-speed 1.0`, libinput's constant 2x): a 600-count Super flick pans 1200 px at 1, 900 at 0.5 and the hand's 600 at 0; `pan-momentum` (hot-reloaded): a 400 px Super pan let go of mid-motion glides on at 1 and stops where the hand stopped at 0, where a three-finger swipe still moves the focus; 8 px rounded corners; a bind added to the config works without a restart; a display's scale set and reset as a live setting (`display-scale`); Escape cancels a drag (home, no drop, no key to the app, a glide in the usage log); the drag preview moves the window in the way; a floating window resized with Super+right and the absolute mouse from a corner (top-left in place) and from its left edge (right edge in place, live before the release), Escape restoring its size and place; a double Super+right click spanning the screen in whole tiles and back; open and close animations show mid-way; floating-follow in its three modes; a Super pan at the screen edge; `queryWindowInfo` waits for a click and Escape cancels it. |
| `vm-lock` | ScreenShot2 works unlocked; `loginctl lock-sessions` -> swayidle -> newm-river-lock, LockedHint set; ScreenShot2 refused while locked; a wrong password keeps it locked; typing the password (PAM `newm-lock`) unlocks; Super+Escape locks; `wlopm --off/--on '*'`; swayidle's idle timeouts lock and turn the monitors off, input turns them on |
| `vm-session` | the tty1 session (logind TTY, river's DRM card, no nested fallback); an X11 xterm through river's Xwayland beside foot, its title; session.json saved (0600); a SIGKILL to the window manager (`supervise.rs`) keeps the unit, river and both windows, and the restarted one puts them back on their spots; IPC quit logs out (river, the unit, the logind session); the next login restores both on their tiles; the config's output block sets scale 2; scale 2 (wlr-randr): 2560x1080 is 1280 px logical, a 2 px border, the placeholder icon; two outputs (DRM + wlroots' headless, QEMU enables one virtio head without a display) showing the wallpaper, and a window opened after a click on the empty second one; the last logout leaves no session |
| `vm-reattach` | two VMs (`nix/vm/reattach.nix`): machine set up as a desktop, with real Ghostty 1.4 (the resident `app-com.mitchellh.ghostty.service` graphical-session.target wants, running fish; Mesa llvmpipe for its OpenGL), tmux 3.8 with a `set-titles-string "#{host_short}: ..."` and fish_title, tmux as a user service (`tmux -D`, `TMUX_TMPDIR=%t`) and OpenSSH multiplexed (`ControlMaster auto`, `~/.ssh/cm-%C`, `ControlPersist 4h`); far, an sshd taking alice's key only, alice lingering. Four windows typed into as a user does: tmux `work`; `ssh far` + tmux `far`; `ssh far`, `cd /tmp` + tmux `far2`; a plain fish. session.json (0600) holds `tmux new-session -A -s work`, the flocked `ssh -t far tmux new-session -A -s far` / `far2` and a plain `--working-directory=$HOME`. After an IPC logout and the next autologin, twice (the second saves what the first relaunched): every window on its saved tile; a client of Ghostty's on `work`, its windows and panes unchanged; far's `far` and `far2` attached, the same sessions; two windows running ssh, the plain shell plain. The key sits in an `ssh-add -c` agent whose askpass counts the asks (a YubiKey touch's stand-in): one for both windows when they were opened, none at a login while the master outlived the logout (Ghostty keeps each surface's processes in their own transient scope), one for both windows once it was closed. A foot window (foot 1.28, a process per window, fish from `foot.ini`) attached to local tmux `ft` in `/tmp` is saved as `foot --working-directory=$HOME sh -c 'tmux new-session -A -s ft; ...'` and comes back on its tile attached to `ft`, its client under the restored foot |
| `vm-browser` | Chromium 157 from nixpkgs (`nix/vm/browser.nix`) launched from a desktop entry alice overrides (`~/.local/share/applications/chromium-browser.desktop`: Wayland, software rendering, its own profile, DevTools on 127.0.0.1:9222), the profile opening one fixed startup page as a desktop profile can. Over DevTools (`cdp-tabs`) two windows of two local pages each (Start + Alpha; Beta one + Beta two), swapped so the saved spots are not the opening order. session.json holds the entry's bare launch, no flag. After an IPC logout (Chromium dies with river) and the next autologin: one Chromium relaunched with `--restore-last-session`, both windows back with the same tabs per window, on their saved spots, and no other window. Control: a clean exit and the same bare launch without the flag open only the startup page |

`nix/vm/xfail.nix` lists the cases that fail until a sibling branch lands, by branch; such a
case reports `xfail`, and `xpass` once it passes (then drop its line). The `vm-report.*`
variants `vm.sh` builds never fail, so screenshots (`shots/*.png`, `*-drm.png`) survive a failure.

The usage log (`model/usage.rs`, `model/motion.rs`): each op line carries its
`seq`, the camera before and after (1:1 and zoomed) with its jump, and every moved window's
distance in tiles and pixels; an op, a pan with its fling, a drag preview or a cancel begins a
motion episode, sampled every frame, which writes one `{"op":"motion"}` line (path, max speed,
acceleration and frame jump of the camera and the windows) once it settles or is cut.
Op lines also carry `op_us`; `open` lines with `size_source`, `app_px`, `span`,
`focused`, `since_last_open_ms` and a placeholder's `restore` {`matched_by`, `waited_ms`}; `app-grow`
lines (an app's minimum, a kept commit or a config reload grew a window through the placement
rules: `cause`, `pushed`); `settle`/`unsettled` lines per new window (`configures`, `ms`); `float`
lines per tile/float decision; `restore-expired`; `restore-rematch` (app, a trade with a window
or a placeholder, gain); `during_drag`, and a drop's `home_moved`. One writer thread takes the
lines through a bounded queue; a full queue drops lines, counted in the next line's `dropped`.
Only a build with the `telemetry` feature writes the log (`withTelemetry` in `nix/package.nix`);
a default build makes, times and writes no line and starts no writer thread.

niri-config's `tests/wiki-parses.rs` reads a `docs/wiki` directory this repository does not
carry: run niri-config's library tests only.

The bar: zero warnings, clippy `-D warnings` clean, every test green.

river renders through [scenefx](https://github.com/wlrfx/scenefx) (nixpkgs `scenefx`, in the dev
shell), whose renderer is GLES2 only: river needs EGL on a render node even headless
(`WLR_RENDERER` is ignored; on a headless NVIDIA host the GPU's `renderD128`). `zig build -Dscenefx=false`
builds against plain wlroots (pixman works again; render protocol v1 only, so newm-river keeps
its software glow and square corners).

Run nested for a test: `river -c newm-river` inside any Wayland session (wlroots' wayland
backend), or headless with `WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1` plus `grim` for
screenshots. river starts the command given with `-c` as its init; newm-river reads its config
from `$NIRI_CONFIG` (else `~/.config/newm-river/config.kdl`).

## river patches (fork-only protocol files live in `protocol/newm-*.xml`)

| Patch | Why | Status |
|---|---|---|
| `protocol/newm-river-render-v1.xml` v1 (`river/NewmRender.zig`, `river/NewmScale.zig`): per-window `set_scale` and `set_opacity`, rendering state applied at `render_finish`; a scaled window is drawn scaled about its node's top-left (surfaces, subsurfaces, popups, borders, decorations), never resized, and gets no pointer/touch/tablet input while the WM still sees `window_interaction` | the zoom is a camera; unfocused windows at 90% | done; `newm/newm-river/examples/render_probe.sh` checks it headless |
| scenefx (`build.zig`, `river/Server.zig`, `river/NewmFx.zig`): `libscenefx-0.5` linked before wlroots so its `wlr_scene_*` replace wlroots' (same leading struct fields, so zig-wlroots' bindings hold), river creates scenefx's GLES2 renderer; scenefx's extra node types are skipped where river walks scene trees; `-Dscenefx=false` builds without it. Under GLES an output's captures come in the renderer's read format (BGR888 on NVIDIA, or 10-bit): `capture.rs` reads those too | wlroots 0.20's scene has no corner radius, clip mask or shadow | done; e2e `headless.sh` and `session-socket.sh` green on NVIDIA EGL |
| render v2 (`river/NewmFx.zig`, `river/NewmSnapshot.zig`, `Window.drawBorders`): `set_corner_radius` rounds the buffers at the content box's corners and draws a four-edge border as one rounded outline; `set_shadow`/`clear_shadow` put a scenefx shadow node under the window (not under its content box), scaled with `set_scale`, faded with `set_opacity`; `snapshot` copies the window's drawn frame (buffers, borders, shadow) into a node above it that outlives the window, with its own `set_position`/`set_scale`/`set_opacity` (rendering state) until destroyed, the client disconnects or its lifetime (at most 10 s) passes (`expired`) | 8 px corners and the focus shadow drawn by river; niri's close animation from the last frame | done; `render_probe.sh` checks corners, shadow, snapshot kept and released |
| pointer op motion (`river/Cursor.zig`, `river/Seat.zig`): `op_delta` reports the summed motion asked for since `op_start_pointer` (relative motion after constraints, absolute motion as its displacement), not the cursor's displacement, so it keeps growing while the cursor is clamped at an output edge; warps do not count | a pan goes on past the screen edge, as niri's pans on relative motion | done; `render_probe.sh` moves 400 px right from 10 px before the edge |
| `protocol/newm-river-gestures-v1.xml` (`river/NewmGestures.zig`, `river/NewmGestureState.zig`): per-seat swipe/pinch/hold bindings (finger count + modifiers) with begin/update/end, and axis bindings (orientation + modifiers) with summed value and value120; bound input is consumed, unbound reaches clients. v2: `set_sources` limits an axis binding to wheel/finger/continuous/wheel-tilt events | 3/4-finger focus, zoom, pan and move gestures, and the overview's own inside it; pinches of three or more fingers, and any pinch with Super, zoom while a plain two-finger pinch reaches the app; Super+wheel zooms (`river_pointer_binding_v1` takes buttons only) while Super+two-finger scrolling reaches the app unless a `TouchpadScroll*` bind takes it | done; routing and coalescing covered by `zig build test`, devices untested headless |
| gestures v4 (`river/Cursor.zig`, `river/NewmGestures.zig`, `river/NewmGestureState.zig`): `newm_river_gestures_seat_v1.pointer_motion(dx, dy, unaccel_dx, unaccel_dy)`, the pointer's relative motion summed since the previous one, with and without libinput's acceleration, in every manage sequence the pointer moved (op or not); counted where `op_delta` counts (after constraints, a confinement holding back the unaccelerated part in proportion; past the output edges; absolute motion as its displacement in both); what wl_fixed's 1/256 steps cannot carry waits for the next event | river-window-management-v1 gives the WM only the cursor's (accelerated) motion; `wlr_pointer`'s `unaccel_dx/dy` reached only `wp_relative_pointer_v1` clients, and `pan-acceleration` pans by `unaccel + s * (accel - unaccel)` | done; coalescing, carry and range covered by `zig build test`, `vm-input` pan-acceleration |
| gestures v3 (`river/Cursor.zig`, `river/NewmGestures.zig`): `newm_river_gestures_seat_v1.desktop_press(x, y)` when a button no pointer binding takes is pressed over no surface or a background/bottom layer surface (the wallpaper still gets it) | a click on an empty output reaches no window or shell surface, so the WM never heard of it and could not make that output active (niri does) | done; e2e `session.sh` clicks the empty second output, `vm-session` click-empty-output |
| DPMS on (`river/OutputManager.zig`): a state's color transform equal to the output's is dropped before `wlr_backend_commit` (as `wlr_output_commit_state` drops unchanged fields) | re-enabling an output makes the scene reset its gamma LUT; on a CRTC without `GAMMA_LUT` (virtio-gpu) the DRM backend's legacy gamma path fails that without a log, so `wlopm --on` after `--off` failed ("failed to commit new output configuration") | done; `vm-lock` wlopm-off-on, idle-lock-and-monitors. Upstream-worthy (river), the silent failure too (wlroots) |
| `xdg-toplevel-tag-v1` global (wlroots' implementation) | kwindowsystem 6.31 (Spectacle) segfaults without it | done |
| `zkde_screencast_unstable_v1` v5 (`protocol/upstream/zkde-screencast-unstable-v1.xml`, KDE's) relayed over `protocol/newm-river-screencast-v1.xml` (`river/NewmScreencast.zig`): river serves the global and hands each `stream_output` (by output name), `stream_window` (the client's uuid string) and `stream_region` to the active window manager's relay as a `newm_river_screencast_cast_v1`; the window manager answers `created(node)`, `failed` or `closed`, which river forwards; `stopped` tells it the client closed the stream. Virtual outputs fail in river; with no window manager relay every stream fails. Both globals are hidden from sandboxed clients | Spectacle records through KPipeWire over zkde, a compositor global; the PipeWire stream is the window manager's | done; `e2e/screencast.sh` |
| capture refresh (`river/Server.zig` `CaptureSession`): 16, 100 and 500 ms after an ext-image-copy-capture session starts, river toggles the source's scene (a window's capture scene, or the root scene for an output) | wlroots sources send frames only with new damage: a second session on a window's reused scene source, or a first capture asked after the session's start commit (xdg-desktop-portal-wlr), got no frame until the source changed | done; `e2e/screencast.sh` (second window stream, portal after a cast) |
| opening scale (`river/NewmOpeningScale.zig`, `river/XdgToplevel.zig`, `river/LayerSurface.zig`): a toplevel's initial commit gets the scale of the output under the cursor (a layer surface: its own output) as `wp_fractional_scale_v1.preferred_scale` and `wl_surface.preferred_buffer_scale`, before the first configure | the scene sends a surface its output's scale only once a buffer of it is on that output, so Chromium drew its first frame at DPR 1 and GTK 4 at scale 2 on a 1.25 output, then drew again | done |
| input resume (`river/NewmInputResume.zig`, `river/Server.zig`): on the session's active signal river has the event loop dispatch the libinput backend's fd source (`wl_event_source_check`, through the head of wlroots 0.20's internal `wlr_libinput_backend`), so devices libinput added back on resume appear at once instead of at the first key | TODO(wlroots): dispatch in the libinput backend's `session_signal` |
| first modeset keeps what works (`river/OutputManager.zig` `commitUsableOutputs`): when the first, all-output commit fails, each output is committed on its own (wlr_backend_test passes an output whose multi-GPU import fails only at commit); the failing ones stay off (logged, out of the layout) and the rest are enabled; river exits only when none can be enabled | river exited at the first failed modeset, so one output it cannot drive (compat `backends`: a second GPU without 3D; on hardware a USB display adapter or a refused mode) ended the session at login, in a relogin loop | done |
| modesets river schedules keep what works (`river/OutputManager.zig`): `commitUsableOutputs` covers every modeset river schedules itself, not only the first (only a wlr-output-management client's configuration keeps its all-or-nothing answer); past two failures in a row a failed modeset no longer schedules another manage sequence by itself (a new output or a client configuration re-arms it) | wlroots destroys every output when the session goes inactive and makes new ones when it comes back: a VT switch back retried a failing modeset about 340 times a second and never showed the first output again | done |
| `org_kde_kwin_fake_input` v6 (`river/FakeInput.zig`, `river/NewmFakeInputPolicy.zig`, `river/Server.zig` filter, `river/Keyboard.zig`): a pointer, a touch device and a virtual keyboard per authenticated object on the default seat, the global only for the xdg-desktop-portal-kde executable; `Keyboard.setGroup` no longer assumes every virtual keyboard is a virtual-keyboard-v1 one (it unwrapped `getVirtualKeyboard()` and panicked on any other) | xdg-desktop-portal-kde's RemoteDesktop (KDE Connect's remote mouse and keyboard) types through it; river-window-management gives the WM no input injection | done; `zig build test`, compat `remote-input` |
| wlroots patch `nix/wlroots-output-management-virtual-mode.patch` (`nix/wlroots.nix`, one patched wlroots for river and scenefx; a downstream package uses the same file): wlr-output-management sends a head without a fixed mode its virtual mode on re-enable to every client that lacks one | `wlr-randr --off` then `--on` of a headless or nested output aborted river (`head_send_state: Assertion 'found'`): wlr-randr bound while the head was off, got no mode, and another client's virtual mode kept wlroots from sending one. DRM heads always list their modes and never took this path | done (e2e `outputs.sh`) |
| scenefx dmabuf feedback (`nix/scenefx-dmabuf-feedback-keep-hidden.patch`, applied in `nix/wlroots.nix`): a scene buffer hidden and shown again on the same output keeps the linux-dmabuf feedback it was last sent, as wlroots' own scene does; the feedback is forgotten when the buffer moves straight to another output or the output it was sent for goes away | river disables a window's trees around each render sequence, and NewmScale around each commit of a scaled, rounded or shadowed window; unpatched scenefx forgets the feedback at each toggle and sends an identical one, with a new format table fd, per commit and per animation frame (60 a second for a 60 fps window), and Firefox, which keeps those fds, crashes at RLIMIT_NOFILE | done; `examples/feedback_probe.rs` in e2e `headless.sh` and `vm-session` dmabuf-feedback: at most 3 feedbacks in 5 s at rest, while the camera zooms and around the overview (2 at the map, then 0). Upstream-worthy (scenefx) |
| wlroots dmabuf feedback reuse (`nix/wlroots-dmabuf-feedback-reuse.patch`, applied in `nix/wlroots.nix`): `wlr_linux_dmabuf_v1_set_surface_feedback` sends nothing when the compiled feedback equals the one the surface's feedback objects last got (the default one included), and a format table equal to that one shares its shm file | each send hands the client a format table fd; a caller that sets the same feedback again (the scene's first composition feedback equals the default one) costs the client an fd and a reparse for nothing | done; `feedback_probe` in e2e `headless.sh` gets 1 feedback at the map |

Fork-only protocols keep upstream's XML untouched, so merges from `upstream/main` stay clean.
