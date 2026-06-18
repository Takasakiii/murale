use std::ffi::c_void;
use std::sync::Arc;

use khronos_egl as egl;
use wayland_client::Proxy;
use wayland_egl::WlEglSurface;

use crate::error::MuraleError;

pub type EglInstance = egl::DynamicInstance<egl::EGL1_5>;

pub struct EglState {
    pub egl: Arc<EglInstance>,
    pub display: egl::Display,
    pub context: egl::Context,
    pub surface: egl::Surface,
    pub egl_window: WlEglSurface,
}

pub fn load_egl() -> Result<Arc<EglInstance>, MuraleError> {
    let lib = unsafe { libloading::Library::new("libEGL.so.1") }
        .map_err(|e| MuraleError::Egl(format!("failed to load libEGL.so.1: {e}")))?;
    let egl = unsafe { EglInstance::load_required_from(lib) }
        .map_err(|e| MuraleError::Egl(format!("failed to load EGL functions: {e}")))?;
    Ok(Arc::new(egl))
}

pub fn init_display(
    egl: &Arc<EglInstance>,
    wl_display_ptr: *mut c_void,
) -> Result<(egl::Display, egl::Config, egl::Context), MuraleError> {
    // eglGetDisplay is simpler than eglGetPlatformDisplay and
    // works reliably with NVIDIA's egl-wayland layer.
    let display = unsafe { egl.get_display(wl_display_ptr as egl::NativeDisplayType) }
        .ok_or_else(|| MuraleError::Egl("eglGetDisplay returned EGL_NO_DISPLAY".into()))?;

    egl.initialize(display)?;
    egl.bind_api(egl::OPENGL_API)?;

    let config_attribs = [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::NONE,
    ];

    let config = egl
        .choose_first_config(display, &config_attribs)?
        .ok_or_else(|| MuraleError::Egl("no suitable EGL config found".into()))?;

    let gl_versions = [
        (4, 6),
        (4, 5),
        (4, 4),
        (4, 3),
        (4, 2),
        (4, 1),
        (4, 0),
        (3, 3),
        (3, 2),
        (3, 1),
        (3, 0),
    ];

    let context = gl_versions
        .iter()
        .find_map(|&(major, minor)| {
            let ctx_attribs = [
                egl::CONTEXT_MAJOR_VERSION,
                major,
                egl::CONTEXT_MINOR_VERSION,
                minor,
                egl::NONE,
            ];
            egl.create_context(display, config, None, &ctx_attribs)
                .ok()
        })
        .ok_or_else(|| {
            MuraleError::Egl("failed to create OpenGL context (tried 4.6 down to 3.0)".into())
        })?;

    egl.make_current(display, None, None, Some(context))?;

    gl::load_with(|s| {
        egl.get_proc_address(s)
            .map(|f| f as *const c_void)
            .unwrap_or(std::ptr::null())
    });

    tracing::debug!("EGL initialized");

    Ok((display, config, context))
}

pub fn create_surface(
    egl: &Arc<EglInstance>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    wl_surface: &wayland_client::protocol::wl_surface::WlSurface,
    width: i32,
    height: i32,
) -> Result<EglState, MuraleError> {
    let egl_window = WlEglSurface::new(wl_surface.id(), width, height)
        .map_err(|_| MuraleError::Egl("failed to create wl_egl_window".into()))?;

    let surface = unsafe {
        egl.create_window_surface(display, config, egl_window.ptr() as _, None)
    }?;

    egl.make_current(display, Some(surface), Some(surface), Some(context))?;
    egl.swap_interval(display, 0)?;

    // NVIDIA workaround: force GL_BACK draw buffer
    unsafe {
        gl::DrawBuffer(gl::BACK);
        gl::ClearColor(0.0, 0.0, 0.0, 0.0);
    }

    tracing::debug!("EGL surface created ({width}x{height})");

    Ok(EglState {
        egl: egl.clone(),
        display,
        context,
        surface,
        egl_window,
    })
}

pub fn mpv_get_proc_address(egl: &Arc<EglInstance>, name: &str) -> *mut c_void {
    egl.get_proc_address(name)
        .map(|f| f as *mut c_void)
        .unwrap_or(std::ptr::null_mut())
}

impl Drop for EglState {
    fn drop(&mut self) {
        tracing::debug!("destroying EGL surface");
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
    }
}
