// Minimal stub of libavutil/common.h for the standalone PS oracle build.
#ifndef STUB_LIBAVUTIL_COMMON_H
#define STUB_LIBAVUTIL_COMMON_H

#include <math.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

#define FFMAX(a, b) ((a) > (b) ? (a) : (b))
#define FFMAX3(a, b, c) FFMAX(a, FFMAX(b, c))
#define FFMIN(a, b) ((a) > (b) ? (b) : (a))
#define FFABS(a) ((a) >= 0 ? (a) : (-(a)))
#define FFSIGN(a) ((a) > 0 ? 1 : -1)
#define FF_ARRAY_ELEMS(a) (sizeof(a) / sizeof((a)[0]))

static inline int av_clip(int a, int amin, int amax)
{
    if (a < amin) return amin;
    if (a > amax) return amax;
    return a;
}
static inline float av_clipf(float a, float amin, float amax)
{
    if (a < amin) return amin;
    if (a > amax) return amax;
    return a;
}
static inline double av_clipd(double a, double amin, double amax)
{
    if (a < amin) return amin;
    if (a > amax) return amax;
    return a;
}

#define av_cold
#define av_unused
#define av_always_inline inline
#define av_restrict restrict
#define UNCHECKED_BITSTREAM_READER 0

#endif
