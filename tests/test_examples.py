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
import warnings
from collections.abc import Mapping
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
    proc = subprocess.run(
        [sys.executable, str(EXAMPLES / f"{name}.py")],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
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
        assert payload == (
            b'{"schema":"orders","version":1,"tagged":false,'
            b'"data":{"a":[1,2.5,null,true,"\xc3\xa9"]}}'
        )
        assert codec.decode(payload) == {"a": [1, 2.5, None, True, "é"]}

    @pytest.mark.parametrize(
        "value",
        [
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
        ],
        ids=repr,
    )
    def test_extended_types_round_trip(self, value):
        codec = make_codec()
        payload = codec.encode({"v": value})
        assert b'"tagged":true' in payload
        assert codec.decode(payload) == {"v": value}

    def test_dataclass_round_trip_with_nested_types(self):
        codec = make_codec()
        order = Order(
            uuid.uuid4(),
            decimal.Decimal("9.99"),
            Color.RED,
            dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc),
            ["x"],
        )
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

    @pytest.mark.parametrize(
        "value",
        [
            object(),
            1j,
            float("nan"),
            float("inf"),
            {"k": [float("-inf")]},
            Level,
        ],
        ids=repr,
    )
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
            codec.encode(
                Order(uuid.UUID(int=0), decimal.Decimal(0), Color.RED, dt.datetime(2024, 1, 1))
            )

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

    @pytest.mark.parametrize(
        "payload",
        [
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
        ],
    )
    def test_invalid_payloads_raise_decode_error(self, payload):
        with pytest.raises(codec_mod.DecodeError):
            make_codec().decode(payload)

    def test_decode_error_on_wrong_input_type(self):
        with pytest.raises(codec_mod.DecodeError):
            make_codec().decode(12)  # type: ignore[arg-type]
        with pytest.raises(codec_mod.DecodeError):
            make_codec().decode(memoryview(b"[1, 2]")[::2])  # strided view of b"[,2": invalid
        plain = codec_mod.Codec(envelope=False, type_hooks=False)
        assert plain.decode(memoryview(b"[ 1 ]")[::2]) == [1]  # strided views are accepted

    def test_extra_envelope_keys_are_accepted(self):
        payload = b'{"schema":"orders","version":1,"tagged":false,"data":1,"trace":"t"}'
        assert make_codec().decode(payload) == 1

    def test_version_migrations(self):
        v1 = codec_mod.Codec(schema="s", version=1).encode({"name": "a"})
        v3 = codec_mod.Codec(
            schema="s",
            version=3,
            migrations={
                1: lambda d: {**d, "v2": True},
                2: lambda d: {**d, "v3": True},
            },
        )
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


HAS_ZSTD = codec_mod._std_zstd is not None or codec_mod._zstandard is not None
needs_zstd = pytest.mark.skipif(not HAS_ZSTD, reason="needs Python 3.14+ or zstandard")


