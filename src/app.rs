use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::GuardConfig;
use crate::proc::{self, Level, Snapshot, UnitStat};
use crate::sysd::{self, Action, Actuator};
use crate::util::{self, human_bytes};

mod guard;
use guard::{Guard, GuardPhase};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Ok,
    Err,
}

#[derive(Clone, Debug)]
pub enum Modal {
    None,
    Menu {
        unit: String,
        index: usize,
    },
    Confirm {
        action: Action,
        unit: String,
    },
    ConfirmAll {
        action: Action,
        units: Vec<String>,
    },
    Processes {
        unit: String,
        index: usize,
        offset: usize,
    },
    Emergency {
        index: usize,
    },
    Help,
}

pub enum KeyResult {
    Continue,
    Refresh,
    Quit,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub level: Level,
    pub until: Instant,
}

pub struct App {
    pub snapshot: Snapshot,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub offset: usize,
    pub interval: f64,
    pub show_all: bool,
    pub modal: Modal,
    pub status: String,
    pub status_kind: StatusKind,
    pub stopped: HashMap<String, UnitStat>,
    stopped_since: HashMap<String, Instant>,
    pub frozen: HashSet<String>,
    pub toast: Option<Toast>,
    pub headless: bool,
    allow_system: bool,
    guard: Guard,
    actuator: Box<dyn Actuator>,
}

impl App {
    pub fn new(
        snapshot: Snapshot,
        interval: f64,
        guard_enabled: bool,
        guard_cfg: GuardConfig,
        guard_targets: Vec<String>,
        allow_system: bool,
    ) -> Self {
        let last_alloc = snapshot.gpu_alloc_failures;
        let mut app = Self {
            snapshot,
            filtered: Vec::new(),
            selected: 0,
            offset: 0,
            interval,
            show_all: false,
            modal: Modal::None,
            status: String::new(),
            status_kind: StatusKind::Info,
            stopped: HashMap::new(),
            stopped_since: HashMap::new(),
            frozen: HashSet::new(),
            toast: None,
            allow_system,
            headless: false,
            guard: Guard::new(guard_enabled, guard_cfg, guard_targets, last_alloc),
            actuator: Box::new(sysd::Systemctl),
        };
        app.rebuild_filtered(None);
        app
    }

    pub fn set_snapshot(&mut self, mut snapshot: Snapshot) {
        let keep = self.selected_unit().map(|unit| unit.unit.clone());
        let active: HashSet<String> = snapshot
            .units
            .iter()
            .map(|unit| unit.unit.clone())
            .collect();
        let now = Instant::now();
        let mut restarted = Vec::new();
        for unit in self.stopped.keys() {
            if !active.contains(unit) {
                continue;
            }
            let drained = self
                .stopped_since
                .get(unit)
                .map(|at| now.duration_since(*at) >= STOP_DRAIN_GRACE)
                .unwrap_or(true);
            if drained {
                restarted.push(unit.clone());
            }
        }
        for unit in restarted {
            self.stopped.remove(&unit);
            self.stopped_since.remove(&unit);
        }
        for (unit, placeholder) in &self.stopped {
            if !active.contains(unit) {
                snapshot.units.push(placeholder.clone());
            }
        }
        for unit in &mut snapshot.units {
            if self.frozen.contains(&unit.unit) {
                unit.frozen = true;
            }
        }
        snapshot
            .units
            .sort_by_key(|unit| std::cmp::Reverse((unit.vram, unit.rss)));
        self.snapshot = snapshot;
        self.rebuild_filtered(keep.as_deref());
        let process_clamp = if let Modal::Processes {
            unit,
            index,
            offset,
        } = &self.modal
        {
            Some((unit.clone(), *index, *offset))
        } else {
            None
        };
        if let Some((unit, mut index, mut offset)) = process_clamp {
            let count = self.process_count(&unit);
            if count == 0 {
                index = 0;
                offset = 0;
            } else {
                index = index.min(count - 1);
                if index < offset {
                    offset = index;
                } else if index >= offset + PROCESS_PAGE {
                    offset = index + 1 - PROCESS_PAGE;
                }
            }
            self.modal = Modal::Processes {
                unit,
                index,
                offset,
            };
        }
        if self
            .toast
            .as_ref()
            .map(|toast| toast.until <= Instant::now())
            .unwrap_or(false)
        {
            self.toast = None;
        }
        self.evaluate_guard(now);
    }

