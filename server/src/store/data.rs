//! The data (design 8, 13.4): one SQLite file, `NEXUS_DATA/nexus.db`.
//!
//! **Its settings, durability first:**
//! - `journal_mode = WAL`: a reader never waits for the one thread that writes, nor it for
//!   a reader, and the nightly copy reads it in use (13.4). The file SQLite writes ahead in,
//!   `nexus.db-wal`, is part of the data while the program runs; at a stop it is folded back
//!   in, so that at rest the data is the one file.
//! - `synchronous = FULL`: each step is on the disk before the next begins, so that nothing
//!   the backend has said it took is lost when the power goes. (`NORMAL` in WAL mode keeps
//!   the file whole but can lose the last steps, which a device would never send again.)
//! - `foreign_keys = ON`: a device's sessions, and an account's devices, go with them in the
//!   same step, and nothing points at what is gone.
//! - `busy_timeout = 5 s`: the one writer is never kept waiting by readers in WAL mode; this
//!   bounds the rare wait for the nightly copy's or a command's read to begin.
//! - `secure_delete = ON`: what is deleted (an account's history, 11.1) is written over with
//!   zeros in the file, not left in pages no longer used. (The design does not say; it is the
//!   stricter. What deleting does not reach is said in 11.1: the nightly copies.)
//! - `cell_size_check = ON`, `trusted_schema = OFF`: damage is found sooner, and nothing in
//!   the file is run.
//! - Every start runs SQLite's `quick_check` before anything is read for use.
//!
//! **Its form** is numbered in `user_version`, and the file is marked as the Nexus server's
//! own in `application_id`. A form newer than this program's is refused before anything else
//! is read (11.2). An older form is copied first, then changed in one step.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::collections::BTreeMap;

use nexus_core::nexus::compare::DataView;
use nexus_core::nexus::account::{AccountId, DeviceId, Email, SECRET_BYTES, SessionId};
use nexus_core::nexus::music::MusicInstallation;
use nexus_core::nexus::kept::{SessionKind, AccountState, DeviceRow, Fingerprint, Leave, LiveCode, MusicRecovery, Rows, Session, Status};
use nexus_core::nexus::proto::BackendId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use crate::store::crash;

/// The file's name inside `NEXUS_DATA`.
pub const FILE: &str = "nexus.db";
/// Where a new data file is made before it is renamed into place.
const MAKING: &str = "nexus.db.making-";
/// Where the owner's put-back stages the copy beside the data, checked, before it takes the
/// data's place (and where it waits, if the put-back is cut off, until it is finished).
pub const INCOMING: &str = "nexus.db.incoming";
/// "NXMS": the mark that the file is the Nexus server's data.
pub const APPLICATION_ID: i64 = 0x4e58_4d53;
/// The form this program writes and reads.
pub const FORM: i64 = 2;
/// How long a step waits for another connection's lock.
pub const BUSY: Duration = Duration::from_secs(5);

/// Form 1: what the data keeps (design 11): accounts (with C3r's state of each), one-time
/// codes as keyed fingerprints by the number C3r gives each (C3r's `LiveCode` deadline beside
/// its number), devices, and each device's one session as a fingerprint (with the fingerprint
/// of the secret that can only end it, 11); and the
/// restore number, how far its changes have got, and the latest time a step was written at.
/// For the slices that bring them, each `REFERENCES accounts (number) ON DELETE CASCADE`, so
/// that deleting an account takes them in the same step: an account's sealed music login,
/// bound to the internal address it was checked at (S9; 9.4: its count is the store's, so an older one
/// a put-back brings does not open), and the backend's music address people use
/// (`meta.music_public`); its listening events and their count (S12, 13), deleted in the same
/// step as the account's life moves on (a trigger) unless they are held (12.5); and its
/// Diagnostics reports (S7, 14); and for requests (S13, written by it through `write_data`):
/// their jobs and each job's lines, who asked for each release at which Lidarr (`binding`),
/// with what the history shows of it and whether their ask put it on the list (`placed`, once
/// set never cleared: only that gives the right to take it off, 16.1),
/// and what is kept to undo an artist's setting. The switch for requests and the Lidarr key
/// are authority, and are in the store kept apart alone. No
/// password, code or session is kept but as a fingerprint; a music login only sealed.
const FORM_1: &str = "
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
";

/// One change of form: from `form - 1` to `form`, as SQL run in one step.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub form: i64,
    pub sql: &'static str,
}

/// Every form this program knows, in order. A later slice adds its own at the end.
pub const MIGRATIONS: &[Migration] = &[
    Migration { form: 1, sql: FORM_1 },
    Migration { form: 2, sql: FORM_2 },
    Migration {
        form: 3,
        sql: "CREATE INDEX listening_by_time ON listening_events(at_ms DESC, row_id DESC) WHERE is_start = 1;",
    },
];

/// Form 2 (S13, for the audit's B-M1): the asked history keeps what it shows of each release
/// (`metadata`) and whether this person's ask put it on the list (`placed`, once set never
/// cleared). From form 1, as `4097756` made it: rows asked before have empty metadata and no
/// placement (so no one may take those off by their own right).
const FORM_2: &str = "
ALTER TABLE requests_owners ADD COLUMN metadata BLOB NOT NULL DEFAULT x'';
ALTER TABLE requests_owners ADD COLUMN placed INTEGER NOT NULL DEFAULT 0 CHECK (placed IN (0, 1));
CREATE TRIGGER requests_owners_placed_stays BEFORE UPDATE OF placed ON requests_owners
WHEN OLD.placed = 1 AND NEW.placed = 0 BEGIN
    SELECT RAISE(ABORT, 'a placement once recorded is never cleared');
