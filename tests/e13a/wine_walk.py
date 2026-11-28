#!/usr/bin/env python3
"""The Windows build under Wine, signed in, closed and started again (slice W7b, the review's
MUST 1): what the sign-in keeps in Windows' Credential Manager (Wine's) reads back, and
nothing of a password is in any file of the profile.

    tests/e13a/wine_walk.py <windows exe> [--image nexus-music-w7b-wine:local] [--keep <dir>]
                            [--origin https://music.example.org:4681] [--real]

Against the E13a front (front.py "found": the stand-in backend at /nexus/, the test
Navidrome "w1" behind it), or with `--real` against whatever answers at `--origin` (the real
backend behind the same front). The exe is an xwin debug build with SLINT_EMIT_DEBUG_INFO=1
and `--features slint/mcp`. Wine's own copy of Credential Manager lives in the run's Wine
prefix, apart from the profile (as Windows keeps it apart from the app's folders).

Each step asserts what it saw; any failure exits 1."""
import argparse, json, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import front as F
from drive import App

PORT = 29581
PASSWORDS = ("nexus-pass-tester", "nexus-pass-guest", "tester-pass-1", "guest-pass-1")


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)


def until(test, seconds, what, every=0.3):
    end = time.time() + seconds
    while time.time() < end:
        got = test()
        if got:
            return got
        time.sleep(every)
    raise TimeoutError(f"{what} not seen in {seconds} s")


class Wine:
    def __init__(self, home, exe, image, port=PORT, env=None):
        self.name = f"nexus-w7b-wine-{port}"
        self.home, self.exe, self.image, self.port, self.env = home, os.path.abspath(exe), image, port, env or {}
        self.log = os.path.join(home, f"{self.name}.log")

    def __enter__(self):
        sh(f"docker rm -f {self.name}")
        # The run's certificate authority, beside the system's, where Wine reads the
        # system's trusted authorities from.
        bundle = os.path.join(self.home, "ca-certificates.crt")
        with open(bundle, "w") as out:
            out.write(open("/etc/ssl/certs/ca-certificates.crt").read())
            out.write(open(os.path.join(F.CERTS, "music-self.pem")).read())
        env = {"HOME": "/walk", "WINEPREFIX": "/walk/wine", "WINEARCH": "win64", "WINEDEBUG": "-all",
               "WINEDLLOVERRIDES": "mscoree,mshtml=;winemenubuilder.exe=d", "NEXUS_MUSIC_HOME": r"Z:\walk\profile",
               "SLINT_BACKEND": "headless-skia", "SLINT_MCP_PORT": str(self.port), "NEXUS_WINDOW_SIZE": "1143x746",
               "NEXUS_TRACE": "1", "NEXUS_AUDIO_DUMP": r"Z:\walk\audio.raw", "NEXUS_TEST_NEXUS_LOG": r"Z:\walk\nexus.log", **self.env}
        args = ["docker", "run", "--rm", "--init", "--network", "host", "--name", self.name, "--user", f"{os.getuid()}:{os.getgid()}",
                "--add-host", f"{F.HOST}:127.0.0.1", "--add-host", f"{F.HOST2}:127.0.0.1",
                "-v", f"{self.home}:/walk", "-v", f"{self.exe}:/app/nexus-music.exe:ro", "-v", f"{bundle}:/etc/ssl/certs/ca-certificates.crt:ro"]
        for k, v in env.items():
            args += ["-e", f"{k}={v}"]
        # When the app has ended, Wine's server is waited for: it writes the registry (and so
        # Credential Manager) to its files as it ends.
        args += [self.image, "bash", "-c", "xvfb-run -a /usr/lib/wine/wine64 /app/nexus-music.exe; /usr/lib/wine/wineserver -w"]
        self.out = open(self.log, "a")
        self.process = subprocess.Popen(args, stdout=self.out, stderr=self.out)
        self.app = App(self.port).wait_up(180)
        return self.app

    def __exit__(self, *exc):
        # The app is ended as a crash or a power cut ends it (nothing of its own runs), and
        # Wine then shuts down as a PC does: Wine keeps Credential Manager in its registry
        # and writes that to its files when its server ends (Windows keeps each write at
        # once). The container is not killed: that would lose Wine's registry, not the app's.
        sh(f"docker exec {self.name} /usr/lib/wine/wine64 taskkill /F /IM nexus-music.exe")
        try:
            self.process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            sh(f"docker stop -t 5 {self.name}")
            self.process.wait(timeout=30)
        self.out.close()


def signed_in(a):
    """The app proper: its sidebar's pages (the released app's labels, as E13a reads them)."""
    labels = [e.get("accessibleLabel") or "" for e in a.tree()]
    return any(label.startswith("Good ") for label in labels) or "Albums" in labels


def sign_in_form(a):
    return a.find(label="Nexus server", role="TextInput")


def play_something(a, home):
    audio = os.path.join(home, "audio.raw")
    before = os.path.getsize(audio) if os.path.exists(audio) else 0
    a.click(until(lambda: a.find(label="Albums", role="Button"), 30, "the Albums button")[0]["handle"])
    play = until(lambda: a.find(type="PlayButton"), 40, "an album's Play button")
    a.click(play[0]["handle"])
    until(lambda: os.path.exists(audio) and os.path.getsize(audio) > before + 400_000, 60, "audio played")
    return os.path.getsize(audio) - before


