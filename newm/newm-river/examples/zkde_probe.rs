//! A zkde_screencast_unstable_v1 client for the e2e (`newm/e2e/screencast.sh`), as Spectacle's
//! KPipeWire uses it: asks river for a stream, waits for `created(node)` or `failed`, then
//! consumes the PipeWire node for a while and reports what came.
//!
//! - `output [NAME]`, `window APP_ID`, `window-uuid UUID`, `region X Y W H`, `virtual`: the
//!   source (`window` finds the toplevel's identifier through ext-foreign-toplevel-list).
//! - `node N`: no Wayland; consumes PipeWire node N (a portal's stream, say).
//! - `--secs S` (default 3) consumes that long; `--pointer P` (default 1, hidden).
//!
//! Prints `failed: <error>` and exits 3 when river fails the stream; otherwise prints one line
//! `node=N frames=F size=WxH format=FMT pts_increasing=B center=RRGGBB` and exits 0 (1 when no
//! frame came).

use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use pipewire as pw;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::format_utils::parse_format;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{self, Pod};
use pw::spa::sys::*;
use pw::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pw::stream::{StreamFlags, StreamRc};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, clippy::all)]
mod zkde {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "../../protocol/upstream/zkde-screencast-unstable-v1.xml"
        );
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!(
        "../../protocol/upstream/zkde-screencast-unstable-v1.xml"
    );
}

use zkde::zkde_screencast_stream_unstable_v1::{self as stream_v1, ZkdeScreencastStreamUnstableV1};
use zkde::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, Option<String>, Option<String>)>,
    node: Option<u32>,
    failed: Option<String>,
    closed: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut secs = 3.0f64;
    let mut pointer = 1u32;
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--secs" => secs = it.next().context("--secs S")?.parse()?,
            "--pointer" => pointer = it.next().context("--pointer P")?.parse()?,
            _ => pos.push(a.clone()),
        }
    }
    let kind = pos
        .first()
        .context("output|window|window-uuid|region|virtual|node")?;
    if kind == "node" {
        let node = pos.get(1).context("node N")?.parse()?;
        let report = consume(node, Duration::from_secs_f64(secs))?;
        print_report(node, &report);
        std::process::exit(if report.frames > 0 { 0 } else { 1 });
    }

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let cast: ZkdeScreencastUnstableV1 = globals
        .bind(&qh, 1..=5, ())
        .context("zkde_screencast_unstable_v1 missing")?;
    let list: Option<ExtForeignToplevelListV1> = globals.bind(&qh, 1..=1, ()).ok();
    let mut state = State::default();
    for g in globals.contents().clone_list() {
        if g.interface == wl_output::WlOutput::interface().name && g.version >= 4 {
            let o = globals
                .registry()
                .bind::<wl_output::WlOutput, _, _>(g.name, 4, &qh, ());
            state.outputs.push((o, None));
        }
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let stream = match kind.as_str() {
        "output" => {
            let o = match pos.get(1) {
                Some(name) => state
                    .outputs
                    .iter()
                    .find(|(_, n)| n.as_deref() == Some(name.as_str()))
                    .with_context(|| format!("no output {name}"))?,
                None => state.outputs.first().context("no output")?,
            };
            cast.stream_output(&o.0, pointer, &qh, ())
        }
        "window" => {
            let app = pos.get(1).context("window APP_ID")?;
            let uuid = state
                .toplevels
                .iter()
                .find(|t| t.2.as_deref() == Some(app.as_str()))
                .and_then(|t| t.1.clone())
                .with_context(|| format!("no toplevel {app} (list bound: {})", list.is_some()))?;
            cast.stream_window(format!("{{{uuid}}}"), pointer, &qh, ())
        }
        "window-uuid" => cast.stream_window(pos.get(1).context("UUID")?.clone(), pointer, &qh, ()),
        "region" => {
            let n =
                |i: usize| -> Result<i64> { Ok(pos.get(i).context("region X Y W H")?.parse()?) };
            cast.stream_region(
                n(1)? as i32,
                n(2)? as i32,
                n(3)? as u32,
                n(4)? as u32,
                0.0,
                pointer,
                &qh,
                (),
            )
        }
        "virtual" => cast.stream_virtual_output("probe".into(), 640, 480, 1.0, pointer, &qh, ()),
        k => bail!("unknown source {k}"),
    };
    conn.flush()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.node.is_none() && state.failed.is_none() {
        if Instant::now() > deadline {
            bail!("no created or failed in 10 s");
        }
        queue.roundtrip(&mut state)?;
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(e) = state.failed {
        println!("failed: {e}");
        stream.close();
        conn.flush()?;
        std::process::exit(3);
    }
    let node = state.node.context("no node")?;
    let report = consume(node, Duration::from_secs_f64(secs))?;
    stream.close();
    conn.flush()?;
    print_report(node, &report);
    std::process::exit(if report.frames > 0 { 0 } else { 1 });
}

fn print_report(node: u32, report: &Report) {
    println!(
        "node={node} frames={} size={}x{} format={} pts_increasing={} center={}",
        report.frames,
        report.size.0,
        report.size.1,
        report.format,
        report.pts_increasing,
        report.center
    );
}

#[derive(Default)]
struct Report {
    frames: u32,
    size: (u32, u32),
    format: String,
    format_id: Option<VideoFormat>,
    pts_increasing: bool,
    last_pts: Option<i64>,
    center: String,
}

fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object))
        .expect("serializing a pod");
    Pod::from_bytes(buffer).expect("a pod just serialized")
}

