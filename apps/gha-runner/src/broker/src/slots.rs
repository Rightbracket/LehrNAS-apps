//! The runner's instances (§17.2, §17.3; spec-apps §7.1 in LehrNAS): which the
//! box runs, and each one's folder in the `slot` volume, where its config is
//! handed over on a socket and its stop marker is written.
//!
//! The broker mounts the whole volume at `/slots`; an instance mounts only its
//! own folder, at `/slot`. The box writes `.instances` at the top. With none
//! (an older box, which runs one runner and treats `slot` as a plain volume),
//! the one runner's folder is the volume itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The socket's name in an instance's folder.
pub const SOCKET: &str = "socket";
/// Written to ask an instance's start to stop its listener (§5).
pub const STOP: &str = "stop";
/// The box's list at the top of the volume.
pub const INSTANCES: &str = ".instances";

/// What the box says it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The instances wanted: 1 to `count`.
    pub count: u32,
    /// Instances still running that are to end after their work.
    pub leaving: BTreeSet<u32>,
    /// No `.instances`: one runner, its folder the volume itself.
    pub flat: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self::flat()
    }
}

impl Layout {
    /// One runner, as on a box that knows nothing of instances.
    #[must_use]
    pub fn flat() -> Self {
        Self {
            count: 1,
            leaving: BTreeSet::new(),
            flat: true,
        }
    }

    /// `.instances`'s text: `{"count": 3, "leaving": [4]}`. What can't be read
    /// is one instance, the safe reading: never more runners than asked for.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        #[derive(serde::Deserialize)]
        struct Said {
            count: u32,
            #[serde(default)]
            leaving: Vec<u32>,
        }
        match serde_json::from_str::<Said>(text) {
            Ok(s) if s.count >= 1 => Self {
                count: s.count,
                leaving: s.leaving.into_iter().filter(|n| *n >= 1).collect(),
                flat: false,
            },
            _ => Self {
                count: 1,
                leaving: BTreeSet::new(),
                flat: false,
            },
        }
    }

    /// The instance is to end: the box marked it leaving, or lowered the
    /// number past it.
    #[must_use]
    pub fn leaving(&self, n: u32) -> bool {
        n > self.count || self.leaving.contains(&n)
    }

    /// Every instance the broker has to look after this round: the wanted
    /// ones, and any that has a runner registered.
    #[must_use]
    pub fn instances(&self, registered: impl IntoIterator<Item = u32>) -> BTreeSet<u32> {
        (1..=self.count).chain(registered).collect()
    }
}

/// The volume, and the configs waiting on their sockets.
pub struct Slots {
    root: PathBuf,
    serving: Mutex<BTreeMap<u32, Arc<AtomicBool>>>,
}

impl Slots {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            serving: Mutex::new(BTreeMap::new()),
        }
    }

    /// The box's list, read fresh.
    #[must_use]
    pub fn layout(&self) -> Layout {
        match fs::read_to_string(self.root.join(INSTANCES)) {
            Ok(t) => Layout::parse(&t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Layout::flat(),
            Err(_) => Layout::parse(""),
        }
    }

    /// Instance `n`'s folder: the volume itself with no `.instances`.
    #[must_use]
    pub fn dir(&self, n: u32) -> PathBuf {
        if self.layout().flat {
            self.root.clone()
        } else {
            self.root.join(n.to_string())
        }
    }

    /// Ask instance `n` to stop its listener.
    ///
    /// # Errors
    /// The folder's.
    pub fn stop(&self, n: u32) -> Result<(), String> {
        let d = self.dir(n);
        fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        fs::write(d.join(STOP), "stop\n").map_err(|e| format!("{}: {e}", d.display()))
    }

    /// Hand instance `n` one config on its socket. The files stay in memory
    /// until the runner's start has taken each one, and then the socket goes;
    /// they never touch a disk. Its stop marker from the last runner goes
    /// first. A config still waiting there is withdrawn.
    ///
    /// # Errors
    /// The socket couldn't be made.
    pub fn offer(&self, n: u32, files: BTreeMap<String, Vec<u8>>) -> Result<(), String> {
        self.withdraw(n);
        let d = self.dir(n);
        fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        let _ = fs::remove_file(d.join(STOP));
        let path = d.join(SOCKET);
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let ino = fs::metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .ino();
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(n, cancel.clone());
        std::thread::spawn(move || serve(&listener, &path, ino, files, &cancel));
        Ok(())
    }

    /// Instance `n`'s waiting config, if any, is dropped and its socket goes.
    pub fn withdraw(&self, n: u32) {
        if let Some(c) = self
            .serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&n)
        {
            c.store(true, Ordering::SeqCst);
        }
    }

    /// Instance `n`'s config is still waiting to be taken.
    #[cfg(test)]
    #[must_use]
    pub fn waiting(&self, n: u32) -> bool {
        self.serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&n)
            .is_some_and(|c| !c.load(Ordering::SeqCst))
    }
}

