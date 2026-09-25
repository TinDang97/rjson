"""
Comprehensive test suite for rjson library.

Tests cover:
- Basic type serialization/deserialization
- Edge cases (empty collections, None, large numbers)
- Error handling (invalid JSON, unsupported types, NaN/Infinity)
- Unicode and special characters
- Nested structures
- Round-trip consistency
"""

import pytest
import rjson
import json
import math
import re
import sys


class TestBasicTypes:
    """Test serialization and deserialization of basic Python types."""

    def test_none(self):
        assert rjson.dumps_str(None) == "null"
        assert rjson.loads("null") is None

    def test_bool_true(self):
        assert rjson.dumps_str(True) == "true"
        assert rjson.loads("true") is True

    def test_bool_false(self):
        assert rjson.dumps_str(False) == "false"
        assert rjson.loads("false") is False

    def test_integer_zero(self):
        assert rjson.dumps_str(0) == "0"
        assert rjson.loads("0") == 0

    def test_integer_positive(self):
        assert rjson.dumps_str(42) == "42"
        assert rjson.loads("42") == 42

    def test_integer_negative(self):
        assert rjson.dumps_str(-42) == "-42"
        assert rjson.loads("-42") == -42

    def test_integer_large(self):
        large_int = 9223372036854775807  # Max i64
        assert rjson.dumps_str(large_int) == str(large_int)
        assert rjson.loads(str(large_int)) == large_int

    def test_integer_very_large(self):
        # Python arbitrary precision int
        # Integers beyond 64 bits round-trip exactly (same as stdlib json)
        very_large = 123456789012345678901234567890
        result = rjson.dumps_str(very_large)
        loaded = rjson.loads(result)
        assert isinstance(loaded, int)
        assert loaded == very_large
        assert rjson.loads(str(-very_large)) == -very_large

    def test_float_zero(self):
        assert rjson.dumps_str(0.0) == "0.0"
        assert rjson.loads("0.0") == 0.0

    def test_float_positive(self):
        assert rjson.dumps_str(3.14) == "3.14"
        assert rjson.loads("3.14") == 3.14

    def test_float_negative(self):
        assert rjson.dumps_str(-3.14) == "-3.14"
        assert rjson.loads("-3.14") == -3.14

    def test_float_scientific(self):
        val = 1.23e-10
        serialized = rjson.dumps_str(val)
        assert rjson.loads(serialized) == pytest.approx(val)

    def test_string_empty(self):
        assert rjson.dumps_str("") == '""'
        assert rjson.loads('""') == ""

    def test_string_simple(self):
        assert rjson.dumps_str("hello") == '"hello"'
        assert rjson.loads('"hello"') == "hello"

    def test_string_with_spaces(self):
        assert rjson.dumps_str("hello world") == '"hello world"'
        assert rjson.loads('"hello world"') == "hello world"


class TestCollections:
    """Test serialization and deserialization of collections."""

    def test_list_empty(self):
        assert rjson.dumps_str([]) == "[]"
        assert rjson.loads("[]") == []

    def test_list_single(self):
        assert rjson.dumps_str([1]) == "[1]"
        assert rjson.loads("[1]") == [1]

    def test_list_multiple(self):
        assert rjson.dumps_str([1, 2, 3]) == "[1,2,3]"
        assert rjson.loads("[1,2,3]") == [1, 2, 3]

    def test_list_mixed_types(self):
        data = [1, "two", 3.0, None, True]
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_tuple_empty(self):
        # Tuples serialize as arrays
        assert rjson.dumps_str(()) == "[]"

    def test_tuple_single(self):
        assert rjson.dumps_str((1,)) == "[1]"

    def test_tuple_multiple(self):
        assert rjson.dumps_str((1, 2, 3)) == "[1,2,3]"

    def test_dict_empty(self):
        assert rjson.dumps_str({}) == "{}"
        assert rjson.loads("{}") == {}

    def test_dict_single(self):
        result = rjson.dumps_str({"a": 1})
        assert result == '{"a":1}'
        assert rjson.loads(result) == {"a": 1}

    def test_dict_multiple(self):
        data = {"a": 1, "b": 2, "c": 3}
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_dict_mixed_values(self):
        data = {"int": 1, "str": "hello", "float": 3.14, "none": None, "bool": True}
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data


class TestNestedStructures:
    """Test deeply nested data structures."""

    def test_nested_lists(self):
        data = [[1, 2], [3, 4], [5, 6]]
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_nested_dicts(self):
        data = {"outer": {"inner": {"deep": "value"}}}
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_mixed_nesting(self):
        data = {
            "users": [
                {"name": "Alice", "age": 30, "tags": ["python", "rust"]},
                {"name": "Bob", "age": 25, "tags": ["go", "javascript"]},
            ],
            "count": 2,
        }
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_deep_nesting(self):
        # Create deeply nested structure
        data = {"level": 0}
        current = data
        for i in range(1, 50):
            current["nested"] = {"level": i}
            current = current["nested"]

        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data


class TestUnicode:
    """Test Unicode and special character handling."""

    def test_unicode_simple(self):
        data = "hello 世界"
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_unicode_emoji(self):
        data = "Hello 👋 🌍"
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_unicode_various(self):
        data = {"русский": "текст", "中文": "文本", "العربية": "نص"}
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_escaped_characters(self):
        data = 'quote" backslash\\ newline\n tab\t'
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_control_characters(self):
        # Test various control characters
        data = "line1\nline2\rline3\tcolumn"
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data


class TestEdgeCases:
    """Test edge cases and boundary conditions."""

    def test_integer_cache_boundary_negative(self):
        # Test integer caching boundary at -256
        assert rjson.dumps_str(-256) == "-256"
        assert rjson.dumps_str(-257) == "-257"
        assert rjson.loads("-256") == -256
        assert rjson.loads("-257") == -257

    def test_integer_cache_boundary_positive(self):
        # Test integer caching boundary at 256
        assert rjson.dumps_str(256) == "256"
        assert rjson.dumps_str(257) == "257"
        assert rjson.loads("256") == 256
        assert rjson.loads("257") == 257

    def test_empty_string_key(self):
        data = {"": "empty key"}
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_string_with_quotes(self):
        data = 'He said "hello"'
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_list_of_empty_lists(self):
        data = [[], [], []]
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data

    def test_dict_of_empty_dicts(self):
        data = {"a": {}, "b": {}, "c": {}}
        serialized = rjson.dumps_str(data)
        assert rjson.loads(serialized) == data


