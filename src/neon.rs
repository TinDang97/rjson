//! NEON equivalents of the SSE2 primitives the x86_64 kernels are built on
//! (issue #25). NEON is part of the aarch64 baseline, so these need no
//! runtime detection. The kernels using them mirror their x86_64 versions
//! line for line: `movemask` returns the same one-bit-per-byte mask as
//! `_mm_movemask_epi8`, so the bit arithmetic around it is unchanged.

use std::arch::aarch64::*;

/// Loads 16 bytes (any alignment).
///
/// SAFETY: `p..p+16` readable.
#[inline(always)]
pub unsafe fn load(p: *const u8) -> uint8x16_t {
    vld1q_u8(p)
}

/// Stores 16 bytes (any alignment).
///
/// SAFETY: `p..p+16` writable.
#[inline(always)]
pub unsafe fn store(p: *mut u8, v: uint8x16_t) {
    vst1q_u8(p, v)
}

/// `_mm_movemask_epi8`: bit k is the top bit of byte k.
#[inline(always)]
pub unsafe fn movemask(v: uint8x16_t) -> u32 {
    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    // Top bit -> 0xFF / 0x00, keep each byte's own bit, then add each half
    // (the weights of a half sum to 255, so the adds cannot overflow).
    let full = vreinterpretq_u8_s8(vshrq_n_s8::<7>(vreinterpretq_s8_u8(v)));
    let bits = vandq_u8(full, vld1q_u8(WEIGHTS.as_ptr()));
    let lo = vaddv_u8(vget_low_u8(bits)) as u32;
    let hi = vaddv_u8(vget_high_u8(bits)) as u32;
    lo | (hi << 8)
}

/// Whether any byte of a 0xFF/0x00 lane mask is set.
#[inline(always)]
pub unsafe fn any(m: uint8x16_t) -> bool {
    vmaxvq_u8(m) != 0
}

/// Number of set lanes of a 0xFF/0x00 lane mask.
#[inline(always)]
pub unsafe fn count(m: uint8x16_t) -> u32 {
    vaddvq_u8(vshrq_n_u8::<7>(m)) as u32
}

/// Lanes that need escaping in a JSON string: `"`, `\\` and control
/// characters (< 0x20).
#[inline(always)]
pub unsafe fn special(v: uint8x16_t) -> uint8x16_t {
    vorrq_u8(
        vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'"')), vceqq_u8(v, vdupq_n_u8(b'\\'))),
        vcltq_u8(v, vdupq_n_u8(0x20)),
    )
}

/// Lanes holding JSON whitespace (space, `\n`, `\r`, `\t`).
#[inline(always)]
pub unsafe fn whitespace(v: uint8x16_t) -> uint8x16_t {
    vorrq_u8(
        vorrq_u8(vceqq_u8(v, vdupq_n_u8(b' ')), vceqq_u8(v, vdupq_n_u8(b'\n'))),
        vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'\r')), vceqq_u8(v, vdupq_n_u8(b'\t'))),
    )
}
