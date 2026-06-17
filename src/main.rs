mod egl;
mod error;

use std::ffi::c_void;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use khronos_egl as kegl;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use libmpv2::Mpv;
use nix::poll::{PollFd, PollFlags, PollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, QueueHandle};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn render_nonblocking(
    ctx: *mut libmpv2_sys::mpv_render_context,
    w: i32,
    h: i32,
) -> i32 {
    let mut fbo = libmpv2_sys::mpv_opengl_fbo {
        fbo: 0,
        w,
        h,
        internal_format: 0,
    };
    let mut flip: i32 = 1;
    let mut block: i32 = 0;
    let mut params = [
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
            data: &mut fbo as *mut _ as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
            data: &mut flip as *mut _ as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
            data: &mut block as *mut _ as *mut c_void,
        },
        libmpv2_sys::mpv_render_param {
            type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    unsafe { libmpv2_sys::mpv_render_context_render(ctx, params.as_mut_ptr()) }
}

// --- Frame timing instrumentation ---

struct FrameStats {
    intervals_ms: Vec<f64>,
    last_frame: Option<Instant>,
    last_report: Instant,
}

impl FrameStats {
    fn new() -> Self {
        Self {
            intervals_ms: Vec::with_capacity(2048),
            last_frame: None,
            last_report: Instant::now(),
        }
    }

    fn record(&mut self, now: Instant) {
        if let Some(prev) = self.last_frame {
            let dt = now.duration_since(prev).as_secs_f64() * 1000.0;
            self.intervals_ms.push(dt);
        }
        self.last_frame = Some(now);
    }

    fn should_report(&self) -> bool {
        self.last_report.elapsed().as_secs() >= 5 && self.intervals_ms.len() >= 2
    }

    fn take_stats(&mut self) -> (f64, f64, f64, f64) {
        let n = self.intervals_ms.len() as f64;
        let sum: f64 = self.intervals_ms.iter().sum();
        let avg = sum / n;
        let min = self.intervals_ms.iter().cloned().reduce(f64::min).unwrap();
        let max = self.intervals_ms.iter().cloned().reduce(f64::max).unwrap();
        let variance: f64 =
            self.intervals_ms.iter().map(|dt| (dt - avg).powi(2)).sum::<f64>() / n;
        let jitter = variance.sqrt();

        self.intervals_ms.clear();
        self.last_report = Instant::now();

        (avg, min, max, jitter)
    }
}

#[derive(Parser)]
#[command(
    about = "Lean, memory-safe video wallpaper player for Wayland compositors",
    after_help = "\
EXAMPLES:
    murale ~/Videos/wallpaper.mp4
    murale ~/Videos/wallpaper.mp4 --output DP-2
    murale ~/Videos/wallpaper.mp4 --mpv-options \"input-ipc-server=/tmp/murale.sock\"
    murale ~/Videos/wallpaper.mp4 --stats"
)]
struct Cli {
    /// Path to video file (any format mpv supports)
    video: String,

    /// Target a specific display output by name (e.g. DP-1, HDMI-A-1).
    /// Run without this flag to see available outputs listed in the log.
    #[arg(short, long)]
    output: Option<String>,

    /// Pass options to mpv as comma-separated key=value pairs.
    /// Example: --mpv-options "input-ipc-server=/tmp/murale.sock,volume=0"
    #[arg(long, value_name = "OPTIONS")]
    mpv_options: Option<String>,

    /// Log frame timing and mpv playback stats every 5 seconds
    #[arg(long)]
    stats: bool,
}

struct Murale {
    // SCTK state
    registry_state: RegistryState,
    output_state: OutputState,
    qh: QueueHandle<Self>,

    // Layer surface — None until after output discovery roundtrip
    layer: Option<LayerSurface>,

    // EGL — display/config/context are duplicated here and inside EglState.
    // TODO: consolidate when adding monitor hotplug (EglState recreation needs
    // the display/config/context to survive surface teardown).
    egl_instance: Arc<egl::EglInstance>,
    egl_state: Option<egl::EglState>,
    egl_display: kegl::Display,
    egl_config: kegl::Config,
    egl_context: kegl::Context,

    // SAFETY: render_ctx MUST be declared before mpv — see transmute comment in init_mpv().
    render_ctx: Option<RenderContext<'static>>,
    mpv: Option<Mpv>,

