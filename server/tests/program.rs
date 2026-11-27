//! The program itself, as the container runs it: its settings, its id, its health check, and
//! what it writes to its log. Loopback only; the folders are made for each test and removed.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_nexus-music-server");

struct Folders {
    root: PathBuf,
}

impl Folders {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("nexus-server-program-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        Folders { root }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
}

impl Drop for Folders {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn command(settings: &[(&str, &str)], arguments: &[&str]) -> Command {
    let mut command = Command::new(PROGRAM);
    command.env_clear().args(arguments);
    for (name, value) in settings {
        command.env(name, value);
    }
    command
}

fn settings_for(folders: &Folders, port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("NEXUS_MUSIC_SERVER", "http://navidrome:4533".into()),
        // A made-up address of a documentation range: nothing on loopback is the proxy.
        ("NEXUS_PROXY", "192.0.2.10".into()),
        ("NEXUS_DATA", folders.data().display().to_string()),
        ("NEXUS_STATE", folders.state().display().to_string()),
        ("NEXUS_LISTEN", format!("127.0.0.1:{port}")),
    ]
}

fn borrowed<'a>(settings: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    settings.iter().map(|(name, value)| (*name, value.as_str())).collect()
}

fn health(settings: &[(&'static str, String)]) -> Output {
    command(&borrowed(settings), &["health"]).output().unwrap()
}

fn serve(settings: &[(&'static str, String)]) -> Child {
    command(&borrowed(settings), &[]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

/// Waits until the health check passes, and says so.
fn healthy(settings: &[(&'static str, String)]) {
    let start = Instant::now();
    loop {
        let output = health(settings);
        if output.status.success() {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never healthy: {}", String::from_utf8_lossy(&output.stderr));
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stops it, and gives back all it wrote.
fn stop(mut child: Child) -> String {
    child.kill().unwrap();
    child.wait().unwrap();
    let mut written = String::new();
    child.stdout.take().unwrap().read_to_string(&mut written).unwrap();
    child.stderr.take().unwrap().read_to_string(&mut written).unwrap();
    written
}

fn kept_id(state: &Path) -> String {
    std::fs::read_to_string(state.join("id")).unwrap().trim_end().to_string()
}

#[test]
fn a_missing_or_wrong_setting_is_named_and_it_stops() {
    let folders = Folders::new("settings");
    let data = folders.data().display().to_string();
    let state = folders.state().display().to_string();
    let inside = folders.data().join("state");
    std::fs::create_dir_all(&inside).unwrap();
    let inside = inside.display().to_string();
    let missing = folders.root.join("nowhere").display().to_string();
    let base = [("NEXUS_MUSIC_SERVER", "http://navidrome:4533"), ("NEXUS_PROXY", "172.18.0.2"), ("NEXUS_DATA", data.as_str()), ("NEXUS_STATE", state.as_str()), ("NEXUS_LISTEN", "127.0.0.1:0")];
    let cases: &[(&str, Option<&str>, &str)] = &[
        ("NEXUS_MUSIC_SERVER", None, "NEXUS_MUSIC_SERVER is missing"),
        ("NEXUS_PROXY", None, "NEXUS_PROXY is missing"),
        ("NEXUS_MUSIC_SERVER", Some("navidrome:4533"), "NEXUS_MUSIC_SERVER is wrong"),
        ("NEXUS_PROXY", Some("npm"), "NEXUS_PROXY is wrong"),
        ("NEXUS_LISTEN", Some("everywhere"), "NEXUS_LISTEN is wrong"),
        ("NEXUS_STATE", Some(inside.as_str()), "NEXUS_STATE is wrong"),
        ("NEXUS_DATA", Some(missing.as_str()), "NEXUS_DATA is wrong: there is no folder"),
        ("NEXUS_STATE", Some(missing.as_str()), "NEXUS_STATE is wrong: there is no folder"),
    ];
    for &(name, value, said) in cases {
        let mut settings: Vec<(&str, &str)> = base.iter().copied().filter(|(other, _)| *other != name).collect();
        if let Some(value) = value {
            settings.push((name, value));
        }
        let output = command(&settings, &[]).output().unwrap();
        let written = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{name}: {written}");
        assert!(written.starts_with(said), "{name}: {written}");
        assert!(std::fs::read_dir(folders.state()).unwrap().next().is_none(), "{name}: nothing is made in the store");
    }
}

#[test]
fn it_serves_its_id_kept_across_starts_and_says_it_is_healthy() {
    let folders = Folders::new("serves");
    let settings = settings_for(&folders, free_port());
    // Not yet started: not healthy.
    assert!(!health(&settings).status.success());
    let child = serve(&settings);
    healthy(&settings);
    let id = kept_id(&folders.state());
    let written = stop(child);
    assert!(written.contains(&format!("with id {id} (made now)")), "{written}");
    assert!(!health(&settings).status.success());
    // Started again: the same id, kept.
    let child = serve(&settings);
    healthy(&settings);
    assert_eq!(kept_id(&folders.state()), id);
    let written = stop(child);
    assert!(written.contains(&format!("with id {id} (kept)")), "{written}");
    // An id that the check does not find kept is not healthy.
    let child = serve(&settings);
    healthy(&settings);
    std::fs::write(folders.state().join("id"), "ffffffffffffffffffffffffffffffff\n").unwrap();
    let output = health(&settings);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("an id other than the one kept"));
    stop(child);
}

#[test]
fn a_kept_id_that_is_not_an_id_stops_it_and_is_left() {
    let folders = Folders::new("wrong-id");
    std::fs::write(folders.state().join("id"), "not an id\n").unwrap();
    let settings = settings_for(&folders, free_port());
    let output = command(&borrowed(&settings), &[]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not a backend id"));
    assert_eq!(std::fs::read_to_string(folders.state().join("id")).unwrap(), "not an id\n");
}

#[test]
fn its_log_holds_no_address_header_target_or_body() {
    let folders = Folders::new("log");
    let port = free_port();
    let settings = settings_for(&folders, port);
    let child = serve(&settings);
    healthy(&settings);
    let at: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let request = "POST /nexus/x/../marker-target HTTP/1.1\r\nHost: marker-host\r\nX-Nexus-Scheme: https\r\nX-Marker: marker-header\r\nContent-Length: 11\r\n\r\nmarker-body";
    // From this machine (asking more than hello), and as a stranger.
    for from in ["127.0.0.1", "127.0.0.3"] {
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket.bind(&SocketAddr::new(from.parse().unwrap(), 0).into()).unwrap();
        socket.connect(&at.into()).unwrap();
        let mut stream: TcpStream = socket.into();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = stream.write_all(request.as_bytes());
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        assert!(answer.is_empty(), "{from}: {}", String::from_utf8_lossy(&answer));
    }
    let written = stop(child);
    for secret in ["127.0.0.3", "marker"] {
        assert!(!written.contains(secret), "the log holds {secret:?}: {written}");
    }
    // What it does say: that it started, and with which settings.
    assert!(written.contains("Nexus server 0.1.0 started"), "{written}");
    assert!(written.contains("192.0.2.10") && written.contains("http://navidrome:4533"), "{written}");
}
