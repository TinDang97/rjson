"""Production-shaped benchmark: rjson vs orjson vs stdlib json.

Where ``corpus_benchmark.py`` times single documents in a tight loop, this
suite models how JSON is used by services and pipelines, and tries hard to
avoid the artifacts that make microbenchmarks flattering:

* Rotating inputs: every loads/dumps case cycles through many distinct
  documents (different values, and for the key-cache cases different keys),
  so neither the CPU caches, the branch predictor nor the parser's dict-key
  cache see the same document twice in a row.
* Key-cache cases: keys-per-row from a fixed schema, from a 100k-key
  vocabulary (more distinct keys than the 2048-entry key cache) and keys
  longer than the 64-byte cache limit.
* Fresh ``str`` inputs: CPython caches a str's UTF-8 form on first use, so
  ``loads(str)`` of the same non-ASCII object is free after the first call.
  The ``*_fresh`` cases decode the bytes inside the timed call (all libs).
* GC stays enabled (fixtures are ``gc.freeze()``-d so the heap under test is
  small), as in a service. ``--no-gc`` turns it off for comparison.
* Interleaved rounds: each round times every variant of a case back to back
  (rotating the order), and the median over rounds is reported, so load on
  the (noisy) host hits all libraries alike. Ratios are what to trust.
* Big files (tens of MB) run in a fresh subprocess per library and op,
  reporting the first-call time and the peak RSS growth (VmHWM after the
  call minus VmRSS before it, with the high-water mark reset through
  ``/proc/self/clear_refs``; tracemalloc misses C allocations).

Every case is validated first: all libraries must parse to equal objects,
round trips must be lossless, and ``rjson.dumps`` is compared byte for byte
with ``orjson.dumps`` (``dumps_str`` with ``orjson.dumps().decode()``).
Mismatches are reported and make the exit status 1.

stdlib ``json`` baselines: loads is ``json.loads`` (bytes input accepted);
dumps is ``json.dumps(obj, ensure_ascii=False, separators=(",", ":"))``
through a pre-built encoder, ``.encode()``-d where a bytes result is
compared (the output equivalent to ``orjson.dumps``).

Usage:
    python benches/production_benchmark.py [--quick] [--only NAME] [--repeat N]
        [--output-json PATH] [--no-big | --big-only] [--no-gc] [--list]
        [--data DIR] [--workdir DIR]

``--only`` matches a substring of the case name or group (comma separated
for several). ``--quick`` shrinks sizes and rounds (< 60 s).
"""

from __future__ import annotations

import argparse
import collections
import gc
import json
import os
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, field
from typing import Any, Callable

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

# ---------------------------------------------------------------------------
# Big-file worker (runs in a fresh interpreter; keep imports minimal)
# ---------------------------------------------------------------------------


def _proc_kb(field_name: str) -> int:
    with open("/proc/self/status") as f:
        for line in f:
            if line.startswith(field_name + ":"):
                return int(line.split()[1])
    return -1


def _reset_hwm() -> bool:
    try:
        with open("/proc/self/clear_refs", "w") as f:
            f.write("5")
        return True
    except OSError:
        return False


def worker(spec: dict[str, Any]) -> dict[str, Any]:
    """One op on one big file in this (fresh) process; returns time + memory."""
    lib, op, kind, path = spec["lib"], spec["op"], spec.get("input", "bytes"), spec["path"]
    import orjson  # used to build the dumps input identically for every lib

    if lib == "rjson":
        import rjson as mod
    elif lib == "orjson":
        mod = orjson
    else:
        mod = json
    with open(path, "rb") as f:
        data = f.read()
    if op == "loads":
        fn = mod.loads
        if kind == "str":
            arg: Any = data.decode()
        elif kind == "memoryview":
            arg = memoryview(data)
        else:
            arg = data
        if kind == "str":
            del data
    else:
        arg = orjson.loads(data)
        del data
        if lib == "json":
            enc = json.JSONEncoder(ensure_ascii=False, separators=(",", ":")).encode
            fn = (lambda o: enc(o)) if op == "dumps_str" else (lambda o: enc(o).encode())
        elif op == "dumps_str":
            fn = mod.dumps_str if lib == "rjson" else (lambda o: orjson.dumps(o).decode())
        else:
            fn = mod.dumps
    gc.collect()
    rss0 = _proc_kb("VmRSS")
    reset = _reset_hwm()
    hwm0 = _proc_kb("VmHWM")
    t0 = time.perf_counter()
    res = fn(arg)
    t1 = time.perf_counter()
    hwm = _proc_kb("VmHWM")
    rss_live = _proc_kb("VmRSS")
    out_len = len(res) if isinstance(res, (bytes, str)) else None
    del res
    gc.collect()
    rss_after = _proc_kb("VmRSS")
    # warm second call (allocator already grown)
    t2 = time.perf_counter()
    res = fn(arg)
    t3 = time.perf_counter()
    del res
    return {
        "cold_s": t1 - t0, "warm_s": t3 - t2, "rss0_kb": rss0, "hwm_kb": hwm,
        "peak_kb": hwm - (rss0 if reset else max(rss0, hwm0)), "hwm_reset": reset,
        "live_kb": rss_live - rss0, "retained_kb": rss_after - rss0, "out_len": out_len,
    }


if __name__ == "__main__" and len(sys.argv) == 3 and sys.argv[1] == "--_worker":
    print(json.dumps(worker(json.loads(sys.argv[2]))))
    sys.exit(0)

# ---------------------------------------------------------------------------
# Main process
# ---------------------------------------------------------------------------

