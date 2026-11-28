"""Smoke-test the actual Settings report and export in the Slint MCP build.

Build with SLINT_EMIT_DEBUG_INFO=1 and --features slint/mcp via scripts/dx,
then run python3 -B tests/diagnostics_smoke.py [--shots DIRECTORY] at the repo root.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path.cwd() / "tests/e13a"))
from drive import App
from suite import settings_tab

key = "nxs_shot_private_xzYuW6a"
song = "Paper Satellites"
local_name = "ExampleLocal"
crash_name = "Cardboard Orbit"

class FakeMusic(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "serverVersion": f"0.60.2 (fake version {key})",
            "artists": {"index": []},
            "albumList2": {"album": []},
            "playlists": {"playlist": []},
            "searchResult3": {"artist": [], "album": [], "song": []},
        }}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass

def until(predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.2)
    raise RuntimeError("condition timed out")

def capture(size, server_url, port, shots):
    with tempfile.TemporaryDirectory(prefix=f"nexus-diagnostics-{local_name}-") as location:
        home = Path(location)
        (home / "data").mkdir()
        (home / "config").mkdir()
        (home / "config/settings.json").write_text(json.dumps({
            "server": server_url,
            "username": "melody@example.invalid",
            "check_updates": False,
            "queue_panel": False,
            "sync": False,
            "said_no_links": True,
        }))
        (home / "config/credential").write_text(f"Nexus Music\nmelody@example.invalid@{server_url}\nfake-w6a-password")
        (home / "data/crash.log").write_text(
            f"2026-11-28 02:45:00 UTC version 0.40.13 thread {key} at src/audio/engine.rs:837: index out of bounds: {song}\n"
            f"2026-11-28 02:46:00 UTC version 0.40.13 thread main at src/controller.rs:87: already borrowed: {crash_name} C:\\Users\\{local_name}\n"
            f"2026-11-28 02:47:00 UTC version 0.40.13 thread {key} at src/audio/engine.rs:837: index out of bounds: {song}\n"
        )
        (home / "data/crash.seen").write_text(str((home / "data/crash.log").stat().st_size))
        name = f"nexus-diagnostics-smoke-{size}"
        env = os.environ | {
            "DX_DOCKER_ARGS": " ".join([
                f"--name {name}",
                f"-v {home}:/profile",
                "-e NEXUS_MUSIC_HOME=/profile",
                "-e SLINT_BACKEND=headless-skia",
                f"-e SLINT_MCP_PORT={port}",
                f"-e NEXUS_WINDOW_SIZE={size}",
                "-e NEXUS_SHOW_WINDOW_BUTTONS=1",
            ]),
        }
        with (home / "app.log").open("w") as log:
            app_run = subprocess.Popen(["scripts/dx", "/work/target/debug/nexus-music"], env=env, stdout=log, stderr=log)
            try:
                app = App(port).wait_up(30)
                app.wait_for(label="Settings", role="Button")
                settings_tab(app, "About", "Save diagnostics")
                if not app.find(label="What is sent", role="Button"):
                    scroller = app.one(type="ScrollView")
                    app.tool("scroll_element", elementHandle=scroller["handle"], deltaY=-260)
                app.press("What is sent")
                report = app.one(contains="What the player has met")["accessibleLabel"]
                for raw in [song, crash_name, key, local_name, server_url, "/profile", "melody@example.invalid", "Android Auto", "Local network permission"]:
                    assert raw not in report, f"preview exposed {raw!r}"
                for want in ["Music server version: 0.60.2", "Fault in playback: out of bounds at engine.rs:837  (2 times since", "Fault in app: borrow conflict at controller.rs:87", "Events refused for good: 0"]:
                    assert want in report, report
                app.one(contains="Nexus Music can send this device report")
                if shots:
                    image = shots / f"what-is-sent-{size}.png"
                    app.shot(image)
                    png = image.read_bytes()
                    dimensions = tuple(int.from_bytes(png[offset:offset+4], "big") for offset in (16, 20))
                    assert dimensions == tuple(map(int, size.split("x"))), dimensions
                app.press("Back to About")
                app.press("Save diagnostics")
                until(lambda: app.find(label="Diagnostics saved."))
                saved = (home / "data/diagnostics.txt").read_bytes()
                assert saved == report.encode(), (saved, report)
                print(f"{size}: local export, clean preview and Back to About verified", flush=True)
            except BaseException:
                print((home / "app.log").read_text()[-4000:], file=sys.stderr)
                raise
            finally:
                subprocess.run(["docker", "stop", "-t", "2", name], capture_output=True, timeout=20)
                app_run.wait(timeout=20)

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shots", type=Path)
    args = parser.parse_args()
    if args.shots:
        args.shots.mkdir(parents=True, exist_ok=True)
    with ThreadingHTTPServer(("127.0.0.1", 0), FakeMusic) as fake:
        service = threading.Thread(target=fake.serve_forever)
        service.start()
        try:
            for size, port in [("820x560", 29481), ("1143x746", 29482)]:
                capture(size, f"http://127.0.0.1:{fake.server_port}", port, args.shots)
        finally:
            fake.shutdown()
            service.join()
