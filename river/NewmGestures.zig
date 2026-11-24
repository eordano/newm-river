// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: the newm_river_gestures_v1 global (protocol/newm-river-gestures-v1.xml).
//! The window manager binds touchpad swipes, pinches and holds by finger count and modifiers, and
//! pointer axis motion by orientation and modifiers. A bound gesture or axis event is consumed and
//! reported to the window manager in the next manage sequence; anything unbound reaches clients.

const NewmGestures = @This();

const std = @import("std");
const assert = std.debug.assert;
const wlr = @import("wlroots");
const wayland = @import("wayland");
const wl = wayland.server.wl;
const river = wayland.server.river;
const newm = wayland.server.newm;

const server = &@import("main.zig").server;
const util = @import("util.zig");

const Seat = @import("Seat.zig");

const log = std.log.scoped(.input);

const GestureState = @import("NewmGestureState.zig");
pub const Kind = GestureState.Kind;

global: *wl.Global,
server_destroy: wl.Listener(*wl.Server) = .init(handleServerDestroy),

pub fn init(gestures: *NewmGestures) !void {
    gestures.* = .{
        .global = try wl.Global.create(server.wl_server, newm.RiverGesturesV1, 4, ?*anyopaque, null, bind),
    };
    server.wl_server.addDestroyListener(&gestures.server_destroy);
}

fn handleServerDestroy(listener: *wl.Listener(*wl.Server), _: *wl.Server) void {
    const gestures: *NewmGestures = @fieldParentPtr("server_destroy", listener);
    gestures.global.destroy();
}

fn bind(client: *wl.Client, _: ?*anyopaque, version: u32, id: u32) void {
    const object = newm.RiverGesturesV1.create(client, version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    object.setHandler(?*anyopaque, handleRequest, null, null);
}

fn handleRequest(
    object: *newm.RiverGesturesV1,
    request: newm.RiverGesturesV1.Request,
    _: ?*anyopaque,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .get_seat => |args| {
            const seat_object = newm.RiverGesturesSeatV1.create(object.getClient(), object.getVersion(), args.id) catch {
                object.getClient().postNoMemory();
                log.err("out of memory", .{});
                return;
            };
            // river_seat_v1 objects turn inert when the active window manager goes, so only the
            // active window manager reaches a seat here.
            const data = args.seat.getUserData() orelse {
                seat_object.setHandler(?*anyopaque, handleSeatRequestInert, null, null);
                return;
            };
            const seat: *Seat = @ptrCast(@alignCast(data));
            if (seat.newm_gestures.object != null) {
                object.postError(.object_already_created, "newm_river_gestures_seat_v1 already created");
                return;
            }
            seat.newm_gestures.object = seat_object;
            seat_object.setHandler(*SeatGestures, handleSeatRequest, handleSeatDestroy, &seat.newm_gestures);
        },
    }
}

