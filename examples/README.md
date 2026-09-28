# rjson integration examples

Drop-in patterns for replacing `json`/`orjson` with rjson in service code. Each file is
self-contained, typed (passes `mypy --strict`; rjson ships type stubs) and
runnable. `tests/test_examples.py` exercises all of them.

| file | what it shows | run it |
|---|---|---|
| [`fastapi_app.py`](fastapi_app.py) | `RJSONResponse` (rjson-rendered `JSONResponse`; datetime/UUID/Enum native, a fallback for Decimal/models/dataclasses), `RJSONRoute` (request bodies parsed by `rjson.loads`, FastAPI's 422 errors unchanged), `json_body` dependency (400 with position / 415 on wrong content type) | `python examples/fastapi_app.py` (needs `fastapi`, `httpx`) |
| [`django_json.py`](django_json.py) | `RJSONResponse` (a `JsonResponse` rendered by rjson, Django's `safe=` kept; datetime/UUID/Enum native, a fallback for Decimal/sets/dataclasses), `loads_body` for `request.body` (invalid JSON becomes a 400 with message, line, column and position); no side effects on import | `python examples/django_json.py` (needs `django`) |
| [`flask_json.py`](flask_json.py) | `RJSONProvider` (Flask `JSONProvider`: `jsonify` and dict returns rendered by `rjson.dumps_str`, request JSON parsed by `rjson.loads`), the same fallback, 400/415/422 handling for request bodies | `python examples/flask_json.py` (needs `flask`) |
| [`json_logging.py`](json_logging.py) | `JSONFormatter` for `logging` (one JSON object per line, never raises, stringifies unsupported extras, replaces lone surrogates), `write_ndjson` / `read_ndjson` with blank-line handling and per-line errors | `python examples/json_logging.py` |
| [`codec.py`](codec.py) | bytes codec for Redis/Kafka: schema/version envelope with migrations, round-tripping datetime/UUID/Decimal/set/bytes/Enum/dataclass via registered types, Kafka serializer/deserializer callables (tombstone-safe), optional zstd compression above 1 KB (`compress="zstd"`) | `python examples/codec.py` |

Copy the file you need into your project; nothing here is installed with rjson.

## Things to know before migrating

rjson's API is `loads`, `dumps` (bytes), `dumps_str` (str). The keyword options are
`default=`, `passthrough=` and `non_str_keys=`:

- **No `option=`; `indent=`/`sort_keys=` (orjson's layout and order) and `json.dumps`'s `separators=`, `ensure_ascii=`, `allow_nan=` exist** (defaults: compact, non-ASCII kept, NaN raises). datetime, date, time, UUID,
  dataclasses and `Enum` members are serialized natively, byte for byte like orjson;
  `passthrough=` sends them to `default=` instead (`codec.py` does, to tag them). Decimal,
  sets, bytes and other types raise unless `default=` converts them (`json_logging.py`
  does). `default` is not called for NaN or dict keys, so the examples keep a second
  tier: call rjson first, and only when it raises, convert the data in Python and call it
  again. Data that is already JSON-native pays nothing.
- **`dumps` raises `rjson.JSONEncodeError`**, a subclass of both `TypeError` and
  `ValueError` (like `orjson.JSONEncodeError`), for an unsupported type, a non-str key,
  NaN/Infinity or too-deep nesting, so `except TypeError` handlers written for
  `json.dumps` keep working. A lone surrogate raises `UnicodeEncodeError` (a `ValueError`).
- **Dict keys must be `str` unless `non_str_keys=True`.** With it, `int`/`float`/`bool`/
  `None` keys are written exactly as `json` writes them, and Enum, datetime and UUID keys
  as orjson's `OPT_NON_STR_KEYS` does (`json_logging.py` uses it). Without it rjson raises,
  as orjson does without the option.
- **NaN/Infinity raise** (`json` writes `NaN`, orjson writes `null`), and `loads` rejects
  the `NaN`/`Infinity` literals that `json.loads` accepts unless `lenient=True`.
- **`loads` is stricter than `json.loads`:** it rejects a UTF-8 BOM in `bytes`, UTF-16/32
  input, escaped lone surrogates (`"\ud83d"`, which JavaScript clients send when they cut
  an emoji in half) and numbers that overflow to infinity. In FastAPI these requests get a
  422 where the stdlib accepted them. `rjson.loads(body, lenient=True)` accepts exactly
  what `json.loads` accepts, with the same result.
- **Floats below 1e-4 format differently from `json`** (`1e-7` rather than `1e-07`). The
  output is identical to orjson and the values round-trip exactly, but body hashes, ETags
  and snapshot tests that compare bytes will change.
- **Lone surrogates:** `dumps` raises `UnicodeEncodeError`, while `dumps_str` returns a
  `str` that cannot be encoded to UTF-8 later on.
- **Nesting limit 254 for `dumps`** (orjson: 254; `json`: the recursion limit). `loads`
  accepts 1024 levels.
- **Error messages** differ in wording from `json` (`exc.msg` is the bare reason, as in
  orjson). `pos`, `lineno`, `colno` and `doc` match `json` for delimiter errors
  (missing/trailing `,` or `:`, extra data).

## Measured gains

[`benches/examples_benchmark.py`](../benches/examples_benchmark.py) runs each example as
written, then runs **the same example code** with its `rjson` name bound to stdlib `json`
or to orjson (a shim with the same call signatures), so only the JSON library changes. Every
variant's output is checked against the others first (parsed response bodies, log lines,
NDJSON records, lossless codec round trips). Speedup = baseline time ÷ variant time, so
**higher is better**. Two runs of 9 interleaved rounds each on CPython 3.13.12, PGO wheel of
`main`, orjson 3.12.0, FastAPI 0.141.1, Pydantic 2.13.5, 4-core x86_64 Xeon; ranges span
both runs. Raw data: [`docs/examples-benchmark-results.json`](../docs/examples-benchmark-results.json).

**FastAPI, whole requests through the ASGI app** (routing, dependencies, Pydantic,
response; no network). The baseline is stock FastAPI returning the data from an endpoint
with a return type, which FastAPI 0.141 serializes with Pydantic's `dump_json` (Rust).
All apps mount their routes through an `APIRouter`, as this example does.

| request | stock, return type | stock, no return type | example on `json` | example on orjson | **example on rjson** |
|---|---|---|---|---|---|
| GET page, 50 JSON-native records | 63–70 µs (1.00×) | 0.14–0.15× | 0.62–0.67× | 1.15–1.18× | **1.25–1.38×** (51 µs) |
| GET page, 50 records with UUID/datetime/Enum | 113–122 µs (1.00×) | 0.15–0.17× | 0.14–0.15× | 1.55–1.63× | **1.80–1.92×** (63 µs) |
| GET export, 1,000 records | 399–404 µs (1.00×) | 0.04–0.05× | 0.38–0.42× | 2.25–2.31× | **3.00–3.19×** (125–135 µs) |
| POST 3 KB event, `Depends(json_body)` | 73–89 µs (1.00×) | 0.90–0.93× | 0.87× | 1.04–1.06× | **1.07–1.08×** |
| POST Pydantic body, `RJSONRoute` | 91–94 µs (1.00×) | 1.01–1.04× | 0.99–1.00× | 1.02–1.03× | **1.03–1.04×** |

- Responses are where it pays: `RJSONResponse` serializes UUID/datetime/Enum rows itself,
  so a typed page is 1.8–1.9× faster end to end than FastAPI's own Pydantic path, and a
  1,000-row export 3×. Against an endpoint *without* a return type (FastAPI then runs
  `jsonable_encoder` + `json.dumps`), the same requests are 9–74× faster.
- The pattern needs rjson: on stdlib `json` the same example code is slower than stock
  FastAPI (0.14–0.67×): Pydantic's serializer is faster than `json.dumps`, and typed rows
  make `RJSONResponse` fall back to `jsonable_encoder`.
- Request bodies gain little (3–8%): a 3 KB body parses in a few µs, while FastAPI's
  routing and dependency handling take ~70–90 µs. Use `RJSONRoute`/`json_body` for large
  bodies or when you want `rjson.loads` semantics, not for speed on small ones.
- Mount through `include_router` or not, but compare like with like: in FastAPI 0.141 an
  included route costs ~15 µs more per request than one added to the app directly,
  whichever library serializes.

**Logging and NDJSON** (baseline: the same example code on stdlib `json`):

| operation | `json` | orjson | **rjson** |
|---|---|---|---|
| `JSONFormatter.format()`, 5 plain extras | 5.6–6.1 µs | 1.79–1.95× | **2.40–2.54×** (2.2–2.5 µs) |
| `JSONFormatter.format()`, UUID/datetime/Decimal/Enum/set extras | 11 µs | 2.02–2.10× | **2.68–2.71×** (4.2 µs) |
| whole `logger.info(...)` through a `StreamHandler` | 12–15 µs | 1.30–1.31× | **1.41–1.50×** (8.5–10 µs) |
| `write_ndjson`, per record (10,000 records) | 3.2–3.5 µs | 6.32–6.61× | **12.5–12.6×** (253–281 ns) |
| `read_ndjson`, per line (10,000 lines) | 2.1–2.6 µs | 2.39–2.52× | **2.48–2.82×** (833–924 ns) |

A whole `logger.info` call gains less than `format()` because the `logging` module itself
(record creation, handler lock, filters) costs ~6–8 µs.

**Redis/Kafka codec, `encode` + `decode` round trip** (baseline: the same `Codec` on `json`):

| payload | `json` | orjson | **rjson** | `pickle` (reference) |
|---|---|---|---|---|
| ~1 KB JSON-native | 23–24 µs | 3.44–3.54× | **4.08–4.53×** (5.3–5.8 µs) | 3.26–3.30× |
| ~8 KB JSON-native | 149–153 µs | 3.61–3.69× | **4.51–4.59×** (33 µs) | 3.01–3.02× |
| ~48 KB JSON-native | 800–841 µs | 3.35–3.47× | **4.21–4.38×** (190–192 µs) | 2.57–2.83× |
| 20 dataclasses with UUID/Enum/Decimal/datetime/frozenset | 379–391 µs | 1.27–1.33× | **1.30–1.34×** (283–301 µs) | 4.43–4.56× |

- JSON-native payloads take the codec's fast path (one `rjson.dumps`/`loads` call) and are
  4.1–4.6× faster than on `json`, and faster than `pickle`.
- Typed payloads go through the codec's type tagging, a Python walk that dominates the time
  whichever library is underneath (1.3×). `pickle` is 3.3–3.5× faster there, but it only works
  Python-to-Python, and unpickling untrusted data runs arbitrary code; keep it for private,
  trusted caches.

Reproduce: `python benches/examples_benchmark.py` (add `--quick` for a short run, `--check`
for the equivalence checks alone, `--output-json PATH` for raw data). Needs `fastapi` and
`orjson` installed.
