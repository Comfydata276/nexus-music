//! The machine's moves: what each thing that happened does.

use super::words::{self, Row};
use super::*;

impl Live {
    /// The session, when one is held.
    pub(super) fn session(&self) -> Option<&Session> {
        self.saved.as_ref()?.session.as_ref()
    }

    pub(super) fn signed_in(&self) -> bool {
        self.session().is_some()
    }

    pub(super) fn held_count(&self) -> Option<u64> {
        self.saved.as_ref()?.music.as_ref().map(|login| login.count)
    }

    pub(super) fn waiting(&self, what: impl Fn(&Purpose) -> bool) -> bool {
        self.flight.iter().any(|flight| what(&flight.what))
    }

    /// Where an ask of this kind goes, and the backend expected there (`None` for one that
    /// carries nothing secret, or whose own `hello` is the check).
    fn origin_for(&self, what: &Purpose) -> Option<(NexusOrigin, Option<(String, String)>)> {
        match what {
            Purpose::Hello(pending) => Some((pending.origin.clone(), None)),
            Purpose::SignIn { origin, .. } => Some((origin.clone(), None)),
            Purpose::Verify { origin } => Some((NexusOrigin::typed(origin).ok()?, None)),
            Purpose::Tell { origin, backend, .. } => Some((NexusOrigin::typed(origin).ok()?, Some((origin.clone(), backend.to_hex())))),
            _ if self.stranger => None,
            _ => {
                let saved = self.saved.as_ref()?;
                Some((NexusOrigin::typed(&saved.nexus).ok()?, Some((saved.nexus.clone(), saved.backend_id.clone()))))
            }
        }
    }
}

/// A number for one change, made when the person pressed (12.2), from the random bytes
/// handed in: kept below 2^53 so that it is the same number in a Kotlin `Long` and in JSON
/// read by anything. `None` with no random bytes, or (once in 2^53) all of them zero.
pub(super) fn request_id(given: &Given) -> Option<account::RequestId> {
    let random = given.random.as_ref()?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(random.0.get(..8)?);
    account::RequestId::new(u64::from_le_bytes(bytes) & ((1 << 53) - 1))
}

/// Forgets notes too old to matter (the session they would end is gone by itself, 8.7),
/// and retires their tellings.
fn prune_expired(live: &mut Live, now_ms: u64, out: &mut Vec<Do>) {
    let expired: Vec<Forget> =
        live.notes.iter().filter(|note| now_ms.checked_sub(note.made_ms).is_some_and(|age| age >= NOTE_LIFE_MS)).map(Note::forget).collect();
    for forget in expired {
        if let Forget::Note { backend_id, session } = &forget {
            retire(live, backend_id, *session);
        }
        out.push(Do::Forget(forget));
    }
}

/// Takes a note out of memory and stops waiting for its telling.
fn retire(live: &mut Live, backend_id: &str, session: u64) {
    live.notes.retain(|note| note.key() != (backend_id, session));
    let backend = BackendId::from_hex(backend_id);
    live.flight.retain(|flight| !matches!(&flight.what, Purpose::Tell { backend: told, session: number, .. } if Some(*told) == backend && *number == session));
}

/// A body account encoded, as the text an ask carries.
pub(super) fn hidden(body: Result<account::WireBody, account::BadMessage>) -> Option<Hidden> {
    let body = body.ok()?;
    let text = std::str::from_utf8(body.bytes()).ok()?;
    Some(Hidden(Zeroizing::new(text.to_owned())))
}

impl Client {
    // -----------------------------------------------------------------------------------
    // Asking
    // -----------------------------------------------------------------------------------

    /// Asks, if asking is allowed now: never while Offline is on; never to a backend that
    /// said it speaks no number of ours; never two of one kind at once; and **never anything
    /// carrying a session, a device secret or a note to an origin before its `hello` has
    /// been read this run and named the backend expected** (7.1): until then a `hello` is
    /// asked there in its place. Says whether it asked. The address is made by account's
    /// [`NexusOrigin::call`] from a path constant, never from text handed in.
    pub(super) fn ask(&mut self, what: Purpose, path: &'static str, credential: Option<&Session>, body: Option<Hidden>, out: &mut Vec<Do>) -> bool {
        let Some(live) = self.live.as_mut() else { return false };
        if live.offline || live.flight.len() >= FLIGHT_MAX || live.waiting(|other| other.same_kind(&what)) {
            return false;
        }
        let Some((origin, expected)) = live.origin_for(&what) else { return false };
        let at = origin.origin().to_string();
        if !matches!(what, Purpose::Hello(_) | Purpose::Verify { .. }) && live.too_old.as_deref() == Some(at.as_str()) {
            return false;
        }
        if let Some((nexus, backend)) = expected {
            let Some(backend) = BackendId::from_hex(&backend) else { return false };
            match self.seen(&nexus) {
                Some(seen) if seen == backend => {}
                Some(_) => return false,
                None => {
                    self.ask(Purpose::Verify { origin: nexus }, proto::HELLO, None, None, out);
                    return false;
                }
            }
        }
        let Ok(address) = origin.call(path) else { return false };
        let (method, within_ms, max_body) = match &what {
            Purpose::Hello(_) | Purpose::Verify { .. } => (Method::Get, HELLO_WITHIN_MS, HELLO_MAX_BYTES),
            Purpose::SignIn { .. } => (Method::Post, SIGN_IN_WITHIN_MS, account::SIGN_IN_REPLY_MAX_BYTES),
            Purpose::Devices { .. } => (Method::Post, CALL_WITHIN_MS, account::LIST_REPLY_MAX_BYTES),
            Purpose::Events { .. } => (Method::Post, CALL_WITHIN_MS, events::BATCH_REPLY_BYTES),
            Purpose::App { endpoint, .. } => {
                let (within, max) = endpoint.limits();
                (Method::Post, within, max)
            }
            _ => (Method::Post, CALL_WITHIN_MS, account::SMALL_REPLY_MAX_BYTES),
        };
        if (method == Method::Post) != body.is_some() {
            return false;
        }
        let Some(live) = self.live.as_mut() else { return false };
        self.next_ask = self.next_ask.wrapping_add(1);
        let id = self.next_ask;
        let authorization = credential.map(|session| Hidden(session.authorization()));
        let app_request = match &what {
            Purpose::App { request, .. } => Some(*request),
            _ => None,
        };
        live.flight.push(Flight { id, what, max_body });
        out.push(Do::Ask(Ask { id, generation: self.generation, method, url: address.wire(), authorization, body, within_ms, max_body, app_request }));
        true
    }

    pub(super) fn head(&self) -> Option<RequestHead> {
        let saved = self.live.as_ref()?.saved.as_ref()?;
        Some(RequestHead { speaks: SPEAKS, restore: saved.restore })
    }

    pub(super) fn session(&self) -> Option<Session> {
        self.live.as_ref()?.session().cloned()
    }

    fn ask_devices(&mut self, page: usize, after: Option<DeviceId>, opened: bool, out: &mut Vec<Do>) -> bool {
        let (Some(session), Some(head)) = (self.session(), self.head()) else { return false };
        let Some(body) = hidden(account::encode(&ListDevices { head, after })) else { return false };
        let asked = self.ask(Purpose::Devices { page, opened }, account::LIST_DEVICES, Some(&session), Some(body), out);
        if asked && let Some(live) = self.live.as_mut() {
            live.check_due = false;
        }
        asked
    }

