/// Performance optimizations module
///
/// This module contains various optimization strategies to improve
/// JSON serialization/deserialization performance.

pub mod object_cache;
pub mod type_cache;
pub mod bulk;
pub mod extreme;
pub mod escape_lut;
pub mod simd_parser;
pub mod simd_escape;
pub mod custom_parser;
pub mod raw_parser;
pub mod pylong_fast;
pub mod pyfloat_fast;
pub mod dict_key_fast;
pub mod raw_serialize;  // Phase 39: Raw C API serialization

/// Branch prediction hints for performance-critical code paths
///
/// These are no-ops on stable Rust but document intent and may be
/// optimized by LLVM based on code structure.
#[inline(always)]
#[allow(dead_code)]
pub fn likely(b: bool) -> bool {
    if !b {
        cold_path();
    }
    b
}

#[inline(always)]
pub fn unlikely(b: bool) -> bool {
    if b {
        cold_path();
    }
    b
}

/// Marker for cold (rarely executed) code paths
/// Helps LLVM optimize branch prediction
#[inline(never)]
#[cold]
fn cold_path() {}