END;
";

/// The data, open, with its settings set and its form checked.
pub struct Data {
    connection: Connection,
    path: PathBuf,
}

/// What opening found.
pub enum Opened {
    /// There is no data file.
    Absent,
    Open(Data),
}

/// A failed step, said without anything it held: what SQLite answered, by its kind.
pub fn why(error: &rusqlite::Error) -> String {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DiskFull) => "the disk is full".into(),
        Some(rusqlite::ErrorCode::ReadOnly) => "it can only be read".into(),
        Some(rusqlite::ErrorCode::CannotOpen) => "it can't be opened".into(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => "it is damaged".into(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => "it is in use".into(),
        Some(rusqlite::ErrorCode::SystemIoFailure) => "the disk failed to read or write".into(),
        Some(rusqlite::ErrorCode::ConstraintViolation) => "a rule of its form would be broken".into(),
        Some(code) => format!("SQLite said {code:?}"),
        None => "it was not what this program expects".into(),
    }
}

pub fn place(folder: &Path) -> PathBuf {
    folder.join(FILE)
}

/// Where a put-back's copy is staged: [`INCOMING`].
pub fn incoming(folder: &Path) -> PathBuf {
    folder.join(INCOMING)
}

fn number(value: u64) -> Result<i64, rusqlite::Error> {
    i64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))
}

fn unsigned(value: i64) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn settings(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(BUSY)?;
    let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(rusqlite::Error::InvalidQuery);
    }
    connection.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA secure_delete = ON;
         PRAGMA cell_size_check = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA journal_size_limit = 67108864;",
    )
}

/// The mark and form of an open file, read before anything else.
fn mark_and_form(connection: &Connection) -> rusqlite::Result<(i64, i64)> {
    let mark = connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let form = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok((mark, form))
}

/// Says whether a file is the Nexus server's data in a form this program reads: before
/// anything else is read from it.
fn check_form(connection: &Connection, what: &str, known: &[Migration]) -> Result<i64, String> {
    let newest = known.last().map_or(0, |migration| migration.form);
    let (mark, form) = mark_and_form(connection).map_err(|error| format!("{what} can't be read: {}.", why(&error)))?;
    if mark != APPLICATION_ID {
        return Err(format!("{what} is not the Nexus server's data. It is left as it is."));
    }
    if form > newest {
        return Err(format!(
            "{what} is in form {form}, made by a newer Nexus server than this one, which knows form {newest} at most. Nothing was read from it or changed: run the newer version again."
        ));
    }
    if form < 1 {
        return Err(format!("{what} is not the Nexus server's data. It is left as it is."));
    }
    Ok(form)
}

fn quick_check(connection: &Connection, what: &str) -> Result<(), String> {
    let mut statement = connection.prepare("PRAGMA quick_check").map_err(|error| format!("{what} can't be checked: {}.", why(&error)))?;
    let found: Vec<String> =
        statement.query_map([], |row| row.get(0)).and_then(|rows| rows.collect()).map_err(|error| format!("{what} can't be checked: {}.", why(&error)))?;
    if found == ["ok"] { Ok(()) } else { Err(format!("{what} is damaged (SQLite's check found {} faults).", found.len())) }
}

impl Data {
    /// Opens the data in `folder`, if there is a data file there: its mark and form checked
    /// first, then its settings set, its pages checked, and its form brought up to this
    /// program's (copied first, into `copies`, if it held anything).
    pub fn open(folder: &Path, now: u64) -> Result<Opened, String> {
        Self::open_with(folder, now, MIGRATIONS)
    }

