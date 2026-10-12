# Personal operations WebUI

## Approved scope

The user chose React + Vite with a Rust backend, three screens (settings,
registration/unregistration, receiver status), integration with Docker and
CI/CD, and merging once CI passes. Keep the existing CLI usable and the runtime
lightweight. This is a single-account personal tool.

## Architecture

- Add `serve --bind ADDRESS` to the Rust CLI. The native default is
  `127.0.0.1:8080`; Docker runs it on `0.0.0.0:8080` and Compose publishes only
  `127.0.0.1:${WEBUI_PORT:-8080}:8080`.
- One Rust process owns the HTTP server, receiver task and configuration. It
  starts the receiver automatically when registration is present. Existing
  `listen`, `init`, `register`, `unregister` and `status` remain available.
- React + TypeScript is built with Vite. Rust serves the built assets from a
  configured directory (Docker: `/usr/share/angelic-angel/web`), avoiding a second
  server, Node runtime or a build dependency between Cargo and generated assets.
  API routes never fall back to the SPA. Missing assets return a clear error.
- Use hash navigation for the three screens to avoid server route coupling.
  Development uses Vite's `/api` proxy to the Rust server.

## Authentication and data

- All `/api` requests require an administrator Bearer token. There is no cookie
  authentication and no permissive CORS. Mutating requests are JSON; reject a
  browser Origin that does not match the request Host.
- `WEBUI_TOKEN` optionally supplies a token of at least 16 printable ASCII
  characters. Otherwise generate a cryptographically random persistent token
  beside the config, with owner-only permissions. `web-token` prints the active
  token to the operator's terminal. Do not log the token or put it in a URL.
- A login panel gates the three screens. Keep the token in sessionStorage only;
  logout and unauthorized responses clear it. Do not prefill saved X cookies.
- API DTOs expose only whether `auth_token` and `ct0` are set. Never serialize
  stored cookies, private WebPush keys, raw provider errors or notification
  contents to the browser. Display safe, bounded error messages.
- Add optional `webhook_endpoint` to the existing TOML schema; old configs still
  parse. A nonempty `WEBHOOK_ENDPOINT` environment value retains precedence.
  The UI identifies this source and disables editing the URL while overridden.
- Save TOML atomically with mode 0600 on Unix. All mutating CLI commands, standalone
  `listen` and `serve` take an exclusive config ownership lock. Read-only commands
  may run while the server owns the config. A competing writer fails clearly.
- Inside `serve`, serialize mutations and stop/join the receiver before modifying
  registration or credentials. Restart only from the committed configuration.
  Automatic re-registration must not overwrite a subsequent UI edit. Preserve
  counters during controlled restarts. Credential changes while registered are
  rejected with an instruction to unregister first; webhook-only changes are
  allowed. On remote failure preserve the previous registration and restore the
  receiver. Bound remote operations with timeouts and perform best-effort cleanup
  if registration partially succeeds. Shutdown cancels work and stops the owned
  receiver without leaving detached writers.

## HTTP contract

All data requests are same-origin, authenticated. Failures use
`{"error":{"code":"stable_code","message":"safe message"}}` with an appropriate
HTTP status (401 unauthorized, 400 invalid data, 409 conflicting operation or
registered credential change, 502 provider failure, 500 persistence failure).
Use `Cache-Control: no-store` for API responses. Limit request bodies to 16 KiB.

- `GET /api/settings`:
  `{configured, auth_token_set, ct0_set, webhook_endpoint, webhook_source,
  credentials_locked}`. Booleans are boolean; endpoint is string or null;
  source is `environment`, `config` or `none`.
- `PUT /api/settings`: `{auth_token?: string|null, ct0?: string|null,
  webhook_endpoint: string}`. Null/blank credentials retain the saved value;
  first setup requires both. Empty URL clears the configured endpoint. Validate
  nonempty credential values for length/control characters and HTTP(S) URLs.
  With an environment override, retain the stored webhook value instead of
  silently copying the environment to disk. Return the settings DTO.
- `GET /api/registration`: `{registered, uaid, channel_id}`. IDs are string or null.
- `POST /api/register`: register using the saved cookies; already registered is
  an idempotent success. Return the registration DTO.
- `DELETE /api/registration`: unregister; already unregistered is an idempotent
  success. Return the registration DTO.
- `GET /api/status`: `{state, registered, connected_since, last_notification_at,
  received_count, forwarded_count, failed_count, retry_count, last_error}`.
  State is `unconfigured`, `unregistered`, `stopped`, `connecting`, `connected`,
  `retrying`, `registering`, `unregistering` or `error`. Timestamps are Unix
  seconds or null; counters are numbers; last_error is a safe string or null.
  Poll every two seconds while visible, without overlapping requests.
- `GET /health`: unauthenticated coarse liveness only, no config or status data.

## Screens

Japanese, responsive and keyboard accessible. Use a restrained dark navy theme
with violet accents, good contrast, clear headings and visible focus states.

1. **Settings:** write-only cookie inputs, stored/not-stored indicators, webhook
   URL and source, validation, save feedback. Explain blank means unchanged.
2. **Registration:** registered/not-registered indicator, safe IDs, register and
   unregister actions; confirm unregister inline before sending. Show loading,
   failure and retry states. Disable conflicting actions while pending.
3. **Receiver status:** truthful connection/retry state, connected/last-received
   times, received/forwarded/failed counts, retry count and safe last error. Empty
   states must explain when settings or registration are missing.

## Delivery and validation

- Docker uses a build-platform Node 22 stage for `npm ci` and Vite build; copy only
  compiled assets and the stripped Rust binary into the existing Distroless
  runtime. Preserve UID 65532, mode-0700 `/data`, CA roots, OpenSSL and SIGINT.
- CI retains all existing Rust/commit checks and adds locked frontend install,
  TypeScript, lint, component tests and production build. Docker smoke verifies
  the CLI plus authenticated HTTP/API and static assets with dummy credentials,
  including restart persistence and shutdown. All external notification traffic
  in tests is mocked or disabled; do not use real X credentials.
- Native/backend tests cover secret redaction, auth/origin/body rejection,
  settings validation and old config migration, atomic ownership and lifecycle
  races, provider failure/timeout behavior, accurate connection and forwarding
  counters. Provider non-2xx is a forwarding failure, not success.
- Browser verification exercises login, all screens, initial settings save,
  validation and server errors, credential masking, logout, reload, and narrow
  layout. Preserve screenshots; clean up only owned test processes/containers.
- Independent Claude diff review and, when access remains verified, LocalGPT Pro
  review precede completion. Native Codex verifies actual results, creates a PR,
  waits for all checks at its exact SHA, and merges through guarded Git while
  preserving the opted-in commit identity policy.
