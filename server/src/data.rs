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

use nexus_core::nexus::compare::{DataDevice, DataSession, DataView};
use nexus_core::nexus::kept::{Fingerprint, Rows, SECRET_BYTES, Scope, Session};
use nexus_core::nexus::proto::BackendId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use crate::crash;

/// The file's name inside `NEXUS_DATA`.
pub const FILE: &str = "nexus.db";
/// Where a new data file is made before it is renamed into place.
const MAKING: &str = "nexus.db.making-";
/// "NXMS": the mark that the file is the Nexus server's data.
pub const APPLICATION_ID: i64 = 0x4e58_4d53;
/// The form this program writes and reads.
pub const FORM: i64 = 1;
/// How long a step waits for another connection's lock.
pub const BUSY: Duration = Duration::from_secs(5);

/// Form 1: what is certain under the design as briefed and under the one being written (the
/// lead, 28 November: the backend is to have accounts of its own). An account is a row per
/// person, by the name it is known by; how a person signs in is not fixed here. **Room is
/// left**, for the slices that bring them, for: a password's hash and whether the account
/// is an admin's (new columns of `accounts`), the settings an admin sets for an account (a
/// table of its own, by account), and the store of listening events (a table by account,
/// `REFERENCES accounts (number) ON DELETE CASCADE`, so that deleting an account's history
/// takes them in the same step). Nothing of a music server's sign-in, of a try at proving,
/// or of history carried over is kept.
const FORM_1: &str = "
CREATE TABLE meta (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    backend BLOB NOT NULL CHECK (length(backend) = 16),
    restore INTEGER NOT NULL CHECK (restore >= 0),
    made_at INTEGER NOT NULL,
    written_at INTEGER NOT NULL,
    restored_at INTEGER,
    restored_from INTEGER
) STRICT;
CREATE TABLE accounts (
    number INTEGER PRIMARY KEY CHECK (number > 0),
    name TEXT NOT NULL UNIQUE,
    life INTEGER NOT NULL CHECK (life >= 0)
) STRICT;
CREATE TABLE devices (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    account INTEGER NOT NULL REFERENCES accounts (number) ON DELETE CASCADE,
    secret BLOB NOT NULL CHECK (length(secret) = 32),
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('pc', 'phone')),
    version TEXT NOT NULL,
    first_connected INTEGER NOT NULL,
    last_signed_in INTEGER,
    last_heard INTEGER,
    number INTEGER NOT NULL CHECK (number >= 0),
    UNIQUE (account, secret)
) STRICT;
CREATE INDEX devices_by_account ON devices (account);
CREATE TABLE sessions (
    device INTEGER NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    id INTEGER NOT NULL CHECK (id > 0),
    slot TEXT NOT NULL CHECK (slot IN ('working', 'waiting')),
    hash BLOB NOT NULL UNIQUE CHECK (length(hash) = 32),
    proven INTEGER NOT NULL,
    delete_proven INTEGER,
    PRIMARY KEY (device, id),
    UNIQUE (device, slot)
) STRICT, WITHOUT ROWID;
";

/// One change of form: from `form - 1` to `form`, as SQL run in one step.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub form: i64,
    pub sql: &'static str,
}

