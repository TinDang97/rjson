"""A bytes codec for caches and message queues (Redis, Kafka, SQS, ...), built on rjson.

rjson has no ``object_hook`` option, and its ``default=`` hook never sees tuples or dict
keys (tuples encode natively as lists), so this module adds a small, explicit type-hook
layer on top of it that round-trips both:

* ``encode(obj) -> bytes`` / ``decode(data) -> obj`` with ``bytes``, ``bytearray`` or
  ``memoryview`` input (zero-copy from socket buffers).
* An optional envelope ``{"schema": ..., "version": ..., "tagged": ..., "data": ...}`` so
  consumers can reject foreign payloads and upgrade old ones (rolling deploys, cache keys
  that outlive a release).
* Optional zstd compression (``compress="zstd"``) for payloads above a size threshold.
  Compressed payloads are recognized by the zstd frame magic, which no JSON document can
  start with, so ``decode`` reads both forms and compression can be rolled out without a
  coordinated deploy. Decompression is capped (``max_decompressed_size``) against
  decompression bombs.
* Round-tripping ``datetime``/``date``/``time``/``timedelta``, ``UUID``, ``Decimal``,
  ``set``/``frozenset``, ``bytes``, ``Enum`` members and dataclasses. Enum and dataclass
  types must be registered: the decoder never imports a class named by the payload
  (that would be pickle-style remote code execution).

Performance design: the payload is first handed to ``rjson.dumps`` as-is. Only when that
fails (an unsupported type is somewhere in the tree) is the pure-Python hook walk run,
and the envelope records ``"tagged": true`` so the decoder walks the result only for
those payloads. Plain JSON data therefore pays nothing for the hook layer.

Normalizations shared with ``json``/``orjson`` (not round-tripped): tuples and
namedtuples come back as lists, ``IntEnum``/``StrEnum`` members as plain ``int``/``str``,
and ``str``/``int``/``float``/``dict``/``list`` subclasses as their base type.

Run ``python examples/codec.py`` for a demo with a fake Redis and Kafka-style callables.
"""

from __future__ import annotations

import base64
import dataclasses
import datetime as dt
import decimal
import enum
import importlib
import json
import uuid
from collections.abc import Callable, Mapping
from typing import Any, Literal

import rjson


def _optional_module(name: str) -> Any:
    try:
        return importlib.import_module(name)
    except ImportError:
        return None


# zstd backends: the standard library on Python 3.14+, else ``pip install zstandard``.
_std_zstd: Any = _optional_module("compression.zstd")
_zstandard: Any = _optional_module("zstandard")

__all__ = [
    "TAG",
    "Codec",
    "CodecError",
    "DecodeError",
    "EncodeError",
    "KafkaDeserializer",
    "KafkaSerializer",
]

#: Key that marks a tagged value, e.g. ``{"$rjson": "uuid", "v": "..."}``.
TAG = "$rjson"
#: rjson serializes datetime/UUID/dataclass/Enum natively, as strings or objects that
#: decode as plain JSON. The codec needs exact round trips, so it passes them through:
#: they raise, and are tagged by the type hooks (or rejected without them).
_PASSTHROUGH = (
    rjson.PASSTHROUGH_DATETIME
    | rjson.PASSTHROUGH_UUID
    | rjson.PASSTHROUGH_DATACLASS
    | rjson.PASSTHROUGH_ENUM
)

#: Same nesting limit as ``rjson.dumps``; also stops circular references in the walk.
MAX_DEPTH = 254

Buffer = bytes | bytearray | memoryview
#: First four bytes of every zstd frame. JSON text cannot start with 0x28 ("(").
ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"
_ENVELOPE_KEYS = frozenset({"schema", "version", "tagged", "data"})


class CodecError(ValueError):
    """Base class for codec failures."""


class EncodeError(CodecError, TypeError):
    """An object cannot be encoded.

    Subclasses both ``ValueError`` and ``TypeError`` (like ``rjson.JSONEncodeError``, which
    ``rjson.dumps`` raises), so existing ``except`` clauses keep working whichever library
    the caller migrated from.
    """


class DecodeError(CodecError):
    """A payload is not valid JSON or does not match the expected envelope."""


