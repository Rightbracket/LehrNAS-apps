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
     `catalog: official.toml`, `environment: app-channel`, and the three secrets
     passed by name. The job's `if` names this repository, so a copy never runs it.

   `app-gha-runner.yml` is the example.
4. The release builds the image on our own runner (`lehr-image-build`), signs it with
   the app-channel key, writes the entry into `official.toml`, signs the file, and
   pushes that commit and its tag to `main`. Nothing writes that file by hand. A push
   releases only when the app's folder or `tools/release.py` changed since the app's
   last tag; a dispatch always does, which takes the base image's updates.
   - **The key and the Docker Hub token** are secrets of the `app-channel`
     environment (`CHANNEL_KEY`, `CHANNEL_KEY_PASSWORD`, `DOCKERHUB_TOKEN`), which
     admits `main` alone. The release refuses to start without that rule, so only
     the holder of the key can release.
   - **One release at a time** in the repository: each commits a catalog to `main`.
