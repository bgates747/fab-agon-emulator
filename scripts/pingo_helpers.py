#!/usr/bin/env python3
"""Shared, dependency-free helpers for the Pingo integration scripts."""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Sequence


class PingoToolError(RuntimeError):
    """An expected configuration or subprocess failure."""


@dataclass(frozen=True)
class ProjectPaths:
    fab_root: Path
    vdp_root: Path
    pingoasm_root: Path

    @classmethod
    def discover(
        cls,
        *,
        fab_root: str | os.PathLike[str] | None = None,
        vdp_root: str | os.PathLike[str] | None = None,
        pingoasm_root: str | os.PathLike[str] | None = None,
    ) -> "ProjectPaths":
        default_fab = Path(__file__).resolve().parent.parent
        resolved_fab = _resolve_path(
            fab_root or os.environ.get("FAB_ROOT") or default_fab
        )
        projects_root = resolved_fab.parent
        return cls(
            fab_root=resolved_fab,
            vdp_root=_resolve_path(
                vdp_root
                or os.environ.get("PINGO_VDP_ROOT")
                or projects_root / "agon-vdp-pingo-v216-userspace"
            ),
            pingoasm_root=_resolve_path(
                pingoasm_root
                or os.environ.get("PINGOASM_ROOT")
                or projects_root / "pingoasm"
            ),
        )

    @property
    def fab_binary(self) -> Path:
        override = os.environ.get("FAB_EMULATOR_BIN")
        return _resolve_path(
            override or self.fab_root / "target/release/fab-agon-emulator"
        )

    @property
    def vdp_shared_object(self) -> Path:
        override = os.environ.get("PINGO_VDP_SO")
        return _resolve_path(
            override or self.vdp_root / "video/build/userspace/vdp_pingo.so"
        )

    @property
    def mos_binary(self) -> Path:
        return self.fab_root / "firmware/mos_console8.bin"


def _resolve_path(value: str | os.PathLike[str]) -> Path:
    return Path(value).expanduser().resolve()


def run(
    command: Sequence[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    capture: bool = False,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    printable = [str(part) for part in command]
    try:
        return subprocess.run(
            printable,
            cwd=cwd,
            env=dict(env) if env is not None else None,
            check=check,
            text=True,
            capture_output=capture,
        )
    except FileNotFoundError as error:
        raise PingoToolError(f"Command not found: {printable[0]}") from error
    except subprocess.CalledProcessError as error:
        if capture:
            details = (error.stderr or error.stdout or "").strip()
            suffix = f": {details}" if details else ""
        else:
            suffix = ""
        raise PingoToolError(
            f"Command failed with exit status {error.returncode}: "
            f"{' '.join(printable)}{suffix}"
        ) from error


def require_file(path: Path, description: str) -> Path:
    if not path.is_file():
        raise PingoToolError(f"{description} not found: {path}")
    return path


def require_directory(path: Path, description: str) -> Path:
    if not path.is_dir():
        raise PingoToolError(f"{description} not found: {path}")
    return path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_output(root: Path, *arguments: str) -> str:
    result = run(
        ["git", *arguments],
        cwd=root,
        capture=True,
    )
    return result.stdout.strip()


def git_repository_status(root: Path) -> dict[str, Any]:
    if not (root / ".git").exists():
        return {
            "path": str(root),
            "available": False,
            "error": "not a Git checkout",
        }

    branch = git_output(root, "branch", "--show-current")
    upstream_result = run(
        ["git", "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        cwd=root,
        capture=True,
        check=False,
    )
    upstream = (
        upstream_result.stdout.strip() if upstream_result.returncode == 0 else None
    )
    dirty_lines = git_output(root, "status", "--short").splitlines()
    result: dict[str, Any] = {
        "path": str(root),
        "available": True,
        "branch": branch or "(detached)",
        "head": git_output(root, "rev-parse", "HEAD"),
        "dirty": bool(dirty_lines),
        "changes": dirty_lines,
        "upstream": upstream,
    }
    if upstream:
        counts = git_output(
            root, "rev-list", "--left-right", "--count", f"HEAD...{upstream}"
        ).split()
        result["ahead"] = int(counts[0])
        result["behind"] = int(counts[1])
    return result


def artifact_status(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {"path": str(path), "available": False}
    stat = path.stat()
    return {
        "path": str(path),
        "available": True,
        "bytes": stat.st_size,
        "sha256": sha256_file(path),
    }


def add_path_arguments(parser: Any) -> None:
    parser.add_argument(
        "--fab-root",
        help="Fab checkout (default: this checkout or FAB_ROOT)",
    )
    parser.add_argument(
        "--vdp-root",
        help=(
            "Pingo agon-vdp userspace worktree "
            "(default: PINGO_VDP_ROOT or permanent sibling)"
        ),
    )
    parser.add_argument(
        "--pingoasm-root",
        help="pingoasm checkout (default: PINGOASM_ROOT or sibling)",
    )


def paths_from_arguments(arguments: Any) -> ProjectPaths:
    return ProjectPaths.discover(
        fab_root=arguments.fab_root,
        vdp_root=arguments.vdp_root,
        pingoasm_root=arguments.pingoasm_root,
    )


def print_error(error: Exception) -> None:
    print(f"error: {error}", file=sys.stderr)
