//! What signing in reads from the data, and nothing else: read-only SQL over the data's one
//! connection, on the thread that writes, between steps (19.3: a read is short, and never
//! held open while anything is written to a socket). The form is S3's (`store::data`).

use nexus_core::nexus::account::{DeviceId, DeviceKind, Name, SessionId};
use nexus_core::nexus::proto::Version;
use rusqlite::{Connection, OptionalExtension, params};

fn signed(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))
}

fn unsigned(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn bytes32(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<[u8; 32]> {
    let bytes: Vec<u8> = row.get(at)?;
    bytes.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(at, "fingerprint".into(), rusqlite::types::Type::Blob))
}

/// Whose a session is, found by its fingerprint: (account, device).
pub fn session_by_hash(data: &Connection, hash: &[u8; 32]) -> rusqlite::Result<Option<(u64, DeviceId)>> {
    data.query_row(
        "SELECT devices.account, devices.id FROM sessions JOIN devices ON devices.id = sessions.device WHERE sessions.hash = ?1",
        [hash.as_slice()],
        |row| Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?)),
    )
    .optional()
    .map(|found| found.and_then(|(account, device)| Some((account, DeviceId::new(device)?))))
}

/// A session found by its id: (account, device, its fingerprint, the fingerprint of the
/// secret that can only end it, if one was given).
pub type ById = (u64, DeviceId, [u8; 32], Option<[u8; 32]>);

pub fn session_by_id(data: &Connection, id: SessionId) -> rusqlite::Result<Option<ById>> {
    data.query_row(
        "SELECT devices.account, devices.id, sessions.hash, sessions.sign_out FROM sessions JOIN devices ON devices.id = sessions.device WHERE sessions.id = ?1",
        [signed(id.get())?],
        |row| {
            let revoke: Option<Vec<u8>> = row.get(3)?;
            let revoke = match revoke {
                None => None,
                Some(bytes) => Some(bytes.try_into().map_err(|_| rusqlite::Error::InvalidColumnType(3, "revoke".into(), rusqlite::types::Type::Blob))?),
            };
            Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?, bytes32(row, 2)?, revoke))
        },
    )
    .optional()
    .map(|found| found.and_then(|(account, device, hash, revoke)| Some((account, DeviceId::new(device)?, hash, revoke))))
}

/// The other accounts on which a device with this secret's fingerprint has a session now
/// (8.6: a sign-in to one account ends the session the device had on another).
pub fn other_accounts_signed_in(data: &Connection, secret: &[u8; 32], account: u64) -> rusqlite::Result<Vec<u64>> {
    let mut statement = data.prepare(
        "SELECT DISTINCT devices.account FROM devices JOIN sessions ON sessions.device = devices.id WHERE devices.secret = ?1 AND devices.account != ?2",
    )?;
    let found = statement.query_map(params![secret.as_slice(), signed(account)?], |row| unsigned(row.get(0)?))?;
    found.collect()
}

/// A one-time code, found by its keyed fingerprint: (account, the code's number, until when).
pub fn code(data: &Connection, print: &[u8; 32]) -> rusqlite::Result<Option<(u64, u64, u64)>> {
    data.query_row("SELECT account, number, until FROM codes WHERE code = ?1", [print.as_slice()], |row| {
        Ok((unsigned(row.get(0)?)?, unsigned(row.get(1)?)?, unsigned(row.get(2)?)?))
    })
    .optional()
}

/// The name an account is shown by.
pub fn shown(data: &Connection, account: u64) -> rusqlite::Result<Option<String>> {
    data.query_row("SELECT shown FROM accounts WHERE number = ?1", [signed(account)?], |row| row.get(0)).optional()
}

/// The accounts that are an admin's and switched on.
pub fn admins(data: &Connection) -> rusqlite::Result<u64> {
    data.query_row("SELECT count(*) FROM accounts WHERE admin = 1 AND status = 'on'", [], |row| unsigned(row.get(0)?))
}

/// One device of an account, as its person's list shows it (8.7). No `Debug`: its name is
/// what a person typed.
#[derive(Clone)]
pub struct Listed {
    pub id: DeviceId,
    pub name: Name,
    pub kind: DeviceKind,
    pub version: Version,
    pub last_seen: u64,
    pub session: Option<SessionId>,
}