    /// One renewal of the music login for this number, if the rule allows one now. One that
    /// cannot be asked now is owed, and asked when the backend may be reachable.
    fn ask_music(&mut self, count: u64, refused: bool, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let answered = if refused { live.renewal.refreshed } else { live.renewal.rose };
        if answered == Some(count) || live.waiting(|what| matches!(what, Purpose::Music { .. })) {
            return;
        }
        let (Some(session), Some(head)) = (self.session(), self.head()) else { return };
        let Some(body) = hidden(account::encode(&GetMusicLogin { head })) else { return };
        let asked = self.ask(Purpose::Music { count, refused }, account::MUSIC_LOGIN, Some(&session), Some(body), out);
        if let Some(live) = self.live.as_mut() {
            live.renewal.owed = if asked { None } else { Some((count, refused)) };
        }
    }

    /// Tells the backend of each waiting sign-out, from its note: no session, only the
    /// secret that can end that one session (11), and only to the backend it was made for.
    fn ask_tell(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        prune_expired(live, given.now_ms, out);
        let mut told = false;
        let bodies: Vec<(Purpose, Option<Hidden>)> = live
            .notes
            .iter()
            .filter_map(|note| {
                let head = RequestHead { speaks: SPEAKS, restore: note.restore };
                let body = account::RequestId::new(note.request)
                    .and_then(|request_id| hidden(account::encode(&SignOut { head, request_id, revoke: Some(copy_note(&note.revoke)) })));
                let backend = BackendId::from_hex(&note.backend_id)?;
                Some((Purpose::Tell { backend, session: note.session(), origin: note.nexus.clone() }, body))
            })
            .collect();
        for (what, body) in bodies {
            if let Some(body) = body {
                told |= self.ask(what, account::SIGN_OUT, None, Some(body), out);
            }
        }
        if told && let Some(live) = self.live.as_mut() {
            live.told_ms = Some(given.now_ms);
        }
    }

    /// Something says the backend may be reachable now: ask what is owed (a renewal, the
    /// session's check if one is due, the waiting sign-outs).
    pub(super) fn after_reach(&mut self, given: &Given, out: &mut Vec<Do>, why: Reached) {
        let Some(live) = self.live.as_mut() else { return };
        if why == Reached::Start && live.saved.is_some() {
            live.diagnostics.signed_in(given.now_ms, out);
        }
        if live.offline {
            return;
        }
        if matches!(why, Reached::Start | Reached::Network | Reached::OfflineOff) {
            live.check_due = true;
        }
        // 6.3, step 5: the network coming back renews a login the backend had nothing
        // newer for, once more.
        if why == Reached::Network && live.renewal.refused.is_some() && live.renewal.refreshed == live.renewal.refused {
            live.renewal.refreshed = None;
            live.renewal.owed = live.renewal.refused.map(|count| (count, true));
        }
        let announced = live.renewal.announced;
        // A renewal owed by the answer being settled now is not asked by that same answer.
        let owed = live.renewal.owed.filter(|_| !(why == Reached::Answer && live.renewal.owed_now)).map(|(count, refused)| (announced.map_or(count, |announced| announced.max(count)), refused));
        let missing = live.signed_in() && live.held_count().is_none() && !(why == Reached::Answer && live.renewal.owed_now);
        let seen = live.music_seen;
        let check = live.check_due && live.signed_in();
        if !live.notes.is_empty() {
            self.ask_tell(given, out);
        }
        if let Some((count, refused)) = owed {
            self.ask_music(count, refused, out);
        } else if missing {
            // Signed in with no music login: ask for one, once for the number last told.
            self.ask_music(seen, false, out);
        }
        if check {
            self.ask_devices(0, None, false, out);
        }
        // The sending half: a hold is lifted by the backend being reachable again, never by
        // the answer that made it.
        self.lift_hold(Some(why), given);
        self.events_due(out);
    }

    /// After an answer's own part is done: what the backend being reachable again, or a
    /// number announced, makes due. Decided only now, so that an answer never asks again for
    /// what it has itself just settled.
    fn settle(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if std::mem::take(&mut live.restored) {
            self.after_reach(given, out, Reached::Answer);
        }
        let Some(live) = self.live.as_mut() else { return };
        let due = live.renewal.announced.filter(|announced| *announced > live.music_seen);
        if live.renewal.announced.is_some_and(|announced| announced <= live.music_seen) {
            live.renewal.announced = None;
        }
        if let Some(count) = due
            && live.renewal.owed.is_none()
            && live.signed_in()
            && !live.waiting(|what| matches!(what, Purpose::Music { .. }))
        {
            live.renewal.announced = None;
            self.ask_music(count, false, out);
        }
        // Whatever kept the sending half from beginning (a `hello` not yet read, no one
        // signed in) may be gone now. A hold is not lifted here.
        self.events_due(out);
        if let Some(live) = self.live.as_mut() {
            live.renewal.owed_now = false;
            live.sending.clear_held_now();
        }
    }

    // -----------------------------------------------------------------------------------
    // Presses
    // -----------------------------------------------------------------------------------

    pub(super) fn pressed(&mut self, press: Press, given: &Given, out: &mut Vec<Do>) {
        match press {
            Press::SignIn { address, email, password, look } => {
                if self.sign_in_gate() {
                    self.press_sign_in(address, Some(email), password, None, look, given, out);
                }
            }
            Press::SignInWithCode { address, code, password, again, look } => {
                if !self.sign_in_gate() {
                    return;
                }
                if password.expose() != again.expose() {
                    return self.set_form(SignInState::PasswordsDiffer, 0);
                }
                self.press_sign_in(address, None, code, Some(password), look, given, out)
            }
            Press::FormClosed => self.drop_pending(),
            Press::SignOut => self.sign_out(given, out),
            Press::OpenDevices => self.open_devices(given, out),
            Press::ForceOut { device } => self.press_force_out(device, given, out),
            Press::ChangePassword { current, new, again } => self.press_change(current, new, again, given, out),
            Press::NoticeSeen => {
                if let Some(live) = self.live.as_mut()
                    && let Some(saved) = live.saved.as_mut()
                    && saved.notice.take().is_some()
                {
                    out.push(Do::Keep(Keep::Saved(saved.copy())));
                }
            }
        }
    }

    /// Whether a sign-in press is taken at all, checked before the form is touched: not while
    /// signed in, and not while a sign-in request is out (two quick presses are one try). A
    /// try still at its `hello` is dropped, with what was typed, and this press replaces it.
    fn sign_in_gate(&mut self) -> bool {
        let Some(live) = self.live.as_mut() else { return false };
        if (live.signed_in() && !live.stranger) || live.waiting(|what| matches!(what, Purpose::SignIn { .. })) {
            return false;
        }
        self.drop_pending();
        true
    }

    /// Drops a try still at its `hello`, and the password it holds: its answer asks nothing.
    fn drop_pending(&mut self) {
        if let Some(live) = self.live.as_mut() {
            live.flight.retain(|flight| !matches!(flight.what, Purpose::Hello(_)));
            if live.form.state == SignInState::Working && !live.waiting(|what| matches!(what, Purpose::SignIn { .. })) {
                live.form.state = SignInState::Idle;
                live.form.words.clear();
            }
        }
    }

