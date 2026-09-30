# Changelog

All notable changes to rjson (PyPI: `pyrjson`). The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/), with breaking changes allowed in
0.x minor releases.

## [Unreleased]

### Performance

- `loads` decodes non-ASCII strings 16 bytes per step with SIMD instead of
  branching on each character's length, for all three string kinds:
  accented Latin text 0.53× → 0.31× of orjson's time, Cyrillic and Greek
  ~0.73× → 0.55×, hangul 0.88× → 0.72×, emoji-only text 1.1× → 0.67×, the
  benchmark's unicode strings 0.81× → 0.61×; pure CJK unchanged. Their speed
  also no longer changes from one PGO build to the next (PGO wheels decoded
  such text up to 20% slower than plain builds).

### Changed

- PGO builds are reproducible: `scripts/pgo_train.py` makes a fixed number of
  calls per training step instead of running each for 0.2 s, and
  `scripts/build_pgo.sh` trains with `PYTHONHASHSEED=0` and, on Linux, address
  randomization off (the `loads` shape cache picks slots from object
  addresses). Two builds of the same commit now give a byte-identical
  extension module; before, their speed on single cases differed by up to 20%.
  `RJSON_PGO_SECONDS` is replaced by `RJSON_PGO_SCALE`; `PROFILE_DIR=` keeps
  the merged profiles.

## [0.4.0] - 2026-09-30

### Added
- `rjson.loads_ndjson(data, *, lenient=False)`: newline-delimited JSON (NDJSON / JSON
  Lines) to a list in one call. Each line gives what `loads(line)` gives, blank lines are
  skipped, and errors carry their position (`lineno`, `colno`) in the whole input. 1.4–2.6×
  faster than a per-line loop on orjson, 7–49% faster than one on `rjson.loads`.

### Performance
- `rjson --json-lines` parses with `loads_ndjson` in 16 KiB chunks: `--validate` 26%
  faster, file to file 12–15%, stdin to stdout 6–11% (200k records); output and error
  messages are unchanged.
- `loads` is faster on documents whose objects repeat the same keys (API responses, records,
  logs) on CPython 3.11–3.13: a shape cache builds each such dict by copying a template dict
  and filling in the values. Records are 15–40% faster, twitter.json 9–13%; documents
  without repeated shapes are unchanged (at most 2% slower in the worst case). On
  3.11/3.12, results made of objects with more than 8 keys also take about 16% less memory.

## [0.3.0] - 2026-09-28

### Added
- `rjson` command line (also `python -m rjson` / `python -m rjson.tool`): a JSON
  beautifier and drop-in `python -m json.tool` with the same options and byte-identical
  output, 2–10× faster on files from 0.6 MB up (`benches/cli_benchmark.py`). Extras:
  `-i/--in-place`, `--check` (CI: exit 1 if a file is invalid or not formatted),
  `--validate`, `--color`, `--strict`, `--minify`/`--jsonl`/`--ndjson` aliases, streamed
  JSON Lines with blank lines skipped, and `rjson.tool.beautify()`.
- `dumps`/`dumps_str`: `json.dumps`'s `separators=`, `ensure_ascii=` and `allow_nan=`, and
  a str `indent=` (e.g. `"\t"`), each with `json.dumps`'s output (differential tests). The
  defaults are unchanged (compact, non-ASCII kept, NaN raises).
- Django (`examples/django_json.py`: `JsonResponse` subclass, request-body parsing) and
  Flask (`examples/flask_json.py`: `JSONProvider`) integration examples (#27, #29, thanks
  @HarshRajSinghania), with tests; README section with copy-paste snippets.

### Changed
- `dumps_str` with `indent=`/`sort_keys=` now keeps lone surrogates, as `json.dumps` and
  compact `dumps_str` do (it raised `UnicodeEncodeError`).
- The package is a mixed Python/Rust layout (`python/rjson/`); the type stubs moved from
  `rjson.pyi` to `python/rjson/__init__.pyi`. Installed files are the same plus the CLI.

## [0.2.0] - 2026-09-27

