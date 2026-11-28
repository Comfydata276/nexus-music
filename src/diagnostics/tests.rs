use super::*;
use std::path::PathBuf;
use std::process::Command;

use diag::{Fault, FaultPlace, FaultShape, Line, Note};
use nexus_core::nexus::{account::{RequestHead, RequestId}, proto::SPEAKS};

const AT: i64 = 1_700_000_000;

struct TestFolder(PathBuf);

impl TestFolder {
    fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("nexus-diagnostics-{:016x}", fastrand::u64(..)));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn preview_save_and_c6_upload_have_the_same_clean_text() -> std::io::Result<()> {
    let song = "Paper Satellites";
    let album = "Cardboard Orbit";
    let artist = "The Folded Stars";
    let playlist = "Night Trains of Neptune";
    let folder = r"C:\Users\ExampleLocal\Downloads";
    let server = "https://private-music.example.invalid";
    let email = "melody@example.invalid";
    let key = "6f27276c3c90418fbd11b1a3fd30a84c";
    let token = "nxs_demo_xzYuD9427aVpYtUW";
    let odd = "岬🪁\u{202e}\0\t\x1b\r";
    let long = "A".repeat(61_000);
    let text = format!("{song} {album} {artist} {playlist} {folder} {server} {email} {key} {token} {odd} {long}");
    let settings = config::Settings {
        server: server.into(),
        username: email.into(),
        output_device: token.into(),
        output_device_name: text.clone(),
        downloads_dir: folder.into(),
        songs_dir: folder.into(),
        device_name: text.clone(),
        pinned_playlists: vec![playlist.into()],
        lidarr_address: server.into(),
        lidarr_gate_client: key.into(),
        lidarr_signed_in: email.into(),
        ..config::Settings::default()
    };
    let messages = [
        (AT, format!("Couldn't play “{song}”.")),
        (AT + 1, format!("Added to “{playlist}”")),
        (AT + 2, format!("Your downloads are now in {folder}")),
        (AT + 3, format!("In step: {text}\nAnother line: {token}")),
        (AT + 4, "Foreground entered after 123487 ms".into()),
        (AT + 5, format!("Couldn't play “{album}” by {artist}")),
        (i64::MAX, format!("Couldn't play {text}")),
    ].into();
    let observations = [
        (AT, Line::Fault(Fault::new(FaultPlace::Playback, FaultShape::Decode))),
        (AT + 2, Line::Note(Note::DownloadsFolderChanged)),
        (AT + 5, Line::Fault(Fault::new(FaultPlace::Playback, FaultShape::Decode))),
    ].into_iter().map(|(at, line)| (at as u64 * 1_000, line)).collect();
    let session = Session::from_observations(&settings, true, &messages, observations);
    let dir = TestFolder::new()?;
    let crash = dir.0.join("crash.log");
    let saved = dir.0.join("data/diagnostics.txt");
    fs::write(&crash, [
        b"Old bytes \xff\n".as_slice(),
        format!("2023-11-14 22:13:25 UTC version {key} thread {email} at src/audio/exclusive.rs:71: index out of bounds: {text}\n").as_bytes(),
        format!("2023-11-14 22:13:26 UTC version 0.40.13 thread {token} at src/controller.rs:1: already borrowed: {song}\n").as_bytes(),
        format!("2023-11-14 22:13:27 UTC version 0.40.13 thread main at {folder}\\{artist}.rs:222: {album}\n{odd}{playlist}\n").as_bytes(),
        b"bad date UTC version 0.40.13 thread main at src/main.rs:76: no message\n",
    ].concat())?;
    let model = session.report(&crash);
    let preview = model.text().unwrap();
    session.write_report(&crash, &saved)?;
    assert_eq!(fs::read(&saved)?, preview.as_bytes());
    assert_eq!(diag::Report::read(&preview).unwrap(), model);
    let upload = diag::Upload::new(RequestHead { speaks: SPEAKS, restore: 0 }, RequestId::new(1).unwrap(), &model).unwrap();
    assert_eq!(upload.text(), preview);
    assert_eq!(model.about.loss, LossCounts::default());
    assert!(!preview.contains("Fault in playback: other"));
    for raw in [song, album, artist, playlist, folder, "ExampleLocal", server, email, key, token, "岬", "🪁", "AAAAAAAA", "123487"] {
        assert!(!preview.contains(raw), "included {raw:?}");
    }
    assert!(preview.bytes().all(|b| b == b'\n' || matches!(b, 32..=126)));
    for expected in [
        "HTTPS: yes",
        "2023-11-14 22:13:25  App message shown  (6 times since 2023-11-14 22:13:20)",
        "2023-11-14 22:13:22  Downloads folder changed",
        "2023-11-14 22:13:25  Fault in playback: decode  (2 times since 2023-11-14 22:13:20)",
        "2023-11-14 22:13:25  Fault in audio output: out of bounds at exclusive.rs:71",
        "2023-11-14 22:13:26  Fault in app: borrow conflict at controller.rs:1",
        "2023-11-14 22:13:27  Fault in app: panic at other:222",
    ] {
        assert!(preview.contains(expected), "missing {expected:?} in {preview:?}");
    }
    Ok(())
}