    pub fn selected_unit(&self) -> Option<&UnitStat> {
        self.filtered
            .get(self.selected)
            .and_then(|&index| self.snapshot.units.get(index))
    }

    pub fn find_unit(&self, unit: &str) -> Option<&UnitStat> {
        self.snapshot.units.iter().find(|row| row.unit == unit)
    }

    pub fn visible_units(&self, count: usize) -> Vec<&UnitStat> {
        self.filtered
            .iter()
            .skip(self.offset)
            .take(count)
            .filter_map(|&index| self.snapshot.units.get(index))
            .collect()
    }

    pub fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 {
            self.offset = 0;
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
        if self.offset + visible > self.filtered.len() {
            self.offset = self.filtered.len().saturating_sub(visible);
        }
    }

    pub fn guard_label(&self) -> String {
        if !self.guard.enabled {
            return "guard off".to_string();
        }
        match &self.guard.phase {
            GuardPhase::Idle => "guard on".to_string(),
            GuardPhase::Frozen { unit, since, .. } => {
                let frozen_for = since.elapsed().as_secs();
                let name = short_name(unit);
                match self.guard.critical_since {
                    Some(critical) => {
                        let left = self
                            .guard
                            .cfg
                            .grace
                            .saturating_sub(critical.elapsed())
                            .as_secs();
                        format!("GUARD {name} frozen {frozen_for}s · stop {left}s")
                    }
                    None => format!("GUARD {name} frozen {frozen_for}s"),
                }
            }
            GuardPhase::Handled { unit } => format!("GUARD stopped {}", short_name(unit)),
            GuardPhase::Suppressed => "guard paused".to_string(),
        }
    }

    pub fn emergency_labels(&self) -> Vec<String> {
        let stop_count = self.model_units(true).len();
        let freeze_count = self.model_units(false).len();
        vec![
            "Stop largest model holder".to_string(),
            "Freeze largest model holder".to_string(),
            "Force kill largest model holder".to_string(),
            format!("Thaw all frozen units ({})", self.frozen.len()),
            format!("Stop all model units ({stop_count})"),
            format!("Freeze all model units ({freeze_count})"),
            if self.guard.enabled {
                "Disable guard for this session".to_string()
            } else {
                "Enable guard".to_string()
            },
            "Close".to_string(),
        ]
    }

    fn rebuild_filtered(&mut self, keep: Option<&str>) {
        let show_all = self.show_all;
        let self_unit = self.snapshot.self_unit.clone();
        self.filtered = self
            .snapshot
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| show_all || proc::is_small(unit, &self_unit))
            .map(|(index, _)| index)
            .collect();
        if let Some(name) = keep {
            if let Some(position) = self
                .filtered
                .iter()
                .position(|&index| self.snapshot.units[index].unit == name)
            {
                self.selected = position;
            }
        }
        if self.filtered.is_empty() {
            self.selected = 0;
            self.offset = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }

    fn process_count(&self, unit: &str) -> usize {
        self.snapshot
            .procs
            .iter()
            .filter(|proc_row| proc_row.unit == unit)
            .count()
    }

    fn open_processes(&mut self) -> KeyResult {
        let unit = match self.selected_unit() {
            Some(unit) => unit,
            None => {
                self.set_status(StatusKind::Info, "no unit selected");
                return KeyResult::Continue;
            }
        };
        self.modal = Modal::Processes {
            unit: unit.unit.clone(),
            index: 0,
            offset: 0,
        };
        KeyResult::Continue
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let last = self.filtered.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        let message = text.into();
        if self.headless {
            eprintln!("syslens guard: {message}");
        }
        self.status = format!("{} {message}", util::clock_now());
        self.status_kind = kind;
    }

