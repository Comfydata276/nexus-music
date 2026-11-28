use super::*;
use crate::nexus::account::{AccountId, DeviceId, Email, SECRET_BYTES, SessionId};
use crate::nexus::kept::{AccountState, ApartDevice, DeviceRow, Fingerprint, LiveCode, Permit, Session};
use crate::nexus::proto::BackendId;

const ACCOUNT: u64 = 1;
fn device_id(n: u64) -> DeviceId {
    DeviceId::new(n).unwrap()
}

const DEVICE: u64 = 7;

fn state() -> AccountState {
    AccountState {
        backend: BackendId::from_bytes([1; 16]),
        restore: 2,
        id: AccountId::new(ACCOUNT).unwrap(),
        email: Email::new("alice@example.org").unwrap(),
        life: 1,
        status: Status::On,
        password_changes: 1,
        password_reset_required: false,
        code: Some(LiveCode { number: 1, used: true }),
        admin: false,
        music_login: 0,
        music_managed: false,
        music_recovery: None,
        history_held: false,
        revision: 3,
    }
}

fn session(id: u64) -> Session {
    Session { id: SessionId::new(id).unwrap(), hash: Fingerprint::of(&[id as u8; SECRET_BYTES]), last_used: 0, leave: None }
}

/// One account with one device signed in (session 4), the same in both.
fn both() -> (StoreView, DataView) {
    let mut apart = Apart { account: state(), devices: Default::default() };
    apart.devices.insert(device_id(DEVICE), ApartDevice { number: 3, last_session: 4, last_leave: 0, permit: Some(Permit { id: SessionId::new(4).unwrap(), leave: None }) });
    let mut rows = Rows { account: state(), devices: Default::default() };
    rows.devices.insert(device_id(DEVICE), DeviceRow { number: 3, secret: Fingerprint::of(&[9; SECRET_BYTES]), session: Some(session(4)) });
    let mut store = StoreView { restore: 2, mark: 10, ..Default::default() };
    store.accounts.insert(ACCOUNT, apart);
    let mut data = DataView { restore: 2, changes: 12, ..Default::default() };
    data.accounts.insert(ACCOUNT, rows);
    (store, data)
}

fn found(store: &StoreView, data: &DataView) -> StoreComparison {
    let found = compare(store, data);
    assert_eq!(found.contradiction.is_some(), found.comparison == StoreComparison::StoreLost);
    found.comparison
}

fn kept(store: &mut StoreView) -> &mut Apart {
    store.accounts.get_mut(&ACCOUNT).unwrap()
}

fn row(data: &mut DataView) -> &mut Rows {
    data.accounts.get_mut(&ACCOUNT).unwrap()
}

fn under_way() -> Option<Pending> {
    Some(Pending::Change { accounts: vec![ACCOUNT] })
}

#[test]
fn the_same_age_matches_and_is_served() {
    let (store, data) = both();
    assert_eq!(found(&store, &data), StoreComparison::Matches);
    assert_eq!(compare(&store, &data).startup(false), Startup::Ready);
    assert_eq!(found(&StoreView::default(), &DataView::default()), StoreComparison::Matches);
}

#[test]
fn a_copy_older_than_the_stores_mark_is_older_even_when_nothing_else_differs() {
    // 12.3: a change the data alone has (a device last heard from, an event) is seen by the
    // mark of how far the data had got.
    let (store, mut data) = both();
    data.changes = 9;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    data.changes = 10;
    assert_eq!(found(&store, &data), StoreComparison::Matches);
}

#[test]
fn a_copy_from_before_a_restore_is_older() {
    let (store, mut data) = both();
    data.restore = 1;
    for rows in data.accounts.values_mut() {
        rows.account.restore = 1;
    }
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    assert_eq!(compare(&store, &data).startup(false), Startup::RestoreData);
}

#[test]
fn an_ending_the_data_lacks_is_older_or_a_cut_ending() {
    let (mut store, data) = both();
    let device = kept(&mut store).devices.get_mut(&device_id(DEVICE)).unwrap();
    device.number = 4;
    device.permit = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = under_way();
    assert_eq!(found(&store, &data), StoreComparison::EndingRecorded);
    assert_eq!(compare(&store, &data).startup(false), Startup::FinishEnding);
    // Another account's change does not explain this one.
    store.pending = Some(Pending::Change { accounts: vec![ACCOUNT + 1] });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
}

