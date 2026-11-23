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
HELPERS = ROOT / "ci" / "ignored-subprocess-helpers.txt"
POSTGRES = ROOT / "ci" / "ignored-postgres-tests.txt"


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
    helpers = read_manifest(HELPERS)
    postgres = read_manifest(POSTGRES)

    categories = {
        "crash": set(manifest),
        "subprocess_helpers": set(helpers),
        "postgres": set(postgres),
    }
    if len(set().union(*categories.values())) != sum(len(v) for v in categories.values()):
        raise RuntimeError("Ignored-test category manifests overlap")

    unclassified_ignored = sorted(set(ignored_tests) - set().union(*categories.values()))
    missing_classified = sorted(set().union(*categories.values()) - set(ignored_tests))
    if unclassified_ignored or missing_classified:
        raise RuntimeError(
            f"Ignored-test classification drift: unclassified={unclassified_ignored}, "
            f"missing={missing_classified}"
        )

    expected_ignored_lib = categories["crash"] | categories["subprocess_helpers"] | {
        name for name in postgres if name.startswith("agent::tests::")
    }
    if set(ignored_lib_tests) != expected_ignored_lib:
        raise RuntimeError(
            f"Ignored library classification drift: "
            f"unclassified={sorted(set(ignored_lib_tests) - expected_ignored_lib)}, "
            f"missing={sorted(expected_ignored_lib - set(ignored_lib_tests))}"
        )

    missing_historical = sorted(set(historical) - set(all_tests))
    if missing_historical:
        raise RuntimeError(f"Historical acceptance tests absent from discovered test list: {missing_historical}")

    missing = sorted(set(manifest) - set(ignored_lib_tests))
    if missing:
        raise RuntimeError(f"Dedicated crash tests absent from ignored library tests: {missing}")

    # Subprocess helpers are not independent regression tests: executing
    # them standalone can wait forever for their supervising parent. All
    # ignored tests must be accounted for without running unsafe helpers.
    report = {
        "schema_version": 1,
        "all_discovered_tests": all_tests,
        "ignored_discovered_tests": ignored_tests,
        "ignored_library_tests": ignored_lib_tests,
        "crash_manifest_tests": sorted(manifest),
        "historical_acceptance_tests": sorted(historical),
        "subprocess_helper_tests": sorted(helpers),
        "postgres_opt_in_tests": sorted(postgres),
        "unclassified_ignored_tests": unclassified_ignored,
        "counts": {
            "all_test_list_entries": len(all_tests),
            "all_unique_identifiers": len(set(all_tests)),
            "ignored_list_entries": len(ignored_tests),
            "ignored_library_unique_identifiers": len(set(ignored_lib_tests)),
            "dedicated_crash_manifest": len(manifest),
            "historical_acceptance_manifest": len(historical),
            "subprocess_helpers": len(helpers),
            "postgres_opt_in": len(postgres),
            "unclassified_ignored": len(unclassified_ignored),
        },
    }
    output = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        path = args.output if args.output.is_absolute() else ROOT / args.output
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(output, encoding="utf-8")
    print("[INVENTORY] " + json.dumps(report["counts"], sort_keys=True), flush=True)
    print("[INVENTORY] All ignored tests are explicitly classified; subprocess-only fixtures were not executed.", flush=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"[INVENTORY ERROR] {error}", file=sys.stderr)
        sys.exit(1)
