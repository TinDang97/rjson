"""Native datetime/date/time, uuid.UUID, dataclass and Enum serialization (issue #5).

Output matches orjson byte for byte, except where orjson crashes or writes invalid
RFC 3339 (see TestDeviationsFromOrjson). The differential test at the end runs when
orjson is installed.
"""

import dataclasses
import datetime as dt
import enum
import gc
import random
import subprocess
import sys
import typing
import uuid
import zoneinfo

import pytest
import rjson

UTC = dt.timezone.utc
ALL_PASSTHROUGH = (
    rjson.PASSTHROUGH_DATETIME
    | rjson.PASSTHROUGH_UUID
    | rjson.PASSTHROUGH_DATACLASS
    | rjson.PASSTHROUGH_ENUM
)


def both(obj, **kw):
    """dumps and dumps_str agree; return the bytes."""
    out = rjson.dumps(obj, **kw)
    assert rjson.dumps_str(obj, **kw) == out.decode()
    assert rjson.dumps_bytes(obj, **kw) == out
    return out


class PyTZ(dt.tzinfo):
    """A pure-Python tzinfo (like pytz/dateutil): utcoffset runs Python code."""

    def __init__(self, minutes):
        self.off = dt.timedelta(minutes=minutes)

    def utcoffset(self, d):
        return self.off

    def dst(self, d):
        return dt.timedelta(0)


class Color(enum.Enum):
    RED = "red"
    GREEN = 2
    PAIR = (1, "x")
    DAY = dt.date(2024, 1, 1)


@dataclasses.dataclass
class Point:
    x: int
    y: str = "s"


class TestDatetime:
    @pytest.mark.parametrize(
        "value, want",
        [
            (dt.datetime(2024, 1, 2, 3, 4, 5), "2024-01-02T03:04:05"),
            (dt.datetime(2024, 1, 2, 3, 4, 5, 120), "2024-01-02T03:04:05.000120"),
            (dt.datetime(2024, 1, 2, 3, 4, 5, 999999), "2024-01-02T03:04:05.999999"),
            (dt.datetime(1, 1, 1), "0001-01-01T00:00:00"),
            (dt.datetime(999, 12, 31, 23, 59, 59), "0999-12-31T23:59:59"),
            (dt.datetime(9999, 12, 31, 23, 59, 59, 999999), "9999-12-31T23:59:59.999999"),
            (dt.datetime(2024, 1, 2, tzinfo=UTC), "2024-01-02T00:00:00+00:00"),
            (
                dt.datetime(2024, 1, 2, tzinfo=dt.timezone(dt.timedelta(hours=-5, minutes=-30))),
                "2024-01-02T00:00:00-05:30",
            ),
            (
                dt.datetime(2024, 1, 2, tzinfo=dt.timezone(dt.timedelta(hours=23, minutes=59))),
                "2024-01-02T00:00:00+23:59",
            ),
            (dt.datetime(2024, 7, 2, 3, 4, 5, tzinfo=PyTZ(-240)), "2024-07-02T03:04:05-04:00"),
        ],
    )
    def test_datetime(self, value, want):
        assert both(value) == f'"{want}"'.encode()

    def test_zoneinfo_and_fold(self):
        ny = zoneinfo.ZoneInfo("America/New_York")
        assert both(dt.datetime(2024, 7, 2, 3, 4, 5, tzinfo=ny)) == b'"2024-07-02T03:04:05-04:00"'
        # 01:30 happens twice on 2024-11-03; fold picks the second (EST).
        assert both(dt.datetime(2024, 11, 3, 1, 30, tzinfo=ny)) == b'"2024-11-03T01:30:00-04:00"'
        assert (
            both(dt.datetime(2024, 11, 3, 1, 30, tzinfo=ny, fold=1))
            == b'"2024-11-03T01:30:00-05:00"'
        )

    def test_offsets_with_seconds_round_like_orjson(self):
        # Historical local mean time: Amsterdam was +00:19:32 in 1900.
        ams = zoneinfo.ZoneInfo("Europe/Amsterdam")
        assert both(dt.datetime(1900, 1, 1, tzinfo=ams)) == b'"1900-01-01T00:00:00+00:20"'
        for secs, want in [
            (29, "+00:00"),
            (30, "+00:01"),
            (-29, "-00:00"),
            (-90, "-00:02"),
            (3629, "+01:00"),
            (3630, "+01:01"),
        ]:
            d = dt.datetime(2024, 1, 2, tzinfo=dt.timezone(dt.timedelta(seconds=secs)))
            assert both(d) == f'"2024-01-02T00:00:00{want}"'.encode()

    def test_date_and_time(self):
        assert both(dt.date(2024, 2, 29)) == b'"2024-02-29"'
        assert both(dt.date(5, 1, 2)) == b'"0005-01-02"'
        assert both(dt.time(3, 4, 5)) == b'"03:04:05"'
        assert both(dt.time(0, 0, 0, 7)) == b'"00:00:00.000007"'

    def test_time_with_tzinfo_raises(self):
        with pytest.raises(rjson.JSONEncodeError, match="must not have tzinfo"):
            rjson.dumps(dt.time(3, 4, tzinfo=UTC))

    def test_in_containers(self):
        d = dt.datetime(2024, 1, 2, 3, 4, 5)
        assert both({"at": d, "days": [d.date(), d.time()]}) == (
            b'{"at":"2024-01-02T03:04:05","days":["2024-01-02","03:04:05"]}'
        )

    def test_subclasses_and_timedelta_are_not_native(self):
        class MyDate(dt.date):
            pass

        class MyDT(dt.datetime):
            pass

        for v in (MyDate(2024, 1, 1), MyDT(2024, 1, 1), dt.timedelta(1)):
            with pytest.raises(rjson.JSONEncodeError, match="not JSON serializable"):
                rjson.dumps(v)
            assert rjson.dumps(v, default=str) == f'"{v}"'.encode()

    def test_not_a_dict_key(self):
        with pytest.raises(rjson.JSONEncodeError, match="keys must be strings"):
            rjson.dumps({dt.date(2024, 1, 1): 1})


