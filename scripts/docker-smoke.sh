#!/usr/bin/env bash
# Credential-free smoke test for the runtime image.
# Usage: scripts/docker-smoke.sh IMAGE
# Uses dummy credentials, fabricated registrations and --network none only;
# it never contacts Twitter or AutoPush. The runtime image is distroless (no
# shell), so file checks and seeding use a separate busybox helper image.
# The image starts as root and the application drops to the owner of /config
# (65532 when root owns it); the bind-mount cases below check exactly that.
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
AUTOREG_VOLUME="$PREFIX-autoreg"
LISTEN_CONTAINER="$PREFIX-listen"
AUTOREG_CONTAINER="$PREFIX-autoreg"
CERT_CONTAINER="$PREFIX-cert"
ONESHOT_CONTAINER="$PREFIX-run"
CONFIG=/config/angelic-angel.toml
OTHER_CONFIG=/config/other.toml
DUMMY_AUTH=dummy-auth-token
DUMMY_CT0=dummy-ct0-token
ENV_AUTH=dummy-env-auth-token
ENV_CT0=dummy-env-ct0-token
# `status` prints only the first 20 characters of a stored credential; the TAILSECRET
# markers sit past that, so seeing one in any output means a value was printed in full.
WORK_AUTH=work-auth-token-visible-TAILSECRET1
WORK_CT0=work-ct0-token-visible-TAILSECRET2
WORK_AUTH_SHOWN="${WORK_AUTH:0:20}..."
WORK_CT0_SHOWN="${WORK_CT0:0:20}..."
WEBHOOK_SECRET=dummy-webhook-secret-WHSECRET
MULTI_CONFIG=/config/multi.toml

# Every resource name is recorded before it is created, so the trap only ever
# removes resources owned by this run.
OWNED_CONTAINERS=$LISTEN_CONTAINER$'\n'$AUTOREG_CONTAINER$'\n'$CERT_CONTAINER$'\n'$ONESHOT_CONTAINER
OWNED_VOLUMES=$INIT_VOLUME$'\n'$LISTEN_VOLUME$'\n'$AUTOREG_VOLUME
CERT_DIR=
BIND_DIR=

cleanup() {
  local status=$? name failed=0 resources
  trap - EXIT
  for name in "$LISTEN_CONTAINER" "$AUTOREG_CONTAINER"; do
    if [ "$status" -ne 0 ] && docker container inspect "$name" >/dev/null 2>&1; then
      echo "--- logs of $name ---" >&2
      docker logs "$name" >&2 || true
    fi
  done
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
  if [ -n "$BIND_DIR" ]; then
    # The bind-mount cases leave root- and foreign-owned files behind; only root can remove them.
    docker run --rm --name "$ONESHOT_CONTAINER" --network none -v "$BIND_DIR:/work" "$HELPER_IMAGE" \
      sh -c 'rm -rf /work/* /work/.[!.]*' >/dev/null \
      || { echo "cleanup: failed to empty $BIND_DIR" >&2; failed=1; }
    rmdir "$BIND_DIR" || { echo "cleanup: failed to remove $BIND_DIR" >&2; failed=1; }
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

expect_lacks() { # label text needle
  case $2 in
    *"$3"*) fail "$1: output must not contain '$3', got: $2" ;;
  esac
}

# Runs a command that must fail with the given exit code; its output is left in $out.
expect_failure() { # label code command...
  local label=$1 want=$2 code
  shift 2
  set +e
  out=$("$@" 2>&1)
  code=$?
  set -e
  [ "$code" -eq "$want" ] || fail "$label: exited $code, expected $want: $out"
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

# Helper runs as the application's UID/GID so it sees /config as the runtime does.
helper() {
  oneshot --user 65532:65532 "$@"
}

# Helper that stays root, to seed and inspect ownership that the application must not rely on.
root_helper() {
  oneshot "$@"
}

echo "== helper image"
if ! docker image inspect "$HELPER_IMAGE" >/dev/null 2>&1; then
  docker pull "$HELPER_IMAGE" >/dev/null || fail "cannot pull $HELPER_IMAGE"
fi

echo "== image metadata"
# The image starts as root and the application drops privileges itself. The :nonroot
# base sets USER 65532, so this also catches a Dockerfile that merely deleted its USER line.
image_user=$(docker image inspect -f '{{.Config.User}}' "$IMAGE")
case $image_user in
  "" | 0 | 0:0 | root | root:root) ;;
  *) fail "image user is '$image_user', expected root (the application drops privileges)" ;;
