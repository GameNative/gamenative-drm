//! `EGL_EXT_image_dma_buf_import` presenter (doc `02` §7): imports each
//! committed dma-buf as an `EGLImage`, binds it to an external-OES texture,
//! blits fullscreen, `eglSwapBuffers` — swap back-pressure paces the loop.
//!
//! Per-frame `eglCreateImageKHR`/destroy (no cache) on purpose for bring-up:
//! correctness first; a cache needs an `RMFB` invalidation hook (phase 5).

use core::ffi::{c_char, c_void};
use std::os::fd::RawFd;

use gndrm_daemon::{Framebuffer, Presenter, Stores};

use crate::ffi;
use crate::{log_error, log_info, log_warn};

/// One frame's fence wait budget (a 30 Hz frame). On timeout we sample
/// anyway and log — stalling forever is worse than a torn frame.
const FENCE_TIMEOUT_MS: i32 = 33;

const VERTEX_SRC: &str = "attribute vec4 aPos;\n\
                          attribute vec2 aTex;\n\
                          varying vec2 vTex;\n\
                          void main() { gl_Position = aPos; vTex = aTex; }\n";

const FRAGMENT_SRC: &str = "#extension GL_OES_EGL_image_external : require\n\
                            precision mediump float;\n\
                            varying vec2 vTex;\n\
                            uniform samplerExternalOES sTex;\n\
                            void main() { gl_FragColor = texture2D(sTex, vTex); }\n";

/// Fullscreen triangle strip, interleaved `[x, y, u, v]`. `v = 0` at the
/// top: dma-buf row 0 is the top of the image (kernel-scanout convention).
const QUAD: [ffi::GLfloat; 16] = [
    -1.0, 1.0, 0.0, 0.0, // top-left
    -1.0, -1.0, 0.0, 1.0, // bottom-left
    1.0, 1.0, 1.0, 0.0, // top-right
    1.0, -1.0, 1.0, 1.0, // bottom-right
];

/// EGL/GLES2 presenter. Owned by the server thread: created, used and
/// dropped there (EGL thread affinity).
pub struct EglPresenter {
    display: ffi::EGLDisplay,
    context: ffi::EGLContext,
    surface: ffi::EGLSurface,
    create_image: ffi::FnEglCreateImageKhr,
    destroy_image: ffi::FnEglDestroyImageKhr,
    program: ffi::GLuint,
    texture: ffi::GLuint,
    attrib_pos: ffi::GLint,
    attrib_tex: ffi::GLint,
    warned_fence_timeout: bool,
}