import orjson  # noqa: E402
import rjson  # noqa: E402

import prod_workloads as W  # noqa: E402

_JSON_ENC = json.JSONEncoder(ensure_ascii=False, separators=(",", ":")).encode


def json_dumps_bytes(obj: Any) -> bytes:
    return _JSON_ENC(obj).encode()


LOADS = {"rjson": rjson.loads, "orjson": orjson.loads, "json": json.loads}
DUMPS = {"rjson": rjson.dumps, "orjson": orjson.dumps, "json": json_dumps_bytes}


@dataclass
class Variant:
    label: str  # e.g. "rjson", "orjson", "json", "rjson.dumps_str"
    fn: Callable[[], Any]  # one timed unit of work (a batch)
    lib: str  # rjson / orjson / json / none (baseline)


@dataclass
class Case:
    group: str
    name: str
    op: str
    desc: str
    units: int  # calls per variant invocation (per-call = time / units)
    variants: list[Variant]
    validate: Callable[[], list[str]]  # returns mismatch messages
    size: int = 0  # bytes of JSON per call (for context)
    notes: list[str] = field(default_factory=list)
    # "batch": variant.fn() runs a whole batch (timed in calibrated loops).
    # "fresh": variant.fn(items) with items = factory() rebuilt untimed before
    #          every sample (fresh objects: no CPython UTF-8 caches yet).
    # "percall": variant.fn(x) timed one call at a time, prep() run untimed
    #          before each call (cache eviction, a preceding big dumps, ...).
    mode: str = "batch"
    factory: Callable[[], list[Any]] | None = None
    prep: Callable[[], Any] | None = None
    inputs: list[Any] = field(default_factory=list)


consume = collections.deque(maxlen=0).extend  # run an iterator, dropping each result at once


def o_dec(o: Any) -> str:
    return orjson.dumps(o).decode()


def batch(f: Callable[[Any], Any], items: list[Any]) -> Callable[[], None]:
    def run() -> None:
        for x in items:
            f(x)
    return run


# ---------------------------------------------------------------------------
# Validation helpers
# ---------------------------------------------------------------------------


def check_loads(name: str, inputs: list[Any]) -> list[str]:
    errs = []
    for i, x in enumerate(inputs):
        try:
            r = rjson.loads(x)
        except Exception as e:  # noqa: BLE001 - report anything
            errs.append(f"{name}[{i}]: rjson.loads raised {e!r}")
            continue
        o = orjson.loads(x)
        j = json.loads(x)
        if r != o or r != j:
            errs.append(f"{name}[{i}]: rjson.loads result differs from orjson/json")
    return errs


def check_dumps(name: str, objs: list[Any], expect_identical: bool = True) -> list[str]:
    errs = []
    for i, obj in enumerate(objs):
        try:
            rb = rjson.dumps(obj)
            rs = rjson.dumps_str(obj)
        except Exception as e:  # noqa: BLE001
            errs.append(f"{name}[{i}]: rjson.dumps raised {e!r}")
            continue
        ob = orjson.dumps(obj)
        if not isinstance(rb, bytes) or not isinstance(rs, str):
            errs.append(f"{name}[{i}]: wrong result types {type(rb)}, {type(rs)}")
        if expect_identical and rb != ob:
            errs.append(f"{name}[{i}]: rjson.dumps not byte-identical to orjson.dumps "
                        f"(first diff at {next((k for k, (a, b) in enumerate(zip(rb, ob)) if a != b), min(len(rb), len(ob)))})")
        if rs != rb.decode():
            errs.append(f"{name}[{i}]: dumps_str != dumps().decode()")
        back = json.loads(rb)
        if back != json.loads(ob) or back != obj:
            errs.append(f"{name}[{i}]: rjson.dumps does not round-trip to the input")
    return errs


def check_roundtrip(name: str, objs: list[Any]) -> list[str]:
    errs = []
    for i, obj in enumerate(objs):
        for lib in ("rjson", "orjson", "json"):
            if LOADS[lib](DUMPS[lib](obj)) != obj:
                errs.append(f"{name}[{i}]: {lib} round trip not lossless")
    return errs


# ---------------------------------------------------------------------------
# Case construction
# ---------------------------------------------------------------------------


def loads_case(group: str, name: str, desc: str, inputs: list[Any], extra: list[Variant] | None = None,
               libs: tuple[str, ...] = ("rjson", "orjson", "json")) -> Case:
    variants = [Variant(lib, batch(LOADS[lib], inputs), lib) for lib in libs]
    size = sum(len(x) for x in inputs) // len(inputs)
    return Case(group, name, "loads", desc, len(inputs), variants + (extra or []),
                lambda: check_loads(name, inputs), size)


def dumps_case(group: str, name: str, desc: str, objs: list[Any], with_str: bool = True,
               expect_identical: bool = True) -> Case:
    variants = [Variant(lib, batch(DUMPS[lib], objs), lib) for lib in ("rjson", "orjson", "json")]
    if with_str:
        variants += [
            Variant("rjson.dumps_str", batch(rjson.dumps_str, objs), "rjson"),
            Variant("orjson.dumps().decode()", batch(lambda o: orjson.dumps(o).decode(), objs), "orjson"),
            Variant("json.dumps(str)", batch(_JSON_ENC, objs), "json"),
        ]
    size = sum(len(orjson.dumps(o)) for o in objs) // len(objs)
    return Case(group, name, "dumps", desc, len(objs), variants,
                lambda: check_dumps(name, objs, expect_identical), size)


