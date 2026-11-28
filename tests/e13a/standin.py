#!/usr/bin/env python3
"""A stand-in for the Nexus server, behind the stand-in proxy (front.py), for the tests of
design 5.3. Loopback only; it answers nothing but what the mode says.

    standin.py <port> <mode file> <log file>

The mode file is read at every request, so a test can change the mode under a running app:

    found      `hello` answered as the backend answers it
    stalled    the request is read, and nothing is ever sent back (the connection is held)
    saturated  every answer is "busy" (503), slowly: nothing for 10 s, then a long body a
               byte a second
    ended      every answer is "not signed in" (401), at once
    half       `hello`'s headers and half its body, then the connection is reset
    bodystall  200 and `hello`'s headers, and the body never comes
    gzip302    a 302 said to be gzip-compressed, and the body never comes

("absent" and "blackhole" are the proxy's: front.py sends /nexus/ to the music server, or to
a port whose queue of connections is full, and this stand-in never sees them.)

In "found" it is also **the backend's accounts, for slice W7**, in the shapes of the shared
library (`core/src/nexus/account.rs`: every answer status 200, `{"done":{"result":…,"receipt":…}}`
or `{"refused":…}`): `hello` names `accounts`; `signin` takes an email and password (or a
one-time code, which gives a session for choosing a password) and hands a music login for
the test Navidrome (a salt it makes and the token for it); `devices`, `force-out`,
`change-password`, `music-login` and `signout` under the session. Made-up accounts only:
`tester@example.org` and `guest@example.org`, with the test Navidrome's own passwords.

A state file beside the mode file (`<mode file>.state`, JSON) lets a test change what the
backend holds under a running app: `"ended": true` ends every session ("not_signed_in");
`"count": n` raises the number of each person's music login; `"bad_token": true` hands a
token the music server will refuse until the music login is asked for again.

Every request is logged, one JSON line each: when, the method, the target exactly as it
arrived, and the User-Agent; for an account call, the call and, if the body named one, the
email (never a password or a session). The suite reads it to see who asked what."""
import hashlib, http.server, json, os, secrets, socket, socketserver, string, struct, sys, threading, time

port, mode_file, log_file = int(sys.argv[1]), sys.argv[2], sys.argv[3]
lock = threading.Lock()

BACKEND = os.environ.get("STANDIN_ID", "5c0ffee05c0ffee05c0ffee05c0ffee0")
HELLO = json.dumps({"nexus": "0.2.0", "id": BACKEND, "speaks": [1, 1], "has": ["accounts"]}).encode()
# The people, and the music server each is handed: made-up names, the test Navidrome's own
# passwords, and the origin their music login names.
MUSIC = os.environ.get("STANDIN_MUSIC", "https://music.example.org:4681")
PEOPLE = {
    "tester@example.org": {"name": "Tester", "password": "nexus-pass-tester", "user": "tester", "music_password": "tester-pass-1"},
    "guest@example.org": {"name": "Guest", "password": "nexus-pass-guest", "user": "guest", "music_password": "guest-pass-1"},
}
CODE = "ABCDEFGH3479"   # a one-time code, for tester
SESSIONS = {}           # session hex -> {"email", "device", "setup"}
DEVICES = {}            # device secret hex -> {"id", "email", "name", "kind", "version", "seen", "session"}
STATE_FILE = mode_file + ".state"


def state():
    try:
        return json.load(open(STATE_FILE))
    except (OSError, ValueError):
        return {}


def receipt(email):
    st = state()
    return {"stamp": {"backend": BACKEND, "restore": 1, "account": email, "life": 1, "change": 1, "tag": "ab" * 32},
            "music_login_count": st.get("count", 1)}


def music_login(email, good=True):
    person = PEOPLE[email]
    salt = "".join(secrets.choice(string.ascii_letters + string.digits) for _ in range(16))
    secret = person["music_password"] if good else "not-the-password"
    token = hashlib.md5((secret + salt).encode()).hexdigest()
    return {"address": os.environ.get("STANDIN_MUSIC_" + person["user"].upper(), MUSIC), "name": person["user"], "salt": salt, "token": token,
            "count": state().get("count", 1)}


def done(email, result):
    return json.dumps({"done": {"result": result, "receipt": receipt(email)}}).encode()


def refused(why):
    return json.dumps({"refused": why}).encode()


