//! Counts the linux-dmabuf feedback a compositor sends to one animated window
//! (`newm/e2e/headless.sh`, `nix/vm/session.py`):
//!
//! `feedback_probe --app-id A --counts FILE [--size WxH] [--seconds N]`
//!
//! Binds `zwp_linux_dmabuf_v1` v4 or v5, asks `get_surface_feedback` for an xdg toplevel's
//! surface before it maps, then commits a new solid colour every 16 ms (wl_shm buffers, frame
//! callbacks or not, so a hidden or zoomed window keeps committing). FILE always holds the totals
//! so far as one JSON object: `format_table`, `done`, `tranche_done`, `main_device` (feedback
//! events), `enter`, `leave` (wl_surface events), `commits`; a script reads it before and after
//! a step. Each second stderr gets that second's increments. Exits 77 when the compositor has no
//! linux-dmabuf v4 global (a pixman renderer), 0 when the window is closed or N seconds passed.

use std::fs;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Clone, Copy, Default, PartialEq)]
struct Counts {
    format_table: u64,
    done: u64,
    tranche_done: u64,
    main_device: u64,
    enter: u64,
    leave: u64,
    commits: u64,
}

impl Counts {
    fn json(&self) -> String {
        format!(
            "{{\"format_table\":{},\"done\":{},\"tranche_done\":{},\"main_device\":{},\
             \"enter\":{},\"leave\":{},\"commits\":{}}}",
            self.format_table,
            self.done,
            self.tranche_done,
            self.main_device,
            self.enter,
            self.leave,
            self.commits
        )
    }

    fn since(&self, b: &Counts) -> String {
        format!(
            "format_table +{} done +{} enter +{} leave +{} commits +{}",
            self.format_table - b.format_table,
            self.done - b.done,
            self.enter - b.enter,
            self.leave - b.leave,
            self.commits - b.commits
        )
    }
}

/// Two buffers of one size in one memfd pool, drawn alternately.
struct Pool {
    size: (i32, i32),
    map: *mut u32,
    len: usize,
    buffers: [wl_buffer::WlBuffer; 2],
}

