//! GitHub's REST API through `curl`, as lehrd does (D286: no TLS stack in the
//! binary, and no new crates). The token goes in on curl's stdin, so it's never
//! on a command line, and any text that comes back is scrubbed of it.

use crate::policy::{Exposure, RunnerSeen, Scope};
use serde_json::Value;
use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::process::{Command, Stdio};

/// A runner just registered.
#[derive(Clone)]
pub struct Registered {
    pub id: u64,
    pub name: String,
    pub encoded: String,
}

impl fmt::Debug for Registered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Registered {{ id: {}, name: {} }}", self.id, self.name)
    }
}

/// What the scope is, and the runner group to register in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeFacts {
    pub exposure: Exposure,
    pub group_id: u64,
}

/// The calls the broker makes. Tests answer them with a fake.
pub trait GitHub: Send + Sync {
    /// # Errors
    /// GitHub couldn't be reached or wouldn't say.
    fn facts(&self, scope: &Scope) -> Result<ScopeFacts, String>;
    /// The repository's fork-approval setting.
    ///
    /// # Errors
    /// As [`GitHub::facts`].
    fn fork_approval(&self, scope: &Scope) -> Result<String, String>;
    /// # Errors
    /// As [`GitHub::facts`].
    fn register(
        &self,
        scope: &Scope,
        group: u64,
        name: &str,
        labels: &[String],
    ) -> Result<Registered, String>;
    /// # Errors
    /// As [`GitHub::facts`].
    fn runner(&self, scope: &Scope, id: u64) -> Result<RunnerSeen, String>;
    /// # Errors
    /// As [`GitHub::facts`]; GitHub refuses while the runner has a job.
    fn remove(&self, scope: &Scope, id: u64) -> Result<(), String>;
    /// actions/runner's releases: tag and `published_at`, newest first.
    ///
    /// # Errors
    /// As [`GitHub::facts`].
    fn releases(&self) -> Result<Vec<(String, u64)>, String>;
    /// Workflow runs waiting in a repository. The token may not be allowed to
    /// say (it needs no Actions permission), which reads as `None`.
    fn queued(&self, scope: &Scope) -> Option<u64>;
}

/// The real one.
pub struct Curl {
    api: String,
    token: String,
}

/// A value inside curl's config file: quoted, with its escapes.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The config curl reads on stdin: the secret and the body stay off argv.
#[must_use]
pub fn curl_config(token: &str, body: Option<&str>) -> String {
    let mut c = format!(
        "header = {}\nheader = \"Accept: application/vnd.github+json\"\nheader = \"X-GitHub-Api-Version: 2022-11-28\"\nuser-agent = \"lehrnas-gha-runner\"\n",
        quoted(&format!("Authorization: Bearer {token}"))
    );
    if let Some(b) = body {
        c.push_str("header = \"Content-Type: application/json\"\n");
        let _ = writeln!(c, "data-binary = {}", quoted(b));
    }
    c
}

/// Any text with the token taken out, in case an error echoes a request.
#[must_use]
pub fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "[token]")
    }
}

/// `https://`, or plain HTTP to this machine for a test's fake GitHub.
///
/// # Errors
/// Anywhere else the token shouldn't travel to.
pub fn api_ok(api: &str) -> Result<(), String> {
    let local = ["http://127.0.0.1:", "http://localhost:"]
        .iter()
        .any(|p| api.starts_with(p));
    if api.starts_with("https://") || local {
        Ok(())
    } else {
        Err(format!("RUNNER_GITHUB_API is {api}: it must be https"))
    }
}

/// Encode one path segment (a repository name can't need it; a group's can).
fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn base(scope: &Scope) -> String {
    match scope {
        Scope::Repo { owner, name } => format!("repos/{}/{}", segment(owner), segment(name)),
        Scope::OrgGroup { org, .. } => format!("orgs/{}", segment(org)),
    }
}

/// The answer to `GET /repos/...`: `private`, or `visibility` when it's there.
/// Anything but a clear answer is an error: never read as closed.
///
/// # Errors
/// A body that doesn't say.
pub fn exposure_of_repo(v: &Value) -> Result<Exposure, String> {
    match v["visibility"].as_str() {
        Some("public") => Ok(Exposure::Public),
        Some("private" | "internal") => Ok(Exposure::Closed),
        Some(other) => Err(format!("GitHub calls the repository {other:?}")),
        None => match v["private"].as_bool() {
            Some(true) => Ok(Exposure::Closed),
            Some(false) => Ok(Exposure::Public),
            None => Err("GitHub didn't say whether the repository is public".to_owned()),
        },
    }
}

