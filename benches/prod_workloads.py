"""Deterministic, production-shaped JSON workload generators.

Used by ``benches/production_benchmark.py``. Every generator takes a
``random.Random`` (or a seed) so the same arguments always produce the same
objects: no network, no wall clock, no hash randomization dependence.

Shapes modelled:

* Web API: paginated REST list responses, a GraphQL-style nested response,
  ~300 B request bodies and small responses.
* Logs / events: flat structured-log records (a few percent non-ASCII or
  escape-heavy messages, e.g. tracebacks and Windows paths).
* Data pipelines: large record arrays, numeric arrays, high-cardinality and
  long (> 64 byte) dict keys that miss the parser's key cache.
* Cache / MQ codec: mid-size blobs of a target serialized size.
"""

from __future__ import annotations

import random
from datetime import datetime, timedelta, timezone
from typing import Any

# Bump when a generator changes so cached big-file fixtures are regenerated.
GENERATOR_VERSION = 1

_EPOCH = datetime(2024, 1, 1, tzinfo=timezone.utc)

FIRST = ["alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy",
         "mallory", "niaj", "olivia", "peggy", "rupert", "sybil", "trent", "victor", "walter"]
LAST = ["smith", "jones", "garcia", "miller", "davis", "martinez", "lopez", "wilson", "anderson",
        "thomas", "taylor", "moore", "jackson", "martin", "lee", "thompson", "white", "harris"]
# Non-ASCII names/text as seen in real user data: Latin-1, CJK, Cyrillic, emoji.
INTL = ["Zoë Müller", "José Álvarez", "François Dubois", "Søren Kierkegård", "Łukasz Wójcik",
        "山田太郎", "李小龍", "Дмитрий Иванов", "Ελένη Παπαδοπούλου", "محمد علي", "Nguyễn Văn An",
        "🚀 launch team", "café ☕ crew", "naïve résumé"]
CITIES = ["Springfield", "Riverside", "Franklin", "Greenville", "Bristol", "Clinton", "Fairview",
          "Salem", "Madison", "Georgetown", "Arlington", "Ashland", "Dover", "Oxford", "Jackson"]
TAGS = ["beta", "premium", "trial", "enterprise", "churn-risk", "vip", "internal", "partner",
        "eu", "us", "apac", "mobile", "web", "api", "legacy", "sso"]
LEVELS = ["DEBUG", "INFO", "INFO", "INFO", "INFO", "WARNING", "ERROR"]
LOGGERS = ["app.api.handlers", "app.db.pool", "app.auth", "uvicorn.access", "app.worker.tasks",
           "app.cache", "sqlalchemy.engine", "app.billing.stripe"]
PATHS = ["/api/v1/users", "/api/v1/orders", "/api/v1/orders/{id}", "/api/v1/search", "/healthz",
         "/api/v2/graphql", "/api/v1/cart/items", "/api/v1/sessions"]
MSG_TEMPLATES = [
    "request completed", "cache miss for key user:{id}", "db query took {ms} ms",
    "user {id} logged in", "retrying task {id} (attempt {n}/5)", "payment authorized for order {id}",
    "rate limit exceeded for client {id}", "connection returned to pool",
]


def iso(rng: random.Random, days: int = 900) -> str:
    """ISO-8601 UTC timestamp string with milliseconds, e.g. 2024-03-05T10:11:12.345Z."""
    dt = _EPOCH + timedelta(seconds=rng.randrange(days * 86400), milliseconds=rng.randrange(1000))
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


def hexid(rng: random.Random, n: int = 32) -> str:
    return f"{rng.getrandbits(n * 4):0{n}x}"


def uuid(rng: random.Random) -> str:
    h = hexid(rng, 32)
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"


def text(rng: random.Random, words: int) -> str:
    vocab = ["the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "lorem", "ipsum",
             "dolor", "sit", "amet", "service", "order", "shipping", "account", "update"]
    return " ".join(rng.choice(vocab) for _ in range(words))


# ---------------------------------------------------------------------------
# Web API
# ---------------------------------------------------------------------------

