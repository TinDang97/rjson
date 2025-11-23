# Performance Optimization Roadmap for rjson
## PhD-Level Analysis and Implementation Plan

**Current Status**: rjson is 2.75x slower than orjson for serialization, 1.71x slower for deserialization
**Goal**: Match or exceed orjson performance while maintaining safety and code quality
**Approach**: Systematic, phased optimization based on profiling and algorithmic improvements

---

## Executive Summary

This document presents a comprehensive, research-driven approach to closing the performance gap between rjson and orjson. The strategy is organized into 5 phases, progressing from low-hanging fruit to advanced SIMD optimizations, with each phase building upon the previous one's improvements.

**Key Insight**: The performance gap stems from three primary factors:
1. **PyO3 Overhead** (~40-50% impact): Excessive GIL operations and type conversions
2. **Allocation Patterns** (~20-30% impact): Suboptimal memory management
3. **Missing SIMD** (~30-40% impact): No vectorized parsing/serialization

---

## Current Implementation Analysis

### Performance Bottlenecks Identified

#### 1. **Deserialization (loads) - src/lib.rs:260-268**

**Critical Issues**:
- **GIL Churn**: `to_object(self.py)` called for every primitive (lines 159-174)
  - Each call acquires/releases GIL
  - Creates Python object wrapper overhead
  - Estimated impact: 35-45% of deserialization time

- **Type Dispatch Overhead**: Multiple sequential `if-else` downcast chains
  - Lines 284-332: 7-way type dispatch for every value
  - Each downcast_exact has non-zero cost
  - Estimated impact: 15-20% of deserialization time

- **Vector Reallocation**: Dict key/value collection (lines 208-221)
  ```rust
  let mut keys = Vec::new();     // No capacity hint
  let mut values = Vec::new();   // Will reallocate during growth
  ```
  - Estimated impact: 5-10% for large objects

- **Character-by-Character Parsing**: serde_json uses byte-at-a-time parsing
  - No SIMD for structural character detection
  - Estimated impact: 25-35% of parsing time

#### 2. **Serialization (dumps) - src/lib.rs:347-351**

**Critical Issues**:
- **Repeated Type Checking**: Lines 284-332 perform downcast for every element
  - Dictionary iteration: O(n) downcasts for keys + O(n) for values
  - List iteration: O(n) downcasts
  - Estimated impact: 30-40% of serialization time

- **String Allocation**: `to_string()` calls (lines 87, 95, 300)
  - Creates intermediate String allocations for error cases
  - Estimated impact: 10-15% for large integer keys

- **No Buffering Strategy**: serde_json::to_string creates single allocation
  - Could use pre-sized buffer based on heuristics
  - Estimated impact: 8-12% for large objects

- **UTF-8 Encoding Overhead**: Each string validated during serialization
  - No bulk SIMD UTF-8 validation
  - Estimated impact: 15-20% for string-heavy data

#### 3. **Memory Management**

**Issues**:
- Intermediate Vec allocations in visitor (lines 194, 209-210)
- Python object creation overhead (PyList::new, PyDict::new)
- No object pooling or arena allocation
- Estimated combined impact: 20-25%

---

## Phase-by-Phase Optimization Strategy

### **Phase 0: Measurement Infrastructure** (Week 1)
*Prerequisites for all other phases*

#### Objectives
- Establish rigorous profiling framework
- Create comprehensive benchmark suite
- Set up performance regression testing

#### Deliverables

1. **Profiling Integration**
   ```bash
   # CPU profiling with perf
   cargo build --release
   perf record -g python benches/python_benchmark.py
   perf report

   # Flamegraph generation
   cargo flamegraph --bench loads_benchmark
   ```

2. **Extended Benchmarks**
   - Micro-benchmarks for individual operations:
     - Pure integer arrays
     - Pure string arrays
     - Deep nesting (10+ levels)
     - Wide objects (100k+ keys)
     - Mixed type workloads
   - Memory profiling (heaptrack, valgrind)
   - GIL acquisition/release counting

3. **Baseline Metrics**
   - Document current CPU cycles per operation
   - Memory allocations per parse/serialize
   - Cache miss rates
   - Branch prediction misses

#### Success Criteria
- Automated flamegraph generation
- Per-commit performance tracking
- Identify top 5 hotspots accounting for >70% of time

