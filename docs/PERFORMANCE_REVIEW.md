# rjson performance review: closing the gap to orjson

Four parallel reviews ran against the pre-review code (commit `c12dbe3`):

- deserialization (`loads`)
- serialization (`dumps`)
- correctness and safety
- build and architecture

The loads, dumps and build reviews each prototyped and measured their changes, and a second round (loads, dumps, infra) worked through the resulting roadmap. This branch merges both rounds. This document records what we found, what landed, how it was measured, and what is still left to beat orjson everywhere.

## 1. Results

Numbers are rjson time divided by orjson time on the same run, so **below 1.00 means rjson is faster**. They come from `benches/corpus_benchmark.py --repeat 11` on an x86_64 Xeon (4 cores, otherwise idle) against orjson 3.12.0, with **plain release builds (no PGO)** of the current branch. Treat single cells as ±5–10% and trust the geomeans. `dumps` returns `str`; `dumps_bytes` returns `bytes`, the same type `orjson.dumps` returns.

| case | loads before | loads 3.11 | loads 3.13 | dumps before | dumps 3.11 | dumps 3.13 | dumps_bytes 3.11 | dumps_bytes 3.13 |
|---|---|---|---|---|---|---|---|---|
| twitter | 1.91 | **0.66** | **0.68** | 3.05 | 1.04 | 1.09 | **0.70** | **0.67** |
| citm_catalog | 1.62 | **0.30** | **0.89** | 1.91 | **0.92** | **0.88** | **0.81** | **0.79** |
| canada | 1.20 | **0.31** | **0.88** | 2.23 | **0.95** | **0.87** | **0.89** | **0.88** |
| github | 1.70 | **0.71** | **0.69** | 1.93 | **0.66** | **0.55** | **0.61** | **0.52** |
| small_dict | 1.38 | **0.77** | **0.73** | 1.75 | **0.69** | **0.76** | **0.67** | **0.78** |
| unicode_strings | 1.43 | **0.75** | **0.79** | 25.6 | 2.25 | 2.30 | **0.99** | **0.98** |
| escaped_strings | 1.51 | **0.91**¹ | **0.91** | 1.22 | **0.39** | **0.38** | **0.39** | **0.39** |
| int_array | 1.36 | **0.89** | **0.87** | 2.06 | **0.76** | **0.88** | **0.76** | **0.91** |
| float_array | 1.26 | **0.91** | **0.97** | 2.33 | **0.77** | **0.82** | **0.77** | **0.80** |
| records | 1.60 | **0.45** | **0.80** | 2.10 | **0.66** | **0.63** | **0.65** | **0.63** |
| **geomean** | **1.48** | **0.63** | **0.81** | **2.59** | **0.82** | **0.82** | **0.71** | **0.71** |

"Before" is the pre-review code on 3.11. ¹ Median of three re-runs (0.90–0.92); the full run's first sample read 1.01.

On CPython 3.13, `loads` and `dumps_bytes` beat orjson on all 10 cases. `dumps` → `str` beats it on 8 of 10; the two exceptions are explained below and in §4 item 1.

### dumps, second round (branch `wip-dumps`): per-change measurements

Plain release builds (no PGO), same host. Measured by interleaving `rjson.dumps`, `rjson.dumps_bytes`, `orjson.dumps` and `orjson.dumps(x).decode()` in each round and taking the best of 21 rounds, which is less noisy than the median-of-7 in `corpus_benchmark.py`. The cells are the mean of two runs. The last column compares `dumps` with the like-for-like `str` result from orjson.