def api_user(rng: random.Random, i: int) -> dict[str, Any]:
    """One REST resource: nested address/prefs, nullable fields, mixed scalars."""
    first, last = rng.choice(FIRST), rng.choice(LAST)
    intl = rng.random() < 0.1
    return {
        "id": i,
        "uuid": uuid(rng),
        "type": "user",
        "username": f"{first}.{last}{rng.randrange(1000)}",
        "email": f"{first}.{last}@example.com",
        "display_name": rng.choice(INTL) if intl else f"{first.title()} {last.title()}",
        "created_at": iso(rng),
        "updated_at": iso(rng) if rng.random() < 0.7 else None,
        "last_login_at": iso(rng) if rng.random() < 0.8 else None,
        "is_active": rng.random() < 0.9,
        "is_verified": rng.random() < 0.6,
        "role": rng.choice(["member", "admin", "owner", "viewer"]),
        "score": round(rng.uniform(0, 100), 2),
        "balance": rng.uniform(-500, 25000),
        "login_count": rng.randrange(5000),
        "address": {
            "street": f"{rng.randrange(1, 9999)} {rng.choice(LAST).title()} St",
            "city": rng.choice(CITIES),
            "postal_code": f"{rng.randrange(100000):05d}",
            "country": rng.choice(["US", "DE", "FR", "JP", "VN", "BR"]),
            "geo": {"lat": rng.uniform(-90, 90), "lng": rng.uniform(-180, 180)},
        },
        "tags": rng.sample(TAGS, rng.randrange(0, 5)),
        "preferences": {
            "theme": rng.choice(["light", "dark", "system"]),
            "language": rng.choice(["en-US", "de-DE", "fr-FR", "ja-JP", "vi-VN"]),
            "notifications": {"email": rng.random() < 0.5, "sms": rng.random() < 0.2,
                              "push": rng.random() < 0.7},
        },
        "manager_id": rng.randrange(1, 10**6) if rng.random() < 0.4 else None,
        "avatar_url": f"https://cdn.example.com/avatars/{hexid(rng, 16)}.png" if rng.random() < 0.5 else None,
        "bio": (rng.choice(INTL) + " — " + text(rng, 8)) if intl else (text(rng, 12) if rng.random() < 0.3 else None),
    }


