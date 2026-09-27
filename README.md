# rjson

[![PyPI](https://img.shields.io/pypi/v/pyrjson.svg)](https://pypi.org/project/pyrjson/)
[![CI](https://github.com/TinDang97/rjson/actions/workflows/ci.yml/badge.svg)](https://github.com/TinDang97/rjson/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/TinDang97/rjson/blob/main/LICENSE)
[![Python 3.10–3.14](https://img.shields.io/badge/python-3.10%20%7C%203.11%20%7C%203.12%20%7C%203.13%20%7C%203.14-blue.svg)](#compatibility)
[![Status: experimental](https://img.shields.io/badge/status-experimental-orange.svg)](#status)

**Fast JSON for Python, written in Rust directly against the CPython C API.** A faster
alternative to [orjson](https://github.com/ijl/orjson) and the standard library `json` for
web APIs (FastAPI and others), logging and NDJSON, caches and message queues; switching is
mostly a find-and-replace ([migration guide](#migrating-from-json-or-orjson)).

- **Faster than orjson** on 18 of the 20 cases of the reference benchmark (geomean `loads`
  1.17×, `dumps` 1.36×) and on 21 of 22 production-shaped workloads (1.52×); 3.5× / 14×
  faster than `json`.
- **Same output as orjson**, byte for byte, including `datetime`, `UUID`, dataclasses and
  `Enum`; `dumps_str` returns a `str` directly.
- **Hardened for untrusted input:** differential fuzzing against `json`, an
  AddressSanitizer CI build, and stack checks for deeply nested documents.
- CPython 3.10–3.14 wheels for Linux (glibc and musl), macOS and Windows. MIT licensed.

<img alt="Terminal: python benches/demo.py. rjson vs orjson with the same output: twitter.json loads 1.16x faster, twitter.json dumps 1.84x, github.json dumps 1.89x, 2k events with datetime and UUID 2.04x, twitter.json as str 1.77x." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/demo.svg" width="820">

If rjson saves you CPU time, a ⭐ on [GitHub](https://github.com/TinDang97/rjson) helps
other people find it.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/headline-dark.svg">
  <img alt="Geometric-mean speedups on CPython 3.13: loads 1.17× faster than orjson, dumps 1.36× faster than orjson, loads 3.53× and dumps 14.2× faster than the standard library json." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/headline-light.svg" width="880">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/vs-orjson-dark.svg">
  <img alt="Per-case speed relative to orjson. loads: 1.09× to 1.33× faster on nine cases, 0.95× on citm_catalog. dumps: 1.03× to 3.19× faster on nine cases, 0.93× on the float array." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/vs-orjson-light.svg" width="880">
</picture>

<details>
<summary>Numbers behind the charts, and how they were measured</summary>

Speedup = other library's time ÷ rjson's time (**higher is better**). Median time per call
over 5 runs, CPython 3.13.12, orjson 3.12.0, x86_64 (Xeon), PGO build
(`scripts/build_pgo.sh`, as the published wheels are built). `loads` parses the documents'
UTF-8 `bytes`; `dumps` returns `bytes` in both rjson and orjson. Raw results, with each
case's per-run range: [docs/img/benchmark-results.json](https://github.com/TinDang97/rjson/blob/main/docs/img/benchmark-results.json).

| case | loads vs orjson | dumps vs orjson | loads vs json | dumps vs json |
|---|---|---|---|---|
| twitter.json | 1.20× | 1.71× | 3.23× | 12.9× |
| citm_catalog.json | 0.95× | 1.12× | 2.23× | 6.06× |
| canada.json | 1.09× | 1.10× | 4.34× | 17.7× |
| github.json | 1.33× | 1.89× | 2.98× | 18.0× |
| small dict | 1.27× | 1.27× | 6.37× | 18.5× |
| records | 1.12× | 1.34× | 2.17× | 11.8× |
| unicode strings | 1.12× | 1.05× | 1.80× | 54.0× |
| escaped strings | 1.22× | 3.19× | 5.14× | 5.38× |
| int array | 1.16× | 1.03× | 3.33× | 11.1× |
| float array | 1.26× | 0.93× | 7.53× | 18.6× |
| **geomean** | **1.17×** | **1.36×** | **3.53×** | **14.2×** |

The two cases below 1× are within this host's run-to-run noise (citm `loads` ranged
0.87–1.17×, float array `dumps` 0.88–1.16× over the 5 runs).

Reproduce and redraw:

```bash
benches/fetch_corpus.sh                                   # sha256-pinned corpora -> benches/data/
scripts/build_pgo.sh python3.13 && pip install target/wheels/*cp313*.whl   # or a plain build
python benches/corpus_benchmark.py --json --repeat 11 --output-json results.json
python benches/make_charts.py results.json                # -> docs/img/*.svg + this table
```

`dumps_str` (returns `str`) is faster than orjson's `dumps` on 6 of 10 cases (geomean 1.13×);
it trails on twitter.json and unicode strings, because a `str` holding emoji must be stored
at 4 bytes per character, and is at parity on citm_catalog and the float array (1.01, 1.05). `loads` of the same large non-ASCII `str` object over and
over is 1.1–1.5× slower than orjson (twitter, citm, unicode strings): orjson keeps a UTF-8
copy attached to that string, which rjson deliberately does not (it would double the string's
memory, [#10](https://github.com/TinDang97/rjson/issues/10)); with `bytes` or a new `str` per
call, as a server gets, rjson is faster. In plain (non-PGO) builds single cases move by up
to 1.5× with code layout (canada `dumps`), which PGO removes.
Methodology, per-version results and the roadmap:
[docs/PERFORMANCE_REVIEW.md](https://github.com/TinDang97/rjson/blob/main/docs/PERFORMANCE_REVIEW.md).

</details>

### On production-shaped workloads

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/showcase-dark.svg">
  <img alt="Speed relative to orjson on 22 production-shaped workloads: rjson faster in 21, geomean 1.52×. Application types 1.09× to 3.17× (UTC datetimes), output as str 1.65× to 1.99×, web API 1.24× to 1.78×, escaped strings 1.40× and 1.56× with per-record NDJSON at 0.92×, standard corpora 1.09× to 1.82×." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/showcase-light.svg" width="880">
</picture>

Identical output is checked before timing. FastAPI, logging, NDJSON and Redis/Kafka numbers for the
[examples](https://github.com/TinDang97/rjson/tree/main/examples):
[Faster JSON in Python services](https://github.com/TinDang97/rjson/blob/main/docs/guides/faster-json-in-python-services.md).
Numbers, method and the one slower case:
[docs/SHOWCASE.md](https://github.com/TinDang97/rjson/blob/main/docs/SHOWCASE.md) (`python benches/showcase.py`).

## Installation

The PyPI distribution is **`pyrjson`** (the name `rjson` on PyPI belongs to an unrelated
project). You still `import rjson`.

```bash
pip install pyrjson        # or: uv add pyrjson
```

Wheels are PGO-optimized builds for CPython 3.10–3.14 on Linux (manylinux and musllinux,
x86_64 and aarch64), macOS (arm64 and x86_64) and Windows (x86_64), with build provenance
attestations (`gh attestation verify <wheel> --repo TinDang97/rjson`). Elsewhere pip builds
from the sdist, which needs a Rust toolchain. The development version:

```bash
pip install "git+https://github.com/TinDang97/rjson"
# or, from a checkout:
pip install maturin && maturin develop --release
```

## Usage

```python
from datetime import datetime
from decimal import Decimal
from uuid import uuid4

import rjson

data = rjson.loads('{"name": "rjson", "tags": ["fast", "safe"], "stars": 1e3}')
payload = rjson.dumps(data)        # b'{"name":"rjson","tags":["fast","safe"],"stars":1000.0}'
text = rjson.dumps_str(data)       # the same JSON as a str

rjson.dumps({"at": datetime(2024, 5, 1, 9, 30), "id": uuid4()})  # datetime, UUID, dataclass, Enum: native
rjson.dumps({"price": Decimal("9.99")}, default=str)             # convert the rest with default=
```

| name | kind | notes |
|---|---|---|
| `loads(data, *, lenient=False)` | function → object | `data`: `str`, `bytes`, `bytearray` or `memoryview` (any layout) |
| `dumps(obj, *, default=None, passthrough=0, non_str_keys=False, indent=None, sort_keys=False)` | function → `bytes` | UTF-8 JSON, like `orjson.dumps` |
| `dumps_str(obj, *, default=None, passthrough=0, non_str_keys=False, indent=None, sort_keys=False)` | function → `str` | like `json.dumps(obj, ensure_ascii=False, separators=(",", ":"))` |
| `dumps_bytes(...)` | function → `bytes` | alias of `dumps`, kept for compatibility |
| `PASSTHROUGH_DATETIME`, `_UUID`, `_DATACLASS`, `_ENUM` | `int` flags | for `passthrough=`, combine with `\|` |
| `JSONDecodeError` | exception | `json.JSONDecodeError` itself (a `ValueError`) |
| `JSONEncodeError` | exception | subclass of **both** `TypeError` and `ValueError`, like `orjson.JSONEncodeError` |
| `__version__` | `str` | package version |

The wheel ships type stubs (`py.typed`), so mypy and pyright check calls to rjson.

- **`lenient=True` (`loads`):** accepts everything `json.loads` accepts, with the same
  result: `NaN`/`Infinity`/`-Infinity`, a UTF-8 BOM on bytes and numbers overflowing to
  `inf` are parsed natively (no speed cost); lone surrogates (`"\ud83d"`, sent by
  JavaScript clients that cut an emoji in half), UTF-16/UTF-32 bytes and nesting deeper
  than 1024 go through `json.loads`. The default stays strict, like orjson.
- **Types:** `dict` (str keys), `list`, `tuple`, `str`, `int` (any size), `float`, `bool`,
  `None`, and their subclasses (`IntEnum`, `str` enums, `OrderedDict`, `namedtuple`, …).
- **Native types, byte-identical to orjson:** `datetime` (RFC 3339: `2024-05-01T09:30:00`,
  `.ffffff` only when non-zero, `+HH:MM` when aware), `date`, `time`, `uuid.UUID`
  (lowercase, hyphenated), dataclasses (as objects; `_`-prefixed names skipped, as in
  orjson) and `Enum` members (their value). Subclasses of `datetime`/`date`/`time`/`UUID`
  go to `default=`. Three orjson bugs are not copied: a `tzinfo` whose `utcoffset()`
  raises, or a deleted `__slots__` field, raise the Python error (orjson crashes);
  `utcoffset()` returning `None` gives no offset, as in `isoformat()` (orjson writes
  `+00:00`); an offset whose seconds round up to a whole hour carries into the hour
  (orjson writes `+00:60`).
- **`passthrough=`:** `rjson.PASSTHROUGH_*` flags send those kinds to `default=` (or make
  them raise) instead, e.g. to tag them for an exact round trip
  ([`examples/codec.py`](https://github.com/TinDang97/rjson/blob/main/examples/codec.py)), like orjson's `OPT_PASSTHROUGH_*`.
- **`non_str_keys=True`:** dict keys may be `int` (any size), `float`, `bool`, `None`
  (written exactly as `json.dumps` writes them: `"1"`, `"1e-07"`, `"NaN"`, `"true"`,
  `"null"`), `Enum` members (their value), and `datetime`/`date`/`time`/`UUID` (their
  native text, like orjson's `OPT_NON_STR_KEYS`). Other key types raise; `default=` is not
  called for keys. Like `json` and orjson, a coerced key can duplicate a str key
  (`{1: …, "1": …}` writes `"1"` twice). Off by default: a non-str key raises.
- **`indent=` / `sort_keys=`:** `indent=2` and `sort_keys=True` give exactly the bytes of
  orjson's `OPT_INDENT_2` and `OPT_SORT_KEYS` (keys sorted by code point; dataclass fields
  keep their order); other widths (`indent=4`, `indent=0`) lay out like
  `json.dumps(indent=n)`. Compact calls pay nothing for them; the options themselves are
  not yet as fast as orjson's (twitter.json: `sort_keys` 0.78×, `indent=2` 0.48×). With
  either option, `dumps_str` raises `UnicodeEncodeError` on a lone surrogate, as `dumps`
  does.
- **`default=`:** called with each value rjson cannot serialize; its return value is
  serialized in its place (and passed to `default` again if still unsupported), as in
  `json.dumps` and orjson. Exceptions it raises propagate unchanged. It is not called for
  NaN/Infinity or dict keys (see `non_str_keys=`), which still raise. Supported values never reach it, so
  native data runs at full speed.
- **Errors:** `loads` raises `JSONDecodeError` with `pos`/`lineno`/`colno` matching `json`.
  `dumps`/`dumps_str` raise `JSONEncodeError` for unsupported types, non-str keys (unless
  `non_str_keys=True`),
  NaN/Infinity, and nesting deeper than 254 (which also catches circular references), so
  both `except TypeError` and `except ValueError` catch it. A lone surrogate raises
  `UnicodeEncodeError` in `dumps`; `dumps_str` passes it through.
- **Precision:** floats round-trip exactly (shortest representation, e.g. `1e+16`), and
  integers of any size are exact in both directions. `loads` accepts nesting up to 1024.

## Migrating from `json` or orjson

Most code migrates with a find-and-replace:

| you wrote | with rjson |
|---|---|
| `json.loads(s)` / `orjson.loads(s)` | `rjson.loads(s)` |
| `orjson.dumps(obj)` | `rjson.dumps(obj)` (same bytes) |
| `json.dumps(obj, separators=(",", ":"), ensure_ascii=False)` | `rjson.dumps_str(obj)` |
| `json.dumps(obj).encode()` | `rjson.dumps(obj)` |
| `except json.JSONDecodeError` / `orjson.JSONDecodeError` | `except rjson.JSONDecodeError` |
| `except TypeError` / `orjson.JSONEncodeError` around `dumps` | `except rjson.JSONEncodeError` (`TypeError` keeps working) |
| `json.dumps(obj, default=f)` / `orjson.dumps(obj, default=f)` | `rjson.dumps(obj, default=f)` |
| `orjson.dumps(datetime/UUID/dataclass/Enum)` | the same bytes |
| `json.dumps({1: "a", None: "b"})` / `orjson.dumps(obj, option=OPT_NON_STR_KEYS)` | `rjson.dumps(obj, non_str_keys=True)` (key text as `json` writes it) |
| `orjson.dumps(obj, option=OPT_INDENT_2 \| OPT_SORT_KEYS)` | `rjson.dumps(obj, indent=2, sort_keys=True)` (same bytes) |
| `json.dumps(obj, indent=4, sort_keys=True)` | `rjson.dumps_str(obj, indent=4, sort_keys=True)` (non-ASCII kept, like `ensure_ascii=False`) |
| `orjson.dumps(obj, option=OPT_PASSTHROUGH_DATETIME)` | `rjson.dumps(obj, passthrough=rjson.PASSTHROUGH_DATETIME)` (also `_UUID`, `_DATACLASS`, `_ENUM`) |
| `json.dumps(obj, default=lambda o: o.isoformat())` for datetimes | `rjson.dumps(obj)` (the same text, except UTC offsets with a seconds part, which are rounded to the minute) |

What does **not** carry over yet, and how to handle it:

| feature | status | workaround |
|---|---|---|
| NaN / Infinity | by design | `dumps` raises (`json` writes `NaN`, orjson `null`) |
| floats below 1e-4 | by design | `1e-7`, same as orjson; `json` writes `1e-07` (same value, different bytes) |

### FastAPI

```python
from fastapi import FastAPI
from fastapi.responses import JSONResponse
import rjson

class RJSONResponse(JSONResponse):
    def render(self, content) -> bytes:
        return rjson.dumps(content)

app = FastAPI()

@app.get("/items")
def items() -> RJSONResponse:
    return RJSONResponse([{"id": 1, "name": "widget"}])
```

Return `RJSONResponse(...)` directly for dicts and lists. Measured on whole requests
through the app, against stock FastAPI's own fast path (a return type, serialized by
Pydantic's `dump_json`): a 50-row page is 1.25–1.4× faster, 1.8–1.9× with UUID/datetime/Enum
fields, and a 1,000-row export 3.0–3.2× faster; against an endpoint without a return type,
9–74× ([numbers](https://github.com/TinDang97/rjson/blob/main/examples/README.md#measured-gains)). Don't set it as
`default_response_class`: endpoints that return Pydantic models already go through
`dump_json`. [`examples/fastapi_app.py`](https://github.com/TinDang97/rjson/blob/main/examples/fastapi_app.py) adds a fallback for
Decimal, Pydantic models and dataclasses, and rjson-parsed request bodies.

### Logging, NDJSON, Redis and Kafka

- [`examples/json_logging.py`](https://github.com/TinDang97/rjson/blob/main/examples/json_logging.py): a `logging.Formatter` writing one
  JSON object per line (2.2–2.5 µs per record, 2.4–2.7× faster than the same formatter on
  `json`), plus NDJSON read/write (writing 12.5× faster, reading 2.5–2.8×).
- [`examples/codec.py`](https://github.com/TinDang97/rjson/blob/main/examples/codec.py): a versioned bytes codec for Redis/Kafka with
  typed round trips, and `value_serializer`/`value_deserializer` callables. Round trips of
  JSON-native payloads are 4.1–4.6× faster than the same codec on `json`, and faster than
  `pickle`.
- [`docs/ASYNC.md`](https://github.com/TinDang97/rjson/blob/main/docs/ASYNC.md): aiohttp, httpx, asyncpg, `redis.asyncio` and aiokafka
  one-liners.

## Production use

The full report, with every number and how it was measured:
[docs/PRODUCTION_READINESS.md](https://github.com/TinDang97/rjson/blob/main/docs/PRODUCTION_READINESS.md).
In short, rjson is ready for services whose payloads are JSON-native (dicts, lists,
strings, numbers), and not yet a drop-in for code that relies on orjson's options.

<details>
<summary>FAQ</summary>

**Is it faster than orjson in real services, not just benchmarks?**
On our production-shaped suite (`benches/production_benchmark.py`: REST pages, request
bodies, NDJSON logs, 100 MB files, cache blobs) rjson is faster on most shapes (geomean
`dumps` 0.67×, `loads` 0.83× of orjson's time, PGO wheels) and uses 30–37% less peak
memory on large `loads`. The report lists the few shapes where it is not.

**Is it safe?**
It checks the exact type of every value, reserves the worst-case output size before
writing, and version-gates every CPython internal it uses, with self-tests at import.
Untrusted input is bounded: nesting limits plus a thread-stack check (deep documents raise
`RecursionError` instead of overflowing a small thread stack), and CPython's integer digit
limit. CI runs the tests, differential fuzzing against `json` and thread/re-entrancy
stress tests, also on an AddressSanitizer build. It is still 0.x: pin the version.
Report vulnerabilities privately: [SECURITY.md](https://github.com/TinDang97/rjson/blob/main/SECURITY.md).

**Does it work with asyncio / uvloop?**
Yes. Calls are synchronous and hold the GIL, so a large payload blocks the event loop, and
`asyncio.to_thread` does not help. [docs/ASYNC.md](https://github.com/TinDang97/rjson/blob/main/docs/ASYNC.md) shows what to do instead.

**Is it thread-safe?**
Yes, on regular (GIL) CPython builds, including calls re-entered from `default=` and
threads switching mid-call (stress-tested). Threads do not parallelize JSON work.
Free-threaded builds (3.13t/3.14t) and subinterpreters are not supported yet. In threads
with small stacks (musl/Alpine's 128 KiB default, `threading.stack_size`), very deep
documents raise `RecursionError`: about 975 nested arrays or 620 nested objects fit in
128 KiB, the full 1024 levels in 256 KiB.

**Why does `dumps` return `bytes`?**
It is what you send over the network or write to a file, and it avoids a copy. Use
`dumps_str` when you need a `str`.

**Should I compress, or switch to msgpack / Arrow to go faster?**
Compress in the transport (HTTP middleware, Kafka `compression.type`), and only payloads
of a few KB and up: compression costs as much CPU as serialization. Binary formats aren't
faster when the result is Python dicts, because creating objects dominates. Arrow and
Polars win when data stays columnar. Numbers:
[Transfer size](https://github.com/TinDang97/rjson/blob/main/docs/PRODUCTION_READINESS.md#transfer-size-compression-and-binary-formats).

**Why is `dumps_str` sometimes slower than `dumps`?**
A `str` containing emoji must store every character in 4 bytes, and CJK text in 2, while
UTF-8 `bytes` stay compact.

**Why does my previously accepted request now fail with a 422?**
By default `loads` rejects a UTF-8 BOM, `NaN`/`Infinity` literals and escaped lone
surrogates, like orjson does (the stdlib accepts them). Pass `lenient=True` to accept
exactly what `json.loads` accepts.

</details>

## Compatibility

- **CPython 3.10–3.14** on Linux x86_64/aarch64, macOS arm64 and Windows (CI). x86_64
  builds target x86-64-v2 and select AVX2/AVX-512 kernels at runtime.
- **Not supported:** PyPy, GraalPy, free-threaded (no-GIL) builds, subinterpreters.

### Status

Experimental. The API may change before 1.0; pin the version you test against.

## Why it is faster than orjson

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/architecture-dark.svg">
  <img alt="Design differences from orjson 3.12. loads: orjson parses into a yyjson tree, then converts the tree to Python objects in a second pass; rjson builds Python objects in one pass (1.09–1.29× faster, 30–37% less peak memory on large files). dumps: orjson starts from a 4 KiB buffer that doubles, escapes with SSE2 or an AVX-512 build and returns bytes; rjson sizes the buffer from recent calls, escapes with AVX-512, AVX2 or SSE2 and writes bytes or str directly (1.12–1.82× faster, as str 1.65–1.99×). Native types: orjson probes three attributes and calls utcoffset() per aware datetime; rjson caches the timezone offset and per-class facts (UTC datetimes 3.17×, datetime/UUID/Enum records 2.34×, slots dataclasses 2.14×)." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/architecture-light.svg" width="880">
</picture>

Both are Rust on the CPython C API, cache dict keys and format floats with zmij.
Internals rjson relies on are version-gated and self-tested at import (`CLAUDE.md`);
details in [docs/PERFORMANCE_REVIEW.md](https://github.com/TinDang97/rjson/blob/main/docs/PERFORMANCE_REVIEW.md).

## Contributing

Bug reports and pull requests are welcome. [CONTRIBUTING.md](https://github.com/TinDang97/rjson/blob/main/CONTRIBUTING.md) covers the
development setup, the hard rules for code that touches the C API, and how to benchmark.
Security issues: [SECURITY.md](https://github.com/TinDang97/rjson/blob/main/SECURITY.md). Changes: [CHANGELOG.md](https://github.com/TinDang97/rjson/blob/main/CHANGELOG.md).

```bash
uv venv .venv -p 3.13 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release && python -m pytest tests -q
```

| path | contents |
|---|---|
| `src/parser.rs`, `src/lemire.rs` | `loads` |
| `src/ser.rs` | `dumps` / `dumps_str` |
| `src/entry.rs`, `src/compat.rs` | C-API entry points, version-portable helpers |
| `rjson.pyi` | type stubs |
| `tests/` | pytest suites |
| `examples/` | FastAPI, logging/NDJSON and Redis/Kafka integrations (tested) |
| `benches/` | `corpus_benchmark.py` (reference), `showcase.py` (vs orjson on production shapes), `production_benchmark.py` (production workloads), `examples_benchmark.py` (the examples on rjson vs json vs orjson), `perf_gate.py`, `make_charts.py` / `make_showcase_charts.py` / `make_demo_svg.py` (README charts, demo), `demo.py` |
| `docs/` | showcase, performance review, production readiness report, async guide |

<details>
<summary>Troubleshooting source builds</summary>

- **Linker errors** (`symbol(s) not found for architecture arm64`): Python and Rust must
  target the same architecture. Check with `python3 -c "import platform; print(platform.machine())"`;
  on Apple Silicon use an arm64 Python such as `/opt/homebrew/bin/python3`, then
  `cargo clean` and rebuild.
- **Missing Python headers:** install your Python's development package (e.g. `python3-dev`).

</details>

## Roadmap

- Streaming decoder/encoder for async I/O; free-threading and subinterpreter support ([docs/ASYNC.md](https://github.com/TinDang97/rjson/blob/main/docs/ASYNC.md#roadmap))
- Performance: NEON kernels for aarch64

## License

[MIT](https://github.com/TinDang97/rjson/blob/main/LICENSE) © 2025 Tin Dang. `src/lemire.rs` is adapted from
[fast-float](https://github.com/aldanor/fast-float-rust) (MIT OR Apache-2.0).
