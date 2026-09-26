"""loads(data, lenient=True): accept what json.loads accepts (issue #7).

With lenient=True the result must equal json.loads's for every input json.loads
accepts. NaN/Infinity/-Infinity, a UTF-8 BOM on bytes and numbers overflowing to
infinity are parsed natively; lone surrogates, UTF-16/UTF-32 bytes and nesting
beyond 1024 go through json.loads itself. The default stays strict.
"""

import json
import math
import random
import sys

import pytest
import rjson

sys.setrecursionlimit(max(sys.getrecursionlimit(), 20000))


def as_json_input(data):
    return bytes(data) if isinstance(data, memoryview) else data


def assert_matches_json(data):
    want = json.loads(as_json_input(data))
    got = rjson.loads(data, lenient=True)
    assert repr(got) == repr(want)
    return got


ACCEPTED = [
    # NaN / Infinity (json.dumps writes them by default: allow_nan=True)
    b"NaN",
    b"Infinity",
    b"-Infinity",
    b'[NaN, Infinity, -Infinity, 1]',
    b'{"a": NaN, "b": {"c": [Infinity]}}',
    # numbers overflowing to infinity
    b"[1e400, -1e400, 1.5e99999, 1E400, -1E+400]",
    # UTF-8 BOM on bytes (json.loads decodes as utf-8-sig)
    b'\xef\xbb\xbf{"a": 1}',
    b"\xef\xbb\xbf  [NaN]",
    # escaped lone surrogates (JavaScript clients truncating mid-emoji)
    b'["\\ud800"]',
    b'["\\udc00x"]',
    b'["\\udc00\\ud800"]',
    b'["\\ud83dabc"]',
    b'{"\\ud800": "\\udfff"}',
    # raw lone surrogates: a str holding one, bytes in surrogatepass UTF-8
    '["\ud800"]',
    b'["\xed\xa0\x80"]',
    # UTF-16 / UTF-32 (BOM or null-byte detection, as json.detect_encoding)
    '{"a": [1, "é\U0001F600"]}'.encode("utf-16"),
    '{"a": 1}'.encode("utf-16-le"),
    '{"a": 1}'.encode("utf-16-be"),
    '{"a": [NaN]}'.encode("utf-32"),
    '["x"]'.encode("utf-32-le"),
    # nesting deeper than rjson's 1024 levels
    b"[" * 2000 + b"]" * 2000,
    # ordinary JSON is unaffected
    b'{"id": 1, "tags": ["x"], "f": 2.5, "n": null, "big": 123456789012345678901234567890}',
    '{"é": "日本"}',
]


@pytest.mark.parametrize("data", ACCEPTED, ids=lambda d: repr(d)[:40])
def test_matches_json_loads(data):
    assert_matches_json(data)


@pytest.mark.parametrize("kind", [bytes, bytearray, memoryview])
def test_bytes_like_inputs(kind):
    for data in (b'\xef\xbb\xbf[NaN, 1e999]', b'["\\ud800"]', '[1, "é"]'.encode("utf-16")):
        assert_matches_json(kind(data))


def test_strided_memoryview_fallback():
    raw = b'["\\ud800"]'
    spaced = bytes(b for c in raw for b in (c, 0))
    view = memoryview(spaced)[::2]
    assert repr(rjson.loads(view, lenient=True)) == repr(json.loads(raw))


def test_nan_values_are_floats():
    v = rjson.loads(b"[NaN, Infinity, -Infinity]", lenient=True)
    assert math.isnan(v[0]) and v[1] == math.inf and v[2] == -math.inf
    assert all(type(x) is float for x in v)


REJECTED_BY_BOTH = [
    b"[nan]",
    b"[+Infinity]",
    b"[-NaN]",
    b"[infinity]",
    b"NaNx",
    b'["a\tb"]',  # raw control character (json's strict=True default)
    b"[1,]",
    b"",
    b"   ",
    b"\xef\xbb\xbf",
    "﻿[1]",  # json.loads rejects a BOM in a str too
    b"[\xff]",
    b'{"a" 1}',
]


def json_error(data):
    with pytest.raises(ValueError) as e:
        json.loads(data)
    return e.value


def strict_error(data):
    with pytest.raises(rjson.JSONDecodeError) as e:
        rjson.loads(data)
    return e.value


def assert_error_rule(data):
    """Rejected by both: rjson's own error, unless json.loads got further
    (an error past something only the fallback accepts); then json's."""
    theirs = json_error(data)
    with pytest.raises(rjson.JSONDecodeError) as e:
        rjson.loads(data, lenient=True)
    got = e.value
    bom = isinstance(data, bytes) and data.startswith(b"\xef\xbb\xbf")
    ours = strict_error(data[3:] if bom else data)
    if isinstance(theirs, json.JSONDecodeError) and theirs.pos > ours.pos:
        # Reported where json.loads found it; the message is rjson's when the
        # native lenient parser got there itself (b"NaNx": trailing content).
        assert got.pos == theirs.pos
    else:
        assert (got.msg, got.pos) == (ours.msg, ours.pos)
    return got


@pytest.mark.parametrize("data", REJECTED_BY_BOTH, ids=lambda d: repr(d)[:30])
def test_rejected_by_both(data):
    assert_error_rule(data)


def test_rejected_by_both_uses_rjsons_message():
    # Where json.loads stops no later than rjson, the message is rjson's.
    for data in (b"[nan]", b'["a\tb"]', b"", b'{"a" 1}', b"[\xff]"):
        got = assert_error_rule(data)
        assert "Expecting" not in got.msg


