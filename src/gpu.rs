use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::util::run_capture;

const REFRESH_AFTER: Duration = Duration::from_secs(2);
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);

type GpuSnapshot = (Option<Gpu>, HashMap<u32, u64>);

#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub name: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub util: u64,
    pub temp: u64,
}

pub fn query() -> Option<(Gpu, HashMap<u32, u64>)> {
    let text = run_capture(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,memory.used,memory.free,utilization.gpu,temperature.gpu",
            "--format=csv,noheader,nounits",
        ],
        QUERY_TIMEOUT,
    )?;
    let row = text.lines().next()?;
    let fields: Vec<&str> = row.split(',').map(str::trim).collect();
    if fields.len() < 6 {
        return None;
    }
    let gpu = Gpu {
        name: fields[0].to_string(),
        total: number(fields[1]),
        used: number(fields[2]),
        free: number(fields[3]),
        util: number(fields[4]),
        temp: number(fields[5]),
    };

    let mut procs = HashMap::new();
    if let Some(text) = run_capture(
        "nvidia-smi",
        &[
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ],
        QUERY_TIMEOUT,
    ) {
        for line in text.lines() {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 2 {
                continue;
            }
            if let Ok(pid) = fields[0].parse::<u32>() {
                procs.insert(pid, number(fields[1]));
            }
        }
    }

    Some((gpu, procs))
}

/// GPU usage refreshed on a background thread so a hung `nvidia-smi` never
/// blocks the UI or the sampling loop.
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
            let result = match query() {
                Some((gpu, procs)) => (Some(gpu), procs),
                None => (None, HashMap::new()),
            };
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

fn number(text: &str) -> u64 {
    text.parse::<u64>()
        .or_else(|_| text.parse::<f64>().map(|value| value as u64))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_fields() {
        assert_eq!(number("12288"), 12288);
        assert_eq!(number("1370.5"), 1370);
        assert_eq!(number("N/A"), 0);
    }
}