#[test]
fn every_account_wide_change_the_data_lacks_is_older_or_cut() {
    let changes: [(&str, fn(&mut AccountState)); 6] = [
        ("switched off", |s| { s.status = Status::Off; s.revision += 1 }),
        ("password changed", |s| { s.password_changes += 1; s.revision += 1 }),
        ("reset", |s| { s.password_reset_required = true; s.password_changes += 1 }),
        ("code used", |s| { s.code = Some(LiveCode { number: 1, used: true }); s.revision += 1 }),
        ("history deleted", |s| s.life += 1),
        ("no longer an admin", |s| s.admin = false),
    ];
    for (name, change) in changes {
        let (mut store, mut data) = both();
        if name == "no longer an admin" {
            row(&mut data).account.admin = true;
        } else if name == "code used" {
            row(&mut data).account.code = Some(LiveCode { number: 1, used: false });
        } else {
            change(&mut kept(&mut store).account);
        }
        if name == "no longer an admin" || name == "code used" {
            change(&mut kept(&mut store).account);
            kept(&mut store).account.revision = 3;
            if name == "code used" { kept(&mut store).account.revision = 4 }
        }
        assert_eq!(found(&store, &data), StoreComparison::OlderCopy, "{name}");
        store.pending = under_way();
        assert_eq!(found(&store, &data), StoreComparison::EndingRecorded, "{name}");
    }
    // Deleted: older, or a cut deletion.
    let (mut store, data) = both();
    kept(&mut store).account.status = Status::Deleted;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = under_way();
    assert_eq!(found(&store, &data), StoreComparison::EndingRecorded);
}

#[test]
fn a_grant_only_in_the_data_is_told_from_one_the_store_ended() {
    let (mut store, mut data) = both();
    // A new session, 5, reserved in the store, written to the data, not yet recognized.
    let device = kept(&mut store).devices.get_mut(&device_id(DEVICE)).unwrap();
    device.last_session = 5;
    device.permit = None;
    row(&mut data).devices.get_mut(&device_id(DEVICE)).unwrap().session = Some(session(5));
    store.pending = under_way();
    assert_eq!(found(&store, &data), StoreComparison::GrantOnlyInData);
    assert_eq!(compare(&store, &data).startup(false), Startup::DiscardGrant);
    store.pending = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    // A session the store still permits and the data lacks is older, cut or not.
    let (store, mut data) = both();
    row(&mut data).devices.get_mut(&device_id(DEVICE)).unwrap().session = None;
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
}

#[test]
fn a_device_or_account_the_data_lacks_is_older_unless_its_making_was_cut() {
    let (mut store, data) = both();
    kept(&mut store).devices.insert(device_id(8), ApartDevice { number: 0, last_session: 1, last_leave: 0, permit: Some(Permit { id: SessionId::new(1).unwrap(), leave: None }) });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = under_way();
    assert_eq!(found(&store, &data), StoreComparison::GrantOnlyInData);
    // A record with no session, left of a device an older copy lost, is no sign of age.
    store.pending = None;
    kept(&mut store).devices.get_mut(&device_id(8)).unwrap().permit = None;
    assert_eq!(found(&store, &data), StoreComparison::Matches);
    let (mut store, data) = both();
    store.accounts.insert(2, Apart { account: AccountState { id: AccountId::new(2).unwrap(), ..state() }, devices: Default::default() });
    assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
    store.pending = Some(Pending::Change { accounts: vec![2] });
    assert_eq!(found(&store, &data), StoreComparison::Matches);
    // A deleted account the data no longer has is not missed.
    store.pending = None;
    store.accounts.get_mut(&2).unwrap().account.status = Status::Deleted;
    assert_eq!(found(&store, &data), StoreComparison::Matches);
}

#[test]
fn what_no_cut_and_no_copy_can_make_means_the_store_is_not_this_datas() {
    let cases: [(&str, fn(&mut StoreView, &mut DataView), Contradiction); 8] = [
        ("restore ahead", |_, data| data.restore = 3, Contradiction::DataRestoreAhead),
        ("account unknown", |store, _| store.accounts.clear(), Contradiction::AccountUnknown),
        ("life ahead", |_, data| row(data).account.life = 2, Contradiction::AccountAhead),
        ("passwords ahead", |_, data| row(data).account.password_changes = 2, Contradiction::AccountAhead),
        ("revision ahead", |_, data| row(data).account.revision = 4, Contradiction::AccountAhead),
        ("device unknown", |store, _| kept(store).devices.clear(), Contradiction::DeviceUnknown),
        ("device ahead", |_, data| row(data).devices.get_mut(&device_id(DEVICE)).unwrap().number = 4, Contradiction::DeviceAhead),
        ("session never reserved", |_, data| row(data).devices.get_mut(&device_id(DEVICE)).unwrap().session = Some(session(9)), Contradiction::SessionNeverReserved),
    ];
    for (name, change, why) in cases {
        let (mut store, mut data) = both();
        change(&mut store, &mut data);
        let found = compare(&store, &data);
        assert_eq!(found, Found { comparison: StoreComparison::StoreLost, contradiction: Some(why) }, "{name}");
        assert_eq!(found.startup(false), Startup::Stop, "{name}");
        assert_eq!(found.startup(true), Startup::AdoptData, "{name}");
    }
}

#[test]
fn a_restore_under_way_is_done_again_from_the_start() {
    for pending in [Pending::Restore { restore: 3 }, Pending::TakeAsIs { restore: 3 }] {
        for restore in [1, 2, 3] {
            let (mut store, mut data) = both();
            store.restore = 3;
            store.pending = Some(pending.clone());
            data.restore = restore;
            assert_eq!(found(&store, &data), StoreComparison::OlderCopy);
        }
        let (mut store, mut data) = both();
        store.pending = Some(pending);
        data.restore = 4;
        assert_eq!(compare(&store, &data).contradiction, Some(Contradiction::DataRestoreAhead));
    }
}
