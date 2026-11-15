//! GPU memory telemetry without a vendor dependency.
//!
//! Three sources, combined:
//!
//! 1. Device totals and usage. NVIDIA through `nvidia-smi` when it is installed.
//!    Otherwise the first DRM card whose sysfs exposes `mem_info_vram_*`
//!    (amdgpu does). Intel discrete cards that expose no VRAM counters yield no
//!    device totals, so VRAM headroom checks are skipped and the RAM and
//!    GPU-allocation-failure triggers still work.
//! 2. Per-process VRAM from DRM client `fdinfo` (`/proc/<pid>/fdinfo/<fd>` on a
//!    `/dev/dri/*` fd). This works for amdgpu, i915/xe and other DRM drivers
//!    that implement the usage-stats spec. It is the only per-process source
//!    for graphics clients on most drivers.
//! 3. Per-process VRAM from `nvidia-smi --query-compute-apps`, for NVIDIA
//!    compute clients that do not expose fdinfo. Where both sources report a
//!    process, the larger value wins.
//!
//! Everything here runs on a background thread (see `GpuCache`), so a slow
//! `nvidia-smi` or a large `/proc` scan never stalls the UI.

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::util::run_capture;

const REFRESH_AFTER: Duration = Duration::from_secs(2);
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
/// Upper bound on one fdinfo scan. Processes past the budget are skipped for
/// this refresh and picked up on the next one.
const FDINFO_BUDGET: Duration = Duration::from_millis(1500);
const MIB: u64 = 1024 * 1024;

type GpuSnapshot = (Option<Gpu>, HashMap<u32, u64>);

#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub name: String,
    /// MiB.
    pub total: u64,
    /// MiB.
    pub used: u64,
    /// MiB.
    pub free: u64,
    /// Percent busy, when the source reports it.
    pub util: Option<u64>,
    /// Degrees C, when the source reports it.
    pub temp: Option<u64>,
}

/// One query round: the device (if any) and per-process VRAM in MiB.
pub fn query() -> GpuSnapshot {
    let mut procs = scan_fdinfo(FDINFO_BUDGET);
    if let Some((gpu, compute)) = query_nvidia() {
        for (pid, mib) in compute {
            let slot = procs.entry(pid).or_insert(0);
            *slot = (*slot).max(mib);
        }
        return (Some(gpu), procs);
    }
    (query_sysfs(), procs)
}

fn query_nvidia() -> Option<(Gpu, HashMap<u32, u64>)> {
    let text = run_capture(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,memory.used,memory.free,utilization.gpu,temperature.gpu",
            "--format=csv,noheader,nounits",
        ],
        QUERY_TIMEOUT,
    )?;
    let row = text.lines().next()?;
    let gpu = parse_nvidia_row(row)?;

    let mut procs = HashMap::new();
    if let Some(text) = run_capture(
        "nvidia-smi",
        &[
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ],
        QUERY_TIMEOUT,
    ) {
        procs = parse_nvidia_apps(&text);
    }
    Some((gpu, procs))
}

/// Parse one `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` row.
/// Fields the driver reports as `[N/A]` or `[Not Supported]` become `None`.
pub fn parse_nvidia_row(row: &str) -> Option<Gpu> {
    let fields: Vec<&str> = row.split(',').map(str::trim).collect();
    if fields.len() < 6 {
        return None;
    }
    Some(Gpu {
        name: fields[0].to_string(),
        total: number(fields[1]).unwrap_or(0),
        used: number(fields[2]).unwrap_or(0),
        free: number(fields[3]).unwrap_or(0),
        util: number(fields[4]),
        temp: number(fields[5]),
    })
}

/// Parse `nvidia-smi --query-compute-apps=pid,used_memory` rows into MiB.
pub fn parse_nvidia_apps(text: &str) -> HashMap<u32, u64> {
    let mut procs = HashMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 2 {
            continue;
        }
        if let (Ok(pid), Some(mib)) = (fields[0].parse::<u32>(), number(fields[1])) {
            procs.insert(pid, mib);
        }
    }
    procs
}

