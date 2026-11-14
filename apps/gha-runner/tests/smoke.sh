#!/bin/bash
# The start script's hand-off and restart, with no GitHub, no image and no root:
# a fake listener and a fake `take` in a temporary directory. Run by CI. The
# socket itself is the broker's tests' and pod-check.sh's (§17.3).
#   apps/gha-runner/tests/smoke.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'kill "${pid:-0}" "${ear:-0}" 2>/dev/null || true; rm -rf "$t"' EXIT
# As tini starts it: a background command in a script would start with SIGINT
# ignored and pass that on, which isn't what the service sees.
set -m
mkdir -p "$t/runner/bin" "$t/slot" "$t/offer"
cat > "$t/runner/bin/fake" <<'F'
#!/bin/bash
# Stays until told, like a listener; a signal ends it with a word.
echo "args: $*"
ls -A "$RUNNER_STATE_DIR" | tr '\n' ' '; echo
cat "$RUNNER_STATE_DIR/.runner"; echo
echo "rustup: $RUSTUP_HOME cargo: $CARGO_HOME tools: $RUNNER_TOOL_CACHE"
trap 'echo got-INT; exit 7' INT
trap 'echo got-TERM; exit 8' TERM
for _ in $(seq 1 300); do sleep 0.1; [ -e "$RUNNER_STATE_DIR/quit" ] && exit 0; done
F
chmod +x "$t/runner/bin/fake"
# A listener that runs one job and ends as a real just-in-time one does: with 2.
# FAKE_SAY_DONE=0 leaves out the job's last line (a listener that failed).
cat > "$t/runner/bin/fake-job" <<'F'
#!/bin/bash
echo "2026-11-14 05:20:02Z: Running job: harness"
[ "${FAKE_SAY_DONE:-1}" = 1 ] && echo "2026-11-14 05:21:46Z: Job harness completed with result: Succeeded"
exit "${FAKE_EXIT:-2}"
F
chmod +x "$t/runner/bin/fake-job"
# The broker's binary as the start calls it: `take <socket> <dir>`, here from a
# folder the test fills, the socket going once taken as the real one does.
cat > "$t/take" <<'F'
#!/bin/bash
[ "$1" = take ] && [ -S "$2" ] && [ -e "$OFFER/.runner" ] || exit 1
for f in .runner .credentials .credentials_rsaparams; do mv -f "$OFFER/$f" "$3/$f" || exit 1; done
rm -f "$2"
F
chmod +x "$t/take"
printf '2.338.2\n' > "$t/runner/VERSION"
printf 'gha-runner-2026.1106.2\n' > "$t/RELEASE"
export RUNNER_DIR="$t/runner" RUNNER_STATE_DIR="$t/state" RUNNER_SLOT_DIR="$t/slot" RUNNER_LISTENER=bin/fake RUNNER_STOP_WAIT=5
export RUNNER_TAKE="$t/take" OFFER="$t/offer"
export RUNNER_RELEASE_FILE="$t/RELEASE"

# The broker's end of the loopback port: what arrives, a line each, in a file.
python3 -u -c '
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 0))
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
out = open(sys.argv[2], "a", buffering=1)
while True:
    out.write(s.recv(1024).decode())
' "$t/port" "$t/heard" &
ear=$!
for _ in $(seq 1 50); do [ -s "$t/port" ] && break; sleep 0.1; done
export RUNNER_REPORT_PORT="$(cat "$t/port")"
: > "$t/heard"

jit() {
  printf '{"marker":"%s"}' "$1" > "$t/offer/.runner"
  printf c > "$t/offer/.credentials"
  printf r > "$t/offer/.credentials_rsaparams"
  # A socket file where the broker's would be; the fake take reads the folder.
  python3 -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$t/slot/socket"
}
check() { grep -q -- "$1" "$2" || { echo "FAIL: no '$1' in $2:"; cat "$2"; exit 1; }; }

# It waits for a config, doesn't start without one.
bash "$here/image/runner-start.sh" > "$t/out1" 2>&1 &
pid=$!
sleep 2
kill -0 "$pid" || { echo "FAIL: exited without a config"; cat "$t/out1"; exit 1; }
[ ! -s "$t/out1" ] || { echo "FAIL: started without a config"; cat "$t/out1"; exit 1; }
[ ! -s "$t/heard" ] || { echo "FAIL: reported before it took its config"; cat "$t/heard"; exit 1; }
jit one
sleep 3
check 'args: run' "$t/out1"
check '"marker":"one"' "$t/out1"
[ ! -e "$t/slot/socket" ] || { echo "FAIL: the config's socket stayed"; exit 1; }
# The first instance keeps today's cache paths.
check 'rustup: /cache/rustup cargo: /cache/cargo tools: /cache/hostedtoolcache' "$t/out1"
# Once it has its config: the runner's version and the release, on the loopback port.
check '^Runner report: version=2.338.2 release=gha-runner-2026.1106.2$' "$t/heard"
[ "$(wc -l < "$t/heard" | tr -d ' ')" = 1 ] || { echo "FAIL: reported more than once"; cat "$t/heard"; exit 1; }
# Never on the command line.
if ps -ax -o command | grep -F "$t" | grep -q jitconfig; then echo "FAIL: on a command line"; exit 1; fi
[ "$(stat -c %a "$t/state/.credentials" 2>/dev/null || stat -f %Lp "$t/state/.credentials")" = 600 ] || { echo "FAIL: credentials' mode"; exit 1; }

