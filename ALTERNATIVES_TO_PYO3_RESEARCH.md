# Alternatives to PyO3: Research and Proposals

## Executive Summary

**Current situation**: rjson uses PyO3 (Rust ↔ Python bindings) which adds ~5-15% overhead compared to pure C extensions like orjson.

**Research goal**: Identify alternatives to eliminate PyO3 overhead while maintaining or improving on our current advantages.

**Key findings**:
1. ✅ **Hybrid Approach** (PyO3 + Raw FFI): Best balance of safety and performance
2. ⚠️ **Pure C Rewrite**: Maximum performance, loses Rust benefits
3. ❌ **Alternative Rust bindings**: No significant advantage over PyO3
4. ✅ **Strategic unsafe blocks**: Targeted overhead elimination

---

## Option 1: Hybrid Approach (PyO3 + Direct CPython FFI)

### Concept

Keep PyO3 for high-level API and safety, but use direct CPython C API for hot paths (bulk array processing).

### Architecture

```rust
// High-level API: Use PyO3 (safe, maintainable)
#[pyfunction]
fn dumps(py: Python, data: &Bound<'_, PyAny>) -> PyResult<String> {
    // PyO3 for type checking, error handling
    let obj_type = detect_type(data);

    match obj_type {
        FastType::List => {
            // HOT PATH: Direct FFI (bypass PyO3)
            unsafe { serialize_list_direct_ffi(data.as_ptr(), &mut buffer) }
        }
        _ => {
            // Use PyO3 for other types
            serialize_with_pyo3(data)
        }
    }
}

// Hot path: Direct CPython C API (no PyO3 overhead)
unsafe fn serialize_list_direct_ffi(list_ptr: *mut ffi::PyObject, buf: &mut Vec<u8>) {
    let size = ffi::PyList_GET_SIZE(list_ptr);

    // Direct pointer arithmetic, no PyO3 abstractions
    for i in 0..size {
        let item = ffi::PyList_GET_ITEM(list_ptr, i);  // Borrowed ref, no refcount

        // Direct type pointer comparison (no PyO3 downcast)
        let obj_type = (*item).ob_type;

        if obj_type == INT_TYPE_PTR {
            // Direct integer extraction (no PyO3 wrapper)
            let value = ffi::PyLong_AsLongLong(item);
            write_int_fast(buf, value);
        }
    }
}
```

### Implementation Plan

**Phase 1: Profiling** (1-2 days)
1. Identify exact overhead sources in current PyO3 code
2. Profile hot paths with perf/flamegraph
3. Quantify PyO3 overhead per operation

**Phase 2: Direct FFI Hot Paths** (1 week)
1. Rewrite `serialize_int_array_bulk` with direct FFI
2. Rewrite `serialize_float_array_bulk` with direct FFI
3. Rewrite `serialize_bool_array_bulk` with direct FFI
4. Keep string array with PyO3 (complexity not worth it)

**Phase 3: Benchmark & Validate** (2-3 days)
1. Comprehensive benchmarks
2. Memory safety testing (valgrind, ASAN)
3. Cross-platform testing

### Pros ✅

1. **Best of both worlds**: Safety where it matters, speed in hot paths
2. **Incremental migration**: Can convert one function at a time
3. **Maintainability**: Keep PyO3 for complex logic, FFI only for simple loops
4. **Expected gains**: 10-20% improvement on bulk operations
5. **No major rewrite**: Existing architecture stays mostly intact

### Cons ⚠️

1. **Increased complexity**: Two APIs to maintain
2. **More unsafe code**: Requires careful review
3. **Platform-specific**: Need to test on Windows, macOS, Linux
4. **CPython version compatibility**: Direct FFI may break with CPython updates

### Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Memory safety bugs | Medium | High | Extensive testing, ASAN, valgrind |
| Platform incompatibility | Low | Medium | CI testing on all platforms |
| CPython API changes | Low | Medium | Pin to stable ABI subset |
| Maintenance burden | Medium | Medium | Clear documentation, limit unsafe scope |

