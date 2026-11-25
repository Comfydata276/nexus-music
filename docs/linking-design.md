# How a person's own devices find and trust each other

A design, for the owner to decide on and a reviewer to attack. Nothing here is built.
The third version. The first two are in this file's history. Sections 1 and 2 are for
the owner.

## 1. In brief

Today a device proves it is yours with values worked out from your music-server
password, and sends them through the relay, the proxy and Cloudflare, all of which can
read them. Under this design:

- **One random secret for your devices, kept on your music server** in a hidden
  playlist. Any device signed in to your account reads it. Nothing is worked out from
  the password.
- **Everything your devices say to each other is sealed with that secret.** The relay
  and the proxies carry it and cannot read it or make it up.
- **One rule for commands.** A command names the state of the other device it was made
  against. If that device has been given any other choice since, it does nothing and
  says what is true now.

**Given up, knowingly:**

- **Anything that can sign in as you is one of your devices,** and so is whoever runs
  the music server. Nothing tells you when a device joins.
- **One device cannot be told from another.** A device with the secret can speak as
  any of them.
- **One device cannot be removed.** "Reset linking" starts all of them afresh.
- **No forward secrecy.** Someone who records the relay's traffic and later learns the
  secret can read all of it, back to when the secret was made.
- **The relay sees** which devices are there, which talks to which, when, and how much.

## 2. For the owner: what changes

| | Today | Proposed |
|---|---|---|
| Adding a device | Sign in, paste the host link | The same |
| A playlist named "Nexus Music link" | Does not exist | Exists. Hidden in new versions. Other music apps and Navidrome's own page show it, empty, with a line of letters as its description. Do not share it or make it public. If it is deleted your devices make a new one. |
| Removing a device | Not possible | No list and no Remove. "Reset linking" under Settings, Jam: every device still signed in rejoins by itself within two minutes. |
| The list under "Connect to a device" | Shows what each is playing at once | Shows the devices it has seen before at once, and what each is playing a moment later |
| A button pressed just as the other device was given something else to do | Acts on whatever is there | Does nothing, and shows what changed. Press again if you still mean it. |
| Two people steering one device at the same instant | Both act | One acts; the other is shown the result |
| Moving music to another device | This one stops, then the other starts | The other starts, then this one stops. If anything gets in the way you are told which of them is playing. |
| Your music server out of reach | Linking carries on | Devices stop seeing each other after ten minutes |
| Changing your password | Splits your devices until each signs in again | No effect once each has signed in again |
| A PC on 0.40.6 or earlier | Sees your other devices | Does not see devices on the new version, nor they it. Jams with guests work throughout. |

**The relay is updated first.** That is done for you; it ends any jam in progress.

**Once every device is updated,** in this order: switch "Link my devices" off on any
device that cannot be updated; change your music-server password (a fingerprint of the
current one is in the relay's and the proxy's logs, and a new password is safe from
those lines; it does not undo what was already possible, or protect another account
where you used the same password); give the relay a new key if the host link has been
anywhere you do not trust; clear the proxy's logs for the relay's address; switch old
linking off in the relay.

**If you lose a device:** change your music-server password, then "Reset linking", in
that order. Until you have done both, that device can see what your devices are
playing and steer them.

## 3. Where this goes further than the brief

Four things the brief did not ask for, or asked for differently. Each can be cut, at
the cost stated.

1. **A minute number beside the generation** (5.6). With the generation alone, a
   command the relay's path holds back stays good until someone next makes a choice on
   that device. A PC left paused overnight has made none: a Play withheld in the
   evening could be delivered at three in the morning. The minute ends a held command
   within two minutes, for one number read off the device's own clock and no table.
2. **The state says which command made it** (`by`, 5.5). Without it a device whose
   answer was lost cannot tell "my Skip was done" from "someone else skipped", and the
   person is invited to press again. One remembered value, not a record of results.
