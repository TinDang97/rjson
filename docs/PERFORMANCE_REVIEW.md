# rjson performance review: closing the gap to orjson

Four parallel reviews ran against the pre-review code (commit `c12dbe3`):

- deserialization (`loads`)
- serialization (`dumps`)
- correctness and safety
- build and architecture

The loads, dumps and build reviews each prototyped and measured their changes. This branch merges those prototypes. This document records what we found, what landed, how it was measured, and what is still left to beat orjson everywhere.

## 1. Results

Numbers are rjson time divided by orjson time on the same run, so **below 1.00 means rjson is faster**. They come from `benches/corpus_benchmark.py` on an x86_64 Xeon (4 shared cores) against orjson 3.11. The host is noisy, so treat single cells as ±10% and trust the geomeans. `dumps` returns `str`; `dumps_bytes` returns `bytes`, the same type `orjson.dumps` returns.

| case | loads before | loads 3.11 PGO | loads 3.13 | dumps before | dumps 3.11 PGO | dumps 3.13 | dumps_bytes 3.11 PGO | dumps_bytes 3.13 |
|---|---|---|---|---|---|---|---|---|
| twitter | 1.91 | **0.81** | 1.04 | 3.05 | 1.44 | 1.50 | **0.88** | **0.89** |
| citm_catalog | 1.62 | **0.38** | **0.92** | 1.91 | 1.05 | 1.18 | 1.22 | 1.41 |
| canada | 1.20 | **0.36** | 1.02 | 2.23 | 1.03 | 1.02 | 1.03 | 1.05 |
| github | 1.70 | **0.79** | **0.84** | 1.93 | **0.74** | **0.89** | **0.71** | **0.84** |
| small_dict | 1.38 | **0.76** | **0.79** | 1.75 | **0.78** | **0.90** | **0.77** | **0.86** |
| unicode_strings | 1.43 | **0.81** | **0.79** | 25.6 | 5.30 | 5.00 | **0.92** | 1.11 |
| escaped_strings | 1.51 | 1.40 | 1.46 | 1.22 | **0.34** | **0.33** | **0.35** | **0.39** |
| int_array | 1.36 | **0.99** | **0.96** | 2.06 | **0.79** | 1.21 | **0.94** | 1.26 |
| float_array | 1.26 | 1.19 | 1.13 | 2.33 | **0.95** | **0.98** | **0.98** | 1.06 |
| records | 1.60 | **0.54** | 1.01 | 2.10 | **0.84** | **0.97** | **0.82** | **0.97** |
| **geomean** | **1.48** | **0.74** | **0.98** | **2.59** | **1.00** | **1.11** | **0.82** | **0.94** |

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
| Python versions verified | 3.11 only (3.12+ crashed) | 3.11, 3.12, 3.13 | 3.9–3.14 |

## 2. Correctness and safety defects found (all fixed on this branch)

Every one of these has a regression test in `tests/`.

| severity | defect | where it was |
|---|---|---|
| critical | `dumps` read string data at a hard-coded offset of 48 bytes. That is only valid up to 3.11: on 3.12 it aborted and on 3.13 it silently emitted garbage. | `lib.rs` `ASCII_DATA_OFFSET`, `bulk.rs` |
| critical | Heap buffer overflow in the SIMD escaper. It reserved `len + 64` bytes, but escaping can write up to `6 * len`. | `simd_escape.rs` |
| critical | Homogeneous-list fast paths checked only the first 16 elements. `[1]*16+[True]` became `…,1]`, `[1.0]*16+[7]` became `7.0`, and later elements could be read as the wrong object type. | `bulk.rs` |
| critical | Dict keys that are str subclasses were serialized as garbage. | `lib.rs` key path |
| critical | No recursion limit: a circular or deeply nested structure segfaulted. Now a `ValueError` at depth 254, as in orjson. | `dumps`, `dumps_bytes`, `loads_simd` |
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

### dumps (`src/ser.rs`)

