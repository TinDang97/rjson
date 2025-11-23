# Performance Optimization Quick Start Guide

## Overview

This guide provides a quick-start path to implementing the performance optimizations outlined in `PERFORMANCE_OPTIMIZATION_ROADMAP.md`. The goal is to close the performance gap with orjson (currently 2.75x faster for dumps, 1.71x for loads).

## Current Performance Analysis

### Root Causes of Performance Gap

```
Performance Breakdown (Estimated Impact):
┌─────────────────────────────────────────────┐
│ PyO3 GIL & Type Overhead:        40-50%     │
│ ├─ GIL acquire/release cycles               │
│ ├─ Type downcasting chains                  │
│ └─ Python object creation overhead          │
├─────────────────────────────────────────────┤
│ Memory Allocation Patterns:      20-30%     │
│ ├─ Unbounded Vec growth                     │
│ ├─ Repeated small allocations               │
│ └─ String allocations in hot paths          │
├─────────────────────────────────────────────┤
│ Missing SIMD Optimizations:      30-40%     │
│ ├─ Byte-by-byte parsing                     │
│ ├─ Sequential UTF-8 validation              │
│ └─ Character-by-character string processing │
└─────────────────────────────────────────────┘
```

### Key Hotspots (from static analysis)

**Deserialization (loads)**:
- `src/lib.rs:159-174` - Excessive `to_object()` calls (GIL overhead)
- `src/lib.rs:208-221` - Unbounded Vec allocation for dict keys/values
- `serde_json` parser - No SIMD structural character detection

**Serialization (dumps)**:
- `src/lib.rs:284-332` - Repeated type downcasting (7-way if-else chain)
- `src/lib.rs:87,95,300` - Unnecessary `to_string()` allocations
- `serde_json::to_string` - No pre-sizing, sequential writing

## Phase-by-Phase Implementation Priority

### 🚀 Phase 1: PyO3 Overhead Reduction (HIGHEST ROI)
**Estimated Effort**: 2-3 weeks
**Expected Gain**: 20-30% speedup
**Difficulty**: Medium

#### Quick Wins
1. **Integer Caching** (2-3 days)
   - Cache integers [-256, 256] as Python objects
   - Reuse singleton `None`, `True`, `False`
   - File: `src/lib.rs:147-224`

2. **Type Pointer Caching** (3-4 days)
   - Cache Python type pointers at module init
   - Replace `downcast_exact` with pointer comparison
   - File: `src/lib.rs:274-334`

3. **Pre-allocate Dict Vecs** (1-2 days)
   - Use `size_hint()` for dict keys/values Vecs
   - File: `src/lib.rs:208-221`

#### Implementation Template

```rust
// src/optimizations/object_cache.rs
use pyo3::prelude::*;
use std::sync::OnceLock;

static INT_CACHE: OnceLock<Vec<PyObject>> = OnceLock::new();

pub fn init_cache(py: Python) {
    let mut cache = Vec::with_capacity(513);
    for i in -256..=256 {
        cache.push(i.to_object(py));
    }
    INT_CACHE.set(cache).unwrap();
}

#[inline(always)]
pub fn get_cached_int(py: Python, v: i64) -> Option<PyObject> {
    if v >= -256 && v <= 256 {
        let cache = INT_CACHE.get()?;
        Some(cache[(v + 256) as usize].clone_ref(py))
    } else {
        None
    }
}
```

### ⚡ Phase 2: Memory Allocation Optimization
**Estimated Effort**: 2-3 weeks
**Expected Gain**: 15-25% additional speedup
**Difficulty**: Medium

#### Quick Wins
1. **Pre-sized Output Buffer** (2-3 days)
   - Estimate JSON size before serialization
   - Allocate buffer once instead of growing
   - File: `src/lib.rs:347-351`

2. **Arena Allocator for Temp Objects** (4-5 days)
   - Use `bumpalo` for temporary allocations
   - Eliminate Drop overhead for intermediate Vecs
   - New file: `src/optimizations/arena.rs`

