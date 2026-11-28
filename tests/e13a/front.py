"""The stand-in proxy at the music server's address, and what it sends /nexus/ to.

The music server's address in these tests is https://music.example.org:4681 (a made-up name:
the app's container is told it is 127.0.0.1, and trusts only a certificate authority made
here for the run, so nothing on the internet can be reached over TLS). nginx (the local
`nginx:alpine` image, host network, loopback only) answers on it:

- every path goes to the test Navidrome (instance "w1", http://127.0.0.1:4680);
- /nexus/, by the mode:
    absent     no location for it at all: Navidrome answers (302 to /app/ on GET, 405 else)
    refused    to a port nothing listens on (the proxy answers 502)
    blackhole  to a port whose queue of connections is full, so the kernel drops every
               packet that would open one (the proxy waits 75 s to connect, then 504)
    found, stalled, saturated, ended, half
               to standin.py, which answers as its mode says

The location's box carries the design's rule (6.2) as given, so what the app gets is what it
would get through the owner's proxy."""
import os, socket, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
HOST, TLS_PORT = "music.example.org", 4681
# A second origin on the same music server, with nothing under /nexus/: another account's.
HOST2, TLS_PORT2 = "music2.example.org", 4686
NAVIDROME = 4680
STANDIN, BLACKHOLE, REFUSED = 4682, 4683, 4684
# The second origin's own backend (slice W7): always answering, so that signing in as
# another person works whatever the first origin's backend is doing.
STANDIN2 = 4687
BACKEND_A, BACKEND_B = "5c0ffee05c0ffee05c0ffee05c0ffee0", "b0ffee11b0ffee11b0ffee11b0ffee11"
# A session the profile was signed in with before the run (`suite.to_signed_in`): the
# first origin's backend knows it from the start.
SEED_SESSION = "5e" * 32
SERVER = f"https://{HOST}:{TLS_PORT}"
SERVER2 = f"https://{HOST2}:{TLS_PORT2}"
NGINX = "nexus-w1-front"
STATE = os.path.expanduser("~/.cache/nexus-music-android/shots/nexus-w1/front")
CERTS = os.path.join(STATE, "certs")
MODES = ("absent", "refused", "blackhole", "found", "stalled", "saturated", "ended", "half", "bodystall", "gzip302")


def sh(cmd, check=False):
    out = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    if check and out.returncode != 0:
        raise RuntimeError(f"{cmd}: {out.stderr}")
    return out


# Where the certificate's revocation list is served (plain HTTP, loopback): Windows' own TLS
# under Wine fails a certificate whose revocation it cannot check, where Windows itself
# lets "unknown" pass; a list it can fetch says "not revoked" on both.
CRL_PORT = 4689


def certs():
    """A certificate authority for the run, and music.example.org's certificate from it,
    naming a revocation list (empty, signed by the authority) that the proxy serves."""
    os.makedirs(CERTS, exist_ok=True)
    if os.path.exists(os.path.join(CERTS, "two-names-crl")):
        return
    run = lambda cmd: sh(f"cd {CERTS} && {cmd}", check=True)
    if not os.path.exists(os.path.join(CERTS, "ca.pem")):
        run("openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 30 -subj '/CN=Nexus W1 test authority'")
    run("openssl req -newkey rsa:2048 -nodes -keyout music.key -out music.csr -subj '/CN=music.example.org'")
    open(os.path.join(CERTS, "ext.cnf"), "w").write(
        "subjectAltName=DNS:music.example.org,DNS:music2.example.org\nbasicConstraints=CA:FALSE\n"
        f"crlDistributionPoints=URI:http://127.0.0.1:{CRL_PORT}/test-ca.crl\n")
    run("openssl x509 -req -in music.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out music.pem -days 30 -extfile ext.cnf")
    os.chmod(os.path.join(CERTS, "music.key"), 0o644)
    # An empty revocation list, signed by the authority.
    open(os.path.join(CERTS, "index.txt"), "w").write("")
    open(os.path.join(CERTS, "crlnumber"), "w").write("01\n")
    open(os.path.join(CERTS, "ca.cnf"), "w").write(
        "[ ca ]\ndefault_ca = test\n[ test ]\ndatabase = index.txt\ncrlnumber = crlnumber\n"
        "default_md = sha256\ndefault_crl_days = 30\n")
    run("openssl ca -config ca.cnf -gencrl -keyfile ca.key -cert ca.pem -out test-ca.crl.pem")
    run("openssl crl -in test-ca.crl.pem -outform DER -out test-ca.crl")
    run("openssl req -x509 -newkey rsa:2048 -nodes -keyout music-self.key -out music-self.pem -days 30 -subj '/CN=music.example.org' "
        "-addext 'subjectAltName=DNS:music.example.org,DNS:music2.example.org' -addext 'basicConstraints=critical,CA:TRUE'")
    os.chmod(os.path.join(CERTS, "music-self.key"), 0o644)
    open(os.path.join(CERTS, "two-names-crl"), "w").write("")


