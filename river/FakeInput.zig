// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: org_kde_kwin_fake_input (protocol/upstream/kde-fake-input.xml), which
//! xdg-desktop-portal-kde's RemoteDesktop sessions drive (KDE Connect's remote mouse and
//! keyboard). Only the xdg-desktop-portal-kde executable sees the global, never a sandboxed
//! client (the portal asks the user before any session starts), and requests count only after
//! authenticate. Each
//! authenticated object gets a pointer, a touch device and a keyboard on the default seat, so
//! its events take the path a real device's take: bindings, focus, idle and the cursor.

const FakeInput = @This();

const std = @import("std");
const mem = std.mem;
const wayland = @import("wayland");
const wl = wayland.server.wl;
const org = wayland.server.org;
const wlr = @import("wlroots");
const xkb = @import("xkbcommon");

const server = &@import("main.zig").server;
const util = @import("util.zig");
const policy = @import("NewmFakeInputPolicy.zig");

const log = std.log.scoped(.input);

const version = 6;

/// Linux evdev code of the left Shift key.
const key_leftshift = 42;

global: *wl.Global,
server_destroy: wl.Listener(*wl.Server) = .init(handleServerDestroy),

pub fn init(fake: *FakeInput) !void {
    fake.* = .{
        .global = try wl.Global.create(server.wl_server, org.KdeKwinFakeInput, version, *FakeInput, fake, bind),
    };
    server.wl_server.addDestroyListener(&fake.server_destroy);
}

fn handleServerDestroy(listener: *wl.Listener(*wl.Server), _: *wl.Server) void {
    const fake: *FakeInput = @fieldParentPtr("server_destroy", listener);
    fake.global.destroy();
}

/// Whether `client` may see the global: not sandboxed, and its executable is a root-owned
/// xdg-desktop-portal-kde in a directory only root may write (NewmFakeInputPolicy.zig).
pub fn allows(_: *const FakeInput, client: *const wl.Client) bool {
    if (server.security_context_manager.lookupClient(client) != null) return false;
    const pid = @constCast(client).getCredentials().pid;
    var link_buf: [64]u8 = undefined;
    const link = terminated(&link_buf, "/proc/{d}/exe", .{pid}) orelse return false;
    var exe_buf: [4096]u8 = undefined;
    const len = std.c.readlink(link, &exe_buf, exe_buf.len);
    if (len <= 0) return false;
    const exe = exe_buf[0..@intCast(len)];
    // The link reaches the running file even after its path is gone.
    const file = statOf(link) orelse return false;
    var dir_buf: [4096]u8 = undefined;
    const dir_path = terminated(&dir_buf, "{s}", .{policy.executableDir(exe)}) orelse return false;
    const dir = statOf(dir_path) orelse return false;
    return policy.executableMayFakeInput(exe, file, dir);
}

fn terminated(buf: []u8, comptime fmt: []const u8, args: anytype) ?[*:0]const u8 {
    const printed = std.fmt.bufPrint(buf[0 .. buf.len - 1], fmt, args) catch return null;
    buf[printed.len] = 0;
    return buf[0..printed.len :0];
}

fn statOf(path: [*:0]const u8) ?policy.Stat {
    const linux = std.os.linux;
    var stat: linux.Statx = undefined;
    const mask: linux.STATX = .{ .TYPE = true, .MODE = true, .UID = true };
    if (std.c.statx(linux.AT.FDCWD, path, 0, mask, &stat) != 0) return null;
    return .{ .uid = stat.uid, .mode = stat.mode };
}

// zig-wlroots binds neither wlr_pointer_init nor wlr_touch_init (wlr/interfaces/*.h).
const PointerImpl = extern struct { name: [*:0]const u8 };
const TouchImpl = extern struct { name: [*:0]const u8 };
extern fn wlr_pointer_init(pointer: *wlr.Pointer, impl: *const PointerImpl, name: [*:0]const u8) void;
extern fn wlr_pointer_finish(pointer: *wlr.Pointer) void;
extern fn wlr_pointer_notify_button(pointer: *wlr.Pointer, event: *wlr.Pointer.event.Button) void;
extern fn wlr_touch_init(touch: *wlr.Touch, impl: *const TouchImpl, name: [*:0]const u8) void;
extern fn wlr_touch_finish(touch: *wlr.Touch) void;

