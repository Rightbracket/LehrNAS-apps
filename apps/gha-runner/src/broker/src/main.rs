//! The GitHub runner app's broker (`docs/spec-runner-app.md` in LehrNAS): holds
//! the token, registers one just-in-time runner at a time, and serves the status
//! page. It is the only service that reads `GITHUB_TOKEN`.

mod clock;
mod engine;
mod github;
mod jit;
mod pod;
mod policy;
mod state;
mod web;

use engine::{Engine, Shared};
use github::{Curl, GitHub, Registered, ScopeFacts};
use policy::{RunnerSeen, Scope};
use std::fmt;
use std::net::UdpSocket;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PORT: u16 = 8711;
/// Where the runner's start sends the lines of its output the page wants.
const REPORT_PORT: u16 = 8712;

/// The environment. Its `Debug` leaves the token out, so no logging of this
/// value can print it.
struct Settings {
    token: String,
    scope: String,
    api: String,
    labels: Vec<String>,
    job_hours: u64,
    reset: Option<String>,
    version: String,
    config: String,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field(
                "token",
                &if self.token.is_empty() {
                    "unset"
                } else {
                    "set"
                },
            )
            .field("scope", &self.scope)
            .field("labels", &self.labels)
            .field("job_hours", &self.job_hours)
            .finish_non_exhaustive()
    }
}

/// `RUNNER_LABELS` on top of the ones every self-hosted runner carries.
fn labels(extra: &str, arch: &str) -> Vec<String> {
    let arch = if arch == "aarch64" { "ARM64" } else { "X64" };
    let mut out = vec![
        "self-hosted".to_owned(),
        "Linux".to_owned(),
        arch.to_owned(),
    ];
    out.extend(
        extra
            .split(',')
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned),
    );
    out
}

fn settings() -> Settings {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    Settings {
        token: var("GITHUB_TOKEN"),
        scope: var("RUNNER_SCOPE"),
        api: std::env::var("RUNNER_GITHUB_API")
            .unwrap_or_else(|_| "https://api.github.com".to_owned()),
        labels: labels(
            &std::env::var("RUNNER_LABELS").unwrap_or_else(|_| "lehrnas".to_owned()),
            std::env::consts::ARCH,
        ),
        job_hours: var("RUNNER_JOB_HOURS")
            .parse()
            .ok()
            .filter(|h| *h > 0)
            .unwrap_or(6),
        reset: std::env::var("RUNNER_RESET_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty()),
        version: std::fs::read_to_string("/opt/runner/VERSION")
            .map_or_else(|_| "unknown".to_owned(), |v| v.trim().to_owned()),
        config: std::env::var("RUNNER_CONFIG_DIR").unwrap_or_else(|_| "/config".to_owned()),
    }
}

/// A new runner's name: `lehrnas-` and eight hex digits, new every time, as
/// GitHub wants them unique.
fn runner_name() -> String {
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        b = clock::now().to_le_bytes()[..4]
            .try_into()
            .unwrap_or_default();
    }
    format!("lehrnas-{:08x}", u32::from_le_bytes(b))
}

/// GitHub, when it can't be asked: every call says why.
struct NoGitHub(String);

impl GitHub for NoGitHub {
    fn facts(&self, _: &Scope) -> Result<ScopeFacts, String> {
        Err(self.0.clone())
    }
    fn fork_approval(&self, _: &Scope) -> Result<String, String> {
        Err(self.0.clone())
    }
    fn register(&self, _: &Scope, _: u64, _: &str, _: &[String]) -> Result<Registered, String> {
        Err(self.0.clone())
    }
    fn runner(&self, _: &Scope, _: u64) -> Result<RunnerSeen, String> {
        Err(self.0.clone())
    }
    fn remove(&self, _: &Scope, _: u64) -> Result<(), String> {
        Err(self.0.clone())
    }
    fn releases(&self) -> Result<Vec<(String, u64)>, String> {
        Err(self.0.clone())
    }
    fn queued(&self, _: &Scope) -> Option<u64> {
        None
    }
}

