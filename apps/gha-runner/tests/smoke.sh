#!/bin/bash
# The start script's hand-off and restart, with no GitHub, no image and no root:
# a fake listener in a temporary directory. Run by CI.
#   apps/gha-runner/tests/smoke.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'kill "${pid:-0}" "${ear:-0}" 2>/dev/null || true; rm -rf "$t"' EXIT
# As tini starts it: a background command in a script would start with SIGINT
# ignored and pass that on, which isn't what the service sees.
set -m
mkdir -p "$t/runner/bin" "$t/shm"
cat > "$t/runner/bin/fake" <<'F'
#!/bin/bash
# Stays until told, like a listener; a signal ends it with a word.
echo "args: $*"
ls -A "$RUNNER_STATE_DIR" | tr '\n' ' '; echo
cat "$RUNNER_STATE_DIR/.runner"; echo
trap 'echo got-INT; exit 7' INT
trap 'echo got-TERM; exit 8' TERM
for _ in $(seq 1 300); do sleep 0.1; [ -e "$RUNNER_STATE_DIR/quit" ] && exit 0; done
F
chmod +x "$t/runner/bin/fake"
printf '2.338.2\n' > "$t/runner/VERSION"
printf 'gha-runner-2026.1106.2\n' > "$t/RELEASE"
export RUNNER_DIR="$t/runner" RUNNER_STATE_DIR="$t/state" RUNNER_SHM_DIR="$t/shm" RUNNER_LISTENER=bin/fake RUNNER_STOP_WAIT=5
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
  mkdir "$t/shm/new"
  printf '{"marker":"%s"}' "$1" > "$t/shm/new/.runner"
  printf c > "$t/shm/new/.credentials"
  printf r > "$t/shm/new/.credentials_rsaparams"
  mv "$t/shm/new" "$t/shm/gha-runner-jit"
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
[ ! -e "$t/shm/gha-runner-jit" ] || { echo "FAIL: the config stayed in shm"; exit 1; }
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
touch "$t/shm/gha-runner-stop"
rc=0; wait "$pid" || rc=$?
check 'got-INT' "$t/out2"
rm -f "$t/shm/gha-runner-stop"

# With no VERSION and no RELEASE, and nobody listening, the job still starts.
rm -rf "$t/state" "$t/runner/VERSION" "$t/RELEASE"
jit three
kill "$ear" 2>/dev/null || true
wait "$ear" 2>/dev/null || true
bash "$here/image/runner-start.sh" > "$t/out3" 2>&1 &
pid=$!
sleep 2
check '"marker":"three"' "$t/out3"
touch "$t/shm/gha-runner-stop"
rc=0; wait "$pid" || rc=$?
check 'got-INT' "$t/out3"
rm -f "$t/shm/gha-runner-stop"

# The stub's sentence, and the role.
out=$("$here/image/docker-stub" network create x 2>&1 || true)
[ "$out" = "This runner has no container support, so \`container:\` and \`services:\` can't run here." ] || { echo "FAIL: stub said: $out"; exit 1; }
if "$here/image/docker-stub" ps 2>/dev/null; then echo "FAIL: the stub succeeded"; exit 1; fi
grep -q 'GITHUB_TOKEN+set' "$here/image/start.sh"

echo "OK | runner start | hand-off, version report, signal, stop marker, stub"
