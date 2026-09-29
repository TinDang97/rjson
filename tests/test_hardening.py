"""Production hardening: deep nesting on small thread stacks, resource limits.

Cases that used to kill the interpreter run in a subprocess, so a regression
fails the test instead of the test run.
"""

import json
import os
import subprocess
import sys
import textwrap

import pytest
import rjson

# One (stack size, operation, depth) case in a thread with that stack; prints
# the outcome: "ok" or the exception type.
CHILD = textwrap.dedent(
    r"""
    import datetime as dt, decimal, sys, threading
    import rjson

    stack, what, depth = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
    try:
        threading.stack_size(stack)
    except ValueError:  # below this interpreter's minimum (3.14: 52 KiB)
        print("unsupported stack size")
        sys.exit(0)

    class Opaque:
        pass

    def default(o):  # runs real Python code at the deepest level
        return {"d": str(decimal.Decimal("1.5") * 3), "t": dt.date(2024, 1, 2).isoformat()}

    def work():
        if what == "loads_list":
            rjson.loads(b"[" * depth + b"]" * depth)
        elif what == "loads_dict":
            rjson.loads(b'{"a":' * depth + b"1" + b"}" * depth)
        elif what == "loads_lenient":
            rjson.loads(b"[NaN," * depth + b"1" + b"]" * depth, lenient=True)
        elif what == "loads_lenient_beyond_limit":  # > 1024: the json.loads fallback
            n = depth + 1100
            rjson.loads(b"[" * n + b"]" * n, lenient=True)
        elif what == "loads_ndjson":  # the fast path, then loads() on the line
            rjson.loads_ndjson(b"1\n" + b"[" * depth + b"]" * depth + b"\n2")
        elif what == "loads_ndjson_lenient_beyond_limit":
            n = depth + 1100
            rjson.loads_ndjson(b"1\n" + b"[" * n + b"]" * n, lenient=True)
        else:
            x = Opaque() if what.startswith("dumps_default") else []
            for _ in range(depth):
                if what == "dumps_str":
                    x = {"k": [x]}
                elif what == "dumps_default_dict":
                    x = {"k": x}
                else:
                    x = [x]
            if what == "dumps_str":
                rjson.dumps_str(x)
            else:
                rjson.dumps(x, default=default)

    out = []

    def run():
        try:
            work()
            out.append("ok")
        except Exception as exc:
            out.append(type(exc).__name__)

    t = threading.Thread(target=run)
    t.start()
    t.join()
    print(out[0])
    """
)


