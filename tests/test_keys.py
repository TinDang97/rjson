"""dumps(..., non_str_keys=True): non-str dict keys (issue #6).

bool/None/int/float keys get exactly json.dumps's key text; Enum, datetime, date,
time and UUID keys (which json rejects) follow orjson's OPT_NON_STR_KEYS.
"""

import collections
import datetime as dt
import enum
import gc
import json
import random
import struct
import sys
import uuid

import pytest
import rjson


def json_text(obj):
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False)


def both(obj, **kw):
    """dumps and dumps_str agree; return the str."""
    out = rjson.dumps_str(obj, non_str_keys=True, **kw)
    assert rjson.dumps(obj, non_str_keys=True, **kw) == out.encode()
    return out


class IntE(enum.IntEnum):
    A = 3


class StrE(str, enum.Enum):
    S = "s"


class FloatE(float, enum.Enum):
    F = 1.5


class Plain(enum.Enum):
    NUM = 1
    TEXT = "tëxt"
    REAL = 2.5
    DAY = dt.date(2024, 1, 2)
    NONE = None


class IntSub(int):
    def __repr__(self):  # json uses int.__repr__, not this
        return "IntSub!"


class FloatSub(float):
    def __repr__(self):
        return "FloatSub!"


class TestMatchesJson:
    KEYS = [
        0,
        1,
        -7,
        2**31,
        2**63 - 1,
        2**63,
        2**64,
        -(2**100),
        10**400,
        0.0,
        -0.0,
        1.0,
        1.5,
        0.1,
        1e16,
        1e-7,
        1e22,
        123456789.0,
        5e-324,
        1.7976931348623157e308,
        float("nan"),
        float("inf"),
        float("-inf"),
        True,
        False,
        None,
        IntE.A,
        StrE.S,
        FloatE.F,
        IntSub(5),
        FloatSub(2.5),
    ]

    @pytest.mark.parametrize("key", KEYS, ids=repr)
    def test_key(self, key):
        assert both({key: 0}) == json_text({key: 0})

    def test_mixed_and_nested(self):
        obj = {
            "s": 1,
            2: [{3.5: None, None: {True: False}}],
            (2**70): "big",
        }
        assert both(obj) == json_text(obj)

    def test_random_floats_and_ints(self):
        rng = random.Random(6)
        keys = []
        for _ in range(3000):
            (f,) = struct.unpack("<d", rng.getrandbits(64).to_bytes(8, "little"))
            keys.append(f)
            keys.append(rng.uniform(-1e6, 1e6))
            keys.append(rng.getrandbits(rng.choice([8, 40, 64, 65, 200])) * rng.choice([1, -1]))
        for k in keys:
            assert both({k: 1}) == json_text({k: 1}), repr(k)

    def test_counter_and_dict_subclasses(self):
        c = collections.Counter([1, 1, 2, 3, 3, 3])
        assert both(c) == json_text(c)
        d = collections.defaultdict(list, {1: [1], 2.5: [2]})
        assert both(d) == json_text(d)
        o = collections.OrderedDict([(None, 1), (False, 2)])
        assert both(o) == json_text(o)

    def test_duplicates_after_coercion_are_kept(self):
        # Like json and orjson: both keys are written; parsers keep the last.
        obj = {1: "int", "1": "str", None: "none", "null": "s"}
        assert both(obj) == json_text(obj) == '{"1":"int","1":"str","null":"none","null":"s"}'
        assert rjson.loads(both(obj)) == {"1": "str", "null": "s"}

    def test_values_are_unchanged(self):
        # The option only affects keys: NaN values still raise.
        with pytest.raises(rjson.JSONEncodeError, match="non-finite"):
            rjson.dumps({1: float("nan")}, non_str_keys=True)


