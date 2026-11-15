# Security Policy

## Supported versions

Security fixes target the latest release and the `main` branch. Older releases
are not maintained.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting:

<https://github.com/PipettingBeaver/SysLens/security/advisories/new>

Do not open a public issue for a problem that could be exploited. Include:

- the SysLens version (`syslens --version`) and how it was installed;
- distribution, kernel and systemd version;
- whether you ran the TUI, `--headless-guard`, or `--allow-system`;
- a minimal reproduction and the relevant `guard.conf`, if any.

You will get a best-effort acknowledgement, usually within a few days. There is
no formal SLA. Please allow time for a fix and a release before public
disclosure.

## Scope

SysLens is a local tool. It opens no network socket and stores no secrets.

Areas that matter for security:

- unit-name validation and construction of `systemctl` / `sudo` command lines;
- the guard's freeze, stop and thaw decisions, especially protected units;
- the `--allow-system` path, which can use `sudo -n`;
- parsing of `guard.conf`, `/proc`, `fdinfo` and kernel-log output;
- denial of service through resource exhaustion while scanning many processes.

Out of scope: vulnerabilities in systemd, GPU drivers, `nvidia-smi`, Docker, or
other programs SysLens invokes.

## Safe defaults

- Actions target `systemctl --user` units unless `--allow-system` is set.
- Compositor, desktop session, kernel, slice/session scopes and SysLens' own
  scope are protected.
- Unit names are validated before they reach a process argument list, and `--`
  is always placed before the unit.
- The guard prefers idle units, has a grace period, and thaws on recovery.