/// The files of one config, in the order the runner's start takes them.
pub const FILES: [&str; 3] = [".runner", ".credentials", ".credentials_rsaparams"];

/// The runner's side (`gha-runner-broker take <socket> <dir>`): each file of
/// the config from the socket into `dir`, mode 0600, each one whole by a
/// rename and only once all three have arrived. The runner's image has the
/// broker's binary already, so its start needs no client of its own.
///
/// # Errors
/// The socket isn't answering, or a file didn't come.
pub fn take(socket: &Path, dir: &Path) -> Result<(), String> {
    let mut got = Vec::new();
    for name in FILES {
        let body = fetch(socket, name)?;
        got.push((name, body));
    }
    for (name, body) in got {
        let tmp = dir.join(format!("{name}.new"));
        let write = || -> std::io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            fs::rename(&tmp, dir.join(name))
        };
        write().map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn fetch(socket: &Path, name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    write!(s, "GET /{name} HTTP/1.0\r\nHost: broker\r\n\r\n").map_err(|e| e.to_string())?;
    let mut b = Vec::new();
    s.read_to_end(&mut b).map_err(|e| e.to_string())?;
    let at = b
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("{name}: no answer"))?;
    if !b.starts_with(b"HTTP/1.0 200") {
        return Err(format!("{name}: not offered"));
    }
    Ok(b[at + 4..].to_vec())
}

/// `GET /<name>` answers that file once; when every file has been taken, or
/// the config is withdrawn, the socket goes and the files with it. Only its own
/// socket: a newer config's, bound at the same path, stays.
fn serve(
    listener: &UnixListener,
    path: &Path,
    ino: u64,
    mut files: BTreeMap<String, Vec<u8>>,
    cancel: &AtomicBool,
) {
    while !files.is_empty() && !cancel.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => answer(&stream, &mut files),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    cancel.store(true, Ordering::SeqCst);
    if fs::metadata(path).is_ok_and(|m| m.ino() == ino) {
        let _ = fs::remove_file(path);
    }
}

