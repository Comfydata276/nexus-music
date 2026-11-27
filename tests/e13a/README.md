# The tests of design 5.3, for the Windows app

Experiment E13a, and the proof for slice W1: the app driven through its real entry points,
headless on Linux (its development build with Slint's inspection port), while the Nexus
server's side at the music server's own origin, under `/nexus/`, is broken one way or another.
Each test records how long the app took; the released app's numbers are what every slice that
touches the app is then held to. `NUMBERS.md` has them.

## What it runs against

All on this machine, all made-up names, nothing of anyone's:

- **The music server**: the test Navidrome "w1", made by the phone repository's
  `NEXUS_TEST_PORT=4680 scripts/server --instance w1 start` (accounts `tester` and `guest`).
- **Its proxy**: nginx (`nginx:alpine`, host network, loopback only) at
  `https://music.example.org:4681`, with a certificate from an authority made for the run
  (`front.py`). The app's container is told that name is 127.0.0.1 and trusts that authority
  alone. `/nexus/` carries the rule of design 6.2 (logged apart, where the rule says not to log,
  so that the tests can see what was asked).
- **The backend's side**, by mode (`front.py`, `standin.py`):

| Mode | What the app gets from `/nexus/` |
|---|---|
| `absent` | No location for it: Navidrome's own answer (302 to `/app/` on GET, 405 otherwise) |
| `refused` | Nothing listens behind the proxy: 502 |
| `blackhole` | The proxy connects to a port whose queue is full, so the kernel drops every packet: 75 s of nothing, then 504 |
| `found` | A valid `hello` |
| `stalled` | The connection is taken and never answered (the proxy waits an hour) |
| `saturated` | 503 "busy" after 10 s, its body a byte every half second |
| `ended` | 401 "not signed in" |
| `half` | `hello`'s headers and half its body, then a reset |

And with the component (`new`) also, under `absent` and `stalled`:

| Hook | |
|---|---|
| `held` | The component's worker is held in a call that never returns (`NEXUS_TEST_HOLD_NEXUS`, read by a debug build only) |
| `disk-hang` | The component's folder (`data/nexus`, where design 5.1 gives it files of its own) is a named pipe: anything opened there for writing hangs |
| `disk-full` | That folder is a file system of its own, 4 KB, full |

W1 keeps no files, so the disk cases show that nothing was opened there and that the app is as
fast with that folder unusable; they are kept for the slices that will write there.

## The tests

Each run is one cold start on a copy of a kept profile (made by the released app:
`suite.py populate`), a `kill -9`, and a second start. The server is put back to the same
state first (the sync record, the saved queue, a list, a like).

| Test (design 5.3) | Entered at | Timed |
|---|---|---|
| `cold_start_publishes_the_saved_account` | The process started (`controller::start`) | to the first page drawn |
| `cold_start_puts_the_queue_back_and_plays` | `restore_queue`, then Play | to the queue on screen; Play to the first sound |
| `a_direct_play_starts_it_cold` | Space, as the first thing after a start (media keys and the tray are Windows-only code; they call the same `toggle_play`) | to the first sound |
| `two_songs_join_without_a_gap` | Two streamed with the next fetched ahead, two cached, two downloaded (a continuous sweep cut into songs) | Play to sound; the longest stall in the sound around the join; silent frames and the largest step at the join |
| `the_queue_is_changed_and_kept` | Add, move ("Play next"), remove, clear; `kill -9` and started again | each to `queue.json`; the queue after the kill is the queue before it |
| `downloads_start_and_resume` | By hand (8 songs); a kept list that gained a song, at start-up; 80 asked for while the server is lost, then back | to the index |
| `offline_by_hand_and_by_loss` | The switch; the server lost as a network is lost (the proxy closes every connection) | to offline, to a downloaded song's sound, back online |
| `a_like_and_a_play_reach_the_music_server` | A like; a counted play; one counted while the server is lost | to the server having it |
| `a_play_is_counted_as_released` | The counted play | Play to the line in `plays.log`; start-up leaves the log byte for byte; the line, with its time written as seconds after Play, compared across every run |
| `an_account_change_is_not_held_up` | Sign out, sign in as another account (the component stuck, in the stuck modes) | Connect to the new account's first page |

Times are wall-clock, seen through the inspection port (the window's element tree) or in the
profile's files, from this script.

## Running it

```sh
python3 tests/e13a/suite.py populate              # once, with target/walk/nexus-music-old
tests/e13a/run-pass.sh 1 old new                  # every mode, both builds in turn (~1 h)
python3 tests/e13a/suite.py run new stalled held  # one run
python3 tests/e13a/suite.py table                 # the numbers, from results/
python3 tests/e13a/shots.py new                   # the Settings row at the owner's window sizes
```

The builds are `target/walk/nexus-music-old` (the released app) and `-new`, each built with
`SLINT_EMIT_DEBUG_INFO=1 cargo build --features slint/mcp` (through `build-turn`).
