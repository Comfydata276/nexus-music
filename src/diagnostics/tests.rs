use super::*;
use std::path::PathBuf;

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

fn session(settings: &config::Settings, said: VecDeque<(i64, String)>) -> Session {
    Session {
        options: settings.into(),
        signed_in: true,
        unreachable: true,
        songs: 7,
        pending: 3,
        said,
    }
}

#[test]
fn local_file_matches_the_legacy_bytes_for_the_same_session() -> std::io::Result<()> {
    let settings = config::Settings {
        server: "https://music.example.invalid".into(),
        bitrate: 192,
        crossfade: 2.5,
        offline: true,
        normalize: true,
        ..config::Settings::default()
    };
    let session = session(&settings, [
        (AT, "Couldn't play “Paper Satellites”.\nA second line.".into()),
        (AT + 1, r"Saved to C:\Users\ExampleLocal\diagnostics.txt".into()),
        (i64::MAX, "Time could not be read".into()),
    ].into());
    let local = LocalDetails {
        written: DateTime::from_timestamp(AT, 0).unwrap(),
        server_version: "server-supplied version".into(),
        output_status: "Shared with other apps, 48 kHz.".into(),
        downloads: "1 song, 5 MB".into(),
        cache_size: "Under 1 MB".into(),
    };
    let dir = TestFolder::new()?;
    let crash = dir.0.join("crash.log");
    let saved = dir.0.join("local/diagnostics.txt");
    let recent: Vec<_> = (0..40).map(|i| format!("Raw crash line {i}: Paper Satellites")).collect();
    fs::write(&crash, format!("Old crash line\n{}\r\n", recent.join("\r\n")))?;
    local.write(&session, &crash, &saved)?;

    let mut expected = format!("Nexus Music {} on {} ({})\r\n",
        env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    expected.push_str(concat!(
        "Written 2023-11-14 22:13:20 UTC\r\n\r\n",
        "Signed in: yes\r\n",
        "Server version: server-supplied version\r\n",
        "Server address uses HTTPS: yes\r\n",
        "Offline mode: on; server out of reach: on\r\n",
        "Sound output: Shared with other apps, 48 kHz.\r\n",
        "Exclusive mode: off; streaming limit: 192 kbps (0 is none)\r\n",
        "Gapless: on; crossfade: 2.5 s; levelling: on; equaliser: off\r\n",
        "Library on this PC: 7 songs\r\n",
        "Downloads: 1 song, 5 MB\r\n",
        "Kept songs limit: 2048 MB; cache: Under 1 MB\r\n",
        "Sharing between devices: on; requests through Lidarr: off\r\n",
        "Changes waiting to be sent: 3\r\n\r\n",
        "Recent messages:\r\n",
        "  22:13:20 UTC  Couldn't play “Paper Satellites”.\nA second line.\r\n",
        "  22:13:21 UTC  Saved to C:\\Users\\ExampleLocal\\diagnostics.txt\r\n",
        "   UTC  Time could not be read\r\n\r\n",
        "Times the app stopped by itself:",
    ));
    for line in recent {
        expected.push_str(&format!("\r\n  {line}"));
    }
    assert_eq!(fs::read(&saved)?, expected.as_bytes());
    Ok(())
}

#[test]
fn clean_preview_uses_fixed_words_even_with_private_or_malformed_input() -> std::io::Result<()> {
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
    let session = session(&settings, [
        (AT, format!("Couldn't play “{text}”.")),
        (AT + 1, format!("Saved to {folder}\\diagnostics.txt")),
        (AT + 2, format!("Your downloads have moved to {folder}, but the setting could not be saved.")),
        (AT + 3, format!("Added to “{playlist}”")),
        (AT + 4, format!("In step: {text}\nAnother line: {token}")),
        (AT + 5, format!("Couldn't play “{album}” by {artist}")),
        (AT + 6, "Foreground entered after 123487 ms".into()),
        (i64::MAX, format!("Couldn't play {text}")),
    ].into());
    let dir = TestFolder::new()?;
    let crash = dir.0.join("crash.log");
    fs::write(&crash, [
        b"Old bytes \xff\n".as_slice(),
        format!("2023-11-14 22:13:25 UTC version {key} thread {email} at {folder}\\src\\audio\\exclusive.rs:71: index out of bounds: {text}\n").as_bytes(),
        format!("2023-11-14 22:13:26 UTC version 0.40.13 thread {token} at src/controller.rs:1: already borrowed: {song}\n").as_bytes(),
        format!("2023-11-14 22:13:27 UTC version 0.40.13 thread main at {folder}\\{artist}.rs:222: {album}\n{odd}{playlist}\n").as_bytes(),
        b"bad date UTC version 0.40.13 thread main at src/main.rs:76: no message\n",
    ].concat())?;
    let model = session.report(&crash);
    let report = model.text().unwrap();
    assert_eq!(diag::Report::read(&report).unwrap(), model);
    assert_eq!(diag::Upload::new(&model).unwrap().text(), report);
    for raw in [song, album, artist, playlist, folder, "ExampleLocal", server, email, key, token, "岬", "🪁", "AAAAAAAA", "123487"] {
        assert!(!report.contains(raw), "included {raw:?}");
    }
    assert!(report.bytes().all(|b| b == b'\n' || matches!(b, 32..=126)));
    for expected in [
        "HTTPS: yes",
        "2023-11-14 22:13:22.000  Fault in downloads: write",
        "2023-11-14 22:13:25.000  Fault in playback: other  (2 times since 2023-11-14 22:13:20.000)",
        "2023-11-14 22:13:25.000  Fault in audio output: other",
        "2023-11-14 22:13:27.000  Fault in app: other  (2 times since 2023-11-14 22:13:26.000)",
    ] {
        assert!(report.contains(expected), "missing {expected:?} in {report:?}");
    }
    Ok(())
}

#[test]
fn crash_tail_and_rendered_entries_are_bounded() -> std::io::Result<()> {
    let dir = TestFolder::new()?;
    let log = dir.0.join("crash.log");
    let crash_line = "2023-11-14 22:13:20 UTC version 0.40.13 thread main at src/main.rs:83: no message\n";
    fs::write(&log, format!("old{}\n{}", "x".repeat(CRASH_WINDOW as usize), crash_line.repeat(250)))?;
    let tail = recent_crash_text(&log)?;
    assert_eq!(tail, crash_line.repeat(250));
    let session = session(&config::Settings::default(), VecDeque::new());
    let report = session.report(&log).text().unwrap();
    assert!(report.contains("Fault in app: other  (250 times since 2023-11-14 22:13:20.000)"));
    assert!(report.split_once("times in UTC):\n").unwrap().1.lines().count() <= diag::MAX_ENTRIES);
    assert!(report.lines().all(|line| line.len() <= diag::MAX_LINE_CHARS));
    assert!(report.len() < diag::CALL_BYTES);
    assert!(session.report(&dir.0.join("absent.log")).text().unwrap().ends_with("  (nothing yet)"));
    Ok(())
}
