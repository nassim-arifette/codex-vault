#!/usr/bin/env python3
"""Keep the public Codex compatibility claims tied to actual validation evidence."""

from __future__ import annotations

import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MATRIX_PATH = ROOT / "docs" / "validation" / "codex-compatibility.json"
CI_PATH = ROOT / ".github" / "workflows" / "ci.yml"
SEMVER = re.compile(r"^\d+\.\d+\.\d+$")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def ci_versions(text: str) -> list[str]:
    match = re.search(r"(?m)^\s*codex:\s*\[([^\]]+)\]\s*$", text)
    require(match is not None, "CI Codex matrix not found")
    return re.findall(r"['\"](\d+\.\d+\.\d+)['\"]", match.group(1))


def main() -> None:
    matrix = json.loads(MATRIX_PATH.read_text(encoding="utf-8"))
    require(matrix.get("schema_version") == 1, "unexpected compatibility schema")
    required = matrix.get("tested_requires")
    require(
        required
        == ["synthetic", "representative_real_corpus", "event_type_audit", "refusal_behavior"],
        "tested evidence policy changed unexpectedly",
    )

    entries = matrix.get("versions")
    require(isinstance(entries, list) and len(entries) >= 4, "compatibility matrix did not expand")
    versions = [entry.get("codex") for entry in entries]
    require(all(isinstance(v, str) and SEMVER.fullmatch(v) for v in versions), "invalid Codex version")
    require(len(versions) == len(set(versions)), "duplicate Codex version")

    for entry in entries:
        version = entry["codex"]
        if entry.get("status") != "tested":
            continue
        for key in required:
            evidence = entry.get(key)
            require(isinstance(evidence, dict), f"{version}: missing {key} evidence")
            require(evidence.get("status") == "pass", f"{version}: {key} is not PASS")
        synthetic = entry["synthetic"]
        real = entry["representative_real_corpus"]
        audit = entry["event_type_audit"]
        refusal = entry["refusal_behavior"]
        require(synthetic.get("cases", 0) > 0, f"{version}: empty synthetic corpus")
        require(synthetic.get("resumed_turns_per_arm", 0) >= 2, f"{version}: insufficient resumed turns")
        require(real.get("cases", 0) >= 5, f"{version}: representative real corpus too small")
        require(real.get("compact_allowed", 0) > 0, f"{version}: no real compactable case")
        require(real.get("protected", 0) > 0, f"{version}: no real refusal/archive case")
        require(audit.get("fresh_writer") is True, f"{version}: event audit is not fresh-writer evidence")
        require(audit.get("unknown_outer_types") == [], f"{version}: unresolved outer rollout types")
        require(audit.get("unknown_event_types") == [], f"{version}: unresolved event_msg types")
        require(
            audit.get("unknown_response_item_types") == [],
            f"{version}: unresolved response-item types",
        )
        require(refusal.get("byte_identical") is True, f"{version}: refusal byte identity not proven")

    expected_ci = [entry["codex"] for entry in entries if entry.get("ci") is True]
    actual_ci = ci_versions(CI_PATH.read_text(encoding="utf-8"))
    require(actual_ci == expected_ci, f"CI matrix {actual_ci} != documented matrix {expected_ci}")
    print(f"compatibility matrix OK: {', '.join(versions)}")


if __name__ == "__main__":
    main()