# SIGTERM reaches the listener as SIGINT, and its status is the script's.
kill -TERM "$pid"
rc=0; wait "$pid" || rc=$?
sleep 0.3
check 'got-INT' "$t/out1"
[ "$rc" = 7 ] || { echo "FAIL: exit $rc, not the listener's 7"; exit 1; }

# The broker's stop marker stops it too.
rm -rf "$t/state"
jit two
bash "$here/image/runner-start.sh" > "$t/out2" 2>&1 &
pid=$!
sleep 2
check '"marker":"two"' "$t/out2"
touch "$t/slot/stop"
rc=0; wait "$pid" || rc=$?
check 'got-INT' "$t/out2"
rm -f "$t/slot/stop"

# With no VERSION and no RELEASE, and nobody listening, the job still starts.
rm -rf "$t/state" "$t/runner/VERSION" "$t/RELEASE"
jit three
kill "$ear" 2>/dev/null || true
wait "$ear" 2>/dev/null || true
bash "$here/image/runner-start.sh" > "$t/out3" 2>&1 &
pid=$!
sleep 2
check '"marker":"three"' "$t/out3"
touch "$t/slot/stop"
rc=0; wait "$pid" || rc=$?
check 'got-INT' "$t/out3"
rm -f "$t/slot/stop"

# An instance the box is ending, still waiting for its config, leaves quietly.
rm -rf "$t/state"
touch "$t/slot/.leaving"
bash "$here/image/runner-start.sh" > "$t/out4" 2>&1 &
pid=$!
rc=0; wait "$pid" || rc=$?
[ "$rc" = 0 ] || { echo "FAIL: a leaving instance exited $rc"; cat "$t/out4"; exit 1; }
if grep -q 'args: run' "$t/out4"; then echo "FAIL: a leaving instance started its listener"; exit 1; fi
rm -f "$t/slot/.leaving"

# A second instance: its lines carry its number, and the caches with no lock of
# their own are its own (§17.5); cargo's is shared.
python3 -u -c '
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 0))
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
out = open(sys.argv[2], "a", buffering=1)
while True:
    out.write(s.recv(1024).decode())
' "$t/port2" "$t/heard2" &
ear=$!
for _ in $(seq 1 50); do [ -s "$t/port2" ] && break; sleep 0.1; done
printf '2.338.2\n' > "$t/runner/VERSION"
rm -rf "$t/state"
jit four
RUNNER_REPORT_PORT="$(cat "$t/port2")" LEHR_INSTANCE=2 bash "$here/image/runner-start.sh" > "$t/out5" 2>&1 &
pid=$!
sleep 2
check 'rustup: /cache/slot2/rustup cargo: /cache/cargo tools: /cache/slot2/hostedtoolcache' "$t/out5"
check '^@2 Runner report: version=2.338.2' "$t/heard2"
touch "$t/slot/stop"
wait "$pid" || true
rm -f "$t/slot/stop"
kill "$ear" 2>/dev/null || true

# A finished job's exit 2 is the runner's work ending: 0 to the box (§17.9). With
# no finished job, or any other status, the listener's status stands.
ends() {
  rm -rf "$t/state"; jit "$1"
  rc=0
  RUNNER_LISTENER=bin/fake-job bash "$here/image/runner-start.sh" > "$t/out-$1" 2>&1 || rc=$?
  echo "$rc"
}
[ "$(ends job-done)" = 0 ] || { echo "FAIL: a finished job's exit 2 isn't 0"; cat "$t/out-job-done"; exit 1; }
[ "$(FAKE_SAY_DONE=0 ends no-job)" = 2 ] || { echo "FAIL: exit 2 with no finished job became clean"; exit 1; }
[ "$(FAKE_EXIT=1 ends other)" = 1 ] || { echo "FAIL: another exit status was changed"; exit 1; }

# The stub's sentence, and the role.
out=$("$here/image/docker-stub" network create x 2>&1 || true)
[ "$out" = "This runner has no container support, so \`container:\` and \`services:\` can't run here." ] || { echo "FAIL: stub said: $out"; exit 1; }
if "$here/image/docker-stub" ps 2>/dev/null; then echo "FAIL: the stub succeeded"; exit 1; fi
grep -q 'GITHUB_TOKEN+set' "$here/image/start.sh"

echo "OK | runner start | hand-off, version report, signal, stop marker, leaving, second instance, a finished job's exit, stub"