fn answer(stream: &UnixStream, files: &mut BTreeMap<String, Vec<u8>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    // The rest of the request's head, unread otherwise.
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
        line.clear();
    }
    let name = first
        .strip_prefix("GET /")
        .and_then(|r| r.split(' ').next())
        .unwrap_or_default();
    let mut out = stream;
    match files.remove(name) {
        Some(body) => {
            let _ = write!(
                out,
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = out.write_all(&body);
        }
        None => {
            let _ = out.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        }
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn dir(name: &str) -> PathBuf {
        // Short: a socket's path has a length limit.
        let d = std::env::temp_dir().join(format!("gs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn get(path: &Path, name: &str) -> Option<Vec<u8>> {
        let mut s = UnixStream::connect(path).ok()?;
        write!(s, "GET /{name} HTTP/1.0\r\nHost: lehr\r\n\r\n").ok()?;
        let mut b = Vec::new();
        s.read_to_end(&mut b).ok()?;
        let at = b.windows(4).position(|w| w == b"\r\n\r\n")?;
        b.starts_with(b"HTTP/1.0 200").then(|| b[at + 4..].to_vec())
    }

    fn wait_gone(p: &Path) {
        for _ in 0..50 {
            if !p.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{} still there", p.display());
    }

    #[test]
    fn no_instances_file_is_one_runner_in_the_volume_itself() {
        let d = dir("flat");
        let s = Slots::new(&d);
        assert_eq!(s.layout(), Layout::flat());
        assert_eq!(s.dir(1), d);
    }

    #[test]
    fn the_box_list_says_how_many_and_which_are_leaving() {
        let l = Layout::parse(r#"{"count": 3, "leaving": [4]}"#);
        assert_eq!(l.count, 3);
        assert!(l.leaving(4) && l.leaving(5) && !l.leaving(3));
        assert_eq!(
            l.instances([4, 2]).into_iter().collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let d = dir("list");
        fs::write(d.join(INSTANCES), r#"{"count": 2, "leaving": []}"#).unwrap();
        assert_eq!(Slots::new(&d).dir(2), d.join("2"));
    }

    #[test]
    fn a_list_that_cant_be_read_is_one_instance_never_more() {
        for t in ["", "{", r#"{"count": 0}"#, r#"{"count": "9"}"#] {
            let l = Layout::parse(t);
            assert_eq!((l.count, l.flat), (1, false), "{t:?}");
        }
    }

    #[test]
    fn a_config_is_taken_once_per_file_then_the_socket_goes() {
        let d = dir("offer");
        let s = Slots::new(&d);
        fs::write(d.join(STOP), "stop\n").unwrap();
        let files = BTreeMap::from([
            (".runner".to_owned(), b"{}".to_vec()),
            (".credentials".to_owned(), b"c".to_vec()),
        ]);
        s.offer(1, files).unwrap();
        assert!(!d.join(STOP).exists(), "the last runner's stop goes first");
        let sock = d.join(SOCKET);
        assert_eq!(
            fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(s.waiting(1));
        assert_eq!(get(&sock, ".credentials").as_deref(), Some(&b"c"[..]));
        assert_eq!(get(&sock, ".credentials"), None, "once only");
        assert_eq!(get(&sock, ".runner").as_deref(), Some(&b"{}"[..]));
        wait_gone(&sock);
        assert!(!s.waiting(1));
    }

    #[test]
    fn each_instance_has_its_own_socket_and_a_withdrawn_config_is_gone() {
        let d = dir("two");
        fs::write(d.join(INSTANCES), r#"{"count": 2}"#).unwrap();
        let s = Slots::new(&d);
        let one = |b: &[u8]| BTreeMap::from([(".runner".to_owned(), b.to_vec())]);
        s.offer(1, one(b"first")).unwrap();
        s.offer(2, one(b"second")).unwrap();
        assert_eq!(
            get(&d.join("2").join(SOCKET), ".runner").as_deref(),
            Some(&b"second"[..])
        );
        s.withdraw(1);
        wait_gone(&d.join("1").join(SOCKET));
        assert!(!s.waiting(1));
    }

    #[test]
    fn a_new_config_replaces_one_still_waiting() {
        let d = dir("again");
        let s = Slots::new(&d);
        let one = |b: &[u8]| BTreeMap::from([(".runner".to_owned(), b.to_vec())]);
        s.offer(1, one(b"old")).unwrap();
        s.offer(1, one(b"new")).unwrap();
        // The old server may take a moment to notice; the socket is the new one.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            get(&d.join(SOCKET), ".runner").as_deref(),
            Some(&b"new"[..])
        );
    }

    #[test]
    fn the_runners_take_gets_all_three_files_private_or_none() {
        let d = dir("take");
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        let s = Slots::new(&d);
        assert!(take(&d.join(SOCKET), &out).is_err(), "nothing offered yet");
        let files = FILES
            .iter()
            .map(|f| ((*f).to_owned(), f.as_bytes().to_vec()))
            .collect();
        s.offer(1, files).unwrap();
        take(&d.join(SOCKET), &out).unwrap();
        for f in FILES {
            assert_eq!(fs::read(out.join(f)).unwrap(), f.as_bytes());
            assert_eq!(
                fs::metadata(out.join(f)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        wait_gone(&d.join(SOCKET));
        // A config missing a file writes none of them.
        let out2 = d.join("out2");
        fs::create_dir(&out2).unwrap();
        s.offer(1, BTreeMap::from([(".runner".to_owned(), b"x".to_vec())]))
            .unwrap();
        assert!(take(&d.join(SOCKET), &out2).is_err());
        assert_eq!(fs::read_dir(&out2).unwrap().count(), 0);
    }

    #[test]
    fn stop_marks_only_its_own_instance() {
        let d = dir("stop");
        fs::write(d.join(INSTANCES), r#"{"count": 2}"#).unwrap();
        let s = Slots::new(&d);
        s.stop(2).unwrap();
        assert!(d.join("2").join(STOP).exists());
        assert!(!d.join("1").join(STOP).exists());
    }
}