    /// The sign-in form's state and its sentence. `wait_s`: for "busy", the exact seconds the
    /// backend said to come back in, shown for the form to count down.
    fn set_form(&mut self, state: SignInState, wait_s: u64) {
        if let Some(live) = self.live.as_mut() {
            live.form.words = words::sign_in(state, live.app.kind, wait_s);
            live.form.wait_s = (state == SignInState::Busy && wait_s > 0).then_some(wait_s);
            live.form.state = state;
            live.form.ask_permission = state == SignInState::NeedsPermission && live.app.kind == DeviceKind::Phone;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn press_sign_in(&mut self, address: String, email: Option<String>, secret: Typed, new: Option<Typed>, look: Look, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        // Bounded before anything of it is kept or shown.
        if address.len() > TYPED_MAX_BYTES {
            return self.set_form(SignInState::BadAddress, 0);
        }
        if email.as_ref().is_some_and(|email| email.len() > TYPED_MAX_BYTES) {
            return self.set_form(SignInState::BadEmail, 0);
        }
        live.form.address = address.trim().to_owned();
        if let Some(email) = &email {
            live.form.email = email.trim().to_owned();
        }
        if live.offline {
            return self.set_form(SignInState::Offline, 0);
        }
        let origin = match typed_origin(&address) {
            Ok(origin) => origin,
            Err(state) => return self.set_form(state, 0),
        };
        // 6.4: judged before connecting. It leads inside and the permission is not held:
        // nothing is asked (Android would drop it silently); the phone's screen asks.
        if look.blocked() {
            return self.set_form(SignInState::NeedsPermission, 0);
        }
        let credential = match (email, new) {
            (Some(email), _) => {
                let Some(email) = Email::new(&email) else { return self.set_form(SignInState::BadEmail, 0) };
                if secret.expose().is_empty() {
                    return self.set_form(SignInState::NoPassword, 0);
                }
                // A password typed to sign in is presented as typed, of any length (account's
                // rule for a new password is not this one's). Longer than any can be: nobody's.
                let Some(password) = Password::presented(secret.expose()) else { return self.set_form(SignInState::WrongLogin, 0) };
                Pressed::Password { email, password }
            }
            (None, new) => {
                let Some(new) = new.filter(|new| !new.expose().is_empty()) else { return self.set_form(SignInState::NoPassword, 0) };
                if secret.expose().trim().is_empty() {
                    return self.set_form(SignInState::NoPassword, 0);
                }
                // The password chosen is held to the rule before the code is spent on it. The
                // email is not known here: "not the email" is the backend's to check.
                let Some(new_password) = Password::new(new.expose()) else { return self.set_form(SignInState::PasswordNotAllowed, 0) };
                match OneTimeCode::typed(secret.expose()) {
                    Some(code) => Pressed::Code { code, new_password },
                    None => return self.set_form(SignInState::WrongLogin, 0),
                }
            }
        };
        if live.device_secret.is_none() {
            match &given.random {
                Some(random) => {
                    let key = DeviceKey { secret: DeviceSecret::from_bytes(*random.0), backend_id: None };
                    out.push(Do::Keep(Keep::DeviceSecret(key.clone())));
                    live.device_secret = Some(key);
                }
                None => return self.set_form(SignInState::NoRandom, 0),
            }
        }
        if self.ask(Purpose::Hello(Pending { origin, credential }), proto::HELLO, None, None, out) {
            self.set_form(SignInState::Working, 0);
        } else {
            self.set_form(SignInState::CantReach, 0);
        }
    }

    /// Asks to sign in, once `hello` has said this is a backend that has accounts. The device
    /// secret is the one kept for this backend id (or made before any sign-in); for any other
    /// id this is a new device, with a new secret from this call's random bytes, kept only if
    /// the sign-in succeeds (as ruled). Says whether it asked; `None` with no random bytes.
    fn ask_sign_in(&mut self, origin: NexusOrigin, backend_id: BackendId, credential: Pressed, given: &Given, out: &mut Vec<Do>) -> Option<bool> {
        let Some(live) = self.live.as_ref() else { return Some(false) };
        let ours = live.device_secret.as_ref().filter(|key| key.backend_id.as_deref().is_none_or(|id| BackendId::from_hex(id) == Some(backend_id)));
        let device_secret = match ours {
            Some(key) => key.secret.clone(),
            None => DeviceSecret::from_bytes(*given.random.as_ref()?.0),
        };
        // 12.3: a sign-in names the restore number the device has, only if it has one from
        // this backend at this origin. A different id there is a sign-in to a new backend:
        // it names none, and what is kept changes only if it succeeds.
        let restore = live
            .saved
            .as_ref()
            .filter(|saved| NexusOrigin::typed(&saved.nexus).ok().as_ref() == Some(&origin) && BackendId::from_hex(&saved.backend_id) == Some(backend_id))
            .map(|saved| saved.restore);
        let credential = match credential {
            Pressed::Password { email, password } => Credential::Password { email, password },
            Pressed::Code { code, new_password } => Credential::Code { code, new_password },
        };
        let message = SignIn {
            credential,
            device_secret: device_secret.clone(),
            device_name: live.app.device_name.clone(),
            device_kind: live.app.kind,
            app_version: live.app.version,
            speaks: SPEAKS,
            restore,
        };
        let Some(body) = hidden(account::encode(&message)) else { return Some(false) };
        Some(self.ask(Purpose::SignIn { origin, backend_id, secret: device_secret }, account::SIGN_IN, None, Some(body), out))
    }

    /// Signing out here: lasting on the device at once, whatever the network. The note that
    /// can end the session is kept **before** what signing in left is forgotten, so that a
    /// death between the two leaves the session endable; then the backend is told from it
    /// (11, as ruled).
    fn sign_out(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let Some(saved) = live.saved.take() else { return };
        if let (Some(_), Some(revoke)) = (&saved.session, saved.sign_out.as_ref()) {
            // One number for this sign-out, the same for every telling of it (12.2). With
            // no random bytes, the session's own number, which is never given again.
            let request = request_id(given).map_or(revoke.session.get(), account::RequestId::get);
            let note = Note {
                request,
                revoke: copy_note(revoke),
                nexus: saved.nexus.clone(),
                backend_id: saved.backend_id.clone(),
                restore: saved.restore,
                made_ms: given.now_ms,
            };
            // The new note is kept first; only then, past four, the oldest goes, and its
            // telling is no longer waited for (as ruled). Expired ones go before that.
            prune_expired(live, given.now_ms, out);
            let (backend_id, session) = (note.backend_id.clone(), note.session());
            live.notes.retain(|kept| kept.key() != (backend_id.as_str(), session));
            out.push(Do::Keep(Keep::Note(note.copy())));
            // Room is made among the notes there before this one, never by the new one, even
            // if the clock was set back behind them all (as ruled).
            let mut dropped = Vec::new();
            while live.notes.len() >= NOTES_MAX {
                let Some(index) = live.notes.iter().enumerate().min_by_key(|(_, kept)| kept.made_ms).map(|(index, _)| index) else { break };
                let gone = live.notes.remove(index);
                out.push(Do::Forget(gone.forget()));
                retire(live, &gone.backend_id, gone.session());
                dropped.push((BackendId::from_hex(&gone.backend_id), gone.session()));
            }
            live.notes.push(note);
            self.parting.retain(|parting| !dropped.contains(&(Some(parting.backend), parting.session)));
        }
        out.push(Do::Forget(Forget::Saved));
        drop(saved);
        self.ended(SignInState::SignedOut);
        self.ask_tell(given, out);
    }

    /// The account signed in is gone (signed out, forced out, the data put back): fence
    /// what was asked for it, and go back to the sign-in form. Telling the backend of a
    /// sign-out is the device's, not the account's, and is kept.
    fn ended(&mut self, state: SignInState) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(live) = self.live.as_mut() {
            live.flight.retain(|flight| flight.what.parting());
            live.renewal = Renewal::default();
            live.devices = Devices::not_asked();
            live.password = PasswordShown::idle();
            live.music_words = None;
            live.reach = Reach::Unknown;
            live.check_due = false;
            live.stranger = false;
            live.backend_blocked = false;
            live.sending.reset();
            live.diagnostics.drop_read();
        }
        self.set_form(state, 0);
    }

    fn open_devices(&mut self, given: &Given, out: &mut Vec<Do>) {
        let _ = given;
        let Some(live) = self.live.as_mut() else { return };
        if !live.signed_in() || live.stranger {
            return;
        }
        if live.offline {
            live.devices.state = ListState::Failed;
            live.devices.words = words::devices(words::Devices::Offline);
            return;
        }
        // A check under way lists them too: it is now looked at.
        for flight in &mut live.flight {
            if let Purpose::Devices { opened, .. } = &mut flight.what {
                *opened = true;
                live.devices.state = ListState::Working;
                return;
            }
        }
        if self.ask_devices(0, None, true, out) {
            if let Some(live) = self.live.as_mut() {
                live.devices.state = ListState::Working;
                live.devices.words.clear();
                live.devices.reading.clear();
            }
        } else if let Some(live) = self.live.as_mut() {
            live.devices.state = ListState::Failed;
            live.devices.words = words::devices(words::Devices::CantReach);
        }
    }

    fn press_force_out(&mut self, device: u64, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        let Some(saved) = &live.saved else { return };
        if !live.signed_in() || live.stranger {
            return;
        }
        let row = live.devices.rows.iter().find(|row| row.id == device);
        if device == saved.device || row.is_some_and(|row| row.this_one) {
            // Forcing out this device is signing out here.
            return self.sign_out(given, out);
        }
        // 8.7: forcing out names the session it ends, as the list showed it; one that is
        // signed out, unknown, or being forced out already asks nothing.
        let Some(session) = row.filter(|row| row.can_force_out && !row.working).and_then(|row| row.session).and_then(SessionId::new) else { return };
        let offline = live.offline;
        let (Some(bearer), Some(head), Some(id)) = (self.session(), self.head(), DeviceId::new(device)) else { return };
        if offline {
            if let Some(live) = self.live.as_mut() {
                live.devices.words = words::devices(words::Devices::Offline);
            }
            return;
        }
        let Some(body) = hidden(account::encode(&ForceOut { head, device: id, session })) else { return };
        if self.ask(Purpose::ForceOut { device, session: session.get() }, account::FORCE_OUT, Some(&bearer), Some(body), out) {
            if let Some(live) = self.live.as_mut() {
                live.devices.words.clear();
                for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                    row.working = true;
                }
            }
        } else if let Some(live) = self.live.as_mut() {
            live.devices.words = words::devices(words::Devices::CantReach);
        }
    }

