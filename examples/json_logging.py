"""Structured JSON logging and NDJSON files with rjson.

* :class:`JSONFormatter` turns each ``logging.LogRecord`` into one JSON object per line
  (timestamp, level, logger, message, exception, ``extra=`` fields). It never raises:
  values rjson cannot encode (datetime, UUID, NaN, arbitrary objects, non-str keys) are
  converted to strings on a fallback path that only runs when the fast path fails.
* :func:`write_ndjson` / :func:`read_ndjson` stream newline-delimited JSON to and from a
  file object, skipping blank lines and reporting bad lines with their line number.

Run ``python examples/json_logging.py`` for a demo.
"""

from __future__ import annotations

import dataclasses
import datetime as dt
import decimal
import enum
import io
import json
import logging
import re
import sys
import time
import uuid
from collections.abc import Callable, Iterable, Iterator, Mapping
from typing import IO, Any, Literal

import rjson

__all__ = ["JSONFormatter", "NDJSONError", "read_ndjson", "setup_json_logging", "write_ndjson"]

log = logging.getLogger(__name__)

#: Attributes every LogRecord has (``taskName`` exists on 3.12+); the rest came from ``extra=``.
_RESERVED = frozenset(vars(logging.LogRecord("", 0, "", 0, "", (), None))) | {
    "message",
    "asctime",
}
_CORE_FIELDS = ("ts", "level", "logger", "message")
_MAX_DEPTH = 16
# rjson.dumps_str passes lone surrogates through (rjson.dumps raises instead). A log line
# holding one cannot be written to a UTF-8 stream, so they are replaced by U+FFFD.
_SURROGATES = re.compile("[\ud800-\udfff]")


class JSONFormatter(logging.Formatter):
    """Format log records as single-line JSON objects.

    Output keys, in order: ``ts`` (ISO 8601 UTC, milliseconds), ``level``, ``logger``,
    ``message``, then optionally ``module``/``func``/``line``, ``exc_info``,
    ``stack_info``, the ``static_fields`` and the record's ``extra=`` fields. An extra
    field whose name collides with an output key is emitted as ``extra_<name>``.

    Args:
        static_fields: Constant fields added to every line (service name, env, version);
            names that collide with the core keys are ignored.
        include_location: Add ``module``, ``func`` and ``line``.

    Example:
        >>> handler = logging.StreamHandler()
        >>> handler.setFormatter(JSONFormatter(static_fields={"service": "api"}))
    """

    def __init__(
        self,
        *,
        static_fields: Mapping[str, Any] | None = None,
        include_location: bool = False,
    ) -> None:
        super().__init__()
        self.static_fields = dict(static_fields or {})
        self.include_location = include_location

    def format(self, record: logging.LogRecord) -> str:
        """Return ``record`` as one line of JSON (without the trailing newline)."""
        payload: dict[str, Any] = {
            "ts": _utc_iso(record.created),
            "level": record.levelname,
            "logger": record.name,
            "message": _message(record),
        }
        if self.include_location:
            payload["module"] = record.module
            payload["func"] = record.funcName
            payload["line"] = record.lineno
        if record.exc_info and not record.exc_text:
            # Cache like logging.Formatter does: several handlers may format the record.
            record.exc_text = self.formatException(record.exc_info)
        if record.exc_text:
            payload["exc_info"] = record.exc_text
        if record.stack_info:
            payload["stack_info"] = self.formatStack(record.stack_info)
        for key, value in self.static_fields.items():
            payload.setdefault(key, value)
        for key, value in record.__dict__.items():
            if key not in _RESERVED:
                payload[f"extra_{key}" if key in payload else key] = value
        return _to_json_line(payload)


#: (whole second, "YYYY-MM-DDTHH:MM:SS") of the last record: records mostly arrive within
#: the same second, and formatting the date costs about 1 us. One tuple, so that
#: concurrent threads never see a second paired with another second's prefix.
_last_second: tuple[int, str] = (-1, "")


