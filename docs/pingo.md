# Pingo development

The `pingo` branch keeps Fab's emulator integration separate from both the
official checkout and the Pingo firmware implementation.

The repositories have distinct responsibilities:

- `fab-agon-emulator` loads and runs a native VDP module;
- `agon-vdp` builds the Pingo-enabled VDP module;
- `pingoasm` supplies test programs and their runtime assets.

No generated firmware, sample binary, or private SD-card tree is committed to
this repository.

## Recorded integration baseline

The durable workflow was established on 2026-07-26:

```text
Fab upstream base
  98bbb392b75b196171cc620b60839220e5ce53ed

Pingo integration
  branch pingo
  commit 1b582ed38e57541ca902319e42fe28677800316b

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
  agon-vdp/
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

Check out `pingo-v2.16-userspace` in `agon-vdp` and build its native module:

```sh
make -C ../agon-vdp/userspace FAB_ROOT="$PWD"
make -C ../agon-vdp/userspace FAB_ROOT="$PWD" smoke
```

The expected module is:

```text
../agon-vdp/video/build/userspace/vdp_pingo.so
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

The launcher creates a disposable SD card, copies the selected binary and
the non-source files beside it, writes an `autoexec.txt` that loads and runs
the sample, and removes the SD card when Fab exits.

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

## Intended helper layer

This repository is the orchestration layer for the Pingo development loop:

```text
edit agon-vdp
  -> build and smoke-test vdp_pingo.so
  -> stage a pingoasm fixture
  -> restart Fab with the explicit module
  -> capture or review the result
```

The next helpers should remain thin wrappers around commands owned by the
component repositories:

- `build-pingo-vdp`: build the module, run its ABI smoke test, and report its
  identity;
- `test-pingo`: execute the accepted fixture set and compare deterministic
  captures;
- `pingo-status`: report commits, dirty trees, submodule pins, and artifact
  hashes across the three checkouts;
- `update-upstream`: report or apply emulator-upstream updates without
  silently changing `agon-vdp` or `pingoasm`;
- a `--rebuild` mode for `run-pingo` to provide the everyday
  edit-build-run-test command.

The VDP library is loaded for the lifetime of the Fab process. Updating it
therefore requires a rebuild and emulator restart; this is not an in-process
hot reload.
