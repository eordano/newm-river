// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: rounded corners and shadows through scenefx, for newm_river_window_render_v1
//! version 2 (protocol/newm-river-render-v1.xml).
//!
//! scenefx (https://github.com/wlrfx/scenefx) replaces wlroots' scene graph: libscenefx exports
//! the same wlr_scene_* functions and keeps the leading fields of wlroots' scene structs (so
//! zig-wlroots' bindings stay valid for every field river reads), and adds corner radii to
//! buffers and rects, a clipped region (a rounded box left undrawn) to rects and shadows, and
//! shadow nodes. It draws with its own GLES2 renderer only. Built with -Dscenefx=false this file
//! does nothing and river advertises version 1 of the render protocol.
//!
//! Every effect is applied to the drawn (already scaled, see NewmScale.zig) scene by apply(),
//! after NewmScale wrote the scaled geometry: at Window.renderFinish, after each commit of a
//! tracked surface and at every output frame.

const std = @import("std");
const build_options = @import("build_options");
const math = std.math;
const wlr = @import("wlroots");

const NewmScale = @import("NewmScale.zig");
const Window = @import("Window.zig");

const log = std.log.scoped(.wm);

pub const enabled = build_options.scenefx;

/// scenefx's struct fx_corner_radii.
pub const CornerRadii = extern struct {
    top_left: u16 = 0,
    top_right: u16 = 0,
    bottom_right: u16 = 0,
    bottom_left: u16 = 0,

    pub fn all(r: u16) CornerRadii {
        return .{ .top_left = r, .top_right = r, .bottom_right = r, .bottom_left = r };
    }

    pub fn scaled(corners: CornerRadii, s: f64) CornerRadii {
        return .{
            .top_left = radius(corners.top_left, s),
            .top_right = radius(corners.top_right, s),
            .bottom_right = radius(corners.bottom_right, s),
            .bottom_left = radius(corners.bottom_left, s),
        };
    }
};

/// scenefx's struct clipped_region: a rounded box, relative to its node, left undrawn.
pub const ClippedRegion = extern struct {
    area: wlr.Box = .{ .x = 0, .y = 0, .width = 0, .height = 0 },
    corners: CornerRadii = .{},

    pub fn scaled(region: ClippedRegion, s: f64) ClippedRegion {
        return .{
            .area = .{
                .x = NewmScale.edge(region.area.x, s),
                .y = NewmScale.edge(region.area.y, s),
                .width = NewmScale.span(region.area.x, region.area.width, s),
                .height = NewmScale.span(region.area.y, region.area.height, s),
            },
            .corners = region.corners.scaled(s),
        };
    }
};

/// scenefx's struct wlr_scene_rect: wlroots' fields, then these.
pub const Rect = extern struct {
    node: wlr.SceneNode,
    width: c_int,
    height: c_int,
    color: [4]f32,
    corners: CornerRadii,
    accepts_input: bool,
    clipped_region: ClippedRegion,

    pub fn from(rect: *wlr.SceneRect) *Rect {
        return @ptrCast(@alignCast(rect));
    }
};

/// scenefx's struct wlr_scene_shadow.
pub const Shadow = extern struct {
    node: wlr.SceneNode,
    width: c_int,
    height: c_int,
    corner_radius: c_int,
    color: [4]f32,
    blur_sigma: f32,
    clipped_region: ClippedRegion,
};

pub const c = struct {
    extern fn fx_renderer_create(backend: *wlr.Backend) ?*wlr.Renderer;
    pub extern fn wlr_scene_buffer_set_corner_radii(buffer: *wlr.SceneBuffer, corners: CornerRadii) void;
    pub extern fn wlr_scene_rect_set_corner_radii(rect: *wlr.SceneRect, corners: CornerRadii) void;
    pub extern fn wlr_scene_rect_set_clipped_region(rect: *wlr.SceneRect, region: ClippedRegion) void;
    pub extern fn wlr_scene_shadow_create(
        parent: *wlr.SceneTree,
        width: c_int,
        height: c_int,
        corner_radius: c_int,
        blur_sigma: f32,
        color: *const [4]f32,
    ) ?*Shadow;
    pub extern fn wlr_scene_shadow_set_size(shadow: *Shadow, width: c_int, height: c_int) void;
    pub extern fn wlr_scene_shadow_set_corner_radius(shadow: *Shadow, corner_radius: c_int) void;
    pub extern fn wlr_scene_shadow_set_blur_sigma(shadow: *Shadow, blur_sigma: f32) void;
    pub extern fn wlr_scene_shadow_set_color(shadow: *Shadow, color: *const [4]f32) void;
    pub extern fn wlr_scene_shadow_set_clipped_region(shadow: *Shadow, region: ClippedRegion) void;
};

