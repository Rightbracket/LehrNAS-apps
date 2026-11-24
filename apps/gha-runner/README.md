# Runner for GitHub Actions

An app for a LehrNAS box: a self-hosted GitHub Actions runner for your own
repositories, one job at a time unless you raise the number, on a box that is
usually idle. The design is
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

- **`broker`** holds `GITHUB_TOKEN`. It asks GitHub for one just-in-time config per
  runner, sets `disableUpdate` in it, and offers its three files on a socket in that
  runner's folder of the `slot` volume, in memory only; the socket goes once they're
  taken. A runner's next config comes only after GitHub shows its previous runner
  gone, and the pod's IPC is emptied first while no job is running.
- **`runner`** has no token. Its start takes those files into its own tmpfs with the
  broker's binary (`gha-runner-broker take`), runs one job and exits. The container
  goes with it, so the next one starts clean. Only `/cache` survives.
- **The page,** on port 8711, shows the state, the runner's version and its days left,
  and the last 50 jobs, with *Drain*, *Resume* and *Clear cache*. The first visit
  sets its password, and no job runs before then, because a job shares the pod's
  network and can reach the page too.
- **The versions on the page** are what the runner's start reports on a loopback
  port after it takes its config: the runner's version, and the release its image
  was built from (the `RELEASE` file beside the start scripts). A job can send these
  too, so they only ever fill the page.

## Several runners at once

The number is the box's, on this app's card under Apps (*Instances* and
*Change…*). The page shows it and a row per runner, and doesn't set it.

- **Jobs running together share more.** They can read each other's files in
  `/cache`, `/dev/shm` and any share this app has, and they share `localhost`, so
  tests that use a fixed port clash. Use more than one only for repositories that
  trust each other.
- **Memory is the app's,** so each runner's `mem_limit` adds up: the card gives the
  sum. A job past the limit may get a neighbour's process killed.
- **Caches:** cargo, npm and uv lock their own and are shared. rustup, the tool
  cache, pip, Playwright and `XDG_CACHE_HOME` aren't safe to share, so each runner
  above the first has its own under `/cache/slot<n>/`. The 20 GiB cap is for all of
  `/cache`. *Clear cache* waits until no job is running, and no runner starts a job
  until then.
- **A public repository runs one job at a time,** whatever the number.
- **Lowering the number** never ends a running job: that runner finishes, then
  leaves.
- **An older box** ignores the setting and runs one runner.

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

A release is a push to `main` that changes this folder or `tools/release.py`, or
a dispatch of `app-gha-runner.yml`: `release.yml` builds, signs and commits it on
our own runner (`docs/adding-an-app.md`), and every official publish waits for
the owner's approval in the run. Once the workflow has released the runner, nothing
is released by hand: a release by hand alongside a push's run would pick the same
day's tag, and one of the two would be refused. Until then, LehrNAS's `lehrdev
runner release` is the way by hand.

The image is built from this folder, `-f image/Dockerfile`; its first stage
builds the broker for both architectures, so building it needs no Rust here:

```bash
docker buildx build -f apps/gha-runner/image/Dockerfile --platform linux/amd64,linux/arm64 \
  --build-arg APP_RELEASE=gha-runner-YYYY.MMDD.N apps/gha-runner
```

The image pins the Ubuntu base and the Rust image by digest and verifies the
runner tarball's SHA-256. A new upstream version changes `RUNNER_VERSION` and
both hashes in the `Dockerfile`; a new toolchain in `rust-toolchain.toml`
changes the Rust image's tag and digest with it.