/// Device VRAM from the first DRM card that exposes `mem_info_vram_total`.
fn query_sysfs() -> Option<Gpu> {
    let mut cards: Vec<String> = fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.strip_prefix("card")
                .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or(false)
        })
        .collect();
    cards.sort();
    for card in cards {
        let device = format!("/sys/class/drm/{card}/device");
        let total = match fs::read_to_string(format!("{device}/mem_info_vram_total")) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let used = fs::read_to_string(format!("{device}/mem_info_vram_used")).unwrap_or_default();
        let busy = fs::read_to_string(format!("{device}/gpu_busy_percent")).ok();
        let temp = first_hwmon_temp(&device);
        let vendor = fs::read_to_string(format!("{device}/vendor")).unwrap_or_default();
        if let Some(gpu) = parse_sysfs(
            &total,
            &used,
            busy.as_deref(),
            temp.as_deref(),
            vendor_name(&vendor),
        ) {
            return Some(gpu);
        }
    }
    None
}

fn first_hwmon_temp(device: &str) -> Option<String> {
    let hwmon = fs::read_dir(format!("{device}/hwmon")).ok()?;
    for entry in hwmon.flatten() {
        if let Ok(text) = fs::read_to_string(entry.path().join("temp1_input")) {
            return Some(text);
        }
    }
    None
}

fn vendor_name(vendor_id: &str) -> String {
    match vendor_id.trim().to_ascii_lowercase().as_str() {
        "0x1002" => "AMD GPU".to_string(),
        "0x8086" => "Intel GPU".to_string(),
        "0x10de" => "NVIDIA GPU".to_string(),
        _ => "GPU".to_string(),
    }
}

/// Build a `Gpu` from sysfs text. `total` and `used` are bytes; `busy` is a
/// percent; `temp_milli` is millidegrees C. A zero total means the counter
/// exists but is not meaningful, so no device is reported.
pub fn parse_sysfs(
    total: &str,
    used: &str,
    busy: Option<&str>,
    temp_milli: Option<&str>,
    name: String,
) -> Option<Gpu> {
    let total_bytes = total.trim().parse::<u64>().ok().filter(|v| *v > 0)?;
    let used_bytes = used.trim().parse::<u64>().unwrap_or(0);
    let total_mib = total_bytes / MIB;
    let used_mib = used_bytes / MIB;
    Some(Gpu {
        name,
        total: total_mib,
        used: used_mib,
        free: total_mib.saturating_sub(used_mib),
        util: busy.and_then(|text| number(text.trim())),
        temp: temp_milli
            .and_then(|text| text.trim().parse::<u64>().ok())
            .map(|milli| milli / 1000),
    })
}

/// Parse one DRM `fdinfo` document. Returns the client id (when present) and
/// that client's VRAM in bytes.
///
/// VRAM regions are named `vram`, `vram0`, ... (GTT and system regions are
/// excluded). Keys are preferred in this order: `drm-resident-*` (current),
/// `drm-memory-*` (older drivers), `drm-total-*` (allocated, not necessarily
/// resident). Returns `None` when the document has no DRM usage keys at all.
pub fn parse_fdinfo(text: &str) -> Option<(Option<u64>, u64)> {
    let mut client = None;
    let mut saw_drm = false;
    let (mut resident, mut memory, mut total) = (0u64, 0u64, 0u64);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if !key.starts_with("drm-") {
            continue;
        }
        saw_drm = true;
        if key == "drm-client-id" {
            client = value.parse::<u64>().ok();
            continue;
        }
        let region = key.rsplit('-').next().unwrap_or("");
        if !region.starts_with("vram") {
            continue;
        }
        let bytes = parse_size(value);
        if key.starts_with("drm-resident-") {
            resident += bytes;
        } else if key.starts_with("drm-memory-") {
            memory += bytes;
        } else if key.starts_with("drm-total-") {
            total += bytes;
        }
    }
    if !saw_drm {
        return None;
    }
    let vram = if resident > 0 {
        resident
    } else if memory > 0 {
        memory
    } else {
        total
    };
    Some((client, vram))
}

