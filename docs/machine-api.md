# Machine API v1

The optional machine listener runs alongside `serve`, not `listen`. It is a separate router and TCP listener from the existing local WebUI/management API. Never expose the unauthenticated management port to a Bot or an untrusted network. Existing notification reception, registration, configuration precedence and CLI defaults are unchanged.

## Configuration and startup

Set the following environment variables in the deployment's secret/configuration mechanism, then run:

```sh
angelic-angel --config /config/angelic-angel.toml serve
```

| Variable | Meaning |
| --- | --- |
| `MACHINE_SERVICE_TOKEN` | New independent high-entropy Bearer token, 32–1024 visible ASCII characters. Do not reuse an X Cookie or webhook secret. Absent disables the listener; empty/invalid is an error. |
| `MACHINE_BIND` | Optional numeric socket address, default `127.0.0.1:8081`. Set deliberately to expose the listener. IPv6 uses `[::1]:8081`. |
| `MACHINE_CLIENT_ID` | Required stable principal ID, 1–64 ASCII letters/digits/underscore/hyphen. Changing it creates a new idempotency namespace. |
| `MACHINE_ACCOUNTS` | Required comma-separated exact Angel account IDs; no whitespace or wildcard. |
| `MACHINE_OPERATIONS` | Required comma-separated subset of `read,identity,post,operations`; no implicit grants. |

**Posting is disabled per account by default.** In the local management UI, select the account, open Settings, enable “APIからの投稿・返信を許可” and save. The persisted `[[accounts]]` field is `machine_post_enabled = true`. Missing keys in existing or legacy files, newly created accounts and environment-only `default` accounts all mean OFF. No environment variable implicitly grants this permission. The management settings API can edit it; the machine API cannot. Reading account readiness/identity and existing operation results does not require this posting permission.

Use TLS at a trusted reverse proxy or a private tunnel for non-loopback traffic. Do not publish port 8080. Compose users must explicitly configure/publish the machine port; no new port is exposed by default. Keep the config directory on durable storage; the service must be able to create the ledger alongside its config file. Do not edit configuration outside Angel while `serve` is running; management operations share the machine dispatch lock, external file editors do not.

The listener starts only after its ledger and socket can be opened; failure aborts startup instead of silently serving without durable idempotency. Both listeners stop accepting requests on SIGINT/SIGTERM and drain within the existing shutdown grace period. Incomplete operations remain tombstones and become `unknown` after restart. No token is accepted through cookies, query parameters or the webhook signing mechanism. Responses use `Cache-Control: no-store`.

## Authentication and scope

Every request, including result queries, requires:

```http
Authorization: Bearer <machine-service-token>
```

Account IDs are stable Angel IDs, not screen names. The configured principal can see only explicitly permitted accounts. Each route also requires its operation scope. New posts/replies additionally require the account's saved `machine_post_enabled` permission: token authentication, account allowlist, `post` scope and account permission must all allow the request. Operation results are isolated by account and principal. Do not change the client ID or discard the ledger to retry a request: that defeats deduplication.

## Routes

All routes are under `/api/machine/v1` on the **machine listener only**.

| Method | Path | Scope |
| --- | --- | --- |
| GET | `/accounts` | `read` |
| GET | `/accounts/{id}/readiness` | `read` |
| GET | `/accounts/{id}/identity` | `identity` |
| POST | `/accounts/{id}/posts` | `post` |
| GET | `/accounts/{id}/operations/{operation_id}` | `operations` |

Other paths return 404. Management routes, static files, arbitrary URLs, raw GraphQL and secret retrieval are not mounted.

### Readiness and identity

Account listing and readiness are cache-only and never contact X/GitHub. They return `id`, `registered`, `receiver_state`, `credentials_configured`, opaque `credential_revision`, `metadata_ready`, `post_ready`, and only implemented `capabilities` (`identity`, `text_post`, `reply`). Push registration is **not** proof of valid X credentials or posting readiness. `post_ready`, `capabilities.text_post` and `capabilities.reply` are true only when the token has the `post` scope, the account permission is ON, credentials are locally configured and cached metadata/signature material is fresh. They are false when posting is OFF; `identity` remains filtered by its own scope. Readiness is not a live credential validation; the post preflight always verifies the actual user.

Identity explicitly contacts X with that account's saved credentials, verifies a minimal response and warms posting metadata. It returns:

```json
{
  "account_id": "default",
  "credential_revision": "opaque-uuid",
  "identity": {
    "user_id": "123",
    "screen_name": "fixture",
    "name": "Fixture",
    "avatar_url": null
  }
}
```

Only HTTPS avatar URLs on `pbs.twimg.com/profile_images/` without credentials, query or fragment are returned. A missing/invalid upstream identity or required metadata produces a fixed safe error; no fallback account is selected. An invalid avatar is omitted, not proxied.

The revision is a persisted random UUID, not a public Cookie hash. Saved credential changes and account recreation produce new revisions. Environment credentials apply only to `default`; changing them across restarts also rotates the ledger revision. Old TOML remains readable and receives an internal revision on its next normal save. An environment-only account needs no TOML entry. Deleting an account does **not** delete the operations ledger.

