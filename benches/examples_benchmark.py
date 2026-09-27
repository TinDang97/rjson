"""Benchmark the example integrations (``examples/``) with rjson, stdlib json and orjson.

Every example calls its JSON library through a module-level ``rjson`` name. This
script loads each example once per backend, as a separate module, and rebinds that
name to a small shim with the same call signatures (``dumps``, ``dumps_str``,
``loads`` and the ``PASSTHROUGH_*`` flags) backed by ``json`` or ``orjson``. The
example code under test is therefore identical in every variant; only the JSON
library changes. Before anything is timed, every variant's output is checked
against the others (parsed response bodies, log lines, NDJSON records, codec round
trips), and a variant that cannot do the same job is reported as n/a instead of
being timed.

Workloads:

* ``fastapi``: whole requests through the ASGI app (routing, dependencies, Pydantic,
  response), driven in-process without a network. Baselines are stock FastAPI
  endpoints that return the data and parse bodies with ``await request.json()``:
  with a return type, FastAPI 0.141 serializes with Pydantic's ``dump_json`` (the
  baseline for speedups); without one, with ``jsonable_encoder`` + ``json.dumps``.
  All apps mount their routes through an ``APIRouter`` like the example.
* ``logging``: ``JSONFormatter.format(record)``, and a whole ``logger.info(...)`` call
  through a handler.
* ``ndjson``: ``write_ndjson`` / ``read_ndjson`` over 10,000 log-like records.
* ``codec``: ``Codec.encode`` + ``Codec.decode`` round trips (Redis/Kafka payloads);
  ``pickle`` is shown for reference.

Usage:
    python benches/examples_benchmark.py [--quick] [--only NAME] [--rounds N]
                                         [--output-json PATH] [--check]

``--check`` runs only the equivalence checks (used by ``tests/test_examples.py``).
Times are the median over interleaved rounds; ratios are what to trust on a noisy
host.
"""

import argparse
import dataclasses
import datetime as dt
import decimal
import enum
import importlib.util
import io
import json
import logging
import pickle
import statistics
import sys
import time
import types
import uuid
from pathlib import Path
from typing import Any, Callable

import orjson
import rjson

ROOT = Path(__file__).resolve().parent.parent
EXAMPLES = ROOT / "examples"
BACKENDS = ("json", "orjson", "rjson")


# -- backend shims ----------------------------------------------------------------------


class ShimEncodeError(TypeError, ValueError):
    """What rjson raises (``JSONEncodeError`` subclasses both); json and orjson raise
    ``TypeError`` or ``ValueError`` alone, and the codec example relies on the pair."""


def _encode_errors(fn: Callable[..., Any]) -> Callable[..., Any]:
    def wrapper(obj: Any, **kw: Any) -> Any:
        try:
            return fn(obj, **kw)
        except UnicodeEncodeError:
            raise  # rjson raises this one as is
        except (TypeError, ValueError) as exc:
            raise ShimEncodeError(str(exc)) from exc
    return wrapper


def _json_shim() -> types.SimpleNamespace:
    """stdlib json with rjson's call signatures and output format.

    ``json`` never serializes datetime/UUID/dataclass/Enum natively, so it behaves as
    if every ``passthrough`` flag were set; NaN is rejected like rjson does.
    """

    def dumps_str(obj: Any, *, default: Any = None, passthrough: int = 0,
                  non_str_keys: bool = False) -> str:
        return json.dumps(obj, default=default, ensure_ascii=False, separators=(",", ":"),
                          allow_nan=False)

    def dumps(obj: Any, **kw: Any) -> bytes:
        return dumps_str(obj, **kw).encode()

    return types.SimpleNamespace(dumps=_encode_errors(dumps), dumps_str=_encode_errors(dumps_str),
                                 loads=json.loads, **_flags())


