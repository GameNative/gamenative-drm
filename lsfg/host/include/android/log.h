/* Host-build shim for Android's <android/log.h>.
 *
 * The vendored LSFG engine logs through __android_log_print. On device that is
 * liblog; on the host it is stderr, so the engine compiles and the host
 * integration tests can link it. Only the surface the engine uses is provided:
 * the priority constants and __android_log_print itself.
 */
#pragma once

#include <stdarg.h>
#include <stdio.h>

#ifndef ANDROID_LOG_UNKNOWN
#define ANDROID_LOG_UNKNOWN 0
#define ANDROID_LOG_DEFAULT 1
#define ANDROID_LOG_VERBOSE 2
#define ANDROID_LOG_DEBUG 3
#define ANDROID_LOG_INFO 4
#define ANDROID_LOG_WARN 5
#define ANDROID_LOG_ERROR 6
#define ANDROID_LOG_FATAL 7
#endif

__attribute__((format(printf, 3, 4))) static inline int
__android_log_print(int prio, const char* tag, const char* fmt, ...) {
    static const char* const kLevel[] = {"?", "?", "V", "D", "I", "W", "E", "F"};
    const char* level = (prio >= 2 && prio <= 7) ? kLevel[prio] : "?";
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "%s/%s: ", level, tag != NULL ? tag : "?");
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    return 0;
}