impl EglPresenter {
    /// Bring up EGL14 + GLES2 on `window` and validate the dma-buf import
    /// extensions. Must be called on the thread that will present.
    pub fn new(window: *mut ffi::ANativeWindow, width: i32, height: i32) -> Result<Self, String> {
        // SAFETY: all EGL entry points are valid NDK symbols; `window` is a
        // live ANativeWindow owned by the caller (Runtime) for the whole
        // presenter lifetime.
        unsafe {
            let display = ffi::eglGetDisplay(core::ptr::null_mut());
            if display.is_null() {
                return Err("eglGetDisplay failed".into());
            }
            if ffi::eglInitialize(display, core::ptr::null_mut(), core::ptr::null_mut())
                == ffi::EGL_FALSE
            {
                return Err(format!("eglInitialize: {:#x}", ffi::eglGetError()));
            }

            // Client + display extension gate: dma-buf import must exist.
            let client_ext = query_string(core::ptr::null_mut(), ffi::EGL_EXTENSIONS);
            let display_ext = query_string(display, ffi::EGL_EXTENSIONS);
            let exts = format!("{client_ext} {display_ext}");
            for need in ["EGL_EXT_image_dma_buf_import", "EGL_KHR_image"] {
                if !exts.contains(need) {
                    ffi::eglTerminate(display);
                    return Err(format!("missing EGL extension: {need}"));
                }
            }

            let config_attribs = [
                ffi::EGL_SURFACE_TYPE,
                ffi::EGL_WINDOW_BIT,
                ffi::EGL_RENDERABLE_TYPE,
                ffi::EGL_OPENGL_ES2_BIT,
                ffi::EGL_RED_SIZE,
                8,
                ffi::EGL_GREEN_SIZE,
                8,
                ffi::EGL_BLUE_SIZE,
                8,
                ffi::EGL_ALPHA_SIZE,
                8,
                ffi::EGL_NONE,
            ];
            let mut config: ffi::EGLConfig = core::ptr::null_mut();
            let mut num: ffi::EGLint = 0;
            if ffi::eglChooseConfig(display, config_attribs.as_ptr(), &mut config, 1, &mut num)
                == ffi::EGL_FALSE
                || num == 0
            {
                ffi::eglTerminate(display);
                return Err(format!(
                    "eglChooseConfig(RGBA8888): {:#x}",
                    ffi::eglGetError()
                ));
            }

            let ctx_attribs = [ffi::EGL_CONTEXT_CLIENT_VERSION, 2, ffi::EGL_NONE];
            let context =
                ffi::eglCreateContext(display, config, core::ptr::null_mut(), ctx_attribs.as_ptr());
            if context.is_null() {
                ffi::eglTerminate(display);
                return Err(format!("eglCreateContext(ES2): {:#x}", ffi::eglGetError()));
            }

            let surface = ffi::eglCreateWindowSurface(display, config, window, core::ptr::null());
            if surface.is_null() {
                ffi::eglDestroyContext(display, context);
                ffi::eglTerminate(display);
                return Err(format!("eglCreateWindowSurface: {:#x}", ffi::eglGetError()));
            }
            if ffi::eglMakeCurrent(display, surface, surface, context) == ffi::EGL_FALSE {
                ffi::eglDestroySurface(display, surface);
                ffi::eglDestroyContext(display, context);
                ffi::eglTerminate(display);
                return Err(format!("eglMakeCurrent: {:#x}", ffi::eglGetError()));
            }
            let _ = ffi::eglSwapInterval(display, 1);

            let gl_ext = c_str_owned(ffi::glGetString(ffi::GL_EXTENSIONS).cast());
            if !gl_ext.contains("GL_OES_EGL_image_external") {
                return Err("missing GL extension: GL_OES_EGL_image_external".into());
            }

            let create_image: ffi::FnEglCreateImageKhr = load_proc(b"eglCreateImageKHR\0")?;
            let destroy_image: ffi::FnEglDestroyImageKhr = load_proc(b"eglDestroyImageKHR\0")?;

            let program = build_program()?;
            let attrib_pos = ffi::glGetAttribLocation(program, c"aPos".as_ptr());
            let attrib_tex = ffi::glGetAttribLocation(program, c"aTex".as_ptr());
            if attrib_pos < 0 || attrib_tex < 0 {
                return Err("blit program attribute lookup failed".into());
            }
            let sampler = ffi::glGetUniformLocation(program, c"sTex".as_ptr());
            ffi::glUseProgram(program);
            ffi::glUniform1i(sampler, 0);

            let mut texture = 0;
            ffi::glGenTextures(1, &mut texture);
            ffi::glViewport(0, 0, width, height);

            log_info!("EGL up: {width}x{height}, dma-buf import ready");
            Ok(Self {
                display,
                context,
                surface,
                create_image,
                destroy_image,
                program,
                texture,
                attrib_pos,
                attrib_tex,
                warned_fence_timeout: false,
            })
        }
    }

