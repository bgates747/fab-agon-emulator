#!/usr/bin/env python3
"""Report Fab upstream divergence and optionally fetch or merge it."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from pingo_helpers import (
    PingoToolError,
    git_output,
    git_repository_status,
    print_error,
    run,
)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--fetch",
        action="store_true",
        help="fetch upstream before reporting",
    )
    parser.add_argument(
        "--merge",
        action="store_true",
        help="fetch and merge upstream/main into the current clean branch",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        root = Path(__file__).resolve().parent.parent
        remotes = git_output(root, "remote").splitlines()
        if "upstream" not in remotes:
            raise PingoToolError(
                "No upstream remote; expected tomm/fab-agon-emulator as upstream"
            )

        if arguments.fetch or arguments.merge:
            run(["git", "fetch", "--prune", "upstream"], cwd=root)

        try:
            upstream_head = git_output(root, "rev-parse", "upstream/main")
        except PingoToolError as error:
            raise PingoToolError(
                "upstream/main is unavailable; run with --fetch"
            ) from error

        status = git_repository_status(root)
        counts = git_output(
            root, "rev-list", "--left-right", "--count", "HEAD...upstream/main"
        ).split()
        ahead, behind = int(counts[0]), int(counts[1])
        print(f"current:       {status['branch']} {status['head']}")
        print(f"upstream/main: {upstream_head}")
        print(f"divergence:    {ahead} local commit(s), {behind} upstream commit(s)")

        if arguments.merge:
            if status["dirty"]:
                raise PingoToolError("Refusing to merge into a dirty working tree")
            if behind == 0:
                print("merge:         already contains upstream/main")
            else:
                run(["git", "merge", "--no-edit", "upstream/main"], cwd=root)
                print("merge:         completed locally; review and push explicitly")
        elif behind:
            print("action:        review changes, then rerun with --merge if desired")
        else:
            print("action:        no upstream merge required")
        return 0
    except (OSError, PingoToolError) as error:
        print_error(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
