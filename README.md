# lightwatch

Lightwatch is a Linux system monitor for continuous use. I made it after the monitor from my distribution showed an occasional memory leak. The project also gave me a reason to learn [iced](https://iced.rs).

On my machine, the warm default uses less than 30 MiB of resident private memory. GNOME System Monitor uses approximately 80–100 MiB.

Lightwatch uses Rust and [iced](https://iced.rs). It supports Linux and uses the MIT license.

![Lightwatch Resources tab with CPU, memory, AMD and NVIDIA GPUs, and history presets](lightwatch-dashboard.png)

## Quick start

```bash
cargo build --release
cargo run --release              # GUI
cargo run --release -- --once    # one snapshot (waits ~1s for CPU deltas)
cargo run --release -- --soak 30 # headless RSS/CPU soak

# Install/update the binary used by a desktop/tray launcher:
cargo install --path . --force   # → ~/.cargo/bin/lightwatch
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--once` | | Write one snapshot to stdout, then exit. |
| `--soak SECS` | | Operate the headless sample loop, then write a summary. |
| `--interval MS` | `1000` | Set the sample period from 100 ms through 60 seconds. |
| `--history SECS` | `60` | Set the graph window to a maximum of 2 hours. Capacity is `window ÷ interval + 6` edge samples. |
| `-h`, `--help` | | Write the flag summary, then exit. |
| `-V`, `--version` | | Write the version, then exit. |

Use a recent stable Rust toolchain. The GUI requires Wayland or X11. NVIDIA metrics require `libnvidia-ml` from the driver package. Other panels work without this library.

## GPU power configuration

By default, Lightwatch pins the iced/wgpu compositor to an integrated AMD GPU. The full Radeon pin requires detected AMD hardware and the Radeon ICD.

Lightwatch manages `WGPU_POWER_PREF`, `VK_ICD_FILENAMES`, and `WGPU_BACKEND` as one group.

If all three variables are absent, startup sets `low`, the Radeon ICD, and `vulkan`. If the hardware or ICD is absent, startup sets only `low`. Any managed variable disables the automatic group.

```bash
# Recover through GL while clearing a stale ICD filter:
env -u VK_ICD_FILENAMES WGPU_BACKEND=gl WGPU_POWER_PREF=low cargo run --release
```

NVML telemetry uses a separate power gate. It reads sysfs `runtime_status` and does not wake a suspended discrete GPU.

By default, Tokio uses one worker. The `TOKIO_WORKER_THREADS` variable changes this count.

## What Lightwatch shows

The application has three tabs: **Resources**, **Processes**, and **Health**. **Resources** is the default tab.

- **CPU** shows total use, temperature, frequency, and a per-core chart. Lightwatch supports 256 cores and repeats a 16-color palette.
- **Memory and swap** show use charts and summary values. Lightwatch calculates used memory as `MemTotal − MemAvailable`.
- **GPUs** use the PCI bus-device-function identifier. AMD metrics come from sysfs. NVIDIA metrics use NVML only while `runtime_status` is `active`.
- **Processes** shows the full userspace list. Memory is the default descending sort.
  - The name uses the executable basename and the Electron `(type)` value.
  - CPU is the process share of total machine capacity. Memory is `RssAnon`.
  - The table also shows disk-read totals, disk-write totals, and the process ID.
  - Search matches a name or a PID prefix.
  - One click selects a row and enables **End Process**.
  - Two clicks open a live profile with CPU, private-memory, and block-I/O history.
  - The profile also shows fault rates, memory details, and process context.
  - Profile history starts after selection. It remains active across tabs until **Back**.
  - The history stops after the process exits.
  - **End Process** sends SIGTERM after a `pid+starttime` identity test.
  - A Chromium helper resolves to its application root only after each ancestry hop passes its tests.
- **Health** shows condition, not live load. It refreshes every 5 seconds.
  - Storage rows show block-backed mounts, a fill bar, and aligned used and available values.
  - Drive cards show the model, kind, size, and temperature. NVMe wear appears only for readable SMART data.
  - System batteries show charge, health, and cycles. Peripheral batteries show the name and charge. Lightwatch does not show charging or AC status.
- **Layout** gives the same height to each expanded Resource panel. The interface uses **▾** and **▸** disclosure controls.

The configuration file is `$XDG_CONFIG_HOME/lightwatch/ui.conf` or `~/.config/lightwatch/ui.conf`.

The MVP excludes network panels, disk-I/O dashboards, alerts, plugins, remote operation, daemons, a process tree, and SIGKILL escalation.

## Architecture

```text
UI (iced)  ←── notify + pull latest Arc ──  Sampler thread
                                              │
                         collectors (I/O) → pure parsers → Snapshot
                         history rings live only in the sampler
```

| Item | Rule |
|------|------|
| Snapshots | Each tick creates an immutable snapshot. Process rows and health data use latest-only state. Only the selected process owns detail rings. |
| History | Fixed rings use `capacity = floor(window/interval) + 6`. Capacity cannot exceed 7,206 points. |
| Charts | Canvas redraw uses explicit axis units, horizontal tangents, two-interval look-ahead, stable edges, and preserved gaps. |
| Handoff | One synchronized state owns the generation and `Arc<Published>`. A reader can skip intermediate snapshots but cannot receive a mismatched pair. |
| Time | Sample timestamps use `CLOCK_BOOTTIME`. |
| Scheduler | Deadline ticks control sampling. A late tick causes a skip. |
| Process CPU | Lightwatch calculates `(utime + stime) ÷ wall time ÷ online CPUs × 100`. |
| Process memory | Lightwatch uses `RssAnon`. Names come from the executable or `argv0` basename. |

```text
src/
  model/     Snapshot, History, ProcessRow/Profile, HealthSnapshot, name helpers
  parse/     /proc parsers + mounts, power_supply, nvme_smart (pure, tested)
  collect/   cpu, mem, self, process table/profile, health, gpu/{amd,nvidia}
  sample/    worker + latest
  ui/        tabs, prefs, sparklines
  diag.rs    --once / --soak
```

The UI uses The Elm Architecture. Collectors do not depend on the UI.

## Performance

Lightwatch uses one sampler thread, one Tokio worker, a single latest state, and fixed history. The application polls for publications every 100 ms.

Only expanded Resource charts request compositor redraw. History rings convert to linear data once for each publication. Cached axes and paths do not rebuild each frame.

Process profiling keeps expensive procfs reads and six history rings only for the selected `(pid, starttime)`. Open-file-descriptor counting stops at 4,096 entries for each slow refresh.

### Measured results

The baseline test used Pop!_OS 24.04 with COSMIC Wayland on 2026-07-17. The machine has a Ryzen 9 6900HS, AMD 680M, and RTX 3050 Mobile.

The release GUI completed its history warm-up before measurement. These baseline values predate compositor-paced chart motion.

| Resident private (`RssAnon`) | Total RSS | Threads | Swap | CPU |
|------------------------------|-----------|---------|------|-----|
| 28.7 MiB | 88.8 MiB | 7 | 0 | 0.68% of one logical CPU |

The compositor build used the same machine on 2026-07-27.

| State | Resident private (`RssAnon`) | Total RSS | Threads | Swap | CPU |
|-------|------------------------------|-----------|---------|------|-----|
| Resources expanded | 29.6 MiB | 90.4 MiB | 7 | 0 | 18.70% of one logical CPU |
| All charts collapsed | 28.6 MiB | 89.6 MiB | 7 | 0 | 4.97% of one logical CPU |

The preserved pre-change binary used 5.40% with expanded charts and 4.45% with collapsed charts. Current desktop conditions did not reproduce the earlier 0.68% result.

Visible compositor motion adds approximately 0.13 of one logical core. Collapsed charts stop the redraw chain. One visible Canvas drives redraw for all charts.

`RssAnon` measures the private resident footprint. Total RSS also includes shared files and GPU mappings. A leak test with swap uses `RssAnon + VmSwap`.

During a long test, private memory remained between 29.5 MiB and 30.3 MiB with no swap. Compiler pressure moved memory from residence to swap.

During that pressure, `RssAnon` decreased to 4.2 MiB. The combined `RssAnon + VmSwap` value remained approximately 30.1 MiB.

On this machine, headless `--once` and `--soak` use approximately 0.9 MiB of `RssAnon` and 6.5 MiB of RSS.

A paired process-detail test showed 4.2 MiB in Lightwatch and 78.9 MiB in GNOME System Monitor. The GNOME value excluded swapped pages.

The tools use different memory formulas. Do not expect equal process-memory values.

The geometry stress test uses 256 series with 7,200 points. A low-load sample took 23.70 ms for bursty data and 10.53 ms for fragmented data.

## Differences from GNOME System Monitor

- Lightwatch process memory uses `RssAnon`. The GNOME value is often similar to resident memory minus shared memory.
- Both CPU values target the total machine share. Different sample windows can produce different values.
- Lightwatch system memory uses `MemTotal − MemAvailable`.
- GPU and VRAM values can differ because the applications use different data sources.

## Development

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Local agent plans use the ignored `plans/` directory. This README is the source of truth for product behavior, architecture, and performance.

## License

Lightwatch uses the MIT license. Read [LICENSE](LICENSE) for the license text.