    pub fn open_with(folder: &Path, now: u64, known: &[Migration]) -> Result<Opened, String> {
        let path = place(folder);
        match fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Opened::Absent),
            Err(error) => return Err(format!("The data at {} can't be read ({}).", path.display(), error.kind())),
            Ok(meta) if !meta.is_file() => return Err(format!("The data at {} is not a file.", path.display())),
            Ok(_) => {}
        }
        let what = format!("The data at {}", path.display());
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
        let connection = Connection::open_with_flags(&path, flags).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
        let form = check_form(&connection, &what, known)?;
        settings(&connection).map_err(|error| format!("{what} can't be opened for writing: {}.", why(&error)))?;
        quick_check(&connection, &what)?;
        let mut data = Data { connection, path };
        if form < known.last().map_or(0, |migration| migration.form) {
            data.migrate(folder, form, known, now)?;
        }
        Ok(Opened::Open(data))
    }

    /// Makes new data in `folder` for `backend`: made whole under a name of its own, made
    /// lasting, then renamed into place, so that a cut leaves no data file or a whole one.
    pub fn create(folder: &Path, backend: &BackendId, now: u64) -> Result<Data, String> {
        Self::create_with(folder, backend, now, MIGRATIONS)
    }

    pub fn create_with(folder: &Path, backend: &BackendId, now: u64, known: &[Migration]) -> Result<Data, String> {
        let making = folder.join(format!("{MAKING}{}", std::process::id()));
        let failed = |error: String| format!("The data couldn't be made in {}: {error}.", folder.display());
        let _ = fs::remove_file(&making);
        let made = (|| -> rusqlite::Result<()> {
            // Made empty and the owner's alone before SQLite writes a byte to it (finding 8).
            crate::store::private::create(&making).map_err(io_error)?;
            let mut connection = Connection::open(&making)?;
            connection.busy_timeout(BUSY)?;
            connection.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
            let step = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            for migration in known {
                step.execute_batch(migration.sql)?;
            }
            step.execute(
                "INSERT INTO meta (one, backend, restore, changes, made_at, written_at) VALUES (1, ?1, 0, 0, ?2, ?2)",
                params![backend.as_bytes().as_slice(), number(now)?],
            )?;
            step.execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID}; PRAGMA user_version = {};",
                known.last().map_or(0, |migration| migration.form)
            ))?;
            step.commit()?;
            connection.close().map_err(|(_, error)| error)
        })();
        if let Err(error) = made {
            let _ = fs::remove_file(&making);
            return Err(failed(why(&error)));
        }
        let placed = (|| -> std::io::Result<()> {
            fs::File::open(&making)?.sync_all()?;
            crash::point("data: made")?;
            fs::rename(&making, place(folder))?;
            crash::point("data: renamed")?;
            fs::File::open(folder)?.sync_all()
        })();
        if let Err(error) = placed {
            let _ = fs::remove_file(&making);
            return Err(failed(format!("{}", error.kind())));
        }
        match Self::open_with(folder, now, known)? {
            Opened::Open(data) => Ok(data),
            Opened::Absent => Err(failed("it was not there once made".into())),
        }
    }

    /// Copies the data first (11.2, and the backend's own copy an update needs to go back
    /// with), then brings its form forward a step at a time: each step one transaction, the
    /// form's number moved in it. A step that fails changes nothing and stops the start, naming
    /// the forms; steps before it stay done (each whole), and the next start goes on from there.
    fn migrate(&mut self, folder: &Path, form: i64, known: &[Migration], now: u64) -> Result<(), String> {
        let copy = crate::store::copies::before_form(folder, &self.connection, form, now)?;
        let what = format!("The data at {}", self.path.display());
        let to = forward(&mut self.connection, form, known).map_err(|(step, error)| {
            format!("{what} couldn't be brought from form {} to form {step}: {}. That step changed nothing; a copy made before is {}. Nothing is served.", step - 1, why(&error), copy.display())
        })?;
        println!("The data's form was brought forward from {form} to {to}; a copy made before is {}.", copy.display());
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The data's own form.
    pub fn form(&self) -> rusqlite::Result<i64> {
        self.connection.query_row("PRAGMA user_version", [], |row| row.get(0))
    }

    /// What the data says of itself.
    pub fn meta(&self) -> rusqlite::Result<Meta> {
        meta_of(&self.connection)
    }

    /// Every account's email, by number.
    pub fn names(&self) -> rusqlite::Result<BTreeMap<u64, Email>> {
        names_of(&self.connection)
    }

    /// What `compare` reads: every account's rows.
    pub fn view(&self) -> rusqlite::Result<DataView> {
        view_of(&self.connection)
    }

    /// An account's number, by its email (as tidied: C7's).
    pub fn account_named(&self, email: &str) -> rusqlite::Result<Option<u64>> {
        account_named_of(&self.connection, email)
    }

    /// An account's rows (C3r's `Rows`), as the data has them.
    pub fn rows(&self, meta: &Meta, number: u64) -> rusqlite::Result<Option<Rows>> {
        rows_of(&self.connection, meta, number)
    }

    /// An account's password, as kept, or `None` when it has none yet.
    pub fn password(&self, number: u64) -> rusqlite::Result<Option<Password>> {
        self.connection
            .query_row(
                "SELECT password, password_salt, password_memory, password_passes, password_lanes FROM accounts WHERE number = ?1 AND password IS NOT NULL",
                [self::number(number)?],
                |row| {
                    let hash: Vec<u8> = row.get(0)?;
                    let salt: Vec<u8> = row.get(1)?;
                    let bad = |at| rusqlite::Error::InvalidColumnType(at, "password".into(), rusqlite::types::Type::Blob);
                    let count = |at: usize| -> rusqlite::Result<u32> {
                        let value: i64 = row.get(at)?;
                        u32::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(at, value))
                    };
                    Ok(Password { hash: hash.try_into().map_err(|_| bad(0))?, salt: salt.try_into().map_err(|_| bad(1))?, memory: count(2)?, passes: count(3)?, lanes: count(4)? })
                },
            )
            .optional()
    }

    /// Does `changes` as one step: all of them, or (if any fails, or the program stops
    /// part-way) none; and counts the step (12.3's mark). The points it passes: `data: before
    /// commit`, `data: committed`. How far the data's changes have got, after it.
    pub fn apply(&mut self, changes: &[Change<'_>], written_at: u64) -> rusqlite::Result<u64> {
        let step = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for change in changes {
            change.apply(&step)?;
        }
        step.execute("UPDATE meta SET written_at = max(written_at, ?1), changes = changes + 1 WHERE one = 1", [number(written_at)?])?;
        let changes: i64 = step.query_row("SELECT changes FROM meta WHERE one = 1", [], |row| row.get(0))?;
        crash::point("data: before commit").map_err(io_error)?;
        step.commit()?;
        crash::point("data: committed").map_err(io_error)?;
        unsigned(changes)
    }

    /// Does `step` as one step of its own (another slice's rows: events, Diagnostics). If it
    /// says it changed something, the step is counted (12.3's mark) and committed at
    /// `written_at`; if not, nothing of it is kept. The points: `data: before commit`, `data:
    /// committed`. What it gave, and how far the data's changes have got if it changed any.
    pub fn apply_with<T>(&mut self, written_at: u64, step: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<(T, bool)>) -> rusqlite::Result<(T, Option<u64>)> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (value, changed) = step(&tx)?;
        if !changed {
            tx.rollback()?;
            return Ok((value, None));
        }
        tx.execute("UPDATE meta SET written_at = max(written_at, ?1), changes = changes + 1 WHERE one = 1", [number(written_at)?])?;
        let changes: i64 = tx.query_row("SELECT changes FROM meta WHERE one = 1", [], |row| row.get(0))?;
        crash::point("data: before commit").map_err(io_error)?;
        tx.commit()?;
        crash::point("data: committed").map_err(io_error)?;
        Ok((value, Some(unsigned(changes)?)))
    }

    /// Folds the file SQLite writes ahead in back into the data, so that the data is one file
    /// again. Done at a deletion and as the program stops; it fails harmlessly if a read is
    /// under way.
    pub fn fold(&self) -> rusqlite::Result<()> {
        self.connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
    }

    /// Closes the data, folded back into its one file if that can be done now.
    pub fn close(self) {
        let _ = self.fold();
        let _ = self.connection.close();
    }
}

