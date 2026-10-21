// lsfg_host.h — the engine's one dependency on its embedder.
//
// Upstream, `lsfg_shaders.cpp` called `lsfg_load_modules` / `lsfg_release_modules`
// directly: the C++ engine and the DLL reader were compiled into the same
// artifact and shared a link unit. This port keeps them apart — the engine ships
// as `liblsfg_engine.so`, the DLL reader and the shader cache are Rust inside
// gamenative-drm — so those two calls are made through a table the embedder
// installs before the first `vkr_lsfg_create`.
//
// This is the only edit to the vendored engine beyond removing its own
// `lsfg_dll.c`: three call sites in `lsfg_shaders.cpp` and the two wrappers at
// the bottom of this file.
#pragma once

#include "lsfg_dll.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct LsfgHostOps {
    /* Fills `out_set` with the cache's modules; the embedder owns the words. */
    LsfgStatus (*load_modules)(const char* cache_path, LsfgModuleSet* out_set);
    void (*release_modules)(LsfgModuleSet* set);
} LsfgHostOps;

/* Install the embedder's functions. Called once, before the first create. */
void lsfg_engine_set_host(const LsfgHostOps* ops);

/* ABI revision of this library; the embedder refuses a mismatch. */
uint32_t lsfg_engine_abi(void);

/* Internal: what `lsfg_shaders.cpp` calls instead of the direct symbols. */
LsfgStatus lsfg_host_load_modules(const char* cache_path, LsfgModuleSet* out_set);
void lsfg_host_release_modules(LsfgModuleSet* set);

#ifdef __cplusplus
}
#endif
