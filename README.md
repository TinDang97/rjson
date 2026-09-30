# rjson

[![PyPI](https://img.shields.io/pypi/v/pyrjson.svg)](https://pypi.org/project/pyrjson/)
[![CI](https://github.com/TinDang97/rjson/actions/workflows/ci.yml/badge.svg)](https://github.com/TinDang97/rjson/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/TinDang97/rjson/blob/main/LICENSE)
[![Python 3.10–3.14](https://img.shields.io/badge/python-3.10%20%7C%203.11%20%7C%203.12%20%7C%203.13%20%7C%203.14-blue.svg)](#compatibility)
[![Status: experimental](https://img.shields.io/badge/status-experimental-orange.svg)](#status)

**Fast JSON for Python, written in Rust directly against the CPython C API.**
rjson is a drop-in replacement for [orjson](https://github.com/ijl/orjson) and the standard
library `json`: orjson's exact output, `json`'s errors and options, and less CPU for every
request, log line and message you handle.

| | vs orjson | vs `json` |
|---|---|---|
| `loads` (parse) | **1.29× faster** | **3.96× faster** |
| `dumps` (serialize) | **1.46× faster** | **15.4× faster** |
| NDJSON / JSON Lines | **1.4–2.6× faster** (`loads_ndjson`) | **2.3–9.3× faster** |
| compatibility | byte-identical output to `orjson.dumps` | `json.JSONDecodeError`, every `json.dumps` option |

<sub>Geometric mean over the 10 cases of the [reference benchmark](#benchmarks), CPython 3.13.12,
orjson 3.12.0, PGO wheels as published. NDJSON: 60–700-byte lines, CPython 3.11–3.13.</sub>

## Quick start

```bash
pip install pyrjson          # the PyPI name; you still `import rjson`
```

```python
import rjson
from datetime import datetime
from uuid import uuid4

data = rjson.loads(b'{"name": "rjson", "tags": ["fast", "safe"]}')
payload = rjson.dumps({"at": datetime(2024, 5, 1, 9, 30), "id": uuid4(), **data})  # -> bytes
text = rjson.dumps_str(data)                  # -> str, without a .decode() copy
events = rjson.loads_ndjson(b'{"a": 1}\n{"a": 2}\n')   # -> [{'a': 1}, {'a': 2}]
```

Coming from orjson or `json`? Most code switches with a find-and-replace:
[migration guide](#migrating-from-json-or-orjson).

## Why rjson

- **Faster where services spend time.** Record-shaped data (API pages, DB rows, events) is
  where rjson leads most: `loads` of records is 1.44× faster than orjson, `dumps`
  1.64×. Datetimes, UUIDs and Enums serialize 2.2–3.2× faster than orjson, with no `default=`.
- **Drop-in, verified.** `dumps` output is byte-identical to orjson's, including `datetime`,
  `UUID`, dataclasses and `Enum`. Errors are `json.JSONDecodeError` with `json`'s positions.
  Every `json.dumps` option (`indent`, `separators`, `sort_keys`, `ensure_ascii`,
  `allow_nan`) has an exact equivalent, and `lenient=True` accepts exactly what `json.loads`
  accepts.
- **More than a parser.** `loads_ndjson` for NDJSON and JSON Lines, `dumps_str` for code
  that needs a `str`, and an `rjson` command line that replaces `python -m json.tool`
  (same output, 2–10× faster on real files).
- **Hardened for untrusted input.** Differential fuzzing against `json`, an
  AddressSanitizer CI job, nesting and thread-stack limits, and every CPython internal it
  uses gated by version and self-tested at import.
- **Ready to install.** PGO-optimized wheels for CPython 3.10–3.14 on Linux (glibc and
  musl, x86_64 and aarch64), macOS and Windows, with build provenance attestations.
  MIT licensed.

<img alt="Terminal: python benches/demo.py. rjson vs orjson with the same output: twitter.json loads 1.39x faster, twitter.json dumps 1.69x, github.json dumps 1.90x, 2k events with datetime and UUID 2.11x, twitter.json as str 1.77x." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/demo.svg" width="820">

If rjson saves you CPU time, a ⭐ on [GitHub](https://github.com/TinDang97/rjson) helps
other people find it.

## rjson, orjson and `json` compared

| | `json` (stdlib) | orjson | **rjson** |
|---|---|---|---|
| `loads` / `dumps` speed vs `json` | 1× / 1× | 3.07× / 10.6× | **3.96× / 15.4×** |
| `dumps` returns | `str` | `bytes` | `bytes`, or `str` with `dumps_str` (no decode copy) |
| `datetime`, `UUID`, dataclass, `Enum` | via `default=` | native | native, byte-identical to orjson |
| NDJSON / JSON Lines in one call | no | no | `loads_ndjson` |
| `indent`, `separators`, `sort_keys`, `ensure_ascii`, `allow_nan` | all | `indent=2` and `sort_keys` only | all, with `json`'s output |
| accepts `NaN`, a BOM, lone surrogates | yes | no | with `lenient=True` |
| non-`str` dict keys | int, float, bool, None | `OPT_NON_STR_KEYS` | `non_str_keys=True` |
| decode errors | `JSONDecodeError` | `JSONDecodeError` (subclass) | `json.JSONDecodeError` itself, `json`'s positions |
| command-line formatter | `python -m json.tool` | no | `rjson`: same output, 2–10× faster |
| type stubs | yes | yes | yes |

## What's new in 0.4.1

- **Non-ASCII text parses up to 1.8× faster.** `loads` decodes accented, Cyrillic, Greek,
  CJK and emoji strings 16 bytes per step with SIMD instead of one character at a time:
  accented Latin text 0.46× of orjson's time (was 0.65×), Cyrillic 0.56× (0.74×),
  hangul 0.70× (0.88×), emoji-only strings 0.68× (1.10×, slower than orjson before).
- **Reproducible wheels.** Two builds of the same commit now produce the same binary, so a
  release's speed no longer depends on the build: before, single cases varied by up to 20%.

0.4.0 added a shape cache (records up to 40% faster to parse) and `rjson.loads_ndjson`
(NDJSON 1.4–2.6× faster than a per-line loop on orjson).

Full list: [CHANGELOG.md](https://github.com/TinDang97/rjson/blob/main/CHANGELOG.md).

## Benchmarks

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/headline-dark.svg">
  <img alt="Geometric-mean speedups on CPython 3.13.12: loads 1.29× faster than orjson, dumps 1.46× faster than orjson, loads 3.96× and dumps 15.4× faster than the standard library json." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/headline-light.svg" width="880">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/vs-orjson-dark.svg">
  <img alt="Per-case speed relative to orjson. loads: 1.12× to 1.66× faster on 9 cases, 0.98× on float array. dumps: 1.06× to 2.89× faster on 10 cases." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/vs-orjson-light.svg" width="880">
</picture>

rjson is faster than orjson on 19 of the 20 `loads`/`dumps` cases. The other one is at parity within this host's run-to-run noise: float array `loads` (0.98×; per-run 0.94–1.40×).

<details>
<summary>All numbers, and how they were measured</summary>

Speedup = the other library's time ÷ rjson's time (**higher is better**). Median time per
call over 5 runs, CPython 3.13.12, orjson 3.12.0, x86_64 (Xeon, 4 cores), PGO build
(`scripts/build_pgo.sh`, as the published wheels are built). `loads` parses the documents'
UTF-8 `bytes`; `dumps` returns `bytes` in both rjson and orjson. Raw results, with each
case's per-run range: [docs/img/benchmark-results.json](https://github.com/TinDang97/rjson/blob/main/docs/img/benchmark-results.json).

| case | loads vs orjson | dumps vs orjson | loads vs json | dumps vs json |
|---|---|---|---|---|
| twitter.json | 1.35× | 1.52× | 3.49× | 13.0× |
| citm_catalog.json | 1.12× | 1.37× | 2.59× | 10.6× |
| canada.json | 1.13× | 1.26× | 4.59× | 19.5× |
| github.json | 1.50× | 1.72× | 3.21× | 17.2× |
| small dict | 1.58× | 1.37× | 7.34× | 18.5× |
| records | 1.44× | 1.64× | 2.72× | 14.1× |
| unicode strings | 1.66× | 1.06× | 2.98× | 46.4× |
| escaped strings | 1.15× | 2.89× | 4.99× | 5.80× |
| int array | 1.14× | 1.12× | 3.47× | 12.8× |
| float array | 0.98× | 1.27× | 6.83× | 18.5× |
| **geomean** | **1.29×** | **1.46×** | **3.96×** | **15.4×** |

Reproduce and redraw:

```bash
benches/fetch_corpus.sh                                   # sha256-pinned corpora -> benches/data/
scripts/build_pgo.sh python3.13 && pip install target/wheels/*cp313*.whl   # or a plain build
python benches/corpus_benchmark.py --json --repeat 11 --output-json results.json
python benches/make_charts.py results.json                # -> docs/img/*.svg + this table
```

- **`dumps_str`** (returns `str`) is faster than orjson's `dumps` + `.decode()` on real
  documents (1.71–1.91× in the [showcase](https://github.com/TinDang97/rjson/blob/main/docs/SHOWCASE.md)). Against
  orjson's `bytes` alone it trails on emoji-heavy text, because a `str` holding emoji stores
  4 bytes per character.
- **`loads` of the same large non-ASCII `str` object, over and over,** is slower than
  orjson: orjson keeps a UTF-8 copy attached to that string, which rjson deliberately does
  not (it would double the string's memory,
  [#10](https://github.com/TinDang97/rjson/issues/10)). With `bytes`, or a new `str` per
  call as a server gets, rjson is faster.
- Methodology, per-version results and the roadmap:
  [docs/PERFORMANCE_REVIEW.md](https://github.com/TinDang97/rjson/blob/main/docs/PERFORMANCE_REVIEW.md).

</details>

### NDJSON / JSON Lines

rjson ÷ orjson time (lower is better), each library's per-line loop vs `rjson.loads_ndjson`
(plain release builds, best of 15):

| NDJSON input | `rjson.loads` per line | `rjson.loads_ndjson` |
|---|---|---|
| log lines, 200 B × 50k | 0.70–0.75 | **0.44–0.47** |
| events, 60 B × 100k | 0.72–0.76 | **0.39–0.41** |
| API records, 700 B × 8k | 0.44–0.82 | **0.41–0.70** |
| mixed shapes × 60k | 0.76–0.83 | **0.46–0.52** |

Ranges span CPython 3.11, 3.12 and 3.13. Details:
[PERFORMANCE_REVIEW.md, loads item 13](https://github.com/TinDang97/rjson/blob/main/docs/PERFORMANCE_REVIEW.md).

### On production-shaped workloads

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/showcase-dark.svg">
  <img alt="Speed relative to orjson on 22 production-shaped workloads: rjson faster in 20 in every run, canada.json loads faster on the median and per-record NDJSON at parity, geomean 1.60×. Application types 1.20× to 3.18× (UTC datetimes), output as str 1.71× to 1.91×, web API 1.21× to 1.80×, escaped strings 1.63× and 1.80× with per-record NDJSON at 0.99×, standard corpora 1.08× to 1.70×." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/showcase-light.svg" width="880">
</picture>

API responses and request bodies, log records, application types and `str` output, each
checked for identical output before timing:
[docs/SHOWCASE.md](https://github.com/TinDang97/rjson/blob/main/docs/SHOWCASE.md) (`python benches/showcase.py`). The
examples below, measured end to end:
[Faster JSON in Python services](https://github.com/TinDang97/rjson/blob/main/docs/guides/faster-json-in-python-services.md).

## Use it in your stack

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

Return `RJSONResponse(...)` directly for dicts and lists. Measured on whole requests through
the app, against stock FastAPI's own fast path (a return type, serialized by Pydantic's
`dump_json`): a 50-row page is 1.25–1.4× faster, 1.8–1.9× with UUID/datetime/Enum fields,
and a 1,000-row export 3.0–3.2× faster; against an endpoint without a return type, 9–74×
([numbers](https://github.com/TinDang97/rjson/blob/main/examples/README.md#measured-gains)). Don't set it as
`default_response_class`: endpoints that return Pydantic models already go through
`dump_json`. [`examples/fastapi_app.py`](https://github.com/TinDang97/rjson/blob/main/examples/fastapi_app.py) adds a fallback for
Decimal, Pydantic models and dataclasses, and rjson-parsed request bodies.

### Django and Flask

```python
# Django: a JsonResponse rendered by rjson
from django.http import HttpResponse, JsonResponse
import rjson

class RJSONResponse(JsonResponse):
    def __init__(self, data, safe=True, **kwargs):
        if safe and not isinstance(data, dict):
            raise TypeError("set safe=False to serialize non-dict objects")
        kwargs.setdefault("content_type", "application/json")
        HttpResponse.__init__(self, content=rjson.dumps(data), **kwargs)

# Flask: every jsonify() / dict return goes through rjson
from flask.json.provider import JSONProvider

class RJSONProvider(JSONProvider):
    def dumps(self, obj, **kwargs):
        return rjson.dumps_str(obj)

    def loads(self, s, **kwargs):
        return rjson.loads(s)

app.json = RJSONProvider(app)
```

[`examples/django_json.py`](https://github.com/TinDang97/rjson/blob/main/examples/django_json.py)
and [`examples/flask_json.py`](https://github.com/TinDang97/rjson/blob/main/examples/flask_json.py)
add a fallback for Decimal, sets and dataclasses, and a request-body parser that turns
invalid JSON into a 400 with the error position.

### Logs, NDJSON, Redis and Kafka

```python
records = rjson.loads_ndjson(body)                       # NDJSON in memory: one call
line = rjson.dumps(record) + b"\n"                        # one JSON object per log line
producer = KafkaProducer(value_serializer=rjson.dumps)    # bytes in, bytes out
```

- [`examples/json_logging.py`](https://github.com/TinDang97/rjson/blob/main/examples/json_logging.py): a `logging.Formatter` writing one
  JSON object per line (2.2–2.5 µs per record, 2.4–2.7× faster than the same formatter on
  `json`), plus streamed NDJSON read/write (writing 12.5× faster, reading 2.5–2.8×).
- [`examples/codec.py`](https://github.com/TinDang97/rjson/blob/main/examples/codec.py): a versioned bytes codec for Redis/Kafka with
  typed round trips, and `value_serializer`/`value_deserializer` callables. Round trips of
  JSON-native payloads are 4.1–4.6× faster than the same codec on `json`, and faster than
  `pickle`.
- [`docs/ASYNC.md`](https://github.com/TinDang97/rjson/blob/main/docs/ASYNC.md): aiohttp, httpx, asyncpg, `redis.asyncio` and aiokafka
  one-liners, and how to keep large payloads from blocking the event loop.

## Migrating from `json` or orjson

| you wrote | with rjson |
|---|---|
| `json.loads(s)` / `orjson.loads(s)` | `rjson.loads(s)` |
| `[json.loads(l) for l in data.splitlines() if l.strip()]` | `rjson.loads_ndjson(data)` |
| `orjson.dumps(obj)` | `rjson.dumps(obj)` (same bytes) |
| `json.dumps(obj, separators=(",", ":"), ensure_ascii=False)` | `rjson.dumps_str(obj)` |
| `json.dumps(obj).encode()` | `rjson.dumps(obj)` |
| `except json.JSONDecodeError` / `orjson.JSONDecodeError` | `except rjson.JSONDecodeError` |
| `except TypeError` / `orjson.JSONEncodeError` around `dumps` | `except rjson.JSONEncodeError` (`TypeError` keeps working) |
| `json.dumps(obj, default=f)` / `orjson.dumps(obj, default=f)` | `rjson.dumps(obj, default=f)` |
| `orjson.dumps(datetime/UUID/dataclass/Enum)` | the same bytes |
| `json.dumps({1: "a", None: "b"})` / `orjson.dumps(obj, option=OPT_NON_STR_KEYS)` | `rjson.dumps(obj, non_str_keys=True)` (key text as `json` writes it) |
| `orjson.dumps(obj, option=OPT_INDENT_2 \| OPT_SORT_KEYS)` | `rjson.dumps(obj, indent=2, sort_keys=True)` (same bytes) |
| `json.dumps(obj, indent=4, sort_keys=True)` | `rjson.dumps_str(obj, indent=4, sort_keys=True, ensure_ascii=True)` (drop `ensure_ascii` to keep non-ASCII) |
| `json.dumps(obj)` (all defaults) | `rjson.dumps_str(obj, separators=(", ", ": "), ensure_ascii=True, allow_nan=True)` (same text) |
| `json.loads(s)` of input with `NaN`, a BOM or lone surrogates | `rjson.loads(s, lenient=True)` (the same result) |
| `python -m json.tool` | `rjson` / `python -m rjson` (same options, same output; [command line](#command-line-json-beautifier)) |
| `orjson.dumps(obj, option=OPT_PASSTHROUGH_DATETIME)` | `rjson.dumps(obj, passthrough=rjson.PASSTHROUGH_DATETIME)` (also `_UUID`, `_DATACLASS`, `_ENUM`) |
| `json.dumps(obj, default=lambda o: o.isoformat())` for datetimes | `rjson.dumps(obj)` (the same text, except UTC offsets with a seconds part, which are rounded to the minute) |

Two differences are by design: `dumps` raises on NaN/Infinity unless `allow_nan=True`
(then `NaN`, as `json` writes it; orjson writes `null`), and floats below 1e-4 are written
in shortest form (`1e-7`, as orjson; `json` writes `1e-07`, the same value).

## API

| name | kind | notes |
|---|---|---|
| `loads(data, *, lenient=False)` | function → object | `data`: `str`, `bytes`, `bytearray` or `memoryview` (any layout) |
| `loads_ndjson(data, *, lenient=False)` | function → `list` | NDJSON / JSON Lines, one document per line, in one call |
| `dumps(obj, *, default=None, passthrough=0, non_str_keys=False, indent=None, separators=None, sort_keys=False, ensure_ascii=False, allow_nan=False)` | function → `bytes` | UTF-8 JSON, like `orjson.dumps` |
| `dumps_str(...)` | function → `str` | same options; like `json.dumps(obj, ensure_ascii=False, separators=(",", ":"))` |
| `dumps_bytes(...)` | function → `bytes` | alias of `dumps`, kept for compatibility |
| `PASSTHROUGH_DATETIME`, `_UUID`, `_DATACLASS`, `_ENUM` | `int` flags | for `passthrough=`, combine with `\|` |
| `JSONDecodeError` | exception | `json.JSONDecodeError` itself (a `ValueError`) |
| `JSONEncodeError` | exception | subclass of **both** `TypeError` and `ValueError`, like `orjson.JSONEncodeError` |
| `__version__` | `str` | package version |

The wheel ships type stubs (`py.typed`), so mypy and pyright check calls to rjson.

<details>
<summary>Options and behavior in detail</summary>

- **`lenient=True` (`loads`, `loads_ndjson`):** accepts everything `json.loads` accepts,
  with the same result: `NaN`/`Infinity`/`-Infinity`, a UTF-8 BOM on bytes and numbers
  overflowing to `inf` are parsed natively (no speed cost); lone surrogates (`"\ud83d"`,
  sent by JavaScript clients that cut an emoji in half), UTF-16/UTF-32 bytes and nesting
  deeper than 1024 go through `json.loads`. The default stays strict, like orjson.
- **`loads_ndjson`:** `[rjson.loads(line) for line in data.split(b"\n") if line.strip()]`
  in one call: each line gives what `loads(line)` gives (the same errors too), blank lines
  are skipped, `\r\n` works, and an error's `lineno`/`colno` are those of the whole input.
  For a very large input, call it on chunks of about 16 KiB cut after a newline, so each
  chunk's documents are used while they are still in the CPU cache.
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
  not yet as fast as orjson's (twitter.json: `sort_keys` 0.78×, `indent=2` 0.48×).
- **`json.dumps` options:** `indent` also takes a str (`indent="\t"`), and `separators=`,
  `ensure_ascii=True` (non-ASCII and DEL as `\uXXXX`, surrogate pairs above U+FFFF) and
  `allow_nan=True` (`NaN`/`Infinity`/`-Infinity`) behave as in `json.dumps`, so any
  `json.dumps` call has an exact equivalent (numbers aside, see floats below 1e-4). Only
  the defaults differ: compact, non-ASCII kept, NaN raises. With `ensure_ascii=True`,
  `dumps` also writes lone surrogates (as `\udXXX`, like `json`).
- **`default=`:** called with each value rjson cannot serialize; its return value is
  serialized in its place (and passed to `default` again if still unsupported), as in
  `json.dumps` and orjson. Exceptions it raises propagate unchanged. It is not called for
  NaN/Infinity or dict keys (see `non_str_keys=`), which still raise. Supported values
  never reach it, so native data runs at full speed.
- **Errors:** `loads` raises `JSONDecodeError` with `pos`/`lineno`/`colno` matching `json`.
  `dumps`/`dumps_str` raise `JSONEncodeError` for unsupported types, non-str keys (unless
  `non_str_keys=True`), NaN/Infinity, and nesting deeper than 254 (which also catches
  circular references), so both `except TypeError` and `except ValueError` catch it. A
  lone surrogate raises `UnicodeEncodeError` in `dumps`; `dumps_str` passes it through.
- **Precision:** floats round-trip exactly (shortest representation, e.g. `1e+16`), and
  integers of any size are exact in both directions. `loads` accepts nesting up to 1024.

</details>

## Command line: JSON beautifier

`pip install pyrjson` also installs `rjson`, a faster drop-in for `python -m json.tool`: the
same options and byte-for-byte the same output (checked against `json.tool` on the whole
benchmark corpus), plus a formatter mode for files and CI.

```console
$ curl -s https://api.github.com/repos/TinDang97/rjson | rjson --sort-keys --no-ensure-ascii
$ rjson data.json pretty.json            # beautify (indent 4, like json.tool)
$ rjson --compact data.json min.json     # minify (also --minify)
$ rjson --tab / --indent 2 / --no-indent # other layouts
$ rjson --json-lines --compact < events.jsonl   # JSON Lines / NDJSON, streamed
$ rjson -i --indent 2 config/*.json      # reformat files in place (only changed ones)
$ rjson --check --indent 2 config/*.json # CI: exit 1 if a file is invalid or not formatted
$ rjson --validate *.json                # exit 1 if a file is not valid JSON
```

End to end, process start included (CPython 3.13.12, median of 5;
[`benches/cli_benchmark.py`](https://github.com/TinDang97/rjson/blob/main/benches/cli_benchmark.py),
[results](https://github.com/TinDang97/rjson/blob/main/docs/cli-benchmark-results.json)):

| input | `python -m json.tool` | `rjson` | speedup |
|---|---|---|---|
| twitter.json (0.6 MB) | 82 ms | 42 ms | 2.0× |
| citm_catalog.json (1.7 MB) | 170 ms | 52 ms | 3.2× |
| canada.json (2.2 MB, floats) | 385 ms | 79 ms | 4.9× |
| github.json (55 KB) | 37 ms | 39 ms | 0.9× (process start dominates) |
| 60 MB file, pretty-print | 4.29 s | 635 ms | 6.8× |
| 60 MB file, `--sort-keys` | 4.41 s | 704 ms | 6.3× |
| 60 MB file, `--compact` | 3.89 s | 526 ms | 7.4× |
| 200k JSON Lines, `--compact` | 4.32 s | 422 ms | 10.2× |

Syntax colors on a terminal (`--color auto|always|never`; honors `NO_COLOR`,
`FORCE_COLOR`, `PYTHON_COLORS`), NaN/Infinity accepted like `json.tool` (`--strict`
rejects them), errors as `json.tool` prints them (JSON Lines errors name the input line),
atomic in-place writes that keep file permissions. Also `python -m rjson`,
`python -m rjson.tool`, and `rjson.tool.beautify(text, indent=4, ...)` from Python. The
only output difference from `json.tool`: floats below 1e-4 are written in shortest form
(`1e-7`; `json` writes `1e-07`, the same value). `json.tool --json-lines FILE` fails on
CPython 3.13+ ("I/O operation on closed file"); `rjson --json-lines FILE` works.

## Why it is faster than orjson

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/architecture-dark.svg">
  <img alt="Design differences from orjson 3.12. loads: orjson parses into a yyjson tree, then converts the tree to Python objects in a second pass; rjson builds Python objects in one pass (1.08–1.52× faster, 30–37% less peak memory on large files). dumps: orjson starts from a 4 KiB buffer that doubles, escapes with SSE2 or an AVX-512 build and returns bytes; rjson sizes the buffer from recent calls, escapes with AVX-512, AVX2 or SSE2 and writes bytes or str directly (1.21–1.80× faster, as str 1.71–1.91×). Native types: orjson probes three attributes and calls utcoffset() per aware datetime; rjson caches the timezone offset and per-class facts (UTC datetimes 3.18×, datetime/UUID/Enum records 2.28×, slots dataclasses 2.24×)." src="https://raw.githubusercontent.com/TinDang97/rjson/main/docs/img/architecture-light.svg" width="880">
</picture>

Both are Rust on the CPython C API with raw `METH_FASTCALL` entry points, a dict-key
cache and zmij float formatting, so none of that explains the gap. These design choices do.
Each row isolates one of them, is measured head to head with identical output
([`benches/why_faster.py`](https://github.com/TinDang97/rjson/blob/main/benches/why_faster.py),
[results](https://github.com/TinDang97/rjson/blob/main/docs/why-faster-results.json);
CPython 3.13, orjson 3.12.0, x86_64 with AVX-512, PGO wheel), and cites the orjson 3.12.0
source it describes.

| | orjson 3.12.0 | rjson | measured (rjson vs orjson) |
|---|---|---|---|
| **`loads`** | parses into a yyjson document tree, then walks the tree to create Python objects (`src/deserialize/backend/yyjson.rs`) | creates each Python object as it parses: one pass, no tree | 60 MB file: **85 MB vs 183 MB** peak memory above the input (both build the same result, so the ~99 MB difference is what orjson holds while parsing: the tree), 0.22 s vs 0.30 s |
| **string escaping** | 32-byte AVX-512 blocks, but each escape restarts the block: one load, compare and store per escape (`src/serialize/writer/str/avx512.rs`) | every escape in a block handled from one compare mask; AVX-512, AVX2 or SSE2 chosen at run time | 1 MB of text with an escape every 12 characters: **3.39 vs 0.96 GB/s**, same CPU, both on AVX-512 |
| **aware datetimes** | for `datetime.timezone.utc`, per value: up to three `hasattr` probes, then a `utcoffset()` call that allocates a `timedelta` (`src/ffi/pydatetimeref.rs`, `slow_offset`) | reads a `timezone`'s offset once and reuses it; date fields read from the C struct | 10,000 UTC datetimes: **28 vs 93 ns** per datetime |
| **output to `str`** | returns `bytes`; getting a `str` means `.decode()`, a second allocation, copy and UTF-8 validation | writes the `str`'s own storage directly (`dumps_str`) | twitter.json: **1.9 MB vs 3.2 MB** allocated per call, 282 vs 475 µs |
| **records** (0.4.0) | creates each dict and inserts its keys one by one (`src/deserialize/backend/yyjson.rs`) | recognizes objects that repeat a key set and copies a template dict (CPython 3.11–3.13) | records `loads`: **0.90 → 0.70** of orjson's time with the shape cache (0.3.0 → 0.4.1) |

The remaining difference is per-value work. rjson dispatches on exact type pointers, keeps
the output cursor in a register, and sizes the output buffer from recent calls (orjson
starts at 4 KiB and doubles, `src/serialize/writer/byteswriter.rs`). Instruction counts
are deterministic, so host noise can't move them:

| instructions per call (valgrind) | rjson | orjson | orjson ÷ rjson |
|---|---|---|---|
| `dumps` small dict | 2,217 | 2,476 | 1.12 |
| `loads` small dict | 4,063 | 4,924 | 1.21 |
| `dumps` twitter.json | 2.01 M | 3.11 M | 1.55 |
| `loads` twitter.json | 9.07 M | 13.20 M | 1.45 |

Where it is *not* different: page faults for large results are the same (the output has
to be written either way), and `indent=`/`sort_keys=` are still slower than orjson's (a
second pass over the compact output). Reproduce with `python benches/why_faster.py`
(valgrind for the instruction counts). **The full design, stage by stage:**
[docs/ARCHITECTURE.md](https://github.com/TinDang97/rjson/blob/main/docs/ARCHITECTURE.md) walks both pipelines function by function
(input handling, the key and shape caches, number parsing, string decoding, the
output-buffer policy, escaping kernels, the dict walk, native types, guarded mode and the
safety checks), with the reason and the measured effect of each choice.

## Production use

The full report, with every number and how it was measured:
[docs/PRODUCTION_READINESS.md](https://github.com/TinDang97/rjson/blob/main/docs/PRODUCTION_READINESS.md).
In short, rjson is ready for services whose payloads are JSON-native (dicts, lists,
strings, numbers, datetimes, UUIDs), and not yet a drop-in for code that relies on
orjson-only options.

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

## Compatibility

- **CPython 3.10–3.14** on Linux x86_64/aarch64 (glibc and musl), macOS arm64 and Windows
  (CI). x86_64 builds target x86-64-v2 and select AVX2/AVX-512 kernels at runtime.
- **Not supported:** PyPy, GraalPy, free-threaded (no-GIL) builds, subinterpreters.

### Status

Experimental. The API may change before 1.0; pin the version you test against.

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
| `src/parser.rs`, `src/lemire.rs` | `loads`, `loads_ndjson` |
| `src/ser.rs`, `src/native.rs` | `dumps` / `dumps_str`, native types |
| `src/entry.rs`, `src/compat.rs` | C-API entry points, version-portable helpers |
| `tests/` | pytest suites |
| `python/rjson/` | package: `__init__.py`, the command line (`tool.py`, `__main__.py`), type stubs (`__init__.pyi`) |
| `examples/` | FastAPI, Django, Flask, logging/NDJSON and Redis/Kafka integrations (tested) |
| `benches/` | `corpus_benchmark.py` (reference), `showcase.py` (vs orjson on production shapes), `production_benchmark.py` (production workloads), `examples_benchmark.py` (the examples on rjson vs json vs orjson), `cli_benchmark.py` (the command line vs `json.tool`), `why_faster.py` (each design difference, measured), `perf_gate.py`, `make_charts.py` / `make_showcase_charts.py` / `make_demo_svg.py` (README charts, demo), `demo.py` |
| `docs/` | architecture, showcase, performance review, production readiness report, async guide |

<details>
<summary>Troubleshooting source builds</summary>

- **Linker errors** (`symbol(s) not found for architecture arm64`): Python and Rust must
  target the same architecture. Check with `python3 -c "import platform; print(platform.machine())"`;
  on Apple Silicon use an arm64 Python such as `/opt/homebrew/bin/python3`, then
  `cargo clean` and rebuild.
- **Missing Python headers:** install your Python's development package (e.g. `python3-dev`).

</details>

## Roadmap

- An incremental NDJSON decoder for sockets and a streaming encoder for async I/O;
  free-threading and subinterpreter support ([docs/ASYNC.md](https://github.com/TinDang97/rjson/blob/main/docs/ASYNC.md#roadmap))
- Performance: measured NEON speedups on ARM hardware, and NEON UTF-8 decoders;
  `indent=`/`sort_keys=` written by the serializer itself

## License

[MIT](https://github.com/TinDang97/rjson/blob/main/LICENSE) © 2025 Tin Dang. `src/lemire.rs` is adapted from
[fast-float](https://github.com/aldanor/fast-float-rust) (MIT OR Apache-2.0).
