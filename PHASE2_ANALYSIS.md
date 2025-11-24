# Phase 2: Expert Analysis - Closing the Gap to orjson

## Current Performance Gap

**Baseline**: After Phase 1.5+
- dumps: **2.88x slower** than orjson (0.176s vs 0.061s)
- loads: **2.09x slower** than orjson (0.598s vs 0.287s)

**Target**: Close gap to 1.5-2x slower (achievable without full rewrite)

## Root Cause Analysis: Why is orjson Faster?

### 1. **dumps Path (2.88x gap)**

#### orjson's Advantages:
```
┌─────────────────────────────────────────────────────────────┐
│ orjson dumps Architecture (Rust expert analysis)            │
├─────────────────────────────────────────────────────────────┤
│ 1. Direct buffer writing (no serde_json)                    │
│    - Pre-allocates Vec<u8> with size estimate               │
│    - Writes JSON bytes directly                             │
│    - Zero intermediate allocations                          │
│                                                              │
│ 2. Custom number formatting                                 │
│    - itoa crate: 10x faster than fmt::Display for integers  │
│    - ryu crate: 5x faster than fmt::Display for floats      │
│    - Eliminates UTF-8 validation overhead                   │
│                                                              │
│ 3. Optimized string escaping                                │
│    - SIMD-based string scanning (AVX2)                      │
│    - Bulk memcpy for unescaped runs                         │
│    - Lookup table for escape sequences                      │
│                                                              │
│ 4. No GIL release/reacquire                                 │
│    - Holds GIL for entire serialization                     │
│    - We do the same ✅                                       │
└─────────────────────────────────────────────────────────────┘
```

#### rjson's Current Bottlenecks:
```rust
// CURRENT: serde_json::to_string
serde_json::to_string(&PyAnySerialize { obj: data })

// PROBLEMS:
// 1. Uses fmt::Display for numbers (slow)
// 2. No buffer pre-sizing (reallocs during write)
// 3. Generic serde path (not specialized)
// 4. Extra UTF-8 validation passes
```

**Estimated dumps gap breakdown**:
- Number formatting: **40%** (itoa/ryu would fix)
- Buffer reallocation: **25%** (pre-sizing would fix)
- String escaping: **20%** (SIMD would fix - Phase 3+)
- serde overhead: **15%** (custom serializer would fix)

### 2. **loads Path (2.09x gap)**

#### orjson's Advantages:
```
┌─────────────────────────────────────────────────────────────┐
│ orjson loads Architecture                                    │
├─────────────────────────────────────────────────────────────┤
│ 1. Custom JSON parser (simd-json based)                     │
│    - SIMD whitespace skipping                               │
│    - SIMD string validation                                 │
│    - Branchless number parsing                              │
│                                                              │
│ 2. Zero-copy string handling                                │
│    - PyUnicode_FromStringAndSize with buffer pointer        │
│    - No intermediate String allocation                      │
│    - Direct UTF-8 validation with SIMD                      │
│                                                              │
│ 3. Integer interning (like CPython)                         │
│    - We do this ✅                                           │
│                                                              │
│ 4. Specialized dict construction                            │
│    - Pre-sized dict with exact capacity                     │
│    - Bulk insertion without hash checks                     │
│    - We do direct insertion ✅                               │
└─────────────────────────────────────────────────────────────┘
```

#### rjson's Current Bottlenecks:
```rust
// CURRENT: serde_json::Deserializer
serde_json::Deserializer::from_str(json_str)

// PROBLEMS:
// 1. No SIMD (byte-by-byte parsing)
// 2. Intermediate String allocations
// 3. Multiple UTF-8 validation passes
// 4. Branch-heavy number parsing
```

**Estimated loads gap breakdown**:
- SIMD parsing: **50%** (would need custom parser - Phase 3+)
- String allocations: **25%** (zero-copy would fix)
- Number parsing: **15%** (branchless would fix)
- Generic serde: **10%** (custom deserializer)

## Phase 2 Optimization Strategy

### High-Impact Quick Wins (This Session)

#### 1. Custom Number Formatting (dumps) 🎯
**Impact**: +30-40% dumps performance
**Effort**: Low (add dependencies, swap formatting)
**Dependencies**: `itoa`, `ryu`

```rust
// BEFORE: fmt::Display (slow)
serializer.serialize_i64(val_i64)

// AFTER: itoa (10x faster)
let mut buf = itoa::Buffer::new();
serializer.serialize_str(buf.format(val_i64))
```

#### 2. Pre-sized Output Buffer (dumps) 🎯
**Impact**: +10-15% dumps performance
**Effort**: Medium (size estimation heuristic)

```rust
// Estimate JSON size before serializing
fn estimate_json_size(obj: &PyAny) -> usize {
    match type_cache::get_fast_type(obj) {
        FastType::String => obj.len() + 2 + escapes,
        FastType::Int => 20, // max i64 digits
        FastType::List => sum(estimate_json_size(item)) + len,
        // ...
    }
}

// Pre-allocate buffer
let capacity = estimate_json_size(data);
let mut buf = Vec::with_capacity(capacity);
```

#### 3. Direct Buffer Writing (dumps) 🎯
**Impact**: +15-20% dumps performance
**Effort**: High (custom serializer)

Replace `serde_json::to_string` with custom buffer-based serializer that:
- Writes directly to `Vec<u8>`
- Uses itoa/ryu for numbers
- Minimizes allocations
- Avoids intermediate String creation

#### 4. String Interning for Dict Keys (loads) 🎯
**Impact**: +8-12% loads performance
**Effort**: Medium (string deduplication)