/// The real GitHub, or the reason there isn't one.
fn connect(s: &Settings) -> Arc<dyn GitHub> {
    if s.token.is_empty() {
        return Arc::new(NoGitHub(
            "GITHUB_TOKEN isn't set: set it in this app's settings on your LehrNAS".to_owned(),
        ));
    }
    match Curl::new(&s.api, &s.token) {
        Ok(c) => Arc::new(c),
        Err(e) => Arc::new(NoGitHub(e)),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        println!("FAULT | gha-runner | the broker stopped: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let s = settings();
    let file = state::File::new(Path::new(&s.config));
    let mut saved = file.load()?;
    if let Some(v) = &s.reset
        && saved.reset_seen.as_ref() != Some(v)
    {
        saved.password_hash = None;
        saved.sessions.clear();
        saved.reset_seen = Some(v.clone());
        file.save(&saved)?;
        println!(
            "PASSWORD | gha-runner | cleared by RUNNER_RESET_PASSWORD; the next visit sets one"
        );
    }
    let scope = Scope::parse(&s.scope);
    let shared = Shared::new(saved, file);
    let github = connect(&s);
    let engine = Engine {
        github: github.clone(),
        pod: Arc::new(pod::RealPod::standard()),
        scope: scope.clone(),
        labels: s.labels.clone(),
        job_limit_secs: s.job_hours * 3600,
        shared: shared.clone(),
        names: Box::new(runner_name),
    };
    std::thread::spawn(move || engine.run());

    let reports = UdpSocket::bind(("127.0.0.1", REPORT_PORT))
        .map_err(|e| format!("port {REPORT_PORT}: {e}"))?;
    let heard = shared.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(n) = reports.recv(&mut buf) {
            heard.heard(&String::from_utf8_lossy(&buf[..n]));
        }
    });

    let exposure_scope = scope.clone();
    let exposure_hub = github;
    let page = Arc::new(web::Page {
        shared,
        scope,
        version: s.version.clone(),
        exposure: Box::new(move || {
            let sc = exposure_scope.as_ref().ok()?;
            exposure_hub.facts(sc).ok().map(|f| f.exposure)
        }),
        wrong_password_delay: Duration::from_secs(1),
    });
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT))
        .await
        .map_err(|e| format!("port {PORT}: {e}"))?;
    println!("BROKER | gha-runner | listening on {PORT}; {s:?}");
    axum::serve(listener, web::router(page))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_never_print_the_token() {
        let s = Settings {
            token: "ghp_s3cret".into(),
            scope: "me/thing".into(),
            api: String::new(),
            labels: vec![],
            job_hours: 6,
            reset: None,
            version: String::new(),
            config: String::new(),
        };
        let shown = format!("{s:?}");
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(shown.contains("set"), "{shown}");
    }

    #[test]
    fn labels_start_with_what_every_runner_carries() {
        assert_eq!(
            labels("lehrnas, rig ,", "x86_64"),
            ["self-hosted", "Linux", "X64", "lehrnas", "rig"]
        );
        assert_eq!(labels("", "aarch64"), ["self-hosted", "Linux", "ARM64"]);
    }

    #[test]
    fn runner_names_differ() {
        assert_ne!(runner_name(), runner_name());
        assert!(runner_name().starts_with("lehrnas-"));
    }

    #[test]
    fn no_token_says_so_in_every_call() {
        let s = Settings {
            token: String::new(),
            scope: "me/thing".into(),
            api: "https://api.github.com".into(),
            labels: vec![],
            job_hours: 6,
            reset: None,
            version: String::new(),
            config: String::new(),
        };
        let e = connect(&s)
            .facts(&Scope::parse("me/thing").unwrap())
            .unwrap_err();
        assert!(e.contains("GITHUB_TOKEN"), "{e}");
    }
}
