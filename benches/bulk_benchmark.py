#!/usr/bin/env python3
"""
Benchmark specifically for bulk array operations.

This benchmark tests the performance of serializing large homogeneous arrays,
which should benefit significantly from the Phase 6A bulk optimizations.
"""

import timeit
import rjson
import orjson
import json

REPETITIONS = 100

def benchmark_int_array():
    """Benchmark homogeneous integer array serialization."""
    data = list(range(10000))  # 10k integers

    rjson_time = timeit.timeit(lambda: rjson.dumps(data), number=REPETITIONS)
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=REPETITIONS)
    json_time = timeit.timeit(lambda: json.dumps(data), number=REPETITIONS)

    print("\n--- Integer Array (10k elements) Serialization ---")
    print(f"rjson.dumps:  {rjson_time:.6f} seconds")
    print(f"orjson.dumps: {orjson_time:.6f} seconds")
    print(f"json.dumps:   {json_time:.6f} seconds")
    print(f"\nrjson is {json_time / rjson_time:.2f}x faster than json")
    print(f"orjson is {rjson_time / orjson_time:.2f}x faster than rjson")
    print(f"Gap to orjson: {(rjson_time / orjson_time - 1) * 100:.1f}% slower")

def benchmark_float_array():
    """Benchmark homogeneous float array serialization."""
    data = [float(i) + 0.5 for i in range(10000)]  # 10k floats

    rjson_time = timeit.timeit(lambda: rjson.dumps(data), number=REPETITIONS)
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=REPETITIONS)
    json_time = timeit.timeit(lambda: json.dumps(data), number=REPETITIONS)

    print("\n--- Float Array (10k elements) Serialization ---")
    print(f"rjson.dumps:  {rjson_time:.6f} seconds")
    print(f"orjson.dumps: {orjson_time:.6f} seconds")
    print(f"json.dumps:   {json_time:.6f} seconds")
    print(f"\nrjson is {json_time / rjson_time:.2f}x faster than json")
    print(f"orjson is {rjson_time / orjson_time:.2f}x faster than rjson")
    print(f"Gap to orjson: {(rjson_time / orjson_time - 1) * 100:.1f}% slower")

def benchmark_string_array():
    """Benchmark homogeneous string array serialization."""
    data = [f"string_{i}" for i in range(10000)]  # 10k strings

    rjson_time = timeit.timeit(lambda: rjson.dumps(data), number=REPETITIONS)
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=REPETITIONS)
    json_time = timeit.timeit(lambda: json.dumps(data), number=REPETITIONS)

    print("\n--- String Array (10k elements) Serialization ---")
    print(f"rjson.dumps:  {rjson_time:.6f} seconds")
    print(f"orjson.dumps: {orjson_time:.6f} seconds")
    print(f"json.dumps:   {json_time:.6f} seconds")
    print(f"\nrjson is {json_time / rjson_time:.2f}x faster than json")
    print(f"orjson is {rjson_time / orjson_time:.2f}x faster than rjson")
    print(f"Gap to orjson: {(rjson_time / orjson_time - 1) * 100:.1f}% slower")

def benchmark_bool_array():
    """Benchmark homogeneous boolean array serialization."""
    data = [i % 2 == 0 for i in range(10000)]  # 10k bools

    rjson_time = timeit.timeit(lambda: rjson.dumps(data), number=REPETITIONS)
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=REPETITIONS)
    json_time = timeit.timeit(lambda: json.dumps(data), number=REPETITIONS)

    print("\n--- Boolean Array (10k elements) Serialization ---")
    print(f"rjson.dumps:  {rjson_time:.6f} seconds")
    print(f"orjson.dumps: {orjson_time:.6f} seconds")
    print(f"json.dumps:   {json_time:.6f} seconds")
    print(f"\nrjson is {json_time / rjson_time:.2f}x faster than json")
    print(f"orjson is {rjson_time / orjson_time:.2f}x faster than rjson")
    print(f"Gap to orjson: {(rjson_time / orjson_time - 1) * 100:.1f}% slower")

def benchmark_mixed_array():
    """Benchmark mixed-type array (no bulk optimization)."""
    data = [i if i % 2 == 0 else f"str_{i}" for i in range(10000)]  # Mixed

    rjson_time = timeit.timeit(lambda: rjson.dumps(data), number=REPETITIONS)
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=REPETITIONS)
    json_time = timeit.timeit(lambda: json.dumps(data), number=REPETITIONS)

    print("\n--- Mixed Array (10k elements) Serialization (no bulk) ---")
    print(f"rjson.dumps:  {rjson_time:.6f} seconds")
    print(f"orjson.dumps: {orjson_time:.6f} seconds")
    print(f"json.dumps:   {json_time:.6f} seconds")
    print(f"\nrjson is {json_time / rjson_time:.2f}x faster than json")
    print(f"orjson is {rjson_time / orjson_time:.2f}x faster than rjson")
    print(f"Gap to orjson: {(rjson_time / orjson_time - 1) * 100:.1f}% slower")

def main():
    print(f"Bulk Array Benchmark ({REPETITIONS} repetitions)")
    print("=" * 60)

    benchmark_int_array()
    benchmark_float_array()
    benchmark_string_array()
    benchmark_bool_array()
    benchmark_mixed_array()

    print("\n" + "=" * 60)
    print("Summary:")
    print("- Bulk optimizations target homogeneous arrays (all same type)")
    print("- Expected gains: 30-40% for int/float/bool/string arrays")
    print("- Mixed arrays use normal per-element serialization")

if __name__ == "__main__":
    main()
