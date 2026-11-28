use super::*;
use crate::diagnostics::{ReportStore, Session, WORKER_FIXTURE, export_report, preview_text, record_path, tests::TestFolder};
use nexus_core::nexus::{
    account::{AccountId, DeviceKind, Name, Session as NexusSession},
    client::{AnswerBody, App, Ask, Client, Do, Given, JournalDo, Keep, Random, Saved},
    proto::{self, SPEAKS, Version},
};
use std::{fs, sync::mpsc, time::Duration};

fn queued_read(paths: &config::Paths, number: u64) -> Happened {
    let (send, receive) = mpsc::channel();
    read(Some(paths.clone()), number, move |reply| send.send(reply).unwrap());
    receive.recv_timeout(Duration::from_secs(10)).expect("Diagnostics read must answer")
}

fn session() -> Session {
    Session {
        about: Session::about(&config::Settings { server: NEXUS.into(), ..Default::default() }, true),
        observations: diag::Record::default(),
    }
}

fn local_reports(paths: &config::Paths) -> io::Result<String> {
    let paths = paths.clone();
    let (sent, received) = mpsc::channel();
    on_worker(move || {
        let preview = preview_text(&paths, session());
        let export = export_report(&paths, session());
        sent.send((preview, export)).unwrap();
    });
    let (preview, export) = received.recv_timeout(Duration::from_secs(10)).unwrap();
    export?;
    Ok(preview)
}

const START: u64 = 1_700_000_000_000;
const NEXUS: &str = "https://diagnostics.example.invalid";

fn version() -> Version {
    Version { major: 0, minor: 40, patch: 13 }
}

fn backend() -> BackendId {
    BackendId::from_hex("5c0ffee05c0ffee05c0ffee05c0ffee0").unwrap()
}

fn saved() -> Saved {
    Saved {
        nexus: NEXUS.into(),
        backend_id: backend().to_hex(),
        account: AccountId::new(1).unwrap(),
        email: "fixture@example.invalid".into(),
        name: "Example User".into(),
        device: 1,
        restore: 1,
        life: 1,
        change: 1,
        notice: None,
        session: Some(NexusSession::from_bytes([99; 32])),
        sign_out: None,
        music: None,
    }
}

fn given(now_ms: u64) -> Given {
    Given { now_ms, local_offset_minutes: 0, random: Some(Random::new([42; 32])) }
}

fn begin(kept: Kept, now: u64) -> (Client, Vec<Do>) {
    let mut client = Client::default();
    let app = App { version: version(), kind: DeviceKind::Pc, device_name: Name::new("Test PC").unwrap() };
    let actions = client.start(app, kept, &given(now));
    (client, actions)
}

fn accept_hello(client: &mut Client, ask: &Ask, body: Vec<u8>, now: u64) {
    assert!(client.waits_for(ask.id));
    client.happened(Happened::Answered { ask: ask.id, status: 200, body: AnswerBody::new(body) }, &given(now));
    client.happened(Happened::InUse(true), &given(now));
}

fn start(default: Option<bool>, kept: Kept, now: u64) -> Client {
    let (mut client, actions) = begin(kept, now);
    let ask = asking(&actions, proto::HELLO).expect("hello at start");
    let hello = Hello::for_accounts(version(), backend(), SPEAKS);
    let hello = match default {
        Some(on) => hello.with_diagnostics_default(on),
        None => hello,
    };
    accept_hello(&mut client, ask, hello.to_json().into_bytes(), now);
    client
}

fn asking<'a>(actions: &'a [Do], path: &str) -> Option<&'a Ask> {
    actions.iter().find_map(|action| match action {
        Do::Ask(ask) if ask.url == format!("{NEXUS}{path}") => Some(ask),
        _ => None,
    })
}

fn read_number(actions: &[Do]) -> Option<u64> {
    actions.iter().find_map(|action| match action {
        Do::Journal(JournalDo::ReadDiagnostics { read }) => Some(*read),
        _ => None,
    })
}

