use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::util::one_line;

const ACTION_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_UNIT_LEN: usize = 255;
const UNIT_SUFFIXES: [&str; 3] = [".service", ".scope", ".slice"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Freeze,
    Thaw,
    Stop,
    Start,
    Kill,
}

impl Action {
    pub const ALL: [Action; 5] = [
        Action::Freeze,
        Action::Thaw,
        Action::Stop,
        Action::Start,
        Action::Kill,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Freeze => "Freeze",
            Action::Thaw => "Thaw",
            Action::Stop => "Stop",
            Action::Start => "Start",
            Action::Kill => "Force kill",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Action::Freeze => "pause processes, keep memory",
            Action::Thaw => "resume a frozen unit",
            Action::Stop => "suspend, frees RAM/VRAM",
            Action::Start => "resume a stopped unit",
            Action::Kill => "SIGKILL all processes",
        }
    }

    pub fn destructive(self) -> bool {
        matches!(self, Action::Stop | Action::Kill)
    }

    fn verb(self) -> &'static str {
        match self {
            Action::Freeze => "froze",
            Action::Thaw => "thawed",
            Action::Stop => "stopped",
            Action::Start => "started",
            Action::Kill => "killed",
        }
    }

    /// systemctl arguments for this action, before the unit name.
    fn systemctl_args(self) -> &'static [&'static str] {
        match self {
            Action::Freeze => &["freeze"],
            Action::Thaw => &["thaw"],
            Action::Stop => &["stop"],
            Action::Start => &["start"],
            Action::Kill => &["kill", "--signal=SIGKILL"],
        }
    }
}

/// Reject anything that is not a plausible systemd unit name before it reaches
/// a process argument list. Allowed characters follow systemd's unit-name
/// grammar (escapes such as `\x2d` use a backslash), and the unit must carry a
/// suffix we act on, so placeholders like `kernel` or `-` are refused too.
pub fn validate_unit(unit: &str) -> Result<(), String> {
    if unit.is_empty() || unit.len() > MAX_UNIT_LEN {
        return Err(format!("refusing unit name of length {}", unit.len()));
    }
    if unit.starts_with('-') {
        return Err(format!("refusing unit name that starts with '-': {unit}"));
    }
    let allowed = unit.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'@' | b'\\')
    });
    if !allowed {
        return Err(format!(
            "refusing unit name with unsafe characters: {unit:?}"
        ));
    }
    if !UNIT_SUFFIXES.iter().any(|suffix| unit.ends_with(suffix)) {
        return Err(format!("refusing non-unit name: {unit}"));
    }
    Ok(())
}

/// A fully built command line. Kept as data so the argument order is testable
/// without spawning anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// Build the command for `systemctl <args> -- <unit>`.
///
/// - User units: `systemctl --user <args> -- <unit>`.
/// - System units as root: `systemctl <args> -- <unit>`.
/// - System units as a normal user: `sudo -n -- systemctl <args> -- <unit>`.
///
/// `--` always precedes the unit, so a name can never be parsed as an option.
pub fn invocation(
    args: &[&str],
    unit: &str,
    system: bool,
    is_root: bool,
) -> Result<Invocation, String> {
    validate_unit(unit)?;
    let mut argv: Vec<String> = Vec::new();
    let program = if system && !is_root {
        argv.extend(["-n", "--", "systemctl"].map(String::from));
        "sudo"
    } else {
        if !system {
            argv.push("--user".to_string());
        }
        "systemctl"
    };
    argv.extend(args.iter().map(|arg| arg.to_string()));
    argv.push("--".to_string());
    argv.push(unit.to_string());
    Ok(Invocation {
        program,
        args: argv,
    })
}

fn current_user_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn spawn_and_wait(invocation: &Invocation, label: &str) -> Result<String, String> {
    let mut child = Command::new(invocation.program)
        .args(&invocation.args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{}: {error}", invocation.program))?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("{}: {error}", invocation.program)),
        }
        if started.elapsed() >= ACTION_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("systemctl {label} timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    if status.success() {
        Ok(String::new())
    } else {
        let message = one_line(&String::from_utf8_lossy(&stderr));
        if message.is_empty() {
            Err(format!("systemctl failed ({status})"))
        } else {
            Err(message)
        }
    }
}

pub fn is_frozen(unit: &str, system: bool) -> Option<bool> {
    let inv = invocation(
        &["show", "-p", "FreezerState", "--value"],
        unit,
        system,
        current_user_is_root(),
    )
    .ok()?;
    let output = Command::new(inv.program)
        .args(&inv.args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim() == "frozen")
}

pub fn run_as(action: Action, unit: &str, system: bool) -> Result<String, String> {
    let inv = invocation(
        action.systemctl_args(),
        unit,
        system,
        current_user_is_root(),
    )?;
    spawn_and_wait(&inv, &action.label().to_lowercase())?;
    Ok(format!("{} {}", action.verb(), unit))
}

/// Seam between the guard and the real service manager. The app holds one of
/// these so guard transitions can be tested with a recording fake.
pub trait Actuator {
    fn run(&mut self, action: Action, unit: &str, system: bool) -> Result<String, String>;
}

pub struct Systemctl;

impl Actuator for Systemctl {
    fn run(&mut self, action: Action, unit: &str, system: bool) -> Result<String, String> {
        run_as(action, unit, system)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_unit_uses_user_manager_and_separator() {
        let inv = invocation(&["stop"], "ollama.service", false, false).unwrap();
        assert_eq!(inv.program, "systemctl");
        assert_eq!(inv.args, ["--user", "stop", "--", "ollama.service"]);
    }

    #[test]
    fn system_unit_as_root_has_no_user_flag() {
        let inv = invocation(&["freeze"], "ollama.service", true, true).unwrap();
        assert_eq!(inv.program, "systemctl");
        assert_eq!(inv.args, ["freeze", "--", "ollama.service"]);
    }

    #[test]
    fn system_unit_as_user_goes_through_non_interactive_sudo() {
        let inv = invocation(&["kill", "--signal=SIGKILL"], "ollama.service", true, false).unwrap();
        assert_eq!(inv.program, "sudo");
        assert_eq!(
            inv.args,
            [
                "-n",
                "--",
                "systemctl",
                "kill",
                "--signal=SIGKILL",
                "--",
                "ollama.service"
            ]
        );
    }

    #[test]
    fn option_like_and_placeholder_names_are_refused() {
        assert!(invocation(&["stop"], "--now", false, false).is_err());
        assert!(invocation(&["stop"], "-", false, false).is_err());
        assert!(invocation(&["stop"], "kernel", false, false).is_err());
        assert!(invocation(&["stop"], "", false, false).is_err());
        assert!(invocation(&["stop"], "a b.service", false, false).is_err());
        assert!(invocation(&["stop"], "x;rm -rf.service", false, false).is_err());
        assert!(invocation(&["stop"], "../etc.service", false, false).is_err());
    }

    #[test]
    fn escaped_and_scoped_names_are_accepted() {
        assert!(validate_unit("app-org\\x2ekde\\x2ekonsole-42.scope").is_ok());
        assert!(validate_unit("user@1000.service").is_ok());
        assert!(validate_unit("app.slice").is_ok());
        assert!(validate_unit("docker-0123abcd.scope").is_ok());
    }

    #[test]
    fn overlong_name_is_refused() {
        let long = format!("{}.service", "a".repeat(260));
        assert!(validate_unit(&long).is_err());
    }
}
