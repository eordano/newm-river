# wlroots 0.20 with newm-river's patches, and scenefx built against it: river links both, so they
# must share one wlroots. nix/package.nix, the dev shell and a downstream package use it.
#
# wlroots-output-management-virtual-mode.patch: a head without a fixed mode (headless, nested
# Wayland/X11 outputs) is advertised with a virtual mode only while enabled, and wlroots sent the
# virtual mode on re-enable only when no client at all held one. A client that bound while the
# head was off (wlr-randr --on) then had none, and head_send_state's assertion ended the session.
#
# wlroots-dmabuf-feedback-reuse.patch: wlr_linux_dmabuf_v1_set_surface_feedback sends nothing when
# the compiled feedback equals the one the surface's feedback objects last got (the default one
# included), and a format table equal to that one shares its shm file instead of a new one.
#
# scenefx-dmabuf-feedback-keep-hidden.patch: a scene buffer hidden and shown again on the same
# output keeps the dmabuf feedback it was last sent, as wlroots' own scene does. river disables a
# window's trees around each render sequence and NewmScale around each commit of a scaled, rounded
# or shadowed window; unpatched scenefx forgets the feedback at each toggle and sends an identical
# one with a new format table fd per commit and per animation frame, and Firefox, which keeps
# those fds, crashes at RLIMIT_NOFILE.
{ wlroots_0_20, scenefx }:
rec {
  wlroots = wlroots_0_20.overrideAttrs (old: {
    patches = (old.patches or [ ]) ++ [
      ./wlroots-output-management-virtual-mode.patch
      ./wlroots-dmabuf-feedback-reuse.patch
    ];
  });
  scenefx' = (scenefx.override { wlroots_0_20 = wlroots; }).overrideAttrs (old: {
    patches = (old.patches or [ ]) ++ [ ./scenefx-dmabuf-feedback-keep-hidden.patch ];
  });
}
