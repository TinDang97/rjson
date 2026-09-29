"""Command-line JSON beautifier and validator built on rjson.

    rjson [options] [infile] [outfile]      pretty-print (or minify) one document
    rjson --in-place [options] FILE...      reformat files in place
    rjson --check [options] FILE...         exit 1 if a file is not formatted
    rjson --validate FILE...                exit 1 if a file is not valid JSON

Also ``python -m rjson`` and ``python -m rjson.tool``. A drop-in, faster
``python -m json.tool``: the same options and the same output, except that
floats below 1e-4 are written in shortest form (``1e-7``; json writes
``1e-07``, the same value). Like json.tool it accepts NaN/Infinity (use
``--strict`` to reject them).
"""

from __future__ import annotations

import argparse
import os
import re  # already imported by json, which rjson imports
import sys

import rjson

# Startup time matters for a command line tool: typing, shutil and tempfile
# (~11 ms together) are imported only for type checkers or when needed.
TYPE_CHECKING = False
if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator
    from typing import IO, Any, NoReturn

__all__ = ["beautify", "main"]

# Output is valid JSON, so a loose pattern finds its tokens (as json.tool).
_TOKENS = re.compile(
    r"""
    (?P<key>"(?:\\.|[^"\\])*")(?=\s*:)      |
    (?P<string>"(?:\\.|[^"\\])*")           |
    (?P<number>NaN|-?Infinity|[0-9\-+.Ee]+) |
    (?P<keyword>true|false|null)
    """,
    re.VERBOSE,
)

# CPython 3.14's json.tool theme (keys bold, strings green, numbers yellow,
# true/false/null bold blue); on 3.14 the user's _colorize theme is used.
_DEFAULT_THEME = {
    "key": "\x1b[1m",
    "string": "\x1b[32m",
    "number": "\x1b[33m",
    "keyword": "\x1b[1;34m",
    "reset": "\x1b[0m",
}


def beautify(
    data: str | bytes | bytearray | memoryview,
    *,
    indent: int | str | None = 4,
    sort_keys: bool = False,
    ensure_ascii: bool = True,
    compact: bool = False,
    json_lines: bool = False,
    strict: bool = False,
) -> bytes:
    """Reformat JSON text; returns UTF-8 bytes ending with a newline.

    The defaults give ``python -m json.tool``'s output. ``compact=True``
    minifies; ``indent=None`` puts each document on one line with spaces after
    ``,`` and ``:``. ``json_lines=True`` treats each non-blank line as a
    document. ``strict=True`` rejects NaN/Infinity (read and written).

    Raises:
        ValueError: the input is not valid JSON (a ``json.JSONDecodeError``,
            which for JSON Lines names the line) or cannot be written.
    """
    opts = _dump_opts(indent, sort_keys, ensure_ascii, compact, strict)
    if isinstance(data, str):
        data = data.encode("utf-8", "surrogatepass")
    docs = _all_lines(bytes(data), not strict) if json_lines else [_load(data, not strict)]
    return b"".join(rjson.dumps(doc, **opts) + b"\n" for doc in docs)


def _dump_opts(
    indent: int | str | None, sort_keys: bool, ensure_ascii: bool, compact: bool, strict: bool
) -> dict[str, Any]:
    """json.tool's json.dumps arguments, spelled for rjson.dumps."""
    opts: dict[str, Any] = {
        "sort_keys": sort_keys,
        "ensure_ascii": ensure_ascii,
        "allow_nan": not strict,
    }
    if compact:
        return opts  # rjson's default layout: no whitespace at all
    if indent is None:
        opts["separators"] = (", ", ": ")  # json.dumps's one-line default
    elif isinstance(indent, int):
        # json.dumps takes any int (negative: newlines only); rjson's int
        # indent stops at 1024, a str unit does not.
        opts["indent"] = indent if 0 <= indent <= 1024 else " " * max(indent, 0)
    else:
        opts["indent"] = indent
    return opts


def _load(data: bytes | bytearray | memoryview, lenient: bool) -> Any:
    try:
        return rjson.loads(data, lenient=lenient)
    except RecursionError as exc:
        raise ValueError(f"document nested too deep: {exc}") from None


