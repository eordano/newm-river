// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: draws one window scaled and translucent, for newm_river_window_render_v1
//! (protocol/newm-river-render-v1.xml). The scale is a camera zoom: the app is never resized.
//!
//! wlroots 0.20's scene graph has no subtree transform. The scale is therefore written into every
//! node below the window's two trees (tree and popup_tree, whose own positions stay the window
//! manager's): each child position, buffer destination size and rect size is multiplied about the
//! window's top-left, and every buffer opacity and rect color by the opacity.
//!
//! wlroots rewrites these values to their unscaled ("base") values when a surface commits, and
//! river does in Window.renderFinish. Both run inside a hold: before them the window's trees are
//! disabled, so wlroots computes no output enter/leave for the intermediate geometry, and every
//! node returns to its recorded base; after them every node's values are the base, get scaled
//! again, and the trees are re-enabled. Each surface of a scaled window has a commit listener at
//! the head of its commit signal (before wlroots' scene listeners) and one at the tail (after).
//! A pass at every output frame catches what changed outside these hooks (a popup's first commit).
//!
//! Version 2's corner radius and shadow (NewmFx.zig, scenefx) are applied after each scaling pass.

const NewmScale = @This();

const std = @import("std");
const assert = std.debug.assert;
const wlr = @import("wlroots");
const wl = @import("wayland").server.wl;

const server = &@import("main.zig").server;
const util = @import("util.zig");

const NewmFx = @import("NewmFx.zig");
const NewmRender = @import("NewmRender.zig");
const Window = @import("Window.zig");

const log = std.log.scoped(.wm);

pub const State = struct {
    scale: f64 = 1,
    opacity: f64 = 1,
    /// Logical pixels, 0 for square corners.
    corner_radius: u32 = 0,
    shadow: ?NewmFx.ShadowStyle = null,

    pub fn identity(state: State) bool {
        return state.scale == 1 and state.opacity == 1 and state.corner_radius == 0 and
            state.shadow == null;
    }
};

/// Values one scene node holds that the scale or the opacity changes.
const Geometry = struct {
    x: c_int,
    y: c_int,
    /// Buffer destination size (0 = the buffer's own size) or rect size.
    width: c_int = 0,
    height: c_int = 0,
    opacity: f32 = 1,
    color: [4]f32 = .{ 0, 0, 0, 0 },

    fn eql(a: Geometry, b: Geometry) bool {
        return a.x == b.x and a.y == b.y and a.width == b.width and a.height == b.height and
            a.opacity == b.opacity and std.mem.eql(f32, &a.color, &b.color);
    }
};

const Record = struct {
    node: *wlr.SceneNode,
    base: Geometry,
    applied: Geometry,
};

/// One surface of the window, listened to while the window is scaled.
const Tracked = struct {
    window: *Window,
    surface: *wlr.Surface,
    seen: bool = true,
    pre_commit: wl.Listener(*wlr.Surface) = .init(handlePreCommit),
    post_commit: wl.Listener(*wlr.Surface) = .init(handlePostCommit),
    destroy: wl.Listener(*wlr.Surface) = .init(handleSurfaceDestroy),
};

/// Requested by the window manager, applied in Window.renderFinish.
requested: State = .{},
/// Applied to the scene.
current: State = .{},
/// The newm_river_window_render_v1 of this window, if any.
object: ?*NewmRender.WindowRender = null,
/// Scene state of the corner radius and shadow.
fx: NewmFx.WindowFx = .{},

/// Every scaled node with its base values; empty while the nodes hold their base values.
records: std.ArrayList(Record) = .empty,
tracked: std.ArrayList(*Tracked) = .empty,
/// Set while a commit or Window.renderFinish rewrites the base values.
hold: ?struct {
    depth: u32,
    tree_enabled: bool,
    popups_enabled: bool,
} = null,

pub fn deinit(ns: *NewmScale) void {
    if (ns.object) |object| object.window = null;
    ns.object = null;
    ns.untrackAll();
    ns.tracked.deinit(util.gpa);
    ns.records.deinit(util.gpa);
}

