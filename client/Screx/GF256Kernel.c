#include "GF256Kernel.h"

#include <pthread.h>
#include <string.h>

#if defined(__aarch64__) || defined(__ARM_NEON)
#include <arm_neon.h>
#define SCREX_HAVE_NEON 1
#else
#define SCREX_HAVE_NEON 0
#endif

/* 4-bit split multiplication tables over GF(2^8) with primitive polynomial
   0x11D (x^8 + x^4 + x^3 + x^2 + 1), matching the Rust `reed-solomon-erasure`
   crate. Total footprint: 8 KB (stays hot in L1 data cache). */
static uint8_t g_low_tbl[256][16];
static uint8_t g_high_tbl[256][16];
static pthread_once_t g_tables_once = PTHREAD_ONCE_INIT;

static uint8_t gf_mul_scalar(uint8_t a, uint8_t b) {
    uint8_t result = 0;
    uint8_t hi_bit;
    for (int i = 0; i < 8; i++) {
        if (b & 1) {
            result ^= a;
        }
        hi_bit = a & 0x80;
        a <<= 1;
        if (hi_bit) {
            a ^= 0x1D; /* 0x11D without x^8 */
        }
        b >>= 1;
    }
    return result;
}

static void gf256_init_tables(void) {
    for (int c = 0; c < 256; c++) {
        for (int nibble = 0; nibble < 16; nibble++) {
            g_low_tbl[c][nibble] = gf_mul_scalar((uint8_t)c, (uint8_t)nibble);
            g_high_tbl[c][nibble] = gf_mul_scalar((uint8_t)c, (uint8_t)(nibble << 4));
        }
    }
}

void screx_gf256_mul_add(
    uint8_t *dst,
    const uint8_t *src,
    size_t len,
    uint8_t coeff
) {
    if (coeff == 0 || len == 0 || !dst || !src) {
        return;
    }

    if (coeff == 1) {
        size_t i = 0;
#if SCREX_HAVE_NEON
        for (; i + 32 <= len; i += 32) {
            uint8x16_t d0 = vld1q_u8(dst + i);
            uint8x16_t s0 = vld1q_u8(src + i);
            uint8x16_t d1 = vld1q_u8(dst + i + 16);
            uint8x16_t s1 = vld1q_u8(src + i + 16);
            vst1q_u8(dst + i, veorq_u8(d0, s0));
            vst1q_u8(dst + i + 16, veorq_u8(d1, s1));
        }
        for (; i + 16 <= len; i += 16) {
            vst1q_u8(dst + i, veorq_u8(vld1q_u8(dst + i), vld1q_u8(src + i)));
        }
#endif
        for (; i < len; i++) {
            dst[i] ^= src[i];
        }
        return;
    }

    pthread_once(&g_tables_once, gf256_init_tables);
    const uint8_t *lo_ptr = g_low_tbl[coeff];
    const uint8_t *hi_ptr = g_high_tbl[coeff];
    size_t i = 0;

#if SCREX_HAVE_NEON
    const uint8x16_t tbl_lo = vld1q_u8(lo_ptr);
    const uint8x16_t tbl_hi = vld1q_u8(hi_ptr);
    const uint8x16_t mask_lo = vdupq_n_u8(0x0F);

    for (; i + 32 <= len; i += 32) {
        uint8x16_t v0 = vld1q_u8(src + i);
        uint8x16_t d0 = vld1q_u8(dst + i);
        uint8x16_t v1 = vld1q_u8(src + i + 16);
        uint8x16_t d1 = vld1q_u8(dst + i + 16);

        uint8x16_t p0 = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(v0, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(v0, 4))
        );
        uint8x16_t p1 = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(v1, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(v1, 4))
        );

        vst1q_u8(dst + i, veorq_u8(d0, p0));
        vst1q_u8(dst + i + 16, veorq_u8(d1, p1));
    }
    for (; i + 16 <= len; i += 16) {
        uint8x16_t v = vld1q_u8(src + i);
        uint8x16_t d = vld1q_u8(dst + i);
        uint8x16_t p = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(v, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(v, 4))
        );
        vst1q_u8(dst + i, veorq_u8(d, p));
    }
#endif

    for (; i < len; i++) {
        uint8_t v = src[i];
        dst[i] ^= lo_ptr[v & 0x0F] ^ hi_ptr[v >> 4];
    }
}

void screx_gf256_mul_scale(
    uint8_t *dst,
    size_t len,
    uint8_t coeff
) {
    if (coeff == 1 || len == 0 || !dst) {
        return;
    }
    if (coeff == 0) {
        memset(dst, 0, len);
        return;
    }

    pthread_once(&g_tables_once, gf256_init_tables);
    const uint8_t *lo_ptr = g_low_tbl[coeff];
    const uint8_t *hi_ptr = g_high_tbl[coeff];
    size_t i = 0;

#if SCREX_HAVE_NEON
    const uint8x16_t tbl_lo = vld1q_u8(lo_ptr);
    const uint8x16_t tbl_hi = vld1q_u8(hi_ptr);
    const uint8x16_t mask_lo = vdupq_n_u8(0x0F);

    for (; i + 16 <= len; i += 16) {
        uint8x16_t v = vld1q_u8(dst + i);
        uint8x16_t p = veorq_u8(
            vqtbl1q_u8(tbl_lo, vandq_u8(v, mask_lo)),
            vqtbl1q_u8(tbl_hi, vshrq_n_u8(v, 4))
        );
        vst1q_u8(dst + i, p);
    }
#endif

    for (; i < len; i++) {
        uint8_t v = dst[i];
        dst[i] = lo_ptr[v & 0x0F] ^ hi_ptr[v >> 4];
    }
}
