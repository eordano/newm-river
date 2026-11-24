// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: keyboard-shortcuts-inhibit-unstable-v1 for clients, and the
//! newm_river_shortcuts_v1 global (protocol/newm-river-shortcuts-v1.xml) that lets the window
//! manager keep some bindings working while a client inhibits shortcuts. Like niri, river activates
//! every inhibitor when it is created; KeyboardGroup.handleKey asks `inhibits` before matching.

const NewmShortcuts = @This();

const std = @import("std");
const wlr = @import("wlroots");
const wayland = @import("wayland");
const wl = wayland.server.wl;
const newm = wayland.server.newm;

const server = &@import("main.zig").server;

const Seat = @import("Seat.zig");
const XkbBinding = @import("XkbBinding.zig");

const log = std.log.scoped(.input);

global: *wl.Global,
wlr_manager: *wlr.KeyboardShortcutsInhibitManagerV1,
new_inhibitor: wl.Listener(*wlr.KeyboardShortcutsInhibitorV1) = .init(handleNewInhibitor),
server_destroy: wl.Listener(*wl.Server) = .init(handleServerDestroy),

pub fn init(shortcuts: *NewmShortcuts) !void {
    shortcuts.* = .{
        .global = try wl.Global.create(server.wl_server, newm.RiverShortcutsV1, 1, ?*anyopaque, null, bind),
        .wlr_manager = try wlr.KeyboardShortcutsInhibitManagerV1.create(server.wl_server),
    };
    shortcuts.wlr_manager.events.new_inhibitor.add(&shortcuts.new_inhibitor);
    server.wl_server.addDestroyListener(&shortcuts.server_destroy);
}

fn handleServerDestroy(listener: *wl.Listener(*wl.Server), _: *wl.Server) void {
    const shortcuts: *NewmShortcuts = @fieldParentPtr("server_destroy", listener);
    shortcuts.new_inhibitor.link.remove();
    shortcuts.global.destroy();
}

fn handleNewInhibitor(_: *wl.Listener(*wlr.KeyboardShortcutsInhibitorV1), inhibitor: *wlr.KeyboardShortcutsInhibitorV1) void {
    log.debug("activating a keyboard shortcuts inhibitor", .{});
    inhibitor.activate();
}

/// The inhibitor of the surface with the seat's keyboard focus, if any.
fn focusedInhibitor(shortcuts: *NewmShortcuts, seat: *Seat) ?*wlr.KeyboardShortcutsInhibitorV1 {
    const focused = seat.wlr_seat.keyboard_state.focused_surface orelse return null;
    var it = shortcuts.wlr_manager.inhibitors.iterator(.forward);
    while (it.next()) |inhibitor| {
        if (inhibitor.seat == seat.wlr_seat and inhibitor.surface == focused) return inhibitor;
    }
    return null;
}

/// Whether an active inhibitor on the seat's focused surface holds back inhibitable bindings.
pub fn inhibits(shortcuts: *NewmShortcuts, seat: *Seat) bool {
    const inhibitor = shortcuts.focusedInhibitor(seat) orelse return false;
    return inhibitor.active;
}

fn bind(client: *wl.Client, _: ?*anyopaque, version: u32, id: u32) void {
    const object = newm.RiverShortcutsV1.create(client, version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    object.setHandler(?*anyopaque, handleRequest, null, null);
}

fn handleRequest(
    object: *newm.RiverShortcutsV1,
    request: newm.RiverShortcutsV1.Request,
    _: ?*anyopaque,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .set_inhibitable => |args| {
            const data = args.binding.getUserData() orelse return;
            const binding: *XkbBinding = @ptrCast(@alignCast(data));
            binding.newm_inhibitable = args.inhibitable != 0;
        },
        .toggle_inhibit => |args| {
            const data = args.seat.getUserData() orelse return;
            const seat: *Seat = @ptrCast(@alignCast(data));
            const inhibitor = server.newm_shortcuts.focusedInhibitor(seat) orelse return;
            if (inhibitor.active) inhibitor.deactivate() else inhibitor.activate();
            log.debug("keyboard shortcuts inhibitor toggled: active {}", .{inhibitor.active});
        },
    }
}
