"""Differential fuzzing against the standard library ``json`` module.

Random documents, random objects and randomly corrupted inputs; every result
must match ``json`` (or both must reject the input), and rjson may only ever
raise its documented exceptions.

Scale with environment variables (CI runs the defaults; the sanitizer job and
long local runs raise them):

    RJSON_FUZZ_ITERS  cases per test (default 1000)
    RJSON_FUZZ_SEED   base seed (default 0); a failure message names its seed
"""

import json
import math
import os
import random
import string
import struct

import pytest
import rjson

ITERS = int(os.environ.get("RJSON_FUZZ_ITERS", "1000"))
SEED = int(os.environ.get("RJSON_FUZZ_SEED", "0"))
SEEDS = [SEED + i for i in range(4)]

ALPHABET = (
    string.ascii_letters + string.digits + " _-/\\\"'\t\n\r\b\f\x00\x01\x1f\x7f"
    + "éßøñ日本語한국어Ωж€" + "\U0001F600\U0001F680\U00010348"
)


def rand_str(rng, max_len=12):
    return "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, max_len)))


def rand_float(rng):
    k = rng.random()
    if k < 0.3:
        (f,) = struct.unpack("<d", rng.getrandbits(64).to_bytes(8, "little"))
        return f if math.isfinite(f) else 0.5
    if k < 0.6:
        return rng.uniform(-1e6, 1e6)
    return rng.choice([0.0, -0.0, 1e-7, 5e-324, 1.7976931348623157e308, 0.1, 1e16, 123.456])


def rand_int(rng):
    bits = rng.choice([7, 31, 53, 63, 64, 65, 128, 1000])
    return rng.getrandbits(bits) * rng.choice([1, -1])


def rand_value(rng, depth=0):
    k = rng.randrange(10 if depth < 6 else 6)
    if k == 0:
        return rand_str(rng)
    if k == 1:
        return rand_int(rng)
    if k == 2:
        return rand_float(rng)
    if k == 3:
        return rng.choice([None, True, False])
    if k in (4, 5):
        return rand_str(rng, 3)
    if k in (6, 7):
        return [rand_value(rng, depth + 1) for _ in range(rng.randint(0, 6))]
    return {rand_str(rng, 8): rand_value(rng, depth + 1) for _ in range(rng.randint(0, 6))}


def rand_text(rng, value):
    """A json.dumps rendering with random formatting."""
    return json.dumps(
        value,
        ensure_ascii=rng.random() < 0.5,
        indent=rng.choice([None, None, 0, 2, "\t"]),
        separators=rng.choice([None, (",", ":"), (" ,", " : ")]),
    )


def same(a, b):
    """Equality that also distinguishes 0.0/-0.0 and int/float/bool."""
    if type(a) is not type(b):
        return False
    if isinstance(a, float):
        if math.isnan(a) or math.isnan(b):
            return math.isnan(a) and math.isnan(b)
        return a == b and math.copysign(1, a) == math.copysign(1, b)
    if isinstance(a, list):
        return len(a) == len(b) and all(same(x, y) for x, y in zip(a, b))
    if isinstance(a, dict):
        return list(a) == list(b) and all(same(a[k], b[k]) for k in a)
    return a == b


@pytest.mark.parametrize("seed", SEEDS)
def test_loads_matches_json(seed):
    rng = random.Random(seed)
    for i in range(ITERS):
        value = rand_value(rng)
        text = rand_text(rng, value)
        want = json.loads(text)
        data = rng.choice([text, text.encode(), bytearray(text.encode()), memoryview(text.encode())])
        got = rjson.loads(data)
        assert same(got, want), f"seed={seed} case={i}: {text[:200]!r}"


@pytest.mark.parametrize("seed", SEEDS)
def test_dumps_round_trips_and_matches_json(seed):
    rng = random.Random(seed)
    for i in range(ITERS):
        value = rand_value(rng)
        out = rjson.dumps(value)
        assert type(out) is bytes
        assert same(json.loads(out), value), f"seed={seed} case={i}"
        assert same(rjson.loads(out), value), f"seed={seed} case={i}"
        assert rjson.dumps_str(value) == out.decode()
        assert rjson.dumps_bytes(value) == out
        # Byte-identical to json except float spelling (shortest repr vs
        # repr's exponent form, e.g. 1e-07 vs 1e-7): compare parsed forms.
        assert same(json.loads(json.dumps(value, ensure_ascii=False, separators=(",", ":"))),
                    json.loads(out))