| case | 3.11 str before → after | 3.11 bytes before → after | 3.13 str before → after | 3.13 bytes before → after | 3.13 str vs `orjson.dumps().decode()` |
|---|---|---|---|---|---|
| twitter | 1.57 → 1.12 | 0.95 → **0.66** | 1.60 → 1.14 | 0.95 → **0.67** | **0.54** |
| citm_catalog | 1.15 → **0.85** | 1.01 → **0.77** | 1.14 → **0.90** | 1.01 → **0.80** | **0.68** |
| canada | 0.89 → **0.84** | 0.89 → **0.84** | 0.91 → **0.84** | 0.91 → **0.83** | **0.76** |
| github | 0.98 → **0.69** | 0.91 → **0.62** | 0.98 → **0.70** | 0.91 → **0.63** | **0.56** |
| small_dict | 0.74 → **0.71** | 0.71 → **0.68** | 0.80 → **0.73** | 0.82 → **0.72** | **0.50** |
| unicode_strings | 4.64 → 2.32 | 1.07 → **0.98** | 4.51 → 2.24 | 1.08 → **0.97** | **0.12** |
| escaped_strings | 0.37 → **0.40** | 0.38 → **0.39** | 0.37 → **0.39** | 0.37 → **0.38** | **0.36** |
| int_array | 1.02 → **0.89** | 1.03 → **0.89** | 1.05 → **0.85** | 1.06 → **0.85** | **0.76** |
| float_array | 0.84 → **0.76** | 0.84 → **0.77** | 0.88 → **0.78** | 0.87 → **0.78** | **0.74** |
| records | 1.00 → **0.65** | 0.99 → **0.65** | 0.98 → **0.66** | 0.98 → **0.66** | **0.61** |
| **geomean** | 1.06 → **0.83** | 0.85 → **0.70** | 1.07 → **0.83** | 0.87 → **0.71** | **0.51** |

`dumps` → `str` is still above 1.0 on twitter and unicode_strings. Most of what is left is memory traffic that comes from the return type: five emoji make twitter's 403k-character result UCS4 (1.6 MB, against 467 KB of UTF-8), and filling it takes about 100 µs of the 248 µs. Without the fill, str mode is faster than bytes mode.

### loads, second round (branch `wip-loads`): per-change measurements

Median of 3 interleaved in-process runs (old `.so` vs new `.so` vs orjson), no PGO.

| case | 3.11 before → after | 3.12 before → after | 3.13 before → after |
|---|---|---|---|
| twitter | 0.79 → 0.77 | 0.87 → 0.83 | 0.89 → 0.83 |
| citm_catalog | 0.62 → 0.59 | 0.94 → 0.90 | 0.94 → 0.89 |
| canada | 0.61 → 0.51 | 1.04 → 0.92 | 1.05 → 0.91 |
| github | 0.74 → 0.70 | 0.76 → 0.71 | 0.76 → 0.70 |
| small_dict | 0.77 → 0.76 | 0.77 → 0.75 | 0.74 → 0.74 |
| unicode_strings | 0.75 → 0.80 | 0.78 → 0.81 | 0.76 → 0.76 |
| escaped_strings | 1.40 → 0.90 | 1.41 → 0.94 | 1.42 → 0.90 |
| int_array | 1.06 → 0.92 | 1.02 → 0.97 | 1.04 → 0.91 |
| float_array | 1.15 → 0.90 | 1.17 → 0.94 | 1.22 → 0.94 |
| records | 0.69 → 0.64 | 0.92 → 0.85 | 0.91 → 0.82 |
| **geomean** | 0.83 → 0.74 | 0.95 → 0.87 | 0.95 → 0.84 |

unicode_strings got about 5% slower on 3.11/3.12 (flat on 3.13); bisecting points at a commit that doesn't touch that path, so it is most likely code layout.

**The 3.11 loads numbers include a garbage-collector pause.** CPython 3.10 and 3.11 run cyclic-GC passes while a large document is being built. `loads` pauses the collector for the duration of the call and restores its previous state afterwards (§5, decision 2). CPython 3.12+ already defers collection until the call returns, so the 3.13 column is the like-for-like parser comparison.

**Other metrics** (from the build review):

| metric | before | now | orjson |
|---|---|---|---|
| `.so` size | 4.7 MB (debug info) | 0.48 MB | 0.24 MB |
| import time | 0.42 ms | ~0.4 ms | 19.7 ms |
| `loads` of 300k records, resulting memory | 420 MB | 193 MB | 192 MB |
| `dumps` of a 79 MB output, peak memory | 3N | ~1N (writes into the result object) | 1N |
| memory held after a large `dumps` | 77 MB per thread, forever | 0 | 0 |
| per-call time, `loads('1')` / `dumps(None)` | 41 / 55 ns | ~25 / ~35 ns | 67 / 53 ns |
| Python versions verified | 3.11 only (3.12+ crashed) | 3.10–3.14 (3.14.0rc2) x86_64; 3.9, 3.12 aarch64 (qemu); `requires-python >=3.10` | 3.9–3.14 |

