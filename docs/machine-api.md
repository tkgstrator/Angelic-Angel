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
| `MACHINE_ACCOUNTS` | Comma-separated exact Angel account IDs; no whitespace or wildcard. Required unless `MACHINE_OPERATIONS` contains `accounts`, in which case it may be empty or omitted. |
| `MACHINE_OPERATIONS` | Required comma-separated subset of `read,identity,post,operations,like,follow,accounts`; no implicit grants. The values `like` and `follow` are separate from `post`, and `accounts` (account management, v0.7) is separate from all of them; the existing values keep their meaning. |

**Posting, liking and following are each disabled per account by default.** In the local management UI, select the account, open Settings, enable the switches you want in the “Machine API” section and save:

| Switch in Settings | `[[accounts]]` field and management settings JSON key | Allows |
| --- | --- | --- |
| APIからの投稿・返信を許可 | `machine_post_enabled` | text posts and replies |
| APIからのいいねを許可 | `machine_like_enabled` | likes |
| APIからのフォローを許可 | `machine_follow_enabled` | follows |

The three switches are independent and default to OFF. Missing keys in existing or legacy files, newly created accounts and environment-only `default` accounts all mean OFF, so an existing deployment does not gain likes or follows when it is upgraded. In the management settings API an omitted key keeps the saved value. No environment variable implicitly grants any of these permissions. The management settings API can edit them; the machine API cannot. Reading account readiness/identity, existing operation results and post/user lookups does not require any of these account permissions.

Use TLS at a trusted reverse proxy or a private tunnel for non-loopback traffic. Do not publish port 8080. Compose users must explicitly configure/publish the machine port; no new port is exposed by default. Keep the config directory on durable storage; the service must be able to create the ledger alongside its config file. Do not edit configuration outside Angel while `serve` is running; management operations share the machine dispatch lock, external file editors do not.

The listener starts only after its ledger and socket can be opened; failure aborts startup instead of silently serving without durable idempotency. Both listeners stop accepting requests on SIGINT/SIGTERM and drain within the existing shutdown grace period. Incomplete operations remain tombstones and become `unknown` after restart. No token is accepted through cookies, query parameters or the webhook signing mechanism. Responses use `Cache-Control: no-store`.

## Authentication and scope

Every request, including result queries, requires:

```http
Authorization: Bearer <machine-service-token>
```

Account IDs are stable Angel IDs, not screen names. The configured principal can see only explicitly permitted accounts: the static `MACHINE_ACCOUNTS` allowlist plus, since v0.7, the accounts it created itself through `POST /accounts` (see "Account management"). Each route also requires its operation scope. New posts/replies, likes and follows additionally require the matching saved account permission. The effective permission is the conjunction of all of these:

> token authentication AND account allowlist AND route scope AND account permission

| Operation | Scope | Account permission |
| --- | --- | --- |
| Post / reply | `post` | `machine_post_enabled` |
| Like | `like` | `machine_like_enabled` |
| Follow | `follow` | `machine_follow_enabled` |
| Post lookup, user lookup | `read` | none |
| Accounts, readiness | `read` | none |
| Identity | `identity` | none |
| Operation result | `operations` | none |
| Verify Cookies, create / update / delete an account | `accounts` | none (only accounts this principal owns) |

Granting one scope or one switch never grants another: `like` in `MACHINE_OPERATIONS` does nothing for an account whose `machine_like_enabled` is OFF, and an ON switch does nothing for a token without the scope. Operation results are isolated by account and principal. Do not change the client ID or discard the ledger to retry a request: that defeats deduplication.

## Routes

All routes are under `/api/machine/v1` on the **machine listener only**.

| Method | Path | Scope |
| --- | --- | --- |
| GET | `/accounts` | `read` |
| GET | `/accounts/{id}/readiness` | `read` |
| GET | `/accounts/{id}/identity` | `identity` |
| POST | `/accounts/{id}/posts` | `post` |
| GET | `/accounts/{id}/operations/{operation_id}` | `operations` |
| GET | `/accounts/{id}/posts/{post_id}` | `read` |
| GET | `/accounts/{id}/users/{user_id}` | `read` |
| POST | `/accounts/{id}/likes` | `like` |
| POST | `/accounts/{id}/follows` | `follow` |
| POST | `/credentials/verify` | `accounts` |
| POST | `/accounts` | `accounts` |
| PUT | `/accounts/{id}/credentials` | `accounts` (owned accounts only) |
| PUT | `/accounts/{id}/webhook` | `accounts` (owned accounts only) |
| PUT | `/accounts/{id}/permissions` | `accounts` (owned accounts only) |
| DELETE | `/accounts/{id}` | `accounts` (owned accounts only) |

