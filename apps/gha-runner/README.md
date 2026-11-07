# Runner for GitHub Actions

An app for a LehrNAS box: a self-hosted GitHub Actions runner for your own
repositories, one job at a time, on a box that is usually idle. The design is
`docs/spec-runner-app.md` in the LehrNAS repository.

| Path | What it is |
|---|---|
| `entry.toml` | The catalog entry, without its version and digests. The release fills it into `official.toml` |
| `compose.yaml` | The app's two services, one image. The entry points at it |
| `image/` | The image: Ubuntu, the actions/runner release tarball, tini, the broker |
| `src/broker/` | The `broker` service, in Rust, a Cargo project of its own: holds the token, registers runners, serves the status page |
| `tests/smoke.sh` | The runner's start script against a fake listener; needs nothing |
| `tests/pod-check.sh` | Both services in real read-only containers with a fake GitHub; needs docker and the Ubuntu base |
| `CHANGELOG.md` | What each release changed |

## How it works

- **`broker`** holds `GITHUB_TOKEN`. It asks GitHub for one just-in-time config,
  sets `disableUpdate` in it, and writes its three files into `/dev/shm`. The next
  config comes only after GitHub shows the previous runner gone, and the pod's IPC
  is emptied first.
- **`runner`** has no token. Its start takes those files into its own tmpfs, runs
  one job and exits. The container goes with it, so the next one starts clean. Only
  `/cache` survives.
- **The page,** on port 8711, shows the state, the runner's version and its days left,
  and the last 50 jobs, with *Drain*, *Resume* and *Clear cache*. The first visit
  sets its password, and no job runs before then, because a job shares the pod's
  network and can reach the page too.
- **The versions on the page** are what the runner's start reports on a loopback
  port after it takes its config: the runner's version, and the release its image
  was built from (the `RELEASE` file beside the start scripts). A job can send these
  too, so they only ever fill the page.

## Settings

| Key | Service | |
|---|---|---|
| `GITHUB_TOKEN` | broker | A fine-grained token. "Self-hosted runners: write" for an organisation's runner group; "Administration: write" for one repository |
| `RUNNER_SCOPE` | broker | `owner/repository`, or `org/<organisation>/<runner group>` |
| `RUNNER_JOB_HOURS` | any | A job's limit, default 6 |
| `RUNNER_LABELS` | any | Labels on top of `self-hosted`, `Linux` and the architecture, comma-separated. None by default, so this runner can take any job in its group that asks for `self-hosted`. Set a label and name it in your workflows, or use a tight scope, if that matters. |
| `RUNNER_RESET_PASSWORD` | any | Clears the page's password at the next start. Give it a new word each time: a value already acted on does nothing |

## Public repositories

Refused by default, because anyone who can open a pull request may be able to run
code on the box. The page offers a typed confirmation of the repository's name.
After that, each job also needs GitHub's strictest fork-approval setting and no
read-write share grants. Visibility is read every 30 seconds, and a repository that
turns public stops taking jobs.

## Limits, stated

- A job can read its own runner's credentials, which are good for that one runner.
- `/cache` is shared between jobs, so a job can poison it. Everything sharing an
  install shares one level of trust.
- The time limit asks through a marker a job could remove, so it stops a job that
  overruns, not one that means to.
- A job that stops its own listener can outlive its removal and read the next
  config (LehrNAS #263).
- No `container:` or `services:`, and no `sudo`.

## Build and test

```bash
cd apps/gha-runner/src/broker && cargo test
```

```bash
apps/gha-runner/tests/smoke.sh
```

```bash
apps/gha-runner/tests/pod-check.sh
```

```bash
python3 tools/check_template.py
```

A release is `lehrdev runner release gha-runner-YYYY.MMDD.N` in the LehrNAS
repository, which holds the signing key's tooling. `--no-push` is a rehearsal.

The image pins the Ubuntu base by digest and verifies the runner tarball's SHA-256.
A new upstream version changes `RUNNER_VERSION` and both hashes in the `Dockerfile`.
