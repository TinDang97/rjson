import timeit
import rjson
import orjson
import json  # for comparison with standard library

# Sample data for benchmarking
data = {
    "large_array": list(range(100000)),
    "large_object": {f"key_{i}": i for i in range(10000)},
    "nested_object": {
        "data": {f"level1_{i}": {"level2": ["item"] * 100} for i in range(10)}
    },
}
# Number of repetitions for timeit
NUMBER = 100


def benchmark_rjson_dumps():
    rjson.dumps(data)


def benchmark_rjson_loads(s):
    rjson.loads(s)


def benchmark_orjson_dumps():
    orjson.dumps(data)


def benchmark_orjson_loads(s):
    orjson.loads(s)


def benchmark_json_dumps():
    json.dumps(data)


def benchmark_json_loads(s):
    json.loads(s)


if __name__ == "__main__":
    print(f"Benchmarking with {NUMBER} repetitions...")

    # --- Serialization (dumps) ---
    print("\n--- Serialization (dumps) ---")
    time_rjson_dumps = timeit.timeit(benchmark_rjson_dumps, number=NUMBER)
    print(f"rjson.dumps:  {time_rjson_dumps:.6f} seconds")

    time_orjson_dumps = timeit.timeit(benchmark_orjson_dumps, number=NUMBER)
    print(f"orjson.dumps: {time_orjson_dumps:.6f} seconds")

    time_json_dumps = timeit.timeit(benchmark_json_dumps, number=NUMBER)
    print(f"json.dumps:   {time_json_dumps:.6f} seconds")

    # Prepare serialized strings for loading tests
    rjson_serialized = rjson.dumps(data)
    orjson_serialized = orjson.dumps(data)
    json_serialized = json.dumps(data)

    # --- Deserialization (loads) ---
    print("\n--- Deserialization (loads) ---")
    time_rjson_loads = timeit.timeit(
        lambda: benchmark_rjson_loads(rjson_serialized), number=NUMBER
    )
    print(f"rjson.loads:  {time_rjson_loads:.6f} seconds")

    time_orjson_loads = timeit.timeit(
        lambda: benchmark_orjson_loads(orjson_serialized), number=NUMBER
    )
    print(f"orjson.loads: {time_orjson_loads:.6f} seconds")

    time_json_loads = timeit.timeit(
        lambda: benchmark_json_loads(json_serialized), number=NUMBER
    )
    print(f"json.loads:   {time_json_loads:.6f} seconds")

    print("\n--- Comparisons ---")
    # Dumps comparison
    if time_rjson_dumps < time_orjson_dumps:
        print(
            f"rjson.dumps is {time_orjson_dumps / time_rjson_dumps:.2f}x faster than orjson.dumps"
        )
    else:
        print(
            f"orjson.dumps is {time_rjson_dumps / time_orjson_dumps:.2f}x faster than rjson.dumps"
        )

    if time_rjson_dumps < time_json_dumps:
        print(
            f"rjson.dumps is {time_json_dumps / time_rjson_dumps:.2f}x faster than json.dumps"
        )
    else:
        print(
            f"json.dumps is {time_rjson_dumps / time_json_dumps:.2f}x faster than rjson.dumps"
        )

    if time_orjson_dumps < time_json_dumps:
        print(
            f"orjson.dumps is {time_json_dumps / time_orjson_dumps:.2f}x faster than json.dumps"
        )
    else:
        print(
            f"json.dumps is {time_orjson_dumps / time_json_dumps:.2f}x faster than orjson.dumps"
        )

    # Loads comparison
    if time_rjson_loads < time_orjson_loads:
        print(
            f"rjson.loads is {time_orjson_loads / time_rjson_loads:.2f}x faster than orjson.loads"
        )
    else:
        print(
            f"orjson.loads is {time_rjson_loads / time_orjson_loads:.2f}x faster than rjson.loads"
        )

    if time_rjson_loads < time_json_loads:
        print(
            f"rjson.loads is {time_json_loads / time_rjson_loads:.2f}x faster than json.loads"
        )
    else:
        print(
            f"json.loads is {time_rjson_loads / time_json_loads:.2f}x faster than rjson.loads"
        )

    if time_orjson_loads < time_json_loads:
        print(
            f"orjson.loads is {time_json_loads / time_orjson_loads:.2f}x faster than json.loads"
        )
    else:
        print(
            f"json.loads is {time_orjson_loads / time_json_loads:.2f}x faster than orjson.loads"
        )