class TestErrorHandling:
    """Test error handling for invalid inputs."""

    def test_dumps_nan_raises(self):
        with pytest.raises(ValueError, match="Cannot serialize non-finite float"):
            rjson.dumps_str(float("nan"))

    def test_dumps_infinity_raises(self):
        with pytest.raises(ValueError, match="Cannot serialize non-finite float"):
            rjson.dumps_str(float("inf"))

    def test_dumps_negative_infinity_raises(self):
        with pytest.raises(ValueError, match="Cannot serialize non-finite float"):
            rjson.dumps_str(float("-inf"))

    def test_dumps_unsupported_type_raises(self):
        class CustomClass:
            pass

        with pytest.raises(ValueError, match="Type is not JSON serializable"):
            rjson.dumps_str(CustomClass())

    def test_dumps_dict_non_string_key_raises(self):
        with pytest.raises(ValueError, match="keys must be strings"):
            rjson.dumps_str({1: "value"})

    def test_loads_invalid_json_raises(self):
        with pytest.raises(json.JSONDecodeError, match="expected a string key"):
            rjson.loads("{invalid json}")

    def test_loads_truncated_json_raises(self):
        with pytest.raises(json.JSONDecodeError, match="unexpected end of data"):
            rjson.loads('{"key": "incomplete')

    def test_loads_trailing_comma_raises(self):
        with pytest.raises(json.JSONDecodeError, match="trailing comma"):
            rjson.loads('[1, 2, 3,]')

    @pytest.mark.parametrize("doc", ["[1] x", "{} {}", "1 2", "null,"])
    def test_loads_trailing_characters_raises(self, doc):
        with pytest.raises(json.JSONDecodeError, match="unexpected content after document"):
            rjson.loads(doc)

    @pytest.mark.parametrize("conv", [str, lambda s: s.encode(), lambda s: bytearray(s.encode()), lambda s: memoryview(s.encode())])
    def test_loads_accepts_str_bytes_bytearray(self, conv):
        doc = '{"a": [1, 2.5, "héllo \U0001F600", null, true]}'
        assert rjson.loads(conv(doc)) == {"a": [1, 2.5, "héllo \U0001F600", None, True]}

    @pytest.mark.parametrize("bad", [None, 1, ["[]"], 1.5])
    def test_loads_rejects_other_input_types(self, bad):
        with pytest.raises(TypeError):
            rjson.loads(bad)

    def test_loads_invalid_utf8_bytes_raises(self):
        with pytest.raises(ValueError):
            rjson.loads(b'"\xff"')

    def test_loads_trailing_whitespace_ok(self):
        assert rjson.loads("[1] \n\t ") == [1]

    @pytest.mark.parametrize(
        "obj", ["\ud800", ["\ud800"], {"a": "\ud800"}, {"\ud800": 1}, ["x", 1, "\udfff"]]
    )
    def test_dumps_lone_surrogate_no_systemerror(self, obj):
        # Regression: used to return with an exception set (SystemError).
        # str output passes surrogates through like
        # json.dumps(ensure_ascii=False); bytes output cannot encode them.
        import json
        assert rjson.dumps_str(obj) == json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
        with pytest.raises(UnicodeEncodeError):
            rjson.dumps(obj)


class TestStringLayout:
    """ASCII fast path must honour the running interpreter's str layout
    (the data offset changed in CPython 3.12)."""

    @pytest.mark.parametrize("n", [0, 1, 7, 8, 15, 16, 31, 32, 33, 100, 1000])
    def test_ascii_lengths(self, n):
        s = "".join(chr(97 + i % 26) for i in range(n))
        assert rjson.dumps_str(s) == '"' + s + '"'
        assert rjson.dumps_str({s: [s, s]}) == '{"%s":["%s","%s"]}' % (s, s, s)

    def test_non_ascii_kinds(self):
        for s in ["caf\u00e9", "\u65e5\u672c", "\U0001F600", "a\u00e9\u65e5\U0001F600"]:
            assert rjson.loads(rjson.dumps_str(s)) == s
            assert rjson.loads(rjson.dumps_str([s, {s: s}])) == [s, {s: s}]


def _check_loads(s):
    """loads of a JSON string containing s, from str and bytes, and as a dict key."""
    doc = json.dumps([s, {s: s}], ensure_ascii=False)
    expected = [s, {s: s}]
    assert rjson.loads(doc) == expected
    assert rjson.loads(doc.encode()) == expected
    assert rjson.loads(doc.encode()) == json.loads(doc)


