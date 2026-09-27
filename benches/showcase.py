"""rjson vs orjson, head to head, on the workloads where the difference shows.

Each case runs a realistic job both ways and checks first that both produce
the same result (byte-identical output for dumps, equal objects for loads).
Then it times them interleaved: every round runs rjson and orjson back to
back in alternating order, and the report gives the median over the rounds.

    python benches/showcase.py [--data DIR] [--rounds N] [--only SUBSTR]
                               [--output-json PATH]

``speedup`` is orjson's time divided by rjson's: 2.00x means rjson does the
same job in half the time. Cases where rjson is not faster are reported the
same way (below 1.00x); the suite is meant to show where the difference is,
not to hide where there is none.
"""

import argparse
import dataclasses
import datetime as dt
import enum
import io
import json
import os
import platform
import random
import statistics
import sys
import time
import uuid

import orjson
import rjson

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import prod_workloads as wl  # noqa: E402

UTC = dt.timezone.utc


# -- fixtures -------------------------------------------------------------------------


class Status(enum.Enum):
    ACTIVE = "active"
    SUSPENDED = "suspended"
    DELETED = "deleted"


@dataclasses.dataclass
class Order:
    id: int
    customer: str
    total: float
    currency: str
    paid: bool
    items: list


def log_lines(n, seed=5):
    """Application log messages with stack traces, SQL and quoted user input:
    newlines, tabs, quotes and backslashes in most strings."""
    rng = random.Random(seed)
    out = []
    for i in range(n):
        tb = "".join(
            f'  File "/srv/app/{rng.choice(["api", "db", "auth"])}/mod{k}.py", line {rng.randrange(900)}, '
            f"in handler_{k}\n    result = call(\"{wl.text(rng, 3)}\")\n"
            for k in range(rng.randrange(1, 5))
        )
        out.append({
            "ts": wl.iso(rng),
            "level": rng.choice(wl.LEVELS),
            "logger": rng.choice(wl.LOGGERS),
            "msg": f'query failed: SELECT * FROM "orders" WHERE note = \'{wl.text(rng, 4)}\'\n\tparams={i}',
            "exc_info": "Traceback (most recent call last):\n" + tb + "ValueError: bad \"input\" \\ path",
            "path": "C:\\Users\\svc\\app\\data\\" + wl.hexid(rng, 8) + ".json",
        })
    return out


def typed_events(n, seed=3):
    """Event records as an application holds them: datetime, UUID, Enum."""
    rng = random.Random(seed)
    base = dt.datetime(2024, 1, 1, tzinfo=UTC)
    return [
        {
            "id": uuid.UUID(int=rng.getrandbits(128)),
            "at": base + dt.timedelta(seconds=rng.randrange(10**8), microseconds=rng.randrange(10**6)),
            "day": dt.date(2024, 1, 1) + dt.timedelta(days=rng.randrange(900)),
            "status": rng.choice(list(Status)),
            "user_id": rng.randrange(10**6),
            "amount": round(rng.uniform(1, 500), 2),
        }
        for _ in range(n)
    ]


@dataclasses.dataclass(slots=True)
class Point:
    x: float
    y: float
    label: str
    visible: bool


def points(n, seed=15):
    rng = random.Random(seed)
    return [Point(rng.uniform(-1e3, 1e3), rng.uniform(-1e3, 1e3), f"p{i}", i % 3 != 0) for i in range(n)]


def orders(n, seed=9):
    rng = random.Random(seed)
    return [
        Order(i, f"{rng.choice(wl.FIRST)} {rng.choice(wl.LAST)}", round(rng.uniform(5, 900), 2),
              rng.choice(["USD", "EUR", "VND"]), rng.random() < 0.8,
              [{"sku": wl.hexid(rng, 8), "qty": rng.randrange(1, 5)} for _ in range(rng.randrange(1, 4))])
        for i in range(n)
    ]


def int_keyed(n, seed=21):
    """Aggregates keyed by id / hour: dicts with int keys."""
    rng = random.Random(seed)
    return {
        "by_user": {rng.randrange(10**7): rng.randrange(1000) for _ in range(n)},
        "by_hour": {h: round(rng.uniform(0, 1e4), 2) for h in range(24)},
    }


def load_corpus(data_dir, name):
    path = os.path.join(data_dir, name + ".json")
    if not os.path.exists(path):
        return None
    with open(path, "rb") as f:
        return f.read()


# -- cases ----------------------------------------------------------------------------
#
# A case: (group, name, description, rjson job, orjson job, check). Each job
# is a zero-argument callable doing one unit of work; `check` compares one run
# of each and returns an error string or None.


def ndjson_writer(dumps, records):
    """What a JSON log handler or an NDJSON exporter does: one call per
    record, each line written out and dropped."""

    def run():
        buf = io.BytesIO()
        write = buf.write
        for r in records:
            write(dumps(r))
            write(b"\n")
        return buf.getvalue()

    return run


