#!/usr/bin/env python3
"""
Profiling script to identify PyO3 overhead sources in rjson.

This script benchmarks different workloads to identify where optimization
efforts should be focused.
"""

import timeit
import json
import sys

try:
    import rjson
except ImportError:
    print("ERROR: rjson not installed. Run: maturin develop --release")
    sys.exit(1)

try:
    import orjson
except ImportError:
    print("ERROR: orjson not installed. Run: pip install orjson")
    sys.exit(1)


def benchmark_workload(name, data, repetitions=100):
    """Benchmark a specific workload."""
    # Warm up
    for _ in range(10):
        rjson.dumps(data)
        orjson.dumps(data)
        json.dumps(data)

    # Benchmark
    rjson_time = timeit.timeit(lambda: rjson.dumps(data), number=repetitions)
    orjson_time = timeit.timeit(lambda: orjson.dumps(data), number=repetitions)
    json_time = timeit.timeit(lambda: json.dumps(data), number=repetitions)

    # Calculate per-operation time (microseconds)
    per_op_rjson = (rjson_time / repetitions) * 1_000_000
    per_op_orjson = (orjson_time / repetitions) * 1_000_000
    per_op_json = (json_time / repetitions) * 1_000_000

    # Calculate gaps
    gap_to_orjson = rjson_time / orjson_time
    gap_to_json = json_time / rjson_time

    return {
        'name': name,
        'rjson_us': per_op_rjson,
        'orjson_us': per_op_orjson,
        'json_us': per_op_json,
        'gap_orjson': gap_to_orjson,
        'gap_json': gap_to_json,
        'overhead_us': per_op_rjson - per_op_orjson,
    }


def main():
    print("=" * 80)
    print("rjson Performance Profiling - Identifying PyO3 Overhead Sources")
    print("=" * 80)
    print()

    # Define workloads
    workloads = [
        # Dict-heavy workloads (target for PyDict_Next optimization)
        ("Dict small keys (100)", {f"k{i}": i for i in range(100)}),
        ("Dict large keys (1000)", {f"key_{i}": i for i in range(1000)}),
        ("Dict nested", {f"k{i}": {"nested": i, "value": i*2} for i in range(100)}),

        # List-heavy workloads (target for bounds checking removal)
        ("List ints (1000)", list(range(1000))),
        ("List mixed (1000)", [i if i % 2 == 0 else f"str_{i}" for i in range(1000)]),
        ("List nested", [[i, i+1, i+2] for i in range(100)]),

        # String-heavy workloads (target for zero-copy)
        ("Strings short (1000)", [f"s{i}" for i in range(1000)]),
        ("Strings medium (1000)", [f"string_value_{i}" for i in range(1000)]),
        ("Strings long (100)", ["x" * 100 for _ in range(100)]),

        # Complex nested structures
        ("Complex nested", {
            "users": [
                {"id": i, "name": f"User{i}", "tags": [f"tag{j}" for j in range(5)]}
                for i in range(50)
            ],
            "metadata": {"count": 50, "version": "1.0"}
        }),
    ]

    results = []
    for name, data in workloads:
        result = benchmark_workload(name, data)
        results.append(result)

    # Print results
    print(f"{'Workload':<25} {'rjson':<10} {'orjson':<10} {'Gap':<8} {'Overhead':<12}")
    print(f"{'':25} {'(μs)':<10} {'(μs)':<10} {'(x)':<8} {'(μs)':<12}")
    print("-" * 80)

    for r in results:
        print(f"{r['name']:<25} {r['rjson_us']:<10.2f} {r['orjson_us']:<10.2f} "
              f"{r['gap_orjson']:<8.2f} {r['overhead_us']:<12.2f}")

    print("\n" + "=" * 80)
    print("Analysis: Highest Overhead Sources")
    print("=" * 80)

    # Sort by absolute overhead
    sorted_by_overhead = sorted(results, key=lambda x: x['overhead_us'], reverse=True)

    print("\nTop 5 workloads by absolute overhead (μs):")
    for i, r in enumerate(sorted_by_overhead[:5], 1):
        print(f"{i}. {r['name']:<25} Overhead: {r['overhead_us']:>8.2f} μs "
              f"({r['gap_orjson']:.2f}x slower)")

    # Sort by gap ratio
    sorted_by_gap = sorted(results, key=lambda x: x['gap_orjson'], reverse=True)

    print("\nTop 5 workloads by relative gap (x slower than orjson):")
    for i, r in enumerate(sorted_by_gap[:5], 1):
        print(f"{i}. {r['name']:<25} Gap: {r['gap_orjson']:>6.2f}x "
              f"(overhead: {r['overhead_us']:.2f} μs)")

    # Categorize by workload type
    print("\n" + "=" * 80)
    print("Overhead by Category")
    print("=" * 80)

    dict_workloads = [r for r in results if 'Dict' in r['name']]
    list_workloads = [r for r in results if 'List' in r['name']]
    string_workloads = [r for r in results if 'String' in r['name']]

    if dict_workloads:
        avg_dict_gap = sum(r['gap_orjson'] for r in dict_workloads) / len(dict_workloads)
        avg_dict_overhead = sum(r['overhead_us'] for r in dict_workloads) / len(dict_workloads)
        print(f"Dict operations: {avg_dict_gap:.2f}x gap, {avg_dict_overhead:.2f} μs overhead (avg)")

    if list_workloads:
        avg_list_gap = sum(r['gap_orjson'] for r in list_workloads) / len(list_workloads)
        avg_list_overhead = sum(r['overhead_us'] for r in list_workloads) / len(list_workloads)
        print(f"List operations: {avg_list_gap:.2f}x gap, {avg_list_overhead:.2f} μs overhead (avg)")

    if string_workloads:
        avg_str_gap = sum(r['gap_orjson'] for r in string_workloads) / len(string_workloads)
        avg_str_overhead = sum(r['overhead_us'] for r in string_workloads) / len(string_workloads)
        print(f"String operations: {avg_str_gap:.2f}x gap, {avg_str_overhead:.2f} μs overhead (avg)")

    print("\n" + "=" * 80)
    print("Optimization Priority Recommendations")
    print("=" * 80)

    # Determine priorities
    priorities = []

    if dict_workloads:
        dict_total_overhead = sum(r['overhead_us'] for r in dict_workloads)
        priorities.append(("Dict optimization (PyDict_Next)", dict_total_overhead, avg_dict_gap))

    if list_workloads:
        list_total_overhead = sum(r['overhead_us'] for r in list_workloads)
        priorities.append(("List bounds checking", list_total_overhead, avg_list_gap))

    if string_workloads:
        str_total_overhead = sum(r['overhead_us'] for r in string_workloads)
        priorities.append(("String zero-copy", str_total_overhead, avg_str_gap))

    priorities.sort(key=lambda x: x[1], reverse=True)

    for i, (opt, overhead, gap) in enumerate(priorities, 1):
        print(f"Priority {i}: {opt}")
        print(f"  - Total overhead: {overhead:.2f} μs")
        print(f"  - Average gap: {gap:.2f}x slower than orjson")
        print()


if __name__ == "__main__":
    main()