def _lines(lines: Iterable[bytes], lenient: bool, first: int = 1) -> Iterator[Any]:
    """Documents of JSON Lines input; blank lines are skipped. Errors name
    the line of the input (``first`` is the number of the first one), not of
    the document."""
    for lineno, line in enumerate(lines, first):
        if not line.strip():
            continue
        try:
            yield _load(line, lenient)
        except rjson.JSONDecodeError as exc:
            raise rjson.JSONDecodeError(
                f"{exc.msg} (JSON Lines input, line {lineno})", exc.doc, exc.pos
            ) from None


def _all_lines(data: bytes, lenient: bool) -> Iterator[Any]:
    """Documents of JSON Lines input already in memory, parsed by
    ``rjson.loads_ndjson`` in chunks of about ``_CHUNK`` bytes (cut after a
    newline, so documents are held one chunk at a time). Only when it splits
    the input as ``bytes.splitlines()`` does: lines end with ``\n`` or
    ``\r\n`` (no lone ``\r``) and blank lines hold only spaces and tabs (no
    ``\x0b``/``\x0c``). Otherwise, and from a chunk with any error on, the
    lines go through ``_lines``, whose per-line parse names the bad line after
    yielding the documents before it (json.tool prints those first)."""
    if b"\x0b" in data or b"\x0c" in data or data.count(b"\r") != data.count(b"\r\n"):
        yield from _lines(data.splitlines(), lenient)
        return
    view = memoryview(data)
    start = 0
    while start < len(data):
        end = data.find(b"\n", start + _CHUNK)
        end = len(data) if end < 0 else end + 1
        try:
            docs = rjson.loads_ndjson(view[start:end], lenient=lenient)
        except (ValueError, RecursionError):
            yield from _lines(data[start:].splitlines(), lenient, data.count(b"\n", 0, start) + 1)
            return
        yield from docs
        start = end


_CHUNK = 16 << 10  # small: documents of one chunk stay in cache (1 MiB chunks were 40% slower)


def _stream_lines(stream: IO[bytes], lenient: bool) -> Iterator[Any]:
    """Documents of JSON Lines read from a stream (stdin). Takes whatever
    input is available (``read1``, up to ``_CHUNK`` bytes, so documents are
    written as their lines arrive) and parses its complete lines with one
    ``rjson.loads_ndjson`` call. Same documents and errors as ``_lines`` over
    the stream's lines (split at ``\n`` only), which parses any block with an
    error, or with ``\x0b``/``\x0c`` (blank for ``bytes.strip``), line by line."""
    read1 = getattr(stream, "read1", None)
    if read1 is None:
        yield from _lines(stream, lenient)
        return
    rest = b""
    lineno = 1
    while True:
        chunk = read1(_CHUNK)
        data = rest + chunk if rest else chunk
        cut = len(data) if not chunk else data.rfind(b"\n") + 1
        if cut:
            block, rest = data[:cut], data[cut:]
            yield from _block_lines(block, lenient, lineno)
            lineno += block.count(b"\n")
        else:
            rest = data
        if not chunk:
            return


def _block_lines(block: bytes, lenient: bool, first: int) -> Iterator[Any]:
    """``_stream_lines``: the documents of one block of complete lines."""
    if b"\x0b" not in block and b"\x0c" not in block:
        try:
            docs = rjson.loads_ndjson(block, lenient=lenient)
        except (ValueError, RecursionError):
            pass
        else:
            yield from docs
            return
    import io

    yield from _lines(io.BytesIO(block), lenient, first)


def _can_color(stream: IO[Any], when: str) -> bool:
    """``--color``: auto follows CPython's rules (PYTHON_COLORS, NO_COLOR,
    FORCE_COLOR, TERM=dumb, a terminal)."""
    if when != "auto":
        return when == "always"
    if not sys.flags.ignore_environment:
        if os.environ.get("PYTHON_COLORS") == "0":
            return False
        if os.environ.get("PYTHON_COLORS") == "1":
            return True
    if os.environ.get("NO_COLOR"):
        return False
    if os.environ.get("FORCE_COLOR"):
        return True
    if os.environ.get("TERM") == "dumb":
        return False
    try:
        return os.isatty(stream.fileno())
    except (AttributeError, OSError, ValueError):
        return False