def _utc_iso(created: float) -> str:
    """Format a ``time.time()`` value as ``2024-01-02T03:04:05.678Z``."""
    global _last_second
    second = int(created // 1)  # floor, also for (unlikely) pre-1970 times
    cached_second, prefix = _last_second
    if second != cached_second:
        prefix = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(second))
        _last_second = (second, prefix)
    return f"{prefix}.{int((created - second) * 1000):03d}Z"


def _message(record: logging.LogRecord) -> str:
    try:
        return record.getMessage()
    except Exception:  # bad %-args: logging would print a traceback and drop the line.
        return f"{_safe_repr(record.msg)} % {_safe_repr(record.args)} (message formatting failed)"


def _to_json_line(payload: dict[str, Any]) -> str:
    try:
        text = rjson.dumps_str(payload)
    except (TypeError, ValueError):  # unsupported type, NaN, non-str key, too deep
        try:
            text = rjson.dumps_str(_jsonable(payload, 0))
        except (TypeError, ValueError) as exc:  # not expected; logging must not raise
            text = rjson.dumps_str(
                {key: _safe_repr(payload.get(key)) for key in _CORE_FIELDS}
                | {"formatter_error": _safe_repr(exc)}
            )
    if not text.isascii():  # O(1) in CPython: ASCII-only lines skip the scan
        text = _SURROGATES.sub("\ufffd", text)
    return text


def _jsonable(value: Any, depth: int) -> Any:
    """Return a copy of ``value`` that rjson can encode, stringifying what it cannot."""
    if depth > _MAX_DEPTH:
        return "<max depth exceeded>"
    if value is None or isinstance(value, (str, bool)):
        return value
    if isinstance(value, enum.Enum):
        return _jsonable(value.value, depth + 1)
    if isinstance(value, int):
        return int(value)
    if isinstance(value, float):
        return value if value - value == 0 else repr(value)  # NaN/inf -> "nan"/"inf"
    if isinstance(value, Mapping):
        return {
            k if isinstance(k, str) else _safe_str(k): _jsonable(v, depth + 1)
            for k, v in value.items()
        }
    if isinstance(value, (list, tuple, set, frozenset)):
        return [_jsonable(v, depth + 1) for v in value]
    if isinstance(value, (dt.datetime, dt.date, dt.time)):
        return value.isoformat()
    if isinstance(value, (uuid.UUID, decimal.Decimal, dt.timedelta)):
        return str(value)
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {
            f.name: _jsonable(getattr(value, f.name, None), depth + 1)
            for f in dataclasses.fields(value)
        }
    return _safe_repr(value)


def _safe_str(value: Any) -> str:
    try:
        return str(value)
    except Exception:
        return f"<unprintable {type(value).__qualname__}>"


def _safe_repr(value: Any) -> str:
    try:
        return repr(value)
    except Exception:
        return f"<unprintable {type(value).__qualname__}>"


def setup_json_logging(
    level: int = logging.INFO,
    stream: IO[str] | None = None,
    **formatter_kwargs: Any,
) -> logging.Handler:
    """Send the root logger's records to ``stream`` (default stderr) as JSON lines.

    Args:
        level: Root logger level.
        stream: Text stream to write to.
        **formatter_kwargs: Passed to :class:`JSONFormatter`.

    Returns:
        The installed handler (remove it with ``logging.getLogger().removeHandler``).
    """
    handler = logging.StreamHandler(stream if stream is not None else sys.stderr)
    handler.setFormatter(JSONFormatter(**formatter_kwargs))
    root = logging.getLogger()
    root.addHandler(handler)
    root.setLevel(level)
    return handler


# -- NDJSON ---------------------------------------------------------------------------


class NDJSONError(ValueError):
    """A line could not be encoded or decoded.

    Attributes:
        lineno: 1-based line number in the file.
    """

    def __init__(self, lineno: int, reason: str) -> None:
        super().__init__(f"line {lineno}: {reason}")
        self.lineno = lineno