    // mpv wakeup eventfd
    wakeup_fd: EventFd,

    // Frame state
    first_configure: bool,
    width: u32,
    height: u32,
    scale: u32,
    frame_callback_pending: bool,
    redraw_needed: bool,
    frame_count: u64,

    // Instrumentation
    stats_enabled: bool,
    frame_stats: FrameStats,

    // Control
    exit: bool,
    video_path: String,
    mpv_options: Option<String>,

    // Raw wl_display pointer for mpv
    wl_display_ptr: *mut c_void,
}

impl Murale {
    fn layer(&self) -> &LayerSurface {
        self.layer.as_ref().expect("layer surface not initialized")
    }

    fn render_frame(&mut self) {
        let egl_state = match &self.egl_state {
            Some(s) => s,
            None => return,
        };
        let render_ctx = match &self.render_ctx {
            Some(ctx) => ctx,
            None => return,
        };
        let layer = match &self.layer {
            Some(l) => l,
            None => return,
        };

        let w = (self.width * self.scale) as i32;
        let h = (self.height * self.scale) as i32;

        if let Err(e) = self.egl_instance.make_current(
            egl_state.display,
            Some(egl_state.surface),
            Some(egl_state.surface),
            Some(egl_state.context),
        ) {
            tracing::error!("eglMakeCurrent failed: {e:?}");
            return;
        }

        // NVIDIA workaround: proprietary drivers leave GL_NONE after eglMakeCurrent
        unsafe {
            gl::DrawBuffer(gl::BACK);
            gl::Viewport(0, 0, w, h);
        }

        // Call mpv_render_context_render directly with BLOCK_FOR_TARGET_TIME=0.
        // libmpv2's safe render() omits this param, causing mpv to block until
        // PTS time — unacceptable at 240Hz where the compositor delivers frame
        // callbacks every 4.17ms.
        let err = render_nonblocking(render_ctx.ctx, w, h);
        if err < 0 {
            tracing::error!("mpv render failed: {err}");
            return;
        }

        self.frame_count += 1;

        if self.stats_enabled {
            self.frame_stats.record(Instant::now());
            if self.frame_stats.should_report() {
                let (avg, min, max, jitter) = self.frame_stats.take_stats();
                self.log_combined_stats(avg, min, max, jitter);
            }
        }

        // Register next frame callback before swap
        layer
            .wl_surface()
            .frame(&self.qh, layer.wl_surface().clone());
        self.frame_callback_pending = true;
        self.redraw_needed = false;

        // eglSwapBuffers commits the surface internally on Wayland — no explicit commit needed.
        // If swap fails, no buffer reaches the compositor and the frame callback will never
        // fire. Reset frame_callback_pending so the next mpv wakeup retries rendering.
        if let Err(e) = self.egl_instance.swap_buffers(egl_state.display, egl_state.surface) {
            tracing::error!("eglSwapBuffers failed: {e:?}");
            self.frame_callback_pending = false;
        }
    }

    fn log_combined_stats(&self, avg: f64, min: f64, max: f64, jitter: f64) {
        let mpv = match &self.mpv {
            Some(m) => m,
            None => return,
        };
        let fps: f64 = mpv.get_property("estimated-vf-fps").unwrap_or(0.0);
        let delayed: i64 = mpv.get_property("vo-delayed-frame-count").unwrap_or(-1);
        let vo_drops: i64 = mpv.get_property("frame-drop-count").unwrap_or(-1);
        let dec_drops: i64 = mpv.get_property("decoder-frame-drop-count").unwrap_or(-1);
        let vsync_ratio: f64 = mpv.get_property("vsync-ratio").unwrap_or(0.0);
        tracing::info!(
            "[stats] fps={fps:.1} delayed={delayed} vo_drops={vo_drops} dec_drops={dec_drops} vsync_ratio={vsync_ratio:.2} | interval avg={avg:.2}ms min={min:.2}ms max={max:.2}ms jitter={jitter:.2}ms"
        );
    }

    fn handle_mpv_wakeup(&mut self) {
        let _ = self.wakeup_fd.read();

        let render_ctx = match &self.render_ctx {
            Some(ctx) => ctx,
            None => return,
        };

        let flags = match render_ctx.update() {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("mpv render update failed: {e}");
                return;
            }
        };

