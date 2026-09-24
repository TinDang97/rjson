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
import math


class TestBasicTypes:
    """Test serialization and deserialization of basic Python types."""

    def test_none(self):
        assert rjson.dumps(None) == "null"
        assert rjson.loads("null") is None

    def test_bool_true(self):
        assert rjson.dumps(True) == "true"
        assert rjson.loads("true") is True

    def test_bool_false(self):
        assert rjson.dumps(False) == "false"
        assert rjson.loads("false") is False

    def test_integer_zero(self):
        assert rjson.dumps(0) == "0"
        assert rjson.loads("0") == 0

    def test_integer_positive(self):
        assert rjson.dumps(42) == "42"
        assert rjson.loads("42") == 42

    def test_integer_negative(self):
        assert rjson.dumps(-42) == "-42"
        assert rjson.loads("-42") == -42

    def test_integer_large(self):
        large_int = 9223372036854775807  # Max i64
        assert rjson.dumps(large_int) == str(large_int)
        assert rjson.loads(str(large_int)) == large_int

    def test_integer_very_large(self):
        # Python arbitrary precision int
        # Integers beyond 64 bits round-trip exactly (same as stdlib json)
        very_large = 123456789012345678901234567890
        result = rjson.dumps(very_large)
        loaded = rjson.loads(result)
        assert isinstance(loaded, int)
        assert loaded == very_large
        assert rjson.loads(str(-very_large)) == -very_large

    def test_float_zero(self):
        assert rjson.dumps(0.0) == "0.0"
        assert rjson.loads("0.0") == 0.0

    def test_float_positive(self):
        assert rjson.dumps(3.14) == "3.14"
        assert rjson.loads("3.14") == 3.14

    def test_float_negative(self):
        assert rjson.dumps(-3.14) == "-3.14"
        assert rjson.loads("-3.14") == -3.14

    def test_float_scientific(self):
        val = 1.23e-10
        serialized = rjson.dumps(val)
        assert rjson.loads(serialized) == pytest.approx(val)

    def test_string_empty(self):
        assert rjson.dumps("") == '""'
        assert rjson.loads('""') == ""

    def test_string_simple(self):
        assert rjson.dumps("hello") == '"hello"'
        assert rjson.loads('"hello"') == "hello"

    def test_string_with_spaces(self):
        assert rjson.dumps("hello world") == '"hello world"'
        assert rjson.loads('"hello world"') == "hello world"


class TestCollections:
    """Test serialization and deserialization of collections."""

    def test_list_empty(self):
        assert rjson.dumps([]) == "[]"
        assert rjson.loads("[]") == []

    def test_list_single(self):
        assert rjson.dumps([1]) == "[1]"
        assert rjson.loads("[1]") == [1]

    def test_list_multiple(self):
        assert rjson.dumps([1, 2, 3]) == "[1,2,3]"
        assert rjson.loads("[1,2,3]") == [1, 2, 3]

    def test_list_mixed_types(self):
        data = [1, "two", 3.0, None, True]
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_tuple_empty(self):
        # Tuples serialize as arrays
        assert rjson.dumps(()) == "[]"

    def test_tuple_single(self):
        assert rjson.dumps((1,)) == "[1]"

    def test_tuple_multiple(self):
        assert rjson.dumps((1, 2, 3)) == "[1,2,3]"

    def test_dict_empty(self):
        assert rjson.dumps({}) == "{}"
        assert rjson.loads("{}") == {}

    def test_dict_single(self):
        result = rjson.dumps({"a": 1})
        assert result == '{"a":1}'
        assert rjson.loads(result) == {"a": 1}

    def test_dict_multiple(self):
        data = {"a": 1, "b": 2, "c": 3}
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_dict_mixed_values(self):
        data = {"int": 1, "str": "hello", "float": 3.14, "none": None, "bool": True}
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data


class TestNestedStructures:
    """Test deeply nested data structures."""

    def test_nested_lists(self):
        data = [[1, 2], [3, 4], [5, 6]]
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_nested_dicts(self):
        data = {"outer": {"inner": {"deep": "value"}}}
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_mixed_nesting(self):
        data = {
            "users": [
                {"name": "Alice", "age": 30, "tags": ["python", "rust"]},
                {"name": "Bob", "age": 25, "tags": ["go", "javascript"]},
            ],
            "count": 2,
        }
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_deep_nesting(self):
        # Create deeply nested structure
        data = {"level": 0}
        current = data
        for i in range(1, 50):
            current["nested"] = {"level": i}
            current = current["nested"]

        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data


class TestUnicode:
    """Test Unicode and special character handling."""

    def test_unicode_simple(self):
        data = "hello 世界"
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_unicode_emoji(self):
        data = "Hello 👋 🌍"
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_unicode_various(self):
        data = {"русский": "текст", "中文": "文本", "العربية": "نص"}
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_escaped_characters(self):
        data = 'quote" backslash\\ newline\n tab\t'
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_control_characters(self):
        # Test various control characters
        data = "line1\nline2\rline3\tcolumn"
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data


class TestEdgeCases:
    """Test edge cases and boundary conditions."""

    def test_integer_cache_boundary_negative(self):
        # Test integer caching boundary at -256
        assert rjson.dumps(-256) == "-256"
        assert rjson.dumps(-257) == "-257"
        assert rjson.loads("-256") == -256
        assert rjson.loads("-257") == -257

    def test_integer_cache_boundary_positive(self):
        # Test integer caching boundary at 256
        assert rjson.dumps(256) == "256"
        assert rjson.dumps(257) == "257"
        assert rjson.loads("256") == 256
        assert rjson.loads("257") == 257

    def test_empty_string_key(self):
        data = {"": "empty key"}
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_string_with_quotes(self):
        data = 'He said "hello"'
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_list_of_empty_lists(self):
        data = [[], [], []]
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data

    def test_dict_of_empty_dicts(self):
        data = {"a": {}, "b": {}, "c": {}}
        serialized = rjson.dumps(data)
        assert rjson.loads(serialized) == data


