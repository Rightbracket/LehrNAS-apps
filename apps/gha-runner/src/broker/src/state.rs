//! The broker's state in `config` (§2): one small JSON file, written only when
//! it changes, so an idle app writes nothing. It holds no token: that stays in
//! the environment (§3).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The page keeps the last 50 (§8).
pub const JOBS_KEPT: usize = 50;
pub const SESSION_SECS: u64 = 30 * 86_400;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    pub repository: String,
    pub started: u64,
    pub finished: u64,
    pub result: String,
}

/// The runner registered now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub written: u64,
    #[serde(default)]
    pub busy_since: Option<u64>,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub stop_asked: bool,
    /// This runner's start has reported its versions (§13.4).
    #[serde(default)]
    pub reported: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// A random 32-byte value in hex; only the broker mounts `config`.
    pub token: String,
    pub expires: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// The last value of `RUNNER_RESET_PASSWORD` acted on, so a restart with
    /// the key still set doesn't clear the password again.
    #[serde(default)]
    pub reset_seen: Option<String>,
    /// The repository name typed for a public repository (§7).
    #[serde(default)]
    pub confirmed: Option<String>,
    #[serde(default)]
    pub drain: bool,
    #[serde(default)]
    pub clear_cache: bool,
    #[serde(default)]
    pub current: Option<Runner>,
    #[serde(default)]
    pub jobs: Vec<Job>,
    /// actions/runner's releases as last read, and when.
    #[serde(default)]
    pub releases: Vec<(String, u64)>,
    #[serde(default)]
    pub releases_read: u64,
    /// The runner service's version and the app release its image was built
    /// from, as last heard (§13.4). Kept here because an idle runner reports
    /// only once, and the broker can restart without it. Display only.
    #[serde(default)]
    pub runner_version: Option<String>,
    #[serde(default)]
    pub runner_release: Option<String>,
}

impl State {
    /// A finished job, the newest first, the list capped.
    pub fn push_job(&mut self, job: Job) {
        self.jobs.insert(0, job);
        self.jobs.truncate(JOBS_KEPT);
    }
}

/// Where the state lives.
#[derive(Clone, Debug)]
pub struct File(PathBuf);

impl File {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self(dir.join("state.json"))
    }

    /// The saved state; none yet is an empty one. A file that can't be read is
    /// an error, not a fresh start: that would forget the registered runner and
    /// the password.
    ///
    /// # Errors
    /// The file is there and unreadable.
    pub fn load(&self) -> Result<State, String> {
        match fs::read(&self.0) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", self.0.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(format!("{}: {e}", self.0.display())),
        }
    }

    /// Whole or not at all: a temporary file, synced, renamed over.
    ///
    /// # Errors
    /// The disk's.
    pub fn save(&self, s: &State) -> Result<(), String> {
        let tmp = self.0.with_extension("json.new");
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(s)?)?;
            f.sync_all()?;
            fs::rename(&tmp, &self.0)
        };
        write().map_err(|e| format!("{}: {e}", self.0.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gha-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_state_round_trips_and_a_missing_file_is_empty() {
        let d = dir("trip");
        let f = File::new(&d);
        assert_eq!(f.load().unwrap(), State::default());
        let s = State {
            drain: true,
            confirmed: Some("a/b".into()),
            ..State::default()
        };
        f.save(&s).unwrap();
        assert_eq!(f.load().unwrap(), s);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_fresh_start() {
        let d = dir("bad");
        fs::write(d.join("state.json"), "{not json").unwrap();
        assert!(File::new(&d).load().is_err());
    }

    #[test]
    fn the_job_list_keeps_the_newest_fifty() {
        let mut s = State::default();
        for i in 0..60 {
            s.push_job(Job {
                started: i,
                ..Job::default()
            });
        }
        assert_eq!(s.jobs.len(), JOBS_KEPT);
        assert_eq!(s.jobs[0].started, 59);
    }
}
