"""Compare W8 with the released source tag using the kept file-walk profile.

Build both headless Slint MCP binaries through build-turn. Run this from the repo root
with the kept profile and both paths. It uses only copies and a loopback Subsonic server.
"""
import argparse
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import shutil
import wave

sys.path.insert(0, str(Path.cwd() / "tests/e13a"))
from drive import App
from suite import settings_tab

PLAYLIST = "W8 fictional set"
PLAYLIST_ID = "w8-set"
FORGOTTEN_SEARCH = "pale enamel suitcase"
FRAME_BYTES = 8
PCM_SECOND = 48000 * FRAME_BYTES
SONGS = [{
    "id": song_id, "title": title, "artist": "Paper Astronauts", "album": "Tin Weather",
    "duration": length, "suffix": "wav", "contentType": "audio/wav", "track": track,
} for track, (song_id, title, length) in enumerate([
    ("w8-a", "The Unpainted Boat", 6),
    ("w8-b", "Cardboard Lanterns", 20),
], 1)]


def wav(length, pitch):
    stream = io.BytesIO()
    with wave.open(stream, "wb") as file:
        file.setparams((2, 2, 48000, 0, "NONE", ""))
        frame = struct.Struct("<hh")
        period = b"".join(frame.pack(8192 if tick < pitch / 2 else -8192, -2048)
                          for tick in range(pitch))
        file.writeframes((period * (48000 * length // pitch + 1))[:48000 * length * 4])
    return stream.getvalue()


AUDIO = {song["id"]: wav(song["duration"], 240 // song["track"]) for song in SONGS}


class Music(BaseHTTPRequestHandler):
    def do_GET(self):
        from urllib.parse import parse_qs, urlsplit

        url = urlsplit(self.path)
        method = url.path.rsplit("/", 1)[-1].removesuffix(".view")
        query = parse_qs(url.query)
        playlist = {"id": PLAYLIST_ID, "name": PLAYLIST, "songCount": len(SONGS)}
        if method == "stream":
            data = AUDIO.get(query.get("id", [""])[0], b"")
            if not data:
                return self.send_error(404)
            match = self.headers.get("Range", "").removeprefix("bytes=").split("-", 1)
            if len(match) == 2 and match[0].isdigit():
                length = len(data)
                start = int(match[0])
                end = min(length - 1, int(match[1])) if match[1].isdigit() else length - 1
                if start > end:
                    return self.send_error(416)
                data = data[start:end + 1]
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{length}")
            else:
                self.send_response(200)
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Type", "audio/wav")
        else:
            data = json.dumps({"subsonic-response": {
                "status": "ok", "version": "1.16.1", "serverVersion": "0.60.2",
                "artists": {"index": []}, "albumList2": {"album": []}, "song": SONGS[0],
                "playlists": {"playlist": [playlist]}, "playlist": playlist | {"entry": list(reversed(SONGS))},
                "searchResult3": {"artist": [], "album": [], "song": []},
            }}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_):
        pass


def until(check, description, timeout=30):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if value := check():
            return value
        time.sleep(0.15)
    raise AssertionError(f"timed out waiting for {description}")


def prepare(source, target, port):
    shutil.copytree(source, target)
    path = target / "config/settings.json"
    server = f"http://127.0.0.1:{port}"
    settings = json.loads(path.read_text()) | {
        "server": server, "username": "fictional-listener", "check_updates": False,
        "requests": False, "sync": False, "autoplay": False, "cache_limit_mb": 0,
        "gapless": True, "crossfade": 0.0, "bitrate": 0, "repeat": 0, "shuffle": False,
        "scrobble": False, "said_no_links": True, "offline": False,
    }
    path.write_text(json.dumps(settings))
    (target / "config/credential").write_text(
        f"Nexus Music\nfictional-listener@{server}\nfictional-only\n"
    )


def comparison_clock(temporary):
    # Both logs should have identical bytes, including the UTC timestamp. Leave
    # monotonic time alone so that playback and the runtime measurements use real time.
    source = temporary / "comparison-clock.c"
    source.write_text("""
#define _GNU_SOURCE
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

int clock_gettime(clockid_t clock_id, struct timespec *value) {
    if (clock_id == CLOCK_REALTIME) {
        *value = (struct timespec){ .tv_sec = 1795824000, .tv_nsec = 0 };
        return 0;
    }
    return syscall(SYS_clock_gettime, clock_id, value);
}
""")
    library = temporary / "comparison-clock.so"
    subprocess.run(["cc", "-shared", "-fPIC", "-o", str(library), str(source)], check=True)
    return library


class Player:
    def __init__(self, home, port, recording, binary, clock):
        self.home = home
        self.port = port
        self.recording = recording
        self.binary = "/work/" + str(binary.resolve().relative_to(Path.cwd()))
        self.clock = clock
        self.name = f"nexus-w8-walk-{port}"
        self.log = home / "app.log"

    def __enter__(self):
        args = [
            "--name", self.name, "-v", f"{self.home}:/profile",
            "-e", "NEXUS_MUSIC_HOME=/profile", "-e", "SLINT_BACKEND=headless-skia",
            "-e", f"SLINT_MCP_PORT={self.port}", "-e", "NEXUS_WINDOW_SIZE=1143x746",
            "-e", "NEXUS_AUDIO_DUMP=/profile/audio.pcm", "-e", "NEXUS_TRACE=1",
            "-v", f"{self.clock}:/comparison-clock.so:ro", "-e", "LD_PRELOAD=/comparison-clock.so",
        ]
        if self.recording:
            args += ["-e", "NEXUS_TEST_LISTENING_JOURNAL=1"]
        with self.log.open("w") as output:
            self.process = subprocess.Popen(["scripts/dx", self.binary],
                env=os.environ | {"DX_DOCKER_ARGS": " ".join(args)}, stdout=output, stderr=output)
        try:
            self.app = App(self.port).wait_up(40)
            self.app.wait_for(label="Settings", role="Button")
            return self.app
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def kill(self):
        if self.process.poll() is None:
            subprocess.run(["docker", "kill", "-s", "KILL", self.name], capture_output=True,
                           check=True, timeout=20)
            assert self.process.wait(timeout=20) == 137

    def __exit__(self, kind, *_):
        try:
            if self.process.poll() is None:
                subprocess.run(["docker", "stop", "-t", "2", self.name], capture_output=True, timeout=20)
                self.process.wait(timeout=20)
        finally:
            if kind:
                print(self.log.read_text()[-4000:], file=sys.stderr)
            assert not any("listening-journal" in line for line in self.log.read_text().splitlines()
                           if "flushed on the window's thread" in line)


def journal_files(home):
    return sorted((home / "data/listening-journal").glob("*/*/*.journal"))


def rows(home):
    found = []
    for file in journal_files(home):
        data = file.read_bytes()
        lines = data.splitlines(keepends=True)
        header = json.loads(lines[0])
        assert header == {"format": 1, "life": 1}, header
        found += [json.loads(line) for line in lines[1:] if line.endswith(b"\n")]
    return found


def event(home, kind, **fields):
    for row in rows(home):
        if row["event"]["kind"] == kind and all(row["event"].get(key) == value for key, value in fields.items()):
            return row


def play(app):
    playlist = app.one(label=PLAYLIST, type="PlaylistItem")
    app.click(playlist["handle"])
    tracks = until(lambda: app.find(role="ListItem", type="TrackRow"), "the playlist's tracks")
    assert tracks[0]["accessibleLabel"].startswith(SONGS[0]["title"]), [
        track["accessibleLabel"] for track in tracks
    ]
    app.click(tracks[0]["handle"], action="DoubleClick")


def pcm_at_least(home, size):
    path = home / "audio.pcm"
    return path.is_file() and path.stat().st_size >= size


def flushes(log):
    lines = [line for line in log.read_text().splitlines() if "flushed on the window's thread" in line]
    assert any("plays.log" in line for line in lines), "the trace must include the legacy play-log flush"
    durations = [float(line.split(" in ", 1)[1].split(" ms:", 1)[0]) for line in lines]
    return len(lines), max(durations, default=0)


def walk(profile, port, temporary, baseline, candidate):
    clock = comparison_clock(temporary)
    kept = temporary / "kept-by-released"
    prepare(profile, kept, port)
    with Player(kept, 29484, False, baseline, clock):
        assert (kept / "config/plays.log").read_bytes() == (profile / "config/plays.log").read_bytes()
    unrecorded = temporary / "released"
    recorded = temporary / "with-journal"
    for home in (unrecorded, recorded):
        shutil.copytree(kept, home)

    original = (kept / "config/plays.log").read_bytes()
    original_json = (kept / "config/plays.json").read_bytes()
    media = "config/plays.log"
    output = []
    for home, recording, app_port, binary in (
        (unrecorded, False, 29485, baseline), (recorded, True, 29486, candidate),
    ):
        player = Player(home, app_port, recording, binary, clock)
        with player as app:
            assert (home / media).read_bytes() == original
            assert (home / "config/plays.json").read_bytes() == original_json
            if recording:
                assert until(lambda: event(home, "open"), "flushed Open")
            play(app)
            until(lambda: pcm_at_least(home, PCM_SECOND), "the first second of PCM")
            pcm_started = time.monotonic()
            until(lambda: pcm_at_least(home, 7 * PCM_SECOND), "seven seconds of PCM, across the gapless join")
            pcm_elapsed = time.monotonic() - pcm_started
            app.one(label=SONGS[1]["title"])
            if recording:
                first = until(lambda: event(home, "start", song_id="w8-a"), "first audible song")
                second = until(lambda: event(home, "start", song_id="w8-b"), "gapless second song")
                finished = until(lambda: event(home, "stop", start=first["id"], why={"kind": "finished"}), "first Stop")
                assert first["event"]["from"] == {"kind": "playlist", "id": PLAYLIST_ID}
                assert first["event"]["title"] == SONGS[0]["title"]
                assert first["event"]["sound"] == "streamed"
                assert second["event"]["began"] == "next_in_queue"
                assert finished["event"]["heard_ms"] > 5000

            app.press("Pause")
            if recording:
                paused = until(lambda: event(home, "pause", start=second["id"]), "Pause")
            app.press("Play")
            if recording:
                until(lambda: event(home, "resume", start=second["id"]), "Resume")
            app.press("Pause")
            if recording:
                until(lambda: any(row["sequence"] > paused["sequence"] and row["event"]["kind"] == "pause"
                                  for row in rows(home)), "the second Pause")
            app.press("Seek", role="Slider")
            if recording:
                sought = until(lambda: event(home, "seek", start=second["id"]), "Seek")
                assert sought["event"]["to_ms"] > sought["event"]["from_ms"]
            app.one(label="Play", role="Button")
            size = (home / "audio.pcm").stat().st_size
            time.sleep(0.25)
            after_seek = (home / "audio.pcm").stat().st_size
            assert after_seek == size, "headless output must be stationary while paused"
            app.press("Play")
            until(lambda: pcm_at_least(home, after_seek + PCM_SECOND), "PCM from the seek target")
            app.press("Pause")

            raw = until(lambda: (home / media).read_bytes() if (home / media).stat().st_size > len(original) else None,
                        "legacy play count")
            assert raw.startswith(original)
            suffix = raw[len(original):]
            song, at, source = json.loads(suffix)
            assert suffix == f'["w8-a",{at},"playlist:w8-set"]\n'.encode(), (song, source, suffix)
            assert song == "w8-a" and source == "playlist:w8-set"
            pcm = (home / "audio.pcm").read_bytes()
            output.append((raw, pcm[:7 * PCM_SECOND], pcm[after_seek:after_seek + PCM_SECOND]))
            if recording:
                search = app.one(label="Search", role="TextInput")
                app.click(search["handle"])
                app.set(search["handle"], FORGOTTEN_SEARCH)
                app.key("\n")
                until(lambda: event(home, "search", words=FORGOTTEN_SEARCH, found=0), "zero-result search")
                settings_tab(app, "About", "Save diagnostics")
                if not app.find(label="What is sent", role="Button"):
                    scroller = app.one(type="ScrollView")
                    app.tool("scroll_element", elementHandle=scroller["handle"], deltaY=-260)
                app.press("What is sent")
                report = app.one(contains="What the player has met")["accessibleLabel"]
                for private in [FORGOTTEN_SEARCH, "Paper Astronauts", "Tin Weather", SONGS[0]["title"]]:
                    assert private not in report and private not in player.log.read_text()
                app.press("Close settings")
            else:
                assert not journal_files(home)
            app.press("Repeat")
            until(lambda: json.loads((home / "config/settings.json").read_bytes())["repeat"] == 1,
                  "the new Repeat setting on disk")
            player.kill()
        count, longest = flushes(player.log)
        print(f"{home.name}: six seconds of PCM arrived in {pcm_elapsed:.3f} s; "
              f"{count} window flushes, longest {longest:.1f} ms", flush=True)

    assert output[0][0] == output[1][0], "the full play log bytes differ"
    assert output[0][1] == output[1][1], "PCM through the gapless join differs"
    assert output[0][2] == output[1][2], "PCM after the seek differs"

    file = journal_files(recorded)[-1]
    flushed = file.read_bytes()
    old_rows = rows(recorded)
    with file.open("ab") as tail:
        tail.write(b'{"id":"torn-test')
    settings_path = recorded / "config/settings.json"
    settings = json.loads(settings_path.read_bytes())
    settings_path.write_text(json.dumps(settings | {"offline": True}))
    with Player(recorded, 29486, True, candidate, clock) as app:
        recovered = until(lambda: event(recorded, "stop", start=second["id"], why={"kind": "unknown"}), "restart Stop")
        assert recovered["event"]["position_ms"] is not None
        assert file.read_bytes() == flushed
        assert rows(recorded)[:len(old_rows)] == old_rows
        state = json.loads((recorded / "data/listening-journal/state.json").read_bytes())
        assert state["damaged"] == 1
        until(lambda: any(row["event"]["kind"] == "open" for row in rows(recorded)[len(old_rows):]),
              "the new Open recorded while offline")
        assert json.loads(settings_path.read_bytes())["repeat"] == 1
        settings_tab(app, "About", "What is sent")
        app.press("What is sent")
        report = app.one(contains="What the player has met")["accessibleLabel"]
        assert any(line in report.splitlines() for line in (
            "Events dropped because the journal queue was full: 1", "Events not written: 1",
        )), report
    print("Full play-log bytes, PCM through the gapless join and after a seek, Start/Stop, "
          "pause/resume, zero-result search, Diagnostics privacy, zero journal window flushes, "
          "Repeat across SIGKILL and torn-tail recovery verified.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kept_profile", type=Path)
    parser.add_argument("--baseline", type=Path, default=Path("target/nexus-music-v0.40.13"))
    parser.add_argument("--candidate", type=Path, default=Path("target/nexus-music-w8-audit"))
    options = parser.parse_args()
    for binary in (options.baseline, options.candidate):
        if not binary.is_file():
            parser.error(f"missing headless binary {binary}")
    with tempfile.TemporaryDirectory(prefix="nexus-w8-") as location, ThreadingHTTPServer(("127.0.0.1", 0), Music) as server:
        run = threading.Thread(target=server.serve_forever)
        run.start()
        try:
            walk(options.kept_profile, server.server_port, Path(location), options.baseline, options.candidate)
        finally:
            server.shutdown()
            run.join()
