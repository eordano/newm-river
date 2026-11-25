# river (this fork) + the newm-river window manager + the session wrapper a NixOS configuration
# installs: `newm-river-session` starts `newm-river.service`, whose
# `river -c newm-river-init` exports NIRI_SOCKET, imports the environment and runs newm-river.
# The VM tests (nix/vm) run this package the way a TTY login runs it.
{
  lib,
  river,
  scenefx,
  wlroots_0_20,
  rustPlatform,
  pkg-config,
  libinput,
  libxkbcommon,
  linux-pam,
  pipewire,
  wayland,
  libglvnd,
  runCommand,
  newmSrc ? ../.,
  # Extra river flags in the unit, e.g. "-log-level debug" for a VM test.
  riverFlags ? "",
  # The usage log (newm-river's `telemetry` feature), for tuning placement; off unless asked for.
  withTelemetry ? false,
}:
let
  version = "0.6.0-dev";

  # river sees everything but newm/ and nix/, the window manager newm/ (without its scripts and
  # README) and protocol/: an edit to a VM test or to the WM rebuilds neither river nor the other.
  rel = path: lib.removePrefix (toString newmSrc + "/") (toString path);
  junk = path: builtins.elem (baseNameOf path) [ "target" "zig-out" ".zig-cache" "result" ".agent-work" ];
  riverSrc = lib.cleanSourceWith {
    src = newmSrc;
    name = "river-src";
    filter = path: _: !junk path && !builtins.elem (rel path) [ "newm" "nix" ];
  };
  wmSrc = lib.cleanSourceWith {
    src = newmSrc;
    name = "newm-src";
    filter =
      path: _:
      let
        r = rel path;
      in
      !junk path
      && (r == "newm" || lib.hasPrefix "newm/" r || r == "protocol" || lib.hasPrefix "protocol/" r)
      && !builtins.elem r [ "newm/e2e" "newm/README.md" ];
  };

  # The fork's build.zig.zon pins the deps nixpkgs' river pins, so its zig deps (fetched once,
  # in the store) serve; a new fetchDeps would download from Codeberg, which hangs.
  # river renders through scenefx (rounded corners, shadows, snapshots: render v2), linked
  # before wlroots so its wlr_scene_* symbols win; build.zig finds it with pkg-config.
  # One patched wlroots for river and scenefx (nix/wlroots.nix).
  w = import ./wlroots.nix { inherit wlroots_0_20 scenefx; };
  river' = (river.override { wlroots_0_20 = w.wlroots; }).overrideAttrs (old: {
    inherit version;
    src = riverSrc;
    inherit (river) zigDeps;
    buildInputs = old.buildInputs ++ [ w.scenefx' ];
    postInstall = "";
  });

  wm = rustPlatform.buildRustPackage {
    pname = "newm-river-wm";
    version = "26.4.0";
    src = wmSrc;
    cargoRoot = "newm";
    buildAndTestSubdir = "newm";
    cargoLock = {
      lockFile = newmSrc + "/newm/Cargo.lock";
      outputHashes."smithay-0.7.0" = "sha256-sUyyYEcnTPPXykadHMBBvDvioXGFRxldezBTPI3Nz+s=";
    };
    # pipewire: Spectacle's recording (zkde-screencast) streams through it; libspa-sys runs bindgen.
    nativeBuildInputs = [
      pkg-config
      rustPlatform.bindgenHook
    ];
    buildInputs = [
      libinput
      libxkbcommon
      linux-pam
      pipewire
      wayland
    ];
    # The VM tests need the e2e test client and the feedback probe too; the unit tests run in the
    # dev shell.
    cargoBuildFlags = [
      "--bins"
      "--example"
      "e2e_client"
      "--example"
      "feedback_probe"
    ];
    buildFeatures = lib.optional withTelemetry "newm-river/telemetry";
    # The synth background's GPU path dlopens libEGL when it first draws (not at start): glvnd's,
    # which finds the system's driver under /run/opengl-driver as river's own does.
    env.NEWM_RIVER_LIBEGL = "${libglvnd}/lib/libEGL.so.1";
    doCheck = false;
    postInstall = ''
      for e in e2e_client feedback_probe; do
        install -Dm755 "$(find . -path "*/release/examples/$e" -type f | head -n 1)" $out/bin/$e
      done
    '';
  };