class TestErrorHandling:
    """Test error handling for invalid inputs."""

    def test_dumps_nan_raises(self):
        with pytest.raises(ValueError, match="Cannot serialize non-finite float"):
            rjson.dumps(float("nan"))

    def test_dumps_infinity_raises(self):
        with pytest.raises(ValueError, match="Cannot serialize non-finite float"):
            rjson.dumps(float("inf"))

    def test_dumps_negative_infinity_raises(self):
        with pytest.raises(ValueError, match="Cannot serialize non-finite float"):
            rjson.dumps(float("-inf"))

    def test_dumps_unsupported_type_raises(self):
        class CustomClass:
            pass

        with pytest.raises(ValueError, match="Unsupported Python type"):
            rjson.dumps(CustomClass())

    def test_dumps_dict_non_string_key_raises(self):
        with pytest.raises(ValueError, match="keys must be strings"):
            rjson.dumps({1: "value"})

    def test_loads_invalid_json_raises(self):
        with pytest.raises(ValueError, match="JSON parsing error"):
            rjson.loads("{invalid json}")

    def test_loads_truncated_json_raises(self):
        with pytest.raises(ValueError, match="JSON parsing error"):
            rjson.loads('{"key": "incomplete')

    def test_loads_trailing_comma_raises(self):
        with pytest.raises(ValueError, match="JSON parsing error"):
            rjson.loads('[1, 2, 3,]')

    @pytest.mark.parametrize("doc", ["[1] x", "{} {}", "1 2", "null,"])
    def test_loads_trailing_characters_raises(self, doc):
        with pytest.raises(ValueError, match="JSON parsing error"):
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
        assert rjson.dumps(obj) == json.dumps(obj, ensure_ascii=False, separators=(",", ":"))
        with pytest.raises(UnicodeEncodeError):
            rjson.dumps_bytes(obj)


class TestStringLayout:
    """ASCII fast path must honour the running interpreter's str layout
    (the data offset changed in CPython 3.12)."""

    @pytest.mark.parametrize("n", [0, 1, 7, 8, 15, 16, 31, 32, 33, 100, 1000])
    def test_ascii_lengths(self, n):
        s = "".join(chr(97 + i % 26) for i in range(n))
        assert rjson.dumps(s) == '"' + s + '"'
        assert rjson.dumps({s: [s, s]}) == '{"%s":["%s","%s"]}' % (s, s, s)

    def test_non_ascii_kinds(self):
        for s in ["caf\u00e9", "\u65e5\u672c", "\U0001F600", "a\u00e9\u65e5\U0001F600"]:
            assert rjson.loads(rjson.dumps(s)) == s
            assert rjson.loads(rjson.dumps([s, {s: s}])) == [s, {s: s}]


class TestRoundTrip:
    """Test round-trip consistency (dumps -> loads == original)."""

    def test_roundtrip_simple_dict(self):
        original = {"name": "test", "value": 42, "active": True}
        assert rjson.loads(rjson.dumps(original)) == original

    def test_roundtrip_complex_nested(self):
        original = {
            "data": [
                {"id": 1, "values": [1, 2, 3]},
                {"id": 2, "values": [4, 5, 6]},
            ],
            "metadata": {"count": 2, "timestamp": None},
        }
        assert rjson.loads(rjson.dumps(original)) == original

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
        assert rjson.loads(rjson.dumps(original)) == original


class TestPerformance:
    """Basic performance sanity checks."""

    def test_large_list(self):
        # Test with reasonably large list
        data = list(range(10000))
        serialized = rjson.dumps(data)
        assert len(rjson.loads(serialized)) == 10000

    def test_large_dict(self):
        # Test with reasonably large dict
        data = {f"key_{i}": i for i in range(1000)}
        serialized = rjson.dumps(data)
        assert len(rjson.loads(serialized)) == 1000

    def test_deeply_nested_list(self):
        # Create deeply nested list
        data = []
        current = data
        for _ in range(100):
            new_list = []
            current.append(new_list)
            current = new_list

        serialized = rjson.dumps(data)
        result = rjson.loads(serialized)
        assert isinstance(result, list)


class TestCompatibility:
    """Test compatibility with standard library json."""

    def test_output_matches_json_primitives(self):
        import json

        for value in [None, True, False, 0, 42, -10, 3.14, "hello"]:
            assert rjson.dumps(value) == json.dumps(value, separators=(",", ":"))

    def test_output_matches_json_collections(self):
        import json

        data = [1, 2, 3]
        assert rjson.dumps(data) == json.dumps(data, separators=(",", ":"))

        data = {"a": 1, "b": 2}
        rjson_result = rjson.dumps(data)
        json_result = json.dumps(data, separators=(",", ":"), sort_keys=True)
        # Note: dict order may differ, so we parse and compare
        assert rjson.loads(rjson_result) == json.loads(json_result)


class TestLoadsParser:
    """Regression tests for the hand-written loads parser."""

    def test_trailing_content_rejected(self):
        with pytest.raises(ValueError, match="JSON parsing error"):
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

    def test_duplicate_keys_last_wins(self):
        assert rjson.loads('{"a": 1, "b": 2, "a": 3}') == {"a": 3, "b": 2}

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


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
