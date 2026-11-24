// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: newm_river_snapshot_v1 (protocol/newm-river-render-v1.xml, version 2), a copy
//! of a window's last frame that river keeps drawing after the window is gone, so the window
//! manager can animate the close.
//!
//! The copy is a scene tree of new nodes: the window's buffers (locked again by the copies, so
//! they outlive the surfaces), border rects and shadow, at their drawn geometry, placed directly
//! above the window's tree. It takes no input. It is destroyed when the client destroys the
//! object or disconnects, or when its lifetime timer fires (river then sends expired).

const NewmSnapshot = @This();

const std = @import("std");
const wlr = @import("wlroots");
const wayland = @import("wayland");
const wl = wayland.server.wl;
const newm = wayland.server.newm;

const server = &@import("main.zig").server;
const util = @import("util.zig");

const NewmFx = @import("NewmFx.zig");
const NewmScale = @import("NewmScale.zig");
const Window = @import("Window.zig");

const log = std.log.scoped(.wm);

/// The longest a snapshot is kept, in milliseconds.
pub const max_lifetime = 10000;

const State = struct {
    x: c_int,
    y: c_int,
    scale: f64 = 1,
    opacity: f64 = 1,
};

/// One copied node with the values it was copied with (scale 1, opacity 1 of the snapshot).
const Item = struct {
    node: *wlr.SceneNode,
    kind: enum { buffer, rect, shadow },
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    opacity: f32 = 1,
    color: [4]f32 = .{ 0, 0, 0, 0 },
    corners: NewmFx.CornerRadii = .{},
    clip: NewmFx.ClippedRegion = .{},
    shadow_radius: c_int = 0,
    sigma: f32 = 0,
};

object: *newm.RiverSnapshotV1,
/// Null once expired.
tree: ?*wlr.SceneTree = null,
items: std.ArrayList(Item) = .empty,
timer: ?*wl.EventSource = null,
requested: State,
current: State,

/// Create the snapshot object for the given window (null: the window is gone, the snapshot is
/// expired at once).
pub fn create(client: *wl.Client, version: u32, id: u32, window: ?*Window, lifetime: u32) !void {
    const list = &server.newm_render.snapshots;
    try list.ensureUnusedCapacity(util.gpa, 1);
    const snapshot = try util.gpa.create(NewmSnapshot);
    errdefer util.gpa.destroy(snapshot);
    const object = try newm.RiverSnapshotV1.create(client, version, id);
    var x: c_int = 0;
    var y: c_int = 0;
    if (window) |w| {
        x = w.tree.node.x;
        y = w.tree.node.y;
    }
    snapshot.* = .{
        .object = object,
        .requested = .{ .x = x, .y = y },
        .current = .{ .x = x, .y = y },
    };
    object.setHandler(*NewmSnapshot, handleRequest, handleDestroy, snapshot);
    list.appendAssumeCapacity(snapshot);

    const w = window orelse {
        object.sendExpired();
        return;
    };
    snapshot.copy(w) catch {
        log.err("out of memory", .{});
        snapshot.drop();
        object.sendExpired();
        return;
    };
    const timer = server.wl_server.getEventLoop().addTimer(*NewmSnapshot, handleTimer, snapshot) catch {
        log.err("out of memory", .{});
        snapshot.drop();
        object.sendExpired();
        return;
    };
    snapshot.timer = timer;
    timer.timerUpdate(@intCast(std.math.clamp(lifetime, 1, max_lifetime))) catch {
        log.err("snapshot timer failed", .{});
    };
}

fn copy(snapshot: *NewmSnapshot, window: *Window) !void {
    const parent = window.tree.node.parent orelse return;
    const tree = try parent.createSceneTree();
    snapshot.tree = tree;
    tree.node.setPosition(window.tree.node.x, window.tree.node.y);
    tree.node.placeAbove(&window.tree.node);
    const box = NewmFx.content(window);
    try snapshot.copyTree(window, window.tree, 0, 0, box);
}

