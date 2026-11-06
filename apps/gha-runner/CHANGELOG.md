# Changelog

## Unreleased

- The broker's binary no longer carries paths from the build machine (LehrNAS #305).

## gha-runner-2026.1106.1

- `actions/checkout` works: the runner's work folder is its real path, where it
  was a link and git got no token (LehrNAS #300).

## gha-runner-2026.1105.2

- The first version: actions/runner 2.338.2, one job at a time, the broker and its
  status page, and the public-repository refusal.
