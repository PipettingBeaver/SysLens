use std::collections::{HashMap, HashSet};
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::docker::DockerCache;
use crate::gpu::{Gpu, GpuCache};
use crate::util::{clock_now, hostname, human_bytes};

pub const GIB: u64 = 1024 * 1024 * 1024;
const SMALL_UNIT_BYTES: u64 = 8 * 1024 * 1024;
const MODEL_HINTS: &[&str] = &[
    "viitorvoice",
    "voiceforge",
    "nounrevoicer",
    "ollama",
    "whisper",
    "tts",
    "llm",
    "embed",
];
const COMPOSITOR_HINTS: &[&str] = &[
    "kwin",
    "gnome-shell",
    "mutter",
    "wayland",
    "xorg",
    "sddm",
    "gdm",
    "greetd",
];
const DESKTOP_HINTS: &[&str] = &[
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
];
const SYSTEM_HINTS: &[&str] = &[
    "systemd",
    "journald",
    "dbus",
    "pipewire",
    "wireplumber",
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
];

pub fn is_model_unit(unit: &str) -> bool {
    let lowered = unit.to_lowercase();
    MODEL_HINTS.iter().any(|hint| lowered.contains(hint))
}

pub fn is_compositor_unit(unit: &str) -> bool {
    let lowered = unit.to_lowercase();
    COMPOSITOR_HINTS.iter().any(|hint| lowered.contains(hint))
}

pub fn is_desktop_unit(unit: &str) -> bool {
    is_compositor_unit(unit) || {
        let lowered = unit.to_lowercase();
        DESKTOP_HINTS.iter().any(|hint| lowered.contains(hint))
    }
}

pub fn is_system_unit(unit: &str) -> bool {
    let lowered = unit.to_lowercase();
    SYSTEM_HINTS.iter().any(|hint| lowered.contains(hint))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Model,
    Desktop,
    Container,
    System,
    SelfUnit,
    Kernel,
    User,
}

pub fn category(unit: &UnitStat, self_unit: &str) -> Category {
    if unit.unit == "syslens" || (!self_unit.is_empty() && unit.unit == self_unit) {
        Category::SelfUnit
    } else if unit.unit.starts_with("docker-") || unit.unit.starts_with("podman-") {
        Category::Container
    } else if is_model_unit(&unit.unit) {
        Category::Model
    } else if is_desktop_unit(&unit.unit) {
        Category::Desktop
    } else if matches!(unit.unit.as_str(), "kernel" | "root" | "-") {
        Category::Kernel
    } else if !unit.user || is_system_unit(&unit.unit) {
        Category::System
    } else {
        Category::User
    }
}

#[cfg(test)]
pub fn protect_reason(unit: &UnitStat, self_unit: &str) -> Option<&'static str> {
    protect_reason_ex(unit, self_unit, false)
}

pub fn protect_reason_ex(
    unit: &UnitStat,
    self_unit: &str,
    allow_system_model: bool,
) -> Option<&'static str> {
    if unit.unit == "syslens" || (!self_unit.is_empty() && unit.unit == self_unit) {
        Some("this is SysLens' own scope")
    } else if is_compositor_unit(&unit.unit) {
        Some("compositor: stopping it ends the desktop session")
    } else if is_desktop_unit(&unit.unit) {
        Some("desktop service: use systemctl manually")
    } else if matches!(unit.unit.as_str(), "kernel" | "root" | "-") {
        Some("kernel thread group")
    } else if unit.unit.starts_with("user@")
        || unit.unit == "init.scope"
        || unit.unit.starts_with("session-")
        || unit.unit.ends_with(".slice")
    {
        Some("session/slice scope: too broad to stop safely")
    } else if !unit.user {
        if allow_system_model && is_model_unit(&unit.unit) {
            None
        } else {
            Some("system unit: use sudo/systemctl")
        }
    } else {
        None
    }
}