    pub fn reconcile_frozen(&mut self) {
        let rows: Vec<UnitStat> = self
            .snapshot
            .units
            .iter()
            .filter(|unit| self.is_model(&unit.unit))
            .filter(|unit| unit.user || self.allow_system)
            .cloned()
            .collect();
        let mut found = false;
        for row in rows {
            if sysd::is_frozen(&row.unit, !row.user) == Some(true) {
                self.frozen.insert(row.unit.clone());
                self.apply_frozen_flag(&row.unit, true);
                found = true;
            }
        }
        if found {
            self.guard.phase = GuardPhase::Suppressed;
            if self.headless {
                eprintln!(
                    "syslens guard: found frozen model unit(s); pausing auto-guard until pressure clears"
                );
            }
        }
    }

    fn protection(&self, unit: &UnitStat) -> Option<&'static str> {
        proc::protect_reason_ex(unit, &self.snapshot.self_unit, self.allow_system)
    }

    fn apply_frozen_flag(&mut self, unit: &str, frozen: bool) {
        for row in &mut self.snapshot.units {
            if row.unit == unit {
                row.frozen = frozen;
            }
        }
    }

    fn notify(&mut self, level: Level, message: String) {
        self.toast = Some(Toast {
            text: message.clone(),
            level,
            until: Instant::now() + Duration::from_secs(10),
        });
        if !self.headless {
            let _ = std::io::stdout().write_all(b"\x07");
            let _ = std::io::stdout().flush();
        }

        desktop_notify(level, message);
    }

    fn model_units(&self, include_frozen: bool) -> Vec<UnitStat> {
        self.snapshot
            .units
            .iter()
            .filter(|unit| {
                (unit.user || self.allow_system) && self.is_model(&unit.unit) && !unit.stopped
            })
            .filter(|unit| include_frozen || !unit.frozen)
            .filter(|unit| self.protection(unit).is_none())
            .cloned()
            .collect()
    }

    fn largest_model_holder(&self) -> Option<UnitStat> {
        self.snapshot
            .units
            .iter()
            .filter(|unit| {
                (unit.user || self.allow_system) && self.is_model(&unit.unit) && !unit.stopped
            })
            .filter(|unit| self.protection(unit).is_none())
            .max_by_key(|unit| (unit.vram, unit.rss))
            .cloned()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> KeyResult {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return KeyResult::Quit;
        }
        if matches!(self.modal, Modal::None) {
            self.on_main_key(key)
        } else {
            self.on_modal_key(key)
        }
    }

    fn on_main_key(&mut self, key: KeyEvent) -> KeyResult {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => KeyResult::Quit,
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.modal = Modal::Help;
                KeyResult::Continue
            }
            KeyCode::Char('!') => {
                self.modal = Modal::Emergency { index: 0 };
                KeyResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                KeyResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                KeyResult::Continue
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                KeyResult::Continue
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                KeyResult::Continue
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                KeyResult::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.filtered.len().saturating_sub(1);
                KeyResult::Continue
            }
            KeyCode::Enter | KeyCode::Char('m') => {
                self.open_menu();
                KeyResult::Continue
            }
            KeyCode::Char('p') => self.open_processes(),
            KeyCode::Char('s') => self.request(Action::Stop),
            KeyCode::Char('r') => self.request(Action::Start),
            KeyCode::Char('f') => self.request(Action::Freeze),
            KeyCode::Char('t') => self.request(Action::Thaw),
            KeyCode::Char('K') => self.request(Action::Kill),
            KeyCode::Char('x') => {
                let keep = self.selected_unit().map(|unit| unit.unit.clone());
                self.show_all = !self.show_all;
                self.rebuild_filtered(keep.as_deref());
                self.set_status(
                    StatusKind::Info,
                    if self.show_all {
                        "showing all units"
                    } else {
                        "hiding idle units"
                    },
                );
                KeyResult::Continue
            }
            KeyCode::Esc => {
                self.status.clear();
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) -> KeyResult {
        let modal = std::mem::replace(&mut self.modal, Modal::None);
        match modal {
            Modal::None => KeyResult::Continue,
            Modal::Help => {
                if !matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.modal = Modal::Help;
                }
                KeyResult::Continue
            }
            Modal::Menu { unit, index } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => KeyResult::Continue,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.modal = Modal::Menu {
                        unit,
                        index: (index + 1).min(Action::ALL.len() - 1),
                    };
                    KeyResult::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.modal = Modal::Menu {
                        unit,
                        index: index.saturating_sub(1),
                    };
                    KeyResult::Continue
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let action = Action::ALL[index];
                    if action.destructive() {
                        self.modal = Modal::Confirm { action, unit };
                        KeyResult::Continue
                    } else {
                        self.perform(action, &unit)
                    }
                }
                _ => {
                    self.modal = Modal::Menu { unit, index };
                    KeyResult::Continue
                }
            },
            Modal::Confirm { action, unit } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.perform(action, &unit)
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                    self.set_status(StatusKind::Info, "cancelled");
                    KeyResult::Continue
                }
                _ => {
                    self.modal = Modal::Confirm { action, unit };
                    KeyResult::Continue
                }
            },
            Modal::ConfirmAll { action, units } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.perform_many(action, units)
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                    self.set_status(StatusKind::Info, "cancelled");
                    KeyResult::Continue
                }
                _ => {
                    self.modal = Modal::ConfirmAll { action, units };
                    KeyResult::Continue
                }
            },
            Modal::Processes {
                unit,
                index,
                offset,
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => KeyResult::Continue,
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = self.process_count(&unit);
                    let index = if count == 0 {
                        0
                    } else {
                        (index + 1).min(count - 1)
                    };
                    let offset = if index < offset {
                        index
                    } else if index >= offset + PROCESS_PAGE {
                        index + 1 - PROCESS_PAGE
                    } else {
                        offset
                    };
                    self.modal = Modal::Processes {
                        unit,
                        index,
                        offset,
                    };
                    KeyResult::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let index = index.saturating_sub(1);
                    let offset = offset.min(index);
                    self.modal = Modal::Processes {
                        unit,
                        index,
                        offset,
                    };
                    KeyResult::Continue
                }
                KeyCode::PageDown => {
                    let count = self.process_count(&unit);
                    let index = if count == 0 {
                        0
                    } else {
                        (index + PROCESS_PAGE).min(count - 1)
                    };
                    let offset = if index >= offset + PROCESS_PAGE {
                        index + 1 - PROCESS_PAGE
                    } else {
                        offset
                    };
                    self.modal = Modal::Processes {
                        unit,
                        index,
                        offset,
                    };
                    KeyResult::Continue
                }
                KeyCode::PageUp => {
                    let index = index.saturating_sub(PROCESS_PAGE);
                    let offset = offset.saturating_sub(PROCESS_PAGE);
                    self.modal = Modal::Processes {
                        unit,
                        index,
                        offset,
                    };
                    KeyResult::Continue
                }
                KeyCode::Home => {
                    self.modal = Modal::Processes {
                        unit,
                        index: 0,
                        offset: 0,
                    };
                    KeyResult::Continue
                }
                KeyCode::End => {
                    let count = self.process_count(&unit);
                    let index = count.saturating_sub(1);
                    let offset = index.saturating_sub(PROCESS_PAGE - 1);
                    self.modal = Modal::Processes {
                        unit,
                        index,
                        offset,
                    };
                    KeyResult::Continue
                }
                _ => {
                    self.modal = Modal::Processes {
                        unit,
                        index,
                        offset,
                    };
                    KeyResult::Continue
                }
            },
            Modal::Emergency { index } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => KeyResult::Continue,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.modal = Modal::Emergency {
                        index: (index + 1).min(7),
                    };
                    KeyResult::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.modal = Modal::Emergency {
                        index: index.saturating_sub(1),
                    };
                    KeyResult::Continue
                }
                KeyCode::Enter | KeyCode::Char(' ') => self.run_emergency(index),
                KeyCode::Char(character) if ('1'..='8').contains(&character) => {
                    self.run_emergency(character as usize - '1' as usize)
                }
                _ => {
                    self.modal = Modal::Emergency { index };
                    KeyResult::Continue
                }
            },
        }
    }

    fn run_emergency(&mut self, index: usize) -> KeyResult {
        match index {
            0 => {
                if let Some(unit) = self.largest_model_holder() {
                    self.modal = Modal::Confirm {
                        action: Action::Stop,
                        unit: unit.unit,
                    };
                } else {
                    self.set_status(StatusKind::Info, "no model unit to stop");
                }
                KeyResult::Continue
            }
            1 => {
                if let Some(unit) = self.largest_model_holder() {
                    self.perform(Action::Freeze, &unit.unit)
                } else {
                    self.set_status(StatusKind::Info, "no model unit to freeze");
                    KeyResult::Continue
                }
            }
            2 => {
                if let Some(unit) = self.largest_model_holder() {
                    self.modal = Modal::Confirm {
                        action: Action::Kill,
                        unit: unit.unit,
                    };
                } else {
                    self.set_status(StatusKind::Info, "no model unit to kill");
                }
                KeyResult::Continue
            }
            3 => self.thaw_all(),
            4 => {
                let units = self.model_units(true);
                if units.is_empty() {
                    self.set_status(StatusKind::Info, "no model units to stop");
                    KeyResult::Continue
                } else {
                    self.modal = Modal::ConfirmAll {
                        action: Action::Stop,
                        units: units.into_iter().map(|unit| unit.unit).collect(),
                    };
                    KeyResult::Continue
                }
            }
            5 => self.freeze_all(),
            6 => {
                self.guard.enabled = !self.guard.enabled;
                self.guard.reset();
                self.set_status(
                    StatusKind::Info,
                    if self.guard.enabled {
                        "guard enabled"
                    } else {
                        "guard disabled for this session"
                    },
                );
                KeyResult::Continue
            }
            _ => KeyResult::Continue,
        }
    }

    fn thaw_all(&mut self) -> KeyResult {
        let units: Vec<String> = self.frozen.iter().cloned().collect();
        if units.is_empty() {
            self.set_status(StatusKind::Info, "no guard-frozen units");
            return KeyResult::Continue;
        }
        let mut thawed = 0;
        let mut failures = 0;
        for unit in units {
            let system = self.find_unit(&unit).map(|row| !row.user).unwrap_or(false);
            match sysd::run_as(Action::Thaw, &unit, system) {
                Ok(_) => {
                    self.frozen.remove(&unit);
                    self.apply_frozen_flag(&unit, false);
                    thawed += 1;
                }
                Err(_) => failures += 1,
            }
        }
        self.guard.phase = GuardPhase::Suppressed;
        self.set_status(
            if failures == 0 {
                StatusKind::Ok
            } else {
                StatusKind::Err
            },
            format!("thawed {thawed} unit(s), {failures} failed"),
        );
        KeyResult::Refresh
    }

    fn freeze_all(&mut self) -> KeyResult {
        let units = self.model_units(false);
        if units.is_empty() {
            self.set_status(StatusKind::Info, "no model units to freeze");
            return KeyResult::Continue;
        }
        let mut froze = 0;
        let mut failures = 0;
        for unit in units {
            match sysd::run_as(Action::Freeze, &unit.unit, !unit.user) {
                Ok(_) => {
                    self.frozen.insert(unit.unit.clone());
                    self.apply_frozen_flag(&unit.unit, true);
                    froze += 1;
                }
                Err(_) => failures += 1,
            }
        }
        self.guard.phase = GuardPhase::Suppressed;
        self.set_status(
            if failures == 0 {
                StatusKind::Ok
            } else {
                StatusKind::Err
            },
            format!("froze {froze} model unit(s), {failures} failed"),
        );
        KeyResult::Refresh
    }

    fn open_menu(&mut self) {
        let unit = match self.selected_unit() {
            Some(unit) => unit,
            None => {
                self.set_status(StatusKind::Info, "no unit selected");
                return;
            }
        };
        if let Some(reason) = self.protection(unit) {
            let message = format!("{}: protected ({reason})", unit.unit);
            self.set_status(StatusKind::Err, message);
            return;
        }
        self.modal = Modal::Menu {
            unit: unit.unit.clone(),
            index: 0,
        };
    }

    fn request(&mut self, action: Action) -> KeyResult {
        let unit = match self.selected_unit() {
            Some(unit) => unit,
            None => {
                self.set_status(StatusKind::Info, "no unit selected");
                return KeyResult::Continue;
            }
        };
        if let Some(reason) = self.protection(unit) {
            let message = format!("{}: protected ({reason})", unit.unit);
            self.set_status(StatusKind::Err, message);
            return KeyResult::Continue;
        }
        if unit.stopped && matches!(action, Action::Stop | Action::Kill) {
            let message = format!("{} is already stopped", unit.unit);
            self.set_status(StatusKind::Info, message);
            return KeyResult::Continue;
        }
        let unit_name = unit.unit.clone();
        if action.destructive() {
            self.modal = Modal::Confirm {
                action,
                unit: unit_name,
            };
            KeyResult::Continue
        } else {
            self.perform(action, &unit_name)
        }
    }

    fn perform(&mut self, action: Action, unit_name: &str) -> KeyResult {
        let row = match self.find_unit(unit_name) {
            Some(row) => row.clone(),
            None => {
                let message = format!("{unit_name} is no longer listed");
                self.set_status(StatusKind::Err, message);
                return KeyResult::Refresh;
            }
        };
        if let Some(reason) = self.protection(&row) {
            let message = format!("{}: protected ({reason})", row.unit);
            self.set_status(StatusKind::Err, message);
            return KeyResult::Continue;
        }

        match self.run_action(action, &row) {
            Ok(message) => {
                self.note_success(action, &row);
                self.set_status(StatusKind::Ok, message);
                KeyResult::Refresh
            }
            Err(message) => {
                self.set_status(StatusKind::Err, message);
                KeyResult::Continue
            }
        }
    }

    fn perform_many(&mut self, action: Action, units: Vec<String>) -> KeyResult {
        let mut ok = 0;
        let mut failures = Vec::new();
        for unit in units {
            let row = match self.find_unit(&unit).cloned() {
                Some(row) => row,
                None => continue,
            };
            if let Some(reason) = self.protection(&row) {
                failures.push(format!("{}: {reason}", row.unit));
                continue;
            }
            match self.run_action(action, &row) {
                Ok(_) => {
                    self.note_success(action, &row);
                    ok += 1;
                }
                Err(error) => failures.push(format!("{}: {error}", row.unit)),
            }
        }
        let verb = match action {
            Action::Stop => "stopped",
            Action::Kill => "killed",
            Action::Freeze => "froze",
            Action::Thaw => "thawed",
            Action::Start => "started",
        };
        if failures.is_empty() {
            self.set_status(StatusKind::Ok, format!("{verb} {ok} unit(s)"));
        } else {
            self.set_status(
                StatusKind::Err,
                format!(
                    "{verb} {ok} unit(s), {} failed: {}",
                    failures.len(),
                    failures.join("; ")
                ),
            );
        }
        KeyResult::Refresh
    }
}

