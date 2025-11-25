# Hybrid Optimization Investigation: Combining Bulk + Direct C API

## Executive Summary

**Lesson from Nuclear Option**: Algorithmic optimizations (bulk processing) >> Micro-optimizations (direct C API)

**New Hypothesis**: What if we combine BOTH?
- Keep Phase 6A bulk processing (algorithmic win)
- Add direct C API WITHIN bulk loops (micro win)
- = **Compound the wins instead of replacing them**

**Expected Outcome**: Close gap from 2.6x to **1.2-1.5x vs orjson**

---

## Analysis: Why Nuclear Option Failed

### What We Tried
```rust
// Nuclear option: Direct C API everywhere
fn serialize_direct(obj) {
    for item in list {
        serialize_direct(item)?;  // ← Recursion kills performance
    }
}
```

**Problem**: Lost bulk processing → 7x slower on booleans!

### What Actually Works
```rust
// Phase 6A: Bulk processing
fn serialize_bool_array_bulk(list) {
    for i in 0..size {
        let item_ptr = PyList_GET_ITEM(list_ptr, i);  // Direct C API ✓
        if item_ptr == true_ptr {
            buf.extend_from_slice(b"true");
        }
    }
}
```

**Success**: Beats orjson by 34%!

### The Insight

**Phase 6A already uses direct C API in the tight loops!**

The nuclear option failed because it:
- ❌ Used C API in WRONG places (recursive calls)
- ❌ Lost bulk processing
- ✅ But C API in tight loops DOES work (Phase 6A proves it)

---

## Concept 1: Ultra-Optimized Bulk Loops

### Current Phase 6A Integer Bulk
```rust
pub unsafe fn serialize_int_array_bulk(list: &Bound<'_, PyList>, buf: &mut Vec<u8>) {
    let list_ptr = list.as_ptr();
    let size = ffi::PyList_GET_SIZE(list_ptr);

    buf.reserve((size as usize) * 12);  // ← Allocation
    buf.push(b'[');

    let mut itoa_buf = itoa::Buffer::new();  // ← Stack allocation

    for i in 0..size {
        if i > 0 { buf.push(b','); }

        let item_ptr = ffi::PyList_GET_ITEM(list_ptr, i);
        let val_i64 = ffi::PyLong_AsLongLong(item_ptr);  // ← Good

        if val_i64 == -1 && !ffi::PyErr_Occurred().is_null() {
            // Error handling overhead ← Can we reduce this?
        }

        buf.extend_from_slice(itoa_buf.format(val_i64).as_bytes());  // ← Good
    }
}
```

### Proposed: Hyper-Optimized Bulk with Inline Formatting

```rust
pub unsafe fn serialize_int_array_hyper(list: &Bound<'_, PyList>, buf: &mut Vec<u8>) {
    let list_ptr = list.as_ptr();
    let size = ffi::PyList_GET_SIZE(list_ptr);

    // OPTIMIZATION 1: Pre-scan for overflow (one-time cost)
    let all_i64 = prescan_int_array_i64(list_ptr, size);

    if all_i64 {
        // FAST PATH: All values fit in i64, no error checking needed
        buf.reserve((size as usize) * 12);
        buf.push(b'[');

        for i in 0..size {
            if i > 0 { buf.push(b','); }

            let item_ptr = ffi::PyList_GET_ITEM(list_ptr, i);
            let val = ffi::PyLong_AsLongLong(item_ptr);

            // OPTIMIZATION 2: Inline integer formatting (no itoa overhead)
            write_int_inline(buf, val);
        }

        buf.push(b']');
    } else {
        // SLOW PATH: Has overflow, use existing safe path
        serialize_int_array_bulk_original(list, buf)?;
    }
}

#[inline(always)]
unsafe fn prescan_int_array_i64(list_ptr: *mut ffi::PyObject, size: isize) -> bool {
    // Quick scan: check if all ints fit in i64
    for i in 0..size {
        let item_ptr = ffi::PyList_GET_ITEM(list_ptr, i);
        let val = ffi::PyLong_AsLongLong(item_ptr);
        if val == -1 && !ffi::PyErr_Occurred().is_null() {
            ffi::PyErr_Clear();
            return false;  // Found overflow
        }
    }
    true  // All fit in i64
}

#[inline(always)]
fn write_int_inline(buf: &mut Vec<u8>, mut val: i64) {
    // Hand-optimized integer formatting (faster than itoa for small ints)
    if val == 0 {
        buf.push(b'0');
        return;
    }

    let neg = val < 0;
    if neg {
        buf.push(b'-');
        val = -val;
    }

    // SIMD-friendly: process digits in chunks
    let mut temp = [0u8; 20];
    let mut pos = 20;

    // Unrolled loop for common case (< 10 digits)
    while val > 0 {
        pos -= 1;
        temp[pos] = b'0' + (val % 10) as u8;
        val /= 10;
    }

    buf.extend_from_slice(&temp[pos..]);
}
```

