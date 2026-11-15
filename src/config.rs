//! Guard configuration.
//!
//! The file is plain `key = value` lines. `#` starts a comment, list values are
//! comma separated, and durations are in seconds. Unknown keys and malformed
//! values are errors that name the line, because a silently ignored typo in a
//! safety setting is worse than a refusal to start.
//!
//! Default location: `$XDG_CONFIG_HOME/syslens/guard.conf`, falling back to
//! `~/.config/syslens/guard.conf`. A missing default file means defaults; a
//! missing file passed with `--guard-config` is an error.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::hints::{self, HintSet};

#[derive(Clone, Debug, PartialEq)]
pub struct GuardConfig {
    /// Seconds a pressure stop must persist after a freeze before stopping.
    pub grace: Duration,
    /// Seconds critical pressure must persist before the first freeze.
    pub sustain: Duration,
    /// Seconds of normal pressure required before a freeze is undone.
    pub recover: Duration,
    /// Minimum seconds between two guard freezes.
    pub cooldown: Duration,
    /// CPU percent at or above which a unit counts as busy and is skipped.
    pub busy_cpu: f64,
    /// Allow busy units as candidates (they are then ordered last).
    pub allow_busy: bool,
    pub ram_critical_mib: u64,
    pub ram_warn_mib: u64,
    pub swap_critical_pct: f64,
    pub swap_warn_pct: f64,
    /// Memory PSI `some avg10` percent that counts as critical / warning.
    pub psi_critical: f64,
    pub psi_warn: f64,
    /// Overrides the built-in kernel-log pattern for GPU allocation failures.
    pub gpu_log_pattern: Option<String>,
    pub model_hints_add: Vec<String>,
    pub model_hints_only: Option<Vec<String>>,
    pub protect_hints_add: Vec<String>,
    pub system_hints_add: Vec<String>,
    /// Units or hint tokens that are always guard-eligible, even without a
    /// model hint.
    pub allow: Vec<String>,
    /// Units or hint tokens the guard never touches.
    pub deny: Vec<String>,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            sustain: Duration::from_secs(3),
            recover: Duration::from_secs(10),
            cooldown: Duration::from_secs(120),
            busy_cpu: 50.0,
            allow_busy: false,
            ram_critical_mib: 1536,
            ram_warn_mib: 3072,
            swap_critical_pct: 85.0,
            swap_warn_pct: 70.0,
            psi_critical: 60.0,
            psi_warn: 20.0,
            gpu_log_pattern: None,
            model_hints_add: Vec::new(),
            model_hints_only: None,
            protect_hints_add: Vec::new(),
            system_hints_add: Vec::new(),
            allow: Vec::new(),
            deny: Vec::new(),
        }
    }
}

impl GuardConfig {
    /// The hint lists this config produces (defaults plus customizations).
    pub fn hint_set(&self) -> HintSet {
        let mut set = HintSet::defaults();
        if let Some(only) = &self.model_hints_only {
            set.model = only.clone();
        }
        set.model.extend(self.model_hints_add.iter().cloned());
        set.compositor
            .extend(self.protect_hints_add.iter().cloned());
        set.system.extend(self.system_hints_add.iter().cloned());
        set
    }
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("syslens").join("guard.conf"))
}

/// Load the config. An explicit path must exist. The default path may be
/// absent, in which case built-in defaults apply.
pub fn load(explicit: Option<&Path>) -> Result<(GuardConfig, Option<PathBuf>), String> {
    let path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => default_path().filter(|path| path.exists()),
    };
    match path {
        None => Ok((GuardConfig::default(), None)),
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let config = parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
            Ok((config, Some(path)))
        }
    }
}

pub fn parse(text: &str) -> Result<GuardConfig, String> {
    let mut config = GuardConfig::default();
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = match raw.find('#') {
            Some(at) => &raw[..at],
            None => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected key = value"))?;
        let key = key.trim();
        let value = value.trim();
        apply(&mut config, key, value).map_err(|error| format!("line {line_no}: {error}"))?;
    }
    Ok(config)
}

