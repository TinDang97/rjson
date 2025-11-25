//! Phase 10: SIMD-accelerated string escaping for JSON serialization
//!
//! This module provides high-performance string escaping using SIMD instructions.
//! The key insight is that most strings don't need escaping, so we optimize for
//! bulk copying of clean chunks.
//!
//! Performance targets:
//! - Clean strings: 10-15x improvement (from 13.88x slower to ~1.5x slower vs orjson)
//! - Strings with escapes: Keep current performance (already beats orjson!)
//!
//! Architecture:
//! - SSE2 (16 bytes): Baseline for all x86_64 (available since 2003)
//! - AVX2 (32 bytes): Fast path when available (~2x throughput)
//! - Scalar fallback: For non-x86 platforms

use super::escape_lut::{EscapeAction, ESCAPE_LUT};

/// Minimum string length to use SIMD path
/// Below this, the setup overhead exceeds the benefit
const SIMD_THRESHOLD: usize = 16;

/// Write a JSON string with SIMD-accelerated escape detection
///
/// This is the main entry point that dispatches to the appropriate
/// implementation based on CPU features and string length.
#[inline]
pub fn write_json_string_simd(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();

    buf.push(b'"');

    if bytes.is_empty() {
        buf.push(b'"');
        return;
    }

    // Use SIMD for strings >= threshold on x86_64
    #[cfg(target_arch = "x86_64")]
    {
        if bytes.len() >= SIMD_THRESHOLD {
            // Try AVX2 first (32 bytes at a time)
            if is_x86_feature_detected!("avx2") {
                unsafe { write_escaped_avx2(buf, bytes); }
            } else {
                // Fall back to SSE2 (always available on x86_64)
                unsafe { write_escaped_sse2(buf, bytes); }
            }
            buf.push(b'"');
            return;
        }
    }

    // Scalar fallback for short strings or non-x86
    write_escaped_scalar(buf, bytes);
    buf.push(b'"');
}

/// Fast scalar path that assumes no escapes needed
/// Used for bulk copying when we know string is safe
#[inline]
pub fn write_json_string_fast(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.reserve(bytes.len() + 2);
    buf.push(b'"');
    buf.extend_from_slice(bytes);
    buf.push(b'"');
}

/// SSE2 implementation: Process 16 bytes at a time
///
/// # Safety
/// Caller must ensure bytes.len() >= 16
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn write_escaped_sse2(buf: &mut Vec<u8>, bytes: &[u8]) {
    use std::arch::x86_64::*;

    // Pre-allocate worst case (every char escaped = 6x for \uXXXX)
    // But realistically, reserve original size + some padding
    buf.reserve(bytes.len() + 64);

    let mut i = 0;
    let len = bytes.len();

    // SIMD constants for escape detection
    let quote_vec = _mm_set1_epi8(b'"' as i8);
    let backslash_vec = _mm_set1_epi8(b'\\' as i8);
    let space_vec = _mm_set1_epi8(0x20);  // First non-control char

    // Process 16 bytes at a time
    while i + 16 <= len {
        // Load 16 bytes (unaligned load is fine on modern CPUs)
        let chunk = _mm_loadu_si128(bytes.as_ptr().add(i) as *const __m128i);

        // Check for characters that need escaping:
        // 1. byte == '"' (0x22)
        // 2. byte == '\\' (0x5C)
        // 3. byte < 0x20 (control characters)

        let is_quote = _mm_cmpeq_epi8(chunk, quote_vec);
        let is_backslash = _mm_cmpeq_epi8(chunk, backslash_vec);

        // For control chars: we need byte < 0x20
        // SSE2 doesn't have unsigned compare, but we can use the fact that
        // 0x00-0x1F when interpreted as signed are 0-31, and 0x20+ are 32+
        // Actually, bytes 0x80-0xFF are negative when signed, so we need care
        // Safe approach: check if (byte < 0x20) using saturating subtract
        // If byte >= 0x20, then (byte - 0x20) >= 0, else it underflows
        // Better: use _mm_cmplt_epi8 which treats as signed
        // Bytes 0x00-0x1F are 0-31 (positive, < 32)
        // Bytes 0x20-0x7F are 32-127 (positive, >= 32) - SAFE
        // Bytes 0x80-0xFF are -128 to -1 (negative, < 32) - but these are valid UTF-8!
        //
        // For JSON/UTF-8, bytes >= 0x80 are continuation bytes and DON'T need escaping.
        // So we need: (byte < 0x20) AND (byte >= 0)
        // Which simplifies to: (unsigned)byte < 0x20
        //
        // Trick: XOR with 0x80 to flip sign bit, then signed compare
        // Or: compare as unsigned by adding 0x80 to both sides

        // Simpler approach: Check for common control chars explicitly
        // Most strings have 0 control chars, so any detection is fine
        let control_mask = _mm_cmplt_epi8(chunk, space_vec);
        // Also mask out negative bytes (0x80-0xFF are valid UTF-8, not control)
        let zero_vec = _mm_setzero_si128();
        let is_positive = _mm_cmpgt_epi8(chunk, _mm_set1_epi8(-1)); // chunk > -1 means chunk >= 0
        let is_control = _mm_and_si128(control_mask, is_positive);

        // Combine all escape conditions
        let needs_escape = _mm_or_si128(_mm_or_si128(is_quote, is_backslash), is_control);

        // Convert to bitmask (1 bit per byte)
        let mask = _mm_movemask_epi8(needs_escape);

        if mask == 0 {
            // FAST PATH: No escapes in this 16-byte chunk
            // Bulk copy directly to output buffer
            let dst_ptr = buf.as_mut_ptr().add(buf.len());
            std::ptr::copy_nonoverlapping(bytes.as_ptr().add(i), dst_ptr, 16);
            buf.set_len(buf.len() + 16);
            i += 16;
        } else {
            // SLOW PATH: Has escapes - find first one and handle
            let first_escape = mask.trailing_zeros() as usize;

            // Copy bytes before the escape
            if first_escape > 0 {
                buf.extend_from_slice(&bytes[i..i + first_escape]);
            }

            // Handle the escape character
            let escape_byte = bytes[i + first_escape];
            write_escape_sequence(buf, escape_byte);

            // Continue from after the escaped char
            i += first_escape + 1;

            // For remaining bytes in this chunk, use scalar
            // (simpler than trying to resume SIMD mid-chunk)
            let chunk_end = std::cmp::min(i + (16 - first_escape - 1), len);
            while i < chunk_end && i + 16 > len {
                let b = bytes[i];
                if ESCAPE_LUT[b as usize] != EscapeAction::None {
                    write_escape_sequence(buf, b);
                } else {
                    buf.push(b);
                }
                i += 1;
            }
        }
    }

    // Handle remaining bytes (< 16) with scalar code
    write_escaped_scalar_range(buf, bytes, i, len);
}

