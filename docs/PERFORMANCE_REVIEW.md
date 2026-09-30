# rjson performance review: closing the gap to orjson

Four parallel reviews ran against the pre-review code (commit `c12dbe3`):

- deserialization (`loads`)
- serialization (`dumps`)
- correctness and safety
- build and architecture

The loads, dumps and build reviews each prototyped and measured their changes, and a second round (loads, dumps, infra) worked through the resulting roadmap. This branch merges both rounds. This document records what we found, what landed, how it was measured, and what is still left to beat orjson everywhere.

## 1. Results

### 0.4.1 (PGO wheels, CPython 3.13)

rjson time ÷ orjson time, so **below 1.00 means rjson is faster**. Median time per case over 5 runs of `benches/corpus_benchmark.py --json --repeat 11`, in parentheses the lowest and highest ratio of the 5 runs; CPython 3.13.12, orjson 3.12.0, x86_64 Xeon (4 cores), PGO build from `scripts/build_pgo.sh` (reproducible since 0.4.1). Raw data: `docs/img/benchmark-results.json`.

| case | loads | loads_str | dumps | dumps_str |
|---|---|---|---|---|
| twitter | **0.74** (0.69–0.78) | 1.24 (1.11–1.52) | **0.66** (0.63–0.70) | 1.26 (1.21–1.29) |
| citm_catalog | **0.90** (0.86–0.92) | 1.14 (0.95–1.15) | **0.73** (0.71–0.75) | **0.81** (0.80–0.86) |
| canada | **0.89** (0.81–0.94) | **0.92** (0.83–0.97) | **0.80** (0.75–0.81) | **0.77** (0.72–0.82) |
| github | **0.67** (0.59–0.68) | **0.64** (0.60–0.68) | **0.58** (0.55–0.58) | **0.61** (0.58–0.65) |
| small_dict | **0.63** (0.60–0.68) | **0.62** (0.57–0.98) | **0.73** (0.70–0.74) | **0.77** (0.72–0.78) |
| records | **0.70** (0.60–0.71) | **0.68** (0.64–0.72) | **0.61** (0.55–0.67) | **0.60** (0.57–0.63) |
| unicode_strings | **0.60** (0.59–0.63) | 1.02 (1.01–1.06) | **0.94** (0.88–0.96) | 2.34 (2.32–2.42) |
| escaped_strings | **0.87** (0.83–0.92) | **0.89** (0.82–0.93) | **0.35** (0.34–0.35) | **0.35** (0.33–0.36) |
| int_array | **0.88** (0.76–0.95) | **0.88** (0.88–0.96) | **0.90** (0.86–0.93) | **0.91** (0.90–0.95) |
| float_array | 1.02 (0.71–1.06) | **0.97** (0.90–1.17) | **0.79** (0.76–0.86) | **0.79** (0.77–0.84) |
| **geomean** | **0.78** | **0.88** | **0.68** | **0.82** |

- **Headline** (README): `loads` 1.29× and `dumps` 1.46× faster than orjson on the geomean (0.4.0: 1.23× and 1.47×); 19 of 20 `loads`/`dumps` medians below 1.00; the other (float array `loads` 1.02, per-run 0.71–1.06) is at parity. Int array `dumps` went from parity to 0.90 in this build with the same code.
- **Where 0.4.1 moved `loads`:** the block decoders (§4 item 20): unicode_strings 0.77 → 0.60, twitter 0.83 → 0.74 (its Japanese text), `loads_str` unicode_strings 1.34 → 1.02. Per script, 2,000 distinct strings, 0.4.0 → 0.4.1 release wheels in one session: French 0.65 → 0.46, Cyrillic 0.74 → 0.56, Greek 0.75 → 0.57, hangul 0.88 → 0.70, Japanese 0.82 → 0.73, CJK with ASCII 0.84 → 0.74, emoji only 1.10 → 0.68; pure CJK 0.85 → 0.84 (unchanged: in the first 0.4.1 build it was 0.74, so the 3-byte run step still depends on placement). Design experiment: `loads` of twitter.json 10.19 M → 9.07 M instructions (orjson 13.20 M).
- **`dumps_str` of UCS4 results** (§4 item 21): a first 0.4.1 build was 5–8% slower on twitter.json with unchanged `dumps` code, because the PGO profile shaped the auto-vectorized widening loop of the fill; explicit SSE4.1 widening fixed it before release. Per-record NDJSON `dumps` stays ~2.5% slower than 0.4.0 in an A/B (placement); in the showcase it is 0.99× of orjson (0.4.0: 1.00×).

### 0.4.0 (PGO wheels, CPython 3.13)

rjson time ÷ orjson time, so **below 1.00 means rjson is faster**. Median time per case over 5 runs of `benches/corpus_benchmark.py --json --repeat 11`, in parentheses the lowest and highest ratio of the 5 runs; CPython 3.13.12, orjson 3.12.0, x86_64 Xeon (4 cores), PGO build from `scripts/build_pgo.sh`. Raw data: `docs/img/benchmark-results.json`.

| case | loads | loads_str | dumps | dumps_str |
|---|---|---|---|---|
| twitter | **0.83** (0.79–0.87) | 1.34 (1.29–1.39) | **0.63** (0.59–0.64) | 1.18 (1.14–1.21) |
| citm_catalog | **0.85** (0.72–0.87) | 1.16 (1.11–1.29) | **0.72** (0.71–0.76) | **0.80** (0.77–0.83) |
| canada | **0.91** (0.87–0.95) | **0.93** (0.90–1.02) | **0.80** (0.78–0.82) | **0.80** (0.79–0.81) |
| github | **0.68** (0.65–0.70) | **0.67** (0.64–0.69) | **0.54** (0.53–0.61) | **0.62** (0.60–0.68) |
| small_dict | **0.64** (0.63–0.65) | **0.66** (0.60–0.67) | **0.69** (0.65–0.72) | **0.72** (0.67–0.74) |
| records | **0.71** (0.69–0.72) | **0.70** (0.69–0.72) | **0.61** (0.59–0.63) | **0.61** (0.59–0.62) |
| unicode_strings | **0.77** (0.75–0.82) | 1.34 (1.28–1.37) | **0.90** (0.89–0.93) | 2.40 (2.36–2.46) |
| escaped_strings | **0.91** (0.89–1.29) | **0.92** (0.90–1.24) | **0.36** (0.34–0.36) | **0.35** (0.34–0.37) |
| int_array | **0.88** (0.84–0.91) | **0.88** (0.85–0.88) | 1.01 (0.98–1.01) | 1.01 (0.97–1.07) |
| float_array | **0.99** (0.91–1.02) | 1.00 (0.99–1.01) | **0.77** (0.75–0.82) | **0.78** (0.77–0.81) |
| **geomean** | **0.81** | **0.93** | **0.68** | **0.82** |