def run_child(stack, what, depth):
    proc = subprocess.run(
        [sys.executable, "-c", CHILD, str(stack), what, str(depth)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert proc.returncode == 0, f"crashed (exit {proc.returncode}): {proc.stderr[-400:]}"
    out = proc.stdout.strip()
    if out == "unsupported stack size":
        pytest.skip(f"this Python refuses {stack // 1024} KiB thread stacks")
    return out


SMALL_STACKS = [
    # CPython 3.14 (rc2) threads with a 32 KiB stack hang before running any
    # Python code, with or without rjson.
    pytest.param(32 * 1024, marks=pytest.mark.skipif(
        sys.version_info >= (3, 14), reason="CPython 3.14 cannot run a 32 KiB-stack thread")),
    48 * 1024,
    64 * 1024,
    128 * 1024,
]


@pytest.mark.parametrize("stack", SMALL_STACKS, ids=["32KiB", "48KiB", "64KiB", "128KiB"])
@pytest.mark.parametrize(
    "what,depth",
    [
        ("loads_list", 1024),
        ("loads_dict", 1024),
        ("loads_lenient", 1000),
        ("loads_lenient_beyond_limit", 0),
        ("loads_ndjson", 1024),
        ("loads_ndjson_lenient_beyond_limit", 0),
        ("dumps_default", 250),  # + default() call + its dict = 252 levels
        # 3.14 checks the C stack before running Python code (default=) and
        # aborted the process when rjson had recursed too close to the end.
        ("dumps_default_dict", 250),
        ("dumps_str", 126),
    ],
)
def test_deep_nesting_on_small_stack_raises_instead_of_crashing(stack, what, depth):
    # Before: SIGSEGV on 64-128 KiB stacks (musl/Alpine thread default,
    # threading.stack_size); json.loads crashes the same way on 3.12/3.13.
    outcome = run_child(stack, what, depth)
    if what.endswith("lenient_beyond_limit"):
        # rjson's depth error, or on 3.14 json.loads's own (stack-checked) result.
        assert outcome in ("JSONDecodeError", "RecursionError", "ok")
    else:
        assert outcome in ("RecursionError", "ok")


@pytest.mark.parametrize("what,depth", [("loads_list", 1024), ("loads_dict", 1024), ("loads_ndjson", 1024),
                                        ("dumps_default", 250), ("dumps_str", 126)])
@pytest.mark.skipif(bool(os.environ.get("RJSON_SANITIZER")),
                    reason="sanitizer red zones make every stack frame larger")
def test_deep_nesting_still_works_with_a_normal_stack(what, depth):
    # 256 KiB is far below the usual 8 MiB; the full depth limits still fit.
    assert run_child(256 * 1024, what, depth) == "ok"


@pytest.mark.skipif(sys.platform == "win32",
                    reason="threading.stack_size sets the committed, not the reserved, stack on Windows")
def test_stack_error_is_a_recursion_error_with_advice():
    code = CHILD.replace('out.append(type(exc).__name__)', 'out.append(f"{type(exc).__name__}: {exc}")')
    stack = 128 * 1024 if sys.platform == "linux" and os.uname().machine == "aarch64" else 64 * 1024
    proc = subprocess.run([sys.executable, "-c", code, str(stack), "loads_dict", "1024"],
                          capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0, proc.stderr
    if proc.stdout.strip() == "unsupported stack size":
        pytest.skip(f"this Python refuses {stack // 1024} KiB thread stacks")
    assert proc.stdout.startswith("RecursionError:") and "stack_size" in proc.stdout


def test_main_thread_limits_unchanged():
    assert rjson.loads(b"[" * 1024 + b"]" * 1024) is not None
    with pytest.raises(rjson.JSONDecodeError, match="depth limit exceeded"):
        rjson.loads(b"[" * 1025 + b"]" * 1025)
    x = []
    for _ in range(254):
        x = [x]
    with pytest.raises(rjson.JSONEncodeError, match="nesting depth"):
        rjson.dumps(x)


def test_full_depth_on_a_fresh_main_thread():
    # musl reports only the mapped part of the main thread's stack (~100-200
    # KiB of an 8 MiB limit) until it grows: rjson 0.1.0 raised
    # RecursionError at ~600 levels on Alpine's main thread. A new process,
    # so the stack has not grown yet.
    code = (
        "import rjson\n"
        "rjson.loads(b'[' * 1024 + b']' * 1024)\n"
        "rjson.loads(b'{\"a\":' * 1024 + b'1' + b'}' * 1024)\n"
        "x = object()\n"
        "for _ in range(250):\n    x = [x]\n"
        "rjson.dumps(x, default=lambda o: {'d': 1})\n"
        "try:\n    rjson.loads(b'[' * 2000 + b']' * 2000)\n"
        "except rjson.JSONDecodeError:\n    print('ok')\n"
    )
    proc = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=60)
    assert proc.returncode == 0, proc.stderr[-400:]
    assert proc.stdout.strip() == "ok"


def test_lenient_beyond_depth_limit_on_main_thread_uses_json():
    # Plenty of stack on the main thread: the json.loads fallback still runs.
    doc = b"[" * 1500 + b"]" * 1500
    try:
        want = json.loads(doc)
    except RecursionError:
        pytest.skip("this Python's json.loads stops before 1500 levels")
    assert rjson.loads(doc, lenient=True) == want


# -- integer string conversion limit (CVE-2020-10735) -----------------------------------


@pytest.mark.skipif(not hasattr(sys, "get_int_max_str_digits"), reason="no int digit limit")
class TestIntDigitLimit:
    def test_loads_names_the_limit(self):
        with pytest.raises(json.JSONDecodeError) as exc:
            rjson.loads(b"[" + b"7" * 5000 + b"]")
        assert "set_int_max_str_digits" in exc.value.msg
        assert exc.value.pos == 1
        with pytest.raises(ValueError):  # json refuses the same input
            json.loads(b"[" + b"7" * 5000 + b"]")

    def test_limit_is_the_interpreters(self):
        old = sys.get_int_max_str_digits()
        try:
            sys.set_int_max_str_digits(0)  # disabled
            assert rjson.loads(b"7" * 5000) == int("7" * 5000)
            assert rjson.dumps(int("7" * 5000)) == b"7" * 5000
            sys.set_int_max_str_digits(4300)
            assert rjson.loads(b"7" * 4300) == int("7" * 4300)
            with pytest.raises(ValueError):
                rjson.loads(b"7" * 4301)
        finally:
            sys.set_int_max_str_digits(old)

    def test_dumps_refuses_like_json(self):
        with pytest.raises(ValueError, match="int_max_str_digits|Exceeds the limit"):
            rjson.dumps(10**5000)


# -- threads and re-entrancy --------------------------------------------------------------
#
# rjson keeps pooled buffers, a dict-key cache and output size hints across
# calls. Python code run mid-call (default=, a GIL switch inside it) can call
# rjson again on the same thread or let other threads call it; every result
# must still be exact.


def _docs():
    docs = []
    for i in range(40):
        obj = {
            f"key{i % 7}": [i, i * 1.5, str(i) * (i % 5), None, True],
            "shared": {"nested": [{"k": j, f"u{i % 3}": "é" * (i % 4)} for j in range(i % 6)]},
            f"only_{i}": "x" * (i * 37 % 300),
        }
        docs.append((obj, json.dumps(obj, ensure_ascii=bool(i % 2)).encode()))
    return docs


def test_many_threads_share_pools_and_key_cache():
    import threading

    docs = _docs()
    errors = []
    old = sys.getswitchinterval()
    sys.setswitchinterval(1e-6)  # switch threads as often as possible
    try:

        def worker(seed):
            try:
                for k in range(300):
                    obj, text = docs[(seed * 7 + k) % len(docs)]
                    assert rjson.loads(text) == obj
                    assert rjson.loads(rjson.dumps(obj)) == obj
                    assert json.loads(rjson.dumps_str(obj)) == obj
            except BaseException as exc:  # reported below
                errors.append(exc)

        threads = [threading.Thread(target=worker, args=(s,)) for s in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    finally:
        sys.setswitchinterval(old)
    assert not errors, errors[0]


def test_reentrant_calls_and_thread_switches_inside_default():
    import threading
    import time

    docs = _docs()
    stop = threading.Event()
    errors = []

    class Marker:
        def __init__(self, i):
            self.i = i

    def default(o):
        # Re-enter rjson on this thread (pools, key cache, size hints in
        # use by the outer call) and let other threads run mid-call.
        obj, text = docs[o.i % len(docs)]
        assert rjson.loads(text) == obj
        assert rjson.loads(rjson.dumps(obj)) == obj
        time.sleep(0)
        return {"marker": o.i, "big": "y" * (o.i * 101 % 5000)}

    def background():
        try:
            k = 0
            while not stop.is_set():
                obj, text = docs[k % len(docs)]
                assert rjson.loads(text) == obj and rjson.loads(rjson.dumps(obj)) == obj
                k += 1
        except BaseException as exc:
            errors.append(exc)

    old = sys.getswitchinterval()
    sys.setswitchinterval(1e-5)  # hand the GIL back quickly after sleep(0)
    workers = [threading.Thread(target=background) for _ in range(3)]
    for t in workers:
        t.start()
    try:
        for n in range(60):
            value = {"items": [Marker(n + j) for j in range(n % 9)], "tail": [Marker(n)] * 2}
            expected = {
                "items": [{"marker": n + j, "big": "y" * ((n + j) * 101 % 5000)} for j in range(n % 9)],
                "tail": [{"marker": n, "big": "y" * (n * 101 % 5000)}] * 2,
            }
            assert rjson.loads(rjson.dumps(value, default=default)) == expected
            assert json.loads(rjson.dumps_str(value, default=default)) == expected
    finally:
        stop.set()
        for t in workers:
            t.join()
        sys.setswitchinterval(old)
    assert not errors, errors[0]


def test_failed_calls_leave_no_state_behind():
    # Errors in the middle of a document (pooled buffers half-filled, key
    # cache touched) must not affect the next call.
    docs = _docs()
    for obj, text in docs:
        for bad in (text[: len(text) // 2], text + b",", text.replace(b":", b"", 1)):
            with pytest.raises(ValueError):
                rjson.loads(bad)
        with pytest.raises(TypeError):
            rjson.dumps([obj, object()])
        assert rjson.loads(text) == obj
        assert rjson.loads(rjson.dumps(obj)) == obj


def test_package_versions_agree():
    # The release workflow publishes tag vX.Y.Z only if Cargo.toml (which sets
    # rjson.__version__) and pyproject.toml (the PyPI version) both say X.Y.Z.
    import pathlib
    import re

    root = pathlib.Path(__file__).resolve().parent.parent
    versions = {
        name: re.search(r'(?m)^version = "([^"]+)"', (root / name).read_text()).group(1)
        for name in ("Cargo.toml", "pyproject.toml")
    }
    assert versions["Cargo.toml"] == versions["pyproject.toml"] == rjson.__version__