/// Window.renderFinish start: the nodes return to their base values before river rewrites them.
pub fn renderStart(ns: *NewmScale, window: *Window) void {
    if (ns.hold == null) ns.unscale(window);
    // Window.drawBorders reads the corner radius of this render sequence.
    ns.current = ns.requested;
}

/// Window.renderFinish end: apply the requested state and enable the trees as river decided.
pub fn renderFinish(ns: *NewmScale, window: *Window, enabled: bool) void {
    ns.current = ns.requested;
    if (ns.hold) |*hold| {
        hold.tree_enabled = enabled;
        hold.popups_enabled = enabled;
        return;
    }
    ns.sync(window);
    window.tree.node.setEnabled(enabled);
    window.popup_tree.node.setEnabled(enabled);
}

/// Called at every output frame: rescale what changed outside the hooks.
pub fn syncAll() void {
    var it = server.wm.windows.iterator();
    while (it.next()) |window| {
        if (!window.newm.current.identity() and window.newm.hold == null) window.newm.sync(window);
    }
}

/// Bring every node back to its base values and forget the records.
fn unscale(ns: *NewmScale, window: *Window) void {
    if (ns.records.items.len == 0) return;
    ns.unscaleTree(window.tree);
    ns.unscaleTree(window.popup_tree);
    ns.records.clearRetainingCapacity();
}

fn unscaleTree(ns: *NewmScale, tree: *wlr.SceneTree) void {
    var it = tree.children.iterator(.forward);
    while (it.next()) |node| {
        if (NewmFx.isFxNode(node) or savedBuffer(node)) continue;
        if (node.type == .buffer) setScaledInput(wlr.SceneBuffer.fromNode(node), false);
        if (ns.find(node)) |record| {
            // A node changed since it was scaled holds a new base value already.
            if (read(node).eql(record.applied)) write(node, record.base, false);
        }
        if (node.type == .tree) ns.unscaleTree(wlr.SceneTree.fromNode(node));
    }
}

/// Record each node's base and write its scaled values. A node whose values differ from the ones
/// last written was changed by wlroots or river: its current values are its new base.
fn sync(ns: *NewmScale, window: *Window) void {
    if (ns.current.identity()) {
        ns.unscale(window);
        ns.untrackAll();
        NewmFx.clear(window);
        return;
    }
    var previous = ns.records;
    defer previous.deinit(util.gpa);
    ns.records = .empty;
    for (ns.tracked.items) |tracked| tracked.seen = false;

    ns.syncTree(window, window.tree, &previous);
    ns.syncTree(window, window.popup_tree, &previous);

    var i: usize = 0;
    while (i < ns.tracked.items.len) {
        const tracked = ns.tracked.items[i];
        if (tracked.seen) {
            i += 1;
        } else {
            _ = ns.tracked.swapRemove(i);
            untrack(tracked);
        }
    }
    NewmFx.apply(window);
}

fn syncTree(ns: *NewmScale, window: *Window, tree: *wlr.SceneTree, previous: *const std.ArrayList(Record)) void {
    var it = tree.children.iterator(.forward);
    while (it.next()) |node| {
        if (NewmFx.isFxNode(node) or savedBuffer(node)) continue;
        if (node.type == .buffer) {
            const scene_buffer = wlr.SceneBuffer.fromNode(node);
            if (wlr.SceneSurface.tryFromBuffer(scene_buffer)) |scene_surface| {
                ns.track(window, scene_surface.surface);
            }
        }
        const now = read(node);
        const base = for (previous.items) |record| {
            if (record.node == node) break if (now.eql(record.applied)) record.base else now;
        } else now;
        const applied = ns.scaled(node, base);
        write(node, applied, ns.current.scale != 1);
        ns.records.append(util.gpa, .{ .node = node, .base = base, .applied = applied }) catch {
            log.err("out of memory", .{});
        };
        if (node.type == .tree) ns.syncTree(window, wlr.SceneTree.fromNode(node), previous);
    }
}

fn find(ns: *NewmScale, node: *wlr.SceneNode) ?*const Record {
    for (ns.records.items) |*record| {
        if (record.node == node) return record;
    }
    return null;
}