/// An account's devices in id order, after `after`, at most `limit`, and whether there are more.
pub fn devices(data: &Connection, account: u64, after: Option<DeviceId>, limit: usize) -> rusqlite::Result<(Vec<Listed>, bool)> {
    let mut statement = data.prepare(
        "SELECT devices.id, devices.name, devices.kind, devices.version, max(devices.first_signed_in, coalesce(devices.last_heard, 0)), sessions.id
         FROM devices LEFT JOIN sessions ON sessions.device = devices.id WHERE devices.account = ?1 AND devices.id > ?2 ORDER BY devices.id LIMIT ?3",
    )?;
    let after = signed(after.map_or(0, DeviceId::get))?;
    let take = signed(limit as u64 + 1)?;
    let mut listed = Vec::new();
    let rows = statement.query_map(params![signed(account)?, after, take], |row| {
        Ok((
            unsigned(row.get(0)?)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            unsigned(row.get(4)?)?,
            row.get::<_, Option<i64>>(5)?.map(unsigned).transpose()?,
        ))
    })?;
    for row in rows {
        let (id, name, kind, version, last_seen, session) = row?;
        // A row whose description is not one the backend would have written is shown with a
        // stand-in rather than dropped, so that it can still be forced out.
        let Some(id) = DeviceId::new(id) else { continue };
        listed.push(Listed {
            id,
            name: Name::new(&name).unwrap_or_else(|| Name::new("A device").expect("a fixed name is a name")),
            kind: if kind == "phone" { DeviceKind::Phone } else { DeviceKind::Pc },
            version: Version::parse(&version).unwrap_or_else(|| Version::parse("0.0.0").expect("a fixed version is a version")),
            last_seen,
            session: session.and_then(SessionId::new),
        });
    }
    let more = listed.len() > limit;
    listed.truncate(limit);
    Ok((listed, more))
}

/// The fingerprint of every device secret the data has a device for (which lane a sign-in waits
/// in: `session::Seen`).
pub fn device_secrets(data: &Connection) -> rusqlite::Result<Vec<[u8; 32]>> {
    let mut statement = data.prepare("SELECT secret FROM devices")?;
    let found = statement.query_map([], |row| bytes32(row, 0))?.collect();
    found
}

/// An account's music login as the data keeps it (S9; the form is S3b's `music_logins`): the
/// sealed password with what it was sealed for, and the internal address it was checked at.
/// `None` when the data has none, or what it has is not a login's shape (then nothing opens).
pub fn music_login(data: &Connection, backend: nexus_core::nexus::proto::BackendId, account: u64) -> rusqlite::Result<Option<(nexus_core::nexus::music::MusicInstallation, String)>> {
    use nexus_core::nexus::account::{AccountId, MusicAddress};
    use nexus_core::nexus::music::{CredentialBinding, MusicInstallation, SealedCredential};
    let found = data
        .query_row("SELECT count, address, name, sealed, internal FROM music_logins WHERE account = ?1", [signed(account)?], |row| {
            Ok((unsigned(row.get(0)?)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, String>(4)?))
        })
        .optional()?;
    Ok(found.and_then(|(count, address, name, sealed, internal)| {
        let binding = CredentialBinding { backend, account: AccountId::new(account)?, address: MusicAddress::new(&address)?, name: Name::new(&name)?, count };
        Some((MusicInstallation { binding, sealed: SealedCredential::from_bytes(&sealed)? }, internal))
    }))
}

/// The address people reach the music server at, as the admin set it (`meta.music_public`).
pub fn music_public(data: &Connection) -> rusqlite::Result<Option<nexus_core::nexus::account::MusicAddress>> {
    let text: Option<String> = data.query_row("SELECT music_public FROM meta WHERE one = 1", [], |row| row.get(0))?;
    Ok(text.and_then(|text| nexus_core::nexus::account::MusicAddress::new(&text)))
}

/// Whether the data has an account numbered `account`.
pub fn account_exists(data: &Connection, account: u64) -> rusqlite::Result<bool> {
    data.query_row("SELECT count(*) FROM accounts WHERE number = ?1", [signed(account)?], |row| row.get::<_, i64>(0)).map(|n| n > 0)
}

/// The account with the lowest number, if there is one (the decoy an unknown email's sign-in
/// does its preparatory work on, 8.3).
pub fn first_account(data: &Connection) -> rusqlite::Result<Option<u64>> {
    let first: Option<i64> = data.query_row("SELECT min(number) FROM accounts", [], |row| row.get(0))?;
    first.map(unsigned).transpose()
}

/// The admin's Diagnostics default (`meta.diagnostics_default`), if one was set: what `hello`
/// says a new device does unless told otherwise (design 14.3).
pub fn diagnostics_default(data: &Connection) -> rusqlite::Result<Option<bool>> {
    let set: Option<i64> = data.query_row("SELECT diagnostics_default FROM meta WHERE one = 1", [], |row| row.get(0))?;
    Ok(set.map(|on| on == 1))
}
