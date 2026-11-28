use super::*;
use crate::nexus::kept::{Access, Clock, Denied, Device, Fingerprint, SECRET_BYTES, Scope, Session};
use crate::nexus::proto::BackendId;

const ACCOUNT: u64 = 1;
const DEVICE: u64 = 7;

fn permit(id: u64) -> Permit {
    Permit { id, delete_proven: None }
}

/// One account and one device, a session working and one waiting, the same in both.
fn both() -> (StoreView, DataView) {
    let mut store = StoreView { restore: 2, ..Default::default() };
    store.accounts.insert(ACCOUNT, StoreAccount { life: 1, held: None, present: true });
    store.devices.insert(
        DEVICE,
        StoreDevice { account: ACCOUNT, restore: 2, life: 1, number: 3, last_session: 5, working: Some(permit(4)), waiting: Some(permit(5)), gone: false },
    );
    let mut data = DataView { restore: 2, ..Default::default() };
    data.accounts.insert(ACCOUNT, 1);
    data.devices.insert(
        DEVICE,
        DataDevice { account: ACCOUNT, number: 3, sessions: vec![DataSession { id: 4, delete_proven: None }, DataSession { id: 5, delete_proven: None }] },
    );
    (store, data)
}

fn found(store: &StoreView, data: &DataView) -> StoreComparison {
    let found = compare(store, data);
    assert_eq!(found.contradiction.is_some(), found.comparison == StoreComparison::StoreLost);
    found.comparison
}

fn device(store: &mut StoreView) -> &mut StoreDevice {
    store.devices.get_mut(&DEVICE).unwrap()
}

fn row(data: &mut DataView) -> &mut DataDevice {
    data.devices.get_mut(&DEVICE).unwrap()
}

fn update(family: Option<Family>) -> Option<Pending> {
    Some(Pending::Update { device: DEVICE, account: ACCOUNT, family })
}

#[test]
fn the_same_age_matches_and_is_served() {
    let (store, data) = both();
    assert_eq!(compare(&store, &data), Found { comparison: StoreComparison::Matches, contradiction: None });
    assert_eq!(compare(&store, &data).startup(false), Startup::Ready);
    // Nothing on either side, as at a first start.
    assert_eq!(found(&StoreView::default(), &DataView::default()), StoreComparison::Matches);
}

#[test]
fn a_copy_from_before_a_restore_is_older() {
    let (store, mut data) = both();
    data.restore = 1;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    assert_eq!(compare(&store, &data).startup(false), Startup::RestoreData);
}

#[test]
fn a_copy_from_before_an_ending_is_older_and_a_cut_ending_is_finished() {
    // Sessions ended and nothing else changed (X2): the store's number moved, the data's not.
    let (mut store, data) = both();
    let kept = device(&mut store);
    kept.number = 4;
    kept.working = None;
    kept.waiting = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    // The same, with the ending the store names as under way: the data is one step behind it.
    store.pending = update(Some(Family::Device));
    assert_eq!(found(&store, &data), StoreComparison::EndingRecorded);
    assert_eq!(compare(&store, &data).startup(false), Startup::FinishEnding);
    // Two steps behind is not that ending.
    device(&mut store).number = 5;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    // An ending of another device does not explain this one.
    device(&mut store).number = 4;
    store.pending = Some(Pending::Update { device: DEVICE + 1, account: ACCOUNT, family: Some(Family::Device) });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
}

#[test]
fn a_copy_from_before_a_replacement_is_older() {
    // O (4) was replaced by N (5) being used: the store permits N alone.
    let (mut store, data) = both();
    let kept = device(&mut store);
    kept.working = Some(permit(5));
    kept.waiting = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    // Cut between the store and the data: O is still in the data.
    store.pending = update(None);
    assert_eq!(found(&store, &data), StoreComparison::EndingRecorded);
}