def write_ndjson(fp: IO[bytes] | IO[str], records: Iterable[Any]) -> int:
    """Write each record as one compact JSON line.

    rjson escapes control characters, so a record never spans lines. Binary files get
    UTF-8 bytes; text files get ``str`` (open them with ``encoding="utf-8"``).

    Args:
        fp: File object opened for writing, binary (preferred) or text.
        records: JSON-serializable objects.

    Returns:
        The number of lines written.

    Raises:
        NDJSONError: A record cannot be serialized. Earlier lines stay written; nothing
            of the failing record is.
    """
    text = isinstance(fp, io.TextIOBase)
    write: Callable[[Any], object] = fp.write
    count = 0
    for count, record in enumerate(records, 1):
        try:
            line: str | bytes
            if text:
                line = text_line = rjson.dumps_str(record) + "\n"
                if not text_line.isascii():  # dumps_str passes lone surrogates through; fail
                    text_line.encode("utf-8")  # here (UnicodeEncodeError) like rjson.dumps
            else:
                line = rjson.dumps(record) + b"\n"
        except (TypeError, ValueError) as exc:  # UnicodeEncodeError is a ValueError
            raise NDJSONError(count, f"cannot serialize record: {exc}") from exc
        write(line)
    return count


def read_ndjson(
    fp: Iterable[bytes | str],
    *,
    on_error: Literal["raise", "skip"] = "raise",
) -> Iterator[Any]:
    """Yield one object per non-blank line of ``fp``.

    Open files in binary mode: invalid UTF-8 is then reported for the offending line
    (as ``NDJSONError``) instead of aborting iteration with ``UnicodeDecodeError``.
    Lines may end with LF or CRLF. Each line is fully buffered, so cap line
    length upstream when reading untrusted input.

    Args:
        fp: Binary or text file object, or any iterable of lines.
        on_error: ``"raise"`` stops at the first bad line; ``"skip"`` logs a warning with
            the line number and continues.

    Yields:
        The decoded JSON value of each line.

    Raises:
        NDJSONError: A line is not valid JSON and ``on_error="raise"``.
    """
    if on_error not in ("raise", "skip"):
        raise ValueError(f"on_error must be 'raise' or 'skip', not {on_error!r}")
    for lineno, line in enumerate(fp, 1):
        if not line or line.isspace():
            continue
        try:
            yield rjson.loads(line)
        except json.JSONDecodeError as exc:
            error = NDJSONError(lineno, f"invalid JSON at column {exc.colno}: {exc.msg}")
            if on_error == "raise":
                raise error from exc
            log.warning("skipping %s", error)


# -- demo -----------------------------------------------------------------------------


class _Opaque:
    def __repr__(self) -> str:
        return "<Opaque>"


def _demo() -> None:
    handler = setup_json_logging(stream=sys.stdout, static_fields={"service": "demo"})
    demo_log = logging.getLogger("demo")
    demo_log.info(
        "user %s logged in",
        "ada",
        extra={
            "user_id": uuid.UUID(int=42),
            "at": dt.datetime(2024, 1, 1),
            "ratio": float("nan"),
            "obj": _Opaque(),
            "level": "shadowed",
        },
    )
    try:
        1 / 0  # noqa: B018
    except ZeroDivisionError:
        demo_log.exception("division failed", extra={"request_id": "r-1"})
    logging.getLogger().removeHandler(handler)

    buf = io.BytesIO()
    written = write_ndjson(buf, [{"n": i, "text": "line\nbreak é"} for i in range(3)])
    print(f"wrote {written} lines: {buf.getvalue()!r}")
    buf = io.BytesIO(buf.getvalue() + b"\n  \n{not json}\n" + b'{"n": 3}\r\n')
    print("skip mode:", list(read_ndjson(buf, on_error="skip")))
    buf.seek(0)
    try:
        list(read_ndjson(buf))
    except NDJSONError as exc:
        print("raise mode:", exc)


if __name__ == "__main__":
    _demo()
