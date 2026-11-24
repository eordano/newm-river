// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: the bookkeeping of NewmGestures.zig without wlroots, so `zig build test`
//! covers it: where each gesture in progress goes, and how motion between two manage sequences
//! is coalesced into one event.

const std = @import("std");

pub const Kind = enum(u2) {
    swipe = 0,
    pinch = 1,
    hold = 2,
};

pub fn Route(comptime Binding: type) type {
    return union(enum) {
        /// Not bound: the event goes to clients over pointer-gestures-v1.
        clients,
        /// Consumed by a binding that is gone: the event is dropped.
        drop,
        binding: Binding,
    };
}

/// Where the gesture in progress of each kind goes. A gesture keeps the route its begin chose until
/// it ends, so a client never sees half a gesture.
pub fn Routing(comptime Binding: type) type {
    return struct {
        const Self = @This();

        active: [3]Active = @splat(.idle),

        const Active = union(enum) {
            idle,
            clients,
            /// Null once the binding is destroyed.
            binding: ?Binding,
        };

        /// Returns true if the gesture is consumed.
        pub fn begin(routing: *Self, kind: Kind, binding: ?Binding) bool {
            routing.active[@backingInt(kind)] = if (binding) |b| .{ .binding = b } else .clients;
            return binding != null;
        }

        pub fn route(routing: *const Self, kind: Kind) Route(Binding) {
            return switch (routing.active[@backingInt(kind)]) {
                // An update or end whose begin came before routing started goes where it always went.
                .idle, .clients => .clients,
                .binding => |b| if (b) |binding| .{ .binding = binding } else .drop,
            };
        }

        pub fn end(routing: *Self, kind: Kind) Route(Binding) {
            defer routing.active[@backingInt(kind)] = .idle;
            return routing.route(kind);
        }

        /// The binding was destroyed: the rest of its gestures is dropped.
        pub fn forget(routing: *Self, binding: Binding) void {
            for (&routing.active) |*active| {
                switch (active.*) {
                    .binding => |b| if (b == binding) {
                        active.* = .{ .binding = null };
                    },
                    .idle, .clients => {},
                }
            }
        }
    };
}

/// Gesture motion since the last manage sequence.
pub const Update = struct {
    dx: f64 = 0,
    dy: f64 = 0,
    /// Pinch scale relative to the begin: the latest value wins.
    scale: f64 = 1,
    rotation: f64 = 0,

    pub fn add(previous: ?Update, dx: f64, dy: f64, scale: f64, rotation: f64) Update {
        const p = previous orelse Update{};
        return .{ .dx = p.dx + dx, .dy = p.dy + dy, .scale = scale, .rotation = p.rotation + rotation };
    }
};

/// Axis motion since the last manage sequence.
pub const Axis = struct {
    value: f64 = 0,
    value120: i32 = 0,

    pub fn add(previous: ?Axis, value: f64, value120: i32) Axis {
        const p = previous orelse Axis{};
        return .{ .value = p.value + value, .value120 = p.value120 +| value120 };
    }
};

/// Pointer motion since the last manage sequence, with and without acceleration (version 4's
/// pointer_motion). What wl_fixed's 1/256 steps cannot carry stays for the next event, so slow
/// motion is not rounded away.
pub const Motion = struct {
    dx: f64 = 0,
    dy: f64 = 0,
    unaccel_dx: f64 = 0,
    unaccel_dy: f64 = 0,

    /// The largest magnitude wl_fixed holds.
    const limit: f64 = 8388607;

    pub fn add(m: *Motion, dx: f64, dy: f64, unaccel_dx: f64, unaccel_dy: f64) void {
        m.dx = std.math.clamp(m.dx + dx, -limit, limit);
        m.dy = std.math.clamp(m.dy + dy, -limit, limit);
        m.unaccel_dx = std.math.clamp(m.unaccel_dx + unaccel_dx, -limit, limit);
        m.unaccel_dy = std.math.clamp(m.unaccel_dy + unaccel_dy, -limit, limit);
    }

    /// The motion to send now, in whole wl_fixed steps, or null if that is none; the rest stays.
    pub fn take(m: *Motion) ?Motion {
        const sent: Motion = .{
            .dx = fixed(m.dx),
            .dy = fixed(m.dy),
            .unaccel_dx = fixed(m.unaccel_dx),
            .unaccel_dy = fixed(m.unaccel_dy),
        };
        if (sent.dx == 0 and sent.dy == 0 and sent.unaccel_dx == 0 and sent.unaccel_dy == 0) return null;
        m.dx -= sent.dx;
        m.dy -= sent.dy;
        m.unaccel_dx -= sent.unaccel_dx;
        m.unaccel_dy -= sent.unaccel_dy;
        return sent;
    }

    fn fixed(v: f64) f64 {
        return @round(v * 256) / 256;
    }
};