fn fixture_report(paths: &config::Paths) -> io::Result<()> {
    ReportStore::default().checkpoint(&record_path(paths), session())
}

#[test]
fn settings_and_the_machine_apply_the_same_switch_precedence() {
    let account = (NEXUS.to_owned(), backend());
    let mut settings = Settings::default();
    assert!(settings.display().0);
    assert!(settings.display().1.contains("Nothing is sent until you sign in"));
    settings.account = Some(account.clone());
    for (default, choice, expected) in [
        (None, None, true), // The hello omitted the field.
        (Some(true), None, true),
        (Some(false), None, false),
        (Some(false), Some(true), true),
        (Some(true), Some(false), false),
    ] {
        settings.defaults.insert(account.clone(), diag::enabled(None, default));
        settings.choice = choice;
        assert_eq!(settings.display().0, expected);

        let mut client = start(default, Kept { saved: Some(saved()), diagnostics: choice, ..Kept::default() }, START);
        client.happened(Happened::DiagnosticsFault { at_ms: START }, &given(START));
        let actions = client.happened(Happened::Tick, &given(START + diag::FAULT_WAIT_MS));
        assert_eq!(read_number(&actions).is_some(), expected);
    }
}

#[test]
fn switch_off_and_offline_fence_both_a_late_read_and_a_queued_upload() -> io::Result<()> {
    let temp = TestFolder::new()?;
    let paths = temp.paths();
    fixture_report(&paths)?;
    let due = given(START + diag::FAULT_WAIT_MS);
    for offline in [false, true] {
        let fence = || if offline { Happened::Offline(true) } else { Happened::DiagnosticsSwitch(Some(false)) };
        for answer_before_switch in [false, true] {
            let mut client = start(Some(true), Kept { saved: Some(saved()), ..Kept::default() }, START);
            client.happened(Happened::DiagnosticsFault { at_ms: START }, &given(START));
            let actions = client.happened(Happened::Tick, &due);
            let read = read_number(&actions).expect("a read was queued");
            let answer = read_for_machine(Some(&paths), read, true);
            if answer_before_switch {
                let actions = client.happened(answer, &due);
                let ask = asking(&actions, diag::SEND_PATH).expect("an upload was queued");
                client.happened(fence(), &due);
                assert!(!client.waits_for(ask.id), "the transport must drop the queued upload");
            } else {
                client.happened(fence(), &due);
                let actions = client.happened(answer, &due);
                assert!(asking(&actions, diag::SEND_PATH).is_none());
            }
            let actions = client.happened(Happened::Tick, &given(due.now_ms + diag::MIN_GAP_MS));
            assert!(read_number(&actions).is_none());
        }
    }
    Ok(())
}

#[test]
fn window_events_reach_the_client_and_keep_the_first_fault_time() {
    let events = Rc::new(RefCell::new(Vec::new()));
    attach({
        let events = events.clone();
        move |event| events.borrow_mut().push(event)
    });
    in_use(true);
    in_use(true);
    note_fault(START);
    note_fault(START + 2);
    report_fault();
    choose(false);
    offline_changed(true);
    in_use(false);
    detach();
    assert!(matches!(events.borrow().as_slice(), [
        Happened::InUse(false),
        Happened::InUse(true),
        Happened::DiagnosticsFault { at_ms: START },
        Happened::DiagnosticsSwitch(Some(false)),
        Happened::Offline(true),
        Happened::InUse(false),
    ]));
}

#[test]
fn a_fault_does_not_send_without_a_signed_in_account() {
    let (mut client, _) = begin(Kept::default(), START);
    client.happened(Happened::InUse(true), &given(START));
    client.happened(Happened::DiagnosticsFault { at_ms: START }, &given(START));
    let actions = client.happened(Happened::Tick, &given(START + diag::FAULT_WAIT_MS));
    assert!(read_number(&actions).is_none());
    assert!(asking(&actions, diag::SEND_PATH).is_none());
}