const pointer_impl: PointerImpl = .{ .name = "newm-river.fake-input" };
const touch_impl: TouchImpl = .{ .name = "newm-river.fake-input" };

const device_name = "newm-river fake input";

/// One bound org_kde_kwin_fake_input object and, once authenticated, its devices.
const Fake = struct {
    object: *org.KdeKwinFakeInput,
    authenticated: bool = false,
    pointer: wlr.Pointer = undefined,
    touch: wlr.Touch = undefined,
    keyboard: wlr.Keyboard = undefined,
    /// Touch points down, for touch_cancel (which names none).
    touches: policy.TouchPoints = .{},

    fn authenticate(fake: *Fake) void {
        if (fake.authenticated) return;
        const seat = server.input_manager.defaultSeat();

        wlr_pointer_init(&fake.pointer, &pointer_impl, device_name);
        wlr_touch_init(&fake.touch, &touch_impl, device_name);
        fake.keyboard.init(&.{ .name = "newm-river.fake-input", .led_update = null }, device_name);
        const keymap, _ = currentKeymap();
        _ = fake.keyboard.setKeymap(keymap);
        fake.authenticated = true;

        seat.attachNewDevice(&fake.pointer.base, .{ .virtual = true });
        seat.attachNewDevice(&fake.touch.base, .{ .virtual = true });
        seat.attachNewDevice(&fake.keyboard.base, .{ .virtual = true });
    }

    fn deinit(fake: *Fake) void {
        if (fake.authenticated) {
            fake.releaseAll();
            fake.keyboard.finish();
            wlr_pointer_finish(&fake.pointer);
            wlr_touch_finish(&fake.touch);
        }
        fake.touches.deinit(util.gpa);
    }

    /// Lets go of what the client holds when its object goes, the client's exit included. The
    /// finish calls fall short: wlr_keyboard_finish releases keys without updating the
    /// modifiers (a held Shift stays on), wlr_pointer_finish releases buttons without the frame
    /// that delivers them, and wlr_touch_finish ends no touch point.
    fn releaseAll(fake: *Fake) void {
        while (fake.keyboard.num_keycodes > 0) {
            fake.key(fake.keyboard.keycodes[fake.keyboard.num_keycodes - 1], false);
        }
        while (fake.pointer.button_count > 0) {
            fake.button(fake.pointer.buttons[fake.pointer.button_count - 1], false);
        }
        if (fake.touches.ids.items.len > 0) {
            fake.touchCancel();
            fake.touch.events.frame.emit();
        }
    }

    fn motion(fake: *Fake, dx: f64, dy: f64) void {
        var event: wlr.Pointer.event.Motion = .{
            .device = &fake.pointer.base,
            .time_msec = util.msecTimestamp(),
            .delta_x = dx,
            .delta_y = dy,
            .unaccel_dx = dx,
            .unaccel_dy = dy,
        };
        fake.pointer.events.motion.emit(&event);
        fake.pointer.events.frame.emit(&fake.pointer);
    }

    fn motionAbsolute(fake: *Fake, x: f64, y: f64) void {
        const fx, const fy = layoutFraction(x, y) orelse return;
        var event: wlr.Pointer.event.MotionAbsolute = .{
            .device = &fake.pointer.base,
            .time_msec = util.msecTimestamp(),
            .x = fx,
            .y = fy,
        };
        fake.pointer.events.motion_absolute.emit(&event);
        fake.pointer.events.frame.emit(&fake.pointer);
    }

    fn button(fake: *Fake, code: u32, pressed: bool) void {
        var event: wlr.Pointer.event.Button = .{
            .device = &fake.pointer.base,
            .time_msec = util.msecTimestamp(),
            .button = code,
            .state = if (pressed) .pressed else .released,
        };
        wlr_pointer_notify_button(&fake.pointer, &event);
        fake.pointer.events.frame.emit(&fake.pointer);
    }

    fn axis(fake: *Fake, which: policy.Axis, value: f64) void {
        var event: wlr.Pointer.event.Axis = .{
            .device = &fake.pointer.base,
            .time_msec = util.msecTimestamp(),
            .source = .wheel,
            .orientation = switch (which) {
                .vertical => .vertical_scroll,
                .horizontal => .horizontal_scroll,
            },
            .relative_direction = .identical,
            .delta = value,
            .delta_discrete = 0,
        };
        fake.pointer.events.axis.emit(&event);
        fake.pointer.events.frame.emit(&fake.pointer);
    }

    fn key(fake: *Fake, code: u32, pressed: bool) void {
        var event: wlr.Keyboard.event.Key = .{
            .time_msec = util.msecTimestamp(),
            .keycode = code,
            .update_state = true,
            .state = if (pressed) .pressed else .released,
        };
        fake.keyboard.notifyKey(&event);
    }

    fn keysym(fake: *Fake, sym: u32, pressed: bool) void {
        const code, const shift = keyForKeysym(sym) orelse {
            log.debug("fake input: no key in the keymap types keysym 0x{x}", .{sym});
            return;
        };
        if (shift and pressed) fake.key(key_leftshift, true);
        fake.key(code, pressed);
        if (shift and !pressed) fake.key(key_leftshift, false);
    }

    fn touchDown(fake: *Fake, id: i32, x: f64, y: f64) void {
        const fx, const fy = layoutFraction(x, y) orelse return;
        const down = fake.touches.down(util.gpa, id) catch {
            log.err("out of memory", .{});
            return;
        };
        if (!down) return;
        var event: wlr.Touch.event.Down = .{
            .device = &fake.touch.base,
            .time_msec = util.msecTimestamp(),
            .touch_id = id,
            .x = fx,
            .y = fy,
        };
        fake.touch.events.down.emit(&event);
    }

    fn touchMotion(fake: *Fake, id: i32, x: f64, y: f64) void {
        if (!fake.touches.isDown(id)) return;
        const fx, const fy = layoutFraction(x, y) orelse return;
        var event: wlr.Touch.event.Motion = .{
            .device = &fake.touch.base,
            .time_msec = util.msecTimestamp(),
            .touch_id = id,
            .x = fx,
            .y = fy,
        };
        fake.touch.events.motion.emit(&event);
    }

    fn touchUp(fake: *Fake, id: i32) void {
        if (!fake.touches.up(id)) return;
        var event: wlr.Touch.event.Up = .{
            .device = &fake.touch.base,
            .time_msec = util.msecTimestamp(),
            .touch_id = id,
        };
        fake.touch.events.up.emit(&event);
    }

    fn touchCancel(fake: *Fake) void {
        for (fake.touches.ids.items) |id| {
            var event: wlr.Touch.event.Cancel = .{
                .device = &fake.touch.base,
                .time_msec = util.msecTimestamp(),
                .touch_id = id,
            };
            fake.touch.events.cancel.emit(&event);
        }
        fake.touches.clear();
    }
};