def mutate(rng, data: bytes) -> bytes:
    b = bytearray(data)
    for _ in range(rng.randint(1, 4)):
        op = rng.randrange(6)
        pos = rng.randint(0, len(b))
        if op == 0 and b:  # delete a byte
            del b[min(pos, len(b) - 1)]
        elif op == 1:  # insert a structural or odd byte
            b[pos:pos] = bytes([rng.choice(b'{}[],:"\\0123456789eE+-.tfnu \x00\x80\xc3\xed\xf0\xff')])
        elif op == 2 and b:  # flip a bit
            i = min(pos, len(b) - 1)
            b[i] ^= 1 << rng.randrange(8)
        elif op == 3:  # truncate
            del b[pos:]
        elif op == 4 and len(b) > 1:  # duplicate a slice
            i = rng.randrange(len(b))
            b[pos:pos] = b[i:i + rng.randint(1, 8)]
        else:  # splice in a tricky token
            b[pos:pos] = rng.choice([b"NaN", b"-Infinity", b"1e999", b'"\\ud800"', b"\\u", b'"\\uDFFF"',
                                     b"\xef\xbb\xbf", b"[" * 5, b"}" * 3, b"-", b"0x1", b"01", b"1.", b".5"])
    return bytes(b)


@pytest.mark.parametrize("seed", SEEDS)
def test_corrupted_input_matches_json_or_both_reject(seed):
    rng = random.Random(seed)
    for i in range(ITERS):
        doc = mutate(rng, rand_text(rng, rand_value(rng)).encode())
        try:
            text = doc.decode("utf-8")
        except UnicodeDecodeError:
            with pytest.raises(ValueError):
                rjson.loads(doc)
            continue
        try:
            want = json.loads(text)
        except (ValueError, RecursionError):
            want = ValueError
        for data in (doc, text):
            try:
                got = rjson.loads(data)
            except ValueError:
                got = ValueError
            if want is ValueError or got is ValueError:
                if want is not ValueError and got is ValueError:
                    # json is more lenient (NaN, Infinity, overflow to inf, lone
                    # surrogates): lenient=True must then give json's result.
                    # (Compared on the same input type: json.loads(bytes)
                    # detects UTF-16/32, which NUL bytes can trigger.)
                    try:
                        want_same_input = json.loads(data)
                    except (ValueError, RecursionError):
                        want_same_input = ValueError
                    try:
                        lenient = rjson.loads(data, lenient=True)
                    except ValueError:
                        lenient = ValueError
                    assert (lenient is want_same_input is ValueError) or same(
                        lenient, want_same_input), f"seed={seed} case={i}: {doc[:200]!r}"
                else:
                    assert got is want, f"seed={seed} case={i}: rjson accepted {doc[:200]!r}"
            else:
                assert same(got, want), f"seed={seed} case={i}: {doc[:200]!r}"


@pytest.mark.parametrize("seed", SEEDS)
def test_random_bytes_never_escape_documented_errors(seed):
    rng = random.Random(seed)
    for _ in range(ITERS):
        blob = bytes(rng.getrandbits(8) for _ in range(rng.randint(0, 64)))
        for kw in ({}, {"lenient": True}):
            try:
                rjson.loads(blob, **kw)
            except (ValueError, RecursionError):  # JSONDecodeError, UnicodeDecodeError
                pass


@pytest.mark.parametrize("seed", SEEDS)
def test_dumps_options_on_random_objects(seed):
    import datetime as dt
    import enum
    import uuid

    class Color(enum.Enum):
        RED = "red"
        ONE = 1

    rng = random.Random(seed)

    def extra(depth=0):
        k = rng.randrange(9 if depth < 4 else 5)
        if k == 0:
            return dt.datetime(2024, 1, 1 + rng.randrange(28), rng.randrange(24), tzinfo=rng.choice(
                [None, dt.timezone.utc, dt.timezone(dt.timedelta(hours=rng.randint(-12, 12)))]))
        if k == 1:
            return uuid.UUID(int=rng.getrandbits(128))
        if k == 2:
            return rng.choice(list(Color))
        if k == 3:
            return {1, 2}  # needs default=
        if k == 4:
            return rand_value(rng, 5)
        if k in (5, 6):
            return [extra(depth + 1) for _ in range(rng.randint(0, 4))]
        keys = [rand_str(rng, 4), rng.randint(-5, 5), rng.choice([True, None, 1.5])]
        return {rng.choice(keys): extra(depth + 1) for _ in range(rng.randint(0, 4))}

    def default(o):
        if isinstance(o, set):
            return sorted(o)
        raise TypeError(type(o).__name__)

    for i in range(ITERS // 2):
        obj = extra()
        for kw in ({}, {"default": default}, {"non_str_keys": True, "default": default},
                   {"passthrough": rjson.PASSTHROUGH_DATETIME | rjson.PASSTHROUGH_UUID,
                    "default": str, "non_str_keys": True}):
            try:
                out = rjson.dumps(obj, **kw)
            except (TypeError, ValueError):  # JSONEncodeError, or default's TypeError
                continue
            assert rjson.dumps_str(obj, **kw) == out.decode(), f"seed={seed} case={i}"
            json.loads(out)  # always valid JSON
