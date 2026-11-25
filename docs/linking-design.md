# How a person's own devices find and trust each other

A design, for the owner to decide on and a reviewer to attack. Nothing here is built.
It covers the Windows app, the Android app and the jam relay, in terms of the shared
library (`core/`), as of 0.40.5.

This is the second version. The first was reviewed
(`review-linking-design-result.md`); what changed is listed in section 17. Sections 1
and 2 are for the owner. The rest is for the reviewer.

## 1. What reading the code found

**A linked device can be steered today without the owner word.** Each device keeps a
private session at the relay, and that session is an ordinary jam. Anyone who has its
id can join it with no key, see what is playing, and (with "Guests can steer" on, the
default) pause, skip and reorder. The id is in the path of every request, so it is in
the proxy's access log. A stop-gap for this is being built as 0.40.6; section 4 says
what it leaves open.

**"The relay" is three parties.** The relay sits behind Nginx Proxy Manager and
Cloudflare. Both end the TLS connection and see every header and body. The host key
is therefore already held by the parties to be defended against, and cannot be what
devices trust each other by.

## 2. For the owner: what is proposed

**In four sentences.** Each device makes a key of its own the first time it links,
and keeps the secret half on the device. It puts the public half in a small hidden
playlist on your music server, the way the sync record is kept, so your other devices
can read it. When one device wants to see or steer another, the two set up a private
channel through the relay using those keys; the relay, the proxy and Cloudflare carry
it but cannot read it, change it, or play it back later. Nothing worked out from your
music-server password is sent anywhere.

**What changes for you.**

| | Today | Proposed |
|---|---|---|
| Adding a device | Sign in, paste the host link | The same. No new step. |
| When a device is added | Nothing | Every other device says "Study PC joined your devices" |
| Seeing and removing devices | Not possible | A list under Settings, Jam, with Remove |
| The list under "Connect to a device" | Shows what each is playing at once | Shows the devices at once, and what each is playing a moment later |
| Moving music to another device | This one stops, then the other starts | The other starts, then this one stops. You may hear both for a second. If the other cannot start, this one carries on. |
| A button pressed while the other device changed | Acts on whatever is there now | Is refused, and the screen shows what changed. Press again if you still mean it. |
| Your music server out of reach | Linking carries on | Devices stop seeing each other after ten minutes, until it is back |
| Changing your password | Splits your devices until each signs in again | No effect on linking |
| A playlist named "Nexus Music devices" | Does not exist | Exists. Hidden in new versions; old versions and other apps show it, empty. Leave it alone. |

**What you must do on your server.** One thing, and it comes first: update the relay
(pull the new image and restart it; that ends any jam in progress). New linking is
not switched on in any app until the relay it talks to has been updated, and the apps
say so if it has not.

**What stops working with old versions.** A PC on 0.40.6 or earlier and a device on
the new version do not see each other. Jams with guests work throughout.

**Once every device is updated,** in this order:

1. On any device that cannot be updated, switch "Link my devices" off.
2. Change your music-server password. A fingerprint of the current one has been
   written to the relay's and the proxies' logs for as long as linking has been on.
   A new password is safe from those log lines. It does not undo anything already
   done with the old one, and it does not protect any other account where you used
   the same password.
3. If the host link has been anywhere you do not trust, give the relay a new key and
   paste the new link into each device.
4. Clear what logs the proxy keeps for the relay's address.
5. Switch the old linking off in the relay (one setting).

**If you lose a device:** change your music-server password first. That is what takes
the account away from it. Then remove it from the list, look through the list for
anything you do not recognise and remove that too, and give the relay a new key.

**What has been decided, and what it means.** Your music server is what says which
devices are yours. Anything that can sign in as you, or that runs the music server,
can add a device, and then see what your devices play and steer them. That party can
already read and change everything else in your account. Every other device says so
when a device is added. If you want more than that later, the design leaves room for
a device to need your approval on one you already have; that is not designed here.

## 3. Where this differs from what was decided, and why

The lead's decisions are followed. In three places the document goes a different way
or further, and says so here instead of quietly doing it.

**The account's identity (decision 3).** The decision was to bind entries to "the
server's address and the username exactly", as the sync work does. That identity is
right for filing a device's own things (its key, what it has read), and is used for
that. It cannot be what two devices agree on, because one person's devices
legitimately sign in to the same server at different addresses: `http://192.168.1.5`
on a PC at home, `https://music.example.org` on the phone. Sync never compares that
text between devices; a channel has to. So what goes into the channel is what every
device reads identically from the server: the owner as the server states it, the
list's playlist id, and a random value kept in the list (6.2). The first two are not
copyable by another account on that server; the third makes the room's name
unguessable on servers whose playlist ids are small numbers.

**The relay's private route (decision 5).** The decision was a private kind of
session with no guests, peeks or covers. The design goes further: a device no longer
opens a session at all. Devices in a room pass each other sealed frames through a
mailbox each (6.3). It is less relay code than a second kind of session, it cannot
leak into the jam's code because it shares none, it does not use the jams' pool, and
a linked device holds one request open at the relay instead of two.

**What a device says of itself (finding 3 of the review).** In the first version
every device broadcast what it was playing, sealed to each other device with a key of
our own construction. That is gone. The room now carries only a hint that a device is
there. What it is playing is asked over a channel, when the person opens the list.
This is the change in the fourth row of the table in section 2.

## 4. Today, and after the stop-gap

All of this is `src/jam.rs` and `relay/src/relay.rs` at 0.40.5.

1. A device works out `room = hex(SHA-256("nexus-music room\n" + username + "\n" +
   password))` and `word = the first 32 hex digits of SHA-256("nexus-music owner\n" +
   …)` (`account_words`).
2. It opens a private session (`POST /api/host/start`, the call that starts a jam)
   and enters the room (`POST /api/room/{room}/enter`), saying which session it can
   be steered through and what it is playing.
3. To steer B, A joins B's session as a guest (no key), and sends questions carrying
   `data.owner = word`. B compares the word and, if equal, does anything asked.