class TestUUID:
    def test_format(self):
        u = uuid.UUID(int=0x0123456789ABCDEF0123456789ABCDEF)
        assert both(u) == b'"01234567-89ab-cdef-0123-456789abcdef"'
        assert both(uuid.UUID(int=0)) == b'"00000000-0000-0000-0000-000000000000"'
        assert both(uuid.UUID(int=2**128 - 1)) == b'"ffffffff-ffff-ffff-ffff-ffffffffffff"'
        for _ in range(100):
            u = uuid.uuid4()
            assert both([u]) == f'["{u}"]'.encode()

    def test_subclass_is_not_native(self):
        class MyUUID(uuid.UUID):
            pass

        with pytest.raises(rjson.JSONEncodeError, match="not JSON serializable"):
            rjson.dumps(MyUUID(int=1))


class TestEnum:
    def test_values(self):
        assert both(Color.RED) == b'"red"'
        assert both(Color.GREEN) == b"2"
        assert both(Color.PAIR) == b'[1,"x"]'
        assert both(Color.DAY) == b'"2024-01-01"'  # value is native too
        assert both({"c": [Color.RED, Color.GREEN]}) == b'{"c":["red",2]}'

    def test_flag_and_mixins(self):
        class Perm(enum.Flag):
            R = 1
            W = 2

        class Num(enum.IntEnum):
            A = 3

        class Word(str, enum.Enum):
            W = "w"

        assert both(Perm.R | Perm.W) == b"3"
        assert both(Num.A) == b"3"
        assert both(Word.W) == b'"w"'

    def test_enum_value_is_unsupported(self):
        class Bad(enum.Enum):
            X = object()

        with pytest.raises(rjson.JSONEncodeError, match="not JSON serializable"):
            rjson.dumps(Bad.X)

    def test_custom_getattribute_enum(self):
        # Reading _value_ runs Python code here: serialized in guarded mode.
        calls = []

        class Traced(enum.Enum):
            A = 1

            def __getattribute__(self, name):
                calls.append(name)
                return super().__getattribute__(name)

        assert both([Traced.A, {"k": Traced.A}]) == b'[1,{"k":1}]'
        assert "_value_" in calls

    def test_not_a_dict_key(self):
        with pytest.raises(rjson.JSONEncodeError, match="keys must be strings"):
            rjson.dumps({Color.RED: 1})


