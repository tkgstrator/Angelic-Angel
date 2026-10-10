#!/usr/bin/env bash
# Credential-free smoke test for the runtime image.
# Usage: scripts/docker-smoke.sh IMAGE
# Uses dummy credentials, a fabricated registration and --network none only;
# it never contacts Twitter or AutoPush. The runtime image is distroless (no
# shell), so file checks and seeding use a separate busybox helper image.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 IMAGE" >&2
  exit 2
fi

IMAGE=$1
HELPER_IMAGE=busybox:1.37.0
SUFFIX="$$-$(date +%s)"
PREFIX="angelic-smoke-$SUFFIX"
INIT_VOLUME="$PREFIX-init"
LISTEN_VOLUME="$PREFIX-listen"
LISTEN_CONTAINER="$PREFIX-listen"
CERT_CONTAINER="$PREFIX-cert"
ONESHOT_CONTAINER="$PREFIX-run"
CONFIG=/data/angelic-angel.toml
DUMMY_AUTH=dummy-auth-token
DUMMY_CT0=dummy-ct0-token

# Every resource name is recorded before it is created, so the trap only ever
# removes resources owned by this run.
OWNED_CONTAINERS=$LISTEN_CONTAINER$'\n'$CERT_CONTAINER$'\n'$ONESHOT_CONTAINER
OWNED_VOLUMES=$INIT_VOLUME$'\n'$LISTEN_VOLUME
CERT_DIR=

cleanup() {
  local status=$? name failed=0 resources
  trap - EXIT
  if [ "$status" -ne 0 ] && docker container inspect "$LISTEN_CONTAINER" >/dev/null 2>&1; then
    echo "--- logs of $LISTEN_CONTAINER ---" >&2
    docker logs "$LISTEN_CONTAINER" >&2 || true
  fi
  if resources=$(docker container ls -a --format '{{.Names}}'); then
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      if grep -Fxq "$name" <<<"$resources"; then
        docker rm -f "$name" >/dev/null || { echo "cleanup: failed to remove container $name" >&2; failed=1; }
      fi
    done <<<"$OWNED_CONTAINERS"
  else
    echo "cleanup: cannot inspect owned containers" >&2
    failed=1
  fi
  if resources=$(docker volume ls --format '{{.Name}}'); then
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      if grep -Fxq "$name" <<<"$resources"; then
        docker volume rm "$name" >/dev/null || { echo "cleanup: failed to remove volume $name" >&2; failed=1; }
      fi
    done <<<"$OWNED_VOLUMES"
  else
    echo "cleanup: cannot inspect owned volumes" >&2
    failed=1
  fi
  if [ -n "$CERT_DIR" ]; then
    rm -rf "$CERT_DIR" || { echo "cleanup: failed to remove $CERT_DIR" >&2; failed=1; }
  fi
  if [ "$status" -eq 0 ] && [ "$failed" -ne 0 ]; then
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

expect_contains() { # label text needle
  case $2 in
    *"$3"*) ;;
    *) fail "$1: expected output to contain '$3', got: $2" ;;
  esac
}

# Named one-shot container; the name is recorded so cleanup can reap it.
# Usage: oneshot [docker run options...] IMAGE [args...]
oneshot() {
  # Pre-register one reusable name so command substitutions cannot lose ownership.
  docker run --rm --name "$ONESHOT_CONTAINER" --network none "$@"
}

# Run the application image offline. Usage: app [docker options] IMAGE [args...]
app() {
  oneshot "$@"
}

# Helper runs as the application's UID/GID so it sees /data as the runtime does.
helper() {
  oneshot --user 65532:65532 "$@"
}

echo "== helper image"
if ! docker image inspect "$HELPER_IMAGE" >/dev/null 2>&1; then
  docker pull "$HELPER_IMAGE" >/dev/null || fail "cannot pull $HELPER_IMAGE"
fi

echo "== image metadata"
[ "$(docker image inspect -f '{{.Config.User}}' "$IMAGE")" = "65532:65532" ] \
  || fail "image user is not 65532:65532"
[ "$(docker image inspect -f '{{.Config.StopSignal}}' "$IMAGE")" = "SIGINT" ] \
  || fail "stop signal is not SIGINT"