/// Per-seat state, the field Seat.newm_gestures.
pub const SeatGestures = struct {
    object: ?*newm.RiverGesturesSeatV1 = null,
    /// In creation order: the oldest matching binding wins.
    gestures: std.ArrayList(*GestureBinding) = .empty,
    axes: std.ArrayList(*AxisBinding) = .empty,
    routing: GestureState.Routing(*GestureBinding) = .{},
    /// Pointer motion for the next pointer_motion event (version 4).
    motion: GestureState.Motion = .{},

    fn seat(gseat: *SeatGestures) *Seat {
        return @fieldParentPtr("newm_gestures", gseat);
    }

    pub fn deinit(gseat: *SeatGestures) void {
        gseat.makeInert();
        gseat.gestures.deinit(util.gpa);
        gseat.axes.deinit(util.gpa);
    }

    /// The window manager went away: its bindings turn inert.
    pub fn makeInert(gseat: *SeatGestures) void {
        if (gseat.object) |object| {
            object.setHandler(?*anyopaque, handleSeatRequestInert, null, null);
            handleSeatDestroy(object, gseat);
        }
    }

    /// A button press with no window or shell surface under the pointer (version 3).
    pub fn desktopPress(gseat: *SeatGestures, lx: f64, ly: f64) void {
        const object = gseat.object orelse return;
        if (object.getVersion() < 3) return;
        object.sendDesktopPress(@intFromFloat(@floor(lx)), @intFromFloat(@floor(ly)));
    }

    /// Relative pointer motion, accelerated and not, for the next pointer_motion (version 4).
    pub fn pointerMotion(gseat: *SeatGestures, dx: f64, dy: f64, unaccel_dx: f64, unaccel_dy: f64) void {
        const object = gseat.object orelse return;
        if (object.getVersion() < 4) return;
        gseat.motion.add(dx, dy, unaccel_dx, unaccel_dy);
    }

    fn modifiers(gseat: *SeatGestures) ?wlr.Keyboard.ModifierMask {
        const keyboard = gseat.seat().wlr_seat.getKeyboard() orelse return null;
        return keyboard.getModifiers();
    }

    /// Returns true if the gesture is consumed.
    pub fn begin(gseat: *SeatGestures, kind: Kind, finger_count: u32) bool {
        const found: ?*GestureBinding = blk: {
            const mods = gseat.modifiers() orelse break :blk null;
            for (gseat.gestures.items) |binding| {
                if (binding.match(kind, finger_count, mods)) break :blk binding;
            }
            break :blk null;
        };
        if (!gseat.routing.begin(kind, found)) return false;
        const binding = found.?;
        binding.scheduled = .{ .begin = finger_count };
        // Further input waits until the window manager has seen the begin.
        server.wm.dirtyWindowing();
        return true;
    }

    /// Returns true if the gesture is consumed.
    pub fn update(gseat: *SeatGestures, kind: Kind, dx: f64, dy: f64, scale: f64, rotation: f64) bool {
        switch (gseat.routing.route(kind)) {
            .clients => return false,
            .drop => return true,
            .binding => |binding| {
                binding.scheduled.update = GestureState.Update.add(binding.scheduled.update, dx, dy, scale, rotation);
                server.wm.dirtyWindowingLazy();
                return true;
            },
        }
    }

    /// Returns true if the gesture is consumed.
    pub fn end(gseat: *SeatGestures, kind: Kind, cancelled: bool) bool {
        switch (gseat.routing.end(kind)) {
            .clients => return false,
            .drop => return true,
            .binding => |binding| {
                binding.scheduled.end = cancelled;
                server.wm.dirtyWindowing();
                return true;
            },
        }
    }

    /// Returns true if the axis event is consumed.
    pub fn axis(gseat: *SeatGestures, event: *const Seat.Event.PointerAxis) bool {
        const mods = gseat.modifiers() orelse return false;
        for (gseat.axes.items) |binding| {
            if (binding.match(event.orientation, event.source, mods)) {
                binding.scheduled = GestureState.Axis.add(binding.scheduled, event.delta, event.delta_discrete);
                binding.source = event.source;
                server.wm.dirtyWindowingLazy();
                return true;
            }
        }
        return false;
    }

    /// Send what happened since the last manage sequence.
    pub fn manageStart(gseat: *SeatGestures) void {
        for (gseat.gestures.items) |binding| binding.sendScheduled();
        for (gseat.axes.items) |binding| binding.sendScheduled();
        if (gseat.object) |object| {
            if (gseat.motion.take()) |m| {
                assert(object.getVersion() >= 4);
                object.sendPointerMotion(.fromDouble(m.dx), .fromDouble(m.dy), .fromDouble(m.unaccel_dx), .fromDouble(m.unaccel_dy));
            }
        }
    }

    fn forgetGesture(gseat: *SeatGestures, binding: *GestureBinding) void {
        gseat.routing.forget(binding);
        for (gseat.gestures.items, 0..) |b, i| {
            if (b == binding) {
                _ = gseat.gestures.orderedRemove(i);
                return;
            }
        }
    }

    fn forgetAxis(gseat: *SeatGestures, binding: *AxisBinding) void {
        for (gseat.axes.items, 0..) |b, i| {
            if (b == binding) {
                _ = gseat.axes.orderedRemove(i);
                return;
            }
        }
    }
};

fn handleSeatRequestInert(
    object: *newm.RiverGesturesSeatV1,
    request: newm.RiverGesturesSeatV1.Request,
    _: ?*anyopaque,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .get_gesture_binding => |args| {
            const binding = newm.RiverGestureBindingV1.create(object.getClient(), object.getVersion(), args.id) catch {
                object.getClient().postNoMemory();
                return;
            };
            binding.setHandler(?*anyopaque, handleGestureRequestInert, null, null);
        },
        .get_axis_binding => |args| {
            const binding = newm.RiverAxisBindingV1.create(object.getClient(), object.getVersion(), args.id) catch {
                object.getClient().postNoMemory();
                return;
            };
            binding.setHandler(?*anyopaque, handleAxisRequestInert, null, null);
        },
    }
}