fn due_read(client: &mut Client) -> (u64, Given, DiagSchedule) {
    client.happened(Happened::DiagnosticsFault { at_ms: START }, &given(START));
    let early = client.happened(Happened::Tick, &given(START + diag::FAULT_WAIT_MS - 1));
    assert!(read_number(&early).is_none());

    let due = given(START + diag::FAULT_WAIT_MS);
    let actions = client.happened(Happened::Tick, &due);
    let [Do::Keep(Keep::DiagSchedule(schedule)), Do::Journal(JournalDo::ReadDiagnostics { read }), ..] = &actions[..] else {
        panic!("the schedule must be kept before the read: {actions:?}");
    };
    assert_eq!(schedule.attempts, [due.now_ms]);
    (*read, due, schedule.clone())
}

#[test]
fn the_profiles_choice_and_attempt_survive_a_client_restart() -> io::Result<()> {
    let temp = TestFolder::new()?;
    let paths = temp.paths();
    save_choice(&choice_path(&paths), Some(true))?;
    let mut kept = Kept { saved: Some(saved()), ..Kept::default() };
    load_kept(&paths, &mut kept);
    assert_eq!(kept.diagnostics, Some(true));
    let mut client = start(Some(false), kept, START);
    let (_, due, schedule) = due_read(&mut client);
    save_schedule(&schedule_path(&paths), Some(&schedule))?;

    let mut kept = Kept { saved: Some(saved()), ..Kept::default() };
    load_kept(&paths, &mut kept);
    assert_eq!(kept.diagnostics, Some(true));
    let schedule = kept.diag_schedule.as_ref().unwrap();
    assert_eq!(schedule.started_ms, START);
    assert_eq!(schedule.attempts, [due.now_ms]);
    let mut client = start(Some(false), kept, due.now_ms);
    client.happened(Happened::DiagnosticsFault { at_ms: due.now_ms }, &due);
    let early = client.happened(Happened::Tick, &given(due.now_ms + diag::MIN_GAP_MS - 1));
    assert!(read_number(&early).is_none());
    let next = client.happened(Happened::Tick, &given(due.now_ms + diag::MIN_GAP_MS));
    assert!(read_number(&next).is_some());
    Ok(())
}

#[test]
fn the_worker_requires_a_saved_schedule_before_returning_diagnostics() -> io::Result<()> {
    let _worker = WORKER_FIXTURE.lock().unwrap();
    let temp = TestFolder::new()?;
    let paths = temp.paths();
    fs::write(&paths.crash_log, "1700000000000\tFault in app: no panic payload\n")?;
    let preview = local_reports(&paths)?;
    assert!(preview.contains("Fault in app: no panic payload"));
    assert_eq!(fs::read(&paths.diagnostics)?, preview.as_bytes());

    let schedule = DiagSchedule { started_ms: START, fault_ms: None, attempts: vec![START] };
    let path = schedule_path(&paths);
    keep_schedule(&paths, Some(schedule.clone()));
    assert!(matches!(queued_read(&paths, 4), Happened::DiagnosticsRead { read: 4, text: Some(text) } if text == preview));
    fs::remove_file(&path)?;
    fs::create_dir(&path)?;

    let mut client = start(Some(true), Kept { saved: Some(saved()), ..Kept::default() }, START);
    let (read, due, attempt) = due_read(&mut client);
    keep_schedule(&paths, Some(attempt));
    let answer = queued_read(&paths, read);
    assert!(matches!(&answer, Happened::DiagnosticsRead { read: number, text: None } if *number == read));
    assert!(asking(&client.happened(answer, &due), diag::SEND_PATH).is_none());

    fs::remove_dir(&path)?;
    keep_schedule(&paths, Some(schedule.clone()));
    assert!(matches!(queued_read(&paths, 8), Happened::DiagnosticsRead { read: 8, text: Some(_) }));
    assert_eq!(read_profile::<DiagSchedule>(&path, 2_048)?, Some(schedule));
    fs::write(record_path(&paths), "a name or address would fail C6 validation")?;
    assert!(matches!(queued_read(&paths, 9), Happened::DiagnosticsRead { read: 9, text: None }));
    Ok(())
}