RULE = """
            proxy_set_header Cookie "";
            proxy_set_header X-Nexus-Peer $realip_remote_addr;
            proxy_set_header X-Nexus-Scheme $scheme;
            proxy_set_header Remote-User "";
            proxy_set_header Forwarded "";
            proxy_set_header True-Client-IP "";
            proxy_set_header CF-Connecting-IP "";
            proxy_set_header X-Forwarded-Host "";
            proxy_no_cache 1;
            proxy_cache_bypass 1;
            # (The rule says "access_log off"; here it is logged apart, for the tests.)
            access_log /state/nexus-access.log timed;
            client_max_body_size 1m;"""


# For the Windows build under Wine (wine_walk.py): the proxy shows a certificate that is
# itself the trusted authority, so that Windows' TLS has no revocation to check.
SELF = os.environ.get("FRONT_SELF_SIGNED") == "1"
# And no answer over HTTPS that a read cuts in two: Wine's Schannel loses a TLS record that
# one read takes only part of (SEC_E_INCOMPLETE_MESSAGE, and the rest never comes), so an
# answer longer than about 1000 bytes never ends under Wine. Windows itself reads them. (The
# same as the updater's download from GitHub under Wine: memory "wine-github-download-stall".)
# Each answer is one record, and the headers are kept short.
WINE_RECORDS = """
        server_tokens off;"""


def conf(mode, music=True):
    nexus = ""
    if mode != "absent":
        port = {"refused": REFUSED, "blackhole": BLACKHOLE}.get(mode, STANDIN)
        nexus = f"""
        location /nexus/ {{
            proxy_pass http://127.0.0.1:{port};
            proxy_http_version 1.1;
            proxy_buffering off;
            proxy_connect_timeout 75s;
            proxy_read_timeout 3600s;
            proxy_send_timeout 3600s;{RULE}
        }}"""
    return f"""
worker_processes 1;
pid /tmp/nginx.pid;
error_log /dev/stderr warn;
events {{ worker_connections 1024; }}
http {{
    log_format timed '$msec $request_time $status "$request" "$http_user_agent"';
    access_log /state/access.log timed;
    client_body_temp_path /tmp/client_temp;
    proxy_temp_path /tmp/proxy_temp;
    fastcgi_temp_path /tmp/fastcgi_temp;
    uwsgi_temp_path /tmp/uwsgi_temp;
    scgi_temp_path /tmp/scgi_temp;
    upstream navidrome {{ server 127.0.0.1:{NAVIDROME}; keepalive 32; }}
    server {{
        listen 127.0.0.1:{TLS_PORT} ssl;
        server_name {HOST};
        ssl_certificate /certs/{"music-self" if SELF else "music"}.pem;
        ssl_certificate_key /certs/{"music-self" if SELF else "music"}.key;{WINE_RECORDS if SELF else ""}
        client_max_body_size 0;
        location / {{{"" if music else """
            return 444;"""}
            proxy_pass http://navidrome;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host $host;
            proxy_buffering off;
            proxy_read_timeout 3600s;
        }}{nexus}
    }}
    server {{
        listen 127.0.0.1:{CRL_PORT};
        location = /test-ca.crl {{
            root /certs;
            default_type application/pkix-crl;
        }}
    }}
    server {{
        listen 127.0.0.1:{TLS_PORT2} ssl;
        server_name {HOST2};
        ssl_certificate /certs/{"music-self" if SELF else "music"}.pem;
        ssl_certificate_key /certs/{"music-self" if SELF else "music"}.key;{WINE_RECORDS if SELF else ""}
        client_max_body_size 0;
        location / {{{"" if music else """
            return 444;"""}
            proxy_pass http://navidrome;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host $host;
            proxy_buffering off;
            proxy_read_timeout 3600s;
        }}
        location /nexus/ {{
            proxy_pass http://127.0.0.1:{STANDIN2};
            proxy_http_version 1.1;
            proxy_buffering off;{RULE.replace("nexus-access.log", "nexus2-access.log")}
        }}
    }}
}}
"""


def blackhole():
    """A port that takes no connection: its queue is filled, and never emptied."""
    hole = socket.socket()
    hole.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    hole.bind(("127.0.0.1", BLACKHOLE))
    hole.listen(0)
    fillers = []
    for _ in range(4):
        filler = socket.socket()
        filler.setblocking(False)
        try:
            filler.connect(("127.0.0.1", BLACKHOLE))
        except BlockingIOError:
            pass
        fillers.append(filler)
    time.sleep(0.5)
    return hole, fillers