class TestDataclass:
    def test_basic_and_nested(self):
        @dataclasses.dataclass
        class Line:
            a: Point
            b: Point
            when: dt.date
            tags: list

        v = Line(Point(1), Point(2, "é"), dt.date(2024, 1, 1), [Point(3)])
        assert both(v) == (
            b'{"a":{"x":1,"y":"s"},"b":{"x":2,"y":"\xc3\xa9"},'
            b'"when":"2024-01-01","tags":[{"x":3,"y":"s"}]}'
        )

    def test_dict_path_like_orjson(self):
        # Without __slots__: the instance __dict__ in order, '_' names skipped,
        # extra attributes included.
        @dataclasses.dataclass
        class Rec:
            _private: int
            public: int

        r = Rec(1, 2)
        r.extra = 3
        r._hidden = 4
        assert both(r) == b'{"public":2,"extra":3}'
        del r.public
        assert both(r) == b'{"extra":3}'

    def test_slots_path(self):
        @dataclasses.dataclass(slots=True)
        class S:
            a: int
            _p: int = 0
            c: typing.ClassVar[int] = 9
            b: str = "x"

        assert both(S(1)) == b'{"a":1,"b":"x"}'

    def test_classvar_and_initvar_excluded(self):
        @dataclasses.dataclass
        class C:
            a: int
            c: typing.ClassVar[int] = 1
            i: dataclasses.InitVar[int] = 0

        assert both(C(1, 5)) == b'{"a":1}'

    def test_frozen_and_empty(self):
        @dataclasses.dataclass(frozen=True)
        class F:
            a: int

        @dataclasses.dataclass
        class E:
            pass

        @dataclasses.dataclass(slots=True)
        class ES:
            pass

        assert both(F(1)) == b'{"a":1}'
        assert both(E()) == b"{}"
        assert both(ES()) == b"{}"

    def test_dataclass_subclass(self):
        @dataclasses.dataclass
        class Child(Point):
            z: int = 0

        class NotDC(Point):
            pass

        assert both(Child(1, "q", 3)) == b'{"x":1,"y":"q","z":3}'
        # Like orjson: only a type that is itself a dataclass.
        with pytest.raises(rjson.JSONEncodeError, match="not JSON serializable"):
            rjson.dumps(NotDC(1))
        with pytest.raises(rjson.JSONEncodeError, match="not JSON serializable"):
            rjson.dumps(Point)  # the class, not an instance

    def test_cycle_and_bad_keys(self):
        @dataclasses.dataclass
        class Node:
            nxt: object = None

        n = Node()
        n.nxt = n
        with pytest.raises(rjson.JSONEncodeError, match="nesting depth"):
            rjson.dumps(n)
        m = Node()
        m.__dict__[1] = 2
        with pytest.raises(rjson.JSONEncodeError, match="keys must be strings"):
            rjson.dumps(m)

    def test_property_raising_propagates(self):
        @dataclasses.dataclass
        class P:
            a: int

            def __getattribute__(self, name):
                if name == "__dict__":
                    raise KeyError("boom")
                return super().__getattribute__(name)

        with pytest.raises(KeyError, match="boom"):
            rjson.dumps(P(1))