#### Implementation Template

```rust
// src/optimizations/sized_buffer.rs
fn estimate_json_size(obj: &Bound<'_, PyAny>) -> usize {
    if obj.is_none() { return 4; }  // "null"

    if let Ok(dict) = obj.downcast_exact::<PyDict>() {
        // Heuristic: avg 20 bytes per kv pair + overhead
        return dict.len() * 20 + 10;
    }

    if let Ok(list) = obj.downcast_exact::<PyList>() {
        return list.len() * 10 + 10;
    }

    256  // Default size for unknown types
}

pub fn dumps_with_capacity(py: Python, data: &Bound<'_, PyAny>) -> PyResult<String> {
    let est_size = estimate_json_size(data);
    let mut buf = Vec::with_capacity(est_size);

    // ... serialize directly to buf
}
```

### 🔥 Phase 3: Algorithm-Level Optimizations
**Estimated Effort**: 3-4 weeks
**Expected Gain**: 20-30% additional speedup
**Difficulty**: High

#### Key Tasks
1. **Custom JSON Serializer** (7-10 days)
   - Bypass `serde_json::to_string`
   - Direct byte buffer writing
   - Use `itoa`/`ryu` crates for number formatting

2. **String Interning** (3-4 days)
   - Cache repeated dictionary keys
   - Reduces allocation for API response patterns

#### Dependencies to Add

```toml
# Cargo.toml
[dependencies]
itoa = "1.0"        # Fast integer to string (3-10x faster)
ryu = "1.0"         # Fast float to string
bumpalo = "3.14"    # Arena allocator
```

### 🚄 Phase 4: Basic SIMD
**Estimated Effort**: 4-5 weeks
**Expected Gain**: 30-50% additional speedup
**Difficulty**: High

#### Prerequisites
- Proficiency with x86 intrinsics
- Understanding of SIMD fundamentals
- Familiarity with `std::arch` module

#### Quick Wins
1. **SIMD UTF-8 Validation** (2-3 days)
   - Replace `std::str::from_utf8` with `simdutf8`
   - Instant 2-3x speedup for validation

```toml
# Cargo.toml
[dependencies]
simdutf8 = "0.1"
```

```rust
// src/optimizations/fast_utf8.rs
use simdutf8::basic::from_utf8;

pub fn validate_utf8(bytes: &[u8]) -> Result<&str, Utf8Error> {
    from_utf8(bytes)  // Uses AVX2 when available
}
```

2. **Vectorized Number Parsing** (5-7 days)
   - Parse 8 digits at once using SIMD
   - Significant speedup for integer-heavy JSON

### 🎯 Phase 5: Advanced SIMD (simdjson-style)
**Estimated Effort**: 6-8 weeks
**Expected Gain**: 40-60% additional speedup
**Difficulty**: Expert

#### This is PhD-level work
- Two-stage parsing architecture
- Branchless quote scanning using bit manipulation
- Requires deep understanding of:
  - SIMD instruction sets (AVX2/AVX-512)
  - Bit manipulation algorithms
  - Compiler optimization behaviors

**Recommendation**: Only attempt after Phases 1-4 complete and validated

---

## Getting Started TODAY

### Step 1: Set Up Profiling (1 hour)

```bash
# Install profiling tools
cargo install flamegraph
cargo install cargo-benchcmp

# Run baseline benchmark
python benches/python_benchmark.py > baseline_results.txt

# Generate flamegraph
cargo build --release
sudo flamegraph -- python -c "import rjson; rjson.loads('[1,2,3]')"
```

### Step 2: Implement Phase 1.1 (Day 1-2)

Create integer cache:

```bash
# Create new module
mkdir -p src/optimizations
touch src/optimizations/mod.rs
touch src/optimizations/object_cache.rs
```

