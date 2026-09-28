"""dumps(indent=..., sort_keys=...) (issues #23, #24).

indent=2 must equal orjson's OPT_INDENT_2 and sort_keys=True its OPT_SORT_KEYS
byte for byte; other indent widths follow json.dumps(indent=n). The differential
tests against orjson run when it is installed.
"""

import dataclasses
import datetime as dt
import enum
import json
import random
import uuid

import pytest
import rjson

try:
    import orjson
except ImportError:  # pragma: no cover
    orjson = None

needs_orjson = pytest.mark.skipif(orjson is None, reason="orjson not installed")

KEY_CHARS = 'abcAB_ -"\\\n\t\x01/é日😀' + "0123456789"


def rand_key(rng):
    return "".join(rng.choice(KEY_CHARS) for _ in range(rng.randint(0, 6)))


def rand_doc(rng, depth=0):
    k = rng.random()
    if depth > 4 or k < 0.35:
        return rng.choice([
            None, True, False, rng.randint(-10**12, 10**12), rng.random() * 1e6,
            rand_key(rng), "", 0, [], {},
        ])
    if k < 0.65:
        return [rand_doc(rng, depth + 1) for _ in range(rng.randint(0, 5))]
    return {rand_key(rng): rand_doc(rng, depth + 1) for _ in range(rng.randint(0, 6))}


def both(obj, **kw):
    """dumps and dumps_str agree; return the bytes."""
    b = rjson.dumps(obj, **kw)
    assert rjson.dumps_str(obj, **kw) == b.decode()
    return b


class Shape(enum.Enum):
    SQUARE = "square"


@dataclasses.dataclass
class Point:
    y: int
    x: int
    meta: dict


@needs_orjson
@pytest.mark.parametrize("seed", range(4))
def test_random_documents_match_orjson(seed):
    rng = random.Random(seed)
    for _ in range(300):
        doc = rand_doc(rng)
        for kw, opt in (
            ({"indent": 2}, orjson.OPT_INDENT_2),
            ({"sort_keys": True}, orjson.OPT_SORT_KEYS),
            ({"indent": 2, "sort_keys": True}, orjson.OPT_INDENT_2 | orjson.OPT_SORT_KEYS),
        ):
            assert both(doc, **kw) == orjson.dumps(doc, option=opt), (seed, kw, doc)


@needs_orjson
def test_native_types_dataclasses_and_non_str_keys_match_orjson():
    doc = {
        "z": [Point(2, 1, {"b": 1, "a": 2})],
        "at": dt.datetime(2024, 5, 1, 9, 30, tzinfo=dt.timezone.utc),
        "id": uuid.UUID(int=7),
        "shape": Shape.SQUARE,
        "a": {"nested": {"y": 1, "x": {}}},
    }
    for kw, opt in (
        ({"indent": 2}, orjson.OPT_INDENT_2),
        ({"sort_keys": True}, orjson.OPT_SORT_KEYS),
        ({"indent": 2, "sort_keys": True}, orjson.OPT_INDENT_2 | orjson.OPT_SORT_KEYS),
    ):
        # Dataclass fields keep their order; dicts inside are sorted (as orjson).
        assert both(doc, **kw) == orjson.dumps(doc, option=opt)
    keys = {3: "a", 1: "b", "2": "c", None: 1, Shape.SQUARE: 2, 10: 3}
    assert rjson.dumps(keys, non_str_keys=True, sort_keys=True) == orjson.dumps(
        keys, option=orjson.OPT_NON_STR_KEYS | orjson.OPT_SORT_KEYS
    )


@pytest.mark.parametrize("width", [0, 1, 2, 4, 8])
def test_indent_widths_match_json(width):
    rng = random.Random(width)
    for _ in range(200):
        doc = rand_doc(rng)
        # floats aside (rjson writes the shortest form, json uses repr)
        doc = json.loads(json.dumps(doc), parse_float=lambda s: 1)
        want = json.dumps(doc, indent=width, sort_keys=True, ensure_ascii=False)
        assert rjson.dumps_str(doc, indent=width, sort_keys=True) == want
        want = json.dumps(doc, indent=width, ensure_ascii=False)
        assert rjson.dumps_str(doc, indent=width) == want


def test_layout():
    assert rjson.dumps({"a": [1, {}], "b": []}, indent=2) == (
        b'{\n  "a": [\n    1,\n    {}\n  ],\n  "b": []\n}'
    )
    assert rjson.dumps([], indent=2) == b"[]"
    assert rjson.dumps("x", indent=2) == b'"x"'
    assert rjson.dumps({"s": "a,b:{c}[d]\"e"}, indent=2) == b'{\n  "s": "a,b:{c}[d]\\"e"\n}'


