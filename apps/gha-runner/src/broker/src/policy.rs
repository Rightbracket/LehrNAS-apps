//! What the broker decides, with nothing around it: no GitHub, no files, no
//! clock of its own (`docs/spec-runner-app.md` §3, §6, §7, §8). Everything here
//! is a function of what it's given, so all of it is a plain test.

/// How long a registered runner may look offline before the broker takes it
/// off GitHub. A runner that died (a crash, a power loss) would otherwise hold
/// the one slot for the day GitHub takes to remove it.
pub const OFFLINE_GRACE_SECS: u64 = 300;
/// The fork-approval value that asks for approval from every outside
/// contributor: GitHub's strictest.
pub const STRICTEST_FORK_APPROVAL: &str = "all_external_contributors";
/// `cache`'s cap (§2): emptied between jobs when it has grown past this.
pub const CACHE_CAP_BYTES: u64 = 20 << 30;
/// GitHub's clock for a runner with updates off, and where the page warns (§8).
pub const UPDATE_DEADLINE_DAYS: u64 = 30;
pub const UPDATE_WARN_DAYS: u64 = 20;

/// What the admin pointed this install at (§1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Repo { owner: String, name: String },
    OrgGroup { org: String, group: String },
}

fn word_ok(w: &str) -> bool {
    !w.is_empty()
        && w != "."
        && w != ".."
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Scope {
    /// `owner/repo`, or `org/<org>/<runner group>`.
    ///
    /// # Errors
    /// Anything else, in a sentence for the page.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "RUNNER_SCOPE is {text:?}: it should be owner/repository, or org/<organisation>/<runner group>"
            )
        };
        let parts: Vec<&str> = text.trim().split('/').collect();
        match parts.as_slice() {
            ["org", org, group] if word_ok(org) && !group.trim().is_empty() => Ok(Self::OrgGroup {
                org: (*org).to_owned(),
                group: (*group).to_owned(),
            }),
            [owner, name] if word_ok(owner) && word_ok(name) => Ok(Self::Repo {
                owner: (*owner).to_owned(),
                name: (*name).to_owned(),
            }),
            _ => Err(bad()),
        }
    }

    /// The name people know it by, and the word a public repository's
    /// confirmation types.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Repo { owner, name } => format!("{owner}/{name}"),
            Self::OrgGroup { org, group } => format!("{org}'s runner group {group}"),
        }
    }
}

/// Who can reach the scope, as GitHub says today.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// A private (or enterprise-internal) repository, or a runner group
    /// GitHub keeps public repositories out of.
    Closed,
    Public,
    /// A runner group that lets public repositories in. The token's one
    /// permission can't read their fork settings, so this is refused
    /// outright, with what to change (D-next in the spec).
    GroupAllowsPublic,
}

/// A share the app was granted, seen at `/shares/<name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub share: String,
    pub read_write: bool,
}

/// Why no job may run, as a fact line and a sentence for the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub line: String,
    pub sentence: String,
    /// The typed confirmation would get past this one.
    pub can_confirm: bool,
}

fn refused(subject: &str, why: &str, sentence: &str, can_confirm: bool) -> Refusal {
    Refusal {
        line: format!("REFUSED | APP SETUP | gha-runner | {subject} {why}"),
        sentence: sentence.to_owned(),
        can_confirm,
    }
}

/// The sentence above the typed confirmation (§7).
pub const PUBLIC_WARNING: &str = "Anyone who can open a pull request on this repository may be able to run code on this box, inside this app. They could use its grants, and your internet connection.";

