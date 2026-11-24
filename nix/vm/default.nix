# Live tests of newm-river in a NixOS VM: river on a virtio GPU through its DRM backend, started
# by a TTY autologin the way a NixOS session starts it (newm-river-session -> the systemd user
# unit -> river -c newm-river-init), driven through uinput devices (nix/vm/uin.py: keyboard,
# absolute and relative mice, a multitouch clickpad) that libinput takes for hardware.
#
#   vm-input    gestures, pinch zoom, Super tap, Super+wheel zoom, two-finger scroll and pinch,
#               Alt+Tab, zoomed clicks, middle-button scrolling, config reload
#   vm-lock     newm-river-lock through loginctl and swayidle, PAM, ScreenShot2 refused, wlopm
#   vm-session  TTY detection, Xwayland, a WM crash restarted, session restore across a logout
#               and login, HiDPI, two outputs, logout through IPC
#   vm-reattach Ghostty windows come back in their tmux session or ssh across two logouts, with
#               real Ghostty, tmux and ssh to a second VM (nix/vm/reattach.nix)
#   vm-browser  Chromium's two windows come back with their tabs on their saved spots across a
#               logout, relaunched with --restore-last-session (nix/vm/browser.nix)
#
# `checks.<system>.vm-*` fail on any failure but the expected ones (xfail.nix); the
# `legacyPackages.<system>.vm-report.vm-*` variants never fail, so their screenshots survive a
# failure; newm/e2e/vm.sh runs those one at a time and sums them up.
{
  pkgs,
  newm-river,
  strict ? true,
}:
let
  inherit (pkgs) lib;

  uin = pkgs.writeScriptBin "uin" (
    "#!${pkgs.python3.withPackages (p: [ p.evdev ])}/bin/python3\n" + builtins.readFile ./uin.py
  );

  password = "correct-horse-7";

  asAlice = pkgs.writeShellScriptBin "as-alice" ''
    # Runs one shell command as alice inside her graphical session's environment.
    exec ${pkgs.util-linux}/bin/runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 \
      DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus sh -c '
        eval "$(systemctl --user show-environment \
          | grep -E "^(WAYLAND_DISPLAY|DISPLAY|NIRI_SOCKET|XDG_SESSION_ID|PATH)=" \
          | sed "s/^/export /")"
        exec sh -c "$1"' sh "$1"
  '';

  config = import ./config.nix { inherit pkgs newm-river; };

  guest =
    {
      heads,
      mode,
      riverDebug,
    }:
    { ... }:
    let
      # river's and wlroots' debug log, for a case that fails inside river.
      newm-river' = if riverDebug then newm-river.override { riverFlags = "-log-level debug"; } else newm-river;
    in
    {
      virtualisation = {
        memorySize = 3072;
        cores = 4;
        qemu.options = [
          "-vga none"
          "-device virtio-gpu-pci,max_outputs=${toString heads},xres=${mode.x},yres=${mode.y}"
        ];
      };
      hardware.uinput.enable = true;
      hardware.graphics.enable = true;
      fonts.enableDefaultPackages = true;

      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
        inherit password;
      };
      services.getty.autologinUser = "alice";
      # tty1's login shell runs the session as sddm's Exec= would; its end logs out and getty
      # logs in again. /run/newm-hold holds the next login back while a test edits the config.
      environment.loginShellInit = ''
        if [ "$(tty)" = /dev/tty1 ] && [ -z "$NEWM_VM_SESSION" ]; then
          export NEWM_VM_SESSION=1
          while [ -e /run/newm-hold ]; do sleep 0.2; done
          [ -f "$HOME/.config/newm-vm/env" ] && . "$HOME/.config/newm-vm/env"
          ${newm-river'}/bin/newm-river-session
          exit
        fi
      '';
      environment.sessionVariables = {
        RUST_LOG = "newm_river=debug";
        WLR_RENDERER = "pixman";
        XDG_CURRENT_DESKTOP = "river:KDE";
      };

      systemd.packages = [ newm-river' ];
      systemd.tmpfiles.rules = [
        "d /home/alice/.config 0755 alice users -"
        "d /home/alice/.config/newm-river 0755 alice users -"
        "d /home/alice/.config/newm-vm 0755 alice users -"
        "C /home/alice/.config/newm-river/config.kdl 0644 alice users - ${config}"
        "d /tmp/shots 1777 root root -"
      ];
      security.pam.services.newm-lock = { };

      systemd.services.uin = {
        description = "uinput devices for the tests";
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${uin}/bin/uin serve";
      };

      environment.systemPackages = [
        newm-river'
        uin
        asAlice
        pkgs.dbus
        pkgs.foot
        pkgs.xterm
        pkgs.wev
        pkgs.grim
        pkgs.jq
        pkgs.swaybg
        pkgs.swayidle
        pkgs.wlopm
        pkgs.wlr-randr
        pkgs.wayland-utils
        pkgs.libinput
      ];
    };

  xfail = import ./xfail.nix;

  test =
    name:
    {
      heads ? 1,
      riverDebug ? false,
      # Wide like a 4086x1100 desktop: two 960 px windows side by side, none stacked.
      mode ? {
        x = "2560";
        y = "1080";
      },
      # More of machine's config, and other nodes.
      machine ? { },
      nodes ? { },
    }:
    pkgs.testers.runNixOSTest {
      name = "newm-river-${name}${lib.optionalString (!strict) "-report"}";
      nodes = {
        machine = {
          imports = [
            (guest { inherit heads mode riverDebug; })
            machine
          ];
        };
      }
      // nodes;
      extraPythonPackages = p: [ p.pillow ];
      skipTypeCheck = true;
      skipLint = true;
      testScript = ''
        TEST = ${builtins.toJSON name}
        XFAIL = ${builtins.toJSON xfail}
        STRICT = ${if strict then "True" else "False"}
        PASSWORD = ${builtins.toJSON password}
      ''
      + builtins.readFile ./lib.py
      + builtins.readFile (./. + "/${name}.py");
    };
in
{
  vm-input = test "input" { };
  vm-lock = test "lock" { riverDebug = true; };
  vm-session = test "session" { heads = 2; };
  vm-reattach = test "reattach" (import ./reattach.nix { inherit pkgs; });
  vm-browser = test "browser" (import ./browser.nix { inherit pkgs; });
}