### Expected Performance Improvement

| Workload | Current Gap | After Hybrid | Improvement |
|----------|-------------|--------------|-------------|
| Integers | 2.1x slower | **1.6-1.8x slower** | +25-30% |
| Floats | 1.05x slower | **0.95-1.0x** | +5-10% |
| Booleans | **32% faster** | **40-50% faster** | +10-15% |
| Strings | 4.1x slower | **3.5-3.8x slower** | +10-15% |

---

## Option 2: Pure C Extension Rewrite

### Concept

Rewrite the entire library in C, matching orjson's architecture.

### Implementation

**Structure**:
```
rjson/
├── rjson.c           # Main implementation (pure C)
├── buffer.c          # Custom buffer management
├── encoder.c         # JSON encoding
├── decoder.c         # JSON decoding
├── module.c          # Python module definition
└── rjson.h           # Header
```

**Key techniques from orjson**:
1. Custom buffer management (no malloc overhead)
2. AVX2/SIMD for string operations
3. Hand-optimized assembly for critical paths
4. Zero-copy where possible

### Implementation Timeline

**Phase 1: C Module Scaffold** (1 week)
- Set up C extension build system
- Basic module initialization
- Simple dumps/loads functions

**Phase 2: Core Serialization** (2-3 weeks)
- Integer/float/bool serialization
- String serialization with escaping
- Dict/list traversal

**Phase 3: Optimization** (2-3 weeks)
- Custom buffer management
- SIMD string operations
- Profiling and tuning

**Phase 4: Testing & Validation** (1-2 weeks)
- Comprehensive test suite
- Memory safety testing
- Performance validation

**Total**: 6-9 weeks (1.5-2 months)

### Pros ✅

1. **Maximum performance**: Match or exceed orjson
2. **Proven approach**: orjson shows it works
3. **No abstraction overhead**: Direct CPython C API
4. **Full control**: Custom allocators, SIMD, etc.

### Cons ❌

1. **Complete rewrite**: Throw away all existing Rust code
2. **Memory safety**: Manual memory management, buffer overflows possible
3. **Maintenance burden**: C is harder to maintain than Rust
4. **Security concerns**: More vulnerable to CVEs
5. **Development time**: 2+ months of work
6. **Loss of Rust benefits**: No borrow checker, no type safety
7. **Debugging difficulty**: C debugging is harder than Rust

### Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Memory safety bugs | **High** | **Critical** | Extensive testing, fuzzing, ASAN |
| Buffer overflows | Medium | **Critical** | Careful bounds checking |
| Use-after-free | Medium | **Critical** | Reference counting discipline |
| Integer overflow | Low | High | Bounds checking |
| Maintenance burden | **High** | High | Comprehensive documentation |
| Developer onboarding | High | Medium | Detailed code comments |

### Expected Performance

**Best case**: Match orjson (eliminate 2-4x gap)

**Realistic case**: 10-20% slower than orjson due to:
- Lack of years of optimization
- Missing SIMD expertise
- Not full-time development

### Recommendation: ❌ **NOT RECOMMENDED**

**Why**:
1. **High risk, moderate reward**: 2+ months work to maybe close gap
2. **Loses Rust advantages**: Memory safety, type safety, maintainability
3. **Not aligned with project goals**: Project uses Rust for safety
4. **Diminishing returns**: Already 9x faster than stdlib json
5. **Security concerns**: C introduces vulnerability surface

---

## Option 3: Alternative Rust ↔ Python Bindings

### Investigated Alternatives

#### 3.1 rust-cpython (Deprecated)

**Status**: Archived, superseded by PyO3

**Pros**: None (deprecated)

**Cons**: No longer maintained, PyO3 is the successor

**Verdict**: ❌ Not viable

---

#### 3.2 Direct FFI (no bindings library)

**Concept**: Use only `pyo3-ffi` (raw CPython C API) without PyO3 abstractions.

