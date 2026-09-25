"""Tests for the integration examples in ``examples/``.

The examples are plain scripts (not an installed package), so they are loaded by file
path. The FastAPI tests skip when fastapi/httpx are not installed, so the core suite
still runs with only pytest.
"""

from __future__ import annotations

import dataclasses
import datetime as dt
import decimal
import enum
import importlib.util
import io
import json
import logging
import subprocess
import sys
import uuid
from pathlib import Path
from types import ModuleType

import pytest
import rjson

EXAMPLES = Path(__file__).resolve().parent.parent / "examples"


def load_example(name: str) -> ModuleType:
    """Import ``examples/<name>.py`` as module ``rjson_example_<name>``."""
    mod_name = f"rjson_example_{name}"
    if mod_name in sys.modules:
        return sys.modules[mod_name]
    spec = importlib.util.spec_from_file_location(mod_name, EXAMPLES / f"{name}.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[mod_name] = module  # dataclasses resolve their module through sys.modules
    spec.loader.exec_module(module)
    return module


def run_demo(name: str) -> str:
    """Run ``python examples/<name>.py`` and return its stdout."""
    proc = subprocess.run([sys.executable, str(EXAMPLES / f"{name}.py")],
                          capture_output=True, text=True, timeout=60, check=False)
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


# ---------------------------------------------------------------------------------------
# examples/codec.py
# ---------------------------------------------------------------------------------------

codec_mod = load_example("codec")


class Color(enum.Enum):
    RED = "red"
    GREEN = "green"


class Level(enum.IntEnum):
    LOW = 1


@dataclasses.dataclass
class Order:
    id: uuid.UUID
    total: decimal.Decimal
    color: Color
    placed: dt.datetime
    items: list[str] = dataclasses.field(default_factory=list)


def make_codec(**kwargs):
    codec = codec_mod.Codec(schema="orders", **kwargs)
    codec.register(Color, "Color")
    codec.register(Order, "Order")
    return codec


class TestCodec:
    def test_plain_data_uses_fast_path(self):
        codec = make_codec()
        payload = codec.encode({"a": [1, 2.5, None, True, "é"]})
        assert payload == (b'{"schema":"orders","version":1,"tagged":false,'
                           b'"data":{"a":[1,2.5,null,true,"\xc3\xa9"]}}')
        assert codec.decode(payload) == {"a": [1, 2.5, None, True, "é"]}

    @pytest.mark.parametrize("value", [
        dt.datetime(2024, 1, 2, 3, 4, 5, 678901),
        dt.datetime(2024, 1, 2, 3, 4, 5, tzinfo=dt.timezone(dt.timedelta(hours=-5))),
        dt.date(2024, 2, 29),
        dt.time(23, 59, 59, 1),
        dt.timedelta(days=-1, seconds=5, microseconds=7),
        uuid.UUID("12345678-1234-5678-1234-567812345678"),
        decimal.Decimal("0.1000000000000000000000000001"),
        decimal.Decimal("-Infinity"),
        {1, 2, 3},
        frozenset({"a"}),
        b"\x00\xffbinary",
        Color.GREEN,
        {1: "int key", (2, 3): "tuple key", None: "none key"},
        {"$rjson": "user data that looks like a tag", "x": dt.date(2020, 1, 1)},
    ], ids=repr)
    def test_extended_types_round_trip(self, value):
        codec = make_codec()
        payload = codec.encode({"v": value})
        assert b'"tagged":true' in payload
        assert codec.decode(payload) == {"v": value}

    def test_dataclass_round_trip_with_nested_types(self):
        codec = make_codec()
        order = Order(uuid.uuid4(), decimal.Decimal("9.99"), Color.RED,
                      dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc), ["x"])
        restored = codec.decode(codec.encode([order, order]))
        assert restored == [order, order]
        assert type(restored[0].total) is decimal.Decimal

    def test_tag_like_user_dict_on_fast_path_is_untouched(self):
        codec = make_codec()
        data = {"$rjson": "uuid", "v": "not-a-uuid"}
        assert codec.decode(codec.encode(data)) == data

    def test_json_native_normalizations(self):
        codec = make_codec()
        assert codec.decode(codec.encode({"t": (1, 2), "e": Level.LOW})) == {"t": [1, 2], "e": 1}

    def test_memoryview_and_bytearray_input(self):
        codec = make_codec()
        payload = codec.encode({"id": uuid.UUID(int=7)})
        assert codec.decode(memoryview(payload)) == codec.decode(bytearray(payload))

    @pytest.mark.parametrize("value", [
        object(), 1j, float("nan"), float("inf"), {"k": [float("-inf")]}, Level,
    ], ids=repr)
    def test_unsupported_values_raise_encode_error(self, value):
        with pytest.raises(codec_mod.EncodeError):
            make_codec().encode(value)

    def test_encode_error_is_both_type_and_value_error(self):
        for exc_type in (TypeError, ValueError):
            with pytest.raises(exc_type):
                make_codec().encode(object())

    def test_unregistered_enum_and_dataclass(self):
        codec = codec_mod.Codec()
        with pytest.raises(codec_mod.EncodeError, match="not registered"):
            codec.encode(Color.RED)
        with pytest.raises(codec_mod.EncodeError, match="not registered"):
            codec.encode(Order(uuid.UUID(int=0), decimal.Decimal(0), Color.RED,
                               dt.datetime(2024, 1, 1)))

    def test_decoder_rejects_unregistered_type_names(self):
        payload = make_codec().encode(Color.RED)
        with pytest.raises(codec_mod.DecodeError, match="unregistered"):
            codec_mod.Codec(schema="orders").decode(payload)

    def test_register_rejects_value_enums_and_other_types(self):
        codec = codec_mod.Codec()
        with pytest.raises(TypeError):
            codec.register(Level)
        with pytest.raises(TypeError):
            codec.register(dict)
        codec.register(Color, "C")
        with pytest.raises(ValueError, match="already registered"):
            codec.register(Order, "C")

    def test_circular_reference(self):
        data: list = []
        data.append(data)
        with pytest.raises(codec_mod.EncodeError):
            make_codec().encode(data)
        cyclic: dict = {"when": dt.date(2020, 1, 1)}
        cyclic["self"] = cyclic
        with pytest.raises(codec_mod.EncodeError, match="nesting"):
            make_codec().encode(cyclic)

    def test_lone_surrogate_is_an_encode_error(self):
        with pytest.raises(codec_mod.EncodeError):
            make_codec().encode({"s": "\ud800"})

    @pytest.mark.parametrize("payload", [
        b"",
        b"{",
        b'"\xff"',
        b"[1]",
        b'{"schema":"orders","version":1,"data":1}',
        b'{"schema":"orders","version":1,"tagged":"yes","data":1}',
        b'{"schema":"other","version":1,"tagged":false,"data":1}',
        b'{"schema":"orders","version":9,"tagged":false,"data":1}',
        b'{"schema":"orders","version":true,"tagged":false,"data":1}',
        b'{"schema":"orders","version":1,"tagged":true,"data":{"$rjson":"uuid","v":"zz"}}',
        b'{"schema":"orders","version":1,"tagged":true,"data":{"$rjson":"nope","v":1}}',
        b'{"schema":"orders","version":1,"tagged":true,"data":{"$rjson":"dict","v":[[{},1]]}}',
    ])
    def test_invalid_payloads_raise_decode_error(self, payload):
        with pytest.raises(codec_mod.DecodeError):
            make_codec().decode(payload)

    def test_decode_error_on_wrong_input_type(self):
        with pytest.raises(codec_mod.DecodeError):
            make_codec().decode(12)  # type: ignore[arg-type]
        with pytest.raises(codec_mod.DecodeError):
            make_codec().decode(memoryview(b'[1, 2]')[::2])  # not C-contiguous

    def test_extra_envelope_keys_are_accepted(self):
        payload = b'{"schema":"orders","version":1,"tagged":false,"data":1,"trace":"t"}'
        assert make_codec().decode(payload) == 1

    def test_version_migrations(self):
        v1 = codec_mod.Codec(schema="s", version=1).encode({"name": "a"})
        v3 = codec_mod.Codec(schema="s", version=3, migrations={
            1: lambda d: {**d, "v2": True},
            2: lambda d: {**d, "v3": True},
        })
        assert v3.decode(v1) == {"name": "a", "v2": True, "v3": True}
        missing = codec_mod.Codec(schema="s", version=2)
        with pytest.raises(codec_mod.DecodeError, match="no migration"):
            missing.decode(v1)

    def test_bare_mode_is_plain_json(self):
        codec = codec_mod.Codec(envelope=False, type_hooks=False)
        assert codec.encode({"a": 1}) == b'{"a":1}'
        assert codec.decode(b'{"a":1}') == {"a": 1}
        with pytest.raises(codec_mod.EncodeError):
            codec.encode({"a": dt.date(2020, 1, 1)})
        with pytest.raises(ValueError):
            codec_mod.Codec(envelope=False)  # hooks need the envelope

    def test_hooks_disabled_rejects_tagged_payloads(self):
        tagged = make_codec().encode(uuid.UUID(int=1))
        with pytest.raises(codec_mod.DecodeError, match="type_hooks=False"):
            codec_mod.Codec(schema="orders", type_hooks=False).decode(tagged)

    def test_kafka_callables(self):
        codec = make_codec()
        ser = codec_mod.KafkaSerializer(codec)
        de = codec_mod.KafkaDeserializer(codec)
        assert ser(None) is None and de(None) is None  # tombstones
        value = {"at": dt.datetime(2024, 1, 1), "n": 1}
        assert de(ser(value, object()), object()) == value

    def test_payload_is_readable_by_stdlib_json(self):
        doc = json.loads(make_codec().encode({"id": uuid.UUID(int=1)}))
        assert doc["data"] == {"id": {"$rjson": "uuid", "v": str(uuid.UUID(int=1))}}

    def test_demo_runs(self):
        out = run_demo("codec")
        assert "round-trip equal: True" in out