## 2. Correctness and safety defects found (all fixed on this branch)

Every one of these has a regression test in `tests/`.

| severity | defect | where it was |
|---|---|---|
| critical | `dumps` read string data at a hard-coded offset of 48 bytes. That is only valid up to 3.11: on 3.12 it aborted and on 3.13 it silently emitted garbage. | `lib.rs` `ASCII_DATA_OFFSET`, `bulk.rs` |
| critical | Heap buffer overflow in the SIMD escaper. It reserved `len + 64` bytes, but escaping can write up to `6 * len`. | `simd_escape.rs` |
| critical | Homogeneous-list fast paths checked only the first 16 elements. `[1]*16+[True]` became `…,1]`, `[1.0]*16+[7]` became `7.0`, and later elements could be read as the wrong object type. | `bulk.rs` |
| critical | Dict keys that are str subclasses were serialized as garbage. | `lib.rs` key path |
| critical | No recursion limit: a circular or deeply nested structure segfaulted. Now `rjson.JSONEncodeError` (a `TypeError` and `ValueError`) at depth 254, as in orjson. | `dumps`, `dumps_bytes`, `loads_simd` |
| critical | `.cargo/config.toml` forced `target-cpu=native` plus AVX2. Wheels contained AVX-512 instructions and would crash with SIGILL on most CPUs. | build config |
| high | `dumps_bytes` leaked its whole buffer on every call (a `mem::forget` after the bytes had already been copied). It also wrote `-2**63` as `-` and segfaulted on huge ints. | `extreme.rs` |
| high | A lone surrogate left a Python exception set, which surfaced as `SystemError`. | `lib.rs` |
| medium | `loads("[1] x")` returned `[1]`: trailing content was accepted. | serde path |
| medium | About 11% of canada.json's floats parsed 1 ulp off; `"-0"` became `-0.0`; ints beyond 64 bits became lossy floats; the nesting limit was 128. | serde defaults |
| medium | Subclasses (IntEnum, OrderedDict, namedtuple, …) were rejected. stdlib and orjson accept them. | type dispatch |

## 3. What changed and why it is fast

### loads (`src/parser.rs`, `src/lemire.rs`)

Dropping serde for a hand-written single-pass parser took the geomean from 1.48 to 0.98 (3.13). What mattered most, in order of measured impact:

1. **Dict-key cache.** Repeated keys reuse one `PyUnicode` whose hash is already computed; lookups compare bytes, so hash collisions are safe. Worth 25–30% on record-shaped documents, and it fixes the memory bloat.
2. **GC pause on 3.10/3.11.** See §5, decision 2.
3. **One reusable value stack, kept across calls.** Lists are created at their exact size; there is no `Vec` per array and no `String` per key. Reuse saves about 100 ns per call on small documents.
4. **Faster numbers.** Integers are parsed 8 digits at a time. Floats use an exact fast path, then Eisel-Lemire, then `fast_float` for more than 19 digits, and are correctly rounded.
5. **Strings.** Our own UTF-8 decoder writes straight into the final `str`. `bytes` input is validated once up front with simdutf8.
6. **Whitespace.** SIMD skipping, plus an inline fast path for a single space.
7. **Error handling.** The error type is zero-sized, so results come back in registers.

Second round (branch `wip-loads`, results in §1):

8. **Escaped strings.** A 32-byte block kernel (SSE2, AVX2 detected at runtime) handles every escape in a block from one bitmask and decodes `\u` escapes and surrogate pairs inline; errors and the last 64 bytes go to the scalar tail, so messages and positions are unchanged (escaped_strings 1.40 → 0.90).
9. **Numbers.** A one-pass fast path for `-?d{1,15}(.d{1,15})?([eE]…)?` with at most 19 digits and a cut-down inlined Eisel-Lemire; every other shape and every error falls back to the unchanged general parser. Verified against `float()` on canada's 111k numbers and ~1.5M random shapes (float_array 1.15–1.22 → 0.90–0.94).
10. **Allocation.** On 3.13, dicts are built with `_PyDict_FromItems` (exported but private; gated to 3.13), which presizes and uses the compact str-only key table. Lists get a `PyMem_Malloc` item array on `PyList_New(0)`, skipping calloc zeroing; fast-path floats use `PyObject_Malloc` + `PyObject_Init` on 3.13+.
11. **Key cache and bounds checks.** One folded multiply for the key hash, 16-byte compares instead of `memcmp`, and `peek()` without bounds checks by relying on the NUL byte after str/bytes/bytearray data (memoryview input is copied with a NUL appended). valgrind memcheck is clean.

