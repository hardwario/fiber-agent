# firmware

**Stance: copy-and-edit (decision #3)**

Zephyr firmware for the two microcontrollers on the Hub board:

| Directory | MCU | What it does |
| --- | --- | --- |
| [`fiber-northbridge/`](fiber-northbridge/README.md) | STM32WL5MOC (sub-GHz LoRa) | TOWER P2P gateway; HDLC link to the CM4 on `ttyAMA3` |
| [`fiber-southbridge/`](fiber-southbridge/README.md) | STM32C031C6 | GPIO, ADC and LED controller; line protocol on the CM4's UART, shell on RTT |

Both are upstream Zephyr (not NCS), built with Zephyr SDK 1.0.0 and flashed
with a J-Link. Flashing by hand is covered in [`FLASHING.md`](FLASHING.md).

## Provenance

Imported as files, without history, from
proximos-v1/firmware@80805499a03b9621d994719092094305c514578e. That is the tip
of `hynek/northbridge-p2p-protocol` as of 2026-09-28. It contains v1 `main`
(43f8379) plus the unmerged P2P gateway work, which was never reviewed in v1.
The v1 repo still holds the history. To reproduce the import:

```bash
V1=<path to>/proximos-v1/firmware
git -C "$V1" archive 80805499a03b9621d994719092094305c514578e \
    fiber-northbridge fiber-southbridge FLASHING.md .gitignore \
  | tar -x -C firmware --exclude='fiber-southbridge/.vscode'
```

### What was deliberately not copied

| Not copied | Why |
| --- | --- |
| `.gitlab-ci.yml`, `.gitlab/ci/` | moved into the root `.gitlab-ci.yml` as the `firmware:*` jobs |
| `.vscode/` (root and southbridge) | editor state, gitignored in this repo; `launch.json` pointed at a long-gone CubeIDE build |
| `tools/mesh_*.py` and their `.pyc` | bench scripts for the pre-Zephyr northbridge UART protocol |
| `README.md` | the unedited GitLab project template |

Edits made during the import are confined to docs plus one lint fix. The build
instructions in both bridge READMEs and in `FLASHING.md` now use the `firmware/`
workspace, and absolute `/home/...` paths were removed. The lint fix is in
`fiber-southbridge/bridge/console.sh`, which now reads the reset vector with
`read -a`; the old unquoted array assignment failed shellcheck. The port's
real changes, such as southbridge versioning and the northbridge protocol
extension, belong to `plan/firmware/plan.md`, which is still pending (see the
design queue in `plan/README.md`).

## Workspace

`firmware/` is the west workspace topdir. `zephyr/`, `modules/` and `.west/`
are fetched here and gitignored. Both bridges have their own `west.yml`, and
the two pin the same Zephyr revision (`main` @ 2026-03-23, v4.3.99) and the
same module allowlist, so one workspace serves both and CI can share a single
cache. **If you change the pin, change it in both files.**

One-time setup, from `firmware/`:

```bash
west init -l fiber-northbridge
west update --narrow
west zephyr-export
pip install -r zephyr/scripts/requirements.txt   # in a venv of your choice
```

To switch between the bridges, change the manifest path. There is no need to
re-initialise:

```bash
west config manifest.path fiber-southbridge && west update
```

## Build and test

From `firmware/`. The commands are the same ones the CI jobs run:

```bash
# northbridge
west build -b stm32wl5moc       -d build-north -p always fiber-northbridge/app
west twister -T fiber-northbridge/tests -p native_sim

# southbridge
west build -b fiber_southbridge -d build-south -p always fiber-southbridge/app
west twister -T fiber-southbridge/tests -p native_sim
```

The ztest suites are pure logic and run on `native_sim`. The two-probe
northbridge harness in `fiber-northbridge/tests/hil_p2p/` needs real hardware.
It is Python with no `testcase.yaml`, so twister skips it.

## CI

The root `.gitlab-ci.yml` runs four jobs whenever `firmware/` or the CI file
changes. They use the `zephyrprojectrtos/ci:v0.29.0` image (SDK 1.0.0, which
must match the Zephyr pin):

- `firmware:north-tests` and `firmware:south-tests` run twister on native_sim and publish a JUnit report.
- `firmware:north-build` and `firmware:south-build` build the app. `zephyr.{elf,hex,bin}` are kept as job artifacts.

Firmware images go out only as job artifacts. They are never committed (see
the binary rule in `CLAUDE.md`) and are not yet published to the package
registry.
