use super::*;
use crate::nexus::account::{Receipt, Refusal, RefusedWithReceipt, Stamp, StampTag};
use crate::nexus::proto::SPEAKS;
use serde_json::{Value, json};

fn head() -> RequestHead {
    RequestHead {
        speaks: SPEAKS,
        restore: 9,
    }
}

fn receipt(life: u64) -> Receipt {
    Receipt {
        stamp: Stamp {
            backend: BackendId::from_bytes([5; 16]),
            restore: 9,
            account: AccountId::new(3).unwrap(),
            life,
            change: 12,
            tag: StampTag::from_bytes([6; 32]),
        },
        music_login_count: 0,
    }
}

fn read_value(value: Value) -> Result<Placed, Wrong> {
    Placed::read(&serde_json::to_vec(&value).unwrap())
}

fn id(n: u8) -> EventId {
    EventId::from_bytes([n; 16])
}

fn event(n: u8, event: Kind) -> Placed {
    Placed {
        record: Record { id: id(n), at_ms: 1_800_000_000_000, kind: event },
        sequence: u64::from(n),
    }
}

fn start(n: u8) -> Placed {
    event(n, Kind::Start {
        song_id: "song-1".into(),
        title: "L'été 🐦".into(),
        artist: "Mira Bell".into(),
        album: "First Flight".into(),
        from: PlayedFrom::Search { words: "L'été".into() },
        began: Began::PressedHere,
        sound: Sound::Streamed,
        position_ms: 0,
    })
}

fn fault() -> Fault {
    Fault { place: FaultPlace::Playback, shape: FaultShape::Decode }
}

#[test]
fn all_eight_events_round_trip() {
    let events = [
        start(1),
        event(2, Kind::Stop {
            start: id(1), position_ms: Some(22), duration_ms: Some(30), heard_ms: Some(42),
            why: Stopped::Error { fault: fault() },
        }),
        event(3, Kind::Pause { start: id(1), position_ms: 25 }),
        event(4, Kind::Resume { start: id(1), position_ms: 25 }),
        event(5, Kind::Seek { start: id(1), from_ms: 25, to_ms: 0 }),
        event(6, Kind::Search { words: " L'été 🐦 ".into(), found: 0, where_: SearchedIn::Library }),
        event(7, Kind::Open { version: Version::parse("1.2.3").unwrap(), device: DeviceKind::Pc }),
        event(8, Kind::Fault { fault: fault(), count: u32::MAX }),
    ];
    for event in events {
        assert_eq!(Placed::read(&event.line().unwrap()), Ok(event));
    }
}

#[test]
fn id_has_one_spelling_and_retry_keeps_it() {
    let id = id(0xab);
    assert_eq!(id.to_hex(), "ab".repeat(16));
    assert_eq!(EventId::from_hex(&id.to_hex()), Some(id));
    assert_eq!(EventId::from_hex(&id.to_hex().to_uppercase()), None);
    assert_eq!(EventId::from_hex("ÿÿÿÿÿÿÿÿÿÿÿÿÿÿÿÿ"), None);
    assert_eq!(EventId::from_hex("abcd"), None);
    let original = start(1);
    let from_file = Placed::read(&original.line().unwrap()).unwrap();
    let sent = Batch::prefix(head(), 2, &[from_file], BATCH_BYTES)
        .unwrap()
        .write()
        .unwrap();
    let incoming = IncomingBatch::read(sent.bytes()).unwrap();
    assert_eq!(incoming.events, vec![(original.record.id, Ok(original))]);
}