3. **A device's place at the relay is random for each run of the app,** not derived
   from the secret. A place is protected by the token it was opened with, and a
   restarted app has lost that token: a derived place would lock a device out of its
   own place for 45 seconds after every restart.
4. **A secret is used for at most ten minutes** without being read again. Otherwise a
   device that can reach the relay and not the music server answers to an old secret
   for ever after a reset.

## 4. Who can do what

"The relay's path" is the relay, Nginx Proxy Manager and Cloudflare: all three see
requests in the clear.

| Party | Today | Under the design |
|---|---|---|
| Someone with the logs for the relay's address | Tests guesses at the password offline. Joins a device's session by its id, watches it, and before 0.40.6 steers it. | Sees a room's name and that devices come and go. The name says nothing about the password or the secret, and the route needs the host key, which is not in a log. |
| The relay's path | Reads the owner word from any question: full control of every device. | Cannot read or forge. Can drop, delay and repeat. A repeat does nothing. A delayed command is carried out only if no choice has been made on that device since, and within two minutes. A state can be shown late. If it records traffic and later learns the secret, it reads all of it. |
| A guest in a jam | Steers the jam as the host allows | The same. Nothing a guest holds reaches devices' own route. |
| Someone else with the host key | Enters a room if they learn its name; collects the owner word there. | Without the room's name: can use up the relay's rooms (6, bounded). With it, from a log: can take free places and fill the room's allowance, so devices are told "busy". Cannot read or steer. |
| Another account on the music server | Nothing | Nothing while the playlist is private, which the apps keep it. If it is found public, the secret must be taken as read by everyone on the server: the app makes it private and resets. |
| The music server, its administrator, its database and backups; anyone signed in as the account, including another music app; someone with the logs for the music server's address (the sign-in is reusable and travels in every address) | Has the password: full control | Reads the secret: sees what every device plays, steers them, speaks as any of them, and reads recorded traffic. Nothing tells the person. |
| A stolen device still signed in | Full control until the password is changed | The same, until the password is changed **and** linking is reset; then for up to two minutes more (ten for a device that cannot reach the music server). |
| Someone on the home network, relay or music server reached over plain HTTP | Sees the host key, the room and the owner word | Relay: sees the host key and places' tokens; can crowd a room and take a device's frames, cannot read them. Music server: reads the sign-in and the secret: full control. |

## 5. The design

### 5.1 The secret

A playlist named **"Nexus Music link"**, with no songs, private, owned by the account.
Its comment:

```json
{"nexus_music_link": 1, "secret": "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}
```

`secret` is 32 random bytes, base64url without padding.

- **Which playlist:** the rule of `core/src/step.rs` (`find`): the lowest id among
  those of that name whose owner the server states to be this account, looked for
  afresh at every look. A server that does not state owners cannot be used for
  linking, and the app says so. The name is added to the reserved names.
- **Reading it** follows `core::record`: a later layout number is never written over
  and linking is off until the app is updated; a comment that is not this record
  means linking is off, and only "Reset linking" replaces it.
- **Making it:** a device that finds none makes one, then looks again and uses what
  `find` gives. Two devices that each made one therefore agree at their next look; a
  spare playlist is left behind.
- **Writing** is by form POST, so the secret is not in an address. (Navidrome takes
  form POST for every call; checked in its source.) Every write sets the playlist
  private. A playlist found public is made private and given a new secret.
- **A look** happens when linking is switched on or the app starts, every two minutes,
  and on "Reset linking". A device holds the secret in memory only and uses it for at
  most ten minutes after the look that read it.
- **"Reset linking"** writes a new secret into the same playlist and reads it back.
  Other devices move at their next look.

### 5.2 What is derived from it

HKDF-SHA256 (RFC 5869), no salt, 32 bytes out, with these as `info`:

| Value | `info` | Use |
|---|---|---|
| Sealing key | `nexus-music link 4 seal` | 5.4 |
| Room | `nexus-music link 4 room` | The room's name at the relay: the 32 bytes in lower-case base32, 52 characters |

