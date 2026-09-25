# Using rjson with asyncio, uvloop and friends

`rjson.loads` / `rjson.dumps` / `rjson.dumps_str` are ordinary synchronous functions.
There is no `async` variant, and one would not help: JSON encoding and decoding is pure
CPU work with no I/O to wait on, so an `async def` wrapper would still block the loop. They
work unchanged under asyncio, [uvloop](https://github.com/MagicStack/uvloop), anyio and
Trio, and in any framework built on them. Nothing needs to be awaited.

Each call holds the GIL from start to finish. It never runs Python code or waits on I/O
in between, which makes it fast and thread-safe. It also means **a call blocks the event
loop for as long as it runs**. For typical API payloads that is microseconds and
irrelevant. For multi-megabyte documents it matters, and the usual fix (`to_thread`)
does not work (see below).

## How long does a call block the loop?

These times are roughly proportional to payload size (CPython 3.13, x86_64). They are the
longest event-loop stall measured with a 1 ms heartbeat task running next to the call:

| payload | `rjson.loads` | `rjson.dumps` |
|---|---|---|
| 1 KB API response | ~5 µs | ~1.5 µs |
| 100 KB | ~0.7 ms | ~0.07 ms |
| 5 MB | ~40 ms | ~5 ms |
| 41 MB (300k records) | ~430 ms | ~65 ms |

Rule of thumb: below ~1 MB, just call rjson inline in your coroutine.

## `asyncio.to_thread` does not help

It is common advice to push CPU-bound work into a thread:

```python
data = await asyncio.to_thread(rjson.loads, payload)   # does NOT unblock the loop
```

The worker thread holds the GIL for the whole call, so the event loop thread can't run
either. With a 41 MB payload, the longest loop stall was 426 ms inline and 447 ms via
`to_thread`, on both asyncio and uvloop. orjson and the standard library `json` behave the
same way. For the same reason, several threads calling rjson do not run in parallel.

(This changes only on free-threaded CPython (3.13t/3.14t) or with per-interpreter GILs.
rjson supports neither yet; see [Roadmap](#roadmap).)

## What to do with large payloads

**1. Stream line-delimited data and yield between chunks.** This is the best option when
you control the format (logs, events, exports). NDJSON lets the loop run between records:

```python
import asyncio
import rjson

async def read_ndjson(reader: asyncio.StreamReader, batch: int = 1000):
    """Yield parsed records, handing control back to the loop every `batch` lines."""
    n = 0
    while line := await reader.readline():
        if line.strip():
            yield rjson.loads(line)
        n += 1
        if n % batch == 0:
            await asyncio.sleep(0)

# 41 MB / 300k records: longest loop stall 16.5 ms, instead of ~430 ms for one loads()

def to_ndjson(records) -> bytes:
    return b"\n".join(map(rjson.dumps, records)) + b"\n"
```

**2. Offload whole documents to a process pool.** This keeps the loop responsive and runs
on other cores. You pay the pickling cost of sending the input and the result between
processes, so it only pays off for large documents with a small result, or when you
reduce the data in the worker:

```python
import asyncio
from concurrent.futures import ProcessPoolExecutor
import rjson

POOL = ProcessPoolExecutor()

def summarize(raw: bytes) -> dict:
    doc = rjson.loads(raw)                        # heavy part runs in the worker
    return {"count": len(doc), "total": sum(r["amount"] for r in doc)}

async def handle(raw: bytes) -> dict:
    return await asyncio.get_running_loop().run_in_executor(POOL, summarize, raw)
```

**3. Otherwise, accept the stall.** A single 5 MB request body blocks the loop for about
40 ms. That is often fine behind a load balancer with several workers.

## Drop-in snippets for async libraries

Each of these libraries takes a JSON callable, so rjson plugs in directly. Use `dumps`
where the library expects `bytes` and `dumps_str` where it expects `str`.

**FastAPI / Starlette.** See [`examples/fastapi_app.py`](../examples/fastapi_app.py).

**aiohttp** (server and client expect `str`):

```python
from aiohttp import ClientSession, web
import rjson

async def handler(request: web.Request) -> web.Response:
    body = rjson.loads(await request.read())
    return web.json_response({"ok": True, "echo": body}, dumps=rjson.dumps_str)

async with ClientSession(json_serialize=rjson.dumps_str) as session:
    async with session.post(url, json={"q": 1}) as resp:
        data = await resp.json(loads=rjson.loads)
```

**httpx** (no global hook; encode and decode explicitly):

```python
import httpx, rjson

async with httpx.AsyncClient() as client:
    resp = await client.post(url, content=rjson.dumps(payload),
                             headers={"content-type": "application/json"})
    data = rjson.loads(resp.content)
```

**asyncpg** (`json`/`jsonb` columns, set per connection, e.g. through the pool's `init`):

```python
import asyncpg, rjson

async def init(conn: asyncpg.Connection) -> None:
    for typ in ("json", "jsonb"):
        await conn.set_type_codec(typ, schema="pg_catalog",
                                  encoder=rjson.dumps_str, decoder=rjson.loads)

pool = await asyncpg.create_pool(dsn, init=init)
```

**redis.asyncio** (values are bytes; see also [`examples/codec.py`](../examples/codec.py)):

```python
await r.set(key, rjson.dumps(value), ex=300)
raw = await r.get(key)
value = rjson.loads(raw) if raw is not None else None
```

**aiokafka**:

```python
from aiokafka import AIOKafkaConsumer, AIOKafkaProducer
import rjson

producer = AIOKafkaProducer(bootstrap_servers=brokers, value_serializer=rjson.dumps)
consumer = AIOKafkaConsumer(topic, bootstrap_servers=brokers, value_deserializer=rjson.loads)
```

All of these work with uvloop too (`uvloop.run(main())` or `uvicorn --loop uvloop`).

## Installing with uv

The PyPI distribution is `pyrjson`; the import name is `rjson`:

```bash
uv add pyrjson             # once published; until then:
uv pip install "git+https://github.com/TinDang97/rjson"   # needs a Rust toolchain
```

## Current limits

| environment | status |
|---|---|
| asyncio, uvloop, anyio, Trio | works (synchronous calls) |
| threads (GIL builds) | safe, but calls serialize on the GIL: no parallel speedup |
| free-threaded CPython 3.13t / 3.14t | not supported: the build stops with an error |
| subinterpreters / `InterpreterPoolExecutor` (3.14) | not supported: `ImportError: ... does not support loading in subinterpreters` |

## Roadmap

In priority order:

1. **Streaming API.** An incremental decoder (`feed(chunk) -> list[obj]`) for NDJSON from
   sockets, and a chunked encoder for streaming responses, so large payloads never stall
   the loop.
2. **Free-threaded CPython.** Per-thread key cache and buffers, locking dicts and lists
   during iteration, no direct dict-layout reads, a GIL-free module declaration, and a
   ThreadSanitizer CI job. With these, threads and `to_thread` run in parallel.
3. **Subinterpreters.** Per-interpreter module state. The key cache currently holds
   Python objects shared by the whole process. This enables `InterpreterPoolExecutor`
   on regular GIL builds.
4. **Smaller wins.** Parse `memoryview` input without copying it (today it costs about
   10–15%), and release the GIL while validating the UTF-8 of large `bytes` input.
