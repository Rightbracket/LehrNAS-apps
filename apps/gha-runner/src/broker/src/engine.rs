//! One round at a time: read what GitHub and the pod say, let `policy` decide,
//! do it. The rounds run on their own thread; the page shares [`Shared`].

use crate::github::GitHub;
use crate::jit;
use crate::pod::Pod;
use crate::policy::{self, Current, Gate, Round, RunnerSeen, Said, Scope, Step, Waiting};
use crate::state::{File, Job, Runner, State};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How often the broker looks when nothing is happening: visibility and the
/// runner, about 240 calls an hour against GitHub's 5,000.
pub const ROUND_SECS: u64 = 30;
/// A step that changes something looks again soon.
pub const SOON_SECS: u64 = 3;
const RELEASES_EVERY_SECS: u64 = 86_400;

/// What the page shows beyond the saved state.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub waiting: Option<Waiting>,
    /// The last thing that went wrong doing a step, for the page.
    pub fault: Option<String>,
    pub queued: Option<u64>,
    /// `Runner update in progress` appeared in the runner's output (§8).
    pub update_seen: bool,
    /// GitHub shows the runner offline and it never reported (§13.2).
    pub no_word: bool,
}

/// The state and the view, with a way to wake the rounds.
pub struct Shared {
    inner: Mutex<(State, View)>,
    wake: Condvar,
    file: File,
}

