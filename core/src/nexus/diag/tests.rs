use super::*;
use crate::nexus::account::{AccountId, Receipt, Refusal, Stamp, StampTag};
use crate::nexus::proto::{BackendId, SPEAKS};
use serde_json::{Value, json};

fn head() -> RequestHead {
    RequestHead {
        speaks: SPEAKS,
        restore: 8,
    }
}

fn upload(report: &Report) -> Upload {
    Upload::new(head(), RequestId::new(4).unwrap(), report).unwrap()
}

fn receipt() -> Receipt {
    Receipt {
        stamp: Stamp {
            backend: BackendId::from_bytes([5; 16]),
            restore: head().restore,
            account: AccountId::new(3).unwrap(),
            life: 5,
            change: 12,
            tag: StampTag::from_bytes([6; 32]),
        },
        music_login_count: 0,
    }
}

const AT: u64 = 1_800_000_000_000;
const STARTED: Line = Line::Note(Note::PlayerStarted);
const REFUSED: Line = Line::Note(Note::OtherControllerRefused);

fn report(record: Record) -> Report {
    Report {
        about: About {
            pc: None,
            version: Version::parse("0.6.1").unwrap(),
            device: DeviceKind::Phone,
            os: Version::parse("17.0.0").unwrap(),
            signed_in: true,
            https: true,
            offline: false,
            local_permission: Some(false),
            android_auto: Some(true),
            loss: LossCounts::default(),
        },
        record,
    }
}

fn trace(reach: Reach) -> Trace {
    Trace {
        reach,
        local_address: Some(true),
        local_permission: Some(false),
        network: Network::Wifi,
    }
}

#[test]
fn the_device_choice_overrides_the_server_and_the_default_is_on() {
    assert!(enabled(None, None));
    assert!(enabled(None, Some(true)));
    assert!(!enabled(None, Some(false)));
    assert!(enabled(Some(true), Some(false)));
    assert!(!enabled(Some(false), Some(true)));
    assert!(!enabled(Some(false), None));
}

#[test]
fn faults_wait_five_minutes_and_do_not_postpone_each_other() {
    let mut schedule = Schedule::new(AT).unwrap();
    schedule.note_fault(AT);
    schedule.note_fault(AT + FAULT_WAIT_MS - 1);
    assert_eq!(schedule.due(AT + FAULT_WAIT_MS - 1, true, true), None);
    assert_eq!(
        schedule.due(AT + FAULT_WAIT_MS, true, true),
        Some(Due::Fault)
    );
    assert_eq!(
        schedule.take_due(AT + FAULT_WAIT_MS, true, true),
        Some(Due::Fault)
    );
    assert_eq!(schedule.take_due(AT + FAULT_WAIT_MS, true, true), None);
    assert_eq!(schedule.fault_ms(), None);
}

#[test]
fn first_report_waits_five_minutes_then_the_daily_timer_waits_a_full_day() {
    let first = AT + 5 * 60 * 1000;
    let mut schedule = Schedule::new(AT).unwrap();
    assert_eq!(schedule.due(first - 1, true, true), None);
    assert_eq!(schedule.due(first, false, true), None);
    assert_eq!(schedule.due(first, true, false), None);
    assert_eq!(schedule.take_due(first, true, true), Some(Due::Daily));
    assert_eq!(schedule.due(first + DAY_MS - 1, true, true), None);
    assert_eq!(schedule.due(first + DAY_MS, true, true), Some(Due::Daily));
    schedule.note_fault(first);
    assert_eq!(schedule.due(first + DAY_MS, false, true), None);
    assert_eq!(schedule.due(first + DAY_MS, true, false), None);
}

#[test]
fn even_an_unanswered_attempt_uses_the_ten_minute_gap() {
    let mut schedule = Schedule::new(AT).unwrap();
    schedule.note_fault(AT);
    let sent = AT + FAULT_WAIT_MS;
    assert_eq!(schedule.take_due(sent, true, true), Some(Due::Fault));
    schedule.note_fault(sent);
    assert_eq!(schedule.due(sent + MIN_GAP_MS - 1, true, true), None);
    assert_eq!(
        schedule.due(sent + MIN_GAP_MS, true, true),
        Some(Due::Fault)
    );
}