    /// Import + blit + swap one framebuffer.
    fn present_fb(&mut self, fb: &Framebuffer, stores: &Stores) -> Result<(), String> {
        let handle = fb.handles[0];
        let Some(gem) = stores.gems.get(&handle) else {
            return Err(format!("FB {} references unknown GEM {handle}", fb.id));
        };
        if fb.handles[1] != 0 {
            return Err(format!("FB {} is multi-plane, unsupported", fb.id));
        }

        // SAFETY: live EGL context current on this thread; `gem.fd` is a
        // valid dma-buf fd owned by the store (EGL dups it internally, the
        // store keeps ownership).
        unsafe {
            let attribs = [
                ffi::EGL_WIDTH,
                fb.width as ffi::EGLint,
                ffi::EGL_HEIGHT,
                fb.height as ffi::EGLint,
                ffi::EGL_LINUX_DRM_FOURCC_EXT,
                fb.pixel_format as ffi::EGLint,
                ffi::EGL_DMA_BUF_PLANE0_FD_EXT,
                gem.fd,
                ffi::EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                fb.offsets[0] as ffi::EGLint,
                ffi::EGL_DMA_BUF_PLANE0_PITCH_EXT,
                fb.pitches[0] as ffi::EGLint,
                ffi::EGL_NONE,
            ];
            let image = (self.create_image)(
                self.display,
                core::ptr::null_mut(),
                ffi::EGL_LINUX_DMA_BUF_EXT,
                core::ptr::null_mut(),
                attribs.as_ptr(),
            );
            if image.is_null() {
                return Err(format!(
                    "eglCreateImageKHR(fmt {:#x} {}x{}): {:#x}",
                    fb.pixel_format,
                    fb.width,
                    fb.height,
                    ffi::eglGetError()
                ));
            }

            ffi::glActiveTexture(ffi::GL_TEXTURE0);
            ffi::glBindTexture(ffi::GL_TEXTURE_EXTERNAL_OES, self.texture);
            ffi::glEGLImageTargetTexture2DOES(ffi::GL_TEXTURE_EXTERNAL_OES, image);

            ffi::glVertexAttribPointer(
                self.attrib_pos as ffi::GLuint,
                2,
                ffi::GL_FLOAT,
                ffi::GL_FALSE,
                16,
                QUAD.as_ptr().cast(),
            );
            ffi::glVertexAttribPointer(
                self.attrib_tex as ffi::GLuint,
                2,
                ffi::GL_FLOAT,
                ffi::GL_FALSE,
                16,
                QUAD.as_ptr().add(2).cast(),
            );
            ffi::glEnableVertexAttribArray(self.attrib_pos as ffi::GLuint);
            ffi::glEnableVertexAttribArray(self.attrib_tex as ffi::GLuint);
            ffi::glDrawArrays(ffi::GL_TRIANGLE_STRIP, 0, 4);
            ffi::glDisableVertexAttribArray(self.attrib_pos as ffi::GLuint);
            ffi::glDisableVertexAttribArray(self.attrib_tex as ffi::GLuint);

            let swap_ok = ffi::eglSwapBuffers(self.display, self.surface);
            (self.destroy_image)(self.display, image);

            if swap_ok == ffi::EGL_FALSE {
                return Err(format!("eglSwapBuffers: {:#x}", ffi::eglGetError()));
            }
        }
        Ok(())
    }
}

impl Presenter for EglPresenter {
    fn present(&mut self, fb: &Framebuffer, stores: &Stores, fence: Option<RawFd>) {
        if let Some(fd) = fence {
            let timed_out = wait_fence(fd);
            if timed_out && !self.warned_fence_timeout {
                self.warned_fence_timeout = true;
                log_warn!("acquire fence timed out; sampling anyway (logged once)");
            }
            // SAFETY: fence ownership transferred to the presenter.
            unsafe { libc::close(fd) };
        }
        if let Err(e) = self.present_fb(fb, stores) {
            log_error!("present fb {}: {e}", fb.id);
        }
    }
}

impl Drop for EglPresenter {
    fn drop(&mut self) {
        // SAFETY: runs on the owning server thread; all handles were
        // created from `display` and are still live.
        unsafe {
            ffi::glDeleteTextures(1, &self.texture);
            ffi::glDeleteProgram(self.program);
            ffi::eglMakeCurrent(
                self.display,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            );
            ffi::eglDestroySurface(self.display, self.surface);
            ffi::eglDestroyContext(self.display, self.context);
            ffi::eglTerminate(self.display);
        }
        log_info!("EGL torn down");
    }
}