// ---- Reading the data, on any connection to it (the data in use, or a copy to be put back) ----

/// What the data says of itself.
pub fn meta_of(connection: &Connection) -> rusqlite::Result<Meta> {
    connection.query_row("SELECT backend, restore, changes, made_at, written_at, restored_at, restored_from FROM meta WHERE one = 1", [], |row| {
        let backend: Vec<u8> = row.get(0)?;
        let backend: [u8; 16] = backend.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(0, "backend".into(), rusqlite::types::Type::Blob))?;
        let optional = |at: usize| -> rusqlite::Result<Option<u64>> { row.get::<_, Option<i64>>(at)?.map(unsigned).transpose() };
        Ok(Meta {
            backend: BackendId::from_bytes(backend),
            restore: unsigned(row.get(1)?)?,
            changes: unsigned(row.get(2)?)?,
            made_at: unsigned(row.get(3)?)?,
            written_at: unsigned(row.get(4)?)?,
            restored_at: optional(5)?,
            restored_from: optional(6)?,
        })
    })
}

/// Every account's email, by number.
pub fn names_of(connection: &Connection) -> rusqlite::Result<BTreeMap<u64, Email>> {
    let mut statement = connection.prepare("SELECT number, email FROM accounts")?;
    let found = statement.query_map([], |row| Ok((unsigned(row.get(0)?)?, row.get::<_, String>(1)?)))?;
    let mut names = BTreeMap::new();
    for found in found {
        let (number, email) = found?;
        let name = canonical(&email).ok_or_else(|| rusqlite::Error::InvalidColumnType(1, "email".into(), rusqlite::types::Type::Text))?;
        names.insert(number, name);
    }
    Ok(names)
}

/// What `compare` reads: every account's rows.
pub fn view_of(connection: &Connection) -> rusqlite::Result<DataView> {
    let meta = meta_of(connection)?;
    let mut view = DataView { restore: meta.restore, changes: meta.changes, ..Default::default() };
    let numbers: Vec<u64> = names_of(connection)?.into_keys().collect();
    for number in numbers {
        if let Some(rows) = rows_of(connection, &meta, number)? {
            view.accounts.insert(number, rows);
        }
    }
    Ok(view)
}

/// An account's number, by its email (as tidied: C7's).
pub fn account_named_of(connection: &Connection, email: &str) -> rusqlite::Result<Option<u64>> {
    connection.query_row("SELECT number FROM accounts WHERE email = ?1", [email], |row| unsigned(row.get(0)?)).optional()
}