#[test]
fn the_hook_writes_no_panic_payload_even_to_the_crash_log() -> std::io::Result<()> {
    const CHILD: &str = "NEXUS_W6A_PANIC_TEST_CHILD";
    const PRIVATE: &str = "Paper Satellites C:\\Users\\ExampleLocal\t岬\nprivate@example.invalid nxs_fake_0123456789";
    if std::env::var_os(CHILD).is_some() {
        install_panic_hook();
        assert!(std::panic::catch_unwind(|| panic!("already borrowed: {PRIVATE}")).is_err());
        return Ok(());
    }

    let dir = TestFolder::new()?;
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", "diagnostics::tests::the_hook_writes_no_panic_payload_even_to_the_crash_log"])
        .env(CHILD, "1")
        .env("NEXUS_MUSIC_HOME", &dir.0)
        .output()?;
    assert!(child.status.success(), "hook child exited {}", child.status);
    assert!(!String::from_utf8_lossy(&child.stderr).contains("Paper Satellites"));
    let log = dir.0.join("data/crash.log");
    let line = fs::read_to_string(&log)?;
    assert_eq!(line.lines().count(), 1);
    let (at, words) = line.trim_end_matches('\n').split_once('\t').unwrap();
    assert!(at.parse::<u64>().is_ok());
    assert!(words.starts_with("Fault in app: borrow conflict at tests.rs:"));
    append_fault(&log, AT as u64 * 1_000, Some(("src/audio/engine.rs", 82)), PRIVATE)?;
    let text = fs::read_to_string(log)?;
    assert!(text.ends_with("1700000000000\tFault in playback: panic at engine.rs:82\n"));
    assert!(!text.contains("ExampleLocal"));
    Ok(())
}

#[test]
fn crash_tail_and_rendered_entries_are_bounded() -> std::io::Result<()> {
    let dir = TestFolder::new()?;
    let log = dir.0.join("crash.log");
    let crash_line = "1700000000000\tFault in app: no panic payload\n";
    fs::write(&log, format!("old{}\n{}", "x".repeat(CRASH_WINDOW as usize), crash_line.repeat(250)))?;
    assert_eq!(recent_crash_text(&log)?, crash_line.repeat(250));

    let session = Session::from_observations(&config::Settings::default(), false, &VecDeque::new(), Vec::new());
    let report = session.report(&log).text().unwrap();
    assert!(report.contains("Fault in app: no panic payload  (250 times since 2023-11-14 22:13:20)"));
    assert!(report.split_once("times in UTC):\n").unwrap().1.lines().count() <= diag::MAX_ENTRIES);
    assert!(report.lines().all(|line| line.len() <= diag::MAX_LINE_CHARS));
    assert!(report.len() < diag::CALL_BYTES);
    assert!(session.report(&dir.0.join("absent.log")).text().unwrap().ends_with("  (nothing yet)"));
    Ok(())
}

#[test]
fn typed_window_observations_join_the_clean_record() {
    let expected = [
        Line::Note(Note::MediaButtonStarted),
        Line::Note(Note::MediaButtonNothingToResume),
        Line::Fault(Fault::new(FaultPlace::Foreground, FaultShape::Refused)),
    ];
    for line in expected {
        observe(line);
    }
    let observations = OBSERVED.with_borrow(|observed| observed.iter().copied().collect());
    let session = Session::from_observations(&config::Settings::default(), true, &VecDeque::new(), observations);
    let report = session.report(Path::new(""));
    assert_eq!(report.record.entries().iter().map(|entry| entry.line).collect::<Vec<_>>(), expected);
}

#[test]
fn connection_fault_uses_the_error_variant_and_ignores_its_words() {
    use crate::subsonic::ApiError;
    let message = "Paper Satellites C:\\Users\\ExampleLocal".to_string();
    assert_eq!(connection_shape(&ApiError::Network(message.clone())), FaultShape::Connection);
    assert_eq!(connection_shape(&ApiError::NotSubsonic(message.clone())), FaultShape::UnexpectedAnswer);
    assert_eq!(connection_shape(&ApiError::Server { code: 40, message }), FaultShape::Refused);
}
