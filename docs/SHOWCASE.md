# rjson vs orjson: where the difference shows

A head-to-head comparison on the jobs a Python service actually does with JSON: API
responses and request bodies, log records, application types (datetimes, UUIDs, Enums,
dataclasses), `str` output, plus the standard corpora for reference. Every case first checks
that both libraries produce the same result (byte-identical output for `dumps`, equal objects
for `loads`), then times them interleaved in one process.

**Result: rjson is faster in 21 of 22 cases, 1.52× on the geometric mean.** The one case
where it is not (per-record NDJSON, 0.92×) is listed with the rest.

Speedup = orjson's time ÷ rjson's time (higher is better). Median of 3 runs of
`benches/showcase.py --rounds 15`, each the median of 15 interleaved rounds; in parentheses
the lowest and highest of the 3 runs. CPython 3.13.12, orjson 3.12.0, x86_64 Xeon (4
cores), rjson PGO build (`scripts/build_pgo.sh`, as the published wheels are built). Raw
numbers: [showcase-results.json](showcase-results.json).

| workload | rjson | orjson | speedup |
|---|---|---|---|
| **Application types** (no `default=` needed in either) | | | |
| 5,000 UTC `datetime`s | 160 µs | 501 µs | **3.17×** (3.12–3.29) |
| 2,000 events with `datetime`, `UUID`, `Enum` | 349 µs | 815 µs | **2.34×** (2.31–2.34) |
| dict with 5,000 int keys (`non_str_keys` / `OPT_NON_STR_KEYS`) | 85 µs | 186 µs | **2.19×** (2.15–2.23) |
| 2,000 `@dataclass(slots=True)` instances | 448 µs | 986 µs | **2.14×** (2.10–2.37) |
| 1,000 dataclasses with nested items | 193 µs | 212 µs | **1.09×** (1.08–1.10) |
| **Output as `str`** (`dumps_str` vs `orjson.dumps(...).decode()`) | | | |
| github.json | 13.7 µs | 27.2 µs | **1.99×** (1.98–2.06) |
| log records | 417 µs | 737 µs | **1.77×** (1.77–1.78) |
| twitter.json | 289 µs | 477 µs | **1.65×** (1.63–1.66) |
| **Web API** | | | |
| github.json response (`dumps`, 55 KB) | 12.0 µs | 21.6 µs | **1.78×** (1.77–1.80) |
| paginated REST page (50 users, 45 KB) | 18.8 µs | 30.8 µs | **1.64×** (1.60–1.68) |
| github.json request (`loads` of bytes) | 70.1 µs | 93.5 µs | **1.29×** (1.25–1.34) |
| 1,000 small request bodies, one `loads` each | 1.28 ms | 1.53 ms | **1.25×** (1.19–1.25) |
| 1,000 small responses, one `dumps` each | 150 µs | 187 µs | **1.24×** (1.20–1.25) |
| **Strings that need escaping** (tracebacks, SQL, paths) | | | |
| 2,000 log records as one document | 412 µs | 642 µs | **1.56×** (1.53–1.61) |
| parse them back | 1.19 ms | 1.67 ms | **1.40×** (1.39–1.45) |
| NDJSON: one `dumps` per record, written to a stream | 958 µs | 880 µs | 0.92× (0.92–0.94) |
| **Standard corpora** | | | |
| twitter.json `dumps` / `loads` | 133 µs / 1.20 ms | 241 µs / 1.26 ms | **1.82×** / **1.10×** |
| citm_catalog.json `dumps` / `loads` | 386 µs / 3.50 ms | 516 µs / 3.76 ms | **1.33×** / **1.09×** |
| canada.json `dumps` / `loads` | 2.58 ms / 7.35 ms | 2.86 ms / 8.30 ms | **1.12×** / **1.11×** |

Times are the medians of the 3 runs.

## Why rjson is ahead