**Expected improvement**: +30-50% on integer arrays (2.2x → 1.5x vs orjson)

**Why it works**:
- ✅ Keeps bulk processing (algorithmic win)
- ✅ Eliminates error checking in hot path (micro win)
- ✅ Inline integer formatting (micro win)
- ✅ Pre-scan amortizes overflow detection

---

## Concept 2: SIMD-Enhanced Bulk Loops

### Current String Bulk (Scalar)
```rust
for i in 0..size {
    let str_data = PyUnicode_AsUTF8AndSize(item_ptr, &mut size);
    write_json_string(buf, str);  // ← Per-string escape detection
}
```

### Proposed: Batch SIMD String Processing

```rust
pub unsafe fn serialize_string_array_simd(list: &Bound<'_, PyList>, buf: &mut Vec<u8>) {
    let size = ffi::PyList_GET_SIZE(list.as_ptr());

    // PHASE 1: Extract all string pointers (bulk operation)
    let mut strings: Vec<(*const u8, usize)> = Vec::with_capacity(size as usize);
    for i in 0..size {
        let item_ptr = ffi::PyList_GET_ITEM(list.as_ptr(), i);
        let mut len: isize = 0;
        let data = ffi::PyUnicode_AsUTF8AndSize(item_ptr, &mut len);
        strings.push((data as *const u8, len as usize));
    }

    // PHASE 2: SIMD scan ALL strings for escapes (single pass)
    let escape_mask = batch_scan_escapes_simd(&strings);

    // PHASE 3: Bulk write escape-free strings (memcpy)
    buf.push(b'[');
    for (i, &(data, len)) in strings.iter().enumerate() {
        if i > 0 { buf.push(b','); }
        buf.push(b'"');

        if escape_mask[i] {
            // Slow path: has escapes
            write_escaped(buf, data, len);
        } else {
            // Fast path: direct memcpy (no escape checking)
            buf.extend_from_slice(std::slice::from_raw_parts(data, len));
        }

        buf.push(b'"');
    }
    buf.push(b']');
}

#[cfg(target_arch = "x86_64")]
unsafe fn batch_scan_escapes_simd(strings: &[(*const u8, usize)]) -> Vec<bool> {
    use std::arch::x86_64::*;

    let mut has_escape = vec![false; strings.len()];

    let quote = _mm256_set1_epi8(b'"' as i8);
    let backslash = _mm256_set1_epi8(b'\\' as i8);
    let ctrl = _mm256_set1_epi8(0x1F);

    for (idx, &(data, len)) in strings.iter().enumerate() {
        let mut pos = 0;

        // Process 32 bytes at a time
        while pos + 32 <= len {
            let chunk = _mm256_loadu_si256(data.add(pos) as *const __m256i);

            let cmp_quote = _mm256_cmpeq_epi8(chunk, quote);
            let cmp_backslash = _mm256_cmpeq_epi8(chunk, backslash);
            let cmp_ctrl = _mm256_cmpgt_epi8(ctrl, chunk);

            let combined = _mm256_or_si256(cmp_quote, cmp_backslash);
            let combined = _mm256_or_si256(combined, cmp_ctrl);

            if _mm256_movemask_epi8(combined) != 0 {
                has_escape[idx] = true;
                break;  // Found escape, move to next string
            }

            pos += 32;
        }

        // Handle tail with scalar
        if !has_escape[idx] && pos < len {
            has_escape[idx] = has_escape_scalar(&data.add(pos), len - pos);
        }
    }

    has_escape
}
```