def build_cases(quick: bool, data_dir: str) -> list[Case]:
    import random

    q = quick
    cases: list[Case] = []
    enc = orjson.dumps

    # ---------------- Web API ----------------
    pages = [W.api_page(s) for s in range(8 if q else 32)]
    page_bytes = [enc(p) for p in pages]
    cases.append(dumps_case("web", "api_page", "REST list page, 50 nested users (~39 KB)", pages))
    cases.append(loads_case("web", "api_page", "REST list page, 50 nested users (~39 KB), bytes input",
                            page_bytes))
    gql = [W.graphql_response(s) for s in range(8 if q else 32)]
    cases.append(dumps_case("web", "graphql", "GraphQL edges/node response, depth ~9 (~30 KB)", gql))
    cases.append(loads_case("web", "graphql", "GraphQL edges/node response (~30 KB)", [enc(g) for g in gql]))
    rng = random.Random(3)
    bodies = [enc(W.request_body(rng)) for _ in range(512)]
    cases.append(loads_case("web", "request_body", "512 distinct ~300 B request bodies, per call", bodies))
    cases.append(loads_case("web", "request_body_str", "same bodies as str (ASCII) input, per call",
                            [b.decode() for b in bodies]))
    resps = [W.small_response(rng) for _ in range(512)]
    cases.append(dumps_case("web", "small_response", "512 distinct ~150 B response dicts, per call", resps))

    # Mixed response sizes: one large response then small ones, as a service
    # sees them (exercises the output-buffer size hint carried between calls).
    big_page = W.api_page(99, per_page=1000)
    mixed = [big_page] + resps[:20]
    cases.append(dumps_case("web", "mixed_sizes", "1 x 780 KB page then 20 x 150 B responses, per call",
                            mixed, with_str=False))
    cases.append(dumps_case("web", "alternating_sizes", "780 KB page / 150 B response alternating, per call",
                            [big_page, resps[0]], with_str=True))

    # Per-call floor: trivially small inputs.
    tiny_l = [b"{}", b"1", b'"ok"', b"[]", b"null", b'{"a":1}'] * 100
    cases.append(loads_case("overhead", "tiny_loads", "600 calls on 2-7 byte documents", tiny_l))
    tiny_d = [{}, 1, "ok", [], None, {"a": 1}] * 100
    cases.append(dumps_case("overhead", "tiny_dumps", "600 calls on trivial objects", tiny_d))

    # ---------------- Logs / NDJSON ----------------
    n_logs = 20_000 if q else 100_000
    recs = W.log_records(n_logs)
    cases.append(Case(
        "logs", "log_ndjson", "dumps",
        f"{n_logs} log records -> dumps each + b'\\n'.join, per record",
        n_logs,
        [
            Variant("rjson", lambda: b"\n".join([rjson.dumps(r) for r in recs]), "rjson"),
            Variant("orjson", lambda: b"\n".join([orjson.dumps(r) for r in recs]), "orjson"),
            Variant("json", lambda: "\n".join([_JSON_ENC(r) for r in recs]).encode(), "json"),
            Variant("rjson.dumps_str", lambda: "\n".join([rjson.dumps_str(r) for r in recs]), "rjson"),
        ],
        lambda: check_dumps("log_ndjson", recs[:5000]) + (
            [] if b"\n".join(map(rjson.dumps, recs)) == b"\n".join(map(orjson.dumps, recs))
            else ["log_ndjson: joined NDJSON blob differs from orjson"]),
        sum(len(enc(r)) for r in recs[:1000]) // 1000,
    ))
    blob = b"\n".join(enc(r) for r in recs)
    lines = blob.split(b"\n")
    cases.append(loads_case("logs", "log_ndjson", f"{n_logs}-line NDJSON blob, loads per line (pre-split)",
                            lines))
    cases.append(Case(
        "logs", "log_ndjson_split", "loads",
        f"{n_logs}-line NDJSON blob, splitlines + loads per line, per record", n_logs,
        [Variant(lib, (lambda f=LOADS[lib]: [f(x) for x in blob.splitlines()]), lib)
         for lib in ("rjson", "orjson", "json")],
        lambda: [] if [rjson.loads(x) for x in lines] == recs else ["log_ndjson_split: records differ"],
        len(blob) // n_logs,
    ))

    # ---------------- Pipelines (in-process sizes) ----------------
    rows = 2000 if q else 5000
    variants_keys = [
        ("keys_fixed", "rows x 10 fixed 10-byte keys (key-cache hits)",
         [W.keyed_rows(rows, 10, W.short_key, 10, seed=s) for s in range(2)]),
        ("keys_hicard", "rows x 10 keys from a 100k vocabulary (key-cache misses)",
         [W.keyed_rows(rows, 10, W.short_key, 100_000, seed=s) for s in range(2)]),
        ("keys_long", "rows x 10 fixed 90-byte keys (> 64 B, never cached)",
         [W.keyed_rows(rows, 10, W.long_key, 10, seed=s) for s in range(2)]),
    ]
    for name, desc, docs in variants_keys:
        cases.append(loads_case("pipeline", name, f"{rows} {desc}", [enc(d) for d in docs]))
        cases.append(dumps_case("pipeline", name, f"{rows} {desc}", docs, with_str=False))
    maps = [W.hicard_map(20_000, off) for off in (0, 20_000)]
    cases.append(loads_case("pipeline", "hicard_map", "one dict with 20k distinct keys user_N",
                            [enc(m) for m in maps]))
    cases.append(dumps_case("pipeline", "hicard_map", "one dict with 20k distinct keys user_N", maps,
                            with_str=False))
    mats = [W.matrix(2000 if q else 5000, seed=s) for s in range(2)]
    cases.append(loads_case("pipeline", "matrix", "list of float rows (16 cols, 6 decimals)",
                            [enc(m) for m in mats]))
    cases.append(dumps_case("pipeline", "matrix", "list of float rows (16 cols, 6 decimals)", mats,
                            with_str=False))
    fl = [W.float_array(50_000 if q else 200_000, seed=s) for s in range(2)]
    cases.append(loads_case("pipeline", "floats", "full-precision doubles, mixed magnitude",
                            [enc(a) for a in fl]))
    cases.append(dumps_case("pipeline", "floats", "full-precision doubles, mixed magnitude", fl,
                            with_str=False))
    ia = [W.int_array(50_000 if q else 200_000, seed=s) for s in range(2)]
    cases.append(loads_case("pipeline", "ints", "ints: 50% small, 40% 31-bit, 10% 64-bit",
                            [enc(a) for a in ia]))
    cases.append(dumps_case("pipeline", "ints", "ints: 50% small, 40% 31-bit, 10% 64-bit", ia,
                            with_str=False))
    ev = [W.big_records(5000 if q else 20000, seed=s) for s in range(2)]
    cases.append(loads_case("pipeline", "events", "event export rows (~180 B each)", [enc(e) for e in ev]))
    cases.append(dumps_case("pipeline", "events", "event export rows (~180 B each)", ev, with_str=False))

    for cname in ("twitter", "citm_catalog", "canada", "github"):
        path = os.path.join(data_dir, cname + ".json")
        if not os.path.exists(path):
            continue
        with open(path, "rb") as f:
            raw = f.read()
        obj = json.loads(raw)
        cases.append(loads_case("corpus", cname, f"{len(raw) // 1024} KB corpus file, bytes input", [raw]))
        cases.append(dumps_case("corpus", cname, f"{len(raw) // 1024} KB corpus file", [obj]))

    # ---------------- Cache / MQ codec ----------------
    for target, count in ((1000, 64), (8000, 32), (48000, 16)):
        objs = [W.blob(target, s) for s in range(count)]
        label = f"codec_{target // 1000}k"
        cases.append(Case(
            "codec", label, "roundtrip", f"loads(dumps(x)) over {count} distinct ~{target // 1000} KB blobs",
            count,
            [Variant(lib, batch(lambda o, d=DUMPS[lib], l=LOADS[lib]: l(d(o)), objs), lib)
             for lib in ("rjson", "orjson", "json")],
            lambda objs=objs, label=label: check_roundtrip(label, objs) + check_dumps(label, objs),
            sum(len(enc(o)) for o in objs) // count,
        ))

    # ---------------- Input-type / output-type paths ----------------
    intl_pages = [W.api_page(1000 + s) for s in range(8 if q else 32)]
    for p in intl_pages:  # make every page non-ASCII so the str is UCS2/UCS4
        p["meta"]["title"] = "Người dùng — ユーザー一覧 🚀"
    ib = [enc(p) for p in intl_pages]
    ab = page_bytes
    extra = [
        Variant("rjson(str)", batch(rjson.loads, [b.decode() for b in ab]), "rjson"),
        Variant("rjson(bytearray)", batch(rjson.loads, [bytearray(b) for b in ab]), "rjson"),
        Variant("rjson(memoryview)", batch(rjson.loads, [memoryview(b) for b in ab]), "rjson"),
        Variant("orjson(str)", batch(orjson.loads, [b.decode() for b in ab]), "orjson"),
        Variant("orjson(memoryview)", batch(orjson.loads, [memoryview(b) for b in ab]), "orjson"),
    ]
    cases.append(loads_case("input", "ascii_page_types", "ASCII api_page: bytes vs str/bytearray/memoryview",
                            ab, extra=extra))
    cached = [b.decode() for b in ib]
    for s in cached:  # pre-populate the UTF-8 cache (what a reused str looks like)
        rjson.loads(s)
    cases.append(loads_case(
        "input", "intl_page_str_cached",
        "non-ASCII api_page as the SAME str objects each round (UTF-8 cached after 1st use)",
        cached, extra=[Variant("rjson(bytes)", batch(rjson.loads, ib), "rjson"),
                       Variant("orjson(bytes)", batch(orjson.loads, ib), "orjson")]))
    cases.append(Case(
        "input", "intl_page_str_fresh", "loads",
        "non-ASCII api_page: bytes.decode() + loads(str) per call (fresh str, as from a framework)",
        len(ib),
        [Variant(lib, batch(lambda b, f=LOADS[lib]: f(b.decode()), ib), lib) for lib in ("rjson", "orjson", "json")]
        + [Variant("decode only", batch(bytes.decode, ib), "none")],
        lambda: check_loads("intl_page_str_fresh", [b.decode() for b in ib]),
        sum(map(len, ib)) // len(ib),
    ))
    cases.append(dumps_case("output", "intl_page", "non-ASCII api_page: dumps vs dumps_str", intl_pages))
    cjk = [[("日本語のテキスト " * 8 + str(i)) for i in range(2000)] for _ in range(2)]
    cases.append(dumps_case("output", "cjk_strings", "2000 CJK strings (UCS2 source)", cjk))
    cases.append(loads_case("output", "cjk_strings", "2000 CJK strings", [enc(c) for c in cjk]))

    # ---------------- Fresh objects (no UTF-8 cache on the source strs) ----------------
    # Serializing a non-ASCII str to UTF-8 bytes attaches CPython's UTF-8
    # cache to it, so re-serializing the same objects (every other case) only
    # measures a memcpy. Real payloads are usually built fresh (from a DB row,
    # a template, loads, ...): these cases rebuild the objects before every
    # sample (untimed) with orjson.loads, which does not attach caches.
    def fresh_dumps_case(name: str, desc: str, src: list[bytes], with_str: bool = True) -> Case:
        variants = [Variant(lib, (lambda items, f=DUMPS[lib]: consume(map(f, items))), lib)
                    for lib in ("rjson", "orjson", "json")]
        if with_str:
            variants += [
                Variant("rjson.dumps_str", lambda items: consume(map(rjson.dumps_str, items)), "rjson"),
                Variant("orjson.dumps().decode()", lambda items: consume(o_dec(o) for o in items),
                        "orjson"),
            ]
        return Case("fresh", name, "dumps", desc, len(src), variants,
                    lambda: check_dumps(name, [orjson.loads(b) for b in src]),
                    sum(map(len, src)) // len(src), mode="fresh",
                    factory=lambda: [orjson.loads(b) for b in src])

    cases.append(fresh_dumps_case("intl_page_fresh", "non-ASCII api_page, freshly built objects", ib))
    cases.append(fresh_dumps_case("cjk_fresh", "2000 CJK strings, freshly built", [enc(c) for c in cjk]))
    n_fresh = 5000 if q else 20000
    cases.append(fresh_dumps_case("log_fresh", f"{n_fresh} log records (8% non-ASCII), freshly built",
                                  lines[:n_fresh]))

    # ---------------- Number shapes ----------------
    zf = [W.sparse_floats(50_000 if q else 200_000, seed=s) for s in range(2)]
    cases.append(loads_case("pipeline", "floats_zeros", "metrics floats: 50% 0.0, rest 2-decimal",
                            [enc(a) for a in zf]))
    cases.append(dumps_case("pipeline", "floats_zeros", "metrics floats: 50% 0.0, rest 2-decimal", zf,
                            with_str=False))

    # ---------------- Cold caches / call-to-call state (one call per sample) ----------------
    ev_src = bytes(4 << 20)
    ev_dst = bytearray(4 << 20)

    def evict() -> None:  # stream 8 MB through the caches, as other request work would
        ev_dst[:] = ev_src

    def percall(name: str, op: str, desc: str, inputs: list[Any], prep: Callable[[], Any],
                libs: dict[str, Callable[[Any], Any]]) -> Case:
        size = sum(len(x) if isinstance(x, bytes) else len(enc(x)) for x in inputs[:50]) // min(len(inputs), 50)
        return Case("percall", name, op, desc, 1, [Variant(k, f, k.split(".")[0]) for k, f in libs.items()],
                    lambda: [], size, mode="percall", prep=prep, inputs=inputs)

    cases.append(percall("cold_request_body", "loads", "~300 B body, caches evicted before each call",
                         bodies, evict, LOADS))
    cases.append(percall("cold_small_response", "dumps", "~150 B response, caches evicted before each call",
                         resps, evict, DUMPS))
    cases.append(percall("cold_api_page", "loads", "39 KB api_page, caches evicted before each call",
                         page_bytes, evict, LOADS))
    cases.append(percall("cold_api_page", "dumps", "39 KB api_page, caches evicted before each call",
                         pages, evict, DUMPS))

    def after_big() -> None:  # every library's per-call state now reflects a big result
        rjson.dumps(big_page)
        rjson.dumps_str(big_page)
        orjson.dumps(big_page)

    three = {"rjson": rjson.dumps, "orjson": orjson.dumps, "rjson.dumps_str": rjson.dumps_str}
    cases.append(percall("small_after_big", "dumps", "150 B response right after a 780 KB dumps",
                         resps, after_big, three))

    def after_small() -> None:
        rjson.dumps(resps[0])
        rjson.dumps_str(resps[0])
        orjson.dumps(resps[0])

    cases.append(percall("big_after_small", "dumps", "780 KB page right after a 150 B dumps",
                         [big_page], after_small, three))
    return cases


