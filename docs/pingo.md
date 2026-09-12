# Pingo development

The `pingo` branch keeps Fab's emulator integration separate from both the
official checkout and the Pingo firmware implementation.

The repositories have distinct responsibilities:

- `fab-agon-emulator` loads and runs a native VDP module;
- `agon-vdp` builds the Pingo-enabled VDP module;
- `pingoasm` owns test programs, runtime assets, Blender sources, and the
  rendering/asset-pipeline scripts that produce them.

No generated firmware, sample binary, or private SD-card tree is committed to
this repository.

## Quick start

From anywhere on this development machine, rebuild the native Pingo VDP,
smoke-test it, and launch the default `moveobj/tri` visual fixture:

```sh
~/Agon/mystuff/fab-agon-emulator/scripts/run-pingo --rebuild
```

## Recorded integration baseline

The durable workflow was established on 2026-07-26:

```text
Fab upstream base
  98bbb392b75b196171cc620b60839220e5ce53ed

Pingo integration
  branch pingo
  launcher 1b582ed38e57541ca902319e42fe28677800316b
  helpers  654ded9
  paths    73452ed

Pingo userspace VDP
  agon-vdp branch pingo-v2.16-userspace
  adapter d0bb3e13c876a9465c5ba19d8d53b97424eca5fa
  capture c490406ee721c5a53f04c069ee10302a855b7564
```

The official checkout at `~/Agon/fab-agon-emulator` remains the upstream
reference. This checkout at `~/Agon/mystuff/fab-agon-emulator` is the owned
integration layer. Its `origin` is `bgates747/fab-agon-emulator`; its
`upstream` is `tomm/fab-agon-emulator`.

## One-time setup

The expected sibling layout is:

```text
~/Agon/mystuff/
  agon-vdp/                         historical/reconstruction checkout
  agon-vdp-pingo-v216-userspace/    native Pingo VDP worktree
  fab-agon-emulator/
  pingoasm/
```

Initialize Fab's submodules and build the release executable:

```sh
git submodule update --init --recursive
cargo build --release
```

If SDL3 is installed under `~/.local` rather than a system library directory,
expose it while linking:

```sh
LIBRARY_PATH="$HOME/.local/lib${LIBRARY_PATH:+:$LIBRARY_PATH}" \
  cargo build --release
```

The Pingo launcher automatically adds an existing `~/.local/lib` SDL3
installation to the runtime library search path.

### Blender tooling

Pingo asset work uses the native Blender package from the Pop!_OS/Ubuntu
repository, not Flatpak:

```sh
sudo apt-get install blender
```

The validated installation is Blender 4.0.2. Its ordinary headless Python
interface is directly available on `PATH`:

```sh
blender --background --python script.py
```

A background Python smoke test successfully rendered a PNG through Blender's
surfaceless EGL fallback. PulseAudio or initial EGL diagnostics may appear in
a restricted headless session without indicating render failure.