/// AVX2 implementation: Process 32 bytes at a time
///
/// # Safety
/// Caller must ensure bytes.len() >= 32 and AVX2 is available
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn write_escaped_avx2(buf: &mut Vec<u8>, bytes: &[u8]) {
    use std::arch::x86_64::*;

    buf.reserve(bytes.len() + 64);

    let mut i = 0;
    let len = bytes.len();

    // AVX2 constants (32 bytes each)
    let quote_vec = _mm256_set1_epi8(b'"' as i8);
    let backslash_vec = _mm256_set1_epi8(b'\\' as i8);
    let space_vec = _mm256_set1_epi8(0x20);

    // Process 32 bytes at a time
    while i + 32 <= len {
        let chunk = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);

        let is_quote = _mm256_cmpeq_epi8(chunk, quote_vec);
        let is_backslash = _mm256_cmpeq_epi8(chunk, backslash_vec);

        // Control char detection (same logic as SSE2)
        let control_mask = _mm256_cmpgt_epi8(space_vec, chunk);
        let is_positive = _mm256_cmpgt_epi8(chunk, _mm256_set1_epi8(-1));
        let is_control = _mm256_and_si256(control_mask, is_positive);

        let needs_escape = _mm256_or_si256(_mm256_or_si256(is_quote, is_backslash), is_control);
        let mask = _mm256_movemask_epi8(needs_escape);

        if mask == 0 {
            // FAST PATH: Bulk copy 32 bytes
            let dst_ptr = buf.as_mut_ptr().add(buf.len());
            std::ptr::copy_nonoverlapping(bytes.as_ptr().add(i), dst_ptr, 32);
            buf.set_len(buf.len() + 32);
            i += 32;
        } else {
            // SLOW PATH: Handle escapes
            let first_escape = mask.trailing_zeros() as usize;

            if first_escape > 0 {
                buf.extend_from_slice(&bytes[i..i + first_escape]);
            }

            let escape_byte = bytes[i + first_escape];
            write_escape_sequence(buf, escape_byte);
            i += first_escape + 1;

            // Process rest of chunk with scalar
            let chunk_end = std::cmp::min(i + (32 - first_escape - 1), len);
            while i < chunk_end && i + 32 > len {
                let b = bytes[i];
                if ESCAPE_LUT[b as usize] != EscapeAction::None {
                    write_escape_sequence(buf, b);
                } else {
                    buf.push(b);
                }
                i += 1;
            }
        }
    }

    // Fall back to SSE2 for 16-31 remaining bytes
    if i + 16 <= len {
        // Process one SSE2 chunk
        write_escaped_sse2_single_chunk(buf, bytes, &mut i, len);
    }

    // Handle final < 16 bytes
    write_escaped_scalar_range(buf, bytes, i, len);
}