class TestUCS2Decoder:
    """UTF-8 -> UCS2 decoding (SIMD paths for 16-byte ASCII blocks and runs of
    five 3-byte sequences, two scalar characters per step otherwise). The
    vector stores write past the characters they account for, so lengths,
    offsets and run boundaries around the 16/15-byte blocks are exercised
    exhaustively."""

    # One character of each UTF-8 length that keeps the result UCS2
    # (a 3-byte character forces UCS2; 2-byte ones alone would too above U+00FF).
    ONE = ["a", " ", "\u00e9", "\u0416", "\u07ff", "\u0800", "\u65e5", "\uac00", "\uffee", "\ud7ff", "\ue000"]

    @pytest.mark.parametrize("n", range(0, 40))
    def test_runs_of_three_byte_chars(self, n):
        for ch in ("\u65e5", "\u0800", "\uffff", "\uac00"):
            _check_loads(ch * n)
            _check_loads("a" + ch * n)
            _check_loads(ch * n + "a")
            _check_loads("\u0416" + ch * n)  # 2-byte char before the run

    @pytest.mark.parametrize("prefix", range(0, 33))
    def test_each_char_kind_at_each_offset(self, prefix):
        # Place every character kind at every offset of two 16-byte blocks,
        # in a UCS2 string long enough to take the vector paths.
        for ch in self.ONE:
            s = "x" * prefix + ch + "\u65e5" * 12 + "y" * 20 + "\u65e5"
            _check_loads(s)
            s = "\u65e5" * prefix + ch + "\u65e5" * 7
            _check_loads(s)

    @pytest.mark.parametrize("n", range(0, 48))
    def test_ascii_runs_in_ucs2_strings(self, n):
        # ASCII runs of every length between CJK characters: 8- and 16-byte
        # widening, and a string that ends right after the run.
        _check_loads("\u65e5" + "a" * n)
        _check_loads("\u65e5" + "a" * n + "\u672c" * 6)
        _check_loads("a" * n + "\u65e5")

    @pytest.mark.parametrize("n", range(0, 40))
    def test_two_byte_runs(self, n):
        _check_loads("\u0416" * n + "\u65e5")  # Cyrillic run, then CJK (UCS2)
        _check_loads("\u65e5" + "\u0416" * n)
        _check_loads("\u65e5" + ("\u0416a" * n))

    def test_mixed_texts(self):
        texts = [
            "\u6211\u4eec\u5728\u5317\u4eac\u5927\u5b66\u5b66\u4e60\u4e2d\u6587\uff0c\u8fd9\u662f\u4e00\u4e2a\u53e5\u5b50\u3002",
            "\u65e5\u672c\u8a9e\u306e\u30c6\u30ad\u30b9\u30c8 ",
            "\ud55c\uad6d\uc5b4 \ud14d\uc2a4\ud2b8 \ucc98\ub9ac\ub294 \ube60\ub985\ub2c8\ub2e4. ",
            "\u0411\u044b\u0441\u0442\u0440\u044b\u0439 \u0440\u0430\u0437\u0431\u043e\u0440 JSON \u0432 Python. ",
            "User \u7530\u4e2d posted: \u4eca\u65e5\u306f\u3044\u3044\u5929\u6c17\u3067\u3059\u306d! (score 42) ",
        ]
        for t in texts:
            for reps in (1, 2, 3, 7, 20):
                _check_loads(t * reps)
                _check_loads(t * reps + "tail")

    def test_escapes_between_runs(self):
        # Escaped strings are unescaped into a scratch buffer, then decoded.
        s = "\u65e5\u672c\u8a9e\n\u30c6\u30ad\u30b9\u30c8\t\u65e5\u672c\u8a9e\u306e\u30c6\u30ad\u30b9\u30c8\"q\" \\ " * 5
        _check_loads(s)

    @pytest.mark.parametrize("pos", range(0, 20))
    def test_invalid_utf8_near_blocks(self, pos):
        # Invalid bytes inside what would be a vector block are rejected with
        # the same error as before (validation runs before decoding).
        good = "\u65e5".encode() * 8
        for bad in (b"\xe6\x97", b"\xff", b"\xed\xa0\x80", b"\xc0\xaf"):
            raw = good[:pos] + bad + good[pos:]
            doc = b'["' + raw + b'"]'
            with pytest.raises(rjson.JSONDecodeError, match="UTF-8"):
                rjson.loads(doc)

    def test_random_ucs2_strings(self):
        import random

        rng = random.Random(8)
        pool = self.ONE + ["\u3042", "\u4e00", "\u9fff", "\u00ff", "\u0100"]
        for _ in range(3000):
            s = "".join(rng.choice(pool) for _ in range(rng.randrange(0, 60)))
            _check_loads(s)


def _check_floats(texts):
    """loads of each number text (in an array, and alone) equals float(text), bit for bit."""
    import math

    doc = "[" + ",".join(texts) + "]"
    got = rjson.loads(doc)
    for t, v in zip(texts, got):
        want = float(t)
        assert v == want and math.copysign(1, v) == math.copysign(1, want), t
        assert type(v) is float, t
    # Alone: the number is at the end of the input, too close for the fast
    # path's lookahead, so this also exercises the general parser.
    for t in texts:
        assert rjson.loads(t) == float(t), t


class TestFloatFastPath:
    """The number fast path reads up to 19 fraction digits (full-precision
    doubles such as 0.8601898621952831 have 16-19), with at most 19
    significant digits; results must be correctly rounded, like float()."""

    def test_repr_of_random_doubles(self):
        import random

        rng = random.Random(9)
        texts = []
        for _ in range(20000):
            x = rng.gauss(0, 1) * 10 ** rng.randrange(-8, 9)
            texts.append(repr(x))
            texts.append(repr(rng.random()))
        _check_floats(texts)

    @pytest.mark.parametrize("n", range(1, 25))
    def test_fraction_lengths(self, n):
        import random

        rng = random.Random(n)
        texts = []
        for _ in range(200):
            frac = "".join(rng.choice("0123456789") for _ in range(n))
            for intpart in ("0", "1", "9", "12", "999", "1234567", "123456789012345"):
                texts.append(f"{intpart}.{frac}")
                texts.append(f"-{intpart}.{frac}")
        _check_floats(texts)

    def test_significant_digit_boundaries(self):
        texts = [
            "0." + "9" * 19,           # 19 significant digits, int part 0
            "0." + "9" * 20,           # 20: general path
            "0.0" + "9" * 19,          # leading zero + 19 digits (20 fraction digits)
            "1." + "9" * 18,           # 19 significant digits
            "1." + "9" * 19,           # 20: general path
            "12." + "3" * 17, "12." + "3" * 18,
            "0.0000000000000000001",   # 19 fraction digits, mantissa 1
            "0.00000000000000000001",  # 20 fraction digits
            "9007199254740993.0", "0.9007199254740993", "0.9007199254740992",
            "0.30000000000000004", "0.1000000000000000055511151231257827",
            "2.2250738585072014e-308", "4.9406564584124654e-324",
            "1.7976931348623157e308", "0.0", "-0.0", "0.00000", "-0.0000000000000000000",
        ]
        _check_floats(texts)

    @pytest.mark.parametrize("n", [15, 16, 17, 18, 19, 20])
    def test_long_fraction_with_exponent(self, n):
        import random

        rng = random.Random(100 + n)
        texts = []
        for _ in range(300):
            frac = "".join(rng.choice("0123456789") for _ in range(n))
            e = rng.randrange(-30, 30)
            for s in (f"0.{frac}e{e}", f"3.{frac}E+{abs(e)}", f"-7.{frac}e-{abs(e)}"):
                texts.append(s)
        _check_floats(texts)

    def test_halfway_cases(self):
        # Decimal strings exactly halfway between two doubles, and one ulp
        # either side: rounding must be ties-to-even, as float() does.
        from decimal import Decimal
        import random

        rng = random.Random(4)
        texts = []
        for _ in range(3000):
            x = rng.random()
            nxt = float.fromhex(x.hex())  # x itself
            up = __import__("math").nextafter(x, 2.0)
            mid = (Decimal(nxt) + Decimal(up)) / 2
            for d in (mid, mid + Decimal("1e-25"), mid - Decimal("1e-25")):
                t = f"{d:.19f}"  # 19 fraction digits: fast path range
                texts.append(t)
        _check_floats(texts)


