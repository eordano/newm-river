// SPDX-FileCopyrightText: © 2020 The River Developers
// SPDX-License-Identifier: GPL-3.0-only

const OutputManager = @This();

const build_options = @import("build_options");
const std = @import("std");
const assert = std.debug.assert;
const math = std.math;
const mem = std.mem;
const wlr = @import("wlroots");
const wl = @import("wayland").server.wl;
const zwlr = @import("wayland").server.zwlr;

const server = &@import("main.zig").server;
const util = @import("util.zig");

const DragIcon = @import("DragIcon.zig");
const LockSurface = @import("LockSurface.zig");
const Output = @import("Output.zig");
const SceneNodeData = @import("SceneNodeData.zig");
const Window = @import("Window.zig");
const XwaylandOverrideRedirect = @import("XwaylandOverrideRedirect.zig");

const log = std.log.scoped(.output);

/// The very first modeset is different in that if it fails we exit river.
first_modeset: bool = true,
/// newm: modesets failed in a row; past two, a failure no longer schedules another manage
/// sequence by itself, so an output that cannot be driven cannot spin river (a VT switch back
/// once retried a failing modeset about 340 times a second).
failed_modesets: u32 = 0,

new_output: wl.Listener(*wlr.Output) = .init(handleNewOutput),

output_layout: *wlr.OutputLayout,

presentation: *wlr.Presentation,
xdg_output_manager: *wlr.XdgOutputManagerV1,

wlr_output_manager: *wlr.OutputManagerV1,
manager_apply: wl.Listener(*wlr.OutputConfigurationV1) = .init(handleManagerApply),
manager_test: wl.Listener(*wlr.OutputConfigurationV1) = .init(handleManagerTest),

power_manager: *wlr.OutputPowerManagerV1,
power_manager_set_mode: wl.Listener(*wlr.OutputPowerManagerV1.event.SetMode) = .init(handlePowerManagerSetMode),

gamma_control_manager: *wlr.GammaControlManagerV1,

/// All Outputs that have a corresponding wlr_output.
outputs: wl.list.Head(Output, .link),

pub fn init(om: *OutputManager) !void {
    const output_layout = try wlr.OutputLayout.create(server.wl_server);
    errdefer output_layout.destroy();

    const gamma_control_manager = try wlr.GammaControlManagerV1.create(server.wl_server);
    server.scene.wlr_scene.setGammaControlManagerV1(gamma_control_manager);

    om.* = .{
        .output_layout = output_layout,
        .outputs = undefined,

        .presentation = try wlr.Presentation.create(server.wl_server, server.backend, 2),
        .xdg_output_manager = try wlr.XdgOutputManagerV1.create(server.wl_server, output_layout),
        .wlr_output_manager = try wlr.OutputManagerV1.create(server.wl_server),
        .power_manager = try wlr.OutputPowerManagerV1.create(server.wl_server),
        .gamma_control_manager = gamma_control_manager,
    };

    om.outputs.init();

    server.backend.events.new_output.add(&om.new_output);
    om.wlr_output_manager.events.apply.add(&om.manager_apply);
    om.wlr_output_manager.events.@"test".add(&om.manager_test);
    om.power_manager.events.set_mode.add(&om.power_manager_set_mode);
}

pub fn deinit(om: *OutputManager) void {
    om.manager_apply.link.remove();
    om.manager_test.link.remove();
    om.power_manager_set_mode.link.remove();

    om.output_layout.destroy();
}

fn handleNewOutput(_: *wl.Listener(*wlr.Output), wlr_output: *wlr.Output) void {
    log.debug("new output {s}", .{wlr_output.name});
    server.om.failed_modesets = 0;

    Output.create(wlr_output) catch |err| {
        switch (err) {
            error.OutOfMemory => log.err("out of memory", .{}),
            error.InitRenderFailed => log.err("failed to initialize renderer for output {s}", .{wlr_output.name}),
        }
        wlr_output.destroy();
        return;
    };
}

/// Returns null if there are no outputs in the output layout
pub fn outputAt(om: *OutputManager, lx: f64, ly: f64) ?*wlr.Output {
    var output_lx: f64 = undefined;
    var output_ly: f64 = undefined;
    om.output_layout.closestPoint(null, lx, ly, &output_lx, &output_ly);
    return om.output_layout.outputAt(output_lx, output_ly);
}