    fn set_password(&mut self, state: PasswordState, wait_s: u64) {
        if let Some(live) = self.live.as_mut() {
            live.password = PasswordShown { state, words: words::password(state, wait_s, live.offline) };
        }
    }

    fn press_change(&mut self, current: Typed, new: Typed, again: Typed, given: &Given, out: &mut Vec<Do>) {
        let (Some(session), Some(head)) = (self.session(), self.head()) else { return };
        let Some(live) = self.live.as_ref() else { return };
        let Some(saved) = &live.saved else { return };
        if live.stranger || live.waiting(|what| matches!(what, Purpose::ChangePassword)) {
            return;
        }
        if new.expose() != again.expose() {
            return self.set_password(PasswordState::Differ, 0);
        }
        let Some(email) = Email::new(&saved.email) else { return self.set_password(PasswordState::NotAllowed, 0) };
        // "Not on the list of the commonest" is the backend's (it holds the list, 8.2); the
        // app checks "not the email".
        let Some(new) = Password::new(new.expose()).filter(|new| new.allowed_for(&email, "")) else {
            return self.set_password(PasswordState::NotAllowed, 0);
        };
        // The present password is presented as typed, of any length.
        let Some(current) = Password::presented(current.expose()) else { return self.set_password(PasswordState::WrongCurrent, 0) };
        if live.offline {
            return self.set_password(PasswordState::CantReach, 0);
        }
        let Some(request_id) = request_id(given) else { return self.set_password(PasswordState::CantReach, 0) };
        let Some(body) = hidden(account::encode(&ChangePassword { head, request_id, current_password: current, new_password: new })) else { return };
        if self.ask(Purpose::ChangePassword, account::CHANGE_PASSWORD, Some(&session), Some(body), out) {
            self.set_password(PasswordState::Working, 0);
        } else {
            self.set_password(PasswordState::CantReach, 0);
        }
    }

    // -----------------------------------------------------------------------------------
    // Answers
    // -----------------------------------------------------------------------------------

    fn take_flight(&mut self, ask: u64) -> Option<(Purpose, usize)> {
        let live = self.live.as_mut()?;
        let index = live.flight.iter().position(|flight| flight.id == ask)?;
        let flight = live.flight.remove(index);
        Some((flight.what, flight.max_body))
    }

    /// The backend answered as the backend: it can be reached, and it is not one the phone
    /// lacks the permission for. If it could not be reached before, what was owed is asked
    /// when the answer's own part is done ([`Client::settle`]).
    pub(super) fn reached(&mut self) {
        let Some(live) = self.live.as_mut() else { return };
        if matches!(live.reach, Reach::Down { .. }) {
            live.restored = true;
        }
        live.reach = Reach::Up;
        live.backend_blocked = false;
    }

    pub(super) fn unreached(&mut self, given: &Given) {
        if let Some(live) = self.live.as_mut()
            && !matches!(live.reach, Reach::Down { .. })
        {
            live.reach = Reach::Down { since_ms: given.now_ms };
        }
    }

    /// What every answer to a call under the session shares (7.1), in this order: a receipt
    /// from another backend or for another account is not acted on; "this session has
    /// ended", "the server's data was put back" and "too old" are dealt with before anything
    /// else is made of the answer; then the backend was reached and the receipt is taken.
    /// Gives back the outcome for the call's own part, or `None` if it was dealt with here.
    pub(super) fn common<T>(&mut self, outcome: Outcome<T>, given: &Given, out: &mut Vec<Do>) -> Option<Outcome<T>> {
        let (expected, account, nexus) = {
            let saved = self.live.as_ref()?.saved.as_ref()?;
            (BackendId::from_hex(&saved.backend_id), saved.account, saved.nexus.clone())
        };
        let seen = match &outcome {
            Outcome::Done(_, seen) | Outcome::Refused(_, Some(seen)) => Some(*seen),
            _ => None,
        };
        if let Some(seen) = seen {
            // The one value: the backend last seen at this address (as ruled).
            self.saw(&nexus, seen.backend);
            if Some(seen.backend) != expected {
                // Another backend answers at this origin: it is not ours. Nothing more is
                // sent to it, and nothing it said is acted on (7.1).
                self.became_stranger();
                return None;
            }
            if seen.account != account {
                // Stamped for another account: none of it is this account's.
                return None;
            }
        }
        match &outcome {
            Outcome::Refused(Why::SessionEnded, _) => {
                self.forced_out(out);
                return None;
            }
            Outcome::Refused(Why::DataPutBack, _) => {
                self.start_again(out);
                return None;
            }
            Outcome::Refused(Why::Incompatible { .. }, _) => {
                if let Some(live) = self.live.as_mut() {
                    live.too_old = live.saved.as_ref().map(|saved| saved.nexus.clone());
                    live.flight.retain(|flight| flight.what.parting());
                    live.sending.reset();
                    live.diagnostics.drop_read();
                }
                return None;
            }
            Outcome::Nothing | Outcome::Refused(Why::NotDealtWith | Why::Malformed, None) => self.unreached(given),
            _ => self.reached(),
        }
        if let Some(seen) = seen
            && !self.absorb(seen, out)
        {
            return None;
        }
        Some(outcome)
    }