/// River's saved copies of surface buffers (Scene.SaveableSurfaces) are copied from the scaled
/// nodes, so they are already scaled; they are the only buffers without a surface in a window.
fn savedBuffer(node: *wlr.SceneNode) bool {
    if (node.type != .buffer) return false;
    return wlr.SceneSurface.tryFromBuffer(wlr.SceneBuffer.fromNode(node)) == null;
}

fn read(node: *wlr.SceneNode) Geometry {
    return switch (node.type) {
        .tree => .{ .x = node.x, .y = node.y },
        .rect => blk: {
            const rect = wlr.SceneRect.fromNode(node);
            break :blk .{ .x = node.x, .y = node.y, .width = rect.width, .height = rect.height, .color = rect.color };
        },
        .buffer => blk: {
            const buffer = wlr.SceneBuffer.fromNode(node);
            break :blk .{
                .x = node.x,
                .y = node.y,
                .width = buffer.dst_width,
                .height = buffer.dst_height,
                .opacity = buffer.opacity,
            };
        },
    };
}

fn write(node: *wlr.SceneNode, geometry: Geometry, scaled_input: bool) void {
    node.setPosition(geometry.x, geometry.y);
    switch (node.type) {
        .tree => {},
        .rect => {
            const rect = wlr.SceneRect.fromNode(node);
            rect.setSize(geometry.width, geometry.height);
            rect.setColor(&geometry.color);
        },
        .buffer => {
            const buffer = wlr.SceneBuffer.fromNode(node);
            buffer.setDestSize(geometry.width, geometry.height);
            buffer.setOpacity(geometry.opacity);
            setScaledInput(buffer, scaled_input);
        },
    }
}

fn scaled(ns: *const NewmScale, node: *wlr.SceneNode, base: Geometry) Geometry {
    const s = ns.current.scale;
    const o = ns.current.opacity;
    var result: Geometry = .{ .x = edge(base.x, s), .y = edge(base.y, s) };
    switch (node.type) {
        .tree => {},
        .rect => {
            result.width = span(base.x, base.width, s);
            result.height = span(base.y, base.height, s);
            for (&result.color, base.color) |*c, b| c.* = @floatCast(@as(f64, b) * o);
        },
        .buffer => {
            var width = base.width;
            var height = base.height;
            if (width <= 0 or height <= 0) {
                // wlroots draws a buffer without a destination size at the buffer's own size.
                const buffer = wlr.SceneBuffer.fromNode(node);
                width = buffer.private.buffer_width;
                height = buffer.private.buffer_height;
                if (@backingInt(buffer.transform) & 1 != 0) std.mem.swap(c_int, &width, &height);
            }
            result.width = span(base.x, width, s);
            result.height = span(base.y, height, s);
            result.opacity = @floatCast(@as(f64, base.opacity) * o);
        },
    }
    return result;
}

/// v * s rounded, saturated to c_int: a window manager's scale times a large coordinate must not
/// reach @intFromFloat out of range, which panics (and ends the session) in ReleaseSafe.
pub fn edge(v: c_int, s: f64) c_int {
    const product = @round(@as(f64, @floatFromInt(v)) * s);
    const lo: f64 = @floatFromInt(std.math.minInt(c_int));
    const hi: f64 = @floatFromInt(std.math.maxInt(c_int));
    return @intFromFloat(std.math.clamp(product, lo, hi));
}

/// The scaled size of [start, start + size), at least 1 pixel unless empty.
pub fn span(start: c_int, size: c_int, s: f64) c_int {
    if (size <= 0) return 0;
    return @max(1, edge(start +| size, s) -| edge(start, s));
}

/// wlroots' input test for a scene surface, saved the first time it is replaced.
var surface_accepts_input: ?*const fn (*wlr.SceneBuffer, *f64, *f64) callconv(.c) bool = null;

fn setScaledInput(buffer: *wlr.SceneBuffer, on: bool) void {
    if (on) {
        if (buffer.point_accepts_input != scaledAcceptsInput) {
            surface_accepts_input = buffer.point_accepts_input;
            buffer.point_accepts_input = scaledAcceptsInput;
        }
    } else if (buffer.point_accepts_input == scaledAcceptsInput) {
        buffer.point_accepts_input = surface_accepts_input;
    }
}