def files_holding_passwords(root):
    found = []
    for base, _, names in os.walk(root):
        for name in names:
            path = os.path.join(base, name)
            try:
                if os.path.getsize(path) < 64 << 20:
                    data = open(path, "rb").read()
                    for word in PASSWORDS:
                        if word.encode() in data or word.encode("utf-16-le") in data:
                            found.append((os.path.relpath(path, root), word))
            except OSError:
                pass
    return found


def walk(exe, image, keep, origin, real):
    home = keep or tempfile.mkdtemp(prefix="nexus-w7b-wine-")
    os.makedirs(home, exist_ok=True)
    for name in ("profile", "wine", "audio.raw", "nexus.log"):
        path = os.path.join(home, name)
        if os.path.isdir(path):
            shutil.rmtree(path)
        elif os.path.exists(path):
            os.remove(path)
    os.makedirs(os.path.join(home, "profile"))
    steps = []

    def step(what):
        steps.append(what)
        print(f"{time.strftime('%H:%M:%S')} {what}", flush=True)

    with Wine(home, exe, image) as a:
        form = until(lambda: sign_in_form(a), 120, "the sign-in form")
        step("first start: the sign-in form")
        a.set(form[0]["handle"], origin)
        a.set(a.one(label="Email", role="TextInput")["handle"], "tester@example.org")
        a.set(a.one(label="Password", role="TextInput")["handle"], "nexus-pass-tester")
        a.press("Sign in")
        try:
            until(lambda: signed_in(a) and not sign_in_form(a), 60, "signed in")
        except TimeoutError:
            a.shot(os.path.join(home, "not-signed-in.png"))
            print([e.get("accessibleLabel") for e in a.tree() if e.get("accessibleLabel")][:80])
            raise
        step("signed in through the Nexus server")
        # W7c item 10: the server's notice stays over the window until OK is pressed.
        notice = until(lambda: a.find(label="What your Nexus server records", role="Text"), 30, "the server's notice")
        time.sleep(4)
        assert a.find(label="What your Nexus server records", role="Text"), "the notice went by itself"
        a.shot(os.path.join(home, "notice.png"))
        a.press("OK")
        until(lambda: not a.find(label="What your Nexus server records", role="Text"), 10, "the notice gone after OK")
        time.sleep(3)
        step("the server's notice shown until OK was pressed")
        if real:
            played = play_something(a, home)
            step(f"playing: {played} bytes of audio")
        a.shot(os.path.join(home, "first-run.png"))
    step("closed")
    reg = open(os.path.join(home, "wine", "user.reg"), errors="replace").read()
    assert "Credential Manager" in reg or "Credentials" in reg, "Wine's Credential Manager holds nothing"
    kept = sorted({line.split("Generic: ")[1].split("]")[0] for line in reg.splitlines() if "Generic: secret:nexus" in line})
    assert any("nexus-sign-in" in name for name in kept), kept
    step(f"Wine's Credential Manager holds the sign-in: {', '.join(kept)}")

    with Wine(home, exe, image) as a:
        until(lambda: signed_in(a), 120, "signed in again with nothing typed")
        assert not sign_in_form(a), "the sign-in form came back"
        handed = until(lambda: "music login handed" in open(os.path.join(home, "nexus.log"), errors="replace").read().split("client started")[-1], 30, "the kept login handed to the player")
        step("second start: signed in, nothing typed")
        time.sleep(3)
        assert not a.find(label="What your Nexus server records", role="Text"), "the notice seen came back"
        step("second start: the notice seen is not shown again")
        a.click(until(lambda: a.find(label="Settings", role="Button"), 30, "Settings")[0]["handle"])
        time.sleep(2)
        if real:
            played = play_something(a, home)
            step(f"playing again: {played} bytes of audio")
        a.shot(os.path.join(home, "second-run.png"))
    step("closed")
    found = files_holding_passwords(os.path.join(home, "profile"))
    assert not found, f"a password in the profile's files: {found}"
    step("no password (Nexus or music server's, UTF-8 or UTF-16) in any file of the profile")
    if not real:
        asked = [json.loads(line) for line in open(os.path.join(F.STATE, "w7b-wine", "standin.jsonl")) if line.strip()]
        signins = [e for e in asked if "signin" in e.get("target", "")]
        assert len(signins) == 1, f"signed in {len(signins)} times"
        step("the backend was asked to sign in once, in the first run alone")
    log = open(os.path.join(home, "nexus.log"), errors="replace").read()
    print(log[-1500:])
    print("PASSED:", "; ".join(steps))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("exe")
    parser.add_argument("--image", default="nexus-music-w7b-wine:local")
    parser.add_argument("--keep")
    parser.add_argument("--origin", default=F.SERVER)
    parser.add_argument("--real", action="store_true")
    args = parser.parse_args()
    if args.real:
        walk(args.exe, args.image, args.keep, args.origin, True)
    else:
        os.environ["FRONT_SELF_SIGNED"] = "1"
        # Under Wine, Windows' TLS loses a record that a read cuts in two (see front.py's
        # WINE_RECORDS): the sign-in's answer is kept to one read (a short notice). The music
        # server's answers are larger than that, so under Wine nothing plays over HTTPS:
        # the walk shows the sign-in kept and read back, not the music. (Real Windows reads
        # both; the released app plays over HTTPS there.)
        os.environ["STANDIN_NOTICE"] = "Recorded."
        F.SELF = True
        for name in ("standin.jsonl", "standin2.jsonl"):
            if os.path.exists(os.path.join(F.STATE, "w7b-wine", name)):
                os.remove(os.path.join(F.STATE, "w7b-wine", name))
        with F.Front("found", os.path.join(F.STATE, "w7b-wine")):
            walk(args.exe, args.image, args.keep, args.origin, False)