Blender and asset-conversion scripts live in `pingoasm`, not this emulator
repository. See the
[pingoasm rendering-pipeline guide](https://github.com/bgates747/pingoasm/blob/main/docs/rendering-pipeline.md).

The registered `pingo-v2.16-userspace` worktree lives permanently beside
this checkout. Build its native module directly or use the helpers below:

```sh
make -C ../agon-vdp-pingo-v216-userspace/userspace FAB_ROOT="$PWD"
make -C ../agon-vdp-pingo-v216-userspace/userspace FAB_ROOT="$PWD" smoke
```

The expected module is:

```text
../agon-vdp-pingo-v216-userspace/video/build/userspace/vdp_pingo.so
```

## Run a sample

From this repository:

```sh
scripts/run-pingo
```

That launches `moveobj/tri`, the current visual near-camera regression
fixture. To launch another self-contained fixture:

```sh
scripts/run-pingo moveair/jet
```

The launcher resolves fixtures such as `moveobj/tri` beneath
`pingoasm/apps/moveobj/tgt`, creates a disposable SD card, copies the selected
binary and its RGBA2 runtime assets, writes an `autoexec.txt` that loads and
runs the sample, and removes the SD card when Fab exits.

The paths can be overridden without editing the script:

```sh
PINGO_VDP_SO=/absolute/path/vdp_pingo.so \
PINGOASM_ROOT=/absolute/path/pingoasm \
FAB_EMULATOR_BIN=/absolute/path/fab-agon-emulator \
scripts/run-pingo moveobj/tri
```

Set `KEEP_PINGO_SD=1` to retain the generated SD-card directory for
inspection. Additional arguments after the fixture are passed to Fab.

The launcher deliberately starts Fab from the VDP module's directory and
uses absolute paths. If the requested Pingo module cannot load, Fab therefore
cannot silently fall back to the stock module in this checkout.

## Validation record

The pinned submodules were initialized and the fork compiled successfully on
Linux. This host's SDL3 installation is under `~/.local`, so the successful
link command was:

```sh
LIBRARY_PATH="$HOME/.local/lib" cargo build --release
```

The resulting executable had SHA-256:

```text
832f6f8a18e4608f420381124ba33c4b550a034eb3973facdd9e8108fef8264b
```

The launcher was exercised headlessly with the exact capture-enabled Pingo
VDP and `moveobj/tri`. Fab loaded the requested module, initialized bitmap
257 and Pingo control buffer 100, created the textured object, and entered
320x240 rendering. A deliberate external timeout ended the run, and the
temporary SD directory was cleaned up.

The same fixture was then reviewed interactively. The Author reported that it
worked flawlessly and did not show the remembered near-camera distortion.
The matching physical-hardware test had the same visual result. This is a
human visual acceptance result rather than pixel-exact final-scanout
comparison.

## Helper commands

This repository is the orchestration layer for the Pingo development loop:

```text
edit agon-vdp
  -> build and smoke-test vdp_pingo.so
  -> stage a pingoasm fixture
  -> restart Fab with the explicit module
  -> capture or review the result
```

All Python helpers use only the Python 3 standard library. They accept
`--fab-root`, `--vdp-root`, and `--pingoasm-root` where cross-repository paths
are relevant. The matching environment variables are `FAB_ROOT`,
`PINGO_VDP_ROOT`, and `PINGOASM_ROOT`. Without overrides, the VDP helpers use
the permanent sibling worktree
`../agon-vdp-pingo-v216-userspace`.

### Build the native VDP

```sh
scripts/build-pingo-vdp.py
```

This invokes the native Makefile in the userspace worktree, runs its ABI and
empty-render smoke test, and reports the resulting module's path, size, and
SHA-256 identity. Useful options:

```sh
scripts/build-pingo-vdp.py --clean
scripts/build-pingo-vdp.py --no-smoke
scripts/build-pingo-vdp.py --vdp-root /path/to/userspace-worktree
```

`--clean` removes only products owned by the native VDP Makefile.

### Everyday interactive loop

Build, smoke-test, and launch the default triangle fixture:

```sh
scripts/run-pingo --rebuild
```

Select another fixture:

```sh
scripts/run-pingo --rebuild moveair/jet
```

When `PINGO_VDP_SO` overrides the module path, `--rebuild` also requires
`PINGO_VDP_ROOT` so the launcher cannot rebuild one checkout and accidentally
run a module from another.

### Deterministic regressions

Run all accepted fixtures, each in a fresh headless Fab process:

```sh
scripts/test-pingo.py
```

Run only one fixture or rebuild first:

```sh
scripts/test-pingo.py moveobj/tri
scripts/test-pingo.py --rebuild
```

The accepted frame-1 oracles are:

| Fixture | Dimensions | Bytes | SHA-256 |
| --- | ---: | ---: | --- |
| `moveobj/tri` | 320x240 | 76,800 | `f81dd66876ef012a6f1e52bae2821c275f1cf33e9a7e977c193be93bad4b4958` |
| `moveair/jet` | 320x148 | 47,360 | `768f07b8115df6391d9a0a1611adf9e293a96740a4962d049788c15777ecdd5e` |

Use `--keep-captures DIRECTORY` to retain successful `.rgba2`, `.ppm`,
metadata, and emulator logs. Failed runs retain their diagnostic directory
automatically. These hashes validate Pingo's render target, not Fab's final
composited scanout.

### Silent performance benchmarks

`benchmark-pingo.py` runs an instrumented Pingo fixture without opening a
window or playing Fab's boot beep. It starts a fresh emulator process for each
repeat and requires explicit paths so a stock VDP or the wrong simulated SD
card cannot pass accidentally:

```sh
scripts/benchmark-pingo.py \
  --emulator ~/Agon/mystuff/fab-agon-emulator/target/release/fab-agon-emulator \
  --vdp ~/Agon/mystuff/agon-vdp/video/build/userspace/vdp_pingo.so \
  --mos ~/Agon/mystuff/fab-agon-emulator/firmware/mos_console8.bin \
  --sdcard ~/Agon/mystuff/pingoasm/emulators/tv-port-baseline/sdcard \
  --expected-count 289 \
  --expected-bmid 1410 \
  --repeats 3 \
  --artifact ~/Agon/mystuff/pingoasm/benchmarks/orbit-scene/fixtures/earth-party-camera-ellipse-rgba2222/effective-profile.json \
  --output ~/Agon/mystuff/pingoasm/benchmarks/orbit-scene/results/camera-ellipse-emulator.json
```

For a chained `autoexec.txt`, replace `--expected-count` and
`--expected-bmid` with one declaration for each independent bitmap stream.
The current full-chain expectation is:

```sh
scripts/benchmark-pingo.py \
  --emulator ~/Agon/mystuff/fab-agon-emulator/target/release/fab-agon-emulator \
  --vdp ~/Agon/mystuff/agon-vdp/video/build/userspace/vdp_pingo.so \
  --mos ~/Agon/mystuff/fab-agon-emulator/firmware/mos_console8.bin \
  --sdcard ~/Agon/mystuff/pingoasm/emulators/tv-port-baseline/sdcard \
  --expected-stream 1257:580 \
  --expected-stream 1410:867 \
  --repeats 3 \
  --output /tmp/pingo-full-chain-emulator.json
```

The two expectation modes are mutually exclusive:

1. `--expected-count N` retains the original single-fixture contract.
   `autoexec.txt` must contain exactly one active `LOAD`, followed by one
   active `RUN`. The complete record sequence must be `0..N-1`;
   `--expected-bmid` may further constrain accepted bitmap IDs.
2. Repeatable `--expected-stream BMID:COUNT` enables a chained suite.
   `autoexec.txt` may contain multiple `LOAD`/`RUN` pairs, but every `LOAD`
   must receive its `RUN` before the next `LOAD`. Each declared bitmap ID has
   its own sequence beginning at zero and continuing without a gap or reset,
   even when records from the declared streams are interleaved. Undeclared
   bitmap IDs, missing records, excess records, and duplicate stream
   declarations fail the run. The expected total is the sum of all declared
   stream counts—1,447 records for the example.

Every selected binary must exist. Lexical paths may not escape the SD root;
the established project-local symlink from the simulated SD card into
`pingoasm` remains supported. Every selected runtime directory is hashed and
checked again before publication. The benchmark helper never edits
`autoexec.txt`. The single-fixture example assumes the current ellipse
fixture, whose profile declares 289 measured frames and target bitmap 1410.

The native VDP must expose its forced diagnostics to the host. The Pingo
firmware does this in `force_debug_log()` by writing to `stderr` under
`USERSPACE`; its embedded build continues to use `DBGSerial`. Absence of that
hook is a hard missing-record failure, not a silent empty result.

Each `PINGO_RENDER` record must contain decimal `seq`, `bmid`, and
`render_us` fields. It rejects gaps, duplicates, sequence resets, unexpected
bitmap IDs, extra records, subprocess failures, timeouts, and benchmark
inputs that change during the suite.

Fab is pinned to dummy SDL video and audio drivers, the software renderer,
zero-initialized RAM, and unlimited eZ80 execution (`-u`). Do not add
`--verbose`; its output volume perturbs the run and obscures the measurement
records. After the expected records and a short cleanup grace period, the
helper uses Fab's debugger path to pause the eZ80 and request orderly VDP and
emulator shutdown.

The JSON report records the exact command and selected environment, host
identity, repository state, SHA-256 identities for the benchmark harness and
shared helpers, emulator, VDP, MOS, `autoexec.txt`, selected runtime directory,
and optional profile artifacts. It includes every raw sample, per-run and
aggregate statistics, per-stream summaries in chained mode, wall-clock
intervals, and hashes of the adjacent raw emulator logs.

Existing outputs are protected unless `--force` is supplied. Forced reruns
stage every new log without touching the old evidence. Only after all runs and
input-identity checks pass are the logs replaced and the new JSON report
published last. If publication itself is interrupted, the old report is
removed first so stale JSON cannot claim a mixture of old and new logs.

These timings are suitable for relative regression work on the same quiet
host. They are not substitutes for the ESP32 measurements: physical hardware
remains the performance ground truth.

### Benchmark maintenance handoff (2026-09-11)

This checkpoint adds the benchmark CLI, its unit contracts, and the usage
contract above as one change. It does not modify the Rust emulator, VDP source,
firmware, or an installed runtime. The existing `pingo` branch also contains
three previously local deterministic headless run, observation, and manifest
checkpoint commits; publication retains that history without rewriting it.

Validation: `python3 -m unittest discover -s tests -p 'test_*.py'` passes all
43 benchmark/helper tests on Linux. No new real-emulator performance run,
macOS benchmark validation, or physical ESP32 measurement was performed for
this commit. The examples above are explicit fixture selections, not proof
that those mutable paths still contain the matching instrumented deployment.

For the next benchmark agent:

1. Read `scripts/benchmark-pingo.py`, `scripts/pingo_helpers.py`, and
   `tests/test_benchmark_pingo.py` before changing the record/report contract.
2. Use an instrumented VDP that emits `PINGO_RENDER` on host stderr and verify
   the exact fixture's LOAD/RUN chain and expected bitmap counts. This harness
   does not add that instrumentation to ordinary VDP firmware or Pingo 2.
3. Preserve strict sequence/count checks, post-run input identity validation,
   and logs-first/report-last publication. The current runtime manifest hashes
   immediate regular files in each selected program directory; it is not a
   recursive inventory of every asset a program might load. Use `--artifact`
   for additional known inputs.
4. Failed runs publish no report; temporary staged logs are cleaned up by the
   current implementation. Capture the command's diagnostics when investigating
   failures. Existing published results survive failures before publication.
5. Treat same-host timing as regression evidence. Keep native shutdown and
   macOS compatibility validation separate, and retain human validation gates
   for future runtime changes. Do not replace the official Mac runtime with
   this user-owned source fork as part of source migration.

### Report exact state

```sh
scripts/pingo-status.py
scripts/pingo-status.py --json
scripts/pingo-status.py --strict
```

The report includes branch, commit, upstream divergence, dirty files, and
artifact hashes for Fab, `agon-vdp`, `pingoasm`, the emulator executable, the
native Pingo module, and the two accepted client binaries. `--strict` returns
nonzero if a repository is dirty or an expected artifact is missing; ordinary
status reporting remains informative and returns success.

### Inspect or incorporate Fab upstream

Read the locally recorded divergence:

```sh
scripts/update-upstream.py
```

Refresh `upstream/*` remote-tracking refs and report again:

```sh
scripts/update-upstream.py --fetch
```

Explicitly merge `upstream/main` into the current clean branch:

```sh
scripts/update-upstream.py --merge
```

The default command is read-only. `--fetch` changes only remote-tracking
metadata. `--merge` refuses a dirty working tree, performs a local merge, and
does not push. The helper never changes `agon-vdp` or `pingoasm`.

### Test the helpers

```sh
python3 -m unittest discover -s tests -v
```

The VDP library is loaded for the lifetime of the Fab process. Updating it
therefore requires a rebuild and emulator restart; this is not an in-process
hot reload.