4. Starting a jam makes the private session public.

**What 0.40.6 changes,** as described to me (it is not written yet): a private
session answers only questions that carry the owner word.

**What someone with the proxy's logs can still do to a PC on 0.40.6 with "Link my
devices" on:**

- **Test guesses at the music-server password offline,** from the room's name, and
  know when one is right. A password that falls gives them the account on the music
  server, and the owner word with it: then they can read and replace that PC's queue,
  play, pause and set the volume, using the session id from the same log.
- **Watch what the PC is playing.** The session's id is enough to join it. A joined
  guest is still sent the session's state: the song, the queue, the position.
  Refusing questions does not stop that.
- **Fetch its cover art, and make it fetch from the music server.** The relay passes
  cover requests on with no token at all, and the PC answers them.
- **See the name the PC gave its session and how many are connected,** without
  joining (`peek`).
- **Get in the way.** A session takes fifty guests. Fifty joined from the log leave
  no place for the person's own devices, and their questions fill its waiting list.

They cannot steer it without the word, and the word is not in a log. The relay and
the two proxies still see the word itself in every question from one of the person's
devices, as before.

## 5. Who can do what

"Relay" means the relay program, Nginx Proxy Manager and Cloudflare: all three see
requests in the clear. "Logs" means access logs alone: addresses and paths.

| Party | Today | Under the design |
|---|---|---|
| Someone with the logs | As in section 4, and steers by default before 0.40.6. | Learns that devices exist and when they are active. The room's name says nothing about the password. Nothing in a log lets them connect: the private route needs the host key, which is in a header. |
| The relay | All of that, and reads the owner word from any question: full control of every device, for ever. | Sees which devices exist, which talks to which, when, and how much. Can drop or delay what it carries. A delayed command is refused if it arrives more than thirty seconds after the receiving device last offered a lease, or after anything changed on that device. Cannot read, forge, alter or replay anything on a channel, and cannot read what was said earlier if it later steals a device's key. |
| A guest in a jam | Steers the jam as the host allows. | The same. Nothing a guest holds reaches the private route. |
| Another account using the same relay (has the host key) | Can enter a room if it knows its name; can then list fake devices and collect the word. | Can enter a room only by learning its name, which needs the list on the music server. Entered, it can take places, push a real device out of its place, and send junk; devices ignore all of it. It cannot open a channel. This is a nuisance the relay cannot prevent (section 16, question 3). |
| Another account on the same music server | Nothing | Nothing, while the playlist is private (the apps keep it so). If it were public: reads device names and public keys, and could work out the room's name. It cannot write the list. |
| The music server, its administrator, anyone who can sign in as the account | Has the password, so the room and the word: full control. | Adds a device of its own to the list: full control from then on, and every real device says so. Cannot read what was said before. |
| Someone who steals one device | Has the password and the word: full control until the password is changed. | Is a device until removed (6.2). Cannot read what other devices said to each other, or what the stolen device said before it was stolen. |
| Someone who copies a device's key and is then shut out of the account | Not a separate case | Can open channels as that device until it is removed. Cannot read recorded traffic, and cannot pose as other devices to it. |
| Someone on the home network, relay reached over plain HTTP | Sees the host key, the room, the word. | Sees the host key and the room's place tokens (enough to be the nuisance above, and to start jams). Sees nothing said on a channel. |

## 6. The design

### 6.1 What a device holds

One X25519 key pair for each account it is signed in to, made when the person
switches linking on. It is filed on the device under that sign-in (the server's
address and the username exactly as signed in with, as sync files things), so another
account on the same device never uses it.

- The **device id** is the first 16 bytes of
  `SHA-256("nexus-music device 3\n" + public key)`, in lower-case base32 (26
  characters).
- The key belongs to one installation. If the relay keeps putting a device out of its
  place because the same id entered from elsewhere, the app says "This device's link
  is in use on another computer" and offers to link again with a new key.

Where the secret half is kept, and what that does and does not do, is in 10.3.

### 6.2 The list of devices

**Where.** The comment of a playlist named "Nexus Music devices", with no songs,
owned by the account, private.

```json
{"nexus_music_devices": 1,
 "salt": "AAECAwQFBgcICQoLDA0ODw",
 "devices": [
   {"key": "hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo", "name": "Study PC", "added": 1795000000}
 ],
 "removed": [
   {"key": "3p7bfXt9wbTTW2HC7OQ1Nz-DQ8hbeGdNrfx-FG-IK08", "at": 1795003600}
 ]}
```

`salt` is 16 random bytes made with the list and never changed. `devices` holds at
most twelve. `removed` holds the keys taken off, **for good**; when there are more
than 64 the oldest goes.

**Reading it** follows `core::record` as the sync record does: only an empty comment
is "no list yet"; a comment that is not a list is left exactly as it is and linking
is off; the layout number is read first and a later layout is never written; fields
not known are written back as they were.

**Which playlist, and holding to it.** This is stricter than sync's rule.

- When the person switches linking on, the device lists the playlists of that name
  whose owner the server states to be this account, by the check in
  `core/src/step.rs` (`Stated::ours`). None: it makes one. One: it uses it. More than
  one: it stops and says so, and offers to keep the one with the lowest id and delete
  the rest. It never chooses silently between two.
- It then **pins** that playlist's id and salt, and from then on reads that playlist
  by id, checking each time its name, that its owner is stated and is this account,
  and its salt.
- If the pinned playlist is gone, has another salt, or a second playlist of that name
  owned by the account has appeared, linking stops on that device and it says "Your
  list of devices on the server has been replaced." Linking again is the person's
  act, and makes a new key.
- A playlist found public is set private before it is used.

The known limits of the owner check are inherited: usernames are compared without
regard to case, and a server that does not state an owner cannot be used for linking
at all (the app says so).

**The account's identity,** which every device reads the same:
`account = SHA-256("nexus-music account 3\n" + owner + "\n" + playlist id + "\n" +
salt)`, with `owner` exactly as the server states it on the pinned playlist.

