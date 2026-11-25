# How a person's own devices find and trust each other

The design the first slice is built to. The third version, brought up to its review;
the first two are in this file's history. Sections 1 and 2 are for the owner.

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
| A playlist named "Nexus Music link" | Does not exist | Exists. Hidden in new versions. Other music apps and Navidrome's own page show it, empty, with a line of letters as its description. Do not share it or make it public. If it is deleted your devices start afresh with a new one. |
| Removing a device | Not possible | No list and no Remove. "Reset linking" under Settings, Jam: every device still signed in rejoins by itself within two minutes. |
| The list under "Connect to a device" | Shows what each is playing at once | Shows the devices it has seen before at once, and what each is playing a moment later |
| A button pressed just as the other device was given something else to do | Acts on whatever is there | Does nothing, and shows what changed. Press again if you still mean it. |
| Two people steering one device at the same instant | Both act | One acts; the other is shown the result |
| Moving music to another device | This one stops, then the other starts | The other starts, then this one stops. If anything gets in the way you are told which of them is playing. |
| Your music server out of reach | Linking carries on | Devices stop seeing each other after ten minutes |
| Changing your password | Splits your devices until each signs in again | No effect once each has signed in again |
| A PC on 0.40.6 or earlier | Sees your other devices | Does not see devices on the new version, nor they it. Jams with guests work throughout. |

**The relay is updated first.** That is done for you; it ends any jam in progress.

**Once every device is updated,** in this order:

1. Switch "Link my devices" off on any device that cannot be updated.
2. Change your music-server password, and any other sign-in that reaches the account
   (an API key, a sign-in your proxy does for you). A fingerprint of the current
   password is in the relay's and the proxy's logs. A new one is safe from those
   lines; it does not undo what was already possible, or protect another account
   where you used the same password.
3. **Then "Reset linking".** The secret your devices made while the old password
   still worked may have been read by whoever had that password.
4. Give the relay a new key if the host link has been anywhere you do not trust,
   clear the proxy's logs for the relay's address, and switch old linking off in the
   relay.

**"Reset linking" is also needed** after the music server's data is restored from a
backup (the old secret comes back with it), and if the playlist was ever public; the
app does the second by itself when it sees it, and says so.

**If you lose a device:** change your music-server password, then "Reset linking", in
that order. Until you have done both, that device can see what your devices are
playing and steer them.

## 3. Beyond the brief, and kept by the review

- **A minute number beside the generation** (5.6). Without it a command the relay's
  path holds back stays good until someone next touches that device: a Play withheld
  in the evening could arrive at three in the morning.
- **The state says which command made it** (`by`, 5.5), so a lost answer can be told
  from someone else's choice.
- **A device's place at the relay is random for each run,** not derived from the
  secret: a restarted app has lost the token that protects its place.
- **A secret is used for at most ten minutes** without being read again.

## 4. Who can do what

"The relay's path" is the relay, Nginx Proxy Manager and Cloudflare: all three see
requests in the clear.

| Party | Today | Under the design |
|---|---|---|
| Someone with the logs for the relay's address | Tests guesses at the password offline. Joins a device's session by its id, watches it, and before 0.40.6 steers it. | Sees a room's name and that devices come and go. The name says nothing about the password or the secret, and the route needs the host key, which is not in a log. |
| The relay's path | Reads the owner word from any question: full control of every device. | Cannot read or forge. Can drop, delay and repeat. A repeat does nothing. A delayed command is carried out only if no choice has been made on that device since, and within two minutes. A state can be shown late. If it records traffic and later learns the secret, it reads all of it. |
| A guest in a jam | Steers the jam as the host allows | The same. Nothing a guest holds reaches devices' own route. |
| Someone else with the host key | Enters a room if they learn its name; collects the owner word there. | Without the room's name: can use up the relay's 20 rooms (section 6). With it, from a log: can take free places and fill the room's allowance, so devices are told "busy". Cannot read or steer. |
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

A **look** is: list the playlists; choose by the rule of `core/src/step.rs` (`find`:
the lowest id among those of that name whose owner the server states to be this
account); fetch that playlist; check what was fetched. The fetched playlist counts
only if its id is the one asked for, its name is the name, its **owner is stated** and
is this account, and it is **stated private** (`public` present and `false`).
Navidrome's JSON leaves `public` out when it is false, so that answer cannot state it;
its XML answer always writes the attribute, and the app asks for the playlist again in
XML and reads it there, from an "ok" answer about that id. An
answer that fails any of these, or leaves one out, is not used, is not written over,
and does not renew the ten minutes. A server that does not state these cannot be used
for linking, and the app says so. The name is added to the reserved names.