### Text post or reply

```http
POST /api/machine/v1/accounts/default/posts
Authorization: Bearer <machine-service-token>
Content-Type: application/json
Idempotency-Key: unique-stable-client-operation-key
```

```json
{
  "text": "Example text",
  "replyToPostId": "456",
  "expectedCredentialRevision": "revision-returned-by-identity",
  "expectedXUserId": "123"
}
```

`replyToPostId` is optional. Other fields, including media, quote, URL overrides and feature overrides, are rejected. Text must be nonblank, at most 280 Unicode scalar values and 4096 UTF-8 bytes; this conservative validation does not implement X's weighted URL/emoji rules, so X can still reject text. IDs must be 1–20 ASCII digits. The idempotency key is required, 1–128 visible ASCII characters. Body limit is 16 KiB.

For a new key, Angel checks the account permission under its per-account lock before reserving any operation or contacting X. OFF returns 403 `account_post_disabled` without reservation or remote calls. Immediately before the durable dispatch marker it rechecks permission under the same lock; a reserved operation whose permission is no longer available becomes a persistent `failed` result and is never sent.

Before dispatch Angel checks the expected revision, confirms the actual immutable X user ID, and obtains validated dynamic `CreateTweet` query ID/features from a fixed public metadata URL. Catalogue variables and headers are never forwarded. The existing Rust transaction signer is reused. Anonymous bootstrap requests and signed session requests disallow cross-origin redirects; catalogue redirects are rejected entirely. Public metadata is not fetched with Cookie headers. Missing metadata fails closed. Required metadata is cached for at most six hours and fetched only on explicit identity/post requests.

The per-account lock is held from credential snapshot through the final result save. Concurrent management credential/permission updates, unregistration or deletion cannot overtake preflight or dispatch; they report 409 `busy`. An OFF save is effective only once it succeeds, and the UI retains unsaved changes after a failed save. A successful OFF save prevents subsequent new dispatches; it does not cancel work that already acquired the lock or recall an already-started send. Retry the management save after the in-flight operation finishes. A deleted previously resolved service handle cannot recreate an account. A fresh post preflight has a 30-second deadline; the dispatch itself has a separate 30-second deadline. No automatic retry is performed.

### Durable result and idempotency

Successful HTTP handling returns an operation object (inspect `state`, not just HTTP 200):

```json
{
  "operation_id": "generated-uuid",
  "state": "succeeded",
  "post_id": "789",
  "error": null
}
```

States are `accepted`, `dispatching`, `succeeded`, `failed`, `unknown`. `failed` means preflight prevented dispatch. Failures after the durable dispatch marker, including HTTP/GraphQL errors, malformed responses, author mismatch, timeouts and response loss, conservatively become `unknown`. Upstream error text is not returned or persisted. Each operation records only its request hash, account/principal/key/revision, state and minimal result/error code; never its text, Cookie, token or raw upstream body.

The private SQLite ledger is `<config-path>.operations.sqlite3` (plus SQLite sidecars), separate from notification history. Existing permissive/symlink ledger paths are rejected. WAL and `synchronous=FULL` are required. Reserve and dispatch markers must be committed before X is called. If reservation/marker storage fails, X is not called. If success persistence fails, the caller receives `unknown` with `result_storage_failed`; the durable `dispatching` tombstone remains and must not be replayed.

The uniqueness key is `(Angel account ID, MACHINE_CLIENT_ID, Idempotency-Key)`. Identical requests return the saved operation without sending again, even after posting is disabled. A request waiting for the account lock rechecks for an existing operation before checking the posting permission. Result queries also remain available while OFF; different content (including expected revision/user) returns 409. Simultaneous identical keys have one dispatch winner. Result queries return the same minimal operation; during an in-flight request or a failed result save they can show `dispatching`. After restart all `accepted`/`dispatching` rows become `unknown`, never queued work. Once reserved, even failed/unknown keys never dispatch again. A result can be queried after account deletion if the principal still has its account scope.

Keys/tombstones have **no expiration or automatic pruning**. Back up the config directory and ledger/sidecars using a consistent SQLite backup method; never remove them as a retry mechanism. Storage grows with unique operations. Exactly-once delivery cannot be promised for this unofficial remote API: `unknown` requires human/out-of-band reconciliation, not a new key or automatic resend. Lookup is not implemented in this version.

## Errors and unsupported operations

Errors contain fixed codes under `error.code`. Typical HTTP statuses are 401 (authentication), 403 (scope or disabled account posting), 404 (missing account/result/path), 400 (invalid request), 409 (revision/key conflict), 415 (JSON required), 413 (body limit), 502/504 (identity/metadata failure), and 503 (durable storage unavailable). Preflight failure after reservation is represented as an operation in state `failed` with `preflight_failed`. This deliberately does not expose raw X errors.

Post lookup, generic profile lookup, likes, follows, media and quote posts are not implemented or advertised. The Bot still needs its existing mechanisms for those operations. No real-X compatibility or live delivery test is implied by synthetic/mock tests.
