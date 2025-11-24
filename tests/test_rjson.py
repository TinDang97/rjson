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
        # Note: serde_json parses very large ints as floats (JSON spec limitation)
        very_large = 123456789012345678901234567890
        result = rjson.dumps(very_large)
        # Round-trip loses precision for numbers > f64 range
        loaded = rjson.loads(result)
        assert isinstance(loaded, float)  # Becomes float on loads
        assert loaded == pytest.approx(very_large, rel=1e-10)

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


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