### dumps (`src/ser.rs`)

1. **Write directly into the result object.** Output goes into a `bytes` object or a compact ASCII `str`. The old path decoded the whole output as UTF-8 again and cloned the buffer. For non-ASCII `str` output, each source string's native UCS1/2/4 data is copied into a result of exactly the right kind. This took unicode_strings from 25.6× to 5×.
2. **Dispatch.** Types are matched by exact `ob_type` pointer on raw borrowed pointers; there is no PyO3 refcount traffic per element. The per-list `detect_array_type` scan is gone.
3. **Floats.** Formatted in place with zmij, byte-identical to orjson (positive exponents are now `1e+16`, as `repr` writes them). Ints are read inline and formatted with itoap.
4. **Escaping.** AVX-512VL/AVX2 kernels selected at runtime, an SSE2 baseline, and exact worst-case reservation. escaped_strings is 0.33× orjson.
5. **Build target.** `x86-64-v2`. On the benchmark host, `native`/v3 made zmij about 1.6× slower because of the BMI2 code it generates.

Second round (branch `wip-dumps`, results above):

6. **Cursor in a register.** Writers take the output cursor and return the new one, and `Out::len` is only synced on growth. Errors are a null cursor: `Result<*mut u8, _>` does not fit in one register, and LLVM spilled it to the stack where code paths merge.
7. **Direct dict iteration** on 3.11–3.13 in place of `PyDict_Next`. This was the biggest win (twitter bytes 0.88 → 0.66, records 0.89 → 0.66). The layout is private, so it has a build-time gate and an import-time self-test; see CLAUDE.md.
8. **Lists.** Runs of exact ints and floats are written by a loop that keeps the item array and the capacity limit in registers and checks the exact type of every item (int_array 1.09 → 0.85). Compact ASCII strings of up to 16 bytes are written inline with one SSSE3 shuffle, because the 16 bytes that end at the string's end lie inside the object.
9. **The bytes-slower-than-str anomaly was glibc page faults.** Output buffers were allocated with 1/8 headroom and then shrunk with realloc on every call. Freeing the shrunk block only raises glibc's dynamic mmap threshold to the shrunk size, so every later (larger) request was mmapped again and page-faulted its whole output. Measured: 390 faults per call, and 571 µs instead of 144 µs for a 1.6 MB result. Whether this happened depended on what the process had freed before, which is why citm, int_array and canada ratios moved between runs. The headroom is now 1/16, below the 1/8 shrink threshold, and the size hints are kept per mode.
10. **Unsplit 256-bit loads and stores.** The generic x86-64-v2 tuning makes LLVM split every unaligned 256-bit access, even inside AVX2/AVX-512 functions. The escape kernels now use `vmovdqu` through inline asm.
11. **Non-ASCII `str` output.** The escape scan narrows UCS2/UCS4 to bytes with saturating packs, checking 64 or 128 bytes per test. The check happens while the result is filled, so each string is read from cache and large same-kind strings are copied and checked in one pass. Escaping is done during the fill instead of by building a temporary `str`. unicode_strings went from 5.0× to 2.2×.
12. **Large strings.** Strings over 64 KiB are escaped in pieces sized to the room left in the buffer. The old code reserved 6× per chunk, which forced a doubling realloc.

Tried and reverted, because each measured slower: SWAR digit formatting for all ints (on small ints the multiply chain costs more than the predicted branches it replaces), an inline 17–32-byte string path (code growth in the dict loop), inlining nested small lists (no gain on canada), and AVX2 widening for the `str` fill (memory-bound).

### Build and entry points (`src/entry.rs`, `Cargo.toml`, `scripts/`)

