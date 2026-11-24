# vm-browser's machine: Chromium from nixpkgs launched as a desktop entry the user overrides
# (~/.local/share/applications/chromium-browser.desktop, which newm-river reads before the system
# one), so the saved launch is that entry's bare Exec, as a desktop's `chromium` is. Its own profile
# opens one fixed startup page, as a desktop profile can: started bare it shows that page,
# not the windows it had. Native Wayland, software rendering, and DevTools on 127.0.0.1:9222 for
# the test to read each window's tabs (cdp-tabs). The pages are local files under /etc.
{ pkgs }:
let
  inherit (pkgs) lib;

  flags = [
    "--user-data-dir=/home/alice/chromium"
    "--no-first-run"
    "--no-default-browser-check"
    "--password-store=basic"
    "--ozone-platform=wayland"
    "--disable-gpu"
    "--remote-debugging-port=9222"
  ];

  desktop = pkgs.writeText "chromium-browser.desktop" ''
    [Desktop Entry]
    Type=Application
    Name=Chromium
    Exec=chromium ${lib.concatStringsSep " " flags} %U
    StartupWMClass=chromium-browser
  '';

  page = name: title: color: ''
    cat > $out/${name}.html <<'EOF'
    <!doctype html><title>${title}</title>
    <body style="background:${color}"><h1>${title}</h1></body>
    EOF
  '';

  pages = pkgs.runCommand "newm-pages" { } (
    "mkdir $out\n"
    + page "start" "Start" "#808080"
    + page "a1" "Alpha" "#c03030"
    + page "b1" "Beta one" "#3030c0"
    + page "b2" "Beta two" "#30c030"
  );

  preferences = pkgs.writeText "Preferences" (
    builtins.toJSON {
      session = {
        restore_on_startup = 4;
        startup_urls = [ "file:///etc/newm-pages/start.html" ];
      };
    }
  );

  cdpTabs = pkgs.writers.writePython3Bin "cdp-tabs" {
    libraries = [ pkgs.python3Packages.websockets ];
    doCheck = false;
  } (builtins.readFile ./cdp-tabs.py);
in
{
  machine = {
    virtualisation.memorySize = lib.mkForce 4096;
    environment.systemPackages = [
      pkgs.chromium
      cdpTabs
    ];
    environment.etc."newm-pages".source = pages;
    systemd.tmpfiles.rules = [
      "d /home/alice/.local 0755 alice users -"
      "d /home/alice/.local/share 0755 alice users -"
      "d /home/alice/.local/share/applications 0755 alice users -"
      "C /home/alice/.local/share/applications/chromium-browser.desktop 0644 alice users - ${desktop}"
      "d /home/alice/chromium 0700 alice users -"
      "d /home/alice/chromium/Default 0700 alice users -"
      "C /home/alice/chromium/Default/Preferences 0600 alice users - ${preferences}"
    ];
  };
}
