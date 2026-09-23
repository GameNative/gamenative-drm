//! Hand-rolled FFI surface for the Android system libraries we link:
//! `liblog`, `libandroid` (`ANativeWindow`), `libEGL` (+ the
//! `EGL_EXT_image_dma_buf_import` entry points), `libGLESv2`.
//!
//! Only what the presenter uses is declared. Constants come from the NDK
//! headers (`android/log.h`, `EGL/egl.h`, `EGL/eglext.h`, `GLES2/gl2.h`,
//! `GLES2/gl2ext.h`).

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use core::ffi::{c_char, c_int, c_uchar, c_uint, c_void};

// ---------------------------------------------------------------- liblog
pub const ANDROID_LOG_INFO: c_int = 4;
pub const ANDROID_LOG_WARN: c_int = 5;
pub const ANDROID_LOG_ERROR: c_int = 6;

#[link(name = "log")]
unsafe extern "C" {
    pub fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

// ------------------------------------------------------------ libandroid
/// Opaque `ANativeWindow`.
#[repr(C)]
pub struct ANativeWindow {
    _private: [u8; 0],
}

#[link(name = "android")]
unsafe extern "C" {
    /// Acquires a reference on the window; release with
    /// [`ANativeWindow_release`].
    pub fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jni::sys::jobject,
    ) -> *mut ANativeWindow;
    pub fn ANativeWindow_release(window: *mut ANativeWindow);
}

// ------------------------------------------------------------------ EGL
pub type EGLBoolean = c_uint;
pub type EGLint = c_int;
pub type EGLenum = c_uint;
pub type EGLDisplay = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLClientBuffer = *mut c_void;
pub type EGLImageKHR = *mut c_void;

pub const EGL_FALSE: EGLBoolean = 0;
pub const EGL_NONE: EGLint = 0x3038;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_HEIGHT: EGLint = 0x3056;
pub const EGL_RED_SIZE: EGLint = 0x3024;
pub const EGL_GREEN_SIZE: EGLint = 0x3023;
pub const EGL_BLUE_SIZE: EGLint = 0x3022;
pub const EGL_ALPHA_SIZE: EGLint = 0x3021;
pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
pub const EGL_SURFACE_TYPE: EGLint = 0x3033;
pub const EGL_WINDOW_BIT: EGLint = 0x0004;
pub const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
pub const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
pub const EGL_EXTENSIONS: EGLint = 0x3055;
pub const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3273;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3274;

pub type FnEglCreateImageKhr = unsafe extern "C" fn(
    dpy: EGLDisplay,
    ctx: EGLContext,
    target: EGLenum,
    buffer: EGLClientBuffer,
    attrib_list: *const EGLint,
) -> EGLImageKHR;
pub type FnEglDestroyImageKhr =
    unsafe extern "C" fn(dpy: EGLDisplay, image: EGLImageKHR) -> EGLBoolean;

#[link(name = "EGL")]
unsafe extern "C" {
    pub fn eglGetDisplay(display_id: *mut c_void) -> EGLDisplay;
    pub fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    pub fn eglQueryString(dpy: EGLDisplay, name: EGLint) -> *const c_char;
    pub fn eglChooseConfig(
        dpy: EGLDisplay,
        attrib_list: *const EGLint,
        configs: *mut EGLConfig,
        config_size: EGLint,
        num_config: *mut EGLint,
    ) -> EGLBoolean;
    pub fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share_context: EGLContext,
        attrib_list: *const EGLint,
    ) -> EGLContext;
    pub fn eglCreateWindowSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        win: *mut ANativeWindow,
        attrib_list: *const EGLint,
    ) -> EGLSurface;
    pub fn eglMakeCurrent(
        dpy: EGLDisplay,
        draw: EGLSurface,
        read: EGLSurface,
        ctx: EGLContext,
    ) -> EGLBoolean;
    pub fn eglSwapBuffers(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    pub fn eglSwapInterval(dpy: EGLDisplay, interval: EGLint) -> EGLBoolean;
    pub fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    pub fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    pub fn eglTerminate(dpy: EGLDisplay) -> EGLBoolean;
    pub fn eglGetError() -> EGLint;
    pub fn eglGetProcAddress(procname: *const c_char) -> *mut c_void;
}

