//! The session's scan-out pool layout ([`ScanoutLayout`]).
//!
//! `BGRA_8888` is the default. Android 16's Skia GLES render engine tags a
//! `BGRA_8888` buffer as `kBGRA_8888`, which GLES cannot pair with the RGBA8
//! external texture it wraps the buffer in, so `SurfaceFlinger` aborts as soon
//! as it has to draw the layer on the GPU. Those devices get `RGBA_8888`; the
//! Vulkan and Graphite render engines, and earlier releases, keep `BGRA_8888`.
//! `GNDRM_SCANOUT_LAYOUT` overrides the rule.
//!
//! The rule and the aconfig decoder are Android-free and have their unit tests
//! run on the host by `tests/layout.rs`; only the property and file reads below
//! them are Android-only.

use gndrm_uapi::wire::ScanoutLayout;

/// First SDK level whose Skia GLES render engine cannot wrap `BGRA_8888`.
const SDK_GLES_BGRA_ABORT: u32 = 36;

/// The aconfig package of the render-engine flags.
const SF_FLAGS_PACKAGE: &str = "com.android.graphics.surfaceflinger.flags";
const FLAG_VULKAN: &str = "vulkan_renderengine";
const FLAG_GRAPHITE: &str = "graphite_renderengine";

/// `parsed_flag.state` value for an enabled flag.
const FLAG_STATE_ENABLED: u64 = 1;

/// The render engine `SurfaceFlinger` composes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderEngine {
    Gles,
    Vulkan,
    Graphite,
    Unknown,
}

impl RenderEngine {
    fn name(self) -> &'static str {
        match self {
            Self::Gles => "gles",
            Self::Vulkan => "vulkan",
            Self::Graphite => "graphite",
            Self::Unknown => "unknown",
        }
    }
}

/// The render engine named by `debug.renderengine.backend` (`backend`), or, when
/// that is unset or empty, by the aconfig flags.
pub(crate) fn render_engine(backend: &str, vulkan_flag: bool, graphite_flag: bool) -> RenderEngine {
    match backend {
        "skiagl" | "skiaglthreaded" => RenderEngine::Gles,
        "skiavk" | "skiavkthreaded" => RenderEngine::Vulkan,
        "graphite" => RenderEngine::Graphite,
        "" if graphite_flag => RenderEngine::Graphite,
        "" if vulkan_flag => RenderEngine::Vulkan,
        "" => RenderEngine::Gles,
        _ => RenderEngine::Unknown,
    }
}

/// Where a session's layout came from, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Env,
    Rule { sdk: u32, re: RenderEngine },
}

/// A session's layout and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    pub layout: ScanoutLayout,
    pub source: Source,
}

impl core::fmt::Display for Decision {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let layout = self.layout.as_env_value();
        match self.source {
            Source::Env => write!(f, "{}", format_args!("{layout} (env)")),
            Source::Rule { sdk, re } => write!(
                f,
                "{}",
                format_args!("{layout} (rule: sdk={sdk} re={})", re.name())
            ),
        }
    }
}

/// The layout for `env` (the parsed `GNDRM_SCANOUT_LAYOUT`), the SDK level and
/// the render engine.
pub(crate) fn decide(env: Option<ScanoutLayout>, sdk: u32, re: RenderEngine) -> Decision {
    if let Some(layout) = env {
        return Decision {
            layout,
            source: Source::Env,
        };
    }
    let layout = if sdk >= SDK_GLES_BGRA_ABORT && re == RenderEngine::Gles {
        ScanoutLayout::Rgba8888
    } else {
        ScanoutLayout::Bgra8888
    };
    Decision {
        layout,
        source: Source::Rule { sdk, re },
    }
}

/// Read one varint at `*pos`.
fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// One field of a protobuf message.
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

/// Read the field at `*pos`: `(number, value)`. `None` at the end of `buf` or
/// on a malformed or group-encoded field.
fn field<'a>(buf: &'a [u8], pos: &mut usize) -> Option<(u64, Field<'a>)> {
    let key = varint(buf, pos)?;
    let value = match key & 7 {
        0 => Field::Varint(varint(buf, pos)?),
        1 | 5 => {
            let len = if key & 7 == 1 { 8 } else { 4 };
            buf.get(*pos..*pos + len)?;
            *pos += len;
            Field::Other
        }
        2 => {
            let len = usize::try_from(varint(buf, pos)?).ok()?;
            let bytes = buf.get(*pos..pos.checked_add(len)?)?;
            *pos += len;
            Field::Bytes(bytes)
        }
        _ => return None,
    };
    Some((key >> 3, value))
}