# ---------------------------------------------------------------------------------------
# examples/json_logging.py
# ---------------------------------------------------------------------------------------

logging_mod = load_example("json_logging")


@pytest.fixture
def json_logger():
    """A logger writing JSON lines into a StringIO; yields (logger, read_lines)."""
    stream = io.StringIO()
    handler = logging.StreamHandler(stream)
    handler.setFormatter(logging_mod.JSONFormatter(static_fields={"service": "svc"}))
    logger = logging.getLogger(f"test_examples.{uuid.uuid4().hex}")
    logger.propagate = False
    logger.setLevel(logging.DEBUG)
    logger.addHandler(handler)

    def lines():
        return [json.loads(line) for line in stream.getvalue().splitlines()]

    yield logger, lines
    logger.removeHandler(handler)


class Unreprable:
    def __repr__(self):
        raise RuntimeError("boom")

    __str__ = __repr__


class TestJSONFormatter:
    def test_core_fields(self, json_logger):
        logger, lines = json_logger
        logger.warning("hello %s", "wörld", extra={"user": "ada", "n": 3})
        (line,) = lines()
        assert list(line)[:4] == ["ts", "level", "logger", "message"]
        assert line["level"] == "WARNING"
        assert line["logger"] == logger.name
        assert line["message"] == "hello wörld"
        assert line["service"] == "svc"
        assert line["user"] == "ada" and line["n"] == 3
        assert dt.datetime.fromisoformat(line["ts"].replace("Z", "+00:00")).tzinfo is not None

    @pytest.mark.parametrize("created", [0.0, 1.9995, 1727250000.123456, 1727250000.999999,
                                         1727250001.0, -1.5])
    def test_timestamp_matches_datetime(self, created):
        ref = dt.datetime.fromtimestamp(created, dt.timezone.utc)
        expected = ref.isoformat(timespec="milliseconds").replace("+00:00", "Z")
        assert logging_mod._utc_iso(created) == expected
        assert logging_mod._utc_iso(created) == expected  # cached second

    def test_one_line_per_record(self, json_logger):
        logger, lines = json_logger
        logger.info("multi\nline\r\nmessage")
        assert lines()[0]["message"] == "multi\nline\r\nmessage"

    def test_exception_info(self, json_logger):
        logger, lines = json_logger
        try:
            raise KeyError("missing")
        except KeyError:
            logger.exception("failed")
        (line,) = lines()
        assert line["level"] == "ERROR"
        assert "KeyError: 'missing'" in line["exc_info"]
        assert line["exc_info"].startswith("Traceback")

    def test_stack_info(self, json_logger):
        logger, lines = json_logger
        logger.info("here", stack_info=True)
        assert "Stack (most recent call last)" in lines()[0]["stack_info"]

    def test_unserializable_extras_are_stringified(self, json_logger):
        logger, lines = json_logger
        cyclic: list = []
        cyclic.append(cyclic)
        logger.info("x", extra={
            "when": dt.datetime(2024, 1, 2, 3, 4, 5),
            "id": uuid.UUID(int=1),
            "amount": decimal.Decimal("1.50"),
            "color": Color.RED,
            "nan": float("nan"),
            "nested": {1: {2, 3}, "inf": [float("-inf")]},
            "big": 2**100,
            "obj": object(),
            "bad": Unreprable(),
            "cyclic": cyclic,
            "order": Order(uuid.UUID(int=2), decimal.Decimal("3"), Color.GREEN,
                           dt.datetime(2024, 1, 1)),
        })
        (line,) = lines()
        assert line["when"] == "2024-01-02T03:04:05"
        assert line["id"] == str(uuid.UUID(int=1))
        assert line["amount"] == "1.50"
        assert line["color"] == "red"
        assert line["nan"] == "nan"
        assert line["nested"] == {"1": [2, 3], "inf": ["-inf"]}
        assert line["big"] == 2**100
        assert line["obj"].startswith("<object object")
        assert line["bad"] == "<unprintable Unreprable>"
        assert "<max depth exceeded>" in json.dumps(line["cyclic"])
        assert line["order"]["color"] == "green"

    def test_colliding_extra_is_renamed(self, json_logger):
        logger, lines = json_logger
        logger.info("x", extra={"level": "custom", "ts": 1, "service": "other"})
        (line,) = lines()
        assert line["level"] == "INFO" and line["extra_level"] == "custom"
        assert line["extra_ts"] == 1
        assert line["service"] == "svc" and line["extra_service"] == "other"

    def test_bad_format_args_do_not_raise(self):
        # Formatted directly: pytest's own capture handler re-raises the TypeError.
        record = logging.LogRecord("n", logging.INFO, __file__, 1, "%d items", ("x",), None)
        line = json.loads(logging_mod.JSONFormatter().format(record))
        assert line["message"] == "'%d items' % ('x',) (message formatting failed)"

    def test_lone_surrogate_is_replaced(self, json_logger):
        logger, lines = json_logger
        logger.info("bad \ud800 text", extra={"k": "\udfff"})
        (line,) = lines()
        assert line["message"] == "bad \ufffd text" and line["k"] == "\ufffd"

    def test_output_is_valid_utf8_json(self):
        record = logging.LogRecord("n", logging.INFO, __file__, 1, "é \ud83d", (), None)
        text = logging_mod.JSONFormatter(include_location=True).format(record)
        assert rjson.loads(text.encode("utf-8"))["line"] == 1

    def test_setup_json_logging(self):
        stream = io.StringIO()
        root = logging.getLogger()
        old_level = root.level
        handler = logging_mod.setup_json_logging(logging.DEBUG, stream)
        try:
            logging.getLogger("test_examples.setup").debug("dbg")
        finally:
            root.removeHandler(handler)
            root.setLevel(old_level)
        assert json.loads(stream.getvalue())["message"] == "dbg"


