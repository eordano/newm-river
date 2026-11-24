{
  description = "newm-river: river (a fork of main) with newm's strict tile canvas as its window manager";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
      compat =
        strict:
        let
          pkgs = nixpkgs.legacyPackages.x86_64-linux;
          compositors = import ./newm/compat/compositors.nix { inherit pkgs; };
        in
        import ./newm/compat {
          inherit pkgs strict;
          compositor = compositors.newm-river (pkgs.callPackage ./nix/package.nix { });
        };
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.callPackage ./nix/package.nix { };
      });

      # Live tests in a NixOS VM (nix/vm): river on DRM, uinput devices. newm/e2e/vm.sh runs all.
      # vm-input reads the usage log, which only a telemetry build writes.
      checks.x86_64-linux =
        import ./nix/vm {
          pkgs = nixpkgs.legacyPackages.x86_64-linux;
          newm-river = nixpkgs.legacyPackages.x86_64-linux.callPackage ./nix/package.nix {
            withTelemetry = true;
          };
        }
        // compat true;
      legacyPackages.x86_64-linux.vm-report = import ./nix/vm {
        pkgs = nixpkgs.legacyPackages.x86_64-linux;
        newm-river = nixpkgs.legacyPackages.x86_64-linux.callPackage ./nix/package.nix {
          withTelemetry = true;
        };
        strict = false;
      };
      legacyPackages.x86_64-linux.compat-report = compat false;

      devShells = forAll (
        pkgs:
        let
          w = import ./nix/wlroots.nix { inherit (pkgs) wlroots_0_20 scenefx; };
        in
        {
        default = pkgs.mkShell {
          inputsFrom = [ (pkgs.river.override { wlroots_0_20 = w.wlroots; }) ];
          packages = with pkgs; [
            cargo
            clippy
            rustc
            rustfmt
            pkg-config
            w.scenefx'
            rustPlatform.bindgenHook
            dbus
            libinput
            libxkbcommon
            linux-pam
            seatd
            systemdLibs
            wayland
            wayland-scanner
            foot
            grim
            wlr-randr
            pipewire
            wireplumber
          ];
          # newm/e2e/screencast.sh runs the ScreenCast portal against river.
          XDPW = "${pkgs.xdg-desktop-portal-wlr}/libexec/xdg-desktop-portal-wlr";
          # The synth background's GPU path dlopens this libEGL (nix/package.nix does the same).
          NEWM_RIVER_LIBEGL = "${pkgs.libglvnd}/lib/libEGL.so.1";
        };
        }
      );
    };
}