- **Raw entry points** (`METH_O` for `loads`; `METH_FASTCALL|METH_KEYWORDS` with a one-compare fast path for `dumps`/`dumps_str`, since `default=`) replace `#[pyfunction]`, saving about 8 ns per call. They keep PyO3's trampoline so panics are caught and PyO3's GIL bookkeeping stays correct.
- **No debug info in release builds**, which shrinks the `.so` about 10×.
- **All ~3.6k lines of the old `src/` replaced** (including dead or slower code: `lib_backup.rs`, `extreme.rs`, `simd_parser.rs`/`loads_simd`, `bulk.rs`, `type_cache.rs`, the old escaper) by ~3.7k lines in five files. The serde, simd-json, ahash, smallvec and memchr dependencies went with it.
- **`scripts/build_pgo.sh`** does instrument → train (`scripts/pgo_train.py`) → merge → rebuild, one profile per interpreter. The first round's "3.11 PGO" numbers (since replaced in §1 by plain release builds) came from an earlier version of the script that had two flaws: it trained on the benchmark itself (corpora and the benchmark's synthetic cases), and it passed the PGO flags through `RUSTFLAGS`, which silently replaced `.cargo/config.toml`'s `target-cpu=x86-64-v2`, so those wheels were baseline x86-64. Both are fixed: flags go through `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` (merged with the config), and training uses seeded synthetic documents of other shapes that read no corpus file.

  **PGO gain, re-measured** (geomean of rjson/orjson over the 10 benchmark cases vs a plain release build; 5 interleaved rounds, median per case; CPython 3.13 / 3.11; noisy shared host, so ±2–3% on a geomean is noise):

  | profile | loads | dumps | dumps_bytes |
  |---|---|---|---|
  | disjoint synthetic training (current) | −0.2% / −2.2% | −0.2% / −2.2% | −4.3% / −2.6% |
  | old, trained on the benchmark | 0.0% / −3.7% | −3.7% / −3.8% | −5.0% / −4.6% |

  PGO is worth about 0–4% once it cannot see the benchmark, not the −7 to −10% measured before. Most of the difference is on cases the old profile trained on verbatim: int_array `dumps` on 3.11 is 0.83× orjson with the old profile, 1.00× plain and 1.01× with the disjoint one. A first draft of the disjoint set, whose documents were almost all non-ASCII, made citm `dumps` (pure ASCII `str` output) 20% slower; the current set is mostly ASCII, like most real JSON.

## 4. Remaining gaps, ranked