class TestRoundTrip:
    """Test round-trip consistency (dumps -> loads == original)."""

    def test_roundtrip_simple_dict(self):
        original = {"name": "test", "value": 42, "active": True}
        assert rjson.loads(rjson.dumps_str(original)) == original

    def test_roundtrip_complex_nested(self):
        original = {
            "data": [
                {"id": 1, "values": [1, 2, 3]},
                {"id": 2, "values": [4, 5, 6]},
            ],
            "metadata": {"count": 2, "timestamp": None},
        }
        assert rjson.loads(rjson.dumps_str(original)) == original

    def test_roundtrip_all_types(self):
        original = {
            "null": None,
            "bool_true": True,
            "bool_false": False,
            "int": 42,
            "float": 3.14,
            "string": "hello",
            "list": [1, 2, 3],
            "dict": {"nested": "value"},
        }
        assert rjson.loads(rjson.dumps_str(original)) == original


class TestPerformance:
    """Basic performance sanity checks."""

    def test_large_list(self):
        # Test with reasonably large list
        data = list(range(10000))
        serialized = rjson.dumps_str(data)
        assert len(rjson.loads(serialized)) == 10000

    def test_large_dict(self):
        # Test with reasonably large dict
        data = {f"key_{i}": i for i in range(1000)}
        serialized = rjson.dumps_str(data)
        assert len(rjson.loads(serialized)) == 1000

    def test_deeply_nested_list(self):
        # Create deeply nested list
        data = []
        current = data
        for _ in range(100):
            new_list = []
            current.append(new_list)
            current = new_list

        serialized = rjson.dumps_str(data)
        result = rjson.loads(serialized)
        assert isinstance(result, list)


class TestCompatibility:
    """Test compatibility with standard library json."""

    def test_output_matches_json_primitives(self):
        import json

        for value in [None, True, False, 0, 42, -10, 3.14, "hello"]:
            assert rjson.dumps_str(value) == json.dumps(value, separators=(",", ":"))

    def test_output_matches_json_collections(self):
        import json

        data = [1, 2, 3]
        assert rjson.dumps_str(data) == json.dumps(data, separators=(",", ":"))

        data = {"a": 1, "b": 2}
        rjson_result = rjson.dumps_str(data)
        json_result = json.dumps(data, separators=(",", ":"), sort_keys=True)
        # Note: dict order may differ, so we parse and compare
        assert rjson.loads(rjson_result) == json.loads(json_result)


