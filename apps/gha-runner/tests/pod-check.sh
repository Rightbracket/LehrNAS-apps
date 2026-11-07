#!/bin/bash
# The two services together, as the box runs them: read-only, non-root, one IPC
# and one network, the real broker and the real start script. GitHub and the
# runner are fakes, so it needs no account, only docker and the plain Ubuntu base
# (the image's own packages aren't installed). It proves the cycle: config handed
# over with updates off, no token in the runner, a job's leftovers gone, the next
# config only after the last runner is gone. Then the runner's report and the two
# kinds of own image (§13.2): one built FROM a stand-in for the app's image takes a
# job, and one that isn't says so on the page after the wait, about six minutes.
#   apps/gha-runner/tests/pod-check.sh [path to a linux gha-runner-broker]
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
base=docker.io/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
case $(uname -m) in arm64|aarch64) triple=aarch64-unknown-linux-musl ;; *) triple=x86_64-unknown-linux-musl ;; esac
bin=${1:-$here/src/broker/target/$triple/release/gha-runner-broker}
n=gha-pod-check-$$
t=$(mktemp -d)
cleanup() {
  docker rm -f "$n-broker" "$n-runner" >/dev/null 2>&1 || true
  docker volume rm "$n-cache" "$n-config" >/dev/null 2>&1 || true
  docker rmi -f "$n-app" "$n-own" >/dev/null 2>&1 || true
  rm -rf "$t"
}
trap cleanup EXIT

# A fake curl: GitHub, kept as files in `cache`. A real run replaces all of this.
cat > "$t/curl" <<'F'
#!/bin/bash
cat > /dev/null
for a in "$@"; do url=$a; done
m=GET; prev=; for a in "$@"; do [ "$prev" = --request ] && m=$a; prev=$a; done
case "$m $url" in
  "GET "*/actions/runners/*)
    id=${url##*/}
    if [ -e "/cache/gone-$id" ]; then printf '{}\n404'
    elif [ -e /cache/github-sees-it-offline ]; then printf '{"status":"offline","busy":false}\n200'
    else printf '{"status":"online","busy":false}\n200'; fi ;;
  "DELETE "*/actions/runners/*)
    touch "/cache/gone-${url##*/}"; printf '\n204' ;;
  "POST "*/generate-jitconfig)
    n=$(ls /cache | grep -c '^issued-' || true); n=$((n + 1))
    touch "/cache/issued-$n"
    r=$(printf '{"agentId":%s,"ephemeral":true}' "$n" | base64 -w0)
    c=$(printf '{}' | base64 -w0)
    j=$(printf '{".runner":"%s",".credentials":"%s",".credentials_rsaparams":"%s"}' "$r" "$c" "$c" | base64 -w0)
    printf '{"runner":{"id":%s},"encoded_jit_config":"%s"}\n201' "$n" "$j" ;;
  "GET "*/repos/actions/runner/releases*) printf '[]\n200' ;;
  "GET "*/actions/runs*) printf '{"total_count":0}\n200' ;;
  "GET "*) printf '{"visibility":"private"}\n200' ;;
  *) printf '{}\n500' ;;
esac
F
# A fake listener: one job, with leftovers in the places a real one could use.
mkdir -p "$t/runner/bin"
cat > "$t/runner/bin/fake-listener" <<'F'
#!/bin/bash
id=$(sed -E 's/.*"agentId":([0-9]+).*/\1/' "$RUNNER_STATE_DIR/.runner")
{
  echo "job $id"
  grep -q '"disableUpdate":true' "$RUNNER_STATE_DIR/.runner" && echo "updates off"
  [ -z "${GITHUB_TOKEN+set}" ] && echo "no token key"
  [ ! -e /proc/1/environ ] || grep -qz GITHUB_TOKEN /proc/1/environ || echo "no token in pid 1"
  [ -e /tmp/left-by-the-last-job ] && echo "LEFTOVER in tmp"
  [ -e /usr/local/bin/a-tool-the-admin-added ] && echo "own image's tool"
  ls -A /dev/shm | grep -v '^$' | sed 's/^/shm has: /'
} >> "/cache/report-$id"
touch /tmp/left-by-the-last-job /dev/shm/left-by-job
touch /dev/mqueue/left 2>/dev/null || true
ipcmk -M 4096 > /dev/null 2>&1 || true
ipcs -m | grep -c '^0x' | sed 's/^/sysv: /' >> "/cache/report-$id"
echo "2026-11-05 11:14:31Z: Running job: demo $id"
sleep 2
echo "2026-11-05 11:14:33Z: Job demo $id completed with result: Succeeded"
touch "/cache/gone-$id"
exit 0
F
chmod +x "$t/curl" "$t/runner/bin/fake-listener"
printf '2.338.2\n' > "$t/runner/VERSION"
printf 'gha-runner-2026.1201.1\n' > "$t/release-app"
printf 'gha-runner-2026.1106.1\n' > "$t/release-older"
cp "$bin" "$t/broker"; chmod +x "$t/broker"