// --------------------------------------------------------------- GLESv2
pub type GLenum = c_uint;
pub type GLboolean = c_uchar;
pub type GLbitfield = c_uint;
pub type GLint = c_int;
pub type GLuint = c_uint;
pub type GLsizei = c_int;
pub type GLfloat = f32;
pub type GLchar = c_char;

pub const GL_TEXTURE_EXTERNAL_OES: GLenum = 0x8D65;
pub const GL_TEXTURE0: GLenum = 0x84C0;
pub const GL_TRIANGLE_STRIP: GLenum = 0x0005;
pub const GL_FLOAT: GLenum = 0x1406;
pub const GL_FALSE: GLboolean = 0;
pub const GL_VERTEX_SHADER: GLenum = 0x8B31;
pub const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
pub const GL_COMPILE_STATUS: GLenum = 0x8B81;
pub const GL_LINK_STATUS: GLenum = 0x8B82;
pub const GL_INFO_LOG_LENGTH: GLenum = 0x8B84;
pub const GL_EXTENSIONS: GLenum = 0x1F03;

#[link(name = "GLESv2")]
unsafe extern "C" {
    pub fn glGetString(name: GLenum) -> *const c_uchar;
    pub fn glGenTextures(n: GLsizei, textures: *mut GLuint);
    pub fn glBindTexture(target: GLenum, texture: GLuint);
    pub fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
    /// `GL_OES_EGL_image_external`; exported by Android's `libGLESv2` stub
    /// since API 1 (SurfaceTexture), safe to link directly.
    pub fn glEGLImageTargetTexture2DOES(target: GLenum, image: *mut c_void);
    pub fn glCreateShader(ty: GLenum) -> GLuint;
    pub fn glShaderSource(
        shader: GLuint,
        count: GLsizei,
        string: *const *const GLchar,
        length: *const GLint,
    );
    pub fn glCompileShader(shader: GLuint);
    pub fn glGetShaderiv(shader: GLuint, pname: GLenum, params: *mut GLint);
    pub fn glGetShaderInfoLog(
        shader: GLuint,
        buf_size: GLsizei,
        length: *mut GLsizei,
        info_log: *mut GLchar,
    );
    pub fn glDeleteShader(shader: GLuint);
    pub fn glCreateProgram() -> GLuint;
    pub fn glAttachShader(program: GLuint, shader: GLuint);
    pub fn glLinkProgram(program: GLuint);
    pub fn glGetProgramiv(program: GLuint, pname: GLenum, params: *mut GLint);
    pub fn glGetProgramInfoLog(
        program: GLuint,
        buf_size: GLsizei,
        length: *mut GLsizei,
        info_log: *mut GLchar,
    );
    pub fn glUseProgram(program: GLuint);
    pub fn glDeleteProgram(program: GLuint);
    pub fn glGetAttribLocation(program: GLuint, name: *const GLchar) -> GLint;
    pub fn glGetUniformLocation(program: GLuint, name: *const GLchar) -> GLint;
    pub fn glUniform1i(location: GLint, v0: GLint);
    pub fn glEnableVertexAttribArray(index: GLuint);
    pub fn glDisableVertexAttribArray(index: GLuint);
    pub fn glVertexAttribPointer(
        index: GLuint,
        size: GLint,
        ty: GLenum,
        normalized: GLboolean,
        stride: GLsizei,
        pointer: *const c_void,
    );
    pub fn glActiveTexture(texture: GLenum);
    pub fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
    pub fn glViewport(x: GLint, y: GLint, width: GLsizei, height: GLsizei);
}
