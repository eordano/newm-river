// SPDX-FileCopyrightText: © 2026 The newm-river contributors
// SPDX-License-Identifier: GPL-3.0-only

//! newm: tells a new surface the scale of the output it opens on before its first configure,
//! so its first buffer is drawn at that scale. wlroots' scene sends a surface the scale of the
//! output it is on only once a buffer of it is there: without this, a client opening on a 1.25
//! output drew its first frame at scale 1 (Chromium) or 2 (GTK 4) and then drew again.

const wlr = @import("wlroots");

const server = &@import("main.zig").server;

/// The output a new window most likely opens on: the one under the default seat's cursor, where
/// newm-river's WM places windows on the active output. The scene corrects it if the WM puts the
/// window elsewhere.
pub fn windowOutput() ?*wlr.Output {
    const seat = server.input_manager.seats.first() orelse return null;
    const cursor = seat.cursor.wlr_cursor;
    return server.om.outputAt(cursor.x, cursor.y);
}

/// Sends `output`'s scale to `surface`: wp_fractional_scale_v1 and wl_surface's preferred buffer
/// scale (rounded up, as the scene does).
pub fn send(surface: *wlr.Surface, output: ?*wlr.Output) void {
    const o = output orelse return;
    const scale: f64 = o.scale;
    if (!(scale > 0)) return;
    surface.setPreferredBufferScale(@intFromFloat(@ceil(scale)));
    wlr.FractionalScaleManagerV1.notifyScale(surface, scale);
}