#[test]
fn a_grant_only_in_the_data_is_told_from_one_the_store_has_ended() {
    let (mut store, mut data) = both();
    // A new waiting session, 6, reserved in the store and written to the data; the store had
    // not yet heard it was written.
    device(&mut store).last_session = 6;
    row(&mut data).sessions = vec![DataSession { id: 4, delete_proven: None }, DataSession { id: 6, delete_proven: None }];
    device(&mut store).waiting = None;
    store.pending = update(None);
    assert_eq!(found(&store, &data), StoreComparison::GrantOnlyInData);
    assert_eq!(compare(&store, &data).startup(false), Startup::EndUnknownSession);
    // With no change under way, a session the store does not permit is one it ended.
    store.pending = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    // Leave to delete given in the data alone (a grant), and the same with nothing under way.
    let (mut store, mut data) = both();
    row(&mut data).sessions[0].delete_proven = Some(9);
    store.pending = update(None);
    assert_eq!(found(&store, &data), StoreComparison::GrantOnlyInData);
    store.pending = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    // Leave the store has and the data not: older, or a cut ending of that leave.
    let (mut store, data) = both();
    device(&mut store).working = Some(Permit { id: 4, delete_proven: Some(9) });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
}

#[test]
fn a_session_the_store_permits_and_the_data_lacks_is_always_older() {
    let (mut store, mut data) = both();
    row(&mut data).sessions.retain(|session| session.id != 5);
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = update(None);
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
}

#[test]
fn a_copy_from_before_a_deletion_is_older_and_its_account_is_held() {
    let (mut store, data) = both();
    let account = store.accounts.get_mut(&ACCOUNT).unwrap();
    account.life = 2;
    account.present = false;
    device(&mut store).gone = true;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    assert_eq!(holds(&store, &data), [(ACCOUNT, 2)]);
    // Cut between the store and the data: the deletion is finished, nothing is held.
    store.pending = update(Some(Family::Account));
    assert_eq!(found(&store, &data), StoreComparison::EndingRecorded);
    // Once held, the same data matches, and is held still.
    store.pending = None;
    store.accounts.get_mut(&ACCOUNT).unwrap().held = Some(2);
    let mut held = data.clone();
    held.devices.clear();
    device(&mut store).gone = true;
    assert_eq!(found(&store, &held), StoreComparison::Matches);
    assert_eq!(holds(&store, &held), [(ACCOUNT, 2)]);
    // A hold through an earlier life does not cover a later deletion.
    store.accounts.get_mut(&ACCOUNT).unwrap().life = 3;
    assert_eq!(found(&store, &held), StoreComparison::OlderCopy);
}

#[test]
fn only_accounts_behind_are_held_and_all_are_when_the_store_is_lost() {
    let (mut store, mut data) = both();
    store.accounts.insert(2, StoreAccount { life: 4, held: None, present: true });
    data.accounts.insert(2, 3);
    store.accounts.insert(3, StoreAccount { life: 0, held: Some(0), present: true });
    data.accounts.insert(3, 0);
    assert_eq!(holds(&store, &data), [(2, 4), (3, 0)]);
    assert_eq!(hold_all(&data), [(ACCOUNT, 1), (2, 3), (3, 0)]);
}

#[test]
fn a_removed_device_is_not_brought_back_by_a_copy() {
    let (mut store, data) = both();
    device(&mut store).gone = true;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = Some(Pending::Remove { device: DEVICE });
    assert_eq!(found(&store, &data), StoreComparison::EndingRecorded);
}

#[test]
fn a_copy_from_before_a_device_or_account_was_made_is_older_unless_that_making_was_cut() {
    let (mut store, data) = both();
    store.devices.insert(8, StoreDevice { account: ACCOUNT, restore: 2, life: 1, number: 0, last_session: 0, working: None, waiting: None, gone: false });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = Some(Pending::MakeDevice { device: 8 });
    assert_eq!(found(&store, &data), StoreComparison::Matches);
    let (mut store, data) = both();
    store.accounts.insert(2, StoreAccount { life: 0, held: None, present: true });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = Some(Pending::MakeAccount { account: 2 });
    assert_eq!(found(&store, &data), StoreComparison::Matches);
    // An account deleted (not present) and not made again is not missed.
    store.accounts.insert(2, StoreAccount { life: 1, held: None, present: false });
    store.pending = None;
    assert_eq!(found(&store, &data), StoreComparison::Matches);
}

