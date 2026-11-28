use super::*;

fn id(n: u8) -> EventId {
    EventId::from_bytes([n; 16])
}

fn event(n: u8, event: Kind) -> Event {
    Event { id: id(n), at_ms: 1_800_000_000_000, sequence: u64::from(n), event }
}

fn start(n: u8) -> Event {
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
        event(7, Kind::Open { version: "1.2.3-rc1".into(), device: DeviceKind::Pc }),
        event(8, Kind::Fault { fault: fault(), count: u32::MAX }),
    ];
    for event in events {
        assert_eq!(Event::read(&event.line().unwrap()), Ok(event));
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
    let from_file = Event::read(&original.line().unwrap()).unwrap();
    let sent = Batch::prefix(2, &[from_file], BATCH_BYTES).unwrap().write().unwrap();
    let incoming = IncomingBatch::read(&sent).unwrap();
    assert_eq!(incoming.events, vec![(original.id, Ok(original))]);
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
        assert_eq!(Event::read(&bytes).is_ok(), good, "{changes:?}");
    }

    use serde_json::json;
    edit(&[("title", json!("é".repeat(NAME_BYTES / 2))), ("album", json!(""))], true);
    for name in ["title", "album", "artist"] {
        edit(&[(name, json!("a".repeat(NAME_BYTES + 1)))], false);
        edit(&[(name, json!("a\tb"))], false);
    }
    edit(&[("song_id", json!("a".repeat(MUSIC_ID_BYTES)))], true);
    edit(&[("song_id", json!("a".repeat(MUSIC_ID_BYTES + 1)))], false);
    edit(&[("song_id", json!(" "))], false);
    edit(&[("from", json!({"kind":"playlist", "id":"a".repeat(MUSIC_ID_BYTES + 1)}))], false);
    edit(&[("from", json!({"kind":"search", "words":"a".repeat(SEARCH_BYTES + 1)}))], false);
    edit(&[("from", json!({"kind":"radio"}))], false);
    for (words, ok) in [("a".repeat(SEARCH_BYTES), true), ("a".repeat(SEARCH_BYTES + 1), false), (" ".into(), false)] {
        assert_eq!(event(1, Kind::Search { words, found: u32::MAX, where_: SearchedIn::Requests }).check().is_ok(), ok);
    }
    for (version, ok) in [("1".repeat(VERSION_BYTES), true), ("1".repeat(VERSION_BYTES + 1), false), ("a/b".into(), false)] {
        assert_eq!(event(1, Kind::Open { version, device: DeviceKind::Phone }).check().is_ok(), ok);
    }
}