/// The shadow asked for with set_shadow, in logical pixels.
pub const ShadowStyle = struct {
    /// Premultiplied.
    color: [4]f32,
    softness: u32,
    spread: i32,
    offset_x: i32,
    offset_y: i32,
};

/// Per-window scene state of the effects (in NewmScale).
pub const WindowFx = struct {
    shadow: ?*Shadow = null,
    /// Window.drawBorders drew a full border as one rect (border.top), to be rounded.
    ring: bool = false,
    /// Some effect is on the scene: clear() has work to do.
    active: bool = false,
};

pub fn createRenderer(backend: *wlr.Backend) ?*wlr.Renderer {
    if (!enabled) return null;
    const renderer = c.fx_renderer_create(backend);
    if (renderer == null) log.err("scenefx: failed to create the fx renderer (needs EGL and GLES2)", .{});
    return renderer;
}

/// True for scenefx's own node types (shadow, blur), which zig-wlroots' SceneNode.Type lacks:
/// never switch on their type.
pub fn isFxNode(node: *const wlr.SceneNode) bool {
    const t: *const c_int = @ptrCast(&node.type);
    return t.* > 2;
}

pub fn radius(r: anytype, s: f64) u16 {
    const v = @round(@as(f64, @floatFromInt(r)) * s);
    return @intFromFloat(math.clamp(v, 0, math.maxInt(u16)));
}

pub fn boxContains(outer: wlr.Box, inner: wlr.Box) bool {
    return outer.x <= inner.x and outer.y <= inner.y and
        outer.x + outer.width >= inner.x + inner.width and
        outer.y + outer.height >= inner.y + inner.height;
}

/// The drawn content box of the window (at its tree's origin) and its drawn corner radius.
pub const Content = struct { width: c_int, height: c_int, radius: u16 };

pub fn content(window: *const Window) Content {
    const st = window.newm.current;
    const s = st.scale;
    const r: u32 = if (window.wm_requested.fullscreen != null) 0 else st.corner_radius;
    return .{
        .width = NewmScale.edge(window.box.width, s),
        .height = NewmScale.edge(window.box.height, s),
        .radius = radius(r, s),
    };
}

/// The drawn size of a scene buffer.
pub fn bufferSize(buffer: *wlr.SceneBuffer) [2]c_int {
    var width = buffer.dst_width;
    var height = buffer.dst_height;
    if (width <= 0 or height <= 0) {
        width = buffer.private.buffer_width;
        height = buffer.private.buffer_height;
        if (@backingInt(buffer.transform) & 1 != 0) std.mem.swap(c_int, &width, &height);
    }
    return .{ width, height };
}

/// The corners of a buffer drawn at (x, y) in the window's tree that lie on a corner of the
/// content box: those are rounded.
pub fn bufferCorners(buffer: *wlr.SceneBuffer, x: c_int, y: c_int, box: Content) CornerRadii {
    if (box.radius == 0) return .{};
    const size = bufferSize(buffer);
    const near = struct {
        fn f(a: c_int, b: c_int) bool {
            return @abs(a - b) <= 1;
        }
    }.f;
    const left = near(x, 0);
    const top = near(y, 0);
    const right = near(x + size[0], box.width);
    const bottom = near(y + size[1], box.height);
    const r: u16 = @intCast(@min(@as(c_int, box.radius), @divTrunc(@min(size[0], size[1]), 2)));
    if (r <= 0) return .{};
    return .{
        .top_left = if (top and left) r else 0,
        .top_right = if (top and right) r else 0,
        .bottom_right = if (bottom and right) r else 0,
        .bottom_left = if (bottom and left) r else 0,
    };
}

/// The rounded border of a ring rect at its drawn position: outer corners and the window's
/// rounded content box cut out of it.
pub fn ringRegion(rect: *wlr.SceneRect, box: Content) struct { corners: CornerRadii, clip: ClippedRegion } {
    const width: c_int = @max(0, -rect.node.x);
    const outer: u16 = if (box.radius == 0) 0 else @intCast(@min(math.maxInt(u16), @as(c_int, box.radius) + width));
    return .{
        .corners = .all(outer),
        .clip = .{
            .area = .{ .x = -rect.node.x, .y = -rect.node.y, .width = box.width, .height = box.height },
            .corners = .all(box.radius),
        },
    };
}