A device's **place** is 16 random bytes made each time the app starts, written in
lower-case base32 (26 characters). Its **device id** is 16 random bytes made once for
the installation and kept in its settings; it is not secret and travels only sealed.

### 5.3 The relay's private route

Required: an app links only through a relay whose `GET /api/hello` says `"link": 4`,
and otherwise says "This relay needs updating before your devices can be linked."
Every call needs the host key. There are no guests, joins, peeks, covers or names on
this route, and it shares no code with jams. Answers carry `Cache-Control: no-store`.

| Call | Sends | Answer |
|---|---|---|
| `POST /api/link/{room}/enter` | `{"place": p}` | `{"token": t, "v": n, "places": [p, …]}`. 409 if `p` is taken. |
| `GET /api/link/{room}/poll?v=n&after=s` | header `X-Nexus-Link: t` | Held up to 20 s: `{"v", "places", "frames": [{"seq", "from", "frame"}], "more"}` |
| `POST /api/link/{room}/send` | header; `{"to": p, "frame": f}` | `{"ok": true}`; 404 if `p` is not there |
| `POST /api/link/{room}/leave` | header | `{"ok": true}` |

- `t` is 32 random bytes. A place belongs to its token until it leaves or goes 45
  seconds without a poll; nothing else can take or replace it.
- `from` is the sender's place as the relay knows it from the token. `seq` counts from
  1 for each place. `after` drops frames up to that number, and a number higher than
  any handed out is ignored.
- The relay keeps nothing on disk, and never logs a body, a token or the host key
  (today it prints a key it made up at startup; that goes).

| Limit | Value | When passed |
|---|---|---|
| Rooms | 20 | 503 |
| Rooms one caller's address may hold places in | 8 | 503 |
| Places in a room | 16 | 503 |
| The whole body of a `send` | 288 KiB | 413 |
| Frames waiting for one place | 32, or 512 KiB | 429 to the sender |
| Frames waiting in one room | 2 MiB | 429 |
| A whole `poll` answer | 512 KiB | The rest waits: `"more": true` |
| Held polls for one place | 1 | The earlier is answered empty |
| `enter` from one address | 30 a minute | 429 |
| `send` from one place; from one address | 20 at once then 5 a second; 40 a second | 429 |

**The total for the process:** at most 40 MiB of waiting frames (20 rooms of 2 MiB),
320 held requests, and answers of at most 512 KiB each. Each room's allowance is its
own, so one room cannot use another's. A caller's address is taken from
`X-Forwarded-For` only when the request comes from the proxy named in a new setting,
`JAM_TRUSTED_PROXY`.

**Old linking** (`/api/room/…`) answers 410 when a new setting, `JAM_OLD_LINK`, is
off. It is on at first. The switch cannot end an old PC's private session, which is
started by the same call as a jam: that ends only when linking is switched off on that
PC, or the PC is updated.

### 5.4 A frame

Everything sent through the route is a frame, base64url without padding:

| Bytes | |
|---|---|
| 4 | `"NML4"` |
| 1 | kind: 1 an ask, 2 a tell |
| 24 | nonce, random for every frame |
| the rest | the message, sealed: XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha-03) with the sealing key and the nonce |

The associated data is `"NML4"`, the kind, the sender's place (16 bytes) and the
recipient's place (16 bytes). The receiver takes the sender's place from the relay's
`from` and uses its own as the recipient's, so a frame the relay delivers to another
place, hands back, relabels, or carries over to a later run of the app does not open.
A frame that does not open is dropped and never answered.

### 5.5 Messages

A message is JSON of at most 210,000 bytes. Unknown fields are ignored.

**Asks** (kind 1):

