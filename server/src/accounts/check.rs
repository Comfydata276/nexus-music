//! The backend's two password workers. The caller authenticates the device,
//! keeps the receipt fences, and owns the code lookup and account transaction.

use crate::store::data::Password as StoredPassword;
use argon2::{Algorithm, Argon2, Block, Params, Version};
use nexus_core::nexus::{
    account::{
        CodeLimit, DeviceId, DummySaltKey, Email, PASSWORD_QUEUE_MS, PASSWORD_SALT_BYTES, Password,
        PasswordAdmission, PasswordDoor, PasswordLoad, PasswordWaits, Refusal,
        admit_password_check, resume_password_check,
    },
    kept::Fingerprint,
};
use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

const HASH_BYTES: usize = 32;
const CURRENT: Profile = Profile { memory: 19 * 1024, passes: 2, lanes: 1 };

#[derive(Clone, Copy, PartialEq, Eq)]
struct Profile {
    memory: u32,
    passes: u32,
    lanes: u32,
}

impl Profile {
    fn stored(password: &StoredPassword) -> Self {
        Self { memory: password.memory, passes: password.passes, lanes: password.lanes }
    }

    fn params(self) -> Result<Params, Refusal> {
        // The design gives no historical profiles. Keep the current resource ceiling.
        if self.memory > CURRENT.memory || self.passes > CURRENT.passes || self.lanes > CURRENT.lanes {
            return Err(Refusal::NotDealtWith);
        }
        Params::new(self.memory, self.passes, self.lanes, Some(HASH_BYTES))
            .map_err(|_| Refusal::NotDealtWith)
    }
}

type Hash = Zeroizing<[u8; HASH_BYTES]>;
type Salt = [u8; PASSWORD_SALT_BYTES];
type Slow = dyn Fn(&Password, &Salt, Profile) -> Result<Hash, Refusal> + Send + Sync;
type Clock = dyn Fn() -> u64 + Send + Sync;

fn argon(password: &Password, salt: &Salt, profile: Profile) -> Result<Hash, Refusal> {
    let params = profile.params()?;
    let blocks = params.block_count();
    // The crate's convenience allocator does not wipe its working blocks.
    let mut memory = Zeroizing::new(Vec::<Block>::new());
    memory.try_reserve_exact(blocks).map_err(|_| Refusal::NotDealtWith)?;
    memory.resize(blocks, Block::default());

    let mut hash = Zeroizing::new([0; HASH_BYTES]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password.expose().as_bytes(), salt, hash.as_mut(), memory.as_mut_slice())
        .map_err(|_| Refusal::NotDealtWith)?;
    Ok(hash)
}

#[derive(Debug)]
pub enum Checked {
    Match { derived: Fingerprint, replacement: Option<StoredPassword> },
    Wrong,
}

enum Work {
    Check(Option<StoredPassword>),
    Fingerprint,
}

#[derive(Debug)]
enum Completed {
    Checked(Checked),
    Fingerprint(StoredPassword),
}

struct Engine {
    clock: Arc<Clock>,
    waits: Mutex<PasswordWaits>,
    dummy_key: DummySaltKey,
    slow: Box<Slow>,
}

impl Engine {
    fn fresh(&self, password: &Password) -> Result<StoredPassword, Refusal> {
        let mut salt = Zeroizing::new([0; PASSWORD_SALT_BYTES]);
        getrandom::fill(salt.as_mut()).map_err(|_| Refusal::NotDealtWith)?;
        let hash = (self.slow)(password, &salt, CURRENT)?;
        Ok(StoredPassword {
            hash: *hash,
            salt: *salt,
            memory: CURRENT.memory,
            passes: CURRENT.passes,
            lanes: CURRENT.lanes,
        })
    }

