#!/usr/bin/env python3
"""
Benchmark the "nuclear option" dumps_bytes() vs regular dumps() vs orjson.

This shows the maximum possible performance with zero-copy bytes return.
"""

import timeit
import rjson
import orjson
import json

REPETITIONS = 100

def benchmark_comparison(name, data):
    """Compare all implementations on a specific dataset."""
    print(f"\n{'=' * 60}")
    print(f"{name}")
    print('=' * 60)

    # Benchmark dumps_bytes (nuclear option)
    dumps_bytes_time = timeit.timeit(lambda: rjson.dumps_bytes(data), number=REPETITIONS)

    # Benchmark regular dumps
    dumps_time = timeit.timeit(lambda: rjson.dumps(data), number=REPETITIONS)

    # Benchmark orjson
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=REPETITIONS)

    # Benchmark stdlib json
    json_time = timeit.timeit(lambda: json.dumps(data), number=REPETITIONS)

    print(f"\nrjson.dumps_bytes: {dumps_bytes_time:.6f}s (NUCLEAR OPTION)")
    print(f"rjson.dumps:       {dumps_time:.6f}s (normal)")
    print(f"orjson.dumps:      {orjson_time:.6f}s")
    print(f"json.dumps:        {json_time:.6f}s")

    print(f"\n--- Comparisons ---")
    print(f"dumps_bytes vs dumps:  {dumps_time / dumps_bytes_time:.2f}x faster")
    print(f"dumps_bytes vs orjson: {orjson_time / dumps_bytes_time:.2f}x (orjson is baseline)")
    print(f"dumps_bytes vs json:   {json_time / dumps_bytes_time:.2f}x faster")

    print(f"\nGap to orjson:")
    print(f"  dumps_bytes: {(dumps_bytes_time / orjson_time - 1) * 100:.1f}% {'slower' if dumps_bytes_time > orjson_time else 'FASTER'}")
    print(f"  dumps:       {(dumps_time / orjson_time - 1) * 100:.1f}% slower")

def main():
    print(f"Extreme Optimization Benchmark ({REPETITIONS} repetitions)")
    print("Testing the 'nuclear option' - dumps_bytes() with zero-copy\n")

    # Test 1: Integer array (10k elements)
    int_array = list(range(10000))
    benchmark_comparison("Integer Array (10k elements)", int_array)

    # Test 2: Float array (10k elements)
    float_array = [float(i) + 0.5 for i in range(10000)]
    benchmark_comparison("Float Array (10k elements)", float_array)

    # Test 3: String array (10k elements)
    string_array = [f"string_{i}" for i in range(10000)]
    benchmark_comparison("String Array (10k elements)", string_array)

    # Test 4: Boolean array (10k elements)
    bool_array = [i % 2 == 0 for i in range(10000)]
    benchmark_comparison("Boolean Array (10k elements)", bool_array)

    # Test 5: Mixed nested structure
    mixed_data = {
        "users": [
            {"id": i, "name": f"user_{i}", "active": i % 2 == 0, "score": float(i) * 1.5}
            for i in range(1000)
        ],
        "metadata": {
            "total": 1000,
            "version": "1.0",
            "timestamp": 1234567890
        }
    }
    benchmark_comparison("Mixed Nested Structure (1000 users)", mixed_data)

    print("\n" + "=" * 60)
    print("SUMMARY")
    print("=" * 60)
    print("""
The 'nuclear option' (dumps_bytes) sacrifices:
- API compatibility (returns bytes instead of str)
- Some PyO3 safety guarantees
- Idiomatic Rust patterns

In exchange for:
- Zero-copy buffer creation
- Direct C API calls (bypasses PyO3)
- AVX2 SIMD string scanning
- Aggressive inlining

This shows the MAXIMUM POSSIBLE performance with Rust+PyO3.
""")

if __name__ == "__main__":
    main()