/// May the next job run? §7's rules, in the order the admin can act on them.
/// `confirmed` is the name typed earlier; `fork_approval` is GitHub's setting,
/// read only for a public repository (`None`: not read).
///
/// # Errors
/// The refusal.
pub fn gate(
    scope: &Scope,
    exposure: &Exposure,
    confirmed: Option<&str>,
    fork_approval: Option<&str>,
    grants: &[Grant],
) -> Result<(), Refusal> {
    let label = scope.label();
    match exposure {
        Exposure::Closed => Ok(()),
        Exposure::GroupAllowsPublic => Err(refused(
            &label,
            "allows public repositories: this app can't read their fork settings with its token",
            "This runner group lets public repositories use it. Turn that off in the group's settings on GitHub, or point the app at one repository.",
            false,
        )),
        Exposure::Public => {
            if confirmed != Some(label.as_str()) {
                return Err(refused(
                    &label,
                    "is public: anyone who can open a pull request may be able to run code on this box",
                    &format!("{label} is public. {PUBLIC_WARNING}"),
                    true,
                ));
            }
            if fork_approval != Some(STRICTEST_FORK_APPROVAL) {
                return Err(refused(
                    &label,
                    "is public and doesn't ask for approval from every outside contributor",
                    &format!(
                        "{label} is public. On GitHub, under the repository's Settings, Actions, General, set approval for fork pull requests to \"Require approval for all external contributors\". Jobs stay stopped until then."
                    ),
                    false,
                ));
            }
            if let Some(g) = grants.iter().find(|g| g.read_write) {
                return Err(refused(
                    &label,
                    &format!(
                        "is public and this app has read-write access to the share {}",
                        g.share
                    ),
                    &format!(
                        "{label} is public, and this app has read-write access to the share {}. A public repository can't use read-write shares here. Make the grant read-only or remove it. Jobs stay stopped until then.",
                        g.share
                    ),
                    false,
                ));
            }
            Ok(())
        }
    }
}

/// The typed confirmation (§7, D49's pattern): the repository's name, exactly.
/// A runner group can't be confirmed, and neither can anything not public.
#[must_use]
pub fn confirmation_ok(scope: &Scope, exposure: &Exposure, typed: &str) -> bool {
    matches!(scope, Scope::Repo { .. })
        && *exposure == Exposure::Public
        && typed.trim() == scope.label()
}

/// What a confirmation is worth now: a repository seen closed gives it up, so
/// turning public again asks again.
#[must_use]
pub fn confirmation_kept(exposure: &Exposure, confirmed: Option<String>) -> Option<String> {
    (*exposure != Exposure::Closed)
        .then_some(confirmed)
        .flatten()
}

/// GitHub's word on the registered runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerSeen {
    Gone,
    Offline,
    Idle,
    Busy,
}

/// The gate's result for this round. `Unreadable`: GitHub wasn't reached or
/// wouldn't say, which never lets a new runner register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Open,
    Refused(Refusal),
    Unreadable(String),
}

/// The runner the broker has registered, with what it knows about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Current {
    /// `None`: GitHub couldn't be asked this round.
    pub seen: Option<RunnerSeen>,
    pub written_secs_ago: u64,
    /// How long it has been busy, as far as the broker has seen.
    pub busy_secs: u64,
    pub stop_already_asked: bool,
}

/// One round's inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Round {
    pub set_up: bool,
    pub drain: bool,
    pub clear_cache: bool,
    pub gate: Gate,
    pub current: Option<Current>,
    pub job_limit_secs: u64,
}

/// One round's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Clean the pod and register the next runner.
    Register,
    /// GitHub shows the runner gone: forget it. The next round registers.
    Forget,
    /// An idle or dead runner has to leave GitHub.
    Remove,
    /// A job ran past its time.
    StopJob,
    Wait(Waiting),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// The page's password isn't set: a job shares the pod's network, so no
    /// job runs until the admin has claimed the page.
    NoPassword,
    Drained,
    Refused(Refusal),
    Unreadable(String),
    /// Registered, online or on its way there.
    Idle,
    Running,
    /// GitHub couldn't be asked about the runner.
    Unknown,
}

