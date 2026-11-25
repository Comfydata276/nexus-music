# How a person's own devices find and trust each other

A design, for the owner to decide on and a reviewer to attack. Nothing here is built.
It covers the Windows app, the Android app and the jam relay, in terms of the shared
library (`core/`), as of 0.40.5.

The first two sections are for the owner. The rest is for the reviewer.

## 1. Before the design: what I found reading the code

The problem is larger than the brief in two places and smaller in one.

**Larger: a linked device can be steered today without the owner word.** Each device
keeps a private session at the relay, and that session is an ordinary jam. Anyone who
has its id can join it as a guest, with no key of any kind (`guest_join` in
`relay/src/relay.rs` asks for none). A guest sees what is playing and the queue. With
"Guests can steer" on, which is the default, a guest can also pause, skip, seek,
reorder and add songs (`jam_asked` in `src/jam.rs` lets any guest steer when
`jam_guests_control` is set). The session's id is in the path of every request the
device and its steerers make, so it is in the proxy's access log. A line of that log is
enough to steer the device.

**Larger: "the relay" is three parties, and two of them see everything.** The owner's
relay sits behind Nginx Proxy Manager and Cloudflare. Both end the TLS connection, so
both see every header and body in the clear: the host key, every session's secret,
every guest's token, the owner word. The host key therefore cannot be what devices
trust each other by. It is already in the hands of the parties in question.

**Smaller: nothing in the fix needs the relay to change first.** The relay passes a
device's status, a guest's question and the host's answer through without reading
them. The new protocol fits inside those, so new apps work with the relay as it is
deployed today. Updating the relay is still worth doing (section 8), but the owner's
step on his server is not on the critical path.

## 2. For the owner: what is proposed, and what you decide

**The proposal in four sentences.** Each device makes a key of its own the first time
it links, and keeps the secret half on the device. It puts the public half in a small
hidden playlist on your music server, the way the sync record is kept, so your other
devices can read it. Everything devices say to each other through the relay is then
sealed with those keys: the relay, the proxy and Cloudflare carry it but cannot read
it, change it, or send it again later. Nothing worked out from your music-server
password is sent anywhere.

**What changes for you.**

| | Today | Proposed |
|---|---|---|
| Adding a device | Sign in, paste the host link | The same. No new step. |
| Other devices when one is added | Nothing | A notice: "Study PC joined your devices" |
| Seeing and removing devices | Not possible | A list under Settings, Jam, with Remove |
| Losing a device | Change your password | Change your password, then Remove it |
| Changing your password | Splits your devices until each signs in again | No effect on linking |
| A playlist named "Nexus Music devices" | Does not exist | Exists. Hidden in new versions; old versions and other apps show it, empty. Leave it alone. |

**What you must do on your server.** Nothing, for this to be safe. Two things are
worth doing once every device is updated:

1. Change your music-server password. A fingerprint of the current one has been
   written to the relay's, the proxy's and Cloudflare's logs for as long as linking
   has been on. Changing it makes those log lines worthless.
2. Update the relay when its new version is out (pull the image, restart it; this
   ends any jam in progress). It closes a few doors that the apps no longer rely on.

**What stops working with old versions.** A PC on 0.40.5 or earlier and a device on
the new version do not see each other. Old PCs go on linking with each other, and go
on sending the password's fingerprint, until they are updated or "Link my devices" is
switched off on them. Jams with guests work throughout, on every mix of versions.

**What I am asking you to decide.** Whether your music server may be what says which
devices are yours.

- *As proposed:* it is. Anything that can sign in as you, or that runs the music
  server, can add a device and then see what your devices are playing and steer them.
  That party can already read and change everything else in your account, including
  the queue a paused device picks up. You run the server yourself. Adding a device
  needs no step, and every other device says so when it happens.
- *The stricter alternative:* a new device is not trusted until you confirm it on one
  you already have, by checking that both show the same six digits. This protects
  your devices' playback from someone who has your password or your server. It costs
  a step every time, and needs an already-linked device running and in reach.

