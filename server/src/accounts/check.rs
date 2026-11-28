//! The backend's two password workers. The caller authenticates the device,
//! keeps the receipt fences, and owns the code lookup and account transaction.

use crate::store::data::Password as StoredPassword;
use argon2::{Algorithm, Argon2, Params, Version};
use nexus_core::nexus::{
    account::{
        CodeLimit, DummySaltKey, Email, PASSWORD_QUEUE_MS, PASSWORD_SALT_BYTES, Password,
        PasswordAdmission, PasswordDoor, PasswordLoad, PasswordWaits, Refusal,
        admit_password_check, resume_password_check,
    },
    kept::Fingerprint,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
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
        // Pending C7's historical-profile policy, retain the current resource ceilings.
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
    let mut hash = Zeroizing::new([0; HASH_BYTES]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, profile.params()?)
        .hash_password_into(password.expose().as_bytes(), salt, hash.as_mut())
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

enum Completed {
    Checked(Checked),
    Fingerprint(StoredPassword),
}

struct Engine {
    clock: Arc<Clock>,
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
    waits: PasswordWaits,
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

    fn expire_waiting(&mut self, now: u64) {
        self.waits.expire(now);
        for _ in 0..self.waiting.len() {
            let Some(waiting) = self.waiting.pop_front() else { break };
            let admission = resume_password_check(
                waiting.queued_at, true, self.account_busy(&waiting.job.email),
                &self.waits, &waiting.job.email, now,
            );
            if let Some(refusal) = admission.refusal() {
                drop(waiting.job.password);
                let _ = waiting.job.reply.send(Err(refusal));
            } else {
                self.waiting.push_back(waiting);
            }
        }
    }

    fn take_next(&mut self, now: u64) -> Option<Job> {
        let job = self.ready.take().or_else(|| {
            while let Some(Waiting { queued_at, job }) = self.waiting.pop_front() {
                let admission = resume_password_check(queued_at, false, false, &self.waits, &job.email, now);
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

struct Lane {
    door: PasswordDoor,
    state: Mutex<State>,
    changed: Condvar,
    engine: Arc<Engine>,
}

impl Lane {
    fn new(door: PasswordDoor, engine: Arc<Engine>) -> Self {
        Self { door, state: Mutex::default(), changed: Condvar::new(), engine }
    }

    fn submit(
        self: &Arc<Self>,
        email: &Email,
        password: Password,
        work: Work,
    ) -> Result<Pending, Refusal> {
        let mut state = self.state.lock().unwrap();
        let now = (self.engine.clock)();
        state.expire_waiting(now);
        let load = PasswordLoad {
            worker_busy: state.running.is_some() || state.ready.is_some(),
            account_busy: state.account_busy(email),
            waiting: state.waiting.len(),
        };
        let admission = admit_password_check(self.door, load, &state.waits, email, now);
        if let Some(refusal) = admission.refusal() {
            return Err(refusal);
        }

        let (reply, receiver) = oneshot::channel();
        let id = Arc::new(());
        let job = Job { id: Arc::clone(&id), email: email.clone(), password, work, reply };
        let queued_at = match admission {
            PasswordAdmission::Check => {
                state.ready = Some(job);
                None
            }
            PasswordAdmission::WaitForWorker => {
                state.waiting.push_back(Waiting { queued_at: now, job });
                Some(now)
            }
            denied => return Err(denied.refusal().unwrap_or(Refusal::NotDealtWith)),
        };
        self.changed.notify_one();
        Ok(Pending { lane: Arc::clone(self), id, queued_at, receiver })
    }

    fn run(&self) {
        loop {
            let mut state = self.state.lock().unwrap();
            let job = loop {
                if state.closed {
                    return;
                }
                if let Some(job) = state.take_next((self.engine.clock)()) {
                    break job;
                }
                state = self.changed.wait(state).unwrap();
            };
            drop(state);

            let Job { email, password, work, reply, .. } = job;
            let result = match work {
                Work::Check(stored) => self.engine.check(&password, &email, stored).map(Completed::Checked),
                Work::Fingerprint => self.engine.fresh(&password).map(Completed::Fingerprint),
            };
            drop(password);
            let mut state = self.state.lock().unwrap();
            let now = (self.engine.clock)();
            if matches!(result, Ok(Completed::Checked(Checked::Wrong))) {
                state.waits.record_wrong(self.door, &email, now);
            }
            state.expire_waiting(now);
            state.running = None;
            let _ = reply.send(result);
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
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
                    self.lane.state.lock().unwrap().expire_waiting(now);
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
        let mut state = self.lane.state.lock().unwrap();
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
        Self::with_engine(Engine { clock, dummy_key, slow: Box::new(argon) })
    }

    fn with_engine(engine: Engine) -> Result<Self, Refusal> {
        let engine = Arc::new(engine);
        let known = Arc::new(Lane::new(PasswordDoor::KnownDevice, Arc::clone(&engine)));
        let other = Arc::new(Lane::new(PasswordDoor::Other, engine));
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
            PasswordDoor::KnownDevice => &self.known,
            PasswordDoor::Other => &self.other,
        }
    }

    /// A missing verifier gets the dummy check. A switched-off account with a
    /// verifier supplies it as usual; kept decides what a correct password permits.
    pub async fn check(
        &self,
        email: &Email,
        door: PasswordDoor,
        password: Password,
        stored: Option<StoredPassword>,
    ) -> Result<Checked, Refusal> {
        match self.lane(door).submit(email, password, Work::Check(stored))?.finish().await? {
            Completed::Checked(answer) => Ok(answer),
            Completed::Fingerprint(_) => Err(Refusal::NotDealtWith),
        }
    }

    /// New passwords use the given door's admission while separate admission
    /// for authorized fingerprint work is pending in core.
    pub async fn fingerprint(
        &self,
        email: &Email,
        door: PasswordDoor,
        password: Password,
    ) -> Result<StoredPassword, Refusal> {
        match self.lane(door).submit(email, password, Work::Fingerprint)?.finish().await? {
            Completed::Fingerprint(password) => Ok(password),
            Completed::Checked(_) => Err(Refusal::NotDealtWith),
        }
    }

    /// Reserve one attempt before computing and looking up the fast code fingerprint.
    pub fn admit_code(&self) -> Result<(), Refusal> {
        let mut codes = self.codes.lock().unwrap();
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
mod tests;
