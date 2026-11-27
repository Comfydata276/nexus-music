#!/usr/bin/env python3
"""A stand-in for the Nexus server, behind the stand-in proxy (front.py), for the tests of
design 5.3. Loopback only; it answers nothing but what the mode says.

    standin.py <port> <mode file> <log file>

The mode file is read at every request, so a test can change the mode under a running app:

    found      `hello` answered as the backend answers it
    stalled    the request is read, and nothing is ever sent back (the connection is held)
    saturated  every answer is "busy" (503), slowly: nothing for 10 s, then the body a byte
               every half second
    ended      every answer is "not signed in" (401), at once
    half       `hello`'s headers and half its body, then the connection is reset
    bodystall  200 and `hello`'s headers, and the body never comes
    gzip302    a 302 said to be gzip-compressed, and the body never comes

("absent" and "blackhole" are the proxy's: front.py sends /nexus/ to the music server, or to
a port whose queue of connections is full, and this stand-in never sees them.)

Every request is logged, one JSON line each: when, the method, the target exactly as it
arrived, and the User-Agent. The suite reads it to see who asked what."""
import http.server, json, socket, socketserver, struct, sys, threading, time

port, mode_file, log_file = int(sys.argv[1]), sys.argv[2], sys.argv[3]
lock = threading.Lock()

HELLO = json.dumps({"nexus": "0.1.0", "id": "5c0ffee05c0ffee05c0ffee05c0ffee0", "speaks": [1, 1], "has": []}).encode()


def mode():
    try:
        return open(mode_file).read().split()[0]
    except (OSError, IndexError):
        return "found"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def note(self, what):
        with lock, open(log_file, "a") as log:
            log.write(json.dumps({"t": round(time.time(), 3), "method": self.command, "target": self.path,
                                  "agent": self.headers.get("User-Agent", ""), "mode": what}) + "\n")

    def answer(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def go(self):
        m = mode()
        self.note(m)
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(min(length, 1 << 20))
        if m == "found":
            if self.command == "GET" and self.path == "/nexus/api/hello":
                return self.answer(200, HELLO)
            return self.answer(404, b'{"error":"no such call"}')
        if m == "stalled":
            while True:
                time.sleep(3600)
        if m == "saturated":
            time.sleep(10)
            body = b'{"error":"busy, try again later"}'
            self.send_response(503)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Retry-After", "60")
            self.end_headers()
            self.wfile.flush()
            for byte in body:
                self.wfile.write(bytes([byte]))
                self.wfile.flush()
                time.sleep(0.5)
            return
        if m == "bodystall":
            # A 200 whose body never comes.
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(HELLO)))
            self.end_headers()
            self.wfile.flush()
            while True:
                time.sleep(3600)
        if m == "gzip302":
            # A redirect said to be compressed, whose body never comes.
            self.send_response(302)
            self.send_header("Location", "/app/")
            self.send_header("Content-Encoding", "gzip")
            self.send_header("Content-Length", "100")
            self.end_headers()
            self.wfile.flush()
            while True:
                time.sleep(3600)
        if m == "ended":
            return self.answer(401, b'{"error":"not signed in"}')
        if m == "half":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(HELLO)))
            self.end_headers()
            self.wfile.write(HELLO[: len(HELLO) // 2])
            self.wfile.flush()
            # A reset, not a close: linger on, for no time.
            self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            self.connection.close()
            self.close_connection = True
            return
        return self.answer(500, b'{"error":"the stand-in has no such mode"}')

    do_GET = do_POST = do_HEAD = do_PUT = do_DELETE = go


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


Server(("127.0.0.1", port), Handler).serve_forever()
