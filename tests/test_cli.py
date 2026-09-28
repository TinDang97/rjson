"""The command line tool (``rjson``, ``python -m rjson``, ``python -m rjson.tool``).

Its output must equal ``python -m json.tool``'s for every shared option (floats
below 1e-4 aside, which these documents avoid).
"""

from __future__ import annotations

import json
import os
import random
import shutil
import stat
import subprocess
import sys
from pathlib import Path

import pytest
import rjson
from rjson.tool import beautify

TEXT = 'aZ "\\/\n\t\x00\x1f\x7f\x80é\xff日￿😀\U0010ffff'


def rand_doc(rng, depth=0):
    k = rng.random()
    if depth > 4 or k < 0.35:
        return rng.choice([
            None, True, False, rng.randint(-10**20, 10**20), 0.5, -1234.25, 1e22, 3.0,
            "".join(rng.choice(TEXT) for _ in range(rng.randint(0, 6))), [], {},
            float("nan"), float("inf"),
        ])
    if k < 0.65:
        return [rand_doc(rng, depth + 1) for _ in range(rng.randint(0, 5))]
    return {"".join(rng.choice(TEXT) for _ in range(rng.randint(0, 4))): rand_doc(rng, depth + 1)
            for _ in range(rng.randint(0, 5))}


def run(args, stdin=b"", env=None, module="rjson"):
    return subprocess.run(
        [sys.executable, "-m", module, *args], input=stdin, capture_output=True,
        timeout=60, env=env, check=False,
    )


def json_tool(args, stdin=b""):
    # UTF-8 stdin/stdout for json.tool also where the locale encoding is not
    # (Windows); rjson always reads and writes UTF-8.
    return run(args, stdin, env={**os.environ, "PYTHONUTF8": "1"}, module="json.tool")


NL = os.linesep.encode()


def nl(b):
    """Expected stdout: json.tool writes os.linesep line ends (CRLF on Windows)."""
    return b.replace(b"\n", NL)


@pytest.fixture(scope="module")
def docs_file(tmp_path_factory):
    rng = random.Random(7)
    doc = [rand_doc(rng) for _ in range(60)]
    path = tmp_path_factory.mktemp("cli") / "doc.json"
    path.write_text(json.dumps(doc, ensure_ascii=False), encoding="utf-8")
    return path


LAYOUTS = [
    [], ["--sort-keys"], ["--no-ensure-ascii"], ["--tab"], ["--no-indent"], ["--compact"],
    ["--indent", "2", "--sort-keys", "--no-ensure-ascii"], ["--indent", "0"], ["--indent", "-2"],
    ["--compact", "--no-ensure-ascii", "--sort-keys"],
]


@pytest.mark.parametrize("flags", LAYOUTS, ids=" ".join)
def test_same_output_as_json_tool(docs_file, flags):
    want = json_tool([*flags, str(docs_file)])
    got = run([*flags, str(docs_file)])
    assert want.returncode == 0, want.stderr
    assert got.returncode == 0, got.stderr
    assert got.stdout == want.stdout
    # stdin -> stdout too.
    assert run(flags, docs_file.read_bytes()).stdout == want.stdout


def test_json_lines_same_as_json_tool(tmp_path):
    lines = [json.dumps(rand_doc(random.Random(i)), ensure_ascii=False) for i in range(40)]
    data = ("\n".join(lines) + "\n").encode()
    for flags in (["--json-lines"], ["--json-lines", "--compact"], ["--json-lines", "--no-indent", "--sort-keys"]):
        want = json_tool(flags, data)
        assert want.returncode == 0, want.stderr
        assert run(flags, data).stdout == want.stdout
        path = tmp_path / "in.jsonl"
        path.write_bytes(data)
        assert run([*flags, str(path)]).stdout == want.stdout


def test_json_lines_skips_blank_lines_and_names_the_bad_line():
    out = run(["--jsonl", "--compact"], b'{"a": 1}\n\n  \n[2]\n')
    assert out.returncode == 0 and out.stdout == nl(b'{"a":1}\n[2]\n')
    bad = run(["--ndjson"], b'{"a": 1}\n[1, 2\n')
    assert bad.returncode == 1
    assert b"JSON Lines input, line 2" in bad.stderr


def test_beautify_api():
    data = b'{"b": [1, 2.5, "\xc3\xa9"], "a": NaN}'
    # beautify() returns "\n" line ends; json.tool's stdout has os.linesep.
    assert nl(beautify(data)) == json_tool([], data).stdout
    assert beautify(data.decode(), compact=True, sort_keys=True, ensure_ascii=False) == (
        '{"a":NaN,"b":[1,2.5,"é"]}\n'.encode()
    )
    assert beautify("[1]\n\n[2]", json_lines=True, indent=None) == b"[1]\n[2]\n"
    assert beautify("[1]", indent="\t") == b"[\n\t1\n]\n"
    with pytest.raises(ValueError):
        beautify("[NaN]", strict=True)
    with pytest.raises(rjson.JSONDecodeError):
        beautify("[1,")


def test_errors_exit_1_with_message(tmp_path):
    out = run([], b"[1, 2")
    assert out.returncode == 1 and out.stdout == b""
    assert b"line 1 column 6" in out.stderr and b"Traceback" not in out.stderr
    missing = run([str(tmp_path / "nope.json")])
    assert missing.returncode == 1 and b"No such file" in missing.stderr
    assert run(["--strict"], b"[NaN]").returncode == 1
    assert run([], b"[NaN, -Infinity]").stdout == nl(b"[\n    NaN,\n    -Infinity\n]\n")
    assert run(["a", "b", "c"]).returncode == 2  # usage error, like argparse
    assert run(["--tab", "--compact"]).returncode == 2


