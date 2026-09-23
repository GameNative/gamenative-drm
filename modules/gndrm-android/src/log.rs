//! `GN_DRM` logtag via `__android_log_write` (mirrors the app's
//! `GN_STEAM_DL` convention). Cold paths only — the per-frame hot path
//! never logs.

/// One log line; `fmt`/`args` go through `format!`.
#[macro_export]
macro_rules! alog {
    ($prio:expr, $($arg:tt)*) => {{
        let text = format!($($arg)*);
        // CString::new fails only on interior NUL, which format! output of
        // our messages never contains; fall back to a fixed string.
        let c = std::ffi::CString::new(text)
            .unwrap_or_else(|_| std::ffi::CString::new("<log error>").unwrap());
        // SAFETY: both pointers are NUL-terminated and outlive the call.
        // (allow: callers already inside an unsafe block would otherwise
        // trip `unused_unsafe` on the nested block)
        #[allow(unused_unsafe)]
        unsafe {
            $crate::ffi::__android_log_write($prio, b"GN_DRM\0".as_ptr().cast(), c.as_ptr());
        }
    }};
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_INFO, $($arg)*) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_WARN, $($arg)*) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::alog!($crate::ffi::ANDROID_LOG_ERROR, $($arg)*) };
}
