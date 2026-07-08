use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ClaudeProc {
    pub pid: u32,
    pub cwd: PathBuf,
}

/// All live `claude` processes and their working directories.
pub fn scan() -> Vec<ClaudeProc> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if comm.trim_end() != "claude" {
            continue;
        }
        let Ok(cwd) = fs::read_link(entry.path().join("cwd")) else {
            continue;
        };
        out.push(ClaudeProc { pid, cwd });
    }
    out
}

pub fn ppid_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized comm: state, ppid, ...
    let (_, after) = stat.rsplit_once(')')?;
    after.split_whitespace().nth(1)?.parse().ok()
}