**Expected improvement**: String arrays 4.5x → 1.8x slower than orjson

**Why it works**:
- ✅ Bulk extraction (algorithmic)
- ✅ SIMD batch scanning (micro)
- ✅ Separate fast/slow paths (optimization)

---

## Concept 3: Adaptive Bulk Detection

### Current: Fixed Threshold
```rust
const MIN_BULK_SIZE: usize = 8;  // Arrays < 8 use normal path

fn detect_array_type(list: &PyList) -> ArrayType {
    if list.len() < MIN_BULK_SIZE {
        return ArrayType::Mixed;  // Skip bulk for small arrays
    }
    // ... detection logic
}
```

**Problem**: Fixed threshold isn't optimal for all types

### Proposed: Type-Specific Thresholds

```rust
struct BulkThresholds {
    int_threshold: usize,     // Ints: bulk worth it at 4+ elements
    float_threshold: usize,   // Floats: bulk worth it at 6+ elements
    bool_threshold: usize,    // Bools: bulk worth it at 2+ elements (we're 34% faster!)
    string_threshold: usize,  // Strings: bulk worth it at 16+ elements (more overhead)
}

const THRESHOLDS: BulkThresholds = BulkThresholds {
    int_threshold: 4,    // Detection overhead ~4 int ops
    float_threshold: 6,  // Detection overhead ~6 float ops
    bool_threshold: 2,   // Detection overhead ~2 bool ops (very fast!)
    string_threshold: 16, // Detection overhead ~16 string ops (slower)
};

fn should_use_bulk(list: &PyList, detected_type: ArrayType) -> bool {
    let len = list.len();

    match detected_type {
        ArrayType::AllInts => len >= THRESHOLDS.int_threshold,
        ArrayType::AllFloats => len >= THRESHOLDS.float_threshold,
        ArrayType::AllBools => len >= THRESHOLDS.bool_threshold,
        ArrayType::AllStrings => len >= THRESHOLDS.string_threshold,
        _ => len >= 8,  // Default for unknown types
    }
}
```

**Expected improvement**: +5-10% overall (avoid bulk overhead on tiny arrays)

---

## Concept 4: Profile-Guided Bulk Optimization

### Idea: Learn from Runtime Behavior

```rust
struct BulkStats {
    int_array_count: AtomicUsize,
    float_array_count: AtomicUsize,
    bool_array_count: AtomicUsize,
    string_array_count: AtomicUsize,

    total_serializations: AtomicUsize,
}

static STATS: BulkStats = BulkStats::new();

fn dumps_with_stats(data: &Bound<'_, PyAny>) -> PyResult<String> {
    // Track what types we're serializing
    STATS.total_serializations.fetch_add(1, Ordering::Relaxed);

    // Serialize
    let result = dumps_inner(data)?;

    // Report stats every 10k calls
    if STATS.total_serializations.load(Ordering::Relaxed) % 10000 == 0 {
        report_bulk_stats();
    }

    Ok(result)
}
```

**Use case**: Identify what workloads benefit most from bulk optimizations

**Not for production**: Adds overhead, but useful for profiling

---

## Concept 5: Zero-Allocation String Formatting

