"""The tests of design 5.3 for the Windows app (experiment E13a, then slice W1's proof).

Each test drives the app through a real entry point while the backend's side, at the music
server's own origin under /nexus/, is broken one way (front.py's modes), and records how long
the app took. Run first against the released app ("old") to fix the numbers; then against the
app with the Nexus server's component ("new"), with the component's own workers held and its
disk blocked as well.

    suite.py populate                       the released app makes the profile every run starts from
    suite.py run <old|new> <mode> [hook]    one run of every test; exits 1 on any failure.
                                            hook (new only): held, switch-off, sabotage
    suite.py offline [build]                Offline mode on from the start, off, on again
    suite.py late-answer [build]            a late answer for one account, another signed in
    suite.py refused [mode]                 slice W7: a refused music login asked for once
                                            (mode "found", or "absent" for the backend down)
    suite.py ended-offline                  slice W7: the session ended while offline
    suite.py count-offline                  slice W7: the music login's number risen while offline

For the "new" build (slice W7: the app signs in to the Nexus server alone), every run starts
from the kept profile as a sign-in to the Nexus server would have left it (`to_signed_in`):
the released app's saved music password removed, and in its place what the Nexus server
hands over (a session the first origin's stand-in knows, and a music login: a salt and the
token for it), with the folder of downloads carried over as design 11 says.
    suite.py table                          every run's numbers (NUMBERS.md); exits 1 if any
                                            W1 time is over its bar, or any run failed

Everything is on this machine: the test Navidrome "w1" (http://127.0.0.1:4680, made by the
phone repository's `scripts/server --instance w1 start` with NEXUS_TEST_PORT=4680), nginx in
front of it at https://music.example.org:4681 (a made-up name, mapped to 127.0.0.1 inside the
app's container), and the stand-in backend. Nothing real is asked anything."""
import hashlib, json, os, re, shutil, sys, threading, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import front
import app as A

KIT = os.path.expanduser("~/.cache/nexus-music-android/shots/nexus-w1")
KEPT = os.path.join(KIT, "kept-profile")
RUNS = os.path.join(KIT, "runs")
RESULTS = os.path.join(HERE, "results")
NAV = "http://127.0.0.1:4680"
ACCOUNTS = {"tester": "tester-pass-1", "guest": "guest-pass-1"}
# Slice W7: the accounts at the stand-in backend (standin.py), made up.
NEXUS = {"tester": ("tester@example.org", "nexus-pass-tester"), "guest": ("guest@example.org", "nexus-pass-guest")}


# ---- the test server, asked directly ------------------------------------------------------

def api(call, user="tester", **params):
    q = urllib.parse.urlencode({"u": user, "p": ACCOUNTS[user], "v": "1.16.1", "c": "e13a", "f": "json", **params}, doseq=True)
    with urllib.request.urlopen(f"{NAV}/rest/{call}?{q}", timeout=30) as r:
        out = json.load(r)["subsonic-response"]
    if out.get("status") != "ok":
        raise RuntimeError(f"{call}: {out}")
    return out


def songs():
    found = api("search3", query="", songCount=500, albumCount=0, artistCount=0)["searchResult3"]["song"]
    return {(s["album"], s.get("discNumber", 1), s.get("track", 0)): s for s in found}


def playlist(name):
    for p in api("getPlaylists")["playlists"]["playlist"]:
        if p["name"] == name:
            return api("getPlaylist", id=p["id"])["playlist"]
    raise RuntimeError(f"no playlist {name}")


def plays_of(song_id):
    return api("getSong", id=song_id)["song"].get("playCount", 0)


def starred(song_id):
    return any(s["id"] == song_id for s in api("getStarred2")["starred2"].get("song", []))


SHORT = [("First Light", 1, 1), ("Collected Works", 2, 7), ("Long Player", 1, 5)]   # Intro, Outro, Runout Groove
GAINED = ("First Light", 1, 4)                                                       # Glass Orchard
# The list's page (and its sidebar Play) puts Runout Groove first: the saved queue starts there,
# and it is the song whose play is counted (at 10 s of its 20).
FIRST = SHORT[2]


STATE = os.path.join(KIT, "server-state.json")
SYNC = "Nexus Music sync"


def sync_list(user):
    for p in api("getPlaylists", user=user)["playlists"]["playlist"]:
        if p["name"] == SYNC and p.get("owner") == user:
            return p
    return None


def forget_devices():
    """Neither account has a sync record or a saved queue on the server."""
    for user in ACCOUNTS:
        found = sync_list(user)
        if found:
            api("deletePlaylist", user=user, id=found["id"])
        api("savePlayQueue", user=user)


def keep_server_state():
    """What populate left on the server for tester: the sync record and the saved queue."""
    found = sync_list("tester")
    queue = api("getPlayQueue").get("playQueue", {})
    json.dump({"sync_comment": found.get("comment", "") if found else None,
               "queue": [e["id"] for e in queue.get("entry", [])], "current": queue.get("current"),
               "position": queue.get("position", 0)}, open(STATE, "w"))


def put_server_state():
    """Back to what populate left: tester's record and saved queue, and nothing of guest's."""
    state = json.load(open(STATE))
    found = sync_list("tester")
    if state["sync_comment"] is not None:
        if not found:
            raise RuntimeError("tester's sync playlist is gone: run populate again")
        api("updatePlaylist", playlistId=found["id"], comment=state["sync_comment"])
    guest = sync_list("guest")
    if guest:
        api("deletePlaylist", user="guest", id=guest["id"])
    api("savePlayQueue", user="guest")
    if state["queue"]:
        api("savePlayQueue", id=state["queue"], current=state["current"], position=state["position"])
    else:
        api("savePlayQueue")


def prepare_server():
    """The same server for every run: "Short songs" as setup made it plus one song (which the
    profile hasn't downloaded: the list schedules its own download at start-up), and Outro not
    liked."""
    by = songs()
    short = playlist("Short songs")
    have = [s["id"] for s in short.get("entry", [])]
    want = [by[k]["id"] for k in SHORT] + [by[GAINED]["id"]]
    if have != want:
        api("updatePlaylist", playlistId=short["id"], songIndexToRemove=list(range(len(have))))
        api("updatePlaylist", playlistId=short["id"], songIdToAdd=want)
    api("unstar", id=by[SHORT[1]]["id"])
    put_server_state()
    return by


def restore_short():
    """"Short songs" as the kept profile knows it (populate)."""
    by = songs()
    short = playlist("Short songs")
    have = [s["id"] for s in short.get("entry", [])]
    want = [by[k]["id"] for k in SHORT]
    if have != want:
        api("updatePlaylist", playlistId=short["id"], songIndexToRemove=list(range(len(have))))
        api("updatePlaylist", playlistId=short["id"], songIdToAdd=want)


# ---- watching ------------------------------------------------------------------------------

def until(test, seconds, every=0.05, what="it"):
    end = time.time() + seconds
    while True:
        value = test()
        if value:
            return value
        if time.time() > end:
            raise TimeoutError(f"waited {seconds} s for {what}")
        time.sleep(every)


class Dump:
    """Samples the size of the file the app plays into, every two milliseconds, for as long
    as the copy runs: when sound began, and whether it ever stopped coming (an underrun)."""

    def __init__(self, path):
        self.path, self.samples, self.going = path, [], True
        self.thread = threading.Thread(target=self.watch, daemon=True)
        self.thread.start()

    def size(self):
        try:
            return os.stat(self.path).st_size
        except OSError:
            return 0

    def watch(self):
        last = -1
        while self.going:
            size = self.size()
            if size != last:
                self.samples.append((time.time(), size))
                last = size
            time.sleep(0.002)

    def stop(self):
        self.going = False

    def sound_after(self, t, seconds=60):
        """When the file first grew after `t`."""
        base = self.size_at(t)
        def grew():
            for at, size in self.samples:
                if at > t and size > base:
                    return at
        return until(grew, seconds, 0.01, "sound")

    def size_at(self, t):
        size = 0
        for at, s in self.samples:
            if at > t:
                break
            size = s
        return size

    def stalls(self, start, end):
        """The longest time within [start, end] that nothing was written while playing."""
        times = [at for at, _ in self.samples if start <= at <= end]
        if len(times) < 2:
            return None
        return max(b - a for a, b in zip(times, times[1:]))


def rate_of(dump, start, end):
    """Bytes a second the file grew by, over a stretch of plain playing."""
    return (dump.size_at(end) - dump.size_at(start)) / (end - start)


def zero_run(path, offset, around, frame=8):
    """The longest run of silent frames (both channels below 1e-4) within `around` bytes of
    `offset`, in frames; and the largest jump between neighbouring samples there."""
    import struct
    start = max(0, offset - around)
    start -= start % frame
    with open(path, "rb") as f:
        f.seek(start)
        data = f.read(2 * around)
    count = len(data) // 4
    samples = struct.unpack(f"<{count}f", data[: count * 4])
    run = best = 0
    for i in range(0, count - 1, 2):
        if abs(samples[i]) < 1e-4 and abs(samples[i + 1]) < 1e-4:
            run += 1
            best = max(best, run)
        else:
            run = 0
    jump = max((abs(samples[i + 2] - samples[i]) for i in range(0, count - 3, 2)), default=0.0)
    return best, jump


# ---- the app -------------------------------------------------------------------------------

def labels(a):
    return [e.get("accessibleLabel") or "" for e in a.tree()]


