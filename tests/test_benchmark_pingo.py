from __future__ import annotations

import argparse
import importlib.util
import io
import math
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = ROOT / "scripts"
sys.path.insert(0, str(SCRIPTS))

spec = importlib.util.spec_from_file_location(
    "benchmark_pingo", SCRIPTS / "benchmark-pingo.py"
)
assert spec is not None and spec.loader is not None
benchmark_pingo = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark_pingo)

from pingo_helpers import PingoToolError  # noqa: E402


class ExpectedStreamTests(unittest.TestCase):
    def test_full_chain_stream_arguments_are_repeatable(self) -> None:
        argv = [
            "benchmark-pingo.py",
            "--emulator",
            "/fab",
            "--vdp",
            "/vdp.so",
            "--sdcard",
            "/sd",
            "--expected-stream",
            "1257:580",
            "--expected-stream",
            "1410:867",
            "--repeats",
            "1",
            "--output",
            "/result.json",
        ]
        with mock.patch.object(sys, "argv", argv):
            arguments = benchmark_pingo.parse_arguments()
        self.assertIsNone(arguments.expected_count)
        self.assertEqual(
            benchmark_pingo.expected_stream_map(
                arguments.expected_stream
            ),
            {1257: 580, 1410: 867},
        )

    def test_stream_argument_requires_bmid_colon_count(self) -> None:
        for value in ("1257", "1257:", ":580", "x:580", "1:2:3"):
            with self.subTest(value=value):
                with self.assertRaises(argparse.ArgumentTypeError):
                    benchmark_pingo.expected_stream_argument(value)

    def test_stream_argument_validates_ranges(self) -> None:
        for value in ("65536:1", "1257:0"):
            with self.subTest(value=value):
                with self.assertRaises(argparse.ArgumentTypeError):
                    benchmark_pingo.expected_stream_argument(value)

    def test_duplicate_bitmap_stream_is_rejected(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "more than once"):
            benchmark_pingo.expected_stream_map(
                [(1257, 580), (1257, 1)]
            )


class RecordParsingTests(unittest.TestCase):
    def test_ordinary_record(self) -> None:
        self.assertEqual(
            benchmark_pingo.parse_render_record(
                "PINGO_RENDER seq=4 bmid=1410 render_us=638"
            ),
            {
                "seq": 4,
                "bmid": 1410,
                "render_us": 638,
                "additional_fields": {},
                "raw": (
                    "PINGO_RENDER seq=4 bmid=1410 render_us=638"
                ),
            },
        )

    def test_diagnostic_tail_is_preserved(self) -> None:
        sample = benchmark_pingo.parse_render_record(
            "prefix PINGO_RENDER seq=0 bmid=1410 render_us=700 "
            "d=2 ras=512 pt=123456"
        )
        assert sample is not None
        self.assertEqual(
            sample["additional_fields"],
            {"d": 2, "ras": 512, "pt": 123456},
        )

    def test_duplicate_field_is_rejected(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "duplicate"):
            benchmark_pingo.parse_render_record(
                "PINGO_RENDER seq=0 bmid=1 render_us=2 seq=3"
            )

    def test_missing_field_is_rejected(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "missing bmid"):
            benchmark_pingo.parse_render_record(
                "PINGO_RENDER seq=0 render_us=2"
            )


