"""Regression tests for the dumps / dumps_bytes serializer.

The reference is the stdlib: ``json.dumps(obj, ensure_ascii=False,
separators=(",", ":"))``. Floats are excluded from those comparisons because
rjson uses the shortest round-trip representation (like orjson).
"""

import collections
import enum
import json
import sys

import pytest
import rjson


def ref(obj):
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"))


def both(obj):
    """Serialize with both entry points and check they agree."""
    s = rjson.dumps(obj)
    b = rjson.dumps_bytes(obj)
    assert isinstance(s, str)
    assert isinstance(b, bytes)
    assert s.encode("utf-8") == b
    return s


ALPHABET = ["a", " ", '"', "\\", "\n", "\t", "\r", "\b", "\f", "\x00", "\x01", "\x1f", "\x7f",
            "/", "é", "ÿ", "Ā", "日", "￿", "😀", "\U0010ffff"]


class TestStrings:
    @pytest.mark.parametrize("n", list(range(0, 70)) + [100, 255, 256, 1000, 70000])
    def test_escape_lengths(self, n):
        for ch in ALPHABET:
            for s in (ch * n, "x" * n + ch, ch + "y" * n, ("ab" + ch) * n):
                assert both(s) == ref(s)
                assert both({s: s}) == ref({s: s})

    def test_escape_heavy_no_overflow(self):
        # Used to overflow the output buffer (escapes expand 6x).
        s = "\x01" * 100 + "a" * 1600
        assert both([s] * 50) == ref([s] * 50)
        s = "\x01" * 70000 + '"' * 70000
        assert both([s, s]) == ref([s, s])

    def test_all_control_chars(self):
        s = "".join(chr(i) for i in range(0x80))
        assert both(s) == ref(s)

    @pytest.mark.parametrize("text", ["héllo", "日本語", "😀 emoji", "mixed é 日 😀", "ÿ\n", "日\\", "😀\x00"])
    def test_non_ascii_kinds(self, text):
        data = {"k": text, text: [text, text * 50, {"x": text}], "ascii": "plain"}
        s = both(data)
        assert s == ref(data)
        # The str result must be canonical (smallest kind) for == to work.
        assert json.loads(s) == data

    def test_str_result_kinds(self):
        for obj in (["a", "é"], ["a", "日"], ["a", "😀"], ["é", "日", "😀"], ["plain"]):
            s = rjson.dumps(obj)
            assert s == ref(obj)
            assert max(map(ord, s)) == max(map(ord, ref(obj)))

    def test_lone_surrogates(self):
        # str output behaves like json.dumps(ensure_ascii=False) ...
        for s in ("\ud800", "a\udfffb", "\ud800\n"):
            assert rjson.dumps([s]) == ref([s])
        # ... bytes output cannot encode them.
        with pytest.raises(UnicodeEncodeError):
            rjson.dumps_bytes(["a\ud800b"])
        with pytest.raises(UnicodeEncodeError):
            rjson.dumps_bytes({"\ud800": 1})


class TestNumbers:
    def test_int_boundaries(self):
        vals = [0, 1, -1, 9, 10, 99, 100, 999, 1000, 9999, 10000, 123456, 2**30 - 1, 2**30, -(2**30),
                2**31, 2**32, 2**60 - 1, 2**60, -(2**60), 2**63 - 1, -(2**63), 2**63, 2**64 - 1,
                2**64, -(2**64), 10**30, -(10**30)]
        assert both(vals) == ref(vals)
        for v in vals:
            assert both(v) == str(v)

    def test_huge_int_raises(self):
        if not hasattr(sys, "get_int_max_str_digits"):
            pytest.skip("no int max str digits limit")
        with pytest.raises(ValueError):
            rjson.dumps(10**5000)
        with pytest.raises(ValueError):
            rjson.dumps_bytes([10**5000])

    def test_float_format(self):
        vals = [0.0, -0.0, 1.0, 0.1, 1.5, 123456789.0, 1e16, 1e-7, 5e-324, 1.7976931348623157e308]
        out = json.loads(both(vals))
        assert out == vals
        assert both([1e16, 1e-7]) == "[1e+16,1e-7]"

    @pytest.mark.parametrize("v", [float("nan"), float("inf"), float("-inf")])
    def test_non_finite(self, v):
        for f in (rjson.dumps, rjson.dumps_bytes):
            with pytest.raises(ValueError, match="non-finite"):
                f([1.0, v])


class TestHomogeneousListsWithOddTail:
    """The old bulk path sampled 16 elements and trusted the rest."""

    @pytest.mark.parametrize("obj", [
        [1] * 16 + [True],
        [1] * 16 + ["x"],
        [1.0] * 16 + [7],
        [1.5] * 16 + ["x"],
        [True] * 16 + [None, 1],
        ["a"] * 16 + [10**30],
        ["a"] * 16 + [12345],
    ])
    def test_mixed(self, obj):
        assert both(obj) == ref(obj)