/// Apply corners, the border ring and the shadow to the drawn scene of the window.
pub fn apply(window: *Window) void {
    if (!enabled) return;
    const ns = &window.newm;
    const box = content(window);
    ns.fx.active = true;

    roundTree(window.tree, 0, 0, box);

    const top = window.border.top;
    if (ns.fx.ring) {
        const ring = ringRegion(top, box);
        c.wlr_scene_rect_set_corner_radii(top, ring.corners);
        c.wlr_scene_rect_set_clipped_region(top, ring.clip);
    } else {
        clearRect(top);
    }

    const fullscreen = window.wm_requested.fullscreen != null;
    if (ns.current.shadow) |style| {
        if (!fullscreen) {
            const shadow = ns.fx.shadow orelse blk: {
                const zero: [4]f32 = .{ 0, 0, 0, 0 };
                const shadow = c.wlr_scene_shadow_create(window.tree, 0, 0, 0, 0, &zero) orelse {
                    log.err("out of memory", .{});
                    return;
                };
                shadow.node.lowerToBottom();
                ns.fx.shadow = shadow;
                break :blk shadow;
            };
            placeShadow(shadow, style, window.box.width, window.box.height, ns.current, box);
            shadow.node.setEnabled(true);
            return;
        }
    }
    if (ns.fx.shadow) |shadow| shadow.node.setEnabled(false);
}

/// Remove every effect from the window's scene (all its render state is the default again).
pub fn clear(window: *Window) void {
    if (!enabled) return;
    const ns = &window.newm;
    if (!ns.fx.active) return;
    ns.fx.active = false;
    roundTree(window.tree, 0, 0, .{ .width = 0, .height = 0, .radius = 0 });
    clearRect(window.border.top);
    if (ns.fx.shadow) |shadow| shadow.node.setEnabled(false);
}

fn clearRect(rect: *wlr.SceneRect) void {
    const fx = Rect.from(rect);
    const none: CornerRadii = .{};
    if (!std.meta.eql(fx.corners, none)) c.wlr_scene_rect_set_corner_radii(rect, none);
    if (fx.clipped_region.area.width != 0 or fx.clipped_region.area.height != 0) {
        c.wlr_scene_rect_set_clipped_region(rect, .{});
    }
}

fn roundTree(tree: *wlr.SceneTree, x: c_int, y: c_int, box: Content) void {
    var it = tree.children.iterator(.forward);
    while (it.next()) |node| {
        if (isFxNode(node)) continue;
        switch (node.type) {
            .tree => roundTree(wlr.SceneTree.fromNode(node), x + node.x, y + node.y, box),
            .buffer => {
                const buffer = wlr.SceneBuffer.fromNode(node);
                c.wlr_scene_buffer_set_corner_radii(buffer, bufferCorners(buffer, x + node.x, y + node.y, box));
            },
            .rect => {},
        }
    }
}

/// scenefx's shadow takes a straight (not premultiplied) colour and premultiplies it itself; the
/// protocol's colour is premultiplied. The opacity multiplies the alpha.
pub fn shadowColor(premultiplied: [4]f32, opacity: f64) [4]f32 {
    const a = premultiplied[3];
    var color = premultiplied;
    if (a > 0) {
        for (color[0..3]) |*ch| ch.* = @min(1, ch.* / a);
    }
    color[3] = @floatCast(@as(f64, a) * opacity);
    return color;
}

/// The shadow's node geometry for a window of the given (unscaled) size, drawn at scale and
/// opacity of the render state. scenefx draws the shadow of the box inset by blur_sigma in the
/// node, fading out over blur_sigma (a Gaussian of standard deviation blur_sigma / 2).
fn placeShadow(shadow: *Shadow, style: ShadowStyle, width: c_int, height: c_int, st: NewmScale.State, box: Content) void {
    const s = st.scale;
    const soft: c_int = @intCast(@min(style.softness, 10000));
    const spread = std.math.clamp(style.spread, -10000, 10000);
    const nx = -spread + style.offset_x - soft;
    const ny = -spread + style.offset_y - soft;
    const nw = width + 2 * spread + 2 * soft;
    const nh = height + 2 * spread + 2 * soft;
    const x = NewmScale.edge(nx, s);
    const y = NewmScale.edge(ny, s);
    shadow.node.setPosition(x, y);
    c.wlr_scene_shadow_set_size(shadow, NewmScale.span(nx, @max(0, nw), s), NewmScale.span(ny, @max(0, nh), s));
    const corner: c_int = if (box.radius == 0) 0 else @max(0, @as(c_int, box.radius) + @as(c_int, radius(@max(0, spread), s)));
    c.wlr_scene_shadow_set_corner_radius(shadow, corner);
    c.wlr_scene_shadow_set_blur_sigma(shadow, @floatCast(@as(f64, @floatFromInt(soft)) * s));
    c.wlr_scene_shadow_set_color(shadow, &shadowColor(style.color, st.opacity));
    c.wlr_scene_shadow_set_clipped_region(shadow, .{
        .area = .{ .x = -x, .y = -y, .width = box.width, .height = box.height },
        .corners = .all(box.radius),
    });
}