esac
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
# the image's /config (UID 65532, mode 700) into it. It starts as root and keeps
# running as 65532 because that is who owns the volume.
for vol in "$INIT_VOLUME" "$LISTEN_VOLUME"; do
  docker volume create "$vol" >/dev/null
  app -v "$vol:/config" "$IMAGE" init --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" >/dev/null 2>&1 \
    || fail "init failed on $vol"
done
# This one stays without a config file: it only ever sees credentials from the environment.
docker volume create "$AUTOREG_VOLUME" >/dev/null
app -v "$AUTOREG_VOLUME:/config" "$IMAGE" status >/dev/null 2>&1 || fail "status failed on $AUTOREG_VOLUME"

echo "== status and persistence"
out=$(app -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1)
expect_contains status "$out" "$DUMMY_AUTH"
expect_contains status "$out" "$DUMMY_CT0"
expect_contains status "$out" "not registered"

echo "== status with credentials from the environment"
out=$(app -e "TWITTER_AUTH_TOKEN=$ENV_AUTH" -e "TWITTER_CSRF_TOKEN=$ENV_CT0" \
  -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1)
expect_contains env-status "$out" "from environment"
case $out in
  *"$ENV_AUTH"* | *"$ENV_CT0"* | *"$DUMMY_AUTH"*) fail "status leaked a credential value: $out" ;;
esac
out=$(app -e "TWITTER_AUTH_TOKEN=$ENV_AUTH" -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1)
expect_contains half-env-status "$out" "TWITTER_CSRF_TOKEN is not"

echo "== ANGELIC_ANGEL_CONFIG selects the config file"
app -e "ANGELIC_ANGEL_CONFIG=$OTHER_CONFIG" -v "$INIT_VOLUME:/config" "$IMAGE" \
  init --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" >/dev/null 2>&1 \
  || fail "init with ANGELIC_ANGEL_CONFIG failed"