// -- helpers ---------------------------------------------------------------

/// `poll(2)` the acquire fence readable (sync_file signals when the
/// producer's GPU work completes). Returns true on timeout.
fn wait_fence(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: valid stack pollfd; `fd` is a live fence.
    let r = unsafe { libc::poll(&mut pfd, 1, FENCE_TIMEOUT_MS) };
    r == 0
}

fn query_string(display: ffi::EGLDisplay, name: ffi::EGLint) -> String {
    // SAFETY: eglQueryString returns a static NUL-terminated string or null.
    unsafe { c_str_owned(ffi::eglQueryString(display, name)) }
}

fn c_str_owned(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: EGL/GL guarantee NUL-termination of returned strings.
    unsafe { core::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

/// Load an EGL extension function, failing with context. `name` is a
/// NUL-terminated byte string (`b"foo\0"`).
unsafe fn load_proc<T>(name: &'static [u8]) -> Result<T, String> {
    // SAFETY: name is NUL-terminated; eglGetProcAddress either returns the
    // entry point or null. Transmuting a null-checked fn pointer is the
    // documented EGL pattern.
    unsafe {
        let p = ffi::eglGetProcAddress(name.as_ptr().cast());
        if p.is_null() {
            return Err(format!(
                "eglGetProcAddress({}): null (eglGetError {:#x})",
                String::from_utf8_lossy(name),
                ffi::eglGetError()
            ));
        }
        Ok(core::mem::transmute_copy::<*mut c_void, T>(&p))
    }
}

unsafe fn compile_shader(ty: ffi::GLenum, src: &str) -> Result<ffi::GLuint, String> {
    // SAFETY: `src` outlives the glShaderSource call; info-log buffer is
    // sized by the driver-reported length.
    unsafe {
        let shader = ffi::glCreateShader(ty);
        let ptr = src.as_ptr().cast::<ffi::GLchar>();
        let len = src.len() as ffi::GLint;
        ffi::glShaderSource(shader, 1, &ptr, &len);
        ffi::glCompileShader(shader);
        let mut ok: ffi::GLint = 0;
        ffi::glGetShaderiv(shader, ffi::GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log_len: ffi::GLint = 0;
            ffi::glGetShaderiv(shader, ffi::GL_INFO_LOG_LENGTH, &mut log_len);
            let mut buf = vec![0u8; log_len.max(1) as usize];
            let mut written: ffi::GLsizei = 0;
            ffi::glGetShaderInfoLog(
                shader,
                buf.len() as ffi::GLsizei,
                &mut written,
                buf.as_mut_ptr().cast(),
            );
            ffi::glDeleteShader(shader);
            return Err(format!(
                "shader compile: {}",
                String::from_utf8_lossy(&buf[..written.max(0) as usize])
            ));
        }
        Ok(shader)
    }
}

unsafe fn build_program() -> Result<ffi::GLuint, String> {
    // SAFETY: shaders are valid, program lifecycle checked via GL status.
    unsafe {
        let vs = compile_shader(ffi::GL_VERTEX_SHADER, VERTEX_SRC)?;
        let fs = compile_shader(ffi::GL_FRAGMENT_SHADER, FRAGMENT_SRC)?;
        let program = ffi::glCreateProgram();
        ffi::glAttachShader(program, vs);
        ffi::glAttachShader(program, fs);
        ffi::glLinkProgram(program);
        ffi::glDeleteShader(vs);
        ffi::glDeleteShader(fs);
        let mut ok: ffi::GLint = 0;
        ffi::glGetProgramiv(program, ffi::GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut buf = [0u8; 512];
            let mut written: ffi::GLsizei = 0;
            ffi::glGetProgramInfoLog(
                program,
                buf.len() as ffi::GLsizei,
                &mut written,
                buf.as_mut_ptr().cast(),
            );
            ffi::glDeleteProgram(program);
            return Err(format!(
                "program link: {}",
                String::from_utf8_lossy(&buf[..written.max(0) as usize])
            ));
        }
        Ok(program)
    }
}