/// The input test of a scaled surface buffer: map the point back to surface coordinates. The
/// window gets no pointer input while scaled; this only lets river tell which window is hit.
fn scaledAcceptsInput(buffer: *wlr.SceneBuffer, sx: *f64, sy: *f64) callconv(.c) bool {
    const scene_surface = wlr.SceneSurface.tryFromBuffer(buffer) orelse return false;
    const surface = scene_surface.surface;
    const clip = scene_surface.private.clip;
    var width: c_int = surface.current.width;
    var height: c_int = surface.current.height;
    if (clip.width > 0 and clip.height > 0) {
        width = @min(clip.width, width - clip.x);
        height = @min(clip.height, height - clip.y);
    }
    if (buffer.dst_width > 0 and buffer.dst_height > 0) {
        sx.* = sx.* * @as(f64, @floatFromInt(width)) / @as(f64, @floatFromInt(buffer.dst_width));
        sy.* = sy.* * @as(f64, @floatFromInt(height)) / @as(f64, @floatFromInt(buffer.dst_height));
    }
    sx.* += @floatFromInt(clip.x);
    sy.* += @floatFromInt(clip.y);
    return surface.pointAcceptsInput(sx.*, sy.*);
}

fn track(ns: *NewmScale, window: *Window, surface: *wlr.Surface) void {
    for (ns.tracked.items) |tracked| {
        if (tracked.surface == surface) {
            tracked.seen = true;
            return;
        }
    }
    const tracked = util.gpa.create(Tracked) catch {
        log.err("out of memory", .{});
        return;
    };
    ns.tracked.append(util.gpa, tracked) catch {
        util.gpa.destroy(tracked);
        log.err("out of memory", .{});
        return;
    };
    tracked.* = .{ .window = window, .surface = surface };
    // Before wlroots' scene listeners, which were added when the scene surface was created.
    surface.events.commit.listener_list.prepend(&tracked.pre_commit);
    surface.events.commit.add(&tracked.post_commit);
    surface.events.destroy.add(&tracked.destroy);
}

fn untrack(tracked: *Tracked) void {
    tracked.pre_commit.link.remove();
    tracked.post_commit.link.remove();
    tracked.destroy.link.remove();
    util.gpa.destroy(tracked);
}

fn untrackAll(ns: *NewmScale) void {
    for (ns.tracked.items) |tracked| untrack(tracked);
    ns.tracked.clearRetainingCapacity();
}

fn handlePreCommit(listener: *wl.Listener(*wlr.Surface), _: *wlr.Surface) void {
    const tracked: *Tracked = @fieldParentPtr("pre_commit", listener);
    const window = tracked.window;
    const ns = &window.newm;
    if (ns.hold) |*hold| {
        hold.depth += 1;
        return;
    }
    ns.hold = .{
        .depth = 1,
        .tree_enabled = window.tree.node.enabled,
        .popups_enabled = window.popup_tree.node.enabled,
    };
    window.tree.node.setEnabled(false);
    window.popup_tree.node.setEnabled(false);
    ns.unscale(window);
}

fn handlePostCommit(listener: *wl.Listener(*wlr.Surface), _: *wlr.Surface) void {
    const tracked: *Tracked = @fieldParentPtr("post_commit", listener);
    // sync() may free this Tracked: do not use it below.
    const window = tracked.window;
    const ns = &window.newm;
    const hold = if (ns.hold) |*h| h else return;
    hold.depth -= 1;
    if (hold.depth > 0) return;
    const tree_enabled = hold.tree_enabled;
    const popups_enabled = hold.popups_enabled;
    ns.hold = null;
    ns.sync(window);
    window.tree.node.setEnabled(tree_enabled);
    window.popup_tree.node.setEnabled(popups_enabled);
}

fn handleSurfaceDestroy(listener: *wl.Listener(*wlr.Surface), _: *wlr.Surface) void {
    const tracked: *Tracked = @fieldParentPtr("destroy", listener);
    const ns = &tracked.window.newm;
    for (ns.tracked.items, 0..) |t, i| {
        if (t == tracked) {
            _ = ns.tracked.swapRemove(i);
            break;
        }
    }
    untrack(tracked);
}
