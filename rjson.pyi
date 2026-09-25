"""Type stubs for rjson (installed as ``rjson/__init__.pyi`` next to ``py.typed``)."""

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

def dumps(obj: Any, /) -> bytes:
    """Serialize to compact UTF-8 JSON bytes; raises JSONEncodeError."""

def dumps_str(obj: Any, /) -> str:
    """Serialize to a compact JSON str (non-ASCII kept as-is); raises JSONEncodeError."""

def dumps_bytes(obj: Any, /) -> bytes:
    """Alias of dumps."""