| The look finds | The device |
|---|---|
| No playlist of ours | Makes one, writes a new secret into it, and looks again from the start. It uses only what a look reads back, never what it wrote. |
| Ours, comment empty or not a record (a device stopped between making and writing) | **Repairs it:** writes a new secret into it and looks again. |
| Ours, with a later layout number | Leaves it alone. Linking is off until the app is updated. |
| Ours, readable | Uses the secret, and remembers the playlist's id for this sign-in. |
| A lower id of ours than the one remembered, the remembered one still there | Moves to the lower (two devices each made one; this is how they agree). |
| The remembered one gone | **Does not take up another playlist's old secret.** Writes a new secret into what `find` now gives, or makes one. |
| Ours, stated public | Makes it private, writes a new secret, and tells the person. |

Two devices that write at the same moment may use different secrets until the next
look.

- **Writing** is by form POST with the sign-in in the form too, so neither the secret
  nor a new address carrying it reaches a log. (Navidrome takes form POST for every
  call; checked in its source.) Every write sets the playlist private. Redirects are
  not followed.
- **When.** A look starts when linking is switched on or the app starts, two minutes
  after the last one finished, and on "Reset linking". **One look at a time for a
  sign-in.** The secret is held in memory only and is used for at most ten minutes,
  counted from the start of the look that read it, on a clock that counts sleep, and
  checked each time the secret is used.
- **"Reset linking"** writes a new secret into the same playlist and looks again.
  Other devices move at their next look.
- **A session** is one sign-in with one secret. When the secret changes or runs out,
  the person signs out, or the account changes, the session ends: looks, answers and
  commands in flight under it are dropped, and a command is carried out only if the
  session it arrived under is still the session (5.6).

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
  seconds without a poll; nothing else can take or replace it. An app that has lost
  its token, or is told 404, enters again **under a new place**.
- `from` is the sender's place as the relay knows it from the token. `seq` counts from
  1 for each place. `after` drops frames up to that number, and a number higher than
  any handed out is ignored.
- A poll is answered when a frame arrives, when the places change, or after 20
  seconds. A second poll for a place answers the first at once, empty. `after` is the
  last frame the app has **dealt with**; frames after it are sent again, so the app
  skips a `seq` it has dealt with. A new token starts again from `seq` 1.
- The relay keeps nothing on disk, and never logs a body, a token or the host key. It
  no longer prints the host key at start-up, whether set or made up; with none set it
  does not start.
- Statuses: 400 a room, place or body that is not well formed; 401 the host key; 404
  no such room, place or token; 409 the place is taken; 410 old linking is off; 411 a
  body that does not state its length; 413; 429 with `Retry-After`; 503 full.

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