class SequenceTests(unittest.TestCase):
    def test_contiguous_sequence_and_allowed_bitmap(self) -> None:
        samples: list[dict[str, object]] = []
        for sequence in range(3):
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": sequence,
                    "bmid": 1410,
                    "render_us": 500,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=3,
                allowed_bmids={1410},
            )
        self.assertEqual(len(samples), 3)

    def test_duplicate_sequence_is_rejected(self) -> None:
        samples = [
            {
                "seq": 0,
                "bmid": 1410,
                "render_us": 500,
                "additional_fields": {},
                "raw": "",
            }
        ]
        with self.assertRaisesRegex(PingoToolError, "duplicate or reset"):
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": 0,
                    "bmid": 1410,
                    "render_us": 500,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=3,
                allowed_bmids=set(),
            )

    def test_gap_is_rejected(self) -> None:
        samples = [
            {
                "seq": 0,
                "bmid": 1410,
                "render_us": 500,
                "additional_fields": {},
                "raw": "",
            }
        ]
        with self.assertRaisesRegex(PingoToolError, "missing record"):
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": 2,
                    "bmid": 1410,
                    "render_us": 500,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=3,
                allowed_bmids=set(),
            )

    def test_unexpected_bitmap_is_rejected(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "unexpected bitmap"):
            benchmark_pingo.add_record(
                [],
                {
                    "seq": 0,
                    "bmid": 1409,
                    "render_us": 500,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=1,
                allowed_bmids={1410},
            )

    def test_extra_record_is_rejected(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "more than"):
            benchmark_pingo.add_record(
                [
                    {
                        "seq": 0,
                        "bmid": 1410,
                        "render_us": 500,
                        "additional_fields": {},
                        "raw": "",
                    }
                ],
                {
                    "seq": 1,
                    "bmid": 1410,
                    "render_us": 500,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=1,
                allowed_bmids={1410},
            )

    def test_interleaved_streams_have_independent_sequences(self) -> None:
        samples: list[dict[str, object]] = []
        records = (
            (1257, 0, 500),
            (1410, 0, 700),
            (1257, 1, 501),
            (1410, 1, 701),
            (1410, 2, 702),
        )
        streams = {1257: 2, 1410: 3}
        positions = {1257: 0, 1410: 0}
        for bmid, sequence, duration in records:
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": sequence,
                    "bmid": bmid,
                    "render_us": duration,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=5,
                allowed_bmids=set(),
                expected_streams=streams,
                stream_positions=positions,
            )
        self.assertEqual(positions, streams)
        benchmark_pingo.validate_record_counts(
            samples,
            expected_count=5,
            expected_streams=streams,
        )

    def test_stream_gap_is_rejected(self) -> None:
        samples = [
            {
                "seq": 0,
                "bmid": 1257,
                "render_us": 500,
                "additional_fields": {},
                "raw": "",
            },
            {
                "seq": 0,
                "bmid": 1410,
                "render_us": 700,
                "additional_fields": {},
                "raw": "",
            },
        ]
        with self.assertRaisesRegex(
            PingoToolError, "bitmap stream 1257.*missing record"
        ):
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": 2,
                    "bmid": 1257,
                    "render_us": 501,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=4,
                allowed_bmids=set(),
                expected_streams={1257: 2, 1410: 2},
            )

    def test_stream_reset_is_rejected(self) -> None:
        samples = [
            {
                "seq": 0,
                "bmid": 1257,
                "render_us": 500,
                "additional_fields": {},
                "raw": "",
            }
        ]
        with self.assertRaisesRegex(
            PingoToolError, "bitmap stream 1257.*duplicate or reset"
        ):
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": 0,
                    "bmid": 1257,
                    "render_us": 501,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=2,
                allowed_bmids=set(),
                expected_streams={1257: 2},
            )

    def test_undeclared_stream_is_rejected(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "unexpected bitmap ID"):
            benchmark_pingo.add_record(
                [],
                {
                    "seq": 0,
                    "bmid": 1258,
                    "render_us": 500,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=2,
                allowed_bmids=set(),
                expected_streams={1257: 2},
            )

    def test_extra_stream_record_is_rejected(self) -> None:
        samples = [
            {
                "seq": 0,
                "bmid": 1257,
                "render_us": 500,
                "additional_fields": {},
                "raw": "",
            }
        ]
        with self.assertRaisesRegex(
            PingoToolError, "more than.*bitmap stream 1257"
        ):
            benchmark_pingo.add_record(
                samples,
                {
                    "seq": 1,
                    "bmid": 1257,
                    "render_us": 501,
                    "additional_fields": {},
                    "raw": "",
                },
                expected_count=1,
                allowed_bmids=set(),
                expected_streams={1257: 1},
            )

    def test_exact_stream_counts_are_required(self) -> None:
        with self.assertRaisesRegex(PingoToolError, "stream record count"):
            benchmark_pingo.validate_record_counts(
                [
                    {
                        "seq": 0,
                        "bmid": 1257,
                        "render_us": 500,
                        "additional_fields": {},
                        "raw": "",
                    }
                ],
                expected_count=1,
                expected_streams={1257: 1, 1410: 1},
            )


class RemainderTests(unittest.TestCase):
    def test_every_complete_record_in_final_pipe_data_is_parsed(self) -> None:
        class FinishedProcess:
            stdout = io.BytesIO(
                b"PINGO_RENDER seq=1 bmid=1410 render_us=501\n"
            )

        samples = [
            {
                "seq": 0,
                "bmid": 1410,
                "render_us": 500,
                "additional_fields": {},
                "raw": "",
            }
        ]
        benchmark_pingo.drain_remainder(
            process=FinishedProcess(),
            pending=bytearray(),
            raw_log=io.BytesIO(),
            samples=samples,
            expected_count=2,
            allowed_bmids={1410},
        )
        self.assertEqual([item["seq"] for item in samples], [0, 1])


class SummaryTests(unittest.TestCase):
    def test_summary_uses_nearest_rank_p95(self) -> None:
        summary = benchmark_pingo.summarize_durations(
            list(range(1, 101))
        )
        self.assertEqual(summary["count"], 100)
        self.assertEqual(summary["median_render_us"], 50.5)
        self.assertEqual(summary["p95_render_us"], 95)
        self.assertEqual(summary["total_render_us"], 5050)

    def test_nonfinite_duration_option_is_rejected(self) -> None:
        for value in (math.nan, math.inf, -math.inf):
            with self.subTest(value=value):
                with self.assertRaisesRegex(PingoToolError, "finite"):
                    benchmark_pingo.validate_positive(value, "--timeout")

    def test_stream_summaries_are_separate(self) -> None:
        summaries = benchmark_pingo.summarize_streams(
            [
                {"bmid": 1257, "render_us": 100},
                {"bmid": 1410, "render_us": 900},
                {"bmid": 1257, "render_us": 300},
            ],
            {1257: 2, 1410: 1},
        )
        self.assertEqual(
            [(item["bmid"], item["count"]) for item in summaries],
            [(1257, 2), (1410, 1)],
        )
        self.assertEqual(summaries[0]["mean_render_us"], 200)
        self.assertEqual(summaries[1]["mean_render_us"], 900)


class CommandTests(unittest.TestCase):
    def test_command_pins_headless_benchmark_settings(self) -> None:
        command = benchmark_pingo.debugger_command(
            emulator=Path("/fab"),
            vdp=Path("/vdp.so"),
            sdcard=Path("/sd"),
            mos=Path("/mos.bin"),
        )
        self.assertIn("--debugger", command)
        self.assertIn("--renderer", command)
        self.assertIn("sw", command)
        self.assertIn("-z", command)
        self.assertIn("-u", command)
        self.assertNotIn("--unlimited-cpu", command)

    def test_environment_suppresses_video_and_audio(self) -> None:
        environment = benchmark_pingo.benchmark_environment()
        self.assertEqual(environment["SDL_VIDEODRIVER"], "dummy")
        self.assertEqual(environment["SDL_AUDIODRIVER"], "dummy")
        self.assertEqual(environment["LC_ALL"], "C")


class SdcardManifestTests(unittest.TestCase):
    def test_selected_runtime_files_are_hashed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            fixture = sdcard / "bench/fixture/tgt"
            fixture.mkdir(parents=True)
            (sdcard / "autoexec.txt").write_text(
                "SET KEYBOARD 1\n"
                "cd /bench/fixture/tgt\n"
                "load benchmark.bin\n"
                "run\n",
                encoding="utf-8",
            )
            (fixture / "benchmark.bin").write_bytes(b"program")
            (fixture / "texture.rgba2").write_bytes(b"texture")
            manifest = benchmark_pingo.sdcard_manifest(sdcard)
            self.assertEqual(
                manifest["load_targets"][0]["logical_path"],
                "/bench/fixture/tgt/benchmark.bin",
            )
            names = {
                Path(item["path"]).name
                for item in manifest["selected_runtime_files"]
            }
            self.assertEqual(names, {"benchmark.bin", "texture.rgba2"})

    def test_exactly_one_load_is_required(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "autoexec.txt").write_text(
                "SET KEYBOARD 1\nrun\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "one active LOAD"):
                benchmark_pingo.sdcard_manifest(sdcard)

    def test_exactly_one_run_is_required(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "benchmark.bin").write_bytes(b"program")
            (sdcard / "autoexec.txt").write_text(
                "load benchmark.bin\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "one active RUN"):
                benchmark_pingo.sdcard_manifest(sdcard)

    def test_run_must_follow_load(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "benchmark.bin").write_bytes(b"program")
            (sdcard / "autoexec.txt").write_text(
                "run\nload benchmark.bin\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "must follow"):
                benchmark_pingo.sdcard_manifest(sdcard)

    def test_selected_program_must_exist(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "autoexec.txt").write_text(
                "load missing.bin\nrun\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "not found"):
                benchmark_pingo.sdcard_manifest(sdcard)

    def test_load_may_not_escape_sdcard(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "autoexec.txt").write_text(
                "cd ../../../../tmp\nload outside.bin\nrun\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "escapes"):
                benchmark_pingo.sdcard_manifest(sdcard)

    def test_project_local_symlink_deployment_is_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sdcard = root / "sdcard"
            fixture = root / "project/fixture/tgt"
            sdcard.mkdir()
            fixture.mkdir(parents=True)
            (fixture / "benchmark.bin").write_bytes(b"program")
            (sdcard / "fixture").symlink_to(fixture.parent)
            (sdcard / "autoexec.txt").write_text(
                "cd /fixture/tgt\nload benchmark.bin\nrun\n",
                encoding="utf-8",
            )
            manifest = benchmark_pingo.sdcard_manifest(sdcard)
            self.assertEqual(len(manifest["load_targets"]), 1)
            self.assertEqual(
                Path(manifest["load_targets"][0]["host_path"]),
                (fixture / "benchmark.bin").resolve(),
            )

    def test_chained_pairs_hash_every_runtime_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            first = sdcard / "bench/first"
            second = sdcard / "bench/second"
            first.mkdir(parents=True)
            second.mkdir(parents=True)
            (first / "benchmark.bin").write_bytes(b"first program")
            (first / "first.rgba2").write_bytes(b"first texture")
            (second / "benchmark.bin").write_bytes(b"second program")
            (second / "second.rgba2").write_bytes(b"second texture")
            (sdcard / "autoexec.txt").write_text(
                "SET KEYBOARD 1\n"
                "cd /bench/first\n"
                "load benchmark.bin\n"
                "run\n"
                "cd /bench/second\n"
                "load benchmark.bin\n"
                "run\n",
                encoding="utf-8",
            )
            manifest = benchmark_pingo.sdcard_manifest(
                sdcard, allow_chained=True
            )
            self.assertEqual(manifest["load_run_mode"], "chained")
            self.assertEqual(len(manifest["load_targets"]), 2)
            directories = {
                Path(item["host_directory"])
                for item in manifest["selected_runtime_files"]
            }
            self.assertEqual(directories, {first, second})
            names = {
                Path(item["path"]).name
                for item in manifest["selected_runtime_files"]
            }
            self.assertEqual(
                names,
                {
                    "benchmark.bin",
                    "first.rgba2",
                    "second.rgba2",
                },
            )

    def test_single_fixture_mode_still_rejects_a_chain(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "first.bin").write_bytes(b"first")
            (sdcard / "second.bin").write_bytes(b"second")
            (sdcard / "autoexec.txt").write_text(
                "load first.bin\nrun\nload second.bin\nrun\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                PingoToolError, "single-fixture mode"
            ):
                benchmark_pingo.sdcard_manifest(sdcard)

    def test_chained_load_must_run_before_the_next_load(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "first.bin").write_bytes(b"first")
            (sdcard / "second.bin").write_bytes(b"second")
            (sdcard / "autoexec.txt").write_text(
                "load first.bin\nload second.bin\nrun\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "before LOAD"):
                benchmark_pingo.sdcard_manifest(
                    sdcard, allow_chained=True
                )

    def test_chained_run_without_load_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sdcard = Path(temporary)
            (sdcard / "autoexec.txt").write_text(
                "run\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(PingoToolError, "must follow"):
                benchmark_pingo.sdcard_manifest(
                    sdcard, allow_chained=True
                )


class PublicationTests(unittest.TestCase):
    def test_force_preflight_does_not_modify_existing_results(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result.json"
            log = benchmark_pingo.planned_log_path(output, 1)
            output.write_text("old report", encoding="utf-8")
            log.write_text("old log", encoding="utf-8")
            benchmark_pingo.ensure_outputs_available(
                output, 1, force=True
            )
            self.assertEqual(output.read_text(encoding="utf-8"), "old report")
            self.assertEqual(log.read_text(encoding="utf-8"), "old log")

    def test_publish_replaces_logs_before_atomic_report(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "result.json"
            old_log = benchmark_pingo.planned_log_path(output, 1)
            output.write_text("old report", encoding="utf-8")
            old_log.write_text("old log", encoding="utf-8")
            staged = root / "staged.log"
            staged.write_text("new log", encoding="utf-8")
            report = {"runs": [{"emulator_log": {}}]}
            benchmark_pingo.publish_report_and_logs(
                output=output,
                staged_logs=[staged],
                report=report,
            )
            self.assertFalse(staged.exists())
            self.assertEqual(
                old_log.read_text(encoding="utf-8"), "new log"
            )
            self.assertEqual(
                report["runs"][0]["emulator_log"]["sha256"],
                benchmark_pingo.sha256_file(old_log),
            )
            self.assertIn('"runs"', output.read_text(encoding="utf-8"))


class RepositoryTests(unittest.TestCase):
    def test_non_repository_is_not_mistaken_for_parent_dot_git(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            self.assertIsNone(
                benchmark_pingo.find_repository(Path(temporary))
            )


if __name__ == "__main__":
    unittest.main()
