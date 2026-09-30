# ingest-api

This is the Phase 2 backend. It handles device login, presigned upload URLs, session and
segment metadata, the game blocklist, and data deletion. It is built with axum, sqlx on
Postgres, and object_store for S3/R2/Garage.

```bash
DATABASE_URL=postgres://... S3_ENDPOINT=https://<acct>.r2.cloudflarestorage.com S3_REGION=auto \
S3_BUCKET=gameplay S3_ACCESS_KEY=... S3_SECRET_KEY=... \
AUTH_PROVIDER=oauth2 OAUTH_DEVICE_AUTHORIZATION_URL=... OAUTH_TOKEN_URL=... OAUTH_CLIENT_ID=... \
BIND_ADDR=0.0.0.0:8080 cargo run -p ingest-api
```

`ingest-api --help` lists every setting. Migrations in `migrations/` run at startup. Queries
are checked at runtime, so building does not need `DATABASE_URL`.

## Endpoints

Errors are always `{"error": "<code>", "error_description": "..."}`. The client wire types
live in `cap_upload::api`.

| Endpoint | Auth | Body | Response |
|---|---|---|---|
| `POST /auth/device` | none | `{"device_name"}` | Starts login: `{device_code, user_code, verification_uri, expires_in, interval}` |
| | | `{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code","device_name"}` | Poll: `400 authorization_pending` / `slow_down` / `access_denied` / `expired_token`, or `{access_token, expires_in, refresh_token, refresh_expires_in, user_id, device_id}` |
| | | `{"grant_type":"refresh_token","refresh_token"}` | New token pair. The old refresh token is revoked (rotation) |
| `GET /config` | none | | `{blocklist, allowlist, default_deny, allowed_encoders, rate_hz, width, height, min_client_version, consent_version}` |
| `POST /sessions` | bearer | `{session_id?, game_id, client_version, consent_version}` | `{session_id, user_id, key_prefix}`. Idempotent. `403 game_blocked`/`game_not_allowed`/`client_too_old`, `422 consent_required`, `409 session_exists` |
| `POST /sessions/{id}/segments/{n}/upload` | bearer | `{files:[{name,size,blake3}]}` (must include `manifest.json`) | `{expires_in, files:[{name,key,url,method:"PUT"}]}` |
| `POST /sessions/{id}/segments/{n}/complete` | bearer | `{files:[{name,size,blake3}], dropped_frames, frame_count}` | The server HEADs every object and compares sizes, then returns `{segment_id, state:"ready"}`. Returns `400 object_missing`/`size_mismatch`/`files_changed` |
| `DELETE /me/data` | bearer | | `202 {deletion_id, segments, status:"pending"}` |

Object keys follow `raw/<user_id>/<session_id>/seg_<nnnnnn>/<file>`, where `user_id` is the
`users.id` UUID and `seg_<nnnnnn>` is `cap_types::segment_dir_name`.

Auth tokens are random 256-bit strings (`gca_…` for access, 1 h by default, and `gcr_…` for
refresh, 30 d by default). Only their SHA-256 hash is stored in `tokens`. The login provider
is pluggable through `auth::DeviceAuthProvider`:

- `AUTH_PROVIDER=dev` approves every login as `DEV_SUBJECT`. It refuses to run unless bound to
  loopback.
- `AUTH_PROVIDER=oauth2` implements the generic RFC 8628 device flow. Set
  `OAUTH_DEVICE_AUTHORIZATION_URL`, `OAUTH_TOKEN_URL`, `OAUTH_CLIENT_ID`, and optionally
  `OAUTH_CLIENT_SECRET`, `OAUTH_SCOPE`, and `OAUTH_USERINFO_URL`. Identity comes from the
  userinfo endpoint (`sub`, or `id` for GitHub), or from the `id_token` if no userinfo URL is
  set.

Set `S3_PUBLIC_ENDPOINT` when clients reach storage at a different URL than the server does.
Presigned URLs are signed for that endpoint.

## Deletion, and how the shard pipeline consumes it

`DELETE /me/data` runs one transaction that does three things:

1. It inserts `deletions(id, user_id, status='pending')`.
2. It freezes the user's current segments into `deletion_segments(deletion_id, segment_id,
   session_id, segment_idx, key_prefix)`.
3. It sets those segments to `state='deletion_pending'`. They are no longer "ready", so no new
   shard picks them up.

A background worker in this service (`deletion::run_worker`) takes pending deletions with
`FOR UPDATE SKIP LOCKED`. For each one it deletes every object under each `key_prefix`, sets
the segments to `deleted`, and sets the deletion to `status='raw_deleted'` with
`raw_deleted_at` and `objects_deleted`. Failures are recorded in `attempts`/`last_error` and
retried. If a client PUTs after deletion, its `complete` call gets `409 segment_deleted` and
those objects are removed again.

The shard pipeline on the training box owns the next step:

```sql
-- 1. deletions whose raw data is gone but whose shards are not rebuilt yet
SELECT d.id, ds.session_id, ds.segment_idx, ds.key_prefix
FROM deletions d JOIN deletion_segments ds ON ds.deletion_id = d.id
WHERE d.status = 'raw_deleted' AND d.shards_rebuilt_at IS NULL;
```

2. Match those `key_prefix` values (or `(session_id, segment_idx)`) against each shard's
   source-segment sidecar under `shards/<dataset_version>/`.
3. Rebuild each affected shard without the deleted segments, or drop it, and upload the
   replacement. Then delete the old shard objects.
4. `UPDATE deletions SET status = 'done', shards_rebuilt_at = now() WHERE id = $1`.

When the pipeline lists new work, it should only take `segments.state = 'ready'`. That way
nothing that is `deletion_pending` or `deleted` is ever sharded again.

## Testing

`tests/e2e.rs` runs the whole flow in-process against real Postgres and Garage, using
cap-upload's `Target::Presigned`. The steps are: device login, config, bad-token retry, a
blocked game ending up `failed`, upload and complete of 2 segments, a tampered complete, then
`DELETE /me/data` and a check that the objects are gone.

```bash
docker run -d --rm --name gcap-e2e-pg -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=ingest -p 127.0.0.1::5432 postgres:16-alpine
# plus a local Garage (deploy/garage/README.md, "Local test instance") and a key for the server
INGEST_E2E_DATABASE_URL=postgres://postgres:pw@127.0.0.1:<port>/ingest \
GARAGE_TEST_ENDPOINT=http://127.0.0.1:39000 GARAGE_TEST_KEY_ID=GK... GARAGE_TEST_SECRET=... \
  cargo test -p ingest-api --test e2e
docker rm -f gcap-e2e-pg
```