    /// The numbers a receipt carries, kept, never going back: life and change only rise; a
    /// receipt of an earlier restore number is not this backend's present and is not taken,
    /// and a later one means the data was put back. A music login's number above what is
    /// known is remembered, to be asked for once the answer's own part is done. Says
    /// whether the answer may be acted on.
    fn absorb(&mut self, seen: Seen, out: &mut Vec<Do>) -> bool {
        let Some(live) = self.live.as_mut() else { return false };
        let Some(saved) = live.saved.as_mut() else { return false };
        if seen.restore < saved.restore {
            return false;
        }
        if seen.restore > saved.restore {
            self.start_again(out);
            return false;
        }
        let (life, change) = (saved.life.max(seen.life), saved.change.max(seen.change));
        let moved = life > saved.life;
        if (saved.life, saved.change) != (life, change) {
            saved.life = life;
            saved.change = change;
            out.push(Do::Keep(Keep::Saved(saved.copy())));
        }
        // The account has a new life (12.5): the journal is told only now that the new life
        // is kept, so that nothing is removed before what replaces it is in place.
        if moved && let Some(backend_id) = BackendId::from_hex(&saved.backend_id) {
            out.push(Do::Journal(JournalDo::LifeMoved { account: JournalAccount { backend_id, account: saved.account }, life }));
        }

        if seen.music_count > live.music_seen {
            live.renewal.announced = Some(live.renewal.announced.map_or(seen.music_count, |announced| announced.max(seen.music_count)));
        }
        // Everything read, queued or held under the earlier life is retired at once: nothing
        // of a deleted life is posted once the machine knows the life moved.
        if moved {
            self.retire_life(life);
        }
        true
    }

    /// The backend said, to this session, that this session has ended (8.7): the one
    /// thing that signs a device out (6.3's table). What signing in left goes, the music
    /// login with it; downloads and settings stay.
    fn forced_out(&mut self, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        live.saved = None;
        out.push(Do::Forget(Forget::Saved));
        self.ended(SignInState::ForcedOut);
    }

    /// "The server's data was put back, sign in again" (12.3, 12.4): the session goes, and
    /// how far it had fetched; the music login stays, and plays. A login set aside stays
    /// set aside, and the person is told signing in renews it.
    fn start_again(&mut self, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let refused = live.renewal.refused;
        // 13.3: every event still held for this backend is sent again, each under its own
        // account once that account is signed in. Told before the session is dropped, so
        // that a death between the two leaves the next answer to say it again.
        if let Some(backend_id) = live.saved.as_ref().and_then(|saved| BackendId::from_hex(&saved.backend_id)) {
            out.push(Do::Journal(JournalDo::ResendAll { backend_id }));
        }
        if let Some(saved) = live.saved.as_mut() {
            saved.session = None;
            saved.sign_out = None;
            saved.notice = None;
            out.push(Do::Keep(Keep::Saved(saved.copy())));
        }
        self.ended(SignInState::StartAgain);
        if let Some(live) = self.live.as_mut()
            && refused.is_some()
        {
            live.renewal.refused = refused;
            live.music_words = Some(words::music(words::Music::SignInToRenew));
        }
    }

    pub(super) fn answered(&mut self, ask: u64, status: u16, body: &AnswerBody, given: &Given, out: &mut Vec<Do>) {
        let bytes: &[u8] = &body.0;
        // Larger than the ask allowed is not an answer: it is refused before the ask is
        // touched, and the driver says what happened ([`How::Cut`]) or hands the right one.
        if self.max_body_for(ask).is_none_or(|max_body| bytes.len() > max_body) {
            return;
        }
        let Some((what, _)) = self.take_flight(ask) else { return };
        match what {
            Purpose::Hello(pending) => self.answered_hello(pending, status, bytes, given, out),
            Purpose::Verify { origin } => self.answered_verify(origin, status, bytes, given),
            Purpose::SignIn { origin, backend_id, secret } => {
                let outcome = outcome(account::read_reply::<SignInAnswer>(status, bytes));
                self.answered_sign_in(origin, backend_id, secret, outcome, given, out);
            }
            Purpose::Devices { page, opened } => {
                let outcome = outcome(account::read_reply::<DevicePage>(status, bytes));
                self.answered_devices(page, opened, outcome, given, out);
            }
            Purpose::Music { count, refused } => {
                let outcome = outcome(account::read_reply::<MusicLoginResult>(status, bytes));
                self.answered_music(count, refused, outcome, given, out);
            }
            Purpose::ForceOut { device, session } => {
                let outcome = outcome(account::read_reply::<ForcedOut>(status, bytes));
                self.answered_force_out(device, session, outcome, given, out);
            }
            Purpose::ChangePassword => {
                let outcome = outcome(account::read_reply::<PasswordChanged>(status, bytes));
                self.answered_change(outcome, given, out);
            }
            Purpose::Tell { backend, session, origin } => {
                let outcome = outcome(account::read_reply::<SignedOut>(status, bytes));
                self.answered_tell(backend, session, origin, outcome, out);
            }
            Purpose::Events { account, batch } => self.answered_events(account, batch, status, bytes, given, out),
            Purpose::Diagnostics => self.answered_diagnostics(status, bytes, given, out),
            Purpose::App { request, endpoint, .. } => {
                let result = self.answer_for_app(endpoint, status, bytes, given, out);
                out.push(Do::AppResult { request, result });
            }
        }
        self.settle(given, out);
    }

    /// A sign-out told: the device's, not the account's, and nothing in it touches the
    /// account now signed in. Its note goes only on a "done" stamped by the backend it was
    /// made for, or a definite "that session is gone"; busy, not dealt with, a failure or
    /// another backend's answer keeps it, to be told again.
    fn answered_tell(&mut self, backend: BackendId, session: u64, origin: String, outcome: Outcome<SignedOut>, out: &mut Vec<Do>) {
        if let Outcome::Done(_, seen) | Outcome::Refused(_, Some(seen)) = &outcome {
            self.saw(&origin, seen.backend);
        }
        let backend_id = backend.to_hex();
        let Some(live) = self.live.as_mut() else { return };
        let Some(note) = live.notes.iter().find(|note| note.key() == (backend_id.as_str(), session)) else { return };
        let put_back = matches!(outcome, Outcome::Refused(Why::DataPutBack, _));
        let settled = match outcome {
            Outcome::Done(_, seen) => seen.backend == backend,
            Outcome::Refused(Why::SessionEnded | Why::DataPutBack, _) => true,
            _ => false,
        };
        // 13.3: the note's answer may be the only one to say the data was put back. Every
        // event held for that backend is to be sent again, each under its own account; the
        // account signed in now, and its session, are not touched.
        if put_back {
            out.push(Do::Journal(JournalDo::ResendAll { backend_id: backend }));
        }
        if settled {
            out.push(Do::Forget(note.forget()));
            retire(live, &backend_id, session);
        }
    }

