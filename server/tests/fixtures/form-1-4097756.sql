CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    backend BLOB NOT NULL CHECK (length(backend) = 16),
    restore INTEGER NOT NULL CHECK (restore >= 0),
    changes INTEGER NOT NULL CHECK (changes >= 0),
    made_at INTEGER NOT NULL,
    written_at INTEGER NOT NULL,
    restored_at INTEGER,
    restored_from INTEGER,
    music_public TEXT,
    diagnostics_default INTEGER CHECK (diagnostics_default IN (0, 1))
) STRICT;
CREATE TABLE accounts (
    number INTEGER PRIMARY KEY CHECK (number > 0),
    email TEXT NOT NULL UNIQUE,
    shown TEXT NOT NULL,
    password BLOB CHECK (password IS NULL OR length(password) = 32),
    password_salt BLOB CHECK (password_salt IS NULL OR length(password_salt) = 16),
    password_memory INTEGER,
    password_passes INTEGER,
    password_lanes INTEGER,
    life INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('on', 'off')),
    password_changes INTEGER NOT NULL,
    reset_required INTEGER NOT NULL CHECK (reset_required IN (0, 1)),
    code_number INTEGER CHECK (code_number IS NULL OR code_number >= 0),
    code_used INTEGER NOT NULL CHECK (code_used IN (0, 1)),
    code_expires INTEGER CHECK ((code_number IS NULL) = (code_expires IS NULL)),
    admin INTEGER NOT NULL CHECK (admin IN (0, 1)),
    music_login INTEGER NOT NULL,
    music_managed INTEGER NOT NULL CHECK (music_managed IN (0, 1)),
    music_recovery TEXT CHECK (music_recovery IN ('rotate', 'enter')),
    history_held INTEGER NOT NULL CHECK (history_held IN (0, 1)),
    revision INTEGER NOT NULL,
    CHECK ((password IS NULL) = (password_salt IS NULL))
) STRICT;
CREATE TABLE codes (
    account INTEGER NOT NULL REFERENCES accounts (number) ON DELETE CASCADE,
    number INTEGER NOT NULL CHECK (number >= 0),
    code BLOB NOT NULL UNIQUE CHECK (length(code) = 32),
    made_at INTEGER NOT NULL,
    until INTEGER NOT NULL,
    PRIMARY KEY (account, number)
) STRICT, WITHOUT ROWID;
CREATE TABLE devices (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    account INTEGER NOT NULL REFERENCES accounts (number) ON DELETE CASCADE,
    secret BLOB NOT NULL CHECK (length(secret) = 32),
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('pc', 'phone')),
    version TEXT NOT NULL,
    first_signed_in INTEGER NOT NULL,
    last_heard INTEGER,
    number INTEGER NOT NULL CHECK (number >= 0),
    UNIQUE (account, secret)
) STRICT;
CREATE INDEX devices_by_account ON devices (account);
CREATE TABLE sessions (
    device INTEGER PRIMARY KEY REFERENCES devices (id) ON DELETE CASCADE,
    id INTEGER NOT NULL CHECK (id > 0),
    hash BLOB NOT NULL UNIQUE CHECK (length(hash) = 32),
    last_used INTEGER NOT NULL,
    sign_out BLOB NOT NULL CHECK (length(sign_out) = 32),
    kind TEXT CHECK (kind IN ('app', 'page')),
    leave_for TEXT,
    leave_life INTEGER,
    leave_revision INTEGER,
    leave_admin INTEGER,
    leave_granted INTEGER,
    leave_serial INTEGER,
    leave_hash BLOB CHECK (leave_hash IS NULL OR length(leave_hash) = 32),
    CHECK ((leave_for IS NULL) = (leave_hash IS NULL))
) STRICT;
CREATE TABLE music_logins (
    account INTEGER PRIMARY KEY REFERENCES accounts (number) ON DELETE CASCADE,
    count INTEGER NOT NULL CHECK (count > 0),
    name TEXT NOT NULL,
    sealed BLOB NOT NULL,
    internal TEXT NOT NULL
) STRICT;
CREATE TABLE listening_events (
    row_id INTEGER PRIMARY KEY AUTOINCREMENT,
    account INTEGER NOT NULL REFERENCES accounts (number) ON DELETE CASCADE,
    device INTEGER NOT NULL,
    event_id TEXT NOT NULL CHECK (length(event_id) = 32),
    at_ms INTEGER NOT NULL,
    is_start INTEGER NOT NULL CHECK (is_start IN (0, 1)),
    line TEXT NOT NULL,
    UNIQUE (account, event_id)
) STRICT;
CREATE INDEX listening_recent ON listening_events (account, at_ms DESC, row_id DESC) WHERE is_start = 1;
CREATE TABLE listening_totals (
    account INTEGER PRIMARY KEY REFERENCES accounts (number) ON DELETE CASCADE,
    starts INTEGER NOT NULL
) STRICT;
CREATE TRIGGER listening_count_start AFTER INSERT ON listening_events
WHEN NEW.is_start = 1 BEGIN
    INSERT INTO listening_totals (account, starts) VALUES (NEW.account, 1)
    ON CONFLICT (account) DO UPDATE SET starts = starts + 1;
END;
CREATE TRIGGER listening_erase_life AFTER UPDATE OF life ON accounts
WHEN NEW.life > OLD.life AND NEW.history_held = 0 BEGIN
    DELETE FROM listening_events WHERE account = NEW.number;
    DELETE FROM listening_totals WHERE account = NEW.number;
END;
CREATE TABLE diagnostic_reports (
    row_id INTEGER PRIMARY KEY AUTOINCREMENT,
    account INTEGER NOT NULL REFERENCES accounts (number) ON DELETE CASCADE,
    device INTEGER NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    restore INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 8),
    received_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (device, restore, request_id)
) STRICT;
CREATE INDEX diagnostics_device ON diagnostic_reports (device, row_id DESC);
CREATE INDEX diagnostics_account ON diagnostic_reports (account, row_id DESC);
CREATE INDEX diagnostics_expiry ON diagnostic_reports (received_ms, row_id);
CREATE TABLE requests_jobs (
    id INTEGER PRIMARY KEY,
    restore INTEGER NOT NULL,
    account INTEGER NOT NULL,
    session INTEGER NOT NULL,
    request BLOB NOT NULL,
    change BLOB NOT NULL,
    work BLOB NOT NULL,
    stage INTEGER NOT NULL DEFAULT 0,
    result TEXT,
    UNIQUE (restore, account, session, request)
) STRICT;
CREATE INDEX requests_jobs_stage ON requests_jobs (restore, stage, id);
CREATE TABLE requests_owners (
    binding BLOB NOT NULL,
    account INTEGER NOT NULL REFERENCES accounts (number) ON DELETE CASCADE,
    release TEXT NOT NULL,
    asked_ms INTEGER NOT NULL,
    PRIMARY KEY (binding, account, release)
) STRICT, WITHOUT ROWID;
CREATE INDEX requests_owners_release ON requests_owners (binding, release, account);
CREATE TABLE requests_lines (
    job INTEGER NOT NULL REFERENCES requests_jobs (id) ON DELETE CASCADE,
    line INTEGER NOT NULL,
    progress TEXT NOT NULL,
    PRIMARY KEY (job, line)
) STRICT, WITHOUT ROWID;
CREATE TABLE requests_way_back (
    id INTEGER PRIMARY KEY,
    binding BLOB NOT NULL,
    artist INTEGER NOT NULL,
    artist_key TEXT NOT NULL,
    setting TEXT NOT NULL,
    UNIQUE (binding, artist)
) STRICT;