#[test]
fn what_no_cut_and_no_copy_can_make_means_the_store_is_not_this_datas() {
    let cases: [(&str, fn(&mut StoreView, &mut DataView), Contradiction); 7] = [
        ("restore ahead", |_, data| data.restore = 3, Contradiction::DataRestoreAhead),
        ("account unknown", |store, _| { store.accounts.clear(); }, Contradiction::AccountUnknown),
        ("account ahead", |_, data| { data.accounts.insert(ACCOUNT, 2); }, Contradiction::AccountAhead),
        ("device unknown", |store, _| { store.devices.clear(); }, Contradiction::DeviceUnknown),
        ("device elsewhere", |_, data| row(data).account = 2, Contradiction::DeviceElsewhere),
        ("device ahead", |_, data| row(data).number = 4, Contradiction::DeviceAhead),
        ("session never reserved", |_, data| row(data).sessions.push(DataSession { id: 6, delete_proven: None }), Contradiction::SessionNeverReserved),
    ];
    for (name, change, why) in cases {
        let (mut store, mut data) = both();
        change(&mut store, &mut data);
        let found = compare(&store, &data);
        assert_eq!(found, Found { comparison: StoreComparison::StoreLost, contradiction: Some(why) }, "{name}");
        assert_eq!(found.startup(false), Startup::Stop, "{name}");
        assert_eq!(found.startup(true), Startup::HoldAll, "{name}");
    }
    // Even with a restore under way, a data restore number the store never gave.
    let (mut store, mut data) = both();
    store.pending = Some(Pending::Restore { restore: 3 });
    data.restore = 4;
    assert_eq!(compare(&store, &data).contradiction, Some(Contradiction::DataRestoreAhead));
}

#[test]
fn a_restore_under_way_is_done_again_from_the_start() {
    for pending in [Pending::Restore { restore: 3 }, Pending::TakeAsIs { restore: 3 }] {
        for data_restore in [1, 2, 3] {
            let (mut store, mut data) = both();
            store.restore = 3;
            store.pending = Some(pending);
            data.restore = data_restore;
            assert_eq!(found(&store, &data), StoreComparison::OlderCopy, "{pending:?} {data_restore}");
        }
    }
}

// --- brought_up follows Device::load ---

fn hash(n: u8) -> Fingerprint {
    Fingerprint::of(&[n; SECRET_BYTES])
}

fn scope(number: u64) -> Scope {
    Scope { backend: BackendId::from_bytes([1; 16]), restore: 2, account: "Alice".into(), life: 1, device: DEVICE, number }
}

fn session(id: u64, delete_proven: Option<u64>) -> Session {
    Session { id, hash: hash(id as u8), proven: 0, delete_proven }
}

/// What a loaded device lets each of sessions 1 to 6 do, read and delete.
fn behaviour(apart: &Apart, rows: &Rows) -> Vec<(Result<(), Denied>, Result<(), Denied>)> {
    (1..=6u8)
        .map(|n| {
            let mut device = Device::load(apart, rows);
            let clock = Clock { now: 1, high_water: 0 };
            let read = device.decide(&apart.scope, clock, |turn| turn.arrive(&hash(n), Access::Read, false).map(|_| ())).apply(|_| Ok::<_, ()>(()));
            let mut device = Device::load(apart, rows);
            let delete = device.decide(&apart.scope, clock, |turn| turn.delete(&hash(n))).apply(|_| Ok::<_, ()>(()));
            (read.unwrap_or(Err(Denied::NotSignedIn)), delete.unwrap_or(Err(Denied::NotSignedIn)))
        })
        .collect()
}

#[test]
fn brought_up_rows_load_as_the_rows_they_came_from_and_hold_nothing_more() {
    let permits = [None, Some(permit(1)), Some(permit(2)), Some(Permit { id: 2, delete_proven: Some(1) }), Some(permit(6))];
    let sessions = [None, Some(session(1, None)), Some(session(2, None)), Some(session(2, Some(1))), Some(session(3, None))];
    let mut cases = 0;
    for number in [3, 4] {
        for working in permits {
            for waiting in permits {
                for data_working in &sessions {
                    for data_waiting in &sessions {
                        let apart = Apart { scope: scope(4), last_session: 5, working, waiting };
                        let rows = Rows { scope: scope(number), working: data_working.clone(), waiting: data_waiting.clone() };
                        let up = brought_up(&apart, &rows);
                        assert_eq!(up.scope, apart.scope);
                        assert_eq!(behaviour(&apart, &up), behaviour(&apart, &rows), "{apart:?} {rows:?}");
                        // Bringing up again changes nothing.
                        assert_eq!(brought_up(&apart, &up), up);
                        // Every row kept is one the store permits.
                        for kept in up.working.iter().chain(&up.waiting) {
                            assert!(working.iter().chain(&waiting).any(|p| p.id == kept.id && p.delete_proven == kept.delete_proven || p.id == kept.id && kept.delete_proven.is_none()));
                        }
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 2 * 5 * 5 * 5 * 5);
}
