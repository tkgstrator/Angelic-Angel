# Angelic Angel

> "Angelic Angel/Hello, Hoshi wo Kazoete" is a single by μ's released on July 1, 2015 from Lantis. The song is an insert song for the film *Love Live! The School Idol Movie*.
>
> — [Wikipedia](https://ja.wikipedia.org/wiki/Angelic_Angel/Hello,%E6%98%9F%E3%82%92%E6%95%B0%E3%81%88%E3%81%A6)

A CLI tool that receives Twitter/X notifications in real time via Mozilla's Web Push infrastructure. Stream tweets from users you follow and have tweet notifications enabled for.

[日本語版 README](README.ja.md)

## Overview

Angelic Angel emulates a browser's Web Push client to receive Twitter/X push notifications. It connects to [Mozilla AutoPush](https://autopush.readthedocs.io/) via WebSocket, decrypts incoming notifications using ECE (Encrypted Content-Encoding), and forwards the decrypted payloads to a configured webhook endpoint.

You will receive notifications for tweets from users that you **follow** and have **tweet notifications turned on** for on Twitter/X.

### How It Works

```
Twitter/X  ──push──▶  Mozilla AutoPush Server  ◀──WebSocket──  Angelic Angel  ──HTTP POST──▶  Webhook
```

1. Angelic Angel registers as a Web Push subscriber with Mozilla's AutoPush server.
2. The push subscription endpoint is registered with Twitter's notification settings API.
3. When Twitter sends a push notification, it goes through Mozilla's AutoPush server — the same infrastructure used by Firefox.
4. Angelic Angel receives and decrypts the notification via WebSocket, then forwards the payload to your webhook.

### Important Notes

- **Data source**: All notification data is received from Mozilla's Web Push server (`push.services.mozilla.com`). Angelic Angel does not access Twitter/X directly for notification data.
- **Minimal API usage**: The Twitter/X API is only called during the initial push subscription registration (`register` command, or the automatic registration of the first `listen` with credentials in the environment) and when you explicitly start the WebUI's bulk notification operation. No API calls are made while listening for notifications.
- **No scraping**: This tool does not perform any web scraping. It uses the standard W3C Push API flow, the same mechanism browsers use to deliver push notifications.

## Requirements

- Rust (edition 2024)
- Twitter/X account credentials (`auth_token` and `ct0` cookies)

### Getting `auth_token` and `ct0`

1. Open [x.com](https://x.com) in your web browser and log in.
2. Open Developer Tools (F12) and go to the **Application** (or **Storage**) tab.
3. Under **Cookies** → `https://x.com`, find the values for `auth_token` and `ct0`.

## Installation

```sh
cargo install --path .
```

## Usage

### 1. Initialize configuration

```sh
# Interactive mode
angelic-angel init

# Or with arguments
angelic-angel init --auth-token YOUR_AUTH_TOKEN --ct0 YOUR_CT0
```

This creates `angelic-angel.toml` with your Twitter credentials.

### 2. Register push subscription

```sh
angelic-angel register
```

This registers a new push subscription with Mozilla AutoPush and then registers the endpoint with Twitter's push notification API.

### 3. Start listening

```sh
WEBHOOK_ENDPOINT=https://your-webhook.example.com/endpoint angelic-angel listen
```

The `WEBHOOK_ENDPOINT` environment variable specifies where decrypted notification payloads are sent via HTTP POST.

Alternatively, skip steps 1 and 2: with `TWITTER_AUTH_TOKEN` and `TWITTER_CSRF_TOKEN` (the `auth_token` and `ct0` cookies) set, `listen` registers on its first start when there is no registration yet. The values are read from the environment and never written to the config file. See [Docker](#docker).

### Other commands

```sh
# Check current configuration and registration status
angelic-angel status

# Remove push subscription
angelic-angel unregister
```

### WebUI operations

The WebUI (`angelic-angel serve`) has three screens: Notifications, Settings and Status. Besides the Cookie and Webhook settings, it offers:

- **Cookie check** (Settings screen): the "確認" button, and an automatic check after every save, ask X whether the *saved* cookies still work and show the account name. A result of "could not check" (network error, rate limit, X outage) is shown as such and is never reported as an invalid cookie. Unsaved input is not checked; save it first.
- **Receiver registration** (Settings screen, below the cookies): registers or unregisters the Web Push subscription. Cookies cannot be changed while registered.
- **Turn on notifications for everyone you follow** (Settings screen, below the registration): after an inline confirmation, it enables post notifications for every account you follow on X. This changes settings on X for all of them, and cancelling midway does not undo the accounts already changed. It runs one job at a time, shows progress (total, succeeded, failed, remaining), reports failures and partial results as they are, and never retries by itself; running it again needs a new confirmation. It needs the saved cookies but not a Web Push registration.
- **Notifications** (Notifications screen): lists received notifications newest first, with the icon, text, a link to the post and the original payload. Every received notification is kept: they are stored in a local SQLite database, `<config>.notifications.sqlite3` (for example `angelic-angel.toml.notifications.sqlite3`), created with mode `0600` and never pruned. It contains the received payloads, so treat it like the config. Older entries are loaded on demand with the "もっと見る" (show more) button.

### Options

| Flag | Description |
|------|-------------|
| `-c, --config <PATH>` | Configuration file path (env: `ANGELIC_ANGEL_CONFIG`, default: `angelic-angel.toml`) |
| `-v, --verbose` | Enable debug logging |

## Docker

Requires Docker Compose 2.24+. The container only needs your X credentials and a webhook in environment variables: on the first start it registers itself and stores the registration in a mounted directory.

```sh
mkdir config && sudo chown 65532:65532 config   # the container runs as UID 65532
cp .env.example .env   # then edit .env: TWITTER_AUTH_TOKEN, TWITTER_CSRF_TOKEN and your webhook

docker compose up -d --build
docker compose logs -f
docker compose stop
```

- `TWITTER_AUTH_TOKEN` and `TWITTER_CSRF_TOKEN` are the `auth_token` and `ct0` cookie values (see [Getting `auth_token` and `ct0`](#getting-auth_token-and-ct0)).
- The container runs as UID 65532, so `./config` must be writable by it. If Docker creates a missing `./config` itself, it is owned by root and the container fails with "cannot open lock file". To run as your own user instead, keep `./config` owned by you and add `user: "1000:1000"` (your `id -u`:`id -g`) to the service. Files in `./config` written earlier by UID 65532 must then be `chown`ed to that user.
- Mount the directory, not the file: the config is saved through a temporary file and a rename, and the lock and the history database live next to it.
- Inside a container, `localhost` is the container itself. For a webhook on the host, use a URL such as `http://host.docker.internal:8080/endpoint`. On Linux, the host service must listen on an address reachable from Docker, such as the bridge IP or `0.0.0.0`.
- After editing `.env`, run `docker compose up -d` to recreate the container with the new values; `restart` alone does not reload them.
- The default command is `listen`; other commands replace it, e.g. `docker compose run --rm angelic-angel status`. Stop the running container first (`docker compose stop`): a config is locked by one process at a time.

### Using the published image

Instead of building, use the image from GHCR. The environment variables are interpolated from the `.env` next to `compose.yaml`:

```yaml
services:
  angelic-angel:
    image: ghcr.io/tkgstrator/angelic-angel:latest
    restart: unless-stopped
    stop_signal: SIGINT
    environment:
      TWITTER_AUTH_TOKEN: ${TWITTER_AUTH_TOKEN}
      TWITTER_CSRF_TOKEN: ${TWITTER_CSRF_TOKEN}
      WEBHOOK_ENDPOINT: ${WEBHOOK_ENDPOINT}
      WEBHOOK_SECRET_KEY: ${WEBHOOK_SECRET_KEY}
    extra_hosts:
      - "host.docker.internal:host-gateway"
    volumes:
      - ./config:/config
```

```sh
# .env (never commit it)
TWITTER_AUTH_TOKEN=your-auth_token-cookie-value
TWITTER_CSRF_TOKEN=your-ct0-cookie-value
WEBHOOK_ENDPOINT=https://your-webhook.example.com/endpoint
WEBHOOK_SECRET_KEY=your-shared-secret
```

- Setting the credentials through environment variables and the automatic registration need an image from a release that includes them; the earlier releases (up to v0.1.1) do not. If `latest` is an older image, `docker compose pull` fetches the current one.
- For reproducible deployments, pin a version tag instead of `latest` (the `X.Y.Z` form, e.g. `ghcr.io/tkgstrator/angelic-angel:0.2.0`).

### Environment variables

| Variable | Description |
|----------|-------------|
| `TWITTER_AUTH_TOKEN` | The `auth_token` cookie. Set it together with `TWITTER_CSRF_TOKEN` |
| `TWITTER_CSRF_TOKEN` | The `ct0` cookie. Set it together with `TWITTER_AUTH_TOKEN` |
| `WEBHOOK_ENDPOINT` | Webhook URL. A non-blank value takes precedence over the config file |
| `WEBHOOK_SECRET_KEY` | Shared secret used to sign webhook requests. Required when a webhook URL is set |
| `ANGELIC_ANGEL_CONFIG` | Config file path. The image sets `/config/angelic-angel.toml`; `--config` takes precedence |

- The two credentials are a pair. When both are set (non-blank) they are used instead of `[twitter]` in the config file; when neither is, the config file is used; **when only one is set, the command fails with an error** (it never falls back to the file).
- The credential values are never written to the config file. They are also used when a registration is renewed after AutoPush invalidates it (this can happen days later), so keep them set for as long as the container runs. `status` shows that they come from the environment without printing the values.

### First start and stored state

- If there is no registration and both credentials are set, `listen` registers with AutoPush and X, saves the registration, and starts receiving. An existing config file keeps its other settings (webhook, notification filter, `[twitter]`); a missing one is created with mode `0600`. Before contacting X it checks that the config can be saved, so a read-only mount fails without leaving registrations behind on X's side.
- The registration (UAID and keys) is stored in `./config/angelic-angel.toml`, and the notification history in `./config/angelic-angel.toml.notifications.sqlite3`. Keep `./config` across restarts; treat it like a credential.
- If the automatic registration fails (for example because the cookies are no longer valid), the error is logged and the process waits 30 seconds before exiting with a non-zero code, so that `restart: unless-stopped` does not hit X's login endpoints in a tight loop. `docker stop` (SIGINT) ends the wait immediately. Errors found before contacting X (half-set credentials, an unwritable `./config`) exit at once.
- Without credentials and without a registration, `listen` exits with "No registration found" and the restart policy keeps retrying. Set the credentials, or place a registered config file in `./config`.

### Changing the credentials

```sh
docker compose stop
docker compose run --rm angelic-angel unregister
sudo rm config/angelic-angel.toml     # then edit .env with the new values
docker compose up -d
```

`unregister` removes the AutoPush subscription; deleting the config file makes the next start register again with the new credentials. This also drops the webhook and filter stored in that file. The history database is kept.

### Running more than one instance

Do not run the same registration (config file) in two places at once. If two containers each register automatically from the environment, they get different UAIDs, so the same account's notifications arrive twice and your webhook receives every one of them twice. Run a single container per config directory.

### Without environment credentials

You can also bring your own config file instead. Place a registered `angelic-angel.toml` in `./config` (owned by UID 65532), or create it in the container:

```sh
# Prompts for credentials, avoiding secrets in shell history
docker compose run --rm -it angelic-angel init
docker compose run --rm angelic-angel register
docker compose run --rm angelic-angel status
docker compose up -d
```

Do not run `up` before the registration exists, or the restart policy will loop on failure.

### Migrating from the `/data` volume

Earlier versions kept everything in the named volume `angelic-angel-data` mounted at `/data`. Copy it into `./config` before switching:

```sh
docker compose down                       # keeps the volume (do not use -v)
docker volume ls | grep angelic-angel-data   # the name is <project>_angelic-angel-data
mkdir config
docker run --rm -v <project>_angelic-angel-data:/from:ro -v "$PWD/config":/to busybox:1.37.0 \
  sh -c 'cp -a /from/. /to/ && chown -R 65532:65532 /to && chmod 700 /to'
docker compose up -d
```

Once it runs from `./config`, delete the old volume with `docker volume rm`.

## Development

Rust is pinned to 1.99.0 (`rust-toolchain.toml`, CI, Docker and the Dev Container).

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features   # currently 0 tests
docker build -t angelic-angel:local .
scripts/docker-smoke.sh angelic-angel:local        # offline, dummy credentials only
```

The runtime uses Distroless with a stripped binary, without a shell, sources or build caches. `scripts/docker-smoke.sh` verifies configuration persistence, `ANGELIC_ANGEL_CONFIG`, permissions, expected errors (including a failed automatic registration, which waits 30 seconds) and SIGINT shutdown with dummy credentials and networking disabled. It pulls a separate BusyBox helper if needed and removes its own test resources.

To debug the image, use `docker logs` and run CLI subcommands (e.g. `docker run --rm IMAGE status`) rather than `exec`ing a shell. Published images can be pulled from GHCR: `docker pull ghcr.io/<owner>/<repo>:latest`.

### WebUI development

`bun dev` from the repository root builds the receiver (`cargo build --locked`), then runs two child processes: `angelic-angel serve` (the API on `127.0.0.1:8080`) and the Vite dev server for `web/` (`http://localhost:11475/`, which proxies `/api` and `/health` to the API). Install the frontend dependencies once with `bun install --cwd web --frozen-lockfile`.

- It uses `angelic-angel.toml` in the repository root. `serve` starts the real receiver when that config already holds a registration, so point `DEV_CONFIG` at a scratch file (for example `DEV_CONFIG=.cache/dev/angelic-angel.toml bun dev`) when you only want to try the UI. The launcher prints the config in use.
- The WebUI is local-only and needs no authentication or login: it opens straight onto the three screens. The `.lock` file is stored next to the config and is git-ignored.
- Nothing is watched or restarted: frontend edits hot-reload, but Rust changes need a restart of `bun dev`. If either child exits, the other is stopped too. `Ctrl+C` (SIGINT/SIGTERM) stops only the processes it started and waits for them; a second `Ctrl+C` forces it.
- Vite and the API both listen on `127.0.0.1` only. The WebUI has no authentication, so do not expose these ports to other hosts or networks.
- The proxy keeps the browser's `Host` and `Origin` headers (`changeOrigin: false`), so the API's same-origin check for `PUT`/`POST`/`DELETE` passes only for the page served by Vite itself. Open the UI at the printed URL, not through another origin.
- `DEV_API_PORT` and `DEV_WEB_PORT` override the ports (default `8080` and `11475`; the Vite port is strict and never falls back). `cd web && bun run dev` runs Vite alone against an API you started yourself (`VITE_API_TARGET` overrides the proxy target).

### Dev Container

`.devcontainer/` provides an Ubuntu 24.04 Rust environment (GitHub CLI, Claude Code, Codex CLI, Bun, Docker outside of Docker, act, direnv). Create the host directories it bind-mounts before opening it: `mkdir -p ~/.claude ~/.codex`. Build artifacts and Cargo caches live in named volumes. Host `gh` is not mounted, so run `gh auth login` inside the Dev Container if needed; no host AWS/SSH/gh config is mounted.

`CODEX_HOME` points at the bind-mounted host `~/.codex` (`/home/vscode/.codex` in the container), so Codex history, sessions, auth, skills and project trust are shared with the host and survive container rebuilds. `model_provider` and the `[model_providers.*]` gateway definition (API keys are referenced by environment variable name, e.g. `env_key = "OPENAI_API_KEY"`) belong in the host `~/.codex/config.toml`; the Dev Container does not write them.

On first create, `.codex/config.toml` (git-ignored) is seeded with project-level overrides only: `model = "gpt-6.1-sol"` and `model_reasoning_effort = "xhigh"`. An existing file is never overwritten. Edit or delete it to change the model.

### CI/CD

- `.github/workflows/integration.yaml` runs on pull requests, non-`master` branch pushes and as a reusable workflow: commitlint over the new commit range, rustfmt, Clippy (warnings denied), `cargo test --locked`, and a Docker build plus `scripts/docker-smoke.sh`. Permissions are `contents: read`.
- `.github/workflows/deployment.yaml` runs on `master` pushes, `v*` tags and manual dispatch (`master` only). It calls the integration workflow first, then publishes a multi-arch (`linux/amd64`, `linux/arm64`) image to `ghcr.io/<owner>/<repo>`. Each architecture is built natively on its own runner (`ubuntu-24.04` for amd64, `ubuntu-24.04-arm` for arm64, no QEMU) and the results are merged into a single multi-arch image. Tags are `sha-<short>`; `latest` on `master`; and `<version>` only for a `v<version>` tag in `master` history that matches `Cargo.toml`. Only the build and merge jobs have `packages: write`.
- Commit messages follow Conventional Commits (`.commitlintrc.yaml`; types: build, chore, ci, docs, feat, fix, perf, refactor, revert, test).

## Reconnection

Angelic Angel implements a Firefox-compatible reconnection strategy:

- Exponential backoff: 5s × 2^n, capped at 5 minutes
- Automatic re-registration on UAID invalidation
- Server backoff (close code 4774): 30-minute delay
- Infinite retries with counter reset on successful connection

## License

MIT