#[test]
fn sequence_timestamp_positions_counts_and_references_are_checked() {
    let mut sample = start(1);
    sample.at_ms = TIME_MAX_MS;
    sample.sequence = u64::MAX;
    assert_eq!(sample.check(), Ok(()));
    sample.at_ms += 1;
    assert_eq!(sample.check(), Err(Wrong::Field));
    sample.at_ms = 0;
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
        Kind::Stop { start: id(2), position_ms: Some(3), duration_ms: Some(2), heard_ms: Some(1), why: Stopped::Finished },
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
    assert_eq!(Event::read(unknown.as_bytes()), Ok(start(1)));
    let depth = |n: usize| with(&format!(r#""unknown":{}0{}"#, "[".repeat(n), "]".repeat(n)));
    assert!(Event::read(depth(15).as_bytes()).is_ok());
    for bad in [
        depth(16),
        with(r#""unknown":{"one":1,"on\u0065":2}"#),
        with(r#""id":0"#),
        format!("{good}{{}}"),
        good.replace(r#""position_ms":0"#, r#""position_ms":"0""#),
        good.replace(r#""kind":"start""#, r#""kind":"like""#),
        good.replace(r#""kind":"search","words":"L'été""#, r#""kind":"search","words":false"#),
    ] {
        assert_eq!(Event::read(bad.as_bytes()), Err(Wrong::Form), "{bad}");
    }
    let mut bytes = unknown.into_bytes();
    let index = bytes.iter().position(|b| *b == b'o').unwrap();
    bytes[index] = 0xff;
    assert_eq!(Event::read(&bytes), Err(Wrong::Form));
    let mut bytes = good.into_bytes();
    bytes.resize(FILE_BYTES, b' ');
    assert!(Event::read(&bytes).is_ok());
    bytes.push(b' ');
    assert_eq!(Event::read(&bytes), Err(Wrong::Size));
}

#[test]
fn faults_and_stop_fields_are_objects_and_use_fixed_words() {
    use serde_json::json;
    let mut bad = serde_json::to_value(event(1, Kind::Fault { fault: fault(), count: 1 })).unwrap();
    bad["event"]["fault"] = json!(["playback", "decode"]);
    assert_eq!(Event::from_value(bad.clone()), Err(Wrong::Form));
    bad["event"]["fault"] = json!({"place":"playback","shape":"Secret words"});
    assert_eq!(Event::from_value(bad), Err(Wrong::Form));
    let stop = end_open_start(id(1), None, id(2), 4, 2).unwrap();
    let mut value = serde_json::to_value(stop).unwrap();
    value["event"].as_object_mut().unwrap().remove("heard_ms");
    assert_eq!(Event::from_value(value), Err(Wrong::Form));
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
    let stop = end_open_start(id(2), Some((id(2), 5)), id(3), 6, 3).unwrap();
    assert!(matches!(stop.event, Kind::Stop {
        start, position_ms: Some(5), duration_ms: None, heard_ms: None, why: Stopped::Unknown,
    } if start == id(2)));
    assert_eq!(open_starts([&b, &a]), vec![id(1), id(2)]);
    assert_eq!(open_starts([&stop, &b, &a, &a]), vec![id(1)]);
    let unknown = end_open_start(id(1), Some((id(2), 5)), id(4), 2, 4).unwrap();
    assert!(matches!(unknown.event, Kind::Stop { position_ms: None, .. }));
    assert!(open_starts([&a, &b, &stop, &unknown]).is_empty());
}

#[test]
fn waiting_files_drop_oldest_and_answered_files_age_for_eight_days() {
    let file = |life, bytes, pending, final_at_ms| KeptFile { life, bytes, pending, final_at_ms };
    let files = [
        file(4, 1, 1, None),
        file(5, 2, 2, None),
        file(5, 3, 3, None),
        file(5, WAITING_BYTES - 1, 4, None),
        file(5, FILE_BYTES as u64, 0, Some(10)),
        file(5, FILE_BYTES as u64, 0, Some(11)),
        file(5, FILE_BYTES as u64, 0, Some(KEEP_MS + 11)),
    ];
    assert_eq!(remove_files(&files, 5, KEEP_MS + 10), RemoveFiles { remove: vec![0, 1, 2, 4], dropped: 5 });
    let exactly = [file(5, WAITING_BYTES, 1, None)];
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

    let keys = [
        EventKey { account: "Mira", id: id(1) },
        EventKey { account: "Ari", id: id(1) },
        EventKey { account: "Mira", id: id(2) },
    ];
    assert_eq!(ids_to_delete(&"Mira", &keys).collect::<Vec<_>>(), vec![id(1), id(2)]);
}

#[test]
fn a_batch_fits_count_and_bytes_and_refuses_only_the_bad_identified_event() {
    let events: Vec<_> = (1u64..=501).map(|n| {
        let mut event = start(1);
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&n.to_be_bytes());
        event.id = EventId::from_bytes(bytes);
        event.sequence = n;
        event
    }).collect();
    let batch = Batch::prefix(5, &events, BATCH_BYTES).unwrap();
    assert_eq!(batch.events().len(), BATCH_EVENTS);
    let first_size = Batch::prefix(5, &events[..1], BATCH_BYTES).unwrap().write().unwrap().len();
    assert_eq!(Batch::prefix(5, &events, first_size).unwrap().events(), &events[..1]);
    assert_eq!(Batch::prefix(5, &events, first_size - 1).err(), Some(Wrong::Size));
    assert_eq!(Batch::prefix(5, &[start(1), start(1)], BATCH_BYTES).err(), Some(Wrong::RepeatedId));

    let pair = Batch::prefix(5, &events[..2], BATCH_BYTES).unwrap().write().unwrap();
    let mut value: Value = serde_json::from_slice(&pair).unwrap();
    value["events"][1]["event"]["position_ms"] = Value::Bool(false);
    let wire = serde_json::to_vec(&value).unwrap();
    let batch = IncomingBatch::read(&wire).unwrap();
    assert_eq!(batch.events, vec![(events[0].id, Ok(events[0].clone())), (events[1].id, Err(Wrong::Form))]);
    value["events"][1]["id"] = value["events"][0]["id"].clone();
    assert_eq!(IncomingBatch::read(&serde_json::to_vec(&value).unwrap()).err(), Some(Wrong::RepeatedId));
    value["events"][1]["id"] = Value::Null;
    assert_eq!(IncomingBatch::read(&serde_json::to_vec(&value).unwrap()).err(), Some(Wrong::Form));
    let too_many = serde_json::to_vec(&serde_json::json!({"life":5, "events":events})).unwrap();
    assert_eq!(IncomingBatch::read(&too_many).err(), Some(Wrong::Count));
    let mut too_big = pair;
    too_big.resize(BATCH_BYTES + 1, b' ');
    assert_eq!(IncomingBatch::read(&too_big).err(), Some(Wrong::Size));
}

#[test]
fn a_batch_answer_uses_only_asked_ids_and_cannot_acknowledge_a_prefix() {
    use Outcome::*;
    let sent = Batch::prefix(5, &[start(1), start(2), start(3), start(4)], BATCH_BYTES).unwrap();
    let rows = vec![Answered { id: id(3), outcome: Taken }, Answered { id: id(1), outcome: Refused }, Answered { id: id(4), outcome: NotDealtWith }];
    let wire = BatchAnswer::new(rows).unwrap().write().unwrap();
    let answer = BatchAnswer::read(200, &wire, &sent).unwrap();
    assert_eq!(answer.outcome(id(1)), Refused);
    assert_eq!(answer.outcome(id(2)), NotDealtWith);
    assert_eq!(answer.outcome(id(3)), Taken);
    assert_eq!(answer.outcome(id(4)), NotDealtWith);
    assert_eq!(BatchAnswer::read(201, &wire, &sent), Err(Wrong::Status));
    assert_eq!(BatchAnswer::read(200, &wire[..wire.len() - 1], &sent), Err(Wrong::Form));
    let unknown = format!(r#"{{"answers":[{{"id":"{}","outcome":"taken"}}]}}"#, id(9).to_hex());
    assert_eq!(BatchAnswer::read(200, unknown.as_bytes(), &sent), Err(Wrong::UnaskedId));
    let duplicate = format!(r#"{{"answers":[{{"id":"{0}","outcome":"taken"}},{{"id":"{0}","outcome":"refused"}}]}}"#, id(1).to_hex());
    assert_eq!(BatchAnswer::read(200, duplicate.as_bytes(), &sent), Err(Wrong::RepeatedId));
}

#[test]
fn debug_does_not_print_event_words_or_ids() {
    let event = start(1);
    let shown = format!("{event:?} {:?}", event.id);
    for private in ["L'été", "Mira", "First Flight", "song-1", &event.id.to_hex()] {
        assert!(!shown.contains(private));
    }
}