- **Headline** (README): `loads` 1.23× and `dumps` 1.47× faster than orjson on the geomean (0.3.0: 1.17× and 1.36×); 19 of 20 `loads`/`dumps` medians below 1.00; the other (int array `dumps` 1.01, per-run 0.98–1.01) is at parity.
- **Where 0.4.0 moved `loads`:** the shape cache (§3 loads item 12): records 0.90 → 0.71, small_dict 0.79 → 0.64, citm 1.06 → 0.85, github 0.75 → 0.68. Cases without dicts are unchanged; float array reads 0.99 here against 0.79 in the 0.3.0 table, but a 0.3.0 PGO wheel measured on the same host on the same day also gives 0.98, so that is the host, not the release.
- **PGO builds were not reproducible run to run** (§4 item 19, fixed after 0.4.0): the first 0.4.0 PGO build measured unicode_strings `loads` 18% slower than 0.3.0; rebuilding the same commit with the same script gave parity (514 vs 514 µs). These numbers are from the second build.
- The `loads_str` column keeps the trade-off described under the 0.3.0 table below.

### After PR #11 (0.2.0/0.3.0, PGO wheels, CPython 3.13)

rjson time ÷ orjson time, so **below 1.00 means rjson is faster**. Median time per case over 5 runs of `benches/corpus_benchmark.py --json --repeat 11`, in parentheses the lowest and highest ratio of the 5 runs; CPython 3.13.12, orjson 3.12.0, x86_64 Xeon (4 cores), PGO build from `scripts/build_pgo.sh` (what the published wheels are). `loads` parses the document's UTF-8 `bytes`; `loads_str` parses the same `str` object every call; `dumps` returns `bytes` like `orjson.dumps`, `dumps_str` returns `str` (compared with the same `orjson.dumps` time). Raw data: `docs/img/benchmark-results.json`.

| case | loads | loads_str | dumps | dumps_str |
|---|---|---|---|---|
| twitter | **0.84** (0.81–0.89) | 1.17 (1.09–1.30) | **0.58** (0.55–0.62) | 1.39 (1.10–1.44) |
| citm_catalog | 1.06 (0.85–1.15) | 1.12 (1.11–1.37) | **0.89** (0.78–1.06) | 1.01 (0.80–1.23) |
| canada | **0.92** (0.88–1.05) | **0.88** (0.73–1.12) | **0.91** (0.73–0.97) | **0.90** (0.76–0.95) |
| github | **0.75** (0.71–0.77) | **0.71** (0.71–0.86) | **0.53** (0.51–0.54) | **0.59** (0.53–0.61) |
| small_dict | **0.79** (0.76–0.94) | **0.79** (0.62–1.04) | **0.79** (0.76–0.92) | **0.67** (0.65–0.86) |
| records | **0.90** (0.71–0.95) | **0.91** (0.87–1.12) | **0.74** (0.52–0.85) | **0.77** (0.55–0.83) |
| unicode_strings | **0.89** (0.68–1.14) | 1.47 (1.40–1.92) | **0.96** (0.93–1.06) | 2.64 (2.12–3.21) |
| escaped_strings | **0.82** (0.77–0.82) | **0.79** (0.77–0.83) | **0.31** (0.30–0.32) | **0.31** (0.30–0.32) |
| int_array | **0.86** (0.83–1.03) | **0.96** (0.78–1.19) | **0.97** (0.92–1.07) | **0.91** (0.86–1.18) |
| float_array | **0.79** (0.67–0.86) | **0.78** (0.78–0.95) | 1.08 (0.86–1.14) | 1.05 (0.70–1.11) |
| **geomean** | **0.86** | **0.94** | **0.73** | **0.89** |

