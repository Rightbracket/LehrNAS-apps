#!/bin/bash
# The runner service, one job per container (docs/spec-runner-app.md §3, §6).
# When this script exits, tini exits, and every process the job left goes with
# the container; `restart: always` brings a clean one with an empty tmpfs.
set -u
umask 077

RUNNER=${RUNNER_DIR:-/opt/runner}
STATE=${RUNNER_STATE_DIR:-/tmp/runner}
SHM=${RUNNER_SHM_DIR:-/dev/shm}
LISTENER=${RUNNER_LISTENER:-bin/Runner.Listener}
JIT="$SHM/lehr-runner-jit"
STOP="$SHM/lehr-runner-stop"
REPORT_PORT=${RUNNER_REPORT_PORT:-8712}
# A job past its limit gets this long to end before it's killed.
STOP_WAIT=${RUNNER_STOP_WAIT:-60}

# The files the links in $RUNNER point at. Each must exist before the runner
# opens it: a dangling .env or .credentials crashes it.
mkdir -p "$STATE/_diag" "$STATE/_work" "$STATE/home"
: > "$STATE/.env"
printf '%s\n' "$PATH" > "$STATE/.path"
for f in .runner .credentials .credentials_rsaparams; do
  printf '{}\n' > "$STATE/$f"
done

# One config, from the broker; it appears whole, by a rename.
until [ -d "$JIT" ]; do
  sleep 1
done
for f in .runner .credentials .credentials_rsaparams; do
  mv -f "$JIT/$f" "$STATE/$f" && chmod 600 "$STATE/$f" || exit 1
done
rm -rf "$JIT"

# A job's only disk is `cache`: toolchains and downloads go there, the rest in RAM.
export HOME="$STATE/home"
export RUNNER_TOOL_CACHE=/cache/hostedtoolcache AGENT_TOOLSDIRECTORY=/cache/hostedtoolcache
export RUSTUP_HOME=/cache/rustup CARGO_HOME=/cache/cargo
export npm_config_cache=/cache/npm PLAYWRIGHT_BROWSERS_PATH=/cache/ms-playwright
export XDG_CACHE_HOME=/cache/xdg UV_CACHE_DIR=/cache/uv PIP_CACHE_DIR=/cache/pip

# Every line stays in the service's log; the ones the status page wants also go
# to the broker. A job can send these too: they only ever fill the page.
report() {
  while IFS= read -r line; do
    printf '%s\n' "$line"
    case $line in
      *'Running job: '*|*' completed with result: '*|*'Runner update in progress'*)
        printf '%s\n' "$line" 2>/dev/null > "/dev/udp/127.0.0.1/$REPORT_PORT"
        ;;
    esac
  done
}

cd "$RUNNER" || exit 1
# With job control on for this one line: without it, a script's background
# command starts with SIGINT ignored, and the signal below would do nothing.
set -m
nice -n 10 "$LISTENER" run > >(report) 2>&1 &
pid=$!
set +m

# GitHub's own service script turns SIGTERM into SIGINT for the runner: that's
# how a stopping box ends the job with the shutdown message (§9.4).
trap 'kill -INT "$pid" 2>/dev/null' TERM INT

asked=0
while kill -0 "$pid" 2>/dev/null; do
  if [ -e "$STOP" ]; then
    if [ "$asked" = 0 ]; then
      kill -INT "$pid" 2>/dev/null
    elif [ "$asked" -ge "$STOP_WAIT" ]; then
      kill -KILL "$pid" 2>/dev/null
    fi
    asked=$((asked + 5))
  fi
  # Backgrounded, so a signal's trap runs at once.
  sleep 5 &
  wait $! 2>/dev/null
done
wait "$pid"
exit $?