def test_error_past_a_lenient_construct_is_reported_where_it_is():
    # rjson stops at the lone surrogate (accepted in lenient mode); the real
    # error is the trailing comma, which json.loads reports.
    data = b'["\\ud800",]'
    theirs = json_error(data)
    with pytest.raises(rjson.JSONDecodeError) as e:
        rjson.loads(data, lenient=True)
    assert (e.value.msg, e.value.pos) == (theirs.msg, theirs.pos)
    assert e.value.pos >= 9


class TestStrictDefault:
    @pytest.mark.parametrize("data", ACCEPTED[:16], ids=lambda d: repr(d)[:40])
    def test_default_rejects(self, data):
        for kw in ({}, {"lenient": False}, {"lenient": None}, {"lenient": 0}):
            with pytest.raises(rjson.JSONDecodeError):
                rjson.loads(data, **kw)

    def test_default_never_calls_json(self, monkeypatch):
        def boom(*a, **k):
            raise AssertionError("json.loads called")

        monkeypatch.setattr(json, "loads", boom)
        with pytest.raises(rjson.JSONDecodeError):
            rjson.loads(b'["\\ud800"]')


class TestNativePaths:
    """The common cases must not go through json.loads (speed)."""

    @pytest.mark.parametrize(
        "data",
        [b"[NaN, Infinity, -Infinity]", b'\xef\xbb\xbf{"a": [NaN]}', b"[1e400, -1e999]"],
    )
    def test_native(self, data, monkeypatch):
        want = repr(json.loads(data))

        def boom(*a, **k):
            raise AssertionError("json.loads called")

        monkeypatch.setattr(json, "loads", boom)
        assert repr(rjson.loads(data, lenient=True)) == want

    def test_fallback_errors_other_than_valueerror_propagate(self, monkeypatch):
        def interrupted(*a, **k):
            raise KeyboardInterrupt

        monkeypatch.setattr(json, "loads", interrupted)
        with pytest.raises(KeyboardInterrupt):
            rjson.loads(b'["\\ud800"]', lenient=True)


class TestArguments:
    def test_truthy_values(self):
        for v in (True, 1, "yes"):
            assert math.isnan(rjson.loads(b"NaN", lenient=v))

    def test_bad_truthiness_propagates(self):
        class NoBool:
            def __bool__(self):
                raise ValueError("no bool")

        with pytest.raises(ValueError, match="no bool"):
            rjson.loads(b"1", lenient=NoBool())

    def test_argument_errors(self):
        with pytest.raises(TypeError, match="unexpected keyword argument 'strict'"):
            rjson.loads(b"1", strict=False)
        with pytest.raises(TypeError, match="exactly one positional argument"):
            rjson.loads(b"1", True)
        with pytest.raises(TypeError):
            rjson.loads()
        with pytest.raises(TypeError):
            rjson.loads(data=b"1")

    def test_wrong_input_type_is_a_type_error(self):
        with pytest.raises(TypeError):
            rjson.loads(123, lenient=True)

    def test_signature(self):
        import inspect

        p = inspect.signature(rjson.loads).parameters
        assert p["lenient"].kind is inspect.Parameter.KEYWORD_ONLY
        assert p["lenient"].default is False


def test_no_reference_leaks():
    data = [b'["\\ud800", NaN]', b"[1e400]", '[1]'.encode("utf-16"), b"[1,]"]
    for d in data:
        try:
            rjson.loads(d, lenient=True)
        except rjson.JSONDecodeError:
            pass
    before = [sys.getrefcount(d) for d in data]
    for _ in range(300):
        for d in data:
            try:
                rjson.loads(d, lenient=True)
            except rjson.JSONDecodeError:
                pass
    assert [sys.getrefcount(d) for d in data] == before


# ---------------------------------------------------------------------------
# Differential fuzz against json.loads
# ---------------------------------------------------------------------------


def _random_value(rng, depth=0):
    k = rng.randrange(9 if depth < 4 else 6)
    if k == 0:
        return rng.choice([float("nan"), float("inf"), -float("inf")])
    if k == 1:
        return rng.choice([1, -5, 2**70, 0.1, -2.5e-300, 1e308])
    if k == 2:
        chars = [chr(rng.choice([0x41, 0xE9, 0x65E5, 0x1F600, 0xD800, 0xDC00, 0xDBFF, 0xDFFF])) for _ in range(rng.randint(0, 5))]
        return "".join(chars)
    if k == 3:
        return rng.choice([None, True, False])
    if k == 4:
        return "plain"
    if k == 5:
        return rng.randint(-10, 10)
    if k == 6:
        return [_random_value(rng, depth + 1) for _ in range(rng.randint(0, 4))]
    return {f"k{i}": _random_value(rng, depth + 1) for i in range(rng.randint(0, 4))}


@pytest.mark.parametrize("seed", range(4))
def test_fuzz_matches_json_loads(seed):
    rng = random.Random(seed)
    for _ in range(600):
        text = json.dumps(_random_value(rng), ensure_ascii=rng.random() < 0.7)
        if rng.random() < 0.15:
            text = text.replace("Infinity", "1e999", 1)
        k = rng.random()
        try:
            if k < 0.3:
                data = text
            elif k < 0.6:
                data = text.encode("utf-8", "surrogatepass")
            elif k < 0.75:
                data = b"\xef\xbb\xbf" + text.encode("utf-8", "surrogatepass")
            else:
                data = text.encode(rng.choice(["utf-16", "utf-16-le", "utf-32", "utf-32-be"]), "surrogatepass")
        except UnicodeEncodeError:
            continue
        try:
            want = repr(json.loads(data))
        except ValueError:
            with pytest.raises(rjson.JSONDecodeError):
                rjson.loads(data, lenient=True)
            continue
        assert repr(rjson.loads(data, lenient=True)) == want, repr(data)[:200]
