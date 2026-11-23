#!/usr/bin/env python3
"""Cross-platform CI regression runner: shared tests for Windows, Linux and macOS.

Normal tests run via 'cargo test --locked' in the workflow. This runner
executes the explicitly ignored, process-interruption regression suite and
the stabilization stress suite on each native CI operating system.
No LLM, secret database, or unapproved workspace operation is involved.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "ci" / "ignored-regressions.txt"


def run(command: list[str]) -> None:
    print("[CI] " + " ".join(command), flush=True)
    subprocess.run(command, cwd=ROOT, check=True)


def ignored_tests() -> list[str]:
    if not MANIFEST.is_file():
        raise RuntimeError(f"Missing regression manifest: {MANIFEST}")
    entries = [
        line.strip()
        for line in MANIFEST.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not entries or len(entries) != len(set(entries)):
        raise RuntimeError("Regression manifest is empty or contains duplicate entries")
    if any(" " in name or not name.endswith(("preserves_original_source", "keeps_recovery_evidence",
        "allows_reapproved_restore", "keeps_target_and_marker_for_reapproval",
        "preserves_target_absence_and_pending_marker", "resumes_with_explicit_approval",
        "requires_new_approval", "requires_fresh_approval",
        "preserves_updated_target_and_marker", "keeps_original_and_recoverable_marker",
        "kills_child_and_resumes")) for name in entries):
        raise RuntimeError("Unexpected or invalid ignored regression identifier")
    return entries


def execute_ignored_regressions() -> None:
    tests = ignored_tests()
    # Assert exact test names before execution: filtering to 0 matches must
    # never produce a misleading green CI job.
    listing = subprocess.run(
        ["cargo", "test", "--locked", "--lib", "--", "--list"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    available = {line.split(": test")[0] for line in listing.splitlines() if line.endswith(": test")}
    missing = sorted(set(tests) - available)
    if missing:
        raise RuntimeError(f"Missing ignored regression tests: {missing}")
    print(f"[CI] Running {len(tests)} explicit process-crash regressions", flush=True)
    for name in tests:
        run(["cargo", "test", "--locked", "--lib", name, "--", "--ignored", "--exact"])


def execute_stress(repeats: int) -> None:
    if repeats < 1:
        raise RuntimeError("At least one stress run required")
    for threads, mode in ((4, "parallel"), (1, "serial")):
        for index in range(repeats):
            print(f"[STRESS] {mode}: {index + 1}/{repeats}", flush=True)
            run([
                "cargo", "test", "--locked", "--test", "stabilization_flow",
                "--", f"--test-threads={threads}",
            ])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("ignored", "stress", "all"))
    parser.add_argument("--repeats", type=int, default=20)
    options = parser.parse_args()
    if options.action in ("ignored", "all"):
        execute_ignored_regressions()
    if options.action in ("stress", "all"):
        execute_stress(options.repeats)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"[CI ERROR] {error}", file=sys.stderr)
        sys.exit(1)