/// `"123 KiB"`, `"4 MiB"`, `"2 GiB"`, or a bare byte count.
fn parse_size(value: &str) -> u64 {
    let mut parts = value.split_whitespace();
    let amount = parts
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    let scale = match parts.next() {
        Some("KiB") => 1024.0,
        Some("MiB") => 1024.0 * 1024.0,
        Some("GiB") => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (amount * scale) as u64
}

/// Walk `/proc` for processes holding a DRM render or card node and total
/// their VRAM. Clients shared across several fds are counted once. Returns
/// MiB per pid, skipping values under 1 MiB.
fn scan_fdinfo(budget: Duration) -> HashMap<u32, u64> {
    let started = Instant::now();
    let mut procs = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return procs;
    };
    for entry in entries.flatten() {
        if started.elapsed() >= budget {
            break;
        }
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let mut anonymous = 0u64;
        let mut clients: HashMap<u64, u64> = HashMap::new();
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if !target.to_string_lossy().starts_with("/dev/dri/") {
                continue;
            }
            let fd_name = fd.file_name();
            let Ok(text) =
                fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fd_name.to_string_lossy()))
            else {
                continue;
            };
            if let Some((client, bytes)) = parse_fdinfo(&text) {
                match client {
                    Some(id) => {
                        let slot = clients.entry(id).or_insert(0);
                        *slot = (*slot).max(bytes);
                    }
                    None => anonymous += bytes,
                }
            }
        }
        let total: u64 = anonymous + clients.values().sum::<u64>();
        if total >= MIB {
            procs.insert(pid, total / MIB);
        }
    }
    procs
}

/// GPU usage refreshed on a background thread so a hung `nvidia-smi` or a
/// slow `/proc` scan never blocks the UI or the sampling loop.
#[derive(Clone)]
pub struct GpuCache {
    shared: Arc<Mutex<GpuSnapshot>>,
    at: Arc<Mutex<Option<Instant>>>,
    inflight: Arc<AtomicBool>,
}

impl GpuCache {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new((None, HashMap::new()))),
            at: Arc::new(Mutex::new(None)),
            inflight: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn maybe_refresh(&self, enabled: bool) {
        if !enabled {
            if let Ok(mut slot) = self.shared.lock() {
                if slot.0.is_some() || !slot.1.is_empty() {
                    *slot = (None, HashMap::new());
                }
            }
            return;
        }
        if self.inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        let stale = self
            .at
            .lock()
            .map(|at| at.map(|at| at.elapsed() >= REFRESH_AFTER).unwrap_or(true))
            .unwrap_or(true);
        if !stale {
            self.inflight.store(false, Ordering::Release);
            return;
        }
        let shared = self.shared.clone();
        let at = self.at.clone();
        let inflight = self.inflight.clone();
        std::thread::spawn(move || {
            let result = query();
            if let Ok(mut slot) = shared.lock() {
                *slot = result;
            }
            if let Ok(mut slot) = at.lock() {
                *slot = Some(Instant::now());
            }
            inflight.store(false, Ordering::Release);
        });
    }

    /// True once at least one query attempt has completed and no query is
    /// currently running.
    pub fn ready(&self) -> bool {
        !self.inflight.load(Ordering::Acquire)
            && self.at.lock().map(|at| at.is_some()).unwrap_or(false)
    }

    pub fn get(&self) -> GpuSnapshot {
        self.shared
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or((None, HashMap::new()))
    }
}

