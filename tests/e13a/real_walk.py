#!/usr/bin/env python3
"""Slice W7b: the Windows app against the REAL Nexus server, on this machine only.

The backend is the build branch's server (`server/`, built into target/debug), run in the
dev container on 127.0.0.1:4684 behind the E13a front (front.py "refused": /nexus/ goes to
4684, everything else to the test Navidrome "w1" on 4680), at https://music.example.org:4681.
Nothing is sent to any address off this machine. Made-up accounts only.

    real_walk.py setup        folders, the admin and two people, their one-time codes
    real_walk.py serve        the backend, detached (docker: nexus-w7b-backend)
    real_walk.py music EMAIL PASSWORD [MUSIC_USER MUSIC_PASSWORD]
                              the admin's tool gives EMAIL's account a music login
    real_walk.py stop         the backend stopped
    real_walk.py log          the backend's log

The app copies are the Linux development build (target/walk/nexus-music-new), driven through
Slint's inspection port by the helpers below (`Copy2`), several at once."""
import json, os, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import front as F
import app as A
from drive import App

WORK = os.path.abspath(os.path.join(HERE, "..", ".."))
DEV = os.path.join(WORK, "target", "nexus-dev")
STATE = os.path.expanduser("~/.cache/nexus-music-android/shots/nexus-w7/real")
BACKEND = "nexus-w7b-backend"
PORT = 4684
ORIGIN = F.SERVER
ADMIN, ADMIN_PASSWORD = "admin@example.org", "admin-pass-for-the-w7b-walk"
PEOPLE = {"tester@example.org": "tester-nexus-pass-15", "guest@example.org": "guest-nexus-pass-15"}


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)


def env_args(extra=""):
    return (f"--add-host {F.HOST}:127.0.0.1 -e NEXUS_MUSIC_SERVER=http://127.0.0.1:{F.NAVIDROME} -e NEXUS_PROXY=127.0.0.1 "
            f"-e NEXUS_DATA=/work/target/nexus-dev/data -e NEXUS_STATE=/work/target/nexus-dev/state -e NEXUS_LISTEN=127.0.0.1:{PORT} "
            f"-e NEXUS_PUBLIC_ORIGIN={ORIGIN} {extra}")


def program(*args):
    out = sh(f'cd {WORK} && DX_DOCKER_ARGS="{env_args()}" scripts/dx target/debug/nexus-music-server ' + " ".join(args))
    return out.stdout + out.stderr


def code_of(text):
    import re
    found = re.search(r"\b([A-Z0-9]{6}-[A-Z0-9]{6})\b", text)
    if not found:
        raise RuntimeError(text)
    return found.group(1)


def setup():
    os.makedirs(STATE, exist_ok=True)
    sh(f"rm -rf {DEV}")
    os.makedirs(os.path.join(DEV, "data"))
    os.makedirs(os.path.join(DEV, "state"))
    codes = {ADMIN: code_of(program("set-up", ADMIN, '"The admin"'))}
    for email in PEOPLE:
        codes[email] = code_of(program("recover", email))
    json.dump(codes, open(os.path.join(STATE, "codes.json"), "w"))
    print(json.dumps(codes, indent=1))


def serve():
    sh(f"docker rm -f {BACKEND}")
    out = sh(f'cd {WORK} && DX_DOCKER_ARGS="-d --name {BACKEND} {env_args()}" scripts/dx target/debug/nexus-music-server')
    print(out.stdout[-200:], out.stderr[-400:])
    time.sleep(3)
    print(sh(f"docker logs {BACKEND} 2>&1 | tail -5").stdout)


def music(email, password, user="tester", music_password="tester-pass-1"):
    codes = json.load(open(os.path.join(STATE, "codes.json")))
    first = not os.path.exists(os.path.join(STATE, "admin-redeemed"))
    extra = (f"-e NEXUS_S9_BACKEND=127.0.0.1:{PORT} -e NEXUS_S9_ORIGIN={ORIGIN} -e NEXUS_S9_ADMIN={ADMIN} "
             f"-e NEXUS_S9_ADMIN_PASSWORD={ADMIN_PASSWORD} -e NEXUS_S9_PUBLIC={ORIGIN} -e NEXUS_S9_MUSIC_NAME={user} "
             f"-e NEXUS_S9_MUSIC_PASSWORD={music_password} -e NEXUS_S9_PERSON={email} -e NEXUS_S9_PERSON_PASSWORD={password}")
    if first:
        extra += f" -e NEXUS_S9_ADMIN_CODE={codes[ADMIN]}"
    out = sh(f'cd {WORK} && DX_DOCKER_ARGS="{extra}" scripts/dx cargo test --locked --manifest-path server/Cargo.toml --test music -- --ignored --nocapture set_a_music_login_on_a_running_backend')
    text = out.stdout + out.stderr
    print("\n".join(line for line in text.splitlines() if "200" in line or "test result" in line or "panicked" in line or "error" in line.lower())[-1500:])
    if "test result: ok" in text:
        open(os.path.join(STATE, "admin-redeemed"), "w").write("")


class Copy2(A.Copy):
    """A copy of the app of its own name and inspection port, so that two run at once."""

    def __init__(self, home, n, env=None):
        super().__init__("new", home, env=env)
        self.n = n

    def launch(self, wait=120):
        A.NAME, A.MCP = f"nexus-w7b-app{self.n}", 9490 + self.n
        app = super().launch(wait)
        return app

    def stop(self, kill=False, keep=None):
        A.NAME = f"nexus-w7b-app{self.n}"
        super().stop(kill, keep)


def until(test, seconds, what, every=0.3):
    end = time.time() + seconds
    while time.time() < end:
        got = test()
        if got:
            return got
        time.sleep(every)
    raise TimeoutError(f"{what} not seen in {seconds} s")


if __name__ == "__main__":
    command = sys.argv[1]
    if command == "setup":
        setup()
    elif command == "serve":
        serve()
    elif command == "music":
        music(*sys.argv[2:])
    elif command == "stop":
        print(sh(f"docker stop -t 5 {BACKEND}").stdout)
    elif command == "log":
        print(sh(f"docker logs {BACKEND} 2>&1 | tail -60").stdout)
