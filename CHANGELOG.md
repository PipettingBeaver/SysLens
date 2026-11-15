# Changelog

All notable changes to SysLens are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1] - 2026-11-15

### Added

- Static musl release builds for `x86_64` and `aarch64`, so the binaries run on
  older distros without a glibc version dependency and on ARM hosts.
- `SECURITY.md` with a private vulnerability reporting path.
- A `main` branch ruleset requiring a pull request and the `test` and `msrv`
  checks, and blocking force-pushes and deletion.
- Dependabot version updates for Cargo and GitHub Actions, with Dependabot
  security updates and alerts enabled.
- A `musl` CI job that builds and verifies static binaries on both working
  architectures.

### Changed

- GitHub Actions now use Node 24 runtimes (`actions/checkout@v7`,
  `softprops/action-gh-release@v3`), clearing the Node 20 deprecation warning.
- Actions are pinned by commit SHA in CI and release workflows.
- CI pins `ubuntu-24.04` instead of `ubuntu-latest`, matching the release job.
- CI uses a read-only token, cancels superseded runs, has a timeout, builds
  with `--locked`, and adds an MSRV job for the declared Rust 1.88.
- Release runs the test suite before publishing.
- The MSRV job passes `toolchain: 1.88.0` as an input so Dependabot does not
  treat it as an action version bump.

## [0.4.0] - 2026-11-14

### Added

- Vendor-neutral GPU memory telemetry: `nvidia-smi` when available, amdgpu
  sysfs VRAM counters otherwise, and per-process VRAM from DRM `fdinfo`
  (amdgpu/i915/xe and other DRM drivers). The VRAM path no longer requires
  NVIDIA.
- Configurable guard gating in `~/.config/syslens/guard.conf`: sustain, grace,
  recover and cooldown timings; busy-unit threshold; RAM, swap and memory-stall
  thresholds; kernel-log pattern; hint lists; and `allow`/`deny` overrides.
  Unknown keys are rejected with the line number.
- `--explain-guard` prints every unit with the reason it is or is not eligible.
- `--guard-config PATH` to load an explicit config file.
- Memory-stall pressure trigger built on `/proc/pressure/memory` (PSI), the same
  signal systemd-oomd uses, so pressure can be detected before RAM or swap
  numbers look alarming.
- Guard skips busy model units and prefers idle ones, so a model that is
  actively serving requests is not frozen mid-work.
- Expanded compositor and desktop protection for Sway, Hyprland, wlroots
  compositors, Xfce, Cinnamon, MATE, LXQt, and common bars, notification, idle
  and lock daemons, in addition to KDE Plasma and GNOME.

### Changed

- The guard is documented as an intended, on-by-default part of SysLens rather
  than an optional extra, in both the README and `--help`.
- Hint matching is whole-token based instead of substring based, so `tts` no
  longer matches `kttsd` and `embed` no longer matches `embedded-viewer`.
  Trailing digits are still allowed (`kded` matches `kded5`).
- Every `systemctl` invocation is built as `systemctl [--user] <verb> -- <unit>`
  and unit names are validated before use; `sudo -n` calls now use
  `sudo -n -- systemctl`.
- GPU warning text is vendor-neutral (`GPU allocation failures` instead of
  `NVKMS allocation failures`).

### Fixed

- The guard now thaws a unit it froze once pressure has been normal for the
  configured recovery window. Previously the README promised this but the code
  left the unit frozen until it was thawed by hand.
- A guard failure (failed freeze, stop or thaw) suppresses the guard until
  pressure clears instead of retrying every sample.
- A frozen unit now thaws once pressure has been below critical (Warning or
  Normal) for the recovery window, instead of waiting for perfectly normal
  pressure that may never arrive.
- The `guard on` label and countdown now reflect the configured grace period.

### Tests

- 63 tests covering hint tokenization, config parsing and rejection, systemctl
  command construction and unit validation, NVIDIA/sysfs/fdinfo parsers, PSI
  parsing, and the guard state machine (sustain, recovery, grace, cooldown,
  busy filtering, allow/deny, targets, protection, failed actions, and a unit
  that vanishes while frozen).

### Documentation

- README now compares SysLens with `systemd-cgtop`, `nvtop`, `btop`, `htop`,
  Mission Center/Resources, earlyoom/`systemd-oomd`, and Windows Process
  Explorer/Task Manager, including when to use each.
- The limitations section now also lists alternatives for per-PID GPU detail,
  history/accounting, unattended OOM policy, process internals, remote
  monitoring, and non-systemd distributions.

## [0.3.0] - 2026-11-14

Initial public release.

### Added

- Per-unit CPU, RAM and VRAM monitor for Linux, grouping processes by systemd
  unit and attributing `nvidia-smi` VRAM back to units.
- TUI plus `--once` plain-text snapshot.
- Freeze, thaw, stop, start and SIGKILL actions on systemd user units, with
  confirmation for destructive actions.
- Pressure guard with a grace period and protected units.
- GPU and Docker queries on background threads with drained pipes and timeouts.
- Colour output in the TUI and in `--once` when attached to a terminal.
- CI running `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and a
  release build.

### Notes

- Before 0.3.0 SysLens existed as a Python sampler. The Rust binary replaced it
  and is the supported product; the legacy sampler was dropped.
