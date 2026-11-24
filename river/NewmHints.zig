// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: xdg-system-bell-v1 and xdg-dialog-v1 for clients, and the newm_river_hints_v1
//! global (protocol/newm-river-hints-v1.xml) that passes the window manager the rings and the
//! modality it acts on. zig-wlroots 0.20 binds neither wlroots type, so the structs below mirror
//! wlr/types/wlr_xdg_system_bell_v1.h and wlr_xdg_dialog_v1.h up to their public fields (river
//! only ever holds pointers to them).

const NewmHints = @This();

const std = @import("std");
const wlr = @import("wlroots");
const wayland = @import("wayland");
const wl = wayland.server.wl;
const newm = wayland.server.newm;
const river = wayland.server.river;

const server = &@import("main.zig").server;
const util = @import("util.zig");

const SceneNodeData = @import("SceneNodeData.zig");

const log = std.log.scoped(.wm);

/// struct wlr_xdg_system_bell_v1
pub const SystemBell = extern struct {
    global: *wl.Global,
    events: extern struct {
        destroy: wl.Signal(void),
        ring: wl.Signal(*RingEvent),
    },

    /// struct wlr_xdg_system_bell_v1_ring_event
    pub const RingEvent = extern struct {
        client: *wl.Client,
        surface: ?*wlr.Surface,
    };

    extern fn wlr_xdg_system_bell_v1_create(server: *wl.Server, version: u32) ?*SystemBell;
};

/// struct wlr_xdg_dialog_v1
pub const Dialog = extern struct {
    resource: *wl.Resource,
    xdg_toplevel: *wlr.XdgToplevel,
    modal: bool,
    events: extern struct {
        destroy: wl.Signal(void),
        set_modal: wl.Signal(void),
    },

    extern fn wlr_xdg_dialog_v1_try_from_wlr_xdg_toplevel(toplevel: *wlr.XdgToplevel) ?*Dialog;
    pub const tryFromToplevel = wlr_xdg_dialog_v1_try_from_wlr_xdg_toplevel;
};

/// struct wlr_xdg_wm_dialog_v1
pub const WmDialog = extern struct {
    global: *wl.Global,
    events: extern struct {
        destroy: wl.Signal(void),
        new_dialog: wl.Signal(*Dialog),
    },

    extern fn wlr_xdg_wm_dialog_v1_create(server: *wl.Server, version: u32) ?*WmDialog;
};

/// Listens to one client's dialog: a change of modality reaches the window manager in the next
/// manage sequence (Window.zig compares Window.newmModal with what it sent).
const DialogWatch = struct {
    set_modal: wl.Listener(void) = .init(handleSetModal),
    destroy: wl.Listener(void) = .init(handleDialogDestroy),

    fn handleSetModal(_: *wl.Listener(void)) void {
        server.wm.dirtyWindowing();
    }

    fn handleDialogDestroy(listener: *wl.Listener(void)) void {
        const watch: *DialogWatch = @fieldParentPtr("destroy", listener);
        watch.set_modal.link.remove();
        watch.destroy.link.remove();
        util.gpa.destroy(watch);
        server.wm.dirtyWindowing();
    }
};

global: *wl.Global,
bell: *SystemBell,
wm_dialog: *WmDialog,
/// newm_river_hints_v1 objects; events go to the ones of the window's own client.
resources: std.ArrayList(*newm.RiverHintsV1) = .empty,
ring: wl.Listener(*SystemBell.RingEvent) = .init(handleRing),
new_dialog: wl.Listener(*Dialog) = .init(handleNewDialog),
server_destroy: wl.Listener(*wl.Server) = .init(handleServerDestroy),

pub fn init(hints: *NewmHints) !void {
    hints.* = .{
        .global = try wl.Global.create(server.wl_server, newm.RiverHintsV1, 2, ?*anyopaque, null, bind),
        .bell = SystemBell.wlr_xdg_system_bell_v1_create(server.wl_server, 1) orelse return error.OutOfMemory,
        .wm_dialog = WmDialog.wlr_xdg_wm_dialog_v1_create(server.wl_server, 1) orelse return error.OutOfMemory,
    };
    hints.bell.events.ring.add(&hints.ring);
    hints.wm_dialog.events.new_dialog.add(&hints.new_dialog);
    server.wl_server.addDestroyListener(&hints.server_destroy);
}

fn handleServerDestroy(listener: *wl.Listener(*wl.Server), _: *wl.Server) void {
    const hints: *NewmHints = @fieldParentPtr("server_destroy", listener);
    hints.ring.link.remove();
    hints.new_dialog.link.remove();
    hints.global.destroy();
    hints.resources.deinit(util.gpa);
}

fn handleRing(listener: *wl.Listener(*SystemBell.RingEvent), event: *SystemBell.RingEvent) void {
    const hints: *NewmHints = @fieldParentPtr("ring", listener);
    const surface = event.surface orelse return;
    const node_data = SceneNodeData.fromSurface(surface) orelse return;
    switch (node_data.data) {
        .window => |window| {
            const window_v1 = window.object orelse return;
            log.debug("a window rang the system bell", .{});
            for (hints.resources.items) |resource| {
                if (resource.getClient() == window_v1.getClient()) resource.sendBell(window_v1);
            }
        },
        else => {},
    }
}

fn handleNewDialog(_: *wl.Listener(*Dialog), dialog: *Dialog) void {
    const watch = util.gpa.create(DialogWatch) catch {
        log.err("out of memory", .{});
        return;
    };
    watch.* = .{};
    dialog.events.set_modal.add(&watch.set_modal);
    dialog.events.destroy.add(&watch.destroy);
    server.wm.dirtyWindowing();
}

/// Whether the toplevel is a modal dialog (xdg_dialog_v1.set_modal).
pub fn toplevelModal(wlr_toplevel: *wlr.XdgToplevel) bool {
    const dialog = Dialog.tryFromToplevel(wlr_toplevel) orelse return false;
    return dialog.modal;
}

/// Tells the window manager owning `window_v1` that a client asked to activate the window
/// (version 2; a window manager bound at version 1 hears nothing, as on upstream river).
pub fn sendActivate(hints: *NewmHints, window_v1: *river.WindowV1, source: newm.RiverHintsV1.Source) void {
    for (hints.resources.items) |resource| {
        if (resource.getClient() == window_v1.getClient() and resource.getVersion() >= 2) {
            resource.sendActivate(window_v1, source);
        }
    }
}

/// Tells the window manager owning `window_v1` the window's modality.
pub fn sendModal(hints: *NewmHints, window_v1: *river.WindowV1, modal: bool) void {
    for (hints.resources.items) |resource| {
        if (resource.getClient() == window_v1.getClient()) resource.sendModal(window_v1, @intFromBool(modal));
    }
}

fn bind(client: *wl.Client, _: ?*anyopaque, version: u32, id: u32) void {
    const object = newm.RiverHintsV1.create(client, version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    server.newm_hints.resources.append(util.gpa, object) catch {
        object.destroy();
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    object.setHandler(?*anyopaque, handleRequest, handleResourceDestroy, null);
}

fn handleRequest(object: *newm.RiverHintsV1, request: newm.RiverHintsV1.Request, _: ?*anyopaque) void {
    switch (request) {
        .destroy => object.destroy(),
    }
}

fn handleResourceDestroy(object: *newm.RiverHintsV1, _: ?*anyopaque) void {
    const list = &server.newm_hints.resources;
    for (list.items, 0..) |resource, i| {
        if (resource == object) {
            _ = list.swapRemove(i);
            return;
        }
    }
}