class Codec:
    """JSON codec with an optional schema/version envelope and round-tripping type hooks.

    Args:
        schema: Name stored in the envelope and checked on decode (``None`` disables the
            check). Use one schema per logical message type or cache namespace.
        version: Current payload version, stored in the envelope.
        migrations: ``{n: fn}`` upgrades ``data`` from version ``n`` to ``n + 1``. Older
            payloads are upgraded step by step; newer ones raise ``DecodeError`` (treat it
            as a cache miss, or route the message to a dead-letter queue).
        envelope: Wrap payloads in the envelope. ``False`` produces bare JSON for
            interoperability with non-Python consumers; type hooks then must be off.
        type_hooks: Encode the extended types listed in the module docstring.
        compress: ``"zstd"`` compresses encoded payloads of at least
            ``compress_min_size`` bytes (needs Python 3.14+ or the ``zstandard`` package).
            Worth it when bytes are expensive: Redis memory, cross-region Kafka, slow links.
            JSON typically shrinks 10-20x, at a CPU cost comparable to ``rjson.dumps``
            itself (see "Transfer size" in docs/PRODUCTION_READINESS.md).
        compress_min_size: Payloads smaller than this stay plain JSON (small payloads
            barely compress and the frame header costs ~10 bytes).
        compress_level: zstd level; 1-3 are fast, higher levels trade CPU for size.
        max_decompressed_size: Upper bound for a decompressed payload; larger ones raise
            ``DecodeError`` instead of exhausting memory.

    Example:
        >>> codec = Codec(schema="user", version=1)
        >>> codec.decode(codec.encode({"id": uuid.UUID(int=1)}))
        {'id': UUID('00000000-0000-0000-0000-000000000001')}
    """

    def __init__(
        self,
        *,
        schema: str | None = None,
        version: int = 1,
        migrations: Mapping[int, Callable[[Any], Any]] | None = None,
        envelope: bool = True,
        type_hooks: bool = True,
        compress: Literal["zstd"] | None = None,
        compress_min_size: int = 1024,
        compress_level: int = 3,
        max_decompressed_size: int = 64 * 1024 * 1024,
    ) -> None:
        if type_hooks and not envelope:
            raise ValueError("type_hooks require envelope=True (the envelope marks tagged data)")
        if version < 1:
            raise ValueError("version must be >= 1")
        self.schema = schema
        self.version = version
        self.migrations = dict(migrations or {})
        self.envelope = envelope
        self.type_hooks = type_hooks
        if compress not in (None, "zstd"):
            raise ValueError(f"unsupported compression {compress!r} (use None or 'zstd')")
        if compress is not None and _std_zstd is None and _zstandard is None:
            raise ImportError("compress='zstd' needs Python 3.14+ or `pip install zstandard`")
        self.compress = compress
        self.compress_min_size = compress_min_size
        self.compress_level = compress_level
        self.max_decompressed_size = max_decompressed_size
        self._types: dict[str, type] = {}
        self._names: dict[type, str] = {}

    def register(self, cls: type, name: str | None = None) -> type:
        """Allow an ``Enum`` or dataclass type to round-trip. Usable as a decorator.

        Args:
            cls: The ``Enum`` subclass or dataclass to register.
            name: Stable wire name; defaults to ``module.QualName``. Set it explicitly if
                the class may move between modules while payloads are in flight.

        Returns:
            ``cls`` unchanged.

        Raises:
            TypeError: ``cls`` is neither an ``Enum`` nor a dataclass, or it is an
                ``IntEnum``/``StrEnum``-style enum: rjson already encodes those as their
                value on the fast path, so they cannot be tagged consistently.
            ValueError: ``name`` is already registered for another type.
        """
        if isinstance(cls, type) and issubclass(cls, enum.Enum):
            if issubclass(cls, (int, str, float)):
                raise TypeError(
                    f"{cls.__qualname__} mixes in int/str/float: it is encoded "
                    "as its value and decodes as a plain value, so it cannot "
                    "be registered"
                )
        elif not (isinstance(cls, type) and dataclasses.is_dataclass(cls)):
            raise TypeError(f"only Enum subclasses and dataclasses can be registered, not {cls!r}")
        key = name or f"{cls.__module__}.{cls.__qualname__}"
        if self._types.get(key, cls) is not cls:
            raise ValueError(f"type name {key!r} is already registered")
        self._types[key] = cls
        self._names[cls] = key
        return cls

    # -- encoding -----------------------------------------------------------------------

    def encode(self, obj: Any) -> bytes:
        """Serialize ``obj`` to UTF-8 JSON bytes.

        Raises:
            EncodeError: ``obj`` contains an unsupported type, a non-finite float, a
                non-string dict key (without type hooks), a lone surrogate, or nesting
                deeper than 254 levels.
        """
        return self._maybe_compress(self._encode_json(obj))

    def _encode_json(self, obj: Any) -> bytes:
        if not self.envelope:
            return _dumps(obj)
        try:
            # Fast path: plain JSON data needs no walk.
            return rjson.dumps(self._wrap(obj, tagged=False), passthrough=_PASSTHROUGH)
        except ValueError as exc:
            if not self.type_hooks or isinstance(exc, UnicodeEncodeError):
                raise EncodeError(str(exc)) from exc
        return _dumps(self._wrap(self._to_json(obj, 0), tagged=True))

    def _maybe_compress(self, raw: bytes) -> bytes:
        if self.compress is None or len(raw) < self.compress_min_size:
            return raw
        if _std_zstd is not None:
            return bytes(_std_zstd.compress(raw, level=self.compress_level))
        assert _zstandard is not None  # checked in __init__
        return bytes(_zstandard.ZstdCompressor(level=self.compress_level).compress(raw))

    def _wrap(self, data: Any, *, tagged: bool) -> dict[str, Any]:
        return {"schema": self.schema, "version": self.version, "tagged": tagged, "data": data}

    def _to_json(self, obj: Any, depth: int) -> Any:
        """Return a copy of ``obj`` that rjson can serialize, with extended types tagged."""
        if depth > MAX_DEPTH:
            raise EncodeError(f"nesting deeper than {MAX_DEPTH} levels (circular reference?)")
        cls = type(obj)
        if cls is str or cls is int or cls is bool or obj is None:
            return obj
        if cls is float:
            if obj != obj or obj in (float("inf"), float("-inf")):
                raise EncodeError(f"cannot encode non-finite float {obj!r}")
            return obj
        if isinstance(obj, dict):
            if all(type(k) is str for k in obj) and TAG not in obj:
                return {k: self._to_json(v, depth + 1) for k, v in obj.items()}
            # Non-str keys (int, UUID, tuple, ...) or a user key that collides with TAG.
            items = [
                [self._to_json(k, depth + 1), self._to_json(v, depth + 1)] for k, v in obj.items()
            ]
            return {TAG: "dict", "v": items}
        if isinstance(obj, (list, tuple)):
            return [self._to_json(v, depth + 1) for v in obj]
        if isinstance(obj, enum.Enum):
            name = self._names.get(cls)
            if name is not None:
                return {TAG: "enum", "t": name, "v": obj.name}
            if isinstance(obj, (int, str)):  # IntEnum/StrEnum: rjson encodes the value.
                return obj
            raise EncodeError(f"Enum type {cls.__qualname__} is not registered with the codec")
        if isinstance(obj, float):  # float subclass: check finiteness on the plain value.
            return self._to_json(float(obj), depth)
        if isinstance(obj, (str, int)):  # str/int subclasses: rjson encodes them natively.
            return obj
        return self._tag_scalar(obj, depth)

    def _tag_scalar(self, obj: Any, depth: int) -> dict[str, Any]:
        # datetime is a subclass of date: test it first.
        if isinstance(obj, dt.datetime):
            return {TAG: "datetime", "v": obj.isoformat()}
        if isinstance(obj, dt.date):
            return {TAG: "date", "v": obj.isoformat()}
        if isinstance(obj, dt.time):
            return {TAG: "time", "v": obj.isoformat()}
        if isinstance(obj, dt.timedelta):
            return {TAG: "timedelta", "v": [obj.days, obj.seconds, obj.microseconds]}
        if isinstance(obj, uuid.UUID):
            return {TAG: "uuid", "v": str(obj)}
        if isinstance(obj, decimal.Decimal):
            return {TAG: "decimal", "v": str(obj)}  # str keeps precision and NaN/Infinity.
        if isinstance(obj, (set, frozenset)):
            kind = "set" if isinstance(obj, set) else "frozenset"
            return {TAG: kind, "v": [self._to_json(v, depth + 1) for v in obj]}
        if isinstance(obj, (bytes, bytearray, memoryview)):
            return {TAG: "bytes", "v": base64.b64encode(obj).decode("ascii")}
        if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
            name = self._names.get(type(obj))
            if name is None:
                raise EncodeError(f"dataclass {type(obj).__qualname__} is not registered")
            fields = {
                f.name: self._to_json(getattr(obj, f.name), depth + 1)
                for f in dataclasses.fields(obj)
                if f.init
            }
            return {TAG: "dataclass", "t": name, "v": fields}
        raise EncodeError(f"cannot encode object of type {type(obj).__qualname__}")

    # -- decoding -----------------------------------------------------------------------

    def decode(self, data: Buffer | str) -> Any:
        """Parse a payload produced by :meth:`encode` (or plain JSON if ``envelope=False``).

        Raises:
            DecodeError: Invalid JSON or UTF-8, wrong schema, unknown version or type tag.
        """
        doc = _loads(self._maybe_decompress(data))
        if not self.envelope:
            return doc
        # Extra keys are allowed so a later release can add headers (trace id, ...)
        # without breaking consumers that are still on this version.
        if not (
            isinstance(doc, dict) and doc.keys() >= _ENVELOPE_KEYS and type(doc["tagged"]) is bool
        ):
            raise DecodeError("payload is not a codec envelope")
        if self.schema is not None and doc["schema"] != self.schema:
            raise DecodeError(f"schema mismatch: expected {self.schema!r}, got {doc['schema']!r}")
        payload = doc["data"]
        if doc["tagged"]:
            if not self.type_hooks:
                raise DecodeError("payload uses type tags but type_hooks=False")
            payload = self._from_json(payload)
        return self._migrate(payload, doc["version"])

    def _maybe_decompress(self, data: Buffer | str) -> Buffer | str:
        """Decompress zstd frames (recognized by magic); pass JSON through untouched."""
        if not isinstance(data, (bytes, bytearray, memoryview)):
            return data  # str input, or a wrong type that _loads reports
        if isinstance(data, memoryview) and (data.ndim != 1 or data.itemsize != 1):
            head = data.tobytes()[:4]  # rare layouts: correctness over a copy
        else:
            head = bytes(data[:4])
        if head != ZSTD_MAGIC:
            return data
        limit = self.max_decompressed_size
        try:
            if _std_zstd is not None:
                out = _std_zstd.ZstdDecompressor().decompress(bytes(data), max_length=limit + 1)
            elif _zstandard is not None:
                reader = _zstandard.ZstdDecompressor().stream_reader(bytes(data))
                out = reader.read(limit + 1)
            else:
                raise DecodeError("zstd payload, but no zstd support (Python 3.14+ or zstandard)")
        except DecodeError:
            raise
        except Exception as exc:  # zstd raises its own error types per backend
            raise DecodeError(f"invalid zstd payload: {exc}") from exc
        if len(out) > limit:
            raise DecodeError(f"decompressed payload exceeds {limit} bytes")
        return bytes(out)

    def _migrate(self, payload: Any, version: Any) -> Any:
        if type(version) is not int or version < 1 or version > self.version:
            raise DecodeError(f"unsupported payload version {version!r} (current {self.version})")
        while version < self.version:
            step = self.migrations.get(version)
            if step is None:
                raise DecodeError(f"no migration from version {version} to {version + 1}")
            payload = step(payload)
            version += 1
        return payload

    def _from_json(self, obj: Any) -> Any:
        if isinstance(obj, list):
            return [self._from_json(v) for v in obj]
        if not isinstance(obj, dict):
            return obj
        tag = obj.get(TAG)
        if tag is None:
            return {k: self._from_json(v) for k, v in obj.items()}
        try:
            return self._untag(tag, obj)
        except DecodeError:
            raise
        except (KeyError, TypeError, ValueError, decimal.InvalidOperation) as exc:
            raise DecodeError(f"malformed {tag!r} value: {exc}") from exc

    def _untag(self, tag: str, obj: dict[str, Any]) -> Any:
        v = obj["v"]
        if tag == "datetime":
            return dt.datetime.fromisoformat(v)
        if tag == "date":
            return dt.date.fromisoformat(v)
        if tag == "time":
            return dt.time.fromisoformat(v)
        if tag == "timedelta":
            return dt.timedelta(days=v[0], seconds=v[1], microseconds=v[2])
        if tag == "uuid":
            return uuid.UUID(v)
        if tag == "decimal":
            return decimal.Decimal(v)
        if tag == "bytes":
            return base64.b64decode(v, validate=True)
        if tag == "set":
            return {self._from_json(x) for x in v}
        if tag == "frozenset":
            return frozenset(self._from_json(x) for x in v)
        if tag == "dict":
            return {_hashable(self._from_json(k)): self._from_json(val) for k, val in v}
        cls = self._types.get(obj.get("t"))  # type: ignore[arg-type]
        if tag == "enum" and cls is not None and issubclass(cls, enum.Enum):
            return cls[v]
        if tag == "dataclass" and cls is not None and dataclasses.is_dataclass(cls):
            return cls(**{k: self._from_json(x) for k, x in v.items()})
        raise DecodeError(f"unknown or unregistered type tag {tag!r} ({obj.get('t')!r})")