    fn check(
        &self,
        password: &Password,
        email: &Email,
        stored: Option<StoredPassword>,
    ) -> Result<Checked, Refusal> {
        // Derive the dummy salt for every check so only choosing the input differs.
        let dummy_salt = self.dummy_key.salt_for(email);
        // An unreadable profile still takes the dummy path and the common answer.
        let stored = stored.filter(|stored| Profile::stored(stored).params().is_ok());
        let (salt, expected, profile) = match stored.as_ref() {
            Some(stored) => (&stored.salt, &stored.hash, Profile::stored(stored)),
            None => (&*dummy_salt, &[0; HASH_BYTES], CURRENT),
        };
        let hash = (self.slow)(password, salt, profile)?;
        let derived = Fingerprint::from_bytes(*hash);
        let matches = (derived == Fingerprint::from_bytes(*expected)) & stored.is_some();
        if !matches {
            return Ok(Checked::Wrong);
        }
        let replacement = if profile == CURRENT { None } else { Some(self.fresh(password)?) };
        Ok(Checked::Match { derived, replacement })
    }
}

type Answer = Result<Completed, Refusal>;

struct Job {
    id: Arc<()>,
    email: Email,
    door: PasswordDoor,
    device: Option<DeviceId>,
    password: Password,
    work: Work,
    reply: oneshot::Sender<Answer>,
}

struct Waiting {
    queued_at: u64,
    job: Job,
}

#[derive(Default)]
struct State {
    waiting: VecDeque<Waiting>,
    ready: Option<Job>,
    running: Option<Email>,
    closed: bool,
}

impl State {
    fn account_busy(&self, email: &Email) -> bool {
        self.running.as_ref() == Some(email)
            || self.ready.as_ref().is_some_and(|job| &job.email == email)
    }

    fn expire_waiting(&mut self, waits: &PasswordWaits, now: u64) {
        for _ in 0..self.waiting.len() {
            let Some(waiting) = self.waiting.pop_front() else { break };
            let admission = resume_password_check(
                waiting.queued_at, true, self.account_busy(&waiting.job.email),
                waits, &waiting.job.email, now,
            );
            if let Some(refusal) = admission.refusal() {
                drop(waiting.job.password);
                let _ = waiting.job.reply.send(Err(refusal));
            } else {
                self.waiting.push_back(waiting);
            }
        }
    }

    fn take_next(&mut self, waits: &PasswordWaits, now: u64) -> Option<Job> {
        let job = self.ready.take().or_else(|| {
            while let Some(Waiting { queued_at, job }) = self.waiting.pop_front() {
                let admission = resume_password_check(queued_at, false, false, waits, &job.email, now);
                if admission == PasswordAdmission::Check {
                    return Some(job);
                }
                drop(job.password);
                let _ = job.reply.send(Err(admission.refusal().unwrap_or(Refusal::NotDealtWith)));
            }
            None
        });
        self.running = job.as_ref().map(|job| job.email.clone());
        job
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Reservation<'a>(&'a Lane);

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        lock(&self.0.state).running = None;
    }
}

struct Lane {
    state: Mutex<State>,
    changed: Condvar,
    engine: Arc<Engine>,
}

impl Lane {
    fn new(engine: Arc<Engine>) -> Self {
        Self { state: Mutex::default(), changed: Condvar::new(), engine }
    }

    fn submit(
        self: &Arc<Self>,
        email: &Email,
        door: PasswordDoor,
        device: Option<DeviceId>,
        password: Password,
        work: Work,
    ) -> Result<Pending, Refusal> {
        let mut state = lock(&self.state);
        let mut waits = lock(&self.engine.waits);
        let now = (self.engine.clock)();
        waits.expire(now);
        state.expire_waiting(&waits, now);
        let load = PasswordLoad {
            worker_busy: state.running.is_some() || state.ready.is_some(),
            account_busy: state.account_busy(email),
            waiting: state.waiting.len(),
        };
        let admission = admit_password_check(door, load, &waits, email, device, now);
        drop(waits);
        let queued_at = match admission {
            PasswordAdmission::Check => None,
            PasswordAdmission::WaitForWorker => Some(now),
            denied => return Err(denied.refusal().unwrap_or(Refusal::NotDealtWith)),
        };
        let (reply, receiver) = oneshot::channel();
        let id = Arc::new(());
        let job = Job {
            id: Arc::clone(&id),
            email: email.clone(),
            door,
            device,
            password,
            work,
            reply,
        };
        if let Some(queued_at) = queued_at {
            state.waiting.push_back(Waiting { queued_at, job });
        } else {
            state.ready = Some(job);
        }
        self.changed.notify_one();
        Ok(Pending { lane: Arc::clone(self), id, queued_at, receiver })
    }