Edit `src/lib.rs`:
```rust
mod optimizations;
use optimizations::object_cache;

#[pymodule]
fn rjson(py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Initialize caches at module load
    object_cache::init_cache(py);

    m.add_function(wrap_pyfunction!(loads, m)?)?;
    m.add_function(wrap_pyfunction!(dumps, m)?)?;
    Ok(())
}
```

### Step 3: Measure and Validate (Day 3)

```bash
# Rebuild
maturin develop --release

# Re-benchmark
python benches/python_benchmark.py > phase1_1_results.txt

# Compare
diff baseline_results.txt phase1_1_results.txt
```

**Expected Result**: 3-5% speedup from integer caching alone

### Step 4: Continue Phase 1.2-1.3 (Week 2)

Follow roadmap for remaining Phase 1 tasks.

---

## Validation Checklist

After each phase:

- [ ] Run full benchmark suite
- [ ] Generate flamegraph and compare
- [ ] Check memory usage (heaptrack)
- [ ] Run `cargo test` - all tests pass
- [ ] Run `cargo clippy` - no new warnings
- [ ] Update CLAUDE.md with new benchmarks
- [ ] Document changes in commit message

---

## Common Pitfalls

### ❌ Don't Do This

```rust
// WRONG: Allocating in hot path
for item in list.iter() {
    let s = format!("value_{}", item);  // Allocates every iteration!
    process(s);
}

// WRONG: Not pre-sizing
let mut vec = Vec::new();  // Will reallocate multiple times
for i in 0..1000 {
    vec.push(i);
}
```

### ✅ Do This

```rust
// RIGHT: Reuse buffer
let mut buf = String::with_capacity(64);
for item in list.iter() {
    buf.clear();
    use std::fmt::Write;
    write!(&mut buf, "value_{}", item).unwrap();
    process(&buf);
}

// RIGHT: Pre-size with capacity
let mut vec = Vec::with_capacity(1000);
for i in 0..1000 {
    vec.push(i);
}
```

---

## Performance Testing Best Practices

### 1. Always Use Release Builds

```bash
# YES
maturin develop --release

# NO (100x slower)
maturin develop
```

### 2. Run Benchmarks Multiple Times

```bash
# Run 10 times, take median
for i in {1..10}; do
    python benches/python_benchmark.py
done | sort -n | sed -n '5p'
```

### 3. Control for System Noise

```bash
# Disable CPU frequency scaling
sudo cpupower frequency-set --governor performance

# Pin to specific cores
taskset -c 0 python benches/python_benchmark.py
```

### 4. Use Realistic Data

Don't just benchmark `[1,2,3]`. Use real-world JSON:

```python
# Download GitHub API responses
import requests
api_data = requests.get('https://api.github.com/repos/rust-lang/rust').json()
json_str = json.dumps(api_data)

# Benchmark
rjson.loads(json_str)
```

---

## Resources

### Learning SIMD
- [Rust SIMD Guide](https://rust-lang.github.io/packed_simd/)
- [Intel Intrinsics Guide](https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html)
- [Lemire's Blog](https://lemire.me/blog/) - Excellent SIMD articles

### Performance Analysis
- [Rust Performance Book](https://nnethercote.github.io/perf-book/)
- [flamegraph.rs](https://github.com/flamegraph-rs/flamegraph)
- [cargo-benchcmp](https://github.com/BurntSushi/cargo-benchcmp)

### Relevant Papers
- [simdjson Paper (2019)](https://arxiv.org/abs/1902.08318)
- [Fast Number Parsing (Lemire)](https://arxiv.org/abs/2101.11408)

---

## Next Steps

1. **Read** `PERFORMANCE_OPTIMIZATION_ROADMAP.md` for detailed technical design
2. **Set up** profiling infrastructure (Phase 0)
3. **Implement** Phase 1.1 (integer caching)
4. **Measure** results and validate improvement
5. **Continue** with Phase 1.2-1.3

**Questions?** Review flamegraph output to identify next highest-impact optimization.

---

**Last Updated**: 2025-11-23
**Status**: Ready to implement
