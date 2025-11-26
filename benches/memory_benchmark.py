#!/usr/bin/env python3
"""
Benchmark script to monitor CPU and memory usage of rjson vs orjson.

Measures:
- Peak memory usage
- Memory allocations
- CPU time (user + system)
- Wall clock time
"""

import gc
import time
import tracemalloc
import resource
import sys
from typing import Callable, Any, Dict, Tuple

import rjson
import orjson
import json

# Test data
def create_test_data() -> Dict:
    """Create test data similar to the main benchmark."""
    return {
        "large_array": list(range(100000)),
        "large_object": {f"key_{i}": i for i in range(10000)},
        "nested_object": {
            "data": {f"level1_{i}": {"level2": ["item"] * 100} for i in range(10)}
        },
    }

def measure_memory_and_time(
    func: Callable[[], Any],
    iterations: int = 10,
    warmup: int = 2
) -> Dict[str, float]:
    """
    Measure memory usage and CPU time for a function.

    Returns dict with:
    - peak_memory_mb: Peak memory usage in MB
    - total_allocations: Total bytes allocated
    - cpu_time_s: Total CPU time (user + system)
    - wall_time_s: Wall clock time
    - avg_time_ms: Average time per iteration in ms
    """
    # Warmup runs
    for _ in range(warmup):
        func()

    # Force garbage collection before measurement
    gc.collect()
    gc.collect()
    gc.collect()

    # Start memory tracking
    tracemalloc.start()

    # Get initial resource usage
    usage_start = resource.getrusage(resource.RUSAGE_SELF)
    wall_start = time.perf_counter()

    # Run the function
    for _ in range(iterations):
        result = func()
        del result  # Ensure result is freed

    # Get final measurements
    wall_end = time.perf_counter()
    usage_end = resource.getrusage(resource.RUSAGE_SELF)

    # Get memory stats
    current, peak = tracemalloc.get_traced_memory()
    tracemalloc.stop()

    # Calculate CPU time
    cpu_user = usage_end.ru_utime - usage_start.ru_utime
    cpu_sys = usage_end.ru_stime - usage_start.ru_stime
    cpu_total = cpu_user + cpu_sys

    wall_time = wall_end - wall_start

    # CPU percentage = (CPU time / Wall time) * 100
    cpu_percent = (cpu_total / wall_time * 100) if wall_time > 0 else 0

    return {
        "peak_memory_mb": peak / (1024 * 1024),
        "current_memory_mb": current / (1024 * 1024),
        "cpu_time_s": cpu_total,
        "cpu_user_s": cpu_user,
        "cpu_sys_s": cpu_sys,
        "cpu_percent": cpu_percent,
        "wall_time_s": wall_time,
        "avg_time_ms": (wall_time / iterations) * 1000,
    }

def format_results(name: str, results: Dict[str, float]) -> str:
    """Format benchmark results for display."""
    return f"""
{name}:
  Peak Memory:    {results['peak_memory_mb']:.2f} MB
  CPU Time:       {results['cpu_time_s']:.4f} s (user: {results['cpu_user_s']:.4f}, sys: {results['cpu_sys_s']:.4f})
  CPU Usage:      {results['cpu_percent']:.1f}%
  Wall Time:      {results['wall_time_s']:.4f} s
  Avg per iter:   {results['avg_time_ms']:.2f} ms
"""

def compare_results(name1: str, r1: Dict, name2: str, r2: Dict) -> str:
    """Compare two benchmark results."""
    memory_ratio = r1['peak_memory_mb'] / r2['peak_memory_mb'] if r2['peak_memory_mb'] > 0 else float('inf')
    time_ratio = r1['avg_time_ms'] / r2['avg_time_ms'] if r2['avg_time_ms'] > 0 else float('inf')
    cpu_ratio = r1['cpu_time_s'] / r2['cpu_time_s'] if r2['cpu_time_s'] > 0 else float('inf')

    faster = name2 if time_ratio > 1 else name1
    slower = name1 if time_ratio > 1 else name2
    speed_ratio = max(time_ratio, 1/time_ratio)

    less_mem = name2 if memory_ratio > 1 else name1
    more_mem = name1 if memory_ratio > 1 else name2
    mem_ratio = max(memory_ratio, 1/memory_ratio)

    return f"""
  Comparison:
    Speed: {faster} is {speed_ratio:.2f}x faster than {slower}
    Memory: {less_mem} uses {mem_ratio:.2f}x less peak memory than {more_mem}
    CPU: {name1} uses {cpu_ratio:.2f}x the CPU time of {name2}
"""