Other paths return 404. Management routes, static files, arbitrary URLs, raw GraphQL and secret retrieval are not mounted.

### Readiness and identity

Account listing and readiness are cache-only and never contact X/GitHub. They return `id`, `registered`, `receiver_state`, `credentials_configured`, opaque `credential_revision`, `x_user_id` (the X user ID the Cookies were verified against when the account was created through the API; `null` for every other account), `permissions` (`{post, like, follow}`: the saved per-account switches, reported whatever the token scopes are), `metadata_ready`, `post_ready`, and only implemented `capabilities` (`identity`, `text_post`, `reply`, `lookup_post`, `lookup_user`, `like`, `follow`). Push registration is **not** proof of valid X credentials or posting readiness. `post_ready`, `capabilities.text_post` and `capabilities.reply` are true only when the token has the `post` scope, the account permission is ON, credentials are locally configured and cached metadata/signature material is fresh. They are false when posting is OFF; `identity` remains filtered by its own scope.

`lookup_post`, `lookup_user`, `like` and `follow` are reported under the same cache-only readiness rule: they reflect the token's scope (`read` for both lookups, `like`, `follow`) and, for `like` and `follow`, the account's `machine_like_enabled` / `machine_follow_enabled` permission. None of them contacts X, and none proves that X will accept the operation. Readiness is not a live credential validation; the post preflight always verifies the actual user.

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
  "kind": "post",
  "state": "succeeded",
  "post_id": "789",
  "error": null
}
```

`kind` is `post`, `like` or `follow`. Operations stored by v0.5.0 have no `kind` column value and are reported as `post`. Likes and follows carry their outcome in an additional `result` object (see below), which is absent for posts.

States are `accepted`, `dispatching`, `succeeded`, `failed`, `unknown`. `failed` means preflight prevented dispatch. Failures after the durable dispatch marker, including HTTP/GraphQL errors, malformed responses, author mismatch, timeouts and response loss, conservatively become `unknown`. Upstream error text is not returned or persisted. Each operation records only its request hash, account/principal/key/revision, state and minimal result/error code; never its text, Cookie, token or raw upstream body.

The private SQLite ledger is `<config-path>.operations.sqlite3` (plus SQLite sidecars), separate from notification history. Existing permissive/symlink ledger paths are rejected. WAL and `synchronous=FULL` are required. Reserve and dispatch markers must be committed before X is called. If reservation/marker storage fails, X is not called. If success persistence fails, the caller receives `unknown` with `result_storage_failed`; the durable `dispatching` tombstone remains and must not be replayed.

The uniqueness key is `(Angel account ID, MACHINE_CLIENT_ID, Idempotency-Key)`. Identical requests return the saved operation without sending again, even after posting is disabled. A request waiting for the account lock rechecks for an existing operation before checking the posting permission. Result queries also remain available while OFF; different content (including expected revision/user) returns 409. Simultaneous identical keys have one dispatch winner. Result queries return the same minimal operation; during an in-flight request or a failed result save they can show `dispatching`. After restart all `accepted`/`dispatching` rows become `unknown`, never queued work. Once reserved, even failed/unknown keys never dispatch again. A result can be queried after account deletion if the principal still has its account scope.

Keys/tombstones have **no expiration or automatic pruning**. Back up the config directory and ledger/sidecars using a consistent SQLite backup method; never remove them as a retry mechanism. Storage grows with unique operations. Exactly-once delivery cannot be promised for this unofficial remote API: `unknown` requires human/out-of-band reconciliation, not a new key or automatic resend. Likes and follows are recorded under the same uniqueness key and `unknown` rules (see below); post and user lookups are not recorded at all.

### Post and user lookup

```http
GET /api/machine/v1/accounts/default/posts/456
GET /api/machine/v1/accounts/default/users/321
Authorization: Bearer <machine-service-token>
```

Both routes need the `read` scope and the account allowlist; they need **no** account permission switch, so they work while posting, liking and following are OFF. IDs are numeric X IDs (as in the post route), not screen names. Lookups are read-only requests made on demand: they take no `Idempotency-Key`, create no operation, and are **not recorded in the ledger**. They are not part of the idempotency or `unknown` rules below.

A post lookup returns a `PostInfo` (camelCase):

```json
{
  "id": "456",
  "text": "Example text",
  "authorId": "123",
  "authorScreenName": "fixture",
  "conversationId": "450",
  "createdAt": "2026-10-18T03:04:05Z",
  "replyToPostId": "450",
  "replyToUserId": "321",
  "quotedPostId": null,
  "quotedAuthorId": null
}
```

A user lookup returns a `UserProfile`:

```json
{
  "userId": "321",
  "screenName": "other",
  "name": "Other",
  "description": "Example profile",
  "createdAt": "2020-01-02T03:04:05Z",
  "automatedLabel": null,
  "defaultProfileImage": false,
  "isProtected": false,
  "followersCount": 10,
  "followingCount": 20,
  "postsCount": 30,
  "relation": {
    "blockedBy": false,
    "blocking": false,
    "followedBy": true,
    "following": false,
    "followRequestSent": null
  }
}
```

**`null` means “X did not say”; it is never turned into `false`, `0` or a guess from the text.** Every optional field above can be `null`, including each flag in `relation` (`blockedBy`, `blocking`, `followedBy`, `following`, `followRequestSent`: the authenticated account's relation to that user). In particular `automatedLabel` is `true` only when the profile explicitly says it is an automated account; a missing label is `null`, never `false`, because the absence of a label is no evidence either way. `createdAt` is UTC ISO 8601 and is `null` when X's timestamp could not be parsed. `replyToUserId` is filled only when it is consistent with what X reported for the reply; `quotedAuthorId` is filled only when the response embeds the quoted post and its ID matches `quotedPostId`. Treat `null` as “unknown” in client logic, not as “no”.

### Like

```http
POST /api/machine/v1/accounts/default/likes
Authorization: Bearer <machine-service-token>
Content-Type: application/json
Idempotency-Key: unique-stable-client-operation-key
```

```json
{
  "postId": "456",
  "expectedCredentialRevision": "revision-returned-by-identity",
  "expectedXUserId": "123"
}
```

Requires the `like` scope, the account allowlist and the account's `machine_like_enabled` permission. When the permission is OFF the request is refused with 403 `account_like_disabled`. The response is the same operation object as for posts, with `kind: "like"`, and `post_id` is always `null`. A successful like carries a `LikeResult` in `result`; `result` is absent in every other state:

```json
{
  "operation_id": "generated-uuid",
  "kind": "like",
  "state": "succeeded",
  "post_id": null,
  "error": null,
  "result": {
    "postId": "456",
    "alreadyLiked": false
  }
}
```

**A post that was already liked is a success**: the result has `alreadyLiked: true` and the same successful state. The post is liked either way, so a client should treat both as “liked” and must not turn `alreadyLiked` into an error or retry.

### Follow

```http
POST /api/machine/v1/accounts/default/follows
Authorization: Bearer <machine-service-token>
Content-Type: application/json
Idempotency-Key: unique-stable-client-operation-key
```

```json
{
  "userId": "321",
  "expectedCredentialRevision": "revision-returned-by-identity",
  "expectedXUserId": "123"
}
```

Requires the `follow` scope, the account allowlist and the account's `machine_follow_enabled` permission. When the permission is OFF the request is refused with 403 `account_follow_disabled`. The response is the same operation object, with `kind: "follow"` and `post_id: null`. A successful follow carries a `FollowResult` in `result`:

```json
{
  "operation_id": "generated-uuid",
  "kind": "follow",
  "state": "succeeded",
  "post_id": null,
  "error": null,
  "result": {
    "userId": "321",
    "following": true,
    "followRequestSent": false
  }
}
```

`following` and `followRequestSent` are `null` when X did not report them (see the `null` rule above). **Protected accounts use approval**: following one can leave `following` false (or `null`) and `followRequestSent` true. That is a success of the request, not of the follow: the request is pending until the other user approves, and you must not record the account as followed. Use a user lookup (`relation.following`, `relation.followRequestSent`) later to see the outcome.

### Likes and follows: idempotency, `unknown` and in-flight saves

Likes and follows are state-changing and use the same durable ledger rules as posts: the uniqueness key is `(Angel account ID, MACHINE_CLIENT_ID, Idempotency-Key)`, an identical request returns the saved operation instead of sending again, and changed content under the same key is a 409. After the durable dispatch marker, a failure whose outcome cannot be known (timeout, response loss, malformed response) becomes `unknown`. **Do not resend an `unknown` like or follow, with the same or a new key, as an automatic retry.** Inspect the operation result and reconcile out of band, for example with a user lookup for a follow, and only then decide.

Saving the account's Settings returns 409 `busy` while a post, like or follow is being processed. Switching a permission OFF is effective only once the save succeeds; it stops later new requests but cannot cancel or recall work that has already started. The UI keeps the unsaved switch after a failed save so it can be retried once the work has finished.

### Not verified against X

The post/user lookups, likes and follows are tested only with synthetic or mock X responses. Their behavior against the real X service has **not** been verified, and this unofficial remote API can change or reject requests without notice. Keep the like and follow switches OFF until you need them.

## Account management (scope `accounts`, v0.7)

A client with the `accounts` scope can hand X Cookies to Angel once and never store them itself. The Cookies are **write-only**: no response or error of these routes contains them and the new code logs none of them (registration reuses the existing code path). The webhook signing secret is write-only as well. The management UI on port 8080 is unchanged (still unauthenticated and local-only) and can still edit these accounts.

### Configuration example

```sh
MACHINE_SERVICE_TOKEN=<new random token, 32-1024 visible ASCII>
MACHINE_CLIENT_ID=bot
MACHINE_OPERATIONS=read,identity,post,operations,like,follow,accounts
# MACHINE_ACCOUNTS may be left out when "accounts" is listed above; list IDs only to expose
# accounts that were created in the WebUI (they are visible but not manageable here).
```

### Ownership and visibility

- An account created by `POST /accounts` stores `machine_owner = <MACHINE_CLIENT_ID>` and `x_user_id` in its `[[accounts]]` entry. Accounts from the WebUI, from the process environment, from an older Angel version or from another client have no / another owner. Do not edit these keys by hand.
- A principal sees (`GET /accounts`, readiness, identity, posts, lookups, likes, follows, operation results) the allowlisted accounts and the accounts it owns. Routes 2-6 below act **only on owned accounts**: another client's account, an unowned account and a missing account all give the same 404 `account_not_found`. An existing id is never taken over: creating it again is 409 `account_exists`.
- When the principal holds the `accounts` scope, an account outside its view is reported as 404 `account_not_found` on every account route, so that `GET /accounts/{id}/readiness` can be used to find out whether a `POST /accounts` whose result was lost took effect. Without the `accounts` scope the existing 403 `scope_denied` is unchanged.
- The `accounts` scope missing is always 403 `scope_denied`, checked before any ownership lookup.
- Requests for one account are serialized by the same per-account lock that protects posts, likes and follows. Routes 3-6 take it right after the request is parsed and hold it until they finish, including across the X identity check (up to 30 seconds), so a concurrent request for the same account waits. Route 2 has no account to lock yet: it checks the id and the Cookies first, then creates the account (the config lock rejects a second create of the same id with 409 `account_exists`), and registers it while holding the new account's lock.

All bodies must be `application/json` (415 `json_required` otherwise), are limited to 16 KiB (413) and reject unknown fields (400 `invalid_request`). Validation failures never repeat the submitted value. Cookies (`authToken`, `ct0`): 1-1024 bytes of visible ASCII (0x21-0x7E) except `;`. Webhook `endpoint`: no surrounding whitespace or control characters, at most 2048 bytes, and **https**, or http only to `localhost`, a loopback address or `host.docker.internal` (the rule the webhook sender enforces). Webhook `secret`: not blank, at most 4096 bytes, no control characters, kept exactly as sent because the webhook signature is computed over it.

### 1. `POST /credentials/verify`: check Cookies without storing anything

```json
{ "authToken": "...", "ct0": "..." }
```

200: `{ "identity": { "userId": "123", "screenName": "fixture", "name": "Fixture", "avatarUrl": "https://pbs.twimg.com/profile_images/1/abc.jpg" } }`. `avatarUrl` is present only for an HTTPS `pbs.twimg.com/profile_images/` URL, and the `_normal`, `_bigger` and `_mini` size suffix is removed so it points at the original image. Nothing is saved, registered or recorded. Errors: 422 `credentials_rejected` (X answered 401/403), 502 `x_unavailable`, 504 `x_timeout`, 400 `invalid_request`.

### 2. `POST /accounts`: create an account

```json
{
  "id": "0123456789abcdef0123456789abcdef",
  "authToken": "...",
  "ct0": "...",
  "expectedXUserId": "123",
  "webhookEndpoint": "https://bot.example/webhooks/angelic/0123456789abcdef0123456789abcdef",
  "webhookSecret": "...",
  "permissions": { "post": true, "like": true, "follow": true }
}
```

`id` follows the Angel account ID rule (`^[a-z0-9][a-z0-9_-]{0,31}$`; a 32-digit hex ID fits). `permissions` is optional and each key defaults to `false`. Angel (a) refuses an existing id, (b) asks X who the Cookies belong to and requires the user ID to equal `expectedXUserId`, (c) saves the Cookies, webhook, permissions, `x_user_id` and `machine_owner` (the config lock makes the second of two racing creates of one id fail with 409 `account_exists`), and (d) registers for notifications (AutoPush plus X push registration, the same code path as the WebUI's register) and starts the receiver, holding the new account's lock from (d) on. Any failure after (c) deletes the new account again. If the client disconnects while a create is running (the X check and each registration step can take up to 30 seconds), the request may stop between (c) and the end of (d) without the rollback: the account then exists but may be unregistered, so check `GET /accounts/{id}/readiness` and `DELETE /accounts/{id}` or repeat the cookies update.

201:

```json
{
  "id": "0123456789abcdef0123456789abcdef",
  "credentialRevision": "opaque-uuid",
  "identity": { "userId": "123", "screenName": "fixture", "name": "Fixture" },
  "registered": true,
  "receiverState": "connecting",
  "permissions": { "post": true, "like": true, "follow": true }
}
```

Use `credentialRevision` and `identity.userId` as `expectedCredentialRevision` and `expectedXUserId` of later posts, likes and follows. Errors: 400 `invalid_request`, 409 `account_exists`, 409 `identity_mismatch`, 422 `credentials_rejected`, 502 `x_unavailable`, 504 `x_timeout`, 502 `registration_failed` (registration failed; the account was rolled back), 500 `rollback_failed` (registration failed **and** the rollback also failed: the account may still exist, call `DELETE /accounts/{id}` to clean up). Any other failure while saving or registering (for example 503 `storage_unavailable`) is also rolled back first. There is no `Idempotency-Key`: duplicates are prevented by the unique id. If the result of a create is unknown, ask `GET /accounts/{id}/readiness`: 200 means it exists (it belongs to you), 404 means it does not.

### 3. `PUT /accounts/{id}/credentials`: replace the Cookies

```json
{ "authToken": "...", "ct0": "...", "expectedXUserId": "123" }
```

Holding the account lock, Angel checks the new Cookies with X and requires the user ID to equal both `expectedXUserId` **and** the saved `x_user_id` (otherwise 409 `identity_mismatch` and nothing changes), then unregisters, saves the Cookies and registers again. 200: `{ "credentialRevision", "identity", "registered": true }`. The revision changes when the Cookies change and stays the same when the same Cookies are sent again (then nothing is unregistered or saved; the account is only made sure to be registered), so a client that sees a new revision must treat approvals bound to the old one as expired. Errors: 400, 404, 409 `identity_mismatch`, 422 `credentials_rejected`, 502 `x_unavailable`, 504 `x_timeout`, 409 `busy` (a bulk notification job of the management UI is running on the account), 502 `registration_failed`.

On 502 `registration_failed` nothing is lost: if the old registration could not be removed, the old Cookies and registration are still in place and the old receiver keeps running; if the new registration failed, the **new Cookies are saved and the account is unregistered**, which readiness reports as `registered: false`. Sending the same request again completes it. The account is never deleted by this route.

### 4. `PUT /accounts/{id}/webhook`: change the forwarding target

```json
{ "endpoint": "https://bot.example/webhooks/angelic/...", "secret": "..." }
```

200: `{ "updated": true }`. Both fields are required and replace the stored values. Cookies, `credential_revision`, the registration and the receiver are untouched. Notifications are signed with the new secret from the next delivery on.

### 5. `PUT /accounts/{id}/permissions`: post, like and follow switches

```json
{ "post": true, "like": false, "follow": true }
```

Every key is optional; a key left out keeps its saved value, `{}` changes nothing. 200: `{ "permissions": { "post", "like", "follow" } }` with the saved values. The revision does not change. These are the same `machine_post_enabled`, `machine_like_enabled` and `machine_follow_enabled` switches the WebUI edits. The effective permission is still the conjunction of token, allowlist or ownership, operation scope and this switch.

### 6. `DELETE /accounts/{id}`: remove an account

204 with no body. Angel unregisters the push subscription, stops the receiver and removes the account entry and its notification history, like the WebUI's delete. If the push service refuses the unregistration the account is kept with its receiver restored and the answer is 502 `unregistration_failed`; repeat the request later. The **operation ledger is kept**: results and idempotency keys of the deleted id are never removed. Because ownership is read from the account entry, an owned account's results can no longer be fetched through the API after its deletion (they are fetchable again if the same client re-creates the same id); the ledger rows themselves stay on disk.

### Error codes added in v0.7

`invalid_request` (400), `scope_denied` (403), `account_not_found` (404), `account_exists` (409), `identity_mismatch` (409), `busy` (409), `history_cleanup_required` (409: leftover notification history files of an earlier account with this id exist next to the config file; remove them manually), `credentials_rejected` (422), `x_unavailable` (502), `x_timeout` (504), `registration_failed` (502), `unregistration_failed` (502), `rollback_failed` (500), `history_cleanup_failed` (500: the account was removed but its history files could not be deleted), `accounts_unavailable` / `credentials_unavailable` / `storage_unavailable` (503).

### How a Bot stops storing Cookies

1. The operator sets `MACHINE_OPERATIONS` with `accounts` (and the other scopes the Bot needs) and gives the Bot the service token. No Cookie is configured in the Bot.
2. In the Bot's screen the user pastes the Cookies. The Bot calls `POST /credentials/verify`, shows the returned identity and, once confirmed, generates a 32-digit hex account ID and a random webhook secret, then calls `POST /accounts` with the same Cookies, `expectedXUserId` and its own webhook URL. The Cookies only pass through the Bot's memory.
3. The Bot keeps the ID, `credentialRevision`, the X user ID, screen name and avatar, and its (encrypted) webhook secret, but **no Cookie**. If saving that row fails it calls `DELETE /accounts/{id}`.
4. Posts, lookups, likes and follows use the account ID with `expectedCredentialRevision` and `expectedXUserId`; notifications arrive at the webhook as before; nothing needs to be registered by hand.
5. When the Cookies expire the user pastes new ones and the Bot calls `PUT /accounts/{id}/credentials`. A changed `credentialRevision` expires approvals bound to the old one.
6. To remove the account the Bot calls `DELETE /accounts/{id}`.

### Not verified against X

Account management was implemented and tested only with synthetic/mock X and AutoPush backends. Its behavior against the real X service has **not** been verified: the X Cookie identity check, push registration and re-registration were not exercised end to end. The change was written without a Rust toolchain being available, so formatting, lint and test results come from CI, not from the author's machine. Keep the `accounts` scope off until you need it.

## Errors and unsupported operations

Errors contain fixed codes under `error.code`. Typical HTTP statuses are 401 (authentication), 403 (scope or a disabled account permission), 404 (missing account/result/path), 400 (invalid request), 409 (revision/key conflict), 415 (JSON required), 413 (body limit), 502/504 (identity/metadata failure), and 503 (durable storage unavailable). Preflight failure after reservation is represented as an operation in state `failed` with `preflight_failed`. This deliberately does not expose raw X errors.

Account management adds the codes listed under "Error codes added in v0.7". A disabled account permission returns a fixed 403 code: `account_post_disabled`, `account_like_disabled` or `account_follow_disabled`. A missing scope or an account outside the allowlist is also 403, with the code `scope_denied`.

Media, quote posts and other operations are not implemented or advertised; the Bot still needs its existing mechanisms for them. The Bot itself still depends on its own X Cookie for whatever it has not moved to this API; migrating it is separate work. No real-X compatibility or live delivery test is implied by synthetic/mock tests.