/// Whether an `aconfig_flags.pb` (`parsed_flags`) lists `package`.`name` as
/// `ENABLED`.
pub(crate) fn aconfig_flag_enabled(buf: &[u8], package: &str, name: &str) -> bool {
    let mut pos = 0;
    while let Some((number, value)) = field(buf, &mut pos) {
        let (1, Field::Bytes(flag)) = (number, value) else {
            continue;
        };
        let (mut pkg, mut flag_name, mut state) = (None, None, None);
        let mut fpos = 0;
        while let Some((n, v)) = field(flag, &mut fpos) {
            match (n, v) {
                (1, Field::Bytes(b)) => pkg = Some(b),
                (2, Field::Bytes(b)) => flag_name = Some(b),
                (6, Field::Varint(s)) => state = Some(s),
                _ => {}
            }
        }
        if pkg == Some(package.as_bytes())
            && flag_name == Some(name.as_bytes())
            && state == Some(FLAG_STATE_ENABLED)
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "android")]
mod device {
    use super::*;
    use std::sync::{Mutex, OnceLock, PoisonError};

    const ACONFIG_FILES: [&str; 4] = [
        "/system/etc/aconfig_flags.pb",
        "/system_ext/etc/aconfig_flags.pb",
        "/product/etc/aconfig_flags.pb",
        "/vendor/etc/aconfig_flags.pb",
    ];

    /// A system property, or an empty string when it is unset.
    fn property(name: &str) -> String {
        let Ok(name) = std::ffi::CString::new(name) else {
            return String::new();
        };
        let mut value = [0u8; 92];
        // SAFETY: `name` is NUL-terminated and `value` is PROP_VALUE_MAX bytes.
        let n = unsafe {
            libc::__system_property_get(name.as_ptr(), value.as_mut_ptr().cast::<libc::c_char>())
        };
        let n = usize::try_from(n).unwrap_or(0).min(value.len());
        String::from_utf8_lossy(&value[..n]).trim().to_owned()
    }

    /// Whether the render-engine flag `name` is enabled in any aconfig file or
    /// by its `persist.device_config` override.
    fn flag_enabled(files: &[Vec<u8>], name: &str) -> bool {
        let prop = format!("persist.device_config.aconfig_flags.surfaceflinger.{name}");
        let qualified =
            format!("persist.device_config.aconfig_flags.surfaceflinger.{SF_FLAGS_PACKAGE}.{name}");
        property(&prop) == "true"
            || property(&qualified) == "true"
            || files
                .iter()
                .any(|buf| aconfig_flag_enabled(buf, SF_FLAGS_PACKAGE, name))
    }

    /// The SDK level and render engine, read once per process.
    fn device_state() -> (u32, RenderEngine) {
        static STATE: OnceLock<(u32, RenderEngine)> = OnceLock::new();
        *STATE.get_or_init(|| {
            let sdk = property("ro.build.version.sdk").parse().unwrap_or(0);
            let backend = property("debug.renderengine.backend");
            let re = if backend.is_empty() {
                let files: Vec<Vec<u8>> = ACONFIG_FILES
                    .iter()
                    .filter_map(|p| std::fs::read(p).ok())
                    .collect();
                render_engine(
                    "",
                    flag_enabled(&files, FLAG_VULKAN),
                    flag_enabled(&files, FLAG_GRAPHITE),
                )
            } else {
                render_engine(&backend, false, false)
            };
            (sdk, re)
        })
    }

    static DECISION: Mutex<Option<Decision>> = Mutex::new(None);

    /// The session's layout, resolved on first use and cached until [`reset`].
    pub(crate) fn session_layout() -> ScanoutLayout {
        let mut slot = DECISION.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(d) = *slot {
            return d.layout;
        }
        let (sdk, re) = device_state();
        let d = decide(gndrm_daemon::scanout_layout_from_env(), sdk, re);
        crate::log_always!("{}", format_args!("gndrm-daemon: scan-out layout {d}"));
        *slot = Some(d);
        d.layout
    }

