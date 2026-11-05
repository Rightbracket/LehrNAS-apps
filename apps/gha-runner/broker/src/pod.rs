//! What the broker does to its pod (§3, §6): the IPC the services share, the
//! files of one JIT config, and `cache`. It only ever writes and deletes in the
//! shared places: nothing a job could have left there is read.

use crate::policy::{self, Grant};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the runner's start looks; it appears whole, by a rename.
pub const JIT_DIR: &str = "lehr-runner-jit";
/// Written to ask the runner's start to stop its listener.
pub const STOP_FILE: &str = "lehr-runner-stop";

/// The seam the tests fake.
pub trait Pod: Send + Sync {
    /// Empty `/dev/shm`, `/dev/mqueue` and SysV IPC.
    ///
    /// # Errors
    /// Something of the last job couldn't be removed.
    fn clean(&self) -> Result<(), String>;
    /// One config's files, mode 0600, for the runner's start to take.
    ///
    /// # Errors
    /// The tmpfs's.
    fn write_config(&self, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String>;
    /// Ask the runner to stop its listener.
    ///
    /// # Errors
    /// The tmpfs's.
    fn stop_runner(&self) -> Result<(), String>;
    fn cache_bytes(&self) -> u64;
    /// # Errors
    /// Something in `cache` couldn't be removed.
    fn clear_cache(&self) -> Result<(), String>;
    fn grants(&self) -> Vec<Grant>;
}

/// The real places.
pub struct RealPod {
    pub shm: PathBuf,
    pub mqueue: PathBuf,
    pub cache: PathBuf,
    pub mountinfo: PathBuf,
    /// `ipcrm`: a parameter so a test can run on any machine.
    pub ipcrm: Vec<String>,
}

impl RealPod {
    #[must_use]
    pub fn standard() -> Self {
        Self {
            shm: "/dev/shm".into(),
            mqueue: "/dev/mqueue".into(),
            cache: "/cache".into(),
            mountinfo: "/proc/self/mountinfo".into(),
            ipcrm: vec!["ipcrm".into(), "-a".into()],
        }
    }
}

/// Everything inside `dir`, dot files and links too, never through a link. A
/// directory that isn't there is empty.
fn empty_dir(dir: &Path) -> Result<(), String> {
    let list = match fs::read_dir(dir) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in list {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        let is_dir = entry.file_type().map_err(|e| e.to_string())?.is_dir();
        let gone = if is_dir {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        gone.map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

/// The bytes under `dir`, not through links; what can't be read counts nothing.
fn bytes_under(dir: &Path) -> u64 {
    let Ok(list) = fs::read_dir(dir) else {
        return 0;
    };
    list.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => bytes_under(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.blocks() * 512),
            _ => 0,
        })
        .sum()
}

impl Pod for RealPod {
    fn clean(&self) -> Result<(), String> {
        empty_dir(&self.shm)?;
        empty_dir(&self.mqueue)?;
        let (cmd, args) = self.ipcrm.split_first().ok_or("no ipcrm")?;
        let o = Command::new(cmd)
            .args(args)
            .output()
            .map_err(|e| format!("{cmd}: {e}"))?;
        if o.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{cmd}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))
        }
    }

    fn write_config(&self, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
        let tmp = self.shm.join(format!("{JIT_DIR}.new"));
        let write = || -> std::io::Result<()> {
            fs::DirBuilder::new().mode(0o700).create(&tmp)?;
            for (name, body) in files {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(tmp.join(name))?;
                f.write_all(body)?;
            }
            fs::rename(&tmp, self.shm.join(JIT_DIR))
        };
        write().map_err(|e| format!("the JIT config couldn't be written: {e}"))
    }

    fn stop_runner(&self) -> Result<(), String> {
        fs::write(self.shm.join(STOP_FILE), "stop\n").map_err(|e| e.to_string())
    }

    fn cache_bytes(&self) -> u64 {
        bytes_under(&self.cache)
    }

    fn clear_cache(&self) -> Result<(), String> {
        empty_dir(&self.cache)
    }

    fn grants(&self) -> Vec<Grant> {
        // Unreadable, it fails closed: an unknown grant counts as read-write.
        fs::read_to_string(&self.mountinfo).map_or_else(
            |_| {
                vec![Grant {
                    share: "(the mount table couldn't be read)".into(),
                    read_write: true,
                }]
            },
            |t| policy::grants_in(&t),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    fn pod(name: &str) -> (RealPod, PathBuf) {
        let d = std::env::temp_dir().join(format!("gha-pod-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for s in ["shm", "mq", "cache"] {
            fs::create_dir_all(d.join(s)).unwrap();
        }
        (
            RealPod {
                shm: d.join("shm"),
                mqueue: d.join("mq"),
                cache: d.join("cache"),
                mountinfo: d.join("mountinfo"),
                ipcrm: vec!["true".into()],
            },
            d,
        )
    }

    #[test]
    fn clean_takes_everything_dot_files_included_and_never_follows_a_link() {
        let (p, d) = pod("clean");
        fs::create_dir(d.join("keep")).unwrap();
        fs::write(d.join("keep/precious"), "x").unwrap();
        fs::write(p.shm.join(".hidden"), "x").unwrap();
        fs::create_dir(p.shm.join("dir")).unwrap();
        fs::write(p.shm.join("dir/f"), "x").unwrap();
        symlink(d.join("keep"), p.shm.join("link")).unwrap();
        fs::write(p.mqueue.join("q"), "x").unwrap();
        p.clean().unwrap();
        assert_eq!(fs::read_dir(&p.shm).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&p.mqueue).unwrap().count(), 0);
        assert!(
            d.join("keep/precious").exists(),
            "a link is removed, not followed"
        );
    }

    #[test]
    fn a_failing_ipcrm_stops_the_clean() {
        let (mut p, _) = pod("ipcrm");
        p.ipcrm = vec!["false".into()];
        assert!(p.clean().is_err());
    }

    #[test]
    fn a_config_appears_whole_private_and_leaves_nothing_else() {
        let (p, _) = pod("write");
        let files = BTreeMap::from([
            (".runner".to_owned(), b"{}".to_vec()),
            (".credentials".to_owned(), b"c".to_vec()),
        ]);
        p.write_config(&files).unwrap();
        let at = p.shm.join(JIT_DIR);
        assert_eq!(fs::read(at.join(".credentials")).unwrap(), b"c");
        assert_eq!(
            fs::metadata(&at).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(at.join(".runner"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_dir(&p.shm).unwrap().count(),
            1,
            "no temporary left"
        );
    }

    #[test]
    fn clearing_the_cache_empties_it_and_sizes_read() {
        let (p, _) = pod("cache");
        fs::create_dir(p.cache.join("rustup")).unwrap();
        fs::write(p.cache.join("rustup/big"), vec![1u8; 8192]).unwrap();
        assert!(p.cache_bytes() >= 8192);
        p.clear_cache().unwrap();
        assert_eq!(p.cache_bytes(), 0);
        assert!(p.cache.exists(), "the volume's own folder stays");
    }

    #[test]
    fn an_unreadable_mount_table_counts_as_a_read_write_grant() {
        let (p, d) = pod("mounts");
        assert!(p.grants()[0].read_write);
        fs::write(
            d.join("mountinfo"),
            "1 2 0:3 / /shares/x ro - btrfs /d rw\n",
        )
        .unwrap();
        assert_eq!(
            p.grants(),
            vec![Grant {
                share: "x".into(),
                read_write: false
            }]
        );
    }
}