1. **Write directly into the result object.** Output goes into a `bytes` object or a compact ASCII `str`. The old path decoded the whole output as UTF-8 again and cloned the buffer. For non-ASCII `str` output, each source string's native UCS1/2/4 data is copied into a result of exactly the right kind. This took unicode_strings from 25.6× to 5×.
2. **Dispatch.** Types are matched by exact `ob_type` pointer on raw borrowed pointers; there is no PyO3 refcount traffic per element. The per-list `detect_array_type` scan is gone.
3. **Floats.** Formatted in place with zmij, byte-identical to orjson (positive exponents are now `1e+16`, as `repr` writes them). Ints are read inline and formatted with itoap.
4. **Escaping.** AVX-512VL/AVX2 kernels selected at runtime, an SSE2 baseline, and exact worst-case reservation. escaped_strings is 0.33× orjson.
5. **Build target.** `x86-64-v2`. On the benchmark host, `native`/v3 made zmij about 1.6× slower because of the BMI2 code it generates.

### Build and entry points (`src/entry.rs`, `Cargo.toml`, `scripts/`)

- **Raw `METH_O` entry points** replace `#[pyfunction]`, saving about 8 ns per call. They keep PyO3's trampoline so panics are caught and PyO3's GIL bookkeeping stays correct.
- **No debug info in release builds**, which shrinks the `.so` about 10×.
- **All ~3.6k lines of the old `src/` replaced** (including dead or slower code: `lib_backup.rs`, `extreme.rs`, `simd_parser.rs`/`loads_simd`, `bulk.rs`, `type_cache.rs`, the old escaper) by ~3.7k lines in five files. The serde, simd-json, ahash, smallvec and memchr dependencies went with it.
- **`scripts/build_pgo.sh`** does instrument → train (`scripts/pgo_train.py`) → merge → rebuild. It is worth −7 to −10% on dumps. The training set overlaps the benchmark corpus, so the PGO numbers above are an upper bound.

## 4. Remaining gaps, ranked

| # | gap (3.13, no PGO) | idea | expected | risk |
|---|---|---|---|---|
| 1 | `dumps` → `str` with non-ASCII text: unicode_strings 5.0×, twitter 1.5× | This is set by the `str` return type itself: twitter's output is 1.6 MB as UCS4 but 467 KB as UTF-8. Either (a) make `dumps` return `bytes` like orjson, which breaks the API (decision 1), or (b) AVX2 widening/copy for UCS2/UCS4 and skip the temporary ASCII buffer once the output is known to be non-ASCII. | (a) all cases ≤ 1.1×; (b) about −20–30% on those cases | (a) API; (b) low |
| 2 | `dumps_bytes` is slower than `dumps` → `str` on int- and number-heavy documents (citm 1.41× vs 1.18×, int_array 1.26× vs 1.21×) | This is unexpected, since bytes mode does strictly less work. Check the bytes growth path (`_PyBytes_Resize` copying on growth) and whether the size hint is shared between the two modes. | brings citm/int_array under 1.0 | low |
| 3 | `loads` escaped_strings 1.46× | Handle every escape in a 16/32-byte block from the backslash bitmask. Stop tracking non-ASCII per chunk. Add an AVX2 kernel with runtime detection. | ~1.0× | low |
| 4 | `loads` float_array 1.13×, canada 1.02× | Parse integer and fraction digits in one pass; fast path for the `d+.d{1,15}` shape (≈150 → ≈90 instructions per float). | ~0.9× | low |
| 5 | `loads` on 3.12+ is at parity on twitter/records/canada | `_PyDict_SetItem_KnownHash` with the cached key hash; inline key comparison instead of `memcmp`; for `str` input, parse non-ASCII directly from its internal form. | −5–10% | low |
| 6 | `dumps` int_array 1.21× | Homogeneous-list loop with a per-item exact type check (the correct version of the old bulk path). | ~1.0× | low |
| 7 | Release wheels without PGO | Build PGO wheels in CI for every Python version, and train on a separate workload so the benchmark isn't overfitted. | −7–10% | build-only |
| 8 | Non-x86 | NEON kernels for escaping and whitespace. The SWAR fallback is untested on aarch64. | parity on Apple Silicon / Graviton | medium |

