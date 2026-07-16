# lightwatch

A Linux system monitor built for leaving open. I like to keep my system monitor open continuously but the one that came with my distro had an occasional memory leak so I figured this was a good excuse to learn [Iced](https://iced.rs).

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
| `--history SECS` | `60` | Graph window (≤ 2 h; capacity = window ÷ interval + 6 edge samples) |

Needs a recent stable Rust. GUI wants Wayland or X11. NVIDIA metrics need `libnvidia-ml` (driver package); without it, other panels still work.

## GPU power posture (iGPU default)

Lightwatch pins its UI compositor (iced/wgpu) to the integrated GPU by default so the GUI does not wake a suspended or idle discrete GPU:

- On startup, if **neither** `WGPU_POWER_PREF` nor `VK_ICD_FILENAMES` is already set in the environment, lightwatch sets both:
  - `WGPU_POWER_PREF=low` (soft wgpu adapter preference), and
  - `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/radeon_icd.json` (hard ICD filter; only set when the radeon ICD file exists).
- If you set **either** env var yourself (including `WGPU_POWER_PREF=high`), lightwatch leaves both alone — you are in full control. Set both to override completely (e.g., force NVIDIA), or unset both to get the automatic iGPU default.
- The hard bar: with the default bundle applied, lightwatch renders on the AMD GPU. NVIDIA metrics are gated by the dGPU's sysfs `runtime_status` before any NVML operation — when the dGPU is suspended, telemetry becomes `Unavailable` and the rest of the UI remains functional. Active-state NVML failures clear the cache and may retry on a later sample; lightwatch never writes power controls.
- **Renderer and NVML are separate namespaces:** renderer DRM/render-node descriptors map to their backing PCI device through sysfs (major/minor → device path → PCI address). NVIDIA `/dev/nvidia*` descriptors are NVML driver handles, not renderer selection — they are permitted only when the dGPU was already active for another reason, and they are counted and reported independently. Do not correlate DRM and NVML by ordinal card number.
- On machines with a broken radv install, the `VK_ICD_FILENAMES` filter can prevent iced from finding an adapter at all. Simply unsetting `VK_ICD_FILENAMES` with `env -u` is not enough — both vars would then be absent and the default bundle would fire again on next launch. Recovery: set **either** env var to a present value (disabling the whole atomic bundle) **and** clear any stale `VK_ICD_FILENAMES` from a prior launch, e.g.:  
  `WGPU_POWER_PREF=low env -u VK_ICD_FILENAMES cargo run --release`  
  (or `WGPU_POWER_PREF=high`, or set `VK_ICD_FILENAMES` to a full ICD list). WGPU and the system ICD loader will then enumerate adapters normally.

Whether the dGPU reaches `runtime_status=suspended` depends on the platform and PRIME configuration. When it is already active, NVML legitimately opens `/dev/nvidia*` descriptors; those do not imply that the UI renderer selected NVIDIA.

## What it shows

- **CPU** — overall %, temp, freq in header; **all logical CPUs** as multi-series overlay chart with stable per-core colors and legend (live % per core). Up to 256 cores supported; palette wraps at 16 colors.
- **Memory / swap** — dual-series chart (used %, swap %) with grid; stat chips for Used, Avail, Swap, Load 1/5/15
- **GPUs** — discovered by **PCI address**, not DRM card index  
  - AMD: sysfs (`gpu_busy_percent`, VRAM, hwmon)  
  - NVIDIA: NVML only when sysfs `runtime_status` is **`active`** (fail-closed; will not wake a suspended dGPU)
- **Self** — private anonymous footprint (RssAnon), total RSS, self CPU%, last sample duration, overruns, skipped ticks. Private footprint answers "what does lightwatch itself own?" while total RSS explains system-monitor differences and GPU mappings.

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
| History | Fixed-capacity rings; `capacity = floor(window/interval) + 6` ≤ **7206** points/series. The guard supports three off-left and two off-right spline neighbors plus inclusive boundary accounting. |
| Charts | Two-interval diagnostic look-ahead for every chart. Dense CPU history uses absolute time buckets plus raw edge bands; the outer two logical pixels remain stable through arrivals and evictions. Gaps remain discontinuities. |
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
  parse/     /proc/stat, meminfo, loadavg, self/stat, self/status  (pure, tested)
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

| Mode | Configured capacity / warm-up | Anon start → end | RSS start → end | CPU over 60s |
|------|-------------------------------|------------------|----------------|--------------|
| `--once` / `--soak` | — | ~0.9 MiB | ~6.5 MiB | ~0.2% self CPU over a short soak |
| GUI, full 1m history | 66 samples / 70s | 41.61 → 41.62 MiB | 140.01 → 140.01 MiB | 2.35% of one logical CPU |

The measured GUI run uses the release binary, default 1s sampler, and fixed 100ms display wake. It warms for longer than its configured ring capacity, then measures for 60 seconds. CPU is `Δ(utime + stime) / CLK_TCK / Δ/proc/uptime × 100`, so 100% means one fully occupied logical CPU; memory comes from `/proc/<pid>/status`. Ring occupancy and lifetime overrun/skip totals are not externally exposed by the current GUI, so the fill claim is based on the deadline schedule plus the stated capacity/warm-up rather than a separately captured occupancy counter.

The unstripped release binary is about 22 MiB on disk.

**Synthetic geometry** (release mode, 256 series × 7 200 points, 700px plot): a gap-free series produces 785 Bézier segments against a derived 794-segment bound. The 256-series bursty-gap case produced 1,172 segments/series in 23.4ms; pathological sub-pixel fragmentation produced no drawable segments after preserving and coalescing its gaps. The fully fragmented hard bound is 7,181 segments/series (the ring capacity remains the final cap), while ordinary gap-free CPU history is pixel-bounded.

UI wakes on a **100 ms** timer; the sampler thread runs on a deadline schedule at the configured cadence (1 s default). Charts use a two-interval diagnostic look-ahead (`window_end = boottime_now − 2 × interval`) so the next two known samples sit off-screen right. X scrolls uniformly and the clipped entrance/exit geometry is final at reveal. Display cadence is uncoupled from sample cadence.

**Verified:** the release GUI renderer descriptors all mapped through DRM `226:128` to AMD PCI `0000:04:00.0`. The already-active NVIDIA GPU opened six separate NVML descriptors (`nvidiactl`, `nvidia0`, and `nvidia-uvm`). Automated tests cover two clip strips through multiple arrivals/evictions with and without decimation, jittered timestamps, and edge-adjacent gaps.

**Deferred:** the full 60-minute-history GUI measurement was not run in this batch. Natural suspended-dGPU verification also remains hardware-state dependent; the current compositor kept NVIDIA `active`.

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