fn copyTree(snapshot: *NewmSnapshot, window: *Window, tree: *wlr.SceneTree, ox: c_int, oy: c_int, box: NewmFx.Content) !void {
    const into = snapshot.tree.?;
    var it = tree.children.iterator(.forward);
    while (it.next()) |node| {
        if (!node.enabled) continue;
        const x = ox + node.x;
        const y = oy + node.y;
        if (NewmFx.isFxNode(node)) {
            if (!NewmFx.enabled) continue;
            const shadow = window.newm.fx.shadow orelse continue;
            if (node != &shadow.node) continue;
            const copied = NewmFx.c.wlr_scene_shadow_create(
                into,
                shadow.width,
                shadow.height,
                shadow.corner_radius,
                shadow.blur_sigma,
                &shadow.color,
            ) orelse return error.OutOfMemory;
            NewmFx.c.wlr_scene_shadow_set_clipped_region(copied, shadow.clipped_region);
            copied.node.setPosition(x, y);
            try snapshot.items.append(util.gpa, .{
                .node = &copied.node,
                .kind = .shadow,
                .x = x,
                .y = y,
                .width = shadow.width,
                .height = shadow.height,
                .color = shadow.color,
                .clip = shadow.clipped_region,
                .shadow_radius = shadow.corner_radius,
                .sigma = shadow.blur_sigma,
            });
            continue;
        }
        switch (node.type) {
            .tree => try snapshot.copyTree(window, wlr.SceneTree.fromNode(node), x, y, box),
            .buffer => {
                const buffer = wlr.SceneBuffer.fromNode(node);
                const wlr_buffer = buffer.buffer orelse continue;
                const size = NewmFx.bufferSize(buffer);
                const copied = try into.createSceneBuffer(wlr_buffer);
                copied.node.setPosition(x, y);
                copied.setDestSize(size[0], size[1]);
                copied.setSourceBox(&buffer.src_box);
                copied.setTransform(buffer.transform);
                copied.setOpacity(buffer.opacity);
                copied.point_accepts_input = rejectInput;
                const corners = if (NewmFx.enabled) NewmFx.bufferCorners(buffer, x, y, box) else NewmFx.CornerRadii{};
                if (NewmFx.enabled) NewmFx.c.wlr_scene_buffer_set_corner_radii(copied, corners);
                try snapshot.items.append(util.gpa, .{
                    .node = &copied.node,
                    .kind = .buffer,
                    .x = x,
                    .y = y,
                    .width = size[0],
                    .height = size[1],
                    .opacity = buffer.opacity,
                    .corners = corners,
                });
            },
            .rect => {
                const rect = wlr.SceneRect.fromNode(node);
                if (rect.width <= 0 or rect.height <= 0 or rect.color[3] <= 0) continue;
                const copied = try into.createSceneRect(rect.width, rect.height, &rect.color);
                copied.node.setPosition(x, y);
                var item: Item = .{
                    .node = &copied.node,
                    .kind = .rect,
                    .x = x,
                    .y = y,
                    .width = rect.width,
                    .height = rect.height,
                    .color = rect.color,
                };
                if (NewmFx.enabled) {
                    const fx = NewmFx.Rect.from(rect);
                    item.corners = fx.corners;
                    item.clip = fx.clipped_region;
                    NewmFx.c.wlr_scene_rect_set_corner_radii(copied, item.corners);
                    NewmFx.c.wlr_scene_rect_set_clipped_region(copied, item.clip);
                    NewmFx.Rect.from(copied).accepts_input = false;
                }
                try snapshot.items.append(util.gpa, item);
            },
        }
    }
}

fn rejectInput(_: *wlr.SceneBuffer, _: *f64, _: *f64) callconv(.c) bool {
    return false;
}