class TestDeviationsFromOrjson:
    """Cases where orjson crashes or writes invalid output (orjson 3.x)."""

    def test_utcoffset_none_is_naive(self):
        # Python: utcoffset() None means naive (isoformat has no offset);
        # orjson writes +00:00.
        class NoneTZ(dt.tzinfo):
            def utcoffset(self, d):
                return None

        d = dt.datetime(2024, 1, 2, tzinfo=NoneTZ())
        assert both(d) == f'"{d.isoformat()}"'.encode() == b'"2024-01-02T00:00:00"'

    def test_raising_tzinfo_propagates(self):
        # orjson segfaults.
        class BadTZ(dt.tzinfo):
            def utcoffset(self, d):
                raise ValueError("boom")

        with pytest.raises(ValueError, match="boom"):
            rjson.dumps([dt.datetime(2024, 1, 2, tzinfo=BadTZ())])

    def test_invalid_utcoffset_result(self):
        class Weird(dt.tzinfo):
            def utcoffset(self, d):
                return 5

        with pytest.raises(TypeError):
            rjson.dumps(dt.datetime(2024, 1, 2, tzinfo=Weird()))

    def test_deleted_slot_field_raises(self):
        # orjson segfaults.
        @dataclasses.dataclass(slots=True)
        class S:
            a: int
            b: int

        s = S(1, 2)
        del s.b
        with pytest.raises(AttributeError):
            rjson.dumps(s)

    def test_offset_rounding_up_to_the_hour_carries(self):
        # orjson writes +00:60 / +23:60, which are not valid RFC 3339.
        for secs, want in [(3599, "+01:00"), (-3599, "-01:00"), (86399, "+23:59")]:
            d = dt.datetime(2024, 1, 2, tzinfo=dt.timezone(dt.timedelta(seconds=secs)))
            assert both(d) == f'"2024-01-02T00:00:00{want}"'.encode()


class TestPassthrough:
    VALUES = {
        rjson.PASSTHROUGH_DATETIME: [
            dt.datetime(2024, 1, 2, 3, 4, 5),
            dt.date(2024, 1, 2),
            dt.time(3, 4, 5),
        ],
        rjson.PASSTHROUGH_UUID: [uuid.UUID(int=5)],
        rjson.PASSTHROUGH_DATACLASS: [Point(1)],
        rjson.PASSTHROUGH_ENUM: [Color.RED],
    }

    def test_flags_are_distinct_bits(self):
        flags = list(self.VALUES)
        assert sorted(flags) == [1, 2, 4, 8]

    @pytest.mark.parametrize("flag", [1, 2, 4, 8])
    def test_flag_routes_to_default(self, flag):
        for v in self.VALUES[flag]:
            with pytest.raises(rjson.JSONEncodeError, match="not JSON serializable"):
                rjson.dumps(v, passthrough=flag)
            assert both([v], passthrough=flag, default=lambda o: "D") == b'["D"]'
        # Other kinds stay native.
        for other, values in self.VALUES.items():
            if other != flag:
                for v in values:
                    assert both(v, passthrough=flag) == both(v)

    def test_default_sees_native_types_only_when_passed_through(self):
        seen = []

        def conv(o):
            seen.append(type(o))
            return repr(o)

        obj = [dt.date(2024, 1, 1), uuid.UUID(int=1), Point(1), Color.RED]
        rjson.dumps(obj, default=conv)
        assert seen == []
        rjson.dumps(obj, default=conv, passthrough=ALL_PASSTHROUGH)
        assert seen == [dt.date, uuid.UUID, Point, Color]

    def test_int_enum_is_still_its_value(self):
        class Num(enum.IntEnum):
            A = 3

        assert both(Num.A, passthrough=rjson.PASSTHROUGH_ENUM) == b"3"

    @pytest.mark.parametrize("value", [0, None])
    def test_zero_and_none(self, value):
        assert both(dt.date(2024, 1, 2), passthrough=value) == b'"2024-01-02"'

    @pytest.mark.parametrize(
        "value, exc",
        [(16, ValueError), (-1, ValueError), (2**70, ValueError), ("1", TypeError), (True, TypeError)],
    )
    def test_invalid(self, value, exc):
        with pytest.raises(exc, match="passthrough"):
            rjson.dumps(1, passthrough=value)