/// Numeric field from a driver or sysfs value. `[N/A]`, `[Not Supported]`,
/// and other non-numeric text give `None`.
fn number(text: &str) -> Option<u64> {
    let text = text.trim();
    text.parse::<u64>().ok().or_else(|| {
        text.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_fields_and_placeholders() {
        assert_eq!(number("12288"), Some(12288));
        assert_eq!(number("1370.5"), Some(1370));
        assert_eq!(number("[N/A]"), None);
        assert_eq!(number("[Not Supported]"), None);
        assert_eq!(number(""), None);
        assert_eq!(number("-3"), None);
    }

    #[test]
    fn nvidia_row_with_full_fields() {
        let gpu = parse_nvidia_row("NVIDIA GeForce RTX 3060, 12288, 4010, 7888, 15, 46").unwrap();
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 3060");
        assert_eq!((gpu.total, gpu.used, gpu.free), (12288, 4010, 7888));
        assert_eq!(gpu.util, Some(15));
        assert_eq!(gpu.temp, Some(46));
    }

    #[test]
    fn nvidia_row_with_unsupported_telemetry_keeps_memory() {
        let gpu = parse_nvidia_row("Tesla T4, 15360, 512, 14848, [N/A], [Not Supported]").unwrap();
        assert_eq!(gpu.total, 15360);
        assert_eq!(gpu.util, None);
        assert_eq!(gpu.temp, None);
    }

    #[test]
    fn nvidia_row_too_short_is_rejected() {
        assert!(parse_nvidia_row("RTX, 12288").is_none());
    }

    #[test]
    fn nvidia_compute_apps_parse_to_mib() {
        let procs = parse_nvidia_apps("1234, 805\n5678, [N/A]\nbad line\n");
        assert_eq!(procs.get(&1234), Some(&805));
        assert!(!procs.contains_key(&5678));
        assert_eq!(procs.len(), 1);
    }

    #[test]
    fn amdgpu_fdinfo_uses_vram_and_excludes_gtt() {
        let text = "pos:\t0\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\ndrm-memory-vram:\t2048 KiB\ndrm-memory-gtt:\t9999 KiB\n";
        assert_eq!(parse_fdinfo(text), Some((Some(42), 2048 * 1024)));
    }

    #[test]
    fn xe_fdinfo_prefers_resident_over_total() {
        let text = "drm-driver:\txe\ndrm-client-id:\t7\ndrm-total-vram0:\t1 MiB\ndrm-resident-vram0:\t512 MiB\ndrm-total-system:\t4 MiB\n";
        assert_eq!(parse_fdinfo(text), Some((Some(7), 512 * 1024 * 1024)));
    }

    #[test]
    fn fdinfo_without_drm_keys_is_not_a_gpu_client() {
        assert_eq!(parse_fdinfo("pos:\t0\nflags:\t02000002\n"), None);
    }

    #[test]
    fn fdinfo_units_default_to_bytes() {
        let text = "drm-client-id:\t1\ndrm-memory-vram:\t4096\n";
        assert_eq!(parse_fdinfo(text), Some((Some(1), 4096)));
    }

    #[test]
    fn sysfs_totals_convert_to_mib_and_report_telemetry() {
        let gpu = parse_sysfs(
            "17179869184",
            "4294967296",
            Some("37\n"),
            Some("51000\n"),
            "AMD GPU".into(),
        )
        .unwrap();
        assert_eq!(gpu.total, 16384);
        assert_eq!(gpu.used, 4096);
        assert_eq!(gpu.free, 12288);
        assert_eq!(gpu.util, Some(37));
        assert_eq!(gpu.temp, Some(51));
    }

    #[test]
    fn sysfs_without_total_reports_no_device() {
        assert!(parse_sysfs("0", "0", None, None, "x".into()).is_none());
        assert!(parse_sysfs("garbage", "0", None, None, "x".into()).is_none());
    }

    #[test]
    fn sysfs_without_optional_telemetry_still_reports_memory() {
        let gpu = parse_sysfs("8589934592", "0", None, None, "Intel GPU".into()).unwrap();
        assert_eq!(gpu.total, 8192);
        assert_eq!(gpu.util, None);
        assert_eq!(gpu.temp, None);
    }

    #[test]
    fn vendor_ids_map_to_names() {
        assert_eq!(vendor_name("0x1002\n"), "AMD GPU");
        assert_eq!(vendor_name("0x8086"), "Intel GPU");
        assert_eq!(vendor_name("0xbeef"), "GPU");
    }
}