/// Apply the requested state; called at every render_finish.
pub fn renderFinish(snapshot: *NewmSnapshot) void {
    const tree = snapshot.tree orelse return;
    const r = snapshot.requested;
    if (std.meta.eql(r, snapshot.current)) return;
    const rescale = r.scale != snapshot.current.scale or r.opacity != snapshot.current.opacity;
    snapshot.current = r;
    tree.node.setPosition(r.x, r.y);
    if (!rescale) return;
    const s = r.scale;
    const o = r.opacity;
    for (snapshot.items.items) |item| {
        item.node.setPosition(NewmScale.edge(item.x, s), NewmScale.edge(item.y, s));
        const width = NewmScale.span(item.x, item.width, s);
        const height = NewmScale.span(item.y, item.height, s);
        var color = item.color;
        for (&color) |*ch| ch.* = @floatCast(@as(f64, ch.*) * o);
        switch (item.kind) {
            .buffer => {
                const buffer = wlr.SceneBuffer.fromNode(item.node);
                buffer.setDestSize(width, height);
                buffer.setOpacity(@floatCast(@as(f64, item.opacity) * o));
                if (NewmFx.enabled) NewmFx.c.wlr_scene_buffer_set_corner_radii(buffer, item.corners.scaled(s));
            },
            .rect => {
                const rect = wlr.SceneRect.fromNode(item.node);
                rect.setSize(width, height);
                rect.setColor(&color);
                if (NewmFx.enabled) {
                    NewmFx.c.wlr_scene_rect_set_corner_radii(rect, item.corners.scaled(s));
                    NewmFx.c.wlr_scene_rect_set_clipped_region(rect, item.clip.scaled(s));
                }
            },
            .shadow => if (NewmFx.enabled) {
                const shadow: *NewmFx.Shadow = @fieldParentPtr("node", item.node);
                NewmFx.c.wlr_scene_shadow_set_size(shadow, width, height);
                NewmFx.c.wlr_scene_shadow_set_corner_radius(shadow, NewmFx.radius(item.shadow_radius, s));
                NewmFx.c.wlr_scene_shadow_set_blur_sigma(shadow, @floatCast(@as(f64, item.sigma) * s));
                // A straight colour (NewmFx.shadowColor): only its alpha fades.
                var straight = item.color;
                straight[3] = @floatCast(@as(f64, straight[3]) * o);
                NewmFx.c.wlr_scene_shadow_set_color(shadow, &straight);
                NewmFx.c.wlr_scene_shadow_set_clipped_region(shadow, item.clip.scaled(s));
            },
        }
    }
}

/// After the window manager restacked its windows: keep the snapshot above them.
pub fn raise(snapshot: *NewmSnapshot) void {
    if (snapshot.tree) |tree| tree.node.raiseToTop();
}

/// Remove the copy from the scene; the object stays until the client destroys it.
fn drop(snapshot: *NewmSnapshot) void {
    if (snapshot.tree) |tree| tree.node.destroy();
    snapshot.tree = null;
    snapshot.items.clearAndFree(util.gpa);
    if (snapshot.timer) |timer| timer.remove();
    snapshot.timer = null;
}

fn handleTimer(snapshot: *NewmSnapshot) c_int {
    // libwayland defers freeing an event source removed from its own callback.
    snapshot.drop();
    snapshot.object.sendExpired();
    return 0;
}

fn handleRequest(object: *newm.RiverSnapshotV1, request: newm.RiverSnapshotV1.Request, snapshot: *NewmSnapshot) void {
    switch (request) {
        .destroy => object.destroy(),
        .set_position => |args| {
            if (snapshot.tree == null or !server.wm.ensureRendering()) return;
            snapshot.requested.x = args.x;
            snapshot.requested.y = args.y;
        },
        .set_scale => |args| {
            const scale = args.scale.toDouble();
            if (!(scale > 0)) {
                object.postError(.invalid_scale, "scale must be greater than zero");
                return;
            }
            if (snapshot.tree == null or !server.wm.ensureRendering()) return;
            snapshot.requested.scale = scale;
        },
        .set_opacity => |args| {
            const opacity = args.opacity.toDouble();
            if (!(opacity >= 0 and opacity <= 1)) {
                object.postError(.invalid_opacity, "opacity must be within [0, 1]");
                return;
            }
            if (snapshot.tree == null or !server.wm.ensureRendering()) return;
            snapshot.requested.opacity = opacity;
        },
    }
}

fn handleDestroy(_: *newm.RiverSnapshotV1, snapshot: *NewmSnapshot) void {
    snapshot.drop();
    const list = &server.newm_render.snapshots;
    for (list.items, 0..) |s, i| {
        if (s == snapshot) {
            _ = list.swapRemove(i);
            break;
        }
    }
    snapshot.items.deinit(util.gpa);
    util.gpa.destroy(snapshot);
}
