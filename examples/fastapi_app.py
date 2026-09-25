"""FastAPI / Starlette integration: rjson for responses and request bodies.

What each piece buys you (measured on 100 small records, see ``examples/README.md``):

* ``return RJSONResponse(content)`` for data that is already JSON-native (dicts from a
  database driver, a cache, another service): FastAPI skips ``jsonable_encoder``, which
  costs ~100x more than ``rjson.dumps`` itself. This is the main win.
* Endpoints with a ``response_model``/return annotation: leave FastAPI's default response
  class alone. FastAPI then serializes with Pydantic's ``dump_json`` (Rust), which is as
  fast as ``dump_python`` + ``rjson.dumps``; a custom ``default_response_class`` would
  disable that path without a gain.
* :class:`RJSONRoute` parses request bodies with ``rjson.loads``. Invalid JSON still
  produces FastAPI's standard 422 ``json_invalid`` error because rjson raises
  ``json.JSONDecodeError``.
* :func:`json_body` is a dependency for endpoints that take arbitrary JSON (no model):
  invalid JSON becomes a 400 with the error position, a wrong content type a 415.

Run ``python examples/fastapi_app.py`` for a demo through ``TestClient``, or serve it with
``uvicorn fastapi_app:app`` from the ``examples`` directory.
"""

from __future__ import annotations

import datetime as dt
import decimal
import enum
import json
import uuid
from collections.abc import Callable
from typing import Annotated, Any, ClassVar

import rjson
from fastapi import APIRouter, Depends, FastAPI, HTTPException, Request, Response
from fastapi.encoders import jsonable_encoder
from fastapi.responses import JSONResponse
from fastapi.routing import APIRoute
from pydantic import BaseModel

__all__ = ["RJSONRequest", "RJSONResponse", "RJSONRoute", "app", "json_body", "to_jsonable"]


# -- responses ------------------------------------------------------------------------


class RJSONResponse(JSONResponse):
    """``JSONResponse`` rendered with ``rjson.dumps``.

    Output matches Starlette's ``JSONResponse`` (compact separators, UTF-8, no
    ASCII-escaping of non-ASCII text, ``NaN``/``Infinity`` rejected), with
    ``Content-Type: application/json``. One byte-level difference: floats below 1e-4
    use orjson's shortest form (``1e-7``, ``0.00001``) instead of Python's ``repr``
    (``1e-07``, ``1e-05``). Values are identical; body hashes/ETags/snapshots are not.

    rjson has no ``default=`` hook, so values it cannot encode (datetime, UUID, Decimal,
    Enum, Pydantic models, dataclasses, sets, non-str dict keys) take a fallback: the
    content is converted with :attr:`fallback_encoder` and serialized again. The fallback
    runs only after the fast path fails, so native data pays nothing for it.

    Choosing the fallback (set it on a subclass):

    * ``jsonable_encoder`` (default): FastAPI's own semantics, so responses look exactly
      as if FastAPI had serialized them. Slow (pure Python) and lossy for ``Decimal``
      (converted to ``float``/``int``).
    * :func:`to_jsonable`: small and explicit; ``Decimal`` becomes a string, so no
      precision is lost. Does not know about Pydantic ``Field`` aliases.
    """

    fallback_encoder: ClassVar[Callable[[Any], Any]] = staticmethod(jsonable_encoder)

    def render(self, content: Any) -> bytes:
        """Serialize ``content``; see the class docstring for the fallback."""
        try:
            return rjson.dumps(content)
        except UnicodeEncodeError:
            raise  # lone surrogate: no encoder can fix this; let it surface as a 500
        except (TypeError, ValueError):
            pass
        # A second failure (NaN/Infinity, circular reference) propagates, as it does with
        # Starlette's JSONResponse (allow_nan=False).
        return rjson.dumps(type(self).fallback_encoder(content))


def to_jsonable(value: Any) -> Any:
    """Convert common non-JSON types to JSON-native values (a cheaper ``jsonable_encoder``).

    datetime/date/time -> ISO 8601 string, UUID/Decimal -> string, Enum -> its value,
    set/frozenset/tuple -> list, Pydantic model -> ``model_dump(mode="json")``, non-str
    dict keys -> ``str(key)``.

    Raises:
        TypeError: ``value`` contains a type that has no JSON form here.
    """
    if isinstance(value, enum.Enum):
        return to_jsonable(value.value)
    if value is None or isinstance(value, (str, int, float)):
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
    if isinstance(value, BaseModel):
        return value.model_dump(mode="json")
    raise TypeError(f"Object of type {type(value).__qualname__} is not JSON serializable")


# -- requests -------------------------------------------------------------------------


