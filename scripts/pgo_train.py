"""PGO training workload for rjson (used by scripts/build_pgo.sh).

Deliberately disjoint from the benchmark (benches/corpus_benchmark.py): it
reads no corpus files and none of the benchmark's synthetic payloads, so the
PGO gain measured on the benchmark is not the result of training on it. The
documents are generated from a fixed seed and cover the same *code paths* with
different data and shapes:

- records of several schemas (orders, log events, users, metrics) with
  varied key sets, optional fields and nesting; keys that hit and miss the
  parser's key cache (including keys over 64 bytes)
- ints of every width (small, 32/64-bit, > 64-bit), floats of varied
  precision and exponent, integer-valued floats, -0.0
- mostly ASCII documents (like most real JSON); Latin-1, BMP (CJK/Cyrillic)
  and astral (emoji) strings in some; strings that need escaping; JSON inputs
  with \\uXXXX escapes and surrogate pairs
- compact and indented JSON inputs; str, bytes, bytearray and memoryview
- deep nesting, wide dicts, empty containers, tiny documents
- subclasses (IntEnum, OrderedDict, str subclass, tuples, namedtuples)
- error paths, touched a few times so they are laid out as cold

Every training step (a document with dumps, dumps_str, or loads of one of its
JSON texts) runs a fixed number of calls from ``CALLS``, never a time budget:
with 0.2 s per step, the call counts (up to 32% apart between two runs), the
profile and with it the optimized code layout changed with host load; two
builds of one commit differed by 18% on a benchmark case (PERFORMANCE_REVIEW.md
§4 item 19). ``CALLS`` keeps the weights of that time budget: it holds the
median counts of ``python scripts/pgo_train.py --calibrate`` (3 time-budgeted
runs on an instrumented build, see ``calibrate``); regenerate it when documents
change. ``RJSON_PGO_SCALE`` (default 1) multiplies every count.
"""

import collections
import enum
import json
import os
import random
import sys
import time

import rjson

SCALE = float(os.environ.get("RJSON_PGO_SCALE", "1"))
CALIBRATE_SECONDS = 0.2  # per step, the former time budget