def _hashable(key: Any) -> Any:
    """Tuples encode as lists; turn them back into tuples so they can be dict keys."""
    return tuple(_hashable(k) for k in key) if isinstance(key, list) else key


def _dumps(obj: Any) -> bytes:
    try:
        return rjson.dumps(obj, passthrough=_PASSTHROUGH)
    except (TypeError, ValueError) as exc:  # JSONEncodeError, or UnicodeEncodeError
        raise EncodeError(str(exc)) from exc


def _loads(data: Buffer | str) -> Any:
    try:
        return rjson.loads(data)
    except json.JSONDecodeError as exc:
        raise DecodeError(f"invalid JSON: {exc}") from exc
    except TypeError as exc:  # wrong input type (any memoryview layout is accepted)
        raise DecodeError(f"cannot decode {type(data).__name__}: {exc}") from exc


# -- Kafka-style callables --------------------------------------------------------------


class KafkaSerializer:
    """Value/key serializer callable for Kafka clients.

    Matches both ``confluent_kafka.serialization.Serializer.__call__(obj, ctx=None)`` and
    aiokafka's ``value_serializer=callable(value)``. ``None`` maps to ``None`` so that
    tombstones (log-compacted deletes) pass through instead of becoming ``b"null"``.
    """

    def __init__(self, codec: Codec) -> None:
        self.codec = codec

    def __call__(self, obj: Any, ctx: object | None = None) -> bytes | None:
        """Encode ``obj``; ``ctx`` (confluent-kafka's SerializationContext) is unused."""
        return None if obj is None else self.codec.encode(obj)


