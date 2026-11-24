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
- **No binaries in git.** A build copies them in, and `.gitignore` keeps them out.
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
     The caller's job has no environment, so it can't pass app-channel's secrets;
     the release's publish job reads them there itself, which GitHub allows only
     with `inherit`. inherit would also hand every job the repository's and the
     organisation's secrets, so the release refuses a run that has any. The job's
     `if` names this repository, so a copy never runs it, and it joins the app's
     own group: `concurrency: {group: release-<id>, cancel-in-progress: false,
     queue: max}`.

   `app-gha-runner.yml` is the example.
4. The release builds the image on our own runner (`lehr-image-build`), signs it with
   the app-channel key, writes the entry into `official.toml`, signs the file, and
   pushes that commit and its tag to `main`. Nothing writes that file by hand. A push
   releases only when the app's folder or `tools/release.py` changed since the app's
   last tag; a dispatch always does, which takes the base image's updates.
   - **The key and the Docker Hub token** are secrets of the `app-channel`
     environment (`CHANNEL_KEY`, `CHANNEL_KEY_PASSWORD`, `DOCKERHUB_TOKEN`), which
     admits `main` alone. The release refuses to start without that rule, or while
     the repository or the organisation holds any secret, and the publish refuses an
     empty one, so only `main` can release. Set them with `--env app-channel`,
     never as repository secrets.
   - **One release of an app at a time, one publish at a time, none cancelled:**
     the build picks the tag and bakes it into the image, so the app's group holds
     a whole release run, and two builds of one app never pick the same tag. Every
     publish commits a catalog to `main`, so the publish job also joins one group
     for the repository. Both say `queue: max`: up to 100 wait, about in the order
     they began waiting. Under GitHub's default a third arrival would cancel the one
     waiting.
   - **An app needs a `src/broker` crate, for now:** `tools/release.py` reads the
     broker's name from `src/broker/Cargo.toml` to scan the image's binary
     (`broker_name`, line 731), so an app without one fails there until LehrNAS
     #861 is fixed.
