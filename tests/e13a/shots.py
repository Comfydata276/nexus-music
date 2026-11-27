"""Pictures of the row under Settings → Account at the window sizes the owner uses, with the
side panel open and closed, for "found" and for "none found".

    shots.py <build>      (new) into ~/.cache/nexus-music-android/shots/nexus-w1/"""
import os, shutil, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import front
import app as A
import suite

SIZES = ["820x560", "1000x650", "1143x746"]


def main(build):
    out = suite.KIT
    suite.prepare_server()
    for mode in ("found", "absent"):
        with front.Front(mode, os.path.join(out, "shots-front", mode)):
            for size in SIZES:
                home = f"shots-{mode}-{size}"
                shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)
                shutil.copytree(suite.KEPT, os.path.join(A.HOMES, home))
                copy = A.Copy(build, home, env={"NEXUS_WINDOW_SIZE": size, "NEXUS_MUSIC_NEXUS": "1"})
                a = copy.launch()
                suite.until(lambda: suite.home_drawn(a), 60, 0.2, "Home")
                for panel in ("panel-open", "panel-closed"):
                    if panel == "panel-closed" and a.find(label="Close", role="Button"):
                        a.click(a.find(label="Close", role="Button")[0]["handle"])
                        time.sleep(0.8)
                    suite.settings_tab(a, "Account", "Sign out")
                    want = "Found at" if mode == "found" else "None found"
                    suite.until(lambda: any(l.startswith(want) for l in suite.labels(a)), 30, 0.2, want)
                    time.sleep(0.8)
                    path = os.path.join(out, f"account-{mode}-{size}-{panel}.png")
                    a.shot(path)
                    print(path, [l for l in suite.labels(a) if l.startswith(("Found at", "None found", "Nexus server"))])
                    suite.close_settings(a)
                copy.stop()
                shutil.rmtree(os.path.join(A.HOMES, home), ignore_errors=True)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "new")