class TestCodecCompression:
    FEED = [{"id": i, "title": f"post {i}", "tags": ["news", "tech"]} for i in range(300)]

    @needs_zstd
    def test_large_payload_is_compressed_and_round_trips(self):
        codec = make_codec(compress="zstd")
        blob = codec.encode(self.FEED)
        assert blob[:4] == codec_mod.ZSTD_MAGIC
        assert len(blob) * 5 < len(make_codec().encode(self.FEED))
        assert codec.decode(blob) == self.FEED
        assert codec.decode(memoryview(blob)) == self.FEED
        assert codec.decode(bytearray(blob)) == self.FEED

    @needs_zstd
    def test_small_payload_stays_plain_json(self):
        codec = make_codec(compress="zstd")
        blob = codec.encode({"hits": 3})
        assert blob.startswith(b"{")
        assert codec.decode(blob) == {"hits": 3}

    @needs_zstd
    def test_tagged_payload_is_compressed_too(self):
        codec = make_codec(compress="zstd", compress_min_size=1)
        order = Order(
            uuid.UUID(int=7),
            decimal.Decimal("1.10"),
            Color.RED,
            dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc),
            ["a"],
        )
        blob = codec.encode(order)
        assert blob[:4] == codec_mod.ZSTD_MAGIC
        assert codec.decode(blob) == order

    @needs_zstd
    def test_mixed_rollout_both_directions(self):
        # Consumers without compress= still read compressed payloads, and vice versa.
        plain, zstd = make_codec(), make_codec(compress="zstd")
        assert plain.decode(zstd.encode(self.FEED)) == self.FEED
        assert zstd.decode(plain.encode(self.FEED)) == self.FEED

    @needs_zstd
    def test_decompression_bomb_is_rejected(self):
        codec = make_codec(compress="zstd", max_decompressed_size=10_000)
        blob = make_codec(compress="zstd").encode(self.FEED)
        with pytest.raises(codec_mod.DecodeError, match="exceeds 10000 bytes"):
            codec.decode(blob)

    @needs_zstd
    def test_corrupt_frame_raises_decode_error(self):
        codec = make_codec(compress="zstd")
        blob = codec.encode(self.FEED)
        with pytest.raises(codec_mod.DecodeError, match="invalid zstd payload"):
            codec.decode(blob[:4] + b"garbage" + blob[11:40])

    def test_invalid_compression_name(self):
        with pytest.raises(ValueError, match="unsupported compression"):
            make_codec(compress="gzip")

    def test_missing_backend_is_reported(self, monkeypatch):
        monkeypatch.setattr(codec_mod, "_std_zstd", None)
        monkeypatch.setattr(codec_mod, "_zstandard", None)
        with pytest.raises(ImportError, match="zstandard"):
            make_codec(compress="zstd")
        # A compressed payload without a backend is a clean DecodeError, not a crash.
        with pytest.raises(codec_mod.DecodeError, match="no zstd support"):
            make_codec().decode(codec_mod.ZSTD_MAGIC + b"\x00" * 8)


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

    @pytest.mark.parametrize(
        "created", [0.0, 1.9995, 1727250000.123456, 1727250000.999999, 1727250001.0, -1.5]
    )
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
        logger.info(
            "x",
            extra={
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
                "order": Order(
                    uuid.UUID(int=2), decimal.Decimal("3"), Color.GREEN, dt.datetime(2024, 1, 1)
                ),
            },
        )
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

    def test_default_hook_matches_python_fallback(self, json_logger, monkeypatch):
        extras = {
            "when": dt.datetime(2024, 1, 2, 3, 4, 5),
            "day": dt.date(2024, 1, 2),
            "id": uuid.UUID(int=1),
            "amount": decimal.Decimal("1.50"),
            "took": dt.timedelta(seconds=1.5),
            "color": Color.RED,
            "tags": frozenset({"a"}),
            "obj": object(),
            "bad": Unreprable(),
            "order": Order(
                uuid.UUID(int=2), decimal.Decimal("3"), Color.GREEN, dt.datetime(2024, 1, 1)
            ),
        }
        expected = json.loads(rjson.dumps_str(logging_mod._jsonable(extras, 0)))
        # No NaN and only str keys: the default= hook handles everything in one call.
        monkeypatch.setattr(logging_mod, "_jsonable", None)
        logger, lines = json_logger
        logger.info("x", extra=extras)
        (line,) = lines()
        assert {k: line[k] for k in extras} == expected

    def test_non_str_keys_skip_the_python_fallback(self, json_logger, monkeypatch):
        monkeypatch.setattr(logging_mod, "_jsonable", None)
        logger, lines = json_logger
        logger.info("x", extra={"counts": {7: 2, 2.5: 3, None: 4, False: 5}})
        (line,) = lines()
        assert line["counts"] == {"7": 2, "2.5": 3, "null": 4, "false": 5}

    def test_default_hook_never_raises(self):
        class BadMapping(Mapping):
            def __getitem__(self, key):
                raise KeyError(key)

            def __iter__(self):
                raise RuntimeError("broken")

            def __len__(self):
                return 1

            def __repr__(self):
                return "<BadMapping>"

        assert logging_mod._encode_default(BadMapping()) == "<BadMapping>"
        assert logging_mod._encode_default(Unreprable()) == "<unprintable Unreprable>"

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
        # dumps_str would accept it, but the line could never be written as UTF-8.
        buf = io.StringIO()
        with pytest.raises(logging_mod.NDJSONError) as info:
            logging_mod.write_ndjson(buf, [{"ok": 1}, {"bad": "\ud800"}])
        assert info.value.lineno == 2
        assert buf.getvalue() == '{"ok":1}\n'

    def test_demo_runs(self):
        out = run_demo("json_logging")
        first = json.loads(out.splitlines()[0])
        assert first["message"] == "user ada logged in" and first["ratio"] == "nan"
        assert "raise mode: line 6" in out


# ---------------------------------------------------------------------------------------
# examples/fastapi_app.py (skipped without fastapi + httpx)
# ---------------------------------------------------------------------------------------


@pytest.fixture(scope="module")
def api():
    """Return ``(example_module, TestClient)``; skip if fastapi or httpx is missing."""
    pytest.importorskip("fastapi")
    pytest.importorskip("httpx")
    with warnings.catch_warnings():  # newer Starlette warns about httpx vs httpx2
        warnings.simplefilter("ignore")
        from fastapi.testclient import TestClient
    mod = load_example("fastapi_app")
    return mod, TestClient(mod.app, raise_server_exceptions=False)


