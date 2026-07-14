# lightwatch

A native Linux system monitor you can leave open.

Not a clone of GNOME System Monitor. The point is **restraint**: flat memory by design, low idle CPU, bounded history, and a self strip that always shows lightwatch’s own cost.

Rust + [iced](https://iced.rs). Linux only. MIT.

## Quick start

```bash
cargo build --release
cargo run --release              # GUI
cargo run --release -- --once    # one snapshot (waits ~1s for CPU deltas)
cargo run --release -- --soak 30 # headless RSS/CPU soak
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--once` | | Snapshot to stdout, then exit |
| `--soak SECS` | | Headless sample loop + summary |
| `--interval MS` | `1000` | Sample period (100 ms–60 s) |
| `--history SECS` | `900` | Graph window (≤ 2 h; capacity = window ÷ interval) |

Needs a recent stable Rust. GUI wants Wayland or X11. NVIDIA metrics need `libnvidia-ml` (driver package); without it, other panels still work.

## What it shows

- **CPU** — overall %, temp, freq; history sparkline (per-core bars/graphs: next polish)
- **Memory / swap** — used ≈ `MemTotal − MemAvailable`, available, swap, load 1/5/15
- **GPUs** — discovered by **PCI address**, not DRM card index  
  - AMD: sysfs (`gpu_busy_percent`, VRAM, hwmon)  
  - NVIDIA: NVML only when sysfs `runtime_status` is **`active`** (fail-closed; will not wake a suspended dGPU)
- **Self** — RSS, self CPU%, last sample duration, overruns, skipped ticks

**Not in MVP:** process table/kill, network, disk I/O, alerts, plugins, remote, daemons, root-only metrics.

## Architecture (agents + humans)

```
UI (iced)  ←── notify + pull latest Arc ──  Sampler thread
                                              │
                         collectors (I/O) → pure parsers → Snapshot
                         history rings live only in the sampler
```

| Idea | Rule |
|------|------|
| Snapshots | Immutable; built each tick |
| History | Fixed-capacity rings; `capacity = floor(window/interval)` ≤ **7200** points/series |
| Handoff | Single-slot latest value; **never** a queue of snapshots |
| Time | `SamplePoint { t_boot_ns, value: Option<f32> }` via `CLOCK_BOOTTIME` (suspend-aware gaps) |
| Scheduler | Deadline ticks; late → skip, no catch-up burst |
| GPU id | Full PCI BDF `domain:bus:slot.function` |
| NVIDIA | Power gate before **any** NVML init/handle/query |
| Memory | `used = MemTotal.saturating_sub(MemAvailable)` |
| CPU % | `/proc/stat` deltas; no guest double-count; counter decrease → rebaseline |

```
src/
  model/     Snapshot, Reading, HistoryConfig, Ring, SamplePoint
  parse/     /proc/stat, meminfo, loadavg, self/stat  (pure, tested)
  collect/   cpu, mem, self, gpu/{amd,nvidia}
  sample/    worker (deadline + rings), latest (single slot)
  ui/        iced view + sparklines
  diag.rs    --once / --soak
```

Layout is TEA-shaped (immutable model, messages, subscription). Collectors stay UI-agnostic.

## Performance

Targets (engineering goals, measured honestly):

| | Goal |
|--|------|
| Sample cadence | 1 Hz default |
| Headless RSS | small; flat at fixed config |
| GUI RSS | aim &lt; 100 MiB after warmup (see measured) |
| Idle CPU | ≪ 1 core |
| History | constant for a given window |
| Steady-state subprocesses | none |

**Measured** (Pop!_OS 24.04 COSMIC Wayland, Ryzen 7 6800HS 16 threads, ~28 GiB, AMD 680M + RTX 3050 Mobile):

| Mode | RSS | Notes |
|------|-----|--------|
| `--once` / `--soak` | ~6–7 MiB | Flat over short soak; self CPU ~0% |
| GUI (iced + wgpu) | ~**230 MiB** | Matches system monitor; iced/wgpu baseline dominates. **Above** the 100 MiB aim — known gap for a later pass |
| Release binary | ~22 MiB | Unstripped |

UI currently wakes on a **250 ms** timer to poll the sampler notify channel (~4 Hz), not pure event-driven repaint. Graphs update at sample cadence (1 Hz by default), so they look stepped next to GNOME’s smoother multi-core charts — intentional MVP tradeoff, not final polish.

**Not verified yet:** worst-case publish cost at 100 ms × 7200 points; “dGPU starts suspended and stays suspended with GUI open” on a machine where the dGPU actually autosuspends (compositor often holds it active).

## Why numbers differ from GNOME System Monitor

- **Memory “used”** — we use `MemTotal − MemAvailable`. GNOME often reports a different used/cache split; totals and “pressure” semantics won’t match line-for-line.
- **CPU** — overall % is from the aggregate `cpu` line; GNOME’s multi-core view weights cores visually. Sampling phase and window also differ.
- **VRAM / GPU** — different sources (sysfs vs NVML vs GNOME’s path) and units.

Treat lightwatch as its own instrument, calibrated for leave-it-open cost, not pixel-identical to GNOME.

## Develop

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Local plans for agent workflows live under `plans/` (gitignored). Do not add speculative docs trees; keep this README the single source of product/architecture/performance truth.

## License

MIT — see [LICENSE](LICENSE).
