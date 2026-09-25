"""Type stubs for rjson (installed as ``rjson/__init__.pyi`` next to ``py.typed``)."""

from collections.abc import Callable
from json import JSONDecodeError as JSONDecodeError
from typing import Any

__all__ = [
    "JSONDecodeError",
    "JSONEncodeError",
    "__version__",
    "dumps",
    "dumps_bytes",
    "dumps_str",
    "loads",
]

__version__: str

class JSONEncodeError(TypeError, ValueError):
    """Raised by dumps/dumps_str/dumps_bytes when an object cannot be serialized."""

def loads(data: str | bytes | bytearray | memoryview, /) -> Any:
    """Deserialize JSON to Python objects; raises JSONDecodeError (a ValueError)."""

def dumps(obj: Any, /, *, default: Callable[[Any], Any] | None = None) -> bytes:
    """Serialize to compact UTF-8 JSON bytes; raises JSONEncodeError.

    default: called with each object that cannot be serialized; its return
    value is serialized instead (it may raise to reject the object).
    """

def dumps_str(obj: Any, /, *, default: Callable[[Any], Any] | None = None) -> str:
    """Serialize to a compact JSON str (non-ASCII kept as-is); raises JSONEncodeError."""

def dumps_bytes(obj: Any, /, *, default: Callable[[Any], Any] | None = None) -> bytes:
    """Alias of dumps."""
