//! The synth background on the GPU: the fragment shader (`newm_synth::FRAGMENT_SHADER`),
//! the same `Frame` uniforms and `Tables` texture, drawn into an EGL window surface on the
//! background's own `wl_surface` at the output's physical size.
//!
//! EGL runs on the Wayland platform over the window manager's own connection (libwayland, the
//! `client_system` backend), so EGL allocates, negotiates and swaps the buffers itself and no
//! frame allocates. libEGL is loaded with dlopen when the background first draws, so GL drivers
//! come into the process only while the mode is not off. Swaps use interval 0: the window
//! manager paces frames by `wl_surface.frame` callbacks itself, as on the CPU path, and a swap
//! never waits.
//!
//! Every EGL or GL failure is returned, never retried here: the caller falls back to the CPU
//! painter. `NEWM_SYNTH_GPU_FAIL_AFTER=N` makes the swap after N frames fail, to test that.

use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;

use anyhow::{anyhow, bail, Context as _, Result};
use glow::HasContext as _;
use khronos_egl as egl;
use newm_synth::{Frame, Tables, TABLE_HEIGHT, TABLE_WIDTH};
use wayland_client::protocol::wl_surface;
use wayland_client::Proxy as _;
use wayland_egl::WlEglSurface;

type Egl = egl::DynamicInstance<egl::EGL1_5>;

/// `EGL_PLATFORM_WAYLAND_KHR`.
const PLATFORM_WAYLAND: egl::Enum = 0x31D8;

/// Where NixOS puts the system's GL driver libraries, for when libEGL is not on the search path.
const DRIVER_EGL: &str = "/run/opengl-driver/lib/libEGL.so.1";

const VERTEX: &str = "#version 100
attribute vec2 pos;
varying vec2 niri_v_coords;
void main() {
    // A triangle over the whole surface; niri_v_coords from (0, 0) at the top left to (1, 1)
    // at the bottom right, as niri's shader elements give it.
    niri_v_coords = vec2(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5);
    gl_Position = vec4(pos, 0.0, 1.0);
}
";

/// The EGL instance and display, shared by the context and every surface.
struct Display {
    egl: Egl,
    display: egl::Display,
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.egl.terminate(self.display);
    }
}

/// One background's EGL window surface.
pub struct GpuSurface {
    display: Rc<Display>,
    surface: egl::Surface,
    // Dropped after the EGL surface (see Drop).
    window: WlEglSurface,
    size: (i32, i32),
}

impl GpuSurface {
    pub fn resize(&mut self, size: (i32, i32)) {
        if size != self.size {
            self.window.resize(size.0, size.1, 0, 0);
            self.size = size;
        }
    }
}

impl Drop for GpuSurface {
    fn drop(&mut self) {
        let d = &self.display;
        let _ = d.egl.make_current(d.display, None, None, None);
        let _ = d.egl.destroy_surface(d.display, self.surface);
    }
}

/// What the GL program needs, made once the first surface is current.
struct Gl {
    gl: glow::Context,
    program: glow::Program,
    uniforms: HashMap<&'static str, glow::UniformLocation>,
    texture: glow::Texture,
    vbo: glow::Buffer,
    seed: Option<u32>,
}

pub struct Gpu {
    display: Rc<Display>,
    config: egl::Config,
    context: egl::Context,
    gl: Option<Gl>,
    /// The driver's name, for the log line.
    pub renderer: String,
    frames: u64,
    fail_after: Option<u64>,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let d = &self.display;
        if let Some(gl) = self.gl.take() {
            // SAFETY: GL objects of this context; the context is current or gone with it.
            unsafe {
                gl.gl.delete_program(gl.program);
                gl.gl.delete_texture(gl.texture);
                gl.gl.delete_buffer(gl.vbo);
            }
        }
        let _ = d.egl.make_current(d.display, None, None, None);
        let _ = d.egl.destroy_context(d.display, self.context);
    }
}

const UNIFORMS: [&str; 19] = [
    "synth_res",
    "synth_phase",
    "synth_scene",
    "synth_sky",
    "synth_glow",
    "synth_grid",
    "synth_sun_top",
    "synth_sun_bottom",
    "synth_sun",
    "synth_stripes",
    "synth_layers",
    "synth_octaves",
    "synth_octave4",
    "synth_layer",
    "synth_floor",
    "synth_lines",
    "synth_colour",
    "synth_post",
    "niri_alpha",
];