def test_outfile_may_be_infile(tmp_path):
    path = tmp_path / "x.json"
    path.write_bytes(b'{"b":1,"a":[]}')
    assert run(["--sort-keys", str(path), str(path)]).returncode == 0
    assert path.read_bytes() == nl(b'{\n    "a": [],\n    "b": 1\n}\n')


def test_in_place_check_and_validate(tmp_path):
    good = tmp_path / "good.json"
    good.write_bytes(b'{\n    "a": 1\n}\n')
    ugly = tmp_path / "ugly.json"
    ugly.write_bytes(b'{"a":1,   "b": [1,2]}')
    ugly.chmod(0o640)
    bad = tmp_path / "bad.json"
    bad.write_bytes(b'{"a":')

    out = run(["--check", str(good), str(ugly)])
    assert out.returncode == 1 and f"would reformat {ugly}".encode() in out.stderr
    assert str(good).encode() not in out.stderr
    assert run(["--check", str(good)]).returncode == 0

    out = run(["--validate", str(good), str(ugly), str(bad)])
    assert out.returncode == 1
    assert out.stderr.count(b"\n") == 1 and str(bad).encode() in out.stderr
    assert run(["--validate", str(good), str(ugly)]).returncode == 0

    before = good.stat().st_mtime_ns
    out = run(["-i", str(good), str(ugly)])
    assert out.returncode == 0 and out.stderr == nl(f"reformatted {ugly}\n".encode())
    assert ugly.read_bytes() == b'{\n    "a": 1,\n    "b": [\n        1,\n        2\n    ]\n}\n'
    if os.name != "nt":  # Windows has only a read-only bit
        assert stat.S_IMODE(ugly.stat().st_mode) == 0o640
    assert good.stat().st_mtime_ns == before  # unchanged files are not rewritten
    assert run(["--check", str(ugly)]).returncode == 0

    out = run(["--in-place", "--compact", str(bad), str(ugly)])
    assert out.returncode == 1 and ugly.read_bytes() == b'{"a":1,"b":[1,2]}\n'
    assert bad.read_bytes() == b'{"a":'  # invalid files are left alone
    assert not [p for p in os.listdir(tmp_path) if p.startswith(".rjson-")]
    assert run(["-i", "-"]).returncode == 2
    assert run(["--check", "--jsonl", "--compact", "-"], b'{"a": 1}\n').returncode == 1
    assert run(["--check", "--jsonl", "--compact", "-"], b'{"a":1}\n').returncode == 0
    # A file keeps its CRLF line ends.
    crlf = tmp_path / "crlf.json"
    crlf.write_bytes(b'{"a":1}')
    assert run(["-i", "--indent", "1", str(crlf)]).returncode == 0
    crlf.write_bytes(crlf.read_bytes().replace(b"\n", b"\r\n"))
    assert run(["--check", "--indent", "1", str(crlf)]).returncode == 0
    assert run(["-i", "--compact", str(crlf)]).returncode == 0
    assert crlf.read_bytes() == b'{"a":1}\r\n'


def test_color():
    plain = run([], b'{"k": ["s", 1, true, null]}')
    env = {**os.environ, "FORCE_COLOR": "1"}
    env.pop("NO_COLOR", None)
    env.pop("PYTHON_COLORS", None)
    colored = run([], b'{"k": ["s", 1, true, null]}', env=env)
    assert b"\x1b[" in colored.stdout
    no_ansi = colored.stdout.replace(b"\x1b[0m", b"")
    for code in (b"\x1b[1m", b"\x1b[32m", b"\x1b[33m", b"\x1b[1;34m"):
        no_ansi = no_ansi.replace(code, b"")
    if sys.version_info < (3, 14):  # 3.14 may take the user's _colorize theme
        assert no_ansi == plain.stdout
    assert b"\x1b[" not in run(["--color", "never"], b"[1]", env=env).stdout
    assert b"\x1b[" not in run([], b"[1]", env={**env, "NO_COLOR": "1", "FORCE_COLOR": ""}).stdout
    assert b"\x1b[" in run(["--color", "always"], b"[1]").stdout
    assert b"\x1b[" not in plain.stdout  # a pipe is not a terminal


def test_entry_points():
    for module in ("rjson", "rjson.tool"):
        out = run(["--version"], module=module)
        assert out.returncode == 0 and out.stdout.strip() == f"rjson {rjson.__version__}".encode()
    assert b"python -m rjson" in run(["--help"]).stdout
    exe = shutil.which("rjson", path=str(Path(sys.executable).parent))
    if exe is None:
        pytest.skip("console script not installed next to this interpreter")
    out = subprocess.run([exe, "--compact"], input=b"[1, 2]", capture_output=True, check=False)
    assert out.returncode == 0 and out.stdout == nl(b"[1,2]\n")


def test_broken_pipe_is_quiet():
    code = (
        "import subprocess, sys\n"
        "p = subprocess.Popen([sys.executable, '-m', 'rjson', '--json-lines', '--compact'],"
        " stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)\n"
        "p.stdout.close()\n"
        "try:\n"
        "    _, err = p.communicate(b'[1]\\n' * 200000)\n"
        "except BrokenPipeError:\n"
        "    err = p.stderr.read(); p.wait()\n"
        "sys.stdout.write(err.decode())\n"
    )
    out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True,
                         timeout=60, check=False)
    assert "Traceback" not in out.stdout, out.stdout