    /// A `hello` read before anything secret goes to an origin (7.1). The expected backend:
    /// what was waiting is asked. Another: nothing carrying the session or the device secret
    /// goes there, nothing is forgotten, the music login plays on, and the person is asked to
    /// sign in again. Nothing that is a backend: as if unreachable, and asked again when it
    /// may be.
    fn answered_verify(&mut self, origin: String, status: u16, body: &[u8], given: &Given) {
        let active = self.live.as_ref().and_then(|live| live.saved.as_ref()).is_some_and(|saved| saved.nexus == origin);
        let Found::Backend(hello) = proto::classify(status, body) else {
            if active {
                self.unreached(given);
            }
            return;
        };
        self.saw(&origin, hello.id);
        self.heard_default(&origin, hello.diagnostics_default());
        self.fence_diagnostics();
        let Some(live) = self.live.as_mut() else { return };
        let ours = live.saved.as_ref().filter(|saved| saved.nexus == origin).map(|saved| BackendId::from_hex(&saved.backend_id) == Some(hello.id));
        if ours == Some(false) && live.signed_in() {
            self.became_stranger();
            return;
        }
        // Reached: what was waiting for it is asked now ([`Client::settle`]). Only the
        // active backend's reachability is what the row shows.
        if active && let Some(live) = self.live.as_mut() {
            live.reach = Reach::Up;
            live.backend_blocked = false;
        }
        if let Some(live) = self.live.as_mut() {
            live.restored = true;
        }
    }

    /// Another backend answers at the active address: nothing more goes to it, nothing is
    /// forgotten, the music plays on, and the person is asked to sign in again.
    fn became_stranger(&mut self) {
        if let Some(live) = self.live.as_mut() {
            live.stranger = true;
            live.flight.retain(|flight| flight.what.parting());
            live.sending.reset();
            live.diagnostics.drop_read();
            if live.renewal.refused.is_some() {
                live.music_words = Some(words::music(words::Music::SignInToRenew));
            }
        }
        self.set_form(SignInState::StartAgain, 0);
    }

    fn answered_hello(&mut self, pending: Pending, status: u16, body: &[u8], given: &Given, out: &mut Vec<Do>) {
        let hello = match proto::classify(status, body) {
            Found::Backend(hello) => hello,
            Found::NoBackend(_) => return self.set_form(SignInState::NoNexusServer, 0),
        };
        let at = pending.origin.origin().to_string();
        self.saw(&at, hello.id);
        self.heard_default(&at, hello.diagnostics_default());
        self.fence_diagnostics();
        let ours: Speaks = SPEAKS;
        if hello.speaks.low() > ours.high() {
            return self.set_form(SignInState::AppTooOld, 0);
        }
        if hello.speaks.high() < ours.low() || !hello.does(proto::HAS_ACCOUNTS) {
            return self.set_form(SignInState::ServerTooOld, 0);
        }
        let Pending { origin, credential } = pending;
        if let Some(live) = self.live.as_mut()
            && live.too_old.as_deref() == Some(at.as_str())
        {
            live.too_old = None;
        }
        match self.ask_sign_in(origin, hello.id, credential, given, out) {
            Some(true) => {}
            Some(false) => self.set_form(SignInState::CantReach, 0),
            None => self.set_form(SignInState::NoRandom, 0),
        }
    }

    fn answered_sign_in(&mut self, origin: NexusOrigin, backend_id: BackendId, secret: DeviceSecret, outcome: Outcome<SignInAnswer>, given: &Given, out: &mut Vec<Do>) {
        if let Outcome::Done(_, seen) | Outcome::Refused(_, Some(seen)) = &outcome {
            self.saw(&origin.origin().to_string(), seen.backend);
        }
        let (signed, seen) = match outcome {
            Outcome::Done(signed, seen) if seen.backend == backend_id && seen.account == signed.person.id => (signed, seen),
            // A receipt from another backend than the one whose hello was read: not ours.
            Outcome::Done(..) => return self.set_form(SignInState::NoNexusServer, 0),
            Outcome::Refused(why, _) => {
                let (state, wait) = match why {
                    Why::WrongLogin | Why::Denied => (SignInState::WrongLogin, 0),
                    Why::AccountOff => (SignInState::AccountOff, 0),
                    // 8.3: a right password is never refused; it may wait. The exact seconds
                    // are shown, every time; the form counts down and the person presses.
                    Why::Busy { seconds } => (SignInState::Busy, seconds.unwrap_or(0)),
                    Why::Incompatible { low, .. } if low > SPEAKS.high() => (SignInState::AppTooOld, 0),
                    Why::Incompatible { .. } => (SignInState::ServerTooOld, 0),
                    Why::PasswordNotAllowed => (SignInState::PasswordNotAllowed, 0),
                    _ => (SignInState::CantReach, 0),
                };
                return self.set_form(state, wait);
            }
            Outcome::Nothing => return self.set_form(SignInState::CantReach, 0),
        };
        // A new account is published: the fence moves, and nothing asked before is acted on.
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let Some(live) = self.live.as_mut() else { return };
        live.flight.retain(|flight| flight.what.parting());
        live.renewal = Renewal::default();
        live.devices = Devices::not_asked();
        live.password = PasswordShown::idle();
        live.reach = Reach::Up;
        live.stranger = false;
        live.backend_blocked = false;
        live.check_due = false;
        live.account_generation = generation;
        live.sending.reset();
        live.diagnostics.drop_read();
        // "Too old" was another backend's, if it was said elsewhere.
        if live.too_old.as_deref().is_some_and(|said| *said != origin.origin().to_string()) {
            live.too_old = None;
        }
        // The device secret, for this backend from now on (a new one, if this was made as a
        // new device, kept only now that it succeeded).
        let key = DeviceKey { secret, backend_id: Some(backend_id.to_hex()) };
        let same = live.device_secret.as_ref().is_some_and(|kept| kept.secret == key.secret && kept.backend_id == key.backend_id);
        if !same {
            out.push(Do::Keep(Keep::DeviceSecret(key.clone())));
            live.device_secret = Some(key);
        }
        let held_before = live.held_count().is_some();
        let (music, none) = match signed.music {
            MusicLoginResult::Login(login) => (Some(login), None),
            MusicLoginResult::NoneSetYet => (None, Some(words::Music::NoneSet)),
            MusicLoginResult::UnusableNow => (None, Some(words::Music::UnusableNow)),
        };
        live.music_words = none.map(words::music);
        live.music_seen = music.as_ref().map_or(seen.music_count, |login| login.count);
        let saved = Saved {
            nexus: origin.origin().to_string(),
            backend_id: backend_id.to_hex(),
            account: signed.person.id,
            email: signed.person.email.as_str().to_owned(),
            name: signed.person.name.as_str().to_owned(),
            device: signed.device.get(),
            restore: signed.restore,
            life: seen.life,
            change: seen.change,
            notice: Some(signed.notice.as_str().to_owned()),
            session: Some(signed.session),
            sign_out: Some(signed.sign_out),
            music,
        };
        live.form.email = saved.email.clone();
        // One thing, whole, in one step; then the player. With no login now, the one the
        // player held (another account's, kept through "start again") is taken from it.
        // Either way the music server's permission decision was about the old login.
        out.push(Do::Keep(Keep::Saved(saved.copy())));
        // The journal of this account writes under the life the backend has now, and what it
        // wrote under an earlier one is removed unsent (12.5), only now that it is kept.
        out.push(Do::Journal(JournalDo::LifeMoved { account: JournalAccount { backend_id, account: saved.account }, life: saved.life }));
        live.music_blocked = false;
        match &saved.music {
            Some(login) => out.push(Do::Hand(Hand::Login { login: copy(login), generation })),
            None if held_before => out.push(Do::Hand(Hand::NoLogin { generation })),
            None => {}
        }
        live.saved = Some(saved);
        live.diagnostics.signed_in(given.now_ms, out);
        self.set_form(SignInState::Idle, 0);
    }