out=$(app -e "ANGELIC_ANGEL_CONFIG=$OTHER_CONFIG" -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1)
expect_contains config-env "$out" "$OTHER_CONFIG"
out=$(helper -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" sh -c "stat -c '%n %u' $OTHER_CONFIG") \
  || fail "ANGELIC_ANGEL_CONFIG did not create $OTHER_CONFIG: $out"
expect_contains config-env-file "$out" "$OTHER_CONFIG 65532"

echo "== multiple accounts: a legacy config is converted on its first save"
# The pre-0.4 layout (top-level [twitter]) must keep working as the `default` account. Reading
# never rewrites the file; the first save backs the original up as <config>.legacy.bak (0600,
# never overwritten) and switches the file to [[accounts]].
multi() {
  app -e "ANGELIC_ANGEL_CONFIG=$MULTI_CONFIG" -e NO_COLOR=1 -v "$INIT_VOLUME:/config" "$IMAGE" "$@"
}
helper -i -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" sh -c "cat > $MULTI_CONFIG" <<TOML \
  || fail "cannot seed the legacy multi-account config"
[twitter]
auth_token = "$DUMMY_AUTH"
ct0 = "$DUMMY_CT0"
TOML
out=$(multi status 2>&1) || fail "status failed on a legacy config: $out"
expect_contains legacy-status "$out" "$DUMMY_AUTH"
expect_lacks legacy-status "$out" "Account:"
helper -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" \
  sh -c "test ! -e $MULTI_CONFIG.legacy.bak && grep -q '^\[twitter\]' $MULTI_CONFIG" \
  || fail "reading a legacy config must neither back it up nor rewrite it"

out=$(multi init --account work --auth-token "$WORK_AUTH" --ct0 "$WORK_CT0" 2>&1) \
  || fail "init --account work failed: $out"
expect_lacks init-work "$out" TAILSECRET
out=$(helper -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%a' $MULTI_CONFIG $MULTI_CONFIG.legacy.bak && cat $MULTI_CONFIG.legacy.bak") \
  || fail "cannot inspect the converted config: $out"
expect_contains legacy-backup "$out" "$MULTI_CONFIG 65532:600"
expect_contains legacy-backup "$out" "$MULTI_CONFIG.legacy.bak 65532:600"
expect_contains legacy-backup "$out" "[twitter]"
expect_contains legacy-backup "$out" "$DUMMY_AUTH"
expect_lacks legacy-backup "$out" "[[accounts]]"
out=$(helper -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" sh -c "cat $MULTI_CONFIG") \
  || fail "cannot read the converted config"
expect_contains converted "$out" "[[accounts]]"
expect_contains converted "$out" 'id = "default"'
expect_contains converted "$out" 'id = "work"'
expect_contains converted "$out" "$DUMMY_AUTH"

out=$(multi status 2>&1) || fail "status failed with two accounts: $out"
expect_contains multi-status "$out" "Account: default"
expect_contains multi-status "$out" "Account: work"
expect_contains multi-status "$out" "$DUMMY_AUTH"
expect_contains multi-status "$out" "$WORK_AUTH_SHOWN"
expect_contains multi-status "$out" "$WORK_CT0_SHOWN"
expect_lacks multi-status "$out" TAILSECRET
out=$(multi status --account work 2>&1) || fail "status --account work failed: $out"
expect_contains work-status "$out" "$WORK_AUTH_SHOWN"
expect_lacks work-status "$out" "$DUMMY_AUTH"
expect_lacks work-status "$out" TAILSECRET
expect_failure unknown-account 1 multi status --account nope
expect_contains unknown-account "$out" '"nope" not found'
expect_failure invalid-account 2 multi status --account=Bad
expect_contains invalid-account "$out" "invalid account id"
expect_failure unregister-account 1 multi unregister --account work
expect_contains unregister-account "$out" 'No registration found for account "work"'

# A second save leaves the first backup alone.
multi init --account work --auth-token other-auth --ct0 other-ct0 >/dev/null 2>&1 \
  || fail "second init --account work failed"
out=$(helper -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" sh -c "cat $MULTI_CONFIG.legacy.bak") \
  || fail "cannot read the legacy backup"
expect_contains legacy-backup-kept "$out" "$DUMMY_AUTH"
expect_lacks legacy-backup-kept "$out" "other-auth"

echo "== multiple accounts: environment credentials apply to default only"
out=$(app -e "ANGELIC_ANGEL_CONFIG=$MULTI_CONFIG" -e NO_COLOR=1 -e "TWITTER_AUTH_TOKEN=$ENV_AUTH" \
  -e "TWITTER_CSRF_TOKEN=$ENV_CT0" -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1) \
  || fail "status with environment credentials failed: $out"
[ "$(grep -c 'credentials from environment' <<<"$out")" -eq 1 ] \
  || fail "environment credentials must show up for default only: $out"
expect_contains env-multi "$out" "other-auth..."
expect_lacks env-multi "$out" "$ENV_AUTH"
expect_lacks env-multi "$out" "$ENV_CT0"
expect_lacks env-multi "$out" "$DUMMY_AUTH"
out=$(app -e "ANGELIC_ANGEL_CONFIG=$MULTI_CONFIG" -e NO_COLOR=1 -e "TWITTER_AUTH_TOKEN=$ENV_AUTH" \
  -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1) \
  || fail "status with half the environment credentials failed: $out"
expect_contains env-multi-half "$out" "TWITTER_CSRF_TOKEN is not"
expect_contains env-multi-half "$out" "other-auth..."
expect_lacks env-multi-half "$out" "$ENV_AUTH"

echo "== permissions"
out=$(helper -v "$INIT_VOLUME:/config" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%a' /config $CONFIG \
    && test -r $CONFIG && test -w $CONFIG \
    && touch /config/.smoke-write && rm /config/.smoke-write") \
  || fail "helper cannot access /config as UID 65532: $out"
expect_contains permissions "$out" "/config 65532:700"
expect_contains permissions "$out" "$CONFIG 65532:"

echo "== bind mounts: the application follows the owner of /config"
BIND_DIR=$(mktemp -d "${TMPDIR:-/tmp}/angelic-smoke.XXXXXX")
# empty: root-owned, as Docker creates a missing bind-mount source.
# legacy: root-owned with files an earlier root run left behind.
# nas: root-owned, holding a 0600 config of UID 1000 that an earlier `user: 1000:1000` run left.
# foreign: owned by an arbitrary UID.
# locked: root-owned, for a container that is told to run as a non-root user.
root_helper -v "$BIND_DIR:/work" "$HELPER_IMAGE" sh -c '
  mkdir /work/empty /work/legacy /work/nas /work/foreign /work/locked &&
  chown 12345:12345 /work/foreign && chmod 700 /work/foreign' \
  || fail "cannot create the bind-mount directories"

# a. A root-owned empty directory is handed to 65532.
out=$(app -v "$BIND_DIR/empty:/config" "$IMAGE" init --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" 2>&1) \
  || fail "init failed on a root-owned empty bind mount: $out"
expect_contains bind-empty "$out" "running as 65532:65532 owning /config"
out=$(root_helper -v "$BIND_DIR/empty:/config" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%g' /config $CONFIG $CONFIG.lock") \
  || fail "cannot inspect the root-owned empty bind mount: $out"
expect_contains bind-empty-owner "$out" "/config 65532:65532"
expect_contains bind-empty-owner "$out" "$CONFIG 65532:65532"
expect_contains bind-empty-owner "$out" "$CONFIG.lock 65532:65532"
out=$(app -v "$BIND_DIR/empty:/config" "$IMAGE" status 2>&1) || fail "status failed on the empty bind mount: $out"
expect_contains bind-empty-status "$out" "$DUMMY_AUTH"

# a'. Known files of a root-owned directory follow it; unrelated files stay as they are.
root_helper -i -v "$BIND_DIR/legacy:/config" "$HELPER_IMAGE" \
  sh -c "cat > $CONFIG && touch $CONFIG.lock /config/unrelated.txt \
    $CONFIG.legacy.bak $CONFIG.work.notifications.sqlite3 $CONFIG.work.notifications.sqlite3-wal \
    $CONFIG.Work.notifications.sqlite3" <<TOML \
  || fail "cannot seed the legacy bind mount"
[twitter]
auth_token = "$DUMMY_AUTH"
ct0 = "$DUMMY_CT0"
TOML
out=$(app -v "$BIND_DIR/legacy:/config" "$IMAGE" status 2>&1) || fail "status failed on the legacy bind mount: $out"
expect_contains bind-legacy "$out" "running as 65532:65532 owning /config"
expect_contains bind-legacy "$out" "$DUMMY_AUTH"
out=$(root_helper -v "$BIND_DIR/legacy:/config" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%g' /config $CONFIG $CONFIG.lock /config/unrelated.txt \
    $CONFIG.legacy.bak $CONFIG.work.notifications.sqlite3 $CONFIG.work.notifications.sqlite3-wal \
    $CONFIG.Work.notifications.sqlite3") \
  || fail "cannot inspect the legacy bind mount: $out"
expect_contains bind-legacy-owner "$out" "/config 65532:65532"
expect_contains bind-legacy-owner "$out" "$CONFIG 65532:65532"
expect_contains bind-legacy-owner "$out" "$CONFIG.lock 65532:65532"
expect_contains bind-legacy-owner "$out" "/config/unrelated.txt 0:0"
# Per-account history databases and the legacy backup are managed files; an id that cannot be
# a valid account id (here an uppercase one) is not.
expect_contains bind-legacy-owner "$out" "$CONFIG.legacy.bak 65532:65532"
expect_contains bind-legacy-owner "$out" "$CONFIG.work.notifications.sqlite3 65532:65532"
expect_contains bind-legacy-owner "$out" "$CONFIG.work.notifications.sqlite3-wal 65532:65532"
expect_contains bind-legacy-owner "$out" "$CONFIG.Work.notifications.sqlite3 0:0"

# a''. An existing config owned by a non-root user wins over the root-owned directory:
# the directory is handed to that user, so a 0600 file stays readable and a lock can be created.
root_helper -i -v "$BIND_DIR/nas:/config" "$HELPER_IMAGE" \
  sh -c "cat > $CONFIG && chown 1000:10 $CONFIG && chmod 600 $CONFIG" <<TOML \
  || fail "cannot seed the nas bind mount"
[twitter]
auth_token = "x"
ct0 = "y"
TOML
out=$(app -v "$BIND_DIR/nas:/config" "$IMAGE" status 2>&1) || fail "status failed on the nas bind mount: $out"
expect_contains bind-nas "$out" "running as 1000:10 owning /config"
expect_contains bind-nas "$out" "$CONFIG"
out=$(app -v "$BIND_DIR/nas:/config" "$IMAGE" init --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" 2>&1) \
  || fail "init failed on the nas bind mount: $out"
out=$(root_helper -v "$BIND_DIR/nas:/config" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%g %a' /config $CONFIG $CONFIG.lock") \
  || fail "cannot inspect the nas bind mount: $out"
expect_contains bind-nas-owner "$out" "/config 1000:10 "
expect_contains bind-nas-owner "$out" "$CONFIG 1000:10 600"
expect_contains bind-nas-owner "$out" "$CONFIG.lock 1000:10 "

# b. A directory owned by another UID is followed as is; the process is not root.
out=$(app -v "$BIND_DIR/foreign:/config" "$IMAGE" init --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" 2>&1) \
  || fail "init failed on a bind mount owned by 12345:12345: $out"
expect_contains bind-foreign "$out" "running as 12345:12345 owning /config"
out=$(app -v "$BIND_DIR/foreign:/config" "$IMAGE" status 2>&1) \
  || fail "status failed on a bind mount owned by 12345:12345: $out"
expect_contains bind-foreign-status "$out" "running as 12345:12345 owning /config"
expect_contains bind-foreign-status "$out" "$DUMMY_AUTH"
out=$(root_helper -v "$BIND_DIR/foreign:/config" "$HELPER_IMAGE" \
  sh -c "stat -c '%n %u:%g' /config $CONFIG $CONFIG.lock") \
  || fail "cannot inspect the bind mount owned by 12345:12345: $out"
expect_contains bind-foreign-owner "$out" "/config 12345:12345"
expect_contains bind-foreign-owner "$out" "$CONFIG 12345:12345"
expect_contains bind-foreign-owner "$out" "$CONFIG.lock 12345:12345"

# c. An explicit non-root user is respected: nothing is switched or chowned.
out=$(app --user 65532:65532 -v "$INIT_VOLUME:/config" "$IMAGE" status 2>&1) \
  || fail "status failed with --user 65532:65532: $out"
expect_contains explicit-user "$out" "$DUMMY_AUTH"
case $out in
  *"running as"*) fail "a non-root start must not switch users: $out" ;;
esac
set +e
out=$(app --user 65532:65532 -v "$BIND_DIR/locked:/config" "$IMAGE" init \
  --auth-token "$DUMMY_AUTH" --ct0 "$DUMMY_CT0" 2>&1)
code=$?
set -e
[ "$code" -ne 0 ] || fail "init as 65532 succeeded on a root-owned directory: $out"
expect_contains explicit-user-denied "$out" "Permission denied"
out=$(root_helper -v "$BIND_DIR/locked:/config" "$HELPER_IMAGE" sh -c "stat -c '%n %u:%g' /config; ls -A /config") \
  || fail "cannot inspect the locked bind mount: $out"
expect_contains explicit-user-untouched "$out" "/config 0:0"
case $out in
  *angelic-angel*) fail "a non-root start must not create files in a root-owned directory: $out" ;;
esac

echo "== listen without registration"
set +e
out=$(app -v "$INIT_VOLUME:/config" "$IMAGE" listen 2>&1)
code=$?
set -e
[ "$code" -eq 1 ] || fail "listen without registration exited $code, expected 1: $out"
expect_contains listen-missing "$out" "No registration found"

echo "== listen with environment credentials: failed auto-registration"
# Offline, so the automatic registration cannot succeed. It must fail for that reason
# (not with "No registration found"), wait before exiting so a restart loop does not
# hammer X, exit non-zero and write no credential into the config directory.
docker run -d --name "$AUTOREG_CONTAINER" --network none \
  -e "TWITTER_AUTH_TOKEN=$ENV_AUTH" -e "TWITTER_CSRF_TOKEN=$ENV_CT0" \
  -v "$AUTOREG_VOLUME:/config" "$IMAGE" listen >/dev/null \
  || fail "cannot start auto-registration container"
i=0
while [ "$i" -lt 90 ]; do
  [ "$(docker inspect -f '{{.State.Running}}' "$AUTOREG_CONTAINER")" = "true" ] || break
  sleep 1
  i=$((i + 1))
done
[ "$(docker inspect -f '{{.State.Running}}' "$AUTOREG_CONTAINER")" != "true" ] \
  || fail "auto-registration container did not exit within 90s: $(docker logs "$AUTOREG_CONTAINER" 2>&1)"
out=$(docker logs "$AUTOREG_CONTAINER" 2>&1)
[ "$(docker inspect -f '{{.State.ExitCode}}' "$AUTOREG_CONTAINER")" = "1" ] \
  || fail "failed auto-registration exited with $(docker inspect -f '{{.State.ExitCode}}' "$AUTOREG_CONTAINER"), expected 1: $out"
expect_contains autoreg "$out" "automatic registration failed"
case $out in
  *"No registration found. Run"*) fail "auto-registration was not attempted: $out" ;;
  *"$ENV_AUTH"* | *"$ENV_CT0"*) fail "log leaked a credential value: $out" ;;
esac
docker rm "$AUTOREG_CONTAINER" >/dev/null || fail "cannot remove $AUTOREG_CONTAINER"
helper -v "$AUTOREG_VOLUME:/config" "$HELPER_IMAGE" \
  sh -c "! grep -rqF -e '$ENV_AUTH' -e '$ENV_CT0' /config" \
  || fail "environment credentials were written under /config"

echo "== listen offline with fabricated registration (legacy layout)"
helper -i -v "$LISTEN_VOLUME:/config" "$HELPER_IMAGE" sh -c "cat > $CONFIG" <<TOML \
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
out=$(app -v "$LISTEN_VOLUME:/config" "$IMAGE" status 2>&1)
expect_contains fabricated-status "$out" "active"

docker run -d --name "$LISTEN_CONTAINER" --network none -v "$LISTEN_VOLUME:/config" "$IMAGE" >/dev/null \
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

echo "== listen offline with two fabricated registrations"
# Two accounts run side by side in one process, each failing to connect on its own.
docker rm "$LISTEN_CONTAINER" >/dev/null || fail "cannot remove $LISTEN_CONTAINER"
helper -i -v "$LISTEN_VOLUME:/config" "$HELPER_IMAGE" sh -c "cat > $CONFIG" <<TOML \
  || fail "cannot seed the fabricated registrations"
[[accounts]]
id = "default"
webhook_endpoint = "https://hook.invalid/default"
webhook_secret = "$WEBHOOK_SECRET"

[accounts.twitter]
auth_token = "$DUMMY_AUTH"
ct0 = "$DUMMY_CT0"

[accounts.registration]
endpoint = "https://invalid.example/push"

[accounts.registration.autopush]
uaid = "00000000-0000-0000-0000-000000000000"
channel_id = "00000000-0000-0000-0000-000000000001"

[accounts.registration.keys]
public_key = [1, 2, 3]
private_key = [1, 2, 3]
auth_secret = [1, 2, 3]

[[accounts]]
id = "work"
webhook_endpoint = "https://hook.invalid/work"
webhook_secret = "$WEBHOOK_SECRET"

[accounts.twitter]
auth_token = "$WORK_AUTH"
ct0 = "$WORK_CT0"

[accounts.registration]
endpoint = "https://invalid.example/push-work"

[accounts.registration.autopush]
uaid = "00000000-0000-0000-0000-000000000002"
channel_id = "00000000-0000-0000-0000-000000000003"

[accounts.registration.keys]
public_key = [1, 2, 3]
private_key = [1, 2, 3]
auth_secret = [1, 2, 3]
TOML
out=$(app -e NO_COLOR=1 -v "$LISTEN_VOLUME:/config" "$IMAGE" status 2>&1)
expect_contains two-accounts-status "$out" "Account: default"
expect_contains two-accounts-status "$out" "Account: work"
[ "$(grep -c 'Registration  active' <<<"$out")" -eq 2 ] \
  || fail "both fabricated registrations should be active: $out"
expect_lacks two-accounts-status "$out" TAILSECRET
expect_lacks two-accounts-status "$out" WHSECRET

docker run -d --name "$LISTEN_CONTAINER" --network none -v "$LISTEN_VOLUME:/config" "$IMAGE" >/dev/null \
  || fail "cannot start the two-account listener"
failures=0
i=0
while [ "$i" -lt 30 ]; do
  failures=$(docker logs "$LISTEN_CONTAINER" 2>&1 | grep -c "connection failed, retrying" || true)
  [ "$failures" -ge 2 ] && break
  [ "$(docker inspect -f '{{.State.Running}}' "$LISTEN_CONTAINER")" = "true" ] \
    || fail "two-account listener exited early: $(docker logs "$LISTEN_CONTAINER" 2>&1)"
  sleep 1
  i=$((i + 1))
done
[ "$failures" -ge 2 ] \
  || fail "expected a connection failure/retry log per account: $(docker logs "$LISTEN_CONTAINER" 2>&1)"
docker stop -t 5 "$LISTEN_CONTAINER" >/dev/null || fail "docker stop failed"
[ "$(docker inspect -f '{{.State.ExitCode}}' "$LISTEN_CONTAINER")" = "0" ] \
  || fail "two-account listener exit code after stop is not 0"
out=$(docker logs "$LISTEN_CONTAINER" 2>&1)
expect_contains two-accounts-sigint "$out" "Interrupted"
expect_lacks two-accounts-log "$out" TAILSECRET
expect_lacks two-accounts-log "$out" WHSECRET

echo "OK"