#[test]
fn a_rolling_day_limits_sends_across_restart_and_clock_rollback() {
    let mut schedule = Schedule::new(AT).unwrap();
    let first = AT + FAULT_WAIT_MS;
    for n in 0..MAX_PER_DAY as u64 {
        let at = first + n * MIN_GAP_MS;
        schedule.note_fault(at - FAULT_WAIT_MS);
        assert_eq!(schedule.take_due(at, true, true), Some(Due::Fault));
    }
    schedule.note_fault(first + 30 * MIN_GAP_MS);
    let attempts = schedule.attempts().collect::<Vec<_>>();
    let mut restored =
        Schedule::restore(schedule.started_ms(), schedule.fault_ms(), &attempts).unwrap();
    assert_eq!(restored, schedule);
    assert_eq!(restored.take_due(AT, true, true), None);
    assert_eq!(restored.take_due(first + DAY_MS - 1, true, true), None);
    assert_eq!(
        restored.take_due(first + DAY_MS, true, true),
        Some(Due::Fault)
    );
    assert_eq!(restored.attempts().count(), MAX_PER_DAY);
    assert!(Schedule::restore(AT, None, &[AT, AT + 1]).is_none());
    assert!(Schedule::restore(AT, None, &[AT - 1]).is_none());
    assert!(Schedule::restore(AT, Some(AT), &[first]).is_none());
    assert!(Schedule::restore(AT, None, &[AT; MAX_PER_DAY + 1]).is_none());
    assert!(Schedule::new(TIME_MAX_MS + 1).is_none());
}

#[test]
fn cleaned_repeated_words_are_counted_and_move_to_last() {
    let mut record = Record::default();
    record.note_text(AT, "\tPlayer\nstarted\r");
    record.note(AT + 1, REFUSED);
    record.note_text(AT + 2, " Player started  ");
    record.note_text(AT + 3, " \t\r\n");
    assert_eq!(record.entries().len(), 2);
    assert_eq!(
        record.entries().back(),
        Some(&Entry {
            first_ms: AT,
            last_ms: AT + 2,
            count: 2,
            line: STARTED
        })
    );
    record.note_times(AT + 2, STARTED, u32::MAX);
    record.note(AT - 1, STARTED);
    assert_eq!(
        record.entries().back(),
        Some(&Entry {
            first_ms: AT - 1,
            last_ms: AT + 2,
            count: u32::MAX,
            line: STARTED
        })
    );
}

#[test]
fn last_attempt_and_last_failure_replace_separately_across_restart() {
    let mut before = Record::default();
    before.note_attempt(AT, trace(Reach::None), true);
    before.note(AT + 1, STARTED);
    let text = report(before).text().unwrap();
    let mut read = Report::read(&text).unwrap().record;
    let mut since = Record::default();
    since.note(AT + 2, STARTED);
    since.note_attempt(AT + 3, trace(Reach::Answered), false);
    read.merge_later(since);
    assert_eq!(
        read.entries()
            .iter()
            .map(|entry| entry.line)
            .collect::<Vec<_>>(),
        [
            Line::LastFailure(trace(Reach::None)),
            STARTED,
            Line::LastAttempt(trace(Reach::Answered)),
        ]
    );
    assert_eq!(read.entries().get(1).unwrap().count, 2);
    read.note_attempt(AT + 4, trace(Reach::MadeSecure), true);
    assert_eq!(
        read.entries()
            .iter()
            .map(|entry| entry.line)
            .collect::<Vec<_>>(),
        [
            STARTED,
            Line::LastAttempt(trace(Reach::MadeSecure)),
            Line::LastFailure(trace(Reach::MadeSecure)),
        ]
    );
    assert_eq!(read.entries().back().unwrap().count, 1);
    read.clear();
    assert!(read.entries().is_empty());
}

