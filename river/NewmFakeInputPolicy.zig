// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: the checks behind org_kde_kwin_fake_input (FakeInput.zig), kept free of
//! wlroots so `zig build test` covers them: which executables may bind the global, which
//! request values count, and the touch points a client holds.

const std = @import("std");
const mem = std.mem;

/// The owner and mode bits of a file or directory, as stat reports them.
pub const Stat = struct {
    uid: u32,
    mode: u32,
};

/// xdg-desktop-portal-kde only: it asks the user before
/// any RemoteDesktop session starts, and KWin likewise grants the interface only to the
/// executables whose desktop file lists it. `exe` is the resolved path /proc/<pid>/exe names;
/// NixOS's wrappers run the binary as `.xdg-desktop-portal-kde-wrapped`. A file name proves
/// nothing, since any user can copy or build a binary of that name, so the file and the
/// directory holding it belong to root and neither group nor others may write them, as in
/// /usr and /nix/store.
pub fn executableMayFakeInput(exe: []const u8, file: Stat, dir: Stat) bool {
    const path = withoutDeleted(exe);
    const slash = mem.lastIndexOfScalar(u8, path, '/') orelse return false;
    if (!mem.startsWith(u8, path, "/")) return false;
    var name = path[slash + 1 ..];
    if (mem.startsWith(u8, name, ".")) name = name[1..];
    if (mem.endsWith(u8, name, "-wrapped")) name = name[0 .. name.len - "-wrapped".len];
    if (!mem.eql(u8, name, "xdg-desktop-portal-kde")) return false;
    return rootOnly(file) and rootOnly(dir);
}

/// The directory holding `exe`, whose owner and mode executableMayFakeInput checks.
pub fn executableDir(exe: []const u8) []const u8 {
    const path = withoutDeleted(exe);
    const slash = mem.lastIndexOfScalar(u8, path, '/') orelse return path;
    return if (slash == 0) "/" else path[0..slash];
}

/// The kernel marks the path of an executable replaced since it started.
fn withoutDeleted(exe: []const u8) []const u8 {
    const deleted = " (deleted)";
    return if (mem.endsWith(u8, exe, deleted)) exe[0 .. exe.len - deleted.len] else exe;
}

/// Owned by root, and writable by neither group nor others.
fn rootOnly(stat: Stat) bool {
    return stat.uid == 0 and stat.mode & 0o022 == 0;
}

/// Linux's KEY_MAX: the highest evdev key or button code.
pub const key_max = 0x2ff;

/// A key or button code an evdev device can send, else null. river adds 8 to a key code for
/// xkb, which a code near the u32 maximum overflows.
pub fn evdevCode(code: u32) ?u32 {
    return if (code <= key_max) code else null;
}

/// A key or button state: 0 released, 1 pressed. KWin ignores any other value.
pub fn pressState(state: u32) ?bool {
    return switch (state) {
        0 => false,
        1 => true,
        else => null,
    };
}

pub const Axis = enum { vertical, horizontal };

/// wl_pointer's axis numbering. KWin ignores any other value.
pub fn axis(which: u32) ?Axis {
    return switch (which) {
        0 => .vertical,
        1 => .horizontal,
        else => null,
    };
}

/// The touch points one client holds down. A second down on an id already down does not
/// count, since river asserts that the ids down are distinct. The points still down when the
/// client goes are the ones FakeInput.zig cancels: wlr_touch_finish ends none.
pub const TouchPoints = struct {
    /// As many as river's seat tracks (Seat.zig `touch_points_max`).
    pub const max = 10;

    ids: std.ArrayList(i32) = .empty,

    pub fn deinit(points: *TouchPoints, gpa: mem.Allocator) void {
        points.ids.deinit(gpa);
    }

    pub fn isDown(points: *const TouchPoints, id: i32) bool {
        return mem.indexOfScalar(i32, points.ids.items, id) != null;
    }

    /// Whether `id` goes down: not when it is down already, nor past `max`.
    pub fn down(points: *TouchPoints, gpa: mem.Allocator, id: i32) !bool {
        if (points.isDown(id) or points.ids.items.len >= max) return false;
        try points.ids.append(gpa, id);
        return true;
    }

    /// Whether `id` was down.
    pub fn up(points: *TouchPoints, id: i32) bool {
        const i = mem.indexOfScalar(i32, points.ids.items, id) orelse return false;
        _ = points.ids.swapRemove(i);
        return true;
    }

    pub fn clear(points: *TouchPoints) void {
        points.ids.clearRetainingCapacity();
    }
};