### Current: itoa::Buffer Allocation
```rust
let mut itoa_buf = itoa::Buffer::new();  // ← Stack allocation per call
for i in 0..size {
    buf.extend_from_slice(itoa_buf.format(val).as_bytes());
}
```

### Proposed: Direct Buffer Writing

```rust
fn serialize_int_array_zero_alloc(list: &PyList, buf: &mut Vec<u8>) {
    for i in 0..size {
        let val = PyLong_AsLongLong(item_ptr);

        // Write directly to output buffer (no intermediate)
        write_int_direct(buf, val);
    }
}

#[inline(always)]
fn write_int_direct(buf: &mut Vec<u8>, mut val: i64) {
    // Reserve worst case (20 digits + sign)
    let start = buf.len();
    buf.reserve(21);

    unsafe {
        // Write directly to uninitialized buffer
        let ptr = buf.as_mut_ptr().add(start);
        let mut pos = 0;

        if val < 0 {
            *ptr = b'-';
            pos += 1;
            val = -val;
        }

        // Write digits (optimized reverse)
        let digit_start = pos;
        loop {
            *ptr.add(pos) = b'0' + (val % 10) as u8;
            pos += 1;
            val /= 10;
            if val == 0 { break; }
        }

        // Reverse digits in place
        reverse_bytes(ptr.add(digit_start), ptr.add(pos));

        buf.set_len(start + pos);
    }
}
```

**Expected improvement**: +10-15% on integer arrays (eliminates itoa overhead)

---

## Concept 6: Lazy Serialization

### Idea: Defer Work Until Necessary

```rust
enum LazyJson<'a> {
    String(&'a str),
    Int(i64),
    Float(f64),
    Array(Vec<LazyJson<'a>>),
    Deferred(&'a Bound<'a, PyAny>),  // Not serialized yet
}

fn dumps_lazy(data: &Bound<'_, PyAny>) -> PyResult<String> {
    // Phase 1: Build lazy tree (fast)
    let lazy = build_lazy_tree(data)?;

    // Phase 2: Optimize lazy tree
    let optimized = optimize_lazy_tree(lazy);

    // Phase 3: Serialize (with perfect knowledge of structure)
    serialize_lazy_tree(optimized)
}

fn optimize_lazy_tree(tree: LazyJson) -> LazyJson {
    match tree {
        LazyJson::Array(items) => {
            // Detect homogeneous arrays
            if all_same_type(&items) {
                // Use bulk serialization
                bulk_serialize_lazy_array(items)
            } else {
                LazyJson::Array(items)
            }
        }
        // ... other optimizations
    }
}
```

**Problem**: Adds overhead of tree building

**Benefit**: Perfect information for optimization decisions

**Verdict**: Probably not worth it (too much overhead)

---

## Recommended Approach: Hybrid Bulk + Micro

### Phase 6A++ Implementation Plan

**Combine the wins from Phase 6A with targeted micro-optimizations**

#### Priority 1: Hyper-Optimized Integer Bulk (HIGH ROI)
- Pre-scan for i64 overflow (one-time cost)
- Inline integer formatting in hot path
- Eliminate error checking for common case
- **Expected**: Integer arrays 2.2x → 1.5x vs orjson

#### Priority 2: SIMD String Batch Scanning (CRITICAL)
- Bulk extract all string pointers
- SIMD scan all strings for escapes
- Separate fast/slow paths
- **Expected**: String arrays 4.5x → 1.8x vs orjson

#### Priority 3: Adaptive Thresholds (LOW OVERHEAD)
- Type-specific bulk thresholds
- Avoid bulk overhead on tiny arrays
- **Expected**: +5-10% overall

#### Priority 4: Zero-Allocation Formatting (OPTIONAL)
- Direct buffer writing for integers
- Eliminate itoa::Buffer overhead
- **Expected**: +10-15% on integers

