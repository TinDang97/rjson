"""Type stubs for rjson (``rjson/__init__.pyi`` next to ``py.typed``)."""

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

def loads(data: str | bytes | bytearray | memoryview, /, *, lenient: bool = False) -> Any:
    """Deserialize JSON to Python objects; raises JSONDecodeError (a ValueError).

    lenient: also accept what json.loads accepts (NaN/Infinity, a UTF-8 BOM on
    bytes, numbers overflowing to inf, lone surrogates, UTF-16/32 bytes); the
    result then equals json.loads's.
    """

def dumps(
    obj: Any,
    /,
    *,
    default: Callable[[Any], Any] | None = None,
    passthrough: int | None = 0,
    non_str_keys: bool = False,
    indent: int | str | None = None,
    separators: tuple[str, str] | None = None,
    sort_keys: bool = False,
    ensure_ascii: bool = False,
    allow_nan: bool = False,
) -> bytes:
    """Serialize to UTF-8 JSON bytes (compact unless indent is given); raises JSONEncodeError.

    indent=2 and sort_keys=True give the same bytes as orjson's OPT_INDENT_2 and
    OPT_SORT_KEYS. indent (int or str such as "\t"), separators, ensure_ascii and
    allow_nan work as in json.dumps, except that the defaults stay compact,
    non-ASCII is kept and NaN/Infinity raise.

    Besides JSON types, serializes datetime/date/time (RFC 3339), uuid.UUID,
    dataclasses and Enum members, like orjson.

    default: called with each object that cannot be serialized; its return
    value is serialized instead (it may raise to reject the object).
    passthrough: PASSTHROUGH_* flags; those types go to default instead.
    non_str_keys: allow int, float, bool, None, Enum, datetime/date/time and UUID
    dict keys (int/float/bool/None written as json.dumps writes them).
    separators: (item_separator, key_separator); with an indent the default is
    (",", ": "), without one (",", ":").
    ensure_ascii: write non-ASCII characters (and DEL) as \\uXXXX escapes.
    allow_nan: write NaN/Infinity/-Infinity (as json does) instead of raising.
    """

def dumps_str(
    obj: Any,
    /,
    *,
    default: Callable[[Any], Any] | None = None,
    passthrough: int | None = 0,
    non_str_keys: bool = False,
    indent: int | str | None = None,
    separators: tuple[str, str] | None = None,
    sort_keys: bool = False,
    ensure_ascii: bool = False,
    allow_nan: bool = False,
) -> str:
    """Serialize to a JSON str (non-ASCII kept as-is; compact unless indent is given); raises JSONEncodeError."""

def dumps_bytes(
    obj: Any,
    /,
    *,
    default: Callable[[Any], Any] | None = None,
    passthrough: int | None = 0,
    non_str_keys: bool = False,
    indent: int | str | None = None,
    separators: tuple[str, str] | None = None,
    sort_keys: bool = False,
    ensure_ascii: bool = False,
    allow_nan: bool = False,
) -> bytes:
    """Alias of dumps."""