class TestGuardedMode:
    """Values whose serialization runs Python code (dataclasses, Python tzinfo,
    custom Enum attribute access) switch the whole call to guarded mode, so code
    that mutates the containers being serialized cannot free them underneath."""

    def test_python_tzinfo_resizing_parent_dict(self):
        parent = {}

        class Growing(dt.tzinfo):
            def utcoffset(self, d):
                parent.update({f"k{i}": i for i in range(100)})
                return dt.timedelta(0)

        parent["x"] = dt.datetime(2024, 1, 1, tzinfo=Growing())
        parent["y"] = 2
        with pytest.raises(RuntimeError, match="changed size"):
            rjson.dumps({"p": parent})

    def test_python_tzinfo_dropping_the_list_being_serialized(self):
        holder = {}

        class Dropping(dt.tzinfo):
            def utcoffset(self, d):
                holder["a"] = None  # the last reference to the list
                gc.collect()
                return dt.timedelta(0)

        holder["a"] = [dt.datetime(2024, 1, 1, tzinfo=Dropping()), "tail" * 50, [1, 2]]
        out = rjson.dumps(holder)
        assert out == b'{"a":["2024-01-01T00:00:00+00:00","' + b"tail" * 50 + b'",[1,2]]}'

    def test_dataclass_resizing_parent_dict(self):
        parent = {}

        @dataclasses.dataclass
        class Grow:
            a: int

            def __getattribute__(self, name):
                if name == "__dict__":
                    parent.update({f"k{i}": i for i in range(100)})
                return super().__getattribute__(name)

        parent["x"] = Grow(1)
        parent["y"] = 2
        with pytest.raises(RuntimeError, match="changed size"):
            rjson.dumps({"p": parent})

    def test_enum_getattribute_resizing_parent_dict(self):
        parent = {}

        class Growing(enum.Enum):
            A = 1

            def __getattribute__(self, name):
                if name == "_value_":  # also read while the class is created
                    n = len(parent)
                    parent.update({f"k{n + i}": i for i in range(100)})
                return super().__getattribute__(name)

        parent["x"] = Growing.A
        parent["y"] = 2
        with pytest.raises(RuntimeError, match="changed size"):
            rjson.dumps({"p": parent})

    def test_restart_output_is_identical(self):
        # A dataclass late in the document forces a restart in guarded mode;
        # the output must equal the one-pass guarded result (default= set).
        doc = {"rows": [{"id": i, "at": dt.date(2024, 1, 1 + i % 28)} for i in range(500)]}
        doc["tail"] = Point(1)
        assert both(doc) == both(doc, default=lambda o: 1 / 0)

    def test_no_reference_leaks(self):
        ny = zoneinfo.ZoneInfo("America/New_York")
        objs = [
            dt.datetime(2024, 1, 2, tzinfo=ny),
            dt.datetime(2024, 1, 2, tzinfo=PyTZ(60)),
            uuid.UUID(int=9),
            Color.PAIR,
            Point(1),
        ]
        for f in (rjson.dumps, rjson.dumps_str):
            f(objs)
            before = [sys.getrefcount(o) for o in objs]
            for _ in range(500):
                f(objs)
                f({"x": objs})
            assert [sys.getrefcount(o) for o in objs] == before


def test_modules_are_not_imported_by_rjson():
    # Types are looked up in sys.modules, never imported by rjson.
    code = (
        "import sys, rjson\n"
        "try:\n    rjson.dumps(object())\nexcept TypeError:\n    pass\n"
        "print(sorted(m for m in ('datetime', 'uuid', 'dataclasses', 'zoneinfo') "
        "if m in sys.modules))"
    )
    out = subprocess.run(
        [sys.executable, "-I", "-c", code], capture_output=True, text=True, check=True
    )
    assert out.stdout.strip() == "[]"