def side_effects() -> list[dict[str, Any]]:
    """Memory side effects that timing does not show (informational)."""
    import tracemalloc

    rows = []

    def fresh_cjk() -> list[str]:
        return [("日本語のテキスト " * 8 + str(i)) for i in range(2000)]

    for label, f in (("rjson.dumps", rjson.dumps), ("rjson.dumps_str", rjson.dumps_str),
                     ("orjson.dumps", orjson.dumps), ("json.dumps", json_dumps_bytes)):
        xs = fresh_cjk()
        before = sum(map(sys.getsizeof, xs))
        f(xs)
        after = sum(map(sys.getsizeof, xs))
        rows.append({"what": "dumps: growth of the non-ASCII source strs (UTF-8 cache attached)",
                     "lib": label, "value": f"{(after - before) / before * 100:+.0f}%"})
    for label, f in (("rjson.loads", rjson.loads), ("orjson.loads", orjson.loads), ("json.loads", json.loads)):
        doc = orjson.dumps(fresh_cjk()).decode()
        before = sys.getsizeof(doc)
        f(doc)
        rows.append({"what": "loads(str): growth of the non-ASCII input str",
                     "lib": label, "value": f"{(sys.getsizeof(doc) - before) / before * 100:+.0f}%"})
    rec = W.log_records(1)[0]
    for label, f in (("rjson.dumps", rjson.dumps), ("orjson.dumps", orjson.dumps),
                     ("rjson.dumps_str", rjson.dumps_str), ("json.dumps", json_dumps_bytes)):
        f(rec)  # warm any per-call size hint
        tracemalloc.start()
        out = f(rec)
        cur, _peak = tracemalloc.get_traced_memory()
        tracemalloc.stop()
        rows.append({"what": f"bytes allocated per retained {len(out)} B result (e.g. batching NDJSON lines)",
                     "lib": label, "value": f"{cur} B"})
    return rows