    fn next_job(&self) -> Option<Job> {
        let mut state = lock(&self.state);
        while !state.closed {
            let next = {
                let waits = lock(&self.engine.waits);
                state.take_next(&waits, (self.engine.clock)())
            };
            if next.is_some() {
                return next;
            }
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        None
    }

    fn run(&self) {
        while let Some(Job { email, door, device, password, work, reply, .. }) = self.next_job() {
            let reservation = Reservation(self);
            let result = match catch_unwind(AssertUnwindSafe(|| match work {
                Work::Check(stored) => self.engine.check(&password, &email, stored).map(Completed::Checked),
                Work::Fingerprint => self.engine.fresh(&password).map(Completed::Fingerprint),
            })) {
                Ok(result) => result,
                Err(payload) => {
                    // Even discarding a custom panic payload must not kill this worker.
                    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                        std::mem::forget(payload);
                    }
                    Err(Refusal::NotDealtWith)
                }
            };
            drop(password);

            let wrong = matches!(result, Ok(Completed::Checked(Checked::Wrong)));
            if door == PasswordDoor::Other || wrong {
                let mut state = lock(&self.state);
                let mut waits = lock(&self.engine.waits);
                let now = (self.engine.clock)();
                if wrong {
                    waits.record_wrong(door, &email, device, now);
                }
                state.expire_waiting(&waits, now);
            }
            drop(reservation);
            let _ = reply.send(result);
        }
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.ready = None;
        state.waiting.clear();
        self.changed.notify_one();
    }
}

struct Pending {
    lane: Arc<Lane>,
    id: Arc<()>,
    queued_at: Option<u64>,
    receiver: oneshot::Receiver<Answer>,
}

impl Pending {
    async fn finish(mut self) -> Answer {
        if let Some(queued_at) = self.queued_at {
            loop {
                let now = (self.lane.engine.clock)();
                let remaining = PASSWORD_QUEUE_MS.saturating_sub(now.saturating_sub(queued_at));
                if remaining == 0 {
                    // Expire only queued work. Dispatch and expiry share this lock,
                    // so a job already running is left to finish.
                    let mut state = lock(&self.lane.state);
                    let waits = lock(&self.lane.engine.waits);
                    state.expire_waiting(&waits, now);
                    break;
                }
                if let Ok(answer) = tokio::time::timeout(Duration::from_millis(remaining), &mut self.receiver).await {
                    return answer.unwrap_or(Err(Refusal::NotDealtWith));
                }
            }
        }
        (&mut self.receiver).await.unwrap_or(Err(Refusal::NotDealtWith))
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut state = lock(&self.lane.state);
        if state.ready.as_ref().is_some_and(|job| Arc::ptr_eq(&job.id, &self.id)) {
            state.ready = None;
        }
        state.waiting.retain(|waiting| !Arc::ptr_eq(&waiting.job.id, &self.id));
    }
}

/// Construct one pool per backend process and share it among requests. Both
/// workers also run new fingerprints and replacement fingerprints.
pub struct PasswordWorkers {
    known: Arc<Lane>,
    other: Arc<Lane>,
    threads: Option<[JoinHandle<()>; 2]>,
    codes: Mutex<CodeLimit>,
}