fn desktop_notify(level: Level, message: String) {
    std::thread::spawn(move || {
        let (title, urgency, urgency_byte) = match level {
            Level::Crit => ("SysLens: critical", "critical", 2),
            Level::Warn => ("SysLens: warning", "normal", 1),
            _ => ("SysLens", "low", 0),
        };
        let hints = format!("{{'urgency': <byte {urgency_byte}>}}");
        let sent = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.freedesktop.Notifications",
                "--object-path",
                "/org/freedesktop/Notifications",
                "--method",
                "org.freedesktop.Notifications.Notify",
                "SysLens",
                "0",
                "",
                title,
                message.as_str(),
                "[]",
                hints.as_str(),
                "10000",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !sent {
            let _ = Command::new("notify-send")
                .args(["-a", "SysLens", "-u", urgency, title, message.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    });
}

const PROCESS_PAGE: usize = 12;
const STOP_DRAIN_GRACE: Duration = Duration::from_secs(5);

fn short_name(unit: &str) -> String {
    let stripped = unit
        .strip_suffix(".service")
        .or_else(|| unit.strip_suffix(".scope"))
        .or_else(|| unit.strip_suffix(".slice"))
        .unwrap_or(unit);
    if stripped.chars().count() > 18 {
        let mut short: String = stripped.chars().take(17).collect();
        short.push('…');
        short
    } else {
        stripped.to_string()
    }
}

fn usage_desc(unit: &UnitStat) -> String {
    if unit.vram > 0 {
        format!("{} MiB VRAM", unit.vram)
    } else {
        format!("{} RSS", human_bytes(unit.rss))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GuardConfig;
    use crate::gpu::Gpu;
    use crate::proc::{Mem, Swap, GIB};

    fn unit(name: &str, vram: u64, rss: u64) -> UnitStat {
        UnitStat {
            unit: name.to_string(),
            display: name.to_string(),
            origin: name.to_string(),
            procs: 1,
            rss,
            vram,
            cpu: 0.0,
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
            ncpu: 8,
            ram: Mem {
                total: 16 * GIB,
                used: 16 * GIB - available,
                available,
                percent: 0.0,
            },
            swap: Swap {
                total: 8 * GIB,
                used: 0,
                percent: 0.0,
            },
            zram: None,
            gpu: Some(Gpu {
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

    fn app(snap: Snapshot, guard: bool) -> App {
        App::new(snap, 1.0, guard, GuardConfig::default(), vec![], false)
    }

    #[test]
    fn stopped_placeholder_survives_drain_but_drops_after_external_restart() {
        let row = unit("ollama.service", 0, 0);
        let mut app = app(snapshot(8000, 8 * GIB, vec![]), false);
        app.stopped.insert(row.unit.clone(), row.clone());
        app.stopped_since.insert(row.unit.clone(), Instant::now());
        app.set_snapshot(snapshot(8000, 8 * GIB, vec![row.clone()]));
        assert!(
            app.stopped.contains_key("ollama.service"),
            "placeholder should survive the drain window"
        );

        app.stopped_since.insert(
            row.unit.clone(),
            Instant::now() - STOP_DRAIN_GRACE - Duration::from_secs(1),
        );
        app.set_snapshot(snapshot(8000, 8 * GIB, vec![row.clone()]));
        assert!(
            !app.stopped.contains_key("ollama.service"),
            "an externally restarted unit must not resurrect as STOPPED"
        );
    }

    #[test]
    fn stopped_placeholder_is_injected_when_unit_is_gone() {
        let mut row = unit("ollama.service", 0, 0);
        row.stopped = true;
        let mut app = app(snapshot(8000, 8 * GIB, vec![]), false);
        app.stopped.insert(row.unit.clone(), row.clone());
        app.stopped_since.insert(row.unit.clone(), Instant::now());
        app.set_snapshot(snapshot(8000, 8 * GIB, vec![]));
        assert!(app.find_unit("ollama.service").is_some());
        assert!(app.find_unit("ollama.service").unwrap().stopped);
    }
}