/// libEGL: `$NEWM_RIVER_LIBEGL` at run time, else the one the build named (the Nix package
/// names glvnd's), else the library search path, else NixOS's driver directory.
fn load_egl() -> Result<Egl> {
    let candidates = [
        std::env::var("NEWM_RIVER_LIBEGL").ok(),
        option_env!("NEWM_RIVER_LIBEGL").map(str::to_owned),
        Some("libEGL.so.1".to_owned()),
        Some(DRIVER_EGL.to_owned()),
    ];
    let mut errors = Vec::new();
    for path in candidates.into_iter().flatten() {
        // SAFETY: libEGL is the EGL API; khronos-egl checks every entry point it needs.
        match unsafe { Egl::load_required_from_filename(&path) } {
            Ok(egl) => return Ok(egl),
            Err(e) => errors.push(format!("{path}: {e}")),
        }
    }
    bail!("no libEGL ({})", errors.join("; "))
}

impl Gpu {
    /// An EGL context on the window manager's Wayland connection (`display` is libwayland's
    /// `wl_display`).
    pub fn new(display: *mut c_void) -> Result<Self> {
        if display.is_null() {
            bail!("the Wayland connection is not libwayland's");
        }
        let egl = load_egl()?;
        // SAFETY: `display` is the live wl_display of this process's connection.
        let d = unsafe { egl.get_platform_display(PLATFORM_WAYLAND, display, &[egl::ATTRIB_NONE]) }
            .context("no EGL display for Wayland")?;
        egl.initialize(d).context("EGL initialize")?;
        let display = Rc::new(Display { egl, display: d });
        let egl = &display.egl;
        egl.bind_api(egl::OPENGL_ES_API).context("no OpenGL ES")?;
        let attribs = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            0,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(d, &attribs)
            .context("EGL config")?
            .context("no opaque 8-bit GLES2 window config")?;
        let context = egl
            .create_context(
                d,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
            )
            .context("EGL context")?;
        let vendor = egl
            .query_string(Some(d), egl::VENDOR)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fail_after = std::env::var("NEWM_SYNTH_GPU_FAIL_AFTER")
            .ok()
            .and_then(|v| v.parse().ok());
        Ok(Gpu {
            display,
            config,
            context,
            gl: None,
            renderer: vendor,
            frames: 0,
            fail_after,
        })
    }

    /// An EGL window surface on `surface`, `size` physical pixels large.
    pub fn surface(&self, surface: &wl_surface::WlSurface, size: (i32, i32)) -> Result<GpuSurface> {
        let window = WlEglSurface::new(surface.id(), size.0.max(1), size.1.max(1))
            .map_err(|e| anyhow!("wl_egl_window: {e:?}"))?;
        let d = &self.display;
        // SAFETY: the window belongs to this display's Wayland connection.
        let s = unsafe {
            d.egl
                .create_window_surface(d.display, self.config, window.ptr() as _, None)
        }
        .context("EGL window surface")?;
        Ok(GpuSurface {
            display: d.clone(),
            surface: s,
            window,
            size,
        })
    }