def home_drawn(a):
    tree = a.tree()
    return any((e.get("accessibleLabel") or "").startswith("Good ") for e in tree) and any(
        n.get("typeName") == "QuickTile" for e in tree for n in e.get("typeNamesAndIds", []))


def side(a, name):
    for _ in range(8):
        found = a.find(label=name, type="PlaylistItem")
        if found:
            return found[0]
        some = a.find(type="PlaylistItem")
        a.tool("scroll_element", elementHandle=some[0]["handle"], deltaY=-300)
        time.sleep(0.5)
    raise RuntimeError(f"no playlist {name} in the sidebar")


def open_list(a, name):
    for _ in range(4):
        a.click(side(a, name)["handle"])
        try:
            until(lambda: a.find(label=f"Turn on shuffle for {name}", role="Button"), 8, 0.3, f"{name}'s page")
            return
        except TimeoutError:
            pass
    raise RuntimeError(f"couldn't open {name}")


def play_list(a, name):
    """The sidebar's own Play button for a playlist."""
    side(a, name)
    a.click(a.one(label=f"Play {name}")["handle"])


def row(a, title):
    for e in a.find(role="ListItem", type="TrackRow"):
        if e["accessibleLabel"].split(", ")[0] == title:
            return e
    raise RuntimeError(f"no row {title}")


def menu(a, element, item):
    a.click(element["handle"], button="Right")
    found = until(lambda: a.find(label=item, type="MenuRow"), 5, 0.2, f"{item!r} in the menu")
    a.click(found[-1]["handle"])


def queue_rows(a):
    return [e["accessibleLabel"].split(", ")[0] for e in a.find(type="QueueRow")]


def saved_queue(copy):
    try:
        q = json.load(open(copy.path("config", "queue.json")))
    except (OSError, ValueError):
        return None
    return [q["songs"][i]["title"] for i in q["order"]], q["cursor"]


def downloaded(copy, account):
    folder = copy.path("data", "downloads")
    try:
        return {e["song"]["id"] for e in json.load(open(os.path.join(folder, f"index-{account}.json")))}
    except (OSError, ValueError):
        return set()


def settings_tab(a, tab, shows):
    if not a.find(label="Close settings", role="Button"):
        a.press("Settings")
        time.sleep(1.5)
    for attempt in range(10):
        if any(l == shows for l in labels(a)):
            return
        found = [e for e in a.find(label=tab) if any(n.get("typeName") == "Chip" for n in e.get("typeNamesAndIds", []))] or a.find(label=tab, role="Text")
        if not found:
            # A narrow window: the row of tabs scrolls sideways, by its arrow.
            more = a.find(label="Show more", role="Button")
            if more:
                a.click(more[0]["handle"])
            time.sleep(0.8)
            continue
        more = a.find(label="Show more", role="Button")
        if more and found[-1]["absolutePosition"]["x"] + 60 > more[0]["absolutePosition"]["x"]:
            # Under the row's arrow: scroll the row along first.
            a.click(more[0]["handle"])
            time.sleep(0.8)
            continue
        a.click(found[-1]["handle"])
        time.sleep(1)
    raise RuntimeError(f"the {tab} tab didn't open")


def close_settings(a):
    if a.find(label="Close settings", role="Button"):
        a.press("Close settings")
        time.sleep(0.8)


def sign_in(a, user, server=None):
    """Signs in from the sign-in screen: the released app's (to the music server), or the new
    one's (to the Nexus server, at `server`'s origin), whichever is showing."""
    if a.find(label="Nexus server", role="TextInput"):
        email, password = NEXUS[user]
        for label, text in (("Nexus server", server or front.SERVER), ("Email", email), ("Password", password)):
            a.set(a.one(label=label, role="TextInput")["handle"], text)
        pressed = time.time()
        a.press("Sign in")
        return pressed
    for label, text in (("Server address", server or front.SERVER), ("Username", user), ("Password", ACCOUNTS[user])):
        a.set(a.one(label=label, role="TextInput")["handle"], text)
    pressed = time.time()
    a.press("Connect")
    return pressed


def sign_in_form(a):
    """The sign-in screen is showing: the released app's or the new one's."""
    return a.find(label="Connect") or a.find(label="Sign in", role="Button")


def to_signed_in(home, token_of="tester-pass-1", count=1):
    """Slice W7: the profile at `home` as a sign-in to the Nexus server would have left it.
    The released app's saved music password is removed (nothing may read it); in its place,
    one kept item (as `config::SignedIn`, in the file the development build keeps secrets in):
    the session the stand-in was seeded with, a music login (a salt, and the token for it made
    from `token_of`, which a test may make wrong), the account, and the folder carried over."""
    config = os.path.join(A.HOMES, home, "config")
    if os.path.exists(os.path.join(config, "credential")):
        os.remove(os.path.join(config, "credential"))
    salt = "e13asaltsalt0001"
    music = {"server": front.SERVER, "username": "tester", "salt": salt, "token": hashlib.md5((token_of + salt).encode()).hexdigest(), "count": count}
    account = {"backend": front.SERVER, "backend_id": front.BACKEND_A, "email": "tester@example.org", "name": "Tester", "device": 1,
               "restore": 1, "life": 1, "change": 1, "music_count": count, "notice": None, "password_setup": False}
    folder = ACCOUNT_STEM["tester"]
    record = {"session": front.SEED_SESSION, "music": music, "account": account, "folder": folder}
    path = os.path.join(config, "secret-nexus-sign-in")
    open(path, "w").write(json.dumps(record))
    os.chmod(path, 0o600)
    settings = os.path.join(config, "settings.json")
    s = json.load(open(settings))
    s["carried_over"] = [f"{front.BACKEND_A}\ntester@example.org", folder]
    s["sign_in_address"], s["sign_in_email"] = front.SERVER, "tester@example.org"
    json.dump(s, open(settings, "w"))


def no_password_anywhere(home):
    """Every file of the profile, read for the music server's password: none may hold it."""
    found = []
    for root, _, files in os.walk(os.path.join(A.HOMES, home)):
        for name in files:
            path = os.path.join(root, name)
            try:
                if os.path.getsize(path) < 64 << 20 and any(p.encode() in open(path, "rb").read() for p in ACCOUNTS.values()):
                    found.append(os.path.relpath(path, A.HOMES))
            except OSError:
                pass
    return found


ACCOUNT_STEM = {"tester": "https___music_example_org_4681_tester", "guest": "https___music_example_org_4681_guest"}


# ---- populate ------------------------------------------------------------------------------

def populate():
    """The released app makes the profile every run starts from: signed in to the stand-in
    proxy's address as tester; "Short songs" (3 songs) and "Gapless sweep" (8) downloaded and
    kept downloaded; a saved queue of "Short songs" at its start, paused; update checks off."""
    restore_short()
    forget_devices()
    out = os.path.join(KIT, "populate")
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    home = "populate"
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    with front.Front("absent", os.path.join(out, "front")):
        copy = A.Copy("old", home)
        a = copy.launch()
        sign_in(a, "tester")
        until(lambda: home_drawn(a), 60, 0.5, "Home")
        by = songs()
        for name, n in (("Short songs", 3), ("Gapless sweep", 8)):
            open_list(a, name)
            a.press("Download")
            ids = {s["id"] for s in playlist(name)["entry"]}
            until(lambda: ids <= downloaded(copy, ACCOUNT_STEM["tester"]), 300, 1, f"{name} downloaded")
        play_list(a, "Short songs")
        until(lambda: a.find(label="Pause"), 20, 0.1, "playing")
        a.press("Pause")
        time.sleep(1)
        # Back to the start of Intro, so that a run's play is counted at the same point.
        a.key("←")  # (Left: back five seconds)
        time.sleep(4)
        copy.stop(keep=os.path.join(out, "app-output.txt"))
    keep_server_state()
    # A play log that is not empty: three lines the released app wrote (seed-plays.log).
    shutil.copy(os.path.join(HERE, "seed-plays.log"), copy.path("config", "plays.log"))
    s = json.load(open(copy.path("config", "settings.json")))
    s["check_updates"] = False
    json.dump(s, open(copy.path("config", "settings.json"), "w"))
    shutil.rmtree(KEPT, ignore_errors=True)
    shutil.copytree(os.path.join(A.HOMES, home), KEPT)
    os.remove(os.path.join(KEPT, "audio.raw")) if os.path.exists(os.path.join(KEPT, "audio.raw")) else None
    print("kept:", KEPT)
    print("queue:", saved_queue(copy))
    print("plays.log:", open(copy.path("config", "plays.log")).read() if os.path.exists(copy.path("config", "plays.log")) else "(none)")


# ---- one run -------------------------------------------------------------------------------

SUITE = 3   # results of an earlier form of the suite are not compared with
# The stand-in's modes in which a look is held up. In these one look is held for as long as
# the stand-in holds it (the component's deadline made an hour long, a test hook), a look is
# started just before an action when none is running (another), and the same look's process
# must still be running, unanswered, when the action has completed.
# ("saturated" is not one: its "busy" status, after 10 s, is an answer, read before the body.)
STUCK = ("stalled", "blackhole")
# The proxy ends a connection it is still trying to make after 75 s: in the black-hole mode a
# look older than this is let end before an action, and a new one started.
FRESH_FOR = {"blackhole": 35.0}
# What else the component (the "new" build) is put through, beside the stand-in's mode.
HOOKS = {
    None: "",
    # Its look's thread held in a call that never returns, from the first tick on.
    "held": "",
    # Not switched on: nothing of it may be made.
    "switch-off": "",
    # A deliberate fault, which the run must catch and FAIL on: the window's thread held
    # 0.3 s at each look (shorter than the 1 s allowed for timers).
    "sabotage": "",
}
# Steps during which the music server is lost on purpose: not held to the music server's
# own answers being quick.
MUSIC_DOWN_STEPS = ("by_loss_and_retry", "resumed")