def same_bytes(a, b):
    return None if a == b else f"output differs ({len(a)} vs {len(b)} bytes)"


def same_obj(a, b):
    return None if a == b else "parsed objects differ"


def build_cases(data_dir):
    cases = []

    def dumps_case(group, name, desc, obj, ropt=None, oopt=None):
        rk = ropt or {}
        ok = oopt or {}
        cases.append((group, name, desc, lambda: rjson.dumps(obj, **rk), lambda: orjson.dumps(obj, **ok), same_bytes))

    def loads_case(group, name, desc, data):
        cases.append((group, name, desc, lambda: rjson.loads(data), lambda: orjson.loads(data), same_obj))

    # Strings that need escaping: logs, tracebacks, SQL, Windows paths.
    logs = log_lines(2000)
    dumps_case("strings", "log records with tracebacks", "2,000 records, newlines/quotes/backslashes", logs)
    cases.append(("strings", "NDJSON log stream, one line per record",
                  "2,000 dumps calls written to a stream",
                  ndjson_writer(rjson.dumps, logs), ndjson_writer(orjson.dumps, logs), same_bytes))
    loads_case("strings", "parse the log stream back", "the same 2,000 records as one document",
               orjson.dumps(logs))

    # Web API payloads.
    gh = load_corpus(data_dir, "github")
    if gh:
        gh_obj = orjson.loads(gh)
        dumps_case("web api", "github.json response", "GitHub API events (55 KB)", gh_obj)
        loads_case("web api", "github.json request", "GitHub API events (55 KB)", gh)
    page = wl.api_page(1)
    dumps_case("web api", "paginated REST page", "50 users, nested objects, nulls (~45 KB)", page)
    small = [wl.small_response(random.Random(i)) for i in range(1000)]
    cases.append(("web api", "1,000 small responses", "one dumps call per response (~150 B each)",
                  lambda: [rjson.dumps(r) for r in small], lambda: [orjson.dumps(r) for r in small],
                  lambda a, b: same_bytes(b"".join(a), b"".join(b))))
    bodies = [orjson.dumps(wl.request_body(random.Random(i))) for i in range(1000)]
    cases.append(("web api", "1,000 small request bodies", "one loads call per body (bytes)",
                  lambda: [rjson.loads(b) for b in bodies], lambda: [orjson.loads(b) for b in bodies],
                  same_obj))

    # Application types serialized natively.
    events = typed_events(2000)
    dumps_case("native types", "events with datetime, UUID, Enum", "2,000 records, no default= needed", events)
    utc_times = [dt.datetime(2024, 1, 1, tzinfo=UTC) + dt.timedelta(seconds=37 * i, microseconds=i)
                 for i in range(5000)]
    dumps_case("native types", "UTC timestamps", "5,000 aware datetimes", utc_times)
    dumps_case("native types", "dataclasses", "1,000 dataclass instances with nested items", orders(1000))
    dumps_case("native types", "dataclasses with __slots__", "2,000 slots=True instances", points(2000))
    dumps_case("native types", "int-keyed aggregates", "5,000 int keys (non_str_keys / OPT_NON_STR_KEYS)",
               int_keyed(5000), {"non_str_keys": True}, {"option": orjson.OPT_NON_STR_KEYS})

    # When the caller needs str: rjson writes the str directly, orjson users
    # decode its bytes (templates, logging.Formatter.format, str-only APIs).
    tw = load_corpus(data_dir, "twitter")
    for name, obj in (("github.json", gh and orjson.loads(gh)), ("twitter.json", tw and orjson.loads(tw)),
                      ("log records", logs)):
        if obj is None:
            continue
        cases.append(("str output", f"{name} as str", "rjson.dumps_str vs orjson.dumps().decode()",
                      (lambda o=obj: rjson.dumps_str(o)), (lambda o=obj: orjson.dumps(o).decode()),
                      lambda a, b: None if a == b else "output differs"))

    # Standard corpora, for reference.
    for name in ("twitter", "citm_catalog", "canada"):
        data = load_corpus(data_dir, name)
        if data:
            loads_case("corpus", f"{name}.json loads", f"{len(data) // 1024} KB", data)
            dumps_case("corpus", f"{name}.json dumps", f"{len(data) // 1024} KB", orjson.loads(data))
    return cases


# -- fresh process --------------------------------------------------------------------
#
# Reported separately (not in the geomean): the same per-record encoding in a
# new process that has not produced a large output yet, keeping the 2,000
# results (a batch for a queue or a bulk insert). glibc's malloc then trims
# and regrows the heap around orjson's per-call buffers and each call takes a
# page fault; once some big output has raised glibc's thresholds (as in the
# main suite, which builds its big fixtures first) the effect is gone.