fn handleSeatDestroy(_: *newm.RiverGesturesSeatV1, gseat: *SeatGestures) void {
    while (gseat.gestures.items.len > 0) gseat.gestures.items[0].destroy();
    while (gseat.axes.items.len > 0) gseat.axes.items[0].destroy();
    gseat.object = null;
    gseat.motion = .{};
}

fn handleSeatRequest(
    object: *newm.RiverGesturesSeatV1,
    request: newm.RiverGesturesSeatV1.Request,
    gseat: *SeatGestures,
) void {
    assert(gseat.object == object);
    switch (request) {
        .destroy => object.destroy(),
        .get_gesture_binding => |args| {
            const kind: Kind = switch (args.kind) {
                .swipe => .swipe,
                .pinch => .pinch,
                .hold => .hold,
                _ => {
                    object.postError(.invalid_kind, "invalid kind");
                    return;
                },
            };
            if (args.finger_count == 0) {
                object.postError(.invalid_finger_count, "finger_count must be greater than zero");
                return;
            }
            GestureBinding.create(gseat, object, args.id, kind, args.finger_count, args.modifiers) catch {
                object.getClient().postNoMemory();
                log.err("out of memory", .{});
            };
        },
        .get_axis_binding => |args| {
            switch (args.axis) {
                .vertical_scroll, .horizontal_scroll => {},
                _ => {
                    object.postError(.invalid_axis, "invalid wl_pointer.axis value");
                    return;
                },
            }
            AxisBinding.create(gseat, object, args.id, args.axis, args.modifiers) catch {
                object.getClient().postNoMemory();
                log.err("out of memory", .{});
            };
        },
    }
}

fn modifiersMatch(bound: river.SeatV1.Modifiers, held: wlr.Keyboard.ModifierMask) bool {
    return @as(u32, @bitCast(held)) == @as(u32, @bitCast(bound));
}

pub const GestureBinding = struct {
    gseat: *SeatGestures,
    object: *newm.RiverGestureBindingV1,
    kind: Kind,
    finger_count: u32,
    modifiers: river.SeatV1.Modifiers,
    enabled: bool = false,
    scheduled: struct {
        begin: ?u32 = null,
        update: ?GestureState.Update = null,
        end: ?bool = null,
    } = .{},

    fn create(
        gseat: *SeatGestures,
        seat_object: *newm.RiverGesturesSeatV1,
        id: u32,
        kind: Kind,
        finger_count: u32,
        modifiers: river.SeatV1.Modifiers,
    ) !void {
        const binding = try util.gpa.create(GestureBinding);
        errdefer util.gpa.destroy(binding);
        try gseat.gestures.ensureUnusedCapacity(util.gpa, 1);
        const object = try newm.RiverGestureBindingV1.create(seat_object.getClient(), seat_object.getVersion(), id);
        binding.* = .{
            .gseat = gseat,
            .object = object,
            .kind = kind,
            .finger_count = finger_count,
            .modifiers = modifiers,
        };
        gseat.gestures.appendAssumeCapacity(binding);
        object.setHandler(*GestureBinding, handleGestureRequest, handleGestureDestroy, binding);
    }

    fn destroy(binding: *GestureBinding) void {
        binding.object.setHandler(?*anyopaque, handleGestureRequestInert, null, null);
        handleGestureDestroy(binding.object, binding);
    }

    fn match(binding: *const GestureBinding, kind: Kind, finger_count: u32, held: wlr.Keyboard.ModifierMask) bool {
        return binding.enabled and binding.kind == kind and binding.finger_count == finger_count and
            modifiersMatch(binding.modifiers, held);
    }

    fn sendScheduled(binding: *GestureBinding) void {
        if (binding.scheduled.begin) |finger_count| binding.object.sendBegin(finger_count);
        if (binding.scheduled.update) |u| {
            binding.object.sendUpdate(
                .fromDouble(u.dx),
                .fromDouble(u.dy),
                .fromDouble(u.scale),
                .fromDouble(u.rotation),
            );
        }
        if (binding.scheduled.end) |cancelled| binding.object.sendEnd(@intFromBool(cancelled));
        binding.scheduled = .{};
    }
};