    /// Draws `frame` into `s` and swaps, which commits the surface. The caller asks for the
    /// frame callback before.
    pub fn draw(&mut self, s: &GpuSurface, frame: &Frame, seed: u32) -> Result<()> {
        let d = self.display.clone();
        d.egl
            .make_current(
                d.display,
                Some(s.surface),
                Some(s.surface),
                Some(self.context),
            )
            .context("EGL make current")?;
        if self.gl.is_none() {
            // A swap never waits for the compositor: the window manager paces frames itself.
            d.egl
                .swap_interval(d.display, 0)
                .context("EGL swap interval")?;
            self.gl = Some(self.make_gl()?);
            self.renderer = format!(
                "{} ({})",
                // SAFETY: the context is current.
                unsafe {
                    self.gl
                        .as_ref()
                        .unwrap()
                        .gl
                        .get_parameter_string(glow::RENDERER)
                },
                self.renderer
            );
        }
        let gl = self.gl.as_mut().unwrap();
        if gl.seed != Some(seed) {
            let tables = Tables::new(seed);
            // SAFETY: the texture belongs to the current context; the data is the table's size.
            unsafe {
                gl.gl.bind_texture(glow::TEXTURE_2D, Some(gl.texture));
                gl.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    TABLE_WIDTH as i32,
                    TABLE_HEIGHT as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(tables.rgba())),
                );
            }
            gl.seed = Some(seed);
        }
        // SAFETY: plain GL calls on the current context with this program's locations.
        unsafe {
            let g = &gl.gl;
            let u = |name: &str| gl.uniforms.get(name);
            g.viewport(0, 0, s.size.0, s.size.1);
            g.use_program(Some(gl.program));
            g.active_texture(glow::TEXTURE0);
            g.bind_texture(glow::TEXTURE_2D, Some(gl.texture));
            g.uniform_2_f32(u("synth_res"), frame.res[0], frame.res[1]);
            let v4 = |name: &str, v: [f32; 4]| g.uniform_4_f32(u(name), v[0], v[1], v[2], v[3]);
            let v3 = |name: &str, v: [f32; 3]| g.uniform_3_f32(u(name), v[0], v[1], v[2]);
            v4("synth_phase", frame.phase);
            v4("synth_scene", frame.scene);
            v3("synth_sky", frame.sky);
            v3("synth_glow", frame.glow);
            v3("synth_grid", frame.grid);
            v3("synth_sun_top", frame.sun_top);
            v3("synth_sun_bottom", frame.sun_bottom);
            v4("synth_sun", frame.sun);
            v4("synth_stripes", frame.stripes);
            g.uniform_1_f32(u("synth_layers"), frame.layers);
            v4("synth_octaves", frame.octaves);
            g.uniform_1_f32(u("synth_octave4"), frame.octave4);
            g.uniform_matrix_4_f32_slice(u("synth_layer"), false, frame.layer.as_flattened());
            v4("synth_floor", frame.floor);
            v4("synth_lines", frame.lines);
            g.uniform_matrix_3_f32_slice(u("synth_colour"), false, &frame.colour);
            v4("synth_post", frame.post);
            g.uniform_1_f32(u("niri_alpha"), 1.);
            g.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        self.frames += 1;
        if self.fail_after.is_some_and(|n| self.frames > n) {
            bail!("NEWM_SYNTH_GPU_FAIL_AFTER: the swap fails, as a lost context would");
        }
        d.egl
            .swap_buffers(d.display, s.surface)
            .context("EGL swap")?;
        Ok(())
    }

    fn make_gl(&self) -> Result<Gl> {
        let egl = &self.display.egl;
        // SAFETY: the loader returns this context's GL entry points.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map_or(std::ptr::null(), |f| f as *const c_void)
            })
        };
        // SAFETY: GL object creation on the current context, checked for errors.
        unsafe {
            let compile = |kind, src: &str| -> Result<glow::Shader> {
                let s = gl.create_shader(kind).map_err(|e| anyhow!("{e}"))?;
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    bail!("shader: {}", gl.get_shader_info_log(s));
                }
                Ok(s)
            };
            let vs = compile(glow::VERTEX_SHADER, VERTEX)?;
            let fs = compile(
                glow::FRAGMENT_SHADER,
                &format!("#version 100\n{}", newm_synth::FRAGMENT_SHADER),
            )?;
            let program = gl.create_program().map_err(|e| anyhow!("{e}"))?;
            gl.attach_shader(program, vs);
            gl.attach_shader(program, fs);
            gl.bind_attrib_location(program, 0, "pos");
            gl.link_program(program);
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            if !gl.get_program_link_status(program) {
                bail!("program: {}", gl.get_program_info_log(program));
            }
            let uniforms = UNIFORMS
                .iter()
                .filter_map(|&n| Some((n, gl.get_uniform_location(program, n)?)))
                .collect();
            gl.use_program(Some(program));
            if let Some(l) = gl.get_uniform_location(program, "synth_tables") {
                gl.uniform_1_i32(Some(&l), 0);
            }
            // The triangle over the whole surface, left bound for every draw.
            let vbo = gl.create_buffer().map_err(|e| anyhow!("{e}"))?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let tri: [f32; 6] = [-1., -1., 3., -1., -1., 3.];
            let bytes: Vec<u8> = tri.iter().flat_map(|v| v.to_ne_bytes()).collect();
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            let texture = gl.create_texture().map_err(|e| anyhow!("{e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            Ok(Gl {
                gl,
                program,
                uniforms,
                texture,
                vbo,
                seed: None,
            })
        }
    }
}