I recommend the first now. The design leaves room for the second as a setting later
without changing anything on the wire (section 10, alternative B).

## 3. Today, exactly

All of this is `src/jam.rs` and `relay/src/relay.rs` at 0.40.5. The phone's copy
(`jam/Room.kt`, `jam/Guest.kt`) does the same and is switched off.

1. A device with a host link and "Link my devices" on works out two values
   (`account_words`):
   `room = hex(SHA-256("nexus-music room\n" + username + "\n" + password))` and
   `word = the first 32 hex digits of SHA-256("nexus-music owner\n" + …)`.
2. It opens a private session: `POST /api/host/start` with the host key, getting a
   session id and secret. This is the same call that starts a jam.
3. It enters the room: `POST /api/room/{room}/enter` with the host key and
   `{id, name, status}`. `status` holds the session id, the song's title and artist,
   whether it is playing, and what it is steering. It long-polls
   `GET /api/room/{room}/poll` for the others.
4. To steer device B, device A joins B's session as a guest
   (`POST /api/jam/{session}/join`, no key), then sends questions
   (`POST /api/jam/{session}/ask`) with `data.owner = word`. B compares the word with
   its own and, if equal, lets A do everything: `hello`, `queue`, `load` (replace the
   queue), `volume`, and every control. A reads B's state as any guest does.
5. Starting a jam makes the private session public. Ending it opens a new private one.

What is wrong with it, beyond section 1:

- The room's name is a fast, unsalted hash of the password, sent in a URL. Whoever
  has a log line can test guesses offline and knows when one is right.
- The word is the same for ever and travels in the clear inside questions. Whoever
  sees one question has the run of every device from then on.
- Nothing ties a room entry to a device. Whoever holds the host key and the room's
  name can list a fake device; a real device that picks it sends it the word.
- Username and password are all that goes into the hash, so two accounts on different
  servers with the same pair share a room and a word, and "Tester" and "tester" do not.

## 4. Who can do what

"Relay" means the relay program, Nginx Proxy Manager and Cloudflare: all three see
requests in the clear. "Logs" means access logs alone: addresses and paths, not
headers or bodies.

| Party | Today | Under the design |
|---|---|---|
| Someone with the logs | Tests password guesses offline. Joins any device's session by its id; reads what plays; steers it if guests may steer. | Learns that devices exist and when they are active. The room's name says nothing about the password. Cannot join a device's session once the relay is updated (it then needs the host key); before that, can join and sees only sealed text and refusals. |
| The relay | All of the above, and reads the owner word from any question. From then on: reads and replaces queues, plays, pauses, sets the volume, on every device, for ever. | Sees which devices exist, when they talk, and how much. Can drop or delay what it carries: a held-back command is refused once it is five minutes old or once a later one from the same device has arrived. Cannot read, forge, alter or replay anything between devices. |
| A guest in a jam | Steers the jam as the host allows. Cannot reach other devices. | The same. A jam and a device's own session are now separate sessions, and nothing a guest holds opens the second. |
| Another account using the same relay (has the host key) | Can enter a room if it knows its name; can then list fake devices and collect the word. | Can enter a room only by learning its name, which is random and kept on the music server. Entered, it sees sealed text. Can fill the room's twelve places (section 12, question 6). |
| Another account on the same music server | Nothing | Nothing, while the playlist is private (the apps keep it so). If it were public: reads the device names and public keys, and could work out the room's name. |
| The music server, its administrator, anyone with the account's password | Has the password, so works out the room and word: full control of devices. | Adds a device of its own to the list: full control of devices from then on, and every real device shows the notice. Cannot read what was said before it joined. |
| Someone who steals one device | Has the password and word: full control until the password is changed. Other devices cannot tell. | Is a device until removed. Password changed and the device removed, it is out: its key is no longer on the list, and it cannot write itself back. |
| Someone on the home network, relay reached over plain HTTP | Sees the host key, the room, the word. | Sees the host key and session tokens (unchanged, and enough to start jams on the relay). Sees nothing of what devices say to each other. |