**Example**:
```rust
// No PyO3, just raw FFI
use pyo3_ffi as ffi;

#[no_mangle]
pub unsafe extern "C" fn PyInit_rjson() -> *mut ffi::PyObject {
    // Manual module initialization
    let module_def = ffi::PyModuleDef {
        m_base: ffi::PyModuleDef_HEAD_INIT,
        m_name: c"rjson".as_ptr(),
        // ...
    };

    ffi::PyModule_Create(&module_def)
}

// Manual function wrapping
unsafe extern "C" fn dumps_impl(
    _self: *mut ffi::PyObject,
    args: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    // Manual argument parsing
    let mut obj: *mut ffi::PyObject = std::ptr::null_mut();

    if ffi::PyArg_ParseTuple(args, c"O".as_ptr(), &mut obj) == 0 {
        return std::ptr::null_mut();
    }

    // Manual serialization...
}
```

**Pros**:
- No PyO3 overhead (5-10% gain potential)
- Full control over memory management
- Direct C API access

**Cons**:
- **Massive complexity**: Manual reference counting, error handling
- **Memory safety**: Easy to introduce bugs
- **Maintenance nightmare**: Verbose, error-prone code
- **Type safety**: Lose Rust's type system benefits
- **Development time**: 3-4x longer than PyO3

**Expected gain**: 5-15% performance improvement

**Cost**: 5-10x increase in code complexity

**Verdict**: ❌ **NOT WORTH IT**
- Overhead saved < complexity added
- Would make codebase unmaintainable
- Similar to writing C with Rust syntax

---

#### 3.3 Cython

**Concept**: Rewrite in Cython (Python → C compiler)

**Example**:
```cython
# rjson.pyx
cdef extern from "Python.h":
    ctypedef struct PyObject
    PyObject* PyList_GET_ITEM(PyObject* list, Py_ssize_t i)

def dumps(obj):
    cdef PyObject* obj_ptr = <PyObject*>obj
    # C-level operations...
```

**Pros**:
- Pythonic syntax
- Automatic C generation
- Good performance (close to C)

**Cons**:
- Not Rust (project goal is Rust)
- Debugging is harder than Rust
- No memory safety guarantees
- Still manual reference counting

**Verdict**: ❌ **OUT OF SCOPE**
- Project is specifically Rust-based
- Doesn't solve the problem (still manual memory management)

---

## Option 4: Strategic Unsafe Optimization (Surgical Approach)

### Concept

Keep PyO3 architecture, but use targeted `unsafe` blocks to eliminate overhead in specific hot paths.

### Current Code Analysis

**Where PyO3 adds overhead**:

1. **Type downcasting** (~5% overhead per check)
```rust
// Current (safe)
if let Ok(int_val) = obj.downcast::<PyInt>() {
    // Process int
}

// Optimized (unsafe)
if obj.is_instance_of::<PyInt>() {
    let int_val = unsafe { obj.downcast_exact::<PyInt>().unwrap_unchecked() };
    // Process int - no overhead!
}
```

2. **List iteration** (~10% overhead)
```rust
// Current (safe)
for item in list.iter() {  // PyO3 iterator overhead
    serialize(item);
}

// Optimized (unsafe)
let size = list.len();
for i in 0..size {
    let item = unsafe { list.get_item_unchecked(i) };  // No bounds check!
    serialize(item);
}
```

3. **Reference counting** (~5% overhead)
```rust
// Current (safe)
let item = list.get_item(i)?;  // Increments refcount

// Optimized (unsafe)
let item = unsafe { list.get_item_borrowed(i) };  // Borrowed, no refcount!
```

### Implementation Strategy

**Target: Hot paths identified by profiling**

1. **Already done**: Bulk array processing uses unsafe
2. **Next targets**:
   - Dict iteration (remove iterator overhead)
   - Type checking (use cached type pointers)
   - String extraction (zero-copy, no validation)

### Example: Optimized Dict Serialization

