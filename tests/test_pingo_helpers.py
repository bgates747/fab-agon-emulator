from __future__ import annotations

import hashlib
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SCRIPTS = Path(__file__).resolve().parent.parent / "scripts"
sys.path.insert(0, str(SCRIPTS))

from pingo_helpers import (  # noqa: E402
    ProjectPaths,
    artifact_status,
    require_file,
    sha256_file,
)


class ProjectPathsTests(unittest.TestCase):
    def test_explicit_paths_take_precedence_over_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            explicit_fab = root / "explicit-fab"
            explicit_vdp = root / "explicit-vdp"
            explicit_asm = root / "explicit-asm"
            with patch.dict(
                os.environ,
                {
                    "FAB_ROOT": str(root / "environment-fab"),
                    "PINGO_VDP_ROOT": str(root / "environment-vdp"),
                    "PINGOASM_ROOT": str(root / "environment-asm"),
                },
            ):
                paths = ProjectPaths.discover(
                    fab_root=explicit_fab,
                    vdp_root=explicit_vdp,
                    pingoasm_root=explicit_asm,
                )
            self.assertEqual(paths.fab_root, explicit_fab.resolve())
            self.assertEqual(paths.vdp_root, explicit_vdp.resolve())
            self.assertEqual(paths.pingoasm_root, explicit_asm.resolve())

    def test_sibling_defaults_follow_fab_root(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            projects = Path(temporary)
            fab = projects / "fab-agon-emulator"
            with patch.dict(os.environ, {}, clear=True):
                paths = ProjectPaths.discover(fab_root=fab)
            self.assertEqual(
                paths.vdp_root,
                (projects / "agon-vdp-pingo-v216-userspace").resolve(),
            )
            self.assertEqual(
                paths.pingoasm_root, (projects / "pingoasm").resolve()
            )


class ArtifactTests(unittest.TestCase):
    def test_hash_and_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "artifact.bin"
            artifact.write_bytes(b"Pingo\n")
            expected = hashlib.sha256(b"Pingo\n").hexdigest()
            self.assertEqual(sha256_file(artifact), expected)
            self.assertEqual(
                artifact_status(artifact),
                {
                    "path": str(artifact),
                    "available": True,
                    "bytes": 6,
                    "sha256": expected,
                },
            )
            self.assertEqual(require_file(artifact, "artifact"), artifact)

    def test_missing_artifact_status(self) -> None:
        missing = Path("/path/that/does/not/exist")
        self.assertEqual(
            artifact_status(missing),
            {"path": str(missing), "available": False},
        )


if __name__ == "__main__":
    unittest.main()