# ---------------------------------------------------------------------------
# Timing
# ---------------------------------------------------------------------------


def calibrate(fn: Callable[[], Any], target: float) -> int:
    n = 1
    while True:
        t0 = time.perf_counter()
        for _ in range(n):
            fn()
        dt = time.perf_counter() - t0
        if dt >= target / 4 or n >= 1 << 20:
            return max(1, round(n * target / max(dt, 1e-9)))
        n *= 2


def time_case_fresh(case: Case, rounds: int) -> dict[str, list[float]]:
    assert case.factory is not None
    samples: dict[str, list[float]] = {v.label: [] for v in case.variants}
    k = len(case.variants)
    for r in range(rounds * 3):
        for j in range(k):
            v = case.variants[(j + r) % k]
            items = case.factory()
            t0 = time.perf_counter()
            v.fn(items)
            samples[v.label].append((time.perf_counter() - t0) / case.units)
            del items
    return samples


def time_case_percall(case: Case, rounds: int, per_round: int) -> dict[str, list[float]]:
    prep = case.prep or (lambda: None)
    pc = time.perf_counter_ns
    # Timer + prep bookkeeping overhead, subtracted from every sample.
    ov = []
    for _ in range(200):
        prep()
        t0 = pc()
        t1 = pc()
        ov.append(t1 - t0)
    overhead = statistics.median(ov)
    samples: dict[str, list[float]] = {v.label: [] for v in case.variants}
    xs = case.inputs
    k = len(case.variants)
    for r in range(rounds):
        for j in range(k):
            v = case.variants[(j + r) % k]
            f = v.fn
            out = samples[v.label]
            for i in range(per_round):
                x = xs[i % len(xs)]
                prep()
                t0 = pc()
                f(x)
                t1 = pc()
                out.append(max(t1 - t0 - overhead, 1) * 1e-9)
    return samples


