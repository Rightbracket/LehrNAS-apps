#!/bin/bash
# The start script's hand-off and restart, with no GitHub, no image and no root:
# a fake listener in a temporary directory. Run by CI.
#   apps/gha-runner/tests/smoke.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'kill "${pid:-0}" 2>/dev/null || true; rm -rf "$t"' EXIT
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
export RUNNER_DIR="$t/runner" RUNNER_STATE_DIR="$t/state" RUNNER_SHM_DIR="$t/shm" RUNNER_LISTENER=bin/fake RUNNER_STOP_WAIT=5

jit() {
  mkdir "$t/shm/new"
  printf '{"marker":"%s"}' "$1" > "$t/shm/new/.runner"
  printf c > "$t/shm/new/.credentials"
  printf r > "$t/shm/new/.credentials_rsaparams"
  mv "$t/shm/new" "$t/shm/lehr-runner-jit"
}
check() { grep -q -- "$1" "$2" || { echo "FAIL: no '$1' in $2:"; cat "$2"; exit 1; }; }

# It waits for a config, doesn't start without one.
bash "$here/image/runner-start.sh" > "$t/out1" 2>&1 &
pid=$!
sleep 2
kill -0 "$pid" || { echo "FAIL: exited without a config"; cat "$t/out1"; exit 1; }
[ ! -s "$t/out1" ] || { echo "FAIL: started without a config"; cat "$t/out1"; exit 1; }
jit one
sleep 3
check 'args: run' "$t/out1"
check '"marker":"one"' "$t/out1"
[ ! -e "$t/shm/lehr-runner-jit" ] || { echo "FAIL: the config stayed in shm"; exit 1; }
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
touch "$t/shm/lehr-runner-stop"
rc=0; wait "$pid" || rc=$?
check 'got-INT' "$t/out2"
rm -f "$t/shm/lehr-runner-stop"

# The stub's sentence, and the role.
out=$("$here/image/docker-stub" network create x 2>&1 || true)
[ "$out" = "This runner has no container support, so \`container:\` and \`services:\` can't run here." ] || { echo "FAIL: stub said: $out"; exit 1; }
if "$here/image/docker-stub" ps 2>/dev/null; then echo "FAIL: the stub succeeded"; exit 1; fi
grep -q 'GITHUB_TOKEN+set' "$here/image/start.sh"

echo "OK | runner start | hand-off, signal, stop marker, stub"
