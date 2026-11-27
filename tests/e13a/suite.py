"""The tests of design 5.3 for the Windows app (experiment E13a, then slice W1's proof).

Each test drives the app through a real entry point while the backend's side, at the music
server's own origin under /nexus/, is broken one way (front.py's modes), and records how long
the app took. Run first against the released app ("old") to fix the numbers; then against the
app with the Nexus server's component ("new"), with the component's own workers held and its
disk blocked as well.

    suite.py populate                       the released app makes the profile every run starts from
    suite.py run <old|new> <mode> [hook]    one run of every test; hook: held, disk-hang, disk-full
    suite.py table                          every run's numbers, side by side (NUMBERS.md)

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
# Where the design gives the component files of its own (5.1). W1 keeps none; the disk hooks
# block this folder, and the run checks it was never touched.
NEXUS_FILES = ("data", "nexus")


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
    for attempt in range(6):
        if any(l == shows for l in labels(a)):
            return
        found = [e for e in a.find(label=tab) if any(n.get("typeName") == "Chip" for n in e.get("typeNamesAndIds", []))] or a.find(label=tab, role="Text")
        a.click(found[-1]["handle"])
        time.sleep(1)
    raise RuntimeError(f"the {tab} tab didn't open")


def close_settings(a):
    if a.find(label="Close settings", role="Button"):
        a.press("Close settings")
        time.sleep(0.8)


def sign_in(a, user):
    for label, text in (("Server address", front.SERVER), ("Username", user), ("Password", ACCOUNTS[user])):
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

HOOKS = {
    None: {},
    # Every worker of the component held in a call that never returns (a test hook of the
    # component's own, read only by a debug build).
    "held": {"env": {"NEXUS_TEST_HOLD_NEXUS": "1"}},
    # The component's folder is a named pipe: opening anything in it for writing hangs.
    "disk-hang": {"fifo": True},
    # The component's folder is a full file system of its own.
    "disk-full": {"tmpfs": True},
}


class Run:
    def __init__(self, build, mode, hook=None):
        self.build, self.mode, self.hook = build, mode, hook
        self.label = f"{build}-{mode}" + (f"-{hook}" if hook else "") + (f"-{os.environ['E13A_REPEAT']}" if os.environ.get("E13A_REPEAT") else "")
        self.out = os.path.join(RUNS, self.label)
        self.results, self.notes, self.failures = {}, [], []

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

    def step(self, test, body):
        self.say(f"\n{test}")
        self.music_down = False
        try:
            body()
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

    def go(self):
        shutil.rmtree(self.out, ignore_errors=True)
        os.makedirs(self.out)
        self.say(f"run {self.label} at {time.strftime('%Y-%m-%d %H:%M:%S')}")
        self.load = [os.getloadavg()[0]]
        self.by = prepare_server()
        home = f"run-{self.label}"
        shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
        shutil.copytree(KEPT, os.path.join(A.HOMES, home))
        hook = HOOKS[self.hook]
        mounts = []
        nexus_dir = os.path.join(A.HOMES, home, *NEXUS_FILES)
        if hook.get("fifo"):
            os.mkfifo(nexus_dir)
        if hook.get("tmpfs"):
            mounts.append(f"type=tmpfs,destination=/work/target/walk/{home}/{'/'.join(NEXUS_FILES)},tmpfs-size=4096")
        self.log_bytes = open(os.path.join(A.HOMES, home, "config", "plays.log"), "rb").read()
        self.copy = A.Copy(self.build, home, env=hook.get("env"), mounts=mounts)
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
        self.disk_untouched(nexus_dir, hook)
        self.save()
        # The profile's files are kept beside the run's log; the songs and the sound are not.
        for name in ("plays.log", "queue.json", "settings.json"):
            src = os.path.join(A.HOMES, home, "config", name)
            if os.path.exists(src):
                shutil.copy(src, os.path.join(self.out, name))
        shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)

    # -- launch 1: a cold start on the kept profile --------------------------------------------

    def first_launch(self):
        c = self.copy
        self.dump = Dump(c.dump)
        a = c.launch()
        self.fill_disk()
        t0 = c.started

        def cold_start():
            until(lambda: home_drawn(a), 60, 0.02, "the first page")
            self.time("cold_start_publishes_the_saved_account", "launch_to_home_drawn", time.time() - t0)
        self.step("cold_start_publishes_the_saved_account", cold_start)

        def queue_back():
            until(lambda: a.find(label="Runout Groove", type="LinkText") and a.find(label="Play"), 60, 0.02, "the queue put back")
            self.time("cold_start_puts_the_queue_back_and_plays", "launch_to_queue_shown", time.time() - t0)
            self.server_plays_before = plays_of(self.by[FIRST]["id"])
            pressed = time.time()
            a.press("Play")
            self.pressed_play = pressed
            sound = self.dump.sound_after(pressed)
            self.time("cold_start_puts_the_queue_back_and_plays", "play_to_sound", sound - pressed)
        self.step("cold_start_puts_the_queue_back_and_plays", queue_back)

        def counted():
            log = c.path("config", "plays.log")
            line = until(lambda: open(log, "rb").read()[len(self.log_bytes):], 60, 0.02, "the play in the log")
            self.counted_at = time.time()
            self.time("a_play_is_counted_as_released", "play_to_counted", self.counted_at - self.pressed_play)
            now = open(log, "rb").read()
            self.fact("a_play_is_counted_as_released", "start_up_left_the_log_as_it_was", now.startswith(self.log_bytes))
            self.fact("a_play_is_counted_as_released", "lines_added", now[len(self.log_bytes):].count(b"\n"))
            self.fact("a_play_is_counted_as_released", "line_with_time_as_offset", normalised(now[len(self.log_bytes):], self.pressed_play))
        self.step("a_play_is_counted_as_released", counted)

        intro = self.by[FIRST]["id"]

        def scrobbled():
            until(lambda: plays_of(intro) > self.server_plays_before, 60, 0.1, "the play on the server")
            self.time("a_like_and_a_play_reach_the_music_server", "counted_to_server_has_play", time.time() - self.counted_at)
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
            # Paused, so that the queue moves only by the edits.
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

    # -- launch 2: after kill -9 -------------------------------------------------------------

    def second_launch(self):
        c = self.copy
        a = c.launch()
        self.fill_disk()
        t0 = c.started

        def put_back():
            test = "the_queue_is_changed_and_kept"
            until(lambda: a.find(label="Play") and a.find(type="QueueRow"), 60, 0.02, "the queue after kill -9")
            self.time(test, "restart_to_queue_shown", time.time() - t0)
            self.fact(test, "queue_after_kill_is_the_queue_before", saved_queue(c) == getattr(self, "kept_queue", None))
        self.step("the_queue_is_changed_and_kept", put_back)

        def direct():
            test = "a_direct_play_starts_it_cold"
            pressed = time.time()
            a.key(" ")
            sound = self.dump.sound_after(pressed, 30)
            self.time(test, "space_to_sound", sound - pressed)
            a.key(" ")
        self.step("a_direct_play_starts_it_cold", direct)

        def clear():
            test = "the_queue_is_changed_and_kept"
            length = len(saved_queue(c)[0])
            pressed = time.time()
            a.click(a.one(label="Clear", type="LinkText")["handle"])
            until(lambda: len((saved_queue(c) or [[0] * length])[0]) < length, 20, 0.02, "the queue cleared")
            self.time(test, "clear_to_kept", time.time() - pressed)
        self.step("the_queue_is_changed_and_kept", clear)

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
            # A page not seen yet in this run: its load finds the server gone.
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

        self.settings_row(a)

        def account():
            test = "an_account_change_is_not_held_up"
            settings_tab(a, "Account", "Sign out")
            a.press("Sign out")
            until(lambda: a.find(label="Connect"), 20, 0.05, "the sign-in form")
            pressed = sign_in(a, "guest")
            until(lambda: home_drawn(a), 60, 0.02, "the new account's first page")
            self.time(test, "connect_to_home_drawn", time.time() - pressed)
            self.fact(test, "signed_in_as", json.load(open(c.path("config", "settings.json"))).get("username"))
        self.step("an_account_change_is_not_held_up", account)
        self.settings_row(a, after="account change")

    # -- what the component showed and asked -------------------------------------------------

    def settings_row(self, a, after=""):
        try:
            settings_tab(a, "Account", "Sign out")
            found = [l for l in labels(a) if l.startswith("Found at") or l == "None found"]
            self.fact("settings_row", "shown" + (f" ({after})" if after else ""), found or "no row")
            # The component's threads alive now (a held one never ends; a let-go one ends
            # when its call returns).
            self.fact("backend", "its_threads" + (f" ({after})" if after else ""), self.copy.pid_threads().count("nexus"))
            close_settings(a)
        except Exception as error:
            self.fact("settings_row", "error", str(error))

    def backend_seen(self):
        asked = self.front.asked()
        seen = self.front.nexus_in_access_log()
        self.fact("backend", "requests_under_nexus_at_the_proxy", len(seen))
        self.fact("backend", "requests_at_the_stand_in", [(r["method"], r["target"], r["agent"].split("/")[0]) for r in asked][:10])
        with open(os.path.join(self.out, "nexus-requests.txt"), "w") as f:
            f.writelines(seen)

    def fill_disk(self):
        """For disk-full: the component's file system filled to the last byte."""
        if self.hook == "disk-full":
            where = f"/work/target/walk/{self.copy.home}/{'/'.join(NEXUS_FILES)}"
            A.sh(f"docker exec {A.NAME} sh -c 'cat /dev/zero > {where}/filler 2>/dev/null; df {where} | tail -1'")

    def disk_untouched(self, nexus_dir, hook):
        if hook.get("fifo"):
            # Nothing was ever written into the pipe, and it is still a pipe.
            import stat
            self.fact("backend", "its_folder_untouched", stat.S_ISFIFO(os.lstat(nexus_dir).st_mode))

    def save(self):
        os.makedirs(RESULTS, exist_ok=True)
        self.load.append(os.getloadavg()[0])
        record = {"run": self.label, "machine_load_at_start_and_end": [round(x, 2) for x in self.load], "build": self.build, "mode": self.mode, "hook": self.hook,
                  "at": time.strftime("%Y-%m-%d %H:%M:%S"), "binary": digest(os.path.join(A.HOMES, f"nexus-music-{self.build}")),
                  "results": self.results, "failures": self.failures}
        json.dump(record, open(os.path.join(RESULTS, f"{self.label}.json"), "w"), indent=1, ensure_ascii=False)
        self.say(f"\n{len(self.failures)} failed" + ("".join(f"\n  {f}" for f in self.failures)))


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
}
SLACK = {"ui": 0.25, "file": 0.25, "timer": 1.0, "gap": 0.02}


def kind(metric):
    if metric.endswith("_longest_stall_around_the_join"):
        return "gap"
    if metric.endswith("_play_to_sound"):
        return "file"
    return KINDS.get(metric)


def table():
    runs = [json.load(open(os.path.join(RESULTS, name))) for name in sorted(os.listdir(RESULTS)) if name.endswith(".json")]
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
    print("all within:", all(v for v in verdicts if v is not None), "| W1 runs:", len(new), "| released runs:", len(old))
    return text


if __name__ == "__main__":
    what = sys.argv[1]
    if what == "populate":
        populate()
    elif what == "table":
        table()
    elif what == "run":
        Run(sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else None).go()
    else:
        raise SystemExit(__doc__)