def run_benchmarks():
    """Run all benchmarks and display results."""
    print("=" * 60)
    print("rjson vs orjson - CPU & Memory Benchmark")
    print("=" * 60)

    # Create test data
    data = create_test_data()

    # Serialize to JSON strings for loads testing
    rjson_str = rjson.dumps(data)
    orjson_bytes = orjson.dumps(data)
    orjson_str = orjson_bytes.decode('utf-8')
    json_str = json.dumps(data)

    iterations = 20

    print(f"\nTest configuration:")
    print(f"  Iterations: {iterations}")
    print(f"  Data size: {len(json_str):,} bytes")
    print(f"  Large array: 100,000 integers")
    print(f"  Large object: 10,000 keys")

    # ============================================================
    # DUMPS (Serialization) Benchmarks
    # ============================================================
    print("\n" + "=" * 60)
    print("SERIALIZATION (dumps)")
    print("=" * 60)

    print("\nMeasuring rjson.dumps()...")
    rjson_dumps = measure_memory_and_time(lambda: rjson.dumps(data), iterations)
    print(format_results("rjson.dumps", rjson_dumps))

    print("Measuring orjson.dumps()...")
    orjson_dumps = measure_memory_and_time(lambda: orjson.dumps(data), iterations)
    print(format_results("orjson.dumps", orjson_dumps))

    print("Measuring json.dumps()...")
    json_dumps = measure_memory_and_time(lambda: json.dumps(data), iterations)
    print(format_results("json.dumps", json_dumps))

    print("-" * 40)
    print("Serialization Comparisons:")
    print(compare_results("rjson", rjson_dumps, "orjson", orjson_dumps))
    print(compare_results("rjson", rjson_dumps, "json", json_dumps))

    # ============================================================
    # LOADS (Deserialization) Benchmarks
    # ============================================================
    print("\n" + "=" * 60)
    print("DESERIALIZATION (loads)")
    print("=" * 60)

    print("\nMeasuring rjson.loads()...")
    rjson_loads = measure_memory_and_time(lambda: rjson.loads(rjson_str), iterations)
    print(format_results("rjson.loads", rjson_loads))

    print("Measuring orjson.loads()...")
    orjson_loads = measure_memory_and_time(lambda: orjson.loads(orjson_bytes), iterations)
    print(format_results("orjson.loads", orjson_loads))

    print("Measuring json.loads()...")
    json_loads = measure_memory_and_time(lambda: json.loads(json_str), iterations)
    print(format_results("json.loads", json_loads))

    print("-" * 40)
    print("Deserialization Comparisons:")
    print(compare_results("rjson", rjson_loads, "orjson", orjson_loads))
    print(compare_results("rjson", rjson_loads, "json", json_loads))

    # ============================================================
    # Summary Table
    # ============================================================
    print("\n" + "=" * 60)
    print("SUMMARY TABLE")
    print("=" * 60)

    print(f"""
{'Operation':<15} {'Library':<10} {'Time (ms)':<12} {'Memory (MB)':<12} {'CPU (s)':<10} {'CPU %':<8}
{'-'*67}
{'dumps':<15} {'rjson':<10} {rjson_dumps['avg_time_ms']:<12.2f} {rjson_dumps['peak_memory_mb']:<12.2f} {rjson_dumps['cpu_time_s']:<10.4f} {rjson_dumps['cpu_percent']:<8.1f}
{'dumps':<15} {'orjson':<10} {orjson_dumps['avg_time_ms']:<12.2f} {orjson_dumps['peak_memory_mb']:<12.2f} {orjson_dumps['cpu_time_s']:<10.4f} {orjson_dumps['cpu_percent']:<8.1f}
{'dumps':<15} {'json':<10} {json_dumps['avg_time_ms']:<12.2f} {json_dumps['peak_memory_mb']:<12.2f} {json_dumps['cpu_time_s']:<10.4f} {json_dumps['cpu_percent']:<8.1f}
{'-'*67}
{'loads':<15} {'rjson':<10} {rjson_loads['avg_time_ms']:<12.2f} {rjson_loads['peak_memory_mb']:<12.2f} {rjson_loads['cpu_time_s']:<10.4f} {rjson_loads['cpu_percent']:<8.1f}
{'loads':<15} {'orjson':<10} {orjson_loads['avg_time_ms']:<12.2f} {orjson_loads['peak_memory_mb']:<12.2f} {orjson_loads['cpu_time_s']:<10.4f} {orjson_loads['cpu_percent']:<8.1f}
{'loads':<15} {'json':<10} {json_loads['avg_time_ms']:<12.2f} {json_loads['peak_memory_mb']:<12.2f} {json_loads['cpu_time_s']:<10.4f} {json_loads['cpu_percent']:<8.1f}
""")

    # ============================================================
    # Performance Ratios
    # ============================================================
    print("=" * 60)
    print("PERFORMANCE RATIOS (vs orjson)")
    print("=" * 60)

    dumps_time_ratio = rjson_dumps['avg_time_ms'] / orjson_dumps['avg_time_ms']
    dumps_mem_ratio = rjson_dumps['peak_memory_mb'] / orjson_dumps['peak_memory_mb']
    loads_time_ratio = rjson_loads['avg_time_ms'] / orjson_loads['avg_time_ms']
    loads_mem_ratio = rjson_loads['peak_memory_mb'] / orjson_loads['peak_memory_mb']

    print(f"""
rjson vs orjson:
  dumps: {dumps_time_ratio:.2f}x slower, {dumps_mem_ratio:.2f}x memory
  loads: {loads_time_ratio:.2f}x slower, {loads_mem_ratio:.2f}x memory
""")

if __name__ == "__main__":
    run_benchmarks()
