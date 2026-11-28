//! App-session features use the same checked origin, ask fence and receipt handling as sign-in.

use super::*;
use crate::nexus::requests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AppScope {
    pub generation: u64,
    #[serde(serialize_with = "hex_id")]
    pub backend: BackendId,
    pub account: AccountId,
    pub session: SessionId,
    pub restore: u64,
    pub life: u64,
}

impl AppScope {
    pub fn head(self) -> RequestHead {
        RequestHead { speaks: SPEAKS, restore: self.restore }
    }

    pub fn same_session(self, other: Self) -> bool {
        Self { life: other.life, ..self } == other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AppSession {
    pub scope: AppScope,
    pub offline: bool,
    /// This run identified the expected backend and no call ruled it incompatible.
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppResult {
    /// The transport may publish the whole answer it handed to the machine.
    Checked,
    Invalid,
    Stopped,
    Unreachable,
}

#[derive(Clone, Copy)]
pub(super) enum Endpoint {
    Stats,
    Grant,
    Delete,
    Requests,
}

impl Endpoint {
    fn at(path: &str) -> Option<Self> {
        Some(match path {
            events::STATS_PATH => Self::Stats,
            account::GRANT => Self::Grant,
            account::DELETE_LISTENING_DATA => Self::Delete,
            requests::REQUESTS => Self::Requests,
            _ => return None,
        })
    }

    fn path(self) -> &'static str {
        match self {
            Self::Stats => events::STATS_PATH,
            Self::Grant => account::GRANT,
            Self::Delete => account::DELETE_LISTENING_DATA,
            Self::Requests => requests::REQUESTS,
        }
    }

    pub(super) fn limits(self) -> (u64, usize) {
        match self {
            Self::Stats => (CALL_WITHIN_MS, events::STATS_REPLY_BYTES),
            Self::Grant | Self::Delete => (CALL_WITHIN_MS, account::SMALL_REPLY_MAX_BYTES),
            Self::Requests => (35_000, requests::LIST_REPLY_MAX_BYTES),
        }
    }

    fn accepts(self, scope: AppScope, bytes: &[u8]) -> bool {
        let head = match self {
            Self::Stats => account::read::<events::StatsRequest>(bytes).ok().map(|ask| ask.head),
            Self::Grant => account::read::<account::GrantRequest>(bytes).ok().and_then(|ask| {
                let account::GrantChange::DeleteListeningData { account, .. } = ask.change;
                (account == scope.account).then_some(ask.head)
            }),
            Self::Delete => account::read::<account::DeleteListeningData>(bytes).ok()
                .and_then(|ask| (ask.change.account == scope.account).then_some(ask.head)),
            Self::Requests => requests::read_call(bytes).ok().map(|ask| ask.head),
        };
        head.is_some_and(|head| head == scope.head())
    }

    fn answer(self, status: u16, body: &[u8]) -> Outcome<()> {
        fn discard<T>(result: Outcome<T>) -> Outcome<()> {
            match result {
                Outcome::Done(_, seen) => Outcome::Done((), seen),
                Outcome::Refused(why, seen) => Outcome::Refused(why, seen),
                Outcome::Nothing => Outcome::Nothing,
            }
        }
        match self {
            Self::Stats => discard(outcome(events::read_stats_reply(status, body))),
            Self::Grant => discard(outcome(account::read_reply::<account::GrantIssued>(status, body))),
            Self::Delete => discard(outcome(account::read_reply::<SignedOut>(status, body))),
            Self::Requests => discard(outcome(requests::read_reply(status, body).map_err(account::BadMessage::from))),
        }
    }

    pub(super) fn life_bound(self) -> bool {
        matches!(self, Self::Grant | Self::Delete)
    }
}

impl Client {
    /// The public identity of the app session. Send authority stays inside the machine.
    pub fn app_session(&self) -> Option<AppSession> {
        let live = self.live.as_ref()?;
        let saved = live.saved.as_ref()?;
        saved.session.as_ref()?;
        let backend = BackendId::from_hex(&saved.backend_id)?;
        let verified = !live.stranger && live.too_old.is_none() && self.seen(&saved.nexus) == Some(backend);
        Some(AppSession {
            scope: AppScope {
                generation: self.generation,
                backend,
                account: saved.account,
                session: saved.sign_out.as_ref()?.session,
                restore: saved.restore,
                life: saved.life,
            },
            offline: live.offline,
            verified,
        })
    }

    pub(super) fn post_for_app(&mut self, request: u64, scope: AppScope, path: &str, body: Typed, out: &mut Vec<Do>) -> AppResult {
        let Some(endpoint) = Endpoint::at(path) else { return AppResult::Invalid };
        let Some(now) = self.app_session().filter(|now| !now.offline) else { return AppResult::Stopped };
        if !now.scope.same_session(scope) || (endpoint.life_bound() && now.scope.life != scope.life) {
            return AppResult::Stopped;
        }
        if !endpoint.accepts(scope, body.expose().as_bytes()) {
            return AppResult::Invalid;
        }
        let secret = self.session();
        let what = Purpose::App { request, endpoint, life: scope.life };
        if secret.is_some() && self.ask(what, endpoint.path(), secret.as_ref(), Some(Hidden(body.0)), out) {
            AppResult::Checked
        } else {
            AppResult::Unreachable
        }
    }

    pub(super) fn answer_for_app(&mut self, endpoint: Endpoint, status: u16, body: &[u8], given: &Given, out: &mut Vec<Do>) -> AppResult {
        let answer = endpoint.answer(status, body);
        let invalid = matches!(answer, Outcome::Nothing);
        match self.common(answer, given, out) {
            None => AppResult::Stopped,
            Some(_) if invalid => AppResult::Invalid,
            Some(_) => AppResult::Checked,
        }
    }
}
