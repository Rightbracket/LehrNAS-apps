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
3. Add `.github/workflows/app-<id>.yml`, which builds and tests the app when its folder
   or `tools/` changes. `app-gha-runner.yml` is the example.
4. The first release writes the entry into `official.toml` and signs the file. Nothing
   writes that file by hand. Only the holder of the key can release.