# Calls per training step: median of 3 `--calibrate` runs on an instrumented
# CPython 3.13 x86-64 build (0.4.0), i.e. what 0.2 s per step did on that host.
CALLS = {
    ('orders', 'dumps'): 249,
    ('orders', 'dumps_str'): 248,
    ('orders', 'loads_compact'): 57,
    ('orders', 'loads_indent'): 50,
    ('orders', 'loads_ascii'): 71,
    ('orders', 'loads_bytes'): 73,
    ('logs', 'dumps'): 263,
    ('logs', 'dumps_str'): 279,
    ('logs', 'loads_compact'): 73,
    ('logs', 'loads_indent'): 64,
    ('logs', 'loads_bytes'): 72,
    ('users', 'dumps'): 984,
    ('users', 'dumps_str'): 619,
    ('users', 'loads_compact'): 124,
    ('users', 'loads_indent'): 114,
    ('users', 'loads_ascii'): 135,
    ('users', 'loads_bytes'): 156,
    ('metrics', 'dumps'): 489,
    ('metrics', 'dumps_str'): 478,
    ('metrics', 'loads_compact'): 212,
    ('metrics', 'loads_indent'): 166,
    ('metrics', 'loads_bytes'): 218,
    ('geometry', 'dumps'): 130,
    ('geometry', 'dumps_str'): 130,
    ('geometry', 'loads_compact'): 68,
    ('geometry', 'loads_indent'): 58,
    ('geometry', 'loads_bytes'): 71,
    ('unique_keys', 'dumps'): 2099,
    ('unique_keys', 'dumps_str'): 1954,
    ('unique_keys', 'loads_compact'): 327,
    ('unique_keys', 'loads_indent'): 311,
    ('unique_keys', 'loads_bytes'): 327,
    ('wide', 'dumps'): 29198,
    ('wide', 'dumps_str'): 29010,
    ('wide', 'loads_compact'): 6902,
    ('wide', 'loads_indent'): 6099,
    ('wide', 'loads_bytes'): 6821,
    ('numbers', 'dumps'): 161,
    ('numbers', 'dumps_str'): 164,
    ('numbers', 'loads_compact'): 96,
    ('numbers', 'loads_indent'): 82,
    ('numbers', 'loads_bytes'): 92,
    ('texts', 'dumps'): 3616,
    ('texts', 'dumps_str'): 3615,
    ('texts', 'loads_compact'): 931,
    ('texts', 'loads_indent'): 855,
    ('texts', 'loads_bytes'): 1010,
    ('intl_texts', 'dumps'): 3603,
    ('intl_texts', 'dumps_str'): 1201,
    ('intl_texts', 'loads_compact'): 288,
    ('intl_texts', 'loads_indent'): 271,
    ('intl_texts', 'loads_ascii'): 273,
    ('intl_texts', 'loads_bytes'): 422,
    ('nested', 'dumps'): 1675,
    ('nested', 'dumps_str'): 1648,
    ('nested', 'loads_compact'): 433,
    ('nested', 'loads_indent'): 331,
    ('nested', 'loads_bytes'): 432,
    ('tiny_obj', 'dumps'): 996901,
    ('tiny_obj', 'dumps_str'): 1017291,
    ('tiny_obj', 'loads_compact'): 691034,
    ('tiny_obj', 'loads_indent'): 640021,
    ('tiny_obj', 'loads_bytes'): 663080,
    ('tiny_list', 'dumps'): 1054247,
    ('tiny_list', 'dumps_str'): 1043899,
    ('tiny_list', 'loads_compact'): 818056,
    ('tiny_list', 'loads_indent'): 635713,
    ('tiny_list', 'loads_bytes'): 760034,
    ('empties', 'dumps'): 750349,
    ('empties', 'dumps_str'): 736897,
    ('empties', 'loads_compact'): 447087,
    ('empties', 'loads_indent'): 397760,
    ('empties', 'loads_bytes'): 432578,
    ('scalar_str', 'dumps'): 1219968,
    ('scalar_str', 'dumps_str'): 1143981,
    ('scalar_str', 'loads_compact'): 1087373,
    ('scalar_str', 'loads_indent'): 1179588,
    ('scalar_str', 'loads_bytes'): 1113693,
    ('scalar_int', 'dumps'): 1252106,
    ('scalar_int', 'dumps_str'): 1247999,
    ('scalar_int', 'loads_compact'): 1118947,
    ('scalar_int', 'loads_indent'): 1220951,
    ('scalar_int', 'loads_bytes'): 1199155,
    ('subclasses', 'dumps'): 80692,
    ('subclasses', 'dumps_str'): 77644,
}

rng = random.Random(0x5EED)

WORDS_ASCII = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor".split()
WORDS_LATIN1 = "café naïve façade señor jalapeño crème brûlée über straße".split()
WORDS_BMP = "東京 大阪 数据 Привет мир γειά σου שלום".split()
WORDS_ASTRAL = ["🚀", "🎉", "𝄞", "🧪", "👍🏽"]
ESCAPES = ['"', "\\", "\n", "\t", "\r", "\x00", "\x1f", "/", "\b", "\f"]


def text(n_words, alphabet):
    return " ".join(rng.choice(alphabet) for _ in range(n_words))


def any_text():
    """Mostly ASCII, like most real-world JSON; 5% need escaping."""
    if rng.random() < 0.95:
        return text(rng.randint(1, 12), WORDS_ASCII)
    return "".join(rng.choice(WORDS_ASCII + ESCAPES) for _ in range(rng.randint(2, 20)))


def intl_text():
    """Non-ASCII text: Latin-1 (UCS1), BMP (UCS2) and astral (UCS4)."""
    r = rng.random()
    if r < 0.4:
        return text(rng.randint(1, 8), WORDS_ASCII + WORDS_LATIN1)
    if r < 0.8:
        return text(rng.randint(1, 8), WORDS_ASCII + WORDS_BMP)
    return text(rng.randint(1, 6), WORDS_ASCII + WORDS_ASTRAL)


def any_int():
    r = rng.random()
    if r < 0.6:
        return rng.randint(-100, 1000)
    if r < 0.9:
        return rng.randint(-(2**31), 2**31)
    if r < 0.99:
        return rng.randint(-(2**63), 2**64 - 1)
    return rng.randint(2**64, 2**100) * rng.choice((1, -1))


