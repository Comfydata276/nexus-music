//! The machine's moves: what each thing that happened does.

use super::words::{self, Row};
use super::*;

impl Client {
    // -----------------------------------------------------------------------------------
    // Asking
    // -----------------------------------------------------------------------------------

    /// Asks, if asking is allowed now: never while Offline is on, never when the backend
    /// speaks no number this app does, and never two of one kind at once. Says whether it
    /// asked.
    fn ask(&mut self, what: Purpose, path: &str, credential: Option<&Session>, body: Option<account::WireBody>, out: &mut Vec<Do>) -> bool {
        let Some(live) = self.live.as_mut() else { return false };
        if live.offline || live.too_old || live.flight.len() >= FLIGHT_MAX || live.flight.iter().any(|flight| flight.what.same_kind(&what)) {
            return false;
        }
        let origin = match &what {
            Purpose::Hello(pending) => pending.origin.as_str(),
            Purpose::SignIn { origin, .. } => origin.as_str(),
            Purpose::Tell => match &live.to_tell {
                Some(tell) => tell.backend.as_str(),
                None => return false,
            },
            _ => match &live.account {
                Some(account) => account.backend.as_str(),
                None => return false,
            },
        };
        let (method, within_ms, max_body) = match &what {
            Purpose::Hello(_) => (Method::Get, HELLO_WITHIN_MS, HELLO_MAX_BYTES),
            Purpose::SignIn { .. } => (Method::Post, SIGN_IN_WITHIN_MS, ANSWER_MAX_BYTES),
            _ => (Method::Post, CALL_WITHIN_MS, ANSWER_MAX_BYTES),
        };
        let body = match body {
            Some(body) => match std::str::from_utf8(body.bytes()) {
                Ok(text) => Some(Hidden(Zeroizing::new(text.to_owned()))),
                Err(_) => return false,
            },
            None => None,
        };
        if method == Method::Post && body.is_none() {
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
        let account = self.live.as_ref()?.account.as_ref()?;
        Some(RequestHead { speaks: SPEAKS, restore: account.restore })
    }

    /// The session, when it may be used for an ordinary call (not one only for choosing a
    /// password).
    fn full_session(&self) -> Option<Session> {
        let live = self.live.as_ref()?;
        let account = live.account.as_ref()?;
        if account.password_setup {
            return None;
        }
        live.session.clone()
    }

    fn ask_devices(&mut self, page: usize, after: Option<DeviceId>, opened: bool, given: &Given, out: &mut Vec<Do>) -> bool {
        let (Some(session), Some(head)) = (self.full_session(), self.head()) else { return false };
        let Ok(body) = account::encode(&ListDevices { head, after }) else { return false };
        let asked = self.ask(Purpose::Devices { page, opened }, account::LIST_DEVICES, Some(&session), Some(body), out);
        if asked && let Some(live) = self.live.as_mut() {
            live.checked_ms = Some(given.now_ms);
        }
        asked
    }

    /// One refresh of the music login for this count, if the rule allows one now.
    fn ask_music(&mut self, count: u64, refused: bool, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let answered = if refused { live.renewal.refreshed } else { live.renewal.rose };
        if answered == Some(count) || live.flight.iter().any(|flight| matches!(flight.what, Purpose::Music { .. })) {
            return;
        }
        let (Some(session), Some(head)) = (self.full_session(), self.head()) else { return };
        let Ok(body) = account::encode(&GetMusicLogin { head }) else { return };
        let asked = self.ask(Purpose::Music { count, refused }, account::MUSIC_LOGIN, Some(&session), Some(body), out);
        if let Some(live) = self.live.as_mut() {
            if asked {
                live.renewal.owed = None;
            } else {
                live.renewal.owed = Some((count, refused));
            }
        }
    }

    fn ask_tell(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(tell) = self.live.as_ref().and_then(|live| live.to_tell.clone()) else { return };
        let head = RequestHead { speaks: SPEAKS, restore: tell.restore };
        let Ok(body) = account::encode(&SignOut { head }) else { return };
        if self.ask(Purpose::Tell, account::SIGN_OUT, Some(&tell.session), Some(body), out)
            && let Some(live) = self.live.as_mut()
        {
            live.told_ms = Some(given.now_ms);
        }
    }

    /// Something says the backend may be reachable now (a start, the network back, Offline
    /// off): ask what is owed. `check`: also ask whether the session stands.
    pub(super) fn after_reach(&mut self, given: &Given, out: &mut Vec<Do>, check: bool) {
        let Some(live) = self.live.as_ref() else { return };
        if live.offline || live.too_old {
            return;
        }
        let signed_in = self.full_session().is_some();
        let owed = live.renewal.owed;
        let missing = live.music.is_none().then(|| live.account.as_ref().map(|account| account.music_count)).flatten();
        if live.to_tell.is_some() {
            self.ask_tell(given, out);
        }
        if signed_in {
            if let Some((count, refused)) = owed {
                self.ask_music(count, refused, out);
            } else if let Some(count) = missing {
                // Signed in with no music login kept: fetch it (it was lost from the store).
                self.ask_music(count, false, out);
            }
            if check {
                self.ask_devices(0, None, false, given, out);
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Presses
    // -----------------------------------------------------------------------------------

    pub(super) fn pressed(&mut self, press: Press, given: &Given, out: &mut Vec<Do>) {
        match press {
            Press::SignIn { address, email, password } => self.press_sign_in(address, Some(email), password, false, given, out),
            Press::SignInWithCode { address, code } => self.press_sign_in(address, None, code, true, given, out),
            Press::ChoosePassword { password, again } => self.press_choose(password, again, given, out),
            Press::SignOut => self.sign_out(given, out),
            Press::OpenDevices => self.open_devices(given, out),
            Press::ForceOut { device } => self.press_force_out(device, given, out),
            Press::ChangePassword { current, new, again } => self.press_change(current, new, again, out),
            Press::NoticeSeen => {
                if let Some(live) = self.live.as_mut()
                    && let Some(account) = live.account.as_mut()
                    && account.notice.take().is_some()
                {
                    out.push(Do::Keep(Keep::Account(account.clone())));
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

    fn press_sign_in(&mut self, address: String, email: Option<String>, secret: Typed, code: bool, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        // Signed in already, or a sign-in under way: a second press asks nothing (one ask is
        // one request; two quick presses are one).
        if live.session.is_some() || live.form.state == SignInState::Working {
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
            None if secret.expose().trim().is_empty() => return self.set_form(SignInState::NoPassword, 0),
            None => match tidy_code(secret.expose()) {
                Some(code) => Pressed::Code(code),
                None => return self.set_form(SignInState::WrongLogin, 0),
            },
        };
        let _ = code;
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
        let email_typed = live.form.email.clone();
        let pending = Pending { origin, credential, email_typed };
        if self.ask(Purpose::Hello(pending), proto::HELLO, None, None, out) {
            self.set_form(SignInState::Working, 0);
        } else {
            self.set_form(SignInState::CantReach, 0);
        }
    }

    /// Asks to sign in, once `hello` has said this is a backend that has accounts.
    fn ask_sign_in(&mut self, origin: String, backend_id: BackendId, email_typed: String, credential: Pressed, out: &mut Vec<Do>) -> bool {
        let Some(live) = self.live.as_ref() else { return false };
        let Some(device_secret) = live.device_secret.clone() else { return false };
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
            // the design does not say whether a sign-in names a restore number. None: a
            // sign-in begins afresh, and takes the number its answer's stamp carries.
            restore: None,
        };
        let Ok(body) = account::encode(&message) else { return false };
        self.ask(Purpose::SignIn { origin, backend_id, email_typed }, account::SIGN_IN, None, Some(body), out)
    }

    fn press_choose(&mut self, password: Typed, again: Typed, _given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        let (Some(session), Some(account)) = (&live.session, &live.account) else { return };
        if !account.password_setup || live.form.state == SignInState::Working {
            return;
        }
        if password.expose() != again.expose() {
            return self.set_form(SignInState::PasswordsDiffer, 0);
        }
        let Some(email) = Email::new(&account.email) else { return self.set_form(SignInState::PasswordNotAllowed, 0) };
        let Some(new) = Password::new(password.expose()).filter(|new| new.allowed_for(&email)) else {
            return self.set_form(SignInState::PasswordNotAllowed, 0);
        };
        let session = session.clone();
        let head = RequestHead { speaks: SPEAKS, restore: account.restore };
        let Ok(body) = account::encode(&ChangePassword { head, current_password: None, new_password: new }) else { return };
        if live.offline {
            return self.set_form(SignInState::Offline, 0);
        }
        if self.ask(Purpose::ChoosePassword { new: password }, account::CHANGE_PASSWORD, Some(&session), Some(body), out) {
            self.set_form(SignInState::Working, 0);
        } else {
            self.set_form(SignInState::CantReach, 0);
        }
    }

    /// Signing out here: lasting on the device at once, whatever the network; the backend
    /// is told from what is kept apart, now if it can be and later if not (as ruled).
    fn sign_out(&mut self, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let (Some(session), Some(account)) = (live.session.take(), live.account.take()) else { return };
        let tell = ToTell { session, backend: account.backend.clone(), restore: account.restore };
        out.push(Do::Forget(Forget::Session));
        out.push(Do::Forget(Forget::Music));
        out.push(Do::Forget(Forget::Account));
        out.push(Do::Keep(Keep::ToTell(tell.clone())));
        live.music = None;
        live.to_tell = Some(tell);
        self.ended(SignInState::SignedOut);
        self.ask_tell(given, out);
    }

    /// The published account is gone (signed out, forced out, start again): fence what was
    /// asked for it, and go back to the sign-in form. A sign-out still to be told is the
    /// device's, not the account's, and is kept.
    fn ended(&mut self, state: SignInState) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(live) = self.live.as_mut() {
            live.flight.retain(|flight| matches!(flight.what, Purpose::Tell));
            live.renewal = Renewal::default();
            live.devices = Devices::not_asked();
            live.password = PasswordShown::idle();
            live.music_words = None;
            live.reach = Reach::Unknown;
            live.checked_ms = None;
        }
        self.set_form(state, 0);
    }

    fn open_devices(&mut self, given: &Given, out: &mut Vec<Do>) {
        if self.full_session().is_none() {
            return;
        }
        let Some(live) = self.live.as_mut() else { return };
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
        if self.ask_devices(0, None, true, given, out)
            && let Some(live) = self.live.as_mut()
        {
            live.devices.state = ListState::Working;
            live.devices.words.clear();
            live.devices.reading.clear();
        }
    }

    fn press_force_out(&mut self, device: u64, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_ref() else { return };
        let Some(account) = &live.account else { return };
        if device == account.device {
            // Forcing out this device is signing out here.
            return self.sign_out(given, out);
        }
        let known = live.devices.rows.iter().any(|row| row.id == device && row.can_force_out && !row.working);
        let (Some(session), Some(head), Some(id)) = (self.full_session(), self.head(), DeviceId::new(device)) else { return };
        if !known {
            return;
        }
        if live.offline {
            if let Some(live) = self.live.as_mut() {
                live.devices.words = words::devices(words::Devices::Offline);
            }
            return;
        }
        let Ok(body) = account::encode(&ForceOut { head, device: id }) else { return };
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

    fn press_change(&mut self, current: Typed, new: Typed, again: Typed, out: &mut Vec<Do>) {
        let (Some(session), Some(head)) = (self.full_session(), self.head()) else { return };
        let Some(live) = self.live.as_ref() else { return };
        let Some(account) = &live.account else { return };
        if live.password.state == PasswordState::Working {
            return;
        }
        if new.expose() != again.expose() {
            return self.set_password(PasswordState::Differ, 0);
        }
        let Some(email) = Email::new(&account.email) else { return self.set_password(PasswordState::NotAllowed, 0) };
        let Some(new) = Password::new(new.expose()).filter(|new| new.allowed_for(&email)) else {
            return self.set_password(PasswordState::NotAllowed, 0);
        };
        // A present password outside the rule is nobody's: refused here, unsent.
        let Some(current) = Password::new(current.expose()) else { return self.set_password(PasswordState::WrongCurrent, 0) };
        if live.offline {
            return self.set_password(PasswordState::CantReach, 0);
        }
        let Ok(body) = account::encode(&ChangePassword { head, current_password: Some(current), new_password: new }) else { return };
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
            self.after_reach(given, out, false);
        }
    }

    fn unreached(&mut self, given: &Given) {
        if let Some(live) = self.live.as_mut()
            && !matches!(live.reach, Reach::Down { .. })
        {
            live.reach = Reach::Down { since_ms: given.now_ms };
        }
    }

    /// The numbers an answer's receipt carries: kept, and the music login's count rising
    /// means asking for the new one (9.4).
    fn absorb(&mut self, seen: Seen, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let Some(account) = live.account.as_mut() else { return };
        if BackendId::from_hex(&account.backend_id) != Some(seen.backend) {
            // Not this account's backend: the design does not say. Take nothing from it.
            return;
        }
        if (account.restore, account.life, account.change) != (seen.restore, seen.life, seen.change) {
            account.restore = seen.restore;
            account.life = seen.life;
            account.change = seen.change;
            out.push(Do::Keep(Keep::Account(account.clone())));
        }
        let held = live.music.as_ref().map(|login| login.count);
        if held.is_none_or(|held| seen.music_count > held) {
            self.ask_music(seen.music_count, false, out);
        }
    }

    /// What every answer to a session's call shares: the backend was reached; its receipt
    /// is taken; "not signed in" and "start again" end the account here; "too old" stops
    /// asking. Gives back the outcome for the call's own part, or `None` if it was dealt
    /// with here.
    fn common<T>(&mut self, outcome: Outcome<T>, given: &Given, out: &mut Vec<Do>) -> Option<Outcome<T>> {
        match &outcome {
            Outcome::Done(_, seen) | Outcome::Refused(_, Some(seen)) => {
                let seen = *seen;
                self.reached(given, out);
                self.absorb(seen, out);
            }
            Outcome::Refused(_, None) => self.reached(given, out),
            Outcome::Nothing => self.unreached(given),
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

    /// The backend says this device's session is over (forced out, the password changed
    /// elsewhere, the account switched off, 90 days unused): it is meant to stop (6.3).
    /// The session and the music login go; downloads and settings stay.
    fn forced_out(&mut self, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        live.session = None;
        live.account = None;
        live.music = None;
        out.push(Do::Forget(Forget::Session));
        out.push(Do::Forget(Forget::Music));
        out.push(Do::Forget(Forget::Account));
        self.ended(SignInState::ForcedOut);
    }

    /// "Start again" (12.3, 12.4): the backend was put back from a copy. The session goes;
    /// the person signs in again; the music login stays, and plays.
    fn start_again(&mut self, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        live.session = None;
        live.account = None;
        out.push(Do::Forget(Forget::Session));
        out.push(Do::Forget(Forget::Account));
        self.ended(SignInState::StartAgain);
        // the design does not say what a device does with its journal here: from step 2 it
        // sends again every event it still holds (13.3). Owed with the sending half.
    }

    pub(super) fn answered(&mut self, ask: u64, status: u16, body: &AnswerBody, given: &Given, out: &mut Vec<Do>) {
        let Some(what) = self.take_flight(ask) else { return };
        let bytes: &[u8] = &body.0;
        match what {
            Purpose::Hello(pending) => self.answered_hello(pending, status, bytes, out),
            Purpose::SignIn { origin, backend_id, email_typed } => {
                let outcome = outcome(account::read_reply::<SignInAnswer>(status, bytes));
                self.answered_sign_in(origin, backend_id, email_typed, outcome, given, out);
            }
            Purpose::ChoosePassword { new } => {
                let outcome = outcome(account::read_reply::<PasswordChanged>(status, bytes));
                self.answered_choose(new, outcome, given, out);
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
            Purpose::Tell => {
                // The device's, not the account's: nothing in it touches the account now
                // signed in. Any answer of the backend's settles it.
                if !matches!(outcome(account::read_reply::<SignedOut>(status, bytes)), Outcome::Nothing)
                    && let Some(live) = self.live.as_mut()
                    && live.to_tell.take().is_some()
                {
                    out.push(Do::Forget(Forget::ToTell));
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
        let Pending { origin, credential, email_typed } = pending;
        if !self.ask_sign_in(origin, hello.id, email_typed, credential, out) {
            self.set_form(SignInState::CantReach, 0);
        }
    }

    fn answered_sign_in(&mut self, origin: String, backend_id: BackendId, email_typed: String, outcome: Outcome<SignInAnswer>, given: &Given, out: &mut Vec<Do>) {
        let _ = email_typed;
        match outcome {
            Outcome::Done(answer, seen) => {
                // A new account is published: the fence moves, and nothing asked before is
                // acted on.
                self.generation = self.generation.wrapping_add(1);
                let Some(live) = self.live.as_mut() else { return };
                live.flight.retain(|flight| matches!(flight.what, Purpose::Tell));
                live.renewal = Renewal::default();
                live.devices = Devices::not_asked();
                live.password = PasswordShown::idle();
                live.music_words = None;
                live.reach = Reach::Up;
                live.checked_ms = Some(given.now_ms);
                let (session, person, device, music, notice, setup) = match answer {
                    SignInAnswer::SignedIn(signed) => (signed.session, signed.person, signed.device, Some(signed.music), Some(signed.notice), false),
                    SignInAnswer::PasswordSetup(setup) => (setup.session, setup.person, setup.device, None, None, true),
                };
                let file = AccountFile {
                    backend: origin.clone(),
                    backend_id: backend_id.to_hex(),
                    email: person.email.as_str().to_owned(),
                    name: person.name.as_str().to_owned(),
                    device: device.get(),
                    restore: seen.restore,
                    life: seen.life,
                    change: seen.change,
                    music_count: music.as_ref().map_or(seen.music_count, |login| login.count),
                    notice: notice.map(|notice| notice.as_str().to_owned()),
                    password_setup: setup,
                };
                live.form.email = file.email.clone();
                out.push(Do::Keep(Keep::Session(session.clone())));
                live.session = Some(session);
                if let Some(login) = music {
                    out.push(Do::Keep(Keep::Music(copy(&login))));
                    out.push(Do::Hand(Hand::Login(copy(&login))));
                    live.music = Some(login);
                }
                out.push(Do::Keep(Keep::Account(file.clone())));
                live.account = Some(file);
                // Signing in from this device ended whatever session it had there (8.5), so
                // a sign-out still to be told to the same backend is told.
                if live.to_tell.as_ref().is_some_and(|tell| tell.backend == origin) {
                    live.to_tell = None;
                    live.flight.retain(|flight| !matches!(flight.what, Purpose::Tell));
                    out.push(Do::Forget(Forget::ToTell));
                }
                self.set_form(SignInState::Idle, 0);
                if let Some(live) = self.live.as_mut()
                    && seen.backend == backend_id
                    && live.music.as_ref().is_some_and(|login| seen.music_count > login.count)
                {
                    self.ask_music(seen.music_count, false, out);
                }
            }
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
                self.set_form(state, wait);
            }
            Outcome::Nothing => self.set_form(SignInState::CantReach, 0),
        }
    }

    fn answered_choose(&mut self, new: Typed, outcome: Outcome<PasswordChanged>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        match outcome {
            Outcome::Done(PasswordChanged {}, _) => {
                // The password is chosen. Sign in with it, as the person would next.
                let Some(live) = self.live.as_ref() else { return };
                let Some(account) = &live.account else { return };
                let (Some(email), Some(password), Some(backend_id)) =
                    (Email::new(&account.email), Password::new(new.expose()), BackendId::from_hex(&account.backend_id))
                else {
                    return self.set_form(SignInState::CantReach, 0);
                };
                let origin = account.backend.clone();
                let email_typed = account.email.clone();
                if !self.ask_sign_in(origin, backend_id, email_typed, Pressed::Password { email, password }, out) {
                    self.set_form(SignInState::CantReach, 0);
                }
            }
            Outcome::Refused(Why::PasswordNotAllowed, _) => self.set_form(SignInState::PasswordNotAllowed, 0),
            Outcome::Refused(Why::TooManyTries { wait_ms }, _) => self.set_form(SignInState::TooManyTries, wait_ms),
            Outcome::Refused(Why::Busy, _) => self.set_form(SignInState::Busy, 0),
            Outcome::Refused(..) | Outcome::Nothing => self.set_form(SignInState::CantReach, 0),
        }
    }

    fn answered_devices(&mut self, page: usize, opened: bool, outcome: Outcome<DevicePage>, given: &Given, out: &mut Vec<Do>) {
        let Some(outcome) = self.common(outcome, given, out) else { return };
        let Some(live) = self.live.as_mut() else { return };
        let this = live.account.as_ref().map(|account| account.device);
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
        match outcome {
            Outcome::Done(login, _) => {
                if refused {
                    live.renewal.refreshed = Some(count);
                } else {
                    live.renewal.rose = Some(count);
                }
                let held = live.music.as_ref().map(|login| login.count);
                if held.is_none_or(|held| login.count > held) {
                    // Newer: kept, and handed over at once, so the car goes back to the
                    // music server.
                    out.push(Do::Keep(Keep::Music(copy(&login))));
                    out.push(Do::Hand(Hand::Login(copy(&login))));
                    if let Some(account) = live.account.as_mut() {
                        account.music_count = login.count;
                        out.push(Do::Keep(Keep::Account(account.clone())));
                    }
                    live.music = Some(login);
                    live.renewal.refused = None;
                    live.music_words = None;
                } else if let Some(refused) = live.renewal.refused {
                    // Nothing newer than the refused one: the backend has answered for it, and
                    // it is not asked again (one refresh for each count).
                    live.renewal.refreshed = Some(refused);
                    live.music_words = Some(words::music(words::Music::NoNewer));
                }
            }
            Outcome::Refused(Why::NoMusicLogin, _) => {
                if refused {
                    live.renewal.refreshed = Some(count);
                } else {
                    live.renewal.rose = Some(count);
                }
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
            Outcome::Refused(..) | Outcome::Nothing => self.set_password(PasswordState::CantReach, 0),
        }
    }

    // -----------------------------------------------------------------------------------
    // Failures, the network, Offline, ticks, the music server
    // -----------------------------------------------------------------------------------

    pub(super) fn failed(&mut self, ask: u64, failure: Failure, given: &Given, out: &mut Vec<Do>) {
        let Some(what) = self.take_flight(ask) else { return };
        let _ = out;
        let blocked = !failure.how.connected() && failure.inside == Some(true) && failure.permission == Some(false);
        match what {
            Purpose::Hello(_) | Purpose::SignIn { .. } => {
                self.set_form(if blocked { SignInState::NeedsPermission } else { SignInState::CantReach }, 0);
            }
            Purpose::ChoosePassword { .. } => {
                self.unreached(given);
                self.set_form(if blocked { SignInState::NeedsPermission } else { SignInState::CantReach }, 0);
            }
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
            // A sign-out still to be told stays kept, and is told later.
            Purpose::Tell => {}
        }
    }

    pub(super) fn offline(&mut self, on: bool, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        live.offline = on;
        if !on {
            return self.after_reach(given, out, true);
        }
        // Nothing is asked while Offline is on, and nothing that was asked is waited for.
        let abandoned: Vec<Flight> = std::mem::take(&mut live.flight);
        let mut form = false;
        for flight in abandoned {
            match flight.what {
                Purpose::Hello(_) | Purpose::SignIn { .. } | Purpose::ChoosePassword { .. } => form = true,
                Purpose::Devices { opened: true, .. } => {
                    live.devices.state = ListState::Failed;
                    live.devices.words = words::devices(words::Devices::Offline);
                }
                Purpose::Music { count, refused } => {
                    live.renewal.owed = Some((count, refused));
                }
                Purpose::ForceOut { device } => {
                    for row in live.devices.rows.iter_mut().filter(|row| row.id == device) {
                        row.working = false;
                    }
                    live.devices.words = words::devices(words::Devices::Offline);
                }
                Purpose::ChangePassword => live.password = PasswordShown { state: PasswordState::CantReach, words: words::password(PasswordState::CantReach, 0, true) },
                Purpose::Devices { opened: false, .. } | Purpose::Tell => {}
            }
        }
        if live.renewal.refused.is_some() {
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
        if live.to_tell.is_some() && due(live.told_ms, TELL_EVERY_MS) {
            self.ask_tell(given, out);
        }
        let Some(live) = self.live.as_ref() else { return };
        if due(live.checked_ms, CHECK_EVERY_MS) {
            self.ask_devices(0, None, false, given, out);
        }
        // The sending half (C8's events, C6's Diagnostics) is driven from here: owed.
    }

    /// 6.3, as ruled: the requests made with a refused login stop at once; what is on the
    /// device plays; one refresh for the login's count, in the background.
    pub(super) fn music_refused(&mut self, count: u64, given: &Given, out: &mut Vec<Do>) {
        let _ = given;
        let Some(live) = self.live.as_mut() else { return };
        let Some(held) = live.music.as_ref() else { return };
        if count < held.count {
            // The player was using an older one than is held: hand it the one held.
            out.push(Do::Hand(Hand::Login(copy(held))));
            return;
        }
        if count > held.count || live.renewal.refused == Some(count) {
            return;
        }
        live.renewal.refused = Some(count);
        out.push(Do::Hand(Hand::Refused { count }));
        if live.renewal.refreshed == Some(count) {
            live.music_words = Some(words::music(words::Music::NoNewer));
            return;
        }
        if live.session.is_none() || live.account.as_ref().is_none_or(|account| account.password_setup) {
            live.music_words = Some(words::music(words::Music::SignInToRenew));
            return;
        }
        if live.offline {
            live.renewal.owed = Some((count, true));
            live.music_words = Some(words::music(words::Music::WhenOnline));
            return;
        }
        live.music_words = Some(words::music(words::Music::Renewing));
        let waiting = live.flight.iter().any(|flight| matches!(flight.what, Purpose::Music { .. }));
        if waiting {
            // A refresh is under way already (the count rose): its answer serves.
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
        let screen = match (&live.session, &live.account) {
            (Some(_), Some(account)) if account.password_setup => Screen::ChoosePassword,
            (Some(_), Some(_)) => Screen::SignedIn,
            _ => Screen::SignIn,
        };
        let row = match (&live.account, live.reach) {
            _ if live.too_old => words::row(Row::TooOld),
            (None, _) => words::row(Row::SignedOut),
            _ if live.offline => words::row(Row::Offline),
            (Some(_), Reach::Up) => words::row(Row::Connected),
            (Some(account), Reach::Unknown) => words::row(Row::SignedIn(&account.email)),
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
            who: live.account.as_ref().map(|account| format!("{} ({})", account.name, account.email)),
            music: live.music_words.clone(),
            notice: live.account.as_ref().filter(|_| screen == Screen::SignedIn).and_then(|account| account.notice.clone()),
            devices: DevicesShown { state: live.devices.state, rows: live.devices.rows.clone(), words: live.devices.words.clone() },
            password: live.password.clone(),
            plays: live.music.is_some(),
        };
        if self.shown.as_ref() != Some(&shown) {
            out.push(Do::Show(shown.clone()));
            self.shown = Some(shown);
        }
    }
}
