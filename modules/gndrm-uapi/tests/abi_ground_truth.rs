//! ABI ground truth: compile a C translation unit full of `_Static_assert`s
//! against the *real* kernel/libdrm headers and compare every struct size and
//! ioctl number with the values mirrored in this crate.
//!
//! The C side emits its constants through a tiny program that prints them;
//! the Rust side asserts equality. If a host header ever disagrees with the
//! mirror, this test fails at compile- or run-time, loudly.

use std::process::Command;

/// C source: prints `name=value` lines for everything we mirror.
const PROBE_C: &str = r#"
#include <stdio.h>
#include <stddef.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <drm/drm_fourcc.h>

/* Compile-time layout guards on the C side itself. */
#define CHK_SIZE(ty, sz) _Static_assert(sizeof(ty) == (sz), #ty " size")
#define CHK_OFF(ty, field, off) _Static_assert(offsetof(ty, field) == (off), #ty "." #field " offset")

CHK_SIZE(struct drm_version, 64);
CHK_SIZE(struct drm_get_cap, 16);
CHK_SIZE(struct drm_set_client_cap, 16);
CHK_SIZE(struct drm_gem_close, 8);
CHK_SIZE(struct drm_prime_handle, 12);
CHK_SIZE(struct drm_event, 8);
CHK_SIZE(struct drm_event_vblank, 32);
CHK_SIZE(struct drm_mode_modeinfo, 68);
CHK_SIZE(struct drm_mode_card_res, 64);
CHK_SIZE(struct drm_mode_crtc, 104);
CHK_SIZE(struct drm_mode_get_encoder, 20);
CHK_SIZE(struct drm_mode_get_connector, 80);
CHK_SIZE(struct drm_mode_get_plane_res, 16);
CHK_SIZE(struct drm_mode_get_plane, 32);
CHK_SIZE(struct drm_mode_obj_get_properties, 32);
CHK_SIZE(struct drm_mode_obj_set_property, 24);
CHK_SIZE(struct drm_mode_property_enum, 40);
CHK_SIZE(struct drm_mode_get_property, 64);
CHK_SIZE(struct drm_mode_create_blob, 16);
CHK_SIZE(struct drm_mode_destroy_blob, 4);
CHK_SIZE(struct drm_mode_get_blob, 16);
CHK_SIZE(struct drm_mode_fb_cmd2, 104);
CHK_SIZE(struct drm_mode_atomic, 56);

/* Spot-check offsets of fields the daemon reads/writes directly. */
CHK_OFF(struct drm_mode_card_res, count_fbs, 32);
CHK_OFF(struct drm_mode_get_connector, count_modes, 32);
CHK_OFF(struct drm_mode_get_connector, encoder_id, 44);
CHK_OFF(struct drm_mode_get_plane, format_type_ptr, 24);
CHK_OFF(struct drm_mode_fb_cmd2, modifier, 72);
CHK_OFF(struct drm_mode_atomic, user_data, 48);
CHK_OFF(struct drm_event_vblank, crtc_id, 28);