| Message | Meaning |
|---|---|
| `{"t":"look","want":w}` | Tell me your state, and go on telling me for 90 seconds. `w` is `brief` (for the list), `full` (for steering: what a jam's state holds today), or `queue` (the whole queue, to take it over). |
| `{"t":"do","do":name,"at":[g, m],"with":{…}}` | A command, made against generation `g` and minute `m`. |

Commands are today's: `play`, `pause`, `next`, `previous`, `seek`, `jump`, `move`,
`remove`, `clear`, `shuffle`, `repeat`, `volume`, `add`, and `adopt` (take this queue
and play it). None is a toggle. `core/` checks each has the fields it needs, by the
function that checks a guest's (7).

**Tells** (kind 2), one shape for an answer and for news:

```json
{"re": "…", "ok": true, "why": "…",
 "device": "…", "name": "Study PC", "at": [8, 12], "by": "…",
 "playing": false, "title": "…", "artist": "…"}
```

| Field | |
|---|---|
| `re` | The nonce of the ask this answers. Absent in news. |
| `ok`, `why` | In an answer to a command. `why` is `changed`, `late`, `invalid` or `cannot`. |
| `device`, `name` | Who is speaking: its device id and the name the person gave it. |
| `at` | Its generation and minute now. |
| `by` | The nonce of the command that made this generation, if a command did. |
| the rest | What it is playing, as much as was asked for. Texts are cut to 80 characters; a `full` state takes queue entries until it reaches 60,000 bytes and counts the rest in `more`. |

**What the asking device accepts.** An answer only if `re` is the nonce of an ask it
is waiting on and it came from the place it asked. News only from a place in the room,
and only if its generation is not below the last seen from that place.

**What the answering device bounds.** It answers at most 20 asks a second and keeps
at most 16 watchers. News is sent when its generation changes, at most four times a
second.

### 5.6 The rule for commands

**A device has a generation:** a number that advances with every choice it accepts,
at the moment the choice is made, before any work is done for it. A tap on its own
screen, a media key, a guest's ask in a jam, a command from another device, a volume
change: each is a choice. It is the number of the app's own order of the person's
choices. It starts at 1 when the app starts.

**And a minute:** whole minutes since the app started, on a clock that goes on
counting while the device sleeps (Windows: interrupt time; Android:
`elapsedRealtime`).

**The rule.** A command quotes the `at` of the last state its sender saw. The
receiving device carries it out only if:

- the generation quoted is its generation now; and
- its minute now is the minute quoted, or the one after.

It checks both when the command arrives, before doing any work for it. If either
fails it does nothing and answers with its state: `changed` for the generation, `late`
for the minute. If both hold, the command is a choice like any other: the generation
advances there and then, and the command has that number as its place.

**At the moment the player is changed** it checks again: the generation is still the
command's own number (nothing has been chosen since), and the minute still holds. Then
it carries the command out, `by` becomes the command's nonce, and the answer says
`ok`.

So a repeat fails (its own first delivery advanced the generation), a command held
back fails once anything else is chosen there or two minutes pass, and a command
overtaken by the person at the device fails. Nothing is remembered about past commands
but `by`.

**The sending device:**

- looks every 30 seconds while it shows a device, and sends a command only against a
  state less than 45 seconds old by its own clock; otherwise it looks first;
- sends one command to a device at a time. A press made while one is unanswered waits
  (up to eight; a volume slider keeps only its latest), and is sent only after an `ok`,
  against the state in that answer;
- reads **`by` equal to its command's nonce as "done"**, whatever else the tell says;
- after 12 seconds with no answer, looks once and says what is true: `by` is its
  nonce, done; the generation is as it was, "That hasn't reached Study PC"; anything
  else, "Study PC changed. Check what is playing before you press again." It never
  sends a command again by itself.

**What this costs.** Two people steering one device at the same instant: one is
refused and shown the result. A device someone is busy with refuses commands from
elsewhere while each choice lands: whoever is at the device wins. A song ending is not
a choice, so a Next that crosses the end of a song skips the one that has just begun,
as it does on the device itself. After "not known" the person may press again and get
two.

**What still needs time:** the minute; the ten minutes a secret is used; and how long
a sender waits before it stops waiting. No clock is compared between devices and no
wall clock is used.

### 5.7 Moving music

Two ordinary commands. A handover keeps both generations as they were when it began.

- **To the other device.** This device (the source) notes its own generation, then
  sends `adopt` with its queue, place and position. The other checks it can play
  before changing anything, takes the queue, starts, and answers `ok` once sound has
  started. On that answer (or `by` in a state, within 30 seconds of the press) the
  source pauses, **only if its own generation is still the one it noted.**
- **From the other device** ("Play here"). This device looks with `want: queue`,
  takes the queue and starts. Once its own sound has started it sends the other
  `pause`, **quoting the `at` of the state it took the queue from.**

| What happens | Result | The person is told |
|---|---|---|
| The other cannot play | The source plays on; the other is as it was | Why |
| No answer, and the look shows it did not start | The source plays on | "That hasn't reached Study PC" |
| The source was touched before the answer | Both play | "Study PC is playing your queue. This device was left as it is." |
| The other device was touched before its `pause` | Both play | The same, the other way round |
| The other started and then stopped (lost the sound, crashed) after answering | Both silent | Its state, when it next arrives |

**Big queues.** One message. A queue that does not fit 210,000 bytes is cut from its
far end, keeping the song to start with: "The next 3,000 of 5,000 songs were moved."

### 5.8 Sizes

| What | Worst case | As it travels | Against |
|---|---|---|---|
| A full frame | 210,000 bytes of message | 280,060 characters; about 280,150 bytes as a `send` | 288 KiB |
| A poll answer | One full frame and thirty-one small ones | about 330 KiB | 512 KiB; the phone refuses answers over 768 KiB |
| `adopt` or a `queue` state, 3,000 songs | Ids of 64 characters, the longest accepted: 201 KB. Navidrome's 22: 75 KB | one frame | 210,000 bytes: about 3,100 songs at the longest ids, 8,400 at Navidrome's |
| Eleven other devices, list open | 11 looks out, 11 states in, every 30 s | about 20 KiB | |

### 5.9 Finding each other

A device enters the room and looks at every other place. A place is shown once it has
answered, under the name in its answer; names are remembered against device ids so the
list shows them at once next time. After "Reset linking", a device still on the old
secret stays in the old room, steerable by any holder of that secret, for up to two
minutes (ten if it cannot reach the music server). Then it leaves.

## 6. What a holder of the host key can do to the relay

Bounded, not ended. Everyone who can start a jam holds the host key.

- **Use up the rooms:** 20 names entered from three addresses. A room with a device in
  it is unaffected; one whose devices were all away is not found until the other
  stops. The cure is a new host key.
- **With a room's name** (from a log): take its free places and fill its 2 MiB, so
  devices are told "busy". No live place can be taken and no frame read. The cure is
  "Reset linking", which moves the room, and a new host key.

## 7. Jams with guests, and the rule both apps must meet

Unchanged from version 2 and separate from all of the above: a jam's guests have the
link, no account and no secret, and nothing in a jam is hidden from the relay. A
device no longer keeps a private session; a jam is the only kind.

**The rule,** for a guest's question and a device's command alike:

1. **Validate:** every field its action needs is there and of the right kind.
2. **Authorise:** this asker may ask this now. For a device: the frame opened and the
   rule of 5.6 holds.
3. Only then give it **a place in the order of choices.** A question that only reads
   (a search, a cover) takes no place.
4. Only then do **work** for it, within stated bounds.
5. **At the moment it is carried out, authorise again:** the guest is still in the jam
   and steering is still on; for a device, 5.6 again.

A removal is applied before any question collected in the same poll, and the host's
app carries out nothing from a guest it is removing while the relay confirms.

The cases each app must pass, and the relay's changes for jams (lock after removal,
uncollected work taken back, waiters for a cover, the size of a host's poll answer,
one cover-id rule in `core/`), are as in version 2, sections 7 and 8.

