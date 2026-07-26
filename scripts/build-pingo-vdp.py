#!/usr/bin/env python3
"""Build and smoke-test the Pingo-enabled native VDP module."""

from __future__ import annotations

import argparse
import sys

from pingo_helpers import (
    PingoToolError,
    add_path_arguments,
    artifact_status,
    paths_from_arguments,
    print_error,
    require_directory,
    require_file,
    run,
)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    add_path_arguments(parser)
    parser.add_argument(
        "--clean",
        action="store_true",
        help="remove native VDP build products before rebuilding",
    )
    parser.add_argument(
        "--no-smoke",
        action="store_true",
        help="build the module without running its ABI/render smoke test",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        paths = paths_from_arguments(arguments)
        userspace = require_directory(
            paths.vdp_root / "userspace", "Pingo userspace build directory"
        )
        require_file(userspace / "Makefile", "Pingo userspace Makefile")
        require_file(
            paths.fab_root / "src/vdp/rust_glue.cpp",
            "Fab native VDP glue",
        )
        require_file(
            paths.fab_root / "src/vdp/userspace-vdp-gl/src/Makefile",
            "Fab userspace-vdp-gl submodule",
        )

        make_base = [
            "make",
            "-C",
            str(userspace),
            f"FAB_ROOT={paths.fab_root}",
        ]
        if arguments.clean:
            run([*make_base, "clean"])
        run([*make_base, "all" if arguments.no_smoke else "smoke"])

        module = require_file(paths.vdp_shared_object, "Pingo VDP module")
        identity = artifact_status(module)
        print("Pingo native VDP ready")
        print(f"  path:   {identity['path']}")
        print(f"  bytes:  {identity['bytes']}")
        print(f"  sha256: {identity['sha256']}")
        print(f"  smoke:  {'skipped' if arguments.no_smoke else 'passed'}")
        return 0
    except (OSError, PingoToolError) as error:
        print_error(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