fn handleGestureRequestInert(
    object: *newm.RiverGestureBindingV1,
    request: newm.RiverGestureBindingV1.Request,
    _: ?*anyopaque,
) void {
    if (request == .destroy) object.destroy();
}

fn handleGestureDestroy(_: *newm.RiverGestureBindingV1, binding: *GestureBinding) void {
    binding.gseat.forgetGesture(binding);
    util.gpa.destroy(binding);
}

fn handleGestureRequest(
    object: *newm.RiverGestureBindingV1,
    request: newm.RiverGestureBindingV1.Request,
    binding: *GestureBinding,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .enable => {
            if (!server.wm.ensureWindowing()) return;
            binding.enabled = true;
        },
        .disable => {
            if (!server.wm.ensureWindowing()) return;
            binding.enabled = false;
        },
    }
}

pub const AxisBinding = struct {
    gseat: *SeatGestures,
    object: *newm.RiverAxisBindingV1,
    axis: wl.Pointer.Axis,
    modifiers: river.SeatV1.Modifiers,
    enabled: bool = false,
    scheduled: ?GestureState.Axis = null,
    /// The source of the latest bound event.
    source: wl.Pointer.AxisSource = .wheel,
    /// The sources it matches (set_sources, version 2).
    sources: newm.RiverAxisBindingV1.Sources = .{ .wheel = true, .finger = true, .continuous = true, .wheel_tilt = true },

    fn create(
        gseat: *SeatGestures,
        seat_object: *newm.RiverGesturesSeatV1,
        id: u32,
        axis: wl.Pointer.Axis,
        modifiers: river.SeatV1.Modifiers,
    ) !void {
        const binding = try util.gpa.create(AxisBinding);
        errdefer util.gpa.destroy(binding);
        try gseat.axes.ensureUnusedCapacity(util.gpa, 1);
        const object = try newm.RiverAxisBindingV1.create(seat_object.getClient(), seat_object.getVersion(), id);
        binding.* = .{
            .gseat = gseat,
            .object = object,
            .axis = axis,
            .modifiers = modifiers,
        };
        gseat.axes.appendAssumeCapacity(binding);
        object.setHandler(*AxisBinding, handleAxisRequest, handleAxisDestroy, binding);
    }

    fn destroy(binding: *AxisBinding) void {
        binding.object.setHandler(?*anyopaque, handleAxisRequestInert, null, null);
        handleAxisDestroy(binding.object, binding);
    }

    fn match(binding: *const AxisBinding, axis: wl.Pointer.Axis, source: wl.Pointer.AxisSource, held: wlr.Keyboard.ModifierMask) bool {
        return binding.enabled and binding.axis == axis and sourceMatch(binding.sources, source) and
            modifiersMatch(binding.modifiers, held);
    }

    fn sendScheduled(binding: *AxisBinding) void {
        const scheduled = binding.scheduled orelse return;
        binding.object.sendAxis(binding.source, .fromDouble(scheduled.value), scheduled.value120);
        binding.scheduled = null;
    }
};

fn sourceMatch(sources: newm.RiverAxisBindingV1.Sources, source: wl.Pointer.AxisSource) bool {
    return switch (source) {
        .wheel => sources.wheel,
        .finger => sources.finger,
        .continuous => sources.continuous,
        .wheel_tilt => sources.wheel_tilt,
        _ => true,
    };
}

fn handleAxisRequestInert(
    object: *newm.RiverAxisBindingV1,
    request: newm.RiverAxisBindingV1.Request,
    _: ?*anyopaque,
) void {
    if (request == .destroy) object.destroy();
}

fn handleAxisDestroy(_: *newm.RiverAxisBindingV1, binding: *AxisBinding) void {
    binding.gseat.forgetAxis(binding);
    util.gpa.destroy(binding);
}

fn handleAxisRequest(
    object: *newm.RiverAxisBindingV1,
    request: newm.RiverAxisBindingV1.Request,
    binding: *AxisBinding,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .enable => {
            if (!server.wm.ensureWindowing()) return;
            binding.enabled = true;
        },
        .disable => {
            if (!server.wm.ensureWindowing()) return;
            binding.enabled = false;
        },
        .set_sources => |args| {
            if (!server.wm.ensureWindowing()) return;
            binding.sources = args.sources;
        },
    }
}