const testing = std.testing;

test "an unbound gesture goes to clients until it ends" {
    var routing: Routing(u32) = .{};
    try testing.expect(!routing.begin(.swipe, null));
    try testing.expect(routing.route(.swipe) == .clients);
    try testing.expect(routing.end(.swipe) == .clients);
    try testing.expect(routing.route(.swipe) == .clients);
}

test "a bound gesture goes to its binding from begin to end" {
    var routing: Routing(u32) = .{};
    try testing.expect(routing.begin(.swipe, 7));
    try testing.expectEqual(Route(u32){ .binding = 7 }, routing.route(.swipe));
    try testing.expectEqual(Route(u32){ .binding = 7 }, routing.end(.swipe));
    try testing.expect(routing.route(.swipe) == .clients);
}

test "a gesture whose binding is destroyed is dropped, never handed to clients halfway" {
    var routing: Routing(u32) = .{};
    _ = routing.begin(.pinch, 3);
    _ = routing.begin(.swipe, 4);
    routing.forget(3);
    try testing.expect(routing.route(.pinch) == .drop);
    try testing.expect(routing.end(.pinch) == .drop);
    try testing.expect(routing.route(.pinch) == .clients);
    try testing.expectEqual(Route(u32){ .binding = 4 }, routing.route(.swipe));
}

test "kinds are routed independently" {
    var routing: Routing(u32) = .{};
    _ = routing.begin(.hold, 1);
    _ = routing.begin(.swipe, null);
    try testing.expectEqual(Route(u32){ .binding = 1 }, routing.end(.hold));
    try testing.expect(routing.route(.swipe) == .clients);
}

test "updates between manage sequences coalesce" {
    var u: ?Update = null;
    u = Update.add(u, 1.5, -2, 1.1, 3);
    u = Update.add(u, 0.5, -1, 1.25, -1);
    try testing.expectEqual(Update{ .dx = 2, .dy = -3, .scale = 1.25, .rotation = 2 }, u.?);
}

test "axis motion coalesces and value120 saturates" {
    var a: ?Axis = null;
    a = Axis.add(a, 15, 120);
    a = Axis.add(a, 15, 120);
    try testing.expectEqual(Axis{ .value = 30, .value120 = 240 }, a.?);
    try testing.expectEqual(std.math.maxInt(i32), Axis.add(.{ .value120 = std.math.maxInt(i32) }, 0, 1).value120);
}

test "pointer motion coalesces, accelerated and unaccelerated apart" {
    var m: Motion = .{};
    try testing.expectEqual(@as(?Motion, null), m.take());
    m.add(30, -12, 10, -4);
    m.add(1.5, 0, 0.5, 0);
    try testing.expectEqual(Motion{ .dx = 31.5, .dy = -12, .unaccel_dx = 10.5, .unaccel_dy = -4 }, m.take().?);
    try testing.expectEqual(@as(?Motion, null), m.take());
}

test "pointer motion below wl_fixed's step waits for a later event instead of vanishing" {
    var m: Motion = .{};
    var sent: Motion = .{};
    for (0..1000) |_| {
        m.add(0.001, 0, 0.001, 0);
        if (m.take()) |s| {
            sent.dx += s.dx;
            sent.unaccel_dx += s.unaccel_dx;
        }
    }
    try testing.expectApproxEqAbs(@as(f64, 1), sent.dx, 1.0 / 256.0);
    try testing.expectApproxEqAbs(@as(f64, 1), sent.unaccel_dx, 1.0 / 256.0);
}

test "a pointer motion sum stays in wl_fixed's range" {
    var m: Motion = .{};
    m.add(1e7, -1e7, 1e7, -1e7);
    m.add(1e7, -1e7, 1e7, -1e7);
    const s = m.take().?;
    try testing.expectEqual(@as(f64, 8388607), s.dx);
    try testing.expectEqual(@as(f64, -8388607), s.unaccel_dy);
}
