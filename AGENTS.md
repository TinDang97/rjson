# AGENTS.md

Instructions for AI coding agents (Codex, Cursor, Copilot, Claude Code, ...) working in
this repository. The full guide, with the architecture and the hard rules learned from
past bugs, is [CLAUDE.md](CLAUDE.md): read it before changing `src/`.

Using rjson in another project rather than changing it? See [llms.txt](llms.txt) for the
API and the migration mapping from `json`/orjson.

## Build and test

```bash
uv venv .venv -p 3.13 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release
python -m pytest tests -q          # must pass on CPython 3.10-3.14
cargo clippy --release
```

## Rules that matter most

- Never hard-code CPython object layouts; version-gate private symbols (listed in CLAUDE.md).
- Reserve worst-case output before raw writes; check the exact type of every element.
- Check every C-API NULL return; `panic = "abort"`, so never `unwrap` Python-derived data.
- New recursion goes through `Parser::enter` / `Serializer::nest_error` (stack checks).
- Code that may run Python code during `dumps` must return `SerError::NeedGuard` first.
- Never set `RUSTFLAGS` or `target-cpu=native`.
- Every bug fix gets a regression test; performance changes are measured against orjson
  in the same process and recorded in `docs/PERFORMANCE_REVIEW.md`.
