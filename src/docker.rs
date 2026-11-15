use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::util::run_capture;

const REFRESH_AFTER: Duration = Duration::from_secs(10);
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug)]
pub struct ContainerInfo {
    pub name: String,
    pub image: String,
    pub project: String,
}

/// Container metadata refreshed on a background thread so a slow or hung
/// Docker daemon never blocks the UI or sampling loop.
#[derive(Clone)]
pub struct DockerCache {
    shared: Arc<Mutex<HashMap<String, ContainerInfo>>>,
    at: Arc<Mutex<Option<Instant>>>,
    inflight: Arc<AtomicBool>,
}

impl DockerCache {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(HashMap::new())),
            at: Arc::new(Mutex::new(None)),
            inflight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start a refresh if the cache is stale. Cheap and non-blocking: the
    /// actual `docker ps` runs on a worker thread.
    pub fn maybe_refresh(&self) {
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
            let map = query().unwrap_or_default();
            if let Ok(mut slot) = shared.lock() {
                *slot = map;
            }
            if let Ok(mut slot) = at.lock() {
                *slot = Some(Instant::now());
            }
            inflight.store(false, Ordering::Release);
        });
    }

    pub fn lookup(&self, unit: &str) -> Option<ContainerInfo> {
        let id = unit.strip_prefix("docker-")?.strip_suffix(".scope")?;
        let map = self.shared.lock().ok()?;
        map.get(id)
            .or_else(|| map.get(&id[..id.len().min(12)]))
            .cloned()
    }
}

fn query() -> Option<HashMap<String, ContainerInfo>> {
    let output = run_capture(
        "docker",
        &[
            "ps",
            "-a",
            "--no-trunc",
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Label \"com.docker.compose.project\"}}",
        ],
        QUERY_TIMEOUT,
    )?;
    let mut map = HashMap::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let info = ContainerInfo {
            name: fields[1].trim().to_string(),
            image: fields[2].trim().to_string(),
            project: fields.get(3).map(|v| v.trim()).unwrap_or("").to_string(),
        };
        let id = fields[0].trim().to_string();
        if id.len() >= 12 {
            map.insert(id[..12].to_string(), info.clone());
        }
        map.insert(id, info);
    }
    Some(map)
}