class TestLoadsParser:
    """Regression tests for the hand-written loads parser."""

    def test_trailing_content_rejected(self):
        with pytest.raises(json.JSONDecodeError, match="unexpected content after document"):
            rjson.loads("1 2")
        with pytest.raises(ValueError):
            rjson.loads("[1] x")
        assert rjson.loads(" \n[1]\r\n\t") == [1]

    def test_raises_json_decode_error(self):
        import json
        with pytest.raises(json.JSONDecodeError):
            rjson.loads("[1,")

    def test_negative_zero(self):
        assert rjson.loads("-0") == 0 and isinstance(rjson.loads("-0"), int)
        assert math.copysign(1.0, rjson.loads("-0.0")) == -1.0

    def test_float_correct_rounding(self):
        import json
        for s in ["43.474709000000125", "0.000000000000000000000000000001", "2.2250738585072014e-308",
                  "5e-324", "1.7976931348623157e308", "9007199254740993", "9007199254740993.0",
                  "0.1", "123456789012345678901234567890.5", "1e-400"]:
            assert rjson.loads(s) == json.loads(s), s

    def test_number_digit_counts(self):
        # The one-pass fast path reads up to 15 integer and 15 fraction
        # digits as 8-byte words, needs <= 19 digits in total, and is only
        # used with >= 40 bytes of input left; every other shape goes through
        # the general parser. Check all digit-count combinations both ways.
        import json
        import random
        rnd = random.Random(7)
        pad = " " * 45
        for n1 in range(1, 22):
            for n2 in range(0, 22):
                for _ in range(3):
                    ip = str(rnd.randint(1, 9)) + "".join(rnd.choice("0123456789") for _ in range(n1 - 1))
                    fp = "".join(rnd.choice("0123456789") for _ in range(n2))
                    for s in (ip, "-" + ip, ip + "." + fp, "-" + ip + "." + fp, "0." + fp, "-0." + fp):
                        if s.endswith("."):
                            continue
                        exp = json.loads(s)
                        for doc in (s, "[" + s + "]", "[" + s + pad + "]", "[" + s + "," + s + pad + "]"):
                            got = rjson.loads(doc)
                            got = got if not isinstance(got, list) else got[0]
                            assert type(got) is type(exp) and got == exp, doc
        for bad in ("01", "-01", "00.5", "1.", "-", "-.5", "1.e5", "1e", "1e+", "01.5"):
            for doc in ("[" + bad + pad + "]", "[" + bad + "]"):
                with pytest.raises(ValueError):
                    rjson.loads(doc)
        for mant in ("1", "0", "-0", "12", "1.5", "-1.25", "123456789012345.6", "9007199254740993", "4.9406564584124654"):
            for exp in ("e0", "E5", "e+5", "e-5", "e22", "e-22", "e23", "e308", "e-324", "e-400", "e0400", "e1234", "e-9999"):
                s = mant + exp
                exp_val = json.loads(s)
                for doc in ("[" + s + pad + "]", "[" + s + "]"):
                    if exp_val in (float("inf"), float("-inf")):
                        with pytest.raises(ValueError):
                            rjson.loads(doc)
                    else:
                        got = rjson.loads(doc)[0]
                        assert got == exp_val and repr(got) == repr(exp_val), doc
        for bad in ("1e", "1e+", "1e-", "1.5E", "1ee5", "1e5.5", "1e+-5"):
            with pytest.raises(ValueError):
                rjson.loads("[" + bad + pad + "]")
        assert rjson.loads("[1.5e3" + pad + "]") == [1500.0]
        assert rjson.loads("[123456789012345.25" + pad + "]") == [123456789012345.25]

    def test_float_overflow_rejected(self):
        with pytest.raises(ValueError):
            rjson.loads("1e400")

    def test_int_boundaries(self):
        for v in [2**63 - 1, -2**63, 2**63, 2**64 - 1, 2**64, -2**63 - 1, 10**19, 10**20, -10**40]:
            assert rjson.loads(str(v)) == v and isinstance(rjson.loads(str(v)), int)

    def test_bytes_bytearray_memoryview(self):
        doc = '{"a": ["é", 1, 2.5, null]}'
        expected = {"a": ["é", 1, 2.5, None]}
        for inp in (doc.encode(), bytearray(doc.encode()), memoryview(doc.encode())):
            assert rjson.loads(inp) == expected

    def test_memoryview_slice_does_not_read_past_end(self):
        # The parser relies on a NUL byte after the input; sliced memoryviews
        # are followed by arbitrary bytes, so they must be copied first.
        assert rjson.loads(memoryview(b"[1]2")[:3]) == [1]
        assert rjson.loads(memoryview(b"1234")[:2]) == 12
        assert rjson.loads(memoryview(b"1.5e3")[:3]) == 1.5
        assert rjson.loads(memoryview(b'"ab"x')[:4]) == "ab"
        assert rjson.loads(memoryview(b"truex")[:4]) is True
        assert rjson.loads(memoryview(b" [ ] ")[1:4]) == []
        for bad in (b'"ab"', b"[1,2]", b"nulx", b"1.5"):
            with pytest.raises(ValueError):
                rjson.loads(memoryview(bad)[: len(bad) - 1])
        with pytest.raises(ValueError, match="empty"):
            rjson.loads(memoryview(b"1")[:0])
        with pytest.raises(ValueError, match="empty"):
            rjson.loads(bytearray())
        big = b"[" + b"1.25," * 100 + b"2]"
        assert rjson.loads(memoryview(big + b"999")[: len(big)]) == [1.25] * 100 + [2]

    def test_invalid_utf8_rejected(self):
        for bad in (b'"\xff"', b'"\xed\xa0\x80"', b'"\xc3"'):
            with pytest.raises(ValueError):
                rjson.loads(bad)
        with pytest.raises(ValueError):
            rjson.loads('"\ud800"')

    def test_unicode_escapes(self):
        assert rjson.loads('"\\ud83d\\ude00 \\u00e9\\u4e2d\\/\\b\\f\\n\\r\\t"') == "😀 é中/\b\f\n\r\t"
        for bad in ('"\\ud800"', '"\\udc00"', '"\\ud800\\u0041"', '"\\x"', '"\\u12"'):
            with pytest.raises(ValueError):
                rjson.loads(bad)

    def test_escapes_at_every_block_offset(self):
        # The escape kernel works on 32-byte blocks with escapes straddling
        # block ends; the last 64 bytes of the input go through a scalar tail.
        import json
        escs = ['\\n', '\\"', '\\\\', '\\/', '\\u00e9', '\\u4e2d', '\\ud83d\\ude00', '\\u0041']
        for esc in escs:
            for pre in range(0, 70):
                for trail in (0, 3, 40, 100):
                    doc = '["' + "a" * pre + esc + "b" * 5 + esc + '"' + " " * trail + "]"
                    assert rjson.loads(doc) == json.loads(doc), doc
                    assert rjson.loads(doc.encode()) == json.loads(doc), doc
        long = "x\\n" * 200 + "é\\t" * 50 + "\\u20ac" * 30
        assert rjson.loads('"' + long + '"') == json.loads('"' + long + '"')

    def test_escape_errors_at_every_block_offset(self):
        import json
        # (bad sequence, offset of the reported position within it)
        cases = [('\\x', 0), ('\\ud800', 0), ('\\udc00', 0), ('\\ud800\\u0041', 6), ('\\u12G4', 4),
                 ('\x01', 0), ('\\uD83D\\uDBFF', 6)]
        for bad, off in cases:
            for pre in range(0, 70, 3):
                doc = '["' + "a\\n" * (pre // 3) + "a" * (pre % 3) + bad + "tail" * 20 + '"]'
                with pytest.raises(json.JSONDecodeError) as e:
                    rjson.loads(doc)
                # The error points at the offending escape / character.
                assert e.value.pos == doc.index(bad) + off, (doc, e.value.pos)
        with pytest.raises(ValueError, match="end of data"):
            rjson.loads('"' + "a\\n" * 40)
        with pytest.raises(ValueError):
            rjson.loads(b'"' + b"a\\n" * 40 + b"\xff" + b"b" * 80 + b'"')

    def test_control_characters_rejected(self):
        with pytest.raises(ValueError):
            rjson.loads('"a\tb"')

    def test_non_ascii_kinds(self):
        for s in ["héllo", "ÿ" * 20, "Ā中" * 10, "日本語テキスト" * 5, "😀" * 3 + "a" * 17, "x" * 100 + "é"]:
            assert rjson.loads('"' + s + '"') == s
            assert rjson.loads(('"' + s + '"').encode()) == s

    def test_lists_are_normal_lists(self):
        # Lists get a PyMem_Malloc'd item array attached to an empty list;
        # they must behave (grow, shrink, free) like any other list.
        import gc
        import json
        import sys
        for n in (0, 1, 2, 7, 100, 5000):
            text = json.dumps(list(range(n)))
            a = rjson.loads(text)
            assert a == list(range(n))
            assert sys.getsizeof(a) <= sys.getsizeof(json.loads(text))
            a.append("x")
            a.extend(range(50))
            a.insert(0, None)
            del a[1:3]
            a.sort(key=str)
            a.clear()
            a += [1, 2]
            assert a == [1, 2]
        nested = rjson.loads("[[1, [2, []]], [], {\"a\": [3]}]")
        nested[0][1].append(nested)
        del nested
        gc.collect()

    def test_duplicate_keys_last_wins(self):
        assert rjson.loads('{"a": 1, "b": 2, "a": 3}') == {"a": 3, "b": 2}

    def test_dict_memory_matches_json(self):
        # Small dicts must keep the compact str-only key table; on 3.13
        # (_PyDict_FromItems) presized large dicts keep it too.
        import json
        import sys
        sizes = (0, 1, 5, 6, 8) if sys.version_info[:2] != (3, 13) else (0, 1, 5, 6, 8, 9, 12, 100, 1000)
        for n in sizes:
            t = json.dumps({"k%d" % i: i for i in range(n)})
            assert sys.getsizeof(rjson.loads(t)) <= sys.getsizeof(json.loads(t)), n
        t = '{"a": 1, "b": {"c": [1]}, "a": 3, "b": 4}'
        assert list(rjson.loads(t).items()) == [("a", 3), ("b", 4)]

    def test_key_cache_lengths_and_collisions(self):
        # The key cache hashes only the first/last 8 bytes and the length, and
        # compares 16 bytes at a time; keys that differ only in the middle, and
        # keys near the end of the input (zero-padded copy), must stay distinct.
        import json
        keys = []
        for n in range(0, 70):
            keys.append("k" * n)
            if n >= 17:
                mid = n // 2
                keys.append("k" * mid + "X" + "k" * (n - mid - 1))
                keys.append("k" * mid + "é" + "k" * (n - mid - 2))
        doc = {k: i for i, k in enumerate(keys)}
        text = json.dumps(doc, ensure_ascii=False)
        for _ in range(3):
            assert rjson.loads(text) == doc
            for k, i in doc.items():
                small = json.dumps({k: i}, ensure_ascii=False)
                assert rjson.loads(small) == {k: i}
                assert rjson.loads(small.encode()) == {k: i}

    def test_long_and_escaped_keys(self):
        k = "k" * 100
        assert rjson.loads('{"%s": 1, "a\\nb": 2}' % k) == {k: 1, "a\nb": 2}

    def test_nesting_limit(self):
        assert rjson.loads("[" * 1000 + "]" * 1000) is not None
        with pytest.raises(ValueError, match="depth"):
            rjson.loads("[" * 1100 + "]" * 1100)

    def test_gc_state_preserved(self):
        import gc
        assert gc.isenabled()
        rjson.loads("[[1], {}]")
        assert gc.isenabled()
        gc.disable()
        try:
            rjson.loads("[[1], {}]")
            assert not gc.isenabled()
        finally:
            gc.enable()

    def test_rejects_non_json_literals(self):
        for bad in ("NaN", "Infinity", "-Infinity", "01", "1.", ".5", "[1,]", '{"a":1,}', "", "  ", "﻿[1]"):
            with pytest.raises(ValueError):
                rjson.loads(bad)

    def test_unsupported_input_type(self):
        with pytest.raises(TypeError):
            rjson.loads(123)


def _spread(doc: bytes, filler: bytes = b" ") -> bytes:
    """doc interleaved with filler, so that ``_spread(doc)[::2] == doc``."""
    return b"".join(bytes([c]) + filler for c in doc)


class TestMemoryviewLayouts:
    """Regression: non-contiguous memoryviews raised BufferError (the view
    was requested as C-contiguous). Every layout must now parse exactly like
    ``rjson.loads(mv.tobytes())``."""

    DOC = '{"a": [1, 2.5, "héllo \U0001F600", null, true], "b": {"c": -3}}'.encode()

    def check(self, mv):
        expected = rjson.loads(mv.tobytes())
        assert rjson.loads(mv) == expected
        return expected

    def test_strided(self):
        mv = memoryview(_spread(self.DOC))[::2]
        assert not mv.c_contiguous
        assert self.check(mv) == rjson.loads(self.DOC)

    def test_strided_from_bytearray_and_offset(self):
        buf = bytearray(b"xx" + _spread(b"[1,2,3]"))
        mv = memoryview(buf)[2::2]
        assert self.check(mv) == [1, 2, 3]

    def test_negative_stride(self):
        mv = memoryview(self.DOC[::-1])[::-1]
        assert not mv.c_contiguous
        assert self.check(mv) == rjson.loads(self.DOC)

    def test_strided_multibyte_items(self):
        # itemsize 2: the view's bytes are gathered item by item.
        doc = b"[10, 20, 30] "  # odd length -> pad to whole items
        doc += b" " * (len(doc) % 2)
        raw = b"".join(doc[i : i + 2] + b"##" for i in range(0, len(doc), 2))
        mv = memoryview(raw).cast("H")[::2]
        assert mv.tobytes() == doc
        assert self.check(mv) == [10, 20, 30]

    def test_multidimensional_c_contiguous(self):
        doc = b"[1, 2]  "
        mv = memoryview(doc).cast("B", shape=[2, 4])
        assert self.check(mv) == [1, 2]

    def test_strided_single_byte_and_empty(self):
        assert rjson.loads(memoryview(b"7x")[::2]) == 7
        with pytest.raises(json.JSONDecodeError, match="empty"):
            rjson.loads(memoryview(b"")[::2])
        with pytest.raises(json.JSONDecodeError, match="empty"):
            rjson.loads(memoryview(b"abc")[3::2])

    def test_strided_invalid_json_matches_tobytes(self):
        # memoryview(b"[1,2]")[::2] is b"[,]": the same error either way.
        mv = memoryview(b"[1,2]")[::2]
        with pytest.raises(json.JSONDecodeError) as a:
            rjson.loads(mv)
        with pytest.raises(json.JSONDecodeError) as b:
            rjson.loads(mv.tobytes())
        assert (a.value.msg, a.value.pos) == (b.value.msg, b.value.pos)

    def test_strided_invalid_utf8(self):
        with pytest.raises(json.JSONDecodeError):
            rjson.loads(memoryview(_spread(b'"\xff"'))[::2])

    def test_released_memoryview_raises_valueerror(self):
        mv = memoryview(b"[1]")
        mv.release()
        with pytest.raises(ValueError):
            rjson.loads(mv)

    def test_strided_does_not_leak_or_pin_the_buffer(self):
        # The exported buffer must be released: resizing a bytearray fails
        # with BufferError while a view on it is still held.
        buf = bytearray(_spread(b"[1]"))
        for _ in range(100):
            assert rjson.loads(memoryview(buf)[::2]) == [1]
        buf.extend(b"  ")  # would raise BufferError if an export leaked


class TestMemoryviewInPlace:
    """Large C-contiguous memoryviews that end where their bytes/bytearray
    ends are parsed in place (no copy); the parser relies on the NUL byte
    after the data, so every other view must still be copied."""

    @staticmethod
    def big_doc(n=3000):
        return json.dumps({"items": [{"id": i, "name": f"n{i}é"} for i in range(n)]}).encode()

    @pytest.mark.parametrize("pad", [0, 1, 4095, 4096, 4097, 100000])
    def test_whole_bytes_and_bytearray(self, pad):
        doc = b"[" + b" " * pad + b"1]"
        for base in (doc, bytearray(doc)):
            assert rjson.loads(memoryview(base)) == [1]
        big = self.big_doc()
        assert rjson.loads(memoryview(big)) == json.loads(big)
        assert rjson.loads(memoryview(bytearray(big))) == json.loads(big)

    @pytest.mark.parametrize("size", [4095, 4096, 5000, 70000])
    def test_slice_followed_by_digits_is_not_overread(self, size):
        # The view ends before the object does; the bytes after it ("999")
        # must not be parsed as part of the number or the document.
        body = b"[1" + b" " * (size - 3) + b"]"
        for base in (body + b"999", bytearray(body + b"999")):
            mv = memoryview(base)[: len(body)]
            assert rjson.loads(mv) == [1]
        num = b" " * size + b"12"
        for base in (num + b"34", bytearray(num + b"34")):
            assert rjson.loads(memoryview(base)[: len(num)]) == 12

    def test_suffix_view_ends_at_object_end(self):
        big = self.big_doc()
        base = b"garbage" + big
        assert rjson.loads(memoryview(base)[7:]) == json.loads(big)
        ba = bytearray(base)
        assert rjson.loads(memoryview(ba)[7:]) == json.loads(big)

    def test_export_released_bytearray_resizable_after(self):
        big = bytearray(self.big_doc())
        mv = memoryview(big)
        assert rjson.loads(mv) == json.loads(bytes(big))
        mv.release()
        big.extend(b" ")  # BufferError if loads leaked its export
        bad = bytearray(b"[" + b"1," * 5000 + b"]")
        mv = memoryview(bad)
        with pytest.raises(rjson.JSONDecodeError):
            rjson.loads(mv)
        mv.release()
        bad.extend(b" ")

    def test_errors_match_bytes_input(self):
        doc = b'{"a": [' + b"1, " * 3000 + b'2,]}'
        with pytest.raises(rjson.JSONDecodeError) as want:
            rjson.loads(doc)
        for base in (doc, bytearray(doc)):
            with pytest.raises(rjson.JSONDecodeError) as got:
                rjson.loads(memoryview(base))
            assert (got.value.msg, got.value.pos, got.value.lineno, got.value.colno) == (
                want.value.msg, want.value.pos, want.value.lineno, want.value.colno)

    def test_other_exporters_and_subclasses(self):
        import array

        big = self.big_doc()

        class B(bytes):
            pass

        assert rjson.loads(memoryview(B(big))) == json.loads(big)
        arr = array.array("B", big)
        assert rjson.loads(memoryview(arr)) == json.loads(big)
        # Invalid UTF-8 near the end, in place.
        with pytest.raises(rjson.JSONDecodeError):
            rjson.loads(memoryview(big[:-3] + b'"\xff"]}'))


class TestInputTypeErrors:
    @pytest.mark.parametrize("bad", [None, 1, 1.5, ["[]"], {"a": 1}, object()])
    def test_rejected_with_type_name(self, bad):
        with pytest.raises(TypeError) as ei:
            rjson.loads(bad)
        msg = str(ei.value)
        assert type(bad).__name__ in msg
        assert "str, bytes, bytearray or memoryview" in msg
        assert "rjson.rjson" not in msg

    def test_other_buffer_objects_are_rejected(self):
        # Only memoryview is accepted among generic buffers (wrap others in one).
        import array

        a = array.array("B", b"[1]")
        with pytest.raises(TypeError, match="array.array"):
            rjson.loads(a)
        assert rjson.loads(memoryview(a)) == [1]


REPO_ROOT = __import__("pathlib").Path(__file__).resolve().parent.parent
PUBLIC_NAMES = {
    "JSONDecodeError",
    "JSONEncodeError",
    "__version__",
    "dumps",
    "dumps_bytes",
    "dumps_str",
    "loads",
}


class TestModuleSurface:
    """Drop-in names shared with json/orjson, version, and the stub."""

    def test_json_decode_error_is_the_stdlib_class(self):
        assert rjson.JSONDecodeError is json.JSONDecodeError
        with pytest.raises(rjson.JSONDecodeError):
            rjson.loads("[")

    def test_json_encode_error_is_exported(self):
        assert issubclass(rjson.JSONEncodeError, TypeError)
        assert issubclass(rjson.JSONEncodeError, ValueError)
        with pytest.raises(rjson.JSONEncodeError):
            rjson.dumps(object())

    def test_version_matches_pyproject(self):
        import re

        text = (REPO_ROOT / "pyproject.toml").read_text()
        m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
        assert m is not None
        assert rjson.__version__ == m.group(1)
        assert isinstance(rjson.__version__, str)

    def test_version_matches_installed_metadata(self):
        from importlib import metadata

        try:
            installed = metadata.version("pyrjson")
        except metadata.PackageNotFoundError:
            pytest.skip("pyrjson distribution metadata not installed")
        assert rjson.__version__ == installed

    def test_all_and_star_import(self):
        assert set(rjson.__all__) == PUBLIC_NAMES
        assert all(hasattr(rjson, n) for n in rjson.__all__)
        ns = {}
        exec("from rjson import *", ns)
        assert PUBLIC_NAMES <= set(ns)
        assert ns["JSONDecodeError"] is json.JSONDecodeError

    @pytest.mark.parametrize("name", ["loads", "dumps", "dumps_str", "dumps_bytes"])
    def test_functions_report_public_module(self, name):
        f = getattr(rjson, name)
        assert f.__module__ == "rjson"
        with pytest.raises(TypeError) as ei:
            f("1", extra=1)
        msg = str(ei.value)
        assert "rjson.rjson" not in msg
        assert msg.startswith("rjson.")

    @pytest.mark.parametrize("name", ["loads", "dumps", "dumps_str", "dumps_bytes"])
    def test_functions_pickle_by_reference(self, name):
        import pickle

        f = getattr(rjson, name)
        assert pickle.loads(pickle.dumps(f)) is f

    def test_error_messages_do_not_expose_internal_module(self):
        for call in (lambda: rjson.loads(None), lambda: rjson.dumps(object())):
            with pytest.raises(TypeError) as ei:
                call()
            assert "rjson.rjson" not in str(ei.value)
        assert "rjson.rjson" not in repr(rjson.JSONEncodeError)

    def test_stub_matches_runtime(self):
        import ast

        stub = REPO_ROOT / "rjson.pyi"
        tree = ast.parse(stub.read_text())
        defined = set()
        stub_all = None
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.ClassDef)):
                defined.add(node.name)
            elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
                defined.add(node.target.id)
            elif isinstance(node, ast.Assign) and node.targets[0].id == "__all__":
                stub_all = ast.literal_eval(node.value)
            elif isinstance(node, ast.ImportFrom):
                defined.update(a.asname or a.name for a in node.names)
        assert stub_all is not None and set(stub_all) == PUBLIC_NAMES
        assert PUBLIC_NAMES <= defined

    def test_stub_and_marker_installed_with_package(self):
        import pathlib

        pkg = pathlib.Path(rjson.__file__)
        if pkg.name != "__init__.py":
            pytest.skip("rjson not installed as a package")
        assert (pkg.parent / "__init__.pyi").is_file()
        assert (pkg.parent / "py.typed").is_file()


