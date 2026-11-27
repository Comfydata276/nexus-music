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
    """Takes every connection and never says a word."""
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
    return server


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
        sink = "import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind((\\\"127.0.0.1\\\",53));[s.recv(512) for _ in iter(int,1)]"
        command = (f'docker run --rm --network none -u 0 -v {A.WORK}:/work -v {resolv}:/etc/resolv.conf:ro {flags} -w /work nexus-music-dev '
                   f'sh -c "python3 -c \'{sink}\' & sleep 0.5; timeout 40 target/walk/nexus-music-{build} {args}"')
    began = time.time()
    out = subprocess.run(command, shell=True, capture_output=True, text=True)
    return out.stdout.strip(), time.time() - began


def main(build):
    os.makedirs(os.path.join(A.HOMES, "certs"), exist_ok=True)
    front.certs()
    subprocess.run(f"cp {front.CERTS}/ca.pem {A.HOMES}/certs/ca.pem", shell=True)
    # (Kept, or they would be closed as soon as they are made.)
    kept = [silent_listener(SILENT), silent_listener(PROXY), full_listener(HOLE)]
    at = lambda port, host=front.HOST: f"https://{host}:{port}{HELLO}"
    # phase, front mode, url, extra environment, network, what the runner must say, at once?
    phases = [
        ("found (the baseline)", "found", at(front.TLS_PORT), {}, "host", "Found", True),
        ("name look-up stalls", "found", at(443, "stall.example.org"), {}, "none", "NoAnswer", False),
        ("forward proxy stalls (HTTPS_PROXY)", "found", at(front.TLS_PORT), {"HTTPS_PROXY": f"http://127.0.0.1:{PROXY}", "ALL_PROXY": f"http://127.0.0.1:{PROXY}"}, "host", "NoAnswer", False),
        ("connect never completes", "found", at(HOLE), {}, "host", "NoAnswer", False),
        ("TLS never begins", "found", at(SILENT), {}, "host", "NoAnswer", False),
        ("headers never come", "stalled", at(front.TLS_PORT), {}, "host", "NoAnswer", False),
        # (A body that does not come whole is "no backend here" by the shared library's rule.)
        ("body never comes (200)", "bodystall", at(front.TLS_PORT), {}, "host", "NoBackend", False),
        ("a compressed 302 whose body never comes", "gzip302", at(front.TLS_PORT), {}, "host", "NoBackend", True),
        ("the look's process aborts (the shipped build's panic)", "found", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_CHILD": "abort"}, "host", "NoAnswer", True),
        ("the look's process never ends", "found", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_CHILD": "stall"}, "host", "NoAnswer", False),
    ]
    # The fault each check exists for, put back on purpose: each must fail.
    controls = [
        ("CONTROL: body opened before the status (W1's first version)", "gzip302", at(front.TLS_PORT), {"NEXUS_TEST_PROBE_BODY_FIRST": "1"}, "host", "NoBackend", True),
    ]
    failures, lines = [], []
    for name, mode, url, env, network, want, quick in phases + controls:
        with front.Front(mode, os.path.join(front.STATE, "phases", mode)):
            said, _ = in_container(build, f"--nexus-probe-timed {url}", env, network)
            alone, alone_took = in_container(build, f"--nexus-probe {url}", env, network)
        outcome, _, ms = said.rpartition("\n")[2].partition(" ")
        took = int(ms) / 1000 if ms.isdigit() else None
        limit = QUICK if quick else DEADLINE + SLACK
        ok = outcome == want and took is not None and took <= limit
        line = f"| {name} | {outcome} in {took} s | {want}, within {limit} s | {'yes' if ok else '**no**'} | {alone or '(nothing)'} after {alone_took:.1f} s |"
        print(line, flush=True)
        lines.append(line)
        control = name.startswith("CONTROL")
        if ok == control:
            failures.append(name)
    table = "\n".join(["| Phase | The component's look said | Must say | Within | Its process left to itself |", "|---|---|---|---|---|"] + lines)
    open(os.path.join(HERE, "PHASES.md"), "w").write(
        "# One look, stalled in each phase\n\nMade by `python3 tests/e13a/phases.py`. The deadline over a look is 10 s. "
        "A CONTROL row puts back the fault its check exists for, and must fail.\n\n" + table + "\n")
    print("\nfailed:", failures or "none")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "new"))