**The total for the process:** at most 40 MiB of waiting frames (20 rooms of 2 MiB)
and 320 held polls. **Work in flight is bounded too:** the route has its own threads
and its own sweeper, so nothing it reads or writes can hold up a jam; at most 64
requests wait for those threads and at most 8 MiB of answers are being written at
once (a poll whose answer does not fit waits, with its frames, for the next round);
a write that makes no progress for 20 seconds is abandoned. A body must state its
length and is read only after the host key and token have been checked. (The HTTP
library gives no deadline on reading a body; a stalled one holds one of the route's
own threads, never a jam's.)

**A caller's address** is the connection's own, unless the connection comes from the
proxy named in a new setting, `JAM_TRUSTED_PROXY`; then it is the last address in
`X-Forwarded-For`, which that proxy wrote. IPv6 addresses are counted by their /64.

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

**Encodings,** all without padding, and refused unless they are the one canonical
spelling (decoding and encoding again gives the same text): a place and a room,
lower-case base32 (`a-z2-7`), 26 and 52 characters; a token, 43 base64url characters;
a nonce in `re` and `by`, 32; a device id, 22; a frame, base64url.

**If the system cannot give random bytes,** nothing is sent: no place, no nonce, no
secret is made any other way. Linking stops and says so.

### 5.5 Messages

A message is JSON of at most 210,000 bytes. Unknown fields are ignored.

**Asks** (kind 1):

| Message | Meaning |
|---|---|
| `{"t":"look","want":w}` | Tell me your state. `w` is `brief` (for the list), `full` (for steering: what a jam's state holds today), or `queue` (the whole queue, to take it over). |
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
| `ok`, `why` | In an answer to a command. `why` is `changed`, `busy`, `late`, `invalid` or `cannot`. |
| `device`, `name` | Who is speaking: its device id and the name the person gave it. |
| `at` | Its generation and minute now. |
| `busy` | A choice has been accepted and not yet carried out (5.6). |
| `by` | The nonce of the command that was last **carried out**, if nothing has been chosen since. |
| the rest | What it is playing, as much as was asked for. Texts are cut to 80 characters; a `full` state takes queue entries until it reaches 60,000 bytes and counts the rest in `more`. |

**What makes a state fresh.** Only an answer whose `re` is the nonce of a `look` the
device is waiting on, from the place it asked. Each waiting nonce is used once. Its
age is counted from when that look was sent. An answer to a command settles that
command and nothing else: it never replaces a newer state on the screen. Anything
else that opens is dropped.

**News is not in the first slice,** and when it is added it never makes a state
fresh. Until then a device shown on the screen is looked at every five seconds.

**Exact shapes for the first slice** (a message that breaks one is dropped by the
receiver, or answered `invalid` if it was an ask that opened):

| Message | Fields | At most |
|---|---|---|
| `look` | `t`, `want` = `brief` | 64 bytes |
| `do` | `t`, `do` = `pause`, `at` = two whole numbers; `with` absent or `{}` | 256 bytes |
| A brief tell | `device`, `name`, `at`, `playing` always; `re`, `ok`, `why`, `by`, `busy`, `title`, `artist` when they apply. `name`, `title`, `artist` cut to 80 characters by the sender and again by the receiver. | 4,096 bytes |

An answering device handles at most 20 asks a second and drops the rest.

### 5.6 The rule for commands

**A device has a generation,** a number that starts at 1 with the app. It advances:

- **when a choice is accepted:** at the press, before any work is done for it. A tap,
  a media key, a tray or taskbar button, a guest's authorised action in a jam, a
  command from another device, an explicit seek, mute and volume, setting or
  cancelling a sleep timer. A choice that changes nothing (Pause when paused) counts;
- **by itself, when what a command would act on changes:** the next song starting,
  autoplay adding songs, a followed playlist changing the queue, a timer's pause, the
  sound output being lost. Not the position ticking.

**Accepting and carrying out.** Accepting a choice *reserves* its generation, once.
Every later step that carries the choice out (a look-up's answer arriving, the
player being changed) first checks the reservation is still the device's generation,
and does nothing if it is not. Between the two the device is **busy**: its state
says so, `by` is empty, and a command from another device is refused `busy`. The
person at the device is never refused: their next press is simply a newer choice.

**And a minute:** whole minutes since the app started, on a clock that goes on
counting while the device sleeps (Windows: interrupt time; Android:
`elapsedRealtime`).

**The rule.** A command quotes the `at` of the fresh state it was made against. When
it arrives the receiving device checks, before any work:

- it is not busy (`busy`);
- the generation quoted is its generation now (`changed`);
- its minute now is the minute quoted, or the one after (`late`).

If any fails it does nothing and answers with its state. Otherwise the command is
accepted like any choice: its generation is reserved. **At the moment the player is
changed** the device checks the reservation, the minute, and that the session the
command arrived under is still the session (5.1). Then it carries the command out,
`by` becomes the command's nonce, and the answer says `ok`. A command carries out
its action on this device only: a Pause never goes on to a jam the device has joined.

So a repeat fails (its first delivery advanced the generation), a command held back
fails once anything is chosen there, a song changes, or two minutes pass, and a
command overtaken by the person at the device fails. **A Pause that crosses the start
of the next song is refused once like anything else,** and the sender looks again.
Nothing is remembered about past commands but `by`.

**The sending device:**

- sends a command only against a fresh state (5.5) that is not busy and is less than
  45 seconds old; otherwise it looks first;
- sends one command to a device at a time. A press made while one is unanswered waits
  (up to eight; a volume slider keeps only its latest), and is sent only after an `ok`,
  against the state in that answer. A refusal or an unknown outcome discards them;
- reads **`by` equal to its command's nonce as "done"**, whatever else the tell says;
- after 12 seconds with no answer, looks once and says what is true: `by` is its
  nonce, done; the generation is as it was, "That hadn't reached Study PC when
  checked" (it may still, within its two minutes); anything else, "Study PC changed.
  Check what is playing before you press again." It never sends a command again by
  itself, and an answer that arrives after this is still believed.

**What this costs.** Two people steering one device at the same instant: one is
refused and shown the result. A device someone is busy with refuses commands from
elsewhere while each choice lands: whoever is at the device wins. A command sent as
a song ends is refused once. After "not known" the person may press again and get
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

**Not in the first slice, and not settled beyond this.** An `ok` proves the other
device started that queue once. It does not say what it is playing now, so what the
person is told is one of: "started" (and what its next fresh state shows); "started,
not confirmed since"; "not known" (no answer and no look got through: either or both
may be playing). The pause that completes a handover is sent or made only within 30
seconds of the press, on either path. The promise that a device which cannot play is
left as it was needs the new queue to be made ready before the old one is stopped;
Windows stops first today.

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

A guest's authorised action reserves the generation as a press does, before any
look-up. A removal is applied before any question collected in the same poll. The
cases each app must pass, and the relay's changes for jams, are in version 2,
sections 7 and 8.

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

Not checked: that these build for Android and iOS.

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
| Review 2's sequence 14: a held Pause; meanwhile Play pressed at B on a list still being looked up | `busy` while the list is looked up, `changed` after: the press reserved the generation before anything played. The Play goes ahead. |
| A look during that look-up; a second device presses Pause | The state said busy, so the sender does not send against it and looks again. One that sent anyway is refused `busy`. |
| A look-up that answers after a newer choice | Its reservation is no longer the generation: it does nothing. |
| A Pause that crosses the start of the next song | `changed`, once. The sender looks again. |
| The secret is reset while a Pause is on its way, or between its arrival and the player | Sealed under the old secret, it does not open under the new; if it had arrived, the session has ended and it is not carried out. |
| A tell delivered again, or an old one in place of the answer | Its `re` is not waited on (each nonce is used once): dropped. The screen is not made fresh. |
| A reports "hadn't reached"; the command then arrives in time | Carried out. The late `ok` is believed and shown. |
| A choice at B while a command's work is under way | `changed` at the player. |
| Crossed presses from one device | Cannot arise: the second waits for the first's answer. |
| Two devices at the same instant | One `ok`; the other `changed`, with the result. |
| An old volume delivered again after a newer one | `changed`. The newer stands. |
| Answer lost, and something else chosen since | Not known; A says so and sends nothing. |
| The answer never came because the command never arrived | "Hasn't reached"; if it then arrives within two minutes and nothing was chosen, it is carried out. |
| Moving: the other cannot play; the answer is lost; the source is touched first | As in the table of 5.7. |
| Taking: the other is touched before its `pause` | `changed`. Both play; told. |
| B restarts | Its place is new: frames sealed for the old one do not open. A's command is not known. |
| A restarts after sending | B may still carry it out within the rule. Its answer goes to a place that is gone; A's new run learns what B is doing from its first look. |
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

The cases the script's model leaves out (the sender restarting, an arrival after the
timeout was reported, a tell replayed, the secret changing at the player, a Play
still looking its songs up) are tests of `core/` and of the app itself.