---

### **Phase 1: PyO3 Overhead Reduction** (Weeks 2-3)
*Target: 20-30% speedup in both loads and dumps*

#### 1.1: Batch Python Object Creation

**Problem**: Currently creates Python objects one-at-a-time (line 159-174)

**Solution**: Pre-allocate and batch-convert primitives

```rust
// Current approach (slow)
fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
    Ok(v.to_object(self.py))  // GIL + allocation per call
}

// Optimized approach
struct BatchedValues<'py> {
    py: Python<'py>,
    int_cache: Vec<(i64, PyObject)>,  // Cache small integers [-256, 256]
}

impl<'py> BatchedValues<'py> {
    fn get_int(&mut self, v: i64) -> PyObject {
        if v >= -256 && v <= 256 {
            let idx = (v + 256) as usize;
            self.int_cache[idx].1.clone_ref(self.py)
        } else {
            v.to_object(self.py)
        }
    }
}
```

**Implementation Steps**:
1. Implement integer caching for common values [-256, 256]
2. Pre-create singleton Python objects (None, True, False)
3. Use PyList::new_bound with pre-sized capacity
4. Batch string intern for repeated keys

**Expected Gain**: 15-20% for loads

#### 1.2: Reduce Type Dispatch Overhead

**Problem**: Sequential if-else chain for type detection (lines 284-332)

**Solution**: Type-tag based dispatch with branch prediction hints

```rust
#[repr(u8)]
enum PyTypeTag {
    None = 0,
    Bool = 1,
    Int = 2,
    Float = 3,
    String = 4,
    List = 5,
    Dict = 6,
    Tuple = 7,
}

#[inline(always)]
fn get_type_tag(obj: &Bound<'_, PyAny>) -> PyTypeTag {
    // Use PyO3's internal type pointer for O(1) detection
    // More info: https://pyo3.rs/v0.24.0/performance
    if obj.is_none() { return PyTypeTag::None; }

    let type_ptr = obj.get_type().as_type_ptr();
    // Use static type pointers (cached at module init)
    match type_ptr {
        t if t == CACHED_BOOL_TYPE => PyTypeTag::Bool,
        t if t == CACHED_INT_TYPE => PyTypeTag::Int,
        // ... etc
        _ => slow_path_type_detection(obj),
    }
}
```

**Implementation Steps**:
1. Cache Python type pointers at module initialization
2. Replace downcast_exact with pointer comparison
3. Add likely/unlikely hints for common paths
4. Implement jump table dispatch for 8 common types

**Expected Gain**: 10-15% for dumps

#### 1.3: Eliminate Intermediate String Allocations

**Problem**: Error paths use `to_string()` (lines 87, 95, 300)

**Solution**: Use stack-allocated buffers for common cases

```rust
// Instead of to_string(), format directly to pre-allocated buffer
use std::io::Write;

let mut buf = [0u8; 64];
let bytes_written = write!(&mut buf[..], "{}", l_val).unwrap();
let s = std::str::from_utf8(&buf[..bytes_written]).unwrap();
```

**Expected Gain**: 5-8% for integer-heavy workloads

---

### **Phase 2: Memory Allocation Optimization** (Weeks 4-5)
*Target: 15-25% additional speedup*

#### 2.1: Pre-sized Buffer Allocation

**Problem**: serde_json::to_string doesn't pre-size output buffer

**Solution**: Implement size estimation heuristic

```rust
fn estimate_json_size(obj: &Bound<'_, PyAny>) -> usize {
    if let Ok(dict) = obj.downcast_exact::<PyDict>() {
        // Heuristic: avg 20 chars per key-value pair
        dict.len() * 20 + 10
    } else if let Ok(list) = obj.downcast_exact::<PyList>() {
        list.len() * 10 + 10
    } else {
        256  // Default size
    }
}

fn dumps_with_capacity(_py: Python, data: &Bound<'_, PyAny>) -> PyResult<String> {
    let estimated_size = estimate_json_size(data);
    let mut buf = Vec::with_capacity(estimated_size);

    let mut serializer = serde_json::Serializer::new(&mut buf);
    PyAnySerialize { obj: data }.serialize(&mut serializer)
        .map_err(|e| PyValueError::new_err(format!("JSON serialization error: {e}")))?;

    // SAFETY: serde_json guarantees valid UTF-8
    Ok(unsafe { String::from_utf8_unchecked(buf) })
}
```

