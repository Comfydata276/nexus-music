//! The machine's moves: what each thing that happened does.

use super::words::{self, Row};
use super::*;

impl Live {
    /// The session, when one is held.
    fn session(&self) -> Option<&Session> {
        self.saved.as_ref()?.session.as_ref()
    }

    fn signed_in(&self) -> bool {
        self.session().is_some()
    }

    fn held_count(&self) -> Option<u64> {
        self.saved.as_ref()?.music.as_ref().map(|login| login.count)
    }

    fn waiting(&self, what: impl Fn(&Purpose) -> bool) -> bool {
        self.flight.iter().any(|flight| what(&flight.what))
    }
}

/// The JSON body of a note's telling.
// to be account's (see SIGN_OUT_NOTE).
fn note_body(note: &Note) -> Option<Hidden> {
    let head = RequestHead { speaks: SPEAKS, restore: note.restore };
    let text = serde_json::to_string(&serde_json::json!({ "head": head, "request_id": note.request, "note": note.session_sha256 })).ok()?;
    Some(Hidden(Zeroizing::new(text)))
}

/// A number for one change, made when the person pressed (12.2), from the random bytes
/// handed in: kept below 2^53 so that it is the same number in a Kotlin `Long` and in JSON
/// read by anything. `None` with no random bytes, or (once in 2^53) all of them zero.
fn request_id(given: &Given) -> Option<account::RequestId> {
    let random = given.random.as_ref()?;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(random.0.get(..8)?);
    account::RequestId::new(u64::from_le_bytes(bytes) & ((1 << 53) - 1))
}

/// A body account encoded, as the text an ask carries.
fn hidden(body: Result<account::WireBody, account::BadMessage>) -> Option<Hidden> {
    let body = body.ok()?;
    let text = std::str::from_utf8(body.bytes()).ok()?;
    Some(Hidden(Zeroizing::new(text.to_owned())))
}

impl Client {
    // -----------------------------------------------------------------------------------
    // Asking
    // -----------------------------------------------------------------------------------

    /// Asks, if asking is allowed now: never while Offline is on, never when the backend
    /// speaks no number this app does or another backend answers at the origin, and never
    /// two of one kind at once. Says whether it asked.
    fn ask(&mut self, what: Purpose, path: &str, credential: Option<&Session>, body: Option<Hidden>, out: &mut Vec<Do>) -> bool {
        let Some(live) = self.live.as_mut() else { return false };
        if live.offline || live.too_old || live.flight.len() >= FLIGHT_MAX || live.waiting(|other| other.same_kind(&what)) {
            return false;
        }
        let origin = match &what {
            Purpose::Hello(pending) => pending.origin.as_str(),
            Purpose::SignIn { origin, .. } | Purpose::SetPassword { origin, .. } => origin.as_str(),
            Purpose::TellNote => match &live.note {
                Some(note) => note.nexus.as_str(),
                None => return false,
            },
            Purpose::Tell => match &live.note {
                Some(note) => note.nexus.as_str(),
                None => return false,
            },
            _ if live.stranger => return false,
            _ => match &live.saved {
                Some(saved) => saved.nexus.as_str(),
                None => return false,
            },
        };
        let (method, within_ms, max_body) = match &what {
            Purpose::Hello(_) => (Method::Get, HELLO_WITHIN_MS, HELLO_MAX_BYTES),
            Purpose::SignIn { .. } => (Method::Post, SIGN_IN_WITHIN_MS, ANSWER_MAX_BYTES),
            _ => (Method::Post, CALL_WITHIN_MS, ANSWER_MAX_BYTES),
        };
        if (method == Method::Post) != body.is_some() {
            return false;
        }
        self.next_ask = self.next_ask.wrapping_add(1);
        let id = self.next_ask;
        let url = format!("{origin}{path}");
        let authorization = credential.map(|session| Hidden(session.authorization()));
        live.flight.push(Flight { id, what });
        out.push(Do::Ask(Ask { id, generation: self.generation, method, url, authorization, body, within_ms, max_body }));
        true
    }

    fn head(&self) -> Option<RequestHead> {
        let saved = self.live.as_ref()?.saved.as_ref()?;
        Some(RequestHead { speaks: SPEAKS, restore: saved.restore })
    }

    fn session(&self) -> Option<Session> {
        self.live.as_ref()?.session().cloned()
    }

    fn ask_devices(&mut self, page: usize, after: Option<DeviceId>, opened: bool, given: &Given, out: &mut Vec<Do>) -> bool {
        let (Some(session), Some(head)) = (self.session(), self.head()) else { return false };
        let Some(body) = hidden(account::encode(&ListDevices { head, after })) else { return false };
        let asked = self.ask(Purpose::Devices { page, opened }, account::LIST_DEVICES, Some(&session), Some(body), out);
        if asked && let Some(live) = self.live.as_mut() {
            live.checked_ms = Some(given.now_ms);
        }
        asked
    }