def any_float():
    r = rng.random()
    if r < 0.6:  # prices, measurements: few decimals
        return round(rng.uniform(-1000, 1000), rng.randint(1, 6))
    if r < 0.85:  # computed values: full 15-17 digit precision
        return rng.uniform(-1, 1) * 10 ** rng.randint(-8, 8)
    if r < 0.95:
        return float(rng.randint(-(10**6), 10**6))
    if r < 0.98:
        return rng.uniform(0, 1) * 10 ** rng.randint(-300, 300)
    return rng.choice((0.0, -0.0, 1e-7, 1e16, 5e-324, 1.7976931348623157e308))


def order(i):
    doc = {
        "orderId": f"ORD-{i:07d}",
        "customer": {"id": rng.randint(1, 10**6), "tier": rng.choice(("gold", "silver", "none"))},
        "placedAt": f"2025-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}T{rng.randint(0, 23):02d}:00:00Z",
        "lines": [
            {"sku": f"SKU{rng.randint(1000, 9999)}", "qty": rng.randint(1, 5), "unitPrice": round(rng.uniform(1, 500), 2)}
            for _ in range(rng.randint(1, 6))
        ],
        "total": round(rng.uniform(5, 3000), 2),
        "paid": rng.random() < 0.8,
        "notes": any_text() if rng.random() < 0.3 else None,
    }
    if rng.random() < 0.4:
        doc["shipping"] = {"city": text(1, WORDS_LATIN1 + WORDS_BMP), "zip": str(rng.randint(10000, 99999))}
    return doc


def log_event(i):
    return {
        "ts": 1.7e9 + i * 0.001337,
        "level": rng.choice(("DEBUG", "INFO", "WARN", "ERROR")),
        "logger": rng.choice(("http.server", "db.pool", "auth", "worker.queue")),
        "message": any_text(),
        "ctx": {"requestId": "%032x" % rng.getrandbits(128), "attempt": rng.randint(0, 3), "latencyMs": any_float()},
        "labels": [text(1, WORDS_ASCII) for _ in range(rng.randint(0, 4))],
    }


def user(i):
    return {
        "user_id": any_int(),
        "handle": f"@{text(1, WORDS_ASCII)}{i}",
        "display": intl_text() if rng.random() < 0.5 else any_text(),
        "followers": rng.randint(0, 10**7),
        "verified": rng.random() < 0.1,
        "location": None if rng.random() < 0.5 else text(2, WORDS_LATIN1 + WORDS_BMP),
        "bio": intl_text() if rng.random() < 0.3 else any_text(),
    }


def metric_series(n):
    return {
        "name": "cpu.load",
        "host": f"node-{rng.randint(1, 64)}",
        "points": [[1_700_000_000 + 10 * k, any_float()] for k in range(n)],
    }


def geometry(n):
    lon, lat = rng.uniform(-180, 180), rng.uniform(-80, 80)
    ring = []
    for _ in range(n):
        lon += rng.uniform(-0.01, 0.01)
        lat += rng.uniform(-0.01, 0.01)
        ring.append([round(lon, rng.randint(5, 9)), round(lat, rng.randint(5, 9))])
    return {"type": "Polygon", "coordinates": [ring]}


def unique_keys(n):
    # Many distinct keys (key cache misses), some longer than 64 bytes.
    return {f"k{rng.getrandbits(40):x}" + ("_" * rng.choice((0, 0, 70))): any_int() for _ in range(n)}


def nested(depth):
    doc = {"leaf": any_text()}
    for d in range(depth):
        doc = {"level": d, "child": doc} if d % 2 else [doc, d, None]
    return doc


class Color(enum.IntEnum):
    RED = 1
    GREEN = 2


Point = collections.namedtuple("Point", "x y")


class Tag(str):
    pass


def subclasses():
    return collections.OrderedDict(
        color=Color.GREEN,
        pts=[Point(any_float(), any_float()) for _ in range(20)],
        tup=tuple(range(20)),
        tags=[Tag(any_text()) for _ in range(20)],
        nums=[True, False, None, 0, -1],
    )


