#!/bin/bash
# The runner service, one job per container (docs/spec-runner-app.md §3, §6,
# §17). When this script exits, tini exits, and every process the job left goes
# with the container; `restart: always` brings a clean one with an empty tmpfs.
# With several instances (§17), each runs this with its own LEHR_INSTANCE and
# its own folder of the `slot` volume at /slot.
set -u
umask 077

RUNNER=${RUNNER_DIR:-/opt/runner}
STATE=${RUNNER_STATE_DIR:-/tmp/runner}
SLOT=${RUNNER_SLOT_DIR:-/slot}
LISTENER=${RUNNER_LISTENER:-bin/Runner.Listener}
INSTANCE=${LEHR_INSTANCE:-1}
# The broker offers this instance's config here, in memory, and the socket
# goes once it's taken (§17.3). Only this instance mounts this folder.
SOCKET="$SLOT/socket"
STOP="$SLOT/stop"
# The box's mark for an instance it lowered the number past (spec-apps §7.1).
LEAVING="$SLOT/.leaving"
RELEASE_FILE=${RUNNER_RELEASE_FILE:-/usr/local/lib/gha-runner/RELEASE}
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

# One config, from the broker, over this instance's socket. The broker's own
# binary, in this image already, takes the three files: each once, all or none.
# A broker that restarted halfway offers a whole new config. An instance the
# box is ending exits here, without ever registering.
TAKE=${RUNNER_TAKE:-/usr/local/bin/gha-runner-broker}
until [ -S "$SOCKET" ] && "$TAKE" take "$SOCKET" "$STATE" 2>/dev/null; do
  [ -e "$LEAVING" ] && exit 0
  sleep 1
done
chmod 600 "$STATE/.runner" "$STATE/.credentials" "$STATE/.credentials_rsaparams" || exit 1

# Which runner this is and the app release its image was built from, for the page
# (§13.4). Once, before the listener starts. Nothing here depends on it being heard.
# Lines to the broker carry this instance's number above the first (§17.3).
TAG=
[ "$INSTANCE" != 1 ] && TAG="@$INSTANCE "
printf '%sRunner report: version=%s release=%s\n' "$TAG" \
  "$(cat "$RUNNER/VERSION" 2>/dev/null)" "$(cat "$RELEASE_FILE" 2>/dev/null)" \
  2>/dev/null > "/dev/udp/127.0.0.1/$REPORT_PORT"

# A job's only disk is `cache`: toolchains and downloads go there, the rest in RAM.
# cargo, npm and uv lock their own caches, so every instance shares them; the
# rest have no such promise, so each instance above the first has its own
# (§17.5). The first keeps today's paths.
OWN=/cache
[ "$INSTANCE" != 1 ] && OWN="/cache/slot$INSTANCE"
export HOME="$STATE/home"
export RUNNER_TOOL_CACHE="$OWN/hostedtoolcache" AGENT_TOOLSDIRECTORY="$OWN/hostedtoolcache"
export RUSTUP_HOME="$OWN/rustup" CARGO_HOME=/cache/cargo
export npm_config_cache=/cache/npm PLAYWRIGHT_BROWSERS_PATH="$OWN/ms-playwright"
export XDG_CACHE_HOME="$OWN/xdg" UV_CACHE_DIR=/cache/uv PIP_CACHE_DIR="$OWN/pip"

# Every line stays in the service's log; the ones the status page wants also go
# to the broker. A job can send these too: they only ever fill the page.
report() {
  while IFS= read -r line; do
    printf '%s\n' "$line"
    case $line in
      *'Running job: '*|*' completed with result: '*|*'Runner update in progress'*)
        printf '%s%s\n' "$TAG" "$line" 2>/dev/null > "/dev/udp/127.0.0.1/$REPORT_PORT"
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