class Front:
    def __init__(self, mode, out):
        assert mode in MODES, mode
        self.mode, self.out = mode, out
        os.makedirs(out, exist_ok=True)
        self.mode_file = os.path.join(out, "standin.mode")
        self.log = os.path.join(out, "standin.jsonl")
        self.mode_file2 = os.path.join(out, "standin2.mode")
        self.log2 = os.path.join(out, "standin2.jsonl")
        self.procs, self.hole = [], None

    def set(self, mode):
        """Changes what standin.py answers, under a running app."""
        open(self.mode_file, "w").write(mode)

    def hold(self, **state):
        """Changes what the first origin's backend holds, under a running app (standin.py's
        state file): `ended`, `count`, `bad_token`. The seeded session is always kept."""
        import json
        now = {}
        try:
            now = json.load(open(self.mode_file + ".state"))
        except (OSError, ValueError):
            pass
        now.update(state)
        now.setdefault("sessions", {SEED_SESSION: {"email": "tester@example.org", "device": 1, "id": 99}})
        json.dump(now, open(self.mode_file + ".state", "w"))

    def __enter__(self):
        certs()
        sh(f"docker rm -f {NGINX}")
        sh(f"pkill -f 'standin[.]py {STANDIN}'")
        self.set(self.mode if self.mode in ("found", "stalled", "saturated", "ended", "half", "bodystall", "gzip302") else "found")
        sh(f"pkill -f 'standin[.]py {STANDIN2}'")
        for name in (self.mode_file + ".state", self.mode_file2 + ".state"):
            if os.path.exists(name):
                os.remove(name)
        self.hold()
        open(self.mode_file2, "w").write("found")
        self.procs.append(subprocess.Popen([sys.executable, "-I", os.path.join(HERE, "standin.py"), str(STANDIN), self.mode_file, self.log],
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={**os.environ, "STANDIN_ID": BACKEND_A, "STANDIN_MUSIC": os.environ.get("FRONT_MUSIC", SERVER)}))
        self.procs.append(subprocess.Popen([sys.executable, "-I", os.path.join(HERE, "standin.py"), str(STANDIN2), self.mode_file2, self.log2],
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env={**os.environ, "STANDIN_ID": BACKEND_B, "STANDIN_MUSIC": SERVER2}))
        if self.mode == "blackhole":
            self.hole = blackhole()
        open(os.path.join(STATE, "nginx.conf"), "w").write(conf(self.mode))
        sh(f"docker run -d --name {NGINX} --network host -u {os.getuid()}:{os.getgid()} "
           f"-v {STATE}/nginx.conf:/etc/nginx/nginx.conf:ro -v {CERTS}:/certs:ro -v {self.out}:/state "
           f"nginx:alpine nginx -g 'daemon off;'", check=True)
        for _ in range(50):
            if sh(f"curl -s --cacert {CERTS}/{"music-self" if SELF else "ca"}.pem --resolve {HOST}:{TLS_PORT}:127.0.0.1 -o /dev/null -w '%{{http_code}}' {SERVER}/rest/ping").stdout == "200":
                return self
            time.sleep(0.2)
        raise RuntimeError("the stand-in proxy didn't come up: " + sh(f"docker logs {NGINX} 2>&1").stdout[-800:])

    def __exit__(self, *exc):
        sh(f"docker logs {NGINX} > {self.out}/nginx.log 2>&1")
        sh(f"docker rm -f {NGINX}")
        for proc in self.procs:
            proc.terminate()
        if self.hole:
            self.hole[0].close()
            for filler in self.hole[1]:
                filler.close()

    def music(self, up):
        """The music server reachable, or lost as a network is lost: the proxy closes every
        connection for it without a word. /nexus/ is left as the mode has it."""
        open(os.path.join(STATE, "nginx.conf"), "w").write(conf(self.mode, up))
        sh(f"docker exec {NGINX} nginx -s reload", check=True)
        want = "200" if up else "000"
        for _ in range(50):
            if sh(f"curl -s --cacert {CERTS}/{"music-self" if SELF else "ca"}.pem --resolve {HOST}:{TLS_PORT}:127.0.0.1 -o /dev/null -w '%{{http_code}}' {SERVER}/rest/ping").stdout == want:
                return
            time.sleep(0.1)
        raise RuntimeError("the proxy didn't take the change")

    def asked(self, second=False):
        """What reached the stand-in (or the second origin's), as it logged it."""
        import json
        log = self.log2 if second else self.log
        if not os.path.exists(log):
            return []
        return [json.loads(line) for line in open(log) if line.strip()]

    def nexus_in_access_log(self):
        """Every request the proxy saw under /nexus/, whatever it sent them to."""
        lines = []
        for name in ("access.log", "nexus-access.log"):
            path = os.path.join(self.out, name)
            if os.path.exists(path):
                lines += [line for line in open(path) if " /nexus" in line]
        return lines


if __name__ == "__main__":
    # front.py <mode>: up until Enter, for looking at it by hand.
    with Front(sys.argv[1], os.path.join(STATE, "by-hand")) as front:
        print(f"{SERVER} with /nexus/ {front.mode}; Enter to stop")
        input()
