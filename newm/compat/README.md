# compat: the application compatibility VM suite

NixOS VM cases (`pkgs.testers.runNixOSTest`) that run real applications against newm-river
(river's DRM backend).

- `compositors.nix`: how the desktop starts, as a NixOS session module starts it: tty1's login
  shell runs `newm-river-session`, which starts the `newm-river.service` user unit and waits on
  it; the window manager reads the KDL in `$NIRI_CONFIG` and answers niri's IPC
  (`newm-river msg`). river runs Xwayland itself. `vmKdl` and `env` hold what the compositor
  needs in the VM, `xfail` the cases that fail for now and why. The VM's virtio GPU has no 3D,
  so river renders on Mesa's llvmpipe. The suite runs the default build, without
  `LIBGL_ALWAYS_SOFTWARE`.
- `default.nix`: `{ pkgs, compositor, strict ? true }` -> `{ compat-<name>[-<suite>] = <test>; }`.
  The guest: a 2560x1080 virtio GPU without 3D (Mesa llvmpipe), 4 GB, 4 cores, uinput
  keyboard/mice/clickpad (`uin.py`, from `nix/vm`), and a KDE session environment
  (`XDG_CURRENT_DESKTOP=river:KDE`, `QT_QPA_PLATFORMTHEME=kde`, `NIXOS_OZONE_WL=1`). Its
  `tests` attrset holds one VM per suite: the script `<name>.py` after `lib.py` and its `libs`,
  with its own QEMU devices, KDL, session environment, udev rules and NixOS modules.
- `config.nix`: the session's KDL: a canvas and new-window size, `prefer-no-csd`, and scale 1.25
  on the only output, so every toolkit case runs at a fractional scale.
- `probes/`: the clients the cases drive: GTK 3/4 (PyGObject) and PyQt6 windows that write what
  was typed and their scale on Enter, a page that puts Chromium's `devicePixelRatio` and the
  typed text in its title, and the protocol probes (`gtkx`, `qtx`, `wl`, `sdl`, `dbusx`,
  `syncobj.c`, ...).

## Suites

- `smoke` (`compat-<name>`): one window per toolkit, the table below.
- `gaps`: protocols newm-river lacked (`PROTOCOLS.md`), each with a real client.
- `input`, `desktop`, `robust` (with `proto_lib.py` and the guest additions in `protocols.nix`):
  input methods, virtual devices, clipboard, drag and drop and pointer kinds; activation,
  portals, layer shell, lock, idle, outputs, foreign toplevels, security context and the KDE
  D-Bus services; weston's demo clients and clients that misbehave or die mid-grab.
- `remote-input` (`remote-input.nix`): KDE Connect's remote mouse and keyboard through the
  RemoteDesktop portal, and explicit sync.
- `backends` (`backends_lib.py`): two virtio GPUs, gpu0 2560x1080 (Virtual-1, scale 1.25) and
  gpu1 1920x1080 (Virtual-2, scale 1); river takes both through `WLR_DRM_DEVICES` (udev names them
  `/dev/dri/gpu{0,1}-{card,render}`). Cases: both outputs drawn; `first-buffer` (foot, GTK 4 and
  Chromium under `WAYLAND_DEBUG`: the first committed buffer is already at 1.25 and a scale
  reached the client before it); Virtual-2 off with a window on it and back on; scale 1.25 ->
  1.5 reaching an open window; mode 2560x1080 -> 1920x1080 with the focus on screen; `chvt 2`
  and back (same compositor process, both outputs drawn, typing works); nested sessions inside
  the VM's session: river itself (its Wayland backend) and nixpkgs' niri, each drawing the GTK
  probe.
- `two-gpu`: the same two GPUs with gpu1 in `ignore-drm-device` (river: `WLR_DRM_DEVICES` gpu0
  only): one output, drawn, typing works, gpu1's card never opened.

## Running

```
nix build .#checks.x86_64-linux.compat-newm-river -L                      # strict
nix build .#legacyPackages.x86_64-linux.compat-report.compat-newm-river   # never fails
nix build .#legacyPackages.x86_64-linux.compat-report.compat-newm-river-backends   # one suite
```

The report's output holds
`results.txt` and `results.json` (one line per case with the facts it measured),
`globals-<name>.txt` and `globals-kwin.txt` (every global and version),
`journal-<name>[-<suite>].txt`, `shots/` and `probe/` (probe outputs, WAYLAND_DEBUG logs,
wayland-info dumps). The VMs are heavy: run them on a build host rather than on a desktop. To
test other application versions, override nixpkgs with `--override-input
nixpkgs 'github:NixOS/nixpkgs/<rev>'`.

## Cases (`smoke`)

| Case | Passes when |
|---|---|
| `inventory` | wayland-info lists the compositor's globals (written to `globals-<name>.txt`) |
| `foot` | foot reads the typed word; the shot changed |
| `gtk4-probe` | GTK 4 reads the word; preferred fractional scale 1.25, `gdk_surface_get_scale` 1.25, largest buffer / viewport destination 1.25; green in the shot |
| `gtk4-widget-factory` | the window maps and draws |
| `gtk3-probe` | GTK 3 reads the word; scale factor 2 (GTK 3 has no fractional scale: the compositor downscales) |
| `gtk3-widget-factory` | the window maps and draws |
| `libadwaita-zenity` | zenity 4 (libadwaita) draws its entry dialog and prints the typed word |
| `qt-kde-probe` | Qt 6 on the wayland platform reads the word; the KDE platform theme loads Breeze; DPR, preferred scale and buffer ratio 1.25 |
| `kwrite` | KWrite maps, draws, and its title changes once typed into |
| `xterm-xwayland` | xterm through Xwayland reads the word; the shot changed |
| `chromium` | Chromium on Wayland (software) reads the typed text; `devicePixelRatio` 1.25 |
| `kwin-reference` | `kwin_wayland --virtual` (private bus, `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`) lists its globals to `globals-kwin.txt`, the reference for `PROTOCOLS.md` |

A compositor descriptor's `xfail` maps a case name to the reason it fails for now; such a case
reports `xfail`, and `xpass` once it passes.
