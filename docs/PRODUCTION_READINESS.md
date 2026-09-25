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

**Not yet a drop-in for orjson users who rely on its options.** There is no `default=`
hook, no native datetime/UUID/dataclass, and no non-str dict keys. You can work around
these today (see [`examples/`](../examples/)), and each is tracked as an issue.

| question | answer |
|---|---|
| Faster than `json`? | Yes: 4–20× on `dumps`, 1.3–5× on `loads`. |
| Faster than orjson? | Yes on most shapes: geomean `dumps` 0.76×, `loads` 0.87×, round trip 0.83× (rjson ÷ orjson time). The exceptions are listed under [Performance](#performance). |
| Correct? | 0 mismatches. 565 tests, fuzzing against `json`, and output byte-identical to orjson. |
| Memory? | Better on large `loads`: peak RSS 30–37% below orjson. Retained small results cost ~400 B instead of ~8 KB each. |
| Safe for async services? | Yes, but each call blocks the event loop, and `to_thread` doesn't help. See [ASYNC.md](ASYNC.md). |
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
| small `dumps` right after a large one | 1.7–2.0× slower | see [Performance fixes](#performance-fixes) |
| large `dumps` peak memory | up to 1.8× the output size, old buffer kept | see [Performance fixes](#performance-fixes) |
| CJK / UCS-2 text `loads` | 1.3× slower (1.6× on 3.11) | see [Performance fixes](#performance-fixes) |
| mixed-magnitude float arrays `loads` | 1.12–1.32× slower | see [Performance fixes](#performance-fixes) |
| cold-cache tiny `dumps` | parity on 3.13, 1.29× on 3.11 | at the noise floor; PGO release wheels should cover it |

Memory side effects shared with orjson (the stdlib `json` has none of them):

- `dumps` to bytes makes CPython attach a UTF-8 copy to each non-ASCII source `str` (+98%
  memory for those strings, for their lifetime). `dumps_str` does not.
- `loads(str)` on non-ASCII text attaches a UTF-8 copy to the input string. Pass `bytes`
  when you can.
- `loads(memoryview)` copies the input (peak 621 MB vs 528 MB from `bytes` on 97 MB).

### Performance fixes

*Filled in from the performance pass on this branch.*

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
| no `default=` hook | high: every unsupported value forces a Python pre-pass | [#4](https://github.com/TinDang97/rjson/issues/4) |
| no native datetime / UUID / dataclass / Enum | high for orjson users; `jsonable_encoder` costs ~100× `dumps` | [#5](https://github.com/TinDang97/rjson/issues/5) |
| no non-str dict keys option | medium | [#6](https://github.com/TinDang97/rjson/issues/6) |
| `loads` stricter than `json` (BOM, NaN, lone surrogates) | medium: new 4xx after migration | [#7](https://github.com/TinDang97/rjson/issues/7) |
| not on PyPI | high: needs a Rust toolchain to install | publish `pyrjson` |
| no free-threading / subinterpreter support | medium, growing with 3.14t adoption | [ASYNC.md roadmap](ASYNC.md#roadmap) |
| no `indent` / `sort_keys` | low for services, high for config/debug output | planned |

## Recommendation

1. **Adopt now** for services whose payloads are JSON-native: internal APIs, logging, event
   pipelines, cache codecs. Use the patterns in [`examples/`](../examples/), pin the
   version, and keep a fallback path for unsupported types.
2. **Wait for #4 and #5** if you are an orjson user who relies on `default=` or native
   datetime/dataclass output.
3. **Before 1.0:** publish `pyrjson` wheels, ship #4–#7, and add free-threading support.

## Reproducing

```bash
benches/fetch_corpus.sh
python benches/production_benchmark.py --quick                 # ~30 s smoke run
python benches/production_benchmark.py --output-json prod.json # full run, ~6 min
python benches/production_benchmark.py --big-only              # large-file time + RSS
python -m pytest tests -q                                      # 565 tests
```