    /// One renewal of the music login for this number, if the rule allows one now.
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

    fn ask_note(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(note) = self.live.as_ref().and_then(|live| live.note.clone()) else { return };
        if given.now_ms.checked_sub(note.made_ms).is_some_and(|age| age >= NOTE_LIFE_MS) {
            // The session it would end is gone by itself by now (8.7).
            if let Some(live) = self.live.as_mut() {
                live.note = None;
            }
            out.push(Do::Forget(Forget::Note));
            return;
        }
        let Some(body) = note_body(&note) else { return };
        if self.ask(Purpose::TellNote, SIGN_OUT_NOTE, None, Some(body), out)
            && let Some(live) = self.live.as_mut()
        {
            live.told_ms = Some(given.now_ms);
        }
    }

    /// Something says the backend may be reachable now: ask what is owed, and, but for an
    /// answer that just came, whether the session stands.
    pub(super) fn after_reach(&mut self, given: &Given, out: &mut Vec<Do>, why: Reached) {
        let Some(live) = self.live.as_mut() else { return };
        if live.offline || live.too_old {
            return;
        }
        // 6.3, step 5: the network coming back renews a login the backend had nothing
        // newer for, once more.
        if why == Reached::Network && live.renewal.refused.is_some() && live.renewal.refreshed == live.renewal.refused {
            live.renewal.refreshed = None;
            live.renewal.owed = live.renewal.refused.map(|count| (count, true));
        }
        let owed = live.renewal.owed;
        let missing = live.signed_in() && live.held_count().is_none();
        if live.note.is_some() {
            self.ask_note(given, out);
        }
        if let Some((count, refused)) = owed {
            self.ask_music(count, refused, out);
        } else if missing {
            // Signed in with no music login: ask for one (its number is not known: any).
            self.ask_music(0, false, out);
        }
        if why != Reached::Answer {
            self.ask_devices(0, None, false, given, out);
        }
    }

    // -----------------------------------------------------------------------------------
    // Presses
    // -----------------------------------------------------------------------------------