impl Shared {
    #[must_use]
    pub fn new(state: State, file: File) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new((state, View::default())),
            wake: Condvar::new(),
            file,
        })
    }

    /// The lock; a panicked holder's data is still the best there is.
    pub fn lock(&self) -> MutexGuard<'_, (State, View)> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Change the saved state: written before anyone is told it happened.
    ///
    /// # Errors
    /// The disk's.
    pub fn change<T>(&self, f: impl FnOnce(&mut State) -> T) -> Result<T, String> {
        let mut g = self.lock();
        let out = f(&mut g.0);
        self.file.save(&g.0)?;
        Ok(out)
    }

    /// Run the next round now (the page's buttons).
    pub fn poke(&self) {
        self.wake.notify_all();
    }

    fn sleep(&self, secs: u64) {
        let g = self.lock();
        drop(
            self.wake
                .wait_timeout(g, Duration::from_secs(secs))
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    /// A line of the runner's output reached the broker. Display only: a job
    /// can send these too, so no decision rests on one.
    pub fn heard(&self, line: &str) {
        let Some(said) = policy::said(line) else {
            return;
        };
        let mut g = self.lock();
        match said {
            Said::UpdateInProgress => g.1.update_seen = true,
            Said::Started(job) => {
                if let Some(c) = g.0.current.as_mut() {
                    c.job = Some(job);
                }
            }
            Said::Finished { job, result } => {
                if let Some(c) = g.0.current.as_mut() {
                    c.job.get_or_insert(job);
                    c.result = Some(result);
                }
            }
            // The first report of each runner stands, and it comes before its
            // listener starts. So a job can't overwrite it, and a job sending
            // these in a loop can't turn into one disk write after another.
            Said::Report { runner, release } => {
                let Some(c) = g.0.current.as_mut() else {
                    return;
                };
                if c.reported {
                    return;
                }
                c.reported = true;
                g.0.runner_version = runner;
                g.0.runner_release = release;
                g.1.no_word = false;
                // A failed save loses a line on a page, and the next runner
                // reports again.
                let _ = self.file.save(&g.0);
            }
        }
    }
}

/// Everything a round needs.
pub struct Engine {
    pub github: Arc<dyn GitHub>,
    pub pod: Arc<dyn Pod>,
    /// Its fault in a sentence when `RUNNER_SCOPE` is unusable.
    pub scope: Result<Scope, String>,
    pub labels: Vec<String>,
    pub job_limit_secs: u64,
    pub shared: Arc<Shared>,
    /// A new runner's name: a function so tests get stable ones.
    pub names: Box<dyn Fn() -> String + Send + Sync>,
}

impl Engine {
    /// The gate for this round, with the group to register in. Read fresh every
    /// round: "before every job" for a runner that may sit idle for days means
    /// while it sits (§7).
    fn gate(&self) -> (Gate, u64) {
        let scope = match &self.scope {
            Ok(s) => s,
            Err(e) => return (Gate::Unreadable(e.clone()), 0),
        };
        let facts = match self.github.facts(scope) {
            Ok(f) => f,
            Err(e) => return (Gate::Unreadable(e), 0),
        };
        let confirmed = {
            let mut g = self.shared.lock();
            let was = g.0.confirmed.clone();
            let kept = policy::confirmation_kept(&facts.exposure, was.clone());
            if kept != was {
                g.0.confirmed.clone_from(&kept);
                if let Err(e) = self.shared.file.save(&g.0) {
                    return (Gate::Unreadable(e), 0);
                }
            }
            kept
        };
        // The fork setting matters only once the name is confirmed.
        let fork = if facts.exposure == policy::Exposure::Public && confirmed.is_some() {
            match self.github.fork_approval(scope) {
                Ok(f) => Some(f),
                Err(e) => return (Gate::Unreadable(e), 0),
            }
        } else {
            None
        };
        let grants = self.pod.grants();
        let verdict = policy::gate(
            scope,
            &facts.exposure,
            confirmed.as_deref(),
            fork.as_deref(),
            &grants,
        );
        (
            verdict.map_or_else(Gate::Refused, |()| Gate::Open),
            facts.group_id,
        )
    }

    /// The runner as GitHub has it. `None` when GitHub can't be asked.
    fn observe(&self, now: u64) -> Option<Current> {
        let (id, written, stop_asked) = {
            let g = self.shared.lock();
            let c = g.0.current.as_ref()?;
            (c.id, c.written, c.stop_asked)
        };
        let seen = match &self.scope {
            Ok(s) => self.github.runner(s, id).ok(),
            Err(_) => None,
        };
        let busy_since = {
            let mut g = self.shared.lock();
            let c = g.0.current.as_mut()?;
            if seen == Some(RunnerSeen::Busy) && c.busy_since.is_none() {
                c.busy_since = Some(now);
                let _ = self.shared.file.save(&g.0);
            }
            g.0.current.as_ref()?.busy_since
        };
        Some(Current {
            seen,
            written_secs_ago: now.saturating_sub(written),
            busy_secs: busy_since.map_or(0, |b| now.saturating_sub(b)),
            stop_already_asked: stop_asked,
        })
    }

    fn refresh_releases(&self, now: u64) {
        let due = now.saturating_sub(self.shared.lock().0.releases_read) >= RELEASES_EVERY_SECS;
        if !due {
            return;
        }
        // A failed read waits for the next round's chance: no state changes.
        if let Ok(r) = self.github.releases() {
            let _ = self.shared.change(|s| {
                s.releases = r;
                s.releases_read = now;
            });
        }
    }

    /// One round. Answers the step and how long to sleep.
    pub fn round(&self, now: u64) -> (Step, u64) {
        self.refresh_releases(now);
        let (gate, group) = self.gate();
        let no_word;
        let current = self.observe(now);
        let round = {
            let g = self.shared.lock();
            let reported = g.0.current.as_ref().is_some_and(|c| c.reported);
            no_word = current.as_ref().and_then(|c| policy::no_word(c, reported));
            Round {
                set_up: g.0.password_hash.is_some(),
                drain: g.0.drain,
                clear_cache: g.0.clear_cache,
                gate,
                current,
                job_limit_secs: self.job_limit_secs,
            }
        };
        let step = policy::decide(&round);
        let result = self.act(&step, group, now);
        let queued = self.scope.as_ref().ok().and_then(|s| self.github.queued(s));
        let mut g = self.shared.lock();
        g.1.queued = queued;
        if let Some(said) = no_word {
            g.1.no_word = said;
        }
        // With no runner wanted, there's none to be silent.
        if matches!(
            step,
            Step::Wait(
                Waiting::NoPassword
                    | Waiting::Drained
                    | Waiting::Refused(_)
                    | Waiting::Unreadable(_)
            )
        ) {
            g.1.no_word = false;
        }
        let waiting = match &step {
            Step::Wait(w) => Some(w.clone()),
            _ => g.1.waiting.clone(),
        };
        // One log line per change, in the alerts' form. Never a value of the
        // environment.
        if waiting != g.1.waiting {
            if let Some(Waiting::Refused(r)) = &waiting {
                println!("{}", r.line);
            }
            if let Some(Waiting::Unreadable(e)) = &waiting {
                println!("WAITING | APP SETUP | gha-runner | {e}");
            }
        }
        g.1.waiting = waiting;
        let fault = result.err();
        if let Some(f) = &fault
            && g.1.fault.as_ref() != Some(f)
        {
            println!("FAULT | gha-runner | {f}");
        }
        g.1.fault = fault;
        drop(g);
        let sleep = if matches!(step, Step::Wait(_)) {
            ROUND_SECS
        } else {
            SOON_SECS
        };
        (step, sleep)
    }

    fn act(&self, step: &Step, group: u64, now: u64) -> Result<(), String> {
        match step {
            Step::Wait(_) => Ok(()),
            Step::Register => self.register(group, now),
            Step::Forget => self.forget(now),
            Step::Remove => {
                let (scope, id) = {
                    let g = self.shared.lock();
                    (
                        self.scope.clone()?,
                        g.0.current.as_ref().map_or(0, |c| c.id),
                    )
                };
                self.github.remove(&scope, id)?;
                // Its listener has no reason left to run, and one still
                // retrying would never take the next config.
                self.pod.stop_runner()
            }
            Step::StopJob => {
                self.pod.stop_runner()?;
                self.shared.change(|s| {
                    if let Some(c) = s.current.as_mut() {
                        c.stop_asked = true;
                        c.result = Some("Stopped: past its time limit".into());
                    }
                })
            }
        }
    }

    /// §3's order: the runner goes in the state first, so a crash here can't
    /// lose track of a registration; the pod is cleaned; only then the files.
    fn register(&self, group: u64, now: u64) -> Result<(), String> {
        let scope = self.scope.clone()?;
        let name = (self.names)();
        let reg = self.github.register(&scope, group, &name, &self.labels)?;
        self.shared.change(|s| {
            s.current = Some(Runner {
                id: reg.id,
                name: reg.name.clone(),
                written: now,
                ..Runner::default()
            });
        })?;
        self.shared.lock().1.update_seen = false;
        let files = jit::files(&reg.encoded)?;
        self.pod.clean()?;
        self.pod.write_config(&files)?;
        println!(
            "RUNNER | gha-runner | registered {name} ({}) for {}",
            reg.id,
            scope.label()
        );
        Ok(())
    }

    /// GitHub shows it gone: record the job, clean up, and free the slot. The
    /// slot is freed last, so a failed clean is tried again before any new
    /// config is written.
    fn forget(&self, now: u64) -> Result<(), String> {
        self.pod.clean()?;
        let (asked, repository) = {
            let g = self.shared.lock();
            let repository = self
                .scope
                .as_ref()
                .map_or_else(|_| String::new(), Scope::label);
            (g.0.clear_cache, repository)
        };
        if policy::empty_cache(asked, self.pod.cache_bytes()) {
            self.pod.clear_cache()?;
        }
        self.shared.change(|s| {
            s.clear_cache = false;
            let Some(c) = s.current.take() else { return };
            if let Some(started) = c.busy_since.or(c.job.as_ref().map(|_| c.written)) {
                s.push_job(Job {
                    name: c.job.unwrap_or_else(|| "A job".to_owned()),
                    repository,
                    started,
                    finished: now,
                    result: c.result.unwrap_or_else(|| "Unknown".to_owned()),
                });
            }
        })
    }

    /// The rounds, for ever.
    pub fn run(&self) {
        loop {
            let (_, sleep) = self.round(crate::clock::now());
            self.shared.sleep(sleep);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Registered, ScopeFacts};
    use crate::jit::tests::config;
    use crate::policy::{Exposure, Grant, STRICTEST_FORK_APPROVAL};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// GitHub, as the test says it is, and the calls it got.
    #[derive(Default)]
    struct FakeHub {
        exposure: Mutex<Option<Result<Exposure, String>>>,
        fork: Mutex<String>,
        seen: Mutex<Option<RunnerSeen>>,
        calls: Mutex<Vec<String>>,
        next_id: Mutex<u64>,
        refuse_remove: Mutex<bool>,
    }

    impl GitHub for FakeHub {
        fn facts(&self, _: &Scope) -> Result<ScopeFacts, String> {
            let e = self
                .exposure
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(Exposure::Closed))?;
            Ok(ScopeFacts {
                exposure: e,
                group_id: 7,
            })
        }
        fn fork_approval(&self, _: &Scope) -> Result<String, String> {
            Ok(self.fork.lock().unwrap().clone())
        }
        fn register(
            &self,
            _: &Scope,
            group: u64,
            name: &str,
            labels: &[String],
        ) -> Result<Registered, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("register {name} group {group} {labels:?}"));
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *self.seen.lock().unwrap() = Some(RunnerSeen::Offline);
            Ok(Registered {
                id: *n,
                name: name.to_owned(),
                encoded: config(br#"{"agentId":1}"#),
            })
        }
        fn runner(&self, _: &Scope, id: u64) -> Result<RunnerSeen, String> {
            self.calls.lock().unwrap().push(format!("runner {id}"));
            (*self.seen.lock().unwrap()).ok_or_else(|| "down".to_owned())
        }
        fn remove(&self, _: &Scope, id: u64) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("remove {id}"));
            if *self.refuse_remove.lock().unwrap() {
                return Err("GitHub answered 422".into());
            }
            *self.seen.lock().unwrap() = Some(RunnerSeen::Gone);
            Ok(())
        }
        fn releases(&self) -> Result<Vec<(String, u64)>, String> {
            Ok(vec![("v9.9.9".into(), 5)])
        }
        fn queued(&self, _: &Scope) -> Option<u64> {
            None
        }
    }

    /// The pod, recording the order of what's done to it.
    #[derive(Default)]
    struct FakePod {
        log: Mutex<Vec<String>>,
        written: Mutex<Vec<BTreeMap<String, Vec<u8>>>>,
        grants: Mutex<Vec<Grant>>,
        cache: Mutex<u64>,
        fail_clean: Mutex<bool>,
    }

    impl Pod for FakePod {
        fn clean(&self) -> Result<(), String> {
            if *self.fail_clean.lock().unwrap() {
                return Err("busy".into());
            }
            self.log.lock().unwrap().push("clean".into());
            Ok(())
        }
        fn write_config(&self, f: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
            self.log.lock().unwrap().push("write".into());
            self.written.lock().unwrap().push(f.clone());
            Ok(())
        }
        fn stop_runner(&self) -> Result<(), String> {
            self.log.lock().unwrap().push("stop".into());
            Ok(())
        }
        fn cache_bytes(&self) -> u64 {
            *self.cache.lock().unwrap()
        }
        fn clear_cache(&self) -> Result<(), String> {
            self.log.lock().unwrap().push("clear-cache".into());
            Ok(())
        }
        fn grants(&self) -> Vec<Grant> {
            self.grants.lock().unwrap().clone()
        }
    }

    struct T {
        e: Engine,
        hub: Arc<FakeHub>,
        pod: Arc<FakePod>,
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gha-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup(name: &str) -> T {
        let hub = Arc::new(FakeHub::default());
        let pod = Arc::new(FakePod::default());
        let state = State {
            password_hash: Some("x".into()),
            ..State::default()
        };
        let e = Engine {
            github: hub.clone(),
            pod: pod.clone(),
            scope: Scope::parse("me/thing"),
            labels: vec!["self-hosted".into()],
            job_limit_secs: 3600,
            shared: Shared::new(state, File::new(&dir(name))),
            names: Box::new(|| "lehrnas-test".into()),
        };
        T { e, hub, pod }
    }

    fn registers(t: &T) -> usize {
        t.hub
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("register"))
            .count()
    }

    /// Through a whole job: runner online, busy, gone.
    fn job_done(t: &T, now: u64) {
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Busy);
        t.e.round(now);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Gone);
        t.e.round(now + 60);
    }

    #[test]
    fn one_config_at_a_time_and_the_next_only_after_the_last_is_gone() {
        let t = setup("one");
        assert_eq!(t.e.round(100).0, Step::Register);
        for (seen, at) in [
            (RunnerSeen::Offline, 110),
            (RunnerSeen::Idle, 120),
            (RunnerSeen::Busy, 130),
        ] {
            *t.hub.seen.lock().unwrap() = Some(seen);
            t.e.round(at);
            assert_eq!(registers(&t), 1, "{seen:?}");
        }
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Gone);
        assert_eq!(t.e.round(140).0, Step::Forget);
        assert_eq!(registers(&t), 1);
        assert_eq!(t.e.round(141).0, Step::Register);
        assert_eq!(registers(&t), 2);
        assert_eq!(t.pod.written.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_pod_is_cleaned_before_every_config_and_after_every_job() {
        let t = setup("order");
        t.e.round(100);
        job_done(&t, 110);
        t.e.round(200);
        assert_eq!(
            *t.pod.log.lock().unwrap(),
            ["clean", "write", "clean", "clean", "write"]
        );
    }

    #[test]
    fn a_failed_clean_writes_nothing_and_holds_the_slot() {
        let t = setup("dirty");
        *t.pod.fail_clean.lock().unwrap() = true;
        t.e.round(100);
        assert!(t.pod.written.lock().unwrap().is_empty());
        assert!(t.e.shared.lock().1.fault.is_some());
        // The registration was kept track of, so GitHub isn't asked for another.
        t.e.round(101);
        assert_eq!(registers(&t), 1);
    }

    #[test]
    fn what_it_writes_has_updates_off() {
        let t = setup("flag");
        t.e.round(100);
        let w = t.pod.written.lock().unwrap();
        let r: serde_json::Value = serde_json::from_slice(&w[0][".runner"]).unwrap();
        assert_eq!(r["disableUpdate"], true);
    }

    #[test]
    fn a_public_repository_registers_nothing_without_its_confirmation() {
        let t = setup("public");
        *t.hub.exposure.lock().unwrap() = Some(Ok(Exposure::Public));
        let (step, _) = t.e.round(100);
        let Step::Wait(Waiting::Refused(r)) = step else {
            panic!("{step:?}")
        };
        assert!(r.can_confirm);
        assert_eq!(registers(&t), 0);
    }

    #[test]
    fn a_confirmed_public_repository_runs_only_with_the_strictest_setting_and_no_read_write_grant()
    {
        let t = setup("confirmed");
        *t.hub.exposure.lock().unwrap() = Some(Ok(Exposure::Public));
        t.e.shared
            .change(|s| s.confirmed = Some("me/thing".into()))
            .unwrap();
        *t.hub.fork.lock().unwrap() = "first_time_contributors".into();
        assert!(matches!(t.e.round(100).0, Step::Wait(Waiting::Refused(_))));
        *t.hub.fork.lock().unwrap() = STRICTEST_FORK_APPROVAL.into();
        *t.pod.grants.lock().unwrap() = vec![Grant {
            share: "p".into(),
            read_write: true,
        }];
        assert!(matches!(t.e.round(101).0, Step::Wait(Waiting::Refused(_))));
        assert_eq!(registers(&t), 0);
        t.pod.grants.lock().unwrap().clear();
        assert_eq!(t.e.round(102).0, Step::Register);
    }

    #[test]
    fn a_repository_turning_public_stops_taking_jobs_and_the_page_says_so() {
        let t = setup("turns");
        t.e.round(100);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Idle);
        *t.hub.exposure.lock().unwrap() = Some(Ok(Exposure::Public));
        assert_eq!(
            t.e.round(110).0,
            Step::Remove,
            "the idle runner leaves GitHub"
        );
        assert_eq!(t.e.round(111).0, Step::Forget);
        assert!(matches!(t.e.round(112).0, Step::Wait(Waiting::Refused(_))));
        assert_eq!(registers(&t), 1);
        let g = t.e.shared.lock();
        assert!(matches!(g.1.waiting, Some(Waiting::Refused(_))));
    }

    #[test]
    fn going_private_gives_up_the_confirmation_so_going_public_again_asks_again() {
        let t = setup("again");
        t.e.shared
            .change(|s| s.confirmed = Some("me/thing".into()))
            .unwrap();
        t.e.round(100);
        assert_eq!(t.e.shared.lock().0.confirmed, None);
    }

    #[test]
    fn visibility_that_cant_be_read_registers_nothing() {
        let t = setup("unread");
        *t.hub.exposure.lock().unwrap() = Some(Err("GitHub answered 500".into()));
        assert!(matches!(
            t.e.round(100).0,
            Step::Wait(Waiting::Unreadable(_))
        ));
        assert_eq!(registers(&t), 0);
    }

    #[test]
    fn a_bad_scope_registers_nothing_and_says_why() {
        let mut t = setup("scope");
        t.e.scope = Scope::parse("nonsense");
        let (step, _) = t.e.round(100);
        let Step::Wait(Waiting::Unreadable(why)) = step else {
            panic!("{step:?}")
        };
        assert!(why.contains("RUNNER_SCOPE"), "{why}");
    }

    #[test]
    fn drain_removes_an_idle_runner_and_registers_none_until_resumed() {
        let t = setup("drain");
        t.e.round(100);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Idle);
        t.e.shared.change(|s| s.drain = true).unwrap();
        assert_eq!(t.e.round(110).0, Step::Remove);
        assert_eq!(t.e.round(111).0, Step::Forget);
        assert_eq!(t.e.round(112).0, Step::Wait(Waiting::Drained));
        t.e.shared.change(|s| s.drain = false).unwrap();
        assert_eq!(t.e.round(113).0, Step::Register);
    }

    #[test]
    fn github_refusing_to_remove_a_runner_that_just_took_a_job_is_a_fault_not_a_new_runner() {
        let t = setup("refuse");
        t.e.round(100);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Idle);
        *t.hub.refuse_remove.lock().unwrap() = true;
        t.e.shared.change(|s| s.drain = true).unwrap();
        assert_eq!(t.e.round(110).0, Step::Remove);
        assert!(
            t.e.shared
                .lock()
                .1
                .fault
                .as_deref()
                .unwrap()
                .contains("422")
        );
        assert_eq!(registers(&t), 1);
    }

    #[test]
    fn a_job_past_its_limit_is_asked_to_stop_once_and_recorded() {
        let t = setup("limit");
        t.e.round(100);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Busy);
        t.e.round(110);
        assert_eq!(t.e.round(110 + 3600).0, Step::StopJob);
        assert!(matches!(
            t.e.round(110 + 3601).0,
            Step::Wait(Waiting::Running)
        ));
        assert_eq!(
            t.pod
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|l| *l == "stop")
                .count(),
            1
        );
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Gone);
        t.e.round(110 + 3700);
        let g = t.e.shared.lock();
        assert_eq!(g.0.jobs[0].result, "Stopped: past its time limit");
    }

    #[test]
    fn a_restart_carries_on_with_the_runner_it_registered() {
        let t = setup("restart");
        t.e.round(100);
        let saved = t.e.shared.file.load().unwrap();
        assert!(
            saved.current.is_some(),
            "saved before the files were written"
        );
        let t2 = setup("restart2");
        t2.e.shared.change(|s| s.current = saved.current).unwrap();
        *t2.hub.seen.lock().unwrap() = Some(RunnerSeen::Offline);
        assert_eq!(t2.e.round(101).0, Step::Wait(Waiting::Idle));
        assert_eq!(registers(&t2), 0);
    }

    #[test]
    fn clear_cache_and_the_cap_act_between_jobs_only() {
        let t = setup("cache");
        t.e.round(100);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Busy);
        t.e.shared.change(|s| s.clear_cache = true).unwrap();
        t.e.round(110);
        assert!(!t.pod.log.lock().unwrap().iter().any(|l| l == "clear-cache"));
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Gone);
        t.e.round(120);
        assert!(t.pod.log.lock().unwrap().iter().any(|l| l == "clear-cache"));
        assert!(!t.e.shared.lock().0.clear_cache);

        let t = setup("cap");
        *t.pod.cache.lock().unwrap() = policy::CACHE_CAP_BYTES + 1;
        t.e.round(100);
        job_done(&t, 110);
        assert!(t.pod.log.lock().unwrap().iter().any(|l| l == "clear-cache"));
    }

    #[test]
    fn the_runners_words_fill_the_history_and_an_update_shows() {
        let t = setup("words");
        t.e.round(100);
        t.e.shared.heard("2026-11-05 11:14:31Z: Running job: web");
        t.e.shared
            .heard("2026-11-05 11:14:31Z: Runner update in progress");
        assert!(t.e.shared.lock().1.update_seen);
        t.e.shared
            .heard("2026-11-05 11:20:00Z: Job web completed with result: Succeeded");
        job_done(&t, 110);
        let g = t.e.shared.lock();
        assert_eq!(g.0.jobs[0].name, "web");
        assert_eq!(g.0.jobs[0].result, "Succeeded");
        assert_eq!(g.0.jobs[0].repository, "me/thing");
    }

    #[test]
    fn a_runner_that_never_took_a_job_leaves_no_history() {
        let t = setup("nojob");
        t.e.round(100);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Gone);
        t.e.round(110);
        assert!(t.e.shared.lock().0.jobs.is_empty());
    }

    const REPORT: &str = "Runner report: version=2.338.2 release=gha-runner-2026.1106.2";

    #[test]
    fn the_runners_version_comes_from_its_report_and_survives_a_restart() {
        let t = setup("report");
        t.e.round(100);
        assert_eq!(t.e.shared.lock().0.runner_version, None);
        t.e.shared.heard(REPORT);
        let saved = t.e.shared.file.load().unwrap();
        assert_eq!(saved.runner_version.as_deref(), Some("2.338.2"));
        assert_eq!(
            saved.runner_release.as_deref(),
            Some("gha-runner-2026.1106.2")
        );
    }

    #[test]
    fn a_runners_first_report_stands_and_the_next_runner_reports_again() {
        let t = setup("forged");
        t.e.round(100);
        t.e.shared.heard(REPORT);
        t.e.shared
            .heard("Runner report: version=9.9.9 release=forged");
        assert_eq!(
            t.e.shared.lock().0.runner_version.as_deref(),
            Some("2.338.2")
        );
        job_done(&t, 110);
        t.e.round(200);
        t.e.shared
            .heard("Runner report: version=2.339.0 release=gha-runner-2026.1201.1");
        assert_eq!(
            t.e.shared.lock().0.runner_version.as_deref(),
            Some("2.339.0")
        );
    }

    #[test]
    fn a_report_with_no_runner_registered_is_dropped() {
        let t = setup("orphan");
        t.e.shared.heard(REPORT);
        assert_eq!(t.e.shared.lock().0.runner_version, None);
    }

    #[test]
    fn no_decision_rests_on_the_report() {
        let run = |name: &str, line: Option<&str>| {
            let t = setup(name);
            let mut steps = vec![t.e.round(100).0];
            if let Some(l) = line {
                t.e.shared.heard(l);
            }
            for (seen, at) in [
                (RunnerSeen::Idle, 110),
                (RunnerSeen::Busy, 120),
                (RunnerSeen::Gone, 130),
            ] {
                *t.hub.seen.lock().unwrap() = Some(seen);
                steps.push(t.e.round(at).0);
            }
            steps
        };
        let plain = run("plain", None);
        assert_eq!(
            plain,
            run("old", Some("Runner report: version=0.0.1 release=x"))
        );
        assert_eq!(
            plain,
            run("new", Some("Runner report: version=99.0.0 release=y"))
        );
    }

    #[test]
    fn a_silent_runner_says_so_past_the_grace_and_through_its_replacement() {
        let t = setup("silent");
        t.e.round(100);
        t.e.round(100 + policy::OFFLINE_GRACE_SECS - 1);
        assert!(!t.e.shared.lock().1.no_word, "inside the grace");
        let at = 100 + policy::OFFLINE_GRACE_SECS;
        assert_eq!(t.e.round(at).0, Step::Remove);
        assert!(t.e.shared.lock().1.no_word);
        assert_eq!(t.e.round(at + 1).0, Step::Forget);
        assert_eq!(t.e.round(at + 2).0, Step::Register);
        t.e.round(at + 3);
        assert!(t.e.shared.lock().1.no_word, "still true of the next one");
        t.e.shared.heard(REPORT);
        assert!(!t.e.shared.lock().1.no_word, "it reported");
    }

    #[test]
    fn a_runner_that_reported_or_shows_online_is_never_called_silent() {
        let t = setup("spoke");
        t.e.round(100);
        t.e.shared.heard(REPORT);
        t.e.round(100 + policy::OFFLINE_GRACE_SECS);
        assert!(!t.e.shared.lock().1.no_word);

        // An image from before the report: GitHub's word that it's online wins.
        let t = setup("online");
        t.e.round(100);
        t.e.round(100 + policy::OFFLINE_GRACE_SECS);
        assert!(t.e.shared.lock().1.no_word);
        t.e.round(101 + policy::OFFLINE_GRACE_SECS);
        t.e.round(102 + policy::OFFLINE_GRACE_SECS);
        *t.hub.seen.lock().unwrap() = Some(RunnerSeen::Idle);
        t.e.round(103 + policy::OFFLINE_GRACE_SECS);
        assert!(!t.e.shared.lock().1.no_word);
    }

    #[test]
    fn draining_ends_the_silent_line() {
        let t = setup("silentdrain");
        t.e.round(100);
        t.e.round(100 + policy::OFFLINE_GRACE_SECS);
        assert!(t.e.shared.lock().1.no_word);
        t.e.shared.change(|s| s.drain = true).unwrap();
        t.e.round(101 + policy::OFFLINE_GRACE_SECS);
        assert_eq!(
            t.e.round(102 + policy::OFFLINE_GRACE_SECS).0,
            Step::Wait(Waiting::Drained)
        );
        assert!(!t.e.shared.lock().1.no_word);
    }

    #[test]
    fn with_no_label_set_it_registers_with_only_what_every_runner_carries() {
        let mut t = setup("labels");
        t.e.labels = crate::labels("", "x86_64");
        t.e.round(100);
        let calls = t.hub.calls.lock().unwrap();
        assert!(
            calls[0].ends_with(r#"["self-hosted", "Linux", "X64"]"#),
            "{calls:?}"
        );
    }

    #[test]
    fn releases_are_read_once_a_day() {
        let t = setup("rel");
        t.e.round(RELEASES_EVERY_SECS);
        assert_eq!(t.e.shared.lock().0.releases, [("v9.9.9".to_owned(), 5)]);
        assert_eq!(t.e.shared.lock().0.releases_read, RELEASES_EVERY_SECS);
    }
}
