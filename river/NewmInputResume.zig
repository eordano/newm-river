// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm: makes wlroots' libinput backend read the devices it adds back when the session comes
//! back from a VT switch. wlroots 0.20 calls libinput_resume() on the session's active signal and
//! leaves the device-added events in libinput's queue until libinput's fd next turns readable,
//! that is until the first key or motion. Switching back with Ctrl+Alt+Fn makes key releases
//! that read them at once; a switch back with no input (chvt, `loginctl activate`, a display
//! manager handing the seat back) left river with no keyboard or pointer until the first key,
//! which then went to clients that had no wl_keyboard yet: the first keystroke was lost.
//! This reads libinput right after resuming it. TODO(wlroots): dispatch in the libinput
//! backend's session_signal, as backend_start already does.

const wl = @import("wayland").server.wl;
const wlr = @import("wlroots");

const server = &@import("main.zig").server;

/// The head of wlroots 0.20's internal `struct wlr_libinput_backend`
/// (include/backend/libinput.h): only these leading fields are read, up to the event source of
/// libinput's fd. wlr_backend and wlr_session are public types.
const LibinputBackendHead = extern struct {
    backend: wlr.Backend,
    session: *wlr.Session,
    libinput_context: *anyopaque,
    input_event: ?*wl.EventSource,
};

extern fn wlr_backend_is_libinput(backend: *wlr.Backend) bool;

var active: wl.Listener(void) = .init(handleActive);

/// Listens to the session's active signal after the backends did (wlroots added its own
/// listener when the session was created), so libinput is resumed before it is read.
pub fn init(session: *wlr.Session) void {
    session.events.active.add(&active);
}

pub fn deinit() void {
    active.link.remove();
}

fn handleActive(_: *wl.Listener(void)) void {
    const session = server.session orelse return;
    if (!session.active) return;
    var found: u32 = 0;
    server.backend.multiForEachBackend(*u32, checkLibinput, &found);
}

/// Has the event loop dispatch libinput's fd source on its next turn, as if it were readable:
/// wlroots' handler reads libinput and handles the queued device-added events.
fn checkLibinput(backend: *wlr.Backend, found: *u32) void {
    if (!wlr_backend_is_libinput(backend)) return;
    const head: *LibinputBackendHead = @ptrCast(@alignCast(backend));
    if (head.input_event) |source| {
        source.check();
        found.* += 1;
    }
}
