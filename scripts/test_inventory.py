#!/usr/bin/env python3
"""Inventory discovered Rust tests without executing opt-in/destructive tests.

Outputs test identifiers and ignored-test classifications for reproducible
cross-platform coverage audits. A passing inventory does not imply historical
PR-by-PR parity or live-model validation.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "ci" / "ignored-regressions.txt"
HISTORICAL = ROOT / "ci" / "historical-acceptance-tests.txt"


def discover(*cargo_args: str, ignored: bool = False) -> list[str]:
    # --ignored is a libtest harness option and must follow the -- separator.
    command = ["cargo", "test", "--locked", *cargo_args, "--", "--list"]
    if ignored:
        command.append("--ignored")
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    # libtest emits '<test name>: test'. Other output, counts and warnings
    # are intentionally ignored. Names can recur between test binaries,
    # hence the full list retains duplicates and the summary reports both.
    return sorted(
        line.removesuffix(": test")
        for line in result.stdout.splitlines()
        if line.endswith(": test")
    )


def read_manifest(path: Path) -> list[str]:
    if not path.is_file():
        raise RuntimeError(f"Missing test manifest: {path}")
    names = [
        line.strip() for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not names or len(names) != len(set(names)):
        raise RuntimeError(f"Test manifest is empty or has duplicate names: {path}")
    return names


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=None)
    args = parser.parse_args()

    all_tests = discover()
    ignored_tests = discover(ignored=True)
    ignored_lib_tests = discover("--lib", ignored=True)
    manifest = read_manifest(MANIFEST)
    historical = read_manifest(HISTORICAL)

    missing_historical = sorted(set(historical) - set(all_tests))
    if missing_historical:
        raise RuntimeError(f"Historical acceptance tests absent from discovered test list: {missing_historical}")

    missing = sorted(set(manifest) - set(ignored_lib_tests))
    if missing:
        raise RuntimeError(f"Dedicated crash tests absent from ignored library tests: {missing}")

    # Not every ignored test is a crash regression: there are opt-in
    # PostgreSQL tests, manual fixtures, and child-process helper tests.
    # Keep those visible for deliberate classification, never auto-run.
    other_ignored_lib = sorted(set(ignored_lib_tests) - set(manifest))
    report = {
        "schema_version": 1,
        "all_discovered_tests": all_tests,
        "ignored_discovered_tests": ignored_tests,
        "ignored_library_tests": ignored_lib_tests,
        "crash_manifest_tests": sorted(manifest),
        "historical_acceptance_tests": sorted(historical),
        "other_ignored_library_tests_for_review": other_ignored_lib,
        "counts": {
            "all_test_list_entries": len(all_tests),
            "all_unique_identifiers": len(set(all_tests)),
            "ignored_list_entries": len(ignored_tests),
            "ignored_library_unique_identifiers": len(set(ignored_lib_tests)),
            "dedicated_crash_manifest": len(manifest),
            "historical_acceptance_manifest": len(historical),
            "other_ignored_library_for_review": len(other_ignored_lib),
        },
    }
    output = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        path = args.output if args.output.is_absolute() else ROOT / args.output
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(output, encoding="utf-8")
    print("[INVENTORY] " + json.dumps(report["counts"], sort_keys=True), flush=True)
    for name in other_ignored_lib:
        print(f"[REVIEW] Unclassified opt-in library test: {name}", flush=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"[INVENTORY ERROR] {error}", file=sys.stderr)
        sys.exit(1)