CERT_DIR=$(mktemp -d)
docker create --name "$CERT_CONTAINER" "$IMAGE" >/dev/null || fail "docker create failed"
docker cp "$CERT_CONTAINER:/etc/ssl/certs/ca-certificates.crt" "$CERT_DIR/ca-certificates.crt" \
  || fail "CA bundle missing from image"
[ -s "$CERT_DIR/ca-certificates.crt" ] || fail "CA bundle is empty"
docker rm "$CERT_CONTAINER" >/dev/null || fail "cannot remove $CERT_CONTAINER"

echo "== help (loads the binary and its native libraries)"
out=$(app "$IMAGE" --help 2>&1) || fail "--help failed (unresolved libraries?): $out"
expect_contains help "$out" "A server for receiving tweet notifications"
expect_contains help "$out" "listen"

echo "== fresh volumes: init"
# The application must be the first to mount each fresh volume so Docker copies
# the image's /data (UID 65532, mode 700) into it.
for vol in "$INIT_VOLUME" "$LISTEN_VOLUME"; do
  docker volume create "$vol" >/dev/null
  app -v "$vol:/data" "$IMAGE" init --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" >/dev/null 2>&1 \
    || fail "init failed on $vol"
done

echo "== status and persistence"
out=$(app -v "$INIT_VOLUME:/data" "$IMAGE" status 2>&1)
expect_contains status "$out" "$DUMMY_AUTH"
expect_contains status "$out" "$DUMMY_CT0"
expect_contains status "$out" "not registered"

echo "== permissions"
out=$(helper -v "$INIT_VOLUME:/data" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%a' /data $CONFIG \
    && test -r $CONFIG && test -w $CONFIG \
    && touch /data/.smoke-write && rm /data/.smoke-write") \
  || fail "helper cannot access /data as UID 65532: $out"
expect_contains permissions "$out" "/data 65532:700"
expect_contains permissions "$out" "$CONFIG 65532:"

echo "== listen without registration"
set +e
out=$(app -v "$INIT_VOLUME:/data" "$IMAGE" listen 2>&1)
code=$?
set -e
[ "$code" -eq 1 ] || fail "listen without registration exited $code, expected 1: $out"
expect_contains listen-missing "$out" "No registration found"

echo "== listen offline with fabricated registration"
helper -i -v "$LISTEN_VOLUME:/data" "$HELPER_IMAGE" sh -c "cat > $CONFIG" <<TOML \
  || fail "cannot seed fabricated registration"
[twitter]
auth_token = "$DUMMY_AUTH"
ct0 = "$DUMMY_CT0"

[registration]
endpoint = "https://invalid.example/push"

[registration.autopush]
uaid = "00000000-0000-0000-0000-000000000000"
channel_id = "00000000-0000-0000-0000-000000000001"

[registration.keys]
public_key = [1, 2, 3]
private_key = [1, 2, 3]
auth_secret = [1, 2, 3]
TOML
out=$(app -v "$LISTEN_VOLUME:/data" "$IMAGE" status 2>&1)
expect_contains fabricated-status "$out" "active"

docker run -d --name "$LISTEN_CONTAINER" --network none -v "$LISTEN_VOLUME:/data" "$IMAGE" >/dev/null \
  || fail "cannot start listener"
seen=0
i=0
while [ "$i" -lt 30 ]; do
  if docker logs "$LISTEN_CONTAINER" 2>&1 | grep -q "connection failed, retrying"; then
    seen=1
    break
  fi
  [ "$(docker inspect -f '{{.State.Running}}' "$LISTEN_CONTAINER")" = "true" ] \
    || fail "listener exited early: $(docker logs "$LISTEN_CONTAINER" 2>&1)"
  sleep 1
  i=$((i + 1))
done
[ "$seen" -eq 1 ] || fail "no connection failure/retry log: $(docker logs "$LISTEN_CONTAINER" 2>&1)"

# Plain `docker stop` uses the image's STOPSIGNAL (SIGINT).
docker stop -t 5 "$LISTEN_CONTAINER" >/dev/null || fail "docker stop failed"
[ "$(docker inspect -f '{{.State.ExitCode}}' "$LISTEN_CONTAINER")" = "0" ] \
  || fail "listener exit code after stop is not 0"
expect_contains sigint "$(docker logs "$LISTEN_CONTAINER" 2>&1)" "Interrupted"

echo "OK"