docker volume create "$n-cache" >/dev/null
docker volume create "$n-config" >/dev/null
docker run --rm -v "$n-cache:/c" -v "$n-config:/d" "$base" chown 1001:1001 /c /d

# --tmpfs: what podman's ReadOnlyTmpfs gives each service on the box.
common=(--read-only --tmpfs /tmp:exec,mode=1777 --tmpfs /run --user 1001:1001 --security-opt no-new-privileges --cap-drop ALL)
docker run -d --name "$n-broker" "${common[@]}" --ipc shareable -p 127.0.0.1::8711 \
  -e GITHUB_TOKEN=s3cret-token -e RUNNER_SCOPE=me/thing -e PATH=/fake:/usr/bin:/bin \
  -v "$n-config:/config" -v "$n-cache:/cache" -v "$t/curl:/fake/curl:ro" \
  -v "$t/broker:/usr/local/bin/gha-runner-broker:ro" -v "$here/image/start.sh:/s/start.sh:ro" \
  -v "$t/release-app:/usr/local/lib/gha-runner/RELEASE:ro" \
  --entrypoint /bin/bash "$base" /s/start.sh >/dev/null

# The service's restart: a loop of fresh containers, as `restart: always` does.
start_runner() {
  docker rm -f "$n-runner" >/dev/null 2>&1 || true
  docker run -d --name "$n-runner" "${common[@]}" --ipc "container:$n-broker" --network "container:$n-broker" \
    -e RUNNER_DIR=/fake-runner -e RUNNER_LISTENER=bin/fake-listener -e RUNNER_STATE_DIR=/tmp/runner \
    -v "$n-cache:/cache" -v "$t/runner:/fake-runner:ro" -v "$here/image/runner-start.sh:/s/runner-start.sh:ro" \
    -v "$here/image/start.sh:/usr/local/lib/gha-runner/start.sh:ro" \
    -v "$t/release-older:/usr/local/lib/gha-runner/RELEASE:ro" \
    --entrypoint /bin/bash "$base" -c 'sed "s#/usr/local/lib/gha-runner/runner-start.sh#/s/runner-start.sh#" /usr/local/lib/gha-runner/start.sh > /tmp/s.sh; exec bash /tmp/s.sh' >/dev/null
}
start_runner

port=$(docker port "$n-broker" 8711/tcp | head -1 | sed 's/.*://')
page() { /usr/bin/curl -s -m 5 "http://127.0.0.1:$port/"; }
for _ in $(seq 1 30); do page >/dev/null 2>&1 && break; sleep 0.5; done
page | grep -q 'Set a password' || { docker logs "$n-broker"; echo "FAIL: no first-visit page"; exit 1; }
/usr/bin/curl -s -m 5 -c "$t/jar" -o /dev/null -d 'password=check' "http://127.0.0.1:$port/password"

in_cache() { docker exec "$n-broker" sh -c "$1"; }
wait_for() {
  for _ in $(seq 1 90); do in_cache "[ -e /cache/$1 ]" && return 0; sleep 1; done
  docker logs "$n-broker" 2>&1 | tail -20; docker logs "$n-runner" 2>&1 | tail -20
  echo "FAIL: no /cache/$1"; exit 1
}
wait_for gone-1
# One config only while the first runner lasted.
[ "$(in_cache 'ls /cache | grep -c ^issued-')" = 1 ] || { echo "FAIL: a second config before the first was gone"; exit 1; }
for _ in $(seq 1 20); do [ "$(docker inspect -f '{{.State.Running}}' "$n-runner")" = false ] && break; sleep 0.5; done
[ "$(docker inspect -f '{{.State.Running}}' "$n-runner")" = false ] || { echo "FAIL: the container outlived its job"; exit 1; }
start_runner
wait_for gone-2
r1=$(in_cache 'cat /cache/report-1'); r2=$(in_cache 'cat /cache/report-2')
echo "--- job 1"; echo "$r1"; echo "--- job 2"; echo "$r2"
for want in "updates off" "no token key" "no token in pid 1"; do
  grep -q "$want" <<<"$r2" || { echo "FAIL: job 2: no '$want'"; exit 1; }