**What a device writes.** Every write is: read the list, change it, write it, read it
back. And every write carries **every removal that device knows of**, whether or not
the copy it just read had it.

1. **Add itself:** when the person switches linking on (a new key), or to repair,
   when its entry is missing, its key is not among the removed, and it was on the
   list before.
2. **Rename itself.**
3. **Remove a device,** on the person's say: the entry moves to `removed`.
4. **Leave,** at sign-out or when linking is switched off: its own entry moves to
   `removed`, so that an old copy of the installation cannot come back as it.
5. **Put a removal back,** whenever a read shows a key it knows to be removed among
   `devices`, or a removal it knows of missing.

**What a device remembers,** on disk: its key; the pin; every removal it has ever
read or made.

**Which keys a device trusts.** A key is trusted when all three hold:

- it was among `devices` at the last read of the pinned list;
- it is not among the removals this device remembers;
- that read happened in this run of the app, no more than **ten minutes** ago by the
  device's own monotonic clock.

The list is read every minute while the server answers, and at once (at most every
ten seconds) when the room shows a device id that is not known. Nothing is trusted
from before the app started: a device reads the list before it links.

When a key stops being trusted, every channel with it is closed and everything it had
asked for that has not been carried out is dropped.

A device tells the person about every key it has not seen before. On the first read
after linking is switched on, that is the whole list: "Linked to: Study PC, Phone."

**Removal, honestly.**

- *How long it takes.* A device that can reach the music server stops trusting a
  removed key at its next read: within about a minute. A device that cannot reach the
  server stops trusting everything within ten minutes. Until then a removed device
  that still holds its key can open a channel to a device that has not caught up.
- *What a concurrent write can undo.* The server has no conditional write. If device
  B read the list before a removal and writes afterwards (a rename, say), B's write
  puts the removed entry back and drops the removal. The repair: the device that made
  the removal, and every device that has since read it, put it back at their next
  read (rule 5), and none of them trusts the key in the meantime, whatever the list
  says. B itself learns of the removal at that read and from then on carries it.
- *What is left.* A device that has **never read the removal**, and reads the list in
  the window where it is undone, trusts the removed key until its next read after the
  repair. If the device that made the removal is the only one that ever read it, and
  it goes away before repairing, the removal stays undone until it comes back. A list
  restored from a backup of the server is repaired the same way, by any device that
  remembers the removals. The 65th removal forgets the first.
- *What removal does not do.* It does not take the account away from a device that
  still has the password: that device can link again with a new key, and every other
  device will say so. Hence the order in section 2: password first.

**Limits that come with the music server being the authority.** Whatever can write
the playlist's comment enrols devices. The sign-in to the music server is the Subsonic
kind: its reusable parameters travel in each request's address, and both apps allow a
home server over plain HTTP. Someone on that network can then read them and write the
list. Linking is as safe as the sign-in to the music server, and no safer.

### 6.3 The relay's private route

New in the relay, and required: the apps do not link through a relay that lacks it.
`GET /api/hello` gains `"link": 3`; an app that does not see it says "This relay
needs updating before your devices can be linked."

Every call needs the host key (`Authorization: Bearer`), as rooms do today. The route
has no guests, no join, no peek, no covers and no names, and shares no code with jams.

| Call | Body or query | Answer |
|---|---|---|
| `POST /api/link/{room}/enter` | `{"device": id, "hint": text}` | `{"token": t, "v": n, "devices": [{"device", "hint"}]}` |
| `GET /api/link/{room}/poll?v=n&after=s` | header `X-Nexus-Link: t` | Held up to 20 s. `{"v", "devices", "frames": [{"seq", "from", "frame"}], "more"}` |
| `POST /api/link/{room}/send` | header; `{"to": id, "frame": text}` | `{"ok": true}` |
| `POST /api/link/{room}/leave` | header | `{"ok": true}` |

- `room` is 52 base32 characters; `device` 26; `hint` at most 64 base64url
  characters; `frame` base64url.
- `enter` gives a device its place and a token for it. With the header set to that
  device's live token, `enter` only changes the hint. Without it, a device id that is
  already in the room is **replaced**: the earlier token stops working and its held
  poll is answered 409. (A device that restarts gets its place back this way. So can
  an intruder with the host key; that is the nuisance in section 5.)
- `send` puts a frame in the receiver's mailbox, marked with the sender's id as the
  relay knows it from the token. `poll` hands over what is in the mailbox; `after`
  says which frames have arrived, and those are dropped.
- The relay keeps nothing on disk and never logs a frame, a hint or a token.

| Limit | Value | When passed |
|---|---|---|
| Rooms | 50 | 503 |
| Devices in a room | 12 | 503 |
| The whole body of a `send` | 90,000 bytes | 413 |
| A mailbox | 16 frames or 1 MiB | 429 to the sender |
| A whole `poll` answer | 512 KiB | The rest waits; `"more": true` |
| Held polls for one device | 1 | The earlier is answered empty |
| Sends from one device | 40 at once, then 8 a second | 429 |
| A device that stops polling | 45 s | Loses its place |

Private traffic does not count against `JAM_MAX_JAMS`.

**Old linking in the relay.** The room calls of today (`/api/room/…`) and the use of
a jam session as a device's own stay, behind a setting `JAM_LEGACY_LINK`, on at
first. Off, they answer 410 with a sentence telling an old app to update. The owner
switches it off as the last step of section 2.

The changes to jams' own calls are in section 8.

### 6.4 Finding each other

The room's name is the lower-case base32 of `SHA-256("nexus-music room 3\n" +
account)`.

A device enters with its id and a **hint**: 16 random bytes it makes each time the
app starts, its *boot value*. The hint is not sealed and is not trusted. Its only use
is to aim a handshake at this run of the device.

A device shows the others from the **list**, by name, marked as here or not here
according to the room. Nothing else is taken from the room. A place in the room whose
id is not on the list is not shown.

### 6.5 The channel

**What it is.** `Noise_KK_25519_ChaChaPoly_BLAKE2s`, revision 34 of the Noise
specification, through the `snow` crate (10.1). KK because both devices already have
each other's public key from the list. The device that wants to look at or steer the
other opens the channel (the initiator, A below); the other is B.

**The prologue,** which both sides must agree on for the handshake to succeed:

```
"nexus-music link 3" + account (32 bytes) + A's device id (16) + B's device id (16)
                     + B's boot value (16)
