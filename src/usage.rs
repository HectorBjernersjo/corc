//! Plan-usage readout for the sidebar menu, shown as a dim row under the
//! provider-switch button: percent used per limit (e.g. Claude's 5h session,
//! weekly, and model-scoped weekly limits). Each provider supplies its own
//! numbers through `Provider::fetch_usage`; only Claude implements it so far.
//!
//! Fetches run on their own thread (network must never stall the 10 Hz draw
//! loop). A failed fetch keeps a provider's last good snapshot rather than
//! blanking the row.

use crate::provider;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REFRESH: Duration = Duration::from_secs(60);

/// One limit as shown in the menu: a short label and percent used.
#[derive(Clone)]
pub struct Entry {
    pub label: String,
    pub percent: u8,
}

/// Handle to the background fetch; `entries(provider_id)` returns the latest
/// snapshot for that provider.
pub struct Fetcher {
    data: Arc<Mutex<HashMap<&'static str, Vec<Entry>>>>,
}

impl Fetcher {
    pub fn spawn() -> Self {
        let data = Arc::new(Mutex::new(HashMap::new()));
        let slot = Arc::clone(&data);
        std::thread::spawn(move || {
            loop {
                for p in provider::all() {
                    if let Some(entries) = p.fetch_usage() {
                        slot.lock().unwrap().insert(p.id(), entries);
                    }
                }
                std::thread::sleep(REFRESH);
            }
        });
        Self { data }
    }

    pub fn entries(&self, provider_id: &str) -> Option<Vec<Entry>> {
        self.data.lock().unwrap().get(provider_id).cloned()
    }
}

/// GET `url` via curl for a provider's `fetch_usage`, shelling out rather
/// than pulling in an HTTP stack the same way the rest of corc shells out to
/// tmux. The headers (auth tokens) go in through a `-K -` stdin config
/// rather than argv, so they never show up in the process list.
pub fn curl_get(url: &str, headers: &[String]) -> Option<Vec<u8>> {
    let mut child = Command::new("curl")
        .args(["-sf", "--max-time", "10", "-K", "-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let config: String = headers
        .iter()
        .map(|h| format!("header = \"{h}\"\n"))
        .collect();
    child.stdin.take()?.write_all(config.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}