/// The one rule of §3: the next config only after GitHub shows the previous
/// runner gone. A job in progress is never interrupted except by its time
/// limit; everything else waits for it.
#[must_use]
pub fn decide(r: &Round) -> Step {
    let Some(c) = r.current else {
        return if !r.set_up {
            Step::Wait(Waiting::NoPassword)
        } else if r.drain {
            Step::Wait(Waiting::Drained)
        } else {
            match &r.gate {
                Gate::Open => Step::Register,
                Gate::Refused(x) => Step::Wait(Waiting::Refused(x.clone())),
                Gate::Unreadable(x) => Step::Wait(Waiting::Unreadable(x.clone())),
            }
        };
    };
    let leave = !r.set_up || r.drain || r.clear_cache || matches!(r.gate, Gate::Refused(_));
    match c.seen {
        None => Step::Wait(Waiting::Unknown),
        Some(RunnerSeen::Gone) => Step::Forget,
        Some(RunnerSeen::Busy) => {
            if c.busy_secs >= r.job_limit_secs && !c.stop_already_asked {
                Step::StopJob
            } else {
                Step::Wait(Waiting::Running)
            }
        }
        Some(RunnerSeen::Idle) => {
            if leave {
                Step::Remove
            } else {
                Step::Wait(Waiting::Idle)
            }
        }
        Some(RunnerSeen::Offline) => {
            if leave || c.written_secs_ago >= OFFLINE_GRACE_SECS {
                Step::Remove
            } else {
                Step::Wait(Waiting::Idle)
            }
        }
    }
}

/// What this round says about §13.2's silent runner: `Some(true)` when GitHub
/// shows it offline and it hasn't reported by the end of the grace, `Some(false)`
/// once GitHub shows it online, `None` when this round doesn't tell. The offline
/// runner is removed and the next one takes its place at that same moment, so the
/// answer has to outlast the runner it was about.
#[must_use]
pub fn no_word(c: &Current, reported: bool) -> Option<bool> {
    match c.seen {
        Some(RunnerSeen::Idle | RunnerSeen::Busy) => Some(false),
        Some(RunnerSeen::Offline) if !reported && c.written_secs_ago >= OFFLINE_GRACE_SECS => {
            Some(true)
        }
        _ => None,
    }
}

/// `cache` empties when the admin asked or when it has grown past its cap.
#[must_use]
pub fn empty_cache(asked: bool, bytes: u64) -> bool {
    asked || bytes > CACHE_CAP_BYTES
}

/// `2.338.2` (a leading `v` allowed) as numbers, to compare.
#[must_use]
pub fn version(tag: &str) -> Option<(u32, u32, u32)> {
    let mut p = tag.trim().trim_start_matches('v').split('.');
    let v = (
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    );
    p.next().is_none().then_some(v)
}

/// The clock of §8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateClock {
    pub newer: String,
    pub days_left: u64,
    pub warn: bool,
}

/// GitHub stops queueing jobs 30 days after a new version on a runner with
/// updates off, so the clock runs from the oldest release newer than this
/// runner. `None`: this is the newest.
#[must_use]
pub fn update_clock(mine: &str, releases: &[(String, u64)], now: u64) -> Option<UpdateClock> {
    let mine = version(mine)?;
    let (tag, published) = releases
        .iter()
        .filter(|(t, _)| version(t).is_some_and(|v| v > mine))
        .min_by_key(|(_, p)| *p)?;
    let days = now.saturating_sub(*published) / 86_400;
    Some(UpdateClock {
        newer: tag.trim_start_matches('v').to_owned(),
        days_left: UPDATE_DEADLINE_DAYS.saturating_sub(days),
        warn: days >= UPDATE_WARN_DAYS,
    })
}

/// A line of the runner's output, as it reaches the broker. Anyone in the pod
/// can send these, a job included, so they feed the page and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    Started(String),
    Finished {
        job: String,
        result: String,
    },
    UpdateInProgress,
    /// The runner's start, once it has taken its config (§13.4). What can't be
    /// a version or a release's name is `None`.
    Report {
        runner: Option<String>,
        release: Option<String>,
    },
}