## 8. What goes in `core/`, and what it uses

`core::link` holds the derivations, sealing and opening a frame, every schema and
size, the rule of 5.6 on both sides, and what the sender concludes after no answer.
The app supplies random bytes, the minute, the generation, the server's answers, and
the player.

| Crate | Pin | Checked (manifests, repositories, RustSec; nothing built) |
|---|---|---|
| `chacha20poly1305` | `=0.10.1` | RustCrypto. Has `XChaCha20Poly1305`. Released 2022 and widely used; no advisory against it. NCC Group reviewed an earlier release (0.3, 2020), not this one. 0.11.0 came out in June 2026; the older line is chosen for its years of use. |
| `hkdf` | `=0.13.0` | RustCrypto, March 2026. Goes with the `sha2` 0.11 the app has. |
| `zeroize` | as brought in | The sealing key and the secret are wiped when replaced. |

**The secret is not stored on any device,** so there is no key to protect in
Credential Manager or the Keystore. It is in the app's memory while linking is on. On
Android it also passes through Kotlin as text on its way to `core/`, where it cannot
be wiped.

Not checked: that these build for Android and iOS; sizes (my estimate is under 0.1 MB
for each architecture).

## 9. The reviewers' sequences, replayed

`docs/linking-vectors.py` holds a model of 5.6 and runs each of these; `core/`'s
tests must decide them the same way. B is the device being steered.