## 5. The design

### 5.1 What a device holds

A device makes one X25519 key pair per account it is signed in to, the first time
linking is switched on there. The random bytes come from the operating system and are
handed to `core/`; the library has no source of randomness of its own.

- The **secret key** stays on the device: in Windows Credential Manager beside the
  password, and on Android in a file sealed by the Android Keystore, as the host key
  is. It is deleted at sign-out and when linking is switched off.
- The **public key** goes on the list (5.2).
- The **device id** is not chosen: it is the first 16 bytes of
  `SHA-256("nexus-music device 2\n" + public key)`, written in lower-case base32 (26
  characters). A device cannot claim another's id without having its key.

### 5.2 The list of devices, on the music server

The list is the comment of a playlist named **"Nexus Music devices"**, owned by the
account, with no songs. It is found, read and written by the rules the sync record
already follows in `core/` (`step::find`, `record::read`), restated here because each
one matters:

- The playlist is the one with the lowest id among those of that name whose owner the
  server **states** to be this account. One whose owner is not stated is not ours:
  nothing is read from it or made beside it, and linking says so under its setting.
- Only an empty comment is "no list yet". A comment that is not empty and is not a
  list is left exactly as it is, and linking is off until it is one.
- The layout number is read first. A later layout is read for what is understood and
  never written. Fields this version does not know are written back as they were.
- The name is reserved: a playlist of the person's own cannot be given it.
- The playlist is kept private. A device that finds it public sets it private
  (`updatePlaylist` with `public=false`) before using it.

The comment:

```json
{"nexus_music_devices": 1,
 "account": "AAECAwQFBgcICQoLDA0ODw",
 "devices": [
   {"key": "hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo", "name": "Study PC", "added": 1795000000}
 ],
 "removed": [
   {"key": "3p7bfXt9wbTTW2HC7OQ1Nz-DQ8hbeGdNrfx-FG-IK08", "at": 1795003600}
 ]}
```

- `account`: 16 random bytes, made by the device that creates the list, never changed.
  It names the room (5.3). It is not a secret from the music server.
- `devices`: at most twelve (the relay's limit for a room). `key` is the public key,
  `name` what the device is called, `added` when, in the server's time.
- `removed`: keys taken off the list by the person, kept 90 days.

What a device may write, and nothing else:

1. **Add itself**, if its key is in neither `devices` nor `removed`.
2. **Rename itself.**
3. **Remove a device** on the person's say: its entry moves to `removed`.
4. **Take itself off** at sign-out or when linking is switched off (its entry is
   dropped; no entry in `removed`).
5. Drop entries of `removed` older than 90 days.

The server has no conditional write, so two devices adding themselves at once can
lose one entry. Each device therefore reads the list again after writing, and at
every later read adds itself again if it is missing and not removed.

A device that finds **its own key in `removed`** stops linking, deletes its key, and
says "This device was removed from your devices." Linking again from there makes a new
key. This is why the order for a lost device is password first: a thief who still has
the password can link again with a new key, and every other device will say so.

**Which keys a device trusts.** The keys in `devices` as it last read them, kept on
the device with the time of that read, so linking works while the music server is out
of reach. It reads the list again at start, every minute while the server answers,
and at once (at most every ten seconds) when the room or a question shows a device id
it does not know. A key that a read shows gone or removed is dropped at that read.

The first time a device sees a key it has not seen before, other than on its own
first read, it tells the person: "‘Name’ joined your devices."

### 5.3 Meeting at the relay

The room's name is the lower-case base32 of
`SHA-256("nexus-music room 2\n" + account)`: 52 letters and digits. It is random to
anyone without the list, has nothing of the password in it, and cannot be the same
for two accounts. It is not a secret that anything rests on: everything said in the
room is sealed.

The relay's room calls are used as they are (`enter`, `poll`, `leave`, with the host
key). What a device sends changes:

```json
{"id": "<its device id>", "name": "",
 "status": {"v": 2, "for": {"<a device id>": "<sealed presence>", "…": "…"}}}
```

One sealed presence for each other device on its list. Names are not sent to the
relay; they come from the list.

### 5.4 Sealing

Everything one device says to another is sealed the same way. The primitives are
X25519, HKDF-SHA256 and XChaCha20-Poly1305, each used as specified in RFC 7748, RFC
5869 and draft-irtf-cfrg-xchacha-03.

**The key.** For a message of a given kind from one device to another:

```
shared = X25519(my secret key, their public key)        refused if all zero
key    = HKDF-SHA256(salt = "nexus-music link 2",
                     ikm  = shared,
                     info = kind (1 byte) + sender's public key + receiver's public key)
```

The kind and the direction are in the key. A message of one kind cannot be opened as
another, and what B sent to A cannot be handed back to B as A's.

**The header**, 61 bytes, sent in the clear and covered by the seal:

| Bytes | Field | |
|---|---|---|
| 4 | `"NML2"` | This scheme, this version |
| 1 | kind | 1 presence, 2 question, 3 answer, 4 state |
| 16 | from | Sender's device id |
| 16 | to | Receiver's device id |
| 16 | session | The session nonce of the device being steered (5.6); zeros for presence |
| 8 | n | Sender's time in milliseconds (5.7), big-endian |

**The message** is `header + nonce + sealed`, in base64url without padding, where
`nonce` is 24 random bytes and
`sealed = XChaCha20-Poly1305(key, nonce, plaintext, associated data = header)`.
The plaintext is JSON.

**Opening**, in this order, refusing at the first failure:

1. The text is base64url, at least 61 + 24 + 16 bytes, and begins `"NML2"`.
2. The kind is the one expected where it arrived.
3. `to` is this device's id.
4. `from` is the id of a key this device trusts (5.2).
5. `session` and `n` pass the rule for this kind (5.7).
6. The seal opens with the key for this kind, from that sender, to this device.

A message that fails any of these is treated as if nothing had arrived. Nothing in an
unsealed reply is ever taken as success.

### 5.5 Presence

The plaintext of a presence:

```json
{"session": "<relay session id, or null>", "nonce": "<16 bytes, base64url, or null>",
 "title": "…", "artist": "…", "playing": true, "steering": "<a name, or null>"}
```

`session` and `nonce` say where and how the device can be steered. Both are null
while it is steering another device or is in a friend's jam, as today. `title` and
`artist` are cut to 80 characters each, so that twelve devices' worth fits what the
relay takes. A device sends presence when what it says changes and at least every
thirty seconds, as today.

A device lists another only if a presence from it opens. An entry in the room that
does not open (a stranger, a removed device, a device whose key this one has not read
yet) is not shown and triggers one read of the list.

### 5.6 Steering a device

A device open to steering keeps a **private session** at the relay, started as today
but marked private (`POST /api/host/start` with `{"name": "", "private": true}`; the
deployed relay ignores the mark). When it opens the session it makes a **session
nonce** of 16 random bytes. The nonce goes out only inside its sealed presence.

**A jam is a different session.** Starting a jam opens a second, public session for
guests; ending the jam ends that one. A device's own devices always steer it through
the private session, whether or not it is hosting a jam. A private session answers no
unsealed question at all, whatever the "Guests can steer" setting says.

To steer B, device A:

1. Takes B's session id and session nonce from B's presence.
2. Joins B's session at the relay (`join`, with the host key), getting a guest token.
3. Sends a question. On the wire every question is the same shape:

   ```json
   {"action": "hello", "data": {"sealed": "<sealed question>"}}
   ```

   The relay sees a `hello` each time. The real question is the plaintext:

   ```json
   {"action": "pause", "data": {}}
   ```

   with header kind 2, from A, to B, session = B's session nonce, n = A's time.