def _theme() -> dict[str, str]:
    try:
        # CPython 3.14+ (private module, so looked up dynamically).
        t = __import__("_colorize").get_theme(force_color=True).syntax
        return {
            "key": t.definition,
            "string": t.string,
            "number": t.number,
            "keyword": t.keyword,
            "reset": t.reset,
        }
    except (ImportError, AttributeError, TypeError):
        return _DEFAULT_THEME


def _colorize(text: str, theme: dict[str, str]) -> str:
    def paint(m: re.Match[str]) -> str:
        group = m.lastgroup or ""
        return f"{theme[group]}{m.group()}{theme['reset']}"

    return _TOKENS.sub(paint, text)


def _prog() -> str:
    arg0 = os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else ""
    if arg0 == "__main__.py":
        return "python -m rjson"
    if arg0 == "tool.py":
        return "python -m rjson.tool"
    return "rjson"


def _parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog=_prog(),
        usage="%(prog)s [options] [infile] [outfile]\n"
        "       %(prog)s (--in-place | --check | --validate) [options] FILE...",
        description="Validate, beautify (pretty-print), minify and reformat JSON with "
        "rjson. A faster drop-in for 'python -m json.tool': the same options and output.",
        epilog="infile/outfile default to stdin/stdout ('-' also means stdin/stdout). "
        "Exit status: 0 on success, 1 on invalid JSON or (--check) unformatted files.",
    )
    p.add_argument("files", nargs="*", metavar="infile/outfile/FILE", help=argparse.SUPPRESS)
    p.add_argument("--version", action="version", version=f"rjson {rjson.__version__}")
    p.add_argument("--sort-keys", action="store_true",
                   help="sort the output of dictionaries alphabetically by key")
    p.add_argument("--no-ensure-ascii", dest="ensure_ascii", action="store_false",
                   help="write non-ASCII characters as they are, not as \\uXXXX escapes")
    p.add_argument("--json-lines", "--jsonl", "--ndjson", dest="json_lines", action="store_true",
                   help="read JSON Lines (one document per line; blank lines skipped). "
                   "Use with --no-indent or --compact to write valid JSON Lines.")
    layout = p.add_mutually_exclusive_group()
    layout.add_argument("--indent", default=4, type=int, metavar="N",
                        help="separate items with newlines, indent by N spaces (default: 4)")
    layout.add_argument("--tab", action="store_const", dest="indent", const="\t",
                        help="separate items with newlines, indent with tabs")
    layout.add_argument("--no-indent", action="store_const", dest="indent", const=None,
                        help="one line per document, with spaces after ',' and ':'")
    layout.add_argument("--compact", "--minify", dest="compact", action="store_true",
                        help="suppress all whitespace (most compact)")
    p.add_argument("--strict", action="store_true",
                   help="reject NaN/Infinity and a UTF-8 BOM (json.tool accepts NaN/Infinity)")
    p.add_argument("--color", choices=("auto", "always", "never"), default="auto",
                   help="syntax colors on stdout (default: auto, on a terminal; "
                   "honors NO_COLOR, FORCE_COLOR, PYTHON_COLORS)")
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("-i", "--in-place", action="store_true",
                      help="reformat each FILE in place (written only if it changes)")
    mode.add_argument("--check", action="store_true",
                      help="report each FILE that is invalid or not formatted as the options "
                      "say, exit 1 if any; write nothing")
    mode.add_argument("--validate", action="store_true",
                      help="only check that each FILE is valid JSON, exit 1 if not")
    return p


def _read(path: str) -> bytes:
    if path == "-":
        return sys.stdin.buffer.read()
    with open(path, "rb") as f:
        return f.read()


def _error(path: str | None, exc: BaseException) -> str:
    msg = str(exc) or type(exc).__name__
    return f"{path}: {msg}" if path else msg


