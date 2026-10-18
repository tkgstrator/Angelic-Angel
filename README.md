# Angelic Angel

> "Angelic Angel/Hello, Hoshi wo Kazoete" is a single by μ's released on July 1, 2015 from Lantis. The song is an insert song for the film *Love Live! The School Idol Movie*.
>
> — [Wikipedia](https://ja.wikipedia.org/wiki/Angelic_Angel/Hello,%E6%98%9F%E3%82%92%E6%95%B0%E3%81%88%E3%81%A6)

A CLI tool that receives Twitter/X notifications in real time via Mozilla's Web Push infrastructure. Stream tweets from users you follow and have tweet notifications enabled for.

[日本語版 README](README.ja.md)

## Overview

Angelic Angel emulates a browser's Web Push client to receive Twitter/X push notifications. It connects to [Mozilla AutoPush](https://autopush.readthedocs.io/) via WebSocket, decrypts incoming notifications using ECE (Encrypted Content-Encoding), and forwards the decrypted payloads to a configured webhook endpoint.

You will receive notifications for tweets from users that you **follow** and have **tweet notifications turned on** for on Twitter/X.

One process can receive for several X accounts at once. Each account has its own cookies, push registration, webhook (endpoint and secret), notification filter and notification history. See [Multiple accounts](#multiple-accounts).

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
- **Minimal API usage**: Notification reception uses Web Push. Initial registration, explicit WebUI operations and the optional machine API contact X; account/readiness listings do not.
- **Web Push**: Notification delivery uses the standard W3C Push API flow. Explicit X operations also obtain public bootstrap/signature metadata; retrieved JavaScript is parsed, never executed.

### Optional machine API (v0.5)

`serve` can additionally expose a dedicated Bearer-authenticated listener (default `127.0.0.1:8081`), completely separate from the local management UI on port 8080. It supports scoped account/readiness/identity reads, text posts/replies, and durable operation result queries. Cookies and webhook secrets never leave Angel through this API. It is disabled when `MACHINE_SERVICE_TOKEN` is absent; empty tokens and incomplete scopes are rejected. `listen` and all existing CLI commands retain their existing behavior; the machine API is enabled only by `serve`.

Configure a new service token, `MACHINE_CLIENT_ID`, `MACHINE_ACCOUNTS` and `MACHINE_OPERATIONS`, then enable “APIからの投稿・返信を許可” for the selected account in the local WebUI Settings and save. Posting is OFF by default, including existing/legacy configurations and environment-only `default` accounts. The account toggle does not override token scopes or the account allowlist. Start `angelic-angel --config /config/angelic-angel.toml serve` to access these settings. See [Machine API](docs/machine-api.md) for the complete configuration, request format, revision checks, indefinite idempotency retention and `unknown` recovery rules. Lookup, likes, follows, media and quotes are not provided; this does not yet eliminate the Bot's other Cookie dependencies.

## Requirements

- Rust (edition 2024)
- Twitter/X account credentials (`auth_token` and `ct0` cookies), for each account you want to receive for

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

This creates `angelic-angel.toml` with your Twitter credentials, as the account `default`. To set up another account, pass `--account <id>` (see [Multiple accounts](#multiple-accounts)); the other accounts in the file are left untouched. `init` refuses to overwrite a config file that cannot be parsed.

### 2. Register push subscription

```sh
angelic-angel register
```

This registers a new push subscription with Mozilla AutoPush and then registers the endpoint with Twitter's push notification API. It acts on the `default` account unless you pass `--account <id>`.

### 3. Start listening

```sh
WEBHOOK_ENDPOINT=https://your-webhook.example.com/endpoint angelic-angel listen
```

The `WEBHOOK_ENDPOINT` environment variable specifies where decrypted notification payloads are sent via HTTP POST. Like the other environment variables, it applies to the `default` account only; see [Environment variables](#environment-variables).

`listen` runs every registered account in one process.

Alternatively, skip steps 1 and 2: with `TWITTER_AUTH_TOKEN` and `TWITTER_CSRF_TOKEN` (the `auth_token` and `ct0` cookies) set, `listen` registers the `default` account on its first start when it has no registration yet. The values are read from the environment and never written to the config file. See [Docker](#docker).

### Other commands

```sh
# Check current configuration and registration status (every account)
angelic-angel status

# Check a single account
angelic-angel status --account shop

# Remove push subscription
angelic-angel unregister

# ... of a single account
angelic-angel unregister --account shop
```

### Multiple accounts

One config file can hold any number of accounts, and one process (`listen` or `serve`) receives for all of them. The file is a list of `[[accounts]]` entries:

```toml
[[accounts]]
id = "default"
webhook_endpoint = "https://example.com/hook"
webhook_secret = "..."

[accounts.twitter]
auth_token = "..."
ct0 = "..."

# Registering the account makes the tool add an [accounts.registration] table
# here (endpoint, AutoPush session, keys). Do not write or edit it by hand.

[[accounts]]
id = "shop"
machine_post_enabled = false
webhook_endpoint = "https://example.com/hook-shop"
webhook_secret = "..."

[accounts.twitter]
auth_token = "..."
ct0 = "..."

[accounts.notification_filter]
quote = false
quote_self = true
```

- `id` must match `^[a-z0-9][a-z0-9_-]{0,31}$` and be unique. It is a stable name: it appears in file names (the history database) and in WebUI URLs, so do not rename it afterwards. `default` is special: the CLI uses it when `--account` is omitted, and the [environment variables](#environment-variables) apply to it only.
- Each account has its own webhook: its own `webhook_endpoint` and `webhook_secret`. The signature scheme and the request body are unchanged. If the same post reaches several of your accounts, every account forwards it to its own webhook; notifications are not de-duplicated across accounts. Give each account its own endpoint if you need to tell them apart.
- Each account has its own notification history database: `<config>.notifications.sqlite3` for `default` (the same file as before) and `<config>.<id>.notifications.sqlite3` for any other account. Both are created with mode `0600` and never pruned.
- Subcommands that act on one account take `--account <id>` (default `default`): `init`, `register`, `unregister` and `status`. `status` without the flag lists every account (a config with a single account prints as before).
- `listen` runs every registered account in one process. An account that fails stops alone and the others keep running; the process exits with a non-zero code only when all accounts have failed. An account other than `default` that has no registration is skipped with a warning: register it with `register --account <id>`, or from the WebUI.
- While `serve` runs, the config is locked by that process, so CLI commands that change it cannot run. Add and remove accounts from the WebUI instead.
- All accounts register with the same device identifier. Whether X accepts several registrations that share it has not been verified here; confirm it with your own accounts.

#### Upgrading a single-account config

A config file in the earlier single-account layout (top-level `[twitter]`, `registration`, `webhook_endpoint` and `notification_filter`) is read as the account `default`. Reading never rewrites the file.

- The first time the config is saved, it is converted to the `[[accounts]]` layout. Just before that, the original is copied to `<config>.legacy.bak` (mode `0600`, never overwritten once it exists).
- The conversion is one-way: **earlier images and versions cannot read the converted file.** To roll back, stop the process, restore `<config>.legacy.bak` over the config file and run the earlier image. Back up the whole config directory before upgrading.
- A file that mixes `[[accounts]]` with the earlier top-level keys is rejected with an error; it is never merged silently.

### WebUI operations

The WebUI (`angelic-angel serve`) has three screens: Notifications, Settings and Status. Every screen works on one account at a time: Notifications, Settings, Status, the receiver registration and the bulk notification job are all per account. Besides the Cookie and Webhook settings, it offers:

- **Machine API posting permission**: “APIからの投稿・返信を許可” controls text posts and replies for the selected account. Changes apply after saving; omitted `machine_post_enabled` means OFF. Saved credentials, token account allowlists and scopes are still required. The toggle is editable with environment credentials and while registered. During a machine preflight/send the save returns busy; disabling is not cancellation of an in-flight send. Existing idempotency/result queries remain available while OFF.

- **Accounts** (header): the switcher in the header chooses the account the screens show. "Add account" asks for an id (same rule as above) and creates an empty account; then enter its cookies and webhook in Settings and register it. Deleting an account asks for confirmation; it unregisters the account (as the `unregister` command does) if it is registered, stops its receiving, removes its entry from the config file and deletes its notification history database. If the unregistration fails, the account is kept and the error is shown. Exceptions: while both cookie [environment variables](#environment-variables) are set, `default` cannot be deleted (409 `account_from_environment`); unset them and restart the process first. If only the deletion of the history database fails after the entry has been removed, the error is a 500 `history_cleanup_failed`: the account is already gone from the config file, and the leftover history database file has to be deleted by hand; until then, creating an account with the same id is refused (409 `history_cleanup_required`) so that it does not inherit that history. The address is `#/<id>/<screen>`; an old address such as `#/settings` opens the `default` account (or the first account when there is none named `default`). Operations of one account never block another account's: a bulk notification job running for one account does not stop you from registering or editing another.
- **Webhook** (Settings screen): the endpoint and the secret of the selected account. The secret is write-only: the screen never shows the saved value, only whether one is set.
- **Cookie check** (Settings screen): the "確認" button, and an automatic check after every save, ask X whether the *saved* cookies still work and show the account name. A result of "could not check" (network error, rate limit, X outage) is shown as such and is never reported as an invalid cookie. Unsaved input is not checked; save it first.
- **Receiver registration** (Settings screen, below the cookies): registers or unregisters the Web Push subscription. Cookies cannot be changed while registered, nor while they come from environment variables (409 `credentials_from_environment`; see [Environment variables](#environment-variables)).
- **Turn on notifications for everyone you follow** (Settings screen, below the registration): after an inline confirmation, it enables post notifications for every account you follow on X. This changes settings on X for all of them, and cancelling midway does not undo the accounts already changed. It runs one job at a time per account, shows progress (total, succeeded, failed, remaining), reports failures and partial results as they are, and never retries by itself; running it again needs a new confirmation. It needs the saved cookies but not a Web Push registration.
- **Notification filter** (Settings screen, below the webhook): turns each kind of notification on or off, for the Notifications list and for Webhook forwarding alike: post (`tweet`), retweet, quote, reply, chat (`xchat_message_received`) and other. Disabled kinds are still recorded in the history. Quotes are split in two, nested under "引用RT": a quote of your own earlier post (`quote_self`) and a quote of another account's post (`quote`). This lets you forward a store account's status updates, which it posts as quotes of its own earlier posts, without forwarding its quotes of other accounts. Ticking or clearing "引用RT" switches both at once, and it shows a mixed state when only one is on. Whether a post is a quote, a self-quote or a reply is looked up on X's public embed API; when that lookup fails the post counts as a plain `tweet`. History entries recorded before `quote_self` existed stay `quote`, since their author can no longer be told. The Webhook body is the received payload as is, so it does not name the kind.
  The filter belongs to the selected account. The same switches are the keys of that account's `[accounts.notification_filter]` table in the config file (`[notification_filter]` in a config that has not been converted yet), and every kind is on when its key is missing. The one exception is `quote_self`: **when it is missing it follows the `quote` value of the same file**, so a config saved before `quote_self` existed keeps behaving as it did (`quote = false` still skips every quote). Saving writes both keys.

  ```toml
  [accounts.notification_filter]
  quote = false       # quotes of other accounts' posts
  quote_self = true   # quotes of the account's own posts; when omitted, same as `quote`
  ```
- **Notifications** (Notifications screen): lists received notifications newest first, with the icon, text, a link to the post and the original payload. Every received notification is kept: they are stored in a local SQLite database per account, `<config>.notifications.sqlite3` for `default` (for example `angelic-angel.toml.notifications.sqlite3`) and `<config>.<id>.notifications.sqlite3` for any other account, created with mode `0600` and never pruned. It contains the received payloads, so treat it like the config. Older entries are loaded on demand with the "もっと見る" (show more) button.

### Options

| Flag | Description |
|------|-------------|
| `-c, --config <PATH>` | Configuration file path (env: `ANGELIC_ANGEL_CONFIG`, default: `angelic-angel.toml`) |
| `-v, --verbose` | Enable debug logging |
| `--account <ID>` | `init`, `register`, `unregister` and `status` only: the account to act on (default: `default`; `status` lists every account when it is omitted) |

## Docker

Requires Docker Compose 2.24+. The container only needs your X credentials and a webhook in environment variables: on the first start it registers itself and stores the registration in a mounted directory. With environment variables alone you get a single account, `default`; to receive for more accounts, see [Adding accounts in Docker](#adding-accounts-in-docker).

```sh
cp .env.example .env   # then edit .env: TWITTER_AUTH_TOKEN, TWITTER_CSRF_TOKEN and your webhook

docker compose up -d --build
docker compose logs -f
docker compose stop
```

- `TWITTER_AUTH_TOKEN` and `TWITTER_CSRF_TOKEN` are the `auth_token` and `ct0` cookie values (see [Getting `auth_token` and `ct0`](#getting-auth_token-and-ct0)).
- No `chown` and no `user:` are needed, whoever creates `./config` (you, or Docker when it is missing). The container starts as root and, before it opens the config, switches to the owner of the existing config file, falling back to the owner of the `/config` directory when the file does not exist or root owns it, and to 65532:65532 when root owns the directory too. If root owns the directory, it also hands the directory, and the `.lock`, history database and `.tmp-*` files in it that root owns (and the config file itself when root owns it), over to that user. So a `./config` that an earlier `user: "1000:1000"` run filled keeps working even if the directory is now owned by root. If the directory belongs to another non-root user than the config file, it follows the config file's owner and does not chown the directory, so the start can still fail with "cannot open lock file" unless that user can write there. The first log line shows the result (`running as 65532:65532 owning /config`), and the process never runs as root afterwards. This needs the default `CHOWN`, `SETUID` and `SETGID` capabilities.
- If you set `user:` (or `docker run --user`) to a non-root user, nothing is switched or chowned, and that UID must be able to write `./config`; otherwise the start fails with "cannot open lock file ... Permission denied". Files you place in `./config` yourself must end up readable by the user the container switches to: files owned by root are handed over only when root owns the directory.
- Mount the directory, not the file: the config is saved through a temporary file and a rename, and the lock and the history database live next to it.
- Inside a container, `localhost` is the container itself. For a webhook on the host, use a URL such as `http://host.docker.internal:8080/endpoint`. On Linux, the host service must listen on an address reachable from Docker, such as the bridge IP or `0.0.0.0`.
- After editing `.env`, run `docker compose up -d` to recreate the container with the new values; `restart` alone does not reload them.
- The default command is `listen`; other commands replace it, e.g. `docker compose run --rm angelic-angel status`. Stop the running container first (`docker compose stop`): a config is locked by one process at a time, and that one process covers all accounts in it.

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
| `TWITTER_AUTH_TOKEN` | The `auth_token` cookie of the `default` account. Set it together with `TWITTER_CSRF_TOKEN` |
| `TWITTER_CSRF_TOKEN` | The `ct0` cookie of the `default` account. Set it together with `TWITTER_AUTH_TOKEN` |
| `WEBHOOK_ENDPOINT` | Webhook URL of the `default` account. A non-blank value takes precedence over the config file |
| `WEBHOOK_SECRET_KEY` | Shared secret used to sign the `default` account's webhook requests. Required when a webhook URL is set |
| `ANGELIC_ANGEL_CONFIG` | Config file path. The image sets `/config/angelic-angel.toml`; `--config` takes precedence |

- **`TWITTER_AUTH_TOKEN`, `TWITTER_CSRF_TOKEN`, `WEBHOOK_ENDPOINT` and `WEBHOOK_SECRET_KEY` apply to the `default` account only.** Any other account takes its cookies, endpoint and secret from the config file (or the WebUI), so an account is never registered by mistake with cookies meant for another one. `ANGELIC_ANGEL_CONFIG` is about the file and is not tied to an account.
- The two credentials are a pair. When both are set (non-blank) they are used instead of `[accounts.twitter]` of `default` in the config file; when neither is, the config file is used; **when only one is set, the command fails with an error** (it never falls back to the file).
- `serve` applies the same rule at startup: with only one of the two set, it fails to start. The variables are read once, when the process starts. While both are set, the `default` account's cookies cannot be changed in the WebUI and the account cannot be deleted from it (409 `credentials_from_environment` and `account_from_environment`); to change or remove them, change the variables and restart the process.
- The credential values are never written to the config file. They are also used when a registration is renewed after AutoPush invalidates it (this can happen days later), so keep them set for as long as the container runs. `status` shows that they come from the environment without printing the values.

### First start and stored state

- If the `default` account has no registration and both credentials are set, `listen` registers it with AutoPush and X, saves the registration, and starts receiving. When the config has no `default` account yet, it is created implicitly, so the environment-only setup needs no config file. An existing config file keeps its other settings (webhook, notification filter, `[accounts.twitter]`, and any other accounts); a missing one is created with mode `0600`. Before contacting X it checks that the config can be saved, so a read-only mount fails without leaving registrations behind on X's side.
- The registration (UAID and keys) of each account is stored in `./config/angelic-angel.toml`, and the notification history of `default` in `./config/angelic-angel.toml.notifications.sqlite3` (other accounts: `./config/angelic-angel.toml.<id>.notifications.sqlite3`). Keep `./config` across restarts; treat it like a credential.
- If the automatic registration fails (for example because the cookies are no longer valid), the error is logged and the process waits 30 seconds before exiting with a non-zero code, so that `restart: unless-stopped` does not hit X's login endpoints in a tight loop. `docker stop` (SIGINT) ends the wait immediately. Errors found before contacting X (half-set credentials, an unwritable `./config`) exit at once.
- Without credentials and without a registration, `listen` exits with "No registration found" and the restart policy keeps retrying. Set the credentials, or place a registered config file in `./config`.
- With several accounts, an account that fails stops alone and the others keep receiving; the process exits non-zero (and the restart policy applies) only when every account has failed. A non-`default` account without a registration is skipped with a warning.

### Changing the credentials

```sh
docker compose stop
docker compose run --rm angelic-angel unregister
sudo rm config/angelic-angel.toml     # then edit .env with the new values
docker compose up -d
```

`unregister` removes the AutoPush subscription of the `default` account (pass `--account <id>` for another one); deleting the config file makes the next start register again with the new credentials. This also drops the webhook and filter stored in that file, **and every other account in it**, along with their registrations. The history databases are kept.

To change the cookies of only one account, use the WebUI: unregister that account, edit its cookies in the Settings screen (cookies cannot be changed while registered) and register it again. The other accounts are not affected. This does not apply to `default` while the cookie environment variables are set: it takes its cookies from them, so change `.env` and restart instead.

### Adding accounts in Docker

The environment variables describe the `default` account only; the Docker setup with environment variables alone is unchanged and keeps working as a single `default` account. To receive for another account, either:

- put it in the mounted config file as an `[[accounts]]` entry with its own `[accounts.twitter]`, `webhook_endpoint` and `webhook_secret`, as in [Multiple accounts](#multiple-accounts), or
- add it from the WebUI (`angelic-angel serve`) against the same config directory. The config is locked by one process at a time, so the container must be stopped first.

An account other than `default` that has no registration is skipped with a warning at `listen`; register it with `docker compose run --rm angelic-angel register --account <id>` (with the container stopped) or from the WebUI. One container then receives for all of them.

### Running more than one instance

Two different things are easy to confuse:

- **Several accounts in one process: supported.** One `listen` (or `serve`) process receives for every account in the config. The lock on the config covers all accounts, so this is the way to receive for more than one account.
- **The same registration (config file) in two processes: do not.** If two containers each register automatically from the environment, they get different UAIDs, so the same account's notifications arrive twice and your webhook receives every one of them twice. Run a single container per config directory.

### Without environment credentials

You can also bring your own config file instead. Place a registered `angelic-angel.toml` in `./config` (ownership as described above), or create it in the container:

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

The runtime uses Distroless with a stripped binary, without a shell, sources or build caches. `scripts/docker-smoke.sh` verifies configuration persistence, `ANGELIC_ANGEL_CONFIG`, the multi-account layout (`status` of an `[[accounts]]` config, the `.legacy.bak` copy made when a single-account file is converted, environment variables together with `[[accounts]]`, and no secret values in the output), permissions, switching to the owner of bind-mounted directories (root-owned and owned by another UID) and an explicit `--user`, expected errors (including a failed automatic registration, which waits 30 seconds) and SIGINT shutdown with dummy credentials and networking disabled. It pulls a separate BusyBox helper if needed and removes its own test resources.

To debug the image, use `docker logs` and run CLI subcommands (e.g. `docker run --rm IMAGE status`) rather than `exec`ing a shell. Published images can be pulled from GHCR: `docker pull ghcr.io/<owner>/<repo>:latest`.

### WebUI development

`bun dev` from the repository root builds the receiver (`cargo build --locked`), then runs two child processes: `angelic-angel serve` (the API on `127.0.0.1:8080`) and the Vite dev server for `web/` (`http://localhost:11475/`, which proxies `/api` and `/health` to the API). Install the frontend dependencies once with `bun install --cwd web --frozen-lockfile`.

- It uses `angelic-angel.toml` in the repository root. `serve` starts the real receiver when that config already holds a registration, so point `DEV_CONFIG` at a scratch file (for example `DEV_CONFIG=.cache/dev/angelic-angel.toml bun dev`) when you only want to try the UI. The launcher prints the config in use.
- The WebUI is local-only and needs no authentication or login: it opens straight onto the three screens (a config without accounts shows an empty state; add an account from the header). The `.lock` file is stored next to the config and is git-ignored.
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