4. B opens it (5.4), does what is asked, and answers through the relay's `reply` with
   `{"sealed": "<sealed answer>"}` as the data. The answer's header has kind 3, from
   B, to A, the same session, and **the question's n**. Its plaintext is
   `{"ok": true, "data": …}` or `{"ok": false, "error": "…"}`.
5. A accepts an answer only if it opens and its n is that of a question A is still
   waiting on. Each question is answered once.

The questions are today's: `hello`, `queue`, `load`, `volume`, the controls, `add`,
`remove`, `search`. `hello` does nothing but mark A as steering.

**State.** While at least one device is steering it, B posts its state to the private
session as

```json
{"v": 2, "for": {"<A's device id>": "<sealed state>"}}
```

one sealed copy for each steering device. The plaintext is the state it posts today
(what is playing, the queue, position, volume). The header has kind 4, from B, to A,
B's session nonce, n = B's time. A works out the position from n, not from the
relay's own "age".

**Size.** A question must fit the relay's 256 KiB once sealed and encoded, so `core/`
caps a question's plaintext at 180 KiB and trims a handed-over queue from its far end
to fit, keeping the song to start with.

### 5.7 Fresh, once, and in order

**Time.** `n` is the sender's clock in milliseconds, set beside the music server's as
the sync work already does (`core::time`): the device's clock plus the difference it
last measured from the server's `Date`. A device whose clock is more than five
minutes from the server's does not link, and says so, by the rule sync already has.
Where the server sends no `Date`, or has not answered yet in this run, the device's
own clock is used as it is. A sender never uses the same n twice towards one device:
if the clock has not moved on, it uses the last n plus one.

| Kind | Accepted only if | What that stops |
|---|---|---|
| Presence | n is within five minutes of the receiver's time, and newer than the last presence accepted from that sender | An old presence shown as current |
| Question | `session` is the receiver's current session nonce; n is within five minutes of the receiver's time; n is newer than the last question accepted from that sender in this session | A question from an earlier session; a question sent again; a question held back and released late |
| Answer | `session` is the nonce of the session being steered; n is that of a question still waiting | An answer to something else; an answer given twice |
| State | `session` as for an answer; n is newer than the last state accepted | An old state shown as current |

A device remembers the last question's n for each sender only for as long as its
session lives. That is enough, because a new session has a new nonce, and a question
made for the old one names the old one.

When B refuses a question that opened but was not newer, or was too old, it says so
in a sealed answer: `{"ok": false, "again": {"after": <last n>, "now": <B's time>}}`.
A then sets its next n above `after` and asks once more. If `now` shows the two clocks
are more than five minutes apart, A tells the person which device's clock is wrong
and does not ask again.

**What a relay that misbehaves can still do.** Drop anything. Deliver a question up
to five minutes late, once, provided no later question from the same device has got
through first. Show a steering device a state up to the last one it accepted. It
cannot make a device do anything its owner's devices did not ask for.

### 5.8 What the relay still learns

- The room's name, and so which devices belong together.
- Each device's id, when it is present, and when it changes what it says.
- Which device steers which, when, and how large each question, answer and state is.
  A handed-over queue is large and a pause is small; a state is posted at each change
  of song, so the lengths of songs show. Nothing is padded.
- The host key, the sessions' secrets and tokens, as now.

It does not learn device names, what is playing, the queue, or which question was
asked.

## 6. What it costs

**Primitives and crates**, all pure Rust, all building for Windows, Android arm64 and
x86_64, and iOS:

| For | Crate | Note |
|---|---|---|
| X25519 | `x25519-dalek` 2 (on `curve25519-dalek` 4) | The `static_secrets` feature; no tables needed |
| HKDF-SHA256 | `hkdf`, `sha2` | The Windows app carries `sha2` already |
| XChaCha20-Poly1305 | `chacha20poly1305` | |
| base64url | `base64` | Already in the tree, through `ureq` |
| base32 | Written in `core/`, about twenty lines | With vectors |

No signatures are used, so no Ed25519. No randomness crate in `core/`: the app hands
in the bytes.