def test_sort_order_is_code_point_order():
    keys = ["b", "B", "a", "é", "e", "日", "😀", "\x01", '"', "\\", "", "ab", "a b"]
    got = json.loads(rjson.dumps({k: i for i, k in enumerate(keys)}, sort_keys=True))
    assert list(got) == sorted(keys)


def test_default_and_guarded_mode():
    class Money:
        def __init__(self, v):
            self.v = v

    doc = {"z": Money(2), "a": [Money(1)], "m": {"q": 1, "p": 2}}
    out = rjson.dumps(doc, default=lambda o: {"v": o.v, "cur": "EUR"}, sort_keys=True)
    assert out == b'{"a":[{"cur":"EUR","v":1}],"m":{"p":2,"q":1},"z":{"cur":"EUR","v":2}}'


def test_dict_changed_during_default_still_raises():
    parent = {"x": None, "y": 2}

    def grow(o):
        parent.update({f"k{i}": i for i in range(50)})
        return 1

    parent["x"] = object()
    with pytest.raises(RuntimeError, match="changed size"):
        rjson.dumps({"p": parent}, default=grow, sort_keys=True)


def test_errors_and_options():
    with pytest.raises(rjson.JSONEncodeError):
        rjson.dumps({"a": object()}, indent=2)
    with pytest.raises(rjson.JSONEncodeError):
        rjson.dumps({1: 2}, sort_keys=True)  # non-str key without non_str_keys
    with pytest.raises(TypeError, match="indent must be None, an int or a str"):
        rjson.dumps({}, indent=2.0)
    with pytest.raises(TypeError):
        rjson.dumps({}, indent=True)
    with pytest.raises(ValueError, match="between 0 and 1024"):
        rjson.dumps({}, indent=-1)
    with pytest.raises(TypeError, match="separators must be"):
        rjson.dumps({}, separators=(",",))
    with pytest.raises(TypeError, match="separators must be"):
        rjson.dumps({}, separators=1)
    with pytest.raises(TypeError, match="key_separator must be str"):
        rjson.dumps({}, separators=(",", b":"))
    with pytest.raises(UnicodeEncodeError):
        rjson.dumps({}, indent="\ud800")
    assert rjson.dumps({"b": 1, "a": 2}, indent=None, sort_keys=False) == b'{"b":1,"a":2}'
    assert rjson.dumps({"b": [1]}, separators=[",", ":"]) == b'{"b":[1]}'
    # Lone surrogates: dumps (bytes) raises unless ensure_ascii escapes them;
    # dumps_str keeps them when formatting too, as json.dumps does.
    assert rjson.dumps_str("\ud800") == '"\ud800"'
    assert rjson.dumps_str(["\ud800"], indent=2) == '[\n  "\ud800"\n]'
    with pytest.raises(UnicodeEncodeError):
        rjson.dumps("\ud800", indent=2)
    assert rjson.dumps("\ud800", ensure_ascii=True) == b'"\\ud800"'


def test_deep_nesting_limits_unchanged():
    x = {}
    for _ in range(253):
        x = {"k": x}
    out = rjson.dumps(x, indent=1, sort_keys=True)
    assert json.loads(out) == x
    for _ in range(2):
        x = {"k": x}
    with pytest.raises(rjson.JSONEncodeError, match="nesting depth"):
        rjson.dumps(x, sort_keys=True)


# json.dumps options (ensure_ascii, separators, str indent, allow_nan): output
# equal to json.dumps with the same options (floats aside: rjson writes the
# shortest form, json uses repr; both agree on NaN/Infinity and on ints).

TEXT_CHARS = 'aZ "\\/\n\t\x00\x1f\x7f\x80é\xff日\uffff😀\U0010ffff\ud800\udfff'


def rand_text(rng):
    return "".join(rng.choice(TEXT_CHARS) for _ in range(rng.randint(0, 8)))


def rand_json_doc(rng, depth=0):
    k = rng.random()
    if depth > 4 or k < 0.35:
        return rng.choice([
            None, True, False, rng.randint(-10**20, 10**20), rand_text(rng), "",
            float("nan"), float("inf"), float("-inf"), 0.5, -2.0, [], {},
        ])
    if k < 0.65:
        return [rand_json_doc(rng, depth + 1) for _ in range(rng.randint(0, 5))]
    return {rand_text(rng): rand_json_doc(rng, depth + 1) for _ in range(rng.randint(0, 6))}


