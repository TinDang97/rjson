import rjson
import time
import json  # Standard library for comparison and data generation

# Example data
data = {
    "large_array": list(range(100000)),
    "large_object": {f"key_{i}": i for i in range(10000)},
    "nested_object": {
        "data": {f"level1_{i}": {"level2": ["item"] * 100} for i in range(10)}
    },
}
json_strings = {name: json.dumps(payload) for name, payload in data.items()}

for name, json_str in json_strings.items():
    # Benchmark rjson.loads
    start_time = time.perf_counter()
    for _ in range(10):  # Adjust iteration count as needed
        rjson_obj = rjson.loads(json_str)
    end_time = time.perf_counter()
    print(
        f"rjson.loads for '{name}' (avg over 10 runs): {(end_time - start_time) / 10:.6f} seconds"
    )