From memory, and to be confirmed before these are adopted: `chacha20poly1305` was
reviewed by NCC Group and `curve25519-dalek` by Quarkslab; `hkdf` and `sha2` are
widely used and I know of no formal review.

**Size.** Not measured: no build was allowed for this task. I expect 0.1 to 0.2 MB
added to the Android library for each architecture and about the same to the Windows
`.exe`. The first build should measure it.

**Per request, on a phone.** One X25519 for each other device, once a run, kept
afterwards: tens of microseconds. Sealing or opening a message: microseconds, for
messages up to a few hundred kilobytes. Presence is sealed once for each other device
at most every thirty seconds. None of this is noticeable beside the two held requests
a linked phone already keeps open at the relay, which this design does not change.

## 7. Moving from today's scheme

**A new version does not speak the old scheme at all.** It never works out or sends
the password's hashes. It never sends or accepts the owner word. Its private session
refuses every unsealed question. An old device and a new one are in different rooms
and do not see each other.

**Order.** Any order is safe, because no step adds exposure and new apps do not need
a new relay.

1. *Windows.* Each PC stops leaking as it is updated. Until all are, updated PCs link
   with each other and old ones with each other. The release notes say to update
   every PC, and that "Link my devices" can be switched off on one that cannot be.
2. *The phone,* when jams and linking are switched back on there. It has never
   shipped the old scheme switched on.
3. *The relay,* at any point. It needs the owner's step on his server.
4. *The owner,* once every device is updated: change the music-server password, and
   clear what logs the proxy keeps for the relay's address.

**Jams with guests** use calls this design does not touch, so they work at every
step, with one change in the apps: a jam is now a session of its own (5.6). A guest's
link and the page are unchanged.

**What changes in the relay** (none of it needed for the apps to be safe):

- A session started with `"private": true` can be joined only with the host key, has
  no `peek` and no covers, and is never listed by name. This is what stops a log line
  from being enough to connect to a device's session at all.
- A room's `status` may be 16 KiB (twelve devices' sealed presences fit in 8 KiB, with
  little to spare).
- Later, on a date the owner chooses: refuse room names in the old form (64
  hexadecimal digits; the new form is 52 base32 characters), with a sentence telling
  an old app to update. This does not stop an old app sending the old name; only
  updating it does.

## 8. Jams with guests

A jam is a different trust problem and stays one. The link is what lets a guest in;
guests have no account and no keys; the page is served by the relay, so nothing in a
jam is or can be hidden from the relay. Nothing of the account's (its keys, its list)
is used anywhere on a guest's path, and no sealed question is ever sent to a jam's
session.

From the review's other findings, what belongs in the protocol and what does not:

| Finding | Where it belongs |
|---|---|
| A removed guest comes back with the same link | **Protocol.** The host can lock a jam: `POST /api/host/{jam}/lock`, after which the relay takes no new guests. Remove then means what it says once the jam is locked. The apps' wording ("Remove" against "Remove and lock") is theirs. |
| Work already handed over still acts after a removal | **Relay.** Removing a guest also takes back their questions that the host has not collected. |
| The limit on callers trusts `X-Forwarded-For` | **Relay,** with a setting naming the proxy it is behind. |
| Unlimited waiters for one cover; covers nobody advertised | **Relay** for the cap. **`core/`** for one rule saying which cover ids exist (today the relay, Windows and the phone each have their own), and for "a host serves only covers its session has shown". |
| A missing `seek.to` seeks to zero, and the like | **`core/`:** one function that reads a question and refuses it unless every field its action needs is there. Both apps' hosts call it before doing anything. |
| A 504 after the host has already acted | **Each app** and the page: say the outcome is unknown and refresh before offering to try again. |
| A guest's question cancels the host's pending Play; work budgets on the phone; the car and other controllers; the exported service; the host key paired with the wrong relay | **Each app's own.** Not protocol. |

## 9. What goes in `core/`

