//! The status page (§8) and its password (spec-fileferry §3's rule: set on the
//! first visit, an argon2 hash, a 30-day cookie, a wrong password answers after
//! a second, never a lockout). It's asked on loopback too, because a job shares
//! the pod's network. Plain forms and no script: there's nothing here to build.

use crate::clock;
use crate::engine::Shared;
use crate::policy::{self, Scope, Waiting};
use crate::state::{SESSION_SECS, Session};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use axum::Form;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

/// What the handlers need.
pub struct Page {
    pub shared: Arc<Shared>,
    pub scope: Result<Scope, String>,
    /// The app release this broker's image was built from; empty if unknown.
    pub release: String,
    /// GitHub's word on the scope now; `None` when it can't be read.
    pub exposure: Box<dyn Fn() -> Option<policy::Exposure> + Send + Sync>,
    pub wrong_password_delay: Duration,
}

type App = State<Arc<Page>>;

/// The app's routes.
pub fn router(page: Arc<Page>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/status.json", get(status_json))
        .route("/password", post(set_password))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/confirm-public", post(confirm_public))
        .route("/drain", post(drain))
        .route("/resume", post(resume))
        .route("/clear-cache", post(clear_cache))
        .with_state(page)
}

#[derive(serde::Deserialize)]
struct PasswordForm {
    password: String,
}

#[derive(serde::Deserialize)]
struct ConfirmForm {
    typed: String,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn signed_in(page: &Page, headers: &HeaderMap) -> bool {
    let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("session=")))
    else {
        return false;
    };
    let now = clock::now();
    page.shared
        .lock()
        .0
        .sessions
        .iter()
        .any(|s| s.token == token && s.expires > now)
}

fn random_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| e.to_string())?;
    Ok(b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    }))
}

/// A new session, with the cookie that carries it.
fn session_cookie(page: &Page) -> Result<HeaderValue, String> {
    let token = random_token()?;
    let now = clock::now();
    page.shared.change(|s| {
        s.sessions.retain(|x| x.expires > now);
        s.sessions.push(Session {
            token: token.clone(),
            expires: now + SESSION_SECS,
        });
    })?;
    HeaderValue::from_str(&format!(
        "session={token}; Max-Age={SESSION_SECS}; Path=/; HttpOnly; SameSite=Strict"
    ))
    .map_err(|e| e.to_string())
}

fn fail(e: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(shell(&format!("<p>{}</p>", esc(e)))),
    )
        .into_response()
}

fn to_home_with(cookie: HeaderValue) -> Response {
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

async fn set_password(State(page): App, Form(f): Form<PasswordForm>) -> Response {
    if page.shared.lock().0.password_hash.is_some() {
        return (
            StatusCode::CONFLICT,
            Html(shell("<p>This page already has a password.</p>")),
        )
            .into_response();
    }
    if f.password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html(setup_form("Type a password.")),
        )
            .into_response();
    }
    let hashed = tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(f.password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| e.to_string())
    })
    .await;
    let hash = match hashed {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return fail(&e),
        Err(e) => return fail(&e.to_string()),
    };
    // Two first visits at once: the first one's password stands.
    let won = page.shared.change(|s| {
        if s.password_hash.is_some() {
            return false;
        }
        s.password_hash = Some(hash);
        true
    });
    match won {
        Ok(true) => {}
        Ok(false) => return Redirect::to("/").into_response(),
        Err(e) => return fail(&e),
    }
    page.shared.poke();
    match session_cookie(&page) {
        Ok(c) => to_home_with(c),
        Err(e) => fail(&e),
    }
}

async fn login(State(page): App, Form(f): Form<PasswordForm>) -> Response {
    let Some(hash) = page.shared.lock().0.password_hash.clone() else {
        return Redirect::to("/").into_response();
    };
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|h| {
            Argon2::default()
                .verify_password(f.password.as_bytes(), &h)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false);
    if !ok {
        tokio::time::sleep(page.wrong_password_delay).await;
        return (
            StatusCode::UNAUTHORIZED,
            Html(login_form("That password isn't right.")),
        )
            .into_response();
    }
    match session_cookie(&page) {
        Ok(c) => to_home_with(c),
        Err(e) => fail(&e),
    }
}