pub fn is_small(unit: &UnitStat, self_unit: &str) -> bool {
    unit.stopped
        || unit.rss >= SMALL_UNIT_BYTES
        || unit.vram > 0
        || unit.cpu >= 2.0
        || is_model_unit(&unit.unit)
        || is_desktop_unit(&unit.unit)
        || unit.unit.starts_with("docker-")
        || unit.unit.starts_with("podman-")
        || unit.unit == "syslens"
        || unit.unit == self_unit
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CpuTimes {
    total: u64,
    idle: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mem {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub percent: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Swap {
    pub total: u64,
    pub used: u64,
    pub percent: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Zram {
    pub data: u64,
    pub compr: u64,
}

#[derive(Clone, Debug)]
pub struct UnitStat {
    pub unit: String,
    pub display: String,
    pub origin: String,
    pub procs: u32,
    pub rss: u64,
    pub vram: u64,
    pub cpu: f64,
    pub user: bool,
    pub stopped: bool,
    pub frozen: bool,
}

#[derive(Clone, Debug)]
pub struct ProcRow {
    pub pid: u32,
    pub unit: String,
    pub comm: String,
    pub rss: u64,
    pub vram: u64,
    pub cpu: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Crit,
    Warn,
    Info,
    Ok,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Crit => "CRIT",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Ok => "OK",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Level::Crit => 0,
            Level::Warn => 1,
            Level::Info => 2,
            Level::Ok => 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Warning {
    pub level: Level,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub clock: String,
    pub host: String,
    pub cpu_pct: f64,
    pub load: [f64; 3],
    pub ncpu: u64,
    pub ram: Mem,
    pub swap: Swap,
    pub zram: Option<Zram>,
    pub gpu: Option<Gpu>,
    pub gpu_unattributed: u64,
    pub reserve_mb: u64,
    pub units: Vec<UnitStat>,
    pub procs: Vec<ProcRow>,
    pub warnings: Vec<Warning>,
    pub self_unit: String,
    pub nvkms: Option<usize>,
}

fn spawn_journal_watcher(cell: Arc<Mutex<Option<usize>>>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let count = journal_nvkms();
            if let Ok(mut slot) = cell.lock() {
                *slot = count;
            }
            for _ in 0..80 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    });
}

fn journal_nvkms() -> Option<usize> {
    let output = Command::new("journalctl")
        .args([
            "-k",
            "-b",
            "--since",
            "-15 min",
            "-g",
            "NVKMS",
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && !stderr.trim().is_empty() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

pub struct Monitor {
    reserve_mb: u64,
    gpu_enabled: bool,
    ncpu: u64,
    page_size: u64,
    prev_cpu: Option<CpuTimes>,
    prev_procs: HashMap<u32, (u64, u64)>,
    cgroups: HashMap<u32, (u64, String, bool)>,
    gpu_cache: GpuCache,
    docker_cache: DockerCache,
    self_unit: String,
    host: String,
    nvkms: Arc<Mutex<Option<usize>>>,
    journal_stop: Arc<AtomicBool>,
}

impl Monitor {
    pub fn new(reserve_mb: u64, gpu_enabled: bool) -> Self {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size = if page > 0 { page as u64 } else { 4096 };
        let ncpu = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        let nvkms = Arc::new(Mutex::new(None));
        let journal_stop = Arc::new(AtomicBool::new(false));
        spawn_journal_watcher(nvkms.clone(), journal_stop.clone());
        Self {
            reserve_mb,
            gpu_enabled,
            ncpu,
            page_size,
            prev_cpu: None,
            prev_procs: HashMap::new(),
            cgroups: HashMap::new(),
            gpu_cache: GpuCache::new(),
            docker_cache: DockerCache::new(),
            self_unit: String::from("root"),
            host: hostname(),
            nvkms,
            journal_stop,
        }
    }

    pub fn warmup(&mut self) {
        let _ = self.sample();
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            let nvkms_ready = self.nvkms.lock().ok().and_then(|slot| *slot).is_some();
            let gpu_ready = !self.gpu_enabled || self.gpu_cache.ready();
            if nvkms_ready && gpu_ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let current_cpu = read_cpu_times();
        let (cpu_pct, delta_total) = match (&self.prev_cpu, current_cpu) {
            (Some(prev), Some(cur)) => cpu_percent(*prev, cur),
            (None, Some(cur)) => (0.0, cur.total),
            _ => (0.0, 0),
        };
        self.prev_cpu = current_cpu;

        self.gpu_cache.maybe_refresh(self.gpu_enabled);
        self.docker_cache.maybe_refresh();
        let (gpu, gpu_procs) = self.gpu_cache.get();
        let gpu_attributed: u64 = gpu_procs.values().sum();
        let gpu_unattributed = gpu
            .as_ref()
            .map(|g| g.used.saturating_sub(gpu_attributed))
            .unwrap_or(0);

        let (mut units, self_unit, procs) = self.scan(delta_total, &gpu_procs);
        self.self_unit = self_unit.clone();
        for unit in &mut units {
            let (display, origin) = friendly_unit(unit, &self.docker_cache);
            unit.display = display;
            unit.origin = origin;
        }
        disambiguate_display_names(&mut units);
        units.sort_by_key(|unit| std::cmp::Reverse((unit.vram, unit.rss)));

        let ram = read_memory();
        let swap = read_swap();
        let zram = read_zram();
        let load = read_load();
        let nvkms = self.nvkms.lock().ok().and_then(|slot| *slot);
        let mut warnings = build_warnings(
            &gpu,
            &ram,
            &swap,
            &units,
            gpu_unattributed,
            self.reserve_mb,
            nvkms,
        );
        warnings.sort_by_key(|w| w.level.rank());

        Snapshot {
            clock: clock_now(),
            host: self.host.clone(),
            cpu_pct,
            load,
            ncpu: self.ncpu,
            ram,
            swap,
            zram,
            gpu,
            gpu_unattributed,
            reserve_mb: self.reserve_mb,
            units,
            procs,
            warnings,
            self_unit,
            nvkms,
        }
    }

    fn scan(
        &mut self,
        delta_total: u64,
        gpu_procs: &HashMap<u32, u64>,
    ) -> (Vec<UnitStat>, String, Vec<ProcRow>) {
        let mut groups: HashMap<String, UnitStat> = HashMap::new();
        let mut procs = Vec::new();
        let mut next_prev = HashMap::with_capacity(self.prev_procs.len());
        let mut live = HashSet::new();
        let mut self_unit = String::from("root");
        let my_pid = std::process::id();
        let ncpu = self.ncpu;
        let page_size = self.page_size;

        let entries = match fs::read_dir("/proc") {
            Ok(entries) => entries,
            Err(_) => return (Vec::new(), self_unit, Vec::new()),
        };

        for entry in entries.flatten() {
            let pid = match entry.file_name().to_string_lossy().parse::<u32>() {
                Ok(pid) => pid,
                Err(_) => continue,
            };
            let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let (jiffies, start, comm) = match parse_stat(&stat) {
                Some(parsed) => parsed,
                None => continue,
            };

            let delta = match self.prev_procs.get(&pid) {
                Some(&(prev_start, prev_jiffies)) if prev_start == start => {
                    jiffies.saturating_sub(prev_jiffies)
                }
                _ => 0,
            };
            next_prev.insert(pid, (start, jiffies));

            let (unit, user) = match self.cgroups.get(&pid) {
                Some((cached_start, unit, user)) if *cached_start == start => (unit.clone(), *user),
                _ => {
                    let (unit, user) = read_cgroup(pid);
                    self.cgroups.insert(pid, (start, unit.clone(), user));
                    (unit, user)
                }
            };

            let rss = read_rss(pid, page_size);
            let cpu = if delta_total > 0 {
                delta as f64 / delta_total as f64 * ncpu as f64 * 100.0
            } else {
                0.0
            };
            let vram = gpu_procs.get(&pid).copied().unwrap_or(0);

            if pid == my_pid {
                self_unit = unit.clone();
            }
            let group = if pid == my_pid {
                "syslens".to_string()
            } else {
                unit
            };
            procs.push(ProcRow {
                pid,
                unit: group.clone(),
                comm,
                rss,
                vram,
                cpu,
            });
            let stat = groups.entry(group.clone()).or_insert_with(|| UnitStat {
                unit: group,
                display: String::new(),
                origin: String::new(),
                procs: 0,
                rss: 0,
                vram: 0,
                cpu: 0.0,
                user,
                stopped: false,
                frozen: false,
            });
            stat.procs += 1;
            stat.rss += rss;
            stat.vram += vram;
            stat.cpu += cpu;
            live.insert(pid);
        }

        self.prev_procs = next_prev;
        self.cgroups.retain(|pid, _| live.contains(pid));

        (groups.into_values().collect(), self_unit, procs)
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.journal_stop.store(true, Ordering::Relaxed);
    }
}

fn cpu_percent(prev: CpuTimes, cur: CpuTimes) -> (f64, u64) {
    let delta_total = cur.total.saturating_sub(prev.total);
    let delta_idle = cur.idle.saturating_sub(prev.idle);
    if delta_total == 0 {
        return (0.0, 0);
    }
    let busy = delta_total.saturating_sub(delta_idle);
    (busy as f64 / delta_total as f64 * 100.0, delta_total)
}

fn read_cpu_times() -> Option<CpuTimes> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let mut values = [0u64; 8];
    for (slot, token) in values.iter_mut().zip(line.split_whitespace().skip(1)) {
        *slot = token.parse().ok()?;
    }
    Some(CpuTimes {
        total: values.iter().sum(),
        idle: values[3] + values[4],
    })
}

fn read_load() -> [f64; 3] {
    let mut load = [0.0; 3];
    if let Ok(text) = fs::read_to_string("/proc/loadavg") {
        for (slot, token) in load.iter_mut().zip(text.split_whitespace()) {
            *slot = token.parse().unwrap_or(0.0);
        }
    }
    load
}

fn read_meminfo() -> HashMap<String, u64> {
    let mut values = HashMap::new();
    if let Ok(text) = fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some((key, rest)) = line.split_once(':') {
                if let Some(kb) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    values.insert(key.to_string(), kb * 1024);
                }
            }
        }
    }
    values
}

fn read_memory() -> Mem {
    let info = read_meminfo();
    let total = info.get("MemTotal").copied().unwrap_or(0);
    let available = info
        .get("MemAvailable")
        .or_else(|| info.get("MemFree"))
        .copied()
        .unwrap_or(0);
    let used = total.saturating_sub(available);
    Mem {
        total,
        used,
        available,
        percent: percentage(used, total),
    }
}

fn read_swap() -> Swap {
    let info = read_meminfo();
    let total = info.get("SwapTotal").copied().unwrap_or(0);
    let free = info.get("SwapFree").copied().unwrap_or(0);
    let used = total.saturating_sub(free);
    Swap {
        total,
        used,
        percent: percentage(used, total),
    }
}

fn percentage(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

fn read_zram() -> Option<Zram> {
    let text = fs::read_to_string("/sys/block/zram0/mm_stat").ok()?;
    let mut parts = text.split_whitespace();
    Some(Zram {
        data: parts.next()?.parse().ok()?,
        compr: parts.next()?.parse().ok()?,
    })
}

fn parse_stat(text: &str) -> Option<(u64, u64, String)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = text.get(open + 1..close)?.to_string();
    let rest = text.get(close + 1..)?.trim_start();
    let mut tokens = rest.split_whitespace();
    tokens.next()?;
    for _ in 0..10 {
        tokens.next()?;
    }
    let utime: u64 = tokens.next()?.parse().ok()?;
    let stime: u64 = tokens.next()?.parse().ok()?;
    for _ in 0..6 {
        tokens.next()?;
    }
    let start: u64 = tokens.next()?.parse().ok()?;
    Some((utime + stime, start, comm))
}

pub fn read_cmdline(pid: u32) -> String {
    let bytes = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => bytes,
        Err(_) => return String::new(),
    };
    let mut command = String::new();
    for (index, part) in bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        if index > 0 {
            command.push(' ');
        }
        command.push_str(&String::from_utf8_lossy(part));
        if command.chars().count() > 512 {
            let mut short: String = command.chars().take(511).collect();
            short.push('…');
            return short;
        }
    }
    command
}

