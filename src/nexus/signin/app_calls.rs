use super::*;
use core::{AppResult, AppSession};
use nexus_core::nexus::requests::{TransportError, TransportReply};

use crate::app_nexus_calls::{self, Context, Driver};

static PUBLISHED: Mutex<Option<AppSession>> = Mutex::new(None);
static PENDING: LazyLock<Mutex<HashMap<u64, Pending>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

struct Pending {
    context: Context,
    done: Sender<Result<(u16, Zeroizing<Vec<u8>>), TransportError>>,
    answer: Option<(u16, Zeroizing<Vec<u8>>)>,
}

pub(super) fn attach() {
    app_nexus_calls::attach(Driver { context, post });
}

fn context() -> Option<Context> {
    if QUITTING.load(Ordering::SeqCst) || NOT_KEPT.load(Ordering::SeqCst) {
        return None;
    }
    let live = machine().client.app_session()?;
    let kept = (*PUBLISHED.lock().ok()?)?;
    if live.scope != kept.scope {
        return None;
    }
    Some(Context::from(AppSession { offline: live.offline || OFFLINE.load(Ordering::SeqCst), ..live }))
}

// Publish after the worker has applied the session's keep and journal effects. Compare
// with the persisted view because the reducer may already have taken another step.
pub(super) fn publish() {
    let state = machine().client.app_session().filter(|app| {
        let view = view();
        let Some(saved) = view.record.saved.as_ref() else { return false };
        let scope = app.scope;
        saved.get("backend_id").and_then(|id| id.as_str()) == Some(scope.backend.to_hex().as_str())
            && saved.get("account").and_then(|id| id.as_u64()) == Some(scope.account.get())
            && saved.pointer("/sign_out/session").and_then(|id| id.as_u64()) == Some(scope.session.get())
            && saved.get("life").and_then(|life| life.as_u64()) == Some(scope.life)
            && saved.get("restore").and_then(|restore| restore.as_u64()) == Some(scope.restore)
    });
    let changed = {
        let mut prior = PUBLISHED.lock().unwrap_or_else(|poison| poison.into_inner());
        if *prior == state {
            return;
        }
        let changed = match (*prior, state) {
            (Some(before), Some(now)) => !before.scope.same_session(now.scope)
                || before.offline != now.offline || before.verified != now.verified,
            _ => true,
        };
        *prior = state;
        changed
    };
    crate::listening::session_changed();
    if changed {
        crate::requests::refresh();
    }
}

fn pending() -> MutexGuard<'static, HashMap<u64, Pending>> {
    PENDING.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(super) fn waiting(request: u64) -> bool {
    pending().contains_key(&request)
}

pub(super) fn stage(request: u64, status: u16, body: &[u8]) {
    if let Some(pending) = pending().get_mut(&request) {
        pending.answer = Some((status, Zeroizing::new(body.to_vec())));
    }
}

pub(super) fn finish(request: u64, result: AppResult) {
    let Some(Pending { done, answer, context: owner }) = pending().remove(&request) else { return };
    let result = if result == AppResult::Checked && !app_nexus_calls::active(owner) {
        AppResult::Stopped
    } else {
        result
    };
    let result = match (result, answer) {
        (AppResult::Checked, Some(answer)) => Ok(answer),
        (AppResult::Stopped, _) => Err(TransportError::Stopped),
        _ => Err(TransportError::Unreachable),
    };
    let _ = done.send(result);
}

fn post(context: Context, path: &str, body: &[u8]) -> Result<TransportReply, TransportError> {
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let text = std::str::from_utf8(body).map_err(|_| TransportError::Unreachable)?;
    let (done, result) = mpsc::channel();
    pending().insert(request, Pending { context, done, answer: None });
    tell(Happened::AppPost { request, scope: context.scope(), path: path.into(), body: Typed::new(text) });
    // Includes the effect queue, W7's initial delay, the child process deadline and the
    // writer's confirmation of a life move. This worker never retries an expired ask.
    match result.recv_timeout(Duration::from_secs(57)) {
        Ok(answer) => answer.map(|(status, mut body)| TransportReply { status, body: std::mem::take(&mut *body) }),
        Err(_) => {
            pending().remove(&request);
            tell(Happened::AppCancelled { request });
            Err(if app_nexus_calls::active(context) { TransportError::Unreachable } else { TransportError::Stopped })
        }
    }
}