in
runCommand "newm-river-${version}"
  {
    passthru = {
      river = river';
      inherit wm;
      providedSessions = [ "newm-river" ];
    };
    meta.mainProgram = "newm-river";
  }
  ''
    mkdir -p $out/bin $out/libexec $out/lib/systemd/user $out/share/wayland-sessions
    ln -s ${river'}/bin/river $out/bin/river
    for b in newm-river newm-river-lock e2e_client feedback_probe; do ln -s ${wm}/bin/$b $out/bin/$b; done

    cat > $out/libexec/newm-river-init <<'EOF'
    #!/bin/sh
    export NIRI_SOCKET="$XDG_RUNTIME_DIR/newm-river.$WAYLAND_DISPLAY.sock"
    dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY NIRI_SOCKET XDG_CURRENT_DESKTOP XDG_SESSION_TYPE
    @wm@/bin/newm-river &
    wm=$!
    i=0
    while [ ! -S "$NIRI_SOCKET" ] && [ "$i" -lt 50 ] && kill -0 "$wm" 2>/dev/null; do
      sleep 0.1
      i=$((i + 1))
    done
    systemd-notify --ready
    wait "$wm"
    rc=$?
    systemctl --user --no-block stop newm-river.service
    exit "$rc"
    EOF
    substituteInPlace $out/libexec/newm-river-init --replace-fail @wm@ ${wm}
    chmod +x $out/libexec/newm-river-init

    cat > $out/bin/newm-river-session <<'EOF'
    #!/bin/sh
    if [ -n "$SHELL" ] &&
       grep -q "$SHELL" /etc/shells &&
       ! (echo "$SHELL" | grep -q "false") &&
       ! (echo "$SHELL" | grep -q "nologin"); then
      if [ "$1" != '-l' ]; then
        exec "$SHELL" -l -c "exec $0 -l $*"
      else
        shift
      fi
    fi
    if systemctl --user -q is-active newm-river.service; then
      echo 'A newm-river session is already running.'
      exit 1
    fi
    systemctl --user reset-failed
    dbus-update-activation-environment --systemd --all
    trap "systemctl --user stop newm-river.service" HUP TERM
    systemctl --user --wait start newm-river.service
    systemctl --user start --job-mode=replace-irreversibly newm-river-shutdown.target
    systemctl --user unset-environment WAYLAND_DISPLAY DISPLAY XDG_SESSION_TYPE XDG_CURRENT_DESKTOP NIRI_SOCKET
    EOF
    chmod +x $out/bin/newm-river-session
    patchShebangs $out/bin/newm-river-session $out/libexec/newm-river-init

    cat > $out/lib/systemd/user/newm-river.service <<EOF
    [Unit]
    Description=newm-river, newm on river
    BindsTo=graphical-session.target
    Before=graphical-session.target
    Wants=graphical-session-pre.target
    After=graphical-session-pre.target
    Wants=xdg-desktop-autostart.target
    Before=xdg-desktop-autostart.target

    [Service]
    X-RestartIfChanged=false
    Slice=session.slice
    Type=notify
    NotifyAccess=all
    ExecStart=${river'}/bin/river ${riverFlags} -c $out/libexec/newm-river-init
    EOF

    cat > $out/lib/systemd/user/newm-river-shutdown.target <<EOF
    [Unit]
    Description=Shutdown running newm-river session
    DefaultDependencies=no
    StopWhenUnneeded=true
    Conflicts=graphical-session.target graphical-session-pre.target
    After=graphical-session.target graphical-session-pre.target
    EOF

    cat > $out/share/wayland-sessions/newm-river.desktop <<EOF
    [Desktop Entry]
    Name=newm-river
    Comment=newm's tile canvas on river
    Exec=$out/bin/newm-river-session
    Type=Application
    DesktopNames=river;KDE
    EOF
  ''