async fn logout(State(page): App, headers: HeaderMap) -> Response {
    if signed_in(&page, &headers) {
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|c| c.to_str().ok())
            .unwrap_or("");
        let token = cookie
            .split(';')
            .find_map(|p| p.trim().strip_prefix("session="))
            .unwrap_or("")
            .to_owned();
        let _ = page
            .shared
            .change(|s| s.sessions.retain(|x| x.token != token));
    }
    Redirect::to("/").into_response()
}

/// A change only from a signed-in admin, then the home page.
fn signed_in_change(
    page: &Page,
    headers: &HeaderMap,
    f: impl FnOnce(&mut crate::state::State),
) -> Response {
    if !signed_in(page, headers) {
        return Redirect::to("/").into_response();
    }
    match page.shared.change(f) {
        Ok(()) => {
            page.shared.poke();
            Redirect::to("/").into_response()
        }
        Err(e) => fail(&e),
    }
}

async fn drain(State(page): App, headers: HeaderMap) -> Response {
    signed_in_change(&page, &headers, |s| s.drain = true)
}

async fn resume(State(page): App, headers: HeaderMap) -> Response {
    signed_in_change(&page, &headers, |s| s.drain = false)
}

async fn clear_cache(State(page): App, headers: HeaderMap) -> Response {
    signed_in_change(&page, &headers, |s| s.clear_cache = true)
}

async fn confirm_public(
    State(page): App,
    headers: HeaderMap,
    Form(f): Form<ConfirmForm>,
) -> Response {
    if !signed_in(&page, &headers) {
        return Redirect::to("/").into_response();
    }
    // Read now, not from the last round: the name is typed against what GitHub
    // says at this moment. curl blocks, so not on the page's own threads.
    let reader = page.clone();
    let exposure = tokio::task::spawn_blocking(move || (reader.exposure)())
        .await
        .unwrap_or(None);
    let ok = match (&page.scope, exposure) {
        (Ok(scope), Some(e)) => policy::confirmation_ok(scope, &e, &f.typed).then(|| scope.label()),
        _ => None,
    };
    let Some(name) = ok else {
        return (
            StatusCode::CONFLICT,
            Html(shell("<p>That isn't the repository's name, so nothing changed. <a href=\"/\">Back</a></p>")),
        )
            .into_response();
    };
    signed_in_change(&page, &headers, |s| s.confirmed = Some(name))
}