- **Headline** (README): `loads` 1.17× and `dumps` 1.36× faster than orjson on the geomean; 18 of 20 `loads`/`dumps` medians below 1.00. The two above (citm `loads` 1.06, float array `dumps` 1.08) have per-run ranges straddling 1.00 on this host.
- **`loads_str` is a trade-off, not parser speed.** Before PR #11 the corpus benchmark parsed the same `str` repeatedly and rjson's first call attached a UTF-8 copy to it, like orjson does. Since #10 a non-ASCII `str` of ≥ 4096 characters is re-encoded into a temporary buffer every call, so nothing doubles the caller's string; parsing that same object again then costs the encode each time (twitter 1.17, unicode strings 1.47). With `bytes` or a new `str` per call (a server decoding request bodies) rjson is faster: twitter, plain build, µs per call — bytes 1134 vs orjson 1241, new `str` per call 2029 vs 2402. The benchmark now times `loads` on `bytes` and reports `loads_str` separately.
- **Plain builds are layout-sensitive.** In plain (non-PGO) builds canada `dumps` measured 1.39–1.55 on `main` against 0.90–1.04 before PR #11, with the same instruction count (callgrind: 596M vs 594M for 5 calls) and no extra page faults or syscalls. Bisecting gave non-monotonic results (18d679f 1.03, the following `loads`-only commit 1.42–1.52), and `-C llvm-args=-align-loops=32` (or `-x86-branches-within-32B-boundaries`) brought canada back to 1.01 while moving other cases the other way (escaped strings `dumps` 0.33 → 0.41, citm `loads` 0.89 → 1.16). PGO builds do not show it (canada `dumps` 1.08 before → 0.98 after), so the published wheels are unaffected; when comparing plain builds, compare geomeans, not single cases.
- **Stack headroom checks** (production hardening): `loads`/`dumps` compare the depth with 16 on entering a container (it was the nesting limit) and only past that check the limit and, every 8 levels, the thread's stack. Instructions per call against `main`: `loads` twitter +0.04%, citm −0.2%, tiny +6; `dumps` twitter −0.1%, citm −0.2%, tiny −3. A first version checking `depth % 8` on every container cost +8% on citm `dumps` (tens of thousands of small containers).
- **Small `dumps` after a big one** ([#13](https://github.com/TinDang97/rjson/issues/13), fixed after these tables): the first growth of a small result (its worst-case string reservation) jumped to the recent peak size, ~935 KB, then copied 150 B out of it. The jump now waits until the output needs max(peak/64, 4 KiB). `percall/small_after_big` with PGO wheels: 1.23–1.31 → 0.79–0.92 (`dumps_str` 1.43–1.54 → 0.91–1.07); steady-state instruction counts unchanged. Details in PRODUCTION_READINESS.md, Performance fixes item 2.
- **PGO, before vs after PR #11** (2 interleaved runs, `loads` on `str` as the benchmark then did): `dumps` geomean 0.71–0.75 → 0.65–0.73, `dumps_str` 0.91–0.92 → 0.84–0.88, `loads` unchanged except the `str`-reuse cases above (twitter 0.81 → 1.44, unicode strings 1.02 → 1.57, citm 0.83 → 1.08).

### First review round (historical)

Numbers are rjson time divided by orjson time on the same run, so **below 1.00 means rjson is faster**. They come from `benches/corpus_benchmark.py --repeat 11` on an x86_64 Xeon (4 cores, otherwise idle) against orjson 3.12.0, with **plain release builds (no PGO)** of the current branch. Treat single cells as ±5–10% and trust the geomeans. `dumps` returns `str`; `dumps_bytes` returns `bytes`, the same type `orjson.dumps` returns.

| case | loads before | loads 3.11 | loads 3.13 | dumps before | dumps 3.11 | dumps 3.13 | dumps_bytes 3.11 | dumps_bytes 3.13 |
|---|---|---|---|---|---|---|---|---|
| twitter | 1.91 | **0.66** | **0.68** | 3.05 | 1.04 | 1.09 | **0.70** | **0.67** |
| citm_catalog | 1.62 | **0.30** | **0.89** | 1.91 | **0.92** | **0.88** | **0.81** | **0.79** |
| canada | 1.20 | **0.31** | **0.88** | 2.23 | **0.95** | **0.87** | **0.89** | **0.88** |
| github | 1.70 | **0.71** | **0.69** | 1.93 | **0.66** | **0.55** | **0.61** | **0.52** |
| small_dict | 1.38 | **0.77** | **0.73** | 1.75 | **0.69** | **0.76** | **0.67** | **0.78** |
| unicode_strings | 1.43 | **0.75** | **0.79** | 25.6 | 2.25 | 2.30 | **0.99** | **0.98** |
| escaped_strings | 1.51 | **0.91**¹ | **0.91** | 1.22 | **0.39** | **0.38** | **0.39** | **0.39** |
| int_array | 1.36 | **0.89** | **0.87** | 2.06 | **0.76** | **0.88** | **0.76** | **0.91** |
| float_array | 1.26 | **0.91** | **0.97** | 2.33 | **0.77** | **0.82** | **0.77** | **0.80** |
| records | 1.60 | **0.45** | **0.80** | 2.10 | **0.66** | **0.63** | **0.65** | **0.63** |
| **geomean** | **1.48** | **0.63** | **0.81** | **2.59** | **0.82** | **0.82** | **0.71** | **0.71** |

"Before" is the pre-review code on 3.11. ¹ Median of three re-runs (0.90–0.92); the full run's first sample read 1.01.

On CPython 3.13, `loads` and `dumps_bytes` beat orjson on all 10 cases. `dumps` → `str` beats it on 8 of 10; the two exceptions are explained below and in §4 item 1.

### dumps, second round (branch `wip-dumps`): per-change measurements

Plain release builds (no PGO), same host. Measured by interleaving `rjson.dumps`, `rjson.dumps_bytes`, `orjson.dumps` and `orjson.dumps(x).decode()` in each round and taking the best of 21 rounds, which is less noisy than the median-of-7 in `corpus_benchmark.py`. The cells are the mean of two runs. The last column compares `dumps` with the like-for-like `str` result from orjson.

| case | 3.11 str before → after | 3.11 bytes before → after | 3.13 str before → after | 3.13 bytes before → after | 3.13 str vs `orjson.dumps().decode()` |
|---|---|---|---|---|---|
| twitter | 1.57 → 1.12 | 0.95 → **0.66** | 1.60 → 1.14 | 0.95 → **0.67** | **0.54** |
| citm_catalog | 1.15 → **0.85** | 1.01 → **0.77** | 1.14 → **0.90** | 1.01 → **0.80** | **0.68** |
| canada | 0.89 → **0.84** | 0.89 → **0.84** | 0.91 → **0.84** | 0.91 → **0.83** | **0.76** |
| github | 0.98 → **0.69** | 0.91 → **0.62** | 0.98 → **0.70** | 0.91 → **0.63** | **0.56** |
| small_dict | 0.74 → **0.71** | 0.71 → **0.68** | 0.80 → **0.73** | 0.82 → **0.72** | **0.50** |
| unicode_strings | 4.64 → 2.32 | 1.07 → **0.98** | 4.51 → 2.24 | 1.08 → **0.97** | **0.12** |
| escaped_strings | 0.37 → **0.40** | 0.38 → **0.39** | 0.37 → **0.39** | 0.37 → **0.38** | **0.36** |
| int_array | 1.02 → **0.89** | 1.03 → **0.89** | 1.05 → **0.85** | 1.06 → **0.85** | **0.76** |
| float_array | 0.84 → **0.76** | 0.84 → **0.77** | 0.88 → **0.78** | 0.87 → **0.78** | **0.74** |
| records | 1.00 → **0.65** | 0.99 → **0.65** | 0.98 → **0.66** | 0.98 → **0.66** | **0.61** |
| **geomean** | 1.06 → **0.83** | 0.85 → **0.70** | 1.07 → **0.83** | 0.87 → **0.71** | **0.51** |

`dumps` → `str` is still above 1.0 on twitter and unicode_strings. Most of what is left is memory traffic that comes from the return type: five emoji make twitter's 403k-character result UCS4 (1.6 MB, against 467 KB of UTF-8), and filling it takes about 100 µs of the 248 µs. Without the fill, str mode is faster than bytes mode.

### loads, second round (branch `wip-loads`): per-change measurements

Median of 3 interleaved in-process runs (old `.so` vs new `.so` vs orjson), no PGO.

| case | 3.11 before → after | 3.12 before → after | 3.13 before → after |
|---|---|---|---|
| twitter | 0.79 → 0.77 | 0.87 → 0.83 | 0.89 → 0.83 |
| citm_catalog | 0.62 → 0.59 | 0.94 → 0.90 | 0.94 → 0.89 |
| canada | 0.61 → 0.51 | 1.04 → 0.92 | 1.05 → 0.91 |
| github | 0.74 → 0.70 | 0.76 → 0.71 | 0.76 → 0.70 |
| small_dict | 0.77 → 0.76 | 0.77 → 0.75 | 0.74 → 0.74 |
| unicode_strings | 0.75 → 0.80 | 0.78 → 0.81 | 0.76 → 0.76 |
| escaped_strings | 1.40 → 0.90 | 1.41 → 0.94 | 1.42 → 0.90 |
| int_array | 1.06 → 0.92 | 1.02 → 0.97 | 1.04 → 0.91 |
| float_array | 1.15 → 0.90 | 1.17 → 0.94 | 1.22 → 0.94 |
| records | 0.69 → 0.64 | 0.92 → 0.85 | 0.91 → 0.82 |
| **geomean** | 0.83 → 0.74 | 0.95 → 0.87 | 0.95 → 0.84 |

unicode_strings got about 5% slower on 3.11/3.12 (flat on 3.13); bisecting points at a commit that doesn't touch that path, so it is most likely code layout.

**The 3.11 loads numbers include a garbage-collector pause.** CPython 3.10 and 3.11 run cyclic-GC passes while a large document is being built. `loads` pauses the collector for the duration of the call and restores its previous state afterwards (§5, decision 2). CPython 3.12+ already defers collection until the call returns, so the 3.13 column is the like-for-like parser comparison.

**Other metrics** (from the build review):

| metric | before | now | orjson |
|---|---|---|---|
| `.so` size | 4.7 MB (debug info) | 0.48 MB | 0.24 MB |
| import time | 0.42 ms | ~0.4 ms | 19.7 ms |
| `loads` of 300k records, resulting memory | 420 MB | 193 MB | 192 MB |
| `dumps` of a 79 MB output, peak memory | 3N | ~1N (writes into the result object) | 1N |
| memory held after a large `dumps` | 77 MB per thread, forever | 0 | 0 |
| per-call time, `loads('1')` / `dumps(None)` | 41 / 55 ns | ~25 / ~35 ns | 67 / 53 ns |
| Python versions verified | 3.11 only (3.12+ crashed) | 3.10–3.14 (3.14.0rc2) x86_64; 3.9, 3.12 aarch64 (qemu); `requires-python >=3.10` | 3.9–3.14 |

## 2. Correctness and safety defects found (all fixed on this branch)

Every one of these has a regression test in `tests/`.

| severity | defect | where it was |
|---|---|---|
| critical | `dumps` read string data at a hard-coded offset of 48 bytes. That is only valid up to 3.11: on 3.12 it aborted and on 3.13 it silently emitted garbage. | `lib.rs` `ASCII_DATA_OFFSET`, `bulk.rs` |
| critical | Heap buffer overflow in the SIMD escaper. It reserved `len + 64` bytes, but escaping can write up to `6 * len`. | `simd_escape.rs` |
| critical | Homogeneous-list fast paths checked only the first 16 elements. `[1]*16+[True]` became `…,1]`, `[1.0]*16+[7]` became `7.0`, and later elements could be read as the wrong object type. | `bulk.rs` |
| critical | Dict keys that are str subclasses were serialized as garbage. | `lib.rs` key path |
| critical | No recursion limit: a circular or deeply nested structure segfaulted. Now `rjson.JSONEncodeError` (a `TypeError` and `ValueError`) at depth 254, as in orjson. | `dumps`, `dumps_bytes`, `loads_simd` |
| critical | `.cargo/config.toml` forced `target-cpu=native` plus AVX2. Wheels contained AVX-512 instructions and would crash with SIGILL on most CPUs. | build config |
| high | `dumps_bytes` leaked its whole buffer on every call (a `mem::forget` after the bytes had already been copied). It also wrote `-2**63` as `-` and segfaulted on huge ints. | `extreme.rs` |
| high | A lone surrogate left a Python exception set, which surfaced as `SystemError`. | `lib.rs` |
| medium | `loads("[1] x")` returned `[1]`: trailing content was accepted. | serde path |
| medium | About 11% of canada.json's floats parsed 1 ulp off; `"-0"` became `-0.0`; ints beyond 64 bits became lossy floats; the nesting limit was 128. | serde defaults |
| medium | Subclasses (IntEnum, OrderedDict, namedtuple, …) were rejected. stdlib and orjson accept them. | type dispatch |

## 3. What changed and why it is fast

### loads (`src/parser.rs`, `src/lemire.rs`)

Dropping serde for a hand-written single-pass parser took the geomean from 1.48 to 0.98 (3.13). What mattered most, in order of measured impact:

1. **Dict-key cache.** Repeated keys reuse one `PyUnicode` whose hash is already computed; lookups compare bytes, so hash collisions are safe. Worth 25–30% on record-shaped documents, and it fixes the memory bloat.
2. **GC pause on 3.10/3.11.** See §5, decision 2.
3. **One reusable value stack, kept across calls.** Lists are created at their exact size; there is no `Vec` per array and no `String` per key. Reuse saves about 100 ns per call on small documents.
4. **Faster numbers.** Integers are parsed 8 digits at a time. Floats use an exact fast path, then Eisel-Lemire, then `fast_float` for more than 19 digits, and are correctly rounded.
5. **Strings.** Our own UTF-8 decoder writes straight into the final `str`. `bytes` input is validated once up front with simdutf8.
6. **Whitespace.** SIMD skipping, plus an inline fast path for a single space.
7. **Error handling.** The error type is zero-sized, so results come back in registers.

Second round (branch `wip-loads`, results in §1):

8. **Escaped strings.** A 32-byte block kernel (SSE2, AVX2 detected at runtime) handles every escape in a block from one bitmask and decodes `\u` escapes and surrogate pairs inline; errors and the last 64 bytes go to the scalar tail, so messages and positions are unchanged (escaped_strings 1.40 → 0.90).
9. **Numbers.** A one-pass fast path for `-?d{1,15}(.d{1,15})?([eE]…)?` with at most 19 digits and a cut-down inlined Eisel-Lemire; every other shape and every error falls back to the unchanged general parser. Verified against `float()` on canada's 111k numbers and ~1.5M random shapes (float_array 1.15–1.22 → 0.90–0.94).
10. **Allocation.** On 3.13, dicts are built with `_PyDict_FromItems` (exported but private; gated to 3.13), which presizes and uses the compact str-only key table. Lists get a `PyMem_Malloc` item array on `PyList_New(0)`, skipping calloc zeroing; fast-path floats use `PyObject_Malloc` + `PyObject_Init` on 3.13+.
11. **Key cache and bounds checks.** One folded multiply for the key hash, 16-byte compares instead of `memcmp`, and `peek()` without bounds checks by relying on the NUL byte after str/bytes/bytearray data (memoryview input is copied with a NUL appended). valgrind memcheck is clean.
12. **Shape cache** (3.11–3.13; design in ARCHITECTURE.md §3.3). Building dicts was the largest remaining `loads` cost on records. Parsing 20k 8-key records took 8.4 ms, the same values as arrays 3.7 ms, so keys and dicts were 56% of the time. The key cache returns the same key objects, so an object's key pointers identify its shape. A shape that repeats gets a template dict (keys → `None`); later objects of that shape are `PyDict_Copy(template)` (one allocation plus a `memcpy` of the key table) with the values written into the copy's entries, using the layout `dumps` already reads and its self-test. The table has 64 slots, and a shape becomes a slot's template the second time it is seen in a row.

    Plain release builds, rjson before → after, best of 5 interleaved process runs (`records_*`: synthetic records; `log_lines`: 3,000 one-line documents, one `loads` each; `unique_shapes`: every object a random key set, the worst case):

    | case | 3.11 | 3.12 | 3.13 |
    |---|---|---|---|
    | records, 3 keys | 0.93 | 0.81 | 0.83 |
    | records, 8 keys | 0.83 | 0.80 | 0.85 |
    | records, 30 keys | 0.60 | 0.63 | 0.76 |
    | nested orders | 0.82 | 0.84 | 0.88 |
    | log_lines | 0.92 | 0.86 | 0.89 |
    | twitter | 0.91 | 0.87 | 0.91 |
    | citm_catalog / canada / github | 0.99–1.01 | 0.98–0.99 | 0.96–1.00 |
    | unique_shapes | 0.97 | 0.98 | 1.02 |

    Against orjson (`corpus_benchmark.py --repeat 7`, 3.13 plain build, best of 2 runs), the `loads` geomean went from 0.85 to 0.76: twitter 0.87 → 0.64, github 0.74 → 0.55, records 0.87 → 0.68, small_dict 0.82 → 0.69, citm 0.94 → 0.84. The other cases have no dicts and moved within noise. The 0.4.0 PGO numbers are in §1.

    Memory: copies have the compact str-key table that `json.loads`'s dicts have. On 3.11/3.12 objects with more than 8 keys used to get `_PyDict_NewPresized`'s generic table; results with 12 and 30 keys per record are now 16% smaller (a 12-key dict: 632 → 464 B). Nothing is larger than before.

    Tried first and dropped: copying the template and then setting each value with `PyDict_SetItem`, which was 7–9% faster on records but 8–15% slower on twitter/citm/github; and a single-slot cache, which kept replacing its template on mixed documents (twitter/citm up to 13% slower).
13. **`loads_ndjson`** (design in ARCHITECTURE.md §3.7). NDJSON was parsed with a Python loop, `[loads(line) for line in data.splitlines()]`, which pays per line for a `bytes` object, a call, input setup and a GC pause: 60-byte event lines took ~650 ns each, about half of it overhead. One call now parses every line with one parser, and anything unusual goes through `loads(line)` itself, so results and errors are unchanged.

    rjson ÷ orjson time, each library's per-line loop vs `loads_ndjson` (plain builds, best of 15; 200-byte log lines ×50k, 60-byte events ×100k, 700-byte API records ×8k, mixed shapes ×60k):

    | case | loop 3.11 | ndjson 3.11 | loop 3.12 | ndjson 3.12 | loop 3.13 | ndjson 3.13 |
    |---|---|---|---|---|---|---|
    | logs | 0.70 | 0.46 | 0.72 | 0.44 | 0.75 | 0.47 |
    | events | 0.73 | 0.40 | 0.76 | 0.41 | 0.76 | 0.39 |
    | API records | 0.44 | 0.41 | 0.79 | 0.53 | 0.82 | 0.70 |
    | mixed shapes | 0.78 | 0.46 | 0.83 | 0.52 | 0.76 | 0.51 |

    `loads_ndjson` is as fast as parsing the same records as one JSON array; what is left is building the objects. Plain `loads` is unchanged. `get_input` got a second caller, and LLVM stopped inlining it into `loads` (+20 instructions per call, valgrind); with `#[inline(always)]` `bytes` input is back to +4 (within 0.2%) and `str` input 72 instructions faster than before.

    The command line's `--json-lines` uses it in 16 KiB chunks. On 200k records (CPython 3.11, whole process, best of 5): `--validate` 0.23 → 0.17 s, file to file 0.48 → 0.42 s, stdin to stdout 0.58 → 0.52–0.57 s, with identical output. Chunk size mattered more than the call itself: 1 MiB chunks made the streamed path 15% *slower* than the per-line loop, because each chunk's ~7,000 documents were built together and were out of cache by the time they were written and freed. Parsing time by chunk: 4 KiB 116 ms, 16 KiB 117 ms, 64 KiB 140 ms, 256 KiB 151 ms, 1 MiB 203 ms (per-line loop: 175 ms).

### dumps (`src/ser.rs`)

1. **Write directly into the result object.** Output goes into a `bytes` object or a compact ASCII `str`. The old path decoded the whole output as UTF-8 again and cloned the buffer. For non-ASCII `str` output, each source string's native UCS1/2/4 data is copied into a result of exactly the right kind. This took unicode_strings from 25.6× to 5×.
2. **Dispatch.** Types are matched by exact `ob_type` pointer on raw borrowed pointers; there is no PyO3 refcount traffic per element. The per-list `detect_array_type` scan is gone.
3. **Floats.** Formatted in place with zmij, byte-identical to orjson (positive exponents are now `1e+16`, as `repr` writes them). Ints are read inline and formatted with itoap.
4. **Escaping.** AVX-512VL/AVX2 kernels selected at runtime, an SSE2 baseline, and exact worst-case reservation. escaped_strings is 0.33× orjson.
5. **Build target.** `x86-64-v2`. On the benchmark host, `native`/v3 made zmij about 1.6× slower because of the BMI2 code it generates.

Second round (branch `wip-dumps`, results above):

6. **Cursor in a register.** Writers take the output cursor and return the new one, and `Out::len` is only synced on growth. Errors are a null cursor: `Result<*mut u8, _>` does not fit in one register, and LLVM spilled it to the stack where code paths merge.
7. **Direct dict iteration** on 3.11–3.13 in place of `PyDict_Next`. This was the biggest win (twitter bytes 0.88 → 0.66, records 0.89 → 0.66). The layout is private, so it has a build-time gate and an import-time self-test; see CLAUDE.md.
8. **Lists.** Runs of exact ints and floats are written by a loop that keeps the item array and the capacity limit in registers and checks the exact type of every item (int_array 1.09 → 0.85). Compact ASCII strings of up to 16 bytes are written inline with one SSSE3 shuffle, because the 16 bytes that end at the string's end lie inside the object.
9. **The bytes-slower-than-str anomaly was glibc page faults.** Output buffers were allocated with 1/8 headroom and then shrunk with realloc on every call. Freeing the shrunk block only raises glibc's dynamic mmap threshold to the shrunk size, so every later (larger) request was mmapped again and page-faulted its whole output. Measured: 390 faults per call, and 571 µs instead of 144 µs for a 1.6 MB result. Whether this happened depended on what the process had freed before, which is why citm, int_array and canada ratios moved between runs. The headroom is now 1/16, below the 1/8 shrink threshold, and the size hints are kept per mode.
10. **Unsplit 256-bit loads and stores.** The generic x86-64-v2 tuning makes LLVM split every unaligned 256-bit access, even inside AVX2/AVX-512 functions. The escape kernels now use `vmovdqu` through inline asm.
11. **Non-ASCII `str` output.** The escape scan narrows UCS2/UCS4 to bytes with saturating packs, checking 64 or 128 bytes per test. The check happens while the result is filled, so each string is read from cache and large same-kind strings are copied and checked in one pass. Escaping is done during the fill instead of by building a temporary `str`. unicode_strings went from 5.0× to 2.2×.
12. **Large strings.** Strings over 64 KiB are escaped in pieces sized to the room left in the buffer. The old code reserved 6× per chunk, which forced a doubling realloc.

Tried and reverted, because each measured slower: SWAR digit formatting for all ints (on small ints the multiply chain costs more than the predicted branches it replaces), an inline 17–32-byte string path (code growth in the dict loop), inlining nested small lists (no gain on canada), and AVX2 widening for the `str` fill (memory-bound).

### Build and entry points (`src/entry.rs`, `Cargo.toml`, `scripts/`)

- **Raw entry points** (`METH_O` for `loads`; `METH_FASTCALL|METH_KEYWORDS` with a one-compare fast path for `dumps`/`dumps_str`, since `default=`) replace `#[pyfunction]`, saving about 8 ns per call. They keep PyO3's trampoline so panics are caught and PyO3's GIL bookkeeping stays correct.
- **No debug info in release builds**, which shrinks the `.so` about 10×.
- **All ~3.6k lines of the old `src/` replaced** (including dead or slower code: `lib_backup.rs`, `extreme.rs`, `simd_parser.rs`/`loads_simd`, `bulk.rs`, `type_cache.rs`, the old escaper) by ~3.7k lines in five files. The serde, simd-json, ahash, smallvec and memchr dependencies went with it.
- **`scripts/build_pgo.sh`** does instrument → train (`scripts/pgo_train.py`) → merge → rebuild, one profile per interpreter. The first round's "3.11 PGO" numbers (since replaced in §1 by plain release builds) came from an earlier version of the script that had two flaws: it trained on the benchmark itself (corpora and the benchmark's synthetic cases), and it passed the PGO flags through `RUSTFLAGS`, which silently replaced `.cargo/config.toml`'s `target-cpu=x86-64-v2`, so those wheels were baseline x86-64. Both are fixed: flags go through `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` (merged with the config), and training uses seeded synthetic documents of other shapes that read no corpus file.

  **PGO gain, re-measured** (geomean of rjson/orjson over the 10 benchmark cases vs a plain release build; 5 interleaved rounds, median per case; CPython 3.13 / 3.11; noisy shared host, so ±2–3% on a geomean is noise):

  | profile | loads | dumps | dumps_bytes |
  |---|---|---|---|
  | disjoint synthetic training (current) | −0.2% / −2.2% | −0.2% / −2.2% | −4.3% / −2.6% |
  | old, trained on the benchmark | 0.0% / −3.7% | −3.7% / −3.8% | −5.0% / −4.6% |

  PGO is worth about 0–4% once it cannot see the benchmark, not the −7 to −10% measured before. Most of the difference is on cases the old profile trained on verbatim: int_array `dumps` on 3.11 is 0.83× orjson with the old profile, 1.00× plain and 1.01× with the disjoint one. A first draft of the disjoint set, whose documents were almost all non-ASCII, made citm `dumps` (pure ASCII `str` output) 20% slower; the current set is mostly ASCII, like most real JSON.

## 4. Remaining gaps, ranked

| # | gap (3.13, no PGO) | idea | expected | risk |
|---|---|---|---|---|
| 1 | `dumps` → `str` with non-ASCII text: now unicode_strings 2.2×, twitter 1.1× (was 5.0× and 1.5×) | The rest is the memory traffic of a UCS2/UCS4 result. Options: (a) make `dumps` return `bytes` (decision 1), or (b) non-temporal stores for multi-MB results, which help benchmarks but not real consumers. | (a) all cases below 1.0 | (a) API |
| 2 | ~~`dumps_bytes` slower than `dumps` → `str`~~ **fixed**: glibc mmap-threshold page faults from the buffer shrink (§3 dumps item 9) | — | — | — |
| 3 | ~~`loads` escaped_strings 1.46×~~ **done**: 0.91× (block kernel, §3 loads item 8) | — | — | — |
| 4 | ~~`loads` float_array 1.13×, canada 1.02×~~ **done**: 0.97× / 0.88× on 3.13. What is left is mostly `PyFloat`/`PyLong` allocation and `pos` living in memory across the array loop | keep `pos` in a local across the array loop | −3–5% | low |
| 5 | ~~`loads` on 3.12+ at parity on twitter/records/canada~~ **done**: 0.68 / 0.80 / 0.88 on 3.13. Next: a SIMD UTF-8 → UCS2/UCS4 decoder (non-ASCII decoding is ~10% of twitter). CPython 3.13 runs a young-generation GC right after `loads` returns that walks every new list (~3 ms of canada's ~8 ms, for both libraries); it can't be avoided without untracking lists, which could leak user-made cycles | SIMD UTF-8 decode | −5% on non-ASCII docs | medium |
| 6 | ~~`dumps` int_array 1.21×~~ **done**: 0.85× with the per-item-checked list loop. Next: canada/float_array spend about 60% of their time in zmij, which orjson uses too | a faster shortest-float formatter | −10–20% on float-heavy docs | medium |
| 7 | Release wheels without PGO | **Done** (`.github/workflows/wheels.yml`): PGO wheels per interpreter for manylinux2014/musllinux x86_64+aarch64, macOS arm64/x86_64, Windows, trained on a workload disjoint from the benchmark. | −0–4% (re-measured, §3) | build-only |
| 8 | Non-x86 | The scalar/SWAR fallbacks now pass the full suite on aarch64 (3.9, 3.12 under qemu; CI runs native arm64 Linux and macOS). NEON kernels would replace the scalar loops at the four `#[cfg(target_arch = "x86_64")]` SSE2 sites in `parser.rs` (`skip_ws_slow`, `scan_special`, the escaped-string copy loop, `utf8_count_and_max`) and the SWAR `escape_long` / scalar `kind_needs_escape` in `ser.rs`; each maps to `vceqq_u8` + a narrowing-shift movemask. Needs native arm64 hardware to measure. | parity on Apple Silicon / Graviton | medium |

### Found by the production benchmark (`benches/production_benchmark.py`)

| # | gap | status |
|---|---|---|
| 9 | small `dumps` after a big one 1.7–2.0× (capacity hint = last size) | **fixed**: hint = min of last two sizes |
| 10 | big `dumps` peaked at 1.8× output, 16 MB stranded (doubling on the brk heap) | **fixed**: jump to recent peak, ≥ 32 MiB mmapped reservation past 1 MiB, shrink back (`Out::reserve`, `into_object`) |
| 11 | CJK/UCS-2 `loads` 1.3× (1.6× on 3.11) | **fixed** ([#8](https://github.com/TinDang97/rjson/issues/8)): `decode_ucs2` SIMD/pairwise decoder; 0.55–0.91× on CJK/hangul/Cyrillic. UCS-4 text: item 20 |
| 12 | mixed-magnitude float arrays `loads` 1.12–1.32× | **fixed** ([#9](https://github.com/TinDang97/rjson/issues/9)): full-precision doubles (16–19 fraction digits) fell off the fast path; now 0.89–0.95× on 3.11 and 3.13. Arrays dominated by `0.0` stay at ~1.05× (float allocation, not parsing) |
| 13 | UTF-8 cache attached by `dumps`, copies in `loads(str)` / `loads(memoryview)` | **fixed** ([#10](https://github.com/TinDang97/rjson/issues/10)): hybrid by size (short strings keep CPython's cached copy, long ones are encoded directly / via a temporary buffer); whole-object memoryviews parsed in place |

Numbers: [docs/PRODUCTION_READINESS.md](PRODUCTION_READINESS.md#performance-fixes).

### Found by the showcase (`benches/showcase.py`, [SHOWCASE.md](SHOWCASE.md))

rjson ÷ orjson time before the fix (plain build, CPython 3.13; below 1.00 is faster):

| # | gap | status |
|---|---|---|
| 14 | datetime/UUID/Enum values 0.95 (events), 0.73 (UTC datetimes): every non-builtin value re-checked `sys.modules` for modules not imported (`zoneinfo`) with a temporary `str` per lookup (~80 ns/value); every Enum member walked its MRO | **fixed**: lookups only when `len(sys.modules)` changes (+ forced refresh before "unsupported"); per-type facts cached by `tp_version_tag`. Now 0.43 / 0.32 (PGO) |
| 15 | dataclasses 1.64: every dataclass restarted the whole document in guarded mode; two `tp_dict` lookups per instance; `PyUnicode_ReadChar` per key | **fixed**: unguarded standard `__dict__` on 3.12+ via `PyObject_GenericGetDict`, cached facts. Now 0.92 (`slots=True`: 0.47) |
| 16 | one `dumps` per ~600 B record with many escapes, streamed: 1.09 (the same records as one document: 0.64); 0.4.1 showcase 0.99× | **fixed** ([#26](https://github.com/TinDang97/rjson/issues/26)): the output was a result object started at the smaller of the last two sizes and grown with a realloc when larger; with records of varying size (1-4 traceback frames) 7,477 of 10,000 calls grew, ~750 of their ~3,500 instructions (callgrind). The output hint variants tried before cut regrowth, not the realloc. Starting at the recent peak removed it (−17% instructions) but left the unused part of the allocation in each result (7.8× the payload on a kept mix of 150 B and 3.5 KB results; capping the start at 2× or 4× the recent size kept half the gain and 3.1× the memory), and copying such results into exact-size objects cost as much as the realloc. Now outputs whose recent peak is ≤ 4 KiB are written into a per-thread scratch buffer (`Scratch`, kept up to 64 KiB, taken by one call at a time: a nested `dumps` from `default=` uses the old path) and copied into an exact-size result; past 1 MiB a scratch output moves into a result object. Instructions per 2,000 records: 927k → 755k (orjson 835k); PGO showcase A/B vs the 0.4.1 wheel, 3 interleaved runs: per-record NDJSON 1.02 → 1.31× (1.29–1.33), 1,000 small responses 1.16 → 1.54×, geomean 1.60 → 1.63; a kept 145 B result holds 187 B instead of 382 B, the mixed workload 1.09× its payload instead of 1.52× |
| 17 | fresh process, per-record results kept: orjson takes one page fault per call (glibc trims/regrows around its buffers) | not rjson's gap: 0.24 (1–4 faults vs 1,976); gone once any output > ~128 KiB was freed |
| 18 | `indent=2` 0.48×, `sort_keys=True` 0.78× on twitter.json (PGO-less build, vs orjson's options) | open: `indent` is a second pass over the compact output (~9 instructions per input byte: a structural byte every few characters); matching orjson means writing the indentation in the serializer, i.e. multi-byte separators in every writer. `sort_keys` sorts each dict's items by key object before writing (`cmp_str`, memcmp for Latin-1 keys); the rest is the per-dict `Vec` |
| 19 | PGO builds differ from build to build: two `scripts/build_pgo.sh` builds of the same 0.4.0 commit gave 606 vs 503 µs on 2,000 emoji-heavy strings (`loads`, UCS-4 decode; 0.3.0: 514 µs), while plain builds were 496 µs | **fixed**: `scripts/pgo_train.py` ran each case for 0.2 s (`RJSON_PGO_SECONDS`), so call counts varied with host load (median 4.6%, up to 32% between two runs). It now makes a fixed number of calls per step (`CALLS`, calibrated from what 0.2 s did: median of 3 `--calibrate` runs on an instrumented 3.13 build, so the weights are unchanged). The remaining difference was the shape cache, whose slots come from object addresses: `build_pgo.sh` trains with `PYTHONHASHSEED=0` and, on Linux, address randomization off (`setarch -R`). Two builds now give identical profile counters and a byte-identical `.so`. Against the released 0.4.0 wheel (4 interleaved runs): `loads` geomean 0.81 → 0.82, `dumps` 0.69 → 0.68; unicode strings `loads` 0.82 → 0.89 with the same instruction count (1.270M vs 1.274M per call, lackey), i.e. a layout effect of the UCS-4 decode loop, fixed by item 20. `PROFILE_DIR=` keeps each merged profile for `llvm-profdata overlap` |
| 20 | Non-ASCII string decoding in `loads`: UCS-4 (strings with an emoji) at 0.79–0.93× orjson depending on the PGO build, emoji-heavy strings 1.1–1.3×; UCS-2 and Latin-1 fine in plain builds but ~20% slower in PGO wheels (UCS-2 0.79 vs 0.66). All three paths had per-character steps that branch on the character's length: the UCS-4 and UCS-1 ones entirely (`decode_into`), the UCS-2 one outside ASCII and CJK runs. They mispredict on mixed text, decoding was ~2/3 of the per-string time, and their speed moved with code layout (the UCS-1 loop got 18% slower when the other decoders changed) | **fixed**: block decoders for all three kinds (`decode_ucs1/2/4_ssse3`, out of line). Per 16-byte block, every byte's code point as a character end is computed at once (payload + up to 3 lookback payloads via `maddubs` and shifts; for UCS-1 one byte of lookback), then the real ends are packed 8 lanes at a time through a shuffle table (`COMPACT_U8`/`COMPACT_U16`, 2 + 4 KB). The cursor advances by 16. Run steps keep what was already fast: all-ASCII blocks, five 3-byte characters at a boundary (UCS-2; needs 8 output units, not 16, or short CJK strings lost up to 15 characters to the scalar tail and got 15% slower), four 4-byte characters (UCS-4). Tried and dropped: branch-free per-character decoding (2–3× slower: the next position waits on the character's load), a start-mask loop with scalar decode per start (1.4× slower), 4-lane compaction (10% slower than 8), a data-dependent advance to stay at character boundaries (lost most of the gain on mixed text). PGO wheel vs released 0.4.0 (4 interleaved corpus runs): unicode_strings `loads` 0.82 → 0.61, `loads_str` 1.29 → 1.08, `loads` geomean 0.82 → 0.78, `dumps` unchanged. 2,000 distinct 18–120 character strings per script: French 0.66 → 0.46, Cyrillic 0.73 → 0.52–0.57, Greek 0.74 → 0.57–0.58, hangul 0.88 → 0.72, CJK with ASCII 0.85 → 0.73, pure CJK 0.84 → 0.74, Japanese 0.82 → 0.72, tweet with an emoji 0.69 → 0.64. 2,000 × 120 characters: Latin-1 0.56 → 0.31, emoji only 1.07 → 0.67, CJK + emoji 1.09 → 0.87. PGO and plain builds now agree on all of these (within 3%); the release wheel (§1, 0.4.1) differs on pure CJK only (0.84, no gain) |
| 21 | twitter.json `dumps_str` 5–8% slower in the first 0.4.1 wheel than in 0.4.0 (274 → 288–298 µs, interleaved A/B), per-record NDJSON `dumps` 3% (showcase 1.00× → 0.97×); the `dumps` code did not change and the instruction counts were equal | **`dumps_str` fixed**: only UCS4 results were affected (the same document without its emoji, or ASCII only, measured the same in both wheels), and a PGO wheel with the new training profile but the old decoders was already slower, so the profile, not the new code, moved it. The fill widens each ASCII run and each segment (twitter.json: 755 segments, runs of median 223 units) with `widen`, an auto-vectorized loop whose runtime overlap check, unroll factor and remainder the profile decided; very long or very short runs measured the same in both wheels. `widen_sse41` (1→2, 1→4, 2→4 bytes: 16-byte steps, one overlapping last step, 8/4-byte step pairs for short runs) made it independent of that: 0.4.0 255–260 µs, first 0.4.1 wheel 278–281, now 260–262 (the object built by `json.loads`); UCS2 results 217–224 → 212–218. Per-record NDJSON `dumps` (bytes, no fill) was ~2.5% slower than 0.4.0 (placement, fewer instructions); superseded by item 16's fix (1.31× orjson after it) |
| 22 | Repeated `dumps` of a long non-ASCII string (400,000 emoji, 1.6 MB of UTF-8) mapped a fresh 32 MiB block and page-faulted its whole output on every call (417 faults per call), since 0.3.0: `write_str`'s direct UTF-8 path reserved 6 bytes per unit up front (2.4 MB), more than the buffer sized from the previous result (1.7 MB), so every call took the large-growth path. `test_large_output_does_not_refault_every_call` passed in CI only when transparent huge pages happened to cover the mapping (its start is not 2 MiB-aligned); a PR that shifted the process's mappings failed it on 3.14 and arm64 | **fixed**: `encode_utf8_chunked` encodes as much as the room surely holds (room / 6 units), and near the end reserves a per-unit bound (`utf8_escaped_bound`: 6 for controls, 2/3/4 by code point) for the rest, as `escape_chunked` does for UTF-8 input. 417 → 0 faults per call, one 32 MiB mapping per process instead of one per call (strace) |

### Threading and I/O (researched, mostly not applicable)

- **io_uring: rejected.** `loads`/`dumps` do no I/O. Even reading a file from the page cache is 1–4% of parse time: canada.json takes 0.2 ms to read and 14 ms for orjson to parse. It only matters for a bulk-ingestion CLI.
- **Thread-per-core for single calls: rejected.** Python objects can only be created or read under the GIL. The only work that could run in parallel is scanning and validating the text, which is a small share of `loads` (Amdahl). A thread hand-off costs microseconds, while typical documents take 0.4–80 µs.
- **Parallel `dumps` for very large documents: experimental, worth trying.** While the caller holds the GIL and waits, worker threads each serialize a slice of a large top-level list or dict into its own buffer, and the buffers are concatenated. Workers do pure reads with no refcounting and no calls into Python, and non-ASCII strings are encoded from their raw data. Documents under about 1 MB use one thread. This could beat orjson on multi-MB payloads such as canada. It needs a different design for free-threaded builds.
- **Releasing the GIL during `loads`: throughput feature.** Validate or tokenize with the GIL released, then build objects with it held. Single calls get no faster, but other threads in a multi-threaded server can run meanwhile.

### Platform and tooling

- **PyO3 upgrade: done (0.24 → 0.29).** Needed for 3.14, since pyo3-ffi 0.24 refuses to build for it. `src/compat.rs` declares `_PyBytes_Resize` / `_PyDict_NewPresized` (no longer re-exported by pyo3-ffi, still exported by CPython) and provides the str accessors: on 3.14, pyo3-ffi drops the inline `PyUnicode_IS_*ASCII` readers and makes `KIND`/`DATA` out-of-line calls, so compat.rs reads the GIL build's unchanged `state` bitfield itself behind an import-time self-test against libpython, and refuses to compile for `Py_GIL_DISABLED`. All 247 tests pass on 3.9–3.14.0rc2. Private symbols and layouts in use, and their version gates, are listed in CLAUDE.md.
- **Free-threaded builds (3.13t/3.14t).** Keep the module marked as GIL-requiring. The serializer iterates lists and dicts through borrowed references and would need critical sections before it could drop that. orjson doesn't support free-threading either.
- **CI.** `.github/workflows/ci.yml`: clippy, then build + pytest on 3.10–3.14 (ubuntu x86_64), a no-AVX-512 variant, ubuntu-24.04-arm (3.10, 3.13), macos-14 and Windows (3.13). `cargo fmt --check` is not enforced yet (`entry.rs` and `parser.rs` are not rustfmt-clean) and clippy warnings are not fatal (one in `ser.rs`; five more dead-code/unused warnings only on aarch64).
- **Performance regression gate.** `.github/workflows/perf.yml` (PRs labelled `perf`, or manual): builds base and head in one job, runs `corpus_benchmark.py --output-json` for both, interleaved ×5 on 3.11 and 3.13, and `benches/perf_gate.py` fails if any geomean is more than 5% worse. Corpora come from `benches/fetch_corpus.sh` (sha256-pinned). On this dev host, two runs of the same build differ by 1–3% in geomean, so 5% with 5 rounds is about the floor. Still to add: per-call time on tiny documents, `.so` size, import time and peak memory.
- **Fuzzing.** Both review rounds ran differential fuzzers against stdlib `json` (≈150k `dumps` cases, plus `loads` value/error/float fuzzing), but the scripts live outside the repo. Next: check them in under `tests/fuzz/` and run a random-structure fuzzer under ASan in CI.
- **Feature parity.** `default=` is done (entry is `METH_FASTCALL|METH_KEYWORDS`; per-call cost unchanged, containers +~8 instructions for the mode check). Native datetime/date/time, UUID, dataclass and Enum are done (issue #5, `src/native.rs`), resolved lazily from `sys.modules` (no import cost), with no change in instructions for documents without them. orjson also offers indent, sorted keys and numpy. Add them as further hand-parsed keyword names.
- **Native types vs orjson** (CPython 3.13, rjson time ÷ orjson time, same process, 1,000 values unless noted): naive datetimes 0.92, UTC datetimes 0.29 (one-entry offset cache for `datetime.timezone`), `ZoneInfo` datetimes 0.45 (its C `utcoffset` called through the method descriptor), dates 0.97, UUIDs 1.04 (slot read with `PyMember_GetOne`, digits read inline, table hex), Enums 0.41, 500 API rows with UUID/datetime/Enum 0.75–0.80, 500 dataclasses 0.70. The FastAPI example's `RJSONResponse` on 100 such rows went from ~700 µs (fallback through `jsonable_encoder`) to 14 µs.
- **`non_str_keys=True` vs orjson `OPT_NON_STR_KEYS`** (rjson time ÷ orjson time, CPython 3.13): 1,000 int keys 0.49, a `Counter` of ints 0.45, records with small int-keyed maps 0.52–0.55, 1,000 float keys 1.14–1.20, UUID keys 1.45, date keys 1.53; `json.dumps` is 7–10× slower than rjson where it accepts the keys. Float keys use `repr` text (as `json`), built from zmij's digits; `PyOS_double_to_string` was 5–6× slower than orjson. With the option off, instruction counts are unchanged on the corpora (tiny calls +6 instructions for the options struct).
- **`loads(..., lenient=True)`** (issue #7): `NaN`/`Infinity`, a UTF-8 BOM and overflow to `inf` are parsed natively, only on paths that are errors in strict mode (5,000 rows with `NaN`/`Infinity`: 1.9× faster than `json.loads`, the same time as strict rjson on numbers). Lone surrogates, UTF-16/32 and nesting beyond 1024 go through `json.loads`. `loads` became `METH_FASTCALL | METH_KEYWORDS` for the keyword: +29 instructions per call on tiny documents (≈1.8% of a 5-byte `loads`); the corpora are within layout noise (twitter +0.5% instructions, canada +0.05%; an unrelated one-line change moved twitter by 3%).

## 5. Decisions (resolved)

1. **`dumps` returns `bytes`** (like `orjson.dumps`). `dumps_str` returns `str`, and `dumps_bytes` stays as an alias of `dumps`. Callers that relied on `dumps` returning `str` move to `dumps_str` or `.decode()`. In the tables above, "dumps_bytes" is today's `dumps` and "dumps → str" is today's `dumps_str`.
2. **Private CPython internals are kept**, each gated to the versions checked and, where it reads a layout, self-tested at import (list in CLAUDE.md).
3. **Minimum Python is 3.10** (`requires-python >=3.10`; CI and wheels cover 3.10–3.14).
4. **GC pause in `loads` on 3.10/3.11** stays on by default; it restores the previous GC state and does nothing on 3.12+.
5. **Lone surrogates:** `dumps` (bytes) raises `UnicodeEncodeError`, as orjson does; `dumps_str` passes them through, as `json.dumps(ensure_ascii=False)` does.

## 6. Reproducing

```bash
uv venv .venv -p 3.11 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release
python -m pytest tests -q                      # 1119 tests
benches/fetch_corpus.sh                        # corpora -> benches/data/ (sha256-pinned)
python benches/corpus_benchmark.py [--json] [--output-json results.json]
python benches/make_charts.py results.json     # README charts -> docs/img/
scripts/build_pgo.sh python3.11 python3.13     # PGO wheels -> target/wheels/
python benches/perf_gate.py --base base-*.json --head head-*.json
scripts/test_aarch64_qemu.sh 3.10 3.12          # aarch64 cross-build + tests under qemu
```