pub fn friendly_unit_name(unit: &str) -> String {
    let mut base = unit;
    for suffix in [".service", ".scope", ".slice"] {
        if let Some(stripped) = unit.strip_suffix(suffix) {
            base = stripped;
            break;
        }
    }
    if let Some(rest) = base.strip_prefix("app-") {
        let rest = rest.split('@').next().unwrap_or(rest);
        let rest = rest
            .rsplit_once('-')
            .filter(|(_, tail)| !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()))
            .map(|(head, _)| head)
            .unwrap_or(rest);
        format!("app:{rest}")
    } else {
        base.to_string()
    }
}

fn unit_instance_tag(unit: &str) -> Option<String> {
    let base = unit
        .strip_suffix(".service")
        .or_else(|| unit.strip_suffix(".scope"))
        .or_else(|| unit.strip_suffix(".slice"))
        .unwrap_or(unit);
    let rest = base.strip_prefix("app-")?;
    if let Some((_, instance)) = rest.split_once('@') {
        if !instance.is_empty() {
            return Some(short_tag(instance));
        }
    }
    if let Some((_, tail)) = rest.rsplit_once('-') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return Some(tail.to_string());
        }
    }
    None
}

fn short_tag(tag: &str) -> String {
    if tag.chars().count() > 8 {
        tag.chars().take(8).collect()
    } else {
        tag.to_string()
    }
}

