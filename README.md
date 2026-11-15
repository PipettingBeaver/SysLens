# SysLens

A lightweight, keyboard-driven monitor for **per-systemd-unit CPU, RAM and GPU
memory** on Linux, with one-key controls to freeze, stop or kill the process
groups that are crowding your GPU or memory, and a guard that does it for you.

It is built for small VRAM budgets (for example a 12 GB card that also drives
the desktop): it shows which *unit* owns the VRAM, warns before the compositor
runs out, and can freeze the largest idle model backend before the desktop
starts flickering.

It works without a vendor-specific agent: GPU memory is read from `nvidia-smi`
when present, otherwise from sysfs VRAM counters (amdgpu) and DRM `fdinfo`,
which also attributes per-process VRAM on amdgpu, i915, xe and other DRM
drivers.

```text
SysLens 0.4.0 snapshot 09:14:02  workstation  (16 cores)
CPU    42.2%  load 2.63 2.00 2.00
RAM   11.5GB / 15.6GB  available 4.0GB
SWAP  10.9GB / 15.6GB (70%)  zram 10.3GB -> 2.9GB
GPU   NVIDIA GeForce RTX 3060  VRAM 4010 / 12288 MiB (free 7888, reserve 2048)  util 15%  46C  unaccounted 1932 MiB
GPU alloc failures (15m) 0

UNIT                             PROCS       RSS      VRAM    CPU%  FLAGS
app:discord#b87eb7dd                 7   287.7MB   533 MiB     7.7
app:opencode-desktop                 7   665.8MB   263 MiB    61.2  SELF
plasma-kwin_wayland                  3   726.6MB   257 MiB    23.0  COMP
app:firefox                         33     5.0GB         -    61.2
plasma-plasmashell                   1   841.4MB         -     0.0  DESKTOP
docker:homeassistant                 9   228.7MB         -     0.0  CONTAINER
```

## Why

Existing monitors show per-process numbers. On a systemd desktop the useful unit
is the *service or app scope*, and the useful action is `systemctl freeze` /
`stop` on that unit. SysLens groups processes by cgroup, attributes GPU memory
back to units, and keeps the compositor, the desktop session, kernel threads and
SysLens itself protected from accidental stops.

## What it does

- Per-unit table: process count, resident memory, VRAM, CPU% and flags
  (`MODEL`, `DESKTOP`, `COMP`, `SYSTEM`, `CONTAINER`, `STOPPED`, `FROZEN`).
- Live warnings for low VRAM headroom, low RAM, swap pressure, memory-stall
  pressure (PSI), GPU allocation failures in the kernel log, and multiple
  co-loaded model backends.
- Actions: freeze/thaw (pause in memory), stop/start, SIGKILL.
- A guard that freezes the largest **idle** model unit under pressure, stops it
  after a grace period if pressure persists, and thaws it when pressure clears.
- `--once` plain-text snapshot for scripts and status bars.
- `--explain-guard` prints every unit with the reason it is or is not
  guard-eligible.
- GPU and Docker queries run on background threads, so a slow `nvidia-smi` or
  Docker daemon never stalls the UI.

## Similar tools and when to use them

If you know Windows, the closest mental model is **Process Explorer**: a live
list you can sort, with suspend/resume and kill actions. SysLens is the same
idea for **systemd units** rather than processes. It aggregates a service or app
scope (including its children and containers), shows the VRAM that scope holds,
and freezes or stops the *unit* instead of guessing which PID to kill. The trade
is deliberate: Process Explorer gives far more per-process detail (handles,
DLLs, threads, per-process GPU bytes), while SysLens gives unit-level accounting
and an automatic guard.

| Tool | Grouping unit | GPU memory | Act on the group | Automatic guard |
| --- | --- | --- | --- | --- |
| **SysLens** | systemd unit/scope | per unit (DRM `fdinfo` + `nvidia-smi`) | freeze, stop, start, SIGKILL | yes: freeze, stop, thaw |
| `systemd-cgtop` | cgroup | no | no | no |
| `nvtop` | process + GPU | yes, per process | signal one PID | no |
| `btop` | process | per process, if built with GPU support | signal one PID | no |
| `htop` / `top` | process | no | signal one PID | no |
| Mission Center / Resources | process or app (GUI) | per app/process (Mission Center uses nvtop) | end process | no |
| earlyoom / `systemd-oomd` | process / cgroup | no | SIGKILL (daemon-chosen victim) | yes, but kill-only and memory-only |
| Process Explorer / Task Manager (Windows) | process | per process | suspend, kill tree | no |

Which one to reach for:

- **`nvtop`** answers "which PID is on the GPU right now?" fastest. It covers
  NVIDIA, AMD and Intel from one binary. Per-process figures need the kernel's
  DRM `fdinfo` interface (AMD 5.14+, Intel 5.19+), and Intel totals may need
  `CAP_PERFMON`.
- **`systemd-cgtop`** is the closest thing to SysLens for grouping: it reports
  CPU, memory and I/O per cgroup. It has no GPU column and no actions, and its
  memory numbers are only complete when `MemoryAccounting=1` is enabled for the
  units you care about.