class Failed(Exception):
    pass


class Run:
    def __init__(self, build, mode, hook=None):
        self.build, self.mode, self.hook = build, mode, hook
        self.label = f"{build}-{mode}" + (f"-{hook}" if hook else "") + (f"-{os.environ['E13A_REPEAT']}" if os.environ.get("E13A_REPEAT") else "")
        self.out = os.path.join(RUNS, self.label)
        self.results, self.failures, self.windows = {}, [], []
        self.new = build == "new"
        self.on = self.new and hook != "switch-off"
        self.stuck = self.on and mode in STUCK and hook not in ("sabotage", "held")
        self.held = self.on and hook == "held"
        self.faulted = self.stuck or self.held
        self.music_down = False
        self.holds = []

    def say(self, *what):
        line = " ".join(str(w) for w in what)
        print(line, flush=True)
        with open(os.path.join(self.out, "run.log"), "a") as log:
            log.write(line + "\n")

    def time(self, test, metric, seconds):
        self.results.setdefault(test, {})[metric] = round(seconds, 3)
        self.say(f"  {test}.{metric}: {seconds:.3f} s")

    def fact(self, test, metric, value):
        self.results.setdefault(test, {})[metric] = value
        self.say(f"  {test}.{metric}: {value}")

    def fail(self, test, why):
        self.failures.append(f"{test}: {why}")
        self.say(f"  FAILED {test}: {why}")

    def expect(self, test, what, ok, seen=""):
        """An outcome the run asserts: a failure if it does not hold."""
        self.fact(test, what, ok if seen == "" else seen)
        if not ok:
            self.fail(test, f"{what}: {seen}")

    def step(self, test, body, cold=False):
        """One action, under the fault: the fault made sure of and named before it (for the
        first action of a start, the fault from the first tick, checked after), the action,
        and the SAME fault found still in force when the action has completed."""
        self.say(f"\n{test}")
        self.music_down = False
        name = getattr(body, "__name__", test)
        try:
            before = None if cold else self.arm(test)
            began = time.time()
            body()
            ended = time.time()
            self.windows.append((test, name, began, ended))
            after = self.still(test, before, cold, ended)
            if self.faulted:
                self.holds.append((test, name, after))
        except Exception as error:  # A test that fails says so, and the run goes on.
            self.fail(test, f"{type(error).__name__}: {error}")
            try:
                n = len(self.failures)
                self.copy.app.shot(os.path.join(self.out, f"failed-{n}-{test}.png"))
                with open(os.path.join(self.out, f"failed-{n}-{test}.txt"), "w") as f:
                    f.write("\n".join(l for l in labels(self.copy.app) if l))
                    f.write("\n\nqueue.json: " + json.dumps(saved_queue(self.copy)))
            except Exception:
                pass
        finally:
            if self.music_down:
                self.front.music(True)

    # -- the fault, live ------------------------------------------------------------------------

    def nexus_log(self):
        """What the component did (NEXUS_TEST_NEXUS_LOG): (time, word, the rest) a line each."""
        try:
            lines = open(self.copy.path("nexus.log")).read().splitlines()
        except OSError:
            return []
        out = []
        for line in lines:
            parts = line.split(" ", 2)
            if len(parts) >= 2:
                out.append((float(parts[0]), parts[1], parts[2] if len(parts) > 2 else ""))
        return out

    def looks(self):
        """Each look's process this start: pid -> (when spawned, when it ended or None)."""
        looks = {}
        for t, word, rest in self.nexus_log():
            if t < self.copy.started:
                continue
            fields = rest.split()
            if word == "spawned" and len(fields) >= 2:
                looks[fields[1]] = [t, None]
            elif word in ("ended", "killed") and len(fields) >= 2 and fields[1] in looks:
                looks[fields[1]][1] = t
        return looks

    def alive(self, pid):
        return A.sh(f"docker exec {A.NAME} test -d /proc/{pid}").returncode == 0

    def live_look(self):
        for pid, (spawned, ended) in self.looks().items():
            if ended is None and self.alive(pid):
                return pid, spawned
        return None, None

    def held_thread(self):
        marker = self.copy.path("held")
        if not os.path.exists(marker):
            return None, None
        thread = open(marker).read().strip()
        return (thread, os.path.getmtime(marker)) if thread and self.alive(thread) else (None, None)

    def arm(self, test):
        """Before an action: the fault in force now, and which it is."""
        if self.held:
            thread, _ = self.held_thread()
            if not thread:
                raise Failed("before: no held look")
            return ("thread", thread)
        if self.stuck:
            pid, spawned = self.live_look()
            fresh = FRESH_FOR.get(self.mode)
            if pid and fresh and time.time() - spawned > fresh:
                # (The proxy would end it during the action: let it end first.)
                until(lambda: self.looks()[pid][1] is not None or not self.alive(pid), 90, 0.2, "the old look to end")
                pid = None
            if not pid:
                open(self.copy.path("trigger"), "w").write("")
                pid, _ = until(lambda: (lambda found: found if found[0] else None)(self.live_look()), 10, 0.05, "a look started")
            return ("look", pid)
        return None

    def still(self, test, before, cold, ended):
        """After an action: the same fault still in force."""
        if not self.faulted:
            return None
        if self.held:
            thread, since = self.held_thread()
            if not thread:
                raise Failed("after: the held look's thread is gone")
            if cold and since > self.home_at:
                raise Failed(f"after: the hold began {since - self.home_at:.2f} s after the first page was drawn")
            if before and before[1] != thread:
                raise Failed(f"after: held thread {thread}, not {before[1]}")
            return ("thread", thread)
        pid, spawned = self.live_look()
        if before:
            pid = before[1]
            spawned, gone = self.looks().get(pid, (None, True))
            if gone is not None or not self.alive(pid):
                raise Failed(f"after: look {pid} ended during the action")
        elif not pid:
            raise Failed("after: no look in flight")
        elif cold and spawned > self.home_at:
            raise Failed(f"after: the look began {spawned - self.home_at:.2f} s after the first page was drawn")
        return ("look", pid)

    def first_look_after_home(self, launched):
        """Unfaulted, the component's first look of a start comes after the account's first
        page and its queue are usable (five seconds after the account is published)."""
        if not self.on or self.faulted or self.hook == "sabotage":
            return
        until(lambda: any(word == "dispatch" and t > self.copy.started for t, word, _ in self.nexus_log()), 20, 0.1, "the first look")
        first = min(t for t, word, _ in self.nexus_log() if word == "dispatch" and t > self.copy.started)
        self.expect("ordering", f"first_look_after_home_and_queue ({launched})", first > self.usable_at, f"{first:.3f} > {self.usable_at:.3f}")

    def music_was_answering(self):
        """Through every action (but those that lose the music server on purpose), the music
        server's own API answered every request the app made, each within a second."""
        api = []
        for line in open(os.path.join(self.out, "front", "access.log")):
            parts = line.split(" ", 3)
            if len(parts) < 4 or "/rest/" not in parts[3] or any(x in parts[3] for x in ("/rest/stream", "/rest/download", "/rest/getCoverArt")):
                continue
            end, took, status = float(parts[0]), float(parts[1]), parts[2]
            api.append((end - took, took, status, parts[3].split("?")[0]))
        slowest, bad = 0.0, []
        for test, name, t0, t1 in self.windows:
            if name in MUSIC_DOWN_STEPS:
                continue
            for start, took, status, what in api:
                if t0 <= start <= t1:
                    slowest = max(slowest, took)
                    if status != "200" or took > 1.0:
                        bad.append(f"{test}: {what} {status} in {took} s")
        self.time("music_server", "slowest_api_answer_during_the_actions", slowest)
        self.expect("music_server", "every_api_request_answered_within_1_s", not bad, bad[:5])

    # -- a run -----------------------------------------------------------------------------------

    def go(self):
        shutil.rmtree(self.out, ignore_errors=True)
        os.makedirs(self.out)
        self.say(f"run {self.label} at {time.strftime('%Y-%m-%d %H:%M:%S')}")
        self.load = [os.getloadavg()[0]]
        self.by = prepare_server()
        home = f"run-{self.label}"
        shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
        shutil.copytree(KEPT, os.path.join(A.HOMES, home))
        if self.new:
            to_signed_in(home)
        inside = f"/work/target/walk/{home}"
        env = {}
        if self.on:
            env.update({"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_NEXUS_LOG": f"{inside}/nexus.log"})
        if self.new:
            env["NEXUS_TEST_NEXUS_LOG"] = f"{inside}/nexus.log"
        if self.hook == "sabotage":
            env["NEXUS_TEST_PROBE_AGAIN_MS"] = "200"
        if self.stuck:
            env.update({"NEXUS_TEST_DEADLINE_MS": "3600000", "NEXUS_TEST_PROBE_WHOLE_MS": "3600000"})
        if self.faulted:
            # A look at the first tick of each start (and on demand), so that the fault is in
            # force from the first page on.
            env["NEXUS_TEST_TRIGGER"] = f"{inside}/trigger"
        if self.held:
            env["NEXUS_TEST_HOLD_NEXUS"] = f"{inside}/held"
        if self.hook == "sabotage":
            env["NEXUS_TEST_SABOTAGE_UI_MS"] = "300"
        self.copy = A.Copy(self.build, home, env=env)
        self.keep = os.path.join(self.out, "app-output.txt")
        with front.Front(self.mode, os.path.join(self.out, "front")) as self.front:
            try:
                self.first_launch()
                self.second_launch()
            finally:
                if self.copy.app:
                    self.copy.stop(keep=self.keep)
                self.dump.stop()
            self.backend_seen()
        self.music_was_answering()
        self.gapless_sources()
        if self.faulted:
            self.fact("backend", "the_fault_at_each_action", [f"{test}/{name}: {what[0]} {what[1]}" for test, name, what in self.holds])
            self.expect("backend", "every_action_ran_under_the_fault", len(self.holds) == len(self.windows), f"{len(self.holds)} of {len(self.windows)}")
        if self.hook == "sabotage":
            # The deliberate fault is judged as every W1 run is (`table`), at once.
            for why in compare(self.results, [r for r in load_results() if r["build"] == "old"]):
                self.fail("compare", why)
        self.save()
        for name in ("plays.log", "queue.json", "settings.json", "nexus.log"):
            for src in (os.path.join(A.HOMES, home, "config", name), os.path.join(A.HOMES, home, name)):
                if os.path.exists(src):
                    shutil.copy(src, os.path.join(self.out, name))
        shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
        return 1 if self.failures else 0

    def log_unchanged(self, launched):
        """At a start, before anything is played: the play log byte for byte as it was."""
        now = open(self.copy.path("config", "plays.log"), "rb").read()
        self.expect("a_play_is_counted_as_released", f"start_up_left_the_log_byte_for_byte ({launched})", now == self.log_before, f"{len(now)} bytes, {len(self.log_before)} before")

    # -- launch 1: a cold start on the kept profile ------------------------------------------

    def first_launch(self):
        c = self.copy
        self.log_before = open(c.path("config", "plays.log"), "rb").read()
        self.expect("a_play_is_counted_as_released", "the_kept_log_is_not_empty", len(self.log_before) > 0, len(self.log_before))
        self.dump = Dump(c.dump)
        self.initial_log = self.log_before
        if self.faulted:
            open(c.path("trigger"), "w").write("")
        a = c.launch()
        t0 = c.started

        def cold_start():
            until(lambda: home_drawn(a), 60, 0.02, "the first page")
            self.home_at = time.time()
            self.time("cold_start_publishes_the_saved_account", "launch_to_home_drawn", time.time() - t0)
        self.step("cold_start_publishes_the_saved_account", cold_start, cold=True)

        def queue_back():
            until(lambda: a.find(label="Runout Groove", type="LinkText") and a.find(label="Play"), 60, 0.02, "the queue put back")
            self.time("cold_start_puts_the_queue_back_and_plays", "launch_to_queue_shown", time.time() - t0)
            self.usable_at = time.time()
            self.log_unchanged("first start")
            self.server_plays_before = plays_of(self.by[FIRST]["id"])
            pressed = time.time()
            a.press("Play")
            self.pressed_play = pressed
            sound = self.dump.sound_after(pressed)
            self.time("cold_start_puts_the_queue_back_and_plays", "play_to_sound", sound - pressed)
        self.step("cold_start_puts_the_queue_back_and_plays", queue_back)
        self.first_look_after_home("first start")

        def counted():
            log = c.path("config", "plays.log")
            until(lambda: open(log, "rb").read()[len(self.log_before):], 60, 0.02, "the play in the log")
            self.counted_at = time.time()
            self.time("a_play_is_counted_as_released", "play_to_counted", self.counted_at - self.pressed_play)
            now = open(log, "rb").read()
            self.expect("a_play_is_counted_as_released", "the_log_was_only_added_to", now.startswith(self.log_before))
        self.step("a_play_is_counted_as_released", counted)

        intro = self.by[FIRST]["id"]

        def scrobbled():
            until(lambda: plays_of(intro) > self.server_plays_before, 60, 0.1, "the play on the server")
            self.time("a_like_and_a_play_reach_the_music_server", "counted_to_server_has_play", time.time() - self.counted_at)
            time.sleep(1)
            self.expect("a_like_and_a_play_reach_the_music_server", "the_play_reached_the_server_once", plays_of(intro) == self.server_plays_before + 1, plays_of(intro) - self.server_plays_before)
        self.step("a_like_and_a_play_reach_the_music_server", scrobbled)

        def schedule():
            gained = self.by[GAINED]["id"]
            until(lambda: gained in downloaded(c, ACCOUNT_STEM["tester"]), 120, 0.1, "the list's new song downloaded")
            self.time("downloads_start_and_resume", "launch_to_list_scheduled_its_own", time.time() - t0)
        self.step("downloads_start_and_resume", schedule)

        def like():
            outro = self.by[SHORT[1]]["id"]
            # Paused from here, so that the queue moves only by what the tests do.
            if a.find(label="Pause"):
                a.press("Pause")
            open_list(a, "Short songs")
            # The page settled: the list as the server has it now, four songs.
            until(lambda: len(a.find(role="ListItem", type="TrackRow")) == 4, 20, 0.2, "the list's four songs")
            target = row(a, "Outro")
            a.click(target["handle"], button="Right")
            until(lambda: "   Outro" in labels(a), 5, 0.1, "Outro's menu")
            item = until(lambda: a.find(label="Add to Liked songs", type="MenuRow"), 5, 0.1, "the menu's like")
            pressed = time.time()
            a.click(item[-1]["handle"])
            until(lambda: starred(outro), 30, 0.05, "the like on the server")
            self.time("a_like_and_a_play_reach_the_music_server", "like_to_server", time.time() - pressed)
        self.step("a_like_and_a_play_reach_the_music_server", like)

        def queue_edits():
            test = "the_queue_is_changed_and_kept"
            if a.find(label="Pause"):
                a.press("Pause")
            time.sleep(0.5)
            titles = lambda: (saved_queue(c) or [[], 0])
            before = titles()[0].count("Glass Orchard")
            pressed = time.time()
            menu(a, row(a, "Glass Orchard"), "Add to queue")
            until(lambda: titles()[0].count("Glass Orchard") == before + 1, 20, 0.02, "the song added")
            self.time(test, "add_to_kept", time.time() - pressed)
            last = [e for e in a.find(type="QueueRow")][-1]
            title = last["accessibleLabel"].split(", ")[0]
            pressed = time.time()
            menu(a, last, "Play next")
            until(lambda: titles()[0][titles()[1] + 1: titles()[1] + 2] == [title], 20, 0.02, "the song moved")
            self.time(test, "move_to_kept", time.time() - pressed)
            count = titles()[0].count("Outro")
            outro = [e for e in a.find(type="QueueRow") if e["accessibleLabel"].startswith("Outro,")][-1]
            pressed = time.time()
            menu(a, outro, "Remove from queue")
            until(lambda: titles()[0].count("Outro") == count - 1, 20, 0.02, "the song removed")
            self.time(test, "remove_to_kept", time.time() - pressed)
            time.sleep(1)
            self.kept_queue = saved_queue(c)
            self.fact(test, "queue_before_kill", self.kept_queue)
        self.step("the_queue_is_changed_and_kept", queue_edits)
        c.stop(kill=True, keep=self.keep)
        # What the first start added, through to its end (kill -9), byte for byte.
        self.log_before = open(c.path("config", "plays.log"), "rb").read()
        self.expect("a_play_is_counted_as_released", "the_log_was_only_added_to (first start, at its end)", self.log_before.startswith(self.initial_log))
        self.fact("a_play_is_counted_as_released", "added_with_times_masked (first start)", masked(self.log_before[len(self.initial_log):]))
        # (The held look's marker is the process's: the next start holds a look of its own.)
        for name in ("held", "trigger"):
            if os.path.exists(c.path(name)):
                os.remove(c.path(name))

    # -- launch 2: after kill -9 ---------------------------------------------------------------

    def second_launch(self):
        c = self.copy
        if self.faulted:
            open(c.path("trigger"), "w").write("")
        a = c.launch()
        t0 = c.started

        def put_back():
            test = "the_queue_is_changed_and_kept"
            until(lambda: a.find(label="Play") and a.find(type="QueueRow"), 60, 0.02, "the queue after kill -9")
            self.time(test, "restart_to_queue_shown", time.time() - t0)
            self.usable_at = self.home_at = time.time()
            self.log_unchanged("second start")
            self.expect(test, "queue_after_kill_is_the_queue_before", saved_queue(c) == getattr(self, "kept_queue", None), saved_queue(c))
        self.step("the_queue_is_changed_and_kept", put_back, cold=True)
        self.first_look_after_home("second start")

        def direct():
            test = "a_direct_play_starts_it_cold"
            pressed = time.time()
            a.key(" ")
            sound = self.dump.sound_after(pressed, 30)
            self.time(test, "space_to_sound", sound - pressed)
            # Paused again at once: what is counted must not depend on how long the next
            # step takes.
            a.key(" ")
            until(lambda: a.find(label="Play"), 5, 0.05, "paused")
        self.step("a_direct_play_starts_it_cold", direct)

        def continue_here():
            # Another device's queue, saved to the server while this PC is paused with its
            # own: newer than this PC's, it is offered.
            test = "the_queue_is_changed_and_kept"
            theirs = [s["id"] for s in playlist("Morning mix")["entry"]][:3]
            title = api("getSong", id=theirs[1])["song"]["title"]
            # (A device that is playing saves where it has got to every second or so, as this
            # PC does: so does the other one here, until the offer is made.)
            saved = time.time()
            def offered():
                api("savePlayQueue", id=theirs, current=theirs[1], position=int((time.time() - saved) * 1000), c="e13a-other-device")
                return a.find(label="Continue here", role="Button")
            button = until(offered, 60, 1.0, "Continue here offered")
            self.time(test, "offered_after_another_device_saved", time.time() - saved)
            pressed = time.time()
            a.click(button[0]["handle"])
            until(lambda: (lambda q: q and q[0][q[1]] == title)(saved_queue(c)), 20, 0.02, "their queue taken up")
            self.time(test, "continue_here_to_kept", time.time() - pressed)
            if a.find(label="Pause"):
                a.press("Pause")
            # (The other device stops saving here.)
        self.step("the_queue_is_changed_and_kept", continue_here)

        def clear():
            test = "the_queue_is_changed_and_kept"
            length = len(saved_queue(c)[0])
            pressed = time.time()
            a.click(a.one(label="Clear", type="LinkText")["handle"])
            until(lambda: len((saved_queue(c) or [[0] * length])[0]) < length, 20, 0.02, "the queue cleared")
            self.time(test, "clear_to_kept", time.time() - pressed)
        self.step("the_queue_is_changed_and_kept", clear)

        self.joins = {}

        def join(kind, start):
            def body():
                test = "two_songs_join_without_a_gap"
                pressed = time.time()
                start()
                sound = self.dump.sound_after(pressed, 30)
                base = self.dump.size_at(sound - 0.001)
                time.sleep(24)
                rate = rate_of(self.dump, sound + 2, sound + 12)
                stall = self.dump.stalls(sound + 17, sound + 23)
                silent, jump = zero_run(c.dump, int(base + 20 * rate), int(rate // 2))
                self.joins[kind] = (pressed, sound + 20, time.time())
                self.time(test, f"{kind}_play_to_sound", sound - pressed)
                self.time(test, f"{kind}_longest_stall_around_the_join", stall)
                self.fact(test, f"{kind}_silent_frames_at_the_join", silent)
                self.fact(test, f"{kind}_largest_step_at_the_join", round(jump, 4))
                a.press("Pause")
                time.sleep(0.5)
            body.__name__ = f"join_{kind}"
            return body
        for kind, start in (("streamed", lambda: album_play(a, "Continuous Sweep (MP3)")),
                            ("cached", lambda: album_play(a, "Continuous Sweep (MP3)")),
                            # (Its songs are the ones "Gapless sweep" has, downloaded.)
                            ("downloaded", lambda: album_play(a, "Continuous Sweep"))):
            self.step("two_songs_join_without_a_gap", join(kind, start))


        def by_hand():
            test = "downloads_start_and_resume"
            open_list(a, "Morning mix")
            ids = {s["id"] for s in playlist("Morning mix")["entry"]}
            pressed = time.time()
            a.press("Download")
            until(lambda: ids <= downloaded(c, ACCOUNT_STEM["tester"]), 300, 0.1, "Morning mix downloaded")
            self.time(test, "by_hand_8_songs", time.time() - pressed)
        self.step("downloads_start_and_resume", by_hand)

        def offline_by_hand():
            test = "offline_by_hand_and_by_loss"
            settings_tab(a, "Storage", "Offline mode")
            switch = a.one(label="Offline mode", role="Switch")
            pressed = time.time()
            a.click(switch["handle"])
            until(lambda: json.load(open(c.path("config", "settings.json"))).get("offline") is True, 20, 0.02, "offline mode on")
            self.time(test, "by_hand_switch_to_offline", time.time() - pressed)
            close_settings(a)
            pressed = time.time()
            play_downloaded(a)
            sound = self.dump.sound_after(pressed, 30)
            self.time(test, "offline_play_downloaded_to_sound", sound - pressed)
            a.press("Pause")
            settings_tab(a, "Storage", "Offline mode")
            pressed = time.time()
            a.click(a.one(label="Offline mode", role="Switch")["handle"])
            until(lambda: json.load(open(c.path("config", "settings.json"))).get("offline") is False, 20, 0.02, "offline mode off")
            self.time(test, "by_hand_switch_back", time.time() - pressed)
            close_settings(a)
        self.step("offline_by_hand_and_by_loss", offline_by_hand)

        def by_loss_and_retry():
            test = "offline_by_hand_and_by_loss"
            intro = self.by[FIRST]["id"]
            before = plays_of(intro)
            log = c.path("config", "plays.log")
            log_before = os.path.getsize(log)
            self.front.music(False)
            self.music_down = True
            down = time.time()
            close_settings(a)
            nav(a, "Artists")
            until(lambda: offline_shown(a), 60, 0.05, "the app to see the server gone")
            self.time(test, "by_loss_noticed", time.time() - down)
            pressed = time.time()
            play_downloaded(a, "Runout Groove")
            sound = self.dump.sound_after(pressed, 30)
            self.time(test, "by_loss_play_downloaded_to_sound", sound - pressed)
            until(lambda: os.path.getsize(log) > log_before, 60, 0.05, "the play counted while the server is gone")
            a.press("Pause")
            up = time.time()
            self.front.music(True)
            until(lambda: plays_of(intro) > before, 120, 0.2, "the play told again once the server is back")
            self.time("a_like_and_a_play_reach_the_music_server", "server_back_to_play_told_again", time.time() - up)
            until(lambda: not offline_shown(a), 60, 0.1, "back online")
            self.time(test, "server_back_to_online", time.time() - up)
            time.sleep(1)
            self.expect("a_like_and_a_play_reach_the_music_server", "the_held_play_reached_the_server_once", plays_of(intro) == before + 1, plays_of(intro) - before)
        self.step("offline_by_hand_and_by_loss", by_loss_and_retry)

        def resumed():
            test = "downloads_start_and_resume"
            name = "Everything (long list)"
            ids = {s["id"] for s in playlist(name)["entry"]}
            open_list(a, name)
            self.front.music(False)
            self.music_down = True
            a.press("Download")
            time.sleep(6)
            up = time.time()
            self.front.music(True)
            def done():
                if ids <= downloaded(c, ACCOUNT_STEM["tester"]):
                    return True
                if time.time() - up > 20:
                    retry_downloads(a)
                return False
            until(done, 180, 0.2, "the downloads after the server came back")
            self.time(test, "server_back_to_downloads_done", time.time() - up)
        self.step("downloads_start_and_resume", resumed)

        now = open(c.path("config", "plays.log"), "rb").read()
        self.expect("a_play_is_counted_as_released", "the_log_was_only_added_to (second start)", now.startswith(self.log_before))
        self.fact("a_play_is_counted_as_released", "added_with_times_masked (second start)", masked(now[len(self.log_before):]))

        self.settings_row(a, "tester")

        def account():
            test = "an_account_change_is_not_held_up"
            settings_tab(a, "Account", "Sign out")
            a.press("Sign out")
            until(lambda: sign_in_form(a), 20, 0.05, "the sign-in form")
            # Another account, at another origin of the same music server (for the released
            # app, nothing under /nexus/ there; for W7, a backend of its own there, always
            # answering, which hands the guest a music login at that origin).
            pressed = sign_in(a, "guest", front.SERVER2)
            until(lambda: home_drawn(a), 60, 0.02, "the new account's first page")
            self.time(test, "connect_to_home_drawn", time.time() - pressed)
            self.expect(test, "signed_in_as_guest", json.load(open(c.path("config", "settings.json"))).get("username") == "guest")
        self.step("an_account_change_is_not_held_up", account)
        self.settings_row(a, "guest")

    # -- what the component showed and asked -----------------------------------------------------

    def settings_row(self, a, who):
        """The row under Settings, Account, for `who`; and the component's threads."""
        test = "settings_row"
        settings_tab(a, "Account", "Sign out")
        if self.on and not self.faulted:
            # (The first look is 5 s after an account is published.)
            try:
                until(lambda: any(l.startswith("Found at") or l == "None found" for l in labels(a)), 20, 0.5, "the row")
            except TimeoutError:
                pass
        shown = [l for l in labels(a) if l.startswith("Found at") or l == "None found"]
        heading = ("Looked for" if self.new else "Nexus server") in labels(a)
        if not self.on:
            want = []
        elif self.faulted:
            # (Its look never answers; the next account's waits behind it.)
            want = []
        elif who == "tester" and self.mode == "found":
            want = [f"Found at {front.SERVER}/nexus/"]
        elif who == "guest" and self.new:
            # (W7: the guest signed in through the second origin's own backend.)
            want = [f"Found at {front.SERVER2}/nexus/"]
        else:
            want = ["None found"]
        if self.stuck and self.mode in FRESH_FOR and who == "tester" and shown == ["None found"]:
            # (A look the proxy ended at 75 s answered "none" before the next was started.)
            want = shown
        self.expect(test, f"shown for {who}", shown == want and heading == bool(want), shown)
        threads = self.copy.pid_threads()
        self.fact("backend", f"threads for {who}", {"nexus-probe": threads.count("nexus-probe")})
        if not self.on:
            self.expect("backend", f"no thread of the component ({who})", threads.count("nexus-probe") == 0, threads.count("nexus-probe"))
        if self.held:
            self.expect("backend", f"the held look is still held ({who})", threads.count("nexus-probe") >= 1, threads.count("nexus-probe"))
        close_settings(a)

    def backend_seen(self):
        seen = self.front.nexus_in_access_log()
        asked = self.front.asked()
        with open(os.path.join(self.out, "nexus-requests.txt"), "w") as f:
            f.writelines(seen)
        # The component's looks (GETs of hello): the W1 rules count these. W7's account calls
        # (POSTs) are checked one by one below.
        count = len([line for line in seen if '"GET /nexus/api/hello' in line])
        test = "backend"
        if self.hook == "sabotage":
            pass
        elif not self.on or self.held:
            # The released app, the component switched off, or its look held before it asks.
            self.expect(test, "requests_under_nexus", count == 0, count)
        elif self.stuck and self.mode in FRESH_FOR:
            # The proxy ends each held look at 75 s; another is started before the next action.
            self.expect(test, "requests_under_nexus (a held look at a time)", 2 <= count <= 15, count)
        elif self.stuck:
            # One look started at the first tick of each start, held through it.
            self.expect(test, "requests_under_nexus (one held look a start)", 2 <= count <= 4, count)
        else:
            # One at each start (5 s after the account is published) and one for the guest
            # (W7: the guest's at the second origin's own backend, not here).
            want = 2 if self.new else 3
            self.expect(test, "requests_under_nexus (one per account published)", count == want, count)
        account_calls = ("signin", "signout", "devices", "force-out", "change-password", "music-login")
        for line in seen:
            ok = '"GET /nexus/api/hello HTTP/1.1"' in line and "NexusMusic/" in line
            # W7: the shared client's own asks, each a POST to one of the account calls.
            ok = ok or (self.new and "NexusMusic/" in line and any(f'"POST /nexus/api/{call} HTTP/1.1"' in line for call in account_calls))
            if not ok:
                self.fail(test, f"a request that is neither a plain GET of hello nor an account call: {line.strip()}")
        self.fact(test, "requests_at_the_stand_in", len(asked))

    def gapless_sources(self):
        """Where each join's songs came from, by the proxy's log: streamed with the next one
        fetched before the join; cached, and downloaded, not asked of the server."""
        if not getattr(self, "joins", None):
            return
        streams = []
        for line in open(os.path.join(self.out, "front", "access.log")):
            parts = line.split(" ", 3)
            if len(parts) > 3 and "/rest/stream?" in parts[3]:
                end, took = float(parts[0]), float(parts[1])
                song = re.search(r"[?&]id=([^& ]+)", parts[3])
                streams.append((end - took, song.group(1) if song else ""))
        mp3 = [self.by[("Continuous Sweep (MP3)", 1, n)]["id"] for n in (1, 2)]
        test = "two_songs_join_without_a_gap"
        asked = lambda kind: [(t, song) for t, song in streams if self.joins[kind][0] <= t <= self.joins[kind][2]]
        if "streamed" in self.joins:
            got = asked("streamed")
            second = [t for t, song in got if song == mp3[1]]
            self.expect(test, "streamed: both songs streamed, the second fetched before the join",
                        any(song == mp3[0] for _, song in got) and second and second[0] < self.joins["streamed"][1], got)
        if "cached" in self.joins:
            got = [song for _, song in asked("cached") if song in mp3]
            self.expect(test, "cached: neither song asked of the server", got == [], got)
        if "downloaded" in self.joins:
            got = asked("downloaded")
            self.expect(test, "downloaded: nothing streamed", got == [], got)

    def save(self):
        os.makedirs(RESULTS, exist_ok=True)
        self.load.append(os.getloadavg()[0])
        record = {"suite": SUITE, "run": self.label, "machine_load_at_start_and_end": [round(x, 2) for x in self.load], "build": self.build,
                  "mode": self.mode, "hook": self.hook, "at": time.strftime("%Y-%m-%d %H:%M:%S"),
                  "binary": digest(os.path.join(A.HOMES, f"nexus-music-{self.build}")), "results": self.results, "failures": self.failures}
        json.dump(record, open(os.path.join(RESULTS, f"{self.label}.json"), "w"), indent=1, ensure_ascii=False)
        self.say(f"\n{len(self.failures)} failed" + ("".join(f"\n  {f}" for f in self.failures)))


def masked(added):
    """Lines added to the play log, byte for byte, with each time (whole seconds since 1970)
    written as T: the only part that differs between two runs of the same plays."""
    return re.sub(rb"(?<=,)\d{10}(?=[,\]])", b"T", added).decode()


def compare(results, released):
    """A W1 run's results against every released run's: each measure within the released
    app's slowest plus the slack; what was added to the play log as the released app wrote
    it; each join no worse. Returns what failed."""
    failures = []
    for test, metric in released_metrics(released):
        limit, worst = allowed(released, test, metric)
        value = results.get(test, {}).get(metric)
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            failures.append(f"{test}: {metric}: not measured")
        elif value > limit:
            failures.append(f"{test}: {metric}: {value:.3f} s, over {limit:.3f} (released slowest {worst:.3f})")
    for metric in ("added_with_times_masked (first start)", "added_with_times_masked (second start)"):
        theirs = {json.dumps(r["results"].get("a_play_is_counted_as_released", {}).get(metric)) for r in released}
        mine = json.dumps(results.get("a_play_is_counted_as_released", {}).get(metric))
        if mine not in theirs:
            failures.append(f"a_play_is_counted_as_released: {metric}: {mine} is not what the released app wrote ({theirs})")
    for kind_ in ("streamed", "cached", "downloaded"):
        # Silent frames exactly as few; the largest step within a tenth of the released app's
        # largest (it moves with where the 20 ms tick falls on the join).
        for metric, margin in ((f"{kind_}_silent_frames_at_the_join", lambda v: v), (f"{kind_}_largest_step_at_the_join", lambda v: v * 1.1)):
            theirs = [r["results"].get("two_songs_join_without_a_gap", {}).get(metric) for r in released]
            theirs = [v for v in theirs if isinstance(v, (int, float))]
            mine = results.get("two_songs_join_without_a_gap", {}).get(metric)
            if theirs and (not isinstance(mine, (int, float)) or mine > margin(max(theirs))):
                failures.append(f"two_songs_join_without_a_gap: {metric}: {mine}, released at most {max(theirs)}")
    return failures


def load_results():
    return [r for r in (json.load(open(os.path.join(RESULTS, n))) for n in sorted(os.listdir(RESULTS)) if n.endswith(".json")) if r.get("suite") == SUITE]


def released_metrics(released):
    out = []
    for r in released:
        for test, values in r["results"].items():
            for metric, value in values.items():
                if isinstance(value, (int, float)) and not isinstance(value, bool) and kind(metric) and (test, metric) not in out:
                    out.append((test, metric))
    return out


def allowed(released, test, metric):
    values = [r["results"].get(test, {}).get(metric) for r in released]
    values = [v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool)]
    worst = max(values)
    return worst + SLACK[kind(metric)], worst


# ---- the two scenarios that are not a whole run ----------------------------------------------

def component_log(path):
    out = []
    if os.path.exists(path):
        for line in open(path).read().splitlines():
            parts = line.split(" ", 2)
            if len(parts) >= 2:
                out.append((float(parts[0]), parts[1], parts[2] if len(parts) > 2 else ""))
    return out


def scenario_offline(build="new"):
    """Offline mode is a boundary, at a look's dispatch on the window's thread. On from the
    start: nothing dispatched, nothing asked. Off (a start with it off): looks dispatched and
    asked, each request one dispatch's. Switched on by hand while looks come every 0.2 s: no
    look dispatched after the switch is seen on the disk, and every request the proxy saw
    belongs to a dispatch (none without one). No grace period. Exits 1 if not."""
    out = os.path.join(RUNS, f"{build}-offline-scenario")
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    failures = []
    prepare_server()
    home = "scenario-offline"
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    shutil.copytree(KEPT, os.path.join(A.HOMES, home))
    settings = os.path.join(A.HOMES, home, "config", "settings.json")
    s = json.load(open(settings)); s["offline"] = True; json.dump(s, open(settings, "w"))
    inside = f"/work/target/walk/{home}"
    log = os.path.join(A.HOMES, home, "nexus.log")
    # (Looks 0.2 s apart here, so that one would come at once.)
    env = {"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_NEXUS_LOG": f"{inside}/nexus.log", "NEXUS_TEST_PROBE_AGAIN_MS": "200"}
    if os.environ.get("E13A_SABOTAGE"):
        env["NEXUS_TEST_SABOTAGE_NO_OFFLINE"] = "1"
    dispatches = lambda since=0.0: [t for t, word, _ in component_log(log) if word == "dispatch" and t > since]
    with front.Front("found", os.path.join(out, "front")) as f:
        copy = A.Copy(build, home, env=env)
        a = copy.launch()
        until(lambda: home_drawn(a), 60, 0.1, "Home")
        time.sleep(12)
        cold = (len(dispatches()), len(f.nexus_in_access_log()))
        if cold != (0, 0):
            failures.append(f"in Offline mode from the start: {cold[0]} dispatched, {cold[1]} asked")
        if not any(word == "offline" for _, word, _ in component_log(log)):
            failures.append("no look came due while Offline mode was on: nothing was tested")
        # (The released app does not show Offline mode's switch as on after a start in it,
        # and its press asks for "on" again: it cannot be switched off from that window. The
        # app is started again with it off. Another engineer is fixing that in the released
        # app; once that fix is in, this can press the switch instead.)
        copy.stop(keep=os.path.join(out, "app-output.txt"))
        s = json.load(open(settings)); s["offline"] = False; json.dump(s, open(settings, "w"))
        a = copy.launch()
        until(lambda: home_drawn(a), 60, 0.1, "Home")
        offline_now = lambda: json.load(open(copy.path("config", "settings.json"))).get("offline")
        try:
            until(lambda: len(dispatches(copy.started)) >= 3, 20, 0.1, "looks once Offline mode is off")
        except TimeoutError:
            failures.append("no looks at a start with Offline mode off")
        settings_tab(a, "Storage", "Offline mode")
        a.click(a.one(label="Offline mode", role="Switch")["handle"])
        until(lambda: offline_now() is True, 5, 0.02, "Offline mode on again")
        seen_on = time.time()
        time.sleep(8)
        after = dispatches(seen_on)
        if after:
            failures.append(f"{len(after)} looks dispatched after Offline mode was on")
        copy.stop(keep=os.path.join(out, "app-output.txt"))
        total_dispatched = len(dispatches())
        total_asked = len(f.nexus_in_access_log())
        if total_asked != total_dispatched:
            failures.append(f"{total_asked} requests for {total_dispatched} looks dispatched")
        if os.path.exists(log):
            shutil.copy(log, os.path.join(out, "nexus.log"))
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    print(f"offline: cold {cold}; dispatched {total_dispatched}, asked {total_asked}; after on: {len(after)}; failures: {failures or 'none'}")
    return 1 if failures else 0


def scenario_sign_out(build="new"):
    """A look that comes due as the account signs out, before the window's account-changed
    handler has run (made 0.3 s late, a test hook; looks 0.1 s apart, so that one is due in
    that time). Signed out and in again ten times: no look is dispatched for the account that
    has gone (the component says "not-ours" when it catches one), it is caught at least once,
    and every request the proxy saw belongs to a dispatch. Exits 1 if not."""
    out = os.path.join(RUNS, f"{build}-sign-out-scenario")
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    failures = []
    prepare_server()
    home = "scenario-sign-out"
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    shutil.copytree(KEPT, os.path.join(A.HOMES, home))
    inside = f"/work/target/walk/{home}"
    log = os.path.join(A.HOMES, home, "nexus.log")
    # The window's account-changed handler 0.3 s late (a test hook): looks come due between an
    # account's going and the handler every time, as they may by Slint's order of work.
    env = {"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_NEXUS_LOG": f"{inside}/nexus.log", "NEXUS_TEST_PROBE_AGAIN_MS": "100", "NEXUS_TEST_LATE_FOLLOW_MS": "300"}
    if os.environ.get("E13A_SABOTAGE"):
        env["NEXUS_TEST_SABOTAGE_NO_GENERATION"] = "1"
    caught = 0
    with front.Front("absent", os.path.join(out, "front")) as f:
        copy = A.Copy(build, home, env=env)
        a = copy.launch()
        until(lambda: home_drawn(a), 60, 0.1, "Home")
        until(lambda: any(word == "dispatch" for _, word, _ in component_log(log)), 20, 0.1, "looks")
        for cycle in range(10):
            settings_tab(a, "Account", "Sign out")
            a.press("Sign out")
            # (The inspection port answers once the window has handled the press: the account
            # has gone by now.)
            clicked = time.time()
            until(lambda: a.find(label="Connect"), 20, 0.02, "the sign-in form")
            form_seen = time.time()
            time.sleep(1)
            lines = component_log(log)
            # The generation of the account that went: the last published before the form.
            gone = [rest.split()[0] for t, word, rest in lines if word == "published" and t < form_seen and rest.split()[0] != "0"]
            gen = gone[-1] if gone else None
            # Looks for the gone account dispatched after it had gone.
            late = [t for t, word, rest in lines if word == "dispatch" and rest.split()[0] == gen and t > clicked]
            if late:
                failures.append(f"cycle {cycle}: {len(late)} looks dispatched for the account that signed out")
            caught += len([1 for t, word, rest in lines if word == "not-ours" and rest.split()[0] == gen])
            sign_in(a, "tester")
            until(lambda: home_drawn(a), 60, 0.05, "signed in again")
            # Looks coming again (five seconds after the account is published) before the next
            # sign-out.
            mark = time.time()
            until(lambda: any(word == "dispatch" and t > mark for t, word, _ in component_log(log)), 15, 0.1, "looks again")
            time.sleep(0.5)
        copy.stop(keep=os.path.join(out, "app-output.txt"))
        if not caught:
            failures.append("no look came due as the account went: the race was never caught, so nothing was shown")
        dispatched = len([1 for _, word, _ in component_log(log) if word == "dispatch"])
        asked = len(f.nexus_in_access_log())
        if asked > dispatched:
            failures.append(f"{asked} requests for {dispatched} looks dispatched")
        if os.path.exists(log):
            shutil.copy(log, os.path.join(out, "nexus.log"))
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    print(f"sign-out: {dispatched} dispatched, {asked} asked; a look due as the account went, caught {caught} times; failures: {failures or 'none'}")
    return 1 if failures else 0


def scenario_late_answer(build="new"):
    """A look for account A finds the backend; its answer is held until account B (at another
    origin, with none) is signed in; then let through. It must never be shown under B, and
    B's own look must say "None found". Exits 1 if not."""
    out = os.path.join(RUNS, f"{build}-late-answer-scenario")
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    failures = []
    prepare_server()
    home = "scenario-late"
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    shutil.copytree(KEPT, os.path.join(A.HOMES, home))
    inside = f"/work/target/walk/{home}"
    env = {"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_NEXUS_LOG": f"{inside}/nexus.log", "NEXUS_TEST_HOLD_NEXUS_RESULT": f"{inside}/release"}
    if os.environ.get("E13A_SABOTAGE"):
        env["NEXUS_TEST_SABOTAGE_NO_GENERATION"] = "1"
    release = os.path.join(A.HOMES, home, "release")
    with front.Front("found", os.path.join(out, "front")):
        copy = A.Copy(build, home, env=env)
        a = copy.launch()
        until(lambda: home_drawn(a), 60, 0.1, "Home")
        until(lambda: os.path.exists(release + ".waiting"), 30, 0.1, "A's answer found and held")
        settings_tab(a, "Account", "Sign out")
        a.press("Sign out")
        until(lambda: a.find(label="Connect"), 20, 0.05, "the sign-in form")
        sign_in(a, "guest", front.SERVER2)
        until(lambda: home_drawn(a), 60, 0.05, "B's first page")
        settings_tab(a, "Account", "Sign out")
        open(release, "w").write("")
        seen = set()
        end = time.time() + 15
        while time.time() < end:
            seen.update(l for l in labels(a) if l.startswith("Found at") or l == "None found")
            time.sleep(0.1)
        copy.stop(keep=os.path.join(out, "app-output.txt"))
        log = open(copy.path("nexus.log")).read() if os.path.exists(copy.path("nexus.log")) else ""
        open(os.path.join(out, "nexus.log"), "w").write(log)
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    if any(l.startswith("Found at") for l in seen):
        failures.append(f"A's answer was shown under B: {sorted(seen)}")
    if "None found" not in seen:
        failures.append(f"B's own row never said None found: {sorted(seen)}")
    if " dropped " not in log:
        failures.append("the component did not drop A's late answer")
    print(f"late answer: rows seen under B {sorted(seen)}; failures: {failures or 'none'}")
    return 1 if failures else 0


def inside(e, box):
    """Whether element `e` is drawn within `box` (both from the tree)."""
    ex, ey = e["absolutePosition"]["x"], e["absolutePosition"]["y"]
    bx, by = box["absolutePosition"]["x"], box["absolutePosition"]["y"]
    return bx <= ex <= bx + box["size"]["width"] and by <= ey <= by + box["size"]["height"]


def nav(a, label):
    """A page of the sidebar's, scrolling the sidebar back up to it if need be."""
    for _ in range(4):
        found = [e for e in a.find(type="NavItem") if e["accessibleLabel"] == label or (label == "Downloads" and e["accessibleLabel"].startswith("Downloading"))]
        if found:
            a.click(found[0]["handle"])
            return
        some = a.find(type="PlaylistItem") or a.find(type="NavItem")
        a.tool("scroll_element", elementHandle=some[0]["handle"], deltaY=1000)
        time.sleep(0.5)
    raise RuntimeError(f"no {label} in the sidebar")


def play_downloaded(a, title=None):
    """A downloaded song, double-clicked on the Downloads page (the first, or `title`)."""
    close_settings(a)
    nav(a, "Downloads")
    rows = until(lambda: a.find(role="ListItem", type="TrackRow"), 20, 0.2, "the downloads listed")
    if title:
        for _ in range(10):
            rows = [r for r in a.find(role="ListItem", type="TrackRow") if r["accessibleLabel"].split(", ")[0] == title]
            if rows:
                break
            a.tool("scroll_element", elementHandle=a.find(role="ListItem", type="TrackRow")[0]["handle"], deltaY=-400)
            time.sleep(0.5)
    a.click(rows[0]["handle"], action="DoubleClick")


def album_play(a, album):
    """An album's own Play button on the Albums page (alphabetical: these are near its top)."""
    close_settings(a)
    nav(a, "Albums")
    until(lambda: a.find(label=f"Play {album}", type="PlayButton"), 20, 0.2, f"{album} on Albums")
    a.click(a.find(label=f"Play {album}", type="PlayButton")[0]["handle"])


def offline_shown(a):
    """The app says it can't reach the server (offline mode by hand says "Offline mode")."""
    return "Can't reach server" in labels(a)


def retry_downloads(a):
    nav(a, "Downloads")
    time.sleep(1)
    for e in a.tree():
        l = e.get("accessibleLabel") or ""
        if e.get("accessibleRole") == "Button" and l.lower().startswith(("try again", "retry")):
            a.click(e["handle"])
            return


def normalised(lines, pressed):
    """The lines a play added, each with its time written as seconds after Play was pressed
    (rounded): the only part of a line that can differ between two runs of the same plays."""
    out = []
    for line in lines.decode().splitlines():
        record = json.loads(line)
        record[1] = round(record[1] - pressed)
        out.append(json.dumps(record, separators=(",", ":"), ensure_ascii=False))
    return out


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:16]