fn bind(client: *wl.Client, _: *FakeInput, bound_version: u32, id: u32) void {
    const object = org.KdeKwinFakeInput.create(client, bound_version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    const fake = util.gpa.create(Fake) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        object.destroy();
        return;
    };
    fake.* = .{ .object = object };
    object.setHandler(*Fake, handleRequest, handleDestroy, fake);
}

fn handleDestroy(_: *org.KdeKwinFakeInput, fake: *Fake) void {
    fake.deinit();
    util.gpa.destroy(fake);
}

fn handleRequest(object: *org.KdeKwinFakeInput, request: org.KdeKwinFakeInput.Request, fake: *Fake) void {
    switch (request) {
        .destroy => return object.destroy(),
        .authenticate => |args| {
            log.info("fake input: {s} authenticated ({s})", .{ mem.span(args.application), mem.span(args.reason) });
            return fake.authenticate();
        },
        else => {},
    }
    // As in KWin: nothing counts before authenticate.
    if (!fake.authenticated) return;
    switch (request) {
        .destroy, .authenticate => unreachable,
        .pointer_motion => |args| fake.motion(args.delta_x.toDouble(), args.delta_y.toDouble()),
        .pointer_motion_absolute => |args| fake.motionAbsolute(args.x.toDouble(), args.y.toDouble()),
        // As in KWin, a state other than 0 or 1 and an unknown axis do nothing; neither does a
        // code past KEY_MAX.
        .button => |args| fake.button(
            policy.evdevCode(args.button) orelse return,
            policy.pressState(args.state) orelse return,
        ),
        .axis => |args| fake.axis(policy.axis(args.axis) orelse return, args.value.toDouble()),
        .keyboard_key => |args| fake.key(
            policy.evdevCode(args.button) orelse return,
            policy.pressState(args.state) orelse return,
        ),
        .keyboard_keysym => |args| fake.keysym(args.keysym, policy.pressState(args.state) orelse return),
        .touch_down => |args| fake.touchDown(@bitCast(args.id), args.x.toDouble(), args.y.toDouble()),
        .touch_motion => |args| fake.touchMotion(@bitCast(args.id), args.x.toDouble(), args.y.toDouble()),
        .touch_up => |args| fake.touchUp(@bitCast(args.id)),
        .touch_cancel => fake.touchCancel(),
        .touch_frame => fake.touch.events.frame.emit(),
    }
}

