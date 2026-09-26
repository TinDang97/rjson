"""Type stubs for rjson (installed as ``rjson/__init__.pyi`` next to ``py.typed``)."""

from collections.abc import Callable
from json import JSONDecodeError as JSONDecodeError
from typing import Any, Final

__all__ = [
    "JSONDecodeError",
    "JSONEncodeError",
    "PASSTHROUGH_DATACLASS",
    "PASSTHROUGH_DATETIME",
    "PASSTHROUGH_ENUM",
    "PASSTHROUGH_UUID",
    "__version__",
    "dumps",
    "dumps_bytes",
    "dumps_str",
    "loads",
]

__version__: str

#: passthrough= flags (combine with |): these types go to default= instead of
#: being serialized natively.
PASSTHROUGH_DATETIME: Final[int]  # datetime.datetime, datetime.date, datetime.time
PASSTHROUGH_UUID: Final[int]  # uuid.UUID
PASSTHROUGH_DATACLASS: Final[int]  # dataclass instances
PASSTHROUGH_ENUM: Final[int]  # enum.Enum members (int/str/float mix-ins stay values)

class JSONEncodeError(TypeError, ValueError):
    """Raised by dumps/dumps_str/dumps_bytes when an object cannot be serialized."""

def loads(data: str | bytes | bytearray | memoryview, /) -> Any:
    """Deserialize JSON to Python objects; raises JSONDecodeError (a ValueError)."""

def dumps(
    obj: Any,
    /,
    *,
    default: Callable[[Any], Any] | None = None,
    passthrough: int | None = 0,
    non_str_keys: bool = False,
) -> bytes:
    """Serialize to compact UTF-8 JSON bytes; raises JSONEncodeError.

    Besides JSON types, serializes datetime/date/time (RFC 3339), uuid.UUID,
    dataclasses and Enum members, like orjson.

    default: called with each object that cannot be serialized; its return
    value is serialized instead (it may raise to reject the object).
    passthrough: PASSTHROUGH_* flags; those types go to default instead.
    non_str_keys: allow int, float, bool, None, Enum, datetime/date/time and UUID
    dict keys (int/float/bool/None written as json.dumps writes them).
    """

def dumps_str(
    obj: Any,
    /,
    *,
    default: Callable[[Any], Any] | None = None,
    passthrough: int | None = 0,
    non_str_keys: bool = False,
) -> str:
    """Serialize to a compact JSON str (non-ASCII kept as-is); raises JSONEncodeError."""

def dumps_bytes(
    obj: Any,
    /,
    *,
    default: Callable[[Any], Any] | None = None,
    passthrough: int | None = 0,
    non_str_keys: bool = False,
) -> bytes:
    """Alias of dumps."""