/// A runner's JSON as the page's words.
#[must_use]
pub fn seen(v: &Value) -> RunnerSeen {
    if v["busy"].as_bool() == Some(true) {
        RunnerSeen::Busy
    } else if v["status"].as_str() == Some("online") {
        RunnerSeen::Idle
    } else {
        RunnerSeen::Offline
    }
}

impl Curl {
    /// # Errors
    /// A token or address that can't be passed safely.
    pub fn new(api: &str, token: &str) -> Result<Self, String> {
        api_ok(api)?;
        if token.chars().any(char::is_control) {
            return Err("GITHUB_TOKEN has a line break or a control character in it".to_owned());
        }
        Ok(Self {
            api: api.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    /// One call: the status and the parsed body (`Null` when there's none).
    /// Never follows a redirect, which would carry the token elsewhere.
    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, Value), String> {
        let url = format!("{}/{path}", self.api);
        let mut child = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--max-time",
                "30",
                "--config",
                "-",
            ])
            .args(["--request", method, "--write-out", "\n%{http_code}", &url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        let config = curl_config(&self.token, body);
        let mut stdin = child.stdin.take().ok_or("curl: no stdin")?;
        stdin
            .write_all(config.as_bytes())
            .map_err(|e| format!("curl: {e}"))?;
        drop(stdin);
        let o = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
        if !o.status.success() {
            let e = String::from_utf8_lossy(&o.stderr);
            return Err(scrub(
                &format!("GitHub couldn't be reached: {}", e.trim()),
                &self.token,
            ));
        }
        let text = String::from_utf8_lossy(&o.stdout);
        let (json, code) = text.rsplit_once('\n').ok_or("curl: no status")?;
        let code: u16 = code.trim().parse().map_err(|_| "curl: no status")?;
        Ok((code, serde_json::from_str(json).unwrap_or(Value::Null)))
    }

    fn ok(&self, method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
        let (code, v) = self.call(method, path, body)?;
        if (200..300).contains(&code) {
            return Ok(v);
        }
        let msg = v["message"].as_str().unwrap_or("no message");
        Err(scrub(
            &format!("GitHub answered {code} to {method} {path}: {msg}"),
            &self.token,
        ))
    }
}

impl GitHub for Curl {
    fn facts(&self, scope: &Scope) -> Result<ScopeFacts, String> {
        match scope {
            Scope::Repo { .. } => Ok(ScopeFacts {
                exposure: exposure_of_repo(&self.ok("GET", &base(scope), None)?)?,
                // A repository's own runners are all in group 1 (REST docs).
                group_id: 1,
            }),
            Scope::OrgGroup { group, .. } => {
                let path = format!("{}/actions/runner-groups?per_page=100", base(scope));
                let v = self.ok("GET", &path, None)?;
                let found = v["runner_groups"]
                    .as_array()
                    .and_then(|g| {
                        g.iter()
                            .find(|x| x["name"].as_str() == Some(group.as_str()))
                    })
                    .ok_or_else(|| {
                        format!("the organisation has no runner group called {group}")
                    })?;
                let exposure = match found["allows_public_repositories"].as_bool() {
                    Some(false) => Exposure::Closed,
                    Some(true) => Exposure::GroupAllowsPublic,
                    None => {
                        return Err(
                            "GitHub didn't say whether the group allows public repositories"
                                .to_owned(),
                        );
                    }
                };
                let group_id = found["id"].as_u64().ok_or("GitHub gave the group no id")?;
                Ok(ScopeFacts { exposure, group_id })
            }
        }
    }

    fn fork_approval(&self, scope: &Scope) -> Result<String, String> {
        let path = format!(
            "{}/actions/permissions/fork-pr-contributor-approval",
            base(scope)
        );
        let v = self.ok("GET", &path, None)?;
        v["approval_policy"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "GitHub didn't give the fork approval setting".to_owned())
    }

    fn register(
        &self,
        scope: &Scope,
        group: u64,
        name: &str,
        labels: &[String],
    ) -> Result<Registered, String> {
        // The real path, not the runner's relative default: `actions/checkout`
        // scopes its credentials to the workspace's path, and git compares that
        // with the real one, so through a link it has no token (LehrNAS #300).
        let body = serde_json::json!({
            "name": name,
            "runner_group_id": group,
            "labels": labels,
            "work_folder": "/tmp/runner/_work",
        })
        .to_string();
        let path = format!("{}/actions/runners/generate-jitconfig", base(scope));
        let v = self.ok("POST", &path, Some(&body))?;
        Ok(Registered {
            id: v["runner"]["id"]
                .as_u64()
                .ok_or("GitHub gave the runner no id")?,
            name: name.to_owned(),
            encoded: v["encoded_jit_config"]
                .as_str()
                .ok_or("GitHub gave no config")?
                .to_owned(),
        })
    }

    fn runner(&self, scope: &Scope, id: u64) -> Result<RunnerSeen, String> {
        let (code, v) = self.call(
            "GET",
            &format!("{}/actions/runners/{id}", base(scope)),
            None,
        )?;
        match code {
            404 => Ok(RunnerSeen::Gone),
            200..=299 => Ok(seen(&v)),
            _ => Err(format!(
                "GitHub answered {code} about the runner: {}",
                scrub(v["message"].as_str().unwrap_or("no message"), &self.token)
            )),
        }
    }

    fn remove(&self, scope: &Scope, id: u64) -> Result<(), String> {
        self.ok(
            "DELETE",
            &format!("{}/actions/runners/{id}", base(scope)),
            None,
        )
        .map(drop)
    }

    fn releases(&self) -> Result<Vec<(String, u64)>, String> {
        let v = self.ok("GET", "repos/actions/runner/releases?per_page=30", None)?;
        Ok(v.as_array()
            .map(|r| {
                r.iter()
                    .filter(|x| {
                        x["prerelease"].as_bool() != Some(true)
                            && x["draft"].as_bool() != Some(true)
                    })
                    .filter_map(|x| {
                        Some((
                            x["tag_name"].as_str()?.to_owned(),
                            crate::clock::parse_rfc3339(x["published_at"].as_str()?)?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn queued(&self, scope: &Scope) -> Option<u64> {
        if !matches!(scope, Scope::Repo { .. }) {
            return None;
        }
        let path = format!("{}/actions/runs?status=queued&per_page=1", base(scope));
        self.ok("GET", &path, None).ok()?["total_count"].as_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead as _, BufReader, Read as _};
    use std::net::TcpListener;

    /// A one-shot fake GitHub on loopback: real curl, a real socket. Answers
    /// `status` with `body` and gives back the request it read.
    fn fake(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let (mut head, mut len) = (String::new(), 0usize);
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if let Some(v) = line.to_lowercase().strip_prefix("content-length: ") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut b = vec![0; len];
            r.read_exact(&mut b).unwrap();
            head.push_str(&String::from_utf8_lossy(&b));
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            std::io::Write::write_all(&mut s, reply.as_bytes()).unwrap();
            head
        });
        (url, t)
    }

    fn scope() -> Scope {
        Scope::parse("me/thing").unwrap()
    }

    #[test]
    fn curl_sends_the_token_in_a_header_and_reads_the_answer() {
        let (url, t) = fake(200, r#"{"visibility":"public"}"#);
        let f = Curl::new(&url, "s3cret").unwrap().facts(&scope()).unwrap();
        assert_eq!(f.exposure, Exposure::Public);
        let req = t.join().unwrap();
        assert!(req.starts_with("GET /repos/me/thing "), "{req}");
        assert!(req.contains("Authorization: Bearer s3cret"), "{req}");
    }

    #[test]
    fn registering_posts_the_json_and_reads_the_config() {
        let (url, t) = fake(201, r#"{"runner":{"id":23},"encoded_jit_config":"abc"}"#);
        let labels = ["self-hosted".to_owned()];
        let r = Curl::new(&url, "t")
            .unwrap()
            .register(&scope(), 1, "n1", &labels)
            .unwrap();
        assert_eq!((r.id, r.encoded.as_str()), (23, "abc"));
        assert!(!format!("{r:?}").contains("abc"), "the config never prints");
        let req = t.join().unwrap();
        assert!(
            req.starts_with("POST /repos/me/thing/actions/runners/generate-jitconfig "),
            "{req}"
        );
        let body: Value = serde_json::from_str(req.rsplit("\r\n").next().unwrap()).unwrap();
        assert_eq!(body["name"], "n1");
        assert_eq!(body["runner_group_id"], 1);
        assert_eq!(body["labels"][0], "self-hosted");
        assert_eq!(body["work_folder"], "/tmp/runner/_work");
    }

    #[test]
    fn a_404_is_the_runner_gone_and_any_other_failure_is_not() {
        let (url, _t) = fake(404, r#"{"message":"Not Found"}"#);
        assert_eq!(
            Curl::new(&url, "t").unwrap().runner(&scope(), 5).unwrap(),
            RunnerSeen::Gone
        );
        let (url, _t) = fake(500, "oops");
        assert!(Curl::new(&url, "t").unwrap().runner(&scope(), 5).is_err());
    }

    #[test]
    fn an_error_echoing_the_token_comes_back_without_it() {
        let (url, _t) = fake(401, r#"{"message":"bad credentials s3cret"}"#);
        let e = Curl::new(&url, "s3cret")
            .unwrap()
            .facts(&scope())
            .unwrap_err();
        assert!(!e.contains("s3cret") && e.contains("401"), "{e}");
    }

    #[test]
    fn a_redirect_is_not_followed_so_the_token_stays_put() {
        let (url, _t) = fake(302, "");
        assert!(Curl::new(&url, "t").unwrap().facts(&scope()).is_err());
    }

    #[test]
    fn the_token_stays_off_the_command_line_and_is_quoted_for_curl() {
        let c = curl_config("tok\"en", Some("{\"a\":\"b\"}"));
        assert!(
            c.contains(r#"header = "Authorization: Bearer tok\"en""#),
            "{c}"
        );
        assert!(c.contains(r#"data-binary = "{\"a\":\"b\"}""#), "{c}");
    }

    #[test]
    fn text_coming_back_never_carries_the_token() {
        assert_eq!(
            scrub("bad Bearer s3cret here", "s3cret"),
            "bad Bearer [token] here"
        );
        assert_eq!(scrub("left alone", ""), "left alone");
    }

    #[test]
    fn a_control_character_in_the_token_is_refused() {
        assert!(Curl::new("https://api.github.com", "a\nheader = x").is_err());
    }

    #[test]
    fn the_token_only_travels_over_https_or_to_this_machine() {
        assert!(api_ok("https://api.github.com").is_ok());
        assert!(api_ok("http://127.0.0.1:9").is_ok());
        for bad in [
            "http://example.org",
            "http://127.0.0.1.example.org",
            "ftp://x",
        ] {
            assert!(api_ok(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn visibility_is_read_and_an_unclear_answer_is_never_closed() {
        assert_eq!(
            exposure_of_repo(&json!({"visibility":"public"})).unwrap(),
            Exposure::Public
        );
        assert_eq!(
            exposure_of_repo(&json!({"visibility":"private"})).unwrap(),
            Exposure::Closed
        );
        assert_eq!(
            exposure_of_repo(&json!({"visibility":"internal"})).unwrap(),
            Exposure::Closed
        );
        assert_eq!(
            exposure_of_repo(&json!({"private":false})).unwrap(),
            Exposure::Public
        );
        assert!(exposure_of_repo(&json!({"visibility":"strange"})).is_err());
        assert!(exposure_of_repo(&json!({})).is_err());
    }

    #[test]
    fn a_runners_json_reads_as_idle_busy_or_offline() {
        assert_eq!(
            seen(&json!({"status":"online","busy":true})),
            RunnerSeen::Busy
        );
        assert_eq!(
            seen(&json!({"status":"online","busy":false})),
            RunnerSeen::Idle
        );
        assert_eq!(
            seen(&json!({"status":"offline","busy":false})),
            RunnerSeen::Offline
        );
    }

    #[test]
    fn a_group_name_is_encoded_and_a_scope_makes_its_path() {
        assert_eq!(segment("ci runners"), "ci%20runners");
        assert_eq!(base(&Scope::parse("me/thing").unwrap()), "repos/me/thing");
        assert_eq!(base(&Scope::parse("org/acme/ci").unwrap()), "orgs/acme");
    }
}
