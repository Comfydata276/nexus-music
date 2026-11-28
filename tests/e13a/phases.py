"""One look of the Nexus server's component, stalled in each phase this machine can stall,
through the component's own runner (`--nexus-probe-timed`, a development build only): each
must end with no answer by the deadline (10 s), or with the right answer at once. Beside
each, the look's process left to itself (`--nexus-probe`, no deadline over it), to show what
the deadline is for.

    phases.py [build]     (new) exits 1 if any phase is not as it must be

Everything is on this machine: listeners on 127.0.0.1 that take a connection and say
nothing, or take none; the stand-in proxy (front.py); and, for the name look-up, a
container with no network whose only name server is a socket that never answers."""
import os, socket, subprocess, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import front
import app as A

DEADLINE = 10.0
SLACK = 1.0          # the runner looks every 20 ms; killing and reaping the process
QUICK = 2.0          # what "at once" is allowed, here
HELLO = "/nexus/api/hello"
SILENT, HOLE, PROXY = 4692, 4691, 4693


def silent_listener(port):
    """Takes every connection and never says a word. (`.held` is what it has taken.)"""
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(64)
    held = []
    def take():
        while True:
            try:
                held.append(server.accept()[0])
            except OSError:
                return
    threading.Thread(target=take, daemon=True).start()

    class Listener:
        pass
    listener = Listener()
    listener.server, listener.held = server, held
    return listener


def full_listener(port):
    """Takes no connection: its queue is full."""
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(0)
    fillers = []
    for _ in range(4):
        filler = socket.socket()
        filler.setblocking(False)
        try:
            filler.connect(("127.0.0.1", port))
        except BlockingIOError:
            pass
        fillers.append(filler)
    time.sleep(0.5)
    return server, fillers


def in_container(build, args, env=None, network="host"):
    """The build run with `args` in the dev container, trusting the run's authority."""
    env = {"SSL_CERT_FILE": "/work/target/walk/certs/ca.pem", "SSL_CERT_DIR": "/nonexistent", **(env or {})}
    flags = " ".join(f"-e {k}={v}" for k, v in env.items())
    if network == "host":
        docker = f"--add-host {front.HOST}:127.0.0.1 {flags}"  # (scripts/dx is on the host's network)
        command = f'cd {A.WORK} && DX_DOCKER_ARGS="{docker}" scripts/dx timeout 40 target/walk/nexus-music-{build} {args}'
    else:
        # No network at all, and a name server that never answers: a look-up that stalls.
        resolv = os.path.join(A.HOMES, "certs", "resolv.conf")
        open(resolv, "w").write("nameserver 127.0.0.1\noptions timeout:30 attempts:3\n")
        sink = "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind((\\\"127.0.0.1\\\",53));s.recv(512);open(\\\"/work/target/walk/certs/dns-ack\\\",\\\"w\\\").write(\\\"asked\\\");[s.recv(512) for _ in iter(int,1)]"
        command = (f'docker run --rm --network none -u 0 -v {A.WORK}:/work -v {resolv}:/etc/resolv.conf:ro {flags} -w /work nexus-music-dev '
                   f'sh -c "python3 -c \'{sink}\' & sleep 0.5; timeout 40 target/walk/nexus-music-{build} {args}"')
    began = time.time()
    out = subprocess.run(command, shell=True, capture_output=True, text=True)
    return out.stdout.strip(), time.time() - began, out.returncode


class SynSeen:
    """Whether anything tried to open a connection to `port` (seen as SYN-SENT) meanwhile."""

    def __init__(self, port):
        self.port, self.seen, self.going = port, False, True
        threading.Thread(target=self.watch, daemon=True).start()

    def watch(self):
        while self.going and not self.seen:
            out = subprocess.run(f"ss -Htan state syn-sent '( dport = :{self.port} )'", shell=True, capture_output=True, text=True).stdout
            self.seen = bool(out.strip())
            time.sleep(0.05)

    def stop(self):
        self.going = False
        return self.seen


