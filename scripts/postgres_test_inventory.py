#!/usr/bin/env python3
"""Check that the opt-in PostgreSQL CI selectors discover exactly the expected tests.

No test is executed here. DB integration execution happens in the separate,
isolated PostgreSQL CI job after this preflight has succeeded.
"""
from __future__ import annotations

from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "ci" / "ignored-postgres-tests.txt"
AGENT = "agent::tests::postgres_agent_uc_dosyali_onay_hafiza_ret_ve_rollback"


def list_ignored(*cargo_options: str) -> set[str]:
    command = ["cargo", "test", "--locked", *cargo_options, "--", "--list", "--ignored"]
    result = subprocess.run(
        command, cwd=ROOT, capture_output=True, text=True, check=True
    )
    names = [
        line.removesuffix(": test")
        for line in result.stdout.splitlines()
        if line.endswith(": test")
    ]
    if len(names) != len(set(names)):
        raise RuntimeError(f"Duplicate test identifiers discovered in {cargo_options}")
    return set(names)


def main() -> int:
    expected = [
        line.strip()
        for line in MANIFEST.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if len(expected) != 13 or len(expected) != len(set(expected)) or AGENT not in expected:
        raise RuntimeError("PostgreSQL manifest must contain 12 integration tests and the agent test")

    expected_integration = set(expected) - {AGENT}
    actual_integration = list_ignored("--test", "postgres_memory")
    if actual_integration != expected_integration:
        raise RuntimeError(
            f"PostgreSQL integration test drift: "
            f"missing={sorted(expected_integration - actual_integration)} "
            f"unexpected={sorted(actual_integration - expected_integration)}"
        )

    actual_lib = list_ignored("--lib")
    matches = {name for name in actual_lib if AGENT.split("::")[-1] in name}
    if matches != {AGENT}:
        raise RuntimeError(f"Agent opt-in selector is not unique: {sorted(matches)}")

    print("[POSTGRES INVENTORY] 12 integration + 1 agent opt-in tests discovered")
    print("[POSTGRES INVENTORY] All test names exactly match the tracked manifest")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as exc:
        print(f"[POSTGRES INVENTORY ERROR] {exc}", file=sys.stderr)
        sys.exit(1)
