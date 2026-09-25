# Production readiness report

Can rjson replace `json` or orjson in a production Python service today? This report
answers that question for the four workloads we tested: web APIs, logs and events, data
pipelines with large files, and cache or message-queue payloads. Everything in it was
measured or checked by running code; nothing is taken from docs or assumed.

*Scope: rjson 0.1.0 (PyPI distribution `pyrjson`), CPython 3.10–3.14, orjson 3.12.0 as
the reference. Measured on a 4-core x86_64 VM (noise ±10%) with plain release builds (no
PGO).*

## Verdict

**Ready for JSON-native payloads.** If your data is made of dict, list, str, int, float,
bool and None (API responses built from dicts, log records, events, cache blobs), rjson is
faster than orjson on nearly every production shape we measured, uses less memory on large
documents, and produced **0 mismatches** against orjson and `json` across all workloads.

**Not yet a drop-in for orjson users who rely on its options.** `dumps(obj, default=f)`
works as in orjson and `json`, but there is no native datetime/UUID/dataclass (each goes
through `default`) and no non-str dict keys. You can work around these today (see
[`examples/`](../examples/)), and each is tracked as an issue.

| question | answer |
|---|---|
| Faster than `json`? | Yes: 4–20× on `dumps`, 1.3–5× on `loads`. |
| Faster than orjson? | Yes on most shapes: geomean `dumps` 0.76×, `loads` 0.87×, round trip 0.83× (rjson ÷ orjson time). CJK text (0.59–0.91×) and full-precision float arrays (0.89–0.95×) now load faster than orjson; see [Performance](#performance). |
| Correct? | 0 mismatches. 856 tests, fuzzing against `json`, and output byte-identical to orjson. |
| Memory? | Better on large `loads`: peak RSS 30–37% below orjson. Retained small results cost ~400 B instead of ~8 KB each. |
| Safe for async services? | Yes, but each call blocks the event loop, and `to_thread` doesn't help. See [ASYNC.md](ASYNC.md). |
| Compress / go binary? | Compress at the transport, and only payloads of a few KB and up. Use Arrow/Polars only for columnar data. See [Transfer size](#transfer-size-compression-and-binary-formats). |
| Installable? | Not on PyPI yet. The planned distribution name is `pyrjson`; CI builds PGO wheels for Linux, macOS and Windows. |
| Stable API? | No: 0.x, experimental. |

## Workload results

Numbers are rjson time ÷ orjson time from `benches/production_benchmark.py` (CPython
3.13, median of 9 interleaved rounds, rotating inputs so caches are not flattered).
**Below 1.00 means rjson is faster.**

### Web API (FastAPI-style)

| case | `dumps` | `loads` |
|---|---|---|
| paginated REST page (50 nested objects) | 0.78 | 0.84 |
| GraphQL-style nested response | 0.81 | 0.84 |
| ~300 B request body | n/a | 0.85 (0.83 from `str`) |
| small response (~150 B) | 0.83 | n/a |
| mixed and alternating response sizes | 0.81–0.86 | n/a |
| per-call overhead (tiny documents) | 0.66 | 0.62 |

In a real FastAPI app the serializer is rarely the bottleneck. `jsonable_encoder` takes
~590 µs for 100 records, while `rjson.dumps` of the same data takes 6.3 µs (orjson 8.5 µs).
Return `RJSONResponse(...)` directly for native data
([`examples/fastapi_app.py`](../examples/fastapi_app.py)). Endpoints with a response model
already use Pydantic's fast `dump_json`, so leave those alone.

### Logs, NDJSON, events

| case | result |
|---|---|
| encode log records and keep the lines (NDJSON join) | **0.08** (orjson allocates ~8 KB per result) |
| encode freshly built log records | 0.84 |
| parse NDJSON line by line | 0.79–0.95 |
| stdlib `logging` formatter ([`examples/json_logging.py`](../examples/json_logging.py)) | 2.4 µs per record vs 5.2 µs with `json.dumps(default=str)` |

### Data pipelines and large files

Large files run each operation in a fresh subprocess, reporting time and peak RSS.

| file | op | rjson ÷ orjson time | peak RSS rjson / orjson |
|---|---|---|---|
| 97 MB records | `loads` (bytes) | 1.00 | **528 / 762 MB** |
| 97 MB records | `dumps` | 0.84 | 94 / 94 MB |
| 21 MB, 100k distinct keys | `dumps` | 0.75 (was 1.27) | 20.3 / 19.8 MB (was 35.8) |
| 99 MB floats | `loads` | 0.84 | **230 / 362 MB** |
| 42 MB ints | `loads` / `dumps` | 0.81 / 0.77 | 226 / 304 MB |
| 21 MB, 100k distinct keys | `loads` | 0.85 | 151 / 182 MB |

Integers beyond 64 bits round-trip exactly in both directions (orjson turns them into
floats on `loads` and rejects them on `dumps`).

### Cache and message queue payloads

| case | round trip (`dumps` + `loads`) |
|---|---|
| 1 KB blob | 0.82 |
| 8 KB blob | 0.82 |
| 48 KB blob | 0.85 |

[`examples/codec.py`](../examples/codec.py) shows a versioned bytes codec with typed
round trips (datetime, UUID, Decimal, Enum, dataclass) and Kafka serializer callables.

## Performance

The benchmark found a few production patterns where rjson was slower than orjson. The
status column reflects this branch.

| pattern | before | status |
|---|---|---|
| small `dumps` right after a large one | 1.7–2.0× slower | **fixed**: ~1.0–1.3× (see [Performance fixes](#performance-fixes)) |
| large `dumps` peak memory | 1.8× the output size, 16 MB kept, 1.27× slower | **fixed**: at orjson's peak, 0.5 MB kept, 0.75× |
| CJK / UCS-2 text `loads` | 1.3× slower (1.6× on 3.11) | **fixed**: 0.74× on the benchmark's CJK case; Chinese 0.59×, Korean 0.91×, Cyrillic 0.59× ([#8](https://github.com/TinDang97/rjson/issues/8)) |
| mixed-magnitude float arrays `loads` | 1.12–1.32× slower | **fixed**: 0.89–0.95× ([#9](https://github.com/TinDang97/rjson/issues/9)); arrays of mostly `0.0` ~1.05× (allocation-bound) |
| cold-cache tiny `dumps` | parity on 3.13, 1.29× on 3.11 | at the noise floor; PGO release wheels should cover it |

Memory side effects shared with orjson (the stdlib `json` has none of them), and what this
branch changed ([#10](https://github.com/TinDang97/rjson/issues/10)):

| side effect | before | now |
|---|---|---|
| `dumps` to bytes attaches a UTF-8 copy to each non-ASCII source `str` (+98% memory for its lifetime) | every non-ASCII string | only strings under 256 characters (keys, names, labels: cheap, and the copy makes repeated `dumps` fast); longer text is encoded directly, and fresh long text is faster than orjson (CJK 0.73–0.81×) |
| `loads(str)` attaches a UTF-8 copy to non-ASCII input (+132%) | every non-ASCII input | only inputs under 4,096 characters; larger ones use a temporary buffer (0.8 MB input: 0.81 MB kept → 0) |
| `loads(memoryview)` copies the input | always (peak 621 vs 528 MB on 97 MB) | not for views of a whole `bytes`/`bytearray` ≥ 4 KiB: peak equals `bytes` input |

The trade-off: serializing the *same* long non-ASCII strings repeatedly now re-encodes them
on every call (8–30× slower for those strings than reusing the attached copy); parsing the
same large `str` object repeatedly re-encodes it too. Fresh data, the common case, is
unaffected or faster. `dumps_str` never attached a copy.

### Performance fixes

Measured on CPython 3.13 (3.11 in brackets), same process as orjson, plain release builds.

**1. Output buffer sized from the smaller of the last two results.** Before, a small
response right after a big page allocated a buffer of the big page's size (~830 KB for a
150 B result), then copied the result out of it.

| case | before | after |
|---|---|---|
| 150 B `dumps` after a big page | 9.0–9.6 µs (10.9 µs) | 4.0–4.5 µs (4.0 µs) |
| `percall/small_after_big` vs orjson | 1.97× | 1.08–1.27× (noisy: ±40% IQR) |

**2. Large outputs grow without stranding the old buffer.**

- The first growth jumps to the largest recent output size.
- Beyond 1 MiB the buffer reserves 32 MiB of address space, which is always mmapped
  (untouched pages cost no RAM) and grows or shrinks without copying.
- The finished result is shrunk back, so the reservation is never kept.

| case | before | after |
|---|---|---|
| 21 MB output (100k distinct keys): peak RSS | 35.8 MB | **20.3 MB** (orjson 19.8) [20.3] |
| same: memory kept after the result is freed | 16.0 MB | **0.5 MB** [0.5] |
| same: first call vs orjson | 1.27× | **0.75×** [0.79×] |
| web `mixed_sizes` / `alternating_sizes` `dumps` | 0.86× / 0.87× | 0.69× / 0.71× |
| 700 KB then 150 B `dumps`, per pair | 810–822 µs | 778–807 µs (orjson 916–974 µs) |

In the last row the small call gets 0.6 µs slower (2.1 → 2.7 µs) while the big call gets
15–30 µs faster. The reference benchmark's geomeans were unchanged (−1.3% `dumps`, −0.2%
`dumps_str`, −2.5% `loads`; lower is better).

**3. UTF-8 → UCS-2 decoding with SIMD fast paths** ([#8](https://github.com/TinDang97/rjson/issues/8)).
Strings whose widest character is in U+0100–U+FFFF (CJK, kana, hangul, Cyrillic) were
decoded one character per loop iteration. Now 16- and 8-byte ASCII runs are widened at once,
runs of five 3-byte characters are converted with shuffles, and other 2- and 3-byte
characters are decoded two per iteration.

| text (`loads`, rjson ÷ orjson) | 3.13 before → after | 3.11 before → after |
|---|---|---|
| benchmark `cjk_strings` (Japanese) | 1.09 → **0.74** | 1.63 (issue) → **0.81–0.83** (micro) |
| Chinese | 1.08 → **0.59** | 1.06 → **0.61** |
| Korean | 1.00 → **0.91** | 1.08 → **0.90** |
| Cyrillic | 0.73 → **0.59** | 0.74 → **0.57** |
| mixed ASCII + CJK | 0.77 → **0.55** | 0.86 → **0.55** |

*Caveat:* on CPython 3.13 on the benchmark Xeon, text containing an astral character (UCS-4,
e.g. emoji) measured slower after this change (0.71 → 0.80 plain, 0.77 → ~1.0 PGO) although
that path is unchanged and executes the same instructions (callgrind). On 3.11 it got
faster (0.77 → 0.71). This looks like code placement (JCC-erratum-class effects) and is
tracked separately.

**4. Full-precision floats stay on the fast path** ([#9](https://github.com/TinDang97/rjson/issues/9)).
The number fast path read at most 15 fraction digits, but `repr()` of a typical double has
16–17 significant digits (`1.5180924662418203`, `0.008601898621952831`). Those numbers fell
through to the general parser, which reads one digit at a time. The fast path now reads up
to 19 fraction digits (19 significant digits, so the mantissa stays exact), with the same
correctly rounded conversion.

| floats (`loads`, rjson ÷ orjson) | 3.13 before → after | 3.11 before → after |
|---|---|---|
| mixed-magnitude doubles (the issue's case) | 1.10–1.16 → **0.95** | 1.14 → **0.95** |
| `random()` values in [0, 1) | 1.09–1.11 → **0.89** | 1.07–1.08 → **0.89** |
| canada, 6-decimal matrix, 4+3-digit values | unchanged (0.6–0.95) | unchanged |
| mostly `0.0` | ~1.05 (unchanged) | ~1.06 (unchanged) |

Arrays dominated by `0.0` cost the same per element as `1.5`: the time is the float object
allocation, which both libraries pay. A shared `0.0` object made them 0.89× but made
realistic mixed data 15% slower (and changes object identity), so it was not adopted.

*Caveat:* the 32 MiB reservation is address space, not RAM, but `tracemalloc` reports it
as the peak, and on Windows it counts against the commit charge while the call runs.

## Transfer size: compression and binary formats

Compression and binary formats are often suggested to "speed up JSON". Here is what they
actually do, measured on CPython 3.13 with 200k flat records (24.2 MB of JSON;
`rjson.dumps` 16.5 ms, `rjson.loads` 108 ms).

### Compression makes the payload smaller, not faster to produce

| codec | size | ratio | compress | decompress |
|---|---|---|---|---|
| lz4 | 4.1 MB | 5.9× | 22 ms | 7 ms |
| zstd level 3 | 1.15 MB | 21× | 29 ms | 9 ms |
| gzip level 6 | 2.4 MB | 10× | 112 ms | 30 ms |
| brotli level 4 | 0.9 MB | 27× | 213 ms | 21 ms |

Compression costs as much CPU as serialization, or more. It pays off only when the link is
slower than the codec:

- **zstd-3** saves 23 MB for 38 ms, so it wins below about 5 Gbit/s (internet, mobile,
  cross-region).
- **gzip-6** saves 21.8 MB for 142 ms, so it wins only below about 1.2 Gbit/s.
- **Inside a data centre on 10 GbE or faster**, compression makes requests slower. Use lz4
  or nothing.

**Small payloads: don't compress.**

| JSON size | zstd size | `rjson.dumps` | zstd compress + decompress |
|---|---|---|---|
| 89 B | 89 B (no gain) | 0.1 µs | 4.6 µs |
| 881 B | 172 B | 1.0 µs | 8.4 µs |
| 9 KB | 458 B | 4.7 µs | 15 µs |
| 94 KB | 3 KB | 46 µs | 92 µs |

A 1 KB HTTP response fits in one TCP packet, and the first round trip carries up to about
14 KB, so compressing it saves no latency. Compress from a few KB up, or when you pay per
stored byte at scale (Redis memory, Kafka retention). These synthetic records are
repetitive, so real data compresses less. For floods of tiny similar messages, zstd with
a trained dictionary is the right tool.

Compression belongs in the transport, not in the JSON library:

- **HTTP:** Starlette's `GZipMiddleware`, nginx or a CDN (browsers accept zstd and brotli).
- **Kafka:** `compression.type=zstd` on the producer.
- **Redis:** compress in the codec. [`examples/codec.py`](../examples/codec.py) supports
  `Codec(compress="zstd")` with a 1 KB threshold, a decompression-bomb limit, and
  compressed payloads recognized by the zstd frame magic, so mixed rollouts work.

`rjson.dumps(..., compress=...)` would add nothing over `zstd.compress(rjson.dumps(x))`.

### Binary formats don't help when the result is Python objects

Round trip of the same 200k records:

| format | size | encode | decode |
|---|---|---|---|
| **rjson (JSON)** | 24.2 MB | **16.5 ms** | **108 ms** |
| ormsgpack | 18.3 MB | 35 ms | 120 ms |
| msgpack | 18.3 MB | 86 ms | 146 ms |
| Arrow IPC, from and back to dicts | 13.4 MB | 134 ms (`from_pylist`) + 1.4 ms | 0.01 ms + 126 ms (`to_pylist`) |

Most of the decode time goes into creating Python objects (1.4 million of them here), not
into parsing bytes. Every format that ends in dicts pays that cost, so a binary format
only moves it around. Arrow is about twice as slow as JSON when you convert from and back
to dicts.

### Where Arrow wins: data that stays columnar

Reading 24 MB of NDJSON:

| reader | result | time |
|---|---|---|
| `rjson.loads` per line | `list[dict]` | 137 ms |
| `pyarrow.json.read_json` | Arrow table | 40 ms |
| `polars.read_ndjson` | DataFrame | 28 ms (then a group-by sum takes 2.2 ms) |

For analytics, reading into columns and never creating dicts is about 5× faster. Once the
data is in Arrow, passing it between processes costs almost nothing (reading an IPC
message: 0.01 ms).

### Which to use

| situation | use |
|---|---|
| API responses, events, cache values consumed as objects | rjson |
| the same, over slow or metered links, payloads of a few KB and up | rjson + zstd/brotli in the transport |
| analytics / ETL over many records | Polars or pyarrow readers; Arrow IPC or Parquet between stages |
| other languages must read it | JSON (universal), or Arrow for columnar data |

## Compatibility and migration

### Error contract (fixed on this branch)

| situation | before | now |
|---|---|---|
| `dumps` of an unsupported type, non-str key, NaN, too deep | `ValueError` (broke `except TypeError`) | `rjson.JSONEncodeError`, a subclass of both `TypeError` and `ValueError` |
| `loads` of invalid JSON | `json.JSONDecodeError`, `.msg` prefixed "JSON parsing error: " | `rjson.JSONDecodeError` (the same class), bare `.msg`, positions matching `json` |
| `loads` of a strided `memoryview` | `BufferError` | parsed |
| `rjson.JSONDecodeError`, `JSONEncodeError`, `__version__`, type stubs | missing | present (`rjson/__init__.pyi` + `py.typed`) |

### Behaviour differences you must plan for

| input | rjson | orjson | json |
|---|---|---|---|
| datetime, UUID, dataclass, plain `Enum` | `JSONEncodeError` | native | `TypeError` |
| `Decimal`, `set`, `bytes` | `JSONEncodeError` | `TypeError` | `TypeError` |
| non-str keys (`int`, `None`, …) | `JSONEncodeError` | `TypeError` (or `OPT_NON_STR_KEYS`) | coerced to str |
| NaN / Infinity in `dumps` | `JSONEncodeError` | `null` | `NaN` |
| `NaN` / BOM / `"\ud800"` in `loads` | `JSONDecodeError` | `JSONDecodeError` | accepted |
| ints ≥ 2^64 | exact | float on `loads`, error on `dumps` | exact |
| floats < 1e-4 | `1e-7` (same as orjson) | `1e-7` | `1e-07` |
| `indent`, `sort_keys`, `option=` | not supported | options | kwargs |

The full migration guide with code is in the [README](../README.md#migrating-from-json-or-orjson).

## Operational notes

- **Threads:** safe (20k concurrent `loads`+`dumps` calls on 8 threads, 0 mismatches), but
  calls serialize on the GIL, so there is no parallel speedup.
- **asyncio / uvloop:** works. A call blocks the loop for its duration (~0.7 ms per 100 KB
  in `loads`); for large payloads, stream NDJSON or use a process pool. See
  [ASYNC.md](ASYNC.md).
- **Free-threaded CPython (3.13t/3.14t) and subinterpreters:** not supported yet (the
  build refuses free-threaded Python; subinterpreter import raises `ImportError`).
- **Limits:** `dumps` nesting 254 (like orjson), `loads` nesting 1024.
- **Crash safety:** `panic = "abort"`, so a Rust panic kills the process. The code avoids
  `unwrap` on Python-derived data, and no crash or memory error came up in any test,
  fuzzing or benchmark run.
- **Platforms:** CPython 3.10–3.14 on Linux x86_64/aarch64, macOS arm64 and Windows (CI).
  x86_64 builds target x86-64-v2 and select AVX2/AVX-512 at runtime. PyPy and GraalPy are
  not supported.

## Adoption blockers, tracked

| blocker | impact | issue |
|---|---|---|
| no native datetime / UUID / dataclass / Enum | medium: `default=` covers them at one Python call per value; orjson needs none | [#5](https://github.com/TinDang97/rjson/issues/5) |
| no non-str dict keys option | medium | [#6](https://github.com/TinDang97/rjson/issues/6) |
| `loads` stricter than `json` (BOM, NaN, lone surrogates) | medium: new 4xx after migration | [#7](https://github.com/TinDang97/rjson/issues/7) |
| not on PyPI | high: needs a Rust toolchain to install | publish `pyrjson` |
| no free-threading / subinterpreter support | medium, growing with 3.14t adoption | [ASYNC.md roadmap](ASYNC.md#roadmap) |
| no `indent` / `sort_keys` | low for services, high for config/debug output | planned |

## Recommendation

1. **Adopt now** for services whose payloads are JSON-native: internal APIs, logging, event
   pipelines, cache codecs. Use the patterns in [`examples/`](../examples/), pin the
   version, and keep a fallback path for unsupported types.
2. **Wait for #5** if you are an orjson user who relies on native datetime/dataclass
   output and cannot afford a `default` call per value; `default=` itself works.
3. **Before 1.0:** publish `pyrjson` wheels, ship #5–#7, and add free-threading support.

## Reproducing

```bash
benches/fetch_corpus.sh
python benches/production_benchmark.py --quick                 # ~30 s smoke run
python benches/production_benchmark.py --output-json prod.json # full run, ~6 min
python benches/production_benchmark.py --big-only              # large-file time + RSS
python -m pytest tests -q                                      # 856 tests
```