class TestListScalarRun:
    """The list fast loop writes runs of exact ints/floats and must hand every
    other item (including subclasses and big ints) to the generic path."""

    class I(int):
        def __str__(self):
            return "nope"

    class F(float):
        def __repr__(self):
            return "nope"

    def test_late_type_switch(self):
        for tail in (True, False, None, "x", "é", 2**60, -(2**61), 10**30, [1, 2], {"a": 1}, (3,),
                     self.I(7), self.F(2.5), 1.5, 3):
            for n in (0, 1, 2, 15, 16, 17, 1000, 5000):
                for obj in ([7] * n + [tail] + [8] * 3, [0.5] * n + [tail] + [1] * 3 + [0.25]):
                    want = ref([float(x) if type(x) is self.F else x for x in obj])
                    assert both(obj) == want

    def test_int_boundaries_in_runs(self):
        vals = []
        for b in (29, 30, 31, 59, 60, 61, 63, 64):
            vals += [2**b - 1, 2**b, 2**b + 1, -(2**b) + 1, -(2**b), -(2**b) - 1]
        vals += [0, -0, 9, 10, 99999, 100000, 99999999, 100000000, 999999999, 10**9]
        vals += [-v for v in vals]
        assert both(vals) == ref(vals)
        assert both(vals * 50) == ref(vals * 50)

    def test_subclass_items(self):
        obj = [1, self.I(2), 3, self.F(1.5), 2.5, self.I(2**70)]
        assert both(obj) == "[1,2,3,1.5,2.5,1180591620717411303424]"

    @pytest.mark.parametrize("v", [float("nan"), float("inf"), float("-inf")])
    def test_non_finite_late(self, v):
        for f in (rjson.dumps, rjson.dumps_bytes):
            with pytest.raises(ValueError, match="non-finite"):
                f([1.0] * 1000 + [v])
            with pytest.raises(ValueError, match="non-finite"):
                f([1] * 1000 + [v])

    def test_growth_inside_run(self):
        # Output far larger than the size hint of the previous call.
        rjson.dumps([])
        for obj in ([-(2**59)] * 20000, [1.2345678901234567e-300] * 20000, list(range(10**6))):
            assert json.loads(both(obj)) == obj


class TestSubclasses:
    def test_str_subclass(self):
        class S(str):
            pass
        for text in ("abcdef", "日本語", "é\n"):
            obj = {S(text): S(text)}
            assert both(obj) == ref(obj)

    def test_int_float_subclasses(self):
        class I(int):
            def __str__(self):
                return "nope"

        class F(float):
            pass

        class E(enum.IntEnum):
            A = 5

        obj = [I(3), I(2**70), F(1.5), E.A]
        assert both(obj) == "[3,1180591620717411303424,1.5,5]"

    def test_container_subclasses(self):
        class D(dict):
            pass

        class L(list):
            pass

        P = collections.namedtuple("P", "x y")
        obj = [D(a=1), L([1, 2]), P(1, 2), collections.OrderedDict(b=2, a=1), collections.defaultdict(int, z=0)]
        assert both(obj) == ref(obj)

    def test_unsupported(self):
        for f in (rjson.dumps, rjson.dumps_bytes):
            with pytest.raises(ValueError, match="Unsupported Python type"):
                f([1, {"a": object()}])
            with pytest.raises(ValueError, match="keys must be strings"):
                f({1: 2})


class TestRecursion:
    def test_circular(self):
        a = []
        a.append(a)
        d = {}
        d["d"] = d
        for f in (rjson.dumps, rjson.dumps_bytes):
            with pytest.raises(ValueError, match="depth"):
                f(a)
            with pytest.raises(ValueError, match="depth"):
                f(d)

    def test_depth_limit(self):
        def nest(n):
            x = []
            for _ in range(n):
                x = [x]
            return x
        assert both(nest(253)) == "[" * 254 + "]" * 254
        with pytest.raises(ValueError):
            rjson.dumps(nest(254))
        with pytest.raises(ValueError):
            rjson.dumps(nest(200000))


class TestOutputBuffer:
    def test_varying_sizes(self):
        # The output buffer is sized from the previous result on the thread.
        big = {"k%d" % i: ["v" * 100, i, None, True] for i in range(20000)}
        small = {"a": [1, 2, "é"]}
        for obj in (big, small, big, [], small, "x" * 10, big, "é" * 3):
            assert both(obj) == ref(obj)

    def test_top_level_scalars(self):
        for obj in (None, True, False, 0, -5, "", "x", "é", [], {}, ()):
            assert both(obj) == ref(obj)


@pytest.mark.skipif(not sys.platform.startswith("linux"), reason="glibc malloc behaviour")
def test_large_output_does_not_refault_every_call():
    # Shrinking each result by the capacity headroom made every call free a
    # block smaller than the next request, so glibc kept serving it with a
    # fresh mmap and every call page-faulted its whole output.
    import resource

    s = "\U0001f600" * 400000  # 1.6 MB of UTF-8
    for _ in range(5):
        rjson.dumps_bytes(s)
    before = resource.getrusage(resource.RUSAGE_SELF).ru_minflt
    for _ in range(20):
        assert len(rjson.dumps_bytes(s)) == 1600002
    faults = (resource.getrusage(resource.RUSAGE_SELF).ru_minflt - before) / 20
    assert faults < 100  # was ~390 (one per 4 KiB page)