```

**Frames.** Everything sent through the route is a frame: 29 bytes in the clear, then
the Noise message.

| Bytes | Field | |
|---|---|---|
| 4 | `"NML3"` | |
| 1 | type | 1 handshake from A, 2 handshake from B, 3 transport, 4 "no such channel" |
| 16 | channel | Chosen at random by A |
| 8 | counter | 0 in types 1, 2 and 4; in type 3, the Noise nonce, big-endian |

**The handshake.**

1. A makes a channel id and sends type 1: Noise's first message, with an empty
   payload.
2. B takes the sender's id from the relay, looks up its key among those it trusts,
   and reads the message. If the id is not trusted it reads the list once and tries
   again; if it still is not, or the message does not read, B says nothing. Otherwise
   B answers type 2: Noise's second message, with an empty payload. B now holds a
   *half-open* channel.
3. A reads type 2. Both sides now have transport keys. A sends its first transport
   frame, counter 0: `open` (6.6).
4. When B reads a transport frame from A that opens, the channel is **confirmed**.

Neither handshake message carries anything. **B says nothing of substance and does
nothing for A until the channel is confirmed.** Noise's first message can be recorded
and sent again by the relay; what that buys is one half-open channel at B, which holds
no state worth having and is dropped after ten seconds.

**Transport.** Each side numbers its frames from 0 and uses the number as the Noise
nonce (`snow`'s stateless transport). A receiver accepts a frame only if its counter
is higher than any it has accepted on that channel from that side, and it opens. So a
frame delivered twice, or after a later one, is dropped. Nothing is ever sent again
as the same frame: a sender that wants to repeat itself sends the message again in a
new frame.

**What each side keeps for a channel.** The channel id; which side it is; the two
transport keys; its next counter; the highest counter accepted; when it last heard
anything (monotonic). B also keeps its leases and its table of operations (6.7); A
its operations still unanswered and the last state it was sent.

**How many.** B keeps at most 4 half-open channels for one device and 24 in all, and
at most 2 confirmed channels from one device (a third closes the oldest).

**How long a channel lives.** Until either side says `close`; until ten minutes pass
with nothing heard; for at most 24 hours; and never past the restart of either app,
a sign-out, linking being switched off, or the other device's key ceasing to be
trusted.

**When B restarts.** B has no channels and a new boot value. A frame for a channel B
does not know is answered with type 4, at most once in five seconds for each sender.
Type 4 is not authenticated and means only "try again": A closes the channel, marks
what it had asked and not heard back about as *not known* (6.7), reads the room for
B's new hint, and opens a new channel. A never takes type 4 as the answer to
anything.

**When A restarts.** Its channels are gone with it. B's side lapses after ten
minutes. A opens new ones.

**When frames are lost.** Nothing in the channel breaks: counters only have to rise.
What was in the lost frame is dealt with by the rules for operations and state.

A opens a new channel to the same device no more often than every five seconds,
backing off to a minute.

### 6.6 What is said on a channel

Each transport frame carries one message, JSON, at most 60,000 bytes. A message with
a `t` not known is ignored.

From A:

| Message | Meaning |
|---|---|
| `{"t":"open","want":"list"}` or `"steer"` | Confirms the channel. `list`: A is showing B in its list. `steer`: A is steering B. May be sent again to change `want`. |
| `{"t":"renew"}` | Asks for a lease and the current state. |
| `{"t":"op","id":…,"lease":…,"rev":…,"do":…,"with":{…}}` | An operation (6.7). |
| `{"t":"close"}` | |

From B:

| Message | Meaning |
|---|---|
| `{"t":"state","rev":…,"lease":…, …}` | What B is doing. For `list`: whether it is playing, the title and the artist, each cut to 80 characters. For `steer`: all of what a jam's state holds today. Sent when the channel is confirmed, on `renew`, and whenever it changes. |
| `{"t":"done","id":…,"ok":…,"rev":…,"lease":…, …}` | The outcome of an operation. |
| `{"t":"close"}` | |

A message too large for one frame is sent in up to four, each
`{"t":"part","id":…,"part":[i, of],"body":…}`, where `body` is the next run of the
message's own text and the whole part, as every message, is at most 60,000 bytes.
They go one at a time, each acknowledged by `{"t":"got","id":…,"part":i}` before the
next is sent. Only an operation and its outcome may be sent this way. Nothing is done
until the last part has arrived and the joined text has been read as one message.

### 6.7 Operations

**An operation has an identity that survives a retry.** `id` is 16 random bytes, made
by A when the person acts. If A does not hear the outcome it sends the same operation
again, with the same `id`, in a new frame.

**B issues leases.** A lease is 8 random bytes that B will honour for **thirty
seconds from the moment it issued them, by its own monotonic clock**. Every `state`
and every `done` B sends carries a new one. An operation quotes the newest lease A
holds. No device's wall clock is used anywhere in the protocol.

**B's state has a revision.** `rev` goes up whenever what B is playing, whether it is
playing, or its queue changes, by anyone: the person at B, another device, a guest in
a jam, a song ending. Volume is not part of it.

**What B does with an operation,** in this order:

1. The frame opened on a confirmed channel and its counter was new. (Otherwise
   nothing: 6.5.)
2. The message is well formed and has every field its `do` needs. If not:
   `done`, `"why":"invalid"`.
3. The sender's key is still trusted. If not, the channel is closed.
4. The `id` is in B's table. If it is finished, B sends the recorded `done` again and
   does nothing else. If it is still being carried out, B sends nothing.
5. The lease is one of B's and is not thirty seconds old. If not: `done`,
   `"why":"lease"`, with a new lease. Nothing is recorded.
6. For everything but `volume` and `give`: `rev` equals B's revision now. If
   not: `done`, `"why":"changed"`, with the state. Nothing is recorded.
7. B **reserves** the `id` in its table and gives the operation its place in the
   order of the person's choices on B, as a press on B's own screen would get.
8. **At the moment it is carried out:** its place is still the latest that applies,
   `rev` still matches, and the key is still trusted. Then B does it.
9. B records the outcome against the `id` and sends `done`.

B keeps the last 64 operations of a channel, for as long as the channel lives.

**What A does.**

- It sends an operation only with a lease it believes is live. If it has none it
  sends `renew` first.
- No `done` within twelve seconds: it sends the same operation again, up to three
  times. A `done` with `"why":"lease"` is answered by sending the same operation with
  the new lease, **but only within ten seconds of the person's press** by A's own
  monotonic clock.
- After that, or when the channel is lost, the operation is **not known**. A says so
  ("That may not have reached Study PC"), shows B's state when it next arrives, and
  sends nothing more for that press. A never makes a new operation on the person's
  behalf.
- `"why":"changed"`: A shows the new state and says what changed. The person presses
  again, or does not.

**The operations.** `play`, `pause`, `next`, `previous`, `seek` (to a time in a named
song), `jump`, `move` and `remove` (each naming the place and the song there),
`clear`, `shuffle`, `repeat`, `volume`, `add`, `adopt` (take this queue and play it),
`give` (hand over your queue). There is no toggle: a Pause is a Pause.

**Moving music.**

- *To B* (`adopt`): A sends its queue, its place and its position. B checks it can
  play now (songs it has, no call in progress, the sound its to take) before it
  changes anything; if it cannot, it answers `"ok":false` with the reason and is as
  it was. Otherwise it takes the queue, starts, and answers `"ok":true` **once sound
  has started**. Only then does A pause. Not known: A carries on playing and says so.
- *To A* (`give`): B answers with its queue. A takes it and starts. Once A's own
  sound has started, A sends B `pause`.

### 6.8 Sizes

Budgets are for whole messages as they travel: sealed, encoded, in the relay's
wrapping.

| What | Worst realistic case | As it travels | Against |
|---|---|---|---|
| A hint | 16 bytes | 22 characters | 64 |
| The room, as a poll answer | 12 devices | under 2 KiB | 512 KiB |
| A handshake frame | 29 + 48 bytes | 103 characters | 90,000 bytes |
| A full frame | 60,000 bytes of message | 80,060 characters, about 80,150 bytes with wrapping | 90,000 bytes |
| A poll answer | 6 full frames | about 470 KiB | 512 KiB; the phone refuses answers over 768 KiB |
| `state` for the list | Title and artist of 80 four-byte characters each | under 1 KiB | one frame |
| `state` for steering | 100 queue entries, every text at its longest in four-byte characters: about 140 KiB | Does not fit | Built to fit: entries are added until 56,000 bytes, and the rest are counted in `more`. With ordinary titles, 100 entries are about 15 KiB. |
| `adopt`, 3,000 songs | Ids of 22 characters: 75 KB. Ids of 64 characters, the longest accepted: 201 KB, 207 KB once carried as text inside a part | 2 parts; 4 parts | 4 parts, about 239 KB of text in all |
| The list on the music server | 12 devices with names of 24 four-byte characters, 64 removals | under 8 KiB | Written as the sync record is |
| Eleven other devices, list open | 11 channels, 11 small states | a few KiB in all | |

**What is refused, and what the person is told.**

- A queue that does not fit four parts (more than about 3,400 songs at the longest
  ids, about 8,800 at Navidrome's) is cut from its far end, keeping the song to start
  with: "The next 3,400 of 5,000 songs were moved to Study PC."
- A mailbox that is full (the other device has stopped collecting): "Study PC isn't
  answering."
- Anything over a limit at the receiving end is dropped and the channel closed; that
  is a fault in an app, not something a person can cause.

### 6.9 What the relay still learns

The room's name; each device's id and boot value, and when it is present; which
device opens a channel to which, when, and how large each frame is. A moved queue is
large and a pause is small. Nothing is padded. It also has the host key and the
places' tokens.

It does not learn device names, what is playing, the queue, or what was asked.

## 7. Before any work is done for a question

This is each app's own behaviour, not protocol. It is written here because both apps
must meet it, for a jam's guests as much as for the person's devices, and because
getting it wrong is how the phone's review found a guest able to cancel the host's
own Play.

**The rule.** For every question that arrives:

1. **Validate it:** every field its action needs is there and of the right kind.
2. **Authorise it:** this asker may ask this now.
3. Only then give it **a place in the order of the person's choices.**
4. Only then do any **work** for it: a lookup, a search, a fetch.
5. **When it is carried out, authorise it again:** the asker is still allowed (the
   guest is still in the jam, steering is still on, the device's key is still
   trusted), and its place has not been overtaken.

A question that fails 1 or 2 costs nothing and changes nothing.

**The cases, each a test in both apps:**

| Case | Must happen |
|---|---|
| The person presses Play on a list still being looked up; a guest searches | The Play goes ahead. |
| The same; a guest sends a question it may not ask | The Play goes ahead; the question is refused. |
| A guest is removed after their question was collected but before it is carried out | It is not carried out. |
| "Guests can steer" is switched off between a skip arriving and being carried out | It is not carried out. |
| `seek` with no time; `move` with no destination; `clear` with no part named | Refused. Nothing seeks to zero, moves to the front or clears. |
| A guest asks for a cover the jam has not shown | Refused before any fetch. |

## 8. Jams with guests

A jam is a different trust problem and stays one. The link lets a guest in; guests
have no account and no keys; the page is served by the relay, so nothing in a jam is
or can be hidden from the relay. Nothing of the account's is used on a guest's path,
and a jam's session carries nothing from the private route.

A device that hosts a jam no longer has a private session that turns into it:
starting a jam starts a session, and ending it ends that session.

| Finding | Where it belongs |
|---|---|
| A removed guest comes back with the same link | **Relay and protocol.** `POST /api/host/{jam}/lock` with `{"locked": true}`: the relay takes no new guests. The app offers "Remove and lock" and says the jam is locked only once the relay has answered. Plain "Remove" is worded as what it is: it disconnects. |
| A removed guest's work still acts | **Relay:** removal takes back that guest's questions the host has not collected. **Apps:** section 7, rule 5, for those it has. |
| The limit on callers trusts `X-Forwarded-For` | **Relay:** it is believed only from the address named in a new setting, `JAM_TRUSTED_PROXY`. |
| Unlimited waiters for one cover | **Relay:** 8 for one cover. |
| What the host has not collected can exceed what a phone will read | **Relay:** an answer to the host's poll is at most 512 KiB; the rest waits. |
| Which cover ids exist | **`core/`:** one rule, used by the relay and both apps. An id the rule refuses is not shown to guests. |
| Fields a question needs | **`core/`:** one function (section 7, rule 1). |
| A 504 after the host has acted | **Apps and the page:** say the outcome is not known and refresh before offering to try again. |
| Work budgets on the phone; the car and other controllers; the exported service; the host key paired with the wrong relay | **Each app's own.** They remain conditions for switching jams on in the phone. |

## 9. What goes in `core/`

One module, `core::link`. It holds the protocol's state machine, the schemas, the
budgets and every decision about what is accepted. It has no clock, no randomness, no
storage and no player: each is handed in or asked of the app.

| Piece | What `core/` decides | What the app supplies |
|---|---|---|
| Identity | The device id of a key | 32 random bytes for a key; where the key is kept |
| The list | Reading a comment; which playlist; the pin; every write, with the removals carried; which keys are trusted now; which are new | The server's answers; a monotonic time; what it has stored |
| The room | Its name; what to enter with | The boot value's random bytes |
| Channels | The handshake and its prologue; frames; counters; the limits on channels; when one is closed | Frames in and out; random bytes; a monotonic time |
| Messages | Every schema; the size of each; splitting and joining parts | |
| Operations | Steps 1 to 7 and 9 of 6.7 on B; the table; the leases; on A, when to send again and when an operation is not known | The player: its revision, a place in the order of choices, and carrying an operation out (step 8) |
| Questions from guests | Reading one and checking its fields; which cover ids exist | |

The relay depends on `core/` for the rules that are not secret (the forms of a room's
name and a device id, the limits of 6.3, which cover ids exist). Its image is built
from `relay/` alone today, so its Dockerfile and workflow take `core/` as well.

## 10. What it costs

### 10.1 Crates, pinned, and what I checked

I read each crate's manifest at the pinned version, its repository, and the RustSec
advisory database, on 25 November 2026. I built nothing.

| Crate | Pin | Checked | Not checked |
|---|---|---|---|
| `snow` | `=0.10.0`, features `default-resolver`, `use-curve25519`, `use-chacha20poly1305`, `use-blake2`, `std` | Released July 2025; the repository has commits through 2026 and is not archived. Its README says it "has not received any formal audit". One advisory, RUSTSEC-2024-0011 (fixed in 0.9.5), which did not affect the stateless transport used here. It has `StatelessTransportState` with explicit nonces, a prologue, both static keys set beforehand, and a limit of 65,535 bytes. Its randomness comes through a resolver, so `core/` can hand in the app's bytes. | Whether it zeroes its keys when dropped. That it builds for Android and iOS. |
| `curve25519-dalek` | `=4.1.3`, which `snow` 0.10 requires at least | It is the release with the fix for RUSTSEC-2024-0344 (timing). An earlier snapshot was audited by Quarkslab. | The 4 line has had no release since June 2024; 5.0 is current and `snow` 0.10 does not take it. |
| `chacha20poly1305` | `=0.10.1` | No advisory against it. An earlier snapshot was reviewed by NCC Group. | The same: 0.11 is current. |
| `blake2` | `=0.10.6` | No advisory that applies. | |
| `sha2` | The app's own (0.11) | For the device id, account and room only. | |
| `subtle`, `zeroize` | As `snow` and the above bring them | | |

`use-getrandom` is left off on purpose: the operating system's randomness is asked
for by each app and handed in, and an app that cannot get it does not link.

If `snow` proves unusable on a target, the fallback is `clatter` 2.3.0: maintained,
`no_std`, the same primitives, far fewer users.

BLAKE2s is chosen over SHA-256 for the handshake because it is the usual partner of
ChaChaPoly and because `snow` 0.10 would otherwise bring a second, older `sha2` into
the Windows app beside the one it has.

### 10.2 Size and speed

**Size.** Not measured. My estimate is 0.15 to 0.3 MB added to the Android library
for each architecture and about the same to the Windows `.exe`. The first slice
(section 13) measures it.

**On a phone.** Opening a channel: four X25519 operations on each side, well under a
millisecond. A frame: microseconds. With the list closed and nothing steering, a
linked device holds one request open at the relay (a linked PC today: two) and no
channels.

### 10.3 Where the secret key is kept

| Platform | Where | What that protects | What it does not |
|---|---|---|---|
| Windows | Credential Manager, as the password is, kept on that machine only (not roaming with the user's profile; the `keyring` crate's setting for this has to be checked) | Other users of the PC; a copied settings folder | Any program running as the person; a full backup of the profile restored elsewhere |
| Android | A file encrypted with a key held in the Android Keystore, in app storage left out of backups | Other apps; a backup; a copied file | A rooted phone; and while linking runs the key is in the app's memory |
| iOS, later | The Keychain, this device only | The same as Android | The same |

Secrets that cross from `core/` to an app and back (the device's key, each way) are
wiped by `core/` after use; whether `snow` wipes its own is the item not checked
above.

## 11. Moving from today's scheme

A new version does not speak the old scheme. It never works out the password's
hashes, never sends or accepts the owner word, and opens no private session.

| Step | Who | What | Safer or the same |
|---|---|---|---|
| 1 | Owner | Update the relay. It has the private route, the jam fixes of section 8, and old linking still on. | The same for devices; jams better. |
| 2 | Windows release | New linking, used only if the relay says `"link": 3`. | Each updated PC stops sending the password's fingerprint. |
| 3 | Phone release | The same, when jams and linking are switched on there. | It has never sent the old values in a released build. |
| 4 | Owner | The five steps of section 2: old linking off on what cannot update; the password; the host link if it was exposed; the logs; `JAM_LEGACY_LINK` off. | The old exposure ends here and not before. |

Between steps 2 and 4, updated PCs link with each other and old ones with each other.
An old PC goes on sending the password's fingerprint in an address until it is
updated or its linking is switched off; the relay refusing it later does not stop it
being sent. That is why step 4 begins with the devices, not the password.

Jams with guests use the same calls throughout, on every mix of versions.

## 12. Test vectors

**Bytes.** `docs/linking-vectors.py` is a separate implementation in Python. Before
printing, it checks itself against the published vector for
`Noise_KK_25519_ChaChaPoly_BLAKE2s` (both handshake messages, the handshake hash and
four transport messages) and against RFC 7748. `core/`'s tests must give the same
bytes. The devices are RFC 7748's Alice (a PC, opening the channel) and Bob (a phone).

| | |
|---|---|
| PC public key | `8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a` |
| PC device id | `yxpvq5xob2xohssguxioj5afsm` |
| Phone public key | `de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f` |
| Phone device id | `6pvwc2vz4l5crv4zz6pjkjcteq` |
| Owner, playlist id, salt | `tester`, `AbCdEfGhIjKlMnOpQrStUv`, `000102030405060708090a0b0c0d0e0f` |
| Account | `1634e06bb83d8949d452dfa345767af1eec6081b0d0fee0c82bc6d18dbce76ad` |
| Room | `epmbldfoum7itjpe45munuvkkcher74ejmoaoqavvknyjd6nq6pa` |
| Phone's boot value | `f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff` |
| Channel | `101112131415161718191a1b1c1d1e1f` |
| PC's ephemeral secret | `a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf` |
| Phone's ephemeral secret | `c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf` |
| Handshake message 1 | `605a725d2a4adfeeb1a29e17edd621c1b7593ee8cdbc44ac6c4ab6e2f805d23ca8d035f94471327e40ed9f10aec0d059` |
| Handshake message 2 | `dc2cca31e8e43bbd91dff7e475cca3347eb478107d5bd765aba4ae4a30c35d44b696ffd0d7e0768ec544256e6e780e79` |
| Handshake hash | `e95f40f1701e9ee9ba5c91b0093e19bb9c377c1b4d3b072d7f58ccf736df006e` |
| First frame from the PC, `{"t":"open","want":"steer"}` | `Tk1MMwMQERITFBUWFxgZGhscHR4fAAAAAAAAAADrBgvT8rXDkL74zi6sIv4XIWIsVpe7qTDfnajMXVx6elLrvD-qEJsSEtxT` |

The script prints the rest: the secret keys, the prologue, the transport keys and all
four frames.

**Frames that must be refused,** each a test: any one bit changed anywhere; cut
short; a type-3 frame for a channel not known; a counter equal to or below the
highest accepted; a transport frame before the handshake has finished; a handshake
whose prologue differs in the account, either id or the boot value; a handshake from
an id not on the list; from a key among the removed; a frame from B handed back to B.

**Decisions.** These are sequences. `core/`'s tests run each against a relay stand-in
that can hold a frame, deliver it twice, or deliver it late.

| Name | What happens | What must be decided |
|---|---|---|
| Skip, answer withheld | A sends `next` (id X, rev 7). B skips; rev is 8. The relay withholds `done` and delivers A's frame again. | The second delivery is dropped: its counter is not new. |
| Skip, sent again | The same; A, hearing nothing, sends X again in a new frame, still rev 7. | B finds X finished and sends the recorded `done`. One skip. |
| Skip, asked anew | The same; a faulty A sends a new id Y for `next`, rev 7. | Refused, `changed`: B is at rev 8. One skip. |
| Play, held | A sends `play` (rev 5). The relay holds it. The person pauses at B (rev 6). The relay delivers within thirty seconds. | Refused, `changed`. B stays paused. |
| Play, held long | The same, with nothing done at B; delivered after forty seconds. | Refused, `lease`. A's ten seconds have passed, so A sends nothing; the operation is not known. |
| Two presses, crossed | A sends `next` (rev 5), then `pause` (rev 5). The relay delivers `pause` first. | `pause` is done (rev 6). `next` arrives with a lower counter: dropped. A hears nothing for it, sends it again (rev 5): refused, `changed`. |
| An old answer | A's operation X is not known. Later A sends Z. The relay delivers the old `done` for X. | Its counter is not new: dropped. Were it new, its id is X, not Z: it settles X and nothing else. |
| B restarts | A sends `pause`; B restarts before it arrives. | Type 4. A closes, marks `pause` not known, opens a new channel, shows B's state. No `pause` is sent by A unasked. |
| Moving, the other cannot play | A sends `adopt`; B is in a call. | B answers `"ok":false` and is as it was. A goes on playing. |
| Moving, answer lost | A sends `adopt`; B starts; `done` is lost. | A goes on playing and sends the same id again. B sends the recorded `done`. A pauses. |
| Removed while asking | C's key is removed. B reads the list after C's `next` was reserved and before it is carried out. | Not carried out. The channel is closed. |
| A removal undone | A removes C and writes. B, from a copy read before, renames itself and writes. A reads. | A's read shows C back: A writes the removal again and at no point trusts C. B's next read shows the removal; B remembers it. |
| The server out of reach | B last read the list eleven minutes ago. A opens a channel. | B says nothing. |

## 13. What to build first

The smallest thing that is safe to have and proves the hard parts:

1. **The relay's private route** (6.3) with its limits and tests, and `"link": 3`.
   Jams untouched. Deployed by the owner.
2. **`core::link`, without membership rules:** identity, frames, the handshake through
   `snow`, counters, leases, the table of operations, and two operations: the `list`
   state (what is playing) and `pause`. With the byte vectors of section 12 and the
   decision vectors that need only those two.
3. **Two Windows copies,** behind a setting that is off and not shown, each given the
   other's public key by adding itself to the list (rule 1 of 6.2 and reading only:
   no removal, no notices). One shows what the other is playing and can pause it.

What it proves: that frames get through the owner's relay, proxy and Cloudflare under
the limits; that the handshake and `snow` behave on Windows as the vectors say; that
the restart rules work with real processes; that a relay holding or repeating frames
cannot cause a second pause or a late one; and how much the `.exe` grows.

What it leaves for after, in this order: the list's full rules (removal, the pin, the
ten minutes); the other operations and steering; moving music; the phone, through
JNI; then switching it on, and the owner's step 4.

## 14. The alternatives, and why not

**Single sealed messages under a key from the two static keys** (the first version).
Withdrawn. One stolen key read every recorded message in both directions and could
pose as the other device to its own. A handshake with fresh keys does neither.

**Noise IK or XX in place of KK.** IK and XX send static keys inside the handshake,
for when one side does not know the other's beforehand. Here both do, from the list,
and a key that is not on the list must be refused whatever it sends. KK needs no key
on the wire, and so tells the relay nothing about which keys exist.

**A pre-shared key added to the handshake (the relay's host key).** It would stop a
party without the host key from even starting a handshake. That party already cannot
reach the route. And the host key is held by the parties to be defended against.

**The relay's host key in a keyed hash, as the trust itself.** As in the first
version: anything that starts from the password breaks the rule, and the host key is
in the wrong hands.

**A pairing secret passed by a code.** Stronger against the music server and against
someone with the password. Not designed here. The first version's six digits worked
out from the two public keys are withdrawn: whoever can write the list can search
offline for a key whose digits match. If a stricter mode is wanted, it needs a proper
pairing whose code carries a fresh secret. The design leaves room for it in one
place: which keys a device trusts is a decision made on the device (6.2), and an
approval that does not come from the playlist can be added to it without changing
anything a device sends.

**One secret key for the whole account.** Any device could then speak as any other,
and a removed device keeps the key.

**Clocks for freshness.** The first version used the device's clock set beside the
music server's. A lease on the receiving device's own monotonic clock needs no
agreement between clocks and gives thirty seconds, not five minutes.

## 15. What a second reviewer should attack

1. **The lease and the revision together** (6.7). The claim is that no delayed,
   repeated or reordered frame makes a device do something its owner did not ask for
   at about that moment. The decision vectors are my attempt to break it.
2. **"Not known".** What A shows and does after an outcome it never heard, in every
   operation, and whether a person can be led to press twice.
3. **Removal** (6.2): the three sentences under "what is left", and whether carrying
   removals in every write can itself do harm (a mistaken removal cannot be undone
   except by linking that device again with a new key).
4. **The pin.** Whether stopping when a second playlist appears hands whoever can
   write to the account an easy way to stop linking, and whether that matters given
   what else they can do.
5. **The prologue.** Whether owner, playlist id and salt are the right account
   identity, and what happens on servers that state owners or ids differently between
   two calls.
6. **Half-open channels.** The limits on what an unconfirmed handshake can cost B.
7. **`snow` at a version whose primitives are a release behind,** with no formal
   audit, as the one implementation everything rests on.

## 16. What I could not settle

1. **Sizes.** Estimated, not measured.
2. **Zeroing of keys inside `snow`,** and across JNI.
3. **The nuisance from a host-key holder in a room.** The relay could check that a
   device holds the key behind its id (a challenge answered with X25519). That would
   end it, at the price of key arithmetic in the relay and the relay learning public
   keys. Left out; worth a decision.
4. **Servers other than Navidrome.** The design needs a playlist's owner stated, its
   comment writable, and whether it is public. Not checked elsewhere.
5. **Whether thirty seconds is right for the lease,** and ten minutes for trusting
   the last read of the list.
6. **Lock, or a new link, after removing a guest.** Lock is proposed. A new link for
   those still in the jam is friendlier and changes the link's form.
7. **How the Windows credential is kept from roaming.** To be read in the `keyring`
   crate before the first slice stores a key.

## 17. What changed from the first version

| First version | Now | Why |
|---|---|---|
| Single sealed messages, key from two static keys | A Noise KK channel through `snow` | Review finding 3; decision 1 |
| A retry was a new message with a newer time | An operation keeps its id; the receiver reserves it and answers a repeat with the recorded outcome | Finding 1; decision 2 |
| Five minutes by clocks set beside the server's | A thirty-second lease on the receiver's monotonic clock; a revision checked when carried out | Finding 4; decision 2 |
| The source paused, then the other started | The other starts and says so; then the source pauses | Finding 4; decision 2 |
| Removals kept 90 days; a writer carried only what it read | Kept for good; every writer carries every removal it knows; trust lasts ten minutes | Finding 2; decision 3 |
| Lowest-id playlist, looked for afresh | Pinned; a second playlist stops linking | Finding 5 |
| `account`: a random value | Owner, playlist id and salt (and not the server's address: section 3) | Finding 5; decision 3 |
| Six digits to confirm a device | Withdrawn | Finding 5; decision 4 |
| Sealed presence in the room | A hint only; what is playing is asked on a channel | Finding 3 |
| Rode inside the jam's calls, no relay change needed | A private route in the relay, required first | Finding 6; decision 5 |
| A device kept a private session | No session; a jam is the only kind | Findings 6, 7 |
| Authorise-then-act mentioned for the phone | A rule for both apps, with cases | Finding 7; decision 6 |
| Sizes asserted | Budgeted end to end | Finding 6; decision 7 |
| Crates named from memory | Pinned, with what was and was not checked | Finding 9; decision 8 |
| Vectors for one sealed message | Byte vectors checked against a published Noise vector; decision vectors | Finding 9 |
| Change the password, then update | Old linking off first; the password; the host link; the logs | Finding 8 |