def _orjson_shim() -> types.SimpleNamespace:
    """orjson with rjson's call signatures.

    orjson can pass datetime and dataclasses through to ``default`` but not UUID or
    Enum values; where an example needs that, its checks fail and the variant is n/a.
    """

    def dumps(obj: Any, *, default: Any = None, passthrough: int = 0,
              non_str_keys: bool = False) -> bytes:
        option = orjson.OPT_NON_STR_KEYS if non_str_keys else 0
        if passthrough & rjson.PASSTHROUGH_DATETIME:
            option |= orjson.OPT_PASSTHROUGH_DATETIME
        if passthrough & rjson.PASSTHROUGH_DATACLASS:
            option |= orjson.OPT_PASSTHROUGH_DATACLASS
        return orjson.dumps(obj, default=default, option=option)

    def dumps_str(obj: Any, **kw: Any) -> str:
        return dumps(obj, **kw).decode()

    return types.SimpleNamespace(dumps=_encode_errors(dumps), dumps_str=_encode_errors(dumps_str),
                                 loads=orjson.loads, **_flags())


def _flags() -> dict[str, Any]:
    names = ("PASSTHROUGH_DATETIME", "PASSTHROUGH_UUID", "PASSTHROUGH_DATACLASS",
             "PASSTHROUGH_ENUM")
    return {n: getattr(rjson, n) for n in names} | {
        "JSONDecodeError": json.JSONDecodeError, "JSONEncodeError": ShimEncodeError}


SHIMS: dict[str, Any] = {"json": _json_shim(), "orjson": _orjson_shim(), "rjson": rjson}


