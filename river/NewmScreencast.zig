// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm-river fork: the zkde_screencast_unstable_v1 global (KDE's screencast protocol, which
//! Spectacle records through) relayed to the window manager over newm_river_screencast_v1
//! (protocol/newm-river-screencast-v1.xml). The window manager makes the PipeWire stream and
//! answers with its node; river only forwards.

const NewmScreencast = @This();

const std = @import("std");
const wayland = @import("wayland");
const wl = wayland.server.wl;
const newm = wayland.server.newm;
const zkde = wayland.server.zkde;
const wlr = @import("wlroots");

const server = &@import("main.zig").server;
const util = @import("util.zig");

const log = std.log.scoped(.wm);

/// zkde_screencast_unstable_v1 version 5: version 6 only adds the serial event.
const zkde_version = 5;

zkde_global: *wl.Global,
relay_global: *wl.Global,
/// The newest relay bound; requests go to it while its client is the active window manager.
relay: ?*newm.RiverScreencastV1 = null,
server_destroy: wl.Listener(*wl.Server) = .init(handleServerDestroy),

pub fn init(cast: *NewmScreencast) !void {
    cast.* = .{
        .zkde_global = try wl.Global.create(server.wl_server, zkde.ScreencastUnstableV1, zkde_version, *NewmScreencast, cast, bindZkde),
        .relay_global = try wl.Global.create(server.wl_server, newm.RiverScreencastV1, 1, *NewmScreencast, cast, bindRelay),
    };
    server.wl_server.addDestroyListener(&cast.server_destroy);
}

/// Sandboxed clients see neither global.
pub fn blocks(cast: *const NewmScreencast, global: *const wl.Global) bool {
    return global == cast.zkde_global or global == cast.relay_global;
}

fn handleServerDestroy(listener: *wl.Listener(*wl.Server), _: *wl.Server) void {
    const cast: *NewmScreencast = @fieldParentPtr("server_destroy", listener);
    cast.zkde_global.destroy();
    cast.relay_global.destroy();
}