/// Append the instance id when two units share a friendly name, so multiple
/// scopes of the same app stay distinguishable in the table.
fn disambiguate_display_names(units: &mut [UnitStat]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for unit in units.iter() {
        *counts.entry(unit.display.clone()).or_insert(0) += 1;
    }
    for unit in units.iter_mut() {
        if counts.get(&unit.display).copied().unwrap_or(0) > 1 {
            if let Some(tag) = unit_instance_tag(&unit.unit) {
                unit.display = format!("{}#{}", unit.display, tag);
            }
        }
    }
}

fn friendly_unit(unit: &UnitStat, docker: &DockerCache) -> (String, String) {
    if let Some(info) = docker.lookup(&unit.unit) {
        let project = if info.project.is_empty() || info.project == "<no value>" {
            String::new()
        } else {
            format!(" · project {}", info.project)
        };
        return (
            format!("docker:{}", info.name),
            format!("image {}{}", info.image, project),
        );
    }
    if let Some(id) = unit
        .unit
        .strip_prefix("docker-")
        .and_then(|rest| rest.strip_suffix(".scope"))
    {
        return (
            format!("docker:{}", &id[..id.len().min(12)]),
            unit.unit.clone(),
        );
    }
    if let Some(id) = unit
        .unit
        .strip_prefix("podman-")
        .and_then(|rest| rest.strip_suffix(".scope"))
    {
        return (
            format!("podman:{}", &id[..id.len().min(12)]),
            unit.unit.clone(),
        );
    }
    (friendly_unit_name(&unit.unit), unit.unit.clone())
}