def time_case(case: Case, rounds: int, target: float) -> dict[str, list[float]]:
    if case.mode == "fresh":
        return time_case_fresh(case, rounds)
    if case.mode == "percall":
        return time_case_percall(case, rounds, 100 if target < 0.05 else 300)
    ns = {v.label: calibrate(v.fn, target) for v in case.variants}
    samples: dict[str, list[float]] = {v.label: [] for v in case.variants}
    k = len(case.variants)
    for r in range(rounds):
        for j in range(k):
            v = case.variants[(j + r) % k]
            n = ns[v.label]
            t0 = time.perf_counter()
            for _ in range(n):
                v.fn()
            samples[v.label].append((time.perf_counter() - t0) / (n * case.units))
    return samples


def fmt_t(t: float | None) -> str:
    if t is None:
        return "-"
    if t < 1e-6:
        return f"{t * 1e9:.0f} ns"
    if t < 1e-3:
        return f"{t * 1e6:.2f} us"
    if t < 1:
        return f"{t * 1e3:.2f} ms"
    return f"{t:.2f} s"


def spread(xs: list[float]) -> float:
    if len(xs) < 3:
        return 0.0
    q = statistics.quantiles(xs, n=4)
    return (q[2] - q[0]) / statistics.median(xs)


# ---------------------------------------------------------------------------
# Big files (subprocess, time + peak RSS)
# ---------------------------------------------------------------------------