int main(void) {
    printf("DRM_IOCTL_VERSION=0x%x\n", (unsigned)DRM_IOCTL_VERSION);
    printf("DRM_IOCTL_GET_CAP=0x%x\n", (unsigned)DRM_IOCTL_GET_CAP);
    printf("DRM_IOCTL_SET_CLIENT_CAP=0x%x\n", (unsigned)DRM_IOCTL_SET_CLIENT_CAP);
    printf("DRM_IOCTL_GEM_CLOSE=0x%x\n", (unsigned)DRM_IOCTL_GEM_CLOSE);
    printf("DRM_IOCTL_PRIME_HANDLE_TO_FD=0x%x\n", (unsigned)DRM_IOCTL_PRIME_HANDLE_TO_FD);
    printf("DRM_IOCTL_PRIME_FD_TO_HANDLE=0x%x\n", (unsigned)DRM_IOCTL_PRIME_FD_TO_HANDLE);
    printf("DRM_IOCTL_SET_MASTER=0x%x\n", (unsigned)DRM_IOCTL_SET_MASTER);
    printf("DRM_IOCTL_DROP_MASTER=0x%x\n", (unsigned)DRM_IOCTL_DROP_MASTER);
    printf("DRM_IOCTL_MODE_GETRESOURCES=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETRESOURCES);
    printf("DRM_IOCTL_MODE_GETCRTC=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETCRTC);
    printf("DRM_IOCTL_MODE_SETCRTC=0x%x\n", (unsigned)DRM_IOCTL_MODE_SETCRTC);
    printf("DRM_IOCTL_MODE_GETENCODER=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETENCODER);
    printf("DRM_IOCTL_MODE_GETCONNECTOR=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETCONNECTOR);
    printf("DRM_IOCTL_MODE_GETPROPERTY=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETPROPERTY);
    printf("DRM_IOCTL_MODE_GETPROPBLOB=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETPROPBLOB);
    printf("DRM_IOCTL_MODE_RMFB=0x%x\n", (unsigned)DRM_IOCTL_MODE_RMFB);
    printf("DRM_IOCTL_MODE_GETPLANERESOURCES=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETPLANERESOURCES);
    printf("DRM_IOCTL_MODE_GETPLANE=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETPLANE);
    printf("DRM_IOCTL_MODE_ADDFB2=0x%x\n", (unsigned)DRM_IOCTL_MODE_ADDFB2);
    printf("DRM_IOCTL_MODE_OBJ_GETPROPERTIES=0x%x\n", (unsigned)DRM_IOCTL_MODE_OBJ_GETPROPERTIES);
    printf("DRM_IOCTL_MODE_OBJ_SETPROPERTY=0x%x\n", (unsigned)DRM_IOCTL_MODE_OBJ_SETPROPERTY);
    printf("DRM_IOCTL_MODE_ATOMIC=0x%x\n", (unsigned)DRM_IOCTL_MODE_ATOMIC);
    printf("DRM_IOCTL_MODE_CREATEPROPBLOB=0x%x\n", (unsigned)DRM_IOCTL_MODE_CREATEPROPBLOB);
    printf("DRM_IOCTL_MODE_DESTROYPROPBLOB=0x%x\n", (unsigned)DRM_IOCTL_MODE_DESTROYPROPBLOB);
    printf("DRM_IOCTL_MODE_GETFB2=0x%x\n", (unsigned)DRM_IOCTL_MODE_GETFB2);
    printf("DRM_CAP_DUMB_BUFFER=0x%llx\n", (unsigned long long)DRM_CAP_DUMB_BUFFER);
    printf("DRM_CAP_TIMESTAMP_MONOTONIC=0x%llx\n", (unsigned long long)DRM_CAP_TIMESTAMP_MONOTONIC);
    printf("DRM_CAP_CURSOR_WIDTH=0x%llx\n", (unsigned long long)DRM_CAP_CURSOR_WIDTH);
    printf("DRM_CAP_CURSOR_HEIGHT=0x%llx\n", (unsigned long long)DRM_CAP_CURSOR_HEIGHT);
    printf("DRM_CAP_ADDFB2_MODIFIERS=0x%llx\n", (unsigned long long)DRM_CAP_ADDFB2_MODIFIERS);
    printf("DRM_CAP_SYNCOBJ=0x%llx\n", (unsigned long long)DRM_CAP_SYNCOBJ);
    printf("DRM_CLIENT_CAP_UNIVERSAL_PLANES=0x%llx\n", (unsigned long long)DRM_CLIENT_CAP_UNIVERSAL_PLANES);
    printf("DRM_CLIENT_CAP_ATOMIC=0x%llx\n", (unsigned long long)DRM_CLIENT_CAP_ATOMIC);
    printf("DRM_CLIENT_CAP_WRITEBACK_CONNECTORS=0x%llx\n", (unsigned long long)DRM_CLIENT_CAP_WRITEBACK_CONNECTORS);
    printf("DRM_EVENT_VBLANK=0x%x\n", DRM_EVENT_VBLANK);
    printf("DRM_EVENT_FLIP_COMPLETE=0x%x\n", DRM_EVENT_FLIP_COMPLETE);
    printf("DRM_MODE_OBJECT_CRTC=0x%x\n", DRM_MODE_OBJECT_CRTC);
    printf("DRM_MODE_OBJECT_CONNECTOR=0x%x\n", DRM_MODE_OBJECT_CONNECTOR);
    printf("DRM_MODE_OBJECT_ENCODER=0x%x\n", DRM_MODE_OBJECT_ENCODER);
    printf("DRM_MODE_OBJECT_PLANE=0x%x\n", DRM_MODE_OBJECT_PLANE);
    printf("DRM_MODE_OBJECT_FB=0x%x\n", DRM_MODE_OBJECT_FB);
    printf("DRM_MODE_OBJECT_BLOB=0x%x\n", DRM_MODE_OBJECT_BLOB);
    printf("DRM_MODE_OBJECT_PROPERTY=0x%x\n", DRM_MODE_OBJECT_PROPERTY);
    printf("DRM_MODE_PROP_RANGE=0x%x\n", DRM_MODE_PROP_RANGE);
    printf("DRM_MODE_PROP_IMMUTABLE=0x%x\n", DRM_MODE_PROP_IMMUTABLE);
    printf("DRM_MODE_PROP_ENUM=0x%x\n", DRM_MODE_PROP_ENUM);
    printf("DRM_MODE_PROP_BLOB=0x%x\n", DRM_MODE_PROP_BLOB);
    printf("DRM_MODE_PROP_BITMASK=0x%x\n", DRM_MODE_PROP_BITMASK);
    printf("DRM_MODE_PROP_OBJECT=0x%x\n", DRM_MODE_PROP_OBJECT);
    printf("DRM_MODE_PROP_ATOMIC=0x%x\n", DRM_MODE_PROP_ATOMIC);
    printf("DRM_MODE_TYPE_PREFERRED=0x%x\n", DRM_MODE_TYPE_PREFERRED);
    printf("DRM_MODE_TYPE_DRIVER=0x%x\n", DRM_MODE_TYPE_DRIVER);
    printf("DRM_MODE_PAGE_FLIP_EVENT=0x%x\n", DRM_MODE_PAGE_FLIP_EVENT);
    printf("DRM_MODE_ATOMIC_TEST_ONLY=0x%x\n", DRM_MODE_ATOMIC_TEST_ONLY);
    printf("DRM_MODE_ATOMIC_NONBLOCK=0x%x\n", DRM_MODE_ATOMIC_NONBLOCK);
    printf("DRM_MODE_ATOMIC_ALLOW_MODESET=0x%x\n", DRM_MODE_ATOMIC_ALLOW_MODESET);
    printf("DRM_MODE_CONNECTOR_HDMIA=0x%x\n", DRM_MODE_CONNECTOR_HDMIA);
    printf("DRM_MODE_CONNECTOR_eDP=0x%x\n", DRM_MODE_CONNECTOR_eDP);
    printf("DRM_MODE_CONNECTOR_DSI=0x%x\n", DRM_MODE_CONNECTOR_DSI);
    printf("DRM_MODE_ENCODER_VIRTUAL=0x%x\n", DRM_MODE_ENCODER_VIRTUAL);
    printf("DRM_MODE_FB_MODIFIERS=0x%x\n", DRM_MODE_FB_MODIFIERS);
    printf("DRM_FORMAT_XRGB8888=0x%x\n", DRM_FORMAT_XRGB8888);
    printf("DRM_FORMAT_ARGB8888=0x%x\n", DRM_FORMAT_ARGB8888);
    printf("DRM_FORMAT_XBGR8888=0x%x\n", DRM_FORMAT_XBGR8888);
    printf("DRM_FORMAT_ABGR8888=0x%x\n", DRM_FORMAT_ABGR8888);
    printf("DRM_FORMAT_RGB565=0x%x\n", DRM_FORMAT_RGB565);
    printf("DRM_FORMAT_XRGB2101010=0x%x\n", DRM_FORMAT_XRGB2101010);
    printf("DRM_FORMAT_XBGR2101010=0x%x\n", DRM_FORMAT_XBGR2101010);
    return 0;
}
"#;

fn ground_truth() -> std::collections::HashMap<String, u64> {
    // Tests run on threads within one process: keep probe paths distinct.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("gndrm-abi-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("probe.c");
    let bin = dir.join("probe");
    std::fs::write(&src, PROBE_C).unwrap();

    let compile = Command::new("cc")
        .arg("-Wall")
        .arg("-Werror")
        .arg("-I/usr/include")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("failed to run cc");
    assert!(
        compile.status.success(),
        "C probe failed to compile:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&bin).output().expect("failed to run probe");
    assert!(run.status.success());

    String::from_utf8(run.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            let v = v.strip_prefix("0x").map_or_else(
                || v.parse().unwrap(),
                |h| u64::from_str_radix(h, 16).unwrap(),
            );
            (k.to_string(), v)
        })
        .collect()
}

macro_rules! chk {
    ($gt:expr, $name:literal, $rust:expr) => {
        assert_eq!(
            u64::from($rust),
            $gt[$name],
            "mismatch for {}: rust=0x{:x} c=0x{:x}",
            $name,
            $rust,
            $gt[$name]
        );
    };
}

#[test]
fn ioctl_numbers_match_c_headers() {
    use gndrm_uapi::{drm::*, mode::*};
    let gt = ground_truth();

    chk!(gt, "DRM_IOCTL_VERSION", DRM_IOCTL_VERSION);
    chk!(gt, "DRM_IOCTL_GET_CAP", DRM_IOCTL_GET_CAP);
    chk!(gt, "DRM_IOCTL_SET_CLIENT_CAP", DRM_IOCTL_SET_CLIENT_CAP);
    chk!(gt, "DRM_IOCTL_GEM_CLOSE", DRM_IOCTL_GEM_CLOSE);
    chk!(
        gt,
        "DRM_IOCTL_PRIME_HANDLE_TO_FD",
        DRM_IOCTL_PRIME_HANDLE_TO_FD
    );
    chk!(
        gt,
        "DRM_IOCTL_PRIME_FD_TO_HANDLE",
        DRM_IOCTL_PRIME_FD_TO_HANDLE
    );
    chk!(gt, "DRM_IOCTL_SET_MASTER", DRM_IOCTL_SET_MASTER);
    chk!(gt, "DRM_IOCTL_DROP_MASTER", DRM_IOCTL_DROP_MASTER);
    chk!(
        gt,
        "DRM_IOCTL_MODE_GETRESOURCES",
        DRM_IOCTL_MODE_GETRESOURCES
    );
    chk!(gt, "DRM_IOCTL_MODE_GETCRTC", DRM_IOCTL_MODE_GETCRTC);
    chk!(gt, "DRM_IOCTL_MODE_SETCRTC", DRM_IOCTL_MODE_SETCRTC);
    chk!(gt, "DRM_IOCTL_MODE_GETENCODER", DRM_IOCTL_MODE_GETENCODER);
    chk!(
        gt,
        "DRM_IOCTL_MODE_GETCONNECTOR",
        DRM_IOCTL_MODE_GETCONNECTOR
    );
    chk!(gt, "DRM_IOCTL_MODE_GETPROPERTY", DRM_IOCTL_MODE_GETPROPERTY);
    chk!(gt, "DRM_IOCTL_MODE_GETPROPBLOB", DRM_IOCTL_MODE_GETPROPBLOB);
    chk!(gt, "DRM_IOCTL_MODE_RMFB", DRM_IOCTL_MODE_RMFB);
    chk!(
        gt,
        "DRM_IOCTL_MODE_GETPLANERESOURCES",
        DRM_IOCTL_MODE_GETPLANERESOURCES
    );
    chk!(gt, "DRM_IOCTL_MODE_GETPLANE", DRM_IOCTL_MODE_GETPLANE);
    chk!(gt, "DRM_IOCTL_MODE_ADDFB2", DRM_IOCTL_MODE_ADDFB2);
    chk!(
        gt,
        "DRM_IOCTL_MODE_OBJ_GETPROPERTIES",
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES
    );
    chk!(
        gt,
        "DRM_IOCTL_MODE_OBJ_SETPROPERTY",
        DRM_IOCTL_MODE_OBJ_SETPROPERTY
    );
    chk!(gt, "DRM_IOCTL_MODE_ATOMIC", DRM_IOCTL_MODE_ATOMIC);
    chk!(
        gt,
        "DRM_IOCTL_MODE_CREATEPROPBLOB",
        DRM_IOCTL_MODE_CREATEPROPBLOB
    );
    chk!(
        gt,
        "DRM_IOCTL_MODE_DESTROYPROPBLOB",
        DRM_IOCTL_MODE_DESTROYPROPBLOB
    );
    chk!(gt, "DRM_IOCTL_MODE_GETFB2", DRM_IOCTL_MODE_GETFB2);
}

#[test]
fn constants_match_c_headers() {
    use gndrm_uapi::{drm::*, mode::*};
    let gt = ground_truth();

    chk!(gt, "DRM_CAP_DUMB_BUFFER", DRM_CAP_DUMB_BUFFER);
    chk!(
        gt,
        "DRM_CAP_TIMESTAMP_MONOTONIC",
        DRM_CAP_TIMESTAMP_MONOTONIC
    );
    chk!(gt, "DRM_CAP_CURSOR_WIDTH", DRM_CAP_CURSOR_WIDTH);
    chk!(gt, "DRM_CAP_CURSOR_HEIGHT", DRM_CAP_CURSOR_HEIGHT);
    chk!(gt, "DRM_CAP_ADDFB2_MODIFIERS", DRM_CAP_ADDFB2_MODIFIERS);
    chk!(gt, "DRM_CAP_SYNCOBJ", DRM_CAP_SYNCOBJ);
    chk!(
        gt,
        "DRM_CLIENT_CAP_UNIVERSAL_PLANES",
        DRM_CLIENT_CAP_UNIVERSAL_PLANES
    );
    chk!(gt, "DRM_CLIENT_CAP_ATOMIC", DRM_CLIENT_CAP_ATOMIC);
    chk!(
        gt,
        "DRM_CLIENT_CAP_WRITEBACK_CONNECTORS",
        DRM_CLIENT_CAP_WRITEBACK_CONNECTORS
    );
    chk!(gt, "DRM_EVENT_VBLANK", DRM_EVENT_VBLANK);
    chk!(gt, "DRM_EVENT_FLIP_COMPLETE", DRM_EVENT_FLIP_COMPLETE);
    chk!(gt, "DRM_MODE_OBJECT_CRTC", DRM_MODE_OBJECT_CRTC);
    chk!(gt, "DRM_MODE_OBJECT_CONNECTOR", DRM_MODE_OBJECT_CONNECTOR);
    chk!(gt, "DRM_MODE_OBJECT_ENCODER", DRM_MODE_OBJECT_ENCODER);
    chk!(gt, "DRM_MODE_OBJECT_PLANE", DRM_MODE_OBJECT_PLANE);
    chk!(gt, "DRM_MODE_OBJECT_FB", DRM_MODE_OBJECT_FB);
    chk!(gt, "DRM_MODE_OBJECT_BLOB", DRM_MODE_OBJECT_BLOB);
    chk!(gt, "DRM_MODE_OBJECT_PROPERTY", DRM_MODE_OBJECT_PROPERTY);
    chk!(gt, "DRM_MODE_PROP_RANGE", DRM_MODE_PROP_RANGE);
    chk!(gt, "DRM_MODE_PROP_IMMUTABLE", DRM_MODE_PROP_IMMUTABLE);
    chk!(gt, "DRM_MODE_PROP_ENUM", DRM_MODE_PROP_ENUM);
    chk!(gt, "DRM_MODE_PROP_BLOB", DRM_MODE_PROP_BLOB);
    chk!(gt, "DRM_MODE_PROP_BITMASK", DRM_MODE_PROP_BITMASK);
    chk!(gt, "DRM_MODE_PROP_OBJECT", DRM_MODE_PROP_OBJECT);
    chk!(gt, "DRM_MODE_PROP_ATOMIC", DRM_MODE_PROP_ATOMIC);
    chk!(gt, "DRM_MODE_TYPE_PREFERRED", DRM_MODE_TYPE_PREFERRED);
    chk!(gt, "DRM_MODE_TYPE_DRIVER", DRM_MODE_TYPE_DRIVER);
    chk!(gt, "DRM_MODE_PAGE_FLIP_EVENT", DRM_MODE_PAGE_FLIP_EVENT);
    chk!(gt, "DRM_MODE_ATOMIC_TEST_ONLY", DRM_MODE_ATOMIC_TEST_ONLY);
    chk!(gt, "DRM_MODE_ATOMIC_NONBLOCK", DRM_MODE_ATOMIC_NONBLOCK);
    chk!(
        gt,
        "DRM_MODE_ATOMIC_ALLOW_MODESET",
        DRM_MODE_ATOMIC_ALLOW_MODESET
    );
    chk!(gt, "DRM_MODE_CONNECTOR_HDMIA", DRM_MODE_CONNECTOR_HDMIA);
    chk!(gt, "DRM_MODE_CONNECTOR_eDP", DRM_MODE_CONNECTOR_EDP);
    chk!(gt, "DRM_MODE_CONNECTOR_DSI", DRM_MODE_CONNECTOR_DSI);
    chk!(gt, "DRM_MODE_ENCODER_VIRTUAL", DRM_MODE_ENCODER_VIRTUAL);
    chk!(gt, "DRM_MODE_FB_MODIFIERS", DRM_MODE_FB_MODIFIERS);
    chk!(gt, "DRM_FORMAT_XRGB8888", DRM_FORMAT_XRGB8888);
    chk!(gt, "DRM_FORMAT_ARGB8888", DRM_FORMAT_ARGB8888);
    chk!(gt, "DRM_FORMAT_XBGR8888", DRM_FORMAT_XBGR8888);
    chk!(gt, "DRM_FORMAT_ABGR8888", DRM_FORMAT_ABGR8888);
    chk!(gt, "DRM_FORMAT_RGB565", DRM_FORMAT_RGB565);
    chk!(gt, "DRM_FORMAT_XRGB2101010", DRM_FORMAT_XRGB2101010);
    chk!(gt, "DRM_FORMAT_XBGR2101010", DRM_FORMAT_XBGR2101010);
}
