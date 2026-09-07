#!/usr/bin/env python3
"""Pure regression tests for benchmark report math; no Windows counters required."""

import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("benchmark.py")
SPEC = importlib.util.spec_from_file_location("vault_benchmark", SCRIPT)
BENCHMARK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCHMARK)


class BenchmarkReportTests(unittest.TestCase):
    def test_amplification_uses_a_stable_nonzero_baseline(self):
        self.assertEqual(BENCHMARK.amplification(250, 100), 2.5)
        self.assertEqual(BENCHMARK.amplification(250, 0), 250.0)

    def test_markdown_renders_normalized_io_columns(self):
        operation = {
            "operation": "compact",
            "seconds": 1.25,
            "peak_ram_bytes": 12_000_000,
            "approximate_read_bytes": 250_000_000,
            "approximate_write_bytes": 10_000_000,
            "logical_read_amplification": 2.5,
            "logical_write_amplification": 0.1,
        }
        scale = {
            "creation_seconds": 1.0,
            "creation_peak_ram_bytes": 12_000_000,
            "incremental_refresh_seconds": 0.2,
            "incremental_refresh_peak_ram_bytes": 11_000_000,
            "incremental_noop_seconds": 0.1,
            "search_seconds": 0.01,
            "read_seconds": 0.02,
            "initial_index_bytes": 1_000,
            "indexed_text_bytes": 100,
            "index_to_indexed_text_ratio": 10.0,
            "sources": 2,
            "passages": 3,
            "occurrences": 4,
            "deduplication_ratio": 0.25,
            "verified_read": True,
            "verified_reference_kind": "backup",
            "duplicate_occurrences_without_duplicate_body": 1,
        }
        report = {
            "vault_version": "codex-vault 0.test",
            "cases": [{
                "requested_gb": 1,
                "operations": [operation],
                "net_saved_after_index_percent": 50.0,
                "index_scalability": scale,
            }],
        }

        text = BENCHMARK.markdown(report)
        self.assertIn("Read × input", text)
        self.assertIn("Write × input", text)
        self.assertIn("2.500×", text)
        self.assertIn("0.100×", text)


if __name__ == "__main__":
    unittest.main()
