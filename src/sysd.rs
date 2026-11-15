use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::util::one_line;

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
}

fn command_for(system: bool) -> Command {
    if system {
        if unsafe { libc::geteuid() } == 0 {
            Command::new("systemctl")
        } else {
            let mut command = Command::new("sudo");
            command.arg("-n").arg("systemctl");
            command
        }
    } else {
        let mut command = Command::new("systemctl");
        command.arg("--user");
        command
    }
}

pub fn is_frozen(unit: &str, system: bool) -> Option<bool> {
    let output = command_for(system)
        .args(["show", unit, "-p", "FreezerState", "--value"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim() == "frozen")
}

pub fn run_as(action: Action, unit: &str, system: bool) -> Result<String, String> {
    let mut command = command_for(system);
    match action {
        Action::Freeze => {
            command.arg("freeze");
        }
        Action::Thaw => {
            command.arg("thaw");
        }
        Action::Stop => {
            command.arg("stop");
        }
        Action::Start => {
            command.arg("start");
        }
        Action::Kill => {
            command.arg("kill").arg("--signal=SIGKILL");
        }
    }
    command.arg(unit);

    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("systemctl: {error}"))?;

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("systemctl: {error}")),
        }
        if started.elapsed() >= Duration::from_secs(8) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "systemctl {} timed out",
                action.label().to_lowercase()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    if status.success() {
        Ok(format!("{} {}", action.verb(), unit))
    } else {
        let message = one_line(&String::from_utf8_lossy(&stderr));
        if message.is_empty() {
            Err(format!("systemctl failed ({status})"))
        } else {
            Err(message)
        }
    }
}
