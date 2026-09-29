"""
rjson.loads_ndjson: newline-delimited JSON (NDJSON / JSON Lines) in one call.

The contract: the result is `[loads(line) for line in lines if line is not
blank]` for lines split at "\n" (a "\r" before it is whitespace), and an
error is the one `loads(line)` raises, with its position moved into the
whole input. Everything here checks that against `rjson.loads` itself.
"""

import gc
import io
import json
import random
import sys

import pytest

import rjson
import rjson.tool as tool


def per_line(data, lenient=False):
    """The reference: rjson.loads on each non-blank line."""
    nl = "\n" if isinstance(data, str) else b"\n"
    ws = " \t\r" if isinstance(data, str) else b" \t\r"
    return [rjson.loads(line, lenient=lenient) for line in data.split(nl) if line.strip(ws)]


def line_error(data, lenient=False):
    """(msg, absolute pos) of the error loads(line) raises for the first bad
    line, as loads_ndjson must report it; pos counts characters of `data`."""
    text = data if isinstance(data, str) else data.decode("utf-8", "replace")
    raw = data if isinstance(data, str) else data
    nl = "\n" if isinstance(data, str) else b"\n"
    offset = 0
    for line, tline in zip(raw.split(nl), text.split("\n")):
        try:
            rjson.loads(line, lenient=lenient)
        except rjson.JSONDecodeError as exc:
            if line.strip(" \t\r" if isinstance(line, str) else b" \t\r"):
                return exc.msg, offset + exc.pos
        offset += len(tline) + 1
    return None


def random_value(rng, depth=0):
    kind = rng.randrange(9 if depth < 3 else 6)
    if kind == 0:
        return None
    if kind == 1:
        return rng.random() < 0.5
    if kind == 2:
        return rng.randint(-(10**20), 10**20)
    if kind == 3:
        return rng.uniform(-1e6, 1e6)
    if kind in (4, 5):
        return "".join(rng.choice("ab é€😀\"\\\n\t/") for _ in range(rng.randint(0, 8)))
    if kind in (6, 7):
        keys = ["id", "name", "tags", "é", "k%d" % rng.randint(0, 5)]
        return {rng.choice(keys): random_value(rng, depth + 1) for _ in range(rng.randint(0, 5))}
    return [random_value(rng, depth + 1) for _ in range(rng.randint(0, 5))]


def random_ndjson(rng, n):
    parts = []
    for _ in range(n):
        r = rng.random()
        if r < 0.05:
            parts.append(rng.choice(["", " ", "\t", "\r", "  \t "]))  # blank line
        else:
            doc = json.dumps(random_value(rng), ensure_ascii=rng.random() < 0.3,
                             separators=rng.choice([(",", ":"), (", ", ": ")]))
            parts.append(rng.choice(["", " ", "\t"]) + doc + rng.choice(["", " ", "\r", " \r"]))
    return "\n".join(parts) + rng.choice(["", "\n", "\r\n", "\n\n"])


@pytest.mark.parametrize("seed", range(40))
def test_matches_loads_per_line(seed):
    rng = random.Random(seed)
    text = random_ndjson(rng, rng.randint(0, 60))
    expected = per_line(text)
    for data in (text, text.encode(), bytearray(text.encode()), memoryview(text.encode())):
        assert rjson.loads_ndjson(data) == expected


def test_basics():
    assert rjson.loads_ndjson(b'{"a":1}\n[2]\r\n"x"\n3') == [{"a": 1}, [2], "x", 3]
    assert rjson.loads_ndjson(b'1\n2\n') == [1, 2]
    for empty in ("", b"", "\n", " \n\t\r\n", b"\r\n\r\n"):
        assert rjson.loads_ndjson(empty) == []
    assert rjson.loads_ndjson("  1  \n\n\n 2 ") == [1, 2]
    assert type(rjson.loads_ndjson(b"1")) is list


def test_input_types():
    data = b'{"a":1}\n{"a":2}\n'
    want = [{"a": 1}, {"a": 2}]
    assert rjson.loads_ndjson(bytearray(data)) == want
    assert rjson.loads_ndjson(memoryview(data)) == want
    assert rjson.loads_ndjson(memoryview(data)[:8]) == want[:1]  # copied (not NUL-terminated)
    big = data * 1000  # >= 4 KiB: memoryview parsed in place
    assert rjson.loads_ndjson(memoryview(big)) == want * 1000
    strided = memoryview(bytes(b for c in b"1\n2\n3" for b in (c, 0x5F)))[::2]  # b"1\n2\n3"
    assert rjson.loads_ndjson(strided) == [1, 2, 3]
    with pytest.raises(TypeError):
        rjson.loads_ndjson(1)
    with pytest.raises(TypeError, match="loads_ndjson"):
        rjson.loads_ndjson(b"1", b"2")
    with pytest.raises(TypeError, match="unexpected keyword argument 'x'"):
        rjson.loads_ndjson(b"1", x=1)


@pytest.mark.parametrize(
    "data",
    [
        b'{"a":1}\n{"a":\n2}\n',  # a value spanning two lines
        b'{"a":\n1}',
        b"[1,\n2]",
        b"1\n2 3\n",  # two values on a line
        b'{"a":1}{"b":2}',
        b"1\n[1,2\n",  # a line ending inside a value
        b'1\n"abc\n',  # a string running into the newline
        b"1\n\"a\nb\"\n",
        '1\n{"é":x}\n'.encode(),
        b"1\n{\"a\":\"\xff\"}",  # invalid UTF-8
        b"1\r2\n",  # a lone \r is whitespace, not a line end
        b"\x0c\n1",  # not JSON whitespace
        b"1\n,\n",
        b"1\n2\n]",
        b"\xef\xbb\xbf1\n2",  # BOM: only lenient mode skips it
        b"1\nNaN",
        b"1\n" + b"[" * 1025 + b"]" * 1025,  # depth limit
    ],
)
def test_errors_match_loads_per_line(data):
    for d in (data, data.decode("utf-8", "replace")):
        if isinstance(d, str) and "\ufffd" in d:
            continue
        with pytest.raises(rjson.JSONDecodeError) as info:
            rjson.loads_ndjson(d)
        exc = info.value
        assert (exc.msg, exc.pos) == line_error(d)
        # lineno/colno are those of the whole input.
        text = d if isinstance(d, str) else d.decode("utf-8", "replace")
        assert exc.lineno == text.count("\n", 0, exc.pos) + 1