```rust
// Current (safe but slower)
pub fn serialize_dict_safe(dict: &Bound<'_, PyDict>, buf: &mut Vec<u8>) -> PyResult<()> {
    buf.push(b'{');

    let mut first = true;
    for (key, value) in dict.iter() {  // PyO3 iterator
        if !first {
            buf.push(b',');
        }
        first = false;

        serialize(key, buf)?;
        buf.push(b':');
        serialize(value, buf)?;
    }

    buf.push(b'}');
    Ok(())
}

// Optimized (unsafe, faster)
pub unsafe fn serialize_dict_fast(dict: &Bound<'_, PyDict>, buf: &mut Vec<u8>) -> PyResult<()> {
    buf.push(b'{');

    let dict_ptr = dict.as_ptr();
    let mut pos: ffi::Py_ssize_t = 0;
    let mut key: *mut ffi::PyObject = std::ptr::null_mut();
    let mut value: *mut ffi::PyObject = std::ptr::null_mut();

    let mut first = true;

    // Direct PyDict_Next (no PyO3 overhead!)
    while ffi::PyDict_Next(dict_ptr, &mut pos, &mut key, &mut value) != 0 {
        if !first {
            buf.push(b',');
        }
        first = false;

        // key and value are borrowed references (no refcount overhead!)
        serialize_direct(key, buf)?;
        buf.push(b':');
        serialize_direct(value, buf)?;
    }

    buf.push(b'}');
    Ok(())
}
```

**Expected improvement**: 15-25% on dict-heavy workloads

### Pros ✅

1. **Surgical precision**: Only optimize what matters
2. **Keep PyO3 benefits**: Safety guarantees for most code
3. **Incremental**: Can add unsafe blocks one at a time
4. **Maintainable**: Unsafe blocks are small and well-documented
5. **Low risk**: Can test each optimization independently

### Cons ⚠️

1. **Some safety loss**: Unsafe blocks can have bugs
2. **Complexity**: Need to understand both PyO3 and CPython C API
3. **Testing burden**: More extensive testing needed

### Implementation Plan

**Phase 1: Profile and Identify** (1-2 days)
1. Profile current code with perf/flamegraph
2. Identify top 5 overhead sources
3. Estimate potential gains

**Phase 2: Optimize Hot Paths** (1 week)
1. Dict iteration: Use PyDict_Next directly
2. List bounds checking: Use get_item_unchecked
3. Type checking: Use cached type pointers (already done!)
4. String extraction: Zero-copy with borrowed refs

**Phase 3: Validate** (2-3 days)
1. Comprehensive testing
2. Memory safety checks (ASAN, valgrind)
3. Benchmark improvements

### Expected Performance

| Workload | Current Gap | After Optimization | Improvement |
|----------|-------------|-------------------|-------------|
| Integers | 2.1x slower | **1.7-1.9x slower** | +15-20% |
| Floats | 1.05x slower | **0.95-1.0x** | +5-10% |
| Dicts | 3.8x slower | **2.8-3.2x slower** | +20-25% |
| Strings | 4.1x slower | **3.5-3.7x slower** | +10-15% |

---

## Option 5: Custom Buffer Management

### Concept

Replace `Vec<u8>` with custom buffer that eliminates capacity checking overhead.

### Implementation

```rust
/// Custom buffer with no bounds checking (unsafe, fast)
pub struct FastBuffer {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
}

impl FastBuffer {
    /// Pre-allocate exact capacity (no reallocation)
    pub fn with_capacity(cap: usize) -> Self {
        let layout = Layout::array::<u8>(cap).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };

        Self {
            ptr,
            len: 0,
            capacity: cap,
        }
    }

    /// Write byte without bounds check (UNSAFE!)
    #[inline(always)]
    pub unsafe fn push_unchecked(&mut self, byte: u8) {
        *self.ptr.add(self.len) = byte;  // No capacity check!
        self.len += 1;
    }

    /// Write slice without bounds check (UNSAFE!)
    #[inline(always)]
    pub unsafe fn extend_unchecked(&mut self, bytes: &[u8]) {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            self.ptr.add(self.len),
            bytes.len(),
        );
        self.len += bytes.len();
    }
}
```