def main(build):
    os.makedirs(os.path.join(A.HOMES, "certs"), exist_ok=True)
    front.certs()
    subprocess.run(f"cp {front.CERTS}/ca.pem {A.HOMES}/certs/ca.pem", shell=True)
    silent, proxy, hole = silent_listener(SILENT), silent_listener(PROXY), full_listener(HOLE)
    # (The address as the program makes it: the one reading of an address leaves out the
    # port when it is HTTPS's own, 443.)
    at = lambda port, host=front.HOST: f"https://{host}{'' if port == 443 else f':{port}'}{HELLO}"
    on = {"NEXUS_MUSIC_NEXUS": "1"}
    dns_ack = os.path.join(A.HOMES, "certs", "dns-ack")
    fds = os.path.join(A.HOMES, "certs", "fds")
    # How each phase shows it was entered: the stand-in asked, a listener reached, a name
    # server asked, a connection tried; or the look running to its deadline.
    asked = lambda f: len(f.asked()) > 0
    entered = {
        "stand-in": lambda f, took: asked(f),
        "dns": lambda f, took: os.path.exists(dns_ack),
        "proxy": lambda f, took: len(proxy.held) > 0,
        "connect": lambda f, took: syn.seen,
        "tls": lambda f, took: len(silent.held) > 0,
        "deadline": lambda f, took: took is not None and took >= DEADLINE - 0.1,
        "spawned": lambda f, took: took is not None,
    }
    # phase, front mode, url, extra environment, network, must say, at once?, how it shows it was entered
    phases = [
        ("found (the baseline)", "found", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_FDS": "/work/target/walk/certs/fds"}, "host", "Found", True, "stand-in"),
        ("name look-up stalls", "found", at(443, "stall.example.org"), {}, "none", "NoAnswer", False, "dns"),
        ("forward proxy stalls (HTTPS_PROXY)", "found", at(front.TLS_PORT), {"HTTPS_PROXY": f"http://127.0.0.1:{PROXY}", "ALL_PROXY": f"http://127.0.0.1:{PROXY}"}, "host", "NoAnswer", False, "proxy"),
        ("connect never completes", "found", at(HOLE), {}, "host", "NoAnswer", False, "connect"),
        ("TLS never begins", "found", at(SILENT), {}, "host", "NoAnswer", False, "tls"),
        ("headers never come", "stalled", at(front.TLS_PORT), {}, "host", "NoAnswer", False, "stand-in"),
        # (A body that does not come whole is "no backend here" by the shared library's rule.)
        ("body never comes (200)", "bodystall", at(front.TLS_PORT), {}, "host", "NoBackend", False, "stand-in"),
        ("a compressed 302 whose body never comes", "gzip302", at(front.TLS_PORT), {}, "host", "NoBackend", True, "stand-in"),
        ("the look's process aborts (the shipped build's panic)", "found", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_CHILD": "abort"}, "host", "NoAnswer", True, "spawned"),
        ("the look's process never ends", "found", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_CHILD": "stall"}, "host", "NoAnswer", False, "deadline"),
    ]
    # The fault each check exists for, put back on purpose: each must fail, and for its reason.
    controls = [
        ("CONTROL: body opened before the status (W1's first version)", "gzip302", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_BODY_FIRST": "1"}, "host", "NoBackend", True, "stand-in"),
    ]
    failures, lines = [], []
    global syn
    for name, mode, url, env, network, want, quick, how in phases + controls:
        for path in (dns_ack, fds):
            if os.path.exists(path):
                os.remove(path)
        proxy.held.clear(); silent.held.clear()
        syn = SynSeen(HOLE)
        with front.Front(mode, os.path.join(front.STATE, "phases", mode)) as f:
            said, _, _ = in_container(build, f"--nexus-probe-timed {url}", {**on, **env}, network)
            syn.stop()
            outcome, _, ms = said.rpartition("\n")[2].partition(" ")
            took = int(ms) / 1000 if ms.isdigit() else None
            was_entered = entered[how](f, took)
            alone, alone_took, alone_code = in_container(build, f"--nexus-probe {url}", {**on, **env}, network)
        limit = QUICK if quick else DEADLINE + SLACK
        right, in_time = outcome == want, took is not None and took <= limit
        ok = right and in_time and was_entered
        said_alone = {10: "found", 11: "no backend", 124: "still running at 40 s"}.get(alone_code, f"exit {alone_code}")
        line = f"| {name} | {outcome} in {took} s | {want}, within {limit} s | {how}: {'yes' if was_entered else '**no**'} | {'yes' if ok else '**no**'} | {said_alone} after {alone_took:.1f} s |"
        print(line, flush=True)
        lines.append(line)
        if name.startswith("CONTROL"):
            # Caught for its reason: the right answer, entered, but not in time.
            if not (right and was_entered and not in_time):
                failures.append(f"{name}: not caught for its reason")
        elif not ok:
            failures.append(name)
        if how == "stand-in" and mode == "found" and os.path.exists(fds):
            given = open(fds).read().split()
            fd_line = f"| the look's process was given descriptors | {' '.join(given)} | 0 1 2, and the one it reads them by | | {'yes' if set(given) <= {'0', '1', '2', '3'} else '**no**'} | |"
            print(fd_line)
            lines.append(fd_line)
            if not set(given) <= {"0", "1", "2", "3"}:
                failures.append(f"descriptors given to the look's process: {given}")
    # (The switch is gone with slice W7: the component is always made, so the rows for
    # "switched off" are no longer run.)
    table = "\n".join(["| Phase | The component's look said | Must say | Entered, and how | As it must be | Its process left to itself |", "|---|---|---|---|---|---|"] + lines)
    open(os.path.join(HERE, "PHASES.md"), "w").write(
        "# One look, stalled in each phase\n\nMade by `python3 tests/e13a/phases.py`. The deadline over a look is 10 s. "
        "A phase counts only if it was entered (the stand-in asked, the listener reached, the name server asked, a connection "
        "tried, or the look ran to its deadline). A CONTROL row puts back the fault its check exists for, and must fail for "
        "that reason.\n\n" + table + "\n")
    print("\nfailed:", failures or "none")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "new"))
