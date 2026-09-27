# Changelog

All notable changes to rjson (PyPI: `pyrjson`). The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/), with breaking changes allowed in
0.x minor releases.

## [Unreleased] (0.1.0, first PyPI release)

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