    fn answered_devices(&mut self, page: usize, opened: bool, outcome: Outcome<DevicePage>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let Some(live) = self.live.as_mut() else { return };
        match outcome {
            Outcome::Done(list, _) => {
                if page == 0 {
                    live.devices.reading.clear();
                }
                for device in list.devices() {
                    let this_one = device.this_device;
                    let session = device.session.map(SessionId::get);
                    live.devices.reading.push(DeviceRow {
                        id: device.id.get(),
                        name: device.name.as_str().to_owned(),
                        kind: device.kind,
                        this_one,
                        session,
                        seen: words::seen(this_one, session.is_some(), device.last_seen_ms, given.now_ms),
                        can_force_out: session.is_some() && !this_one,
                        working: false,
                    });
                }
                let next = page.checked_add(1).filter(|next| *next < DEVICE_PAGES_MAX);
                if let (Some(after), Some(next)) = (list.next(), next)
                    && self.ask_devices(next, Some(after), opened, out)
                {
                    return;
                }
                let Some(live) = self.live.as_mut() else { return };
                live.devices.rows = std::mem::take(&mut live.devices.reading);
                live.devices.state = ListState::Listed;
                live.devices.words.clear();
            }
            Outcome::Refused(..) => {
                if opened {
                    live.devices.state = ListState::Failed;
                    live.devices.words = words::devices(words::Devices::Failed);
                }
            }
            Outcome::Nothing => {
                if opened {
                    live.devices.state = ListState::Failed;
                    live.devices.words = words::devices(words::Devices::CantReach);
                }
            }
        }
    }

    /// A renewal's answer: its outcome is put in place first; what is still due is decided
    /// after ([`Client::settle`]).
    fn answered_music(&mut self, count: u64, refused: bool, outcome: Outcome<MusicLoginResult>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let generation = self.live.as_ref().map_or(0, |live| live.account_generation);
        let Some(live) = self.live.as_mut() else { return };
        let answered = |renewal: &mut Renewal| {
            if refused {
                renewal.refreshed = Some(count);
            } else {
                renewal.rose = Some(count);
            }
            // A renewal answered while a number is set aside counts as that number's one.
            if renewal.refused.is_some() {
                renewal.refreshed = renewal.refused;
            }
        };
        match outcome {
            Outcome::Done(MusicLoginResult::Login(login), _) => {
                answered(&mut live.renewal);
                let held = live.held_count();
                if held.is_none_or(|held| login.count > held) {
                    // Newer: put in place in one step, and handed over at once, so that the
                    // car goes back to the music server and what waited is tried once more.
                    live.renewal.refused = None;
                    live.music_words = None;
                    live.music_seen = login.count;
                    if let Some(saved) = live.saved.as_mut() {
                        saved.music = Some(copy(&login));
                        out.push(Do::Keep(Keep::Saved(saved.copy())));
                    }
                    out.push(Do::Hand(Hand::Login { login, generation }));
                } else if live.renewal.refused.is_some() {
                    // Nothing newer than the refused one: it stays set aside, and is renewed
                    // again only when the network comes back or the number moves.
                    live.music_words = Some(words::music(words::Music::NoNewer));
                }
            }
            Outcome::Done(none, seen) => {
                answered(&mut live.renewal);
                // 9.4: none to hand: the person stays signed in, and is told which.
                live.music_seen = live.music_seen.max(seen.music_count);
                let which = if matches!(none, MusicLoginResult::UnusableNow) { words::Music::UnusableNow } else { words::Music::NoneSet };
                live.music_words = Some(words::music(which));
            }
            Outcome::Refused(..) | Outcome::Nothing => {
                // Unanswered, a stamped "busy" included: owed, and asked again only when the
                // backend may be reachable, never from this answer.
                live.renewal.owed = Some((count, refused));
                live.renewal.owed_now = true;
                if live.renewal.refused.is_some() {
                    live.music_words = Some(words::music(words::Music::WhenReachable));
                }
            }
        }
    }

