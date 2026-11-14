//! What the broker does to its pod (§3, §6, §17.3): the IPC the services
//! share, each instance's config and stop marker in the `slot` volume, and
//! `cache`. It only ever writes and deletes in the shared places: nothing a job
//! could have left there is read.

use crate::policy::{self, Grant};
use crate::slots::{Layout, Slots};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The seam the tests fake.
pub trait Pod: Send + Sync {
    /// Empty `/dev/shm`, `/dev/mqueue` and SysV IPC.
    ///
    /// # Errors
    /// Something of the last job couldn't be removed.
    fn clean(&self) -> Result<(), String>;
    /// The instances the box runs (§17.3), read fresh.
    fn layout(&self) -> Layout;
    /// One config's files for instance `n`'s start to take, over its socket
    /// (§17.3): in memory only, and gone once taken.
    ///
    /// # Errors
    /// The socket's.
    fn write_config(&self, n: u32, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String>;
    /// Instance `n`'s waiting config, if any, is dropped (its runner left).
    fn withdraw(&self, n: u32);
    /// Ask instance `n`'s runner to stop its listener.
    ///
    /// # Errors
    /// The volume's.
    fn stop_runner(&self, n: u32) -> Result<(), String>;
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
    pub slots: Slots,
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
            slots: Slots::new("/slots"),
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

    fn layout(&self) -> Layout {
        self.slots.layout()
    }

    fn write_config(&self, n: u32, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
        self.slots
            .offer(n, files.clone())
            .map_err(|e| format!("the JIT config couldn't be offered: {e}"))
    }

    fn withdraw(&self, n: u32) {
        self.slots.withdraw(n);
    }

    fn stop_runner(&self, n: u32) -> Result<(), String> {
        self.slots.stop(n)
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
    use std::os::unix::fs::symlink;

    fn pod(name: &str) -> (RealPod, PathBuf) {
        let d = std::env::temp_dir().join(format!("gha-pod-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for s in ["shm", "mq", "cache", "slots"] {
            fs::create_dir_all(d.join(s)).unwrap();
        }
        (
            RealPod {
                shm: d.join("shm"),
                mqueue: d.join("mq"),
                cache: d.join("cache"),
                slots: Slots::new(d.join("slots")),
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
    fn a_config_goes_on_the_instances_socket_never_into_shared_memory() {
        let (p, d) = pod("write");
        let files = BTreeMap::from([(".credentials".to_owned(), b"c".to_vec())]);
        p.write_config(1, &files).unwrap();
        assert!(d.join("slots").join(crate::slots::SOCKET).exists());
        assert_eq!(
            fs::read_dir(&p.shm).unwrap().count(),
            0,
            "nothing in /dev/shm"
        );
        p.withdraw(1);
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