### Usage in Serialization

```rust
pub fn serialize_with_fast_buffer(obj: &PyAny) -> String {
    // Pre-calculate size (one extra pass, but eliminates all checks)
    let estimated_size = estimate_size(obj);

    let mut buf = FastBuffer::with_capacity(estimated_size);

    unsafe {
        // All writes are unchecked (fast!)
        serialize_unchecked(obj, &mut buf);
    }

    buf.into_string()
}
```

### Pros ✅

1. **Eliminate Vec overhead**: No capacity checking (5-10% gain)
2. **Predictable performance**: No reallocation surprises
3. **Simple concept**: Just a faster Vec

### Cons ❌

1. **Memory safety**: Buffer overruns if size estimation wrong
2. **Pre-calculation required**: Need accurate size estimation
3. **Complexity**: Custom allocator/deallocator
4. **Limited gain**: Vec is already well-optimized

### Expected Gain: 5-10%

**Verdict**: ⚠️ **HIGH RISK, LOW REWARD**
- Introduces memory safety issues for marginal gain
- Vec capacity checks are already fast (nanoseconds)
- Not worth the complexity

---

## Recommendations

### 🏆 Recommended: **Option 4 (Strategic Unsafe) + Option 1 (Hybrid for bulk paths)**

**Rationale**:
1. **Best risk/reward ratio**: 15-25% improvement for moderate complexity
2. **Incremental**: Can implement piece by piece
3. **Maintainable**: Keep PyO3 for most code, unsafe only for hot paths
4. **Safe enough**: Small, well-tested unsafe blocks
5. **Realistic timeline**: 1-2 weeks of work

**Implementation priority**:
1. ✅ **Already done**: Bulk array processing (Phase 6A)
2. 🔄 **Next**: Dict serialization with PyDict_Next
3. 🔄 **Then**: Remove list bounds checking in hot paths
4. 🔄 **Finally**: Zero-copy string extraction

**Expected outcome**:
- Integers: 2.1x → **1.7x slower** (+25%)
- Floats: 1.05x → **0.95x** (match/beat orjson!)
- Dicts: 3.8x → **3.0x slower** (+25%)
- Strings: 4.1x → **3.5x slower** (+15%)

---

### ❌ Not Recommended

**Option 2 (Pure C Rewrite)**:
- **Why not**: High risk, 2+ months work, loses Rust benefits
- **Verdict**: Only consider if matching orjson is critical business requirement

**Option 3 (Alternative bindings)**:
- **Why not**: No significant advantage over PyO3
- **Verdict**: PyO3 is the best Rust ↔ Python binding available

**Option 5 (Custom buffers)**:
- **Why not**: High risk, low reward (5-10% for memory safety loss)
- **Verdict**: Vec is already well-optimized

---

## Implementation Roadmap

### Phase 1: Profiling (3-4 days)

**Goals**:
1. Identify exact PyO3 overhead sources
2. Quantify potential gains per optimization
3. Prioritize optimizations by impact/effort

**Tasks**:
```bash
# Profile with perf
perf record -g python benches/bulk_benchmark.py
perf report

# Flamegraph
cargo install flamegraph
flamegraph python benches/bulk_benchmark.py

# Identify hot functions
# Look for: downcast, iter, bounds checking
```

**Deliverable**: Profiling report with top 10 overhead sources

---

### Phase 2: Dict Optimization (1 week)

**Goals**: 15-25% improvement on dict-heavy workloads

**Implementation**:
1. Replace PyO3 dict.iter() with PyDict_Next
2. Use borrowed references (no refcount overhead)
3. Direct type pointer comparison