done
if grep -q LEFTOVER <<<"$r2"; then echo "FAIL: /tmp survived"; exit 1; fi
if grep -q 'shm has' <<<"$r2"; then echo "FAIL: /dev/shm survived"; exit 1; fi
grep -q 'sysv: 1$' <<<"$r2" || { echo "FAIL: SysV memory survived (want only job 2's own)"; exit 1; }
if docker logs "$n-broker" 2>&1 | grep -q s3cret-token; then echo "FAIL: the token in the log"; exit 1; fi
home=$(/usr/bin/curl -s -m 5 -b "$t/jar" "http://127.0.0.1:$port/")
grep -q 'demo' <<<"$home" || { echo "$home"; echo "FAIL: the page has no job from the runner's output"; exit 1; }
# The runner's report reached the page: its version, and its image's older release.
page_in() { /usr/bin/curl -s -m 5 -b "$t/jar" "http://127.0.0.1:$port/"; }
want_page() { grep -qF -- "$1" <<<"$(page_in)" || { page_in; echo "FAIL: the page doesn't say: $1"; exit 1; }; }
want_page 'Runner 2.338.2.'
want_page 'built from gha-runner-2026.1106.1, and this app is on gha-runner-2026.1201.1.'
if grep -q 'REFUSED' <<<"$(page_in)"; then echo "FAIL: a log line on the page"; exit 1; fi

# An image built FROM the app's, here a stand-in with the real start scripts and
# the runner's files where the real image has them.
gone() { for _ in $(seq 1 20); do [ "$(docker inspect -f '{{.State.Running}}' "$n-runner")" = false ] && return 0; sleep 0.5; done; echo "FAIL: the container outlived its job"; exit 1; }
gone
c=$t/ctx; mkdir -p "$c"
cp "$here/image/start.sh" "$here/image/runner-start.sh" "$c/"
cp "$t/release-app" "$c/RELEASE"
cp -R "$t/runner" "$c/runner"
cat > "$c/Dockerfile" <<F
FROM $base
COPY start.sh runner-start.sh RELEASE /usr/local/lib/gha-runner/
COPY runner /opt/runner
RUN chmod 0755 /usr/local/lib/gha-runner/start.sh /usr/local/lib/gha-runner/runner-start.sh
USER 1001:1001
ENTRYPOINT ["/usr/local/lib/gha-runner/start.sh"]
F
docker build -q -t "$n-app" "$c" >/dev/null
cat > "$c/Dockerfile" <<F
FROM $n-app
USER 0
RUN touch /usr/local/bin/a-tool-the-admin-added
USER 1001:1001
F
docker build -q -t "$n-own" "$c" >/dev/null
# As the box runs a service: its own entrypoint, nothing mounted over its scripts.
run_image() {
  docker rm -f "$n-runner" >/dev/null 2>&1 || true
  docker run -d --name "$n-runner" "${common[@]}" --ipc "container:$n-broker" --network "container:$n-broker" \
    -e RUNNER_LISTENER=bin/fake-listener -e RUNNER_STATE_DIR=/tmp/runner -v "$n-cache:/cache" "$@" >/dev/null
}
run_image "$n-own"
wait_for gone-3
grep -q "own image's tool" <<<"$(in_cache 'cat /cache/report-3')" || { echo "FAIL: the job didn't run in the own image"; exit 1; }
for _ in $(seq 1 30); do grep -q 'Rebuild it' <<<"$(page_in)" || break; sleep 1; done
if grep -q 'Rebuild it' <<<"$(page_in)"; then page_in; echo "FAIL: the same release still reads as different"; exit 1; fi

# An image that isn't: it starts, but none of this app's start is in it, so nothing
# takes the config, nothing reports, and GitHub never shows the runner online.
gone
in_cache 'touch /cache/github-sees-it-offline'
wait_for issued-4
run_image --entrypoint /bin/sleep "$base" 3600
said=
for _ in $(seq 1 400); do
  if grep -qF 'No word from the runner service in 5 minutes' <<<"$(page_in)"; then said=yes; break; fi
  sleep 1
done
[ -n "$said" ] || { page_in; docker logs "$n-broker" 2>&1 | tail -20; echo "FAIL: the page never said it heard nothing"; exit 1; }
echo "OK | pod check | jobs: hand-off, updates off, no token in the runner, IPC clean, report, own image, silent image"
