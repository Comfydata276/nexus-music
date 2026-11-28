"""Run the E13a stand-in with its Diagnostics route enabled for the Windows test."""
import http.server
import json
from pathlib import Path
import runpy
import sys

received = Path(sys.argv[1])
standin = Path(__file__).with_name("e13a") / "standin.py"
serve_forever = http.server.HTTPServer.serve_forever


def serve(server, *args, **kwargs):
    class Diagnostics(server.RequestHandlerClass):
        def do_GET(self):
            if self.path == "/nexus/api/hello":
                hello = {
                    "nexus": "0.1.0", "id": "5c0ffee05c0ffee05c0ffee05c0ffee0",
                    "speaks": [1, 1], "has": ["accounts"], "diagnostics": True,
                }
                self.note("found")
                return self.answer(200, json.dumps(hello).encode())
            super().do_GET()

        def do_POST(self):
            if self.path != "/nexus/api/diag/send":
                return super().do_POST()
            if self.headers.get("Authorization") != "Bearer " + "63" * 32:
                return self.answer(401, b"{}")
            length = int(self.headers.get("Content-Length", 0))
            if not 0 < length <= 65536:
                return self.answer(400, b"{}")
            received.write_bytes(self.rfile.read(length))
            self.note("found")
            reply = {"done": {"result": {}, "receipt": {
                "stamp": {
                    "backend": "5c0ffee05c0ffee05c0ffee05c0ffee0",
                    "restore": 1, "account": 1, "life": 1, "change": 1,
                    "tag": "01" * 32,
                },
                "music_login_count": 0,
            }}}
            return self.answer(200, json.dumps(reply).encode())

    server.RequestHandlerClass = Diagnostics
    print(server.server_port, flush=True)
    serve_forever(server, *args, **kwargs)


http.server.HTTPServer.serve_forever = serve
sys.argv = [str(standin), "0", str(received.with_suffix(".mode")), str(received.with_suffix(".log"))]
runpy.run_path(str(standin), run_name="__main__")