| Sequence | Decided |
|---|---|
| Double skip: Next is done; the relay delivers the frame again | The second is `changed`. One skip. |
| Double skip: the answer is lost; A looks | `by` is A's nonce: done. A does not ask again. |
| Double skip: a faulty A asks anew against what it last saw | `changed`. One skip. |
| Held Play: the person paused at B meanwhile | `changed`. B stays paused. |
| Held Play: nothing done at B; delivered a minute on | Carried out, up to two minutes late. |
| Held Play: delivered two minutes on | `late`. Nothing. |
| A command that arrives in time and whose work outlasts the minute | `late` at the player. Nothing. |
| Review 2's sequence 14: a held Pause; meanwhile Play pressed at B on a list still being looked up | `changed`: the press advanced the generation before anything played. The Play goes ahead. |
| A choice at B while a command's work is under way | `changed` at the player. |
| Crossed presses from one device | Cannot arise: the second waits for the first's answer. |
| Two devices at the same instant | One `ok`; the other `changed`, with the result. |
| An old volume delivered again after a newer one | `changed`. The newer stands. |
| Answer lost, and something else chosen since | Not known; A says so and sends nothing. |
| The answer never came because the command never arrived | "Hasn't reached"; if it then arrives within two minutes and nothing was chosen, it is carried out. |
| Moving: the other cannot play; the answer is lost; the source is touched first | As in the table of 5.7. |
| Taking: the other is touched before its `pause` | `changed`. Both play; told. |
| B restarts | Its place is new: frames sealed for the old one do not open. A's command is not known. |
| A restarts after sending | B may still carry it out within the rule. Its answer goes to a place that is gone. |
| A removed device (review 1), an undone removal (both) | There is no removal. See section 1. |

**Bytes.** The script checks its HKDF against RFC 5869 and its XChaCha20-Poly1305
against the draft's vectors before printing these.

| | |
|---|---|
| Secret | `202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f` |
| Sealing key | `f910379eb11ec698962649fedca639eb6abac0f94011cf859909f270c9e3180f` |
| Room | `gmpsssbzsbysp73rfdjtchkolqpwl2uvsjde7s2hiioejtponbaq` |
| PC's place; phone's place | `101112131415161718191a1b1c1d1e1f`; `f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff` |
| Nonce | `a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7` |
| `{"t":"look","want":"brief"}`, PC to phone, as sent | `Tk1MNAGgoaKjpKWmp6ipqqusra6vsLGys7S1trcUuaE5S13eLMstpiVnQszIhnutDXboHaDw4qjJQ5-Tu8us8U7tlcaHYClu` |