/// Global layout coordinates as the 0..1 fractions of the output layout box that absolute
/// pointer and touch events carry; null without outputs.
fn layoutFraction(x: f64, y: f64) ?struct { f64, f64 } {
    var box: wlr.Box = undefined;
    server.om.output_layout.getBox(null, &box);
    if (box.width <= 0 or box.height <= 0) return null;
    return .{
        (x - @as(f64, @floatFromInt(box.x))) / @as(f64, @floatFromInt(box.width)),
        (y - @as(f64, @floatFromInt(box.y))) / @as(f64, @floatFromInt(box.height)),
    };
}

/// The keymap and effective layout the seat's keyboard types with, else the default keymap.
fn currentKeymap() struct { *xkb.Keymap, xkb.LayoutIndex } {
    const seat = server.input_manager.defaultSeat();
    if (seat.wlr_seat.getKeyboard()) |keyboard| {
        if (keyboard.keymap) |keymap| {
            const layout = if (keyboard.xkb_state) |state|
                state.serializeLayout(@enumFromInt(xkb.State.Component.layout_effective))
            else
                0;
            return .{ keymap, layout };
        }
    }
    return .{ server.xkb_config.default_keymap, 0 };
}

/// The evdev code of a key that types `sym` in the current layout, and whether it takes Shift
/// (the keysym sits on the key's second level), as KWin resolves fake-input keysyms.
fn keyForKeysym(sym: u32) ?struct { u32, bool } {
    const keymap, const layout = currentKeymap();
    var level: xkb.LevelIndex = 0;
    while (level < 2) : (level += 1) {
        var code = keymap.minKeycode();
        while (code <= keymap.maxKeycode()) : (code += 1) {
            if (keymap.numLayoutsForKey(code) == 0) continue;
            const key_layout = if (layout < keymap.numLayoutsForKey(code)) layout else 0;
            for (keymap.keyGetSymsByLevel(code, key_layout, level)) |s| {
                // xkb keycodes are evdev codes plus 8.
                if (@intFromEnum(s) == sym and code >= 8) return .{ code - 8, level == 1 };
            }
        }
    }
    return null;
}
