# The VM session's config.kdl: a desktop's newm-river config with
# the test clients in place of the desktop's: an e2e_client layer surface stands in for its 30 px
# bar, swaybg paints a solid wallpaper.
{ pkgs, newm-river }:
let
  lock = "${newm-river}/bin/newm-river-lock --daemonize --bg '#101018' --fg '#e0e0e0' --accent '#82fb9c'";
  idle = builtins.concatStringsSep " " [
    "${pkgs.swayidle}/bin/swayidle -w"
    "timeout 600 \\\"${lock}\\\""
    "before-sleep \\\"${lock}\\\""
    "lock \\\"${lock}\\\""
    "timeout 900 \\\"${pkgs.wlopm}/bin/wlopm --off '*'\\\""
    "resume \\\"${pkgs.wlopm}/bin/wlopm --on '*'\\\""
  ];
in
pkgs.writeText "newm-river-vm.kdl" ''
  prefer-no-csd
  environment {
      XDG_CURRENT_DESKTOP "river:KDE"
  }
  input {
      keyboard {
          xkb {
              layout "us"
          }
          repeat-delay 200
          repeat-rate 60
      }
      touchpad {
          tap
          scroll-factor 0.5
      }
      mouse {
          accel-profile "flat"
          scroll-method "on-button-down"
          scroll-button 274
      }
  }
  layout {
      gaps 10
      background-color "transparent"
      focus-ring {
          off
      }
      border {
          width 1
          active-color "#82FB9C8C"
          inactive-color "transparent"
      }
      shadow {
          on
          softness 26
          spread 0
          offset x=0 y=0
          color "#82FB9C36"
          inactive-color "transparent"
      }
      canvas {
          tile-width 119
          tile-height 113
          zoom-step 0.8
          zoom-max 7
          super-tap-overview
          center-on-switch
          default-width { fixed 960; }
          default-height { proportion 0.666700; }
      }
  }
  window-rule {
      match app-id="(?i)ksshaskpass"
      match app-id="(?i)krunner"
      open-floating true
  }
  window-rule {
      geometry-corner-radius 8
      clip-to-geometry true
  }
  window-rule {
      match is-focused=false
      opacity 0.9
  }
  spawn-at-startup "${pkgs.swaybg}/bin/swaybg" "-c" "#203040"
  spawn-at-startup "${newm-river}/bin/e2e_client" "layer" "--namespace" "bar" "--anchor" "top" "--thickness" "30" "--exclusive" "30" "--color" "303030"
  spawn-sh-at-startup "${idle}"
  binds {
      Mod+Return { spawn "foot"; }
      Mod+q { quit-app; }
      Alt+q { close-window; }
      Mod+f { fullscreen-window; }
      Mod+t { toggle-window-floating; }
      Mod+h { focus-column-left; }
      Mod+j { focus-window-down; }
      Mod+k { focus-window-up; }
      Mod+l { focus-column-right; }
      Mod+Shift+h { move-column-left; }
      Mod+Shift+j { move-window-down; }
      Mod+Shift+k { move-window-up; }
      Mod+Shift+l { move-column-right; }
      Mod+u { zoom-out; }
      Mod+n { zoom-in; }
      Mod+Shift+p { quit skip-confirmation=true; }
      Ctrl+Alt+L { spawn-sh "loginctl lock-session"; }
      Mod+Escape { spawn-sh "loginctl lock-session"; }
      Mod+Ctrl+H { swap-neighbour "left"; }
      Mod+Ctrl+J { swap-neighbour "down"; }
      Mod+Ctrl+K { swap-neighbour "up"; }
      Mod+Ctrl+L { swap-neighbour "right"; }
      Mod+WheelScrollUp { zoom-in; }
      Mod+WheelScrollDown { zoom-out; }
  }
''