        if flags & libmpv2::render::mpv_render_update::Frame != 0 {
            if self.frame_callback_pending {
                self.redraw_needed = true;
            } else {
                self.render_frame();
            }
        }
    }

    fn init_mpv(&mut self) -> Result<()> {
        let mpv = Mpv::new()?;

        mpv.set_property("vo", "libmpv")?;
        mpv.set_property("hwdec", "auto")?;
        mpv.set_property("video-sync", "display-resample")?;
        mpv.set_property("loop-file", "inf")?;
        mpv.set_property("keepaspect", true)?;
        mpv.set_property("panscan", 1.0f64)?;
        mpv.set_property("background-color", "#00000000")?;
        mpv.set_property("terminal", "no")?;

        if let Some(opts) = &self.mpv_options {
            for opt in opts.split(',') {
                let opt = opt.trim();
                if opt.is_empty() {
                    continue;
                }
                if let Some((key, value)) = opt.split_once('=') {
                    if let Err(e) = mpv.set_property(key.trim(), value.trim()) {
                        tracing::warn!("failed to set mpv option {key}={value}: {e}");
                    }
                } else {
                    tracing::warn!("invalid mpv option (expected key=value): {opt}");
                }
            }
        }

        let init_params = OpenGLInitParams {
            get_proc_address: egl::mpv_get_proc_address,
            ctx: self.egl_instance.clone(),
        };

        let render_params = [
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(init_params),
            RenderParam::WaylandDisplay(self.wl_display_ptr),
        ];

        let mut render_ctx = mpv.create_render_context(render_params)?;

        let wakeup_fd_raw = self.wakeup_fd.as_raw_fd();
        render_ctx.set_update_callback(move || {
            let buf = 1u64.to_ne_bytes();
            let _ = nix::unistd::write(unsafe { BorrowedFd::borrow_raw(wakeup_fd_raw) }, &buf);
        });

        mpv.command("loadfile", &[&self.video_path])?;

        // SAFETY: RenderContext<'a> borrows Mpv, but we need both in the same struct.
        // This transmute erases the lifetime to 'static. It is sound because:
        //   1. render_ctx is declared BEFORE mpv in the struct
        //   2. Rust drops fields in declaration order (top to bottom)
        //   3. Therefore render_ctx always drops before mpv
        //   4. Murale is never moved after init_mpv (lives on the stack in run())
        //   5. RenderContext internally holds a raw *mut mpv_render_context (C pointer),
        //      not a Rust &Mpv reference — the PhantomData lifetime is purely a
        //      borrow-checker hint to enforce drop ordering, which we handle manually
        // DO NOT reorder the render_ctx/mpv fields without updating this invariant.
        let render_ctx: RenderContext<'static> = unsafe { std::mem::transmute(render_ctx) };

        self.render_ctx = Some(render_ctx);
        self.mpv = Some(mpv);

        tracing::info!("playing {}", self.video_path);
        Ok(())
    }

    fn find_target_output(&self, name: &str) -> Option<wl_output::WlOutput> {
        for output in self.output_state.outputs() {
            if let Some(info) = self.output_state.info(&output) {
                if info.name.as_deref() == Some(name) {
                    tracing::info!("target output matched: {name}");
                    return Some(output);
                }
            }
        }
        tracing::warn!("output '{name}' not found, available outputs:");
        for output in self.output_state.outputs() {
            if let Some(info) = self.output_state.info(&output) {
                tracing::warn!("  {:?}", info.name.as_deref().unwrap_or("(unnamed)"));
            }
        }
        None
    }
}

// --- SCTK handler implementations ---