def _write_atomic(path: str, data: bytes) -> None:
    """Replace `path` with `data` (same directory, permissions kept), so an
    interrupted write never leaves a truncated file."""
    import shutil
    import tempfile

    directory = os.path.dirname(os.path.abspath(path))
    fd, tmp = tempfile.mkstemp(dir=directory, prefix=".rjson-", suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
        shutil.copymode(path, tmp)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def _newlines(out: bytes, newline: bytes) -> bytes:
    """`out` with `newline` line ends. Layout newlines are the only raw
    newlines in rjson's output (strings escape theirs)."""
    return out if newline == b"\n" else out.replace(b"\n", newline)


# json.tool writes text streams, so its line ends are os.linesep (CRLF on
# Windows) on stdout and in outfile; the same here.
_LINESEP = os.linesep.encode()


def _files_mode(args: argparse.Namespace, opts: dict[str, Any]) -> int:
    """--in-place / --check / --validate over several files."""
    status = 0
    paths = args.files or ["-"]
    lenient = not args.strict
    for path in paths:
        name = "<stdin>" if path == "-" else path
        try:
            data = _read(path)
            docs = _all_lines(data, lenient) if args.json_lines else [_load(data, lenient)]
            if args.validate:
                for _ in docs:
                    pass
                continue
            out = b"".join(rjson.dumps(doc, **opts) + b"\n" for doc in docs)
            # A file keeps its line ends (CRLF stays CRLF).
            out = _newlines(out, b"\r\n" if b"\r\n" in data else b"\n")
        except (OSError, ValueError) as exc:
            print(_error(name, exc), file=sys.stderr)
            status = 1
            continue
        if out == data:
            continue
        if args.check:
            print(f"would reformat {name}", file=sys.stderr)
            status = 1
        else:  # --in-place (never stdin, see main)
            try:
                _write_atomic(path, out)
            except OSError as exc:
                print(_error(name, exc), file=sys.stderr)
                status = 1
                continue
            print(f"reformatted {name}", file=sys.stderr)
    return status


def _out_stream() -> Any:
    return getattr(sys.stdout, "buffer", None)


def main(argv: list[str] | None = None) -> int:
    """Run the command line; returns the exit status."""
    parser = _parser()
    args = parser.parse_args(argv)
    opts = _dump_opts(args.indent, args.sort_keys, args.ensure_ascii, args.compact, args.strict)
    if args.in_place or args.check or args.validate:
        if args.in_place and "-" in args.files:
            parser.error("--in-place needs file names, not stdin")
        return _files_mode(args, opts)
    if len(args.files) > 2:
        parser.error("at most infile and outfile (use --in-place, --check or --validate "
                     "for several files)")
    infile = args.files[0] if args.files else "-"
    outfile = args.files[1] if len(args.files) > 1 else "-"
    lenient = not args.strict

    try:
        if args.json_lines and infile == "-" and outfile == "-":
            docs: Iterable[Any] = _stream_lines(sys.stdin.buffer, lenient)
        else:
            data = _read(infile)
            docs = _all_lines(data, lenient) if args.json_lines else [_load(data, lenient)]
        if outfile != "-":
            # Everything is read already, so outfile may be infile (as json.tool).
            out = b"".join(rjson.dumps(doc, **opts) + b"\n" for doc in docs)
            with open(outfile, "wb") as f:
                out = _newlines(out, _LINESEP)
                f.write(out)
            return 0
        if _can_color(sys.stdout, args.color):
            theme = _theme()
            for doc in docs:
                text = rjson.dumps(doc, **opts).decode("utf-8")
                sys.stdout.write(_colorize(text, theme) + "\n")
            return 0
        stream = _out_stream()
        for doc in docs:
            out = rjson.dumps(doc, **opts)
            if stream is None:  # stdout replaced by a text-only stream
                sys.stdout.write(out.decode("utf-8") + "\n")
            else:
                stream.write(_newlines(out, _LINESEP))
                stream.write(_LINESEP)
    except BrokenPipeError:
        raise
    except (OSError, ValueError) as exc:
        # json.tool's behavior: the message on stderr, exit status 1.
        raise SystemExit(_error(None, exc)) from None
    return 0


def run() -> NoReturn:
    """Console entry point with json.tool's broken-pipe handling (``rjson ... | head``)."""
    try:
        status = main()
        sys.stdout.flush()
    except BrokenPipeError as exc:
        # Python flushes stdout again at exit; point it at devnull so that
        # does not print a second error.
        try:
            devnull = os.open(os.devnull, os.O_WRONLY)
            os.dup2(devnull, sys.stdout.fileno())
        except (OSError, ValueError):
            pass
        raise SystemExit(exc.errno) from None
    raise SystemExit(status)


if __name__ == "__main__":
    run()