JSON_HEADERS = {"content-type": "application/json"}
NATIVE_CONTENT = {
    "s": 'é\n"\\\x00\u2028😀',
    "i": [0, -1, 2**63, 2**100],
    "f": [0.5, 1e16, -0.0, 1e-4],
    "b": [True, False, None],
    "nested": {"k": [{"x": []}, {}]},
}


class TestFastAPIResponse:
    def test_render_matches_starlette(self, api):
        from starlette.responses import JSONResponse

        mod, _ = api
        assert mod.RJSONResponse(NATIVE_CONTENT).body == JSONResponse(NATIVE_CONTENT).body

    def test_small_floats_differ_from_starlette_bytes_not_values(self, api):
        from starlette.responses import JSONResponse

        mod, _ = api
        ours, theirs = mod.RJSONResponse([1e-7]).body, JSONResponse([1e-7]).body
        assert (ours, theirs) == (b"[1e-7]", b"[1e-07]")
        assert json.loads(ours) == json.loads(theirs)

    def test_media_type_header(self, api):
        mod, client = api
        response = client.get("/items")
        assert response.status_code == 200
        assert response.headers["content-type"] == "application/json"
        assert mod.RJSONResponse({}).media_type == "application/json"
        assert response.json()["count"] == 3

    def test_fallback_uses_jsonable_encoder(self, api):
        mod, _ = api
        content = {
            "when": dt.datetime(2024, 1, 2, 3, 4, 5),
            "id": uuid.UUID(int=1),
            "color": Color.GREEN,
            "tags": {"x"},
            "price": decimal.Decimal("1.10"),
            "model": mod.ItemIn(name="n", price=decimal.Decimal("2")),
        }
        body = json.loads(mod.RJSONResponse(content).body)
        assert body == {
            "when": "2024-01-02T03:04:05",
            "id": str(uuid.UUID(int=1)),
            "color": "green",
            "tags": ["x"],
            "price": 1.1,  # Decimal -> float
            "model": {"name": "n", "price": "2", "tags": []},
        }

    def test_native_types_skip_the_fallback(self, api, monkeypatch):
        mod, _ = api
        from fastapi.encoders import jsonable_encoder

        content = {
            "when": dt.datetime(2024, 1, 2, 3, 4, 5, 6, tzinfo=dt.timezone.utc),
            "day": dt.date(2024, 1, 2),
            "id": uuid.UUID(int=1),
            "color": Color.GREEN,
        }
        want = json.loads(rjson.dumps(jsonable_encoder(content)))

        class NoFallback(mod.RJSONResponse):
            fallback_encoder = staticmethod(lambda c: pytest.fail("fallback used"))

        assert json.loads(NoFallback(content).body) == want

    def test_dataclasses_keep_fastapi_semantics(self, api):
        mod, _ = api

        @dataclasses.dataclass
        class Rec:
            _internal: int
            name: str

        # jsonable_encoder keeps "_" fields (dataclasses.asdict); rjson/orjson drop them.
        assert json.loads(mod.RJSONResponse({"r": Rec(1, "n")}).body) == {
            "r": {"_internal": 1, "name": "n"}
        }

    def test_to_jsonable_fallback_keeps_decimal_precision(self, api):
        mod, _ = api

        class ExactResponse(mod.RJSONResponse):
            fallback_encoder = staticmethod(mod.to_jsonable)

        content = {
            "price": decimal.Decimal("0.1000000000000000000001"),
            5: Color.RED,
            "t": (dt.date(2024, 1, 1), dt.time(1, 2)),
            "lvl": Level.LOW,
        }
        assert json.loads(ExactResponse(content).body) == {
            "price": "0.1000000000000000000001",
            "5": "red",
            "t": ["2024-01-01", "01:02:00"],
            "lvl": 1,
        }

    def test_to_jsonable_rejects_unknown_types(self, api):
        mod, _ = api
        with pytest.raises(TypeError, match="not JSON serializable"):
            mod.to_jsonable({"x": object()})

    @pytest.mark.parametrize("content", [float("nan"), {"x": [float("inf")]}])
    def test_non_finite_floats_rejected_like_starlette(self, api, content):
        from starlette.responses import JSONResponse

        mod, _ = api
        with pytest.raises(ValueError):
            JSONResponse(content)
        with pytest.raises(ValueError):
            mod.RJSONResponse(content)

    def test_lone_surrogate_raises(self, api):
        mod, _ = api
        with pytest.raises(UnicodeEncodeError):
            mod.RJSONResponse({"s": "\ud800"})

    def test_unencodable_response_is_a_500(self, api):
        mod, client = api
        mod.app.add_api_route("/_nan", lambda: mod.RJSONResponse({"x": float("nan")}))
        assert client.get("/_nan").status_code == 500


