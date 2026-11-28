"""One headless copy of the Windows app (its Linux development build), in the dev container,
driven through Slint's inspection port (drive.py). See the README's "Running it on Linux for
development"."""
import os, subprocess, time
from drive import App
import front

WORK = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
HOMES = os.path.join(WORK, "target", "walk")
NAME, MCP = "nexus-w1-app", 9481


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)


class Copy:
    """`build` is "old" (the released app) or "new" (with the component); `home` a folder
    under target/walk. `env` adds variables (the component's test hooks), `mounts` docker
    --mount arguments (the component's disk blocked)."""

    def __init__(self, build, home, env=None, mounts=()):
        self.build, self.home, self.env, self.mounts = build, home, env or {}, mounts
        self.dump = os.path.join(HOMES, home, "audio.raw")
        self.started = None
        self.app = None

    def path(self, *parts):
        return os.path.join(HOMES, self.home, *parts)

    def launch(self, wait=120):
        sh(f"docker rm -f {NAME}")
        # The certificate authority made for the run is the only one the copy trusts.
        certs = os.path.join(HOMES, "certs")
        os.makedirs(certs, exist_ok=True)
        sh(f"cp {front.CERTS}/ca.pem {certs}/ca.pem")
        env = {"SLINT_MCP_PORT": MCP, "SLINT_BACKEND": "headless-skia", "NEXUS_MUSIC_HOME": f"/work/target/walk/{self.home}",
               "NEXUS_WINDOW_SIZE": "1280x800", "NEXUS_AUDIO_DUMP": f"/work/target/walk/{self.home}/audio.raw", "NEXUS_TRACE": "1",
               "SSL_CERT_FILE": "/work/target/walk/certs/ca.pem", "SSL_CERT_DIR": "/nonexistent", **self.env}
        args = f"-d --name {NAME} --add-host {front.HOST}:127.0.0.1 --add-host {front.HOST2}:127.0.0.1 " + " ".join(f"-e {k}={v}" for k, v in env.items())
        args += "".join(f" --mount {m}" for m in self.mounts)
        for _ in range(10):
            self.started = time.time()
            out = sh(f'cd {WORK} && DX_DOCKER_ARGS="{args}" scripts/dx target/walk/nexus-music-{self.build}')
            if out.returncode == 0 or "already in use" not in out.stderr:
                break
            time.sleep(2)
            sh(f"docker rm -f {NAME}")
        if out.returncode != 0:
            raise RuntimeError(out.stderr)
        self.app = App(MCP).wait_up(wait)
        return self.app

    def logs(self):
        return sh(f"docker logs {NAME} 2>&1").stdout

    def stop(self, kill=False, keep=None):
        if keep:
            with open(keep, "a") as out:
                out.write(f"\n===== {time.strftime('%H:%M:%S')} {self.build} {self.home} {'kill -9' if kill else 'stop'} =====\n" + self.logs())
        sh(f"docker kill -s KILL {NAME}" if kill else f"docker stop -t 10 {NAME}")
        sh(f"docker rm -f {NAME}")
        self.app = None

    def pid_threads(self):
        """The copy's threads by name, as the kernel has them."""
        out = sh(f"docker exec {NAME} sh -c 'for t in /proc/1/task/*; do cat $t/comm; done'")
        return out.stdout.split()