class TestNDJSON:
    def test_round_trip_binary(self):
        buf = io.BytesIO()
        records = [{"i": i, "s": "a\nb\u2028c", "f": 0.1} for i in range(100)]
        assert logging_mod.write_ndjson(buf, records) == 100
        assert buf.getvalue().count(b"\n") == 100
        buf.seek(0)
        assert list(logging_mod.read_ndjson(buf)) == records

    def test_round_trip_text_file(self, tmp_path):
        path = tmp_path / "data.ndjson"
        with path.open("w", encoding="utf-8") as fp:
            assert logging_mod.write_ndjson(fp, [{"é": 1}, [2]]) == 2
        with path.open(encoding="utf-8") as fp:
            assert list(logging_mod.read_ndjson(fp)) == [{"é": 1}, [2]]

    def test_empty_input(self):
        assert logging_mod.write_ndjson(io.BytesIO(), []) == 0
        assert list(logging_mod.read_ndjson(io.BytesIO(b""))) == []

    def test_blank_lines_and_crlf(self):
        data = b'\n{"a":1}\r\n   \n\t\n[2]\n{"b":3}'  # no trailing newline on the last line
        assert list(logging_mod.read_ndjson(io.BytesIO(data))) == [{"a": 1}, [2], {"b": 3}]

    def test_bad_line_raises_with_line_number(self):
        data = io.BytesIO(b'{"a":1}\n\n{"a":\n{"a":3}\n')
        reader = logging_mod.read_ndjson(data)
        assert next(reader) == {"a": 1}
        with pytest.raises(logging_mod.NDJSONError) as info:
            next(reader)
        assert info.value.lineno == 3
        assert isinstance(info.value, ValueError)
        assert isinstance(info.value.__cause__, json.JSONDecodeError)

    def test_invalid_utf8_line_is_reported_not_fatal(self, caplog):
        data = io.BytesIO(b'{"a":1}\n"\xff"\n{"a":2}\n')
        with caplog.at_level(logging.WARNING):
            assert list(logging_mod.read_ndjson(data, on_error="skip")) == [{"a": 1}, {"a": 2}]
        assert "line 2" in caplog.text

    def test_skip_mode_logs_each_bad_line(self, caplog):
        data = io.BytesIO(b"x\n[1]\ny\n")
        with caplog.at_level(logging.WARNING):
            assert list(logging_mod.read_ndjson(data, on_error="skip")) == [[1]]
        assert "line 1" in caplog.text and "line 3" in caplog.text

    def test_invalid_on_error(self):
        with pytest.raises(ValueError):
            list(logging_mod.read_ndjson([], on_error="ignore"))  # type: ignore[arg-type]

    @pytest.mark.parametrize("bad", [object(), float("nan"), {1: 2}, "\ud800"], ids=repr)
    def test_unserializable_record(self, bad):
        buf = io.BytesIO()
        with pytest.raises(ValueError) as info:
            logging_mod.write_ndjson(buf, [{"ok": 1}, {"bad": bad}])
        assert isinstance(info.value, logging_mod.NDJSONError)
        assert info.value.lineno == 2
        assert buf.getvalue() == b'{"ok":1}\n'  # nothing of the bad record was written

    def test_lone_surrogate_in_text_mode_writes_nothing(self):
        buf = io.StringIO()
        with pytest.raises(UnicodeEncodeError):
            logging_mod.write_ndjson(buf, [{"ok": 1}, {"bad": "\ud800"}])
        assert buf.getvalue() == '{"ok":1}\n'

    def test_demo_runs(self):
        out = run_demo("json_logging")
        first = json.loads(out.splitlines()[0])
        assert first["message"] == "user ada logged in" and first["ratio"] == "nan"
        assert "raise mode: line 6" in out