**Code**:
```rust
// New function in lib.rs
unsafe fn serialize_dict_direct(
    dict_ptr: *mut ffi::PyObject,
    buf: &mut Vec<u8>
) -> PyResult<()> {
    buf.push(b'{');

    let mut pos: ffi::Py_ssize_t = 0;
    let mut key: *mut ffi::PyObject = std::ptr::null_mut();
    let mut value: *mut ffi::PyObject = std::ptr::null_mut();
    let mut first = true;

    while ffi::PyDict_Next(dict_ptr, &mut pos, &mut key, &mut value) != 0 {
        if !first { buf.push(b','); }
        first = false;

        // Serialize key (must be string)
        let key_str = ffi::PyUnicode_AsUTF8AndSize(key, &mut size);
        write_json_string_fast(buf, key_str, size);

        buf.push(b':');

        // Serialize value
        serialize_direct(value, buf)?;
    }

    buf.push(b'}');
    Ok(())
}
```

**Testing**:
```python
# Test dict serialization
data = {f"key_{i}": i for i in range(10000)}
assert rjson.dumps(data) == json.dumps(data)

# Benchmark
timeit.timeit(lambda: rjson.dumps(data), number=100)
```

**Expected**: 20-25% improvement on dict workloads

---

### Phase 3: List Bounds Checking (3-4 days)

**Goals**: 10-15% improvement on list workloads

**Implementation**:
```rust
// Remove bounds checking in hot paths
unsafe fn serialize_list_unchecked(
    list: &Bound<'_, PyList>,
    buf: &mut Vec<u8>
) -> PyResult<()> {
    let size = list.len();

    buf.push(b'[');

    for i in 0..size {
        if i > 0 { buf.push(b','); }

        // No bounds check! (we know i < size)
        let item = list.get_item_unchecked(i);
        serialize(item, buf)?;
    }

    buf.push(b']');
    Ok(())
}
```

**Expected**: 10-15% improvement

---

### Phase 4: String Zero-Copy (3-4 days)

**Goals**: 5-10% improvement on string-heavy workloads

**Implementation**:
```rust
// Use borrowed string references (no refcount)
unsafe fn serialize_string_borrowed(
    str_ptr: *mut ffi::PyObject,
    buf: &mut Vec<u8>
) -> PyResult<()> {
    let mut size: ffi::Py_ssize_t = 0;
    let data = ffi::PyUnicode_AsUTF8AndSize(str_ptr, &mut size);

    if data.is_null() {
        return Err(PyValueError::new_err("Invalid string"));
    }

    // Borrowed reference - no refcount overhead!
    let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
    write_json_string_fast(buf, bytes);

    Ok(())
}
```

**Expected**: 5-10% improvement

---

### Total Timeline: 2-3 weeks

**Week 1**: Profiling + Dict optimization
**Week 2**: List bounds checking + String zero-copy
**Week 3**: Testing, validation, benchmarking

**Expected cumulative improvement**: 20-30% overall

---

## Conclusion

**Best path forward**: **Hybrid PyO3 + Strategic Unsafe**

**Why**:
1. ✅ **Realistic gains**: 20-30% improvement achievable
2. ✅ **Maintainable**: Keep PyO3 for complex logic
3. ✅ **Safe enough**: Small, tested unsafe blocks
4. ✅ **Incremental**: Can ship improvements piece by piece
5. ✅ **Low risk**: No complete rewrite needed

**What to avoid**:
1. ❌ **Pure C rewrite**: Too risky, loses Rust benefits
2. ❌ **Custom buffers**: High risk, low reward
3. ❌ **Alternative bindings**: No better options exist

**Next steps**:
1. **Profile** current code to quantify overhead
2. **Implement** dict optimization (biggest impact)
3. **Benchmark** and validate
4. **Iterate** on other hot paths

**Final state prediction**:
- Overall: **6-8x faster than json** (vs current 9x, but closer to orjson)
- Integers: **1.7x slower than orjson** (vs 2.1x)
- Floats: **Match or beat orjson** (vs 1.05x slower)
- Strings: **3.5x slower than orjson** (vs 4.1x)

**The tradeoff remains acceptable**:
- Still much faster than stdlib json
- Keep Rust memory safety for most code
- Manageable complexity increase
- Clear path to further improvements

---

**Date**: 2025-11-25
**Status**: Research complete, recommendations ready
**Decision**: Awaiting user approval to proceed with Hybrid approach
