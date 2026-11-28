//! The container's own check (design 19.4): asks this backend's `hello` on this machine's own
//! loopback address, as the proxy would, and holds the answer to the shared library's rule,
//! and its id to the one kept in the store kept apart; and then opens the store kept apart and
//! the data, to read only, and finds this backend's id in each. (Lightly: the whole file's own
//! check is the start's, not every health check's.)

use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use nexus_core::nexus::proto::{self, Found};

use crate::{identity, probe, settings::Settings};

/// How long the check waits for each step.
pub const WAIT: Duration = Duration::from_secs(3);

/// Where the check connects: the port listened on, at this machine's loopback address when
/// the backend listens on every address.
pub fn address(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listen.port())
}

/// `Ok` if the backend answers its `hello`, with the id that is kept; otherwise what is wrong.
pub fn check(settings: &Settings) -> Result<(), String> {
    let kept = identity::read(&settings.state)?.ok_or("There is no backend id yet: the backend has not started.")?;
    let at = address(settings.listen);
    let mut stream = TcpStream::connect_timeout(&at, WAIT).map_err(|why| format!("Nothing answers on port {} ({}).", at.port(), why.kind()))?;
    stream.set_read_timeout(Some(WAIT)).and_then(|()| stream.set_write_timeout(Some(WAIT))).map_err(|why| why.to_string())?;
    let request = format!("GET {} HTTP/1.1\r\nHost: localhost\r\nX-Nexus-Scheme: https\r\nConnection: close\r\n\r\n", proto::HELLO);
    stream.write_all(request.as_bytes()).map_err(|why| format!("The question couldn't be sent ({}).", why.kind()))?;
    let mut from = BufReader::new(stream);
    let head = probe::read_head(&mut from)
        .map_err(|why| format!("The answer couldn't be read ({}).", why.kind()))?
        .ok_or("The connection was closed without an answer.")?;
    let body = probe::body(&head, from).map_err(|why| format!("The answer couldn't be read ({}).", why.kind()))?;
    match proto::classify(head.status, body) {
        Found::Backend(hello) if hello.id == kept => stores(settings, &kept),
        Found::Backend(_) => Err("The backend answered with an id other than the one kept.".into()),
        Found::NoBackend(why) => Err(format!("The answer was not the backend's hello ({why:?}).")),
    }
}

/// The store kept apart and the data, each opened to read and found to be this backend's.
fn stores(settings: &Settings, kept: &nexus_core::nexus::proto::BackendId) -> Result<(), String> {
    use crate::store::{apart, data};
    match apart::read(&settings.state) {
        apart::Reading::Found(store) if store.backend == *kept => {}
        apart::Reading::Found(_) => return Err("The store kept apart names another backend.".into()),
        apart::Reading::Missing => return Err("The store kept apart is missing.".into()),
        apart::Reading::Unreadable(why) => return Err(format!("The store kept apart can't be read: {why}.")),
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = rusqlite::Connection::open_with_flags(data::place(&settings.data), flags).map_err(|error| format!("The data can't be opened: {}.", data::why(&error)))?;
    connection.busy_timeout(WAIT).map_err(|error| format!("The data can't be opened: {}.", data::why(&error)))?;
    let mark: i64 = connection.query_row("PRAGMA application_id", [], |row| row.get(0)).map_err(|error| format!("The data can't be read: {}.", data::why(&error)))?;
    if mark != data::APPLICATION_ID {
        return Err("The data is not the Nexus server's.".into());
    }
    match data::identify(&connection) {
        Ok((backend, _)) if backend == *kept => Ok(()),
        Ok(_) => Err("The data is another backend's.".into()),
        Err(error) => Err(format!("The data can't be read: {}.", data::why(&error))),
    }
}