### Added
- `dumps`/`dumps_str` `indent=` and `sort_keys=` (#23, #24). `indent=2` and
  `sort_keys=True` produce exactly orjson's `OPT_INDENT_2` / `OPT_SORT_KEYS`
  output (differential-tested, including the benchmark corpora); other
  widths lay out like `json.dumps(indent=n)`. Compact output is unchanged
  and pays nothing (same instruction count). The options are slower than
  orjson's for now (twitter.json: `sort_keys` 0.78x, `indent=2` 0.48x): the
  indentation is a second pass over the compact output.

### Docs
- README images and links are absolute, so they render on PyPI; richer PyPI
  metadata (keywords, classifiers, project links); `llms.txt` and
  `AGENTS.md` for AI assistants and coding agents; a social preview image.

## [0.1.1] - 2026-09-27

First PyPI release, as `pyrjson` (0.1.0 was tagged but never published: its
musllinux wheel builds failed on the bug fixed below).

### Fixed
- On musl (Alpine), `loads`/`dumps` on the main thread no longer raise
  `RecursionError` a few hundred levels deep: musl reports only the part of
  the main thread's stack mapped so far, so the headroom check now takes the
  bound from `RLIMIT_STACK` there, as glibc does.

### Performance
- `dumps` of datetime, UUID, Enum and dataclass values: per-type facts are
  cached (keyed by `tp_version_tag`), and missing native modules (usually
  `zoneinfo`) are no longer looked up in `sys.modules` for every value.
  Dataclasses no longer force guarded mode on CPython 3.12+. Against
  orjson: datetime/UUID/Enum events 1.05x -> 2.3x faster, dataclasses
  0.61x -> 1.1x (`slots=True`: 2.1x).

### Added
- `benches/showcase.py` and [docs/SHOWCASE.md](docs/SHOWCASE.md):
  head-to-head rjson vs orjson on production-shaped workloads.

## [0.1.0] - 2026-09-27

Tagged, not published to PyPI (see 0.1.1); everything below ships in 0.1.1.

### Added
- `dumps(obj, default=...)` hook for unsupported types (#4).
- Native serialization of `datetime`, `date`, `time`, `uuid.UUID`, dataclasses
  and `Enum` members, byte-identical to orjson; `passthrough=` flags
  (`PASSTHROUGH_DATETIME/_UUID/_DATACLASS/_ENUM`) send them to `default` (#5).
- `dumps(obj, non_str_keys=True)` for int/float/bool/None/Enum/datetime/UUID
  dict keys (#6).
- `loads(data, lenient=True)`: accept exactly what `json.loads` accepts (NaN,
  Infinity, a UTF-8 BOM, overflow to inf, lone surrogates, UTF-16/32) (#7).
- `rjson.JSONDecodeError` (= `json.JSONDecodeError`), `rjson.JSONEncodeError`
  (a subclass of both `TypeError` and `ValueError`), `__version__`, `__all__`,
  type stubs (`py.typed`).
- Examples: FastAPI response/request integration, JSON logging and NDJSON,
  a Redis/Kafka codec; benchmarks for them (`benches/examples_benchmark.py`).
- `SECURITY.md`, Dependabot, a release workflow publishing to PyPI with
  trusted publishing and build provenance attestations.

### Changed
- PyPI distribution renamed to `pyrjson` (the import name stays `rjson`);
  MIT license.
- `dumps` failures raise `rjson.JSONEncodeError` instead of `ValueError`, so
  `except TypeError` handlers written for `json`/orjson keep working.
- `loads` error messages match `json` (bare `.msg`, same positions for
  delimiter errors).
- `loads` accepts memoryviews of any layout.

### Fixed
- `loads`/`dumps` no longer crash (SIGSEGV) on deeply nested input in threads
  with small stacks (64-128 KiB: musl/Alpine, `threading.stack_size`); they
  raise `RecursionError`. The lenient `json.loads` fallback is skipped where
  it would overflow the stack itself.
- `loads` names CPython's integer digit limit (`sys.set_int_max_str_digits`)
  instead of reporting "invalid number".
- A small `dumps` right after a big one no longer allocates a buffer of the
  big size (#13).

### Performance
- CJK/UCS-2 text `loads` (#8), full-precision float arrays `loads` (#9),
  output buffer sizing, and fewer UTF-8 copies attached to strings (#10).
  Current numbers: [docs/PERFORMANCE_REVIEW.md](docs/PERFORMANCE_REVIEW.md).

[Unreleased]: https://github.com/TinDang97/rjson/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/TinDang97/rjson/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/TinDang97/rjson/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/TinDang97/rjson/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/TinDang97/rjson/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/TinDang97/rjson/releases/tag/v0.1.0