fn bindRelay(client: *wl.Client, cast: *NewmScreencast, version: u32, id: u32) void {
    const relay = newm.RiverScreencastV1.create(client, version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    relay.setHandler(*NewmScreencast, handleRelayRequest, handleRelayDestroy, cast);
    cast.relay = relay;
}

fn handleRelayRequest(
    relay: *newm.RiverScreencastV1,
    request: newm.RiverScreencastV1.Request,
    _: *NewmScreencast,
) void {
    switch (request) {
        .destroy => relay.destroy(),
    }
}

fn handleRelayDestroy(relay: *newm.RiverScreencastV1, cast: *NewmScreencast) void {
    if (cast.relay == relay) cast.relay = null;
}

/// The relay of the active window manager, if it bound one.
fn activeRelay(cast: *NewmScreencast) ?*newm.RiverScreencastV1 {
    const relay = cast.relay orelse return null;
    const wm_v1 = server.wm.object orelse return null;
    return if (relay.getClient() == wm_v1.getClient()) relay else null;
}

fn bindZkde(client: *wl.Client, cast: *NewmScreencast, version: u32, id: u32) void {
    const object = zkde.ScreencastUnstableV1.create(client, version, id) catch {
        client.postNoMemory();
        log.err("out of memory", .{});
        return;
    };
    object.setHandler(*NewmScreencast, handleZkdeRequest, null, cast);
}

fn handleZkdeRequest(
    object: *zkde.ScreencastUnstableV1,
    request: zkde.ScreencastUnstableV1.Request,
    cast: *NewmScreencast,
) void {
    switch (request) {
        .destroy => object.destroy(),
        .stream_output => |args| {
            const stream = Stream.start(object, args.stream) orelse return;
            const output = wlr.Output.fromWlOutput(args.output) orelse
                return stream.fail("the requested output does not exist");
            const relay = cast.activeRelay() orelse
                return stream.fail("no window manager serves screencasts");
            const cast_v1 = stream.relayTo(relay) orelse return;
            relay.sendStreamOutput(cast_v1, output.name, args.pointer);
        },
        .stream_window => |args| {
            const stream = Stream.start(object, args.stream) orelse return;
            const relay = cast.activeRelay() orelse
                return stream.fail("no window manager serves screencasts");
            const cast_v1 = stream.relayTo(relay) orelse return;
            relay.sendStreamWindow(cast_v1, args.window_uuid, args.pointer);
        },
        .stream_region => |args| {
            const stream = Stream.start(object, args.stream) orelse return;
            const relay = cast.activeRelay() orelse
                return stream.fail("no window manager serves screencasts");
            const cast_v1 = stream.relayTo(relay) orelse return;
            relay.sendStreamRegion(cast_v1, args.x, args.y, args.width, args.height, args.scale, args.pointer);
        },
        .stream_virtual_output => |args| {
            const stream = Stream.start(object, args.stream) orelse return;
            stream.fail("newm-river has no virtual outputs");
        },
        .stream_virtual_output_with_description => |args| {
            const stream = Stream.start(object, args.stream) orelse return;
            stream.fail("newm-river has no virtual outputs");
        },
    }
}

/// One client stream and the window manager's cast serving it. Freed once both objects are gone.
const Stream = struct {
    zkde: ?*zkde.ScreencastStreamUnstableV1,
    cast: ?*newm.RiverScreencastCastV1 = null,
    /// created was forwarded.
    created: bool = false,
    /// failed or closed was forwarded: the client hears nothing more.
    ended: bool = false,

    fn start(object: *zkde.ScreencastUnstableV1, id: u32) ?*Stream {
        const client = object.getClient();
        const stream = util.gpa.create(Stream) catch {
            client.postNoMemory();
            log.err("out of memory", .{});
            return null;
        };
        const zkde_v1 = zkde.ScreencastStreamUnstableV1.create(client, object.getVersion(), id) catch {
            util.gpa.destroy(stream);
            client.postNoMemory();
            log.err("out of memory", .{});
            return null;
        };
        stream.* = .{ .zkde = zkde_v1 };
        zkde_v1.setHandler(*Stream, handleStreamRequest, handleStreamDestroy, stream);
        return stream;
    }

    fn fail(stream: *Stream, reason: [*:0]const u8) void {
        if (stream.ended) return;
        stream.ended = true;
        if (stream.zkde) |zkde_v1| zkde_v1.sendFailed(reason);
    }

    fn close(stream: *Stream) void {
        if (stream.ended) return;
        stream.ended = true;
        if (stream.zkde) |zkde_v1| zkde_v1.sendClosed();
    }

    /// A cast object in the window manager's client for this stream; fails the stream when out of
    /// memory.
    fn relayTo(stream: *Stream, relay: *newm.RiverScreencastV1) ?*newm.RiverScreencastCastV1 {
        const cast_v1 = newm.RiverScreencastCastV1.create(relay.getClient(), relay.getVersion(), 0) catch {
            log.err("out of memory", .{});
            stream.fail("out of memory");
            return null;
        };
        stream.cast = cast_v1;
        cast_v1.setHandler(*Stream, handleCastRequest, handleCastDestroy, stream);
        return cast_v1;
    }

    fn maybeFree(stream: *Stream) void {
        if (stream.zkde == null and stream.cast == null) util.gpa.destroy(stream);
    }

    fn handleStreamRequest(
        zkde_v1: *zkde.ScreencastStreamUnstableV1,
        request: zkde.ScreencastStreamUnstableV1.Request,
        _: *Stream,
    ) void {
        switch (request) {
            .close => zkde_v1.destroy(),
        }
    }

    fn handleStreamDestroy(_: *zkde.ScreencastStreamUnstableV1, stream: *Stream) void {
        stream.zkde = null;
        stream.ended = true;
        if (stream.cast) |cast_v1| cast_v1.sendStopped();
        stream.maybeFree();
    }

    fn handleCastRequest(
        cast_v1: *newm.RiverScreencastCastV1,
        request: newm.RiverScreencastCastV1.Request,
        stream: *Stream,
    ) void {
        switch (request) {
            .destroy => cast_v1.destroy(),
            .created => |args| {
                if (stream.ended or stream.created) return;
                stream.created = true;
                if (stream.zkde) |zkde_v1| zkde_v1.sendCreated(args.node);
            },
            .failed => |args| stream.fail(args.@"error"),
            .closed => stream.close(),
        }
    }

    fn handleCastDestroy(_: *newm.RiverScreencastCastV1, stream: *Stream) void {
        stream.cast = null;
        if (stream.created) stream.close() else stream.fail("the window manager stopped serving the stream");
        stream.maybeFree();
    }
};
