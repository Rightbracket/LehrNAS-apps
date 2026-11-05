#!/bin/bash
# Which service this is. The app's compose can't set a command (spec-apps §1.1),
# and an environment value wouldn't survive a reinstall (§6.1), so the broker is
# the service that was given the token's key. The runner never has it.
if [ -n "${GITHUB_TOKEN+set}" ]; then
  exec /usr/local/bin/gha-runner-broker
fi
exec /usr/local/lib/gha-runner/runner-start.sh