#[test]
fn the_record_keeps_200_bounded_lines_and_reserves_its_latest_traces() {
    let mut record = Record::default();
    record.note_attempt(AT, trace(Reach::None), true);
    for n in 0..=255u8 {
        record.note(AT + u64::from(n) * 1000, Line::DownloadJobStopped(n));
    }
    record.note(AT, Line::ForegroundAfterMs(MAX_FOREGROUND_MS + 1));
    record.note(AT, Line::ForegroundAfterMs(MAX_FOREGROUND_MS));
    assert_eq!(record.entries().len(), MAX_ENTRIES);
    assert_eq!(
        record.entries().front().unwrap().line,
        Line::LastAttempt(trace(Reach::None))
    );
    assert_eq!(
        record.entries().get(1).unwrap().line,
        Line::LastFailure(trace(Reach::None))
    );
    assert_eq!(
        record.entries().get(2).unwrap().line,
        Line::DownloadJobStopped(59)
    );
    assert_eq!(
        record.entries().back().unwrap().line,
        Line::ForegroundAfterMs(MAX_FOREGROUND_MS)
    );
    let report = report(record);
    let text = report.text().unwrap();
    assert!(
        text.lines()
            .all(|line| line.chars().count() <= MAX_LINE_CHARS)
    );
    assert_eq!(Report::read(&text).unwrap(), report);
    assert!(upload(&report).write().unwrap().bytes().len() < CALL_BYTES);
    assert_eq!(clean_line(&" ".repeat(MAX_LINE_CHARS + 1)), None);
}

#[test]
fn the_report_says_when_and_how_often_and_is_the_only_text_uploaded() {
    let mut record = Record::default();
    record.note(AT, STARTED);
    record.note(AT + 61_000, STARTED);
    let report = report(record);
    let shown = report.text().unwrap();
    assert!(
        shown.ends_with(
            "  2027-01-15 08:01:01  Player started  (2 times since 2027-01-15 08:00:00)"
        )
    );
    assert!(!shown.contains('\t'));
    let request = upload(&report);
    let wire = request.write().unwrap();
    let incoming = Upload::read(wire.bytes()).unwrap();
    assert_eq!(incoming.text(), shown);
    assert_eq!(incoming.head, head());
    assert_eq!(incoming.request_id, RequestId::new(4).unwrap());
    let value: Value = serde_json::from_slice(wire.bytes()).unwrap();
    assert_eq!(value, json!({"head":head(), "request_id":4, "text":shown}));
    let empty = self::report(Record::default());
    assert!(empty.text().unwrap().ends_with(EMPTY));
    assert_eq!(Report::read(&empty.text().unwrap()).unwrap(), empty);
}


#[test]
fn names_paths_addresses_keys_tokens_and_odd_characters_never_survive() {
    let mut record = Record::default();
    let raw = [
        "Mira Bell",
        "C:\\Users\\Ari\\Music",
        "/home/ari/secret",
        "https://music.example.test/api",
        "mira@example.test",
        "192.0.2.1",
        "2001:db8::1",
        "https://music.example.test/api?q=made-up-secret",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Mira’s Playlist",
        "L’été à Québec 音楽",
        "secret_api_key=1a2b3c",
        "Bearer made-up-token",
        "Player started\nMira Bell",
        "Player started\u{202e}",
        "Foreground entered after 123 ms\u{0000}",
        "Fault in playback: decode private.mp3",
        "Nexus last attempt: answered, local address https://inside.example.test",
        "Nexus session ended: Forced out Ari",
        "  Player started  ",
    ];
    for line in raw {
        record.note_text(AT, line);
    }
    let places = [
        FaultPlace::Playback, FaultPlace::AudioOutput, FaultPlace::Library, FaultPlace::Downloads,
        FaultPlace::Foreground, FaultPlace::MusicServer, FaultPlace::NexusServer,
        FaultPlace::Journal, FaultPlace::Updates, FaultPlace::App,
    ];
    for place in places {
        for &(shape, _) in SHAPES {
            let line = Line::Fault(Fault::new(place, shape));
            assert_eq!(clean_line(&line.to_string()), Some(line));
            if record.entries().len() < MAX_ENTRIES {
                record.note(AT, line);
            }
        }
    }
    let report = report(record);
    let shown = report.text().unwrap();
    let body = upload(&report).write().unwrap();
    let wire = body.bytes();
    let uploaded = Upload::read(wire).unwrap();
    assert_eq!(uploaded.text(), shown);
    assert_eq!(
        report.record.entries().len(),
        (1 + places.len() * SHAPES.len()).min(MAX_ENTRIES)
    );
    for private in &raw[..raw.len() - 1] {
        assert!(!shown.contains(private));
        assert!(!String::from_utf8_lossy(&wire).contains(private));
    }
    assert!(!shown.contains("private.mp3"));
    for forged in [
        shown.replace("Player started", "Player started: Mira Bell"),
        shown.replace("OS 17.0.0", "OS Ari's phone"),
    ] {
        let wire =
            serde_json::to_vec(&json!({"head":head(), "request_id":4, "text":forged})).unwrap();
        assert_eq!(Upload::read(&wire).err(), Some(Wrong::Form));
    }
}