fn read_rss(pid: u32, page_size: u64) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|text| {
            text.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u64>().ok())
        })
        .map(|pages| pages * page_size)
        .unwrap_or(0)
}

fn read_cgroup(pid: u32) -> (String, bool) {
    let text = match fs::read_to_string(format!("/proc/{pid}/cgroup")) {
        Ok(text) => text,
        Err(_) => return ("kernel".to_string(), false),
    };
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            let path = path.trim();
            let user = path.starts_with("/user.slice") || path.contains("user@");
            return (unit_from_cgroup(path), user);
        }
    }
    ("kernel".to_string(), false)
}

fn unit_from_cgroup(path: &str) -> String {
    for part in path.rsplit('/') {
        if part.ends_with(".service") || part.ends_with(".scope") || part.ends_with(".slice") {
            return unescape(part);
        }
    }
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .map(unescape)
        .unwrap_or_else(|| "root".to_string())
}

fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() && bytes[index + 1] == b'x' {
            if let (Some(high), Some(low)) = (hex(bytes[index + 2]), hex(bytes[index + 3])) {
                out.push((high * 16 + low) as char);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn build_warnings(
    gpu: &Option<Gpu>,
    ram: &Mem,
    swap: &Swap,
    units: &[UnitStat],
    gpu_unattributed: u64,
    reserve_mb: u64,
    nvkms: Option<usize>,
) -> Vec<Warning> {
    let mut warnings = Vec::new();
    let mut add = |level: Level, text: String| warnings.push(Warning { level, text });

    if let Some(gpu) = gpu {
        if gpu.free < reserve_mb {
            let holder = units.iter().find(|unit| unit.vram > 0);
            let detail = holder
                .map(|unit| {
                    format!(
                        "; largest holder {} {}",
                        unit.unit,
                        human_bytes(unit.vram * 1024 * 1024)
                    )
                })
                .unwrap_or_default();
            add(
                Level::Crit,
                format!(
                    "GPU VRAM headroom {} MiB is below the {} MiB compositor reserve{}",
                    gpu.free, reserve_mb, detail
                ),
            );
        } else if gpu.free < reserve_mb * 3 / 2 {
            add(
                Level::Warn,
                format!(
                    "GPU VRAM headroom {} MiB is close to the {} MiB compositor reserve",
                    gpu.free, reserve_mb
                ),
            );
        }
        if gpu_unattributed > (gpu.total * 8 / 100).max(512) {
            add(
                Level::Info,
                format!(
                    "{} VRAM is held outside visible processes (driver/context overhead)",
                    human_bytes(gpu_unattributed * 1024 * 1024)
                ),
            );
        }
    }

    if ram.available < 1536 * 1024 * 1024 {
        add(
            Level::Crit,
            format!(
                "Only {} RAM available; zram-only swap makes OOM likely",
                human_bytes(ram.available)
            ),
        );
    } else if ram.available < 3 * GIB {
        add(
            Level::Warn,
            format!("Low available RAM: {}", human_bytes(ram.available)),
        );
    }

    if swap.total > 0 && swap.percent >= 85.0 {
        add(
            Level::Crit,
            format!(
                "Swap {:.0}% used ({} of {})",
                swap.percent,
                human_bytes(swap.used),
                human_bytes(swap.total)
            ),
        );
    } else if swap.total > 0 && swap.percent >= 70.0 {
        add(
            Level::Warn,
            format!("Swap pressure: {:.0}% used", swap.percent),
        );
    }

    if let Some(count) = nvkms {
        if count > 0 {
            add(
                Level::Warn,
                format!(
                    "{count} NVKMS allocation failures in the last 15 min; desktop may flicker"
                ),
            );
        }
    }

    let models: Vec<&UnitStat> = units
        .iter()
        .filter(|unit| is_model_unit(&unit.unit) && unit.vram >= 512)
        .collect();
    if models.len() > 1 {
        let total: u64 = models.iter().map(|unit| unit.vram).sum();
        let names = models
            .iter()
            .take(4)
            .map(|unit| format!("{} {} MiB", unit.unit, unit.vram))
            .collect::<Vec<_>>()
            .join(", ");
        add(
            Level::Warn,
            format!(
                "{} model backends co-loaded ({} VRAM): {}",
                models.len(),
                human_bytes(total * 1024 * 1024),
                names
            ),
        );
    }

    if warnings.is_empty() {
        warnings.push(Warning {
            level: Level::Ok,
            text: "No resource pressure detected".to_string(),
        });
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comm_with_spaces_and_parentheses() {
        let line = "1234 (my (odd) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 12 34 0 0 20 0 3 0 98765 1000 100 0 0 0 0 0 0";
        assert_eq!(
            parse_stat(line),
            Some((46, 98765, "my (odd) name".to_string()))
        );
    }

    #[test]
    fn extracts_unit_and_unescapes() {
        assert_eq!(
            unit_from_cgroup(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/ollama.service"
            ),
            "ollama.service"
        );
        assert_eq!(
            unit_from_cgroup("/user.slice/user-1000.slice/session-2.scope"),
            "session-2.scope"
        );
        assert_eq!(
            unit_from_cgroup("/user.slice/app-org\\x2ekde\\x2ekonsole-42.scope"),
            "app-org.kde.konsole-42.scope"
        );
        assert_eq!(unit_from_cgroup("/"), "root");
    }

    fn normal_row() -> UnitStat {
        UnitStat {
            unit: "viitorvoice.service".into(),
            display: "viitorvoice".into(),
            origin: "viitorvoice.service".into(),
            procs: 1,
            rss: 1,
            vram: 0,
            cpu: 0.0,
            user: true,
            stopped: false,
            frozen: false,
        }
    }

    #[test]
    fn protection_covers_self_and_compositor() {
        let unit = UnitStat {
            unit: "plasma-kwin_wayland.service".into(),
            display: "plasma-kwin_wayland".into(),
            origin: "plasma-kwin_wayland.service".into(),
            procs: 1,
            rss: 1,
            vram: 0,
            cpu: 0.0,
            user: true,
            stopped: false,
            frozen: false,
        };
        assert!(protect_reason(&unit, "app-org.kde.konsole-1.scope").is_some());
        let self_unit = UnitStat {
            unit: "app-org.kde.konsole-1.scope".into(),
            ..unit.clone()
        };
        assert!(protect_reason(&self_unit, "app-org.kde.konsole-1.scope").is_some());
        for protected in [
            "user@1000.service",
            "init.scope",
            "session-2.scope",
            "app.slice",
        ] {
            let row = UnitStat {
                unit: protected.into(),
                ..normal_row()
            };
            assert!(protect_reason(&row, "").is_some(), "{protected}");
        }
        let normal = UnitStat {
            unit: "viitorvoice.service".into(),
            ..unit
        };
        assert!(protect_reason(&normal, "app-org.kde.konsole-1.scope").is_none());
    }

    #[test]
    fn cpu_delta_math() {
        let prev = CpuTimes {
            total: 1000,
            idle: 800,
        };
        let cur = CpuTimes {
            total: 1400,
            idle: 1000,
        };
        let (pct, delta) = cpu_percent(prev, cur);
        assert_eq!(delta, 400);
        assert!((pct - 50.0).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod category_tests {
    use super::*;

    fn row(name: &str, user: bool) -> UnitStat {
        UnitStat {
            unit: name.to_string(),
            display: name.to_string(),
            origin: name.to_string(),
            procs: 1,
            rss: 1024,
            vram: 0,
            cpu: 0.0,
            user,
            stopped: false,
            frozen: false,
        }
    }

    #[test]
    fn classifies_model_desktop_system_and_user_units() {
        assert_eq!(category(&row("ollama.service", true), ""), Category::Model);
        assert_eq!(
            category(&row("plasma-plasmashell.service", true), ""),
            Category::Desktop
        );
        assert_eq!(
            category(&row("app-org.kde.dolphin@1.service", true), ""),
            Category::User
        );
        assert_eq!(
            category(&row("dbus-broker.service", true), ""),
            Category::System
        );
        assert_eq!(category(&row("syslens", true), ""), Category::SelfUnit);
        assert_eq!(category(&row("kernel", false), ""), Category::Kernel);
        assert_eq!(
            category(&row("docker.service", false), ""),
            Category::System
        );
    }

    #[test]
    fn system_model_units_need_explicit_permission() {
        let row = row("ollama.service", false);
        assert!(protect_reason(&row, "").is_some());
        assert!(protect_reason_ex(&row, "", true).is_none());
    }

    #[test]
    fn shortens_app_and_service_names() {
        assert_eq!(friendly_unit_name("ollama.service"), "ollama");
        assert_eq!(
            friendly_unit_name("app-ai.opencode.desktop-1027436.scope"),
            "app:ai.opencode.desktop"
        );
        assert_eq!(
            friendly_unit_name("app-org.kde.dolphin@abc.service"),
            "app:org.kde.dolphin"
        );
    }

    #[test]
    fn disambiguates_duplicate_app_names() {
        let mut first = row("app-discord-100.scope", true);
        first.display = friendly_unit_name(&first.unit);
        let mut second = row("app-discord-200.scope", true);
        second.display = friendly_unit_name(&second.unit);
        let mut units = vec![first, second];
        disambiguate_display_names(&mut units);
        assert_eq!(units[0].display, "app:discord#100");
        assert_eq!(units[1].display, "app:discord#200");
    }

    #[test]
    fn desktop_services_are_protected_but_kde_apps_are_not() {
        assert!(protect_reason(&row("plasma-plasmashell.service", true), "").is_some());
        assert!(protect_reason(&row("plasma-kwin_wayland.service", true), "").is_some());
        assert!(protect_reason(&row("app-org.kde.dolphin@1.service", true), "").is_none());
        assert!(protect_reason(&row("ollama.service", true), "").is_none());
    }
}
