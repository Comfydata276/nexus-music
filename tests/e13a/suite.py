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
    for label, text in (("Server address", server or front.SERVER), ("Username", user), ("Password", ACCOUNTS[user])):
        a.set(a.one(label=label, role="TextInput")["handle"], text)
    pressed = time.time()
    a.press("Connect")
    return pressed


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

SUITE = 2   # results of an earlier form of the suite are not compared with
# The stand-in's modes in which a look is held up (it waits, and ends only at its deadline):
# in these the component is made to look again 0.2 s after each look ends, so that one is
# being waited on through every action, and the run shows it was.
STUCK = ("stalled", "blackhole", "saturated")
# What else the component (the "new" build) is put through, beside the stand-in's mode.
HOOKS = {
    None: "",
    # Its look's thread held in a call that never returns, from its first look on.
    "held": "",
    # Not switched on: nothing of it may be made.
    "switch-off": "",
    # A deliberate fault, which the run must catch and FAIL on: the window's thread held
    # 0.3 s at each look (shorter than the 1 s allowed for timers).
    "sabotage": "",
}


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
        self.enforce = False   # holds are checked from the component's first look of a start
        self.music_down = False

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

    def step(self, test, body):
        """One action: the component's hold verified live before it, the action, and the
        same hold verified again when it has completed."""
        self.say(f"\n{test}")
        self.music_down = False
        try:
            checked = self.enforce
            before = self.hold(test, "before")
            began = time.time()
            body()
            if checked:
                self.windows.append((test, began, time.time()))
            after = self.hold(test, "after")
            if self.held and self.enforce and before != after:
                self.fail(test, f"the hold before ({before}) is not the hold after ({after})")
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

    # -- the component's side, live ------------------------------------------------------------

    def nexus_log(self):
        """What the component did (NEXUS_TEST_NEXUS_LOG): (time, word, rest) a line each."""
        try:
            lines = open(self.copy.path("nexus.log")).read().splitlines()
        except OSError:
            return []
        return [(float(t), word, rest) for t, word, *more in (l.split(" ", 2) + [""] for l in lines) for rest in [more[0] if more else ""]]

    def in_flight(self):
        """A look the component is waiting on right now."""
        started = ended = 0
        for _, word, _ in self.nexus_log():
            started += word == "start"
            ended += word == "end"
        return started > ended

    def hold(self, test, when):
        """Verifies, live, that the component's side is held up now: its look's thread held
        (`held`), or a look being waited on (the stuck modes). Returns what it saw."""
        if not self.enforce:
            return None
        if self.held:
            marker = self.copy.path("held")
            if not os.path.exists(marker):
                raise Failed(f"{when}: no held look")
            thread = open(marker).read().strip()
            alive = A.sh(f"docker exec {A.NAME} test -d /proc/{thread}").returncode == 0
            if not alive:
                raise Failed(f"{when}: the held look's thread {thread} is gone")
            return thread
        if self.stuck:
            try:
                until(self.in_flight, 3, 0.05, "a look being waited on")
            except TimeoutError:
                raise Failed(f"{when}: no look was being waited on")
            return "in flight"
        return None

    def wait_for_first_look(self, launched):
        """From the component's first look of this start on, its holds are checked. Its first
        look must come after the account's first page and its queue are usable."""
        if not (self.held or self.stuck):
            return
        if self.held:
            until(lambda: os.path.exists(self.copy.path("held")), 20, 0.05, "the held look")
        else:
            until(self.in_flight, 20, 0.05, "the first look")
        first = min((t for t, word, _ in self.nexus_log() if word == "start" and t > self.copy.started), default=None)
        if self.held:
            first = os.path.getmtime(self.copy.path("held"))
        usable = getattr(self, "usable_at", None)
        self.expect("ordering", f"first_look_after_home_and_queue ({launched})", first is not None and usable is not None and first > usable, f"{first} > {usable}")
        self.enforce = True

    def coverage(self):
        """In the stuck modes: for every action from the first look on, a look was being
        waited on throughout, with no gap longer than 1 s between two looks."""
        if not self.stuck:
            return
        looks, opened = [], None
        for t, word, _ in self.nexus_log():
            if word == "start":
                opened = t
            elif word == "end" and opened is not None:
                looks.append((opened, t))
                opened = None
        if opened is not None:
            looks.append((opened, time.time()))
        worst = 0.0
        for test, t0, t1 in self.windows:
            if t1 < (looks[0][0] if looks else 0) or test == "ordering":
                continue
            covered, at = [l for l in looks if l[1] >= t0 and l[0] <= t1], t0
            gap = 0.0
            for start, end in covered:
                gap = max(gap, start - at)
                at = max(at, end)
            gap = max(gap, t1 - at)
            worst = max(worst, gap)
            if gap > 1.0:
                self.fail(test, f"no look was waited on for {gap:.2f} s of it")
        self.fact("backend", "longest_time_no_look_was_waited_on_during_an_action", round(worst, 3))

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
        inside = f"/work/target/walk/{home}"
        env = {}
        if self.on:
            env.update({"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_NEXUS_LOG": f"{inside}/nexus.log"})
        if self.new:
            env["NEXUS_TEST_NEXUS_LOG"] = f"{inside}/nexus.log"
        if self.stuck or self.hook == "sabotage":
            env["NEXUS_TEST_PROBE_AGAIN_MS"] = "200"
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
        self.coverage()
        self.gapless_sources()
        self.compare_with_released()
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
        a = c.launch()
        t0 = c.started

        def cold_start():
            until(lambda: home_drawn(a), 60, 0.02, "the first page")
            self.time("cold_start_publishes_the_saved_account", "launch_to_home_drawn", time.time() - t0)
        self.step("cold_start_publishes_the_saved_account", cold_start)

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
        self.wait_for_first_look("first start")

        def counted():
            log = c.path("config", "plays.log")
            until(lambda: open(log, "rb").read()[len(self.log_before):], 60, 0.02, "the play in the log")
            self.counted_at = time.time()
            self.time("a_play_is_counted_as_released", "play_to_counted", self.counted_at - self.pressed_play)
            now = open(log, "rb").read()
            self.expect("a_play_is_counted_as_released", "the_log_was_only_added_to", now.startswith(self.log_before))
            self.fact("a_play_is_counted_as_released", "added_with_times_masked (first start)", masked(now[len(self.log_before):]))
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
        self.log_before = open(c.path("config", "plays.log"), "rb").read()
        c.stop(kill=True, keep=self.keep)
        # A look the kill cut short ended with the process.
        if self.on and self.in_flight():
            with open(c.path("nexus.log"), "a") as log:
                log.write(f"{time.time():.3f} end killed\n")
        # (The held look's marker is the process's: the next start holds a look of its own.)
        if os.path.exists(c.path("held")):
            os.remove(c.path("held"))
        self.enforce = False

    # -- launch 2: after kill -9 ---------------------------------------------------------------

    def second_launch(self):
        c = self.copy
        a = c.launch()
        t0 = c.started

        def put_back():
            test = "the_queue_is_changed_and_kept"
            until(lambda: a.find(label="Play") and a.find(type="QueueRow"), 60, 0.02, "the queue after kill -9")
            self.time(test, "restart_to_queue_shown", time.time() - t0)
            self.usable_at = time.time()
            self.log_unchanged("second start")
            self.expect(test, "queue_after_kill_is_the_queue_before", saved_queue(c) == getattr(self, "kept_queue", None), saved_queue(c))
        self.step("the_queue_is_changed_and_kept", put_back)
        self.wait_for_first_look("second start")

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

        def joins():
            test = "two_songs_join_without_a_gap"
            for kind, start in (("streamed", lambda: album_play(a, "Continuous Sweep (MP3)")),
                                ("cached", lambda: album_play(a, "Continuous Sweep (MP3)")),
                                # (Its songs are the ones "Gapless sweep" has, downloaded.)
                                ("downloaded", lambda: album_play(a, "Continuous Sweep"))):
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
        self.step("two_songs_join_without_a_gap", joins)

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
            until(lambda: a.find(label="Connect"), 20, 0.05, "the sign-in form")
            # Another account, at another origin of the same music server (nothing under
            # /nexus/ there).
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
        if self.on and not self.held:
            # (The first look is 5 s after an account is published.)
            try:
                until(lambda: any(l.startswith("Found at") or l == "None found" for l in labels(a)), 20, 0.5, "the row")
            except TimeoutError:
                pass
        shown = [l for l in labels(a) if l.startswith("Found at") or l == "None found"]
        heading = "Nexus server" in labels(a)
        if not self.on:
            want = []
        elif self.held:
            want = []
        elif who == "tester" and self.mode == "found":
            want = [f"Found at {front.SERVER}/nexus/"]
        else:
            want = ["None found"]
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
        count = len(seen)
        test = "backend"
        if self.hook == "sabotage":
            pass
        elif not self.on or self.held:
            # The released app, the component switched off, or its look held before it asks.
            self.expect(test, "requests_under_nexus", count == 0, count)
        elif self.stuck:
            self.expect(test, "requests_under_nexus (looks kept going)", count >= 3, count)
        else:
            # One at each start (5 s after the account is published) and one for the guest.
            self.expect(test, "requests_under_nexus (one per account published)", count == 3, count)
        for line in seen:
            ok = '"GET /nexus/api/hello HTTP/1.1"' in line and "NexusMusic/" in line
            if not ok:
                self.fail(test, f"a request that is not a plain GET of hello: {line.strip()}")
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

    def compare_with_released(self):
        """For the new build: every measure the released app has, within its slowest plus the
        slack; every outcome as the released app's."""
        if not self.new:
            return
        released = [r for r in load_results() if r["build"] == "old"]
        if not released:
            self.fail("compare", "no released runs to compare with")
            return
        for test, metric in released_metrics(released):
            limit, worst = allowed(released, test, metric)
            value = self.results.get(test, {}).get(metric)
            if not isinstance(value, (int, float)) or isinstance(value, bool):
                self.fail(test, f"{metric}: not measured")
            elif value > limit:
                self.fail(test, f"{metric}: {value:.3f} s, over {limit:.3f} (released slowest {worst:.3f})")
        for metric in ("added_with_times_masked (first start)", "added_with_times_masked (second start)"):
            theirs = {json.dumps(r["results"].get("a_play_is_counted_as_released", {}).get(metric)) for r in released}
            mine = json.dumps(self.results.get("a_play_is_counted_as_released", {}).get(metric))
            if mine not in theirs:
                self.fail("a_play_is_counted_as_released", f"{metric}: {mine} is not what the released app wrote ({theirs})")
        for kind in ("streamed", "cached", "downloaded"):
            # Silent frames exactly as few; the largest step within a tenth of the released
            # app's largest (it moves with where the 20 ms tick falls on the join).
            for metric, margin in ((f"{kind}_silent_frames_at_the_join", lambda v: v), (f"{kind}_largest_step_at_the_join", lambda v: v * 1.1)):
                theirs = [r["results"].get("two_songs_join_without_a_gap", {}).get(metric) for r in released]
                theirs = [v for v in theirs if isinstance(v, (int, float))]
                mine = self.results.get("two_songs_join_without_a_gap", {}).get(metric)
                if theirs and (not isinstance(mine, (int, float)) or mine > margin(max(theirs))):
                    self.fail("two_songs_join_without_a_gap", f"{metric}: {mine}, released at most {max(theirs)}")

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