## 10. Moving from today's scheme

A new version never works out the password's hashes, never sends or accepts the owner
word, and opens no private session.

1. **The relay,** with the private route and old linking still on.
2. **The Windows app,** then **the phone,** each linking only where the relay says
   `"link": 4`.
3. **The owner's steps** of section 2, in that order: old linking off on every
   device, the password, then "Reset linking". An old PC goes on sending the
   password's fingerprint in an address, its name and what it plays to the old room,
   and keeping a session anyone with the logs can join and watch, until it is updated
   or its linking is switched off; the relay cannot stop any of that.

## 11. The first slice

1. **The relay's route** (5.3) with its limits and bounds. Jams untouched; old
   linking still works and can be refused.
2. **`core::link`:** the secret's record and the look; the derivations; the frame;
   the two messages; the rule as a small state machine (reserve, busy, carry out,
   refuse, the minute, `by`), with the byte vectors and every sequence of section 9.
3. **Two Windows copies,** behind a switch with no setting: each shows what the other
   is playing, and Pause on one pauses the other, with the generation reserved at
   every way into the player.

Seen working against a stand-in for a hostile relay path. After it: the other
commands, news, moving music, "Reset linking" on the screen, the phone.

## 12. Where it is weakest

1. **The secret is in plain sight:** every app signed in to the account shows the
   playlist's description, Navidrome's page does, and its database and backups hold
   it. I see no better place on a Subsonic server.
2. **Signing in once is joining,** and reset is everyone's, a stolen device's too.
3. **One nonce reused breaks the seal for everyone;** recorded traffic is readable
   later by whoever gets the secret.
4. **The generation is only as good as the app's count of choices.** A way into the
   player that does not reserve it is a hole. Every one has to be found and tested, on
   both apps.
5. **After a lost answer, if anyone chose since, the outcome is not known.**
6. **A busy device starves steering from elsewhere.**
7. **The minute and the ten minutes need a clock that counts sleep.**

The relay's path can still hold a command up to two minutes, show a stale state until
the next look, and stop linking altogether.