One module, `core::link`, with no clock, no randomness and no I/O of its own. Both
apps call it; neither writes any of this itself.

| Piece | Functions | Handed in |
|---|---|---|
| Identity | Make a key pair; the device id of a public key | 32 random bytes |
| The list | Read a comment (a list, empty, unreadable, later layout); which playlist is ours; what to write to add, rename, remove, leave; whether this device was removed; which keys are new since the last read | The server's answers, the time |
| The room | Its name from `account` | |
| Sealing | Seal and open one message of a kind | The keys, session, n, 24 random bytes, the time |
| Presence, question, answer, state | The plaintext of each as a type; the header rules of 5.7 as one function per kind, returning the reason for a refusal | Last n accepted |
| Questions | Read a question and check its fields (also used for guests' questions) | |
| Sizes | The cap on a question; trimming a handed-over queue | |

The relay shares what it can by depending on `core/` for the rules that are not
secret: which cover ids exist, the size limits, the forms of a room's name. Its image
is built from `relay/` alone today, so its Dockerfile and workflow would take `core/`
as well.

**Test vectors.** `docs/linking-vectors.py` works these out with Python's
`cryptography` package, after checking itself against the published vectors of RFC
7748 and the XChaCha draft. `core/`'s tests must give the same bytes.

The two devices are RFC 7748's Alice (a PC, asking) and Bob (a phone).

| | |
|---|---|
| PC secret key | `77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a` |
| PC public key | `8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a` |
| PC device id | `prdexlo623udlxwdqh5y52qjru` (`7c464badded6e835dec381fb8eea098d`) |
| Phone secret key | `5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb` |
| Phone public key | `de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f` |
| Phone device id | `gk3ynum4d4ldkn3efbsseuprye` (`32b786d19c1f1635376428652251f1c1`) |
| Shared (X25519) | `4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742` |
| `account` | `000102030405060708090a0b0c0d0e0f` |
| Room | `vzuzg5btepm5heqxrn4d3po4xcungjjrcnn4p5l43lhci3mrb2ja` |
| Key, question, PC to phone | `e320b63e655967447f3f22cbe266048c63384119a5b69d594d93d1e10d2b6678` |
| Key, question, phone to PC | `1e790f433246e85300fb75134f51a7459f2082f509bc790de0f54159846c2f22` |
| Key, presence, PC to phone | `7190cd4629057907677429f8fcc3d2c5ee6a6d98beda9b2105c6f6c234839001` |
| Key, answer, PC to phone | `0868ba236a38853ac50df2d1c6258df4943abb0572b96d0e0354ee58fac740ec` |
| Key, state, PC to phone | `317242f9cfc2e451c822dd74b53fa0e49ac23cce258a23fb87e0fd290818e130` |

A question, PC to phone:

| | |
|---|---|
| Session nonce | `f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff` |
| n | `1795000000123` |
| Nonce | `404142434445464748494a4b4c4d4e4f5051525354555657` |
| Plaintext | `{"action":"pause","data":{}}` |
| Header | `4e4d4c32027c464badded6e835dec381fb8eea098d32b786d19c1f1635376428652251f1c1f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff000001a1ee565e7b` |
| On the wire | `Tk1MMgJ8Rkut3tboNd7DgfuO6gmNMreG0ZwfFjU3ZChlIlHxwfDx8vP09fb3-Pn6-_z9_v8AAAGh7lZee0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV2WFJ30nCOUcKQgMWg86ViQHAJKOXlYNgolltO4tjvH-OOXZloQbOkYjh9ht` |

And the cases that must be refused, each a test: the message with any one bit changed
in the header, the nonce or the sealed part; cut short; with `to` another device;
with a kind other than the one expected; with another session nonce; with n equal to
or below the last accepted; with n more than five minutes from the receiver's time
either way; from a key not on the list; from a key in `removed`; the phone's own
answer handed back to it as a question; a public key for which `shared` is all zeros.

## 10. The alternatives, and why not

**A. The relay's host key in a keyed hash.** Cheap, and every device has the host
link. Rejected on two grounds. Anything keyed that still starts from the password
breaks the rule that nothing worked out from it leaves the device. And the host key
is held by exactly the parties to be defended against (section 1), and by everyone
given the host link.

**B. A pairing secret, passed from device to device by a code.** Stronger than the
proposal against one party: the music server, or someone with the password. The
costs: a step for the person at every device, with an existing device in reach; a
camera and a decoder on the phone to scan a long code, or a short typed code with a
password-authenticated exchange (more cryptography to get right, and no crate for it
that I would call well reviewed); a shared secret to change on every device when one
is lost. The useful part of B survives as an option on top of the proposal: a setting
"Ask me before a new device is linked", under which a device trusts only keys the
person has confirmed by comparing six digits worked out from both public keys. That
changes which keys a device trusts (5.2) and nothing on the wire.

**C. One secret key for the whole account, shared between devices.** One sealed
presence instead of one for each device, and any device could read the room. But any
device could then speak as any other, a removed device keeps the key until every
other device has a new one, and passing the key to a new device needs the sealing of
5.4 anyway. With at most twelve devices the saving is not worth it.

**D. A handshake for each steering session (Noise KK).** Standard, and it adds two
things 5.4 lacks: what was said stays secret if a device's key is stolen later, and a
stolen key cannot be used to pose as others *to* its own device. Neither matters much
here (what was said is which songs played; the thief of a device holds that device).
It costs a handshake to redo whenever either end restarts or the relay drops a
message, and state on both ends that today's questions do not need. I chose single
sealed messages that each stand alone. If the reviewer weighs it the other way, D
replaces the question, answer and state kinds and leaves the list, the room and
presence as they are.

**E. Signatures (Ed25519) with separate encryption.** Needed only if a third party
had to check who said something. Nobody does; a key shared by two devices proves to
each that the other spoke.

**F. A new message path in the relay, addressed device to device.** Cleaner than
riding in a jam's calls, but it is new relay code on the critical path, and the
owner's server step would have to come first. The calls that exist carry sealed text
unchanged.

## 11. What the reviewer should attack first

1. **Trusting the music server for the list** (section 2's decision, and 5.2's read
   and write rules, which have no conditional write under them).
2. **Single sealed messages without a handshake** (5.4, 5.7): the key for a pair of
   devices never changes, so everything rests on the header checks being complete.
3. **Time as the only freshness** (5.7): five minutes, measured against the music
   server's `Date`, with "the clock is taken to be right" when a server sends none.
4. **Removal** (5.2): a removed device is refused by another only after that other
   has read the list again, which needs the music server.

## 12. What I could not settle

1. **Whether five minutes is the right window.** With clocks set beside the server's
   it could be one or two. It is five so that a server that sends no `Date` still
   works, and because that is the rule sync already has.
2. **Whether to seal the state in a jam's own session too,** for the host's own
   devices. As designed they never use that session, so the question does not arise;
   if the apps find they need the jam's guest list on a steering device, it travels
   in the private session's state.
3. **Lock, or a new link, after removing a guest.** Lock is one call and no change to
   links. A new link for those still in the jam is friendlier and changes the link's
   form, the page and the parser in `core/`.
4. **Sizes.** Estimated, not measured.
5. **Servers other than Navidrome.** The design needs a playlist's owner to be stated
   and its comment to be writable. Where they are not, linking is unavailable and
   says so. I have not checked other servers.
6. **Filling a room.** Anyone with the host key who learns a room's name can take its
   twelve places with entries that open for nobody. The relay cannot tell them from
   real ones without knowing the keys. Having the relay hold the account's public
   keys would fix it and would tell the relay more than it knows now; I left it out.
7. **Passing the host link to a new device through the list,** sealed to its key, so
   that a second device needs nothing pasted. It would remove today's step. It would
   also hand the relay's key to whatever joins the list. Not proposed; worth the
   owner's opinion.
