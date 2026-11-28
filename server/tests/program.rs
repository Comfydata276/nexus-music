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
    // A made-up address of a documentation range: nothing on loopback is the proxy.
    settings_with_proxy(folders, port, "192.0.2.10")
}

fn settings_with_proxy(folders: &Folders, port: u16, proxy: &str) -> Vec<(&'static str, String)> {
    vec![
        ("NEXUS_MUSIC_SERVER", "http://navidrome:4533".into()),
        ("NEXUS_PROXY", proxy.into()),
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
    let base = [("NEXUS_MUSIC_SERVER", "http://navidrome:4533"), ("NEXUS_PROXY", "172.18.0.2"), ("NEXUS_DATA", data.as_str()), ("NEXUS_STATE", state.as_str()), ("NEXUS_LISTEN", "127.0.0.1:9")];
    let cases: &[(&str, Option<&str>, &str)] = &[
        ("NEXUS_MUSIC_SERVER", None, "NEXUS_MUSIC_SERVER is missing"),
        ("NEXUS_PROXY", None, "NEXUS_PROXY is missing"),
        ("NEXUS_MUSIC_SERVER", Some("navidrome:4533"), "NEXUS_MUSIC_SERVER is wrong"),
        ("NEXUS_PROXY", Some("npm:81"), "NEXUS_PROXY is wrong"),
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
fn the_health_check_opens_the_store_kept_apart_and_the_data() {
    // 19.4: the container's own check opens both; hello answering is not enough.
    let folders = Folders::new("stores");
    let settings = settings_with_proxy(&folders, free_port(), "127.0.0.1");
    let child = serve(&settings);
    healthy(&settings);
    let kept = std::fs::read(folders.state().join("kept")).unwrap();
    let mut damaged = kept.clone();
    damaged[10] ^= 1;
    std::fs::write(folders.state().join("kept"), &damaged).unwrap();
    let output = health(&settings);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("store kept apart can't be read"), "{}", String::from_utf8_lossy(&output.stderr));
    std::fs::write(folders.state().join("kept"), &kept).unwrap();
    healthy(&settings);
    std::fs::rename(folders.data().join("nexus.db"), folders.data().join("elsewhere.db")).unwrap();
    let output = health(&settings);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("data"), "{}", String::from_utf8_lossy(&output.stderr));
    std::fs::rename(folders.data().join("elsewhere.db"), folders.data().join("nexus.db")).unwrap();
    stop(child);
}

#[test]
fn two_started_at_once_on_one_backends_folders_one_serves_and_the_other_says_so_and_stops() {
    // Design 13.4: one thread writes. A second program on the same folders (from the first
    // slice on, two served one id; from S3, the data must have one writer) is refused before
    // it reads anything, whichever of the two folders it shares.
    for round in 0..5 {
        let folders = Folders::new(&format!("twice-{round}"));
        let first = settings_for(&folders, free_port());
        let second = settings_for(&folders, free_port());
        let children = [serve(&first), serve(&second)];
        let start = Instant::now();
        let (mut serving, mut stopped) = (Vec::new(), Vec::new());
        for (child, settings) in children.into_iter().zip([&first, &second]) {
            let mut child = child;
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    let mut written = String::new();
                    child.stderr.take().unwrap().read_to_string(&mut written).unwrap();
                    stopped.push((status.code(), written));
                    break;
                }
                if health(settings).status.success() {
                    serving.push(child);
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(10), "round {round}: neither served nor stopped");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        assert_eq!((serving.len(), stopped.len()), (1, 1), "round {round}: {stopped:?}");
        let (code, written) = &stopped[0];
        assert_eq!(*code, Some(1), "round {round}: {written}");
        assert!(written.contains("is in use by another Nexus server"), "round {round}: {written}");
        let mut names: Vec<_> = std::fs::read_dir(folders.state()).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["id", "kept", "lock"], "round {round}");
        let id = kept_id(&folders.state());
        let written = stop(serving.pop().unwrap());
        assert!(written.contains(&format!("with id {id} (made now)")), "round {round}: {written}");
    }
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
    // The proxy played from 127.0.0.2.
    let settings = settings_with_proxy(&folders, port, "127.0.0.2");
    let child = serve(&settings);
    healthy(&settings);
    let at: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let request = "POST /nexus/x/../marker-target HTTP/1.1\r\nHost: marker-host\r\nX-Nexus-Scheme: https\r\nX-Marker: marker-header\r\nContent-Length: 11\r\n\r\nmarker-body";
    // From the proxy (refused for its target), from this machine (asking more than hello),
    // and as a stranger.
    for (from, answered) in [("127.0.0.2", true), ("127.0.0.1", false), ("127.0.0.3", false)] {
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket.bind(&SocketAddr::new(from.parse().unwrap(), 0).into()).unwrap();
        socket.connect(&at.into()).unwrap();
        let mut stream: TcpStream = socket.into();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = stream.write_all(request.as_bytes());
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        assert_eq!(!answer.is_empty(), answered, "{from}: {}", String::from_utf8_lossy(&answer));
    }
    let written = stop(child);
    for secret in ["127.0.0.3", "127.0.0.2:", "marker"] {
        assert!(!written.contains(secret), "the log holds {secret:?}: {written}");
    }
    // What it does say: that it started, and with which settings.
    assert!(written.contains("Nexus server 0.1.0 started"), "{written}");
    assert!(written.contains("127.0.0.2") && written.contains("http://navidrome:4533"), "{written}");
}