| # | gap (3.13, no PGO) | idea | expected | risk |
|---|---|---|---|---|
| 1 | `dumps` → `str` with non-ASCII text: now unicode_strings 2.2×, twitter 1.1× (was 5.0× and 1.5×) | The rest is the memory traffic of a UCS2/UCS4 result. Options: (a) make `dumps` return `bytes` (decision 1), or (b) non-temporal stores for multi-MB results, which help benchmarks but not real consumers. | (a) all cases below 1.0 | (a) API |
| 2 | ~~`dumps_bytes` slower than `dumps` → `str`~~ **fixed**: glibc mmap-threshold page faults from the buffer shrink (§3 dumps item 9) | — | — | — |
| 3 | ~~`loads` escaped_strings 1.46×~~ **done**: 0.91× (block kernel, §3 loads item 8) | — | — | — |
| 4 | ~~`loads` float_array 1.13×, canada 1.02×~~ **done**: 0.97× / 0.88× on 3.13. What is left is mostly `PyFloat`/`PyLong` allocation and `pos` living in memory across the array loop | keep `pos` in a local across the array loop | −3–5% | low |
| 5 | ~~`loads` on 3.12+ at parity on twitter/records/canada~~ **done**: 0.68 / 0.80 / 0.88 on 3.13. Next: a SIMD UTF-8 → UCS2/UCS4 decoder (non-ASCII decoding is ~10% of twitter). CPython 3.13 runs a young-generation GC right after `loads` returns that walks every new list (~3 ms of canada's ~8 ms, for both libraries); it can't be avoided without untracking lists, which could leak user-made cycles | SIMD UTF-8 decode | −5% on non-ASCII docs | medium |
| 6 | ~~`dumps` int_array 1.21×~~ **done**: 0.85× with the per-item-checked list loop. Next: canada/float_array spend about 60% of their time in zmij, which orjson uses too | a faster shortest-float formatter | −10–20% on float-heavy docs | medium |
| 7 | Release wheels without PGO | **Done** (`.github/workflows/wheels.yml`): PGO wheels per interpreter for manylinux2014/musllinux x86_64+aarch64, macOS arm64/x86_64, Windows, trained on a workload disjoint from the benchmark. | −0–4% (re-measured, §3) | build-only |
| 8 | Non-x86 | The scalar/SWAR fallbacks now pass the full suite on aarch64 (3.9, 3.12 under qemu; CI runs native arm64 Linux and macOS). NEON kernels would replace the scalar loops at the four `#[cfg(target_arch = "x86_64")]` SSE2 sites in `parser.rs` (`skip_ws_slow`, `scan_special`, the escaped-string copy loop, `utf8_count_and_max`) and the SWAR `escape_long` / scalar `kind_needs_escape` in `ser.rs`; each maps to `vceqq_u8` + a narrowing-shift movemask. Needs native arm64 hardware to measure. | parity on Apple Silicon / Graviton | medium |

### Found by the production benchmark (`benches/production_benchmark.py`)

| # | gap | status |
|---|---|---|
| 9 | small `dumps` after a big one 1.7–2.0× (capacity hint = last size) | **fixed**: hint = min of last two sizes |
| 10 | big `dumps` peaked at 1.8× output, 16 MB stranded (doubling on the brk heap) | **fixed**: jump to recent peak, ≥ 32 MiB mmapped reservation past 1 MiB, shrink back (`Out::reserve`, `into_object`) |
| 11 | CJK/UCS-2 `loads` 1.3× (1.6× on 3.11) | **fixed** ([#8](https://github.com/TinDang97/rjson/issues/8)): `decode_ucs2` SIMD/pairwise decoder; 0.55–0.91× on CJK/hangul/Cyrillic. Open: UCS-4 text shifted 0.71 → 0.80 on 3.13 with identical instruction counts (placement) |
| 12 | mixed-magnitude float arrays `loads` 1.12–1.32× | **fixed** ([#9](https://github.com/TinDang97/rjson/issues/9)): full-precision doubles (16–19 fraction digits) fell off the fast path; now 0.89–0.95× on 3.11 and 3.13. Arrays dominated by `0.0` stay at ~1.05× (float allocation, not parsing) |
| 13 | UTF-8 cache attached by `dumps`, copies in `loads(str)` / `loads(memoryview)` | **fixed** ([#10](https://github.com/TinDang97/rjson/issues/10)): hybrid by size (short strings keep CPython's cached copy, long ones are encoded directly / via a temporary buffer); whole-object memoryviews parsed in place |

Numbers: [docs/PRODUCTION_READINESS.md](PRODUCTION_READINESS.md#performance-fixes).

### Threading and I/O (researched, mostly not applicable)

- **io_uring: rejected.** `loads`/`dumps` do no I/O. Even reading a file from the page cache is 1–4% of parse time: canada.json takes 0.2 ms to read and 14 ms for orjson to parse. It only matters for a bulk-ingestion CLI.
- **Thread-per-core for single calls: rejected.** Python objects can only be created or read under the GIL. The only work that could run in parallel is scanning and validating the text, which is a small share of `loads` (Amdahl). A thread hand-off costs microseconds, while typical documents take 0.4–80 µs.
- **Parallel `dumps` for very large documents: experimental, worth trying.** While the caller holds the GIL and waits, worker threads each serialize a slice of a large top-level list or dict into its own buffer, and the buffers are concatenated. Workers do pure reads with no refcounting and no calls into Python, and non-ASCII strings are encoded from their raw data. Documents under about 1 MB use one thread. This could beat orjson on multi-MB payloads such as canada. It needs a different design for free-threaded builds.
- **Releasing the GIL during `loads`: throughput feature.** Validate or tokenize with the GIL released, then build objects with it held. Single calls get no faster, but other threads in a multi-threaded server can run meanwhile.

### Platform and tooling

- **PyO3 upgrade: done (0.24 → 0.29).** Needed for 3.14, since pyo3-ffi 0.24 refuses to build for it. `src/compat.rs` declares `_PyBytes_Resize` / `_PyDict_NewPresized` (no longer re-exported by pyo3-ffi, still exported by CPython) and provides the str accessors: on 3.14, pyo3-ffi drops the inline `PyUnicode_IS_*ASCII` readers and makes `KIND`/`DATA` out-of-line calls, so compat.rs reads the GIL build's unchanged `state` bitfield itself behind an import-time self-test against libpython, and refuses to compile for `Py_GIL_DISABLED`. All 247 tests pass on 3.9–3.14.0rc2. Private symbols and layouts in use, and their version gates, are listed in CLAUDE.md.
- **Free-threaded builds (3.13t/3.14t).** Keep the module marked as GIL-requiring. The serializer iterates lists and dicts through borrowed references and would need critical sections before it could drop that. orjson doesn't support free-threading either.
- **CI.** `.github/workflows/ci.yml`: clippy, then build + pytest on 3.10–3.14 (ubuntu x86_64), a no-AVX-512 variant, ubuntu-24.04-arm (3.10, 3.13), macos-14 and Windows (3.13). `cargo fmt --check` is not enforced yet (`entry.rs` and `parser.rs` are not rustfmt-clean) and clippy warnings are not fatal (one in `ser.rs`; five more dead-code/unused warnings only on aarch64).
- **Performance regression gate.** `.github/workflows/perf.yml` (PRs labelled `perf`, or manual): builds base and head in one job, runs `corpus_benchmark.py --output-json` for both, interleaved ×5 on 3.11 and 3.13, and `benches/perf_gate.py` fails if any geomean is more than 5% worse. Corpora come from `benches/fetch_corpus.sh` (sha256-pinned). On this dev host, two runs of the same build differ by 1–3% in geomean, so 5% with 5 rounds is about the floor. Still to add: per-call time on tiny documents, `.so` size, import time and peak memory.
- **Fuzzing.** Both review rounds ran differential fuzzers against stdlib `json` (≈150k `dumps` cases, plus `loads` value/error/float fuzzing), but the scripts live outside the repo. Next: check them in under `tests/fuzz/` and run a random-structure fuzzer under ASan in CI.
- **Feature parity.** `default=` is done (entry is `METH_FASTCALL|METH_KEYWORDS`; per-call cost unchanged, containers +~8 instructions for the mode check). orjson also offers indent, sorted keys, and serialization of datetime, UUID, dataclasses and numpy. Add them as further hand-parsed keyword names, resolving those types lazily so import time stays low.

## 5. Decisions (resolved)

1. **`dumps` returns `bytes`** (like `orjson.dumps`). `dumps_str` returns `str`, and `dumps_bytes` stays as an alias of `dumps`. Callers that relied on `dumps` returning `str` move to `dumps_str` or `.decode()`. In the tables above, "dumps_bytes" is today's `dumps` and "dumps → str" is today's `dumps_str`.
2. **Private CPython internals are kept**, each gated to the versions checked and, where it reads a layout, self-tested at import (list in CLAUDE.md).
3. **Minimum Python is 3.10** (`requires-python >=3.10`; CI and wheels cover 3.10–3.14).
4. **GC pause in `loads` on 3.10/3.11** stays on by default; it restores the previous GC state and does nothing on 3.12+.
5. **Lone surrogates:** `dumps` (bytes) raises `UnicodeEncodeError`, as orjson does; `dumps_str` passes them through, as `json.dumps(ensure_ascii=False)` does.

## 6. Reproducing

```bash
uv venv .venv -p 3.11 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release
python -m pytest tests -q                      # 856 tests
benches/fetch_corpus.sh                        # corpora -> benches/data/ (sha256-pinned)
python benches/corpus_benchmark.py [--json] [--output-json results.json]
python benches/make_charts.py results.json     # README charts -> docs/img/
scripts/build_pgo.sh python3.11 python3.13     # PGO wheels -> target/wheels/
python benches/perf_gate.py --base base-*.json --head head-*.json
scripts/test_aarch64_qemu.sh 3.10 3.12          # aarch64 cross-build + tests under qemu
```