    pub(super) fn pressed(&mut self, press: Press, given: &Given, out: &mut Vec<Do>) {
        match press {
            Press::SignIn { address, email, password } => self.press_sign_in(address, Some(email), password, None, given, out),
            Press::SignInWithCode { address, code, password, again } => {
                if password.expose() != again.expose() {
                    return self.set_form(SignInState::PasswordsDiffer, 0);
                }
                self.press_sign_in(address, None, code, Some(password), given, out)
            }
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

    fn set_form(&mut self, state: SignInState, wait_ms: u64) {
        if let Some(live) = self.live.as_mut() {
            live.form.words = words::sign_in(state, live.app.kind, wait_ms);
            live.form.state = state;
            live.form.ask_permission = state == SignInState::NeedsPermission && live.app.kind == DeviceKind::Phone;
        }
    }

    fn press_sign_in(&mut self, address: String, email: Option<String>, secret: Typed, new: Option<Typed>, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        // Signed in already, or a sign-in under way: a second press asks nothing (one ask is
        // one request; two quick presses are one try).
        if live.signed_in() || live.waiting(|what| matches!(what, Purpose::Hello(_) | Purpose::SignIn { .. } | Purpose::SetPassword { .. })) {
            return;
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
        let credential = match email {
            Some(email) => {
                let Some(email) = Email::new(&email) else { return self.set_form(SignInState::BadEmail, 0) };
                if secret.expose().is_empty() {
                    return self.set_form(SignInState::NoPassword, 0);
                }
                // A password outside account's rule is nobody's: refused here, unsent.
                let Some(password) = Password::new(secret.expose()) else { return self.set_form(SignInState::WrongLogin, 0) };
                Pressed::Password { email, password }
            }
            None => {
                if secret.expose().trim().is_empty() || new.as_ref().is_some_and(|new| new.expose().is_empty()) {
                    return self.set_form(SignInState::NoPassword, 0);
                }
                // The password chosen is held to the rule before the code is spent on it. The
                // email is not known yet: "not the email" is the backend's to check here.
                if new.as_ref().is_some_and(|new| Password::new(new.expose()).is_none()) {
                    return self.set_form(SignInState::PasswordNotAllowed, 0);
                }
                match tidy_code(secret.expose()) {
                    Some(code) => Pressed::Code(code),
                    None => return self.set_form(SignInState::WrongLogin, 0),
                }
            }
        };
        if live.device_secret.is_none() {
            match &given.random {
                Some(random) => {
                    let secret = DeviceSecret::from_bytes(*random.0);
                    out.push(Do::Keep(Keep::DeviceSecret(secret.clone())));
                    live.device_secret = Some(secret);
                }
                None => return self.set_form(SignInState::NoRandom, 0),
            }
        }
        if self.ask(Purpose::Hello(Pending { origin, credential, new }), proto::HELLO, None, None, out) {
            self.set_form(SignInState::Working, 0);
        } else {
            self.set_form(SignInState::CantReach, 0);
        }
    }

    /// Asks to sign in, once `hello` has said this is a backend that has accounts.
    fn ask_sign_in(&mut self, origin: String, backend_id: BackendId, credential: Pressed, then: Option<Then>, out: &mut Vec<Do>) -> bool {
        let Some(live) = self.live.as_ref() else { return false };
        let Some(device_secret) = live.device_secret.clone() else { return false };
        // 12.3: a sign-in names the restore number the device has, if it has one from this
        // backend.
        let restore = live.saved.as_ref().filter(|saved| saved.nexus == origin && BackendId::from_hex(&saved.backend_id) == Some(backend_id)).map(|saved| saved.restore);
        let credential = match credential {
            Pressed::Password { email, password } => Credential::Password { email, password },
            Pressed::Code(code) => Credential::Code(code),
        };
        let message = SignIn {
            credential,
            device_secret,
            device_name: live.app.device_name.clone(),
            device_kind: live.app.kind,
            app_version: live.app.version,
            speaks: SPEAKS,
            restore,
        };
        let Some(body) = hidden(account::encode(&message)) else { return false };
        self.ask(Purpose::SignIn { origin, backend_id, then }, account::SIGN_IN, None, Some(body), out)
    }

    /// Signing out here: lasting on the device at once, whatever the network. The backend
    /// is told now under the session if it can be, and otherwise later from the note,
    /// which can end that one session and nothing else (11, as ruled).
    fn sign_out(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let Some(saved) = live.saved.take() else { return };
        let Some(session) = saved.session.clone() else {
            // Nothing signed in (the server's data was put back): forget what is left.
            out.push(Do::Forget(Forget::Saved));
            self.ended(SignInState::SignedOut);
            return;
        };
        // One number for this sign-out, the same for every telling of it (12.2). With no
        // random bytes it is 0, and the note is told only by its session's SHA-256.
        let request = request_id(given).map_or(0, account::RequestId::get);
        let note = Note {
            request,
            session_sha256: session_sha256(&session),
            nexus: saved.nexus.clone(),
            backend_id: saved.backend_id.clone(),
            restore: saved.restore,
            made_ms: given.now_ms,
        };
        out.push(Do::Forget(Forget::Saved));
        out.push(Do::Keep(Keep::Note(note.clone())));
        live.note = Some(note);
        // An older note's telling is no longer waited for: this one replaces it.
        live.flight.retain(|flight| !matches!(flight.what, Purpose::Tell | Purpose::TellNote));
        self.ended(SignInState::SignedOut);
        let head = RequestHead { speaks: SPEAKS, restore: saved.restore };
        if let Some(request_id) = account::RequestId::new(request)
            && let Some(body) = hidden(account::encode(&SignOut { head, request_id }))
        {
            self.ask(Purpose::Tell, account::SIGN_OUT, Some(&session), Some(body), out);
        }
        drop(session);
    }

    /// The account signed in is gone (signed out, forced out, the data put back): fence
    /// what was asked for it, and go back to the sign-in form. Telling the backend of a
    /// sign-out is the device's, not the account's, and is kept.
    fn ended(&mut self, state: SignInState) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(live) = self.live.as_mut() {
            live.flight.retain(|flight| matches!(flight.what, Purpose::Tell | Purpose::TellNote));
            live.renewal = Renewal::default();
            live.devices = Devices::not_asked();
            live.password = PasswordShown::idle();
            live.music_words = None;
            live.reach = Reach::Unknown;
            live.checked_ms = None;
            live.stranger = false;
        }
        self.set_form(state, 0);
    }

    fn open_devices(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if !live.signed_in() {
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
        if self.ask_devices(0, None, true, given, out) {
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
        if !live.signed_in() {
            return;
        }
        if device == saved.device {
            // Forcing out this device is signing out here.
            return self.sign_out(given, out);
        }
        let known = live.devices.rows.iter().any(|row| row.id == device && row.can_force_out && !row.working);
        let offline = live.offline;
        let (Some(session), Some(head), Some(id)) = (self.session(), self.head(), DeviceId::new(device)) else { return };
        if !known {
            return;
        }
        if offline {
            if let Some(live) = self.live.as_mut() {
                live.devices.words = words::devices(words::Devices::Offline);
            }
            return;
        }
        // to be account's: 8.7 has forcing out name the session it ends, not only the
        // device. account's `ForceOut` names the device; the list does not carry sessions.
        let Some(body) = hidden(account::encode(&ForceOut { head, device: id })) else { return };
        if self.ask(Purpose::ForceOut { device }, account::FORCE_OUT, Some(&session), Some(body), out)
            && let Some(live) = self.live.as_mut()
        {
            live.devices.words.clear();
            for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                row.working = true;
            }
        }
    }

    fn set_password(&mut self, state: PasswordState, wait_ms: u64) {
        if let Some(live) = self.live.as_mut() {
            live.password = PasswordShown { state, words: words::password(state, wait_ms, live.offline) };
        }
    }

    fn press_change(&mut self, current: Typed, new: Typed, again: Typed, given: &Given, out: &mut Vec<Do>) {
        let (Some(session), Some(head)) = (self.session(), self.head()) else { return };
        let Some(live) = self.live.as_ref() else { return };
        let Some(saved) = &live.saved else { return };
        if live.waiting(|what| matches!(what, Purpose::ChangePassword)) {
            return;
        }
        if new.expose() != again.expose() {
            return self.set_password(PasswordState::Differ, 0);
        }
        let Some(email) = Email::new(&saved.email) else { return self.set_password(PasswordState::NotAllowed, 0) };
        let Some(new) = Password::new(new.expose()).filter(|new| new.allowed_for(&email)) else {
            return self.set_password(PasswordState::NotAllowed, 0);
        };
        // A present password outside the rule is nobody's: refused here, unsent.
        let Some(current) = Password::new(current.expose()) else { return self.set_password(PasswordState::WrongCurrent, 0) };
        if live.offline {
            return self.set_password(PasswordState::CantReach, 0);
        }
        let Some(request_id) = request_id(given) else { return self.set_password(PasswordState::CantReach, 0) };
        let Some(body) = hidden(account::encode(&ChangePassword { head, request_id, current_password: Some(current), new_password: new })) else { return };
        if self.ask(Purpose::ChangePassword, account::CHANGE_PASSWORD, Some(&session), Some(body), out) {
            self.set_password(PasswordState::Working, 0);
        } else {
            self.set_password(PasswordState::CantReach, 0);
        }
    }

    // -----------------------------------------------------------------------------------
    // Answers
    // -----------------------------------------------------------------------------------

    fn take_flight(&mut self, ask: u64) -> Option<Purpose> {
        let live = self.live.as_mut()?;
        let index = live.flight.iter().position(|flight| flight.id == ask)?;
        Some(live.flight.remove(index).what)
    }

    /// The backend answered as the backend: it can be reached. If it could not before, ask
    /// what was owed.
    fn reached(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let was_down = matches!(live.reach, Reach::Down { .. });
        live.reach = Reach::Up;
        if was_down {
            self.after_reach(given, out, Reached::Answer);
        }
    }

    fn unreached(&mut self, given: &Given) {
        if let Some(live) = self.live.as_mut()
            && !matches!(live.reach, Reach::Down { .. })
        {
            live.reach = Reach::Down { since_ms: given.now_ms };
        }
    }

    /// What every answer to a call under the session shares (7.1): the backend's id is
    /// checked; the backend was reached; its receipt is taken; "this session has ended"
    /// and "the server's data was put back" end the account here; "too old" stops asking.
    /// Gives back the outcome for the call's own part, or `None` if it was dealt with here.
    fn common<T>(&mut self, outcome: Outcome<T>, given: &Given, out: &mut Vec<Do>) -> Option<Outcome<T>> {
        let expected = self.live.as_ref().and_then(|live| live.saved.as_ref()).and_then(|saved| BackendId::from_hex(&saved.backend_id));
        let seen = match &outcome {
            Outcome::Done(_, seen) | Outcome::Refused(_, Some(seen)) => Some(*seen),
            _ => None,
        };
        if let Some(seen) = seen
            && Some(seen.backend) != expected
        {
            // Another backend answers at this origin: it is not ours. Nothing more is sent
            // to it, and nothing it said is acted on (7.1).
            if let Some(live) = self.live.as_mut() {
                live.stranger = true;
                live.flight.retain(|flight| matches!(flight.what, Purpose::Tell | Purpose::TellNote));
            }
            return None;
        }
        match &outcome {
            Outcome::Nothing => self.unreached(given),
            _ => self.reached(given, out),
        }
        if let Some(seen) = seen {
            self.absorb(seen, out);
        }
        match outcome {
            Outcome::Refused(Why::NotSignedIn, _) => {
                self.forced_out(out);
                None
            }
            Outcome::Refused(Why::StartAgain, _) => {
                self.start_again(out);
                None
            }
            Outcome::Refused(Why::Incompatible { .. }, _) => {
                if let Some(live) = self.live.as_mut() {
                    live.too_old = true;
                    live.flight.clear();
                }
                None
            }
            other => Some(other),
        }
    }

    /// The numbers an answer's receipt carries: kept, and the music login's number having
    /// moved means asking for the new one (9.4).
    fn absorb(&mut self, seen: Seen, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let held = live.held_count();
        let Some(saved) = live.saved.as_mut() else { return };
        if (saved.restore, saved.life, saved.change) != (seen.restore, seen.life, seen.change) {
            saved.restore = seen.restore;
            saved.life = seen.life;
            saved.change = seen.change;
            out.push(Do::Keep(Keep::Saved(saved.copy())));
        }
        if held.is_some_and(|held| seen.music_count > held) {
            self.ask_music(seen.music_count, false, out);
        }
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
    /// how far it had fetched; the music login stays, and plays.
    fn start_again(&mut self, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if let Some(saved) = live.saved.as_mut() {
            saved.session = None;
            saved.notice = None;
            out.push(Do::Keep(Keep::Saved(saved.copy())));
        }
        self.ended(SignInState::StartAgain);
        // From step 2 the device also sends again every event it still holds (13.3): owed
        // with the sending half.
    }

    pub(super) fn answered(&mut self, ask: u64, status: u16, body: &AnswerBody, given: &Given, out: &mut Vec<Do>) {
        let Some(what) = self.take_flight(ask) else { return };
        let bytes: &[u8] = &body.0;
        match what {
            Purpose::Hello(pending) => self.answered_hello(pending, status, bytes, out),
            Purpose::SignIn { origin, backend_id, then } => {
                let outcome = outcome(account::read_reply::<SignInAnswer>(status, bytes));
                self.answered_sign_in(origin, backend_id, then, outcome, given, out);
            }
            Purpose::SetPassword { origin, backend_id, email, new } => {
                let outcome = outcome(account::read_reply::<PasswordChanged>(status, bytes));
                self.answered_set_password(origin, backend_id, email, new, outcome, out);
            }
            Purpose::Devices { page, opened } => {
                let outcome = outcome(account::read_reply::<DevicePage>(status, bytes));
                self.answered_devices(page, opened, outcome, given, out);
            }
            Purpose::Music { count, refused } => {
                let outcome = outcome(account::read_reply::<MusicLogin>(status, bytes));
                self.answered_music(count, refused, outcome, given, out);
            }
            Purpose::ForceOut { device } => {
                let outcome = outcome(account::read_reply::<ForcedOut>(status, bytes));
                self.answered_force_out(device, outcome, given, out);
            }
            Purpose::ChangePassword => {
                let outcome = outcome(account::read_reply::<PasswordChanged>(status, bytes));
                self.answered_change(outcome, given, out);
            }
            Purpose::Tell | Purpose::TellNote => {
                // The device's, not the account's: nothing in it touches the account now
                // signed in. Any whole answer of the backend's settles it, done or refused.
                if !matches!(outcome(account::read_reply::<SignedOut>(status, bytes)), Outcome::Nothing)
                    && let Some(live) = self.live.as_mut()
                    && live.note.take().is_some()
                {
                    out.push(Do::Forget(Forget::Note));
                }
            }
        }
    }

    fn answered_hello(&mut self, pending: Pending, status: u16, body: &[u8], out: &mut Vec<Do>) {
        let hello = match proto::classify(status, body) {
            Found::Backend(hello) => hello,
            Found::NoBackend(_) => return self.set_form(SignInState::NoNexusServer, 0),
        };
        let ours: Speaks = SPEAKS;
        if hello.speaks.low() > ours.high() {
            return self.set_form(SignInState::AppTooOld, 0);
        }
        if hello.speaks.high() < ours.low() || !hello.does(HAS_ACCOUNTS) {
            return self.set_form(SignInState::ServerTooOld, 0);
        }
        let Pending { origin, credential, new } = pending;
        let then = new.map(|new| Then { new });
        if !self.ask_sign_in(origin, hello.id, credential, then, out) {
            self.set_form(SignInState::CantReach, 0);
        }
    }

    fn answered_sign_in(&mut self, origin: String, backend_id: BackendId, then: Option<Then>, outcome: Outcome<SignInAnswer>, given: &Given, out: &mut Vec<Do>) {
        let (answer, seen) = match outcome {
            Outcome::Done(answer, seen) if seen.backend == backend_id => (answer, seen),
            // A receipt from another backend than the one whose hello was read: not ours.
            Outcome::Done(..) => return self.set_form(SignInState::NoNexusServer, 0),
            Outcome::Refused(why, _) => {
                let (state, wait) = match why {
                    Why::WrongLogin => (SignInState::WrongLogin, 0),
                    Why::TooManyTries { wait_ms } => (SignInState::TooManyTries, wait_ms),
                    Why::AccountOff => (SignInState::AccountOff, 0),
                    Why::Busy => (SignInState::Busy, 0),
                    Why::Incompatible { low, .. } if low > SPEAKS.high() => (SignInState::AppTooOld, 0),
                    Why::Incompatible { .. } => (SignInState::ServerTooOld, 0),
                    Why::PasswordNotAllowed => (SignInState::PasswordNotAllowed, 0),
                    _ => (SignInState::CantReach, 0),
                };
                return self.set_form(state, wait);
            }
            Outcome::Nothing => return self.set_form(SignInState::CantReach, 0),
        };
        let signed = match answer {
            SignInAnswer::SignedIn(signed) => signed,
            SignInAnswer::PasswordSetup(setup) => {
                // A code was redeemed: choose the password at once, under the session that
                // gives leave for that alone. Nothing is kept between.
                let Some(Then { new }) = then else { return self.set_form(SignInState::CantReach, 0) };
                let email = setup.person.email.clone();
                let Some(password) = Password::new(new.expose()).filter(|password| password.allowed_for(&email)) else {
                    return self.set_form(SignInState::PasswordNotAllowed, 0);
                };
                let head = RequestHead { speaks: SPEAKS, restore: seen.restore };
                let Some(request_id) = request_id(given) else { return self.set_form(SignInState::NoRandom, 0) };
                let Some(body) = hidden(account::encode(&ChangePassword { head, request_id, current_password: None, new_password: password })) else {
                    return self.set_form(SignInState::CantReach, 0);
                };
                let what = Purpose::SetPassword { origin, backend_id, email, new };
                if !self.ask(what, account::CHANGE_PASSWORD, Some(&setup.session), Some(body), out) {
                    self.set_form(SignInState::CantReach, 0);
                }
                return;
            }
        };
        // A new account is published: the fence moves, and nothing asked before is acted on.
        self.generation = self.generation.wrapping_add(1);
        let Some(live) = self.live.as_mut() else { return };
        live.flight.retain(|flight| matches!(flight.what, Purpose::Tell | Purpose::TellNote));
        live.renewal = Renewal::default();
        live.devices = Devices::not_asked();
        live.password = PasswordShown::idle();
        live.music_words = None;
        live.reach = Reach::Up;
        live.stranger = false;
        live.checked_ms = Some(given.now_ms);
        let saved = Saved {
            nexus: origin.clone(),
            backend_id: backend_id.to_hex(),
            account: signed.person.email.as_str().to_owned(),
            email: signed.person.email.as_str().to_owned(),
            name: signed.person.name.as_str().to_owned(),
            device: signed.device.get(),
            restore: seen.restore,
            life: seen.life,
            change: seen.change,
            notice: Some(signed.notice.as_str().to_owned()),
            session: Some(signed.session),
            music: Some(signed.music),
        };
        live.form.email = saved.email.clone();
        // One thing, whole, in one step; then the player.
        out.push(Do::Keep(Keep::Saved(saved.copy())));
        if let Some(login) = &saved.music {
            out.push(Do::Hand(Hand::Login(copy(login))));
        }
        let count = saved.music.as_ref().map(|login| login.count);
        live.saved = Some(saved);
        // Signing in from this device ended whatever session it had at that backend (8.6),
        // so a note for it there is settled.
        if live.note.as_ref().is_some_and(|note| note.nexus == origin && BackendId::from_hex(&note.backend_id) == Some(backend_id)) {
            live.note = None;
            live.flight.retain(|flight| !matches!(flight.what, Purpose::Tell | Purpose::TellNote));
            out.push(Do::Forget(Forget::Note));
        }
        self.set_form(SignInState::Idle, 0);
        if count.is_some_and(|count| seen.music_count > count) {
            self.ask_music(seen.music_count, false, out);
        }
    }

    fn answered_set_password(&mut self, origin: String, backend_id: BackendId, email: Email, new: Typed, outcome: Outcome<PasswordChanged>, out: &mut Vec<Do>) {
        match outcome {
            Outcome::Done(PasswordChanged {}, seen) if seen.backend == backend_id => {
                // The password is chosen: sign in with it, as the person would next.
                let Some(password) = Password::new(new.expose()) else { return self.set_form(SignInState::CantReach, 0) };
                if !self.ask_sign_in(origin, backend_id, Pressed::Password { email, password }, None, out) {
                    self.set_form(SignInState::CantReach, 0);
                }
            }
            Outcome::Refused(Why::PasswordNotAllowed, _) => self.set_form(SignInState::PasswordNotAllowed, 0),
            Outcome::Refused(Why::TooManyTries { wait_ms }, _) => self.set_form(SignInState::TooManyTries, wait_ms),
            Outcome::Refused(Why::Busy, _) => self.set_form(SignInState::Busy, 0),
            Outcome::Refused(Why::NotSignedIn, _) => self.set_form(SignInState::WrongLogin, 0),
            _ => self.set_form(SignInState::CantReach, 0),
        }
    }

    fn answered_devices(&mut self, page: usize, opened: bool, outcome: Outcome<DevicePage>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let Some(live) = self.live.as_mut() else { return };
        let this = live.saved.as_ref().map(|saved| saved.device);
        match outcome {
            Outcome::Done(list, _) => {
                if page == 0 {
                    live.devices.reading.clear();
                }
                for device in list.devices() {
                    let this_one = Some(device.id.get()) == this;
                    live.devices.reading.push(DeviceRow {
                        id: device.id.get(),
                        name: device.name.as_str().to_owned(),
                        kind: device.kind,
                        this_one,
                        seen: words::seen(this_one, device.signed_in, device.last_seen_ms, given.now_ms),
                        can_force_out: device.signed_in && !this_one,
                        working: false,
                    });
                }
                let next = page.checked_add(1).filter(|next| *next < DEVICE_PAGES_MAX);
                if let (Some(after), Some(next)) = (list.next(), next)
                    && self.ask_devices(next, Some(after), opened, given, out)
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

    fn answered_music(&mut self, count: u64, refused: bool, outcome: Outcome<MusicLogin>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let Some(live) = self.live.as_mut() else { return };
        let answered = |renewal: &mut Renewal| {
            if refused {
                renewal.refreshed = Some(count);
            } else {
                renewal.rose = Some(count);
            }
        };
        match outcome {
            Outcome::Done(login, _) => {
                answered(&mut live.renewal);
                let held = live.held_count();
                if held.is_none_or(|held| login.count > held) {
                    // Newer: put in place in one step, and handed over at once, so that the
                    // car goes back to the music server and what waited is tried once more.
                    live.renewal.refused = None;
                    live.music_words = None;
                    if let Some(saved) = live.saved.as_mut() {
                        saved.music = Some(copy(&login));
                        out.push(Do::Keep(Keep::Saved(saved.copy())));
                    }
                    out.push(Do::Hand(Hand::Login(login)));
                } else if let Some(refused) = live.renewal.refused {
                    // Nothing newer than the refused one: it stays set aside, and is renewed
                    // again only when the network comes back or the number moves.
                    live.renewal.refreshed = Some(refused);
                    live.music_words = Some(words::music(words::Music::NoNewer));
                }
            }
            Outcome::Refused(Why::NoMusicLogin, _) => {
                answered(&mut live.renewal);
                if let Some(refused) = live.renewal.refused {
                    live.renewal.refreshed = Some(refused);
                }
                // to be account's: 9.4 has two reasons there is none ("none has been set for
                // you yet", "it cannot be used just now"); account has one.
                live.music_words = Some(words::music(words::Music::NoneSet));
            }
            Outcome::Refused(..) | Outcome::Nothing => {
                // Unanswered: owed, and asked again only when the backend may be reachable.
                live.renewal.owed = Some((count, refused));
                if live.renewal.refused.is_some() {
                    live.music_words = Some(words::music(words::Music::WhenReachable));
                }
            }
        }
    }

    fn answered_force_out(&mut self, device: u64, outcome: Outcome<ForcedOut>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let Some(live) = self.live.as_mut() else { return };
        let gone = |live: &mut Live, words: String| {
            for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                row.working = false;
                row.can_force_out = false;
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
            Outcome::Refused(Why::WrongLogin, _) => self.set_password(PasswordState::WrongCurrent, 0),
            Outcome::Refused(Why::PasswordNotAllowed, _) => self.set_password(PasswordState::NotAllowed, 0),
            Outcome::Refused(Why::TooManyTries { wait_ms }, _) => self.set_password(PasswordState::TooManyTries, wait_ms),
            Outcome::Refused(Why::Busy, _) => self.set_password(PasswordState::Busy, 0),
            Outcome::Refused(..) | Outcome::Nothing => self.set_password(PasswordState::CantReach, 0),
        }
    }

    // -----------------------------------------------------------------------------------
    // Failures, the network, Offline, ticks, the music server
    // -----------------------------------------------------------------------------------

    pub(super) fn failed(&mut self, ask: u64, failure: Failure, given: &Given, out: &mut Vec<Do>) {
        let Some(what) = self.take_flight(ask) else { return };
        let _ = out;
        // 6.4: no connection was ever opened, the address leads inside, and the permission
        // is not held. A certificate that is wrong is never this.
        let blocked = !failure.how.connected() && failure.inside == Some(true) && failure.permission == Some(false);
        let form = if blocked {
            SignInState::NeedsPermission
        } else if failure.how == How::Secure {
            SignInState::NotSecure
        } else {
            SignInState::CantReach
        };
        match what {
            Purpose::Hello(_) | Purpose::SignIn { .. } | Purpose::SetPassword { .. } => self.set_form(form, 0),
            Purpose::Devices { opened, .. } => {
                self.unreached(given);
                if opened && let Some(live) = self.live.as_mut() {
                    live.devices.state = ListState::Failed;
                    live.devices.words = words::devices(words::Devices::CantReach);
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
            Purpose::ForceOut { device } => {
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
            Purpose::Tell | Purpose::TellNote => {}
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
        // Nothing is asked while Offline is on, and nothing that was asked is waited for.
        let abandoned: Vec<Flight> = std::mem::take(&mut live.flight);
        let mut form = false;
        for flight in abandoned {
            match flight.what {
                Purpose::Hello(_) | Purpose::SignIn { .. } | Purpose::SetPassword { .. } => form = true,
                Purpose::Devices { opened: true, .. } => {
                    live.devices.state = ListState::Failed;
                    live.devices.words = words::devices(words::Devices::Offline);
                }
                Purpose::Music { count, refused } => live.renewal.owed = Some((count, refused)),
                Purpose::ForceOut { device } => {
                    for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                        row.working = false;
                    }
                    live.devices.words = words::devices(words::Devices::Offline);
                }
                Purpose::ChangePassword => {
                    live.password = PasswordShown { state: PasswordState::CantReach, words: words::password(PasswordState::CantReach, 0, true) };
                }
                Purpose::Devices { opened: false, .. } | Purpose::Tell | Purpose::TellNote => {}
            }
        }
        if live.renewal.refused.is_some() && live.renewal.owed.is_some() {
            live.music_words = Some(words::music(words::Music::WhenOnline));
        }
        if form {
            self.set_form(SignInState::Offline, 0);
        }
    }

    pub(super) fn tick(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        if live.offline || live.too_old {
            return;
        }
        let due = |last: Option<u64>, every: u64| last.is_none_or(|last| given.now_ms.checked_sub(last).is_none_or(|age| age >= every));
        if live.note.is_some() && due(live.told_ms, TELL_EVERY_MS) {
            self.ask_note(given, out);
        }
        let Some(live) = self.live.as_ref() else { return };
        if live.signed_in() && due(live.checked_ms, CHECK_EVERY_MS) {
            self.ask_devices(0, None, false, given, out);
        }
        // The sending half (C8's events, C6's Diagnostics) is driven from here: owed.
    }

    /// 6.3, as ruled: the first report for a number sets it aside, at once; one renewal for
    /// that number, in the background; never a sign-out.
    pub(super) fn music_refused(&mut self, count: u64, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let Some(held) = live.saved.as_ref().and_then(|saved| saved.music.as_ref()) else { return };
        if count < held.count {
            // A caller still held an older one than is held: it is handed the one held.
            out.push(Do::Hand(Hand::Login(copy(held))));
            return;
        }
        if count > held.count || live.renewal.refused == Some(count) {
            // A number never handed out, or one already set aside: nothing more.
            return;
        }
        live.renewal.refused = Some(count);
        out.push(Do::Hand(Hand::Refused { count }));
        if live.renewal.refreshed == Some(count) {
            live.music_words = Some(words::music(words::Music::NoNewer));
            return;
        }
        if !live.signed_in() {
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
        {
            live.music_words = Some(words::music(words::Music::WhenReachable));
        }
    }

    // -----------------------------------------------------------------------------------
    // Showing
    // -----------------------------------------------------------------------------------

    pub(super) fn show(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        let screen = if live.signed_in() { Screen::SignedIn } else { Screen::SignIn };
        let signed = live.saved.as_ref().filter(|_| live.signed_in());
        let row = match (signed, live.reach) {
            _ if live.too_old => words::row(Row::TooOld),
            (None, _) => words::row(Row::SignedOut),
            _ if live.stranger => words::row(Row::Stranger),
            _ if live.offline => words::row(Row::Offline),
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
            },
            row,
            who: signed.map(|saved| format!("{} ({})", saved.name, saved.email)),
            music: live.music_words.clone(),
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