FRESH = r"""
import resource, sys, time
sys.path.insert(0, sys.argv[2])
import showcase, orjson, rjson
dumps = {"rjson": rjson.dumps, "orjson": orjson.dumps}[sys.argv[1]]
logs = showcase.log_lines(2000)
best, faults = 1e9, 0
for _ in range(10):
    f0 = resource.getrusage(resource.RUSAGE_SELF).ru_minflt
    t0 = time.perf_counter()
    batch = [dumps(r) for r in logs]
    t = time.perf_counter() - t0
    if t < best:
        best, faults = t, resource.getrusage(resource.RUSAGE_SELF).ru_minflt - f0
    del batch
print(best, faults)
"""


def fresh_process(runs=3):
    import subprocess

    out = {}
    for lib in ("rjson", "orjson"):
        res = []
        for _ in range(runs):
            p = subprocess.run([sys.executable, "-c", FRESH, lib, HERE], capture_output=True, text=True, check=True)
            t, faults = p.stdout.split()
            res.append((float(t), int(faults)))
        out[lib] = sorted(res)[len(res) // 2]
    return out


# -- timing ---------------------------------------------------------------------------


def calibrate(fn, target):
    n = 1
    while True:
        t0 = time.perf_counter()
        for _ in range(n):
            fn()
        if time.perf_counter() - t0 >= target / 4:
            return max(1, int(n * target / (time.perf_counter() - t0 + 1e-12)))
        n *= 2


def time_pair(rfn, ofn, rounds, target):
    n = max(calibrate(rfn, target), 1)
    times = {"rjson": [], "orjson": []}
    for r in range(rounds):
        order = (("rjson", rfn), ("orjson", ofn)) if r % 2 == 0 else (("orjson", ofn), ("rjson", rfn))
        for label, fn in order:
            t0 = time.perf_counter()
            for _ in range(n):
                fn()
            times[label].append((time.perf_counter() - t0) / n)
    return times


def fmt_t(t):
    if t < 1e-3:
        return f"{t * 1e6:.1f} µs"
    return f"{t * 1e3:.2f} ms"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data", default=os.path.join(HERE, "data"))
    ap.add_argument("--rounds", type=int, default=15)
    ap.add_argument("--target", type=float, default=0.02, help="seconds per timed sample")
    ap.add_argument("--only", help="run cases whose group or name contains this")
    ap.add_argument("--output-json")
    ap.add_argument("--fresh", action="store_true",
                    help="also time per-record encoding in fresh processes (Linux; reported separately)")
    args = ap.parse_args()

    cases = build_cases(args.data)
    if args.only:
        cases = [c for c in cases if args.only in c[0] or args.only in c[1]]

    print(f"rjson {rjson.__version__} vs orjson {orjson.__version__}, CPython {platform.python_version()}, "
          f"{platform.machine()}; median of {args.rounds} interleaved rounds")
    rows = []
    group = None
    for grp, name, desc, rfn, ofn, check in cases:
        err = check(rfn(), ofn())
        if err:
            print(f"  {name}: {err}", file=sys.stderr)
            return 1
        times = time_pair(rfn, ofn, args.rounds, args.target)
        r, o = statistics.median(times["rjson"]), statistics.median(times["orjson"])
        per_round = sorted(o_ / r_ for r_, o_ in zip(times["rjson"], times["orjson"]))
        q = len(per_round) // 4
        row = {"group": grp, "name": name, "desc": desc, "rjson_s": r, "orjson_s": o, "speedup": o / r,
               "speedup_q1": per_round[q], "speedup_q3": per_round[-1 - q]}
        rows.append(row)
        if grp != group:
            group = grp
            print(f"\n{grp}")
        print(f"  {name:<40} {fmt_t(r):>10} {fmt_t(o):>10}  {o / r:5.2f}x  "
              f"({row['speedup_q1']:.2f}-{row['speedup_q3']:.2f})")

    geo = statistics.geometric_mean([row["speedup"] for row in rows])
    wins = sum(row["speedup"] > 1 for row in rows)
    print(f"\ngeomean speedup {geo:.2f}x; rjson faster in {wins} of {len(rows)} cases")
    fresh = None
    if args.fresh:
        fresh = fresh_process()
        (rt, rf), (ot, of) = fresh["rjson"], fresh["orjson"]
        print(f"\nfresh process, 2,000 log records encoded one per call, results kept:\n"
              f"  rjson {fmt_t(rt)} ({rf} page faults)  orjson {fmt_t(ot)} ({of} page faults)  {ot / rt:.2f}x")
    if args.output_json:
        meta = {"rjson": rjson.__version__, "orjson": orjson.__version__, "python": platform.python_version(),
                "machine": platform.machine(), "rounds": args.rounds}
        with open(args.output_json, "w") as f:
            json.dump({"meta": meta, "geomean": geo, "rows": rows, "fresh_process": fresh}, f, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())