def mode():
    try:
        return open(mode_file).read().split()[0]
    except (OSError, IndexError):
        return "found"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def note(self, what, **more):
        with lock, open(log_file, "a") as log:
            log.write(json.dumps({"t": round(time.time(), 3), "method": self.command, "target": self.path,
                                  "agent": self.headers.get("User-Agent", ""), "mode": what, **more}) + "\n")

    def answer(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def go(self):
        m = mode()
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(min(length, 1 << 20)) if length else b""
        if m == "found":
            if self.command == "GET" and self.path == "/nexus/api/hello":
                self.note(m)
                return self.answer(200, HELLO)
            if self.command == "POST" and self.path.startswith("/nexus/api/"):
                return self.account(self.path[len("/nexus/api/"):], body)
            self.note(m)
            return self.answer(404, b'{"error":"no such call"}')
        self.note(m)
        if m == "stalled":
            while True:
                time.sleep(3600)
        if m == "saturated":
            # "Busy", slowly: the status after 10 s, then a long body a byte a second.
            time.sleep(10)
            self.send_response(503)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", "100000")
            self.send_header("Retry-After", "60")
            self.end_headers()
            self.wfile.flush()
            while True:
                self.wfile.write(b" ")
                self.wfile.flush()
                time.sleep(1)
        if m == "bodystall":
            # A 200 whose body never comes.
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(HELLO)))
            self.end_headers()
            self.wfile.flush()
            while True:
                time.sleep(3600)
        if m == "gzip302":
            # A redirect said to be compressed, whose body never comes.
            self.send_response(302)
            self.send_header("Location", "/app/")
            self.send_header("Content-Encoding", "gzip")
            self.send_header("Content-Length", "100")
            self.end_headers()
            self.wfile.flush()
            while True:
                time.sleep(3600)
        if m == "ended":
            return self.answer(401, b'{"error":"not signed in"}')
        if m == "half":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(HELLO)))
            self.end_headers()
            self.wfile.write(HELLO[: len(HELLO) // 2])
            self.wfile.flush()
            # A reset, not a close: linger on, for no time.
            self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            self.connection.close()
            self.close_connection = True
            return
        return self.answer(500, b'{"error":"the stand-in has no such mode"}')

    def account(self, call, body):
        """One account call, answered as the backend answers it."""
        try:
            asked = json.loads(body or b"{}")
        except ValueError:
            self.note("found", call=call)
            return self.answer(200, refused("bad_request"))
        if call == "signin":
            credential = asked.get("credential", {})
            if "password" in credential:
                email, password = credential["password"].get("email", ""), credential["password"].get("password", "")
                self.note("found", call=call, email=email)
                person = PEOPLE.get(email)
                if not person or person["password"] != password:
                    return self.answer(200, refused("wrong_login"))
                setup = False
            elif credential.get("code") == CODE:
                email, setup = "tester@example.org", True
                self.note("found", call=call, email=email, code=True)
            else:
                self.note("found", call=call)
                return self.answer(200, refused("wrong_login"))
            secret = asked.get("device_secret", "")
            device = DEVICES.get(secret)
            if device:
                SESSIONS.pop(device["session"], None)
            else:
                device = DEVICES[secret] = {"id": len(DEVICES) + 1, "email": email}
            session = secrets.token_hex(32)
            device.update({"email": email, "name": asked.get("device_name", "PC"), "kind": asked.get("device_kind", "pc"),
                           "version": asked.get("app_version", "0.0.0"), "seen": int(time.time() * 1000), "session": session})
            SESSIONS[session] = {"email": email, "device": device["id"], "setup": setup}
            person = {"email": email, "name": PEOPLE[email]["name"], "admin": False}
            if setup:
                return self.answer(200, done(email, {"password_setup": {"session": session, "person": person, "device": device["id"]}}))
            good = not state().get("bad_token")
            return self.answer(200, done(email, {"signed_in": {"session": session, "person": person, "device": device["id"],
                                                               "music": music_login(email, good), "notice": "This test server records what is played."}}))
        auth = self.headers.get("Authorization", "")
        session = SESSIONS.get(auth[7:]) if auth.startswith("Bearer ") else None
        self.note("found", call=call, email=(session or {}).get("email"))
        if not session or state().get("ended"):
            return self.answer(200, refused("not_signed_in"))
        email = session["email"]
        if call == "music-login":
            if session["setup"]:
                return self.answer(200, refused("password_setup_only"))
            return self.answer(200, done(email, music_login(email)))
        if call == "devices":
            rows = [{"id": d["id"], "name": d["name"], "kind": d["kind"], "app_version": d["version"], "last_seen_ms": d["seen"],
                     "signed_in": d["session"] in SESSIONS} for d in sorted(DEVICES.values(), key=lambda d: d["id"]) if d["email"] == email]
            return self.answer(200, done(email, {"devices": rows, "more": False}))
        if call == "force-out":
            target = next((d for d in DEVICES.values() if d["id"] == asked.get("device") and d["email"] == email), None)
            if not target:
                return self.answer(200, refused("no_such_device"))
            SESSIONS.pop(target["session"], None)
            return self.answer(200, done(email, {"music_cut_off": False}))
        if call == "change-password":
            current, new = asked.get("current_password"), asked.get("new_password", "")
            if session["setup"]:
                session["setup"] = False
            elif current != PEOPLE[email]["password"]:
                return self.answer(200, refused("wrong_login"))
            PEOPLE[email]["password"] = new
            for other, held in list(SESSIONS.items()):
                if held["email"] == email and other != auth[7:]:
                    SESSIONS.pop(other)
            return self.answer(200, done(email, {}))
        if call == "signout":
            SESSIONS.pop(auth[7:], None)
            return self.answer(200, done(email, {}))
        return self.answer(200, refused("bad_request"))

    do_GET = do_POST = do_HEAD = do_PUT = do_DELETE = go


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


Server(("127.0.0.1", port), Handler).serve_forever()