def scenario_offline(build="new"):
    """Offline mode on from the start: no look while it is on; looks once a start has it off;
    none again once it is switched back on. Exits 1 if not."""
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
    # (Its looks come a second apart here, so that one would show at once.)
    env = {"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_NEXUS_LOG": f"{inside}/nexus.log", "NEXUS_TEST_PROBE_AGAIN_MS": "1000"}
    if os.environ.get("E13A_SABOTAGE"):
        env["NEXUS_TEST_SABOTAGE_NO_OFFLINE"] = "1"
    with front.Front("found", os.path.join(out, "front")) as f:
        copy = A.Copy(build, home, env=env)
        a = copy.launch()
        until(lambda: home_drawn(a), 60, 0.1, "Home")
        time.sleep(12)
        first = len(f.nexus_in_access_log())
        if first:
            failures.append(f"{first} requests while Offline mode was on from the start")
        # (The released app does not show Offline mode's switch as on after a start in it,
        # and its press asks for "on" again, which changes nothing: it cannot be switched off
        # from that window. So the app is stopped, and started again with it off.)
        copy.stop(keep=os.path.join(out, "app-output.txt"))
        s = json.load(open(settings)); s["offline"] = False; json.dump(s, open(settings, "w"))
        a = copy.launch()
        until(lambda: home_drawn(a), 60, 0.1, "Home")
        offline_now = lambda: json.load(open(copy.path("config", "settings.json"))).get("offline")
        try:
            until(lambda: len(f.nexus_in_access_log()) > 0, 15, 0.2, "a look once Offline mode is off")
        except TimeoutError:
            failures.append("no look within 15 s of a start with Offline mode off")
        settings_tab(a, "Storage", "Offline mode")
        a.click(a.one(label="Offline mode", role="Switch")["handle"])
        until(lambda: offline_now() is True, 5, 0.1, "Offline mode on again")
        time.sleep(1.5)   # (a look already started may finish)
        on_again = len(f.nexus_in_access_log())
        time.sleep(8)
        later = len(f.nexus_in_access_log())
        if later != on_again:
            failures.append(f"{later - on_again} requests after Offline mode went back on")
        copy.stop(keep=os.path.join(out, "app-output.txt"))
        shutil.copy(copy.path("nexus.log"), os.path.join(out, "nexus.log")) if os.path.exists(copy.path("nexus.log")) else None
    shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
    print(f"offline: {first} while on from the start; looks after off; {later - on_again} after on again; failures: {failures or 'none'}")
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
}
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
    facts = ["", "| Run | Load at start, end | Failed | Log line (time as seconds after Play) | Queue kept through kill -9 | Settings row | Under /nexus/ at the proxy | At the stand-in |", "|---|---|---|---|---|---|---|---|"]
    for r in runs:
        res = r["results"]
        get = lambda t, m: res.get(t, {}).get(m)
        stand = get("backend", "requests_at_the_stand_in") or []
        facts.append(f"| {r['run']} | {r.get('machine_load_at_start_and_end')} | {len(r['failures'])} | "
                     f"{get('a_play_is_counted_as_released', 'line_with_time_as_offset')} | {get('the_queue_is_changed_and_kept', 'queue_after_kill_is_the_queue_before')} | "
                     f"{get('settings_row', 'shown')} / {get('settings_row', 'shown (account change)')} | {get('backend', 'requests_under_nexus_at_the_proxy')} | "
                     f"{len(stand)}: {sorted(set(tuple(x) for x in stand))} |")
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
    print(text)
    within = all(v for v in verdicts if v is not None)
    failed = [r["run"] for r in runs if r["failures"]]
    print("all within:", within, "| runs with a failure:", failed or "none", "| W1 runs:", len(new), "| released runs:", len(old))
    return 0 if within and not failed else 1


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
    elif what == "late-answer":
        sys.exit(scenario_late_answer(sys.argv[2] if len(sys.argv) > 2 else "new"))
    else:
        raise SystemExit(__doc__)