def load_example(name: str, backend: str) -> types.ModuleType:
    """Import ``examples/<name>.py`` as its own module with ``rjson`` bound to ``backend``."""
    mod_name = f"rjson_bench_{name}_{backend}"
    if mod_name in sys.modules:
        return sys.modules[mod_name]
    spec = importlib.util.spec_from_file_location(mod_name, EXAMPLES / f"{name}.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[mod_name] = module
    spec.loader.exec_module(module)
    module.rjson = SHIMS[backend]  # the name every example calls its JSON library by
    return module


# -- timing -----------------------------------------------------------------------------


@dataclasses.dataclass
class Case:
    group: str
    name: str
    desc: str
    variants: dict[str, Callable[[], Any]]  # label -> one timed operation
    baseline: str
    check: Callable[[dict[str, Any]], dict[str, str]]  # results -> {label: why n/a}
    loop: int = 1  # operations per call of a variant (batched cases)


def time_case(case: Case, rounds: int, min_time: float) -> dict[str, float]:
    """Median seconds per operation for each variant, over interleaved rounds."""
    labels = list(case.variants)
    n = {}
    for label in labels:  # calibrate: batch size taking ~min_time per round
        fn = case.variants[label]
        k = 1
        while True:
            t0 = time.perf_counter()
            for _ in range(k):
                fn()
            if time.perf_counter() - t0 >= min_time or k >= 1 << 20:
                break
            k *= 2
        n[label] = k
    samples: dict[str, list[float]] = {label: [] for label in labels}
    for r in range(rounds):
        for j in range(len(labels)):
            label = labels[(j + r) % len(labels)]
            fn, k = case.variants[label], n[label]
            t0 = time.perf_counter()
            for _ in range(k):
                fn()
            samples[label].append((time.perf_counter() - t0) / (k * case.loop))
    return {label: statistics.median(s) for label, s in samples.items()}


def same(values: dict[str, Any], ref: str) -> dict[str, str]:
    """Labels whose value differs from ``values[ref]``, with the reason."""
    return {label: f"output differs from {ref}" for label, v in values.items()
            if label != ref and v != values[ref]}


# -- FastAPI ----------------------------------------------------------------------------


class Status(enum.Enum):
    ACTIVE = "active"
    PAUSED = "paused"


def _rows(n: int, typed: bool) -> list[dict[str, Any]]:
    base = dt.datetime(2024, 5, 1, 12, 0, tzinfo=dt.timezone.utc)
    rows = []
    for i in range(n):
        row: dict[str, Any] = {
            "sku": f"SKU-{i:06d}",
            "name": f"Product {i} " + ("café" if i % 7 == 0 else "widget"),
            "price": round(9.99 + i * 0.37, 2),
            "qty": i % 40,
            "tags": ["new", "sale"] if i % 3 == 0 else ["stock"],
        }
        if typed:
            row["id"] = uuid.UUID(int=0x1234_5678 * (i + 1))
            row["status"] = Status.ACTIVE if i % 4 else Status.PAUSED
            row["created"] = base + dt.timedelta(minutes=i)
        else:
            row["id"] = i
        rows.append(row)
    return rows


def _utc_z(value: Any) -> Any:
    """Spell UTC as ``Z`` everywhere: Pydantic writes ``...Z``, rjson/orjson ``...+00:00``."""
    if isinstance(value, str) and value.endswith("+00:00"):
        return value[:-6] + "Z"
    if isinstance(value, list):
        return [_utc_z(v) for v in value]
    if isinstance(value, dict):
        return {k: _utc_z(v) for k, v in value.items()}
    return value


def _asgi_request(app: Any, method: str, path: str, body: bytes = b"") -> tuple[int, bytes]:
    """One request through ``app``; returns (status, body). No network, no client."""
    scope = {
        "type": "http", "asgi": {"version": "3.0"}, "http_version": "1.1",
        "method": method, "scheme": "http", "path": path, "raw_path": path.encode(),
        "root_path": "", "query_string": b"", "server": ("bench", 80),
        "client": ("127.0.0.1", 1234),
        "headers": [(b"host", b"bench"), (b"content-type", b"application/json"),
                    (b"content-length", str(len(body)).encode())],
    }
    sent = False
    status = 0
    out = bytearray()

    async def receive() -> dict[str, Any]:
        nonlocal sent
        if sent:
            return {"type": "http.disconnect"}
        sent = True
        return {"type": "http.request", "body": body, "more_body": False}

    async def send(message: dict[str, Any]) -> None:
        nonlocal status
        if message["type"] == "http.response.start":
            status = message["status"]
        elif message["type"] == "http.response.body":
            out.extend(message.get("body", b""))

    coro = app(scope, receive, send)
    try:  # FastAPI's handlers never really suspend here: drive the coroutine directly
        coro.send(None)
    except StopIteration:
        return status, bytes(out)
    coro.close()
    raise RuntimeError("the ASGI app suspended; run it in an event loop instead")


def _fastapi_apps(pages: dict[str, dict[str, Any]]) -> dict[str, Any]:
    """label -> app. Same routes everywhere; only the JSON handling differs."""
    from fastapi import APIRouter, Depends, FastAPI, Request
    from pydantic import BaseModel

    class ItemIn(BaseModel):
        name: str
        price: decimal.Decimal
        tags: list[str] = []

    class ItemOut(ItemIn):
        id: int

    # Endpoints come from factories: FastAPI reads a route function's parameters
    # (keyword defaults included) as request parameters.
    def returns(value: Any) -> Callable[[], Any]:
        async def endpoint() -> Any:
            return value
        return endpoint

    def renders(ex: Any, value: Any) -> Callable[[], Any]:
        # A new response per request: Starlette renders the body in __init__.
        async def endpoint() -> Any:
            return ex.RJSONResponse(value)
        return endpoint

    def events_endpoint(ex: Any) -> Callable[..., Any]:
        async def endpoint(event: Any = Depends(ex.json_body)) -> Any:
            return ex.RJSONResponse({"accepted": True, "items": len(event["items"])})
        return endpoint

    def returns_untyped(value: Any) -> Callable[[], Any]:
        async def endpoint():  # type: ignore[no-untyped-def]  # no return type on purpose
            return value
        return endpoint

    async def events_typed(request: Request) -> dict[str, Any]:
        event = await request.json()
        return {"accepted": True, "items": len(event["items"])}

    async def events_untyped(request: Request):  # type: ignore[no-untyped-def]
        event = await request.json()
        return {"accepted": True, "items": len(event["items"])}

    async def items(item: ItemIn) -> ItemOut:
        return ItemOut(**item.model_dump(), id=1)

    # Every app mounts its routes through an APIRouter, as the example (and most
    # real apps) do: in FastAPI 0.141, include_router costs ~15 us per request
    # whatever serializes the response, so mounting one side differently skews it.
    def mount(router: Any) -> Any:
        app = FastAPI()
        app.include_router(router)
        return app

    apps: dict[str, Any] = {}
    # What an app has before adopting the example. With a return type (or
    # response_model), FastAPI 0.141 serializes with Pydantic's dump_json (Rust);
    # without one, with jsonable_encoder + Starlette's JSONResponse (json.dumps).
    for label, page_ep, events_ep in (
        ("stock FastAPI, return type", returns, events_typed),
        ("stock FastAPI, no return type", returns_untyped, events_untyped),
    ):
        router = APIRouter()
        for key, page in pages.items():
            router.add_api_route(f"/{key}", page_ep(page), methods=["GET"])
        router.add_api_route("/events", events_ep, methods=["POST"])
        router.add_api_route("/items", items, methods=["POST"], response_model=ItemOut)
        apps[label] = mount(router)

    for backend in BACKENDS:
        ex = load_example("fastapi_app", backend)
        router = APIRouter(route_class=ex.RJSONRoute)
        for key, page in pages.items():
            router.add_api_route(f"/{key}", renders(ex, page), methods=["GET"])
        router.add_api_route("/events", events_endpoint(ex), methods=["POST"])
        router.add_api_route("/items", items, methods=["POST"], response_model=ItemOut)
        apps[f"example on {backend}"] = mount(router)
    return apps


def fastapi_cases(quick: bool) -> list[Case]:
    pages = {
        "page_native": {"items": _rows(50, typed=False), "count": 50, "next": "/page?after=50"},
        "page_typed": {"items": _rows(50, typed=True), "count": 50, "next": "/page?after=50"},
        "export": {"items": _rows(200 if quick else 1000, typed=False), "count": 1000},
    }
    apps = _fastapi_apps(pages)
    event = {
        "type": "order.created", "source": "checkout", "items": _rows(20, typed=False),
        "customer": {"id": 42, "name": "Ada Lovelace", "email": "ada@example.com"},
    }
    event_body = json.dumps(event).encode()
    item_body = json.dumps({"name": "pen", "price": "12.50", "tags": ["office"]}).encode()

    specs = [
        ("GET page, 50 records (JSON-native)", "GET", "/page_native", b""),
        ("GET page, 50 records with UUID/datetime/Enum", "GET", "/page_typed", b""),
        (f"GET export, {len(pages['export']['items'])} records", "GET", "/export", b""),
        ("POST event, 3 KB JSON body (Depends(json_body))", "POST", "/events", event_body),
        ("POST Pydantic body (RJSONRoute)", "POST", "/items", item_body),
    ]
    cases = []
    for desc, method, path, body in specs:
        variants = {label: (lambda app=app, m=method, p=path, b=body: _asgi_request(app, m, p, b))
                    for label, app in apps.items()}

        def check(results: dict[str, Any]) -> dict[str, str]:
            bad = {k: f"HTTP {v[0]}" for k, v in results.items() if v[0] >= 400}
            parsed = {k: _utc_z(json.loads(v[1])) for k, v in results.items() if k not in bad}
            return bad | same(parsed, "stock FastAPI, return type")

        cases.append(Case("fastapi", path.strip("/") + ("" if method == "GET" else "_post"),
                          desc, variants, "stock FastAPI, return type", check))
    return cases


# -- logging and NDJSON -----------------------------------------------------------------


class _NullStream:
    def write(self, s: str) -> int:
        return len(s)

    def flush(self) -> None:
        pass


def logging_cases(quick: bool) -> list[Case]:
    extra_plain = {"request_id": "9f1c2e7a", "path": "/api/orders", "status": 200,
                   "duration_ms": 12.5, "user_id": 1234}
    extra_typed = {"request_id": "9f1c2e7a", "order_id": uuid.UUID(int=77),
                   "at": dt.datetime(2024, 5, 1, 12, 0, tzinfo=dt.timezone.utc),
                   "amount": decimal.Decimal("19.90"), "status": Status.ACTIVE,
                   "retries": 2, "tags": {"vip"}}
    fmts = {b: load_example("json_logging", b).JSONFormatter(static_fields={"service": "api"})
            for b in BACKENDS}
    cases = []
    for key, desc, extra in (("format_plain", "format(): record with 5 plain extras", extra_plain),
                             ("format_typed", "format(): extras with UUID/datetime/Decimal/Enum/set",
                              extra_typed)):
        rec = logging.getLogger("bench").makeRecord(
            "bench", logging.INFO, __file__, 1, "order %s placed", ("A-1",), None, extra=extra)
        variants = {f"example on {b}": (lambda f=f, rec=rec: f.format(rec)) for b, f in fmts.items()}
        cases.append(Case("logging", key, desc, variants, "example on json",
                          lambda r: same({k: json.loads(v) for k, v in r.items()},
                                         "example on json")))

    loggers = {}
    for b, f in fmts.items():
        lg = logging.getLogger(f"bench.{b}")
        lg.handlers[:] = []
        handler = logging.StreamHandler(_NullStream())
        handler.setFormatter(f)
        lg.addHandler(handler)
        lg.propagate = False
        lg.setLevel(logging.INFO)
        loggers[b] = lg

    def emit(lg: logging.Logger) -> None:
        lg.info("order %s placed", "A-1", extra=extra_plain)

    cases.append(Case("logging", "logger_info", "logger.info(...) through a StreamHandler",
                      {f"example on {b}": (lambda lg=lg: emit(lg)) for b, lg in loggers.items()},
                      "example on json", lambda r: {}))

    n = 2000 if quick else 10_000
    records = [{"ts": f"2024-05-01T12:00:{i % 60:02d}.000Z", "level": "INFO", "logger": "api",
                "message": f"GET /api/items/{i} 200", "duration_ms": i % 97 + 0.5,
                "user": {"id": i, "name": "Zoë" if i % 11 == 0 else "bob"}} for i in range(n)]
    mods = {b: load_example("json_logging", b) for b in BACKENDS}
    blob = b"".join(rjson.dumps(r) + b"\n" for r in records)

    def write(mod: Any) -> bytes:
        buf = io.BytesIO()
        mod.write_ndjson(buf, records)
        return buf.getvalue()

    def read(mod: Any) -> list[Any]:
        return list(mod.read_ndjson(io.BytesIO(blob)))

    cases.append(Case("ndjson", "write", f"write_ndjson: {n} records to a binary file",
                      {f"example on {b}": (lambda m=m: write(m)) for b, m in mods.items()},
                      "example on json",
                      lambda r: same({k: [json.loads(x) for x in v.splitlines()]
                                      for k, v in r.items()}, "example on json"),
                      loop=n))
    cases.append(Case("ndjson", "read", f"read_ndjson: {n} lines from a binary file",
                      {f"example on {b}": (lambda m=m: read(m)) for b, m in mods.items()},
                      "example on json", lambda r: same(r, "example on json"), loop=n))
    return cases


# -- codec ------------------------------------------------------------------------------


@dataclasses.dataclass
class User:
    id: uuid.UUID
    name: str
    status: Status
    balance: decimal.Decimal
    created: dt.datetime
    roles: frozenset[str] = frozenset()


def codec_cases(quick: bool) -> list[Case]:
    def feed(n: int) -> list[dict[str, Any]]:
        return [{"id": i, "title": f"Post {i}: an update", "author": {"id": i % 50, "name": "ann"},
                 "tags": ["news", "tech"], "score": i * 0.5, "published": True} for i in range(n)]

    users = [User(uuid.UUID(int=i + 1), f"user{i}", Status.ACTIVE, decimal.Decimal("10.50"),
                  dt.datetime(2024, 5, 1, 12, i % 60, tzinfo=dt.timezone.utc), frozenset({"admin"}))
             for i in range(20)]
    payloads = [
        ("native_1k", "round trip, ~1 KB JSON-native payload", feed(8)),
        ("native_8k", "round trip, ~8 KB JSON-native payload", feed(70)),
        ("native_48k", "round trip, ~48 KB JSON-native payload", feed(420)),
        ("typed", "round trip, 20 dataclasses with UUID/Enum/Decimal/datetime/frozenset", users),
    ]
    cases = []
    for key, desc, obj in payloads:
        variants: dict[str, Callable[[], Any]] = {}
        for b in BACKENDS:
            mod = load_example("codec", b)
            codec = mod.Codec(schema="bench")
            codec.register(Status, "Status")
            codec.register(User, "User")
            variants[f"example on {b}"] = (lambda c=codec, o=obj: c.decode(c.encode(o)))
        variants["pickle (reference)"] = lambda o=obj: pickle.loads(pickle.dumps(o, protocol=5))

        def check(results: dict[str, Any], obj: Any = obj) -> dict[str, str]:
            return {k: "round trip is not lossless" for k, v in results.items() if v != obj}

        cases.append(Case("codec", key, desc, variants, "example on json", check))
    return cases


# -- main -------------------------------------------------------------------------------


def build(quick: bool, only: str | None) -> list[Case]:
    groups = {"fastapi": fastapi_cases, "logging": logging_cases, "codec": codec_cases}
    cases = []
    for name, make in groups.items():
        if only and not any(o in name for o in only.split(",")):
            made = [c for c in make(quick) if any(o in c.name for o in only.split(","))]
        else:
            made = make(quick)
        cases.extend(made)
    return cases


def run_checks(cases: list[Case]) -> dict[tuple[str, str], dict[str, str]]:
    """{(group, name): {label: why n/a}}; also drops those variants from the cases."""
    out = {}
    for case in cases:
        results = {}
        errors = {}
        for label, fn in case.variants.items():
            try:
                results[label] = fn()
            except Exception as exc:  # a backend that cannot do this job
                errors[label] = f"{type(exc).__name__}: {exc}"[:80]
        bad = errors | case.check(results)
        if case.baseline in bad:
            raise SystemExit(f"{case.group}/{case.name}: baseline failed: {bad[case.baseline]}")
        for label in bad:
            case.variants.pop(label, None)
        out[(case.group, case.name)] = bad
    return out


def fmt_t(s: float) -> str:
    return f"{s * 1e9:.0f} ns" if s < 1e-6 else f"{s * 1e6:.2f} µs" if s < 1e-3 else f"{s * 1e3:.2f} ms"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--quick", action="store_true", help="smaller sizes, fewer rounds")
    ap.add_argument("--only", help="comma-separated substrings of group or case names")
    ap.add_argument("--rounds", type=int, help="interleaved rounds per case (default 9, quick 3)")
    ap.add_argument("--output-json", metavar="PATH")
    ap.add_argument("--check", action="store_true", help="equivalence checks only, no timing")
    args = ap.parse_args()

    cases = build(args.quick, args.only)
    na = run_checks(cases)
    if args.check:
        for (group, name), bad in na.items():
            for label, why in bad.items():
                print(f"{group}/{name}: {label}: n/a ({why})")
        print(f"checked {len(cases)} cases: outputs equivalent")
        return

    rounds = args.rounds or (3 if args.quick else 9)
    min_time = 0.02 if args.quick else 0.05
    print(f"Python {sys.version.split()[0]}, rjson {rjson.__version__}, orjson {orjson.__version__}; "
          f"median of {rounds} interleaved rounds; speedup = baseline time / variant time")
    rows = []
    for case in cases:
        times = time_case(case, rounds, min_time)
        base = times[case.baseline]
        print(f"\n{case.group}/{case.name}: {case.desc}")
        for label in [*case.variants, *na[(case.group, case.name)]]:
            if label in times:
                speed = base / times[label]
                print(f"  {label:28} {fmt_t(times[label]):>12}  {speed:5.2f}x")
                rows.append({"group": case.group, "case": case.name, "desc": case.desc,
                             "variant": label, "seconds": times[label], "speedup": speed,
                             "baseline": case.baseline})
            else:
                why = na[(case.group, case.name)][label]
                print(f"  {label:28} {'n/a':>12}  ({why})")
                rows.append({"group": case.group, "case": case.name, "desc": case.desc,
                             "variant": label, "na": why, "baseline": case.baseline})
    if args.output_json:
        meta = {"python": sys.version.split()[0], "rjson": rjson.__version__,
                "orjson": orjson.__version__, "rounds": rounds, "quick": args.quick}
        with open(args.output_json, "w") as f:
            json.dump({"meta": meta, "results": rows}, f, indent=1)


if __name__ == "__main__":
    main()