The script prints three more frames, and checks that a frame is refused when handed
back, delivered elsewhere, relabelled, passed off as the other kind, cut short, sealed
under another secret, or changed in any one bit.

## 10. Moving from today's scheme

A new version never works out the password's hashes, never sends or accepts the owner
word, and opens no private session.

1. **The relay,** with the private route, the jam changes and old linking still on.
2. **The Windows app,** then **the phone,** each linking only where the relay says
   `"link": 4`.
3. **The owner's steps** of section 2. An old PC goes on sending the password's
   fingerprint in an address, and keeping a session anyone with the logs can join and
   watch, until it is updated or its linking is switched off; the relay cannot stop
   either. That is why the devices come before the password.

Until then, on a PC with 0.40.6 and linking on, someone with the logs can still test
password guesses offline, and join its session to watch what it plays, fetch its
covers and crowd it. They cannot steer it without the owner word, which the relay's
path sees and a log does not hold.

## 11. What to build first

1. **The relay's route** (5.3) with its limits and tests. Jams untouched.
2. **`core::link`:** the derivations, frames, the two messages, the rule, with the
   byte vectors and the sequences of section 9.
3. **Two Windows copies,** behind a setting that is off and not shown: read or make
   the secret, enter the room, show what the other is playing, and Pause it.

It proves that frames pass the owner's relay, proxy and Cloudflare within the limits;
that Pause is refused when the other copy was touched first, with real processes and
a relay stand-in that holds and repeats frames; that the minute survives the PC
sleeping; and it gives the measured size. After it: the other commands, moving music,
reset, the phone.

## 12. My own attack on it

**Where one secret fails.**

1. **The comment is in plain sight.** Every app signed in to the account shows the
   playlist and its description; Navidrome's page does; its database and backups hold
   it. A screenshot of that page gives away every device. A password is at least never
   displayed. This is the weakest point, and I see no better place on a Subsonic
   server to keep it.
2. **Signing in once is joining.** A friend's phone signed in to play one album is a
   device until the password is changed. So is a family member who shares the
   account: they see what every device plays and can steer it. That is true today as
   well.
3. **Reset is everyone's.** A stolen device can reset too, and reads each new secret
   until the password is changed. Hence the order in section 2.
4. **One nonce reused breaks the seal for everyone,** and every device shares the key.
   With 24 random bytes that needs a broken source of randomness on some device.
5. **Recorded traffic is readable later** by whoever gets the secret: what was played,
   when, on which device, and the queues.

**Where one generation number fails.**

6. **It is only as good as the app's count of choices.** A choice that reaches the
   player without advancing the number (a media key, the car, a notification button,
   a timer) is a hole of the sequence-14 kind. Every way in has to be found and
   tested, on both apps. This is where I expect the next review to look.
7. **It cannot say what happened after a lost answer if anyone chose since** (5.6).
   `by` covers the usual case and not this one.
8. **A busy device starves steering from elsewhere.** A parent pausing a PC that a
   child is clicking through may need several presses.
9. **The minute depends on a clock that counts sleep.** With the wrong clock a held
   command survives a night's sleep on that device.
10. **A song's end is not a choice,** so a held Next can be timed to skip the song
    after. Small.

**What the relay's path can still do:** hold a command up to two minutes; show a
stale state, or a device that has gone, until the next look; stop linking altogether.

**Whether this is the right trade.** I think it is, for this household. What version
2 had beyond this was forward secrecy, telling devices apart, and removing one. The
second could not mean much once the music server was the authority, since anyone
signed in could enrol; the third could not be made reliable on a stock Navidrome, as
the review showed. That leaves forward secrecy as the real loss, against a party (the
owner's own proxy, or Cloudflare) that would have to record everything and then also
get into the music server. For that the design is a third the size and has one rule to
test. I would not cut the minute or `by`; without the first I would call it a mistake.
The thing I am least comfortable with is item 1.
