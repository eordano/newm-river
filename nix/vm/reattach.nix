# vm-reattach's nodes. machine is set up as a desktop: Ghostty resident as the systemd user unit
# graphical-session.target wants, running fish, tmux titles and fish_title that name the host
# and the window, a tmux server per user as a systemd user service and ssh multiplexed for every
# host. far is a remote host reached over ssh, as a build or file server: sshd taking alice's
# key only, fish, the same tmux, alice lingering. alice's key lives in an ssh-agent that asks to
# confirm every use (ssh-add -c) through /etc/askpass-count, which counts the asks: a YubiKey's
# touches. foot (a process per window, its own launch form) runs fish too, for its window's local
# tmux session.
{ pkgs }:
let
  keys = import "${pkgs.path}/nixos/tests/ssh-keys.nix" pkgs;

  shared = {
    # tmux and ssh in Ghostty need its terminfo, here and over there.
    environment.systemPackages = [ pkgs.ghostty.terminfo ];
    programs.fish.enable = true;
    environment.etc."fish/functions/fish_title.fish".text = ''
      function fish_title
          set -l cmd $argv[1]
          if not set -q argv[1]
              set cmd (status current-command)
              test "$cmd" = fish; and set cmd
          end
          set -l host
          if set -q SSH_TTY; and not set -q TMUX
              set host (prompt_hostname):
          end
          echo -- $host (string sub -l 40 -- $cmd) (prompt_pwd)
      end
    '';
    programs.tmux = {
      enable = true;
      extraConfig = ''
        set -g set-titles on
        set -g set-titles-string "#{host_short}: #{?#{==:#{pane_title},#{host}},#{pane_current_command},#{pane_title}}"
        set -g base-index 1
        setw -g pane-base-index 1
        set -g default-shell ${pkgs.fish}/bin/fish
      '';
    };
  };

  ghosttyConfig = pkgs.writeText "ghostty-config" ''
    command = ${pkgs.fish}/bin/fish
    shell-integration-features = sudo
    confirm-close-surface = false
    window-decoration = true
  '';

  footConfig = pkgs.writeText "foot.ini" ''
    [main]
    shell=${pkgs.fish}/bin/fish
  '';

  askpass = pkgs.writeShellScript "askpass-count" ''
    echo "$(date +%s.%N) $*" >> /tmp/askpass.log
  '';
in
{
  machine =
    { config, ... }:
    {
      imports = [ shared ];
      environment.systemPackages = [
        pkgs.ghostty
        pkgs.foot
        pkgs.foot.terminfo
      ];
      systemd.packages = [ pkgs.ghostty ];
      services.dbus.packages = [ pkgs.ghostty ];
      systemd.user.targets.graphical-session.wants = [ "app-com.mitchellh.ghostty.service" ];
      # virtio-gpu has no 3D here: Mesa's llvmpipe gives Ghostty its OpenGL. No default PATH:
      # Ghostty's shells take the user manager's, as with the unit home-manager installs on a desktop.
      systemd.user.services."app-com.mitchellh.ghostty" = {
        enableDefaultPath = false;
        environment.LIBGL_ALWAYS_SOFTWARE = "1";
      };

      systemd.user.services.tmux = {
        description = "tmux server outside every login session";
        wantedBy = [ "default.target" ];
        restartIfChanged = false;
        enableDefaultPath = false;
        unitConfig.ConditionUser = "!@system";
        environment.TMUX_TMPDIR = "%t";
        serviceConfig = {
          ExecCondition = "${pkgs.bash}/bin/bash -c '! ${config.programs.tmux.package}/bin/tmux list-sessions'";
          ExecStart = "${config.programs.tmux.package}/bin/tmux -D";
          Restart = "always";
          RestartSec = "1s";
        };
      };

      programs.ssh.extraConfig = ''
        Host far
          IdentityAgent /tmp/alice-agent.sock
          StrictHostKeyChecking accept-new
        Host *
          ControlMaster auto
          ControlPath ~/.ssh/cm-%C
          ControlPersist 4h
      '';
      environment.etc = {
        "askpass-count".source = askpass;
        "alice-far-key" = {
          source = keys.snakeOilPrivateKey;
          mode = "0400";
        };
      };
      systemd.tmpfiles.rules = [
        "d /home/alice/.ssh 0700 alice users -"
        "d /home/alice/.config/ghostty 0755 alice users -"
        "C /home/alice/.config/ghostty/config 0644 alice users - ${ghosttyConfig}"
        "d /home/alice/.config/foot 0755 alice users -"
        "C /home/alice/.config/foot/foot.ini 0644 alice users - ${footConfig}"
      ];
    };

  nodes.far = {
    imports = [ shared ];
    virtualisation.memorySize = 1024;
    services.openssh = {
      enable = true;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
      };
    };
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
      shell = pkgs.fish;
      linger = true;
      openssh.authorizedKeys.keys = [ keys.snakeOilPublicKey ];
    };
  };
}