# Trailing commas are reported at the comma, like orjson and json on
# CPython >= 3.13 (json < 3.13 has no trailing-comma error and reports
# "Expecting value" at the bracket instead). rjson used to report the bracket.
TRAILING_COMMA_DOCS = [
    "[1,]",
    '{"a":1,}',
    "[ 1 , ]",
    '{"a":1 ,  }',
    "[1,\n]",
    "[1,\n\n  ]",
    '{"a":\n1,\n}',
    '["é",]',
    '{"é":"😀" , }',
    "[[1,],2]",
    "\n\n [1,\n  2,]",
]

# Documents whose error position (pos/lineno/colno) matches json.loads
# exactly on every supported CPython version.
SAME_POSITION_AS_JSON = [
    "[1 2]",
    '{"a" 1}',
    '{"a":}',
    "[",
    "[1,",
    '{"a":1 "b":2}',
    "[1,,2]",
    "{,}",
    "]",
    "{1:2}",
    "[1]x",
    "[1] x",
    "{} {}",
    '{"a":1}}',
    "[true false]",
    '"a\tb"',
    '"éé\\x"',
]


def _json_error(doc):
    try:
        json.loads(doc)
    except json.JSONDecodeError as e:
        return e
    raise AssertionError(f"json accepted {doc!r}")


