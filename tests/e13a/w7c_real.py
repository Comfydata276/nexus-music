#!/usr/bin/env python3
"""W7c: the Windows app (its Linux development build) against the REAL Nexus server on this
machine (real_walk.py's backend behind the E13a front, the test Navidrome "w1"); nothing is
sent off this machine, and the accounts are made up.

    w7c_real.py            setup, serve, then the walk; exits 1 on any failure

The walk: a first sign-in with a one-time code (the notice of what the server records is
shown until OK; the no-music screen; captures at 820x560 and, for a second account, at
1143x746); the admin sets the music login; Check now; playing. A second copy signs in with
the password and forces the first out from its Devices list; the first learns it at its next
call and shows the sign-in form. The second signs out with Offline on (the note kept), then
turns Offline off from the sign-in form (the note told, and forgotten). No password typed
here is in any file of either profile."""
import os, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import front as F
import real_walk as R
import suite as S
from real_walk import Copy2, until

SHOTS = os.path.expanduser("~/.cache/nexus-music-android/shots/nexus-w7")
OUT = os.path.join(SHOTS, "w7c-real")
TESTER, GUEST = "tester@example.org", "guest@example.org"
failures, steps = [], []


def step(what):
    steps.append(what)
    print(f"{time.strftime('%H:%M:%S')} {what}", flush=True)


def check(ok, what):
    if not ok:
        failures.append(what)
        print(f"{time.strftime('%H:%M:%S')} FAILED {what}", flush=True)
    return ok


def copy(home, n, size):
    import shutil
    shutil.rmtree(os.path.join(R.A.HOMES, home), ignore_errors=True)
    c = Copy2(home, n, env={"NEXUS_MUSIC_NEXUS": "1", "NEXUS_WINDOW_SIZE": size, "NEXUS_TEST_NEXUS_LOG": f"/work/target/walk/{home}/nexus.log"})
    return c, c.launch()


def log_of(c):
    try:
        return open(c.path("nexus.log"), errors="replace").read()
    except OSError:
        return ""


def first_sign_in(a, code, password):
    a.click(a.one(label="I have a one-time code", wait=60)["handle"])
    time.sleep(0.5)
    a.type_into("Nexus server", R.ORIGIN)
    a.type_into("One-time code", code)
    a.type_into("Choose a password", password)
    a.type_into("The same again", password)
    a.press("Sign in")


def notice_then_no_music(a, size):
    shown = until(lambda: S.labels(a) if ("What your Nexus server records" in S.labels(a) or "You're signed in" in S.labels(a)) else None, 60, "an answer to the sign-in")
    if "What your Nexus server records" in shown:
        a.shot(os.path.join(SHOTS, f"w7c-notice-{size}.png"))
        time.sleep(4)
        check("What your Nexus server records" in S.labels(a), "the notice went by itself")
        a.press("OK")
        step(f"the notice shown until OK ({size})")
    until(lambda: "You're signed in" in S.labels(a), 30, "the no-music screen")
    time.sleep(1)
    check("Your admin hasn't set your music sign-in yet. Ask them to set it on the Nexus server's admin page." in S.labels(a), "the no-music words")
    a.shot(os.path.join(SHOTS, f"w7c-no-music-{size}.png"))
    step(f"the no-music screen ({size})")


def devices(a):
    S.settings_tab(a, "Account", "Your devices")
    a.press("Show" if a.find(label="Show", role="Button") else "Refresh")


