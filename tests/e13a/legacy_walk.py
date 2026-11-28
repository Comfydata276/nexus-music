#!/usr/bin/env python3
"""W7c item 6: updating from the released app loses nothing before sign-in.

This build is started on a copy of the kept released profile (made by the released app,
signed in to the test server, with a saved queue, Recently played and downloads), stays at
the sign-in form, and quits as its own Quit does (the development build's
`NEXUS_TEST_QUIT_FILE`), so that what it writes at its end is walked too. Then every file of
the released queue, its place, Recently played and the downloads folder must be byte for
byte as they were; the released saved password must be gone (the app deletes it, unread);
and nothing else of the profile's may have gone. Started twice, to walk a second start too.

    legacy_walk.py            exits 1 if anything is not as it must be
"""
import hashlib, os, shutil, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import app as A
import suite as S

HOME = "w7c-legacy"
OUT = os.path.expanduser("~/.cache/nexus-music-android/shots/nexus-w7/w7c-legacy")


def digests(root):
    found = {}
    for folder, _, files in os.walk(root):
        for name in files:
            path = os.path.join(folder, name)
            found[os.path.relpath(path, root)] = hashlib.sha256(open(path, "rb").read()).hexdigest()
    return found


def main():
    os.makedirs(OUT, exist_ok=True)
    home = os.path.join(A.HOMES, HOME)
    shutil.rmtree(home, ignore_errors=True)
    shutil.copytree(S.KEPT, home, symlinks=True)
    before = digests(home)
    owned = [name for name in before if name.startswith("data/downloads") or name in ("config/queue.json", "config/queue-place.json", "config/history.json")]
    failures = []
    if "config/queue.json" not in before or not any(name.startswith("data/downloads/") for name in before):
        failures.append("the kept profile has no saved queue or no downloads: nothing would be walked")
    quit_file = os.path.join(home, "quit-now")
    for start in (1, 2):
        copy = A.Copy("new", HOME, env={"NEXUS_MUSIC_NEXUS": "1", "NEXUS_TEST_QUIT_FILE": f"/work/target/walk/{HOME}/quit-now", "NEXUS_WINDOW_SIZE": "820x560"})
        a = copy.launch()
        a.wait_for(seconds=60, label="Sign in", role="Button")
        a.shot(os.path.join(OUT, f"start-{start}-sign-in-form-820x560.png"))
        time.sleep(3)
        open(quit_file, "w").write("")
        end = time.time() + 30
        while time.time() < end and "true" in A.sh(f"docker inspect -f '{{{{.State.Running}}}}' {A.NAME}").stdout:
            time.sleep(0.5)
        quit_cleanly = "true" not in A.sh(f"docker inspect -f '{{{{.State.Running}}}}' {A.NAME}").stdout
        copy.stop(keep=os.path.join(OUT, "app-output.txt"))
        os.remove(quit_file)
        if not quit_cleanly:
            failures.append(f"start {start}: the app did not quit by itself")
        after = digests(home)
        for name in owned:
            if after.get(name) != before[name]:
                failures.append(f"start {start}: {name} {'changed' if name in after else 'gone'}")
        new_downloads = [name for name in after if name.startswith("data/downloads") and name not in before]
        if new_downloads:
            failures.append(f"start {start}: written into the downloads folder: {new_downloads}")
        gone = [name for name in before if name not in after and name != "config/credential"]
        if gone:
            failures.append(f"start {start}: gone from the profile: {gone}")
        if "config/credential" in after:
            failures.append(f"start {start}: the released saved password is still there")
        print(f"start {start}: {len(owned)} files of the released queue, history and downloads compared; quit by itself: {quit_cleanly}")
    for line in failures:
        print("FAILED", line)
    print("PASSED" if not failures else f"{len(failures)} FAILED")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