    /// Drop the cached layout so the next call re-reads the environment.
    pub(crate) fn reset() {
        *DECISION.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

#[cfg(target_os = "android")]
pub(crate) use device::{reset, session_layout};

#[cfg(test)]
mod tests {
    use super::*;

    const BGRA: ScanoutLayout = ScanoutLayout::Bgra8888;
    const RGBA: ScanoutLayout = ScanoutLayout::Rgba8888;

    fn varint_bytes(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn bytes_field(number: u64, data: &[u8], out: &mut Vec<u8>) {
        varint_bytes(number << 3 | 2, out);
        varint_bytes(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    fn parsed_flag(package: &str, name: &str, state: u64) -> Vec<u8> {
        let mut f = Vec::new();
        bytes_field(1, package.as_bytes(), &mut f);
        bytes_field(2, name.as_bytes(), &mut f);
        bytes_field(3, b"namespace", &mut f);
        varint_bytes(4 << 3 | 1, &mut f);
        f.extend_from_slice(&[0; 8]);
        varint_bytes(5 << 3, &mut f);
        varint_bytes(300, &mut f);
        varint_bytes(6 << 3, &mut f);
        varint_bytes(state, &mut f);
        varint_bytes(7 << 3 | 5, &mut f);
        f.extend_from_slice(&[0; 4]);
        f
    }

    fn parsed_flags(flags: &[(&str, &str, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        varint_bytes(9 << 3, &mut out);
        varint_bytes(1, &mut out);
        for &(p, n, s) in flags {
            bytes_field(1, &parsed_flag(p, n, s), &mut out);
        }
        out
    }

    #[test]
    fn decoder_finds_enabled_flags_only() {
        let buf = parsed_flags(&[
            ("com.android.other", FLAG_VULKAN, 1),
            (SF_FLAGS_PACKAGE, "some_other_flag", 1),
            (SF_FLAGS_PACKAGE, FLAG_GRAPHITE, 2),
            (SF_FLAGS_PACKAGE, FLAG_VULKAN, 1),
        ]);
        assert!(aconfig_flag_enabled(&buf, SF_FLAGS_PACKAGE, FLAG_VULKAN));
        assert!(!aconfig_flag_enabled(&buf, SF_FLAGS_PACKAGE, FLAG_GRAPHITE));
        assert!(!aconfig_flag_enabled(&buf, SF_FLAGS_PACKAGE, "missing"));
    }

    #[test]
    fn decoder_tolerates_empty_and_truncated_input() {
        assert!(!aconfig_flag_enabled(&[], SF_FLAGS_PACKAGE, FLAG_VULKAN));
        let buf = parsed_flags(&[(SF_FLAGS_PACKAGE, FLAG_VULKAN, 1)]);
        for len in 0..buf.len() {
            assert!(!aconfig_flag_enabled(
                &buf[..len],
                SF_FLAGS_PACKAGE,
                FLAG_VULKAN
            ));
        }
        assert!(!aconfig_flag_enabled(
            &[0xff; 16],
            SF_FLAGS_PACKAGE,
            FLAG_VULKAN
        ));
        let mut group = vec![3 << 3 | 3];
        group.extend(parsed_flags(&[(SF_FLAGS_PACKAGE, FLAG_VULKAN, 1)]));
        assert!(!aconfig_flag_enabled(&group, SF_FLAGS_PACKAGE, FLAG_VULKAN));
    }

    #[test]
    fn render_engine_from_property_and_flags() {
        use RenderEngine::*;
        let cases = [
            ("skiagl", true, true, Gles),
            ("skiaglthreaded", false, false, Gles),
            ("skiavk", false, false, Vulkan),
            ("skiavkthreaded", false, false, Vulkan),
            ("graphite", false, false, Graphite),
            ("", false, false, Gles),
            ("", true, false, Vulkan),
            ("", false, true, Graphite),
            ("", true, true, Graphite),
            ("gles", false, false, Unknown),
        ];
        for (backend, vk, graphite, want) in cases {
            assert_eq!(
                render_engine(backend, vk, graphite),
                want,
                "{backend:?} {vk} {graphite}"
            );
        }
    }

    #[test]
    fn rule_table() {
        use RenderEngine::*;
        let cases = [
            (None, 36, Gles, RGBA),
            (None, 37, Gles, RGBA),
            (None, 35, Gles, BGRA),
            (None, 34, Gles, BGRA),
            (None, 36, Vulkan, BGRA),
            (None, 36, Graphite, BGRA),
            (None, 36, Unknown, BGRA),
            (None, 0, Gles, BGRA),
            (Some(RGBA), 34, Vulkan, RGBA),
            (Some(BGRA), 36, Gles, BGRA),
        ];
        for (env, sdk, re, want) in cases {
            let d = decide(env, sdk, re);
            assert_eq!(d.layout, want, "{env:?} {sdk} {re:?}");
            assert_eq!(d.source == Source::Env, env.is_some());
        }
    }

    #[test]
    fn decision_log_text() {
        assert_eq!(
            decide(None, 36, RenderEngine::Gles).to_string(),
            "rgba (rule: sdk=36 re=gles)"
        );
        assert_eq!(
            decide(None, 35, RenderEngine::Vulkan).to_string(),
            "bgra (rule: sdk=35 re=vulkan)"
        );
        assert_eq!(
            decide(Some(BGRA), 36, RenderEngine::Gles).to_string(),
            "bgra (env)"
        );
    }
}