fn handleManagerTest(_: *wl.Listener(*wlr.OutputConfigurationV1), config: *wlr.OutputConfigurationV1) void {
    defer config.destroy();

    if (!validateConfigCoordinates(config)) {
        config.sendFailed();
        return;
    }

    const states = config.buildState() catch {
        log.err("out of memory", .{});
        config.sendFailed();
        return;
    };
    defer std.c.free(states.ptr);

    var swapchain_manager: wlr.OutputSwapchainManager = undefined;
    swapchain_manager.init(server.backend);
    defer swapchain_manager.finish();

    if (swapchain_manager.prepare(states)) {
        config.sendSucceeded();
    } else {
        config.sendFailed();
    }
}

fn handleManagerApply(_: *wl.Listener(*wlr.OutputConfigurationV1), config: *wlr.OutputConfigurationV1) void {
    log.info("applying output configuration", .{});
    server.om.failed_modesets = 0;

    if (!validateConfigCoordinates(config)) {
        config.sendFailed();
        return;
    }

    var it = config.heads.iterator(.forward);
    while (it.next()) |head| {
        const output: *Output = @ptrCast(@alignCast(head.state.output.data));
        if (head.state.enabled) {
            const previous = output.scheduled;
            output.scheduled = .fromHeadState(&head.state);
            // Maintain power management state set with wlr-output-power-management-v1
            if (previous.state == .disabled_soft) {
                output.scheduled.state = .disabled_soft;
            } else {
                assert(output.scheduled.state == .enabled);
            }
            // Maintain capture_session_count
            output.scheduled.capture_session_count = previous.capture_session_count;
        } else {
            // Avoid overwriting and losing all other output state on disable.
            output.scheduled.state = .disabled_hard;
        }
    }

    if (server.wm.scheduled.output_config) |old| {
        old.sendFailed();
        old.destroy();
    }
    server.wm.scheduled.output_config = config;

    server.wm.dirtyWindowing();
}

fn validateConfigCoordinates(config: *wlr.OutputConfigurationV1) bool {
    var it = config.heads.iterator(.forward);
    while (it.next()) |head| {
        if (!head.state.enabled) continue;

        const proposed: Output.State = .fromHeadState(&head.state);
        if (build_options.xwayland and server.xwayland != null) {
            // Negative output coordinates currently cause Xwayland clients to not receive click events.
            // See: https://gitlab.freedesktop.org/xorg/xserver/-/issues/899
            if (proposed.x < 0 or proposed.y < 0) {
                log.err(
                    \\Attempted to set negative coordinates for output {s}.
                    \\Negative output coordinates are disallowed if Xwayland is enabled due to a limitation of Xwayland.
                , .{head.state.output.name});
                return false;
            }
            const width, const height = proposed.dimensions();
            if (proposed.x + width > math.maxInt(i16) or
                proposed.y + height > math.maxInt(i16))
            {
                log.err(
                    \\Attempted to set too-large coordinates for output {s}.
                    \\Coordinates greater than {d} are disallowed if Xwayland is enabled due to a limitation of X11.
                , .{ head.state.output.name, math.maxInt(i16) });
                return false;
            }
        }
    }
    return true;
}

fn handlePowerManagerSetMode(
    _: *wl.Listener(*wlr.OutputPowerManagerV1.event.SetMode),
    event: *wlr.OutputPowerManagerV1.event.SetMode,
) void {
    // The output may have been destroyed, in which case there is nothing to do
    const output = @as(?*Output, @ptrCast(@alignCast(event.output.data))) orelse return;

    log.debug("client requested dpms {s} for output {s}", .{
        @tagName(event.mode),
        event.output.name,
    });

    switch (output.scheduled.state) {
        .enabled => {
            if (event.mode == .off) output.scheduled.state = .disabled_soft else return;
        },
        .disabled_soft => {
            if (event.mode == .on) output.scheduled.state = .enabled else return;
        },
        .disabled_hard, .destroying => unreachable,
    }

    server.wm.dirtyWindowing();
}

