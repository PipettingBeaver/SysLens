//! Guard: freeze, then stop, the largest idle model unit when memory pressure
//! persists, and thaw it again when pressure clears.
//!
//! The decision logic is split so it can be tested without a terminal or a
//! running service manager:
//!
//! - `assess` turns a snapshot into a pressure level and a trigger.
//! - `App::verdict` says why one unit is or is not eligible.
//! - `App::evaluate_guard` applies both over time (sustain, recovery,
//!   cooldown, grace) and calls the `Actuator`.
//!
//! Freezing keeps memory resident, including GPU allocations, so it relieves
//! CPU pressure only. VRAM and RAM are freed when the unit is stopped, which
//! happens after the grace period if pressure still persists.

use std::collections::HashSet;
use std::time::Instant;

use super::{App, StatusKind};
use crate::config::GuardConfig;
use crate::hints;
use crate::proc::{self, Level, Snapshot, UnitStat};
use crate::sysd::Action;
use crate::util::human_bytes;

const MIB: u64 = 1024 * 1024;
/// Units below this much VRAM are not worth freezing for a VRAM trigger.
const MIN_VRAM_MIB: u64 = 512;
/// RAM trigger: units below both limits are too small to matter.
const MIN_RAM_RSS: u64 = 512 * MIB;
const MIN_RAM_VRAM_MIB: u64 = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Pressure {
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Trigger {
    Vram,
    Ram,
    GpuAlloc,
}

#[derive(Clone, Debug)]
pub(super) enum GuardPhase {
    Idle,
    /// The guard froze `unit` at `since`. Thaws on recovery, stops after grace.
    Frozen {
        unit: String,
        system: bool,
        since: Instant,
    },
    /// The guard stopped a unit. Another one may be handled after cooldown.
    Handled {
        unit: String,
    },
    /// A manual action changed things; wait for pressure to clear.
    Suppressed,
}

pub(super) struct Guard {
    pub(super) enabled: bool,
    pub(super) cfg: GuardConfig,
    /// Model-name hints from the config, used for eligibility.
    pub(super) model_hints: Vec<String>,
    pub(super) targets: HashSet<String>,
    pub(super) phase: GuardPhase,
    pub(super) critical_since: Option<Instant>,
    calm_since: Option<Instant>,
    last_freeze: Option<Instant>,
    notified_no_candidate: bool,
    last_alloc: Option<usize>,
}

impl Guard {
    pub(super) fn reset(&mut self) {
        self.phase = GuardPhase::Idle;
        self.critical_since = None;
        self.calm_since = None;
        self.notified_no_candidate = false;
    }

    pub(super) fn new(
        enabled: bool,
        cfg: GuardConfig,
        targets: Vec<String>,
        last_alloc: Option<usize>,
    ) -> Self {
        let model_hints = cfg.hint_set().model;
        Self {
            enabled,
            cfg,
            model_hints,
            targets: targets.into_iter().collect(),
            phase: GuardPhase::Idle,
            critical_since: None,
            calm_since: None,
            last_freeze: None,
            notified_no_candidate: false,
            last_alloc,
        }
    }
}

/// Why a unit is or is not a guard candidate. Shown by `--explain-guard`.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Verdict {
    /// Eligible, with the reason it matched (hint or allow entry).
    Eligible(String),
    Stopped,
    Frozen,
    Denied,
    NotTargeted,
    SystemUnit,
    Protected(&'static str),
    NotModel,
    BelowThreshold,
    Busy(f64),
}

impl Verdict {
    pub(super) fn is_eligible(&self) -> bool {
        matches!(self, Verdict::Eligible(_))
    }

    pub(super) fn describe(&self, busy_cpu: f64) -> String {
        match self {
            Verdict::Eligible(reason) => format!("eligible ({reason})"),
            Verdict::Stopped => "already stopped".to_string(),
            Verdict::Frozen => "already frozen".to_string(),
            Verdict::Denied => "denied by deny list".to_string(),
            Verdict::NotTargeted => "not in --guard-target".to_string(),
            Verdict::SystemUnit => "system unit (needs --allow-system)".to_string(),
            Verdict::Protected(reason) => format!("protected: {reason}"),
            Verdict::NotModel => "not a model unit (no hint or allow match)".to_string(),
            Verdict::BelowThreshold => "below the size threshold for this trigger".to_string(),
            Verdict::Busy(cpu) => format!("busy: {cpu:.0}% CPU (limit {busy_cpu:.0}%)"),
        }
    }
}

/// Pressure level, trigger, and the human-readable reasons, from one snapshot.
/// `alloc_new` is true when the kernel log recorded new GPU allocation failures
/// since the previous sample.
pub(super) fn assess(
    snapshot: &Snapshot,
    cfg: &GuardConfig,
    alloc_new: bool,
) -> (Pressure, Trigger, Vec<String>) {
    let mut level = Pressure::Normal;
    let mut trigger = Trigger::Vram;
    let mut reasons = Vec::new();
    let mut gpu_pressure = false;
    let mut ram_pressure = false;
    let mut ram_critical = false;

    if let Some(gpu) = &snapshot.gpu {
        if gpu.free < snapshot.reserve_mb {
            level = Pressure::Critical;
            gpu_pressure = true;
            reasons.push(format!(
                "VRAM free {} MiB < reserve {} MiB",
                gpu.free, snapshot.reserve_mb
            ));
        } else if gpu.free < snapshot.reserve_mb * 3 / 2 {
            level = level.max(Pressure::Warning);
            gpu_pressure = true;
            reasons.push(format!(
                "VRAM free {} MiB near reserve {} MiB",
                gpu.free, snapshot.reserve_mb
            ));
        }
    }

    let available_mib = snapshot.ram.available / MIB;
    if available_mib < cfg.ram_critical_mib {
        level = Pressure::Critical;
        ram_critical = true;
        ram_pressure = true;
        reasons.push(format!(
            "RAM available {}",
            human_bytes(snapshot.ram.available)
        ));
    } else if available_mib < cfg.ram_warn_mib {
        level = level.max(Pressure::Warning);
        ram_pressure = true;
        reasons.push(format!(
            "low RAM: {} available",
            human_bytes(snapshot.ram.available)
        ));
    }

    if snapshot.swap.total > 0 {
        if snapshot.swap.percent >= cfg.swap_critical_pct {
            level = Pressure::Critical;
            ram_critical = true;
            ram_pressure = true;
            reasons.push(format!("swap {:.0}% used", snapshot.swap.percent));
        } else if snapshot.swap.percent >= cfg.swap_warn_pct {
            level = level.max(Pressure::Warning);
            ram_pressure = true;
            reasons.push(format!("swap {:.0}% used", snapshot.swap.percent));
        }
    }

    if let Some(psi) = snapshot.psi_mem {
        if psi >= cfg.psi_critical {
            level = Pressure::Critical;
            ram_critical = true;
            ram_pressure = true;
            reasons.push(format!("memory stall {psi:.0}% (PSI avg10)"));
        } else if psi >= cfg.psi_warn {
            level = level.max(Pressure::Warning);
            ram_pressure = true;
            reasons.push(format!("memory stall {psi:.0}% (PSI avg10)"));
        }
    }

    if ram_critical || (!gpu_pressure && ram_pressure) {
        trigger = Trigger::Ram;
    }

    if alloc_new {
        level = Pressure::Critical;
        trigger = Trigger::GpuAlloc;
        reasons.push(match snapshot.gpu_alloc_failures {
            Some(count) => format!("{count} GPU allocation failures in 15 min (new)"),
            None => "new GPU allocation failures".to_string(),
        });
    }

    (level, trigger, reasons)
}

impl App {
    /// A unit the guard may act on: a configured model hint or allow entry.
    pub(super) fn is_model(&self, unit: &str) -> bool {
        hints::any(unit, &self.guard.model_hints)
            || self
                .guard
                .cfg
                .allow
                .iter()
                .any(|entry| hints::matches(unit, entry))
    }

    fn verdict(&self, unit: &UnitStat, trigger: Trigger) -> Verdict {
        let cfg = &self.guard.cfg;
        if unit.stopped {
            return Verdict::Stopped;
        }
        if unit.frozen || self.frozen.contains(&unit.unit) {
            return Verdict::Frozen;
        }
        if cfg
            .deny
            .iter()
            .any(|entry| hints::matches(&unit.unit, entry))
        {
            return Verdict::Denied;
        }
        if !self.guard.targets.is_empty() && !self.guard.targets.contains(&unit.unit) {
            return Verdict::NotTargeted;
        }
        if !unit.user && !self.allow_system {
            return Verdict::SystemUnit;
        }
        if let Some(reason) = self.protection(unit) {
            return Verdict::Protected(reason);
        }
        let model_reason = match hints::first_match(&unit.unit, &self.guard.model_hints) {
            Some(hint) => format!("model hint '{hint}'"),
            None => match cfg
                .allow
                .iter()
                .find(|entry| hints::matches(&unit.unit, entry))
            {
                Some(entry) => format!("allow list '{entry}'"),
                None => return Verdict::NotModel,
            },
        };
        let large_enough = match trigger {
            Trigger::Ram => unit.rss >= MIN_RAM_RSS || unit.vram >= MIN_RAM_VRAM_MIB,
            Trigger::Vram | Trigger::GpuAlloc => unit.vram >= MIN_VRAM_MIB,
        };
        if !large_enough {
            return Verdict::BelowThreshold;
        }
        if unit.cpu >= cfg.busy_cpu && !cfg.allow_busy {
            return Verdict::Busy(unit.cpu);
        }
        Verdict::Eligible(model_reason)
    }

    /// Every unit with its verdict for `trigger`, in snapshot order.
    fn explain(&self, trigger: Trigger) -> Vec<(UnitStat, Verdict)> {
        self.snapshot
            .units
            .iter()
            .map(|unit| (unit.clone(), self.verdict(unit, trigger)))
            .collect()
    }

    /// Best candidate: idle units first, then the largest by the trigger's
    /// resource (RAM for a RAM trigger, VRAM otherwise).
    fn guard_candidate(&self, trigger: Trigger) -> Option<UnitStat> {
        let busy_cpu = self.guard.cfg.busy_cpu;
        let mut eligible: Vec<UnitStat> = self
            .explain(trigger)
            .into_iter()
            .filter(|(_, verdict)| verdict.is_eligible())
            .map(|(unit, _)| unit)
            .collect();
        eligible.sort_by_key(|unit| {
            let idle = unit.cpu < busy_cpu;
            let size = match trigger {
                Trigger::Ram => (unit.rss, unit.vram),
                Trigger::Vram | Trigger::GpuAlloc => (unit.vram, unit.rss),
            };
            std::cmp::Reverse((idle, size))
        });
        eligible.into_iter().next()
    }

    /// Short count of why units were skipped, for the "no candidate" notice.
    fn rejection_summary(&self, trigger: Trigger) -> String {
        let (mut busy, mut protected, mut not_model, mut system) = (0, 0, 0, 0);
        for (_, verdict) in self.explain(trigger) {
            match verdict {
                Verdict::Busy(_) => busy += 1,
                Verdict::Protected(_) => protected += 1,
                Verdict::NotModel => not_model += 1,
                Verdict::SystemUnit => system += 1,
                _ => {}
            }
        }
        let mut parts = Vec::new();
        if busy > 0 {
            parts.push(format!("{busy} busy"));
        }
        if protected > 0 {
            parts.push(format!("{protected} protected"));
        }
        if system > 0 {
            parts.push(format!("{system} system (needs --allow-system)"));
        }
        if not_model > 0 {
            parts.push(format!("{not_model} not model units"));
        }
        if parts.is_empty() {
            "no unit is holding enough memory".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Table printed by `--explain-guard`: one line per unit with its verdict.
    pub fn explain_guard_table(&self) -> String {
        let busy_cpu = self.guard.cfg.busy_cpu;
        let mut out = String::new();
        out.push_str(&format!(
            "{:<58} {:>8} {:>7}  VERDICT (VRAM trigger)\n",
            "UNIT", "VRAM", "CPU%"
        ));
        for (unit, verdict) in self.explain(Trigger::Vram) {
            out.push_str(&format!(
                "{:<58} {:>8} {:>6.1}%  {}\n",
                fit_name(&unit.unit, 58),
                format!("{}M", unit.vram),
                unit.cpu,
                verdict.describe(busy_cpu)
            ));
        }
        out
    }

    pub(super) fn evaluate_guard(&mut self, now: Instant) {
        if !self.guard.enabled {
            self.guard.phase = GuardPhase::Idle;
            self.guard.critical_since = None;
            self.guard.calm_since = None;
            return;
        }

        let alloc_new = match self.snapshot.gpu_alloc_failures {
            Some(count) => {
                let new = self
                    .guard
                    .last_alloc
                    .map(|prev| count > prev)
                    .unwrap_or(false);
                self.guard.last_alloc = Some(count);
                new
            }
            None => false,
        };
        let cfg = self.guard.cfg.clone();
        let (level, trigger, reasons) = assess(&self.snapshot, &cfg, alloc_new);

        self.guard.critical_since = match level {
            Pressure::Critical => Some(self.guard.critical_since.unwrap_or(now)),
            _ => None,
        };
        // Any level below critical counts toward recovery: a frozen unit must
        // not stay frozen forever just because the system settled at Warning.
        self.guard.calm_since = match level {
            Pressure::Normal | Pressure::Warning => Some(self.guard.calm_since.unwrap_or(now)),
            Pressure::Critical => None,
        };
        if level == Pressure::Normal {
            self.guard.notified_no_candidate = false;
        }

        let critical_for = self
            .guard
            .critical_since
            .map(|at| now.duration_since(at))
            .unwrap_or_default();
        let normal_for = self
            .guard
            .calm_since
            .map(|at| now.duration_since(at))
            .unwrap_or_default();
        // GPU allocation failures are already visible flicker: no sustain wait.
        let sustained = level == Pressure::Critical
            && (trigger == Trigger::GpuAlloc || critical_for >= cfg.sustain);
        let recovered = level < Pressure::Critical && normal_for >= cfg.recover;
        let cooldown_over = self
            .guard
            .last_freeze
            .map(|at| now.duration_since(at) >= cfg.cooldown)
            .unwrap_or(true);

        match self.guard.phase.clone() {
            GuardPhase::Idle => {
                if sustained && cooldown_over {
                    self.try_freeze(now, level, trigger, &reasons);
                }
            }
            GuardPhase::Handled { .. } => {
                if recovered || (level == Pressure::Critical && cooldown_over) {
                    self.guard.phase = GuardPhase::Idle;
                }
            }
            GuardPhase::Suppressed => {
                if recovered {
                    self.guard.phase = GuardPhase::Idle;
                }
            }
            GuardPhase::Frozen {
                unit,
                system,
                since,
            } => {
                let active = self
                    .snapshot
                    .units
                    .iter()
                    .any(|row| row.unit == unit && !row.stopped);
                if !active {
                    self.guard.phase = GuardPhase::Handled { unit };
                    return;
                }
                if !self.frozen.contains(&unit) {
                    // The user thawed it by hand. Stand down until pressure clears.
                    self.guard.phase = GuardPhase::Suppressed;
                    return;
                }
                if recovered {
                    self.thaw_after_recovery(&unit, system, normal_for);
                    return;
                }
                let clock_start = self
                    .guard
                    .critical_since
                    .map(|at| at.max(since))
                    .unwrap_or(since);
                if level == Pressure::Critical && now.duration_since(clock_start) >= cfg.grace {
                    self.stop_after_grace(&unit, &reasons);
                }
            }
        }
    }

    fn try_freeze(&mut self, now: Instant, level: Pressure, trigger: Trigger, reasons: &[String]) {
        let reason = reasons.join("; ");
        let Some(candidate) = self.guard_candidate(trigger) else {
            if !self.guard.notified_no_candidate {
                self.guard.notified_no_candidate = true;
                let summary = self.rejection_summary(trigger);
                self.notify(
                    Level::Warn,
                    format!("{reason}; no eligible model unit to freeze ({summary})"),
                );
            }
            return;
        };
        let system = !candidate.user;
        match self.actuator.run(Action::Freeze, &candidate.unit, system) {
            Ok(_) => {
                self.frozen.insert(candidate.unit.clone());
                self.apply_frozen_flag(&candidate.unit, true);
                self.guard.phase = GuardPhase::Frozen {
                    unit: candidate.unit.clone(),
                    system,
                    since: now,
                };
                self.guard.last_freeze = Some(now);
                let text = format!(
                    "{reason}. Froze {} ({}); will stop it after {}s if pressure persists. Thaw with t to cancel.",
                    candidate.unit,
                    super::usage_desc(&candidate),
                    self.guard.cfg.grace.as_secs()
                );
                self.notify(
                    if level == Pressure::Critical {
                        Level::Crit
                    } else {
                        Level::Warn
                    },
                    text,
                );
                self.set_status(StatusKind::Info, format!("guard froze {}", candidate.unit));
            }
            Err(error) => {
                self.guard.phase = GuardPhase::Suppressed;
                let text = format!("guard freeze failed for {}: {error}", candidate.unit);
                self.notify(Level::Warn, text.clone());
                self.set_status(StatusKind::Err, text);
            }
        }
    }

    fn thaw_after_recovery(&mut self, unit: &str, system: bool, normal_for: std::time::Duration) {
        match self.actuator.run(Action::Thaw, unit, system) {
            Ok(_) => {
                self.frozen.remove(unit);
                self.apply_frozen_flag(unit, false);
                self.guard.phase = GuardPhase::Idle;
                let text = format!(
                    "pressure below critical for {}s; thawed {unit}",
                    normal_for.as_secs()
                );
                self.notify(Level::Info, text.clone());
                self.set_status(StatusKind::Ok, text);
            }
            Err(error) => {
                self.guard.phase = GuardPhase::Suppressed;
                let text = format!("guard thaw failed for {unit}: {error}");
                self.notify(Level::Crit, text.clone());
                self.set_status(StatusKind::Err, text);
            }
        }
    }

    fn stop_after_grace(&mut self, unit: &str, reasons: &[String]) {
        let Some(row) = self.find_unit(unit).cloned() else {
            // The unit vanished between samples; nothing left to stop.
            self.guard.phase = GuardPhase::Handled {
                unit: unit.to_string(),
            };
            return;
        };
        match self.run_action(Action::Stop, &row) {
            Ok(_) => {
                self.frozen.remove(unit);
                self.apply_frozen_flag(unit, false);
                let mut placeholder = row.clone();
                placeholder.procs = 0;
                placeholder.rss = 0;
                placeholder.vram = 0;
                placeholder.cpu = 0.0;
                placeholder.stopped = true;
                placeholder.frozen = false;
                self.stopped.insert(unit.to_string(), placeholder);
                self.stopped_since.insert(unit.to_string(), Instant::now());
                self.guard.phase = GuardPhase::Handled {
                    unit: unit.to_string(),
                };
                let text = format!("{}. Stopped {unit} to free resources.", reasons.join("; "));
                self.notify(Level::Crit, text);
                self.set_status(StatusKind::Ok, format!("guard stopped {unit}"));
            }
            Err(error) => {
                self.guard.phase = GuardPhase::Suppressed;
                let text = format!("guard stop failed for {unit}: {error}");
                self.notify(Level::Crit, text.clone());
                self.set_status(StatusKind::Err, text);
            }
        }
    }

    /// Stop a unit, thawing it first when the stop is refused because it is
    /// frozen. A failed stop re-freezes the unit so the state is unchanged.
    pub(super) fn run_action(&mut self, action: Action, row: &UnitStat) -> Result<String, String> {
        let system = !row.user;
        match self.actuator.run(action, &row.unit, system) {
            Ok(message) => Ok(message),
            Err(error)
                if matches!(action, Action::Stop | Action::Kill)
                    && error.to_lowercase().contains("frozen") =>
            {
                self.actuator.run(Action::Thaw, &row.unit, system)?;
                match self.actuator.run(action, &row.unit, system) {
                    Ok(message) => Ok(message),
                    Err(error) => {
                        let _ = self.actuator.run(Action::Freeze, &row.unit, system);
                        Err(error)
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn note_success(&mut self, action: Action, row: &UnitStat) {
        let is_model = proc::is_model_unit(&row.unit);
        match action {
            Action::Freeze => {
                self.frozen.insert(row.unit.clone());
                self.apply_frozen_flag(&row.unit, true);
                if is_model {
                    self.guard.phase = GuardPhase::Suppressed;
                }
            }
            Action::Thaw => {
                self.frozen.remove(&row.unit);
                self.apply_frozen_flag(&row.unit, false);
                if is_model {
                    self.guard.phase = GuardPhase::Suppressed;
                }
            }
            Action::Stop | Action::Kill => {
                self.frozen.remove(&row.unit);
                self.apply_frozen_flag(&row.unit, false);
                let mut placeholder = row.clone();
                placeholder.procs = 0;
                placeholder.rss = 0;
                placeholder.vram = 0;
                placeholder.cpu = 0.0;
                placeholder.stopped = true;
                placeholder.frozen = false;
                self.stopped.insert(row.unit.clone(), placeholder);
                self.stopped_since.insert(row.unit.clone(), Instant::now());
                if is_model {
                    self.guard.phase = match &self.guard.phase {
                        GuardPhase::Frozen { unit, .. } if unit == &row.unit => {
                            GuardPhase::Handled {
                                unit: row.unit.clone(),
                            }
                        }
                        _ => GuardPhase::Suppressed,
                    };
                }
            }
            Action::Start => {
                self.frozen.remove(&row.unit);
                self.apply_frozen_flag(&row.unit, false);
                self.stopped.remove(&row.unit);
                self.stopped_since.remove(&row.unit);
                if is_model {
                    self.guard.phase = GuardPhase::Suppressed;
                }
            }
        }
    }
}

fn fit_name(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut short: String = text.chars().take(width.saturating_sub(1)).collect();
    short.push('\u{2026}');
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{Mem, Swap};
    use crate::sysd::Actuator;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    type Calls = Arc<Mutex<Vec<(Action, String)>>>;

    /// Records every action instead of calling systemctl.
    struct Recorder {
        calls: Calls,
        fail_freeze: bool,
    }

    impl Actuator for Recorder {
        fn run(&mut self, action: Action, unit: &str, _system: bool) -> Result<String, String> {
            self.calls.lock().unwrap().push((action, unit.to_string()));
            if self.fail_freeze && action == Action::Freeze {
                return Err("Unit is not loaded".into());
            }
            Ok(String::new())
        }
    }

    fn unit(name: &str, vram: u64, rss: u64, cpu: f64) -> UnitStat {
        UnitStat {
            unit: name.into(),
            display: name.into(),
            origin: name.into(),
            procs: 1,
            rss,
            vram,
            cpu,
            user: true,
            stopped: false,
            frozen: false,
        }
    }

    fn snapshot(free_mib: u64, available: u64, units: Vec<UnitStat>) -> Snapshot {
        Snapshot {
            clock: "00:00:00".into(),
            host: "test".into(),
            cpu_pct: 0.0,
            load: [0.0; 3],
            ncpu: 4,
            ram: Mem {
                total: 16 * proc::GIB,
                used: 0,
                available,
                percent: 0.0,
            },
            swap: Swap::default(),
            zram: None,
            gpu: Some(crate::gpu::Gpu {
                name: "test".into(),
                total: 12288,
                used: 12288 - free_mib,
                free: free_mib,
                util: Some(0),
                temp: Some(40),
            }),
            gpu_unattributed: 0,
            reserve_mb: 2048,
            units,
            procs: Vec::new(),
            warnings: Vec::new(),
            self_unit: "app-test.scope".into(),
            gpu_alloc_failures: Some(0),
            psi_mem: None,
        }
    }

    fn app_with(
        snap: Snapshot,
        cfg: GuardConfig,
        targets: Vec<String>,
        fail_freeze: bool,
    ) -> (App, Calls) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new(snap, 1.0, true, cfg, targets, false);
        app.actuator = Box::new(Recorder {
            calls: calls.clone(),
            fail_freeze,
        });
        (app, calls)
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn low_vram(units: Vec<UnitStat>) -> Snapshot {
        snapshot(1000, 8 * proc::GIB, units)
    }

    fn fast_cfg() -> GuardConfig {
        GuardConfig {
            sustain: Duration::from_secs(3),
            recover: Duration::from_secs(10),
            cooldown: Duration::from_secs(120),
            grace: Duration::from_secs(60),
            ..GuardConfig::default()
        }
    }

    #[test]
    fn pressure_levels_and_triggers() {
        let cfg = GuardConfig::default();
        let (level, trigger, _) = assess(&snapshot(1500, 8 * proc::GIB, vec![]), &cfg, false);
        assert_eq!((level, trigger), (Pressure::Critical, Trigger::Vram));

        let (level, trigger, _) = assess(&snapshot(2500, 8 * proc::GIB, vec![]), &cfg, false);
        assert_eq!((level, trigger), (Pressure::Warning, Trigger::Vram));

        let (level, trigger, _) = assess(&snapshot(8000, proc::GIB, vec![]), &cfg, false);
        assert_eq!((level, trigger), (Pressure::Critical, Trigger::Ram));

        let (level, trigger, _) = assess(&snapshot(8000, 8 * proc::GIB, vec![]), &cfg, true);
        assert_eq!((level, trigger), (Pressure::Critical, Trigger::GpuAlloc));
    }

    #[test]
    fn psi_stall_escalates_even_with_ram_headroom() {
        let cfg = GuardConfig::default();
        let mut snap = snapshot(8000, 8 * proc::GIB, vec![]);
        snap.psi_mem = Some(75.0);
        let (level, trigger, reasons) = assess(&snap, &cfg, false);
        assert_eq!((level, trigger), (Pressure::Critical, Trigger::Ram));
        assert!(reasons.iter().any(|r| r.contains("PSI")));

        snap.psi_mem = Some(25.0);
        let (level, _, _) = assess(&snap, &cfg, false);
        assert_eq!(level, Pressure::Warning);
    }

    #[test]
    fn customized_thresholds_change_the_level() {
        let cfg = GuardConfig {
            ram_critical_mib: 512,
            ram_warn_mib: 512,
            ..GuardConfig::default()
        };
        let snap = snapshot(8000, 2048 * MIB, vec![]);
        assert_eq!(assess(&snap, &cfg, false).0, Pressure::Normal);
        assert_eq!(
            assess(&snap, &GuardConfig::default(), false).0,
            Pressure::Warning
        );
    }

    #[test]
    fn sustain_delays_the_first_freeze() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 2));
        assert!(calls.lock().unwrap().is_empty(), "no action before sustain");
        app.evaluate_guard(at(base, 3));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [(Action::Freeze, "ollama.service".into())]
        );
    }

    #[test]
    fn gpu_allocation_failures_act_without_waiting() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let mut snap = snapshot(8000, 8 * proc::GIB, units);
        let (mut app, calls) = app_with(snap.clone(), fast_cfg(), vec![], false);
        snap.gpu_alloc_failures = Some(3);
        app.snapshot = snap;
        app.evaluate_guard(at(base, 0));
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0].0, Action::Freeze);
    }

    #[test]
    fn brief_spike_below_sustain_does_nothing() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units.clone()), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.snapshot = snapshot(8000, 8 * proc::GIB, units);
        app.evaluate_guard(at(base, 1));
        app.snapshot = low_vram(vec![unit("ollama.service", 5000, 100, 0.0)]);
        app.evaluate_guard(at(base, 2));
        app.snapshot = snapshot(
            8000,
            8 * proc::GIB,
            vec![unit("ollama.service", 5000, 100, 0.0)],
        );
        app.evaluate_guard(at(base, 10));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn freeze_is_undone_only_after_recovery_window() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units.clone()), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3));
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(app.frozen.contains("ollama.service"));

        // Pressure clears but only for 5 s: still frozen (recover is 10 s).
        app.snapshot = snapshot(8000, 8 * proc::GIB, units.clone());
        app.evaluate_guard(at(base, 10));
        app.evaluate_guard(at(base, 15));
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "no thaw inside recover window"
        );

        // Clear for 10 s: thawed.
        app.evaluate_guard(at(base, 21));
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.last().unwrap().0, Action::Thaw);
        assert!(!app.frozen.contains("ollama.service"));
        assert!(matches!(app.guard.phase, GuardPhase::Idle));
    }

    #[test]
    fn frozen_unit_thaws_when_pressure_settles_at_warning() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units.clone()), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3)); // freeze
                                         // VRAM free 2500 MiB is between the reserve (2048) and 1.5x: Warning.
        app.snapshot = snapshot(2500, 8 * proc::GIB, units);
        app.evaluate_guard(at(base, 10));
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "still frozen inside recover"
        );
        app.evaluate_guard(at(base, 20));
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.last().unwrap().0, Action::Thaw);
    }

    #[test]
    fn persistent_pressure_stops_after_grace_measured_from_freeze() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3)); // freeze
        app.evaluate_guard(at(base, 62)); // 59 s after freeze: not yet
        assert_eq!(calls.lock().unwrap().len(), 1);
        app.evaluate_guard(at(base, 64)); // 61 s after freeze: stop
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.last().unwrap().0, Action::Stop);
        assert!(app.stopped.contains_key("ollama.service"));
    }

    #[test]
    fn cooldown_prevents_freezing_a_second_model_immediately() {
        let base = Instant::now();
        let units = vec![
            unit("ollama.service", 5000, 100, 0.0),
            unit("voiceforge.service", 3000, 100, 0.0),
        ];
        let (mut app, calls) = app_with(low_vram(units), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3)); // freeze ollama
        app.evaluate_guard(at(base, 60)); // grace not over, cooldown not over
        app.evaluate_guard(at(base, 64)); // stop ollama, handled
        assert_eq!(calls.lock().unwrap().len(), 2);
        // The stopped unit is gone from the next sample, as systemctl stop implies.
        app.snapshot = low_vram(vec![unit("voiceforge.service", 3000, 100, 0.0)]);
        app.evaluate_guard(at(base, 70)); // cooldown still running
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "cooldown blocks a new freeze"
        );
        app.evaluate_guard(at(base, 130)); // cooldown over: leave Handled
        app.evaluate_guard(at(base, 131)); // next tick: freeze the next model
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.last().unwrap().1, "voiceforge.service");
    }

    #[test]
    fn busy_units_are_skipped_unless_allowed() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 80.0)];
        let (mut app, calls) = app_with(low_vram(units.clone()), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3));
        assert!(
            calls.lock().unwrap().is_empty(),
            "busy unit must not be frozen"
        );
        assert!(matches!(
            app.verdict(&units[0], Trigger::Vram),
            Verdict::Busy(_)
        ));

        let cfg = GuardConfig {
            allow_busy: true,
            ..fast_cfg()
        };
        let (mut app, calls) = app_with(low_vram(units), cfg, vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn idle_units_are_preferred_over_bigger_busy_ones() {
        let cfg = GuardConfig {
            allow_busy: true,
            ..fast_cfg()
        };
        let units = vec![
            unit("ollama.service", 9000, 100, 90.0),
            unit("voiceforge.service", 2000, 100, 0.0),
        ];
        let (app, _) = app_with(low_vram(units), cfg, vec![], false);
        assert_eq!(
            app.guard_candidate(Trigger::Vram).unwrap().unit,
            "voiceforge.service"
        );
    }

    #[test]
    fn deny_list_and_allow_list_shape_eligibility() {
        let cfg = GuardConfig {
            deny: vec!["voiceforge".into()],
            allow: vec!["custom-engine".into()],
            ..fast_cfg()
        };
        let units = vec![
            unit("voiceforge.service", 9000, 100, 0.0),
            unit("custom-engine.service", 4000, 100, 0.0),
            unit("someapp.service", 8000, 100, 0.0),
        ];
        let (app, _) = app_with(low_vram(units.clone()), cfg, vec![], false);
        assert_eq!(app.verdict(&units[0], Trigger::Vram), Verdict::Denied);
        assert_eq!(
            app.verdict(&units[1], Trigger::Vram),
            Verdict::Eligible("allow list 'custom-engine'".into())
        );
        assert_eq!(app.verdict(&units[2], Trigger::Vram), Verdict::NotModel);
        assert_eq!(
            app.guard_candidate(Trigger::Vram).unwrap().unit,
            "custom-engine.service"
        );
    }

    #[test]
    fn protected_units_are_never_candidates_even_with_model_like_names() {
        let units = vec![
            unit("plasma-kwin_wayland.service", 9000, 100, 0.0),
            unit("sway.service", 9000, 100, 0.0),
            unit("gdm.service", 9000, 100, 0.0),
            unit("syslens-guard.service", 9000, 100, 0.0),
            unit("docker.service", 9000, 100, 0.0),
        ];
        let (app, _) = app_with(
            low_vram(units.clone()),
            GuardConfig::default(),
            vec![],
            false,
        );
        for row in &units {
            assert!(
                !app.verdict(row, Trigger::Vram).is_eligible(),
                "{} must be protected",
                row.unit
            );
        }
    }

    #[test]
    fn system_model_units_need_allow_system() {
        let mut row = unit("ollama.service", 5000, 100, 0.0);
        row.user = false;
        let (app, _) = app_with(low_vram(vec![row.clone()]), fast_cfg(), vec![], false);
        assert_eq!(app.verdict(&row, Trigger::Vram), Verdict::SystemUnit);
        assert!(!app.verdict(&row, Trigger::Vram).is_eligible());
    }

    #[test]
    fn already_frozen_units_are_not_candidates() {
        let mut frozen = unit("viitorvoice.service", 5000, 200, 0.0);
        frozen.frozen = true;
        let units = vec![frozen, unit("ollama.service", 3000, 100, 0.0)];
        let (app, _) = app_with(low_vram(units), fast_cfg(), vec![], false);
        assert_eq!(
            app.guard_candidate(Trigger::Vram).unwrap().unit,
            "ollama.service"
        );
    }

    #[test]
    fn targets_restrict_candidates() {
        let units = vec![
            unit("ollama.service", 9000, 100, 0.0),
            unit("voiceforge.service", 2000, 100, 0.0),
        ];
        let (app, _) = app_with(
            low_vram(units),
            fast_cfg(),
            vec!["voiceforge.service".into()],
            false,
        );
        assert_eq!(
            app.guard_candidate(Trigger::Vram).unwrap().unit,
            "voiceforge.service"
        );
    }

    #[test]
    fn failed_freeze_suppresses_the_guard_instead_of_retrying_every_tick() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units), fast_cfg(), vec![], true);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3));
        app.evaluate_guard(at(base, 4));
        app.evaluate_guard(at(base, 5));
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "one attempt, then suppressed"
        );
        assert!(matches!(app.guard.phase, GuardPhase::Suppressed));
    }

    #[test]
    fn disabled_guard_never_acts() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, calls) = app_with(low_vram(units), fast_cfg(), vec![], false);
        app.guard.enabled = false;
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 30));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn explain_table_lists_every_unit_with_a_reason() {
        let units = vec![
            unit("ollama.service", 5000, 100, 0.0),
            unit("plasma-plasmashell.service", 100, 100, 0.0),
        ];
        let (app, _) = app_with(low_vram(units), fast_cfg(), vec![], false);
        let table = app.explain_guard_table();
        assert!(
            table.contains("ollama.service") && table.contains("eligible (model hint 'ollama')")
        );
        assert!(table.contains("protected:"), "{table}");
    }

    #[test]
    fn unit_that_vanishes_while_frozen_is_handled_not_stuck() {
        let base = Instant::now();
        let units = vec![unit("ollama.service", 5000, 100, 0.0)];
        let (mut app, _) = app_with(low_vram(units), fast_cfg(), vec![], false);
        app.evaluate_guard(at(base, 0));
        app.evaluate_guard(at(base, 3));
        app.snapshot = low_vram(vec![]);
        app.evaluate_guard(at(base, 4));
        assert!(matches!(app.guard.phase, GuardPhase::Handled { .. }));
    }
}
