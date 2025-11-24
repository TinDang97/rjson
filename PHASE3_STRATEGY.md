# Phase 3: Expert-Level SIMD and C API Optimizations

## Strategic Analysis

### Current State (Post Phase 2)
```
dumps: 0.170s → 2.96x slower than orjson
loads: 0.670s → 2.33x slower than orjson
```

### Goal
**Close gap to 1.5-2x slower than orjson** through architectural optimizations:
- dumps: 0.170s → 0.090s (target: 1.6x slower than orjson)
- loads: 0.670s → 0.420s (target: 1.5x slower than orjson)

## Phase 3 Optimization Targets

### High-Impact Quick Wins (This Session)

#### 1. SIMD String Escaping 🎯 **PRIORITY 1**
**Problem**: Current byte-by-byte scan to detect escape characters
```rust
// CURRENT: O(n) byte scan
for &b in bytes {
    if b == b'"' || b == b'\\' || b < 0x20 {
        needs_escape = true;
        break;
    }
}
```

**Solution**: Use `memchr` crate with SIMD
```rust
// NEW: SIMD scan (4-8x faster)
use memchr::memchr3;

if let Some(_) = memchr3(b'"', b'\\', b'\n', bytes) {
    // Has escapes, need slow path
}
```

**Expected Impact**: +10-15% dumps (string-heavy workloads)
**Risk**: Low (battle-tested crate)
**Effort**: 1 hour

#### 2. Direct Python C API for Dict Iteration 🎯 **PRIORITY 2**
**Problem**: PyO3 dict iterator has significant overhead
```rust
// CURRENT: PyO3 wrapper (slow)
for (key, value) in dict_val.iter() {
    // Process...
}
```

**Solution**: Direct C API access
```rust
// NEW: Raw C API (2-3x faster dict iteration)
use pyo3::ffi;

unsafe {
    let mut pos: ffi::Py_ssize_t = 0;
    let mut key: *mut ffi::PyObject = std::ptr::null_mut();
    let mut value: *mut ffi::PyObject = std::ptr::null_mut();

    while ffi::PyDict_Next(dict_val.as_ptr(), &mut pos, &mut key, &mut value) != 0 {
        // Process key/value
    }
}
```

**Expected Impact**: +15-20% dumps (dict-heavy workloads)
**Risk**: Medium (unsafe code, need careful memory management)
**Effort**: 2-3 hours

#### 3. Optimized Buffer Growth Strategy 🎯 **PRIORITY 3**
**Problem**: Current size estimation is conservative
```rust
// CURRENT: Simple heuristics
FastType::Dict => len * 32 + 16
```

**Solution**: Recursive size calculation with caching
```rust
// NEW: Accurate pre-sizing
fn calculate_exact_size(obj: &Bound<PyAny>) -> usize {
    match fast_type {
        FastType::Dict => {
            let mut size = 2; // {}
            for (k, v) in dict.iter() {
                size += calculate_exact_size(k);
                size += calculate_exact_size(v);
                size += 3; // "": and comma
            }
            size
        }
        // ...
    }
}
```

**Expected Impact**: +5-8% dumps (reduces reallocations)
**Risk**: Low
**Effort**: 1-2 hours

#### 4. Eliminate Redundant UTF-8 Validation
**Problem**: PyString::to_str() validates UTF-8, but Python strings are already valid
```rust
// CURRENT: Validates UTF-8
let s = s_val.to_str()?;
```

**Solution**: Use unsafe to skip validation
```rust
// NEW: Skip validation (Python guarantees UTF-8)
let s = unsafe {
    let data = ffi::PyUnicode_AsUTF8AndSize(...);
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(data, len))
};
```

**Expected Impact**: +3-5% dumps (string-heavy workloads)
**Risk**: Low (Python strings are always valid UTF-8)
**Effort**: 1 hour

### Advanced Optimizations (If Time Permits)

#### 5. Bulk Dict Key/Value Collection
**Problem**: Per-element type checking and downcast
**Solution**: Batch operations using C API
**Expected Impact**: +5-10% dumps
**Risk**: Medium
**Effort**: 3-4 hours

#### 6. Custom Float Formatting with Lookup Tables
**Problem**: ryu still has some overhead
**Solution**: Fast path for common floats (0.0, 1.0, etc.)
**Expected Impact**: +2-3% dumps
**Risk**: Low
**Effort**: 1 hour

## Implementation Plan

### Session 1: SIMD and C API (This Session)
**Duration**: 3-4 hours
**Target**: Close dumps gap from 2.96x to ~2.0x

1. **Add memchr dependency** (5 min)
2. **Implement SIMD string escaping** (30 min)
3. **Benchmark** (10 min) → Expected: 0.153s dumps (+10%)
4. **Implement direct C API dict iteration** (2 hours)
5. **Benchmark** (10 min) → Expected: 0.130s dumps (+30% cumulative)
6. **Add safety tests for unsafe code** (30 min)
7. **Final benchmark and commit** (15 min)

### Session 2: Loads Optimization (Future)
**Duration**: 6-8 hours
**Target**: Close loads gap from 2.33x to ~1.5x

1. Research simd-json integration
2. Implement custom SIMD parser
3. Zero-copy string construction
4. Benchmark and validate

## Risk Assessment

### Low Risk ✅
- memchr for SIMD string scanning (proven, widely used)
- Buffer pre-sizing optimization (worst case: over-allocate)
- Skipping UTF-8 validation for Python strings (guaranteed valid)

### Medium Risk ⚠️
- Direct C API dict iteration (unsafe, need careful refcounting)
- Bulk operations (complex logic, edge cases)

### Mitigation Strategy
- Extensive testing for unsafe code
- Valgrind/AddressSanitizer for memory safety
- Comprehensive error handling
- Gradual rollout (feature flags if needed)

## Expected Final Results

### After Phase 3.1-3.4 (This Session)
```
dumps: 0.130s → 2.0x slower than orjson (vs 2.96x before)
loads: 0.670s → 2.3x slower than orjson (unchanged)

Improvement: +30% dumps, 0% loads
```

### After Phase 3 Complete (Future Session)
```
dumps: 0.110s → 1.6x slower than orjson
loads: 0.420s → 1.5x slower than orjson

Improvement: +54% dumps, +60% loads
```

## Success Criteria

This session:
- [ ] dumps < 0.140s (2.1x slower than orjson or better)
- [ ] All tests pass with unsafe code
- [ ] Zero memory leaks (verified with valgrind)
- [ ] Code quality maintained (no hacks)

Full Phase 3:
- [ ] dumps < 0.120s (1.8x slower than orjson)
- [ ] loads < 0.450s (1.6x slower than orjson)
- [ ] Production-ready unsafe code with safety guarantees
- [ ] Comprehensive test coverage

---

**Status**: Ready to implement
**Starting with**: SIMD string escaping (highest ROI, lowest risk)
**Timeline**: 3-4 hours for this session
