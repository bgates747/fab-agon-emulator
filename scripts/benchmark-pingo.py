#!/usr/bin/env python3
"""Run repeatable, silent Pingo benchmarks in fresh Fab processes."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import os
import platform
import re
import selectors
import shlex
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, BinaryIO

from pingo_helpers import (
    PingoToolError,
    artifact_status,
    git_repository_status,
    print_error,
    require_directory,
    require_file,
    sha256_file,
)


SCHEMA_VERSION = 1
RECORD_PREFIX = "PINGO_RENDER "
FIELD_RE = re.compile(r"[a-z][a-z0-9_]*")
MANDATORY_FIELDS = ("seq", "bmid", "render_us")


def expected_stream_argument(value: str) -> tuple[int, int]:
    """Parse one BMID:COUNT stream declaration for argparse."""
    parts = value.split(":")
    if len(parts) != 2 or not all(part.isdecimal() for part in parts):
        raise argparse.ArgumentTypeError(
            "expected stream must use decimal BMID:COUNT"
        )
    bmid, count = (int(part) for part in parts)
    if not 0 <= bmid <= 0xFFFF:
        raise argparse.ArgumentTypeError(
            "expected stream bitmap ID must be between 0 and 65535"
        )
    if count <= 0:
        raise argparse.ArgumentTypeError(
            "expected stream count must be positive"
        )
    return bmid, count


def expected_stream_map(
    declarations: list[tuple[int, int]],
) -> dict[int, int]:
    streams: dict[int, int] = {}
    for bmid, count in declarations:
        if bmid in streams:
            raise PingoToolError(
                f"--expected-stream declares bitmap ID {bmid} more than once"
            )
        streams[bmid] = count
    return streams


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--emulator",
        type=Path,
        required=True,
        help="Fab executable to run",
    )
    parser.add_argument(
        "--vdp",
        type=Path,
        required=True,
        help="exact native Pingo VDP shared object to load",
    )
    parser.add_argument(
        "--sdcard",
        type=Path,
        required=True,
        help="emulated SD-card directory whose autoexec selects the fixture",
    )
    expectation = parser.add_mutually_exclusive_group(required=True)
    expectation.add_argument(
        "--expected-count",
        type=int,
        help=(
            "single-sequence mode: exact PINGO_RENDER record count expected "
            "from each process"
        ),
    )
    expectation.add_argument(
        "--expected-stream",
        type=expected_stream_argument,
        action="append",
        default=[],
        metavar="BMID:COUNT",
        help=(
            "chained-suite mode: exact record count for one independently "
            "sequenced bitmap stream; repeat for each bitmap ID"
        ),
    )
    parser.add_argument(
        "--repeats",
        type=int,
        required=True,
        help="number of fresh emulator processes to run",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="JSON report to create; per-run logs are written beside it",
    )
    parser.add_argument(
        "--mos",
        type=Path,
        help="MOS binary (default: firmware/mos_console8.bin in this checkout)",
    )
    parser.add_argument(
        "--expected-bmid",
        type=int,
        action="append",
        default=[],
        help=(
            "single-sequence mode: allow only this output bitmap ID; may be "
            "repeated (default: accept and report every ID)"
        ),
    )
    parser.add_argument(
        "--artifact",
        type=Path,
        action="append",
        default=[],
        help="additional fixture/profile artifact to hash; may be repeated",
    )
    parser.add_argument(
        "--label",
        default="Pingo emulator benchmark",
        help="human-readable run label stored in the report",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=120.0,
        help="maximum wall seconds per process (default: 120)",
    )
    parser.add_argument(
        "--post-complete-grace",
        type=float,
        default=1.0,
        help=(
            "seconds to allow client cleanup and detect extra records "
            "after the expected final record (default: 1)"
        ),
    )
    parser.add_argument(
        "--shutdown-timeout",
        type=float,
        default=5.0,
        help="seconds to wait for orderly debugger shutdown (default: 5)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="replace an existing report and its planned per-run logs",
    )
    return parser.parse_args()


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def resolve_file(path: Path, description: str) -> Path:
    return require_file(path.expanduser().resolve(), description)


def resolve_directory(path: Path, description: str) -> Path:
    return require_directory(path.expanduser().resolve(), description)


def validate_positive(value: float | int, option: str) -> None:
    if isinstance(value, float) and not math.isfinite(value):
        raise PingoToolError(f"{option} must be finite and positive")
    if value <= 0:
        raise PingoToolError(f"{option} must be positive")


def parse_render_record(line: str) -> dict[str, Any] | None:
    marker = line.find(RECORD_PREFIX)
    if marker < 0:
        return None
    record = line[marker:].strip()
    tokens = record.split()
    if not tokens or tokens[0] != "PINGO_RENDER":
        return None

    fields: dict[str, int] = {}
    for token in tokens[1:]:
        if token.count("=") != 1:
            raise PingoToolError(f"malformed PINGO_RENDER token: {token!r}")
        key, value = token.split("=", 1)
        if not FIELD_RE.fullmatch(key):
            raise PingoToolError(f"invalid PINGO_RENDER key: {key!r}")
        if key in fields:
            raise PingoToolError(f"duplicate PINGO_RENDER key: {key}")
        if not value.isdecimal():
            raise PingoToolError(
                f"non-decimal PINGO_RENDER value for {key}: {value!r}"
            )
        fields[key] = int(value)

    missing = [key for key in MANDATORY_FIELDS if key not in fields]
    if missing:
        raise PingoToolError(
            "PINGO_RENDER record missing " + ", ".join(missing)
        )
    if fields["bmid"] > 0xFFFF:
        raise PingoToolError(
            f"PINGO_RENDER bitmap ID out of range: {fields['bmid']}"
        )
    if fields["seq"] > 0xFFFFFFFF:
        raise PingoToolError(
            f"PINGO_RENDER sequence out of range: {fields['seq']}"
        )
    return {
        "seq": fields.pop("seq"),
        "bmid": fields.pop("bmid"),
        "render_us": fields.pop("render_us"),
        "additional_fields": fields,
        "raw": record,
    }


def add_record(
    samples: list[dict[str, Any]],
    sample: dict[str, Any],
    *,
    expected_count: int,
    allowed_bmids: set[int],
    expected_streams: dict[int, int] | None = None,
    stream_positions: dict[int, int] | None = None,
) -> None:
    bmid = sample["bmid"]
    if expected_streams is not None:
        if bmid not in expected_streams:
            raise PingoToolError(
                f"unexpected bitmap ID {bmid}; declared streams are "
                + ", ".join(str(value) for value in sorted(expected_streams))
            )
        expected_sequence = (
            stream_positions[bmid]
            if stream_positions is not None
            else sum(prior["bmid"] == bmid for prior in samples)
        )
        stream_limit = expected_streams[bmid]
        if expected_sequence >= stream_limit:
            raise PingoToolError(
                f"received more than the expected {stream_limit} records "
                f"for bitmap stream {bmid}"
            )
    else:
        expected_sequence = len(samples)

    sequence = sample["seq"]
    if sequence != expected_sequence:
        if sequence < expected_sequence:
            detail = "duplicate or reset"
        else:
            detail = "missing record"
        stream_detail = (
            f" for bitmap stream {bmid}"
            if expected_streams is not None
            else ""
        )
        raise PingoToolError(
            f"noncontiguous render sequence{stream_detail} ({detail}): "
            f"expected {expected_sequence}, received {sequence}"
        )
    if len(samples) >= expected_count:
        raise PingoToolError(
            f"received more than the expected {expected_count} records"
        )
    if allowed_bmids and bmid not in allowed_bmids:
        raise PingoToolError(
            f"unexpected bitmap ID {bmid}; allowed IDs are "
            + ", ".join(str(value) for value in sorted(allowed_bmids))
        )
    samples.append(sample)
    if stream_positions is not None:
        stream_positions[bmid] = expected_sequence + 1


def validate_record_counts(
    samples: list[dict[str, Any]],
    *,
    expected_count: int,
    expected_streams: dict[int, int] | None,
) -> None:
    if len(samples) != expected_count:
        raise PingoToolError(
            f"produced {len(samples)} records; expected {expected_count}"
        )
    if expected_streams is None:
        return
    actual = {
        bmid: sum(sample["bmid"] == bmid for sample in samples)
        for bmid in expected_streams
    }
    mismatches = [
        f"{bmid}: expected {expected_streams[bmid]}, received {actual[bmid]}"
        for bmid in sorted(expected_streams)
        if actual[bmid] != expected_streams[bmid]
    ]
    if mismatches:
        raise PingoToolError(
            "stream record count mismatch: " + "; ".join(mismatches)
        )


def percentile_nearest_rank(values: list[int], fraction: float) -> int:
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def summarize_durations(values: list[int]) -> dict[str, Any]:
    if not values:
        raise PingoToolError("cannot summarize an empty render sample")
    mean = statistics.fmean(values)
    return {
        "count": len(values),
        "total_render_us": sum(values),
        "minimum_render_us": min(values),
        "mean_render_us": mean,
        "median_render_us": statistics.median(values),
        "population_stdev_us": statistics.pstdev(values),
        "p95_render_us": percentile_nearest_rank(values, 0.95),
        "maximum_render_us": max(values),
        "equivalent_mean_fps": 1_000_000 / mean if mean else None,
    }


def summarize_streams(
    samples: list[dict[str, Any]],
    expected_streams: dict[int, int],
) -> list[dict[str, Any]]:
    summaries = []
    for bmid in sorted(expected_streams):
        durations = [
            sample["render_us"]
            for sample in samples
            if sample["bmid"] == bmid
        ]
        summaries.append(
            {
                "bmid": bmid,
                "expected_count": expected_streams[bmid],
                **summarize_durations(durations),
            }
        )
    return summaries


def debugger_command(
    *,
    emulator: Path,
    vdp: Path,
    sdcard: Path,
    mos: Path,
) -> list[str]:
    return [
        str(emulator),
        "--debugger",
        "--renderer",
        "sw",
        "--firmware",
        "console8",
        "--mos",
        str(mos),
        "--vdp",
        str(vdp),
        "--sdcard",
        str(sdcard),
        "-z",
        "-u",
    ]


def benchmark_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "SDL_VIDEODRIVER": "dummy",
            "SDL_AUDIODRIVER": "dummy",
            "LC_ALL": "C",
        }
    )
    local_sdl = Path.home() / ".local/lib/libSDL3.so.0"
    if local_sdl.is_file():
        local_lib = str(local_sdl.parent)
        inherited = environment.get("LD_LIBRARY_PATH", "")
        parts = [part for part in inherited.split(":") if part]
        if local_lib not in parts:
            environment["LD_LIBRARY_PATH"] = ":".join(
                [local_lib, *parts]
            )
    return environment


def drain_available(
    *,
    process: subprocess.Popen[bytes],
    selector: selectors.BaseSelector,
    pending: bytearray,
    raw_log: BinaryIO,
    samples: list[dict[str, Any]],
    expected_count: int,
    allowed_bmids: set[int],
    wait_seconds: float,
    expected_streams: dict[int, int] | None = None,
    stream_positions: dict[int, int] | None = None,
) -> bool:
    """Drain child output once; return False at clean pipe EOF."""
    events = selector.select(timeout=wait_seconds)
    if not events:
        return True
    assert process.stdout is not None
    chunk = os.read(process.stdout.fileno(), 65536)
    if not chunk:
        return False
    raw_log.write(chunk)
    raw_log.flush()
    pending.extend(chunk)
    while b"\n" in pending:
        raw_line, _, remainder = pending.partition(b"\n")
        pending[:] = remainder
        line = raw_line.decode("utf-8", errors="replace").rstrip("\r")
        sample = parse_render_record(line)
        if sample is not None:
            add_record(
                samples,
                sample,
                expected_count=expected_count,
                allowed_bmids=allowed_bmids,
                expected_streams=expected_streams,
                stream_positions=stream_positions,
            )
    return True


def drain_remainder(
    *,
    process: subprocess.Popen[bytes],
    pending: bytearray,
    raw_log: BinaryIO,
    samples: list[dict[str, Any]],
    expected_count: int,
    allowed_bmids: set[int],
    expected_streams: dict[int, int] | None = None,
    stream_positions: dict[int, int] | None = None,
) -> None:
    assert process.stdout is not None
    remainder = process.stdout.read()
    if remainder:
        raw_log.write(remainder)
        pending.extend(remainder)
    raw_log.flush()
    for raw_line in pending.splitlines():
        line = raw_line.decode("utf-8", errors="replace").rstrip("\r")
        sample = parse_render_record(line)
        if sample is not None:
            add_record(
                samples,
                sample,
                expected_count=expected_count,
                allowed_bmids=allowed_bmids,
                expected_streams=expected_streams,
                stream_positions=stream_positions,
            )
    pending.clear()


def terminate_failed_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def run_once(
    *,
    run_number: int,
    command: list[str],
    environment: dict[str, str],
    vdp_directory: Path,
    log_path: Path,
    expected_count: int,
    allowed_bmids: set[int],
    expected_streams: dict[int, int] | None,
    timeout: float,
    post_complete_grace: float,
    shutdown_timeout: float,
) -> dict[str, Any]:
    started_at = utc_now()
    wall_started = time.monotonic()
    first_record_at: float | None = None
    last_record_at: float | None = None
    shutdown_started: float | None = None
    samples: list[dict[str, Any]] = []
    stream_positions = (
        {bmid: 0 for bmid in expected_streams}
        if expected_streams is not None
        else None
    )
    pending = bytearray()
    deadline = wall_started + timeout
    process = subprocess.Popen(
        command,
        cwd=vdp_directory,
        env=environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=0,
        start_new_session=True,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)

    try:
        with log_path.open("wb") as raw_log:
            while len(samples) < expected_count:
                now = time.monotonic()
                if now >= deadline:
                    raise PingoToolError(
                        f"run {run_number} timed out after {len(samples)} of "
                        f"{expected_count} records"
                    )
                before = len(samples)
                pipe_open = drain_available(
                    process=process,
                    selector=selector,
                    pending=pending,
                    raw_log=raw_log,
                    samples=samples,
                    expected_count=expected_count,
                    allowed_bmids=allowed_bmids,
                    expected_streams=expected_streams,
                    stream_positions=stream_positions,
                    wait_seconds=min(0.25, deadline - now),
                )
                if len(samples) != before:
                    record_time = time.monotonic()
                    first_record_at = first_record_at or record_time
                    last_record_at = record_time
                if not pipe_open or process.poll() is not None:
                    raise PingoToolError(
                        f"run {run_number} exited with status "
                        f"{process.returncode} after {len(samples)} of "
                        f"{expected_count} records"
                    )

            # Let the client execute its post-render tail and return to MOS.
            # Continue parsing so a too-small expected count cannot pass.
            grace_deadline = min(
                deadline, time.monotonic() + post_complete_grace
            )
            while time.monotonic() < grace_deadline:
                pipe_open = drain_available(
                    process=process,
                    selector=selector,
                    pending=pending,
                    raw_log=raw_log,
                    samples=samples,
                    expected_count=expected_count,
                    allowed_bmids=allowed_bmids,
                    expected_streams=expected_streams,
                    stream_positions=stream_positions,
                    wait_seconds=min(
                        0.1, max(0.0, grace_deadline - time.monotonic())
                    ),
                )
                if not pipe_open or process.poll() is not None:
                    break

            if process.poll() is None:
                # Fab's SIGINT handler pauses the eZ80 only when --debugger is
                # enabled. Once paused, "exit" requests orderly main-loop and
                # VDP shutdown. Sending exit through the existing pipe after a
                # short delay avoids relying on ANSI debugger prompt parsing.
                shutdown_started = time.monotonic()
                os.kill(process.pid, signal.SIGINT)
                time.sleep(0.5)
                process.stdin.write(b"exit\n")
                process.stdin.flush()
                try:
                    status = process.wait(timeout=shutdown_timeout)
                except subprocess.TimeoutExpired as error:
                    raise PingoToolError(
                        f"run {run_number} did not shut down cleanly within "
                        f"{shutdown_timeout:g}s"
                    ) from error
            else:
                status = process.returncode

            drain_remainder(
                process=process,
                pending=pending,
                raw_log=raw_log,
                samples=samples,
                expected_count=expected_count,
                allowed_bmids=allowed_bmids,
                expected_streams=expected_streams,
                stream_positions=stream_positions,
            )
    finally:
        selector.close()
        terminate_failed_process(process)

    if status != 0:
        raise PingoToolError(
            f"run {run_number} exited with nonzero status {status}"
        )
    try:
        validate_record_counts(
            samples,
            expected_count=expected_count,
            expected_streams=expected_streams,
        )
    except PingoToolError as error:
        raise PingoToolError(f"run {run_number} {error}") from error

    wall_finished = time.monotonic()
    durations = [sample["render_us"] for sample in samples]
    result = {
        "run": run_number,
        "started_at": started_at,
        "finished_at": utc_now(),
        "exit_status": status,
        "wall_seconds": wall_finished - wall_started,
        "startup_to_first_record_seconds": (
            first_record_at - wall_started
            if first_record_at is not None
            else None
        ),
        "first_to_last_record_seconds": (
            last_record_at - first_record_at
            if first_record_at is not None and last_record_at is not None
            else None
        ),
        "orderly_shutdown_seconds": (
            wall_finished - shutdown_started
            if shutdown_started is not None
            else 0.0
        ),
        "bitmap_ids": sorted({sample["bmid"] for sample in samples}),
        "render_summary": summarize_durations(durations),
        "samples": samples,
        "emulator_log": artifact_status(log_path),
    }
    if expected_streams is not None:
        result["stream_summaries"] = summarize_streams(
            samples, expected_streams
        )
    return result


def command_words(line: str) -> list[str]:
    try:
        return shlex.split(line, posix=True)
    except ValueError:
        return line.split()


def sdcard_manifest(
    sdcard: Path,
    *,
    allow_chained: bool = False,
) -> dict[str, Any]:
    autoexec = require_file(sdcard / "autoexec.txt", "SD autoexec.txt")
    sdcard_root = sdcard.resolve()
    current_directory = Path("/")
    load_targets: list[dict[str, Any]] = []
    run_commands: list[dict[str, Any]] = []
    pending_load: dict[str, Any] | None = None
    for line_number, raw_line in enumerate(autoexec.read_text(
        encoding="utf-8", errors="replace"
    ).splitlines(), start=1):
        words = command_words(raw_line.strip())
        if not words:
            continue
        command = words[0].casefold()
        if command in ("rem", "#") or raw_line.lstrip().startswith("#"):
            continue
        if command == "cd" and len(words) >= 2:
            requested = Path(words[1])
            current_directory = (
                requested
                if requested.is_absolute()
                else current_directory / requested
            )
        elif command == "load" and len(words) >= 2:
            if pending_load is not None:
                raise PingoToolError(
                    "benchmark SD autoexec LOAD on line "
                    f"{pending_load['line']} has no following RUN before "
                    f"LOAD on line {line_number}"
                )
            logical = current_directory / words[1]
            lexical_host = Path(os.path.abspath(
                sdcard_root / str(logical).lstrip("/")
            ))
            try:
                lexical_host.relative_to(sdcard_root)
            except ValueError as error:
                raise PingoToolError(
                    "SD autoexec LOAD escapes the emulated SD card on "
                    f"line {line_number}: {logical}"
                ) from error
            # Project-local emulator deployments intentionally use symlinks
            # from the SD tree into pingoasm. Reject lexical ".." escapes,
            # but retain that supported symlink deployment convention.
            host = lexical_host.resolve()
            pending_load = {
                "logical_path": "/" + str(logical).lstrip("/"),
                "host_path": str(host),
                "line": line_number,
            }
            load_targets.append(pending_load)
        elif command == "run":
            if pending_load is None:
                raise PingoToolError(
                    "benchmark SD autoexec must contain one active LOAD "
                    f"before each RUN; RUN on line {line_number} must follow "
                    "its LOAD"
                )
            pending_load["run_line"] = line_number
            run_commands.append(
                {
                    "line": line_number,
                    "load_line": pending_load["line"],
                    "logical_path": pending_load["logical_path"],
                }
            )
            pending_load = None

    if pending_load is not None:
        raise PingoToolError(
            "benchmark SD autoexec must contain one active RUN for each "
            f"LOAD; LOAD on line {pending_load['line']} has no following RUN"
        )
    if not load_targets:
        raise PingoToolError(
            "benchmark SD autoexec must contain at least one active "
            "LOAD/RUN pair"
        )
    if not allow_chained and len(load_targets) != 1:
        raise PingoToolError(
            "single-fixture mode requires exactly one active LOAD/RUN pair; "
            f"found {len(load_targets)}"
        )
    for target in load_targets:
        require_file(
            Path(target["host_path"]),
            "benchmark program selected by SD autoexec",
        )

    runtime_directories = {
        Path(target["host_path"]).parent for target in load_targets
    }
    runtime_files: list[dict[str, Any]] = []
    for directory in sorted(runtime_directories, key=str):
        if not directory.is_dir():
            continue
        for path in sorted(directory.iterdir(), key=lambda item: item.name):
            if path.is_file():
                identity = artifact_status(path)
                identity["host_directory"] = str(directory)
                runtime_files.append(identity)

    return {
        "root": str(sdcard),
        "load_run_mode": "chained" if allow_chained else "single",
        "autoexec": artifact_status(autoexec),
        "load_targets": load_targets,
        "run_commands": run_commands,
        "selected_runtime_files": runtime_files,
    }


def find_repository(path: Path) -> Path | None:
    candidate = path if path.is_dir() else path.parent
    try:
        result = subprocess.run(
            ["git", "-C", str(candidate), "rev-parse", "--show-toplevel"],
            check=False,
            text=True,
            capture_output=True,
        )
    except FileNotFoundError:
        return None
    if result.returncode != 0:
        return None
    root = result.stdout.strip()
    return Path(root).resolve() if root else None


def planned_log_path(output: Path, run_number: int) -> Path:
    return output.with_name(
        f"{output.stem}.run-{run_number:03d}.log"
    )


def ensure_outputs_available(
    output: Path, repeats: int, *, force: bool
) -> None:
    candidates = [
        output,
        *(planned_log_path(output, index) for index in range(1, repeats + 1)),
    ]
    existing = [path for path in candidates if path.exists()]
    if existing and not force:
        raise PingoToolError(
            "refusing to overwrite existing output: "
            + ", ".join(str(path) for path in existing)
        )


def hashed_artifacts(value: Any) -> dict[Path, str]:
    """Collect every path/hash identity nested in an artifact manifest."""
    result: dict[Path, str] = {}
    if isinstance(value, dict):
        if (
            value.get("available") is True
            and isinstance(value.get("path"), str)
            and isinstance(value.get("sha256"), str)
        ):
            result[Path(value["path"])] = value["sha256"]
        for child in value.values():
            result.update(hashed_artifacts(child))
    elif isinstance(value, list):
        for child in value:
            result.update(hashed_artifacts(child))
    return result


def verify_artifacts_unchanged(
    identities: dict[Path, str],
    *,
    sdcard: Path,
    initial_sdcard_manifest: dict[str, Any],
) -> None:
    for path, before in identities.items():
        if not path.is_file():
            raise PingoToolError(
                f"benchmarked artifact disappeared during the suite: {path}"
            )
        if sha256_file(path) != before:
            raise PingoToolError(
                f"benchmarked artifact changed during the suite: {path}"
            )
    allow_chained = (
        initial_sdcard_manifest.get("load_run_mode") == "chained"
    )
    if sdcard_manifest(
        sdcard, allow_chained=allow_chained
    ) != initial_sdcard_manifest:
        raise PingoToolError(
            "selected SD-card program or runtime directory changed during "
            "the benchmark suite"
        )


def write_json_atomic(path: Path, payload: dict[str, Any]) -> None:
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(
        json.dumps(payload, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


def publish_report_and_logs(
    *,
    output: Path,
    staged_logs: list[Path],
    report: dict[str, Any],
) -> None:
    """Publish staged logs first and the report last.

    A forced rerun leaves the previous report and logs untouched until every
    run succeeds. During publication, the old report is removed before any
    log is replaced, so an interrupted replacement cannot leave stale JSON
    claiming hashes for a mixture of old and new logs.
    """
    runs = report.get("runs")
    if not isinstance(runs, list) or len(runs) != len(staged_logs):
        raise PingoToolError(
            "internal error: staged log count does not match report runs"
        )

    if output.exists():
        output.unlink()
    for run_number, (run_result, staged_log) in enumerate(
        zip(runs, staged_logs), start=1
    ):
        final_log = planned_log_path(output, run_number)
        staged_log.replace(final_log)
        run_result["emulator_log"] = artifact_status(final_log)

    write_json_atomic(output, report)


def main() -> int:
    arguments = parse_arguments()
    try:
        expected_streams = expected_stream_map(
            arguments.expected_stream
        )
        if expected_streams:
            if arguments.expected_bmid:
                raise PingoToolError(
                    "--expected-bmid is only valid with --expected-count; "
                    "--expected-stream already declares every allowed bitmap"
                )
            expected_count = sum(expected_streams.values())
        else:
            validate_positive(arguments.expected_count, "--expected-count")
            expected_count = arguments.expected_count

        validate_positive(arguments.repeats, "--repeats")
        validate_positive(arguments.timeout, "--timeout")
        validate_positive(
            arguments.post_complete_grace, "--post-complete-grace"
        )
        validate_positive(
            arguments.shutdown_timeout, "--shutdown-timeout"
        )
        for bmid in arguments.expected_bmid:
            if not 0 <= bmid <= 0xFFFF:
                raise PingoToolError(
                    f"--expected-bmid is outside 0..65535: {bmid}"
                )

        script_root = Path(__file__).resolve().parent.parent
        emulator = resolve_file(arguments.emulator, "Fab executable")
        vdp = resolve_file(arguments.vdp, "Pingo VDP shared object")
        sdcard = resolve_directory(arguments.sdcard, "emulated SD card")
        mos = resolve_file(
            arguments.mos
            if arguments.mos is not None
            else script_root / "firmware/mos_console8.bin",
            "MOS binary",
        )
        extra_artifacts = [
            resolve_file(path, "additional artifact")
            for path in arguments.artifact
        ]
        output = arguments.output.expanduser().resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        ensure_outputs_available(
            output, arguments.repeats, force=arguments.force
        )

        command = debugger_command(
            emulator=emulator,
            vdp=vdp,
            sdcard=sdcard,
            mos=mos,
        )
        environment = benchmark_environment()
        environment_record = {
            key: environment.get(key)
            for key in (
                "SDL_VIDEODRIVER",
                "SDL_AUDIODRIVER",
                "LC_ALL",
                "LD_LIBRARY_PATH",
            )
        }
        artifact_record = {
            "emulator": artifact_status(emulator),
            "vdp": artifact_status(vdp),
            "mos": artifact_status(mos),
            "harness": {
                "benchmark": artifact_status(Path(__file__).resolve()),
                "helpers": artifact_status(
                    Path(__file__).resolve().with_name("pingo_helpers.py")
                ),
            },
            "sdcard": sdcard_manifest(
                sdcard, allow_chained=bool(expected_streams)
            ),
            "additional": [
                artifact_status(path) for path in extra_artifacts
            ],
        }
        guarded_artifacts = hashed_artifacts(artifact_record)
        repository_roots = {
            root
            for root in (
                find_repository(emulator),
                find_repository(vdp),
                *(find_repository(path) for path in extra_artifacts),
            )
            if root is not None
        }

        report: dict[str, Any] = {
            "schema_version": SCHEMA_VERSION,
            "label": arguments.label,
            "created_at": utc_now(),
            "platform": "fab-agon-emulator-headless",
            "timing_scope": "VDP-emitted PINGO_RENDER render_us field",
            "measurement_warning": (
                "Emulator timing is host-specific functional/regression data; "
                "physical ESP32 hardware remains the performance ground truth."
            ),
            "configuration": {
                "validation_mode": (
                    "per_bitmap_streams"
                    if expected_streams
                    else "single_sequence"
                ),
                "expected_count_per_run": expected_count,
                "expected_streams": [
                    {"bmid": bmid, "count": count}
                    for bmid, count in sorted(expected_streams.items())
                ],
                "repeats": arguments.repeats,
                "expected_bitmap_ids": (
                    sorted(expected_streams)
                    if expected_streams
                    else sorted(set(arguments.expected_bmid))
                ),
                "timeout_seconds": arguments.timeout,
                "post_complete_grace_seconds": (
                    arguments.post_complete_grace
                ),
                "shutdown_timeout_seconds": arguments.shutdown_timeout,
                "zero_initialized_ram": True,
                "unlimited_ez80_cpu": True,
                "software_renderer": True,
                "dummy_video": True,
                "dummy_audio": True,
            },
            "command": command,
            "command_shell": shlex.join(command),
            "environment": environment_record,
            "host": {
                "uname": platform.uname()._asdict(),
                "cpu_count": os.cpu_count(),
                "python": platform.python_version(),
            },
            "repositories": [
                git_repository_status(root)
                for root in sorted(repository_roots, key=str)
            ],
            "artifacts": artifact_record,
            "runs": [],
        }

        allowed_bmids = set(arguments.expected_bmid)
        all_durations: list[int] = []
        all_stream_durations = {
            bmid: [] for bmid in expected_streams
        }
        staged_logs: list[Path] = []
        with tempfile.TemporaryDirectory(
            prefix=f".{output.stem}-benchmark-",
            dir=output.parent,
        ) as staging_directory:
            staging_root = Path(staging_directory)
            for run_number in range(1, arguments.repeats + 1):
                log_path = staging_root / planned_log_path(
                    output, run_number
                ).name
                staged_logs.append(log_path)
                run_result = run_once(
                    run_number=run_number,
                    command=command,
                    environment=environment,
                    vdp_directory=vdp.parent,
                    log_path=log_path,
                    expected_count=expected_count,
                    allowed_bmids=allowed_bmids,
                    expected_streams=(
                        expected_streams if expected_streams else None
                    ),
                    timeout=arguments.timeout,
                    post_complete_grace=arguments.post_complete_grace,
                    shutdown_timeout=arguments.shutdown_timeout,
                )
                report["runs"].append(run_result)
                all_durations.extend(
                    sample["render_us"]
                    for sample in run_result["samples"]
                )
                for bmid in expected_streams:
                    all_stream_durations[bmid].extend(
                        sample["render_us"]
                        for sample in run_result["samples"]
                        if sample["bmid"] == bmid
                    )
                summary = run_result["render_summary"]
                print(
                    f"PASS run {run_number}/{arguments.repeats}: "
                    f"{summary['count']} records, "
                    f"{summary['mean_render_us']:.1f} us mean "
                    f"({summary['equivalent_mean_fps']:.2f} FPS), "
                    f"{run_result['wall_seconds']:.2f}s wall"
                )
                if expected_streams:
                    stream_text = "; ".join(
                        f"bmid {item['bmid']}: {item['count']} at "
                        f"{item['mean_render_us']:.1f} us"
                        for item in run_result["stream_summaries"]
                    )
                    print(f"  streams: {stream_text}")

            report["aggregate_render_summary"] = summarize_durations(
                all_durations
            )
            run_means = [
                item["render_summary"]["mean_render_us"]
                for item in report["runs"]
            ]
            report["run_mean_summary"] = {
                "minimum_us": min(run_means),
                "mean_us": statistics.fmean(run_means),
                "population_stdev_us": statistics.pstdev(run_means),
                "maximum_us": max(run_means),
                "span_us": max(run_means) - min(run_means),
            }
            if expected_streams:
                report["aggregate_stream_summaries"] = [
                    {
                        "bmid": bmid,
                        "expected_count_per_run": expected_streams[bmid],
                        **summarize_durations(all_stream_durations[bmid]),
                    }
                    for bmid in sorted(expected_streams)
                ]

            # Refuse to publish a report assembled from moving inputs.
            verify_artifacts_unchanged(
                guarded_artifacts,
                sdcard=sdcard,
                initial_sdcard_manifest=artifact_record["sdcard"],
            )

            publish_report_and_logs(
                output=output,
                staged_logs=staged_logs,
                report=report,
            )
        aggregate = report["aggregate_render_summary"]
        print(
            f"PASS aggregate: {aggregate['count']} records, "
            f"{aggregate['mean_render_us']:.1f} us mean "
            f"({aggregate['equivalent_mean_fps']:.2f} FPS)"
        )
        if expected_streams:
            stream_text = "; ".join(
                f"bmid {item['bmid']}: {item['count']} at "
                f"{item['mean_render_us']:.1f} us"
                for item in report["aggregate_stream_summaries"]
            )
            print(f"  aggregate streams: {stream_text}")
        print(f"Wrote {output}")
        return 0
    except (OSError, PingoToolError, subprocess.SubprocessError) as error:
        print_error(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
