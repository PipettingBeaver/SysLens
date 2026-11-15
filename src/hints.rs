//! Name-based hint lists used to classify systemd units.
//!
//! Matching is token based. A unit name is split on every non-alphanumeric
//! character (`-`, `_`, `.`, `@`, ...) and a hint matches when its tokens
//! appear as a contiguous run. The last hint token may carry a trailing run of
//! digits, so `kded` matches `kded5.service`, while `tts` does not match
//! `kttsd.service` and `embed` does not match `embedded-viewer`.
//!
//! The defaults cover the common Linux desktops (KDE Plasma, GNOME, Sway,
//! Hyprland, wlroots compositors, Xfce, Cinnamon, MATE, LXQt, i3/Openbox) and
//! the usual display managers. Users extend the lists through the guard config
//! (`model_hints_add`, `protect_hints_add`, `system_hints_add`) or replace the
//! model list outright (`model_hints_only`).

use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HintSet {
    pub model: Vec<String>,
    pub compositor: Vec<String>,
    pub desktop: Vec<String>,
    pub system: Vec<String>,
}

const MODEL: &[&str] = &[
    "ollama",
    "llama",
    "llamacpp",
    "koboldcpp",
    "vllm",
    "comfyui",
    "whisper",
    "tts",
    "llm",
    "embed",
    "embeddings",
    "viitorvoice",
    "voiceforge",
    "nounrevoicer",
];

const COMPOSITOR: &[&str] = &[
    // Wayland and X compositors / window managers.
    "kwin",
    "gnome-shell",
    "mutter",
    "wayland",
    "xwayland",
    "xorg",
    "hyprland",
    "sway",
    "labwc",
    "river",
    "wayfire",
    "niri",
    "cosmic-comp",
    "weston",
    "gamescope",
    "muffin",
    "xfwm4",
    "marco",
    "openbox",
    "i3",
    "picom",
    // Display managers and greeters: stopping these ends the login session.
    "sddm",
    "gdm",
    "greetd",
    "lightdm",
    "lxdm",
    "ly",
    "xdm",
    "cosmic-greeter",
];

const DESKTOP: &[&str] = &[
    // KDE Plasma.
    "plasma",
    "xdg-desktop-portal",
    "kded",
    "kactivitymanagerd",
    "powerdevil",
    "polkit",
    "kwallet",
    "ksmserver",
    "kscreen",
    "kglobalaccel",
    "klipper",
    "kmenuedit",
    // GNOME.
    "gnome-session",
    "settingsdaemon",
    "gsd",
    "gnome-keyring",
    "gcr",
    // Session agents shared across desktops.
    "gpg-agent",
    "ssh-agent",
    // Xfce.
    "xfce4-session",
    "xfce4-panel",
    "xfsettingsd",
    "xfdesktop",
    // Cinnamon, MATE, LXQt.
    "cinnamon",
    "mate-session",
    "mate-panel",
    "lxqt-session",
    "lxqt-panel",
    "lxsession",
    // Bars, notifications, idle and lock helpers used by wlroots setups.
    "waybar",
    "polybar",
    "swaync",
    "mako",
    "dunst",
    "swayidle",
    "swaylock",
    "hypridle",
    "hyprpaper",
];

const SYSTEM: &[&str] = &[
    "systemd",
    "journald",
    "dbus",
    "dbus-broker",
    "pipewire",
    "pipewire-pulse",
    "wireplumber",
    "pulseaudio",
    "networkmanager",
    "tailscaled",
    "syncthing",
    "docker",
    "containerd",
    "podman",
    "bluetooth",
    "upower",
    "rtkit",
    "gvfs",
    "udisks",
    "cups",
    "avahi",
    "wpa_supplicant",
    "modemmanager",
    "smartd",
    "sshd",
    "cron",
    "at-spi",
    "dconf",
    "flatpak-session-helper",
    "xdg-document-portal",
    "xdg-permission-store",
    "tracker",
];

impl HintSet {
    pub fn defaults() -> Self {
        let own = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
        Self {
            model: own(MODEL),
            compositor: own(COMPOSITOR),
            desktop: own(DESKTOP),
            system: own(SYSTEM),
        }
    }
}

static ACTIVE: OnceLock<HintSet> = OnceLock::new();

/// Install the hint set for this process. Called once at startup, after the
/// guard config is loaded. Later calls fail so a surprise reload cannot change
/// classification halfway through a run.
pub fn install(set: HintSet) -> Result<(), String> {
    ACTIVE
        .set(set)
        .map_err(|_| "hint set already installed".to_string())
}

pub fn active() -> &'static HintSet {
    ACTIVE.get_or_init(HintSet::defaults)
}

fn tokens(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn last_token_matches(token: &str, hint: &str) -> bool {
    if token == hint {
        return true;
    }
    token.len() > hint.len()
        && token.starts_with(hint)
        && token[hint.len()..].bytes().all(|b| b.is_ascii_digit())
}

/// True when `hint` occurs in `unit` as a run of whole tokens.
pub fn matches(unit: &str, hint: &str) -> bool {
    let unit_tokens = tokens(unit);
    let hint_tokens = tokens(hint);
    if hint_tokens.is_empty() || unit_tokens.len() < hint_tokens.len() {
        return false;
    }
    let last = hint_tokens.len() - 1;
    unit_tokens.windows(hint_tokens.len()).any(|window| {
        window
            .iter()
            .zip(&hint_tokens)
            .enumerate()
            .all(|(i, (token, hint))| {
                if i == last {
                    last_token_matches(token, hint)
                } else {
                    token == hint
                }
            })
    })
}

pub fn any(unit: &str, list: &[String]) -> bool {
    list.iter().any(|hint| matches(unit, hint))
}

/// The hint that matched, for explanations (`--explain-guard`).
pub fn first_match<'a>(unit: &str, list: &'a [String]) -> Option<&'a str> {
    list.iter()
        .find(|hint| matches(unit, hint))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_token_matching_avoids_substring_false_positives() {
        assert!(!matches("kttsd.service", "tts"));
        assert!(!matches("embedded-viewer.scope", "embed"));
        assert!(matches("text-embed-server.service", "embed"));
        assert!(matches("tts.service", "tts"));
    }

    #[test]
    fn trailing_digits_are_allowed_on_the_last_token() {
        assert!(matches("kded5.service", "kded"));
        assert!(matches("ollama2.service", "ollama"));
        assert!(!matches("kdedx.service", "kded"));
    }

    #[test]
    fn multi_token_hints_match_across_separators() {
        assert!(matches("org.gnome.Shell@wayland.service", "gnome-shell"));
        assert!(matches("gnome-shell.service", "gnome-shell"));
        assert!(!matches("gnome-software.service", "gnome-shell"));
        assert!(matches(
            "org.gnome.SettingsDaemon.Power.service",
            "settingsdaemon"
        ));
    }

    #[test]
    fn defaults_are_nonempty_and_lowercase() {
        let set = HintSet::defaults();
        for list in [&set.model, &set.compositor, &set.desktop, &set.system] {
            assert!(!list.is_empty());
            for hint in list {
                assert_eq!(hint, &hint.to_ascii_lowercase(), "{hint}");
            }
        }
    }
}