fn shell(body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Runner for GitHub Actions</title><style>\
body{{font:16px/1.5 system-ui,sans-serif;max-width:44rem;margin:2rem auto;padding:0 1rem;color:#222;background:#fff}}\
@media (prefers-color-scheme:dark){{body{{color:#eee;background:#181818}}table,td,th{{border-color:#444!important}}.warn{{background:#3a2d10!important}}}}\
h1{{font-size:1.4rem}}h2{{font-size:1.05rem;margin-top:1.6rem}}table{{border-collapse:collapse;width:100%}}td,th{{border-bottom:1px solid #ddd;padding:.3rem .5rem;text-align:left}}\
.warn{{background:#fdf2d0;padding:.6rem .8rem;border-radius:4px}}form{{display:inline-block;margin:.2rem .4rem .2rem 0}}input,button{{font:inherit;padding:.3rem .6rem}}code{{word-break:break-all}}\
</style></head><body><h1>Runner for GitHub Actions</h1>{body}</body></html>"
    )
}

fn setup_form(note: &str) -> String {
    shell(&format!(
        "<p>Set a password for this page. No job runs until it has one, because a job can reach this page too.</p><p>{}</p>\
<form method=post action=/password><input type=password name=password autofocus autocomplete=new-password aria-label=Password> <button>Set password</button></form>",
        esc(note)
    ))
}

fn login_form(note: &str) -> String {
    shell(&format!(
        "<p>{}</p><form method=post action=/login><input type=password name=password autofocus autocomplete=current-password aria-label=Password> <button>Sign in</button></form>",
        esc(note)
    ))
}

async fn home(State(page): App, headers: HeaderMap) -> Response {
    if page.shared.lock().0.password_hash.is_none() {
        return Html(setup_form("")).into_response();
    }
    if !signed_in(&page, &headers) {
        return Html(login_form("Sign in to see this runner.")).into_response();
    }
    Html(shell(&status_body(&page))).into_response()
}

/// The warning (§13.4), in one sentence since the broker can't tell which kind
/// of image the runner service has.
const UPDATE_SOON: &str = "Update this app soon from Apps on your LehrNAS. If you use your own runner image, rebuild it from this app's new version, then update it there.";
/// The runner never spoke and GitHub never saw it (§13.2).
const NO_WORD: &str = "No word from the runner service in 5 minutes, so it may not have started. If you chose your own image for it, check that it's built from this app's image.";

/// The line when the runner's image and this app's come from different
/// releases (§13.4); nothing stops because of it.
fn releases_differ(runner: Option<&str>, app: &str) -> Option<String> {
    let runner = runner?;
    (!app.is_empty() && runner != app).then(|| {
        format!(
            "The runner service's image was built from {runner}, and this app is on {app}. Rebuild it so they match."
        )
    })
}

/// The state, in one sentence.
fn sentence(
    w: Option<&Waiting>,
    current: Option<&crate::state::Runner>,
    label: &str,
    now: u64,
) -> String {
    match w {
        None => "Starting.".to_owned(),
        Some(Waiting::NoPassword) => "Waiting for this page's password.".to_owned(),
        Some(Waiting::Drained) => {
            "Drained: taking no new jobs. No runner is registered with GitHub.".to_owned()
        }
        Some(Waiting::Refused(_)) => "Not taking jobs.".to_owned(),
        Some(Waiting::Unreadable(why)) => format!("Not taking jobs: {why}"),
        Some(Waiting::Unknown) => {
            "GitHub isn't answering about this runner; trying again.".to_owned()
        }
        Some(Waiting::Idle) => format!("Idle: waiting for a job from {label}."),
        Some(Waiting::Running) => {
            let c = current.cloned().unwrap_or_default();
            let mins = c.busy_since.map_or(0, |b| now.saturating_sub(b) / 60);
            format!(
                "Running a job: {} from {label}, {mins} minute{} so far.",
                c.job.as_deref().unwrap_or("its name isn't known yet"),
                if mins == 1 { "" } else { "s" }
            )
        }
    }
}

/// The runner's version as it reported it, its clock, and the lines about its image.
fn version_section(
    b: &mut String,
    page: &Page,
    state: &crate::state::State,
    no_word: bool,
    now: u64,
) {
    b.push_str("<h2>Version</h2>");
    match state.runner_version.as_deref() {
        None => b.push_str("<p>Runner version not known yet.</p>"),
        Some(v) => {
            let _ = write!(b, "<p>Runner {}.", esc(v));
            match policy::update_clock(v, &state.releases, now) {
                None => b.push_str(" No newer version is known.</p>"),
                Some(c) => {
                    let _ = write!(b, " Version {} is out", esc(&c.newer));
                    let _ = write!(
                        b,
                        ": GitHub stops giving this one jobs in {} day{}.</p>",
                        c.days_left,
                        if c.days_left == 1 { "" } else { "s" }
                    );
                    if c.warn {
                        let _ = write!(b, "<p class=warn>{}</p>", esc(UPDATE_SOON));
                    }
                }
            }
        }
    }
    if let Some(line) = releases_differ(state.runner_release.as_deref(), &page.release) {
        let _ = write!(b, "<p class=warn>{}</p>", esc(&line));
    }
    if no_word {
        let _ = write!(b, "<p class=warn>{}</p>", esc(NO_WORD));
    }
}

fn status_body(page: &Page) -> String {
    let now = clock::now();
    let (state, view) = {
        let g = page.shared.lock();
        (g.0.clone(), g.1.clone())
    };
    let label = page
        .scope
        .as_ref()
        .map_or_else(|_| "nowhere yet".to_owned(), Scope::label);
    let mut b = String::new();
    let _ = write!(
        b,
        "<p><strong>{}</strong></p>",
        esc(&sentence(
            view.waiting.as_ref(),
            state.current.as_ref(),
            &label,
            now
        ))
    );
    if let Some(n) = view.queued.filter(|n| *n > 0) {
        let _ = write!(
            b,
            "<p>{n} workflow run{} queued.</p>",
            if n == 1 { "" } else { "s" }
        );
    }
    if state.drain && !matches!(view.waiting, Some(Waiting::Drained)) {
        b.push_str("<p>Drain is on: the current job finishes, and no new one starts.</p>");
    }
    if state.clear_cache {
        b.push_str("<p>The cache empties when the current job ends.</p>");
    }
    if let Some(Waiting::Refused(r)) = &view.waiting {
        let _ = write!(b, "<div class=warn><p>{}</p>", esc(&r.sentence));
        if r.can_confirm {
            let _ = write!(
                b,
                "<form method=post action=/confirm-public><label>Type <code>{0}</code> to allow it: <input name=typed autocomplete=off></label> <button>Allow this repository</button></form>",
                esc(&label)
            );
        }
        b.push_str("</div>");
    }
    if let Some(f) = &view.fault {
        let _ = write!(b, "<p class=warn>{}</p>", esc(f));
    }
    if view.update_seen {
        b.push_str("<p class=warn>The runner's output says <code>Runner update in progress</code>: GitHub offered an update. It can't apply here, because the image is read-only, but it fetches into memory on every job until a new version of this app is installed.</p>");
    }
    version_section(&mut b, page, &state, view.no_word, now);
    b.push_str("<h2>Actions</h2>");
    if state.drain {
        b.push_str("<form method=post action=/resume><button>Resume</button></form>");
    } else {
        b.push_str("<form method=post action=/drain><button>Drain</button></form>");
    }
    b.push_str("<form method=post action=/clear-cache><button>Clear cache</button></form><form method=post action=/logout><button>Sign out</button></form>");
    b.push_str("<h2>Last jobs</h2>");
    if state.jobs.is_empty() {
        b.push_str("<p>None yet.</p>");
    } else {
        b.push_str("<table><tr><th>Job</th><th>Repository</th><th>Took</th><th>Result</th><th>Ended</th></tr>");
        for j in &state.jobs {
            let _ = write!(
                b,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} ago</td></tr>",
                esc(&j.name),
                esc(&j.repository),
                clock::span(j.finished.saturating_sub(j.started)),
                esc(&j.result),
                clock::span(now.saturating_sub(j.finished)),
            );
        }
        b.push_str("</table>");
    }
    b.push_str("<h2>What a job can't do here</h2><ul><li>This runner has no container support, so <code>container:</code> and <code>services:</code> can't run here.</li><li>A job can't use <code>sudo</code>, so a workflow that installs system packages fails with its own error.</li></ul>");
    b
}

async fn status_json(State(page): App, headers: HeaderMap) -> Response {
    if !signed_in(&page, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error":"sign in"})),
        )
            .into_response();
    }
    let (state, view) = {
        let g = page.shared.lock();
        (g.0.clone(), g.1.clone())
    };
    let phase = match &view.waiting {
        None => "starting",
        Some(Waiting::NoPassword) => "needs a password",
        Some(Waiting::Drained) => "drained",
        Some(Waiting::Refused(_)) => "refused",
        Some(Waiting::Unreadable(_)) => "waiting for GitHub",
        Some(Waiting::Unknown) => "unknown",
        Some(Waiting::Idle) => "idle",
        Some(Waiting::Running) => "running",
    };
    axum::Json(serde_json::json!({
        "state": phase,
        "drain": state.drain,
        "version": state.runner_version,
        "job": state.current.and_then(|c| c.job),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Exposure, Refusal};
    use crate::state::{File, State as Saved};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn page(name: &str, exposure: Option<Exposure>) -> Arc<Page> {
        let d = std::env::temp_dir().join(format!("gha-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Arc::new(Page {
            shared: Shared::new(Saved::default(), File::new(&d)),
            scope: Scope::parse("me/thing"),
            release: "gha-runner-2026.1106.2".into(),
            exposure: Box::new(move || exposure.clone()),
            wrong_password_delay: Duration::from_millis(5),
        })
    }

    async fn send(
        p: &Arc<Page>,
        method: &str,
        uri: &str,
        body: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, HeaderMap, String) {
        let mut r = Request::builder().method(method).uri(uri);
        if method == "POST" {
            r = r.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        if let Some(c) = cookie {
            r = r.header(header::COOKIE, c);
        }
        let resp = router(p.clone())
            .oneshot(r.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let (parts, b) = resp.into_parts();
        let text = String::from_utf8(b.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        (parts.status, parts.headers, text)
    }

    async fn signed(p: &Arc<Page>) -> String {
        let (_, h, _) = send(p, "POST", "/password", "password=hunter2", None).await;
        h[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn the_first_visit_sets_the_password_and_only_the_first() {
        let p = page("first", None);
        let (_, _, home) = send(&p, "GET", "/", "", None).await;
        assert!(home.contains("Set a password"), "{home}");
        let c = signed(&p).await;
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains("Drain"), "{home}");
        let hash = p.shared.lock().0.password_hash.clone().unwrap();
        assert!(hash.starts_with("$argon2"), "{hash}");
        let (code, ..) = send(&p, "POST", "/password", "password=other", None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(p.shared.lock().0.password_hash.clone().unwrap(), hash);
    }

    #[tokio::test]
    async fn every_page_and_button_asks_for_the_password() {
        let p = page("auth", None);
        signed(&p).await;
        let (_, _, home) = send(&p, "GET", "/", "", None).await;
        assert!(home.contains("Sign in"), "{home}");
        assert!(!home.contains("Drain"));
        let (code, ..) = send(&p, "GET", "/status.json", "", None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        for path in ["/drain", "/resume", "/clear-cache"] {
            send(&p, "POST", path, "", Some("session=guess")).await;
        }
        send(&p, "POST", "/confirm-public", "typed=me/thing", None).await;
        let g = p.shared.lock();
        assert!(!g.0.drain && !g.0.clear_cache && g.0.confirmed.is_none());
    }

    #[tokio::test]
    async fn a_wrong_password_waits_and_says_so_a_right_one_signs_in() {
        let p = page("login", None);
        signed(&p).await;
        let (code, _, text) = send(&p, "POST", "/login", "password=nope", None).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert!(text.contains("That password isn&#39;t right."), "{text}");
        let (code, h, _) = send(&p, "POST", "/login", "password=hunter2", None).await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        let set = h[header::SET_COOKIE].to_str().unwrap();
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict"),
            "{set}"
        );
    }

    #[tokio::test]
    async fn signing_out_ends_the_session() {
        let p = page("logout", None);
        let c = signed(&p).await;
        send(&p, "POST", "/logout", "", Some(&c)).await;
        let (code, ..) = send(&p, "GET", "/status.json", "", Some(&c)).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn drain_and_resume_change_the_state_and_the_button() {
        let p = page("drain", None);
        let c = signed(&p).await;
        send(&p, "POST", "/drain", "", Some(&c)).await;
        assert!(p.shared.lock().0.drain);
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(
            home.contains("Resume") && !home.contains(">Drain<"),
            "{home}"
        );
        send(&p, "POST", "/resume", "", Some(&c)).await;
        assert!(!p.shared.lock().0.drain);
    }

    #[tokio::test]
    async fn a_public_repository_shows_the_refusal_and_takes_only_its_name() {
        let p = page("public", Some(Exposure::Public));
        let c = signed(&p).await;
        let r: Refusal = policy::gate(
            &Scope::parse("me/thing").unwrap(),
            &Exposure::Public,
            None,
            None,
            &[],
        )
        .unwrap_err();
        p.shared.lock().1.waiting = Some(Waiting::Refused(r));
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains("me/thing is public."), "{home}");
        assert!(
            home.contains("Anyone who can open a pull request on this repository"),
            "{home}"
        );
        assert!(
            !home.contains("REFUSED"),
            "a log line stays in the log: {home}"
        );
        let (code, ..) = send(&p, "POST", "/confirm-public", "typed=thing", Some(&c)).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(p.shared.lock().0.confirmed, None);
        send(&p, "POST", "/confirm-public", "typed=me%2Fthing", Some(&c)).await;
        assert_eq!(p.shared.lock().0.confirmed.as_deref(), Some("me/thing"));
    }

    #[tokio::test]
    async fn a_private_repository_has_nothing_to_confirm() {
        let p = page("private", Some(Exposure::Closed));
        let c = signed(&p).await;
        send(&p, "POST", "/confirm-public", "typed=me%2Fthing", Some(&c)).await;
        assert_eq!(p.shared.lock().0.confirmed, None);
    }

    #[tokio::test]
    async fn a_jobs_words_are_escaped_on_the_page() {
        let p = page("escape", None);
        let c = signed(&p).await;
        p.shared
            .change(|s| {
                s.push_job(crate::state::Job {
                    name: "<script>x</script>".into(),
                    ..Default::default()
                });
            })
            .unwrap();
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(!home.contains("<script>x"), "{home}");
        assert!(home.contains("&lt;script&gt;x"), "{home}");
    }

    #[tokio::test]
    async fn the_page_warns_from_day_twenty_and_states_the_limits() {
        let p = page("warn", None);
        let c = signed(&p).await;
        let now = clock::now();
        p.shared
            .change(|s| {
                s.releases = vec![("v2.339.0".into(), now - 21 * 86_400)];
                s.runner_version = Some("2.338.2".into());
            })
            .unwrap();
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains("in 9 days"), "{home}");
        assert!(home.contains(&esc(UPDATE_SOON)), "{home}");
        assert!(home.contains("no container support"), "{home}");
        p.shared.lock().1.update_seen = true;
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains("Runner update in progress"), "{home}");
    }

    #[tokio::test]
    async fn the_days_left_count_from_what_the_runner_reported() {
        let p = page("reported", None);
        let c = signed(&p).await;
        let now = clock::now();
        p.shared
            .change(|s| s.releases = vec![("v2.339.0".into(), now - 21 * 86_400)])
            .unwrap();
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains("Runner version not known yet."), "{home}");
        assert!(!home.contains("in 9 days"), "no clock yet: {home}");
        assert!(!home.contains("Update this app soon"), "{home}");

        p.shared
            .change(|s| s.runner_version = Some("2.339.0".into()))
            .unwrap();
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains("Runner 2.339.0."), "{home}");
        assert!(home.contains("No newer version is known."), "{home}");
        assert!(!home.contains("not known yet"), "{home}");

        let (_, _, json) = send(&p, "GET", "/status.json", "", Some(&c)).await;
        assert!(json.contains(r#""version":"2.339.0""#), "{json}");
    }

    #[tokio::test]
    async fn two_releases_that_differ_are_said_and_two_that_match_are_not() {
        let p = page("differ", None);
        let c = signed(&p).await;
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(!home.contains("Rebuild it"), "nothing heard: {home}");
        p.shared
            .change(|s| s.runner_release = Some("gha-runner-2026.1106.1".into()))
            .unwrap();
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        let line = "The runner service&#39;s image was built from gha-runner-2026.1106.1, and this app is on gha-runner-2026.1106.2. Rebuild it so they match.";
        assert!(home.contains(line), "{home}");
        p.shared
            .change(|s| s.runner_release = Some("gha-runner-2026.1106.2".into()))
            .unwrap();
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(!home.contains("Rebuild it"), "{home}");
        assert_eq!(releases_differ(Some("a"), ""), None, "our own isn't known");
    }

    #[tokio::test]
    async fn a_silent_runner_and_the_card_are_on_the_page() {
        let p = page("silent", None);
        let c = signed(&p).await;
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(!home.contains("No word from the runner"), "{home}");
        p.shared.lock().1.no_word = true;
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(home.contains(&esc(NO_WORD)), "{home}");
    }

    #[tokio::test]
    async fn the_page_promises_no_label() {
        let p = page("label", None);
        let c = signed(&p).await;
        let (_, _, home) = send(&p, "GET", "/", "", Some(&c)).await;
        assert!(!home.to_lowercase().contains("lehrnas-"), "{home}");
        assert!(!home.contains("`lehrnas`"), "{home}");
        assert!(!home.contains("label"), "{home}");
    }
}
