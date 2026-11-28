//! Worker calls for the account features. W7 owns sign-in, the bearer, dispatch
//! and stamp processing. An operation only carries the context it began under.

use std::sync::OnceLock;
use std::time::Duration;

use nexus_core::nexus::{
    account::{AccountId, RequestHead, RequestId, SessionId},
    proto::{BackendId, Speaks},
    requests::{TransportError, TransportReply},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Context {
    pub generation: u64,
    pub backend: BackendId,
    pub account: AccountId,
    pub session: SessionId,
    pub speaks: Speaks,
    pub restore: u64,
    pub life: u64,
    pub target_revision: Option<u64>,
    pub offline: bool,
}

impl Context {
    pub fn head(self) -> RequestHead {
        RequestHead { speaks: self.speaks, restore: self.restore }
    }

    pub fn same_session(self, other: Self) -> bool {
        self.generation == other.generation
            && self.backend == other.backend
            && self.account == other.account
            && self.session == other.session
            && self.speaks == other.speaks
            && self.restore == other.restore
    }
}

pub(crate) struct Driver {
    pub context: fn() -> Option<Context>,
    pub post: fn(Context, &str, &[u8], usize, Duration) -> Result<TransportReply, TransportError>,
    pub notice_seen: fn(),
}

static DRIVER: OnceLock<Driver> = OnceLock::new();

pub(crate) fn attach(driver: Driver) {
    let _ = DRIVER.set(driver);
}

pub(crate) fn current() -> Option<Context> {
    (DRIVER.get()?.context)()
}

pub(crate) fn active(context: Context) -> bool {
    current().is_some_and(|now| !now.offline && now.same_session(context))
}

pub(crate) fn post(
    context: Context,
    path: &str,
    body: &[u8],
    limit: usize,
    timeout: Duration,
) -> Result<TransportReply, TransportError> {
    let driver = DRIVER.get().ok_or(TransportError::Stopped)?;
    if !active(context) {
        return Err(TransportError::Stopped);
    }
    (driver.post)(context, path, body, limit, timeout)
}

pub(crate) fn notice_seen() {
    if let Some(driver) = DRIVER.get() {
        (driver.notice_seen)();
    }
}

pub(crate) fn new_id() -> Result<RequestId, TransportError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| TransportError::Unreachable)?;
    RequestId::new(u64::from_ne_bytes(bytes)).ok_or(TransportError::Unreachable)
}
