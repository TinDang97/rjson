# Security policy

rjson parses untrusted input in-process, in native code, so memory-safety and
denial-of-service bugs are treated as security issues.

## Supported versions

rjson is pre-1.0. Only the latest release (and `main`) receives fixes.

## Reporting a vulnerability

Please report privately through GitHub: **Security → Report a vulnerability**
on [TinDang97/rjson](https://github.com/TinDang97/rjson/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include the rjson and CPython versions, the platform, and an input or script
that reproduces the problem. You can expect an acknowledgement within 7 days
and a fix or mitigation plan within 30 days for confirmed issues; we will
credit you in the advisory unless you ask us not to.

In scope, for example:

- a crash (segfault, abort), memory corruption or a read of uninitialized or
  out-of-bounds memory reachable from `loads`/`dumps` input or options;
- resource exhaustion out of proportion to the input (CPU or memory);
- output that is not valid JSON, or `loads` accepting input `json` rejects in
  a way that changes its meaning.

Out of scope: code that passes a hostile `default=` callable or mutates
objects from other threads without the GIL (rjson is a GIL-only module).

## Hardening in place

- Nesting limits (`loads` 1024, `dumps` 254) and a thread-stack headroom
  check: deep documents raise `RecursionError` instead of overflowing small
  thread stacks.
- CPython's integer string conversion limit (`sys.set_int_max_str_digits`,
  CVE-2020-10735) applies to `loads` and `dumps`, as in `json`.
- Every write reserves its worst-case size first; every C-API result is
  checked (see the hard rules in [CONTRIBUTING.md](CONTRIBUTING.md)).
- CI runs differential fuzzing against `json` and the whole test suite on an
  AddressSanitizer build, plus thread and re-entrancy stress tests.
- Release artifacts are built in CI from pinned actions, published with PyPI
  trusted publishing and carry build provenance attestations
  (`gh attestation verify <file> --repo TinDang97/rjson`).