JSON_OPTION_SETS = [
    {"ensure_ascii": True},
    {"ensure_ascii": True, "indent": 4},
    {"ensure_ascii": False, "indent": "\t"},
    {"ensure_ascii": True, "indent": "\t", "sort_keys": True},
    {"ensure_ascii": False, "indent": None, "separators": (", ", ": ")},
    {"ensure_ascii": True, "separators": (", ", ": "), "sort_keys": True},
    {"ensure_ascii": False, "indent": 2, "separators": (" ,", " = ")},
    {"ensure_ascii": True, "indent": "->", "separators": (";", "=")},
    {"ensure_ascii": False, "indent": 0},
    {"ensure_ascii": False, "indent": ""},
    {"ensure_ascii": True, "separators": ("", "")},
    {"ensure_ascii": False, "separators": ("é,", "日:"), "indent": 1},
]


@pytest.mark.parametrize("opts", JSON_OPTION_SETS, ids=repr)
def test_json_dumps_options_match_json(opts):
    rng = random.Random(repr(opts))
    for _ in range(300):
        doc = rand_json_doc(rng)
        json_opts = dict(opts)
        if opts.get("indent") is None:
            # json's one-line default is (", ", ": "); rjson stays compact.
            json_opts.setdefault("separators", (",", ":"))
        # sort_keys: json sorts by Python's str order, code point order too.
        want = json.dumps(doc, allow_nan=True, **json_opts)
        got = rjson.dumps_str(doc, allow_nan=True, **opts)
        assert got == want, (opts, doc)
        if opts["ensure_ascii"]:
            # ASCII output: dumps can encode lone surrogates too.
            assert rjson.dumps(doc, allow_nan=True, **opts) == want.encode()


def test_ensure_ascii_escapes():
    assert rjson.dumps("é日😀\x7f\x7e", ensure_ascii=True) == b'"\\u00e9\\u65e5\\ud83d\\ude00\\u007f~"'
    assert rjson.dumps_str({"ключ": "\ud83d\ude00"}, ensure_ascii=True) == '{"\\u043a\\u043b\\u044e\\u0447":"\\ud83d\\ude00"}'
    assert rjson.dumps("x" * 100 + "é" + "y" * 100, ensure_ascii=True) == b'"' + b"x" * 100 + b"\\u00e9" + b"y" * 100 + b'"'
    # Long non-ASCII strings take other encoder paths (UCS2/UCS4, Latin-1).
    for s in ("é" * 5000, "日" * 5000, "😀" * 5000, "日" * 4999 + "\ud800"):
        assert rjson.dumps(s, ensure_ascii=True) == json.dumps(s).encode()
    assert rjson.dumps("abc", ensure_ascii=False) == b'"abc"'
    assert rjson.dumps("é", ensure_ascii=0) == '"é"'.encode()


def test_allow_nan():
    doc = [float("nan"), float("inf"), float("-inf"), 1.5, {"x": float("nan")}]
    assert rjson.dumps(doc, allow_nan=True) == b'[NaN,Infinity,-Infinity,1.5,{"x":NaN}]'
    assert rjson.dumps_str(doc, allow_nan=True, indent=1) == json.dumps(doc, indent=1)
    assert rjson.loads(rjson.dumps(doc, allow_nan=True), lenient=True)[1] == float("inf")
    for v in (float("nan"), [1.0, float("inf")], {"a": float("-inf")}):
        with pytest.raises(rjson.JSONEncodeError, match="non-finite"):
            rjson.dumps(v)
        with pytest.raises(rjson.JSONEncodeError):
            rjson.dumps(v, allow_nan=False)
    assert rjson.dumps({float("nan"): 1}, non_str_keys=True, allow_nan=True) == b'{"NaN":1}'


def test_json_options_with_default_and_native_types():
    class Money:
        def __init__(self, v):
            self.v = v

    doc = {"é": Money(1), "at": dt.datetime(2024, 5, 1, tzinfo=dt.timezone.utc), "s": Shape.SQUARE}
    out = rjson.dumps_str(doc, default=lambda o: {"v": o.v, "cur": "€"}, ensure_ascii=True,
                          indent="\t", separators=(",", ":"), sort_keys=True)
    assert out == (
        '{\n\t"at":"2024-05-01T00:00:00+00:00",\n\t"s":"square",\n'
        '\t"\\u00e9":{\n\t\t"cur":"\\u20ac",\n\t\t"v":1\n\t}\n}'
    )