    fn answered_force_out(&mut self, device: u64, session: u64, outcome: Outcome<ForcedOut>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let Some(live) = self.live.as_mut() else { return };
        // Only a row still naming the session that was forced out: one listed since with a
        // newer session is another sign-in, and is left as it is.
        let gone = |live: &mut Live, words: String| {
            for row in live.devices.rows.iter_mut().filter(|row| row.id == device && row.session == Some(session)) {
                row.working = false;
                row.can_force_out = false;
                row.session = None;
                row.seen = super::words::seen(false, false, 0, 0);
            }
            live.devices.words = words;
        };
        match outcome {
            Outcome::Done(ForcedOut { music_cut_off }, _) => gone(live, words::devices(words::Devices::ForcedOut { music_cut_off })),
            Outcome::Refused(Why::NoSuchDevice, _) => gone(live, words::devices(words::Devices::Gone)),
            Outcome::Refused(..) | Outcome::Nothing => {
                for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                    row.working = false;
                }
                live.devices.words = words::devices(words::Devices::CantReach);
            }
        }
    }

    fn answered_change(&mut self, outcome: Outcome<PasswordChanged>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        match outcome {
            Outcome::Done(PasswordChanged {}, _) => self.set_password(PasswordState::Changed, 0),
            Outcome::Refused(Why::WrongLogin | Why::Denied, _) => self.set_password(PasswordState::WrongCurrent, 0),
            Outcome::Refused(Why::PasswordNotAllowed, _) => self.set_password(PasswordState::NotAllowed, 0),
            Outcome::Refused(Why::Busy { seconds }, _) => self.set_password(PasswordState::Busy, seconds.unwrap_or(0)),
            Outcome::Refused(..) | Outcome::Nothing => self.set_password(PasswordState::CantReach, 0),
        }
    }

    // -----------------------------------------------------------------------------------
    // Failures, the network, Offline, ticks, waits, the music server
    // -----------------------------------------------------------------------------------

    pub(super) fn failed(&mut self, ask: u64, failure: Failure, given: &Given, out: &mut Vec<Do>) {
        let Some((what, _)) = self.take_flight(ask) else { return };
        if let Purpose::App { request, .. } = what {
            out.push(Do::AppResult { request, result: AppResult::Unreachable });
        }
        self.failed_purpose(what, failure, given);
    }

    fn failed_purpose(&mut self, what: Purpose, failure: Failure, given: &Given) {
        // 6.4: no connection was ever opened, the address leads inside, and the permission
        // is not held. A certificate that is wrong is never this. Whatever asked, it is the
        // one permission decision, shown on whatever screen is up.
        let blocked = !failure.how.connected() && Look { inside: failure.inside, permission: failure.permission }.blocked();
        // Only for the active backend's address, or the form's: a note's telling elsewhere
        // says nothing of the one this device uses.
        let active_origin = self.live.as_ref().and_then(|live| live.saved.as_ref()).map(|saved| saved.nexus.clone());
        let here = match &what {
            Purpose::Tell { .. } => false,
            Purpose::Verify { origin } => active_origin.as_deref() == Some(origin.as_str()),
            _ => true,
        };
        if blocked && here && let Some(live) = self.live.as_mut() {
            live.backend_blocked = live.app.kind == DeviceKind::Phone;
        }
        let form = if blocked {
            SignInState::NeedsPermission
        } else if failure.how == How::Secure {
            SignInState::NotSecure
        } else {
            SignInState::CantReach
        };
        match what {
            Purpose::Hello(_) | Purpose::SignIn { .. } => self.set_form(form, 0),
            Purpose::Verify { .. } => {
                if here {
                    self.unreached(given);
                }
            }
            Purpose::Devices { opened, .. } => {
                self.unreached(given);
                if let Some(live) = self.live.as_mut() {
                    live.check_due = true;
                    if opened {
                        live.devices.state = ListState::Failed;
                        live.devices.words = words::devices(words::Devices::CantReach);
                    }
                }
            }
            Purpose::Music { count, refused } => {
                self.unreached(given);
                if let Some(live) = self.live.as_mut() {
                    live.renewal.owed = Some((count, refused));
                    if live.renewal.refused.is_some() {
                        live.music_words = Some(words::music(words::Music::WhenReachable));
                    }
                }
            }
            Purpose::ForceOut { device, .. } => {
                self.unreached(given);
                if let Some(live) = self.live.as_mut() {
                    for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                        row.working = false;
                    }
                    live.devices.words = words::devices(words::Devices::CantReach);
                }
            }
            Purpose::ChangePassword => {
                self.unreached(given);
                self.set_password(PasswordState::CantReach, 0);
            }
            // The note stays kept, and is told later.
            Purpose::Tell { .. } => {}
            Purpose::Events { account, batch } => self.failed_events((account, batch.life()), given),
            // That attempt is spent; the next is the schedule's.
            Purpose::Diagnostics | Purpose::App { .. } => self.unreached(given),
        }
    }

    pub(super) fn offline(&mut self, on: bool, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if live.offline == on {
            return;
        }
        live.offline = on;
        if !on {
            return self.after_reach(given, out, Reached::OfflineOff);
        }
        // Nothing is asked while Offline is on, and nothing that was asked is waited for or
        // sent ([`Client::waits_for`]), but a sign-out being told.
        let abandoned: Vec<Flight> = std::mem::take(&mut live.flight);
        let mut form = false;
        for flight in abandoned {
            match flight.what {
                Purpose::Hello(_) | Purpose::SignIn { .. } => form = true,
                Purpose::Devices { opened, .. } => {
                    live.check_due = true;
                    if opened {
                        live.devices.state = ListState::Failed;
                        live.devices.words = words::devices(words::Devices::Offline);
                    }
                }
                Purpose::Music { count, refused } => live.renewal.owed = Some((count, refused)),
                Purpose::ForceOut { device, .. } => {
                    for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                        row.working = false;
                    }
                    live.devices.words = words::devices(words::Devices::Offline);
                }
                Purpose::ChangePassword => {
                    live.password = PasswordShown { state: PasswordState::CantReach, words: words::password(PasswordState::CantReach, 0, true) };
                }
                Purpose::Tell { .. } => live.flight.push(flight),
                Purpose::App { request, .. } => out.push(Do::AppResult { request, result: AppResult::Stopped }),
                Purpose::Verify { .. } | Purpose::Events { .. } | Purpose::Diagnostics => {}
            }
        }
        live.sending.reset();
        live.diagnostics.drop_read();
        if live.renewal.refused.is_some() && live.renewal.owed.is_some() {
            live.music_words = Some(words::music(words::Music::WhenOnline));
        }
        if form {
            self.set_form(SignInState::Offline, 0);
        }
    }

    /// A tick asks nothing of the session. It tells waiting sign-outs at their pace (11).
    pub(super) fn tick(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        if live.offline {
            return;
        }
        let due = !live.notes.is_empty() && live.told_ms.is_none_or(|last| given.now_ms.checked_sub(last).is_none_or(|age| age >= TELL_EVERY_MS));
        if due {
            self.ask_tell(given, out);
        }
        // Diagnostics alone, at the pace 14.4 names. Events are never sent from a tick.
        self.diagnostics_due(given, out);
    }

    /// 6.3, as ruled: the first report for a number sets it aside, at once; one renewal for
    /// that number, in the background; never a sign-out. A report about a login no longer
    /// held, or handed under another generation, does nothing.
    pub(super) fn music_refused(&mut self, count: u64, generation: u64, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if generation != live.account_generation || live.held_count() != Some(count) || live.renewal.refused == Some(count) {
            return;
        }
        live.renewal.refused = Some(count);
        out.push(Do::Hand(Hand::Refused { count, generation }));
        if live.renewal.refreshed == Some(count) {
            live.music_words = Some(words::music(words::Music::NoNewer));
            return;
        }
        if !live.signed_in() || live.stranger {
            live.music_words = Some(words::music(words::Music::SignInToRenew));
            return;
        }
        if live.offline {
            live.renewal.owed = Some((count, true));
            live.music_words = Some(words::music(words::Music::WhenOnline));
            return;
        }
        live.music_words = Some(words::music(words::Music::Renewing));
        if live.waiting(|what| matches!(what, Purpose::Music { .. })) {
            // A renewal is under way already (the number moved): its answer serves.
            return;
        }
        self.ask_music(count, true, out);
        if let Some(live) = self.live.as_mut()
            && live.renewal.owed.is_some()
            && !live.waiting(|what| matches!(what, Purpose::Verify { .. }))
        {
            live.music_words = Some(words::music(words::Music::WhenReachable));
        }
    }

    // -----------------------------------------------------------------------------------
    // Showing
    // -----------------------------------------------------------------------------------

    pub(super) fn show(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        let signed_in = live.signed_in() && !live.stranger;
        let screen = if signed_in { Screen::SignedIn } else { Screen::SignIn };
        let signed = live.saved.as_ref().filter(|_| signed_in);
        let row = match (live.saved.as_ref().filter(|_| live.signed_in()), live.reach) {
            (None, _) => words::row(Row::SignedOut),
            _ if live.stranger => words::row(Row::Stranger),
            (Some(saved), _) if live.too_old.as_deref() == Some(saved.nexus.as_str()) => words::row(Row::TooOld),
            _ if live.offline => words::row(Row::Offline),
            _ if live.backend_blocked => words::row(Row::NeedsPermission),
            (Some(_), Reach::Up) => words::row(Row::Connected),
            (Some(saved), Reach::Unknown) => words::row(Row::SignedIn(&saved.email)),
            (Some(_), Reach::Down { since_ms }) => words::row(Row::Down { since: words::time_of_day(since_ms, given.local_offset_minutes) }),
        };
        let shown = Shown {
            screen,
            sign_in: SignInShown {
                state: live.form.state,
                words: live.form.words.clone(),
                address: live.form.address.clone(),
                email: live.form.email.clone(),
                ask_permission: live.form.ask_permission,
                wait_s: live.form.wait_s,
            },
            row,
            who: signed.map(|saved| format!("{} ({})", saved.name, saved.email)),
            music: if live.music_blocked && live.music_words.is_none() { Some(words::music(words::Music::NeedsPermission)) } else { live.music_words.clone() },
            music_ask_permission: live.music_blocked,
            ask_permission: live.backend_blocked,
            car: live.renewal.refused.map(|_| words::car()),
            notice: signed.and_then(|saved| saved.notice.clone()),
            devices: DevicesShown { state: live.devices.state, rows: live.devices.rows.clone(), words: live.devices.words.clone() },
            password: live.password.clone(),
            plays: live.held_count().is_some(),
        };
        if self.shown.as_ref() != Some(&shown) {
            out.push(Do::Show(shown.clone()));
            self.shown = Some(shown);
        }
    }
}