/// An account's rows (C3r's `Rows`), as the data has them.
pub fn rows_of(connection: &Connection, meta: &Meta, number: u64) -> rusqlite::Result<Option<Rows>> {
    let found = connection
        .query_row(
            "SELECT email, life, status, password_changes, reset_required, code_number, code_used, admin, music_login, music_managed, music_recovery, history_held, revision, code_expires FROM accounts WHERE number = ?1",
            [self::number(number)?],
            |row| {
                let bad = |at: usize, what: &str| rusqlite::Error::InvalidColumnType(at, what.into(), rusqlite::types::Type::Text);
                let flag = |at: usize| -> rusqlite::Result<bool> { Ok(row.get::<_, i64>(at)? == 1) };
                let email = canonical(&row.get::<_, String>(0)?).ok_or_else(|| bad(0, "email"))?;
                let id = AccountId::new(number).ok_or_else(|| bad(0, "number"))?;
                let status = match row.get::<_, String>(2)?.as_str() {
                    "on" => Status::On,
                    "off" => Status::Off,
                    _ => return Err(bad(2, "status")),
                };
                let music_recovery = match row.get::<_, Option<String>>(10)?.as_deref() {
                    None => None,
                    Some("rotate") => Some(MusicRecovery::RotateManaged),
                    Some("enter") => Some(MusicRecovery::EnterAgain),
                    Some(_) => return Err(bad(10, "music_recovery")),
                };
                let code = match (row.get::<_, Option<i64>>(5)?.map(unsigned).transpose()?, row.get::<_, Option<i64>>(13)?.map(unsigned).transpose()?) {
                    (Some(number), Some(expires)) => Some(LiveCode { number, used: flag(6)?, expires }),
                    (None, None) => None,
                    _ => return Err(bad(13, "code_expires")),
                };
                Ok(AccountState {
                    backend: meta.backend,
                    restore: meta.restore,
                    id,
                    email,
                    life: unsigned(row.get(1)?)?,
                    status,
                    password_changes: unsigned(row.get(3)?)?,
                    password_reset_required: flag(4)?,
                    code,
                    admin: flag(7)?,
                    music_login: unsigned(row.get(8)?)?,
                    music_managed: flag(9)?,
                    music_recovery,
                    history_held: flag(11)?,
                    revision: unsigned(row.get(12)?)?,
                })
            },
        )
        .optional()?;
    let Some(account) = found else { return Ok(None) };
    let mut rows = Rows { account, devices: BTreeMap::new() };
    let mut statement = connection.prepare(
        "SELECT devices.id, devices.number, devices.secret, sessions.id, sessions.hash, sessions.last_used, sessions.sign_out, sessions.leave_for, sessions.leave_life, sessions.leave_revision, sessions.leave_admin, sessions.leave_granted, sessions.leave_serial, sessions.leave_hash, sessions.kind
         FROM devices LEFT JOIN sessions ON sessions.device = devices.id WHERE devices.account = ?1",
    )?;
    let found = statement.query_map([self::number(number)?], |row| {
        let blob = |at: usize| -> rusqlite::Result<[u8; SECRET_BYTES]> {
            let bytes: Vec<u8> = row.get(at)?;
            bytes.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(at, "fingerprint".into(), rusqlite::types::Type::Blob))
        };
        // A session row with no kind (written before the kind was kept) is an ended session:
        // it never defaults to an app's (the audit of core-fu, M3).
        let kind = match row.get::<_, Option<String>>(14)?.as_deref() {
            None => None,
            Some(word) => Some(unkind(word).ok_or_else(|| rusqlite::Error::InvalidColumnType(14, "kind".into(), rusqlite::types::Type::Text))?),
        };
        let session = match (row.get::<_, Option<i64>>(3)?, kind) {
            (None, _) | (_, None) => None,
            (Some(id), Some(kind)) => {
                let leave = match row.get::<_, Option<String>>(7)? {
                    None => None,
                    Some(purpose) => {
                        let intent = crate::store::apart::intent(&purpose).ok_or_else(|| rusqlite::Error::InvalidColumnType(7, "leave_for".into(), rusqlite::types::Type::Text))?;
                        let count = |at: usize| -> rusqlite::Result<u64> { unsigned(row.get::<_, Option<i64>>(at)?.unwrap_or(-1)) };
                        Some(Leave::stored(intent, count(8)?, count(9)?, count(10)? == 1, count(11)?, count(12)?, Fingerprint::from_bytes(blob(13)?)))
                    }
                };
                let id = SessionId::new(unsigned(id)?).ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(3, 0))?;
                Some(Session { id, kind, hash: Fingerprint::from_bytes(blob(4)?), last_used: unsigned(row.get(5)?)?, sign_out: Fingerprint::from_bytes(blob(6)?), leave })
            }
        };
        let id = DeviceId::new(unsigned(row.get(0)?)?).ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(0, 0))?;
        Ok((id, DeviceRow { number: unsigned(row.get(1)?)?, secret: Fingerprint::from_bytes(blob(2)?), session }))
    })?;
    for found in found {
        let (id, device) = found?;
        rows.devices.insert(id, device);
    }
    Ok(Some(rows))
}

fn io_error(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error { code: rusqlite::ErrorCode::DiskFull, extended_code: rusqlite::ffi::SQLITE_FULL },
        Some(format!("{}", error.kind())),
    )
}

/// What the data says of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub backend: BackendId,
    pub restore: u64,
    /// How far its changes have got: one more for every step.
    pub changes: u64,
    pub made_at: u64,
    /// The latest time any step was written at, on the backend's clock (its high-water mark
    /// at the time).
    pub written_at: u64,
    /// When it was last found older than the store, or put back, and the time of the last
    /// step the copy it came from had.
    pub restored_at: Option<u64>,
    pub restored_from: Option<u64>,
}

/// A password as kept (design 8.2): Argon2id's 32 bytes out, its salt, and its numbers.
#[derive(Clone, PartialEq, Eq)]
pub struct Password {
    pub hash: [u8; 32],
    pub salt: [u8; 16],
    pub memory: u32,
    pub passes: u32,
    pub lanes: u32,
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(..)")
    }
}

