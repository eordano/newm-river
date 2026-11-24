// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: the newm_river_render_v1 global (protocol/newm-river-render-v1.xml), per-window
//! scale and opacity for the window manager. NewmScale.zig draws them.

const NewmRender = @This();

const std = @import("std");
const wayland = @import("wayland");
const wl = wayland.server.wl;
const newm = wayland.server.newm;

const server = &@import("main.zig").server;
const util = @import("util.zig");

const NewmFx = @import("NewmFx.zig");
const NewmSnapshot = @import("NewmSnapshot.zig");
const Window = @import("Window.zig");

const log = std.log.scoped(.wm);

global: *wl.Global,
/// Every newm_river_snapshot_v1, expired ones included.
snapshots: std.ArrayList(*NewmSnapshot) = .empty,
server_destroy: wl.Listener(*wl.Server) = .init(handleServerDestroy),

pub fn init(render: *NewmRender) !void {
    render.* = .{
        // Version 2 (corners, shadows, snapshots) needs scenefx.
        .global = try wl.Global.create(server.wl_server, newm.RiverRenderV1, if (NewmFx.enabled) 2 else 1, ?*anyopaque, null, bind),
    };
    server.wl_server.addDestroyListener(&render.server_destroy);
}

fn handleServerDestroy(listener: *wl.Listener(*wl.Server), _: *wl.Server) void {
    const render: *NewmRender = @fieldParentPtr("server_destroy", listener);
    render.global.destroy();
    render.snapshots.deinit(util.gpa);
}

/// The window manager's render_finish: apply the snapshots' state; after a restack keep them
/// above the windows.
pub fn renderFinish(render: *NewmRender, restacked: bool) void {
    for (render.snapshots.items) |snapshot| {
        snapshot.renderFinish();
        if (restacked) snapshot.raise();
    }
}

fn bind(client: *wl.Client, _: ?*anyopaque, version: u32, id: u32) void {
    const object = newm.RiverRenderV1.create(client, version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    object.setHandler(?*anyopaque, handleRequest, null, null);
}

fn handleRequest(
    object: *newm.RiverRenderV1,
    request: newm.RiverRenderV1.Request,
    _: ?*anyopaque,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .get_window_render => |args| {
            // river_window_v1 objects belong to the active window manager only and turn inert
            // when it goes, so only the active window manager reaches a window here.
            const window: ?*Window = if (args.window.getUserData()) |data| @ptrCast(@alignCast(data)) else null;
            if (window) |w| {
                if (w.newm.object != null) {
                    object.postError(.object_already_created, "newm_river_window_render_v1 already created");
                    return;
                }
            }
            WindowRender.create(object.getClient(), object.getVersion(), args.id, window) catch {
                object.getClient().postNoMemory();
                log.err("out of memory", .{});
            };
        },
    }
}

/// A 32-bit colour channel (river_window_v1.set_borders' convention) as a float.
fn channel(v: u32) f32 {
    return @floatCast(@as(f64, @floatFromInt(v)) / std.math.maxInt(u32));
}

pub const WindowRender = struct {
    object: *newm.RiverWindowRenderV1,
    /// Null once the window is destroyed, or if it was inert at creation.
    window: ?*Window,

    fn create(client: *wl.Client, version: u32, id: u32, window: ?*Window) !void {
        const window_render = try util.gpa.create(WindowRender);
        errdefer util.gpa.destroy(window_render);
        const object = try newm.RiverWindowRenderV1.create(client, version, id);
        window_render.* = .{ .object = object, .window = window };
        if (window) |w| w.newm.object = window_render;
        object.setHandler(*WindowRender, handleWindowRequest, handleWindowDestroy, window_render);
    }

    fn handleWindowDestroy(_: *newm.RiverWindowRenderV1, window_render: *WindowRender) void {
        if (window_render.window) |window| {
            window.newm.object = null;
            window.newm.requested = .{};
        }
        util.gpa.destroy(window_render);
    }

    fn handleWindowRequest(
        object: *newm.RiverWindowRenderV1,
        request: newm.RiverWindowRenderV1.Request,
        window_render: *WindowRender,
    ) void {
        switch (request) {
            .destroy => object.destroy(),
            .set_scale => |args| {
                const window = window_render.window orelse return;
                if (!server.wm.ensureRendering()) return;
                const scale = args.scale.toDouble();
                if (!(scale > 0)) {
                    object.postError(.invalid_scale, "scale must be greater than zero");
                    return;
                }
                window.newm.requested.scale = scale;
            },
            .set_opacity => |args| {
                const window = window_render.window orelse return;
                if (!server.wm.ensureRendering()) return;
                const opacity = args.opacity.toDouble();
                if (!(opacity >= 0 and opacity <= 1)) {
                    object.postError(.invalid_opacity, "opacity must be within [0, 1]");
                    return;
                }
                window.newm.requested.opacity = opacity;
            },
            .set_corner_radius => |args| {
                const window = window_render.window orelse return;
                if (!server.wm.ensureRendering()) return;
                window.newm.requested.corner_radius = args.radius;
            },
            .set_shadow => |args| {
                const window = window_render.window orelse return;
                if (!server.wm.ensureRendering()) return;
                window.newm.requested.shadow = .{
                    .color = .{ channel(args.red), channel(args.green), channel(args.blue), channel(args.alpha) },
                    .softness = args.softness,
                    .spread = args.spread,
                    // Bounded as placeShadow bounds the spread: the node geometry sums them.
                    .offset_x = std.math.clamp(args.offset_x, -10000, 10000),
                    .offset_y = std.math.clamp(args.offset_y, -10000, 10000),
                };
            },
            .clear_shadow => {
                const window = window_render.window orelse return;
                if (!server.wm.ensureRendering()) return;
                window.newm.requested.shadow = null;
            },
            .snapshot => |args| {
                NewmSnapshot.create(object.getClient(), object.getVersion(), args.id, window_render.window, args.lifetime) catch {
                    object.getClient().postNoMemory();
                    log.err("out of memory", .{});
                };
            },
        }
    }
};