- **`btop`** is a good general terminal monitor. Note that release binaries are
  often built without GPU support, so GPU panels can be missing even when
  `nvidia-smi` works.
- **Mission Center / Resources** are the GUI equivalents for a desktop: nice
  graphs and per-app breakdowns, but no systemd-unit actions and no guard.
- **earlyoom / `systemd-oomd`** are the automation equivalents. They act on
  memory pressure (systemd-oomd uses PSI, like SysLens' memory-stall trigger)
  and SIGKILL a victim. They do not freeze, do not thaw, have no VRAM signal,
  and choose the victim for you.
- **SysLens** is for the case where the useful object is the *unit* and the
  useful action is reversible: freeze first, stop only if pressure persists, and
  thaw when it clears. Use it alongside `nvtop` (detail) and `systemd-cgtop`
  (accounting) rather than instead of them.

## Install

Requirements:

- Linux with systemd (cgroup v2) and `/proc`
- Rust 1.88+ to build (or download a release binary)
- Optional: `nvidia-smi` for NVIDIA device totals and compute processes;
  amdgpu sysfs counters and DRM `fdinfo` work without it; `docker` for container
  names
- A systemd **user** session (the TUI controls `systemctl --user`)

From a release tarball:

```sh
tar xzf syslens-0.4.0-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 syslens ~/.local/bin/syslens
```

From source:

```sh
git clone https://github.com/PipettingBeaver/SysLens.git
cd SysLens
cargo build --release
install -Dm755 target/release/syslens ~/.local/bin/syslens
```

Or with Cargo:

```sh
cargo install --path .
```

## Run

```sh
syslens            # live TUI (guard on by default)
syslens --once     # one plain snapshot and exit
```

Keys:

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | move selection |
| `Enter` / `m` | actions menu |
| `s` | stop (suspend; frees RAM/VRAM) |
| `r` | start |
| `f` / `t` | freeze / thaw |
| `K` | force kill (SIGKILL) |
| `p` | processes in the selected unit |
| `!` | emergency panel |
| `x` | toggle idle units |
| `?` | help |
| `q` | quit |

## Options

```text
-i, --interval SECS   screen refresh interval (default 1.0)
    --reserve-mb MB   VRAM kept for the compositor in warnings (default 2048)
    --once            print one plain snapshot and exit
    --no-gpu          skip GPU queries
    --no-guard        disable the guard for this run
    --guard-grace S   override the guard grace period (default 60)
    --guard-target U  only auto-guard this unit (repeatable/comma-separated)
    --guard-config P  read guard settings from P (default ~/.config/syslens/guard.conf)
    --explain-guard   print guard eligibility for every unit, then exit
    --allow-system    allow system model units (needs root or passwordless sudo)
    --headless-guard  run the guard without the TUI (for a systemd service)
-h, --help            show help
-V, --version         show version
```

## The guard

The guard is part of the design and **on by default** in the TUI and in
`--headless-guard`. It exists because a 12 GB card that also drives the
compositor has very little headroom, and by the time the desktop flickers it is
too late to react by hand.

What it does, in order:

1. Watches VRAM headroom (against `--reserve-mb`), available RAM, swap usage,
   memory-stall pressure (PSI), and new GPU allocation-failure lines in the
   kernel log.
2. When pressure is critical and has persisted for `sustain` seconds, freezes
   the **largest idle** eligible model unit. GPU allocation failures act
   immediately, because they are already visible flicker.
3. If pressure is still critical after `grace` seconds, stops that unit, which
   frees its RAM and VRAM.
4. If pressure returns to normal for `recover` seconds, thaws a unit it froze.
5. Enforces `cooldown` between freezes so it cannot walk through every backend
   at once.

An honest caveat: **freezing keeps memory resident**, including GPU
allocations. Freezing relieves CPU contention and stops new work, but it does
not free VRAM. Only the stop at step 3 frees memory. That is why the guard
freezes first and stops only if pressure persists.

Eligibility is deliberately conservative:

- A unit must match the model hints (or the `allow` list) and be **idle**
  (`busy_cpu`). Busy units are skipped unless `allow_busy = true`, in which case
  they are ordered last.
- `deny` entries are never touched.
- `--guard-target`, when set, restricts the guard to those units.
- Protected units are never touched: compositor and window managers, desktop
  session services, display managers, kernel thread groups, `user@*.service`,
  `init.scope`, `session-*.scope`, `*.slice`, and SysLens' own scope.
- System units stay out unless `--allow-system` is set.

Run `syslens --explain-guard` to see the decision for every unit:

```text
ollama.service            4200M   12.0%  eligible (model hint 'ollama')
plasma-kwin_wayland.serv…  257M   25.2%  protected: compositor: stopping it ends the desktop session
docker-0123abcd.scope        0M    0.0%  system unit (needs --allow-system)
my-game.scope             1800M   94.5%  not a model unit (no hint or allow match)
```

### Configuration

Create `~/.config/syslens/guard.conf` (or pass `--guard-config P`). A missing
default file means built-in defaults; a missing file passed explicitly is an
error. Unknown keys and malformed values are rejected with the line number.

```ini
# timing (seconds)
sustain = 3
grace = 60
recover = 10
cooldown = 120

# candidate gating
busy_cpu = 50
allow_busy = false

# RAM / swap / memory-stall thresholds (percent for swap and PSI)
ram_critical_mib = 1536
ram_warn_mib = 3072
swap_critical_pct = 85
swap_warn_pct = 70
psi_critical = 60
psi_warn = 20

# kernel log pattern for GPU allocation failures (journalctl -g regex)
# gpu_log_pattern = NVKMS|GEM object|failed to allocate

# hints (comma separated; these extend the built-in lists)
model_hints_add = mychat, mygen
protect_hints_add = my-compositor
system_hints_add = my-daemon

# replace the built-in model list instead of extending it
# model_hints_only = ollama, mychat

# always eligible / never touched
allow = my-engine.service
deny = someapp
```

Hint matching is by whole tokens, so `tts` does not match `kttsd` and `embed`
does not match `embedded-viewer`. A trailing run of digits is allowed on the
last token, so `kded` matches `kded5`.

See [`docs/guard.conf.example`](docs/guard.conf.example) for a commented file.

### Run it as a service

```sh
mkdir -p ~/.config/systemd/user
cp systemd/syslens-guard.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now syslens-guard.service
```

The unit assumes the binary is at `~/.local/bin/syslens` and uses
`Restart=on-failure` so a clean SIGTERM does not loop. Stop it with:

```sh
systemctl --user disable --now syslens-guard.service
```

## Safety model

Actions only target systemd `--user` units by default. The following are always
protected: compositor and window-manager units (KWin, GNOME Shell, Mutter,
wlroots and Hyprland units, Sway, Xorg, and display managers such as SDDM, GDM,
greetd, LightDM, LY), desktop session services (Plasma, GNOME, Xfce, Cinnamon,
MATE, LXQt, and common bars/notification/idle daemons), kernel thread groups,
`user@*.service`, `init.scope`, `session-*.scope`, `*.slice`, and SysLens' own
scope.

System model units (for example a root-run `ollama.service`) are only actionable
with `--allow-system`, which uses `sudo -n`.

Every `systemctl` call is built as `systemctl [--user] <verb> -- <unit>`, and
the unit name is validated first: it must be a plausible systemd unit name
with a `.service`, `.scope` or `.slice` suffix, so a placeholder like `kernel`
or an option-like name can never reach the command line.

Destructive actions (`stop`, force kill) ask for confirmation before running.

## How it works

- Reads `/proc/<pid>/stat`, `statm` and `cgroup` directly (no `psutil`).
- Resolves the unit from the cgroup path; handles systemd `\xHH` escaping.
- Attributes GPU memory from up to three sources: DRM `fdinfo` for every
  vendor that implements it, `nvidia-smi --query-compute-apps` for NVIDIA
  compute, and device totals from `nvidia-smi` or amdgpu sysfs. Where two
  sources report the same process, the larger value wins.
- Reads memory-stall pressure from `/proc/pressure/memory` when available.
- Caches GPU (2 s) and Docker (10 s) lookups and refreshes them off the UI
  thread; each child query has a hard timeout and drains its pipe concurrently
  so large outputs cannot deadlock.
- Counts recent GPU allocation-failure kernel messages with a background
  `journalctl -k` watcher.

## Limitations and alternatives

**Limitations**

- Linux only; systemd (cgroup v2) is assumed for unit resolution.
- Intel discrete GPUs that expose no VRAM counters yield no device totals, so
  VRAM headroom warnings are skipped; RAM, PSI and log-based triggers still
  work, and per-process VRAM still comes from `fdinfo` where the driver
  implements it.
- Reading the kernel journal needs permission; if it is not readable, the
  allocation-failure trigger is skipped rather than fired.
- Docker container names need access to the Docker socket; otherwise the
  container ID is shown.
- Freezing does not free memory (see the guard section).
- Live state only: there is no history, graphing, persistence or remote
  monitoring, and SysLens opens no network socket.

**Alternatives**

- Per-PID GPU detail, graphs and vendor metrics: use **`nvtop`** (NVIDIA, AMD,
  Intel) or **Mission Center** for a GUI.
- Per-cgroup CPU/memory/I/O accounting over time: use **`systemd-cgtop`**,
  `sar`, or a Prometheus/node-exporter setup.
- Unattended out-of-memory policy with no interaction: use **`earlyoom`** or
  **`systemd-oomd`**. Both SIGKILL a victim; neither freezes, thaws or watches
  VRAM.
- Trees, handles, DLLs, threads and stacks: on Windows use **Process Explorer**;
  on Linux use `lsof`, `/proc/<pid>`, `gdb`, or `bpftrace`.
- Multi-host or remote monitoring: use **Glances**, Netdata or
  Prometheus/Grafana. SysLens is deliberately local and read-only on the
  network.
- Non-systemd distributions (runit, OpenRC, s6): SysLens cannot resolve units;
  use `htop`, `btop`, or cgroupfs tooling instead.

## License

MIT. See [LICENSE](LICENSE).