const testing = std.testing;

test "only a root-owned KDE portal may fake input" {
    const root: Stat = .{ .uid = 0, .mode = 0o100555 };
    const root_dir: Stat = .{ .uid = 0, .mode = 0o40555 };
    for ([_][]const u8{
        "/nix/store/x-xdg-desktop-portal-kde-6.8.3/libexec/.xdg-desktop-portal-kde-wrapped",
        "/usr/libexec/xdg-desktop-portal-kde",
        "/usr/libexec/xdg-desktop-portal-kde (deleted)",
    }) |ok| try testing.expect(executableMayFakeInput(ok, root, root_dir));
    for ([_][]const u8{
        "/usr/bin/kdeconnectd",
        "/usr/bin/xdg-desktop-portal-kde-helper",
        "/tmp/.wrapped",
        "xdg-desktop-portal-kde",
        "/",
    }) |no| try testing.expect(!executableMayFakeInput(no, root, root_dir));

    const exe = "/home/u/bin/xdg-desktop-portal-kde";
    // The user's own copy, a root-owned binary in the user's directory, and directories that
    // group or others may write (/tmp's sticky 1777 among them).
    try testing.expect(!executableMayFakeInput(exe, .{ .uid = 1000, .mode = 0o100755 }, root_dir));
    try testing.expect(!executableMayFakeInput(exe, root, .{ .uid = 1000, .mode = 0o40755 }));
    try testing.expect(!executableMayFakeInput(exe, root, .{ .uid = 0, .mode = 0o41777 }));
    try testing.expect(!executableMayFakeInput(exe, root, .{ .uid = 0, .mode = 0o40775 }));
    try testing.expect(!executableMayFakeInput(exe, .{ .uid = 0, .mode = 0o100757 }, root_dir));
}

test "the directory an executable sits in" {
    try testing.expectEqualStrings("/usr/libexec", executableDir("/usr/libexec/xdg-desktop-portal-kde (deleted)"));
    try testing.expectEqualStrings("/", executableDir("/xdg-desktop-portal-kde"));
}

test "only evdev codes, 0 or 1 states and the two axes count" {
    try testing.expectEqual(@as(?u32, 30), evdevCode(30));
    try testing.expectEqual(@as(?u32, key_max), evdevCode(key_max));
    try testing.expectEqual(@as(?u32, null), evdevCode(key_max + 1));
    try testing.expectEqual(@as(?u32, null), evdevCode(std.math.maxInt(u32)));
    try testing.expectEqual(@as(?bool, false), pressState(0));
    try testing.expectEqual(@as(?bool, true), pressState(1));
    try testing.expectEqual(@as(?bool, null), pressState(2));
    try testing.expectEqual(@as(?Axis, .vertical), axis(0));
    try testing.expectEqual(@as(?Axis, .horizontal), axis(1));
    try testing.expectEqual(@as(?Axis, null), axis(2));
}

test "a touch id goes down once, and the ids still down are the ones to cancel" {
    const gpa = testing.allocator;
    var points: TouchPoints = .{};
    defer points.deinit(gpa);
    try testing.expect(try points.down(gpa, 1));
    try testing.expect(try points.down(gpa, 2));
    try testing.expect(!try points.down(gpa, 1));
    try testing.expect(points.up(1));
    try testing.expect(!points.up(1));
    try testing.expectEqualSlices(i32, &.{2}, points.ids.items);
    points.clear();
    var id: i32 = 0;
    while (id < TouchPoints.max) : (id += 1) try testing.expect(try points.down(gpa, id));
    try testing.expect(!try points.down(gpa, TouchPoints.max));
}