#[test]
fn the_record_keeps_typed_observations_across_restart() -> io::Result<()> {
    let temp = TestFolder::new()?;
    let paths = temp.paths();
    let about = Session::about(&config::Settings::default(), false);
    for at in [START, START + 1_000] {
        let mut observations = diag::Record::default();
        observations.note(at, diag::Line::Note(diag::Note::DownloadsFolderChanged));
        ReportStore::default().checkpoint(&record_path(&paths), Session { about, observations })?;
    }
    let text = report_text(&paths)?;
    assert!(text.contains("Downloads folder changed  (2 times since"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn e13a_receives_the_same_bytes_as_preview_and_export() -> io::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    struct StandIn(Child);
    impl Drop for StandIn {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let _worker = WORKER_FIXTURE.lock().unwrap();
    let temp = TestFolder::new()?;
    let paths = temp.paths();
    fs::write(&paths.crash_log, "1700000000000\tFault in app: no panic payload\n")?;
    let preview = local_reports(&paths)?;
    assert!(preview.contains("Fault in app: no panic payload"));

    let received = paths.settings.with_file_name("received.json");
    let mut standin = StandIn(Command::new("python3")
        .args(["-B", "tests/diagnostics_standin.py"])
        .arg(&received)
        .stdout(Stdio::piped())
        .spawn()?);
    let mut port = String::new();
    BufReader::new(standin.0.stdout.take().unwrap()).read_line(&mut port)?;
    let origin = format!("http://127.0.0.1:{}", port.trim());
    let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).max_redirects(0)
        .timeout_global(Some(Duration::from_secs(5))).build().into();

    let (mut client, actions) = begin(Kept { saved: Some(saved()), ..Kept::default() }, START);
    let hello = asking(&actions, proto::HELLO).unwrap();
    let body = agent.get(format!("{origin}{}", proto::HELLO)).call().unwrap()
        .body_mut().read_to_vec().unwrap();
    accept_hello(&mut client, hello, body, START);
    let (read, due, schedule) = due_read(&mut client);
    keep_schedule(&paths, Some(schedule));
    let answer = queued_read(&paths, read);
    assert!(matches!(&answer, Happened::DiagnosticsRead { text: Some(text), .. } if text == &preview));
    let actions = client.happened(answer, &due);
    let ask = asking(&actions, diag::SEND_PATH).unwrap();

    // The E13a stand-in is the HTTP hop behind its HTTPS proxy. Forward only this
    // synthetic fixture's core-generated path, authorization and body.
    let (status, body) = std::thread::scope(|scope| scope.spawn(|| {
        assert!(client.waits_for(ask.id));
        let path = ask.url.strip_prefix(NEXUS).unwrap();
        let mut response = agent.post(format!("{origin}{path}"))
            .header("Authorization", ask.authorization.as_ref().unwrap().expose())
            .header("Content-Type", "application/json")
            .send(ask.body.as_ref().unwrap().expose().as_bytes()).unwrap();
        (response.status().as_u16(), response.body_mut().read_to_vec().unwrap())
    }).join().unwrap());
    assert!(matches!(diag::read_reply(status, &body), Ok(nexus_core::nexus::account::Reply::Done { .. })));
    client.happened(Happened::Answered { ask: ask.id, status, body: AnswerBody::new(body) }, &due);
    assert!(!client.waits_for(ask.id));

    let upload = diag::Upload::read(&fs::read(received)?).unwrap();
    assert_eq!(upload.text().as_bytes(), preview.as_bytes());
    assert_eq!(upload.text().as_bytes(), fs::read(&paths.diagnostics)?);
    Ok(())
}