class KafkaDeserializer:
    """Value/key deserializer callable; the counterpart of :class:`KafkaSerializer`."""

    def __init__(self, codec: Codec) -> None:
        self.codec = codec

    def __call__(self, data: Buffer | None, ctx: object | None = None) -> Any:
        """Decode ``data``; ``ctx`` (confluent-kafka's SerializationContext) is unused."""
        return None if data is None else self.codec.decode(data)


# -- demo -------------------------------------------------------------------------------


class FakeRedis:
    """The subset of the redis-py client used by the demo (values are stored as bytes)."""

    def __init__(self) -> None:
        self._store: dict[str, bytes] = {}

    def set(self, key: str, value: bytes, ex: int | None = None) -> bool:
        """Store ``value``; ``ex`` (TTL seconds) is accepted and ignored."""
        if not isinstance(value, bytes):
            raise TypeError("redis values must be bytes here")
        self._store[key] = value
        return True

    def get(self, key: str) -> bytes | None:
        """Return the stored bytes or ``None``."""
        return self._store.get(key)


def _demo() -> None:
    class Status(enum.Enum):
        ACTIVE = "active"
        BANNED = "banned"

    @dataclasses.dataclass
    class User:
        id: uuid.UUID
        name: str
        status: Status
        balance: decimal.Decimal
        created: dt.datetime
        tags: frozenset[str] = frozenset()

    codec = Codec(schema="user", version=2, migrations={1: lambda d: {**d, "migrated": True}})
    codec.register(Status, "Status")
    codec.register(User, "User")

    user = User(
        uuid.uuid4(),
        "Ada",
        Status.ACTIVE,
        decimal.Decimal("10.50"),
        dt.datetime(2024, 5, 1, 12, 0, tzinfo=dt.timezone.utc),
        frozenset({"admin"}),
    )

    redis = FakeRedis()
    redis.set("user:1", codec.encode(user), ex=300)
    raw = redis.get("user:1")
    assert raw is not None
    print("stored bytes:", raw[:100], b"..." if len(raw) > 100 else b"")
    restored = codec.decode(memoryview(raw))
    print("round-trip equal:", restored == user)

    if _std_zstd is not None or _zstandard is not None:
        zcodec = Codec(schema="feed", compress="zstd")
        feed = [{"id": i, "title": f"post {i}", "tags": ["news", "tech"]} for i in range(500)]
        blob = zcodec.encode(feed)
        print(f"zstd:            {len(rjson.dumps(feed))} B of JSON stored as {len(blob)} B")
        print("zstd round-trip:", zcodec.decode(blob) == feed)

    plain = codec.encode({"hits": 3})  # plain JSON: fast path, "tagged": false
    print("plain payload:  ", plain)

    old = Codec(schema="user", version=1, type_hooks=False).encode({"hits": 3})
    print("v1 -> v2:       ", codec.decode(old))

    produce = KafkaSerializer(codec)  # e.g. Producer(value_serializer=produce)
    consume = KafkaDeserializer(codec)  # e.g. Consumer(value_deserializer=consume)
    print("kafka:          ", consume(produce({"event": "login", "user": user.id})))
    print("tombstone:      ", produce(None), consume(None))

    try:
        codec.decode(b'{"schema":"user","version":2,"tagged":false,"data":')
    except DecodeError as exc:
        print("bad payload:    ", exc)


if __name__ == "__main__":
    _demo()