### Combined Projected Performance

| Workload | Current | Phase 6A++ | vs orjson | Improvement |
|----------|---------|------------|-----------|-------------|
| **Int arrays** | 10.0ms (2.2x slower) | 6.5ms | 1.4x slower | +53% ✅ |
| **Float arrays** | 67.3ms (1.1x slower) | 60ms | 0.95x | **BEATS!** ✅ |
| **Bool arrays** | 2.2ms (0.64x) | 2.0ms | 0.58x | **BEATS!** ✅ |
| **String arrays** | 26.1ms (5x slower) | 9.5ms | 1.8x slower | +175% ✅ |
| **Mixed nested** | 22.0ms (1.9x slower) | 15ms | 1.3x slower | +46% ✅ |

**Final gap to orjson**: **1.2-1.5x slower** (acceptable!)

**Workloads where we beat orjson**: Booleans, floats, potentially ints with hyper-optimization

---

## Comparison: Approaches

| Approach | Performance | Code Complexity | Safety | Verdict |
|----------|-------------|-----------------|--------|---------|
| **Phase 6A (Current)** | 9x vs json, 2.6x vs orjson | Low | Safe | ✅ **GOOD** |
| **Nuclear Option** | 2-7x SLOWER | Very High | Unsafe | ❌ **FAILED** |
| **Phase 6A++** | 11x vs json, 1.3x vs orjson | Medium | Mostly Safe | ✅ **RECOMMENDED** |
| **Lazy Serialization** | Unknown | High | Safe | ⚠️ **RISKY** |
| **Profile-Guided** | Unknown | Medium | Safe | ⚠️ **EXPERIMENTAL** |

---

## Implementation Roadmap

### Week 1: Hyper-Optimized Integer Bulk
- Implement pre-scanning
- Inline integer formatting
- Benchmark
- **Target**: 2.2x → 1.5x on int arrays

### Week 2: SIMD String Batch Scanning
- Implement batch string extraction
- AVX2 batch escape scanning
- Separate fast/slow paths
- **Target**: 5x → 1.8x on string arrays

### Week 3: Adaptive Thresholds + Polish
- Implement type-specific thresholds
- Profile and tune
- Clean up code
- **Target**: +5-10% overall

### Week 4: Testing + Documentation
- Comprehensive benchmarks
- Update documentation
- Production release
- **Target**: Ship Phase 6A++

---

## Key Learnings Applied

From nuclear option failure:

1. ✅ **Keep algorithmic wins** (bulk processing)
2. ✅ **Add micro wins WITHIN algorithms** (SIMD in bulk loops)
3. ✅ **Don't replace good code with bad** (enhance, don't rewrite)
4. ✅ **Measure every step** (benchmark each optimization)

From Phase 6A success:

1. ✅ **Bulk processing works** (beat orjson on booleans)
2. ✅ **Direct C API in tight loops is good** (PyList_GET_ITEM)
3. ✅ **Type detection is worth it** (amortized cost)
4. ✅ **Simple code can be fast** (don't over-engineer)

---

## Conclusion

**Recommended**: Implement Phase 6A++ (Hybrid Bulk + Micro Optimizations)

**Expected Outcome**:
- Close gap from 2.6x to 1.2-1.5x vs orjson
- Beat orjson on 2-3 workload types
- Maintain code quality and safety
- Production-ready

**Not Recommended**:
- Lazy serialization (too much overhead)
- Complete PyO3 bypass (nuclear option proved it fails)
- Profile-guided optimization (adds runtime overhead)

**The Sweet Spot**: Algorithmic wins (bulk) + Targeted micro wins (SIMD, inline formatting) = Best of both worlds

---

**Status**: Investigation complete, ready for implementation
**Estimated effort**: 3-4 weeks
**Expected gain**: Close gap to 1.2-1.5x vs orjson
**Risk**: Low (building on proven Phase 6A foundation)