# ---- the numbers --------------------------------------------------------------------------

# How much slower than the released app's slowest a W1 time may be, and why (NUMBERS.md says
# it again): what the measurement itself can vary by.
#  - "ui": seen by reading the window's element tree, which takes 20-60 ms a read: 0.25 s.
#  - "file": seen in a file, every 20-50 ms; the app's own work is a few ms: 0.25 s.
#  - "timer": waits on one of the app's timers or the server (a count at half the song, the
#    reconnect every 15 s, a retry): 1.0 s, a tenth or less of the wait itself.
#  - "gap": a stall in the sound, sampled every 2 ms against 20 ms ticks of the null output:
#    one tick, 0.02 s.
KINDS = {
    "launch_to_home_drawn": "ui", "launch_to_queue_shown": "ui", "restart_to_queue_shown": "ui",
    "connect_to_home_drawn": "ui", "play_to_sound": "file", "space_to_sound": "file",
    "play_to_counted": "timer", "counted_to_server_has_play": "timer", "like_to_server": "file",
    "server_back_to_play_told_again": "timer", "launch_to_list_scheduled_its_own": "timer",
    "by_hand_8_songs": "file", "server_back_to_downloads_done": "timer",
    "add_to_kept": "file", "move_to_kept": "file", "remove_to_kept": "file", "clear_to_kept": "file",
    "by_hand_switch_to_offline": "file", "by_hand_switch_back": "file", "offline_play_downloaded_to_sound": "file",
    "by_loss_noticed": "ui", "by_loss_play_downloaded_to_sound": "file", "server_back_to_online": "timer",
    "continue_here_to_kept": "file", "offered_after_another_device_saved": "cadence",
    "slowest_api_answer_during_the_actions": "file",
}
# Every run must have measured each of these: one missing is a failure (`table`).
REQUIRED = [m for m in KINDS] + [f"{k}_{m}" for k in ("streamed", "cached", "downloaded") for m in ("play_to_sound", "longest_stall_around_the_join")]
# The runs `table` requires: each mode of both builds at least twice, and the component's
# hooks, before it says anything.
MODES_REQUIRED = ("absent", "refused", "blackhole", "found", "stalled", "saturated", "ended", "half")
# "cadence": the sync's look at the server's saved queue, every 15 s with the window open.
SLACK = {"ui": 0.25, "file": 0.25, "timer": 1.0, "gap": 0.02, "cadence": 15.0}