pub fn autoLayout(om: *OutputManager) void {
    // Find the right most edge of any non-autolayout output.
    var rightmost_edge: i32 = 0;
    var row_y: i32 = 0;
    {
        var it = om.outputs.iterator(.forward);
        while (it.next()) |output| {
            if (output.scheduled.auto_layout) continue;

            const x = output.scheduled.x + output.scheduled.dimensions()[0];
            if (x > rightmost_edge) {
                rightmost_edge = x;
                row_y = output.scheduled.y;
            }
        }
    }
    // Place autolayout outputs in a row starting at the rightmost edge.
    {
        var it = om.outputs.iterator(.forward);
        while (it.next()) |output| {
            if (!output.scheduled.auto_layout) continue;

            output.scheduled.x = rightmost_edge;
            output.scheduled.y = row_y;
            rightmost_edge += output.scheduled.dimensions()[0];
        }
    }
}

pub fn commitOutputState(om: *OutputManager) void {
    const wm = &server.wm;
    {
        var it = wm.sent.outputs.iterator(.forward);
        while (it.next()) |output| {
            assert(output.sent.state != .destroying);
            output.rendering_current = output.rendering_requested;
            // This may be null even when the state is not .destroying if the
            // output is destroyed between manage start and render finish.
            const wlr_output = output.wlr_output orelse continue;
            switch (output.sent.state) {
                .enabled, .disabled_soft => {
                    output.scene_output.?.setPosition(output.sent.x, output.sent.y);
                    _ = om.output_layout.add(wlr_output, output.sent.x, output.sent.y) catch {
                        log.err("out of memory", .{});
                        continue; // Try again next time
                    };
                    if (server.lock_manager.lockSurfaceFromOutput(output)) |lock_surface| {
                        lock_surface.tree.node.setPosition(output.sent.x, output.sent.y);
                    }
                },
                .disabled_hard => {
                    om.output_layout.remove(wlr_output);
                },
                .destroying => unreachable,
            }
        }
    }

    const need_modeset = blk: {
        var it = wm.sent.outputs.iterator(.forward);
        while (it.next()) |output| {
            const wlr_output = output.wlr_output orelse continue;
            switch (output.sent.state) {
                .enabled => if (!wlr_output.enabled) break :blk true,
                .disabled_soft, .disabled_hard => if (wlr_output.enabled and !output.newm_stuck_on) break :blk true,
                .destroying => unreachable,
            }
            switch (output.sent.mode) {
                .standard => |mode| {
                    if (mode != wlr_output.current_mode) break :blk true;
                },
                .custom => |mode| {
                    if (mode.width != wlr_output.width) break :blk true;
                    if (mode.height != wlr_output.height) break :blk true;
                    if (mode.refresh != wlr_output.refresh) break :blk true;
                },
                // This branch is reachable if we fail to enable an output.
                .none => assert(output.sent.state == .disabled_hard),
            }
            // If an output newly exposed to river is already enabled, we
            // must modeset since the mode is otherwise undefined.
            if (output.current.mode == .none and output.sent.state == .enabled) {
                break :blk true;
            }
            if (output.sent.adaptive_sync != (wlr_output.adaptive_sync_status == .enabled)) {
                break :blk true;
            }
        }
        break :blk false;
    };

    if (need_modeset) {
        log.debug("committing output state requires modeset", .{});

        var states: std.ArrayList(wlr.Backend.OutputState) = .empty;
        defer states.deinit(util.gpa);
        defer for (states.items) |*s| s.base.finish();

        {
            var it = wm.sent.outputs.iterator(.forward);
            while (it.next()) |output| {
                const wlr_output = output.wlr_output orelse continue;
                if (output.newm_stuck_on) continue;
                const state = states.addOne(util.gpa) catch {
                    log.err("out of memory", .{});
                    return;
                };

                state.output = wlr_output;
                state.base = wlr.Output.State.init();

                output.sent.applyModeset(&state.base);
            }
        }

        var swapchain_manager: wlr.OutputSwapchainManager = undefined;
        swapchain_manager.init(server.backend);
        defer swapchain_manager.finish();

        if (!swapchain_manager.prepare(states.items)) {
            log.err("failed to prepare new output configuration", .{});
            om.modesetFailed();
            return;
        }

        for (states.items) |*state| {
            const output: *Output = @ptrCast(@alignCast(state.output.data));
            if (!output.scene_output.?.buildState(&state.base, &.{
                .swapchain = swapchain_manager.getSwapchain(state.output),
            })) {
                log.err("failed to render scene for {s}", .{state.output.name});
            }
        }

        for (states.items) |*state| {
            // Unlike wlr_output_commit_state(), wlr_backend_commit() keeps fields that change
            // nothing. When an output is enabled again the scene re-applies the gamma LUT, with
            // no gamma client a reset to no color transform, and the DRM backend fails that
            // silently on a CRTC without gamma support (virtio-gpu): DPMS on after off failed.
            if (state.base.committed.color_transform and
                state.base.color_transform == state.output.private.color_transform)
            {
                state.base.committed.color_transform = false;
            }
        }

        if (!server.backend.commit(states.items)) {
            // newm: one output river cannot drive (a second GPU that cannot show river's
            // buffers, a USB display adapter, a refused mode) made the first modeset fail and
            // river exit, which ended the session at login; keep the outputs that work. The same
            // holds for every modeset river schedules itself: wlroots destroys every output when
            // the session goes inactive and makes new ones when it comes back, so a VT switch
            // back is a new first modeset. Only a wlr-output-management client's configuration
            // keeps its all-or-nothing answer.
            const own = om.first_modeset or wm.sent.output_config == null;
            if (!(own and om.commitUsableOutputs(states.items))) {
                log.err("failed to commit new output configuration", .{});
                om.modesetFailed();
                return;
            }
        }
        om.first_modeset = false;
        om.failed_modesets = 0;

        swapchain_manager.apply();
    }

    if (wm.sent.output_config) |config| {
        config.sendSucceeded();
        config.destroy();
        wm.sent.output_config = null;
    }

    {
        var it = wm.sent.outputs.safeIterator(.forward);
        while (it.next()) |output| {
            const wlr_output = output.wlr_output orelse continue;

            // The wl_output global is created by wlroots when the output is
            // added to the wlr_output_layout and a mode is committed.
            // Wlroots does not directly notify us when the wl_output global is created.
            // However, we want send the river_output_v1.wl_output event as soon as
            // possible and therefore need to check after committing a mode.
            if (!output.sent_wl_output) {
                if (wlr_output.global) |global| {
                    if (output.object) |output_v1| {
                        output_v1.sendWlOutput(global.getName(output_v1.getClient()));
                        output.sent_wl_output = true;
                    }
                }
            }
            switch (output.sent.state) {
                .enabled => {
                    assert(wlr_output.enabled);
                    wlr_output.scheduleFrame();
                },
                .disabled_soft, .disabled_hard => {
                    assert(!wlr_output.enabled or output.newm_stuck_on);
                    output.lock_render_state = .blanked;
                    if (output.sent.state == .disabled_hard) {
                        output.link_sent.remove();
                        output.link_sent.init();
                    }
                },
                .destroying => unreachable,
            }
            output.current = output.sent;
        }
    }

    om.sendConfig() catch {
        log.err("out of memory", .{});
    };
}

