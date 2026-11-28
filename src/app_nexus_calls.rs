//! Worker calls for the account features. W7 owns sign-in, the bearer, dispatch
//! and stamp processing. An operation only carries the context it began under.

use std::sync::OnceLock;

use nexus_core::nexus::{
    account::{AccountId, RequestHead, RequestId, SessionId},
    client::{AppScope, AppSession},
    proto::{BackendId, SPEAKS},
    requests::{TransportError, TransportReply},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Context {
    pub generation: u64,
    pub backend: BackendId,
    pub account: AccountId,
    pub session: SessionId,
    pub restore: u64,
    pub life: u64,
    pub offline: bool,
    pub verified: bool,
}

impl Context {
    pub fn head(self) -> RequestHead {
        RequestHead { speaks: SPEAKS, restore: self.restore }
    }

    pub fn scope(self) -> AppScope {
        AppScope { generation: self.generation, backend: self.backend, account: self.account, session: self.session,
            restore: self.restore, life: self.life }
    }

    pub fn same_session(self, other: Self) -> bool {
        self.scope().same_session(other.scope())
    }
}

impl From<AppSession> for Context {
    fn from(AppSession { scope, offline, verified }: AppSession) -> Self {
        Self { generation: scope.generation, backend: scope.backend, account: scope.account, session: scope.session,
            restore: scope.restore, life: scope.life, offline, verified }
    }
}

pub(crate) struct Driver {
    pub context: fn() -> Option<Context>,
    pub post: fn(Context, &str, &[u8]) -> Result<TransportReply, TransportError>,
}

static DRIVER: OnceLock<Driver> = OnceLock::new();

pub(crate) fn attach(driver: Driver) {
    let _ = DRIVER.set(driver);
}

pub(crate) fn current() -> Option<Context> {
    (DRIVER.get()?.context)()
}

pub(crate) fn active(context: Context) -> bool {
    current().is_some_and(|now| !now.offline && now.verified && now.same_session(context))
}

pub(crate) fn post(
    context: Context,
    path: &str,
    body: &[u8],
) -> Result<TransportReply, TransportError> {
    let driver = DRIVER.get().ok_or(TransportError::Stopped)?;
    if !active(context) {
        return Err(TransportError::Stopped);
    }
    (driver.post)(context, path, body)
}

pub(crate) fn new_id() -> Result<RequestId, TransportError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| TransportError::Unreachable)?;
    RequestId::new(u64::from_ne_bytes(bytes)).ok_or(TransportError::Unreachable)
}
