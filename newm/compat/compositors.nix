{ pkgs }:
{
  newm-river = package: {
    name = "newm-river";
    inherit package;
    session = "${package}/bin/newm-river-session";
    unit = "newm-river.service";
    ipc = "${package}/bin/newm-river";
    process = "river";
    desktop = "river:KDE";
    xwaylandSatellite = null;
    env = {
      RUST_LOG = "newm_river=debug";
    };
    # A nested session of river (its Wayland backend) inside the VM's session, and nixpkgs' niri
    # as a second nested compositor.
    nested = "river -c newm-river";
    nestedOthers = [ "niri" ];
    vmKdl = "";
    xfail = {
      two-outputs = "the software-only second GPU cannot import river's buffers (eglCreateImageKHR: createImageFromDmaBufs failed), so river leaves its output off (it used to exit at the first modeset); dual-GPU hosts with hardware EGL on both are not affected";
      output-off-on = "flaky (2 of 4 runs): typing reaches the window after the output returns, but it is not drawn and gets no wl_surface.enter";
      nested-river = "in the GPU-less VM, Mesa's software EGL cannot make GBM dumb buffers on the host's render node and scenefx has no pixman fallback";
      screencast-pipewire = "the portal's ScreenCast fails in the compat VM only: open";
      layer-keyboard-exclusive = "the GTK4 layer probe gets no wl_keyboard.enter in the VM; headless e2e focus.sh passes the same exclusive take and give-back (river-focus): open";
      layer-keyboard-on-demand = "a click on the on-demand bar gives it no typed text in the VM; headless focus.sh passes on-demand focus on map, IPC and click: open";
      dnd-x11-to-wayland = "a drag from an X11 client to a Wayland one fails on river's Xwayland: open";
    };
  };
}
