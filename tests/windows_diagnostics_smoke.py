"""Walk the Windows executable under Wine using the journal walk's kept profile and Slint driver.

The Wine image needs wine64, Xvfb and xauth. The app is an xwin debug build with
SLINT_EMIT_DEBUG_INFO=1 and slint/mcp. Run from the repository root.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from journal_smoke import App, Music, event, journal_files, prepare, rows, settings_tab, until


class MusicWithRefusal(Music):
    def do_GET(self):
        url = urlsplit(self.path)
        if parse_qs(url.query).get("u") != ["refused-fixture"]:
            return super().do_GET()
        body = json.dumps({"subsonic-response": {
            "status": "failed", "version": "1.16.1",
            "error": {"code": 40, "message": "Fictional refusal for the Windows walk"},
        }}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Player:
    def __init__(self, home, binary, image, port):
        self.name = f"nexus-w6t-wine-{port}"
        self.home = home
        self.binary = binary.resolve()
        self.image = image
        self.port = port
        self.log = home / f"{self.name}.log"

    def __enter__(self):
        with self.log.open("w") as output:
            self.process = subprocess.Popen([
                "docker", "run", "--rm", "--init", "--network", "host", "--name", self.name,
                "--user", f"{os.getuid()}:{os.getgid()}",
                "-v", f"{self.home}:/walk", "-v", f"{self.binary}:/app/nexus-music.exe:ro",
                "-e", "HOME=/walk", "-e", "WINEPREFIX=/walk/wine", "-e", "WINEARCH=win64",
                "-e", "WINEDEBUG=-all", "-e", "WINEDLLOVERRIDES=mscoree,mshtml=;winemenubuilder.exe=d",
                "-e", r"NEXUS_MUSIC_HOME=Z:\walk\profile", "-e", "NEXUS_TEST_LISTENING_JOURNAL=1",
                "-e", "SLINT_BACKEND=headless-skia", "-e", f"SLINT_MCP_PORT={self.port}",
                "-e", "NEXUS_WINDOW_SIZE=1143x746", "-e", "NEXUS_TRACE=1",
                self.image, "xvfb-run", "-a", "/usr/lib/wine/wine64", "/app/nexus-music.exe",
            ], stdout=output, stderr=output)
        try:
            return App(self.port).wait_up(80)
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def kill(self):
        subprocess.run(["docker", "kill", self.name], capture_output=True, check=True, timeout=15)
        assert self.process.wait(timeout=20) == 137

    def __exit__(self, kind, *_):
        if self.process.poll() is None:
            subprocess.run(["docker", "stop", "-t", "2", self.name], capture_output=True, timeout=15)
            self.process.wait(timeout=20)
        if kind:
            print(self.log.read_text(errors="replace")[-5000:])


def enter(app, server, username):
    for label, value in [("Server address", server), ("Username", username), ("Password", "fixture-password")]:
        handle = app.one(label=label, role="TextInput")["handle"]
        app.click(handle)
        app.set(handle, value)
    app.press("Connect")


def preview(app):
    settings_tab(app, "About", "What is sent")
    app.press("What is sent")
    return app.one(contains="What the player has met")["accessibleLabel"]


def walk(home, kept, binary, image, music_port, app_port):
    profile = home / "profile"
    prepare(kept, profile, music_port)
    log = profile / "config/plays.log"
    kept_plays = log.read_bytes()
    record = profile / "data/diagnostics-record.txt"
    fault = "Fault in music server: refused"
    server = f"http://127.0.0.1:{music_port}"
    player = Player(home, binary, image, app_port)
    with player as app:
        enter(app, server, "refused-fixture")
        until(lambda: event(profile, "fault"), "the flushed Windows journal fault")
        old_report = until(lambda: record.read_text() if record.is_file() and fault in record.read_text() else None,
                           "the flushed Windows Diagnostics record")
        enter(app, server, "fictional-listener")
        app.one(label="Home", role="Button")
        assert fault in preview(app)
        assert "Fictional refusal" not in old_report and "refused-fixture" not in old_report
        prior = rows(profile)
        file = journal_files(profile)[-1]
        whole = file.read_bytes()
        assert log.read_bytes() == kept_plays
        player.kill()

    with file.open("ab") as output:
        output.write(b'{"id":"incomplete Windows journal tail')
    with Player(home, binary, image, app_port) as app:
        until(lambda: len(rows(profile)) > len(prior), "the Windows journal to recover and record a second Open")
        assert rows(profile)[:len(prior)] == prior
        assert file.read_bytes() == whole
        if app.find(label="Server address", role="TextInput"):
            enter(app, server, "fictional-listener")
        app.one(label="Home", role="Button")
        settings_tab(app, "Storage", "Offline mode")
        app.press("Offline mode", role="Switch")
        until(lambda: "Offline: yes" in record.read_text(), "the Windows checkpoint to replace the prior report")
        report = preview(app)
        assert "Offline: yes" in report and fault in report
        assert any(line in report.splitlines() for line in (
            "Events dropped because the journal queue was full: 1", "Events not written: 1",
        )), report
        assert log.read_bytes() == kept_plays
    print("Windows app under Wine kept the report fault and the play log, repaired the journal tail, and displayed one loss.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kept_profile", type=Path)
    parser.add_argument("windows_binary", type=Path)
    parser.add_argument("--image", default="nexus-music-w6t-wine:local")
    parser.add_argument("--port", type=int, default=29574)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="nexus-w6t-wine-") as home, ThreadingHTTPServer(("127.0.0.1", 0), MusicWithRefusal) as music:
        service = threading.Thread(target=music.serve_forever)
        service.start()
        try:
            walk(Path(home), args.kept_profile, args.windows_binary, args.image, music.server_port, args.port)
        finally:
            music.shutdown()
            service.join()