#[test]
fn bounds_cover_names_ids_search_and_version() {
    fn edit(changes: &[(&str, Value)], good: bool) {
        let mut value = serde_json::to_value(start(1)).unwrap();
        let fields = value["event"].as_object_mut().unwrap();
        for (field, value) in changes {
            fields.insert((*field).into(), value.clone());
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(Placed::read(&bytes).is_ok(), good, "{changes:?}");
    }

    use serde_json::json;
    edit(
        &[
            ("title", json!("é".repeat(TEXT_CHARS))),
            ("album", json!("")),
        ],
        true,
    );
    for name in ["title", "album", "artist"] {
        edit(&[(name, json!("a".repeat(TEXT_CHARS + 1)))], false);
        edit(&[(name, json!("a\tb"))], false);
    }
    edit(&[("song_id", json!("a".repeat(TEXT_CHARS)))], true);
    edit(&[("song_id", json!("a".repeat(TEXT_CHARS + 1)))], false);
    edit(&[("song_id", json!(" "))], false);
    edit(
        &[(
            "from",
            json!({"kind":"playlist", "id":"a".repeat(TEXT_CHARS + 1)}),
        )],
        false,
    );
    edit(
        &[(
            "from",
            json!({"kind":"search", "words":"a".repeat(TEXT_CHARS + 1)}),
        )],
        false,
    );
    edit(&[("from", json!({"kind":"radio"}))], false);
    for (words, ok) in [
        ("a".repeat(TEXT_CHARS), true),
        ("a".repeat(TEXT_CHARS + 1), false),
        (" ".into(), false),
    ] {
        assert_eq!(
            event(
                1,
                Kind::Search {
                    words,
                    found: u32::MAX,
                    where_: SearchedIn::Requests
                }
            )
            .check()
            .is_ok(),
            ok
        );
    }
    let value = serde_json::to_value(event(
        1,
        Kind::Open {
            version: Version::parse("1.2.3").unwrap(),
            device: DeviceKind::Phone,
        },
    ))
    .unwrap();
    for version in ["1.2", "1.2.3-rc1", "a/b"] {
        let mut bad = value.clone();
        bad["event"]["version"] = json!(version);
        assert_eq!(read_value(bad), Err(Wrong::Form));
    }
}

#[test]
fn sequence_timestamp_positions_counts_and_references_are_checked() {
    let mut sample = start(1);
    sample.record.at_ms = TIME_MAX_MS;
    sample.sequence = u64::MAX;
    assert_eq!(sample.check(), Ok(()));
    sample.record.at_ms += 1;
    assert_eq!(sample.check(), Err(Wrong::Field));
    sample.record.at_ms = 0;
    sample.sequence = 0;
    assert_eq!(sample.check(), Err(Wrong::Field));
    assert_eq!(next_sequence(0), Some(1));
    assert_eq!(next_sequence(u64::MAX), None);

    for kind in [
        Kind::Pause { start: id(2), position_ms: PLAY_MAX_MS },
        Kind::Resume { start: id(2), position_ms: PLAY_MAX_MS },
        Kind::Seek { start: id(2), from_ms: PLAY_MAX_MS, to_ms: 0 },
    ] {
        assert_eq!(event(1, kind).check(), Ok(()));
    }
    for kind in [
        Kind::Pause { start: id(1), position_ms: 0 },
        Kind::Resume { start: id(2), position_ms: PLAY_MAX_MS + 1 },
        Kind::Seek { start: id(2), from_ms: 0, to_ms: PLAY_MAX_MS + 1 },
        Kind::Stop { start: id(2), position_ms: Some(1002), duration_ms: Some(2), heard_ms: Some(1), why: Stopped::Finished },
        Kind::Stop { start: id(2), position_ms: Some(1), duration_ms: Some(2), heard_ms: None, why: Stopped::Next },
        Kind::Stop { start: id(2), position_ms: Some(1), duration_ms: Some(2), heard_ms: Some(PLAY_MAX_MS + 1), why: Stopped::Next },
        Kind::Fault { fault: fault(), count: 0 },
    ] {
        assert_eq!(event(1, kind).check(), Err(Wrong::Field));
    }
}

#[test]
fn json_is_whole_utf8_unique_and_bounded_even_in_unknown_fields() {
    let good = String::from_utf8(start(1).line().unwrap()).unwrap();
    let with = |fields: &str| format!("{{{fields},{}", good.strip_prefix('{').unwrap());
    let unknown = with(r#""unknown":{"one":1}"#);
    assert_eq!(Placed::read(unknown.as_bytes()), Ok(start(1)));
    let depth = |n: usize| with(&format!(r#""unknown":{}0{}"#, "[".repeat(n), "]".repeat(n)));
    assert!(Placed::read(depth(15).as_bytes()).is_ok());
    for bad in [
        depth(16),
        with(r#""unknown":{"one":1,"on\u0065":2}"#),
        with(r#""id":0"#),
        format!("{good}{{}}"),
        good.replace(r#""position_ms":0"#, r#""position_ms":"0""#),
        good.replace(r#""kind":"start""#, r#""kind":"like""#),
        good.replace(r#""kind":"search","words":"L'été""#, r#""kind":"search","words":false"#),
    ] {
        assert_eq!(Placed::read(bad.as_bytes()), Err(Wrong::Form), "{bad}");
    }
    let mut bytes = unknown.into_bytes();
    let index = bytes.iter().position(|b| *b == b'o').unwrap();
    bytes[index] = 0xff;
    assert_eq!(Placed::read(&bytes), Err(Wrong::Form));
    let mut bytes = good.into_bytes();
    bytes.resize(EVENT_BYTES, b' ');
    assert!(Placed::read(&bytes).is_ok());
    bytes.push(b' ');
    assert_eq!(Placed::read(&bytes), Err(Wrong::Size));
}

#[test]
fn faults_and_stop_fields_are_objects_and_use_fixed_words() {
    use serde_json::json;
    let mut bad = serde_json::to_value(event(
        1,
        Kind::Fault {
            fault: fault(),
            count: 1,
        },
    ))
    .unwrap();
    bad["event"]["fault"] = json!(["playback", "decode"]);
    assert_eq!(read_value(bad.clone()), Err(Wrong::Form));
    bad["event"]["fault"] = json!({"place":"playback","shape":"Secret words"});
    assert_eq!(read_value(bad), Err(Wrong::Form));
    let stop = end_open_start(id(1), None, id(2), 4)
        .unwrap()
        .place(2)
        .unwrap();
    let mut value = serde_json::to_value(stop).unwrap();
    value["event"].as_object_mut().unwrap().remove("heard_ms");
    assert_eq!(read_value(value), Err(Wrong::Form));
}

#[test]
fn journal_skips_bad_and_torn_lines_without_losing_complete_ones() {
    let mut file = file_header(4).unwrap();
    file.extend(start(1).line().unwrap());
    file.extend(b"{\xff\n");
    file.extend(start(2).line().unwrap());
    let complete_json = start(3).line().unwrap();
    file.extend(complete_json.strip_suffix(b"\n").unwrap());
    let read = read_file(&file).unwrap();
    assert_eq!(read.life, 4);
    assert_eq!(read.events, vec![start(1), start(2)]);
    assert_eq!(read.skipped_lines, 2);

    let next = append(4, Some(FileTail { life: 4, bytes: file.len(), last_byte: b'}' }), &start(4)).unwrap();
    assert!(next.new_file);
    assert_eq!(read_file(&next.bytes).unwrap().events, vec![start(4)]);
    assert!(read_file(br#"{"life":4}"#).is_err());
    assert!(read_file(b"{\"life\":4,\"life\":5}\n").is_err());
    assert_eq!(read_file(&vec![b' '; FILE_BYTES + 1]).err(), Some(Wrong::Size));
}

#[test]
fn append_rolls_only_at_size_life_or_torn_boundary() {
    let line = start(1).line().unwrap();
    let fits = FILE_BYTES - line.len();
    for (bytes, life, last_byte, new_file) in [
        (fits, 4, b'\n', false),
        (fits + 1, 4, b'\n', true),
        (fits, 3, b'\n', true),
        (fits, 4, b'}', true),
    ] {
        let write = append(4, Some(FileTail { life, bytes, last_byte }), &start(1)).unwrap();
        assert_eq!(write.new_file, new_file);
        if new_file {
            assert_eq!(read_file(&write.bytes).unwrap().life, 4);
        } else {
            assert_eq!(write.bytes, line);
        }
    }
}

#[test]
fn recovered_stops_use_only_matching_saved_progress_and_close_once() {
    let a = start(1);
    let b = start(2);
    let stop = end_open_start(id(2), Some((id(2), 5)), id(3), 6).unwrap().place(3).unwrap();
    assert!(matches!(stop.record.kind, Kind::Stop {
        start, position_ms: Some(5), duration_ms: None, heard_ms: None, why: Stopped::Unknown,
    } if start == id(2)));
    assert_eq!(open_starts([&b, &a]), vec![id(1), id(2)]);
    assert_eq!(open_starts([&stop, &b, &a, &a]), vec![id(1)]);
    let unknown = end_open_start(id(1), Some((id(2), 5)), id(4), 2).unwrap().place(4).unwrap();
    assert!(matches!(unknown.record.kind, Kind::Stop { position_ms: None, .. }));
    assert!(open_starts([&a, &b, &stop, &unknown]).is_empty());
}

#[test]
fn waiting_files_drop_oldest_and_answered_files_age_for_eight_days() {
    let file = |life, bytes, events, pending, final_at_ms| KeptFile { life, bytes, events, pending, final_at_ms };
    let files = [
        file(4, 1, 1, 1, None),
        file(5, 2, 3, 2, None),
        file(5, 3, 4, 3, None),
        file(5, WAITING_BYTES - 1, 4, 4, None),
        file(5, FILE_BYTES as u64, 1, 0, Some(10)),
        file(5, FILE_BYTES as u64, 1, 0, Some(11)),
        file(5, FILE_BYTES as u64, 1, 0, Some(KEEP_MS + 11)),
        file(5, 11, 0, 0, None),
    ];
    assert_eq!(remove_files(&files, 5, KEEP_MS + 10), RemoveFiles { remove: vec![0, 1, 2, 4, 7], dropped: 5 });
    let exactly = [file(5, WAITING_BYTES, 1, 1, None)];
    assert_eq!(remove_files(&exactly, 5, 0), RemoveFiles { remove: vec![], dropped: 0 });
}

#[test]
fn account_life_is_checked_before_idempotence_and_deletion_is_account_wide() {
    assert_eq!(intake(4, 3, Existing::SameDevice, true), Intake::Answer(Outcome::Refused));
    assert_eq!(intake(4, 3, Existing::None, true), Intake::Answer(Outcome::Refused));
    assert_eq!(intake(4, 5, Existing::None, true), Intake::Answer(Outcome::Refused));
    assert_eq!(intake(4, 4, Existing::None, true), Intake::Insert);
    assert_eq!(intake(4, 4, Existing::SameDevice, false), Intake::Answer(Outcome::Taken));
    assert_eq!(intake(4, 4, Existing::AnotherDevice, true), Intake::Answer(Outcome::Refused));
    assert_eq!(intake(4, 4, Existing::None, false), Intake::Answer(Outcome::NotDealtWith));

    let mira = AccountId::new(1).unwrap();
    let ari = AccountId::new(2).unwrap();
    let keys = [
        EventKey { account: mira, id: id(1) },
        EventKey { account: ari, id: id(1) },
        EventKey { account: mira, id: id(2) },
    ];
    assert_eq!(ids_to_delete(mira, &keys).collect::<Vec<_>>(), vec![id(1), id(2)]);
}

fn numbered_event(n: u64) -> Placed {
    let mut event = start(1);
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    event.record.id = EventId::from_bytes(bytes);
    event.sequence = n;
    event
}

#[test]
fn a_batch_fits_count_and_bytes_and_refuses_only_the_bad_identified_event() {
    let events: Vec<_> = (1..=501).map(numbered_event).collect();
    let batch = Batch::prefix(head(), 5, &events, BATCH_BYTES).unwrap();
    assert_eq!(batch.events().len(), BATCH_EVENTS);
    let first_size = Batch::prefix(head(), 5, &events[..1], BATCH_BYTES)
        .unwrap()
        .write()
        .unwrap()
        .bytes()
        .len();
    assert_eq!(
        Batch::prefix(head(), 5, &events, first_size)
            .unwrap()
            .events(),
        &events[..1]
    );
    assert_eq!(
        Batch::prefix(head(), 5, &events, first_size - 1).err(),
        Some(Wrong::Size)
    );


    let pair = Batch::prefix(head(), 5, &events[..2], BATCH_BYTES)
        .unwrap()
        .write()
        .unwrap();
    let mut value: Value = serde_json::from_slice(pair.bytes()).unwrap();
    value["events"][1]["event"]["position_ms"] = Value::Bool(false);
    let wire = serde_json::to_vec(&value).unwrap();
    let batch = IncomingBatch::read(&wire).unwrap();
    assert_eq!(
        batch.events,
        vec![
            (events[0].record.id, Ok(events[0].clone())),
            (events[1].record.id, Err(Wrong::Form))
        ]
    );
    value["events"][1]["id"] = value["events"][0]["id"].clone();
    assert_eq!(
        IncomingBatch::read(&serde_json::to_vec(&value).unwrap()).err(),
        Some(Wrong::Form)
    );
    value["events"][1]["id"] = Value::Null;
    assert_eq!(
        IncomingBatch::read(&serde_json::to_vec(&value).unwrap()).err(),
        Some(Wrong::Form)
    );
    let too_many = serde_json::to_vec(&json!({"head":head(), "life":5, "events":events})).unwrap();
    assert_eq!(IncomingBatch::read(&too_many).err(), Some(Wrong::Form));
    let mut too_big = pair.bytes().to_vec();
    too_big.resize(BATCH_BYTES + 1, b' ');
    assert_eq!(IncomingBatch::read(&too_big).err(), Some(Wrong::Size));
}

#[test]
fn oversized_event_is_refused_without_losing_other_batch_entries() {
    let pair = Batch::prefix(head(), 5, &[start(1), start(2)], BATCH_BYTES)
        .unwrap()
        .write()
        .unwrap();
    let mut value: Value = serde_json::from_slice(pair.bytes()).unwrap();
    value["events"][1]["future"] = json!("x".repeat(EVENT_BYTES));
    let bytes = serde_json::to_vec(&value).unwrap();
    let batch = IncomingBatch::read(&bytes).unwrap();
    assert_eq!(
        batch.events,
        vec![(id(1), Ok(start(1))), (id(2), Err(Wrong::Size))]
    );
}

#[test]
fn a_batch_answer_uses_the_shared_stamp_and_only_asked_ids() {
    use Outcome::*;
    let sent = Batch::prefix(
        head(),
        5,
        &[start(1), start(2), start(3), start(4)],
        BATCH_BYTES,
    )
    .unwrap();
    let rows = vec![
        Answered {
            id: id(3),
            outcome: Taken,
        },
        Answered {
            id: id(1),
            outcome: Refused,
        },
        Answered {
            id: id(4),
            outcome: NotDealtWith,
        },
    ];
    let done = Reply::Done {
        result: BatchAnswer::new(rows).unwrap(),
        receipt: receipt(5),
    };
    let body = write_reply(&done).unwrap();
    let wire = body.bytes();
    let Reply::Done {
        result: answer,
        receipt: stamp,
    } = read_reply(200, wire, &sent).unwrap()
    else {
        panic!("expected a stamped success");
    };
    assert_eq!(stamp, receipt(5));
    assert_eq!(answer.outcome(id(1)), Refused);
    assert_eq!(answer.outcome(id(2)), NotDealtWith);
    assert_eq!(answer.outcome(id(3)), Taken);
    assert_eq!(answer.outcome(id(4)), NotDealtWith);
    assert_eq!(read_reply(201, wire, &sent), Err(Wrong::Status));
    assert_eq!(
        read_reply(200, &wire[..wire.len() - 1], &sent),
        Err(Wrong::Form)
    );
    let mut value: Value = serde_json::from_slice(wire).unwrap();
    value["done"]["result"]["answers"][0]["id"] = json!(id(9).to_hex());
    assert_eq!(
        read_reply(200, &serde_json::to_vec(&value).unwrap(), &sent),
        Err(Wrong::UnaskedId)
    );
    value["done"]["result"]["answers"][0]["id"] =
        value["done"]["result"]["answers"][1]["id"].clone();
    assert_eq!(
        read_reply(200, &serde_json::to_vec(&value).unwrap(), &sent),
        Err(Wrong::Form)
    );
    value["done"].as_object_mut().unwrap().remove("receipt");
    assert_eq!(
        read_reply(200, &serde_json::to_vec(&value).unwrap(), &sent),
        Err(Wrong::Form)
    );
}

#[test]
fn envelope_head_is_required_and_counted_in_the_request_limit() {
    let sent = Batch::prefix(head(), 5, &[start(1)], BATCH_BYTES).unwrap();
    assert_eq!(sent.head(), &head());
    let encoded = sent.write().unwrap();
    let incoming = IncomingBatch::read(encoded.bytes()).unwrap();
    assert_eq!(incoming.head, head());
    let mut at_limit = encoded.bytes().to_vec();
    at_limit.resize(BATCH_BYTES, b' ');
    assert!(IncomingBatch::read(&at_limit).is_ok());
    at_limit.push(b' ');
    assert_eq!(IncomingBatch::read(&at_limit).err(), Some(Wrong::Size));

    let mut malformed: Value = serde_json::from_slice(encoded.bytes()).unwrap();
    malformed["head"]["restore"] = json!("9");
    assert_eq!(
        IncomingBatch::read(&serde_json::to_vec(&malformed).unwrap()).err(),
        Some(Wrong::Form)
    );
    malformed.as_object_mut().unwrap().remove("head");
    assert_eq!(
        IncomingBatch::read(&serde_json::to_vec(&malformed).unwrap()).err(),
        Some(Wrong::Form)
    );
}

#[test]
fn a_batch_reply_uses_the_list_ceiling_but_a_refusal_stays_small() {
    let events: Vec<_> = (1..=BATCH_EVENTS as u64).map(numbered_event).collect();
    let sent = Batch::prefix(head(), 5, &events, BATCH_BYTES).unwrap();
    let rows: Vec<_> = events
        .iter()
        .map(|event| Answered {
            id: event.record.id,
            outcome: Outcome::Taken,
        })
        .collect();
    let answer = Reply::Done {
        result: BatchAnswer::new(rows).unwrap(),
        receipt: receipt(5),
    };
    let mut wide = write_reply(&answer).unwrap().bytes().to_vec();
    assert!(wide.len() > account::SMALL_REPLY_MAX_BYTES);
    assert_eq!(
        read_reply(200, &wide, &sent),
        Ok(Reply::Done {
            result: BatchAnswer::new(
                events
                    .iter()
                    .map(|event| Answered {
                        id: event.record.id,
                        outcome: Outcome::Taken
                    })
                    .collect()
            )
            .unwrap(),
            receipt: receipt(5),
        })
    );
    wide.resize(BATCH_REPLY_BYTES, b' ');
    assert_eq!(read_reply(200, &wide, &sent), Ok(answer));
    wide.push(b' ');
    assert_eq!(read_reply(200, &wide, &sent), Err(Wrong::Size));

    let refusal = Reply::Refused(Refusal::NotDealtWith);
    let mut small = write_reply(&refusal).unwrap().bytes().to_vec();
    small.resize(account::SMALL_REPLY_MAX_BYTES, b' ');
    assert_eq!(read_reply(503, &small, &sent), Ok(refusal));
    assert_eq!(read_reply(200, &small, &sent), Err(Wrong::Status));
    small.push(b' ');
    assert_eq!(read_reply(503, &small, &sent), Err(Wrong::Size));
}

#[test]
fn debug_does_not_print_event_words_or_ids() {
    let event = start(1);
    let shown = format!("{event:?} {:?}", event.record.id);
    for private in ["L'été", "Mira", "First Flight", "song-1", &event.record.id.to_hex()] {
        assert!(!shown.contains(private));
    }
}

#[test]
fn start_again_replays_taken_events_and_never_retries_final_refusals() {
    assert!(Outcome::NotDealtWith.should_send(false));
    assert!(Outcome::NotDealtWith.should_send(true));
    assert!(!Outcome::Taken.should_send(false));
    assert!(Outcome::Taken.should_send(true));
    assert!(!Outcome::Refused.should_send(false));
    assert!(!Outcome::Refused.should_send(true));
}

#[test]
fn display_cleaning_removes_controls_and_preserves_ordinary_scripts() {
    let bidi = ['\u{061c}', '\u{200e}', '\u{200f}'].into_iter()
        .chain('\u{202a}'..='\u{202e}').chain('\u{2066}'..='\u{2069}');
    let controls = ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}').chain(bidi);
    for ch in controls {
        assert_eq!(clean_text(&format!("A{ch}B")), "AB", "U+{:04X}", ch as u32);
    }
    assert_eq!(clean_text("A \u{2028}\u{2029}  \u{202e} B"), "A B");
    for title in ["تملي معاك", "שיר לשלום", "春よ、来い", "می\u{200c}روم"] {
        assert_eq!(clean_text(title), title);
    }
    let long = "音".repeat(TEXT_CHARS + 1);
    assert_eq!(clean_display_text(&long), long);
    assert_eq!(clean_text(&format!("\u{2066}{long}")).chars().count(), TEXT_CHARS);
}

#[test]
fn the_player_makes_and_cleans_a_record_before_the_writer_places_it() {
    let event_id = id(2);
    let kind = Kind::Search {
        words: format!("\t{}", "é".repeat(300)),
        found: 0,
        where_: SearchedIn::Library,
    };
    let record = Record::new(event_id, TIME_MAX_MS, kind).unwrap();
    assert_eq!(record.id(), event_id);
    assert_eq!(record.at_ms(), TIME_MAX_MS);
    assert!(record.kind() == &Kind::Search {
        words: "é".repeat(TEXT_CHARS),
        found: 0,
        where_: SearchedIn::Library,
    });
    assert_eq!(record.clone().place(0), Err(Wrong::Field));
    let placed = record.clone().place(u64::MAX).unwrap();
    assert_eq!(placed.sequence(), u64::MAX);
    assert_eq!(placed.record(), &record);
    assert_eq!(Placed::read(&placed.line().unwrap()).unwrap(), placed);
}

#[test]
fn long_display_fields_are_trimmed_but_untrimmed_wire_events_are_refused() {
    let song = "音".repeat(TEXT_CHARS);
    let source = "🎵".repeat(TEXT_CHARS);
    let title = "🎵".repeat(TEXT_CHARS);
    let artist = "楽".repeat(TEXT_CHARS);
    let album = "\"\\".repeat(TEXT_CHARS / 2);
    for (from, known_source) in [
        (PlayedFrom::Album { id: source.clone() }, Some(&source)),
        (PlayedFrom::Search { words: "曲".repeat(TEXT_CHARS) }, None),
    ] {
        let kind = Kind::Start {
            song_id: song.clone(),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            from,
            began: Began::PressedHere,
            sound: Sound::Streamed,
            position_ms: 0,
        };
        let placed = Record::new(id(1), TIME_MAX_MS, kind.clone()).unwrap().place(u64::MAX).unwrap();
        let Kind::Start { song_id, title: shortened, artist: kept_artist, album: kept_album, from, .. } = placed.record().kind() else {
            panic!("expected a start");
        };
        assert_eq!(song_id, &song);
        assert!(shortened.len() < title.len());
        assert!(title.starts_with(shortened));
        assert!(artist.starts_with(kept_artist));
        assert!(album.starts_with(kept_album));
        match (from, known_source) {
            (PlayedFrom::Album { id }, Some(expected)) => assert_eq!(id, expected),
            (PlayedFrom::Search { words }, None) => assert!(!words.trim().is_empty() && words.chars().all(|ch| ch == '曲')),
            _ => panic!("expected original source kind"),
        }
        let line = placed.line().unwrap();
        assert!(line.len() <= EVENT_BYTES + 1);
        assert_eq!(Placed::read(line.strip_suffix(b"\n").unwrap()), Ok(placed));

        let oversized = event(1, kind);
        assert_eq!(oversized.line(), Err(Wrong::Size));
        assert_eq!(Batch::prefix(head(), 1, &[oversized], BATCH_BYTES).err(), Some(Wrong::Size));
    }
}

#[test]
fn a_finished_song_may_run_through_the_fraction_truncated_from_its_duration() {
    let finished = |position_ms| {
        Record::new(
            id(2),
            3,
            Kind::Stop {
                start: id(1),
                position_ms: Some(position_ms),
                duration_ms: Some(215_000),
                heard_ms: Some(position_ms),
                why: Stopped::Finished,
            },
        )
    };
    let end = finished(215_480).unwrap().place(2).unwrap();
    assert_eq!(Placed::read(&end.line().unwrap()).unwrap(), end);
    assert!(finished(215_999).is_ok());
    let late = finished(220_480).unwrap().place(3).unwrap();
    assert!(matches!(late.record().kind(), Kind::Stop {
        position_ms: Some(215_999), duration_ms: Some(215_000),
        heard_ms: Some(220_480), why: Stopped::Finished, ..
    }));
    let mut wire = serde_json::to_value(&late).unwrap();
    wire["event"]["position_ms"] = json!(216_000);
    assert_eq!(read_value(wire), Err(Wrong::Form));
}

#[test]
fn unknown_or_missing_journal_formats_cannot_be_appended_as_this_format() {
    assert!(read_file(b"{\"life\":1}\n").is_err());
    assert!(read_file(b"{\"format\":2,\"life\":1}\n").is_err());
    assert!(read_file(&file_header(1).unwrap()).is_ok());
}

#[test]
fn taken_files_have_a_separate_cap_and_aging_does_not_count_as_loss() {
    let file = |bytes, pending, final_at_ms| KeptFile { life: 1, bytes, events: 2, pending, final_at_ms };
    let files = [
        file(2, 0, Some(0)),
        file(2, 0, Some(1)),
        file(WAITING_BYTES, 1, None),
        file(TAKEN_BYTES, 0, Some(1)),
    ];
    assert_eq!(remove_files(&files, 1, KEEP_MS), RemoveFiles { remove: vec![0, 1], dropped: 0 });
}

#[test]
fn journal_loss_totals_are_separate_and_saturate_for_persistence() {
    let mut counts = LossCounts::default();
    counts.add(Loss::LineFull, 2);
    counts.add(Loss::FilesFull, u64::MAX);
    counts.add(Loss::FilesFull, 1);
    counts.add(Loss::Refused, 4);
    assert_eq!(counts, LossCounts { line_full: 2, files_full: u64::MAX, refused: 4 });
    let saved = serde_json::to_vec(&counts).unwrap();
    assert_eq!(serde_json::from_slice::<LossCounts>(&saved).unwrap(), counts);
}

#[test]
fn later_duplicate_journal_ids_do_not_block_the_batch() {
    let first = start(1);
    let mut duplicate = first.clone();
    duplicate.sequence += 1;
    let later = start(3);
    let mut journal = vec![first.clone(); BATCH_EVENTS];
    journal.push(duplicate);
    journal.push(later.clone());
    let batch = Batch::prefix(head(), 1, &journal, BATCH_BYTES).unwrap();
    assert_eq!(batch.events(), &[first, later]);
}

#[test]
fn a_stamp_for_another_life_returns_no_event_outcomes() {
    let sent = Batch::prefix(head(), 4, &[start(1), start(2)], BATCH_BYTES).unwrap();
    let refused = BatchAnswer::new(vec![
        Answered { id: id(1), outcome: Outcome::Refused },
        Answered { id: id(2), outcome: Outcome::Refused },
    ]).unwrap();
    let done = Reply::Done { result: refused, receipt: receipt(5) };
    let refusal = Reply::RefusedWithReceipt(
        RefusedWithReceipt::new(Refusal::NotDealtWith, receipt(3)).unwrap(),
    );
    for (reply, life) in [(done, 5), (refusal, 3)] {
        let bytes = write_reply(&reply).unwrap();
        assert_eq!(read_reply(reply.status(), bytes.bytes(), &sent), Err(Wrong::LifeMoved { life }));
    }
}

#[test]
fn stats_reads_only_bounded_pages_of_starts() {
    let page = StatsPage {
        starts: 1,
        recent: vec![RecentPlay { device: DeviceId::new(32).unwrap(), event: start(1) }],
        next: None,
    };
    let reply = Reply::Done { result: page, receipt: receipt(5) };
    let bytes = account::encode(&reply).unwrap();
    assert_eq!(read_stats_reply(200, bytes.bytes()), Ok(reply));
    let original: Value = serde_json::from_slice(bytes.bytes()).unwrap();
    let read = |value: &Value| read_stats_reply(200, &serde_json::to_vec(value).unwrap());
    let path = "/done/result";
    let mut invalid = original.clone();
    invalid.pointer_mut(path).unwrap()["recent"][0]["event"] = json!(event(2, Kind::Fault { fault: fault(), count: 1 }));
    assert_eq!(read(&invalid), Err(BadMessage::Form));

    for broken in [json!(0), json!("1"), json!(null)] {
        let mut invalid = original.clone();
        invalid.pointer_mut(path).unwrap()["recent"][0]["device"] = broken;
        assert_eq!(read(&invalid), Err(BadMessage::Form));
    }
    let mut invalid = original.clone();
    invalid.pointer_mut(path).unwrap()["next"] = json!(1);
    assert_eq!(read(&invalid), Err(BadMessage::Form));

    let row = original.pointer(path).unwrap()["recent"][0].clone();
    let mut invalid = original;
    invalid.pointer_mut(path).unwrap()["recent"] = json!(vec![row; STATS_PAGE + 1]);
    invalid.pointer_mut(path).unwrap()["starts"] = json!(STATS_PAGE + 1);
    assert_eq!(read(&invalid), Err(BadMessage::Form));

    let refusal: StatsReply = Reply::Refused(Refusal::NotDealtWith);
    let mut bytes = account::encode(&refusal).unwrap().bytes().to_vec();
    assert_eq!(read_stats_reply(503, &bytes), Ok(refusal));
    assert!(read_stats_reply(200, &bytes).is_err());
    bytes.resize(account::SMALL_REPLY_MAX_BYTES + 1, b' ');
    assert_eq!(read_stats_reply(503, &bytes), Err(BadMessage::TooLarge));
}