**Expected Gain**: 10-12% for dumps

#### 2.2: Arena Allocation for Temporary Objects

**Problem**: Vec allocations for dict keys/values (lines 209-210)

**Solution**: Use bumpalo arena allocator

```rust
use bumpalo::Bump;

struct PyObjectVisitor<'py> {
    py: Python<'py>,
    arena: &'py Bump,  // Arena for temporary allocations
}

fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
where
    A: MapAccess<'de>,
{
    // Allocate in arena (no Drop overhead)
    let keys = self.arena.alloc_slice_fill_default(size_hint);
    let values = self.arena.alloc_slice_fill_default(size_hint);

    // ... populate ...

    // Create PyDict directly from slices
    create_dict_from_slices(self.py, keys, values)
}
```

**Expected Gain**: 8-10% for loads with large objects

#### 2.3: Object Pooling for Common Patterns

**Problem**: Repeated allocation/deallocation of Vec<PyObject>

**Solution**: Thread-local object pools

```rust
thread_local! {
    static VEC_POOL: RefCell<Vec<Vec<PyObject>>> = RefCell::new(Vec::new());
}

fn get_pooled_vec() -> Vec<PyObject> {
    VEC_POOL.with(|pool| pool.borrow_mut().pop().unwrap_or_default())
}

fn return_pooled_vec(mut v: Vec<PyObject>) {
    v.clear();
    if v.capacity() <= 1024 {  // Don't pool huge vecs
        VEC_POOL.with(|pool| pool.borrow_mut().push(v));
    }
}
```

**Expected Gain**: 5-8% for loads with many arrays

---

### **Phase 3: Algorithm-Level Optimizations** (Weeks 6-8)
*Target: 20-30% additional speedup*

#### 3.1: Custom JSON Serializer (bypass serde_json)

**Problem**: serde_json is general-purpose, not optimized for Python→JSON

**Solution**: Direct byte buffer writing with inlined formatting

```rust
struct FastJsonWriter {
    buf: Vec<u8>,
}

impl FastJsonWriter {
    #[inline(always)]
    fn write_int(&mut self, v: i64) {
        // Use itoa crate (3-10x faster than std formatting)
        self.buf.extend_from_slice(itoa::Buffer::new().format(v).as_bytes());
    }

    #[inline(always)]
    fn write_string(&mut self, s: &str) {
        self.buf.push(b'"');

        // Fast path: no escapes needed
        if !s.bytes().any(|b| b == b'"' || b == b'\\' || b < 0x20) {
            self.buf.extend_from_slice(s.as_bytes());
        } else {
            // Slow path: escape special characters
            for byte in s.bytes() {
                match byte {
                    b'"' => self.buf.extend_from_slice(b"\\\""),
                    b'\\' => self.buf.extend_from_slice(b"\\\\"),
                    // ... other escapes
                    _ => self.buf.push(byte),
                }
            }
        }

        self.buf.push(b'"');
    }
}
```

**Dependencies**:
- Add to Cargo.toml: `itoa = "1.0"`, `ryu = "1.0"` (fast float formatting)

**Expected Gain**: 25-35% for dumps

#### 3.2: Parallel Serialization for Large Objects

**Problem**: Single-threaded serialization of large arrays/objects

**Solution**: Rayon-based parallel chunking

```rust
use rayon::prelude::*;

fn serialize_large_list(list: &PyList) -> PyResult<String> {
    if list.len() < 10000 {
        return serialize_list_sequential(list);  // Small lists stay sequential
    }

    // Split into chunks
    let chunks: Vec<_> = (0..list.len())
        .collect::<Vec<_>>()
        .chunks(list.len() / rayon::current_num_threads())
        .map(|chunk| {
            chunk.iter().map(|&i| {
                serialize_element(list.get_item(i).unwrap())
            }).collect::<Vec<_>>()
        })
        .collect();

    // Parallel serialize chunks
    let serialized_chunks: Vec<String> = chunks
        .par_iter()
        .map(|chunk| serialize_chunk(chunk))
        .collect();

    // Join with commas
    format!("[{}]", serialized_chunks.join(","))
}
```