/// One change to the data, done in a step with others by [`Data::apply`]. Its `Debug` names
/// accounts and devices by number only: no email, name to show, device's name, code or
/// verifier (design 19.4; the review's finding 12).
#[derive(Clone)]
pub enum Change<'a> {
    /// A new account, with its first state (C3r's), its email and its name to show.
    Account { number: u64, shown: &'a str, state: &'a AccountState },
    /// An account's rows as C3r decided them: its state, each device's number and session.
    /// A device the rows do not have is removed; one the data does not have is added, to be
    /// described by [`Change::Describe`].
    Install { number: u64, rows: &'a Rows, at: u64 },
    /// A device's name, kind and app's version, as it signed in.
    Describe { device: u64, name: &'a str, kind: &'a str, version: &'a str },
    /// When a device was last heard from: kept in the data alone (12.3's mark shows it).
    Heard { device: u64, at: u64 },
    /// An account's whole history: its row and everything of it (12.5).
    Erase { account: u64 },
    /// An account's password (or none: it waits for one to be chosen).
    Password { account: u64, password: Option<&'a Password> },
    /// The live one-time code's keyed fingerprint, by the number C3r gave it, good until
    /// `until` (S8 makes it; 8.5). Any code of the account before it goes.
    Code { account: u64, number: u64, code: &'a [u8; 32], made_at: u64, until: u64 },
    /// Every one-time code of an account gone, but the live one (`keep`, by its number).
    Codes { account: u64, keep: Option<u64> },
    /// A new restore number, and when; every session ended (12.4 steps 1 and 2).
    Restore { restore: u64, at: u64, from: Option<u64> },
    /// An account's sealed music login, as kept decided it: its count, its name, the sealed
    /// bytes and the internal address (`NEXUS_MUSIC_SERVER`) it was checked at and sealed to
    /// (core-fu2: the public address is in no binding). Made by
    /// [`crate::store::keeper::Keeper::persist`] only.
    MusicLogin { account: u64, install: &'a MusicInstallation },
    /// The music server's address people use (9.3), backend-wide. Made by `persist` only.
    MusicPublic { address: &'a str },
    /// The admin's default for Diagnostics (14), backend-wide (`hello` says it). `persist` only.
    DiagnosticsDefault { on: bool },
}

impl std::fmt::Debug for Change<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Account { number, .. } => f.debug_struct("Account").field("number", number).finish_non_exhaustive(),
            Change::Install { number, rows, at } => {
                f.debug_struct("Install").field("number", number).field("devices", &rows.devices.keys().collect::<Vec<_>>()).field("at", at).finish_non_exhaustive()
            }
            Change::Describe { device, .. } => f.debug_struct("Describe").field("device", device).finish_non_exhaustive(),
            Change::Heard { device, at } => f.debug_struct("Heard").field("device", device).field("at", at).finish(),
            Change::Erase { account } => f.debug_struct("Erase").field("account", account).finish(),
            Change::Password { account, password } => f.debug_struct("Password").field("account", account).field("set", &password.is_some()).finish(),
            Change::Code { account, number, made_at, until, .. } => f.debug_struct("Code").field("account", account).field("number", number).field("made_at", made_at).field("until", until).finish_non_exhaustive(),
            Change::Codes { account, keep } => f.debug_struct("Codes").field("account", account).field("keep", keep).finish(),
            Change::Restore { restore, at, from } => f.debug_struct("Restore").field("restore", restore).field("at", at).field("from", from).finish(),
            Change::MusicLogin { account, install, .. } => f.debug_struct("MusicLogin").field("account", account).field("count", &install.binding.count).finish_non_exhaustive(),
            Change::MusicPublic { .. } => f.debug_struct("MusicPublic").finish_non_exhaustive(),
            Change::DiagnosticsDefault { on } => f.debug_struct("DiagnosticsDefault").field("on", on).finish(),
        }
    }
}

/// An email as the data keeps it: as C7 tidies one, and nothing else.
fn canonical(text: &str) -> Option<Email> {
    Email::new(text).filter(|email| email.as_str() == text)
}

fn kind_word(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::App => "app",
        SessionKind::AdminPage => "page",
    }
}

fn unkind(word: &str) -> Option<SessionKind> {
    match word {
        "app" => Some(SessionKind::App),
        "page" => Some(SessionKind::AdminPage),
        _ => None,
    }
}

fn status_word(status: Status) -> &'static str {
    // A deleted account has no row: its rows are erased instead.
    if status == Status::Off { "off" } else { "on" }
}

fn recovery_word(recovery: Option<MusicRecovery>) -> Option<&'static str> {
    recovery.map(|recovery| match recovery {
        MusicRecovery::RotateManaged => "rotate",
        MusicRecovery::EnterAgain => "enter",
    })
}

fn set_state(step: &rusqlite::Transaction<'_>, account: u64, state: &AccountState) -> rusqlite::Result<usize> {
    step.execute(
        "UPDATE accounts SET email = ?2, life = ?3, status = ?4, password_changes = ?5, reset_required = ?6, code_number = ?7, code_used = ?8, admin = ?9, music_login = ?10, music_managed = ?11, music_recovery = ?12, history_held = ?13, revision = ?14, code_expires = ?15 WHERE number = ?1",
        params![
            number(account)?,
            state.email.as_str(),
            number(state.life)?,
            status_word(state.status),
            number(state.password_changes)?,
            i64::from(state.password_reset_required),
            state.code.map(|code| number(code.number)).transpose()?,
            i64::from(state.code.is_some_and(|code| code.used)),
            i64::from(state.admin),
            number(state.music_login)?,
            i64::from(state.music_managed),
            recovery_word(state.music_recovery),
            i64::from(state.history_held),
            number(state.revision)?,
            state.code.map(|code| number(code.expires)).transpose()?
        ],
    )
}

