#!/usr/bin/env python3
"""
Comprehensive benchmark suite for rjson performance optimization.
Tests multiple data patterns to identify specific bottlenecks.
"""

import timeit
import json
import sys
from typing import Dict, Any

try:
    import rjson
except ImportError:
    print("ERROR: rjson not installed. Run 'maturin develop --release'")
    sys.exit(1)

try:
    import orjson
    HAS_ORJSON = True
except ImportError:
    print("WARNING: orjson not installed. Comparisons will be limited.")
    HAS_ORJSON = False


# Number of repetitions for timeit
REPETITIONS = 100


class BenchmarkSuite:
    """Comprehensive benchmark suite for JSON operations."""

    def __init__(self):
        self.benchmarks = self._create_benchmarks()

    def _create_benchmarks(self) -> Dict[str, Any]:
        """Create diverse test cases targeting different bottlenecks."""
        return {
            # Test integer caching effectiveness (target: small integers)
            "small_integers": list(range(-100, 100)),

            # Test large integer handling
            "large_integers": list(range(1000000, 1001000)),

            # Test float performance
            "floats": [i * 0.1 for i in range(10000)],

            # Test short string performance
            "strings_short": ["test"] * 10000,

            # Test long string performance
            "strings_long": ["a" * 100] * 1000,

            # Test wide objects (many keys)
            "wide_object": {f"key_{i}": i for i in range(10000)},

            # Test deep nesting
            "nested_deep": self._create_nested(depth=20),

            # Test mixed types (realistic workload)
            "mixed_types": [
                {"id": i, "name": f"user_{i}", "score": i * 0.5, "active": i % 2 == 0}
                for i in range(1000)
            ],

            # Test large array
            "large_array": list(range(100000)),

            # Test dict with repeated keys (string interning opportunity)
            "repeated_keys": [
                {"name": "Alice", "age": 30, "city": "NYC"},
                {"name": "Bob", "age": 25, "city": "LA"},
                {"name": "Charlie", "age": 35, "city": "NYC"},
            ] * 1000,

            # Test unicode strings
            "unicode_strings": ["Hello 世界! 🚀"] * 1000,

            # Test empty structures
            "empty_dict": {},
            "empty_list": [],

            # Test None/bool/null
            "primitives": [None, True, False, 0, 1, -1, ""],
        }

    def _create_nested(self, depth: int) -> dict:
        """Create deeply nested structure."""
        result = {"value": 42}
        for i in range(depth):
            result = {"level": i, "data": result}
        return result

    def run_benchmark(self, name: str, data: Any) -> Dict[str, float]:
        """Run benchmark for a single test case."""
        results = {}

        # Benchmark serialization (dumps)
        results['rjson_dumps'] = timeit.timeit(
            lambda: rjson.dumps(data),
            number=REPETITIONS
        )

        results['json_dumps'] = timeit.timeit(
            lambda: json.dumps(data),
            number=REPETITIONS
        )

        if HAS_ORJSON:
            results['orjson_dumps'] = timeit.timeit(
                lambda: orjson.dumps(data),
                number=REPETITIONS
            )

        # Prepare serialized data for deserialization
        rjson_str = rjson.dumps(data)
        json_str = json.dumps(data)
        orjson_bytes = orjson.dumps(data) if HAS_ORJSON else None

        # Benchmark deserialization (loads)
        results['rjson_loads'] = timeit.timeit(
            lambda: rjson.loads(rjson_str),
            number=REPETITIONS
        )

        results['json_loads'] = timeit.timeit(
            lambda: json.loads(json_str),
            number=REPETITIONS
        )

        if HAS_ORJSON:
            results['orjson_loads'] = timeit.timeit(
                lambda: orjson.loads(orjson_bytes),
                number=REPETITIONS
            )

        return results

    def run_all(self):
        """Run all benchmarks and display results."""
        print(f"Running comprehensive benchmarks ({REPETITIONS} repetitions each)...")
        print("=" * 80)

        all_results = {}

        for name, data in self.benchmarks.items():
            print(f"\n📊 Benchmarking: {name}")
            print("-" * 80)

            try:
                results = self.run_benchmark(name, data)
                all_results[name] = results

                # Display results
                print(f"  Serialization (dumps):")
                print(f"    rjson:  {results['rjson_dumps']:.6f}s")
                print(f"    json:   {results['json_dumps']:.6f}s")
                if HAS_ORJSON:
                    print(f"    orjson: {results['orjson_dumps']:.6f}s")

                print(f"  Deserialization (loads):")
                print(f"    rjson:  {results['rjson_loads']:.6f}s")
                print(f"    json:   {results['json_loads']:.6f}s")
                if HAS_ORJSON:
                    print(f"    orjson: {results['orjson_loads']:.6f}s")

                # Calculate speedups
                print(f"  Speedup vs json:")
                dumps_speedup = results['json_dumps'] / results['rjson_dumps']
                loads_speedup = results['json_loads'] / results['rjson_loads']
                print(f"    dumps: {dumps_speedup:.2f}x")
                print(f"    loads: {loads_speedup:.2f}x")

                if HAS_ORJSON:
                    print(f"  vs orjson:")
                    dumps_ratio = results['rjson_dumps'] / results['orjson_dumps']
                    loads_ratio = results['rjson_loads'] / results['orjson_loads']
                    print(f"    dumps: {dumps_ratio:.2f}x slower" if dumps_ratio > 1 else f"    dumps: {1/dumps_ratio:.2f}x faster")
                    print(f"    loads: {loads_ratio:.2f}x slower" if loads_ratio > 1 else f"    loads: {1/loads_ratio:.2f}x faster")

            except Exception as e:
                print(f"  ❌ ERROR: {e}")
                continue

        # Summary statistics
        print("\n" + "=" * 80)
        print("📈 SUMMARY STATISTICS")
        print("=" * 80)

        total_dumps_speedup = 0
        total_loads_speedup = 0
        count = 0

        for name, results in all_results.items():
            dumps_speedup = results['json_dumps'] / results['rjson_dumps']
            loads_speedup = results['json_loads'] / results['rjson_loads']
            total_dumps_speedup += dumps_speedup
            total_loads_speedup += loads_speedup
            count += 1

        if count > 0:
            avg_dumps_speedup = total_dumps_speedup / count
            avg_loads_speedup = total_loads_speedup / count

            print(f"\nAverage speedup vs json:")
            print(f"  dumps: {avg_dumps_speedup:.2f}x")
            print(f"  loads: {avg_loads_speedup:.2f}x")

        if HAS_ORJSON:
            total_dumps_ratio = 0
            total_loads_ratio = 0

            for name, results in all_results.items():
                total_dumps_ratio += results['rjson_dumps'] / results['orjson_dumps']
                total_loads_ratio += results['rjson_loads'] / results['orjson_loads']

            avg_dumps_ratio = total_dumps_ratio / count
            avg_loads_ratio = total_loads_ratio / count

            print(f"\nAverage vs orjson:")
            print(f"  dumps: {avg_dumps_ratio:.2f}x slower" if avg_dumps_ratio > 1 else f"  dumps: {1/avg_dumps_ratio:.2f}x faster")
            print(f"  loads: {avg_loads_ratio:.2f}x slower" if avg_loads_ratio > 1 else f"  loads: {1/avg_loads_ratio:.2f}x faster")


if __name__ == "__main__":
    suite = BenchmarkSuite()
    suite.run_all()