impl Drop for Pool {
    fn drop(&mut self) {
        for b in &self.buffers {
            b.destroy();
        }
        // SAFETY: map/len are the mapping made in Pool::new, unmapped once here.
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

impl Pool {
    fn new(shm: &wl_shm::WlShm, size: (i32, i32), qh: &QueueHandle<App>) -> Result<Pool> {
        let (w, h) = size;
        let len = (w * h * 4 * 2) as usize;
        // SAFETY: memfd_create takes a NUL-terminated name and flags; a returned fd is ours.
        let raw = unsafe { libc::memfd_create(c"feedback-probe".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh, owned file descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: fd is a valid memfd; ftruncate only sizes it.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            bail!("ftruncate: {}", std::io::Error::last_os_error());
        }
        // SAFETY: a shared read/write mapping of len bytes of our own memfd.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buf = |k: i32| {
            pool.create_buffer(k * w * h * 4, w, h, w * 4, wl_shm::Format::Argb8888, qh, ())
        };
        let buffers = [buf(0), buf(1)];
        pool.destroy();
        Ok(Pool {
            size,
            map: map.cast(),
            len,
            buffers,
        })
    }
}

struct App {
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    size: (i32, i32),
    own: (i32, i32),
    configured: bool,
    closed: bool,
    pool: Option<Pool>,
    frame: u64,
    counts: Counts,
}

impl App {
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        if self.pool.as_ref().is_none_or(|p| p.size != self.size) {
            self.pool = Some(Pool::new(&self.shm, self.size, qh)?);
        }
        let pool = self.pool.as_ref().expect("made above");
        let k = (self.frame % 2) as usize;
        let n = (pool.size.0 * pool.size.1) as usize;
        let t = (self.frame * 3 % 256) as u32;
        let color = 0xff00_0000 | (t << 16) | ((255 - t) << 8) | 0x40;
        // SAFETY: buffer k is the k-th n-pixel half of the live mapping.
        let px = unsafe { std::slice::from_raw_parts_mut(pool.map.add(k * n), n) };
        px.fill(color);
        self.surface.attach(Some(&pool.buffers[k]), 0, 0);
        self.surface.damage(0, 0, pool.size.0, pool.size.1);
        self.surface.commit();
        self.frame += 1;
        self.counts.commits += 1;
        Ok(())
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
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

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => app.counts.enter += 1,
            wl_surface::Event::Leave { .. } => app.counts.leave += 1,
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event as E;
        match event {
            // The table's fd closes here, unlike Firefox's.
            E::FormatTable { fd, .. } => {
                drop(fd);
                app.counts.format_table += 1;
            }
            E::Done => app.counts.done += 1,
            E::TrancheDone => app.counts.tranche_done += 1,
            E::MainDevice { .. } => app.counts.main_device += 1,
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            app.configured = true;
            if let Err(e) = app.draw(qh) {
                eprintln!("feedback_probe: {e}");
                app.closed = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                app.size = (
                    if width > 0 { width } else { app.own.0 },
                    if height > 0 { height } else { app.own.1 },
                );
            }
            xdg_toplevel::Event::Close => app.closed = true,
            _ => {}
        }
    }
}

fn opt(args: &[String], name: &str) -> Option<String> {
    let k = args.iter().position(|a| a == name)?;
    args.get(k + 1).cloned()
}

fn write_counts(path: &str, counts: &Counts) -> Result<()> {
    let tmp = format!("{path}.tmp");
    fs::write(&tmp, counts.json() + "\n")?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app_id = opt(&args, "--app-id").unwrap_or_else(|| "feedback-probe".into());
    let counts_path = opt(&args, "--counts").context("--counts FILE")?;
    let own = match opt(&args, "--size") {
        Some(s) => {
            let (w, h) = s.split_once('x').context("a size is WxH")?;
            (w.parse()?, h.parse()?)
        }
        None => (400, 300),
    };
    let limit = opt(&args, "--seconds")
        .map(|s| s.parse::<u64>())
        .transpose()?;

    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let Ok(dmabuf) = globals.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(&qh, 4..=5, ())
    else {
        eprintln!("feedback_probe: no zwp_linux_dmabuf_v1 v4");
        std::process::exit(77);
    };
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;
    let surface = compositor.create_surface(&qh, ());
    let _feedback = dmabuf.get_surface_feedback(&surface, &qh, ());
    let xdg = base.get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    top.set_app_id(app_id.clone());
    top.set_title(app_id);
    surface.commit();

    let mut app = App {
        shm,
        surface,
        size: own,
        own,
        configured: false,
        closed: false,
        pool: None,
        frame: 0,
        counts: Counts::default(),
    };
    write_counts(&counts_path, &app.counts)?;
    let start = Instant::now();
    let mut next_frame = start;
    let mut next_report = start + Duration::from_secs(1);
    let mut reported = app.counts;
    let mut written = app.counts;
    while !app.closed {
        queue.flush()?;
        queue.dispatch_pending(&mut app)?;
        let now = Instant::now();
        let wake = next_frame.min(next_report);
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = wake.saturating_duration_since(now).as_millis() as i32;
            // SAFETY: one valid pollfd for the duration of the call.
            if unsafe { libc::poll(&mut pfd, 1, ms) } > 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        queue.dispatch_pending(&mut app)?;
        let now = Instant::now();
        if app.configured && now >= next_frame {
            app.draw(&qh)?;
            next_frame += Duration::from_millis(16);
            if next_frame < now {
                next_frame = now + Duration::from_millis(16);
            }
        }
        if app.counts != written {
            write_counts(&counts_path, &app.counts)?;
            written = app.counts;
        }
        if now >= next_report {
            eprintln!(
                "feedback_probe: t={}s {}",
                start.elapsed().as_secs(),
                app.counts.since(&reported)
            );
            reported = app.counts;
            next_report += Duration::from_secs(1);
            if limit.is_some_and(|l| start.elapsed().as_secs() >= l) {
                break;
            }
        }
    }
    eprintln!("feedback_probe: totals {}", app.counts.json());
    Ok(())
}
