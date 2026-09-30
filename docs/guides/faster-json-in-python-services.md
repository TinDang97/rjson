# Faster JSON in Python services: FastAPI, logging and queues with rjson

Most Python services spend more CPU on JSON than their authors expect: every API response,
every structured log line, every cached or queued message goes through it. This guide
shows where that time goes and how much each common pattern gains by switching to
[rjson](https://github.com/TinDang97/rjson) (`pip install pyrjson`), a JSON library written
in Rust against the CPython C API. It is faster than orjson on most workloads, with the
same output byte for byte.

All numbers come from `benches/examples_benchmark.py`, which runs the code in
[`examples/`](https://github.com/TinDang97/rjson/tree/main/examples) as written and the
same code on `json` and orjson. The setup was rjson 0.1.0, orjson 3.12.0, FastAPI 0.141,
Pydantic 2.13, CPython 3.13 and PGO builds, with two runs, both shown. Speedups are the
baseline's time divided by the variant's time.

## FastAPI responses

For data that is already JSON-native (rows from a database driver, a cache, another
service), most of FastAPI's response time is `jsonable_encoder` walking the data in
Python, not the encoding itself. Returning a response that renders with rjson skips it:

```python
from fastapi import APIRouter, FastAPI
from fastapi_app import RJSONResponse, RJSONRoute   # copy examples/fastapi_app.py

router = APIRouter(route_class=RJSONRoute)          # request bodies parsed by rjson.loads

@router.get("/export", response_class=RJSONResponse)
def export() -> RJSONResponse:
    rows = load_rows()                              # your data: dicts, lists, datetime, UUID, Enum
    return RJSONResponse(rows)

app = FastAPI()
app.include_router(router)
```

| endpoint | orjson response | rjson response |
|---|---|---|
| GET page, 50 records | 1.15–1.18× | **1.25–1.38×** |
| GET page, 50 records with UUID/datetime/Enum | 1.55–1.63× | **1.80–1.92×** |
| GET export, 1,000 records | 2.25–2.31× | **3.00–3.19×** |
| POST, 3 KB JSON body (`json_body` dependency) | 1.04–1.06× | **1.07–1.08×** |

These are speedups over stock FastAPI with a return type, which serializes with
Pydantic's Rust `dump_json`. Without a return type, stock FastAPI was 6–25× slower
than that. Endpoints with a `response_model` should keep FastAPI's default response
class: Pydantic's path is already fast there.

## Structured logging

A JSON log formatter runs on every log call. `examples/json_logging.py` has a
`JSONFormatter` that never raises and turns unsupported extras into strings:

| work | orjson | rjson |
|---|---|---|
| `format()`, record with 5 plain extras | 1.79–1.95× | **2.40–2.54×** |
| `format()`, extras with UUID/datetime/Decimal/Enum/set | 2.02–2.10× | **2.68–2.71×** |
| `logger.info(...)` through a `StreamHandler` | 1.30–1.31× | **1.41–1.50×** |

(Speedup over the same formatter on `json`.)

## NDJSON files and streams

| work | orjson | rjson |
|---|---|---|
| write 10,000 records | 6.32–6.61× | **12.50–12.64×** |
| read 10,000 lines | 2.39–2.52× | **2.48–2.82×** |

(Speedup over `json`.)

When the NDJSON is already in memory (a file read at once, a request body, a queue batch),
`rjson.loads_ndjson(data)` parses every line in one call instead of a Python loop. It gives
what `loads(line)` gives for each line (the same errors too, with `lineno` counted in the
whole input) and skips blank lines:

```python
records = rjson.loads_ndjson(body)           # instead of [rjson.loads(l) for l in body.splitlines() if l.strip()]
```

On 60–700-byte lines it takes 0.39–0.70× orjson's time for the same loop (1.4–2.6× faster),
and 7–49% less than the loop on `rjson.loads` (CPython 3.11–3.13). For a very large input,
call it on chunks of about 16 KiB cut after a newline (as the `rjson --json-lines` command
line does): a chunk's documents are then used while they are still in the CPU cache.

## Redis and Kafka payloads

`examples/codec.py` is a bytes codec with a schema-version envelope. It round-trips
datetime, UUID, Decimal, sets, bytes, Enums and dataclasses through registered types.

| payload, round trip | orjson | rjson | pickle |
|---|---|---|---|
| ~1 KB JSON-native | 3.44–3.54× | **4.08–4.53×** | 3.26–3.30× |
| ~8 KB JSON-native | 3.61–3.69× | **4.51–4.59×** | 3.01–3.02× |
| ~48 KB JSON-native | 3.35–3.47× | **4.21–4.38×** | 2.57–2.83× |
| 20 dataclasses with UUID/Enum/Decimal/datetime/frozenset | 1.27–1.33× | 1.30–1.34× | 4.43–4.56× |

(Speedup over `json`.) For JSON-native data, rjson round-trips faster than pickle while
staying readable by other languages. For many custom types, pickle still wins, and the
registered-type conversions in Python dominate for both JSON libraries.

## Switching

Most code switches with a find-and-replace:

| before | after |
|---|---|
| `orjson.loads(x)`, `json.loads(x)` | `rjson.loads(x)` |
| `orjson.dumps(x)` | `rjson.dumps(x)` (same bytes) |
| `orjson.dumps(x).decode()` | `rjson.dumps_str(x)` |
| `json.dumps(x).encode()` | `rjson.dumps(x)` |
| `orjson.dumps(x, default=f)` | `rjson.dumps(x, default=f)` |

`orjson.dumps(x, option=OPT_INDENT_2 | OPT_SORT_KEYS)` becomes
`rjson.dumps(x, indent=2, sort_keys=True)`, with the same bytes. Not supported yet: orjson's
other `option=` flags; keep orjson for those calls. `loads` is strict by default, like orjson;
`lenient=True` accepts exactly what `json.loads` accepts. The full migration guide is in the
[README](https://github.com/TinDang97/rjson#migrating-from-json-or-orjson).

## Why it is faster

rjson builds Python objects in one pass while parsing; orjson parses into a tree first,
then converts it. rjson sizes its output buffer from recent calls, picks AVX-512/AVX2/SSE2
escaping at run time, writes a `str` directly when you need one, and caches timezone
offsets and per-class facts instead of looking them up for every value. The details and
22 more workloads are in the
[showcase](https://github.com/TinDang97/rjson/blob/main/docs/SHOWCASE.md).

---

rjson is MIT licensed and 0.x: pin the version you test against. Issues and pull
requests are welcome, and some are
[marked for first-time contributors](https://github.com/TinDang97/rjson/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22).
If it saves you CPU time, a ⭐ on [GitHub](https://github.com/TinDang97/rjson) helps other
people find it.
