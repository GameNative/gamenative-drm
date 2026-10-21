// lsfg_host.cpp — the embedding seam declared in lsfg_host.h.
//
// Deliberately trivial: the engine has no opinion about where shader modules
// come from. It asks, the embedder answers, and a missing table is a cache
// failure rather than a crash.

#include "lsfg_host.h"

namespace {
const LsfgHostOps* g_host = nullptr;
}

extern "C" void lsfg_engine_set_host(const LsfgHostOps* ops) {
    g_host = ops;
}

extern "C" uint32_t lsfg_engine_abi(void) {
    return 1u;
}

LsfgStatus lsfg_host_load_modules(const char* cache_path, LsfgModuleSet* out_set) {
    if (g_host == nullptr || g_host->load_modules == nullptr) return LSFG_CACHE_UNUSABLE;
    return g_host->load_modules(cache_path, out_set);
}

void lsfg_host_release_modules(LsfgModuleSet* set) {
    if (g_host != nullptr && g_host->release_modules != nullptr) {
        g_host->release_modules(set);
    }
}
