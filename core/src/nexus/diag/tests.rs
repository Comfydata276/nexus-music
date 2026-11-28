use super::*;

const AT: u64 = 1_800_000_000_000;
const STARTED: Line = Line::Note(Note::PlayerStarted);
const REFUSED: Line = Line::Note(Note::OtherControllerRefused);

fn report(record: Record) -> Report {
    Report {
        about: About {
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
fn the_daily_timer_and_every_send_require_use_and_consent() {
    let mut schedule = Schedule::new(AT).unwrap();
    assert_eq!(schedule.due(AT + DAY_MS - 1, true, true), None);
    assert_eq!(schedule.due(AT + DAY_MS, false, true), None);
    assert_eq!(schedule.due(AT + DAY_MS, true, false), None);
    assert_eq!(schedule.take_due(AT + DAY_MS, true, true), Some(Due::Daily));
    assert_eq!(schedule.due(AT + 2 * DAY_MS - 1, true, true), None);
    assert_eq!(schedule.due(AT + 2 * DAY_MS, true, true), Some(Due::Daily));
    schedule.note_fault(AT + DAY_MS);
    assert_eq!(schedule.due(AT + 2 * DAY_MS, false, true), None);
    assert_eq!(schedule.due(AT + 2 * DAY_MS, true, false), None);
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
    assert_eq!(record.entries().front().unwrap().line, Line::LastAttempt(trace(Reach::None)));
    assert_eq!(record.entries().get(1).unwrap().line, Line::LastFailure(trace(Reach::None)));
    assert_eq!(record.entries().get(2).unwrap().line, Line::DownloadJobStopped(59));
    assert_eq!(record.entries().back().unwrap().line, Line::ForegroundAfterMs(MAX_FOREGROUND_MS));
    let report = report(record);
    let text = report.text().unwrap();
    assert!(
        text.lines()
            .all(|line| line.chars().count() <= MAX_LINE_CHARS)
    );
    assert_eq!(Report::read(&text).unwrap(), report);
    assert!(
        Upload::new(&report)
            .unwrap()
            .write(CALL_BYTES)
            .unwrap()
            .len()
            < CALL_BYTES
    );
    assert_eq!(clean_line(&" ".repeat(MAX_LINE_CHARS + 1)), None);
}

#[test]
fn the_report_says_when_and_how_often_and_is_the_only_text_uploaded() {
    let mut record = Record::default();
    record.note(AT, STARTED);
    record.note(AT + 61_000, STARTED);
    let report = report(record);
    let shown = report.text().unwrap();
    assert!(shown.ends_with(
        "  2027-01-15 08:01:01  Player started  (2 times since 2027-01-15 08:00:00)"
    ));
    assert!(!shown.contains('\t'));
    let upload = Upload::new(&report).unwrap();
    let wire = upload.write(CALL_BYTES).unwrap();
    assert_eq!(upload.write(wire.len()).unwrap(), wire);
    assert_eq!(upload.write(wire.len() - 1), Err(Wrong::Size));
    assert_eq!(Upload::read(&wire).unwrap().text(), shown);
    let value: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(value, serde_json::json!({"text":shown}));
    let empty = report::empty();
    assert!(empty.text().unwrap().ends_with(EMPTY));
    assert_eq!(Report::read(&empty.text().unwrap()).unwrap(), empty);
}

mod report {
    use super::*;

    pub fn empty() -> Report {
        super::report(Record::default())
    }
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
    for &(place, _) in PLACES {
        for &(shape, _) in SHAPES {
            let line = Line::Fault(Fault { place, shape });
            assert_eq!(clean_line(&line.to_string()), Some(line));
            record.note(AT, line);
        }
    }
    let report = report(record);
    let shown = report.text().unwrap();
    let wire = Upload::new(&report).unwrap().write(CALL_BYTES).unwrap();
    let uploaded = Upload::read(&wire).unwrap();
    assert_eq!(uploaded.text(), shown);
    assert_eq!(
        report.record.entries().len(),
        1 + PLACES.len() * SHAPES.len()
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
        let wire = serde_json::to_vec(&serde_json::json!({"text":forged})).unwrap();
        assert_eq!(Upload::read(&wire).err(), Some(Wrong::Form));
    }
}

#[test]
fn the_upload_requires_whole_utf8_bounded_unique_json_and_known_types() {
    let wire = Upload::new(&report::empty())
        .unwrap()
        .write(CALL_BYTES)
        .unwrap();
    let json = std::str::from_utf8(&wire).unwrap();
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
    let mut at_limit = wire.clone();
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
fn done_checks_status_and_carries_no_report() {
    assert_eq!(read_done(200, DONE), Ok(()));
    assert_eq!(read_done(200, br#"{"done":true,"later":1}"#), Ok(()));
    assert_eq!(read_done(201, DONE), Err(Wrong::Status));
    for wrong in [
        br#"{"done":false}"#.as_slice(),
        br#"{"done":"true"}"#,
        br#"{"done":true,"done":true}"#,
        b"{",
        b"{}",
    ] {
        assert_eq!(read_done(200, wrong), Err(Wrong::Form));
    }
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
    let fault = Line::Fault(Fault { place: FaultPlace::Foreground, shape: FaultShape::Refused });
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