class TestFastAPIRequest:
    def test_body_model_parsed_by_rjson(self, api):
        _, client = api
        response = client.post("/items", json={"name": "pen", "price": "1.10", "tags": ["a"]})
        assert response.status_code == 201
        body = response.json()
        assert body["price"] == "1.10" and body["tags"] == ["a"]
        uuid.UUID(body["id"])

    def test_route_really_uses_rjson(self, api, monkeypatch):
        mod, client = api
        calls = []
        real = rjson.loads
        monkeypatch.setattr(mod.rjson, "loads", lambda b: calls.append(b) or real(b))
        client.post("/items", json={"name": "pen", "price": 1})
        client.post("/events", json={"a": 1})
        assert len(calls) == 2

    @pytest.mark.parametrize(
        "payload",
        [
            b'{"name": "pen",',
            b"{'name': 'pen'}",
            b'{"name": "pen", "price": NaN}',
            b'\xef\xbb\xbf{"name": "pen", "price": 1}',
            b'{"name": "\xff", "price": 1}',
            b'{"name": "\\ud83d", "price": 1}',
        ],
        ids=["truncated", "single-quotes", "NaN", "BOM", "bad-utf8", "lone-surrogate"],
    )
    def test_invalid_json_is_422_json_invalid(self, api, payload):
        _, client = api
        response = client.post("/items", content=payload, headers=JSON_HEADERS)
        assert response.status_code == 422
        (error,) = response.json()["detail"]
        assert error["type"] == "json_invalid"
        assert error["loc"][0] == "body" and isinstance(error["loc"][1], int)
        assert error["ctx"]["error"]

    def test_validation_error_unchanged(self, api):
        _, client = api
        response = client.post("/items", json={"name": "pen"})
        assert response.status_code == 422
        assert response.json()["detail"][0]["type"] == "missing"

    def test_request_json_is_cached(self, api):
        import asyncio

        mod, _ = api
        body = b'{"a": [1, 2]}'
        sent = []

        async def receive():
            sent.append(1)
            return {"type": "http.request", "body": body, "more_body": False}

        async def run():
            request = mod.RJSONRequest({"type": "http", "method": "POST", "headers": []}, receive)
            first = await request.json()
            assert await request.json() is first
            return first

        assert asyncio.run(run()) == {"a": [1, 2]} and sent == [1]


class TestFastAPIJsonBodyDependency:
    def test_accepts_json_and_plus_json(self, api):
        _, client = api
        for content_type in (
            "application/json",
            "application/cloudevents+json; charset=utf-8",
            "Application/JSON",
        ):
            response = client.post(
                "/events",
                content=b'{"big": 12345678901234567890123}',
                headers={"content-type": content_type},
            )
            assert response.status_code == 202, content_type
            assert response.headers["content-type"] == "application/json"
            body = response.json()
            assert body["accepted"] == {"big": 12345678901234567890123}  # exact big int
            dt.datetime.fromisoformat(body["received_at"])  # datetime took the fallback

    @pytest.mark.parametrize(
        ("payload", "position"),
        [
            (b'{"type": ', 9),
            (b"", 0),
            (b"[1] x", 4),
            (b'{"a": "\xff"}', 7),
        ],
    )
    def test_invalid_json_is_400_with_position(self, api, payload, position):
        _, client = api
        response = client.post("/events", content=payload, headers=JSON_HEADERS)
        assert response.status_code == 400
        detail = response.json()["detail"]
        assert detail["error"] == "invalid_json"
        assert detail["position"] == position and detail["line"] == 1
        assert detail["message"]

    @pytest.mark.parametrize(
        "content_type", ["text/plain", "application/x-www-form-urlencoded", "application/jsonx", ""]
    )
    def test_wrong_content_type_is_415(self, api, content_type):
        _, client = api
        response = client.post("/events", content=b"{}", headers={"content-type": content_type})
        assert response.status_code == 415

    def test_non_object_is_422(self, api):
        _, client = api
        assert client.post("/events", json=[1, 2]).status_code == 422

    def test_demo_runs(self, api):
        out = run_demo("fastapi_app")
        assert "GET /items -> 200 application/json" in out
        assert "POST /events -> 400" in out and "POST /events -> 415" in out
