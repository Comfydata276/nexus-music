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

### The W1 component, switched on (`NEXUS_MUSIC_NEXUS=1`) in every `new` run but one

- **Its log.** It writes what it does to `NEXUS_TEST_NEXUS_LOG`: an account published, a look
  dispatched (on the window's thread, saying Offline mode was off), its process spawned (by
  process id) and ended or killed, an answer shown or dropped, a look caught as "not ours",
  a turn passed over for Offline mode.
- **The stuck modes** (`stalled`, `blackhole`) use three test hooks. (`saturated` is not
  one: its "busy" status, after 10 s, is an answer, classified before the body is read.)
  - its deadline is made an hour long (`NEXUS_TEST_DEADLINE_MS`, `NEXUS_TEST_PROBE_WHOLE_MS`),
    so that one look is held for as long as the stand-in holds it;
  - the startup trigger waits for the account to be published, then starts its first look;
  - the suite can start one on demand (`NEXUS_TEST_TRIGGER`).

  In `blackhole` the proxy itself ends a held look at 75 s. A look older than 35 s is let end
  before an action, and a new one started.

| Hook | |
|---|---|
| `held` | Its look's thread is held in a call that never returns (`NEXUS_TEST_HOLD_NEXUS`), from the first look after account publication |
| `switch-off` | The switch is not set: nothing of the component may be made |
| `sabotage` | A deliberate fault, which the run must catch: the window's thread held 0.3 s at each look |

The test hooks are read only by a development build.

## What a run asserts (and exits 1 if not)

**The same fault through every action.** Each action is one step.

- **Before it:** the fault in force, by name: the held thread's id, or the held look's
  process id (one is started if none is).
- **After it:** the same thread or the same process still alive, its look not ended.
- **The first action of a start:** the first look for the published account is held before
  the suite goes on to the next action.
- **Every action ran under the fault:** "19 of 19".

**The music server through every action.** Each API request the app made was answered 200
within 1 s, except while the music server is lost on purpose. The slowest is held to the
released app's slowest in the table.

**Every run checks these outcomes:**

- **Ordering, unfaulted.** The component's first look of a start comes after Home and the
  queue are usable.
- **The play log.** The profile's log is not empty (`seed-plays.log`). At each start, before
  anything is played, it is byte for byte what it was. What each start added, through to
  its end, is kept with only the times masked; the table compares it with the released
  app's.
- **Plays and the queue.**
  - A counted play reaches the server exactly once, and so does a play counted while the
    server was lost.
  - The queue after `kill -9` is the queue before it.
  - "Continue here" is offered and taken.
- **Gapless sources**, from the proxy's log: streamed with the next fetched before the join;
  cached not asked of the server; downloaded not streamed.
- **The row, the requests, the threads.**
  - The row for the account signed in.
  - Requests under `/nexus/`: one per account published unfaulted, each a plain `GET` of
    `hello`; none switched off or held.
  - The component's threads.

**`suite.py table` fails** on any of these:

- a W1 time over the released app's slowest plus its slack;
- a play log added to otherwise than the released app's;
- a worse join;
- **missing evidence:** fewer than two runs of each mode for either build, or of each hook,
  or a run lacking any required measure (`REQUIRED`, Continue here included).

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

- **`suite.py offline`: Offline mode is a boundary, at a look's dispatch on the window's
  thread.**
  - On from the start: nothing dispatched, nothing asked.
  - At a start with it off: looks come, every request one dispatch's.
  - Switched on by hand, with looks 0.2 s apart: nothing dispatched after the switch is seen
    on the disk. There is no grace period.
  - The released app cannot switch Offline mode off from the window after a start in it.
    The scenario starts it again with it off instead; that released-app fault is being fixed
    elsewhere, and once it is, the scenario can press the switch.
- **`suite.py sign-out`: a look due as the account signs out.** The window's account handler
  is made 0.3 s late (a test hook), and looks come 0.1 s apart. Over ten sign-outs:
  - no look is dispatched for the account that went;
  - the component catches one at dispatch at least once (it said so ten times in ten);
  - every request belongs to a dispatch.
- **`suite.py late-answer`.** A's answer, found and held, is let through after B (at the
  second origin) is signed in. It is never shown under B, and B's row says "None found".
- **`phases.py`.** One look, through the component's own supervisor, stalled in each phase
  this machine can stall: name look-up, forward proxy, connect, TLS, headers, body, a
  compressed redirect, an abort, a process that never ends.
  - Each phase must show it was entered: the stand-in asked, the listener reached, the name
    server asked, a connection tried, or the look run to its deadline.
  - Each must end by the 10 s deadline, or at once.
  - Also checked: the look's process is given descriptors 0, 1 and 2 only; and switched
    off, `--nexus-probe` and `--nexus-probe-timed` are not obeyed (the app starts, and
    nothing is asked).

**Controls.** Each check has a control that puts back the fault it exists for. The control
must fail, and for that reason (`run-pass.sh`):

| Check | Its control | It must fail with |
|---|---|---|
| A whole run | `sabotage` (the window's thread held 0.3 s a look) | a time over its bar |
| `offline` | Offline mode ignored | looks dispatched after Offline mode was on |
| `sign-out` | The account published ignored | looks dispatched for the account that signed out |
| `late-answer` | The generation ignored | A's answer shown under B |
| `phases.py` | The body read before the status | the right answer, but not in time |

## Owed to a real Windows PC

The component stays switched off for everyone until these have been run on a real Windows
PC with the release build. The steps are in the main README, "Known, and accepted":

- a held look ending quietly, with no handle left behind;
- a look's process dying quietly;
- the certificate check held to the deadline;
- quitting ending a look;
- media keys with the window hidden;
- the tray and taskbar under a live fault;
- switched off being as released.

## Running it

```sh
python3 tests/e13a/suite.py populate      # once, with target/walk/nexus-music-old
tests/e13a/run-pass.sh 1                  # every mode, both builds in turn, the scenarios, the phases, the controls
python3 tests/e13a/suite.py table         # the numbers; exits 1 if W1 is over a bar
python3 tests/e13a/phases.py new          # one look, stalled in each phase
python3 tests/e13a/shots.py new           # the Settings row at the owner's window sizes
```

The builds are `target/walk/nexus-music-old` (the released app) and `-new`. Each is built
with `SLINT_EMIT_DEBUG_INFO=1 cargo build --features slint/mcp`, through `build-turn`.