**Note**: Only beneficial for very large arrays (>10k elements). Needs careful benchmarking.

**Expected Gain**: 0-40% for large arrays (size-dependent)

#### 3.3: String Interning for Repeated Keys

**Problem**: Dictionary keys are often repeated (e.g., JSON API responses)

**Solution**: Intern common strings to reduce allocation

```rust
use std::collections::HashMap;

struct StringInterner {
    cache: HashMap<u64, PyObject>,  // Hash -> interned Python string
}

impl StringInterner {
    fn intern(&mut self, py: Python, s: &str) -> PyObject {
        let hash = calculate_hash(s);
        self.cache.entry(hash).or_insert_with(|| {
            PyString::new(py, s).to_object(py)
        }).clone_ref(py)
    }
}
```

**Expected Gain**: 10-15% for loads with repetitive keys

---

### **Phase 4: SIMD Fundamentals** (Weeks 9-12)
*Target: 30-50% additional speedup*

#### 4.1: SIMD UTF-8 Validation

**Problem**: Each string validated byte-by-byte

**Solution**: Use simdutf8 crate for parallel validation

```rust
use simdutf8::basic::from_utf8;

// Replace std::str::from_utf8 with SIMD version
fn validate_utf8_fast(bytes: &[u8]) -> Result<&str, Utf8Error> {
    from_utf8(bytes)  // Uses AVX2/SSE4.2 when available
}
```

**Dependency**: Add `simdutf8 = "0.1"` to Cargo.toml

**Expected Gain**: 15-20% for string-heavy workloads

#### 4.2: Vectorized Number Parsing

**Problem**: Integer parsing done digit-by-digit

**Solution**: Parse 8 digits at once using SIMD

```rust
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[inline]
unsafe fn parse_8_digits_simd(ptr: *const u8) -> Option<u32> {
    // Load 8 bytes
    let chars = _mm_loadl_epi64(ptr as *const __m128i);

    // Check all are digits '0'-'9' (0x30-0x39)
    let zeros = _mm_set1_epi8(b'0' as i8);
    let nines = _mm_set1_epi8(b'9' as i8);
    let sub_zeros = _mm_sub_epi8(chars, zeros);
    let valid = _mm_cmplt_epi8(sub_zeros, _mm_sub_epi8(nines, zeros));

    if _mm_movemask_epi8(valid) != 0xFF {
        return None;  // Not all digits
    }

    // Convert ASCII to numeric: '0' -> 0, '1' -> 1, etc.
    let digits = _mm_sub_epi8(chars, zeros);

    // Multiply by positional weights [10^7, 10^6, ..., 10^0]
    let weights = _mm_set_epi8(0, 0, 0, 0, 0, 0, 0, 0,
                                1, 10, 100, 1000, 10000, 100000, 1000000, 10000000);
    let products = _mm_maddubs_epi16(digits, weights);

    // Horizontal sum
    let sum = horizontal_sum(products);
    Some(sum)
}
```

**Expected Gain**: 20-30% for number-heavy JSON

#### 4.3: SIMD Whitespace Skipping

**Problem**: Whitespace skipped character-by-character in JSON

**Solution**: Scan 16/32 bytes at once for non-whitespace

```rust
#[inline]
unsafe fn skip_whitespace_simd(mut ptr: *const u8, end: *const u8) -> *const u8 {
    while ptr.add(16) <= end {
        let chunk = _mm_loadu_si128(ptr as *const __m128i);

        // Create mask for whitespace characters (0x20, 0x09, 0x0A, 0x0D)
        let spaces = _mm_cmpeq_epi8(chunk, _mm_set1_epi8(0x20));
        let tabs = _mm_cmpeq_epi8(chunk, _mm_set1_epi8(0x09));
        let newlines = _mm_cmpeq_epi8(chunk, _mm_set1_epi8(0x0A));
        let returns = _mm_cmpeq_epi8(chunk, _mm_set1_epi8(0x0D));

        let whitespace = _mm_or_si128(
            _mm_or_si128(spaces, tabs),
            _mm_or_si128(newlines, returns)
        );

        let mask = _mm_movemask_epi8(whitespace);
        if mask != 0xFFFF {
            // Found non-whitespace
            return ptr.add(mask.trailing_ones() as usize);
        }

        ptr = ptr.add(16);
    }

    // Handle remaining bytes
    fallback_skip_whitespace(ptr, end)
}
```