#[test]
fn the_upload_requires_whole_utf8_bounded_unique_json_and_known_types() {
    let body = upload(&self::report(Record::default())).write().unwrap();
    let wire = body.bytes();
    let json = std::str::from_utf8(wire).unwrap();
    let with = |fields: &str| format!("{{{fields},{}", json.strip_prefix('{').unwrap());
    assert!(Upload::read(with(r#""later":{"field":1}"#).as_bytes()).is_ok());
    for bad in [
        format!("{json}{{}}"),
        with(r#""text":"again""#),
        with(r#""later":{"field":1,"field":2}"#),
        with(&format!("\"deep\":{}0{}", "[".repeat(16), "]".repeat(16))),
        r#"{"text":true}"#.into(),
        r#"{"text":null}"#.into(),
        r#"{"unexpected":"text"}"#.into(),
    ] {
        assert_eq!(Upload::read(bad.as_bytes()).err(), Some(Wrong::Form));
    }
    let mut invalid = with(r#""later":"X""#).into_bytes();
    *invalid.iter_mut().find(|x| **x == b'X').unwrap() = 0xff;
    assert_eq!(Upload::read(&invalid).err(), Some(Wrong::Form));
    let mut at_limit = wire.to_vec();
    at_limit.resize(CALL_BYTES, b' ');
    assert!(Upload::read(&at_limit).is_ok());
    at_limit.push(b' ');
    assert_eq!(Upload::read(&at_limit).err(), Some(Wrong::Size));
    assert_eq!(
        Upload::read(&wire[..wire.len() - 1]).err(),
        Some(Wrong::Form)
    );
}

#[test]
fn an_upload_reply_checks_status_and_stamp_and_carries_no_report() {
    let done = Reply::Done {
        result: Uploaded {},
        receipt: receipt(),
    };
    let body = write_reply(&done).unwrap();
    assert_eq!(read_reply(200, body.bytes()), Ok(done));
    assert_eq!(read_reply(201, body.bytes()), Err(Wrong::Status));
    let mut too_large = body.bytes().to_vec();
    too_large.resize(account::SMALL_REPLY_MAX_BYTES + 1, b' ');
    assert_eq!(read_reply(200, &too_large), Err(Wrong::Size));
    let value: Value = serde_json::from_slice(body.bytes()).unwrap();
    assert_eq!(value["done"]["result"], json!({}));
    assert_eq!(value["done"]["receipt"]["stamp"]["restore"], json!(8));
    for wrong in [
        br#"{"done":true}"#.as_slice(),
        br#"{"done":{"result":{}}}"#,
        b"{",
        b"{}",
    ] {
        assert_eq!(read_reply(200, wrong), Err(Wrong::Form));
    }
    let refusal = Reply::Refused(Refusal::Malformed);
    let refused = write_reply(&refusal).unwrap();
    assert_eq!(read_reply(400, refused.bytes()), Ok(refusal));
    assert_eq!(read_reply(200, refused.bytes()), Err(Wrong::Status));
}

#[test]
fn upload_refuses_a_missing_head_or_invalid_operation_number() {
    let body = upload(&self::report(Record::default())).write().unwrap();
    let valid: Value = serde_json::from_slice(body.bytes()).unwrap();
    for bad_number in [json!(0), json!("4"), json!(null)] {
        let mut invalid = valid.clone();
        invalid["request_id"] = bad_number;
        assert_eq!(
            Upload::read(&serde_json::to_vec(&invalid).unwrap()).err(),
            Some(Wrong::Form)
        );
    }
    let mut missing = valid;
    missing.as_object_mut().unwrap().remove("head");
    assert_eq!(
        Upload::read(&serde_json::to_vec(&missing).unwrap()).err(),
        Some(Wrong::Form)
    );
}

#[test]
fn the_server_keeps_at_most_31_reports_and_none_older_than_30_days() {
    let times: Vec<_> = (0..40).map(|n| AT - n).collect();
    assert_eq!(
        reports_to_keep(&times, AT).collect::<Vec<_>>(),
        (0..31).collect::<Vec<_>>()
    );
    assert_eq!(
        reports_to_keep(&[AT + 1, AT, AT - MAX_AGE_MS, AT - MAX_AGE_MS - 1], AT)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn journal_loss_totals_are_separate_and_cannot_age_out() {
    let mut report = report(Record::default());
    report.about.loss = LossCounts { line_full: 2, files_full: 3, refused: u64::MAX };
    for n in 0..=255 {
        report.record.note(AT, Line::DownloadJobStopped(n));
    }
    let text = report.text().unwrap();
    assert!(text.lines().any(|line| line == format!("{LINE_FULL}2")));
    assert!(text.lines().any(|line| line == format!("{FILES_FULL}3")));
    assert!(text.lines().any(|line| line == format!("{REFUSED_FOR_GOOD}{}", u64::MAX)));
    assert_eq!(Report::read(&text).unwrap().about.loss, report.about.loss);
    report.record.clear();
    assert_eq!(Report::read(&report.text().unwrap()).unwrap().about.loss, report.about.loss);
}

#[test]
fn repeated_startup_timings_replace_one_line_and_leave_faults_visible() {
    let mut record = Record::default();
    let fault = Line::Fault(Fault::new(FaultPlace::Foreground, FaultShape::Refused));
    record.note(AT, fault);
    for ms in 0..500 {
        record.note(AT, Line::ForegroundAfterMs(ms));
    }
    assert_eq!(record.entries().len(), 2);
    assert_eq!(record.entries().front().unwrap().line, fault);
    assert_eq!(record.entries().back().unwrap().line, Line::ForegroundAfterMs(499));
    assert_eq!(record.entries().back().unwrap().count, 1);
}

#[test]
fn phone_notifications_and_new_session_endings_have_fixed_wording() {
    for line in [
        Line::DownloadJobStopped(4),
        Line::Note(Note::MediaButtonStarted),
        Line::Note(Note::MediaButtonNothingToResume),
        Line::Note(Note::MediaButtonStartRefused),
        Line::Note(Note::SharedLibraryUnavailable),
        Line::SessionEnded(SessionEnded::PasswordReset),
        Line::SessionEnded(SessionEnded::DataPutBack),
    ] {
        assert_eq!(clean_line(&line.to_string()), Some(line));
    }
    assert_eq!(clean_line("Android stopped the download job (reason 256)"), None);
    assert_eq!(clean_line("Shared library could not be loaded: /private/path"), None);
}

#[test]
fn panic_payloads_only_contribute_a_closed_shape() {
    use FaultShape::*;

    let path = "/home/mira/secret/music.flac";
    let name = "Mira Bell";
    let token = "1a2b3c4d5e6f78900123456789abcdef0123456789abcdef0123456789abcdef";
    let mut record = Record::default();
    for (message, shape) in [
        ("already borrowed: BorrowMutError", BorrowConflict),
        ("already mutably borrowed: BorrowError", BorrowConflict),
        ("called `Result::unwrap()` on an `Err` value: PoisonError { .. }", PoisonedLock),
        ("called `Option::unwrap()` on a `None` value", MissingValue),
        ("called `Result::unwrap()` on an `Err` value", UnexpectedResultError),
        ("index out of bounds: the len is 1 but the index is 2", Bounds),
        ("assertion `left == right` failed", Assertion),
        ("attempt to add with overflow", Overflow),
        ("unexpected custom panic", Panic),
    ] {
        let payload = format!("{message}: {path} for {name}\nBearer {token}");
        let classified = classify_rust_panic(&payload);
        assert_eq!(classified, shape);
        record.note(AT, Line::Fault(Fault::new(FaultPlace::App, classified)));
    }
    for empty in ["", " \n", "no message", "no payload"] {
        assert_eq!(classify_rust_panic(empty), NoPayload);
    }
    record.note(AT, Line::Fault(Fault::new(FaultPlace::App, NoPayload)));

    let report = report(record);
    let text = report.text().unwrap();
    assert_eq!(Report::read(&text).unwrap(), report);
    let body = upload(&report).write().unwrap();
    for private in [path, name, token] {
        assert!(!text.as_bytes().windows(private.len()).any(|bytes| bytes == private.as_bytes()));
        assert!(!body.bytes().windows(private.len()).any(|bytes| bytes == private.as_bytes()));
    }
}

#[test]
fn windows_messages_and_download_moves_are_counted_as_fixed_notes() {
    let mut record = Record::default();
    for note in [Note::AppMessage, Note::DownloadsFolderChanged] {
        let line = Line::Note(note);
        assert_eq!(clean_line(&line.to_string()), Some(line));
        record.note(AT, line);
        record.note(AT, line);
    }
    assert!(record.entries().iter().all(|entry| entry.count == 2));
    let report = report(record);
    assert_eq!(Report::read(&report.text().unwrap()).unwrap(), report);
}

#[test]
fn a_far_future_attempt_cannot_block_diagnostics_after_clock_correction() {
    let wrong_time = AT + 365 * DAY_MS;
    let mut schedule = Schedule::restore(AT, Some(wrong_time + MIN_GAP_MS), &[wrong_time]).unwrap();
    let corrected = AT + DAY_MS;
    assert_eq!(schedule.due(corrected, true, true), Some(Due::Daily));
    assert_eq!(schedule.take_due(corrected, true, true), Some(Due::Daily));
    assert_eq!(schedule.attempts().collect::<Vec<_>>(), vec![corrected]);
    assert_eq!(schedule.fault_ms(), None);

    let mut first_use = Schedule::new(wrong_time).unwrap();
    first_use.note_fault(corrected);
    assert_eq!(first_use.started_ms(), corrected);
    assert_eq!(first_use.due(corrected + FAULT_WAIT_MS, true, true), Some(Due::Fault));
}

#[test]
fn a_panic_location_carries_only_the_known_basename_and_line() {
    let root = r"C:\Users\ExampleLocal\Nexus";
    let file = SourceFile::from_location(r"C:\Users\ExampleLocal\Nexus\src\controller.rs", root);
    assert_eq!(file, SourceFile::Controller);
    assert_eq!(
        SourceFile::from_location(r"C:\Users\ExampleLocal\.cargo\registry\src\controller.rs", root),
        SourceFile::Other,
    );
    let line = Line::Fault(Fault {
        place: FaultPlace::App,
        shape: FaultShape::BorrowConflict,
        source: Some(Source { file, line: 728 }),
    });
    let text = line.to_string();
    assert_eq!(text, "Fault in app: borrow conflict at controller.rs:728");
    assert!(!text.contains("ExampleLocal"));
    assert_eq!(clean_line(&text), Some(line));
    assert_eq!(clean_line("Fault in app: panic at C:/Users/ExampleLocal/controller.rs:728"), None);
}

#[test]
fn pc_report_round_trips_closed_output_and_settings_without_phone_fields() {
    let mut report = report(Record::default());
    report.about.device = DeviceKind::Pc;
    report.about.android_auto = None;
    report.about.local_permission = None;
    for output in [
        SoundOutput::Unknown,
        SoundOutput::WaitingForExclusive,
        SoundOutput::Shared { rate_hz: 48_000 },
        SoundOutput::Exclusive { rate_hz: 44_100, bits: 24 },
        SoundOutput::ExclusiveRefused { rate_hz: 48_000 },
        SoundOutput::NoDevice,
    ] {
        report.about.pc = Some(PcDetails {
            music_version: music_version("0.60.2 (C:\\Users\\Example unicode 岬 private@example.invalid)"),
            unreachable: true,
            output,
            exclusive: true,
            bitrate_kbps: 128,
            gapless: true,
            crossfade_ms: 2_500,
            levelling: Some((Level::Quiet, LevelBy::Album)),
            equaliser: true,
            mono: false,
            cache_limit_mb: 2048,
            sharing: false,
            requests: true,
            library_songs: 500,
            downloads: 15,
            pending: 2,
        });
        let text = report.text().unwrap();
        assert_eq!(Report::read(&text).unwrap(), report);
        assert!(text.contains("Music server version: 0.60.2"));
        for raw in ["Example", "private@", "Local network permission", "Android Auto"] {
            assert!(!text.contains(raw), "{text}");
        }
        assert!(Report::read(&text.replace("Music server version: 0.60.2", "Music server version: 0.60.2 device-name")).is_err());
    }
    assert_eq!(music_version("https://1.2.3.example.invalid"), None);
    assert_eq!(music_version("1.2.3 token"), None);
}
