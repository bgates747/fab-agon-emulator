# Pingo development

The `pingo` branch keeps Fab's emulator integration separate from both the
official checkout and the Pingo firmware implementation.

The repositories have distinct responsibilities:

- `fab-agon-emulator` loads and runs a native VDP module;
- `agon-vdp` builds the Pingo-enabled VDP module;
- `pingoasm` supplies test programs and their runtime assets.

No generated firmware, sample binary, or private SD-card tree is committed to
this repository.

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
