// Minimal stub of libavutil/mem_internal.h for the standalone PS oracle build.
#ifndef STUB_LIBAVUTIL_MEM_INTERNAL_H
#define STUB_LIBAVUTIL_MEM_INTERNAL_H

#include "common.h"

#define DECLARE_ALIGNED(n, t, v) t __attribute__((aligned(n))) v

// LOCAL_ALIGNED_16(INTFLOAT, temp, [8], [2]) -> aligned INTFLOAT temp[8][2]
#define LOCAL_ALIGNED_16(t, v, d0, ...) t __attribute__((aligned(16))) v d0 __VA_ARGS__

#endif
