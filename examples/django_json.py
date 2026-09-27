"""Django integration: rjson for ``JsonResponse`` and request bodies.

* :class:`RJSONResponse` is a ``django.http.JsonResponse`` that renders with
  ``rjson.dumps``. datetime, UUID and Enum values are native; Decimal, sets and
  other types take a small fallback (``Decimal`` becomes a string).
* :func:`loads_body` parses ``request.body`` with ``rjson.loads``. Invalid JSON
  becomes a 400 whose JSON detail carries the message, line, column and position.

Run ``python examples/django_json.py`` for a demo through Django's test client
(needs ``django``).
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

__all__ = ["RJSONResponse", "app", "loads_body", "to_jsonable"]


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


def _render(data: Any, fallback: Callable[[Any], Any]) -> bytes:
    try:
        return rjson.dumps(data, passthrough=rjson.PASSTHROUGH_DATACLASS)
    except UnicodeEncodeError:
        raise
    except (TypeError, ValueError):
        return rjson.dumps(fallback(data))


try:
    from django.http import HttpRequest, HttpResponse, HttpResponseBadRequest, JsonResponse
except ImportError:  # pragma: no cover

    class RJSONResponse:  # type: ignore[no-redef]
        """Placeholder so the module can be imported without Django."""

        def __init__(self, *args: Any, **kwargs: Any) -> None:
            raise ImportError("django is required for examples.django_json")

    def loads_body(request: Any) -> Any:
        raise ImportError("django is required for examples.django_json")

    app = None
else:

    class RJSONResponse(JsonResponse):
        """``JsonResponse`` rendered with ``rjson.dumps``.

        Matches Django's compact JSON (no ASCII escaping of non-ASCII text).
        datetime, UUID and Enum are serialized natively. Values rjson cannot
        encode take :attr:`fallback_encoder` and are serialized again.

        ``safe`` behaves as in Django: a non-dict payload raises ``TypeError``
        unless ``safe=False``.
        """

        fallback_encoder: ClassVar[Callable[[Any], Any]] = staticmethod(to_jsonable)

        def __init__(
            self,
            data: Any,
            encoder: Any = None,
            safe: bool = True,
            json_dumps_params: Any = None,
            **kwargs: Any,
        ) -> None:
            if safe and not isinstance(data, dict):
                raise TypeError(
                    "In order to allow non-dict objects to be serialized set the "
                    "safe parameter to False."
                )
            kwargs.setdefault("content_type", "application/json")
            payload = _render(data, type(self).fallback_encoder)
            HttpResponse.__init__(self, content=payload, **kwargs)

    def loads_body(request: HttpRequest) -> Any:
        """Parse ``request.body`` with rjson.

        Raises:
            json.JSONDecodeError: the body is empty or not valid JSON.
        """
        return rjson.loads(request.body)

    def bad_json(exc: json.JSONDecodeError) -> HttpResponse:
        return HttpResponseBadRequest(
            rjson.dumps(
                {
                    "error": "invalid_json",
                    "message": exc.msg,
                    "line": exc.lineno,
                    "column": exc.colno,
                    "position": exc.pos,
                }
            ),
            content_type="application/json",
        )

    _ITEMS: list[dict[str, Any]] = [
        {"id": i, "name": f"item {i}", "price": i * 1.25, "tags": ["demo"]} for i in range(3)
    ]

    def list_items(request: HttpRequest) -> HttpResponse:
        return RJSONResponse({"items": _ITEMS, "count": len(_ITEMS)})

    def create_event(request: HttpRequest) -> HttpResponse:
        if request.method != "POST":
            return HttpResponse(status=405)
        try:
            event = loads_body(request)
        except json.JSONDecodeError as exc:
            return bad_json(exc)
        if not isinstance(event, dict):
            return HttpResponseBadRequest(
                rjson.dumps({"error": "event must be a JSON object"}),
                content_type="application/json",
            )
        return RJSONResponse(
            {"accepted": event, "received_at": dt.datetime.now(dt.timezone.utc)},
            status=202,
        )

    def _configure() -> list[Any]:
        import sys

        import django
        from django.conf import settings
        from django.urls import path

        sys.modules.setdefault(__name__, sys.modules[__name__])
        if not settings.configured:
            settings.configure(
                DEBUG=True,
                SECRET_KEY="rjson-example",
                ROOT_URLCONF=__name__,
                ALLOWED_HOSTS=["*", "testserver"],
            )
            django.setup()
        return [
            path("items", list_items),
            path("events", create_event),
        ]

    urlpatterns = _configure()

    def get_wsgi_application() -> Any:
        from django.core.wsgi import get_wsgi_application as _get

        return _get()

    app = get_wsgi_application()


def _demo() -> None:
    try:
        from django.test import Client
    except ImportError as exc:
        raise SystemExit(f"the demo needs django: {exc}") from exc

    client = Client()
    calls: list[tuple[str, str, dict[str, Any]]] = [
        ("GET", "/items", {}),
        (
            "POST",
            "/events",
            {
                "data": b'{\"type\": \"click\", \"big\": 12345678901234567890}',
                "content_type": "application/json",
            },
        ),
        ("POST", "/events", {"data": b'{\"type\": ', "content_type": "application/json"}),
    ]
    for method, url, kwargs in calls:
        if method == "POST":
            response = client.generic(method, url, **kwargs)
        else:
            response = client.get(url)
        print(f"{method} {url} -> {response.status_code} {response['Content-Type']}")
        print(f"    {response.content.decode()}")


if __name__ == \"__main__\":
    _demo()
