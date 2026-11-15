# SysLens

A lightweight, keyboard-driven monitor for **per-systemd-unit CPU, RAM and VRAM**
on Linux, with one-key controls to freeze, stop or kill the process groups that
are crowding your GPU or memory.

It is built for small VRAM budgets (for example a 12 GB card that also drives the
desktop): it shows which *unit* owns the VRAM, warns before the compositor runs
out, and in my specific use-case was to help automatically freeze the largest model backend before the desktop
starts flickering from VRAM allocation issues while learning to manage optimization.

<img width="1630" height="1053" alt="image" src="https://github.com/user-attachments/assets/aff491a5-62a7-431a-8f88-ab7adfbc7f36" />

## Why

This was made specifically for helping monitor stuff all in one place for multiple projects at once on limited hardware.
Existing monitors show per-process numbers. On a systemd desktop the useful unit
is the *service or app scope*, and the useful action is `systemctl freeze` /
`stop` on that unit. SysLens groups processes by cgroup, attributes `nvidia-smi`
memory back to units, and keeps the compositor, the desktop session, kernel
threads and SysLens itself protected from accidental stops.

## What it does

- Per-unit table: process count, resident memory, VRAM, CPU% and flags
  (`MODEL`, `DESKTOP`, `COMP`, `SYSTEM`, `CONTAINER`, `STOPPED`, `FROZEN`).
- Live warnings for low VRAM headroom, low RAM, swap pressure, NVKMS allocation
  failures in the kernel log, and multiple co-loaded model backends.
- Actions: freeze/thaw (pause in memory), stop/start, SIGKILL.
- Optional guard: on VRAM/RAM/NVKMS pressure, freeze the largest eligible model
  unit, then stop it after a grace period if pressure persists.
- `--once` plain-text snapshot for scripts and status bars.
- GPU and Docker queries run on background threads, so a slow `nvidia-smi` or
  Docker daemon never stalls the UI.

## Install

Requirements:

- Linux with systemd (cgroup v2) and `/proc`
- Rust 1.88+ to build
- Optional: `nvidia-smi` for VRAM, `docker` for container names
- A systemd **user** session (the TUI controls `systemctl --user`)

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
syslens            # live TUI
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
    --no-gpu          skip nvidia-smi queries
    --no-guard        disable the automatic freeze/stop guard
    --guard-grace S   seconds before a frozen unit is stopped (default 60)
    --guard-target U  only auto-guard this unit (repeatable/comma-separated)
    --allow-system    allow system model units (needs root or passwordless sudo)
    --headless-guard  run the guard without the TUI (for a systemd service)
-h, --help            show help
-V, --version         show version
```

## Safety model

Actions only target systemd `--user` units by default. The following are always
protected: the Wayland/X compositor, desktop session services, kernel thread
groups, `user@*.service`, `init.scope`, `session-*.scope`, `*.slice`, and
SysLens' own scope. System model units (for example a root-run `ollama.service`)
are only actionable with `--allow-system`, which uses `sudo -n systemctl`.

Destructive actions (`stop`, force kill) ask for confirmation before running.

## Guard

The guard watches VRAM headroom (against `--reserve-mb`), available RAM, swap
usage and new `NVKMS` kernel messages. When pressure is critical it freezes the
largest eligible model unit. If pressure persists for `--guard-grace` seconds it
stops that unit; if pressure clears it thaws the unit. It never touches protected
units, and it is disabled for the session when you manually freeze or thaw a
model unit.

Run it as a user service with `--headless-guard`:

```sh
mkdir -p ~/.config/systemd/user
cp systemd/syslens-guard.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now syslens-guard.service
```

The unit assumes the binary is at `~/.local/bin/syslens` and remembers its own
stop command: `systemctl --user disable --now syslens-guard.service`.

## How it works

- Reads `/proc/<pid>/stat`, `statm` and `cgroup` directly (no `psutil`).
- Resolves the unit from the cgroup path; handles systemd `\xHH` escaping.
- Attributes GPU memory by mapping `nvidia-smi --query-compute-apps` PIDs back
  to units.
- Caches GPU (2 s) and Docker (10 s) lookups and refreshes them off the UI
  thread; each child query has a hard timeout and drains its pipe concurrently
  so large outputs cannot deadlock.
- Counts recent NVKMS kernel messages with a background `journalctl -k` watcher.

## Limitations

- Linux only; cgroup v2 is assumed for unit resolution. On Windows you can try Process Explorer
- Reads the kernel journal; the guard's NVKMS check is skipped if the service
  user cannot read it.
- Docker container names need access to the Docker socket; otherwise the
  container ID is shown.

## License

MIT. See [LICENSE](LICENSE).
