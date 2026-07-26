#!/usr/bin/env python3
"""Run deterministic headless Pingo fixture regressions under Fab."""

from __future__ import annotations

import argparse
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

from pingo_helpers import (
    PingoToolError,
    ProjectPaths,
    add_path_arguments,
    paths_from_arguments,
    print_error,
    require_file,
    run,
    sha256_file,
)


@dataclass(frozen=True)
class Oracle:
    fixture: str
    width: int
    height: int
    bytes: int
    sha256: str


ORACLES = {
    "moveobj/tri": Oracle(
        fixture="moveobj/tri",
        width=320,
        height=240,
        bytes=76_800,
        sha256="f81dd66876ef012a6f1e52bae2821c275f1cf33e9a7e977c193be93bad4b4958",
    ),
    "moveair/jet": Oracle(
        fixture="moveair/jet",
        width=320,
        height=148,
        bytes=47_360,
        sha256="768f07b8115df6391d9a0a1611adf9e293a96740a4962d049788c15777ecdd5e",
    ),
}


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    add_path_arguments(parser)
    parser.add_argument(
        "fixtures",
        nargs="*",
        choices=sorted(ORACLES),
        default=None,
        help="fixtures to test (default: all accepted fixtures)",
    )
    parser.add_argument(
        "--rebuild",
        action="store_true",
        help="build and smoke-test the Pingo VDP before running fixtures",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=20.0,
        help="seconds to wait for each capture (default: 20)",
    )
    parser.add_argument(
        "--keep-captures",
        metavar="DIRECTORY",
        help="copy successful capture files and emulator logs here",
    )
    return parser.parse_args()


def stop_process_group(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGINT)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()


def run_fixture(
    oracle: Oracle,
    *,
    paths: ProjectPaths,
    timeout: float,
    output_root: Path,
) -> None:
    prefix = output_root / oracle.fixture.replace("/", "-")
    log_path = prefix.with_suffix(".log")
    environment = os.environ.copy()
    environment.update(
        {
            "FAB_EMULATOR_BIN": str(paths.fab_binary),
            "PINGO_VDP_SO": str(paths.vdp_shared_object),
            "PINGOASM_ROOT": str(paths.pingoasm_root),
            "PINGO_CAPTURE_PREFIX": str(prefix),
            "PINGO_CAPTURE_FRAME": "1",
            "SDL_VIDEODRIVER": "dummy",
            "SDL_AUDIODRIVER": "dummy",
        }
    )
    launcher = paths.fab_root / "scripts/run-pingo"
    require_file(launcher, "Pingo launcher")

    with log_path.open("wb") as log:
        process = subprocess.Popen(
            [str(launcher), oracle.fixture],
            cwd=paths.fab_root,
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        deadline = time.monotonic() + timeout
        marker = prefix.with_suffix(".txt")
        try:
            while time.monotonic() < deadline:
                if marker.is_file():
                    break
                returncode = process.poll()
                if returncode is not None:
                    raise PingoToolError(
                        f"{oracle.fixture} exited before capture "
                        f"(status {returncode}); see {log_path}"
                    )
                time.sleep(0.05)
            else:
                raise PingoToolError(
                    f"{oracle.fixture} did not capture within {timeout:g}s; "
                    f"see {log_path}"
                )
        finally:
            stop_process_group(process)

    raw = require_file(prefix.with_suffix(".rgba2"), "raw Pingo capture")
    actual_bytes = raw.stat().st_size
    actual_sha = sha256_file(raw)
    if actual_bytes != oracle.bytes or actual_sha != oracle.sha256:
        raise PingoToolError(
            f"{oracle.fixture} regression mismatch: expected "
            f"{oracle.bytes} bytes sha256 {oracle.sha256}, got "
            f"{actual_bytes} bytes sha256 {actual_sha}; see {output_root}"
        )

    metadata = marker.read_text(encoding="utf-8")
    expected_size = f"width={oracle.width}\nheight={oracle.height}\n"
    if expected_size not in metadata:
        raise PingoToolError(
            f"{oracle.fixture} metadata does not report "
            f"{oracle.width}x{oracle.height}; see {marker}"
        )
    print(
        f"PASS {oracle.fixture}: {actual_bytes} bytes sha256 {actual_sha}"
    )


def main() -> int:
    arguments = parse_arguments()
    if arguments.timeout <= 0:
        print_error(PingoToolError("--timeout must be positive"))
        return 2

    try:
        paths = paths_from_arguments(arguments)
        fixtures = arguments.fixtures or list(ORACLES)
        require_file(paths.fab_binary, "Fab emulator executable")
        if arguments.rebuild:
            build_script = paths.fab_root / "scripts/build-pingo-vdp.py"
            run(
                [
                    sys.executable,
                    str(build_script),
                    "--fab-root",
                    str(paths.fab_root),
                    "--vdp-root",
                    str(paths.vdp_root),
                    "--pingoasm-root",
                    str(paths.pingoasm_root),
                ],
                cwd=paths.fab_root,
            )
        require_file(paths.vdp_shared_object, "Pingo VDP module")

        keep_root = (
            Path(arguments.keep_captures).expanduser().resolve()
            if arguments.keep_captures
            else None
        )
        output_root = Path(tempfile.mkdtemp(prefix="pingo-regression-"))
        try:
            for fixture in fixtures:
                run_fixture(
                    ORACLES[fixture],
                    paths=paths,
                    timeout=arguments.timeout,
                    output_root=output_root,
                )
            if keep_root:
                keep_root.mkdir(parents=True, exist_ok=True)
                for artifact in output_root.iterdir():
                    shutil.copy2(artifact, keep_root / artifact.name)
                print(f"Captures retained in {keep_root}")
        except Exception:
            print(f"Failed regression artifacts retained in {output_root}")
            raise
        else:
            shutil.rmtree(output_root)
        print(f"All {len(fixtures)} Pingo regression(s) passed")
        return 0
    except (OSError, PingoToolError) as error:
        print_error(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
