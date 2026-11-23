#ifndef SCREX_GF256_KERNEL_H
#define SCREX_GF256_KERNEL_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// Computes `dst[i] ^= gf_mul(src[i], coeff)` over GF(2^8) (primitive
/// polynomial 0x11D, matching `reed-solomon-erasure`) for `0 <= i < len`.
/// Uses ARM64 NEON `vqtbl1q_u8` 4-bit split lookup tables when available.
void screx_gf256_mul_add(
    uint8_t *dst,
    const uint8_t *src,
    size_t len,
    uint8_t coeff
);

/// Computes `dst[i] = gf_mul(dst[i], coeff)` over GF(2^8) (primitive
/// polynomial 0x11D) in place for `0 <= i < len`.
void screx_gf256_mul_scale(
    uint8_t *dst,
    size_t len,
    uint8_t coeff
);

#ifdef __cplusplus
}
#endif

#endif /* SCREX_GF256_KERNEL_H */