def test_error_position_after_non_ascii_lines():
    data = '{"é":"€"}\n{"😀":1}\n{"x":1,}\n'
    for d in (data, data.encode()):
        with pytest.raises(rjson.JSONDecodeError) as info:
            rjson.loads_ndjson(d)
        assert info.value.lineno == 3
        assert info.value.colno == 7
        assert data[info.value.pos] == ","


def test_lenient():
    data = b"NaN\n-Infinity\n[Infinity, 1e999]"
    assert repr(rjson.loads_ndjson(data, lenient=True)) == repr(per_line(data, lenient=True))
    assert rjson.loads_ndjson(b'\xef\xbb\xbf{"a":1}\n2', lenient=True) == [{"a": 1}, 2]
    # Lone surrogates: in a str (json.loads fallback per line) and escaped.
    assert rjson.loads_ndjson('"\ud800"\n1\n', lenient=True) == ["\ud800", 1]
    assert rjson.loads_ndjson(b'1\n"\\udc00"\n', lenient=True) == [1, "\udc00"]
    with pytest.raises(rjson.JSONDecodeError):
        rjson.loads_ndjson('"\ud800"\n1')
    with pytest.raises(rjson.JSONDecodeError) as info:
        rjson.loads_ndjson('"\ud800"\n1\n{x}', lenient=True)
    assert info.value.lineno == 3
    # Beyond the depth limit: whatever loads(line, lenient=True) does (json's
    # fallback where it is safe, else the depth error).
    deep = b"[" * 1100 + b"]" * 1100
    try:
        want = [1, rjson.loads(deep, lenient=True)]
    except rjson.JSONDecodeError as exc:
        with pytest.raises(rjson.JSONDecodeError, match=exc.msg):
            rjson.loads_ndjson(b"1\n" + deep, lenient=True)
    else:
        assert rjson.loads_ndjson(b"1\n" + deep, lenient=True) == want


def test_bytearray_released_after_fallback():
    # The json.loads fallback runs Python code while the input is held; the
    # export is released afterwards, so the bytearray can be resized again.
    data = bytearray(b'1\n"\\udc00"\n2')
    assert rjson.loads_ndjson(data, lenient=True) == [1, "\udc00", 2]
    data.extend(b"\n3")
    assert rjson.loads_ndjson(data, lenient=True) == [1, "\udc00", 2, 3]


def test_many_lines_and_shapes():
    rng = random.Random(7)
    docs = [{"id": i, "v": rng.random(), "tags": ["a", "b"][: i % 3]} if i % 5 else [i, None] for i in range(20000)]
    data = "\n".join(json.dumps(d) for d in docs).encode()
    assert rjson.loads_ndjson(data) == docs


def test_gc_tracking_and_refcounts():
    data = b'{"a":[1]}\n{"a":2}\n[]\n"s"\n'
    got = rjson.loads_ndjson(data)
    assert [gc.is_tracked(x) for x in got] == [gc.is_tracked(rjson.loads(line)) for line in data.split()]

    # true/false/null are handled alike; True and False are checked because
    # None's refcount also moves with caches of the interpreter (it settles
    # after warm-up, but not deterministically).
    def calls():
        got = rjson.loads_ndjson(b"true\nfalse\n[true,false,null]\n")
        del got
        try:
            rjson.loads_ndjson(b"true\nfalse\n[true,false\n")  # the per-line path
        except rjson.JSONDecodeError:
            pass

    calls()
    before = sys.getrefcount(True), sys.getrefcount(False)
    for _ in range(200):
        calls()
    assert (sys.getrefcount(True), sys.getrefcount(False)) == before


# -- the command line (python/rjson/tool.py) uses it in chunks ---------------------


def collect(gen):
    """Documents produced before the error, and the error text."""
    docs = []
    try:
        for d in gen:
            docs.append(d)
    except ValueError as exc:
        return docs, str(exc)
    return docs, None


@pytest.mark.parametrize("chunk", [7, 64, 4096])
@pytest.mark.parametrize(
    "tail",
    [b"", b'{"a":\n7\n8', b"\n1 2\n3", b"\n\x0c\n5", b"\n1\r2\n", b"\n[" + b"[" * 1100 + b"]" * 1101],
)
def test_cli_chunks_match_per_line(monkeypatch, chunk, tail):
    monkeypatch.setattr(tool, "_CHUNK", chunk)
    body = b"".join(
        json.dumps({"i": i, "s": "é" * (i % 4)}).encode() + (b"\r\n" if i % 3 else b"\n") for i in range(60)
    )
    data = body + b"  \n" + tail
    # In memory: as _lines over bytes.splitlines().
    assert collect(tool._all_lines(data, True)) == collect(tool._lines(data.splitlines(), True))
    # Streamed: as _lines over the stream's lines.
    stream = io.BufferedReader(io.BytesIO(data))
    assert collect(tool._stream_lines(stream, True)) == collect(tool._lines(io.BytesIO(data), True))