impl PasswordWorkers {
    pub fn new(dummy_key: DummySaltKey) -> Result<Self, Refusal> {
        let start = Instant::now();
        let clock = Arc::new(move || start.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
        Self::with_engine(Engine { clock, waits: Mutex::default(), dummy_key, slow: Box::new(argon) })
    }

    fn with_engine(engine: Engine) -> Result<Self, Refusal> {
        let engine = Arc::new(engine);
        let known = Arc::new(Lane::new(Arc::clone(&engine)));
        let other = Arc::new(Lane::new(engine));
        let spawn = |name: &str, lane: &Arc<Lane>| {
            let lane = Arc::clone(lane);
            thread::Builder::new().name(name.into()).spawn(move || lane.run())
                .map_err(|_| Refusal::NotDealtWith)
        };
        let first = spawn("nexus-password-known", &known)?;
        let second = match spawn("nexus-password-other", &other) {
            Ok(thread) => thread,
            Err(error) => {
                known.close();
                let _ = first.join();
                return Err(error);
            }
        };
        Ok(Self { known, other, threads: Some([first, second]), codes: Mutex::default() })
    }

    fn lane(&self, door: PasswordDoor) -> &Arc<Lane> {
        match door {
            PasswordDoor::KnownDevice | PasswordDoor::RedeemedCode => &self.known,
            PasswordDoor::Other => &self.other,
        }
    }

    /// A missing verifier gets the dummy check. A switched-off account with a
    /// verifier supplies it as usual; kept decides what a correct password permits.
    pub async fn check(
        &self,
        email: &Email,
        door: PasswordDoor,
        device: Option<DeviceId>,
        password: Password,
        stored: Option<StoredPassword>,
    ) -> Result<Checked, Refusal> {
        if door == PasswordDoor::RedeemedCode {
            return Err(Refusal::NotDealtWith);
        }
        match self.lane(door).submit(email, door, device, password, Work::Check(stored))?.finish().await? {
            Completed::Checked(answer) => Ok(answer),
            Completed::Fingerprint(_) => Err(Refusal::NotDealtWith),
        }
    }

    /// For a verified live code, use `RedeemedCode` with no device ID. Other work
    /// uses the applicable password door and an authenticated ID for `KnownDevice`.
    /// Creating a fingerprint does not record a wrong try.
    pub async fn fingerprint(
        &self,
        email: &Email,
        door: PasswordDoor,
        device: Option<DeviceId>,
        password: Password,
    ) -> Result<StoredPassword, Refusal> {
        match self.lane(door).submit(email, door, device, password, Work::Fingerprint)?.finish().await? {
            Completed::Fingerprint(password) => Ok(password),
            Completed::Checked(_) => Err(Refusal::NotDealtWith),
        }
    }

    /// Records a wrong try at `door` after a real check that matched but that kept (or the
    /// caller's rule) refuses as a wrong password: the next try waits as after any wrong one.
    /// For M2 (the lead's ruling of 14:41): a non-admin's correct password at the admin's page.
    pub fn record_wrong(&self, door: PasswordDoor, email: &Email, device: Option<DeviceId>) {
        let lane = self.lane(door);
        let mut state = lock(&lane.state);
        let mut waits = lock(&lane.engine.waits);
        let now = (lane.engine.clock)();
        waits.record_wrong(door, email, device, now);
        state.expire_waiting(&waits, now);
    }

    /// Reserve one attempt before computing and looking up the fast code fingerprint.
    pub fn admit_code(&self) -> Result<(), Refusal> {
        let mut codes = lock(&self.codes);
        codes.admit((self.other.engine.clock)())
            .map_err(|wait| Refusal::Busy { retry_after_seconds: Some(wait) })
    }
}

impl Drop for PasswordWorkers {
    fn drop(&mut self) {
        self.known.close();
        self.other.close();
        // Closing wakes the idle workers. A running worker still owns its job.
        if let Some(threads) = self.threads.take() {
            for thread in threads {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
#[path = "check/tests.rs"]
mod tests;