class TestTypesJsonRejects:
    """Keys json.dumps raises for; the text matches orjson's OPT_NON_STR_KEYS."""

    def test_enum_keys(self):
        assert both({Plain.NUM: 0}) == '{"1":0}'
        assert both({Plain.TEXT: 0}) == '{"tëxt":0}'
        assert both({Plain.REAL: 0}) == '{"2.5":0}'
        assert both({Plain.DAY: 0}) == '{"2024-01-02":0}'
        assert both({Plain.NONE: 0}) == '{"null":0}'

    def test_datetime_and_uuid_keys(self):
        obj = {
            dt.date(2024, 1, 2): 1,
            dt.datetime(2024, 1, 2, 3, 4, 5, 6): 2,
            dt.datetime(2024, 1, 2, tzinfo=dt.timezone(dt.timedelta(hours=-5))): 3,
            dt.time(1, 2, 3): 4,
            uuid.UUID(int=1): 5,
        }
        assert both(obj) == (
            '{"2024-01-02":1,"2024-01-02T03:04:05.000006":2,'
            '"2024-01-02T00:00:00-05:00":3,"01:02:03":4,'
            '"00000000-0000-0000-0000-000000000001":5}'
        )

    def test_passthrough_kinds_raise_as_keys(self):
        # default= is never called for keys, so a passed-through key raises.
        for key, flag in [
            (dt.date(2024, 1, 1), rjson.PASSTHROUGH_DATETIME),
            (uuid.UUID(int=1), rjson.PASSTHROUGH_UUID),
            (Plain.NUM, rjson.PASSTHROUGH_ENUM),
        ]:
            with pytest.raises(rjson.JSONEncodeError, match="not supported"):
                rjson.dumps({key: 1}, non_str_keys=True, passthrough=flag, default=str)

    @pytest.mark.parametrize(
        "key",
        [b"b", (1, 2), frozenset({1}), object(), 1 + 2j, dt.timedelta(1)],
        ids=lambda k: type(k).__name__,
    )
    def test_unsupported_keys_raise(self, key):
        with pytest.raises(rjson.JSONEncodeError, match="not supported") as e:
            rjson.dumps({key: 1}, non_str_keys=True)
        assert isinstance(e.value, TypeError)

    def test_enum_with_unsupported_value(self):
        class Bad(enum.Enum):
            T = (1, 2)

        with pytest.raises(rjson.JSONEncodeError, match="tuple is not supported"):
            rjson.dumps({Bad.T: 1}, non_str_keys=True)

    def test_default_is_not_called_for_keys(self):
        calls = []
        with pytest.raises(rjson.JSONEncodeError):
            rjson.dumps({b"k": 1}, non_str_keys=True, default=lambda o: calls.append(o) or "x")
        assert calls == []

    def test_matches_orjson(self):
        orjson = pytest.importorskip("orjson")
        obj = {
            Plain.NUM: 1,
            Plain.TEXT: 2,
            Plain.DAY: 3,
            dt.datetime(2024, 1, 2, 3, 4, 5, tzinfo=dt.timezone.utc): 4,
            dt.time(1, 2): 5,
            uuid.UUID(int=12345): 6,
            1: 7,
            True: 8,
            None: 9,
            1.5: 10,
        }
        assert rjson.dumps(obj, non_str_keys=True) == orjson.dumps(
            obj, option=orjson.OPT_NON_STR_KEYS
        )


class TestOption:
    def test_off_by_default(self):
        for kw in ({}, {"non_str_keys": False}, {"non_str_keys": None}, {"non_str_keys": 0}):
            with pytest.raises(rjson.JSONEncodeError, match="keys must be strings"):
                rjson.dumps({1: 1}, **kw)

    def test_truthy_values_enable_it(self):
        for v in (True, 1, "yes"):
            assert rjson.dumps({1: 1}, non_str_keys=v) == b'{"1":1}'

    def test_bad_truthiness_propagates(self):
        class NoBool:
            def __bool__(self):
                raise ValueError("no bool")

        with pytest.raises(ValueError, match="no bool"):
            rjson.dumps({1: 1}, non_str_keys=NoBool())

    def test_str_keys_unchanged(self):
        class S(str):
            pass

        obj = {"a": 1, S("b"): 2, "é": 3}
        assert rjson.dumps(obj, non_str_keys=True) == rjson.dumps(obj)

    def test_signature(self):
        import inspect

        for f in (rjson.dumps, rjson.dumps_str, rjson.dumps_bytes):
            p = inspect.signature(f).parameters["non_str_keys"]
            assert p.kind is inspect.Parameter.KEYWORD_ONLY and p.default is False


class TestGuardedMode:
    def test_enum_key_getattribute_resizing_parent(self):
        parent = {}

        class Growing(enum.Enum):
            A = 1

            def __getattribute__(self, name):
                if name == "_value_":  # also read while the class is created
                    n = len(parent)
                    parent.update({f"k{n + i}": i for i in range(100)})
                return super().__getattribute__(name)

        parent[Growing.A] = 1
        parent["y"] = 2
        with pytest.raises(RuntimeError, match="changed size"):
            rjson.dumps({"p": parent}, non_str_keys=True)

    def test_python_tzinfo_key_dropping_parent(self):
        holder = {}

        class Dropping(dt.tzinfo):
            def utcoffset(self, d):
                holder["a"] = None  # the last reference to the dict being walked
                gc.collect()
                return dt.timedelta(0)

        holder["a"] = {dt.datetime(2024, 1, 1, tzinfo=Dropping()): "x" * 100, 2: [1]}
        out = rjson.dumps(holder, non_str_keys=True)
        assert out == b'{"a":{"2024-01-01T00:00:00+00:00":"' + b"x" * 100 + b'","2":[1]}}'

    def test_no_reference_leaks(self):
        keys = [2**100, 1.5, IntE.A, Plain.TEXT, uuid.UUID(int=3), dt.date(2024, 1, 1)]
        obj = dict.fromkeys(keys, 0)
        for f in (rjson.dumps, rjson.dumps_str):
            f(obj, non_str_keys=True)
            before = [sys.getrefcount(k) for k in keys]
            for _ in range(500):
                f(obj, non_str_keys=True)
                with pytest.raises(TypeError):
                    f({b"x": 1}, non_str_keys=True)
            assert [sys.getrefcount(k) for k in keys] == before