### Threading and I/O (researched, mostly not applicable)

- **io_uring: rejected.** `loads`/`dumps` do no I/O. Even reading a file from the page cache is 1–4% of parse time: canada.json takes 0.2 ms to read and 14 ms for orjson to parse. It only matters for a bulk-ingestion CLI.
- **Thread-per-core for single calls: rejected.** Python objects can only be created or read under the GIL. The only work that could run in parallel is scanning and validating the text, which is a small share of `loads` (Amdahl). A thread hand-off costs microseconds, while typical documents take 0.4–80 µs.
- **Parallel `dumps` for very large documents: experimental, worth trying.** While the caller holds the GIL and waits, worker threads each serialize a slice of a large top-level list or dict into its own buffer, and the buffers are concatenated. Workers do pure reads with no refcounting and no calls into Python, and non-ASCII strings are encoded from their raw data. Documents under about 1 MB use one thread. This could beat orjson on multi-MB payloads such as canada. It needs a different design for free-threaded builds.
- **Releasing the GIL during `loads`: throughput feature.** Validate or tokenize with the GIL released, then build objects with it held. Single calls get no faster, but other threads in a multi-threaded server can run meanwhile.

### Platform and tooling

- **PyO3 upgrade.** Upgrading from 0.24 is required for Python 3.14 support. It costs nothing per call now that the entry points are raw, but deprecated APIs (`to_object`/`into_py`) need replacing.
- **Free-threaded builds (3.13t/3.14t).** Keep the module marked as GIL-requiring. The serializer iterates lists and dicts through borrowed references and would need critical sections before it could drop that. orjson doesn't support free-threading either.
- **CI.** Add a maturin-action matrix: 3.9–3.14; manylinux, musllinux, macOS universal2 and Windows; x86_64 and aarch64. Run pytest on every Python version; the 3.12 layout bug is why.
- **Performance regression gate.** Compare against orjson in the same process, interleaved and median-of-N; fail CI if either geomean gets more than 5% worse than main. Also track per-call time on tiny documents, `.so` size, import time and peak memory.
- **Fuzzing.** Add a differential fuzzer for `dumps` against `json.dumps` (the loads review already fuzzed `loads`), plus a random-structure fuzzer under ASan in CI.
- **Feature parity.** orjson also offers indent, sorted keys, `default=`, and serialization of datetime, UUID, dataclasses and numpy. Add them behind `METH_FASTCALL|METH_KEYWORDS` with hand-parsed keyword names, resolving those types lazily so import time stays low.

## 5. Decisions for the maintainer

1. **Return type of `dumps`.** Today `dumps` returns `str`, matching stdlib, and `dumps_bytes` returns `bytes`, matching orjson. Making `dumps` return `bytes` would win every serialization benchmark, but it breaks the API. Recommendation: keep both functions, document `dumps_bytes` as the fast path, and decide before 1.0.
2. **GC pause in `loads` on 3.10/3.11.** It is on by default. It is the largest single win on 3.11 (citm 0.96 → 0.62 before the other work). It restores the previous GC state and does nothing on 3.12+. The trade-off: no cyclic collection happens during one `loads` call.
3. **Lone surrogates in `dumps` → `str`.** They are passed through, as `json.dumps(ensure_ascii=False)` does. `dumps_bytes` raises `UnicodeEncodeError`, and orjson always raises.

## 6. Reproducing

```bash
uv venv .venv -p 3.11 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release
python -m pytest tests -q                      # 212 tests
# corpora: twitter/citm_catalog/canada from serde-rs/json-benchmark data/,
# github.json from ijl/orjson data/github.json.xz
RJSON_BENCH_DATA=/path/to/corpus python benches/corpus_benchmark.py
RJSON_BENCH_DATA=/path/to/corpus scripts/build_pgo.sh python3.11   # PGO wheel -> target/wheels/
```