- **Application types.** orjson serializes datetime/UUID/Enum/dataclass natively too, and
  rjson matches its output byte for byte. rjson reads the values straight from the C
  structs (datetime fields, the UUID's `int` slot), and caches per type what it learned
  about the class: whether it is a dataclass, whether it has `__slots__`, whether its Enum
  `_value_` and instance `__dict__` can be read without running Python code. The cache is
  keyed by CPython's `tp_version_tag`, which changes whenever the class or a base changes.
- **Escaping.** AVX-512 / AVX2 / SSE2 kernels chosen at run time, with the worst case
  reserved up front, so the inner loop has no bounds checks.
- **`str` output.** `dumps_str` writes the `str` object directly. orjson users who need a
  `str` (templates, `logging.Formatter.format`, APIs that take text) pay for a `bytes`
  result and then a decode.
- **Per-call overhead.** The entry points are raw `METH_FASTCALL` builtins (about 8 ns
  cheaper than a PyO3 `#[pyfunction]`), and the output buffer is sized from the previous
  calls.
- **`loads`.** A dict-key cache that reuses `str` objects together with their hashes,
  8-digits-at-a-time integers, and correctly rounded floats on a fast path that covers
  full-precision doubles.

## Found and fixed while building this comparison

The first run had rjson behind orjson on dataclasses (0.61×) and only at parity on
datetime/UUID/Enum events (1.05×). Profiling found per-value costs that orjson does not pay:

- For every datetime, UUID, Enum or dataclass value, rjson re-checked `sys.modules` for
  modules that are not imported. `zoneinfo` usually isn't, and each check built a
  temporary `str`, about 80 ns per value. Now the check runs again only when
  `len(sys.modules)` changes, with a forced re-check before a value is reported
  unsupported.
- Every Enum member walked its class's MRO, and every dataclass did two class-dict
  lookups. Both are now cached per type.
- Any dataclass sent the whole document to guarded mode, the slower path used when Python
  code may run mid-serialization. On CPython 3.12+ the standard `__dict__` of a class
  runs no Python code, so those dataclasses no longer do. On 3.10 and 3.11, creating the
  dict can run the garbage collector on the spot, so they stay guarded there.

The same comparison on the plain build, before → after: events 1.05× → 2.24×, UTC
timestamps 1.36× → 3.64×, dataclasses 0.61× → 1.1×.

## Where rjson is not ahead

**One `dumps` call per ~600-byte record with many escapes, results streamed out: 0.92×.**
The same records as one document are 1.56× faster, so the escaping is not the cause. The
per-call fixed cost is, and on this record size it outweighs rjson's per-byte advantage.
Two output-buffer sizing variants (hinting from the largest reservation a call made, and
using the larger of the last two sizes for small outputs) cut buffer regrowth by 60% but
made no measurable difference, so they were not kept. Smaller records are faster in rjson
(1,000 small responses: 1.24×).

## Allocator effect: short-lived processes

Encode the same 2,000 log records one call at a time, keep the results (a batch for a queue
or a bulk insert), and do it in a new process that has not yet produced a large output:

| | time | page faults |
|---|---|---|
| rjson | 0.81–0.86 ms | 1–4 |
| orjson | 3.32–3.74 ms | 1,976 |

That is about 4.2× in rjson's favour. In this state glibc's malloc returns memory to the OS
and takes it back around every orjson call, one page fault per call. Once any output over
~128 KiB has been freed, glibc's thresholds rise and the effect disappears. The main table
above is measured in that warm state (the suite builds its big fixtures first), so it does
not include this effect. It is real for CLI tools, batch jobs, workers that restart often,
and serverless cold starts. `benches/showcase.py --fresh` measures it.

## Reproduce

```bash
benches/fetch_corpus.sh                         # sha256-pinned corpora -> benches/data/
scripts/build_pgo.sh python3.13 && pip install target/wheels/*cp313*.whl orjson
python benches/showcase.py --rounds 15 --fresh --output-json showcase.json
```

The reference benchmark (per-case `loads`/`dumps` on the corpora and synthetic cases, also
against `json`) is `benches/corpus_benchmark.py`. Production-shaped workloads with memory
(peak RSS) are in `benches/production_benchmark.py`, and the example integrations
(FastAPI, logging, codecs) are in `benches/examples_benchmark.py`. Methodology and history
are in [PERFORMANCE_REVIEW.md](PERFORMANCE_REVIEW.md).