impl CompositorHandler for Murale {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor as u32;
        tracing::debug!("scale factor changed to {new_factor}");
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.frame_callback_pending = false;
        if self.redraw_needed {
            self.render_frame();
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Murale {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Murale {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        tracing::info!("layer surface closed by compositor");
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let w = configure.new_size.0;
        let h = configure.new_size.1;

        if w == 0 || h == 0 {
            return;
        }

        self.width = w;
        self.height = h;
        tracing::debug!("configure: {w}x{h} scale={}", self.scale);

        if self.first_configure {
            self.first_configure = false;

            let scaled_w = (w * self.scale) as i32;
            let scaled_h = (h * self.scale) as i32;

            match egl::create_surface(
                &self.egl_instance,
                self.egl_display,
                self.egl_config,
                self.egl_context,
                self.layer().wl_surface(),
                scaled_w,
                scaled_h,
            ) {
                Ok(state) => self.egl_state = Some(state),
                Err(e) => {
                    tracing::error!("failed to create EGL surface: {e}");
                    self.exit = true;
                    return;
                }
            }

            if let Err(e) = self.init_mpv() {
                tracing::error!("failed to initialize mpv: {e}");
                self.exit = true;
                return;
            }

            self.render_frame();
        } else if let Some(egl_state) = &self.egl_state {
            let scaled_w = (w * self.scale) as i32;
            let scaled_h = (h * self.scale) as i32;
            egl_state.egl_window.resize(scaled_w, scaled_h, 0, 0);
        }
    }
}

delegate_compositor!(Murale);
delegate_output!(Murale);
delegate_layer!(Murale);
delegate_registry!(Murale);

impl ProvidesRegistryState for Murale {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    unsafe {
        libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
    }

    let conn = Connection::connect_to_env().context(
        "could not connect to Wayland compositor (is a Wayland session running?)",
    )?;
    let (globals, mut event_queue) = registry_queue_init(&conn)
        .map_err(|e| anyhow::anyhow!("Wayland registry init failed: {e}"))?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .context("compositor does not support wl_compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context(
        "compositor does not support wlr-layer-shell (required for wallpaper surfaces)",
    )?;
    let output_state = OutputState::new(&globals, &qh);

    let wl_display_ptr = conn.backend().display_ptr() as *mut c_void;

    let egl_instance = egl::load_egl()?;
    let (egl_display, egl_config, egl_context) =
        egl::init_display(&egl_instance, wl_display_ptr)?;

    let wakeup_fd = EventFd::from_flags(
        EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_SEMAPHORE,
    )?;

    let mut state = Murale {
        registry_state: RegistryState::new(&globals),
        output_state,
        qh: qh.clone(),
        layer: None,
        egl_instance,
        egl_state: None,
        egl_display,
        egl_config,
        egl_context,
        render_ctx: None,
        mpv: None,
        wakeup_fd,
        first_configure: true,
        width: 0,
        height: 0,
        scale: 1,
        frame_callback_pending: false,
        redraw_needed: false,
        frame_count: 0,
        stats_enabled: cli.stats,
        frame_stats: FrameStats::new(),
        exit: false,
        video_path: cli.video,
        mpv_options: cli.mpv_options,
        wl_display_ptr,
    };

    event_queue.roundtrip(&mut state)?;

    let target_output = cli
        .output
        .as_deref()
        .and_then(|name| state.find_target_output(name));

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Background,
        Some("murale"),
        target_output.as_ref(),
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(0, 0);
    layer.commit();

    state.layer = Some(layer);

    let wl_fd = conn.as_fd();
    tracing::debug!("entering main loop");

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            tracing::info!("received shutdown signal");
            break;
        }

        let mut mpv_ready = false;

        {
            let read_guard = event_queue.prepare_read();
            conn.flush()?;

            match read_guard {
                Some(guard) => {
                    let mut pollfds = [
                        PollFd::new(wl_fd, PollFlags::POLLIN),
                        PollFd::new(state.wakeup_fd.as_fd(), PollFlags::POLLIN),
                    ];

                    match nix::poll::poll(&mut pollfds, PollTimeout::from(16u16)) {
                        Ok(_) => {}
                        Err(nix::errno::Errno::EINTR) => continue,
                        Err(e) => return Err(anyhow::anyhow!("poll failed: {e}")),
                    }

                    if pollfds[0].any().unwrap_or(false) {
                        guard.read()?;
                    }

                    mpv_ready = pollfds[1].any().unwrap_or(false);
                }
                None => {}
            }
        }

        // Always check mpv wakeups — EFD_NONBLOCK returns EAGAIN instantly if empty.
        // This catches signals that arrive when prepare_read() returns None
        // (buffered Wayland events skip the poll block entirely).
        if !mpv_ready && state.wakeup_fd.read().is_ok() {
            mpv_ready = true;
        }

        event_queue.dispatch_pending(&mut state)?;

        if mpv_ready {
            state.handle_mpv_wakeup();
        }

        if state.exit {
            break;
        }
    }

    tracing::info!("shutting down");
    state.render_ctx.take();
    state.mpv.take();
    state.egl_state.take();

    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "murale=info".parse().unwrap()),
        )
        .init();

    if let Err(e) = run() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}
