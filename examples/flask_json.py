"""Flask integration: rjson as the app JSON provider.

:class:`RJSONProvider` implements ``flask.json.provider.JSONProvider`` with
``rjson.dumps_str`` / ``rjson.loads``. Register it with
``app.json = RJSONProvider(app)``. datetime, UUID and Enum values are native;
Decimal, sets and other types take a small fallback (``Decimal`` becomes a string).

Run ``python examples/flask_json.py`` for a demo through Flask's test client
(needs ``flask``).
"""

from __future__ import annotations

import datetime as dt
import decimal
import enum
import json
import uuid
from collections.abc import Callable
from typing import Any, ClassVar

import rjson

__all__ = ["RJSONProvider", "app", "to_jsonable"]


def to_jsonable(value: Any) -> Any:
    """Convert common non-JSON types to JSON-native values.

    datetime/date/time -> ISO 8601 string, UUID/Decimal -> string, Enum -> its
    value, set/frozenset/tuple -> list, non-str dict keys -> ``str(key)``.
    """
    if isinstance(value, enum.Enum):
        return to_jsonable(value.value)
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, dict):
        return {
            k if isinstance(k, str) else str(to_jsonable(k)): to_jsonable(v)
            for k, v in value.items()
        }
    if isinstance(value, (list, tuple, set, frozenset)):
        return [to_jsonable(v) for v in value]
    if isinstance(value, (dt.datetime, dt.date, dt.time)):
        return value.isoformat()
    if isinstance(value, (uuid.UUID, decimal.Decimal)):
        return str(value)
    raise TypeError(f"Object of type {type(value).__qualname__} is not JSON serializable")


try:
    from flask import Flask, request
    from flask.json.provider import JSONProvider
except ImportError:  # pragma: no cover

    class RJSONProvider:  # type: ignore[no-redef]
        """Placeholder so the module can be imported without Flask."""

        def __init__(self, *args: Any, **kwargs: Any) -> None:
            raise ImportError("flask is required for examples.flask_json")

    app = None
else:

    class RJSONProvider(JSONProvider):
        """Flask JSON provider backed by rjson.

        ``dumps`` returns a ``str`` (Flask's contract) via ``rjson.dumps_str``.
        ``loads`` uses ``rjson.loads``. Unsupported values take
        :attr:`fallback_encoder` and are serialized again.
        """

        fallback_encoder: ClassVar[Callable[[Any], Any]] = staticmethod(to_jsonable)

        def dumps(self, obj: Any, **kwargs: Any) -> str:
            try:
                return rjson.dumps_str(obj, passthrough=rjson.PASSTHROUGH_DATACLASS)
            except UnicodeEncodeError:
                raise
            except (TypeError, ValueError):
                return rjson.dumps_str(type(self).fallback_encoder(obj))

        def loads(self, s: str | bytes, **kwargs: Any) -> Any:
            return rjson.loads(s)

    _ITEMS: list[dict[str, Any]] = [
        {"id": i, "name": f"item {i}", "price": i * 1.25, "tags": ["demo"]} for i in range(3)
    ]

    app = Flask(__name__)
    app.json = RJSONProvider(app)

    @app.get("/items")
    def list_items() -> Any:
        return {"items": _ITEMS, "count": len(_ITEMS)}

    @app.post("/events")
    def create_event() -> Any:
        content_type = request.headers.get("content-type", "")
        media_type = content_type.split(";", 1)[0].strip().lower()
        if not (media_type == "application/json" or media_type.endswith("+json")):
            return {"error": f"expected application/json, got {content_type!r}"}, 415
        try:
            event = rjson.loads(request.get_data())
        except json.JSONDecodeError as exc:
            return (
                {
                    "error": "invalid_json",
                    "message": exc.msg,
                    "line": exc.lineno,
                    "column": exc.colno,
                    "position": exc.pos,
                },
                400,
            )
        if not isinstance(event, dict):
            return {"error": "event must be a JSON object"}, 422
        return (
            {"accepted": event, "received_at": dt.datetime.now(dt.timezone.utc)},
            202,
        )


_JSON_HEADERS = {"content-type": "application/json"}


def _demo() -> None:
    if app is None:
        raise SystemExit("the demo needs flask")
    client = app.test_client()
    payload_ok = b'{"type": "click", "big": 12345678901234567890}'
    payload_bad = b'{"type": '
    calls: list[tuple[str, str, dict[str, Any]]] = [
        ("GET", "/items", {}),
        ("POST", "/events", {"data": payload_ok, "headers": _JSON_HEADERS}),
        ("POST", "/events", {"data": payload_bad, "headers": _JSON_HEADERS}),
        ("POST", "/events", {"data": b"type=click", "headers": {"content-type": "text/plain"}}),
    ]
    for method, url, kwargs in calls:
        response = client.open(url, method=method, **kwargs)
        print(f"{method} {url} -> {response.status_code} {response.content_type}")
        print(f"    {response.get_data(as_text=True)}")


if __name__ == "__main__":
    _demo()