```rust
// Cache repeated dict keys
static KEY_CACHE: OnceLock<DashMap<String, PyObject>> = OnceLock::new();

fn get_interned_key(py: Python, key: &str) -> PyObject {
    KEY_CACHE.entry(key.to_owned())
        .or_insert_with(|| key.to_object(py))
        .clone()
}
```

### Implementation Plan

**Session Goal**: Implement optimizations to achieve:
- dumps: **2x slower** than orjson (currently 2.88x) = **+44% improvement**
- loads: **1.7x slower** than orjson (currently 2.09x) = **+23% improvement**

**Priority Order**:
1. ✅ Custom number formatting (itoa/ryu) - **highest ROI**
2. ✅ Pre-sized buffer estimation
3. ✅ Direct buffer serializer
4. ⏭️ String key interning (if time permits)

## Detailed Implementation: Custom Number Formatting

### Step 1: Add Dependencies

```toml
# Cargo.toml
[dependencies]
itoa = "1.0"
ryu = "1.0"
```

### Step 2: Custom Serializer with Fast Numbers

```rust
use itoa;
use ryu;

impl Serialize for PyAnySerialize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match fast_type {
            FastType::Int => {
                // FAST PATH: Use itoa for integer formatting
                let l_val = unsafe { obj.downcast_exact::<PyInt>().unwrap_unchecked() };
                if let Ok(val_i64) = l_val.extract::<i64>() {
                    let mut buf = itoa::Buffer::new();
                    serializer.serialize_str(buf.format(val_i64))
                } else if let Ok(val_u64) = l_val.extract::<u64>() {
                    let mut buf = itoa::Buffer::new();
                    serializer.serialize_str(buf.format(val_u64))
                }
                // ... handle large ints
            }

            FastType::Float => {
                // FAST PATH: Use ryu for float formatting
                let f_val = unsafe { obj.downcast_exact::<PyFloat>().unwrap_unchecked() };
                let val_f64 = f_val.extract::<f64>().map_err(serde::ser::Error::custom)?;
                let mut buf = ryu::Buffer::new();
                serializer.serialize_str(buf.format(val_f64))
            }
            // ... rest
        }
    }
}
```

**Problem**: serde's `Serializer` trait expects `serialize_i64()` not `serialize_str()`.

**Solution**: We need a **custom serializer** that writes directly to a buffer, not using serde's trait.

## Phase 2.1: Direct Buffer Serializer (Complete Solution)

### Architecture

```rust
struct JsonBuffer {
    buf: Vec<u8>,
}

impl JsonBuffer {
    fn write_int(&mut self, val: i64) {
        let mut itoa_buf = itoa::Buffer::new();
        self.buf.extend_from_slice(itoa_buf.format(val).as_bytes());
    }

    fn write_float(&mut self, val: f64) {
        let mut ryu_buf = ryu::Buffer::new();
        self.buf.extend_from_slice(ryu_buf.format(val).as_bytes());
    }

    fn write_string(&mut self, s: &str) {
        self.buf.push(b'"');
        // TODO: escape special characters
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(b'"');
    }

    fn serialize_pyany(&mut self, obj: &Bound<PyAny>) -> PyResult<()> {
        match get_fast_type(obj) {
            FastType::None => self.buf.extend_from_slice(b"null"),
            FastType::Bool => { /* ... */ },
            FastType::Int => { /* use write_int */ },
            FastType::Float => { /* use write_float */ },
            FastType::String => { /* use write_string */ },
            FastType::List => {
                self.buf.push(b'[');
                // recurse...
                self.buf.push(b']');
            }
            FastType::Dict => { /* ... */ }
        }
        Ok(())
    }
}

#[pyfunction]
fn dumps(_py: Python, data: &Bound<PyAny>) -> PyResult<String> {
    let capacity = estimate_json_size(data);
    let mut buf = JsonBuffer { buf: Vec::with_capacity(capacity) };
    buf.serialize_pyany(data)?;

    // SAFETY: We only write valid UTF-8
    Ok(unsafe { String::from_utf8_unchecked(buf.buf) })
}
```

### Estimated Performance Impact

```
Current dumps: 0.176s
After itoa/ryu: 0.123s (-30%)
After pre-sizing: 0.111s (-37%)
After direct buffer: 0.088s (-50%)

orjson: 0.061s
Gap: 1.44x (vs current 2.88x) ✅ GOAL ACHIEVED
```

## Risk Assessment

### Low Risk ✅
- Adding itoa/ryu dependencies (battle-tested crates)
- Pre-sized buffer estimation (worst case: over-allocate)
- Custom serializer for simple types

### Medium Risk ⚠️
- String escaping logic (must handle all JSON escape sequences)
- Large integer handling (beyond i64/u64)
- NaN/Infinity float handling

### High Risk 🔴
- Unsafe string conversion (must guarantee UTF-8)
- Dict key ordering (JSON spec allows any order, but users may expect consistency)

## Next Steps

1. **Implement custom serializer with itoa/ryu** (this session)
2. **Add comprehensive tests** (before and after)
3. **Benchmark each optimization** incrementally
4. **Document performance improvements**
5. **Commit with detailed analysis**

## Success Criteria

- [ ] dumps: <0.09s (2x slower than orjson, vs current 2.88x)
- [ ] loads: <0.49s (1.7x slower than orjson, vs current 2.09x)
- [ ] All tests pass
- [ ] Zero unsafe code violations
- [ ] Clean code quality (no hacks)

---

**Status**: Ready to implement
**Estimated Time**: 2-3 hours for full Phase 2.1
**Confidence**: High (proven techniques from orjson)
