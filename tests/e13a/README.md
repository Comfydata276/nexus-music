# The tests of design 5.3, for the Windows app

Experiment E13a, and the proof for slice W1. The app is driven through its real entry points,
headless on Linux (its development build with Slint's inspection port), while the Nexus
server's side at the music server's own origin, under `/nexus/`, is broken one way or
another. **Each run asserts what it saw and exits 1 on any failure.** `suite.py table`
holds every W1 run to the released app's numbers, and exits 1 if one is over its bar.
`NUMBERS.md` and `PHASES.md` hold the latest results.

## What it runs against

Everything runs on this machine, under made-up names; nothing of anyone's is used.

- **The music server** is the test Navidrome "w1", made by the phone repository's
  `NEXUS_TEST_PORT=4680 scripts/server --instance w1 start`, with the accounts `tester` and
  `guest`.
- **Its proxy** is nginx (`nginx:alpine`, host network, loopback only), at two origins:
  - `https://music.example.org:4681`, where `/nexus/` is as the mode says;
  - `https://music2.example.org:4686`, the second account's origin, which has nothing under
    `/nexus/`.

  Both use a certificate from an authority made for the run (`front.py`). The app's
  container is told the two names are 127.0.0.1, and trusts that authority alone. `/nexus/`
  carries the rule of design 6.2, except that it is logged (with the time each request began
  and ended), which the rule says not to do.
- **The backend's side**, by mode (`front.py`, `standin.py`):

| Mode | What the app gets from `/nexus/` |
|---|---|
| `absent` | No location for it: Navidrome's own answer (302 to `/app/` on GET, 405 otherwise) |
| `refused` | Nothing listens behind the proxy: 502 |
| `blackhole` | The proxy connects to a port whose queue is full, so every packet is dropped: 75 s of nothing, then 504 |
| `found` | A valid `hello` |
| `stalled` | The connection is taken and never answered |
| `saturated` | 503 "busy" after 10 s, its body a byte every half second |
| `ended` | 401 "not signed in" |
| `half` | `hello`'s headers and half its body, then a reset |
| `bodystall`, `gzip302` | A 200 whose body never comes; a "compressed" 302 whose body never comes (`phases.py`) |

### The W1 component, which is switched on (`NEXUS_MUSIC_NEXUS=1`) in every `new` run but one

- **The component's log.** It writes what it does to `NEXUS_TEST_NEXUS_LOG`: an account
  published, a look started and ended, an answer shown or dropped, a turn passed over for
  Offline mode.
- **The stuck modes** (`stalled`, `blackhole`, `saturated`): it looks again 0.2 s after each
  look ends (`NEXUS_TEST_PROBE_AGAIN_MS`), so that a look is being waited on all the time.

| Hook | |
|---|---|
| `held` | Its look's thread is held in a call that never returns (`NEXUS_TEST_HOLD_NEXUS`), from its first look on |
| `switch-off` | The switch is not set: nothing of the component may be made |
| `sabotage` | A deliberate fault, which the run must catch: the window's thread is held 0.3 s at each look |

The test hooks are read only by a development build.

## What a run asserts

**Each action is a step.** Before and after every action, from the component's first look of
that start on:

- with `held`, the held thread (by its id) is alive, and is the same thread after as before;
- in the stuck modes, a look is in flight. At the end of the run, every action is checked to
  have had a look waited on throughout, with no gap longer than 1 s between looks.

**Every run checks these outcomes:**

- **Ordering.** The component's first look of each start comes after Home is drawn and the
  queue is on screen.
- **The play log.** The profile starts with a play log that is not empty (`seed-plays.log`,
  three lines the released app wrote). At each start, before anything is played, the log is
  byte for byte what it was. What each start added is kept with only the times masked; the
  table compares it with the released app's.
- **Plays and the queue.**
  - A counted play reaches the server exactly once, and so does a play counted while the
    server was lost.
  - The queue after `kill -9` is the queue before it.
  - "Continue here" is offered for another device's queue, and taking it puts that queue
    in place.
- **Gapless.** Where each join's songs came from, read from the proxy's log:
  - streamed: both songs were streamed, the second fetched before the join;
  - cached: neither was asked of the server;
  - downloaded: nothing was streamed.
- **The Settings row.** For the account signed in it says exactly what the mode should
  give; with `held` or switched off, there is no row.
- **Requests under `/nexus/`.**
  - Exactly one per account published, and each a plain `GET` of `hello`.
  - None from the released app, from the component switched off, or with its look held.
  - At least three in the stuck modes.
- **Threads.** None of the component's when it is switched off; the held one still held
  after the account change.

**`suite.py table`** fails if:

- any W1 time is over the released app's slowest plus the slack;
- what a W1 run added to the play log is not what a released run added;
- a join is worse than the released app's.

### The tests, and where each is entered

| Test (design 5.3) | Entered at |
|---|---|
| `cold_start_publishes_the_saved_account` | The process started, to the first page drawn |
| `cold_start_puts_the_queue_back_and_plays` | `restore_queue` (queue on screen), then Play |
| `a_direct_play_starts_it_cold` | Space, as the first action of the second start |
| `two_songs_join_without_a_gap` | Two streamed with the next fetched ahead; two cached; two downloaded (a continuous sweep cut into songs) |
| `the_queue_is_changed_and_kept` | Add, "Play next", remove, clear; `kill -9` and start again; "Continue here" for another device's queue |
| `downloads_start_and_resume` | By hand (8 songs); a kept list that gained a song, at start-up; 80 asked for while the server is lost, then back |
| `offline_by_hand_and_by_loss` | The switch; the server lost as a network is lost (the proxy closes every connection) |
| `a_like_and_a_play_reach_the_music_server` | A like from the song's menu; a counted play; one counted while the server is lost |
| `a_play_is_counted_as_released` | The counted plays, and the log at each start |
| `an_account_change_is_not_held_up` | Sign out, then sign in as `guest` at the second origin |

Times are wall clock, seen through the inspection port or in the profile's files.

### The scenarios, and `phases.py`

- **`suite.py offline`.** Offline mode is on from the start: there must be no look and no
  request. At a start with it off there are looks; once it is switched back on, none.
  - The released app does not show the switch as on after a start in Offline mode, and
    pressing it asks for "on" again. So it cannot be switched off from that window, and the
    scenario starts the app again with it off.
- **`suite.py late-answer`.** Account A's look finds the backend, and its answer is held
  (`NEXUS_TEST_HOLD_NEXUS_RESULT`) until account B, at the other origin, is signed in. Then it
  is let through. It must never be shown under B, and B's own row must say "None found".
- **`phases.py`.** One look, stalled in each phase this machine can stall: name look-up,
  forward proxy, connect, TLS, headers, body, a compressed redirect, an abort, a process that
  never ends. Each must end by the 10 s deadline, or at once where it should.

**Controls.** Each check above has a control that puts back the fault it exists for, and must
fail (`run-pass.sh` checks that it does):

| Check | Its control |
|---|---|
| A whole run | `sabotage` (the window's thread held 0.3 s at each look) |
| `offline` | `E13A_SABOTAGE=1`: Offline mode ignored |
| `late-answer` | `E13A_SABOTAGE=1`: the generation ignored |
| `phases.py` | The body read before the status, as W1's first version did |

## Owed to a real Windows PC

These cannot be run on this Linux machine, and the component stays switched off for
everyone until they have been run (the main README, "Known, and accepted"):

- **Media keys and the tray.** A play started by the keyboard's media keys with the window
  hidden, from the notification-area icon, and from the taskbar's buttons, each with the
  component on and its look held. These are Windows-only code; here the suite enters at
  Space, which reaches the same `toggle_play`.
- **The look's own process on Windows.**
  - It is started with no console window, and its answer read from its pipe.
  - Killed at the deadline, Windows ends it (TerminateProcess).
  - A certificate check that fetches certificates of its own (Schannel's chain retrieval) is
    held up, and ended by that deadline.
- **The `phases.py` cases** on Windows, and one run of the suite against the shipped
  (release, `panic = "abort"`) build.

## Running it

```sh
python3 tests/e13a/suite.py populate      # once, with target/walk/nexus-music-old
tests/e13a/run-pass.sh 1                  # every mode, both builds in turn, the scenarios, the controls
python3 tests/e13a/suite.py table         # the numbers; exits 1 if W1 is over a bar
python3 tests/e13a/phases.py new          # one look, stalled in each phase
python3 tests/e13a/shots.py new           # the Settings row at the owner's window sizes
```

The builds are `target/walk/nexus-music-old` (the released app) and `-new`. Each is built
with `SLINT_EMIT_DEBUG_INFO=1 cargo build --features slint/mcp`, through `build-turn`.