impl Change<'_> {
    fn apply(&self, step: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        match *self {
            Change::Account { number: account, shown, state } => {
                step.execute(
                    "INSERT INTO accounts (number, email, shown, life, status, password_changes, reset_required, code_used, admin, music_login, music_managed, history_held, revision) VALUES (?1, ?2, ?3, 0, 'on', 0, 0, 0, 0, 0, 0, 0, 0)",
                    params![number(account)?, state.email.as_str(), shown],
                )?;
                set_state(step, account, state)?;
            }
            Change::Install { number: account, rows, at } => {
                if set_state(step, account, &rows.account)? != 1 {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                let mut ids: Vec<i64> = Vec::new();
                for (&id, device) in &rows.devices {
                    let id = id.get();
                    ids.push(number(id)?);
                    step.execute(
                        "INSERT INTO devices (id, account, secret, name, kind, version, first_signed_in, number) VALUES (?1, ?2, ?3, '', 'pc', '', ?4, ?5)
                         ON CONFLICT (id) DO UPDATE SET number = excluded.number WHERE devices.account = excluded.account",
                        params![number(id)?, number(account)?, device.secret.as_bytes().as_slice(), number(at)?, number(device.number)?],
                    )?;
                    match &device.session {
                        None => {
                            step.execute("DELETE FROM sessions WHERE device = ?1", [number(id)?])?;
                        }
                        Some(session) => {
                            let leave = session.leave.as_ref().map(|leave| leave.fields());
                            // With the session, the fingerprint of the secret that can only end it (11).
                            step.execute(
                                "INSERT INTO sessions (device, id, hash, last_used, sign_out, leave_for, leave_life, leave_revision, leave_admin, leave_granted, leave_serial, leave_hash, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                                 ON CONFLICT (device) DO UPDATE SET id = excluded.id, hash = excluded.hash, last_used = excluded.last_used, sign_out = excluded.sign_out, kind = excluded.kind,
                                 leave_for = excluded.leave_for, leave_life = excluded.leave_life, leave_revision = excluded.leave_revision, leave_admin = excluded.leave_admin,
                                 leave_granted = excluded.leave_granted, leave_serial = excluded.leave_serial, leave_hash = excluded.leave_hash",
                                params![
                                    number(id)?,
                                    number(session.id.get())?,
                                    session.hash.as_bytes().as_slice(),
                                    number(session.last_used)?,
                                    session.sign_out.as_bytes().as_slice(),
                                    leave.map(|(intent, ..)| crate::store::apart::intent_word(intent)),
                                    leave.map(|(_, life, ..)| number(life)).transpose()?,
                                    leave.map(|(_, _, revision, ..)| number(revision)).transpose()?,
                                    leave.map(|(_, _, _, admin, ..)| i64::from(admin)),
                                    leave.map(|(_, _, _, _, granted, ..)| number(granted)).transpose()?,
                                    leave.map(|(.., serial, _)| number(serial)).transpose()?,
                                    leave.map(|(.., hash)| hash.as_bytes().to_vec()),
                                    kind_word(session.kind)
                                ],
                            )?;
                        }
                    }
                }
                let mut statement = step.prepare("SELECT id FROM devices WHERE account = ?1")?;
                let present: Vec<i64> = statement.query_map([number(account)?], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
                for id in present.into_iter().filter(|id| !ids.contains(id)) {
                    step.execute("DELETE FROM devices WHERE id = ?1", [id])?;
                }
            }
            Change::Describe { device, name, kind, version } => {
                step.execute("UPDATE devices SET name = ?2, kind = ?3, version = ?4 WHERE id = ?1", params![number(device)?, name, kind, version])?;
            }
            Change::Heard { device, at } => {
                step.execute("UPDATE devices SET last_heard = max(coalesce(last_heard, 0), ?2) WHERE id = ?1", params![number(device)?, number(at)?])?;
            }
            Change::Erase { account } => {
                step.execute("DELETE FROM accounts WHERE number = ?1", [number(account)?])?;
            }
            Change::Password { account, password } => {
                let (hash, salt, memory, passes, lanes) = match password {
                    Some(p) => (Some(p.hash.to_vec()), Some(p.salt.to_vec()), Some(i64::from(p.memory)), Some(i64::from(p.passes)), Some(i64::from(p.lanes))),
                    None => (None, None, None, None, None),
                };
                step.execute(
                    "UPDATE accounts SET password = ?2, password_salt = ?3, password_memory = ?4, password_passes = ?5, password_lanes = ?6 WHERE number = ?1",
                    params![number(account)?, hash, salt, memory, passes, lanes],
                )?;
            }
            Change::Code { account, number: code_number, code, made_at, until } => {
                step.execute("DELETE FROM codes WHERE account = ?1", [number(account)?])?;
                step.execute(
                    "INSERT INTO codes (account, number, code, made_at, until) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![number(account)?, number(code_number)?, code.as_slice(), number(made_at)?, number(until)?],
                )?;
            }
            Change::Codes { account, keep } => {
                step.execute("DELETE FROM codes WHERE account = ?1 AND (?2 IS NULL OR number != ?2)", params![number(account)?, keep.map(number).transpose()?])?;
            }
            Change::MusicLogin { account, install } => {
                let binding = &install.binding;
                step.execute(
                    "INSERT INTO music_logins (account, count, name, sealed, internal) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (account) DO UPDATE SET count = excluded.count, name = excluded.name, sealed = excluded.sealed, internal = excluded.internal",
                    params![number(account)?, number(binding.count)?, binding.name.as_str(), install.sealed.as_bytes(), binding.internal.as_str()],
                )?;
            }
            Change::MusicPublic { address } => {
                step.execute("UPDATE meta SET music_public = ?1 WHERE one = 1", [address])?;
            }
            Change::DiagnosticsDefault { on } => {
                step.execute("UPDATE meta SET diagnostics_default = ?1 WHERE one = 1", [i64::from(on)])?;
            }
            Change::Restore { restore, at, from } => {
                step.execute("DELETE FROM sessions", [])?;
                step.execute(
                    "UPDATE meta SET restore = ?1, restored_at = ?2, restored_from = coalesce(?3, restored_from) WHERE one = 1",
                    params![number(restore)?, number(at)?, from.map(number).transpose()?],
                )?;
            }
        }
        Ok(())
    }
}

/// Brings an open database of an older form forward, a step at a time: each step one
/// transaction with `user_version` moved in it. The form it reached, or the step that failed
/// (which changed nothing) and why.
fn forward(connection: &mut Connection, from: i64, known: &[Migration]) -> Result<i64, (i64, rusqlite::Error)> {
    let mut form = from;
    for migration in known.iter().filter(|migration| migration.form > from) {
        let stepped = (|| -> rusqlite::Result<()> {
            let step = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            step.execute_batch(migration.sql)?;
            step.execute_batch(&format!("PRAGMA user_version = {};", migration.form))?;
            crash::point("data: before commit").map_err(io_error)?;
            step.commit()?;
            crash::point("data: committed").map_err(io_error)
        })();
        stepped.map_err(|error| (migration.form, error))?;
        form = migration.form;
    }
    Ok(form)
}

/// A put-back's staged copy of an older form, brought forward in place before it is read
/// for planning (the copy it was made from is untouched). Newer forms are refused by the
/// form check before this.
pub fn bring_forward(path: &Path) -> Result<(), String> {
    let what = format!("The staged copy at {}", path.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    let mut connection = Connection::open_with_flags(path, flags).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    connection.busy_timeout(BUSY).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    let form = check_form(&connection, &what, MIGRATIONS)?;
    forward(&mut connection, form, MIGRATIONS).map_err(|(step, error)| format!("{what} couldn't be brought to form {step}: {}.", why(&error)))?;
    connection.close().map_err(|(_, error)| format!("{what} can't be closed: {}.", why(&error)))?;
    fs::File::open(path).and_then(|file| file.sync_all()).map_err(|error| format!("{what} couldn't be made lasting ({}).", error.kind()))
}

/// Removes what a cut making of the data left behind (only files of its own making name).
pub fn tidy(folder: &Path) {
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            // A data file being made, or a copy being staged (`copies`' own making name).
            if entry.file_name().to_str().is_some_and(|name| name.starts_with(MAKING) || name.starts_with(".making-")) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Opens a data file only to read it: for the nightly copy, a command, or a check. Its mark
/// and form are checked first.
pub fn open_to_read(path: &Path, what: &str) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    let connection = Connection::open_with_flags(path, flags).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    connection.busy_timeout(BUSY).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    check_form(&connection, what, MIGRATIONS)?;
    quick_check(&connection, what)?;
    Ok(connection)
}

/// Whose data the file at `path` is, read before anything of it is changed (the review's
/// finding 9): opened only to read, its mark and form checked, its backend id read. Nothing
/// is set and nothing migrated.
pub fn backend_of(path: &Path) -> Result<BackendId, String> {
    let what = format!("The data at {}", path.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    let connection = Connection::open_with_flags(path, flags).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    connection.busy_timeout(BUSY).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    check_form(&connection, &what, MIGRATIONS)?;
    let (backend, _) = identify(&connection).map_err(|error| format!("{what} can't be read: {}.", why(&error)))?;
    connection.close().map_err(|(_, error)| format!("{what} can't be read: {}.", why(&error)))?;
    Ok(backend)
}

/// What a data file says of itself, read only (nothing set, nothing migrated): whose it is,
/// its restore number, how far its changes have got and when its last step was written.
/// `None` if there is no file there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub backend: BackendId,
    pub restore: u64,
    pub changes: u64,
    pub written_at: u64,
}

pub fn identity_of(path: &Path) -> Result<Option<Identity>, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("The data at {} can't be read ({}).", path.display(), error.kind())),
        Ok(_) => {}
    }
    let what = format!("The data at {}", path.display());
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    let connection = Connection::open_with_flags(path, flags).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    connection.busy_timeout(BUSY).map_err(|error| format!("{what} can't be opened: {}.", why(&error)))?;
    check_form(&connection, &what, MIGRATIONS)?;
    let meta = meta_of(&connection).map_err(|error| format!("{what} can't be read: {}.", why(&error)))?;
    connection.close().map_err(|(_, error)| format!("{what} can't be read: {}.", why(&error)))?;
    Ok(Some(Identity { backend: meta.backend, restore: meta.restore, changes: meta.changes, written_at: meta.written_at }))
}

/// The backend id and the time of the last step of a data file opened to read.
pub fn identify(connection: &Connection) -> rusqlite::Result<(BackendId, u64)> {
    connection.query_row("SELECT backend, written_at FROM meta WHERE one = 1", [], |row| {
        let backend: Vec<u8> = row.get(0)?;
        let backend: [u8; 16] = backend.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(0, "backend".into(), rusqlite::types::Type::Blob))?;
        Ok((BackendId::from_bytes(backend), unsigned(row.get(1)?)?))
    })
}