def api_page(seed: int, per_page: int = 50) -> dict[str, Any]:
    """Paginated REST list response (~50 resources, ~45 KB compact JSON)."""
    rng = random.Random(seed)
    page = rng.randrange(1, 200)
    total = rng.randrange(10_000, 50_000)
    return {
        "data": [api_user(rng, page * per_page + k) for k in range(per_page)],
        "pagination": {
            "page": page, "per_page": per_page, "total": total,
            "total_pages": -(-total // per_page),
            "next": f"https://api.example.com/v1/users?page={page + 1}&per_page={per_page}",
            "prev": None if page == 1 else f"https://api.example.com/v1/users?page={page - 1}&per_page={per_page}",
        },
        "meta": {"request_id": uuid(rng), "took_ms": round(rng.uniform(0.5, 80), 3),
                 "api_version": "2024-06-01", "cached": False},
    }


def graphql_response(seed: int, repos: int = 20) -> dict[str, Any]:
    """GraphQL connection/edges/node response, nesting depth ~9."""
    rng = random.Random(seed)

    def issue(n: int) -> dict[str, Any]:
        return {
            "__typename": "Issue", "id": "I_" + hexid(rng, 20), "number": n,
            "title": text(rng, 6), "state": rng.choice(["OPEN", "CLOSED"]), "createdAt": iso(rng),
            "author": {"__typename": "User", "login": rng.choice(FIRST) + str(rng.randrange(99)),
                       "avatarUrl": f"https://avatars.example.com/u/{rng.randrange(10**7)}?v=4"},
            "labels": {"nodes": [{"__typename": "Label", "name": t, "color": hexid(rng, 6)}
                                 for t in rng.sample(TAGS, rng.randrange(0, 3))]},
            "reactions": {"totalCount": rng.randrange(50)},
        }

    edges = []
    for _ in range(repos):
        lang = rng.choice([None, ("Python", "#3572A5"), ("Rust", "#dea584"), ("Go", "#00ADD8")])
        edges.append({
            "cursor": hexid(rng, 24),
            "node": {
                "__typename": "Repository", "id": "R_" + hexid(rng, 20), "name": text(rng, 1) + "-" + text(rng, 1),
                "description": text(rng, 10) if rng.random() < 0.8 else None,
                "stargazerCount": rng.randrange(100000), "forkCount": rng.randrange(5000),
                "isPrivate": rng.random() < 0.2, "updatedAt": iso(rng),
                "primaryLanguage": None if lang is None else {"name": lang[0], "color": lang[1]},
                "issues": {"totalCount": rng.randrange(500),
                           "nodes": [issue(rng.randrange(1, 5000)) for _ in range(3)]},
            },
        })
    return {"data": {"viewer": {"__typename": "User", "login": "octocat", "repositories": {
        "totalCount": 1234, "pageInfo": {"hasNextPage": True, "endCursor": hexid(rng, 24)},
        "edges": edges}}}, "extensions": {"cost": {"requestedQueryCost": 42, "throttleStatus": "OK"}}}


def request_body(rng: random.Random) -> dict[str, Any]:
    """~300 B JSON request body (checkout-style)."""
    return {
        "event": rng.choice(["checkout", "add_to_cart", "view_item", "begin_checkout"]),
        "user_id": rng.randrange(10**9), "session_id": hexid(rng, 32),
        "items": [{"sku": f"SKU-{rng.randrange(100000):05d}", "qty": rng.randrange(1, 5),
                   "price": round(rng.uniform(1, 300), 2)} for _ in range(rng.randrange(1, 4))],
        "currency": "USD", "coupon": rng.choice([None, None, "SAVE10", "FREESHIP"]),
        "client": {"ip": f"10.{rng.randrange(256)}.{rng.randrange(256)}.{rng.randrange(256)}",
                   "platform": rng.choice(["ios", "android", "web"])},
        "ts": iso(rng),
    }


def small_response(rng: random.Random) -> dict[str, Any]:
    """~150 B response body (201 Created style)."""
    i = rng.randrange(10**9)
    return {"ok": True, "id": i, "status": "created", "created_at": iso(rng),
            "links": {"self": f"https://api.example.com/v1/orders/{i}"}}


# ---------------------------------------------------------------------------
# Logs / events
# ---------------------------------------------------------------------------

def log_record(rng: random.Random, i: int) -> dict[str, Any]:
    """Flat structured-log record as a JSON log formatter would emit it.

    ~85% plain ASCII messages, ~8% non-ASCII (user names, CJK, emoji), ~7%
    escape-heavy (quoted strings, tracebacks with newlines, Windows paths).
    """
    r = rng.random()
    t = rng.choice(MSG_TEMPLATES).format(id=rng.randrange(10**6), ms=rng.randrange(900), n=rng.randrange(1, 6))
    if r < 0.85:
        msg = t
    elif r < 0.93:
        msg = f"{t} for {rng.choice(INTL)}"
    else:
        msg = (f'{t}: error "{text(rng, 2)}"\nTraceback (most recent call last):\n'
               f'  File "C:\\srv\\app\\handlers.py", line {rng.randrange(900)}, in handle\n'
               f'\tValueError: bad value \'{hexid(rng, 6)}\'')
    level = rng.choice(LEVELS)
    return {
        "timestamp": iso(rng, 30), "level": level, "logger": rng.choice(LOGGERS), "message": msg,
        "module": "handlers", "func": "handle", "line": rng.randrange(1, 900),
        "process": 4242, "thread": 139_872_000 + rng.randrange(16),
        "request_id": hexid(rng, 16), "user_id": rng.randrange(10**7) if rng.random() < 0.7 else None,
        "method": rng.choice(["GET", "GET", "POST", "PUT", "DELETE"]), "path": rng.choice(PATHS),
        "status": rng.choice([200, 200, 200, 201, 204, 400, 404, 500]),
        "duration_ms": round(rng.expovariate(1 / 25), 3), "seq": i,
    }


def log_records(n: int, seed: int = 7) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    return [log_record(rng, i) for i in range(n)]


# ---------------------------------------------------------------------------
# Data pipelines / big files
# ---------------------------------------------------------------------------

def event_record(rng: random.Random, i: int) -> dict[str, Any]:
    """Analytics/event row for multi-MB exports (~200 B compact)."""
    return {
        "id": i, "ts": iso(rng, 365), "user": rng.randrange(10**6),
        "event": rng.choice(["click", "view", "purchase", "signup", "scroll"]),
        "props": {"page": rng.choice(PATHS), "ref": rng.choice([None, "google", "twitter", "email"]),
                  "value": round(rng.uniform(0, 500), 2), "ab": rng.random() < 0.5},
        "geo": [round(rng.uniform(-90, 90), 5), round(rng.uniform(-180, 180), 5)],
        "label": rng.choice(INTL) if rng.random() < 0.1 else rng.choice(TAGS),
    }


def big_records(n: int, seed: int = 11) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    return [event_record(rng, i) for i in range(n)]


def float_array(n: int, seed: int = 13) -> list[float]:
    """Sensor/ML-style floats: full-precision doubles of mixed magnitude."""
    rng = random.Random(seed)
    return [rng.gauss(0, 1) * 10 ** rng.randrange(-3, 6) for _ in range(n)]


def sparse_floats(n: int, seed: int = 31) -> list[float]:
    """Metrics/time-series floats: half exact zeros, the rest 2-decimal values."""
    rng = random.Random(seed)
    return [0.0 if rng.random() < 0.5 else round(rng.uniform(0, 1000), 2) for _ in range(n)]


def int_array(n: int, seed: int = 17) -> list[int]:
    """IDs/counters: mix of small, 32-bit and 63-bit magnitudes, some negative."""
    rng = random.Random(seed)
    out = []
    for _ in range(n):
        r = rng.random()
        if r < 0.5:
            out.append(rng.randrange(-1000, 1000))
        elif r < 0.9:
            out.append(rng.randrange(2**31))
        else:
            out.append(rng.randrange(-(2**63), 2**63))
    return out


def matrix(rows: int, cols: int = 16, seed: int = 19) -> list[list[float]]:
    """Row-major numeric table (list of float rows), e.g. embeddings/features."""
    rng = random.Random(seed)
    return [[round(rng.uniform(-1, 1), 6) for _ in range(cols)] for _ in range(rows)]


def keyed_rows(rows: int, keys_per_row: int, key_fn, vocab: int, seed: int = 23) -> list[dict[str, Any]]:
    """Rows of ``keys_per_row`` int-valued keys drawn from a vocabulary.

    With ``vocab == keys_per_row`` every row has the same keys (key-cache
    friendly); with a vocabulary much larger than the 2048-entry parser key
    cache nearly every key is a cache miss. ``key_fn(k)`` builds key ``k``.
    """
    rng = random.Random(seed)
    if vocab == keys_per_row:
        names = [key_fn(k) for k in range(vocab)]
        return [{k: rng.randrange(10**6) for k in names} for _ in range(rows)]
    return [{key_fn(k): rng.randrange(10**6) for k in rng.sample(range(vocab), keys_per_row)}
            for _ in range(rows)]


def short_key(k: int) -> str:
    return f"user_{k:05d}"  # 10 bytes


def long_key(k: int) -> str:
    # 90+ bytes: longer than the 64-byte key-cache limit (URL/path-like keys).
    return f"https://api.example.com/v1/organizations/{k % 97}/projects/{k}/resources?fields=all&v=2"


def hicard_map(n: int, offset: int, seed: int = 29) -> dict[str, int]:
    """One flat dict with ``n`` distinct keys (e.g. per-user counters)."""
    rng = random.Random(seed + offset)
    return {f"user_{offset + k}": rng.randrange(10**6) for k in range(n)}


# ---------------------------------------------------------------------------
# Cache / MQ codec
# ---------------------------------------------------------------------------

def blob(target_bytes: int, seed: int) -> dict[str, Any]:
    """Cached object of roughly ``target_bytes`` of compact JSON.

    A session/profile style blob: header fields plus a list of resources,
    grown until the size target is reached.
    """
    rng = random.Random(seed)
    out: dict[str, Any] = {"v": 3, "key": f"user:{rng.randrange(10**7)}:profile", "cached_at": iso(rng),
                           "ttl": 3600, "items": []}
    size = 80
    i = 0
    while size < target_bytes:
        if target_bytes < 2000:
            item = request_body(rng)
            size += 290
        else:
            item = api_user(rng, i)
            size += 870
        out["items"].append(item)
        i += 1
    return out