/// Process a single SSE2 chunk (helper for AVX2 tail)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn write_escaped_sse2_single_chunk(buf: &mut Vec<u8>, bytes: &[u8], i: &mut usize, len: usize) {
    use std::arch::x86_64::*;

    if *i + 16 > len {
        return;
    }

    let quote_vec = _mm_set1_epi8(b'"' as i8);
    let backslash_vec = _mm_set1_epi8(b'\\' as i8);
    let space_vec = _mm_set1_epi8(0x20);

    let chunk = _mm_loadu_si128(bytes.as_ptr().add(*i) as *const __m128i);

    let is_quote = _mm_cmpeq_epi8(chunk, quote_vec);
    let is_backslash = _mm_cmpeq_epi8(chunk, backslash_vec);
    let control_mask = _mm_cmplt_epi8(chunk, space_vec);
    let is_positive = _mm_cmpgt_epi8(chunk, _mm_set1_epi8(-1));
    let is_control = _mm_and_si128(control_mask, is_positive);

    let needs_escape = _mm_or_si128(_mm_or_si128(is_quote, is_backslash), is_control);
    let mask = _mm_movemask_epi8(needs_escape);

    if mask == 0 {
        let dst_ptr = buf.as_mut_ptr().add(buf.len());
        std::ptr::copy_nonoverlapping(bytes.as_ptr().add(*i), dst_ptr, 16);
        buf.set_len(buf.len() + 16);
        *i += 16;
    } else {
        let first_escape = mask.trailing_zeros() as usize;
        if first_escape > 0 {
            buf.extend_from_slice(&bytes[*i..*i + first_escape]);
        }
        let escape_byte = bytes[*i + first_escape];
        write_escape_sequence(buf, escape_byte);
        *i += first_escape + 1;
    }
}

/// Write escape sequence for a single byte
#[inline(always)]
fn write_escape_sequence(buf: &mut Vec<u8>, b: u8) {
    match ESCAPE_LUT[b as usize] {
        EscapeAction::None => buf.push(b),
        EscapeAction::Quote => buf.extend_from_slice(b"\\\""),
        EscapeAction::Backslash => buf.extend_from_slice(b"\\\\"),
        EscapeAction::Newline => buf.extend_from_slice(b"\\n"),
        EscapeAction::CarriageReturn => buf.extend_from_slice(b"\\r"),
        EscapeAction::Tab => buf.extend_from_slice(b"\\t"),
        EscapeAction::Backspace => buf.extend_from_slice(b"\\b"),
        EscapeAction::FormFeed => buf.extend_from_slice(b"\\f"),
        EscapeAction::Unicode => {
            buf.extend_from_slice(b"\\u00");
            let high = b >> 4;
            let low = b & 0x0F;
            buf.push(HEX_CHARS[high as usize]);
            buf.push(HEX_CHARS[low as usize]);
        }
    }
}

/// Hex characters lookup table
static HEX_CHARS: [u8; 16] = *b"0123456789abcdef";

/// Scalar fallback for short strings
#[inline]
fn write_escaped_scalar(buf: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if ESCAPE_LUT[b as usize] != EscapeAction::None {
            write_escape_sequence(buf, b);
        } else {
            buf.push(b);
        }
    }
}

/// Scalar processing for a range of bytes
#[inline]
fn write_escaped_scalar_range(buf: &mut Vec<u8>, bytes: &[u8], start: usize, end: usize) {
    for i in start..end {
        let b = bytes[i];
        if ESCAPE_LUT[b as usize] != EscapeAction::None {
            write_escape_sequence(buf, b);
        } else {
            buf.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_string() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "hello world");
        assert_eq!(String::from_utf8(buf).unwrap(), "\"hello world\"");
    }

    #[test]
    fn test_string_with_quotes() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "say \"hello\"");
        assert_eq!(String::from_utf8(buf).unwrap(), "\"say \\\"hello\\\"\"");
    }

    #[test]
    fn test_string_with_newline() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "line1\nline2");
        assert_eq!(String::from_utf8(buf).unwrap(), "\"line1\\nline2\"");
    }

    #[test]
    fn test_string_with_backslash() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "path\\to\\file");
        assert_eq!(String::from_utf8(buf).unwrap(), "\"path\\\\to\\\\file\"");
    }

    #[test]
    fn test_long_clean_string() {
        let s = "a".repeat(1000);
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, &s);
        assert_eq!(buf.len(), 1002); // 1000 + 2 quotes
        assert_eq!(buf[0], b'"');
        assert_eq!(buf[1001], b'"');
    }

    #[test]
    fn test_unicode_string() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "日本語テスト");
        // UTF-8 bytes should pass through unchanged
        let result = String::from_utf8(buf).unwrap();
        assert_eq!(result, "\"日本語テスト\"");
    }

    #[test]
    fn test_control_char() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "hello\x00world");
        assert_eq!(String::from_utf8(buf).unwrap(), "\"hello\\u0000world\"");
    }

    #[test]
    fn test_empty_string() {
        let mut buf = Vec::new();
        write_json_string_simd(&mut buf, "");
        assert_eq!(String::from_utf8(buf).unwrap(), "\"\"");
    }
}