/// newm: commits each output's state on its own, after the commit of all of them failed; an
/// output whose commit fails stays off (logged), out of the layout, with no mode, so later
/// manage sequences leave it alone. wlr_backend_test passes such an output (a second GPU's
/// multi-GPU import fails only at commit), so only a commit finds it. Returns whether some
/// output was enabled.
fn commitUsableOutputs(om: *OutputManager, states: []wlr.Backend.OutputState) bool {
    var enabled: usize = 0;
    for (states, 0..) |*state, i| {
        const enabling = state.base.committed.enabled and state.base.enabled;
        const ok = server.backend.commit(states[i..][0..1]);
        log.debug("modeset of {s} alone: {s}", .{ state.output.name, if (ok) "ok" else "failed" });
        if (ok) {
            if (enabling) enabled += 1;
            continue;
        }
        const output: *Output = @ptrCast(@alignCast(state.output.data));
        // The output may be lit already: wlroots reads a connector's CRTC at connect, and the
        // kernel console lights it during a VT switch (or a crashed river left it on). Turn it
        // off, so river's state matches the hardware (river asserts that a disabled_hard output
        // is off: a VT switch back crashed it here).
        if (state.output.enabled and !turnOff(state.output)) {
            log.err("output {s} cannot be enabled by river nor turned off; river leaves it alone", .{state.output.name});
            output.newm_stuck_on = true;
        }
        log.err("output {s} cannot be enabled (its GPU cannot show river's buffers, or its mode is refused); leaving it off", .{state.output.name});
        om.output_layout.remove(state.output);
        output.scheduled.state = .disabled_hard;
        output.scheduled.mode = .none;
        output.sent.state = .disabled_hard;
        output.sent.mode = .none;
    }
    return enabled > 0;
}