/// A release's name as the page may show it.
fn release_word(w: &str) -> Option<String> {
    (!w.is_empty()
        && w.len() <= 64
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    .then(|| w.to_owned())
}

/// The lines the page cares about, with anything unprintable taken out.
#[must_use]
pub fn said(line: &str) -> Option<Said> {
    let clean: String = line.chars().filter(|c| !c.is_control()).take(300).collect();
    // The runner prefixes its lines with a time: "2026-11-05 11:14:31Z: ".
    let text = clean
        .split_once("Z: ")
        .map_or(clean.as_str(), |(_, t)| t)
        .trim();
    if let Some(rest) = text.strip_prefix("Runner report: ") {
        let field = |key: &str| {
            rest.split(' ')
                .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
        };
        return Some(Said::Report {
            runner: field("version")
                .filter(|v| version(v).is_some())
                .map(str::to_owned),
            release: field("release").and_then(release_word),
        });
    }
    if text.contains("Runner update in progress") {
        return Some(Said::UpdateInProgress);
    }
    if let Some(job) = text.strip_prefix("Running job: ") {
        return Some(Said::Started(job.to_owned()));
    }
    let rest = text.strip_prefix("Job ")?;
    let (job, result) = rest.rsplit_once(" completed with result: ")?;
    Some(Said::Finished {
        job: job.to_owned(),
        result: result.to_owned(),
    })
}

/// The share grants in `/proc/self/mountinfo`'s text: mounts at `/shares/<name>`,
/// read-write unless their options say `ro`.
#[must_use]
pub fn grants_in(mountinfo: &str) -> Vec<Grant> {
    mountinfo
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let at = f.nth(4)?;
            let opts = f.next()?;
            let share = at.strip_prefix("/shares/")?;
            (!share.is_empty() && !share.contains('/')).then(|| Grant {
                share: share.to_owned(),
                read_write: !opts.split(',').any(|o| o == "ro"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Scope {
        Scope::parse("me/thing").unwrap()
    }

    fn round() -> Round {
        Round {
            set_up: true,
            drain: false,
            clear_cache: false,
            gate: Gate::Open,
            current: None,
            job_limit_secs: 6 * 3600,
        }
    }

    fn with(seen: RunnerSeen) -> Round {
        Round {
            current: Some(Current {
                seen: Some(seen),
                written_secs_ago: 10,
                busy_secs: 0,
                stop_already_asked: false,
            }),
            ..round()
        }
    }

    fn refusal() -> Gate {
        Gate::Refused(gate(&repo(), &Exposure::Public, None, None, &[]).unwrap_err())
    }

    #[test]
    fn a_scope_is_a_repository_or_an_organisations_group() {
        assert_eq!(
            Scope::parse("org/acme/ci runners").unwrap(),
            Scope::OrgGroup {
                org: "acme".into(),
                group: "ci runners".into()
            }
        );
        assert_eq!(repo().label(), "me/thing");
        for bad in ["", "thing", "a/b/c", "me/..", "me/a b", "org/acme/ "] {
            assert!(Scope::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_next_runner_waits_until_github_shows_the_last_one_gone() {
        assert_eq!(decide(&round()), Step::Register);
        for seen in [RunnerSeen::Idle, RunnerSeen::Busy, RunnerSeen::Offline] {
            assert!(
                matches!(decide(&with(seen)), Step::Wait(_)),
                "{seen:?} must not register another"
            );
        }
        assert_eq!(decide(&with(RunnerSeen::Gone)), Step::Forget);
    }

    #[test]
    fn github_not_answering_about_the_runner_never_registers_another() {
        let mut r = with(RunnerSeen::Idle);
        r.current.as_mut().unwrap().seen = None;
        assert_eq!(decide(&r), Step::Wait(Waiting::Unknown));
    }

    #[test]
    fn a_public_repository_is_refused_with_the_specs_line() {
        let e = gate(
            &repo(),
            &Exposure::Public,
            None,
            Some(STRICTEST_FORK_APPROVAL),
            &[],
        )
        .unwrap_err();
        assert_eq!(
            e.line,
            "REFUSED | APP SETUP | gha-runner | me/thing is public: anyone who can open a pull request may be able to run code on this box"
        );
        assert!(e.can_confirm);
        assert!(e.sentence.ends_with(PUBLIC_WARNING), "{e:?}");
        assert_eq!(
            decide(&Round {
                gate: refusal(),
                ..round()
            }),
            Step::Wait(Waiting::Refused(
                gate(&repo(), &Exposure::Public, None, None, &[]).unwrap_err()
            ))
        );
    }

    #[test]
    fn a_confirmed_public_repository_still_needs_both_conditions() {
        let name = Some("me/thing");
        let strict = Some(STRICTEST_FORK_APPROVAL);
        assert!(gate(&repo(), &Exposure::Public, name, strict, &[]).is_ok());
        for weak in [Some("first_time_contributors"), None] {
            let e = gate(&repo(), &Exposure::Public, name, weak, &[]).unwrap_err();
            assert!(e.line.contains("outside contributor"), "{e:?}");
            assert!(!e.can_confirm);
        }
        let rw = [
            Grant {
                share: "ro".into(),
                read_write: false,
            },
            Grant {
                share: "photos".into(),
                read_write: true,
            },
        ];
        let e = gate(&repo(), &Exposure::Public, name, strict, &rw).unwrap_err();
        assert!(
            e.line.contains("read-write access to the share photos"),
            "{e:?}"
        );
        assert!(gate(&repo(), &Exposure::Public, name, strict, &rw[..1]).is_ok());
        // Another repository's confirmation is none.
        assert!(gate(&repo(), &Exposure::Public, Some("me/other"), strict, &[]).is_err());
    }

    #[test]
    fn a_private_repository_needs_no_confirmation_and_may_have_any_grant() {
        let rw = [Grant {
            share: "x".into(),
            read_write: true,
        }];
        assert!(gate(&repo(), &Exposure::Closed, None, None, &rw).is_ok());
    }

    #[test]
    fn a_group_that_allows_public_repositories_cant_be_confirmed() {
        let s = Scope::parse("org/acme/ci").unwrap();
        let e = gate(&s, &Exposure::GroupAllowsPublic, Some("org"), None, &[]).unwrap_err();
        assert!(!e.can_confirm);
        assert!(!confirmation_ok(
            &s,
            &Exposure::GroupAllowsPublic,
            &s.label()
        ));
    }

    #[test]
    fn the_confirmation_is_the_name_typed_and_only_while_public() {
        assert!(confirmation_ok(&repo(), &Exposure::Public, " me/thing "));
        assert!(!confirmation_ok(&repo(), &Exposure::Public, "thing"));
        assert!(!confirmation_ok(&repo(), &Exposure::Public, "ME/THING"));
        assert!(!confirmation_ok(&repo(), &Exposure::Closed, "me/thing"));
    }

    #[test]
    fn a_repository_seen_private_gives_up_its_confirmation() {
        let c = Some("me/thing".to_owned());
        assert_eq!(confirmation_kept(&Exposure::Public, c.clone()), c);
        assert_eq!(confirmation_kept(&Exposure::Closed, c), None);
    }

    #[test]
    fn turning_public_stops_taking_jobs_but_not_the_one_running() {
        let idle = Round {
            gate: refusal(),
            ..with(RunnerSeen::Idle)
        };
        assert_eq!(decide(&idle), Step::Remove);
        let busy = Round {
            gate: refusal(),
            ..with(RunnerSeen::Busy)
        };
        assert_eq!(decide(&busy), Step::Wait(Waiting::Running));
        let after = Round {
            gate: refusal(),
            ..round()
        };
        assert!(matches!(decide(&after), Step::Wait(Waiting::Refused(_))));
    }

    #[test]
    fn github_not_answering_about_visibility_keeps_the_idle_runner_and_registers_nothing() {
        let g = Gate::Unreadable("no route".into());
        let idle = Round {
            gate: g.clone(),
            ..with(RunnerSeen::Idle)
        };
        assert_eq!(decide(&idle), Step::Wait(Waiting::Idle));
        let none = Round { gate: g, ..round() };
        assert_eq!(
            decide(&none),
            Step::Wait(Waiting::Unreadable("no route".into()))
        );
    }

    #[test]
    fn no_job_runs_before_the_page_has_its_password() {
        let r = Round {
            set_up: false,
            ..round()
        };
        assert_eq!(decide(&r), Step::Wait(Waiting::NoPassword));
        let idle = Round {
            set_up: false,
            ..with(RunnerSeen::Idle)
        };
        assert_eq!(decide(&idle), Step::Remove);
    }

    #[test]
    fn drain_lets_the_job_finish_and_registers_nothing() {
        assert_eq!(
            decide(&Round {
                drain: true,
                ..round()
            }),
            Step::Wait(Waiting::Drained)
        );
        assert_eq!(
            decide(&Round {
                drain: true,
                ..with(RunnerSeen::Idle)
            }),
            Step::Remove
        );
        assert_eq!(
            decide(&Round {
                drain: true,
                ..with(RunnerSeen::Busy)
            }),
            Step::Wait(Waiting::Running)
        );
    }

    #[test]
    fn clearing_the_cache_waits_for_the_job_and_moves_an_idle_runner_out_of_the_way() {
        assert_eq!(
            decide(&Round {
                clear_cache: true,
                ..with(RunnerSeen::Idle)
            }),
            Step::Remove
        );
        assert_eq!(
            decide(&Round {
                clear_cache: true,
                ..with(RunnerSeen::Busy)
            }),
            Step::Wait(Waiting::Running)
        );
    }

    #[test]
    fn a_dead_runner_leaves_after_the_grace_a_new_one_gets_to_start() {
        let mut r = with(RunnerSeen::Offline);
        assert_eq!(decide(&r), Step::Wait(Waiting::Idle));
        r.current.as_mut().unwrap().written_secs_ago = OFFLINE_GRACE_SECS;
        assert_eq!(decide(&r), Step::Remove);
    }

    #[test]
    fn a_job_past_its_time_is_stopped_once() {
        let mut r = with(RunnerSeen::Busy);
        r.current.as_mut().unwrap().busy_secs = r.job_limit_secs;
        assert_eq!(decide(&r), Step::StopJob);
        r.current.as_mut().unwrap().stop_already_asked = true;
        assert_eq!(decide(&r), Step::Wait(Waiting::Running));
    }

    #[test]
    fn the_cache_empties_when_asked_or_past_its_cap() {
        assert!(empty_cache(true, 0));
        assert!(empty_cache(false, CACHE_CAP_BYTES + 1));
        assert!(!empty_cache(false, CACHE_CAP_BYTES));
    }

    #[test]
    fn the_update_clock_runs_from_the_oldest_newer_release() {
        let day = 86_400;
        let rel = [
            ("v2.340.0".to_owned(), 50 * day),
            ("v2.339.0".to_owned(), 30 * day),
            ("v2.338.2".to_owned(), 10 * day),
            ("nightly".to_owned(), 99 * day),
        ];
        assert_eq!(update_clock("2.340.0", &rel, 60 * day), None);
        let c = update_clock("2.338.2", &rel, 45 * day).unwrap();
        assert_eq!(
            (c.newer.as_str(), c.days_left, c.warn),
            ("2.339.0", 15, false)
        );
        let c = update_clock("2.338.2", &rel, 50 * day).unwrap();
        assert_eq!((c.days_left, c.warn), (10, true));
        assert_eq!(
            update_clock("2.338.2", &rel, 99 * day).unwrap().days_left,
            0
        );
    }

    #[test]
    fn the_runners_output_reads_as_the_page_needs_it() {
        assert_eq!(
            said("2026-11-05 11:14:31Z: Running job: web (x)"),
            Some(Said::Started("web (x)".into()))
        );
        assert_eq!(
            said("2026-11-05 11:20:00Z: Job web (x) completed with result: Succeeded"),
            Some(Said::Finished {
                job: "web (x)".into(),
                result: "Succeeded".into()
            })
        );
        assert_eq!(
            said("Runner update in progress, do not shutdown"),
            Some(Said::UpdateInProgress)
        );
        assert_eq!(said("Listening for Jobs"), None);
        assert_eq!(
            said("Running job: a\u{1b}[31mb"),
            Some(Said::Started("a[31mb".into())),
            "no control characters reach the page"
        );
    }

    #[test]
    fn the_runners_report_reads_and_what_isnt_a_version_or_a_release_is_dropped() {
        assert_eq!(
            said("Runner report: version=2.338.2 release=gha-runner-2026.1106.2"),
            Some(Said::Report {
                runner: Some("2.338.2".into()),
                release: Some("gha-runner-2026.1106.2".into())
            })
        );
        assert_eq!(
            said("Runner report: version= release="),
            Some(Said::Report {
                runner: None,
                release: None
            }),
            "an image without the files still reports that it started"
        );
        assert_eq!(
            said("Runner report: version=soon release=<b>x"),
            Some(Said::Report {
                runner: None,
                release: None
            })
        );
    }

    #[test]
    fn no_word_is_only_an_offline_runner_silent_past_the_grace() {
        let c = |seen, written_secs_ago| Current {
            seen: Some(seen),
            written_secs_ago,
            busy_secs: 0,
            stop_already_asked: false,
        };
        let grace = OFFLINE_GRACE_SECS;
        assert_eq!(no_word(&c(RunnerSeen::Offline, grace), false), Some(true));
        assert_eq!(no_word(&c(RunnerSeen::Offline, grace - 1), false), None);
        assert_eq!(no_word(&c(RunnerSeen::Offline, grace), true), None);
        assert_eq!(no_word(&c(RunnerSeen::Idle, grace), false), Some(false));
        assert_eq!(no_word(&c(RunnerSeen::Busy, grace), false), Some(false));
        assert_eq!(no_word(&c(RunnerSeen::Gone, grace), false), None);
    }

    #[test]
    fn a_refusals_sentence_keeps_every_fact_of_its_line() {
        let s = Scope::parse("me/thing").unwrap();
        let e = gate(&s, &Exposure::Public, None, None, &[]).unwrap_err();
        assert!(e.sentence.starts_with("me/thing is public. "), "{e:?}");
        assert!(e.sentence.ends_with(PUBLIC_WARNING), "{e:?}");
        let e = gate(&s, &Exposure::Public, Some("me/thing"), Some("none"), &[]).unwrap_err();
        assert!(e.sentence.starts_with("me/thing is public. "), "{e:?}");
        let g = [Grant {
            share: "builds".into(),
            read_write: true,
        }];
        let e = gate(
            &s,
            &Exposure::Public,
            Some("me/thing"),
            Some(STRICTEST_FORK_APPROVAL),
            &g,
        )
        .unwrap_err();
        assert!(e.sentence.contains("the share builds"), "{e:?}");
        assert!(e.line.starts_with("REFUSED | "), "the log keeps its line");
        assert!(!e.sentence.contains('|'), "{e:?}");
    }

    #[test]
    fn grants_come_from_the_mount_table() {
        let t = "\
36 35 0:30 / /shares/photos rw,relatime shared:1 - btrfs /dev/x rw
37 35 0:30 / /shares/music ro,relatime shared:1 - btrfs /dev/x rw
38 35 0:30 / /cache rw,relatime - btrfs /dev/x rw
39 35 0:30 / /shares/a/b rw - btrfs /dev/x rw
";
        assert_eq!(
            grants_in(t),
            vec![
                Grant {
                    share: "photos".into(),
                    read_write: true
                },
                Grant {
                    share: "music".into(),
                    read_write: false
                },
            ]
        );
    }
}