**Expected Gain**: 8-12% overall (more for whitespace-heavy JSON)

---

### **Phase 5: Advanced SIMD (simdjson-style)** (Weeks 13-20)
*Target: 40-60% additional speedup (2-3x total from baseline)*

#### 5.1: Two-Stage Parsing Architecture

**Current**: Single-pass recursive descent
**Target**: Stage 1 (structural indexing) + Stage 2 (tree building)

**Stage 1: Structural Character Detection**

```rust
struct StructuralIndex {
    positions: Vec<u32>,     // Byte positions of {, }, [, ], :, ,
    char_types: Vec<u8>,     // Type of each structural char
}

#[target_feature(enable = "avx2")]
unsafe fn find_structural_chars(json: &[u8]) -> StructuralIndex {
    let mut positions = Vec::with_capacity(json.len() / 8);
    let mut char_types = Vec::with_capacity(json.len() / 8);

    let mut i = 0;
    while i + 32 <= json.len() {
        let chunk = _mm256_loadu_si256(json.as_ptr().add(i) as *const __m256i);

        // Classify characters using vpshufb lookup table
        let lookup = _mm256_setr_epi8(
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,       // 0x00-0x09
            CHAR_CLASS_WHITESPACE,              // 0x0A (newline)
            0, 0,
            CHAR_CLASS_WHITESPACE,              // 0x0D (carriage return)
            // ... full 256-entry table
        );

        let classes = _mm256_shuffle_epi8(lookup, chunk);

        // Extract positions of structural characters
        let structural_mask = _mm256_cmpeq_epi8(
            _mm256_and_si256(classes, _mm256_set1_epi8(CHAR_CLASS_STRUCTURAL)),
            _mm256_set1_epi8(CHAR_CLASS_STRUCTURAL)
        );

        let mask = _mm256_movemask_epi8(structural_mask) as u32;

        // Extract bit positions
        let mut remaining = mask;
        while remaining != 0 {
            let bit_pos = remaining.trailing_zeros();
            positions.push((i + bit_pos as usize) as u32);
            char_types.push(json[i + bit_pos as usize]);
            remaining &= remaining - 1;  // Clear lowest set bit
        }

        i += 32;
    }

    StructuralIndex { positions, char_types }
}
```

**Stage 2: Build Parse Tree from Structural Index**

```rust
fn parse_from_structural_index(
    json: &[u8],
    index: &StructuralIndex,
    py: Python
) -> PyResult<PyObject> {
    let mut stack = Vec::with_capacity(64);
    let mut idx = 0;

    while idx < index.positions.len() {
        let pos = index.positions[idx] as usize;
        match index.char_types[idx] {
            b'{' => {
                stack.push(ParseState::Object(PyDict::new(py)));
                idx += 1;
            }
            b'}' => {
                let obj = stack.pop().unwrap();
                if stack.is_empty() {
                    return Ok(obj.to_object(py));
                }
                // Add to parent container
                add_to_parent(&mut stack, obj);
                idx += 1;
            }
            // ... handle other structural chars
            _ => idx += 1,
        }
    }

    Ok(stack.pop().unwrap().to_object(py))
}
```

**Expected Gain**: 30-40% for loads

#### 5.2: Branchless Quote Scanning

**Problem**: Finding end of string requires checking each character for quote/escape

**Solution**: Use bit manipulation to find quotes in parallel