/// newm: commits an output off on its own; returns whether it worked.
fn turnOff(wlr_output: *wlr.Output) bool {
    var off = [1]wlr.Backend.OutputState{.{ .output = wlr_output, .base = wlr.Output.State.init() }};
    defer off[0].base.finish();
    off[0].base.setEnabled(false);
    return server.backend.commit(&off);
}

fn modesetFailed(om: *OutputManager) void {
    const wm = &server.wm;

    // If the very first modeset fails, the user's hardware/drivers are
    // probably not compatible with river. In this case, exit rather
    // than running forever without rendering anything.
    if (om.first_modeset) {
        log.err("initial modeset failed, exiting river", .{});
        server.wl_server.terminate();
        return;
    }

    if (wm.sent.output_config) |config| {
        config.sendFailed();
        config.destroy();
        wm.sent.output_config = null;
    }

    {
        // Revert to last working state on failure
        var it = wm.sent.outputs.iterator(.forward);
        while (it.next()) |output| {
            output.scheduled = output.current;
            output.sent = output.current;
        }
        om.failed_modesets +|= 1;
        if (om.failed_modesets <= 2) {
            wm.dirtyWindowing();
        } else if (om.failed_modesets == 3) {
            log.err("output modesets keep failing; retrying only when something else changes", .{});
        }
    }
}

/// Send the current output state to all wlr-output-manager clients.
fn sendConfig(om: *OutputManager) !void {
    const config = try wlr.OutputConfigurationV1.create();
    // this destroys all associated config heads as well
    errdefer config.destroy();

    var it = om.outputs.iterator(.forward);
    while (it.next()) |output| {
        const wlr_output = output.wlr_output orelse continue;
        // newm: a VT switch back crashed river here (a ReleaseSafe panic on this switch); name
        // the output and skip it rather than end the session.
        const raw = @backingInt(output.current.state);
        if (raw > 2) {
            log.err("output {s}: current state {d} with a wlr_output, not sent to output managers", .{ wlr_output.name, raw });
            continue;
        }
        const head = try wlr.OutputConfigurationV1.Head.create(config, wlr_output);

        // It's only necessary to overwrite the state that does not require a modeset.
        // All state that requires a modeset will have already been committed to the wlr_output.
        head.state.enabled = switch (output.current.state) {
            .enabled, .disabled_soft => true,
            .disabled_hard => false,
            .destroying => unreachable,
        };
        head.state.scale = output.current.scale;
        head.state.transform = output.current.transform;
        head.state.x = output.current.x;
        head.state.y = output.current.y;
    }

    // wlroots won't send events to clients unless something has changed
    // compared to the last config set.
    om.wlr_output_manager.setConfiguration(config);
}

// Returning a wlr.Output rather than Output is more convenient at the callsites.
pub fn maxOverlapOutput(om: *OutputManager, box: *const wlr.Box) ?*wlr.Output {
    var max_overlap_area: i32 = 0;
    var max_overlap_output: ?*wlr.Output = null;
    var it = om.outputs.iterator(.forward);
    while (it.next()) |output| {
        const wlr_output = output.wlr_output orelse continue;
        var overlap: wlr.Box = undefined;
        om.output_layout.getBox(wlr_output, &overlap);
        if (overlap.empty()) continue; // output not in layout
        _ = overlap.intersection(&overlap, box);
        const overlap_area = overlap.width * overlap.height;
        if (overlap_area > max_overlap_area) {
            max_overlap_area = overlap_area;
            max_overlap_output = wlr_output;
        }
    }
    return max_overlap_output;
}
