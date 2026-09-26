# rjson integration examples

Drop-in patterns for replacing `json`/`orjson` with rjson in service code. Each file is
self-contained, typed (passes `mypy --strict`; rjson ships type stubs) and
runnable. `tests/test_examples.py` exercises all of them.

| file | what it shows | run it |
|---|---|---|
| [`fastapi_app.py`](fastapi_app.py) | `RJSONResponse` (rjson-rendered `JSONResponse`; datetime/UUID/Enum native, a fallback for Decimal/models/dataclasses), `RJSONRoute` (request bodies parsed by `rjson.loads`, FastAPI's 422 errors unchanged), `json_body` dependency (400 with position / 415 on wrong content type) | `python examples/fastapi_app.py` (needs `fastapi`, `httpx`) |
| [`json_logging.py`](json_logging.py) | `JSONFormatter` for `logging` (one JSON object per line, never raises, stringifies unsupported extras, replaces lone surrogates), `write_ndjson` / `read_ndjson` with blank-line handling and per-line errors | `python examples/json_logging.py` |
| [`codec.py`](codec.py) | bytes codec for Redis/Kafka: schema/version envelope with migrations, round-tripping datetime/UUID/Decimal/set/bytes/Enum/dataclass via registered types, Kafka serializer/deserializer callables (tombstone-safe), optional zstd compression above 1 KB (`compress="zstd"`) | `python examples/codec.py` |

Copy the file you need into your project; nothing here is installed with rjson.

## Things to know before migrating

rjson's API is `loads`, `dumps` (bytes), `dumps_str` (str). The keyword options are
`default=`, `passthrough=` and `non_str_keys=`:

- **No `option=`, `indent`, `sort_keys`, `ensure_ascii`.** datetime, date, time, UUID,
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

## Performance notes (measured, CPython 3.13, noisy host)

- FastAPI, 100 small records: `jsonable_encoder` + `json` 664 µs, `jsonable_encoder` +
  rjson 593 µs, **`rjson.dumps` alone 6.3 µs** (orjson 8.5 µs). Return `RJSONResponse(...)`
  directly for native data. For endpoints with a response model, FastAPI's default path
  (Pydantic `dump_json`, 47 µs) is already as fast as `dump_python` + rjson (45 µs), so
  leave those alone.
- Logging: 2.4 µs per record with `JSONFormatter` against 5.2 µs for the same payload
  through `json.dumps(default=str)`. A record with datetime and UUID extras takes 3.4–4 µs
  through the formatter's `default=` hook, against 7.6 µs with the old convert-and-retry
  fallback, which now only runs for NaN and unsupported key types.