def documents():
    """name -> python object"""
    return {
        "orders": [order(i) for i in range(1500)],
        "logs": [log_event(i) for i in range(3000)],
        "users": [user(i) for i in range(1500)],
        "metrics": [metric_series(200) for _ in range(30)],
        "geometry": geometry(20000),
        "unique_keys": [unique_keys(40) for _ in range(100)],
        "wide": {f"field_{i}": any_int() if i % 3 else any_text() for i in range(400)},
        "numbers": [any_int() if rng.random() < 0.5 else any_float() for _ in range(40000)],
        "texts": [any_text() for _ in range(4000)],
        "intl_texts": [intl_text() for _ in range(4000)],
        "nested": [nested(60) for _ in range(50)],
        "tiny_obj": {"ok": True, "n": 7},
        "tiny_list": [1, "a", None],
        "empties": [{}, [], "", 0, [[]], {"a": {}}],
        "scalar_str": "rjson",
        "scalar_int": 123456789,
        "subclasses": subclasses(),
    }


def inputs(obj):
    """JSON texts for loads: compact, indented, ASCII-escaped (\\uXXXX)."""
    compact = json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
    out = [compact, json.dumps(obj, ensure_ascii=False, indent=2)]
    if not compact.isascii():
        out.append(json.dumps(obj, ensure_ascii=True))
    return out


def steps(docs):
    """(key, function, argument) of every training step, in a fixed order."""
    for name, obj in docs.items():
        for fn in (rjson.dumps, rjson.dumps_str):
            yield (name, fn.__name__), fn, obj
        if name == "subclasses":
            continue
        texts = inputs(obj)
        for kind, t in zip(("compact", "indent", "ascii"), texts):
            yield (name, "loads_" + kind), rjson.loads, t
        # bytes is the common non-str input; the others share its code path
        yield (name, "loads_bytes"), rjson.loads, texts[0].encode()


def fallback_calls(fn, arg):
    """Calls for a step missing from CALLS (a new document before the next
    --calibrate): a deterministic size-based estimate."""
    size = len(rjson.dumps(arg)) if fn is not rjson.loads else len(arg)
    budget, fixed = (44 << 20, 64) if fn is rjson.loads else (144 << 20, 160)
    return max(5, round(budget / (size + fixed)))


def calibrate(runs=3):
    """Print CALLS: for each step, the median over `runs` runs of the calls
    made in CALIBRATE_SECONDS. Run it on an instrumented build (the calls are
    what `scripts/build_pgo.sh` trains with), on an otherwise idle host."""
    counts = {}
    for _ in range(runs):
        rng.seed(0x5EED)
        for key, fn, arg in steps(documents()):
            n, end = 0, time.perf_counter() + CALIBRATE_SECONDS
            while time.perf_counter() < end:
                fn(arg)
                n += 1
            counts.setdefault(key, []).append(n)
    print("CALLS = {")
    for key, ns in counts.items():
        print(f"    {key!r}: {sorted(ns)[len(ns) // 2]},")
    print("}")


def main():
    if sys.argv[1:2] == ["--calibrate"]:
        calibrate()
        return
    docs = documents()
    t0 = time.perf_counter()
    for key, fn, arg in steps(docs):
        n = CALLS.get(key)
        if n is None:
            n = fallback_calls(fn, arg)
            print(f"pgo_train: {key} not in CALLS, using {n} calls (run --calibrate)", file=sys.stderr)
        for _ in range(max(1, round(n * SCALE))):
            fn(arg)
        if key[1] == "loads_bytes":
            rjson.loads(bytearray(arg))
            rjson.loads(memoryview(arg))
    for _ in range(3):
        for bad in ("[1,", '{"a" 1}', "nan", "[1] x", '"\\ud800', "", "[" * 2000, '{"a":tru}', b"\xff"):
            try:
                rjson.loads(bad)
            except ValueError:
                pass
        for bad in (float("nan"), {1: 2}, object(), "\ud800", {"a": set()}):
            try:
                rjson.dumps(bad)
            except (ValueError, TypeError, UnicodeEncodeError):
                pass
    print(f"pgo_train: {len(docs)} documents, {time.perf_counter() - t0:.1f}s", file=sys.stderr)


if __name__ == "__main__":
    main()