class TestDecodeErrorMessages:
    @pytest.mark.parametrize("doc", SAME_POSITION_AS_JSON, ids=repr)
    @pytest.mark.parametrize(
        "conv", [str, str.encode, lambda s: memoryview(s.encode())], ids=["str", "bytes", "memoryview"]
    )
    def test_position_matches_json(self, doc, conv):
        expected = _json_error(doc)
        with pytest.raises(json.JSONDecodeError) as ei:
            rjson.loads(conv(doc))
        e = ei.value
        assert (e.pos, e.lineno, e.colno) == (expected.pos, expected.lineno, expected.colno)
        assert e.doc == doc

    @pytest.mark.parametrize("doc", TRAILING_COMMA_DOCS, ids=repr)
    @pytest.mark.parametrize(
        "conv", [str, str.encode, lambda s: memoryview(s.encode())], ids=["str", "bytes", "memoryview"]
    )
    def test_trailing_comma_reported_at_the_comma(self, doc, conv):
        # Regression: the position pointed at the closing bracket.
        comma = re.search(r",\s*[\]}]", doc).start()  # char index of the offending comma
        at_comma = json.JSONDecodeError("x", doc, comma)
        with pytest.raises(json.JSONDecodeError) as ei:
            rjson.loads(conv(doc))
        e = ei.value
        assert e.msg == "trailing comma is not allowed"
        assert (e.pos, e.lineno, e.colno) == (comma, at_comma.lineno, at_comma.colno)
        if sys.version_info >= (3, 13):
            expected = _json_error(doc)
            assert "trailing comma" in expected.msg
            assert (e.pos, e.lineno, e.colno) == (expected.pos, expected.lineno, expected.colno)

    @pytest.mark.parametrize("doc,col", [("[1,]", 3), ('{"a":1,}', 7), ("[1, 2, 3,]", 9)])
    def test_trailing_comma_column(self, doc, col):
        with pytest.raises(json.JSONDecodeError) as ei:
            rjson.loads(doc)
        assert ei.value.colno == col
        assert ei.value.msg == "trailing comma is not allowed"

    @pytest.mark.parametrize("doc", ["[1,]", "{", "tru", "", '"\\x"', b'"\xff"', "[1] x", "1" * 5 + "."])
    def test_msg_is_the_bare_reason(self, doc):
        # Regression: .msg started with "JSON parsing error: " (json/orjson have no prefix).
        with pytest.raises(json.JSONDecodeError) as ei:
            rjson.loads(doc)
        e = ei.value
        assert not e.msg.startswith("JSON parsing error")
        assert e.msg and e.msg[0].islower()
        assert str(e) == f"{e.msg}: line {e.lineno} column {e.colno} (char {e.pos})"


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
