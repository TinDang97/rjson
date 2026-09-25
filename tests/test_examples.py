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
import json
import subprocess
import sys
import uuid
from pathlib import Path
from types import ModuleType

import pytest

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