```rust
#[target_feature(enable = "avx2")]
unsafe fn find_string_end(json: &[u8], start: usize) -> usize {
    let mut pos = start;

    while pos + 32 <= json.len() {
        let chunk = _mm256_loadu_si256(json.as_ptr().add(pos) as *const __m256i);

        // Find quotes
        let quotes = _mm256_cmpeq_epi8(chunk, _mm256_set1_epi8(b'"' as i8));
        let quote_mask = _mm256_movemask_epi8(quotes) as u32;

        // Find backslashes
        let backslashes = _mm256_cmpeq_epi8(chunk, _mm256_set1_epi8(b'\\' as i8));
        let backslash_mask = _mm256_movemask_epi8(backslashes) as u32;

        // Compute escaped quotes using carry-less multiplication
        let escaped = compute_escaped_quotes(backslash_mask);

        // Unescaped quotes = all quotes & ~escaped
        let unescaped_quotes = quote_mask & !escaped;

        if unescaped_quotes != 0 {
            return pos + unescaped_quotes.trailing_zeros() as usize;
        }

        pos += 32;
    }

    // Fallback for remaining bytes
    fallback_find_string_end(json, pos)
}

#[inline]
fn compute_escaped_quotes(backslash_mask: u32) -> u32 {
    // Use carry-less multiplication to find odd-length backslash sequences
    // This is the "clever bit manipulation" from simdjson paper
    let mut odd_backslashes = backslash_mask;
    odd_backslashes ^= odd_backslashes << 1;
    odd_backslashes ^= odd_backslashes << 2;
    odd_backslashes ^= odd_backslashes << 4;
    odd_backslashes ^= odd_backslashes << 8;
    odd_backslashes ^= odd_backslashes << 16;

    // Quotes after odd-length backslash sequences are escaped
    (odd_backslashes << 1) & 0xFFFFFFFF
}
```

**Expected Gain**: 15-20% for string-heavy JSON

#### 5.3: Output Buffer Direct Writing (for dumps)

**Problem**: String serialization involves multiple copies

**Solution**: Pre-allocate output buffer, write directly without intermediate allocations

```rust
struct DirectJsonWriter {
    buf: Vec<u8>,
    pos: usize,
}

impl DirectJsonWriter {
    #[inline(always)]
    fn reserve(&mut self, additional: usize) {
        if self.pos + additional > self.buf.len() {
            self.buf.resize(self.pos + additional, 0);
        }
    }

    #[inline(always)]
    fn write_bytes_unchecked(&mut self, bytes: &[u8]) {
        // SAFETY: Caller ensures reserve() was called
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.buf.as_mut_ptr().add(self.pos),
                bytes.len()
            );
        }
        self.pos += bytes.len();
    }

    #[target_feature(enable = "avx2")]
    unsafe fn write_string_vectorized(&mut self, s: &str) {
        let bytes = s.as_bytes();
        self.reserve(bytes.len() + 2 + bytes.len() / 8);  // Overestimate for escapes

        self.write_bytes_unchecked(b"\"");

        // Process 32 bytes at a time
        let mut i = 0;
        while i + 32 <= bytes.len() {
            let chunk = _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i);

            // Check for characters needing escaping
            let needs_escape = detect_escape_chars(chunk);

            if needs_escape == 0 {
                // Fast path: no escaping needed
                self.write_bytes_unchecked(&bytes[i..i+32]);
                i += 32;
            } else {
                // Slow path: handle escapes
                self.write_string_with_escapes(&bytes[i..i+32]);
                i += 32;
            }
        }

        // Handle remaining bytes
        self.write_string_with_escapes(&bytes[i..]);
        self.write_bytes_unchecked(b"\"");
    }
}
```

**Expected Gain**: 20-30% for dumps

---

## Implementation Timeline

### Month 1: Foundation
- **Week 1**: Phase 0 - Measurement infrastructure
- **Week 2-3**: Phase 1 - PyO3 overhead reduction
- **Week 4**: Evaluate and benchmark Phase 1 results

### Month 2: Core Optimizations
- **Week 5-6**: Phase 2 - Memory allocation optimization
- **Week 7-8**: Phase 3 - Algorithm-level optimizations
- **Week 8**: Mid-point evaluation and adjustment

### Month 3: SIMD Introduction
- **Week 9-10**: Phase 4.1-4.2 - Basic SIMD (UTF-8, number parsing)
- **Week 11-12**: Phase 4.3 - SIMD whitespace and benchmarking
- **Week 12**: Phase 4 evaluation

### Month 4-5: Advanced SIMD
- **Week 13-15**: Phase 5.1 - Two-stage parsing
- **Week 16-17**: Phase 5.2 - Branchless quote scanning
- **Week 18-19**: Phase 5.3 - Direct buffer writing
- **Week 20**: Final benchmarking and optimization

---

## Expected Performance Trajectory

