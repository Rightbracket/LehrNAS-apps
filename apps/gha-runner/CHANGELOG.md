# Changelog

## Unreleased

## gha-runner-2026.1106.3

- There is no default label any more. If your workflows name `lehrnas`, set
  `RUNNER_LABELS=lehrnas`, or their jobs wait for a runner that doesn't match.
- The runner's start reports its version and the release its image was built from,
  and the page's days left count from that. The page says when the two releases
  differ, and when the runner service hasn't been heard from.
- The two services' files in `/dev/shm` are now `gha-runner-jit` and `gha-runner-stop`.
  This changes the contract between them: an image built from an earlier release
  won't start with this one, so rebuild it from this release.
- The page's refusals are sentences. The `REFUSED |` line stays in the service's log.

## gha-runner-2026.1106.2

- The broker's binary no longer carries paths from the build machine (LehrNAS #305).

## gha-runner-2026.1106.1

- `actions/checkout` works: the runner's work folder is its real path, where it
  was a link and git got no token (LehrNAS #300).

## gha-runner-2026.1105.2

- The first version: actions/runner 2.338.2, one job at a time, the broker and its
  status page, and the public-repository refusal.