fn apply(config: &mut GuardConfig, key: &str, value: &str) -> Result<(), String> {
    match key {
        "grace" => config.grace = secs(key, value)?,
        "sustain" => config.sustain = secs(key, value)?,
        "recover" => config.recover = secs(key, value)?,
        "cooldown" => config.cooldown = secs(key, value)?,
        "busy_cpu" => config.busy_cpu = percent(key, value)?,
        "allow_busy" => config.allow_busy = boolean(key, value)?,
        "ram_critical_mib" => config.ram_critical_mib = whole(key, value)?,
        "ram_warn_mib" => config.ram_warn_mib = whole(key, value)?,
        "swap_critical_pct" => config.swap_critical_pct = percent(key, value)?,
        "swap_warn_pct" => config.swap_warn_pct = percent(key, value)?,
        "psi_critical" => config.psi_critical = percent(key, value)?,
        "psi_warn" => config.psi_warn = percent(key, value)?,
        "gpu_log_pattern" => {
            config.gpu_log_pattern = (!value.is_empty()).then(|| value.to_string());
        }
        "model_hints_add" => config.model_hints_add.extend(list(value)),
        "model_hints_only" => config.model_hints_only = Some(list(value)),
        "protect_hints_add" => config.protect_hints_add.extend(list(value)),
        "system_hints_add" => config.system_hints_add.extend(list(value)),
        "allow" => config.allow.extend(list(value)),
        "deny" => config.deny.extend(list(value)),
        other => return Err(format!("unknown key '{other}'")),
    }
    Ok(())
}

fn secs(key: &str, value: &str) -> Result<Duration, String> {
    let number: f64 = value
        .parse()
        .map_err(|_| format!("{key} expects seconds, got '{value}'"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("{key} must be zero or positive, got '{value}'"));
    }
    Ok(Duration::from_secs_f64(number))
}

fn percent(key: &str, value: &str) -> Result<f64, String> {
    let number: f64 = value
        .parse()
        .map_err(|_| format!("{key} expects a percentage, got '{value}'"))?;
    if !number.is_finite() || !(0.0..=100.0).contains(&number) {
        return Err(format!("{key} must be between 0 and 100, got '{value}'"));
    }
    Ok(number)
}

fn whole(key: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{key} expects a whole number, got '{value}'"))
}

fn boolean(key: &str, value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("{key} expects true or false, got '{value}'")),
    }
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Install the hint set derived from the config. Call once at startup.
pub fn install_hints(config: &GuardConfig) -> Result<(), String> {
    hints::install(config.hint_set())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(parse("").unwrap(), GuardConfig::default());
        assert_eq!(
            parse("# only a comment\n\n").unwrap(),
            GuardConfig::default()
        );
    }

    #[test]
    fn parses_gating_knobs_and_comments() {
        let config = parse(
            "sustain = 5   # seconds\ncooldown=30\nbusy_cpu = 25\nallow_busy = yes\nswap_critical_pct = 90\n",
        )
        .unwrap();
        assert_eq!(config.sustain, Duration::from_secs(5));
        assert_eq!(config.cooldown, Duration::from_secs(30));
        assert_eq!(config.busy_cpu, 25.0);
        assert!(config.allow_busy);
        assert_eq!(config.swap_critical_pct, 90.0);
    }

    #[test]
    fn lists_extend_or_replace_hints() {
        let config = parse(
            "model_hints_add = mychat, mygen\nprotect_hints_add = my-compositor\nsystem_hints_add = my-daemon\n",
        )
        .unwrap();
        let set = config.hint_set();
        assert!(set.model.contains(&"ollama".to_string()));
        assert!(set.model.contains(&"mychat".to_string()));
        assert!(set.compositor.contains(&"my-compositor".to_string()));
        assert!(set.system.contains(&"my-daemon".to_string()));

        let only = parse("model_hints_only = acme\n").unwrap().hint_set();
        assert_eq!(only.model, vec!["acme".to_string()]);
        assert!(!only.model.contains(&"ollama".to_string()));
    }

    #[test]
    fn unknown_keys_and_bad_values_name_the_line() {
        let error = parse("sustain = 3\nsustian = 4\n").unwrap_err();
        assert!(
            error.contains("line 2") && error.contains("sustian"),
            "{error}"
        );

        let error = parse("busy_cpu = 150\n").unwrap_err();
        assert!(error.contains("line 1"), "{error}");

        let error = parse("allow_busy = maybe\n").unwrap_err();
        assert!(error.contains("true or false"), "{error}");

        let error = parse("just words\n").unwrap_err();
        assert!(error.contains("key = value"), "{error}");
    }

    #[test]
    fn negative_or_nan_durations_are_refused() {
        assert!(parse("grace = -1\n").is_err());
        assert!(parse("grace = nan\n").is_err());
        assert!(parse("grace = 0\n").is_ok());
    }
}