| Phase | Cumulative Speedup (dumps) | Cumulative Speedup (loads) | vs orjson |
|-------|---------------------------|---------------------------|-----------|
| Baseline | 1.0x | 1.0x | 0.36x / 0.58x |
| Phase 1 | 1.25x | 1.20x | 0.45x / 0.70x |
| Phase 2 | 1.50x | 1.40x | 0.55x / 0.81x |
| Phase 3 | 2.00x | 1.70x | 0.73x / 0.99x |
| Phase 4 | 2.50x | 2.20x | 0.91x / 1.28x |
| Phase 5 | 3.00x | 2.80x | 1.09x / 1.63x |

**Target**: Match orjson for dumps (1.0x), exceed for loads (1.5x)

---

## Risk Mitigation

### Technical Risks

1. **SIMD Portability**
   - Mitigation: Feature-gated implementations with scalar fallback
   - Use `#[cfg(target_feature)]` for runtime detection

2. **PyO3 API Changes**
   - Mitigation: Pin PyO3 version during development
   - Comprehensive test coverage for PyO3 boundary

3. **Unsafe Code Correctness**
   - Mitigation: Extensive fuzzing with cargo-fuzz
   - MIRI testing for undefined behavior detection
   - Formal verification for critical SIMD kernels (where feasible)

4. **Performance Regression**
   - Mitigation: Automated benchmarking in CI
   - Performance budgets with automatic alerts
   - Mandatory flamegraph review for PRs

### Process Risks

1. **Scope Creep**
   - Mitigation: Strict phase boundaries
   - No Phase N+1 work until Phase N validated

2. **Measurement Bias**
   - Mitigation: Multiple benchmark scenarios
   - Real-world data corpus (github JSON archives)
   - Cross-validation with perf/vtune

---

## Success Metrics

### Performance Targets
- ✅ **Primary**: Achieve ≥ 1.0x orjson speed for dumps by Phase 5
- ✅ **Primary**: Achieve ≥ 1.0x orjson speed for loads by Phase 4
- ✅ **Secondary**: Maintain < 10% variance across different data types

### Code Quality Targets
- ✅ No unsafe code outside of SIMD modules
- ✅ 100% test coverage for optimization paths
- ✅ Zero memory leaks (valgrind clean)
- ✅ Pass MIRI undefined behavior checks

### Maintainability Targets
- ✅ Comprehensive inline documentation for SIMD code
- ✅ Fallback implementations for all SIMD paths
- ✅ Ablation tests (can disable each optimization independently)

---

## References

1. **simdjson Paper**: [Parsing Gigabytes of JSON per Second](https://arxiv.org/html/1902.08318v7)
2. **orjson GitHub**: [Fast, correct Python JSON library](https://github.com/ijl/orjson)
3. **PyO3 Performance Guide**: https://pyo3.rs/v0.24.0/performance
4. **SIMD UTF-8 Validation**: https://github.com/rusticstuff/simdutf8
5. **Fast Integer Parsing**: Lemire, Daniel. "Number Parsing at a Gigabyte per Second"

---

## Appendix A: Profiling Commands

```bash
# CPU profiling
perf record -g -F 99 python benches/python_benchmark.py
perf report --stdio > perf_report.txt

# Flamegraph generation
cargo flamegraph --bench python_benchmark -- --bench

# Cache analysis
perf stat -e cache-misses,cache-references python benches/python_benchmark.py

# Branch prediction analysis
perf stat -e branches,branch-misses python benches/python_benchmark.py

# Memory profiling
heaptrack python benches/python_benchmark.py
heaptrack_gui heaptrack.python.*.gz
```

---

## Appendix B: Benchmark Data Corpus

Create diverse test cases:

```python
# benches/comprehensive_benchmark.py
import json

BENCHMARKS = {
    "integers": list(range(100000)),
    "floats": [i * 0.1 for i in range(100000)],
    "strings_short": ["test"] * 100000,
    "strings_long": ["a" * 100] * 10000,
    "mixed_small": [{"id": i, "name": f"user{i}"} for i in range(10000)],
    "nested_deep": create_nested_dict(depth=20),
    "wide_object": {f"key_{i}": i for i in range(100000)},
    "real_world_api": load_github_api_response(),
}
```

---

**Document Version**: 1.0
**Author**: Performance Optimization Team
**Last Updated**: 2025-11-23
**Status**: Planning Phase