class RJSONRequest(Request):
    """A ``Request`` whose ``json()`` uses ``rjson.loads`` (cached like Starlette's)."""

    async def json(self) -> Any:
        """Parse the body with rjson; raises ``json.JSONDecodeError`` on invalid JSON."""
        if not hasattr(self, "_json"):
            self._json = rjson.loads(await self.body())
        return self._json


class RJSONRoute(APIRoute):
    """Route class that hands FastAPI an :class:`RJSONRequest`.

    FastAPI reads JSON bodies through ``await request.json()`` and turns
    ``json.JSONDecodeError`` into a 422 ``json_invalid`` error, so body models and
    validation errors work unchanged. Use it with ``APIRouter(route_class=RJSONRoute)``.
    """

    def get_route_handler(self) -> Callable[[Request], Any]:
        """Wrap FastAPI's handler so it receives an :class:`RJSONRequest`."""
        handler = super().get_route_handler()

        async def rjson_route_handler(request: Request) -> Response:
            return await handler(RJSONRequest(request.scope, request.receive))

        return rjson_route_handler


_JSON_TYPES = ("application/json",)


async def json_body(request: Request) -> Any:
    """Dependency returning the request body parsed with rjson.

    Raises:
        HTTPException: 415 if the content type is not JSON, 400 if the body is empty or
            not valid JSON (the detail carries the message, line, column and position).
    """
    content_type = request.headers.get("content-type", "")
    media_type = content_type.split(";", 1)[0].strip().lower()
    if not (media_type in _JSON_TYPES or media_type.endswith("+json")):
        raise HTTPException(415, detail=f"expected application/json, got {content_type!r}")
    body = await request.body()
    try:
        return rjson.loads(body)
    except json.JSONDecodeError as exc:
        raise HTTPException(
            400,
            detail={
                "error": "invalid_json",
                "message": exc.msg,
                "line": exc.lineno,
                "column": exc.colno,
                "position": exc.pos,
            },
        ) from exc


# -- the app --------------------------------------------------------------------------


class ItemIn(BaseModel):
    """Request body for ``POST /items``."""

    name: str
    price: decimal.Decimal
    tags: list[str] = []


class ItemOut(ItemIn):
    """Response model for ``POST /items``."""

    id: uuid.UUID
    created: dt.datetime


router = APIRouter(route_class=RJSONRoute)
_ITEMS: list[dict[str, Any]] = [
    {"id": i, "name": f"item {i}", "price": i * 1.25, "tags": ["demo"]} for i in range(3)
]


@router.get("/items")
async def list_items() -> RJSONResponse:
    """Native data: return RJSONResponse directly, skipping jsonable_encoder."""
    return RJSONResponse({"items": _ITEMS, "count": len(_ITEMS)})


@router.post("/items", status_code=201)
async def create_item(item: ItemIn) -> ItemOut:
    """Body parsed by rjson (RJSONRoute); response serialized by Pydantic's dump_json."""
    return ItemOut(**item.model_dump(), id=uuid.uuid4(), created=dt.datetime.now(dt.timezone.utc))


@router.post("/events", response_class=RJSONResponse)
async def ingest_event(event: Annotated[Any, Depends(json_body)]) -> RJSONResponse:
    """Arbitrary JSON; the response holds a datetime, so render() takes the fallback."""
    if not isinstance(event, dict):
        raise HTTPException(422, detail="event must be a JSON object")
    return RJSONResponse(
        {"accepted": event, "received_at": dt.datetime.now(dt.timezone.utc)}, status_code=202
    )


app = FastAPI(title="rjson example")
app.include_router(router)


_JSON_HEADERS = {"content-type": "application/json"}


def _demo() -> None:
    try:
        from fastapi.testclient import TestClient
    except (ImportError, RuntimeError) as exc:  # Starlette raises RuntimeError without httpx
        raise SystemExit(f"the demo needs httpx: {exc}") from exc

    client = TestClient(app)
    calls: list[tuple[str, str, dict[str, Any]]] = [
        ("GET", "/items", {}),
        ("POST", "/items", {"json": {"name": "pen", "price": "1.10", "tags": ["x"]}}),
        ("POST", "/items", {"content": b'{"name": "pen",', "headers": _JSON_HEADERS}),
        (
            "POST",
            "/events",
            {
                "content": b'{"type": "click", "big": 12345678901234567890}',
                "headers": _JSON_HEADERS,
            },
        ),
        ("POST", "/events", {"content": b'{"type": ', "headers": _JSON_HEADERS}),
        ("POST", "/events", {"content": b"type=click", "headers": {"content-type": "text/plain"}}),
    ]
    for method, url, kwargs in calls:
        response = client.request(method, url, **kwargs)
        print(f"{method} {url} -> {response.status_code} {response.headers['content-type']}")
        print(f"    {response.text}")


if __name__ == "__main__":
    _demo()
