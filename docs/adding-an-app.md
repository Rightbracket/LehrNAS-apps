# Adding an official app

Only official LehrNAS apps live here; anyone else's go in their own catalog
([publish your own](publish-your-own-catalog.md)). This repository takes no pull
requests: its branches are reviewed and merged by the project's lead.

## The folder

`apps/<id>/`, named by the catalog id, which is permanent. The same id names the
image (`eshork/<id>`) and its tags (`<id>-<version>`).

| Path | What it is |
|---|---|
| `entry.toml` | The entry, without its version and digests |
| `image/` | What builds the image |
| `src/` | The app's code |
| `tests/` | Its tests |
| `README.md` | What it is and how it works |
| `CHANGELOG.md` | What each release changed |
| anything the entry points at | `compose.yaml`, for example |

`apps/gha-runner/` is a worked example.

## The rules

- **Each app stands alone:** its own language, lockfile and toolchain, with no
  repository-wide workspace. `shared/` appears only when two apps really share code.
- **No binaries in git.** An image builds its own binaries in a stage of its
  Dockerfile, so nothing built is ever copied into the tree.
- **A retired app loses its entry and its folder.** Its tags keep the code reachable.
- **A new compose key goes by path.** A box parses inline compose while it reads the
  catalog, and an entry it can't parse refuses the whole file, for every app. Use
  `compose_path` when the compose has a key older boxes may not know.

## The steps

1. Make the folder.
2. Check the entry from the repository's root: `python3 tools/check_template.py`.
3. Add `.github/workflows/app-<id>.yml`, a thin caller of the two shared workflows.
   It owns the triggers (a push that changes the app's folder, `tools/` or the shared
   workflows, and a dispatch) and names what to run:
   - **`app.yml`, the app's CI:** its Rust crates (`rust:`, folders under the app's,
     one a line: `cargo fmt --check`, clippy with warnings denied and the tests, all
     `--locked`) and its test scripts (`tests:`, one a line, run from the root).
   - **`release.yml`, its release, after the CI and on `main` alone:** `app`,
     `catalog: official.toml` and `environment: app-channel`, and `secrets: inherit`.
     The release's publish job reads app-channel's secrets there itself; GitHub
     gives it one only for a name its caller passes, and `inherit` passes them all.
     inherit would also hand every job the repository's and the
     organisation's secrets, so the release refuses a run that has any. The job's
     `if` names this repository, so a copy never runs it, and it joins the app's
     own group: `concurrency: {group: release-<id>, cancel-in-progress: false,
     queue: max}`. It grants what the release needs, `permissions: {contents:
     write, actions: read, deployments: read}`: without that, GitHub refuses the
     whole workflow at its start, the CI too, because the called jobs ask for more
     than the caller allows.

   `app-gha-runner.yml` is the example.
4. The release builds the image on our own runner (`lehr-image-build`), in the VM its
   `.env` names (`LEHR_BUILD_ENGINE=lehr-build-apps`; the build refuses a runner that
   names none or another), signs it with
   the app-channel key, writes the entry into `official.toml`, signs the file, and
   pushes that commit and its tag to `main`. Nothing writes that file by hand. A push
   releases only when the app's folder or `tools/release.py` changed since the app's
   last tag; a dispatch always does, which takes the named apt packages' updates. The
   base images are pinned by digest, so their updates come with a change to the
   Dockerfile's digests, which a push then releases.
   - **The key and the Docker Hub token** are secrets of the `app-channel`
     environment (`CHANNEL_KEY`, `CHANNEL_KEY_PASSWORD`, `DOCKERHUB_TOKEN`), which
     admits `main` alone. The release refuses to start without that rule, or while
     this repository can read any repository or organisation secret, and the publish
     refuses an empty one, so only `main` can release. Set them with `--env
     app-channel`, never as repository secrets.
   - **Every official publish waits for the owner:** `app-channel` has the owner as
     its required reviewer, and the release refuses to start without one, so each
     publish waits for the owner to approve it in the run (LehrNAS #875).
   - **One release of an app at a time, none cancelled:** the build picks the tag and
     bakes it into the image, so the app's group is meant to hold a whole release run,
     and two builds of one app to pick different tags. GitHub's pages don't say that
     a caller job's group covers the jobs it calls, and no two runs have overlapped
     yet; if it doesn't, the second publish is refused at its tag check. It says `queue: max`: up to 100 wait,
     about in the order they began waiting. Under GitHub's default a third arrival
     would cancel the one waiting.
   - **Two apps' releases can collide on `main`:** each publish commits its catalog
     entry on its own run's commit, so one that finds `main` moved by another app's
     release is refused, and its signed image waits for a dispatch (LehrNAS #874).
     With one app, nothing does yet.
   - **An app needs a `src/broker` crate, for now:** `tools/release.py` reads the
     broker's name from `src/broker/Cargo.toml` to scan the image's binary
     (`broker_name`), so an app without one fails there until LehrNAS
     #861 is fixed.
