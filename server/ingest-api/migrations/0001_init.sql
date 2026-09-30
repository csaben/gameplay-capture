-- ingest-api schema. Applied at startup by sqlx::migrate!.

CREATE TABLE users (
    id          UUID PRIMARY KEY,
    -- Auth provider name ("dev", "oauth2:<issuer>") and its stable subject id.
    provider    TEXT NOT NULL,
    subject     TEXT NOT NULL,
    email       TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, subject)
);

CREATE TABLE devices (
    id            UUID PRIMARY KEY,
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, name)
);

-- Bearer tokens. Only the SHA-256 of the token is stored.
CREATE TABLE tokens (
    token_hash  BYTEA PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('access', 'refresh')),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id   UUID REFERENCES devices(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at  TIMESTAMPTZ
);
CREATE INDEX tokens_expires_at ON tokens (expires_at);

CREATE TABLE consents (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    version      TEXT NOT NULL,
    accepted_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    device_id    UUID REFERENCES devices(id) ON DELETE SET NULL,
    PRIMARY KEY (user_id, version)
);

-- Game allow/block list served by GET /config. `id` is the exe name or bundle id.
CREATE TABLE games (
    id          TEXT PRIMARY KEY,
    status      TEXT NOT NULL CHECK (status IN ('allowed', 'blocked')),
    publisher   TEXT,
    notes       TEXT,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id               TEXT PRIMARY KEY,
    user_id          UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id        UUID REFERENCES devices(id) ON DELETE SET NULL,
    game_id          TEXT NOT NULL,
    client_version   TEXT NOT NULL,
    consent_version  TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user ON sessions (user_id);

-- uploading: presigned URLs issued, not confirmed yet
-- ready: server HEADed every object, sizes match; eligible for sharding
-- deletion_pending / deleted: user asked for deletion (see `deletions`)
CREATE TABLE segments (
    id              UUID PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    segment_idx     INTEGER NOT NULL CHECK (segment_idx >= 0),
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- raw/<user_id>/<session_id>/seg_<nnnnnn>
    key_prefix      TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('uploading', 'ready', 'deletion_pending', 'deleted')),
    sizes           JSONB NOT NULL DEFAULT '{}'::jsonb,   -- file name -> bytes
    hashes          JSONB NOT NULL DEFAULT '{}'::jsonb,   -- file name -> blake3 hex
    total_bytes     BIGINT,
    dropped_frames  BIGINT,
    frame_count     INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at    TIMESTAMPTZ,
    deleted_at      TIMESTAMPTZ,
    UNIQUE (session_id, segment_idx)
);
CREATE INDEX segments_user_state ON segments (user_id, state);
CREATE INDEX segments_ready ON segments (state, completed_at);

-- DELETE /me/data. Lifecycle:
--   pending      -> queued; the ingest-api worker deletes the raw objects
--   raw_deleted  -> raw objects gone; shard pipeline must rebuild affected shards
--   done         -> shard pipeline rebuilt every shard listing one of the
--                   segments in deletion_segments and set shards_rebuilt_at
CREATE TABLE deletions (
    id                 UUID PRIMARY KEY,
    user_id            UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status             TEXT NOT NULL CHECK (status IN ('pending', 'raw_deleted', 'done')),
    requested_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    raw_deleted_at     TIMESTAMPTZ,
    shards_rebuilt_at  TIMESTAMPTZ,
    objects_deleted    BIGINT NOT NULL DEFAULT 0,
    attempts           INTEGER NOT NULL DEFAULT 0,
    last_error         TEXT
);
CREATE INDEX deletions_status ON deletions (status, requested_at);

-- The segments a deletion covers, frozen at request time. The shard pipeline
-- matches (session_id, segment_idx) / key_prefix against each shard's
-- source-segment sidecar.
CREATE TABLE deletion_segments (
    deletion_id  UUID NOT NULL REFERENCES deletions(id) ON DELETE CASCADE,
    segment_id   UUID NOT NULL,
    session_id   TEXT NOT NULL,
    segment_idx  INTEGER NOT NULL,
    key_prefix   TEXT NOT NULL,
    PRIMARY KEY (deletion_id, segment_id)
);
CREATE INDEX deletion_segments_key ON deletion_segments (session_id, segment_idx);
