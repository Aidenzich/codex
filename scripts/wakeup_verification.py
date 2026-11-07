#!/usr/bin/env python3
"""Translate one native Rust test invocation into the Loops result protocol."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys


# Approved evidence identifiers. Cases and assertions remain in the native tests.
SCENARIOS = {
    "tool-contract-and-registry",
    "existing-clock-exact-tools",
    "existing-clock-code-mode",
    "existing-agent-tool-policy",
    "existing-clock-feature-matrices",
    "existing-review-clock-exclusion",
    "existing-prompt-tool-stability",
    "delayed-resume-busy-cancel-shutdown",
    "existing-mail-admission-boundaries",
    "subscriber-close-revokes-wakeups",
    "framework-marker-native-history",
    "typed-marker-published-schema",
    "visible-management-and-turn-feedback",
}
SUMMARY = re.compile(
    r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;"
)


def write_result(value):
    """Only the executor supplies the destination; never change reserved inputs."""
    target = os.environ.get("LOOPS_VERIFICATION_RESULT_PATH")
    if target:
        path = Path(target)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--requirement-rows", required=True)
    parser.add_argument("--scenario", choices=sorted(SCENARIOS), required=True)
    parser.add_argument("--cwd", default=".")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("one native test command is required after --")
    # Cargo's standard harness summary is the execution oracle, not our exit alone.
    output = []
    try:
        process = subprocess.Popen(
            command, cwd=args.cwd, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, bufsize=1,
        )
        for line in process.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            output.append(line)
        returncode = process.wait()
    except OSError as error:
        print(f"Native verification setup failed: {error}", file=sys.stderr)
        return 2
    text = "".join(output)
    summaries = [tuple(map(int, match)) for match in SUMMARY.findall(text)]
    passed = sum(row[0] for row in summaries)
    failed = sum(row[1] for row in summaries)
    skipped = sum(row[2] for row in summaries)
    # These existing native fixture macros return early rather than marking a
    # Rust harness test ignored. Such a return is not substantive execution.
    early_returns = text.count(
        "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
    ) + text.count("is set to 'seatbelt', skipping test.")
    substantive = bool(summaries) and all(sum(row[:2]) > 0 for row in summaries)
    success = returncode == 0 and substantive and failed == 0 and skipped == 0 and early_returns == 0
    value = {
        "schema": "loops.verification.result.v1",
        "executed": passed + failed + skipped,
        "passed": passed, "failed": failed, "skipped": skipped,
        "undefined": 0, "pending": 0, "ambiguous": 0,
        "relevantEvidence": [
            {"scenario": args.scenario, "requirementRow": row,
             "command": command, "nativeSummaries": summaries}
            for row in args.requirement_rows.split(",")
        ] if success else [],
    }
    write_result(value)
    if not success:
        print("Native check failed, skipped required tests, or selected no substantive tests.",
              file=sys.stderr)
    return 0 if success else (returncode or 1)


if __name__ == "__main__":
    sys.exit(main())