def main():
    os.makedirs(OUT, exist_ok=True)
    R.setup()
    R.serve()
    import json
    codes = json.load(open(os.path.join(R.STATE, "codes.json")))
    with F.Front("refused", os.path.join(OUT, "front")):
        # A: the first sign-in with a one-time code, no music login yet.
        ca, a = copy("w7c-real-a", 1, "820x560")
        first_sign_in(a, codes[TESTER], R.PEOPLE[TESTER])
        notice_then_no_music(a, "820x560")
        # A second account at the other size, for the no-music screen there.
        cc, c = copy("w7c-real-c", 3, "1143x746")
        first_sign_in(c, codes[GUEST], R.PEOPLE[GUEST])
        notice_then_no_music(c, "1143x746")
        cc.stop(keep=os.path.join(OUT, "app-c.txt"))
        # The admin sets the tester's music login; Check now; playing.
        R.music(TESTER, R.PEOPLE[TESTER])
        a.press("Check now")
        until(lambda: S.home_drawn(a), 60, "the player after Check now")
        step("the music login set by the admin: Check now, the player")
        audio = ca.path("audio.raw")
        before = os.path.getsize(audio) if os.path.exists(audio) else 0
        try:
            a.click(until(lambda: a.find(label="Albums", role="Button"), 30, "the Albums button")[0]["handle"])
            a.click(until(lambda: a.find(type="PlayButton"), 40, "an album's Play button")[0]["handle"])
            until(lambda: os.path.exists(audio) and os.path.getsize(audio) > before + 400_000, 60, "audio played")
            step(f"playing on A: {os.path.getsize(audio) - before} bytes of audio")
        except (TimeoutError, RuntimeError) as error:
            check(False, f"playing on A: {error}")
        # B: the password, on a second profile.
        cb, b = copy("w7c-real-b", 2, "1143x746")
        b.type_into("Nexus server", R.ORIGIN)
        b.type_into("Email", TESTER)
        b.type_into("Password", R.PEOPLE[TESTER])
        b.press("Sign in")
        until(lambda: S.home_drawn(b), 60, "B signed in with the password")
        S.see_notice(b, wait=2)
        step("B signed in with the password")
        # B forces A out; A learns it at its next call carrying the session (a press).
        devices(b)
        rows = until(lambda: [e for e in b.find(role="Button", label="Sign out") if e], 30, "the device rows")
        time.sleep(2)
        buttons = b.find(role="Button", label="Sign out")
        check(len(buttons) >= 2, f"A's row has its Sign out ({len(buttons)} Sign out buttons)")
        b.click(buttons[-1]["handle"])
        time.sleep(3)
        step("B signed A out from its Devices list")
        devices(a)
        try:
            until(lambda: S.sign_in_form(a), 30, "A's sign-in form")
            a.shot(os.path.join(SHOTS, "w7c-forced-out-820x560.png"))
            step(f"A forced out: {[l for l in S.labels(a) if 'signed out' in l.lower()][:1]}")
        except TimeoutError as error:
            check(False, f"A forced out: {error}")
        # B signs out with Offline on: the note kept; Offline off from the form: told.
        S.settings_tab(b, "Storage", "Offline mode")
        b.click(b.one(label="Offline mode", role="Switch")["handle"])
        time.sleep(1)
        S.settings_tab(b, "Account", "Sign out")
        b.press("Sign out")
        until(lambda: S.sign_in_form(b), 30, "B's sign-in form")
        until(lambda: "sign-out note kept" in log_of(cb), 20, "the note kept")
        check("Offline mode is on." in S.labels(b), "the form says Offline mode is on")
        b.shot(os.path.join(SHOTS, "w7c-sign-in-offline-1143x746.png"))
        b.press("Turn off Offline mode")
        try:
            until(lambda: "sign-out note forgotten" in log_of(cb), 30, "the note told and forgotten")
            step("B signed out while Offline: the note kept, told the moment Offline went off from the form, forgotten")
        except TimeoutError as error:
            check(False, str(error))
        for c2 in (ca, cb):
            c2.stop(keep=os.path.join(OUT, "apps.txt"))
        for home in ("w7c-real-a", "w7c-real-b", "w7c-real-c"):
            found = []
            for base, _, names in os.walk(os.path.join(R.A.HOMES, home)):
                for name in names:
                    data = open(os.path.join(base, name), "rb").read() if os.path.getsize(os.path.join(base, name)) < 64 << 20 else b""
                    for word in list(R.PEOPLE.values()) + list(codes.values()) + ["tester-pass-1"]:
                        if word.encode() in data or word.encode("utf-16-le") in data:
                            found.append((name, word[:4]))
            check(not found, f"{home}: a password or code on disk: {found}")
        step("no password or one-time code typed here is in any file of the three profiles")
    print(R.sh(f"docker logs {R.BACKEND} 2>&1 | tail -40").stdout, file=open(os.path.join(OUT, "backend.txt"), "w"))
    R.sh(f"docker stop -t 5 {R.BACKEND}")
    print("\n".join(steps))
    print("PASSED" if not failures else f"FAILED: {failures}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