def kind(metric):
    if metric.endswith("_longest_stall_around_the_join"):
        return "gap"
    if metric.endswith("_play_to_sound"):
        return "file"
    return KINDS.get(metric)


def table():
    runs = [r for r in load_results() if r.get("hook") != "sabotage"]
    old = [r for r in runs if r["build"] == "old"]
    new = [r for r in runs if r["build"] == "new"]
    metrics = []
    for r in runs:
        for test, values in r["results"].items():
            for metric, value in values.items():
                if isinstance(value, (int, float)) and not isinstance(value, bool) and kind(metric) and (test, metric) not in metrics:
                    metrics.append((test, metric))
    lines = ["| Test | Measure | Released: each run | Released: slowest | Allowed (slack) | W1: each run | W1: slowest | Within |", "|---|---|---|---|---|---|---|---|"]
    verdicts = []
    for test, metric in metrics:
        get = lambda rs: [r["results"].get(test, {}).get(metric) for r in rs]
        before = [v for v in get(old) if isinstance(v, (int, float))]
        after = [v for v in get(new) if isinstance(v, (int, float))]
        if not before:
            continue
        worst = max(before)
        slack = SLACK[kind(metric)]
        allowed = worst + slack
        ok = all(v <= allowed for v in after) if after else None
        verdicts.append(ok)
        lines.append(f"| `{test}` | {metric} | {', '.join(f'{v:.3f}' for v in before)} | {worst:.3f} | {allowed:.3f} ({kind(metric)}) | "
                     f"{', '.join(f'{v:.3f}' for v in after) or '-'} | {max(after) if after else float('nan'):.3f} | {'yes' if ok else ('-' if ok is None else '**no**')} |")
    facts = ["", "| Run | Load at start, end | Failed | Row for tester / guest | Requests under /nexus/ | Longest stretch of an action with no look waited on (s) |", "|---|---|---|---|---|---|"]
    for r in runs:
        res = r["results"]
        nexus = [v for k, v in res.get("backend", {}).items() if k.startswith("requests_under_nexus")]
        facts.append(f"| {r['run']} | {r.get('machine_load_at_start_and_end')} | {len(r['failures'])} | "
                     f"{res.get('settings_row', {}).get('shown for tester')} / {res.get('settings_row', {}).get('shown for guest')} | "
                     f"{nexus[0] if nexus else '-'} | {res.get('backend', {}).get('longest_time_no_look_was_waited_on_during_an_action', '-')} |")
    gaps = ["", "| Run | " + " | ".join(f"{k} silent frames, largest step" for k in ("streamed", "cached", "downloaded")) + " |", "|---|---|---|---|"]
    for r in runs:
        res = r["results"].get("two_songs_join_without_a_gap", {})
        gaps.append(f"| {r['run']} | " + " | ".join(f"{res.get(k + '_silent_frames_at_the_join')}, {res.get(k + '_largest_step_at_the_join')}" for k in ("streamed", "cached", "downloaded")) + " |")
    # The whole of each run's plays.log at its end, with each time left out: the same plays,
    # the same bytes otherwise, in every run of either build.
    logs = {}
    for r in runs:
        path = os.path.join(RUNS, r["run"], "plays.log")
        if os.path.exists(path):
            lines_ = [json.loads(l) for l in open(path, encoding="utf-8") if l.strip()]
            shape = json.dumps([[l[0]] + l[2:] for l in lines_]) + f" {[len(l) for l in open(path, 'rb').read().split(b'\n')]}"
            logs.setdefault(shape, []).append(r["run"])
    plays = ["", "| plays.log at the end of the run, times left out (song, source; bytes per line) | Runs |", "|---|---|"]
    plays += [f"| `{shape}` | {len(names)}: {', '.join(names)} |" for shape, names in logs.items()]
    text = "\n".join(lines + facts + gaps + plays) + "\n"
    within = all(v for v in verdicts if v is not None)
    failed = [r["run"] for r in runs if r["failures"]]
    # Missing evidence is a failure: too few runs, or a measure a run lacks.
    missing = []
    for mode in MODES_REQUIRED:
        for build in ("old", "new"):
            n = len([r for r in runs if r["build"] == build and r["mode"] == mode and not r.get("hook")])
            if n < 2:
                missing.append(f"{build} {mode}: {n} runs of 2")
    for mode, hook in (("absent", "held"), ("stalled", "held"), ("absent", "switch-off")):
        n = len([r for r in runs if r["mode"] == mode and r.get("hook") == hook])
        if n < 2:
            missing.append(f"new {mode} {hook}: {n} runs of 2")
    for r in runs:
        have = {m for values in r["results"].values() for m, v in values.items() if isinstance(v, (int, float)) and not isinstance(v, bool)}
        lacking = [m for m in REQUIRED if m not in have]
        if lacking:
            missing.append(f"{r['run']} lacks {lacking}")
    text += "\n## Missing evidence\n\n" + ("\n".join(f"- {m}" for m in missing) or "None.") + "\n"
    failed += [f"missing: {m}" for m in missing]
    compared = {r["run"]: compare(r["results"], old) for r in new if r.get("hook") != "switch-off"}
    text += "\n## Each W1 run against every released run\n\n" + "\n".join(
        f"- {run}: " + ("as released" if not why else "; ".join(why)) for run, why in compared.items()) + "\n"
    failed += [run for run, why in compared.items() if why]
    print(text)
    print("all within:", within, "| runs with a failure:", failed or "none", "| W1 runs:", len(new), "| released runs:", len(old))
    return 0 if within and not failed and old and new else 1


if __name__ == "__main__":
    what = sys.argv[1]
    if what == "populate":
        populate()
    elif what == "table":
        sys.exit(table())
    elif what == "run":
        sys.exit(Run(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else None).go())
    elif what == "offline":
        sys.exit(scenario_offline(sys.argv[2] if len(sys.argv) > 2 else "new"))
    elif what == "sign-out":
        sys.exit(scenario_sign_out(sys.argv[2] if len(sys.argv) > 2 else "new"))
    elif what == "late-answer":
        sys.exit(scenario_late_answer(sys.argv[2] if len(sys.argv) > 2 else "new"))
    else:
        raise SystemExit(__doc__)