/// Every form this program knows, in order. A later slice adds its own at the end.
pub const MIGRATIONS: &[Migration] = &[Migration { form: 1, sql: FORM_1 }];

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
            let mut connection = Connection::open(&making)?;
            connection.busy_timeout(BUSY)?;
            connection.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
            let step = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            for migration in known {
                step.execute_batch(migration.sql)?;
            }
            step.execute(
                "INSERT INTO meta (one, backend, restore, made_at, written_at) VALUES (1, ?1, 0, ?2, ?2)",
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

    /// Copies the data first (11.2), then changes its form in one step.
    fn migrate(&mut self, folder: &Path, form: i64, known: &[Migration], now: u64) -> Result<(), String> {
        let copy = crate::copies::before_form(folder, &self.connection, form, now)?;
        let what = format!("The data at {}", self.path.display());
        let changed = (|| -> rusqlite::Result<()> {
            let step = self.connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            for migration in known.iter().filter(|migration| migration.form > form) {
                step.execute_batch(migration.sql)?;
            }
            step.execute_batch(&format!("PRAGMA user_version = {};", known.last().map_or(form, |migration| migration.form)))?;
            crash::point("data: before commit").map_err(io_error)?;
            step.commit()?;
            crash::point("data: committed").map_err(io_error)
        })();
        changed.map_err(|error| {
            format!("{what} couldn't be changed from form {form}: {}. It is as it was; a copy made before is {}.", why(&error), copy.display())
        })?;
        println!("The data's form was changed from {form} to {}; a copy made before is {}.", known.last().map_or(form, |m| m.form), copy.display());
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
        self.connection.query_row("SELECT backend, restore, made_at, written_at, restored_at, restored_from FROM meta WHERE one = 1", [], |row| {
            let backend: Vec<u8> = row.get(0)?;
            let backend: [u8; 16] = backend.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(0, "backend".into(), rusqlite::types::Type::Blob))?;
            let optional = |at: usize| -> rusqlite::Result<Option<u64>> { row.get::<_, Option<i64>>(at)?.map(unsigned).transpose() };
            Ok(Meta {
                backend: BackendId::from_bytes(backend),
                restore: unsigned(row.get(1)?)?,
                made_at: unsigned(row.get(2)?)?,
                written_at: unsigned(row.get(3)?)?,
                restored_at: optional(4)?,
                restored_from: optional(5)?,
            })
        })
    }

    /// What `compare` reads.
    pub fn view(&self) -> rusqlite::Result<DataView> {
        let meta = self.meta()?;
        let mut view = DataView { restore: meta.restore, ..Default::default() };
        let mut accounts = self.connection.prepare("SELECT number, life FROM accounts")?;
        for row in accounts.query_map([], |row| Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?)))? {
            let (number, life) = row?;
            view.accounts.insert(number, life);
        }
        let mut devices = self.connection.prepare("SELECT id, account, number FROM devices")?;
        for row in devices.query_map([], |row| Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?, unsigned(row.get(2)?)?)))? {
            let (id, account, number) = row?;
            view.devices.insert(id, DataDevice { account, number, sessions: Vec::new() });
        }
        let mut sessions = self.connection.prepare("SELECT device, id, delete_proven FROM sessions")?;
        for row in sessions.query_map([], |row| Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?, row.get::<_, Option<i64>>(2)?)))? {
            let (device, id, delete_proven) = row?;
            let delete_proven = delete_proven.map(unsigned).transpose()?;
            if let Some(device) = view.devices.get_mut(&device) {
                device.sessions.push(DataSession { id, delete_proven });
            }
        }
        Ok(view)
    }

    /// An account's number and life, by its name.
    pub fn account_named(&self, name: &str) -> rusqlite::Result<Option<(u64, u64)>> {
        self.connection
            .query_row("SELECT number, life FROM accounts WHERE name = ?1", [name], |row| Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?)))
            .optional()
    }

    /// An account's name, by its number.
    pub fn account_name(&self, number: u64) -> rusqlite::Result<Option<String>> {
        self.connection.query_row("SELECT name FROM accounts WHERE number = ?1", [self::number(number)?], |row| row.get(0)).optional()
    }

    /// The device of this account with this secret's fingerprint (C3's `same_device`: the
    /// account and the secret together, never a claimed id alone).
    pub fn device_with_secret(&self, account: u64, secret: &Fingerprint) -> rusqlite::Result<Option<u64>> {
        self.connection
            .query_row(
                "SELECT id FROM devices WHERE account = ?1 AND secret = ?2",
                params![number(account)?, secret.as_bytes().as_slice()],
                |row| unsigned(row.get(0)?),
            )
            .optional()
    }

    /// A device's rows (C3's `Rows`), as the data has them; `None` if it has no such device.
    pub fn rows(&self, backend: &BackendId, device: u64) -> rusqlite::Result<Option<Rows>> {
        let meta = self.meta()?;
        let found = self
            .connection
            .query_row(
                "SELECT accounts.name, accounts.life, devices.number FROM devices JOIN accounts ON accounts.number = devices.account WHERE devices.id = ?1",
                [number(device)?],
                |row| Ok((row.get::<_, String>(0)?, unsigned(row.get(1)?)?, unsigned(row.get(2)?)?)),
            )
            .optional()?;
        let Some((account, life, device_number)) = found else { return Ok(None) };
        let mut rows = Rows {
            scope: Scope { backend: *backend, restore: meta.restore, account, life, device, number: device_number },
            working: None,
            waiting: None,
        };
        let mut sessions = self.connection.prepare("SELECT slot, id, hash, proven, delete_proven FROM sessions WHERE device = ?1")?;
        let found = sessions.query_map([number(device)?], |row| {
            let hash: Vec<u8> = row.get(2)?;
            let hash: [u8; SECRET_BYTES] = hash.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(2, "hash".into(), rusqlite::types::Type::Blob))?;
            let session = Session {
                id: unsigned(row.get(1)?)?,
                hash: Fingerprint::from_bytes(hash),
                proven: unsigned(row.get(3)?)?,
                delete_proven: row.get::<_, Option<i64>>(4)?.map(unsigned).transpose()?,
            };
            Ok((row.get::<_, String>(0)?, session))
        })?;
        for found in found {
            let (slot, session) = found?;
            match slot.as_str() {
                "working" => rows.working = Some(session),
                _ => rows.waiting = Some(session),
            }
        }
        Ok(Some(rows))
    }

    /// Does `changes` as one step: all of them, or (if any fails, or the program stops
    /// part-way) none. The points it passes: `data: before commit`, `data: committed`.
    pub fn apply(&mut self, changes: &[Change<'_>], written_at: u64) -> rusqlite::Result<()> {
        let step = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for change in changes {
            change.apply(&step)?;
        }
        step.execute("UPDATE meta SET written_at = max(written_at, ?1) WHERE one = 1", [number(written_at)?])?;
        crash::point("data: before commit").map_err(io_error)?;
        step.commit()?;
        crash::point("data: committed").map_err(io_error)
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
    pub made_at: u64,
    /// The latest time any step was written at, on the backend's clock (its high-water mark
    /// at the time).
    pub written_at: u64,
    /// When it was last found older than the store, or put back, and the time of the last
    /// step the copy it came from had.
    pub restored_at: Option<u64>,
    pub restored_from: Option<u64>,
}

/// One change to the data, done in a step with others by [`Data::apply`].
#[derive(Debug)]
pub enum Change<'a> {
    Account { number: u64, name: &'a str, life: u64 },
    Device { id: u64, account: u64, secret: &'a Fingerprint, name: &'a str, kind: &'a str, version: &'a str, at: u64 },
    /// A device's rows as C3 decided them: its number, and its sessions replaced.
    Install(&'a Rows),
    /// A device's number set, and every session it had ended.
    Ended { device: u64, number: u64 },
    /// An account's whole history: its devices and sessions, its row, and (through their
    /// own `ON DELETE CASCADE`) whatever later slices keep for it (11.1 step 2).
    Erase { account: u64 },
    Remove { device: u64 },
    Life { account: u64, life: u64 },
    /// A new restore number, and when; every session ended (11.2 steps 1 and 2).
    Restore { restore: u64, at: u64, from: Option<u64> },
}

impl Change<'_> {
    fn apply(&self, step: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        match *self {
            Change::Account { number: account, name, life } => {
                step.execute("INSERT INTO accounts (number, name, life) VALUES (?1, ?2, ?3)", params![number(account)?, name, number(life)?])?;
            }
            Change::Device { id, account, secret, name, kind, version, at } => {
                step.execute(
                    "INSERT INTO devices (id, account, secret, name, kind, version, first_connected, number) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                    params![number(id)?, number(account)?, secret.as_bytes().as_slice(), name, kind, version, number(at)?],
                )?;
            }
            Change::Install(rows) => {
                let device = number(rows.scope.device)?;
                let changed = step.execute("UPDATE devices SET number = ?2 WHERE id = ?1", params![device, number(rows.scope.number)?])?;
                if changed != 1 {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                step.execute("DELETE FROM sessions WHERE device = ?1", [device])?;
                for (slot, session) in [("working", &rows.working), ("waiting", &rows.waiting)] {
                    if let Some(session) = session {
                        step.execute(
                            "INSERT INTO sessions (device, id, slot, hash, proven, delete_proven) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                device,
                                number(session.id)?,
                                slot,
                                session.hash.as_bytes().as_slice(),
                                number(session.proven)?,
                                session.delete_proven.map(number).transpose()?
                            ],
                        )?;
                    }
                }
            }
            Change::Ended { device, number: n } => {
                step.execute("DELETE FROM sessions WHERE device = ?1", [number(device)?])?;
                step.execute("UPDATE devices SET number = ?2 WHERE id = ?1", params![number(device)?, number(n)?])?;
            }
            Change::Erase { account } => {
                step.execute("DELETE FROM accounts WHERE number = ?1", [number(account)?])?;
            }
            Change::Remove { device } => {
                step.execute("DELETE FROM devices WHERE id = ?1", [number(device)?])?;
            }
            Change::Life { account, life } => {
                step.execute("UPDATE accounts SET life = ?2 WHERE number = ?1", params![number(account)?, number(life)?])?;
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

/// Removes what a cut making of the data left behind (only files of its own making name).
pub fn tidy(folder: &Path) {
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(|name| name.starts_with(MAKING)) {
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

/// The backend id and the time of the last step of a data file opened to read.
pub fn identify(connection: &Connection) -> rusqlite::Result<(BackendId, u64)> {
    connection.query_row("SELECT backend, written_at FROM meta WHERE one = 1", [], |row| {
        let backend: Vec<u8> = row.get(0)?;
        let backend: [u8; 16] = backend.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(0, "backend".into(), rusqlite::types::Type::Blob))?;
        Ok((BackendId::from_bytes(backend), unsigned(row.get(1)?)?))
    })
}
