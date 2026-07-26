#!/usr/bin/env python3
"""Report exact repository and artifact identities for Pingo development."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

from pingo_helpers import (
    PingoToolError,
    add_path_arguments,
    artifact_status,
    git_repository_status,
    paths_from_arguments,
    print_error,
)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    add_path_arguments(parser)
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit machine-readable JSON",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="return nonzero for a missing artifact or dirty repository",
    )
    return parser.parse_args()


def fixture_artifact(root: Path, relative: str) -> dict[str, Any]:
    result = artifact_status(root / "src/asm" / relative)
    result["fixture"] = relative.removesuffix(".bin")
    return result


def render_report(report: dict[str, Any]) -> None:
    print("Repositories")
    for name, repository in report["repositories"].items():
        if not repository["available"]:
            print(f"  {name}: unavailable ({repository['path']})")
            continue
        tracking = ""
        if repository.get("upstream"):
            tracking = (
                f", {repository['ahead']} ahead/"
                f"{repository['behind']} behind {repository['upstream']}"
            )
        print(
            f"  {name}: {repository['branch']} "
            f"{repository['head'][:12]}{tracking}"
        )
        print(
            f"    {repository['path']} "
            f"({'dirty' if repository['dirty'] else 'clean'})"
        )
        for change in repository["changes"]:
            print(f"      {change}")

    print("Artifacts")
    for name, artifact in report["artifacts"].items():
        if artifact["available"]:
            print(
                f"  {name}: {artifact['bytes']} bytes "
                f"sha256 {artifact['sha256']}"
            )
            print(f"    {artifact['path']}")
        else:
            print(f"  {name}: missing")
            print(f"    {artifact['path']}")


def main() -> int:
    arguments = parse_arguments()
    try:
        paths = paths_from_arguments(arguments)
        report = {
            "repositories": {
                "fab": git_repository_status(paths.fab_root),
                "agon-vdp": git_repository_status(paths.vdp_root),
                "pingoasm": git_repository_status(paths.pingoasm_root),
            },
            "artifacts": {
                "fab-emulator": artifact_status(paths.fab_binary),
                "pingo-vdp": artifact_status(paths.vdp_shared_object),
                "moveobj/tri": fixture_artifact(
                    paths.pingoasm_root, "moveobj/tri.bin"
                ),
                "moveair/jet": fixture_artifact(
                    paths.pingoasm_root, "moveair/jet.bin"
                ),
            },
        }
        if arguments.json:
            print(json.dumps(report, indent=2, sort_keys=True))
        else:
            render_report(report)

        if arguments.strict:
            repositories_ok = all(
                repository["available"] and not repository.get("dirty", True)
                for repository in report["repositories"].values()
            )
            artifacts_ok = all(
                artifact["available"] for artifact in report["artifacts"].values()
            )
            return 0 if repositories_ok and artifacts_ok else 1
        return 0
    except (OSError, PingoToolError) as error:
        print_error(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