/// Connects to the node as KPipeWire does and counts the frames that come in `time`.
fn consume(node: u32, time: Duration) -> Result<Report> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let stream = StreamRc::new(core, "zkde-probe", pw::properties::PropertiesBox::new())?;
    let report = Rc::new(RefCell::new(Report {
        pts_increasing: true,
        center: "none".into(),
        ..Report::default()
    }));
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed({
            let report = report.clone();
            move |_, (), id, pod| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(pod) = pod else { return };
                match parse_format(pod) {
                    Ok((t, s)) if t == MediaType::Video && s == MediaSubtype::Raw => {}
                    _ => return,
                }
                let mut info = VideoInfoRaw::new();
                if info.parse(pod).is_ok() {
                    let mut r = report.borrow_mut();
                    r.size = (info.size().width, info.size().height);
                    r.format = format!("{:?}", info.format())
                        .trim_start_matches("VideoFormat::")
                        .to_owned();
                    r.format_id = Some(info.format());
                }
            }
        })
        .process({
            let report = report.clone();
            move |stream, ()| {
                // SAFETY: the buffer comes from this stream and is queued back.
                unsafe {
                    let b = stream.dequeue_raw_buffer();
                    if b.is_null() {
                        return;
                    }
                    let sb = (*b).buffer;
                    let mut r = report.borrow_mut();
                    let header: *mut spa_meta_header = spa_buffer_find_meta_data(
                        sb,
                        SPA_META_Header,
                        size_of::<spa_meta_header>(),
                    )
                    .cast();
                    if (*sb).n_datas > 0 {
                        let d = &*(*sb).datas;
                        let chunk = &*d.chunk;
                        if chunk.size > 0 && !d.data.is_null() {
                            r.frames += 1;
                            if let Some(h) = header.as_ref() {
                                if r.last_pts.is_some_and(|p| h.pts <= p) {
                                    r.pts_increasing = false;
                                }
                                r.last_pts = Some(h.pts);
                            }
                            let (w, hgt) = r.size;
                            let stride = chunk.stride.max(0) as usize;
                            let off = (hgt as usize / 2) * stride + (w as usize / 2) * 4;
                            if off + 4 <= chunk.size as usize {
                                let p = std::slice::from_raw_parts(
                                    d.data.cast::<u8>().add(chunk.offset as usize + off),
                                    4,
                                );
                                let (red, g, blue) = match r.format_id {
                                    Some(f) if f == VideoFormat::RGBx || f == VideoFormat::RGBA => {
                                        (p[0], p[1], p[2])
                                    }
                                    _ => (p[2], p[1], p[0]),
                                };
                                r.center = format!("{red:02x}{g:02x}{blue:02x}");
                            }
                        }
                    }
                    stream.queue_raw_buffer(b);
                }
            }
        })
        .register()?;

    let obj = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 320,
                height: 240
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let mut b = Vec::new();
    let mut params = [make_pod(&mut b, obj)];
    stream.connect(
        Direction::Input,
        Some(node),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let timer = main_loop.loop_().add_timer({
        let main_loop = main_loop.downgrade();
        move |_| {
            if let Some(l) = main_loop.upgrade() {
                l.quit();
            }
        }
    });
    let _ = timer.update_timer(Some(time), None);
    main_loop.run();
    let _ = stream.disconnect();
    let r = std::mem::take(&mut *report.borrow_mut());
    Ok(r)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(o) = state.outputs.iter_mut().find(|(o, _)| o == proxy)
        {
            o.1 = Some(name);
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push((toplevel, None, None));
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(t) = state.toplevels.iter_mut().find(|(h, ..)| h == proxy) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                t.1 = Some(identifier)
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.2 = Some(app_id),
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: zkde::zkde_screencast_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: stream_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            stream_v1::Event::Created { node } => state.node = Some(node),
            stream_v1::Event::Failed { error } => state.failed = Some(error),
            stream_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}
