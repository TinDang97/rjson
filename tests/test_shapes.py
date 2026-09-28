"""
The loads shape cache (parser.rs, "Shape cache"): objects whose keys repeat in
the same order are built by copying a template dict and writing the values
into its entries (CPython 3.11-3.13). Everything here must behave exactly as
the dicts `json.loads` builds: contents, key order, independence, GC tracking.
"""

import gc
import json
import random
import sys

import pytest

import rjson


def records(n, keys, value=lambda i, j: i * j):
    return [{k: value(i, j) for j, k in enumerate(keys)} for i in range(n)]


def check(doc):
    got = rjson.loads(doc)
    assert got == json.loads(doc)
    return got


@pytest.mark.parametrize("nkeys", [1, 2, 3, 8, 9, 16, 63, 64, 65, 100])
def test_same_shape_records(nkeys):
    keys = ["key_%d" % j for j in range(nkeys)]
    got = check(json.dumps(records(50, keys)))
    for d in got:
        assert list(d) == keys


def test_key_order_is_per_object():
    # Same keys, different orders, alternating and in runs.
    objs = [{"a": i, "b": i} if i % 2 else {"b": i, "a": i} for i in range(40)]
    objs += [{"x": 1, "y": 2, "z": 3}] * 5 + [{"z": 3, "y": 2, "x": 1}] * 5
    got = check(json.dumps(objs))
    for g, o in zip(got, objs):
        assert list(g) == list(o)


def test_dicts_are_independent():
    got = rjson.loads(json.dumps([{"x": 1, "y": [2]}] * 10))
    got[3]["x"] = 99
    got[4]["z"] = 1
    del got[5]["x"]
    got[6]["y"].append(3)
    assert got[2] == {"x": 1, "y": [2]}
    assert got[3] == {"x": 99, "y": [2]}
    assert got[4] == {"x": 1, "y": [2], "z": 1}
    assert got[5] == {"y": [2]}
    assert got[6] == {"x": 1, "y": [2, 3]}
    assert got[7] == {"x": 1, "y": [2]}
    # The template behind them is untouched: a later call is still right.
    assert rjson.loads(json.dumps([{"x": 5, "y": [6]}] * 4)) == [{"x": 5, "y": [6]}] * 4


def test_values_of_every_type():
    values = ["s", "ü€😀", 1, -(10**30), 1.5, True, False, None, [], [1, {"a": 1}], {}, {"n": {"m": []}}]
    doc = json.dumps([{"k1": v, "k2": w} for v in values for w in values])
    check(doc)


def test_gc_tracking_matches_json():
    objs = []
    for v in [1, "s", None, [1], {"a": 1}, {}, 2.5, []]:
        objs += [{"p": 1, "q": v}] * 4
    doc = json.dumps(objs)
    for g, j in zip(rjson.loads(doc), json.loads(doc)):
        assert gc.is_tracked(g) == gc.is_tracked(j), g


def test_cycle_through_shape_dict_is_collected():
    doc = json.dumps([{"a": 1, "b": []}] * 6)
    collected = []

    class Sentinel:
        def __del__(self):
            collected.append(True)

    got = rjson.loads(doc)
    d = got[5]  # built from the template
    d["b"].append(Sentinel())
    d["b"].append(d)  # d -> list -> d
    del got, d
    gc.collect()
    assert collected == [True]


def test_duplicate_keys():
    doc = "[" + ",".join(['{"a":%d,"b":0,"a":%d}' % (i, i + 1) for i in range(20)]) + "]"
    got = check(doc)
    assert got[-1] == {"a": 20, "b": 0}
    assert list(got[-1]) == ["a", "b"]


def test_many_shapes():
    # More shapes than slots, revisited in several passes and interleaved.
    rng = random.Random(3)
    pool = ["k%d" % j for j in range(300)]
    shapes = [rng.sample(pool, rng.randint(1, 12)) for _ in range(500)]
    objs = []
    for _ in range(3):
        for s in shapes:
            objs += [{k: rng.randint(0, 9) for k in s}] * rng.randint(1, 3)
    check(json.dumps(objs))


def test_keys_not_in_key_cache():
    # Keys over 64 bytes and keys with escapes are new objects every time.
    long_keys = ["long_" + "x" * 70 + str(j) for j in range(4)]
    check(json.dumps(records(30, long_keys)))
    doc = "[" + ",".join(['{"a\\u0062":%d,"c\\n":%d}' % (i, i) for i in range(30)]) + "]"
    got = check(doc)
    assert list(got[-1]) == ["ab", "c\n"]


def test_nested_same_shape():
    doc = json.dumps([{"id": i, "child": {"id": i + 1, "child": {"id": i + 2, "child": None}}} for i in range(30)])
    check(doc)


def test_across_calls_and_after_errors():
    doc = json.dumps(records(20, ["a", "b", "c"]))
    for _ in range(3):
        check(doc)
    bad = doc[:-30]
    for _ in range(3):
        with pytest.raises(rjson.JSONDecodeError):
            rjson.loads(bad)
        check(doc)
    for line in (json.dumps({"a": i, "b": [i], "c": None}) for i in range(50)):
        check(line)


def test_dumps_round_trip():
    doc = json.dumps(records(30, ["a", "b", "c", "d"], lambda i, j: [i, {"j": j}]))
    assert rjson.dumps(rjson.loads(doc)) == doc.replace(" ", "").encode()


def test_no_reference_leak():
    # Refcounts of shared values: a leak or an over-release of the value
    # references written into the copied dicts (or of the `None` placeholders
    # they replace) would show up here. No assert inside the loop: pytest's
    # assertion rewriting keeps references to intermediate values.
    doc = json.dumps([{"a": "v", "b": True, "c": None}] * 100)
    rjson.loads(doc)
    ok = True  # (holds a reference to True itself)
    before = sys.getrefcount(True), sys.getrefcount(None)
    for _ in range(20):
        got = rjson.loads(doc)
        ok = ok and all(d["b"] is True and d["c"] is None for d in got)
        del got
    after = sys.getrefcount(True), sys.getrefcount(None)
    assert ok
    assert after == before


@pytest.mark.skipif(not ((3, 11) <= sys.version_info[:2] <= (3, 13)), reason="shape cache: CPython 3.11-3.13")
@pytest.mark.parametrize("nkeys", [3, 8, 12, 30])
def test_dict_size_matches_json(nkeys):
    # Copies of a template are no larger than the dicts json.loads builds
    # (before, > 8 keys on 3.11/3.12 took a presized generic key table).
    doc = json.dumps(records(20, ["key%d" % j for j in range(nkeys)]))
    for g, j in list(zip(rjson.loads(doc), json.loads(doc)))[4:]:
        assert sys.getsizeof(g) <= sys.getsizeof(j)