def test_types_imported_after_first_call_are_found():
    code = (
        "import rjson\n"
        "try:\n    rjson.dumps(object())\nexcept TypeError:\n    pass\n"
        "import datetime, uuid, dataclasses\n"
        "@dataclasses.dataclass\nclass P:\n    a: int\n"
        "print(rjson.dumps([datetime.date(2024, 1, 2), uuid.UUID(int=1), P(1)]).decode())"
    )
    out = subprocess.run(
        [sys.executable, "-I", "-c", code], capture_output=True, text=True, check=True
    )
    assert out.stdout.strip() == '["2024-01-02","00000000-0000-0000-0000-000000000001",{"a":1}]'


# ---------------------------------------------------------------------------
# Differential test against orjson
# ---------------------------------------------------------------------------


class _Diff:
    ZONES = [
        "UTC",
        "America/New_York",
        "Asia/Kolkata",
        "Australia/Lord_Howe",
        "Pacific/Chatham",
        "America/St_Johns",
        "Asia/Kathmandu",
    ]

    class Col(enum.Enum):
        RED = "red"
        TWO = 2
        DAY = dt.date(2024, 1, 1)

    @dataclasses.dataclass
    class Inner:
        a: int
        when: object = None

    @dataclasses.dataclass(slots=True)
    class Slotted:
        x: float
        _hidden: int = 0
        inner: object = None

    def __init__(self, seed):
        self.rng = random.Random(seed)
        self.zi = [zoneinfo.ZoneInfo(z) for z in self.ZONES]

    def datetime(self):
        r = self.rng
        y = r.choice([1, 99, 999, 1970, 2000, 2024, 2037, 9999])
        d = dt.datetime(
            y,
            r.randint(1, 12),
            r.randint(1, 28),
            r.randint(0, 23),
            r.randint(0, 59),
            r.randint(0, 59),
            r.choice([0, 1, 999999, r.randint(0, 999999)]),
            fold=r.randint(0, 1),
        )
        k = r.random()
        if k < 0.3:
            return d
        if k < 0.5:
            return d.replace(tzinfo=UTC)
        if k < 0.75 or not 1970 <= y <= 2037:
            minutes = r.randint(-(23 * 60 + 59), 23 * 60 + 59)
            return d.replace(tzinfo=dt.timezone(dt.timedelta(minutes=minutes)))
        return d.replace(tzinfo=r.choice(self.zi))

    def value(self, depth=0):
        r = self.rng
        k = r.randrange(11 if depth < 3 else 7)
        if k == 0:
            return self.datetime()
        if k == 1:
            return self.datetime().date()
        if k == 2:
            return dt.time(r.randint(0, 23), r.randint(0, 59), r.randint(0, 59), r.choice([0, 5]))
        if k == 3:
            return uuid.UUID(int=r.getrandbits(128))
        if k == 4:
            return r.choice(list(self.Col))
        if k == 5:
            return r.choice([1, 2.5, "s", "é😀", None, True])
        if k == 6:
            return self.Inner(r.randint(0, 9), r.choice([None, self.datetime()]))
        if k == 7:
            return [self.value(depth + 1) for _ in range(r.randint(0, 4))]
        if k == 8:
            return {f"k{i}": self.value(depth + 1) for i in range(r.randint(0, 4))}
        if k == 9:
            return self.Slotted(r.random(), 1, self.Inner(1))
        return self.Col.DAY


@pytest.mark.parametrize("seed", range(4))
def test_matches_orjson(seed):
    orjson = pytest.importorskip("orjson")
    gen = _Diff(seed)
    for _ in range(1500):
        v = gen.value()
        assert both(v) == orjson.dumps(v), repr(v)