def big_fixtures(quick: bool, workdir: str) -> list[tuple[str, str, str]]:
    """(name, description, path) of big JSON files, generated once and cached."""
    os.makedirs(workdir, exist_ok=True)
    scale = 10 if quick else 1
    specs = [
        ("big_records", f"{500_000 // scale} event rows, ~10% non-ASCII labels",
         lambda: W.big_records(500_000 // scale)),
        ("big_floats", f"{5_000_000 // scale} doubles", lambda: W.float_array(5_000_000 // scale)),
        ("big_ints", f"{5_000_000 // scale} mixed-width ints", lambda: W.int_array(5_000_000 // scale)),
        ("big_hicard", f"one dict with {1_000_000 // scale} distinct keys",
         lambda: W.hicard_map(1_000_000 // scale, 0)),
    ]
    out = []
    for name, desc, gen in specs:
        path = os.path.join(workdir, f"{name}-v{W.GENERATOR_VERSION}-{'q' if quick else 'f'}.json")
        if not os.path.exists(path):
            obj = gen()
            tmp = path + ".tmp"
            with open(tmp, "wb") as f:
                f.write(orjson.dumps(obj))
            os.replace(tmp, path)
            del obj
        out.append((name, desc, path))
    return out


BIG_OPS = [
    ("loads", "rjson", "bytes"), ("loads", "orjson", "bytes"), ("loads", "json", "bytes"),
    ("loads", "rjson", "str"), ("loads", "orjson", "str"),
    ("loads", "rjson", "memoryview"), ("loads", "orjson", "memoryview"),
    ("dumps", "rjson", None), ("dumps", "orjson", None), ("dumps", "json", None),
    ("dumps_str", "rjson", None), ("dumps_str", "orjson", None),
]


def run_big(fixtures: list[tuple[str, str, str]], reps: int, only: list[str] | None) -> list[dict[str, Any]]:
    rows = []
    for name, desc, path in fixtures:
        if only and not any(o in name or o == "big" for o in only):
            continue
        size = os.path.getsize(path)
        ops = BIG_OPS if name == "big_records" else [o for o in BIG_OPS if o[2] in ("bytes", None)
                                                       and o[0] != "dumps_str"]
        results: dict[tuple, list[dict]] = {o: [] for o in ops}
        for r in range(reps):
            for o in ops[r % len(ops):] + ops[:r % len(ops)]:
                spec = {"op": o[0], "lib": o[1], "input": o[2] or "bytes", "path": path}
                p = subprocess.run([sys.executable, os.path.abspath(__file__), "--_worker", json.dumps(spec)],
                                   capture_output=True, text=True, check=False)
                if p.returncode != 0:
                    print(f"  worker failed {name} {o}: {p.stderr.strip()[-300:]}", file=sys.stderr)
                    continue
                results[o].append(json.loads(p.stdout))
        for o, rs in results.items():
            if not rs:
                continue
            rows.append({
                "case": name, "desc": desc, "size": size, "op": o[0], "lib": o[1], "input": o[2],
                "cold_s": statistics.median(x["cold_s"] for x in rs),
                "warm_s": statistics.median(x["warm_s"] for x in rs),
                "peak_mb": statistics.median(x["peak_kb"] for x in rs) / 1024,
                "live_mb": statistics.median(x["live_kb"] for x in rs) / 1024,
                "retained_mb": statistics.median(x["retained_kb"] for x in rs) / 1024,
                "out_len": rs[0]["out_len"], "hwm_reset": rs[0]["hwm_reset"], "n": len(rs),
            })
        print_big(rows, name)
    return rows


def print_big(rows: list[dict[str, Any]], name: str) -> None:
    sel = [r for r in rows if r["case"] == name]
    if not sel:
        return
    print(f"\n{name}: {sel[0]['desc']} ({sel[0]['size'] / 1e6:.1f} MB)")
    print(f"  {'op':10} {'lib':7} {'input':10} {'cold':>9} {'warm':>9} {'vs orjson':>9} "
          f"{'peak MB':>8} {'live MB':>8} {'kept MB':>8}")
    ref = {(r["op"], r["input"]): r for r in sel if r["lib"] == "orjson"}
    for r in sel:
        o = ref.get((r["op"], r["input"]))
        ratio = r["cold_s"] / o["cold_s"] if o else float("nan")
        print(f"  {r['op']:10} {r['lib']:7} {str(r['input'] or '-'):10} {fmt_t(r['cold_s']):>9} "
              f"{fmt_t(r['warm_s']):>9} {ratio:9.2f} {r['peak_mb']:8.1f} {r['live_mb']:8.1f} "
              f"{r['retained_mb']:8.1f}")


# ---------------------------------------------------------------------------
# Behaviour probes (informational: differences from json / orjson)
# ---------------------------------------------------------------------------


def probes() -> list[dict[str, str]]:
    import collections
    import enum

    class S(str):
        pass

    class I(int):  # noqa: E742
        pass

    class E(enum.IntEnum):
        A = 1

    def nested(n: int) -> list:
        x: list = []
        for _ in range(n - 1):
            x = [x]
        return x

    def outcome(f: Callable[[], Any]) -> str:
        try:
            v = f()
        except Exception as e:  # noqa: BLE001
            return f"{type(e).__name__}"
        if isinstance(v, bytes):
            v = v.decode("utf-8", "replace")
        s = repr(v)
        return s if len(s) <= 40 else s[:37] + "..."

    def jd(o: Any) -> str:
        return json.dumps(o, ensure_ascii=False, separators=(",", ":"))

    load_inputs = [
        b"18446744073709551616", b"-9223372036854775809", b"123456789012345678901234567890",
        b"1e400", b"-0.0", b"[1.0e]", b"NaN", b"[1,]", b'{"a":1,"a":2}', b"\xef\xbb\xbf{}",
        b'"\\ud800"', b'"\xff"', b"[" * 1100 + b"]" * 1100, b" ", b"1 2", b'"\x01"',
    ]
    dump_inputs = [
        ("2**64", 2**64), ("-(2**63)-1", -(2**63) - 1), ("nan", float("nan")), ("tuple", (1, 2)),
        ("int key", {1: 2}), ("str subclass", S("x")), ("int subclass", I(5)), ("IntEnum", E.A),
        ("OrderedDict", collections.OrderedDict(a=1)), ("lone surrogate", "\ud800"),
        ("depth 300", nested(300)), ("set", {1}), ("bytes", b"x"),
        ("datetime", __import__("datetime").datetime(2024, 1, 1)),
    ]
    rows = []
    for x in load_inputs:
        r = {lib: outcome(lambda f=f: f(x)) for lib, f in LOADS.items()}
        rows.append({"op": "loads", "input": repr(x)[:40], **r})
    for label, x in dump_inputs:
        r = {"rjson": outcome(lambda: rjson.dumps(x)), "orjson": outcome(lambda: orjson.dumps(x)),
             "json": outcome(lambda: jd(x))}
        rows.append({"op": "dumps", "input": label, **r})
    return rows


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quick", action="store_true", help="smaller sizes and fewer rounds (< 60 s)")
    ap.add_argument("--only", help="comma-separated substrings of case names/groups")
    ap.add_argument("--repeat", type=int, help="interleaved rounds per case (default 9, quick 3)")
    ap.add_argument("--big-repeat", type=int, help="subprocess runs per big-file op (default 3, quick 1)")
    ap.add_argument("--output-json", metavar="PATH")
    ap.add_argument("--no-big", action="store_true", help="skip the big-file subprocess section")
    ap.add_argument("--big-only", action="store_true")
    ap.add_argument("--no-gc", action="store_true", help="disable the cyclic GC while timing")
    ap.add_argument("--list", action="store_true", help="list cases and exit")
    ap.add_argument("--data", default=os.environ.get("RJSON_BENCH_DATA", os.path.join(HERE, "data")))
    ap.add_argument("--workdir", default=os.path.join(
        os.environ.get("TMPDIR", "/tmp"), "rjson-prodbench"), help="where big-file fixtures are cached")
    args = ap.parse_args()
    if not os.path.isdir(args.data) and os.path.isdir("/home/user/rjson/benches/data"):
        args.data = "/home/user/rjson/benches/data"
    rounds = args.repeat or (3 if args.quick else 9)
    target = 0.02 if args.quick else 0.06
    only = [o for o in args.only.split(",") if o] if args.only else None
    t_start = time.perf_counter()

    cases: list[Case] = []
    if not args.big_only:
        cases = build_cases(args.quick, args.data)
        if only:
            cases = [c for c in cases if any(o in c.name or o == c.group for o in only)]
    if args.list:
        for c in cases:
            print(f"{c.group:9} {c.name:22} {c.op:9} {c.desc}")
        print("big-file (subprocess): big_records big_floats big_ints big_hicard (--only big)")
        return 0

    mismatches: list[str] = []
    for c in cases:
        errs = c.validate()
        mismatches += errs
    probe_rows = probes() if not args.big_only else []
    effect_rows = side_effects() if not args.big_only else []

    gc.collect()
    gc.freeze()
    if args.no_gc:
        gc.disable()

    rows = []
    print(f"Python {sys.version.split()[0]}, orjson {orjson.__version__}, rounds={rounds}, "
          f"gc={'off' if args.no_gc else 'on'}; times are median per call; ratio < 1 means rjson faster")
    hdr = f"{'group':9} {'case':22} {'op':9} {'variant':25} {'per call':>10} {'vs orjson':>9} {'vs json':>8} {'iqr':>5}"
    print(hdr)
    print("-" * len(hdr))
    geo: dict[str, list[float]] = {}
    for c in cases:
        s = time_case(c, rounds, target)
        med = {k: statistics.median(v) for k, v in s.items()}
        o, j = med.get("orjson"), med.get("json")
        for v in c.variants:
            t = med[v.label]
            ro = t / o if o else None
            rj = t / j if j else None
            if v.label == "rjson":
                geo.setdefault(c.group, []).append(ro)
                geo.setdefault("ALL " + c.op, []).append(ro)
            flag = "  <-- slower" if v.lib == "rjson" and ro and ro > 1.05 else ""
            print(f"{c.group:9} {c.name:22} {c.op:9} {v.label:25} {fmt_t(t):>10} "
                  f"{(f'{ro:.2f}' if ro else '-'):>9} {(f'{rj:.3f}' if rj else '-'):>8} "
                  f"{spread(s[v.label]) * 100:4.0f}%{flag}")
            rows.append({"group": c.group, "case": c.name, "op": c.op, "desc": c.desc, "variant": v.label,
                         "lib": v.lib, "per_call_s": t, "ratio_orjson": ro, "ratio_json": rj,
                         "iqr_rel": spread(s[v.label]), "units": c.units, "bytes_per_call": c.size,
                         "samples": s[v.label]})
    geomeans = {k: statistics.geometric_mean(v) for k, v in geo.items() if v}
    if geomeans:
        print()
        for k, g in sorted(geomeans.items()):
            print(f"geomean rjson/orjson {k:14}: {g:.3f}")

    big_rows: list[dict[str, Any]] = []
    if not args.no_big and (not only or any(o.startswith("big") for o in only) or args.big_only):
        gc.unfreeze()
        gc.enable()
        big_rows = run_big(big_fixtures(args.quick, args.workdir),
                           args.big_repeat or (1 if args.quick else 3), only)

    if probe_rows:
        print("\nbehaviour probes (informational):")
        print(f"  {'op':5} {'input':42} {'rjson':28} {'orjson':28} {'json':28}")
        for p in probe_rows:
            mark = " *" if p["rjson"] != p["orjson"] and p["rjson"] != p["json"] else ""
            print(f"  {p['op']:5} {p['input']:42} {p['rjson']:28} {p['orjson']:28} {p['json']:28}{mark}")

    if effect_rows:
        print("\nmemory side effects (informational):")
        for e in effect_rows:
            print(f"  {e['what'][:80]:80} {e['lib']:16} {e['value']}")

    print(f"\nvalidation: {len(mismatches)} mismatch(es)")
    for m in mismatches[:50]:
        print("  MISMATCH", m)
    print(f"total wall time {time.perf_counter() - t_start:.1f} s")

    if args.output_json:
        meta = {"python": sys.version.split()[0], "orjson": orjson.__version__, "rounds": rounds,
                "quick": args.quick, "gc": not args.no_gc, "platform": sys.platform}
        with open(args.output_json, "w") as f:
            json.dump({"meta": meta, "results": rows, "geomean": geomeans, "big": big_rows,
                       "probes": probe_rows, "side_effects": effect_rows,
                       "mismatches": mismatches}, f, indent=1)
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
