# How a person's own devices find and trust each other

The design the Windows app's first two slices are built to. The third version, brought
up to its reviews and to what building showed; the first two are in this file's history.
Sections 1 and 2 are for the owner.

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
| A playlist named "Nexus Music link" | Does not exist | Exists. Hidden in new versions. Other music apps and Navidrome's own page show it, empty, with a line of letters as its description. Do not share it or make it public. If it is deleted your devices start afresh with a new one. If you already have a playlist of exactly that name with no songs and no description, it becomes this one; one with songs or a description of your own is left alone, and you are asked to rename or delete it before linking starts. Another music app may show two playlists called "Nexus Music link" (two devices were set up at the same moment): the one with the higher id can be deleted by hand, and "Reset linking" does not remove it. |
| Removing a device | Not possible | No list and no Remove. "Reset linking" under Settings, Jam: every device still signed in rejoins by itself within two minutes. |
| The list under "Connect to a device" | Shows what each is playing at once | Shows the devices it has seen before at once, and what each is playing a moment later. One seen before that is not there just now is listed all the same, greyed, and cannot be picked. |
| A button pressed just as the other device was given something else to do | Acts on whatever is there | Does nothing, and shows what changed. Press again if you still mean it. |
| Two people steering one device at the same instant | Both act | One acts; the other is shown the result |
| Moving music to another device | This one stops, then the other starts | The other starts, then this one stops. If anything gets in the way you are told which of them is playing. |
| Your music server out of reach | Linking carries on | Devices stop seeing each other after ten minutes |
| Changing your password | Splits your devices until each signs in again | No effect once each has signed in again |
| A PC on 0.40.6 or earlier | Sees your other devices | Does not see devices on the new version, nor they it. Jams with guests work throughout. |

**The relay is updated first.** That is done for you; it ends any jam in progress. It
gains a second port for your devices, and your proxy one rule sending `/api/link/` to
it.

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

**If linking stops on a device.** If something goes wrong inside linking on one of
your devices, linking stops on that device and it tells you, once: "Linking has
stopped on this phone until the app is next opened." The music on it is not affected, and your other devices carry on without
it. It starts again when the app next starts from scratch.

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

**Where linking is on, the app leaves it out of the lists of playlists it shows; where
linking is off, nothing is different.** A PC with linking on leaves the playlist of
that name out of its list where the list arrives, and does not let a playlist of the
person's own be given the name; a phone on which linking exists, is running and is
wanted leaves it out of the app's list, the list songs are added to, and the car's,
in memory as each is shown. **A copy with linking off (which is every released copy)
behaves exactly as it did before linking was written**: the playlist is one like any
other, its name is nobody's, and no cache, import, download or list is touched. Hiding
it in every copy was built, for both apps, and withdrawn after its review: it took a
rule that only matters where linking is on and enforced it where it is off, and there
it reached into things every person relies on (a kept list rewritten or thrown away,
the import resolving against a filtered list, pages still opening by id). The secret
is the owner's own, readable by anything signed in to the account (weakness 1); seen
on the owner's other devices during a test it does no harm. **In every copy, at general switch-on and not before, the
playlist is treated exactly as the sync playlist is today: by its name, where that name
is already treated specially, and by nothing else** (`linking-playlist-everywhere.md`:
the ruling, after a design that went by what the playlist holds was reviewed as a change
to everybody's playlists and not built). Until then a phone's kept list of playlists holds this one's description as
it holds every playlist's.

A **look** is: list the playlists; choose by the rule of `core/src/step.rs` (`find`:
the lowest id among those of that name whose owner the server states to be this
account); fetch that playlist; check what was fetched. The fetched playlist counts
only if its id is the one asked for, its name is the name, its **owner is stated** and
is this account, and it is **stated private** (`public` present and `false`). All four
are read from **one answer**, and it is the XML one: Navidrome's JSON leaves `public`
out when it is false, so only its XML states it, and a comment from one answer with
the privacy from another could renew a secret that was replaced in between. An
answer that fails any of these, or leaves one out, is not used, is not written over,
and does not renew the ten minutes. A server that does not state these cannot be used
for linking, and the app says so. The name is added to the reserved names.

| The look finds | The device |
|---|---|
| No playlist of ours | Makes one of the name, writes a secret into it, and looks again from the start. It uses only what a look reads back, never what it wrote. |
| Ours, with no songs and no comment at all | **Fills it:** writes a secret into it, which makes it private, and looks again. It is what a device leaves if it stops between making the playlist and filling it. |
| Ours, with songs in it, or with a comment that is not a record | **Leaves it exactly as it is.** It is the person's own playlist. Linking does not start, and the person is told to rename or delete it. |
| Ours, with a later layout number | Leaves it alone. Linking is off until the app is updated. |
| Ours, readable | Uses the secret, and remembers the playlist's id for this sign-in. |
| A lower id of ours than the one remembered, the remembered one still there | Moves to the lower (two devices each made one; this is how they agree). |
| The remembered one gone | **Does not take up another playlist's old secret.** Writes a new secret into what `find` now gives if that holds a record, or makes one. |
| Ours, stated public | Makes it private, writes a new secret, and tells the person. |

Two devices that write at the same moment may use different secrets until the next
look. **The secret used is the one that stands, whoever wrote it:** a device whose
read-back shows a secret that is not the one it wrote takes that one.

**Whose a playlist of the name is goes by what it holds, and by nothing else.** With no
songs and no comment it is the app's to fill; with songs, or a comment that is not a
record, it is the person's, and nothing is written into it. The playlist is made with
its name and nothing in it and filled in the next request, so a look that stops between
the two (the app was closed, the server went away) leaves exactly what the next look,
on any device, fills. Nothing is made unless there are random bytes to put in it.
Accepted with this:

- **A playlist of exactly this name that the person made themselves and put nothing
  in**, no song and no description, is used: it is given the record and made private.
- **Two devices that set up at the same moment make a playlist each.** Both go by the
  lower id from their next look on, as with the list that keeps devices in step, and
  the other stays where it is, holding a record nothing reads. Nothing deletes it:
  another music app shows two playlists of the name, the one with the higher id can be
  deleted by hand, and "Reset linking" does not remove it.

**A comment is a record only if it reads as one:** the mark with a layout this version
knows and a well-formed secret, or the mark with a later layout. A description that
only has the mark's name in it is a description.

- **Writing** is by form POST with the sign-in in the form too, so neither the secret
  nor a new address carrying it reaches a log. (Navidrome takes form POST for every
  call; checked in its source.) Every write sets the playlist private. Redirects are
  not followed.
- **A list of playlists that is not one is an error, and never "there are none"**: an
  answer the server calls ok whose list is of some other shape leads to nothing being
  made.
- **Only a 200 whose whole document parses is an answer.** A proxy's error page, or
  any other status with a body that happens to read as a good answer, is no answer;
  nor is the start of a good answer that was cut short on its way (every element has
  to be closed by its own name, the root last, with nothing after it).
- **A write that gets no answer is followed by a read**, because the server may have
  written it all the same. The look goes round again and goes by what it reads: the new
  secret (the write landed and its answer was lost); a secret that is not the one it
  wrote (the write never landed and the old one stands, or another device wrote at the
  same moment: either way that one is used, and for a reset nothing is said to have
  been reset); or, if the read gets no answer either, nothing: the record is not known
  to be good, and what is held is not used.
- **Before the first request that changes anything on the server**, the look says it is
  to be seen through (`keep_going`): a sign-out after that does not leave it half done.
  That first request is the making of the playlist, where one is made. A request
  already on its way when the sign-in ends still finishes; no other is made.
- **When.** A look starts when linking is switched on or the app starts, two minutes
  after the last one finished, and on "Reset linking". **One look at a time for a
  sign-in.** The secret is held in memory only and is used for at most ten minutes,
  counted from the start of the look that read it, on a clock that counts sleep, and
  checked each time the secret is used.
- **"Reset linking"** is a look that, where it would have used the secret it read,
  writes a new one in its place and reads that back. It is no other path to the
  playlist: the same answer is checked the same way before anything is written, so
  a reset cannot write into a playlist a look would not use. Other devices move at
  their next look. **The session on the device where it was pressed ends at the
  press**, not when that look comes back: from that moment the device sends, opens and
  carries out nothing under the old secret, and a look that was on its way (it reads
  the old secret) begins nothing. If the new secret cannot be written, the old one
  stands on the server, the person is told the reset did not happen, and the device
  has a session again when a look next reads it.
- **A session** is one sign-in with one secret. When the secret changes or runs out,
  the person signs out, "Link my devices" is switched off, or the account changes, the
  session ends: looks, answers, what is queued to send and commands in flight under it
  are dropped. **Every use asks whether the session is the live one and inside its
  ten minutes:** a look's answer, anything sent, each frame opened, and the moment the
  player is changed (5.6). A look that finds the record is not one to use (public and
  not replaced, not ours, holding songs or a comment of the person's own, a later layout,
  no longer stated) ends the
  session there and then; a look that could not ask concludes nothing. **The end of a
  session ends everything that was waiting under it, there and then** (5.6).
- **Never listed, never on disk.** With linking on, the playlist is left out of every
  ordinary listing of playlists, as the sync record's is. And the page of a playlist
  whose comment **is a linking record** is never written to the app's cache of pages,
  **whatever the playlist is called**, and one found on disk is thrown away unread: a
  playlist renamed somewhere else (on the music server's own page) is an ordinary
  playlist to every listing, and its comment is still the record. It is by what the
  comment holds and nothing else: an ordinary playlist that only has the name, or
  whose description only looks like a record, is cached as any other.
  **Versions up to 0.40.6
  do list it,** and opening it there writes the secret into that PC's page cache: one
  more reason an old PC is updated before linking is switched on anywhere.

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

**It is served apart from jams:** on a listener of its own (`JAM_LINK_LISTEN`, a
second port), read and written by the relay itself on threads of its own. The proxy
in front sends `/api/link/` there and everything else to the first port; on the first
port the route is not there at all, and `hello` offers it only when the second is up.
Nothing on the route's path runs where a jam is served, refusals included, so it can
never cost a jam anything. (It cannot share the jams' listener and keep that promise:
the HTTP library jams are served with gives no deadline on reading a body, and reads
what is left of one on whichever thread lets go of the request.)

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
  no such room, place or token; 409 the place is taken; 410 old linking is off (on the
  first port); 411 a body that does not state its length; 413; 429 with `Retry-After`;
  431 a head over 8 KiB; 503 full or busy.
- Every connection carries one request and is closed.

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
and 320 held polls. **Work in flight is bounded too:**

- at most 512 connections to the route at once; one more is closed as it arrives,
  unread;
- at most 64 connections waiting for one of the route's six threads; past that a
  connection is told the relay is busy and closed, unread;
- a request's head has a deadline (10 s) and a size (8 KiB);
- a body is read only after the host key and token have been checked, only if it
  states its length and that is within what its call takes (else 411 or 413 with none
  of it read), and it has a deadline (10 s);
- a connection refused with some of its body unread is given a quarter of a second
  and 64 KiB at the most to finish before it is closed, so the refusal is not thrown
  away at the other end; the body itself is never read;
- every reply, head and body, is charged against 8 MiB **before it is built**, and
  until it is written (a poll whose answer does not fit waits, with its frames, for
  the next round); a write that makes no progress for 20 seconds is given up.

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

A message is JSON of at most 210,000 bytes. Unknown fields are ignored. A field a
version knows and finds wrong is never guessed at. **A message that names a field
twice is not read**, whichever field and however deep in it: a reader that kept one
of the two would let a message say two things of itself.

**Asks** (kind 1):

| Message | Meaning | At most |
|---|---|---|
| `{"t":"look","want":w,"speaks":2}` | Tell me your state. `w` is `brief` (for the list), `full` (for steering: what a jam's state holds today), or `queue` (everything playing and to come, by id, to carry on with it elsewhere). | 64 bytes |
| `{"t":"do","do":name,"at":[g, m],"with":{…},"speaks":2}` | A command, made against generation `g` and minute `m`. | 256 bytes; 4,096 for `add`; 210,000 for `adopt` |

**The commands** are what one PC could do to another before this, and no others. None
is a toggle: each says the state it wants, so one delivered late, or twice, cannot undo
itself.

| `do` | `with` | |
|---|---|---|
| `play`, `pause`, `next`, `previous` | absent, or `{}` | Anything else sent with one of these, and it is not carried out. |
| `seek` | `to` | Seconds into the song: 0 to 86,400. |
| `volume` | `to` | 0 to 1. |
| `jump` | `at`, `id` | Play the entry `at` places after the current song now. |
| `move` | `from`, `to`, `queued`, `id` | Move an entry; `queued` says which part it lands in, "Next in queue" or the rest. |
| `remove` | `at`, `id` | Take an entry out. |
| `clear` | `queued` | Empty "Next in queue", or else the rest. |
| `shuffle` | `on` | |
| `repeat` | `mode` | 0 off, 1 the queue, 2 the song. |
| `add` | `ids` (1 to 50), `next` | Queue these songs: at the bottom of "Next in queue", or with `next` at its top. |
| `adopt` | `songs`, `index`, `queued`, `position`, `context` | Take this queue and play it from `songs[index]`, `position` seconds in. `queued` is how many straight after it were queued by hand. |
| `moved` | absent, or `{}` | Stop: the music that was playing here is sounding on the device that asks (5.7). Carried out as a pause, and judged like any other; but it is nobody's choice at the device that goes quiet (5.6.1). |

- A place in the queue is a whole number up to 100,000. **An entry is named twice,** by
  its place and by the id of the song that was seen there: the device being asked
  takes the entry at that place if it is still that song, and otherwise the nearest
  copy of that song. If the song is nowhere in what is to come: `cannot`.
- **A song's id** is its id on the music server, which every device of the account
  shares: 1 to 64 visible ASCII characters, none of them `"` or `\`.
- `adopt` and a `queue` state never carry a radio station.
- `core/` reads each command by one function, which checks that every field it needs
  is there, of its kind and within its bounds, before anything is done for it.
- **Carried out as the same press would be.** Each command does, on the device that
  receives it, exactly what the same press at that device does, and on that device
  only: a device that is itself steering another, or is in a friend's jam, carries a
  command out on its own player and passes nothing on. So `add` queues as "Add to
  queue" does (it starts nothing by itself, and a song already queued is queued
  again), where the old linking queued as a jam's guest does.

**Tells** (kind 2), one shape for an answer and for news:

```json
{"re": "…", "ok": true, "why": "…",
 "device": "…", "name": "Study PC", "speaks": 2, "at": [8, 12], "by": "…",
 "playing": false, "title": "…", "artist": "…"}
```

| Field | |
|---|---|
| `re` | The nonce of the ask this answers. Absent in news. |
| `ok`, `why` | In an answer to a command. Refused: `ok` false, and `why` is `changed`, `busy`, `late`, `invalid` or `cannot`. Carried out: `ok` true. **Nothing to do** (the device was already as the command asked: playing, for a `play`): `ok` true with `why` `nothing`. |
| `device`, `name` | Who is speaking: its device id and the name the person gave it. |
| `speaks` | How its sender speaks (5.10), in every ask too. Left out: 1. |
| `at` | Its generation and minute now. |
| `busy` | A choice has been accepted and not yet carried out, or its work is still under way (5.6, 5.7). |
| `by` | The nonce of the command that made this state, if nothing has been chosen since. And, **while `busy`**, the command the device is busy with: one carried out whose queue has not sounded yet, or one accepted whose songs are still being looked up. |
| `done` | The command named in `by` was **carried out, to its end**. Absent while the device is still at work on it, and if that work came to nothing (a queue it took that never sounded). It is the only thing a sender with no answer of its own takes for success. |
| `steering` | The name of the device this one is steering, if it is steering one. |
| `full` | In answer to a look that asked for it: `position`, `now` and `queue` (entries of `id`, `title`, `artist`, `album`, `duration`, `queued`), `more`, `context`, `shuffle`, `repeat`, `volume`. |
| `queue` | In answer to a look that asked for it: `songs` (ids, the one playing first), `queued`, `position`, `context`, `more`. Empty when nothing is playing that another device could carry on with. |
| the rest | What it is playing. Texts are cut to 80 characters by the sender and again by the receiver. |

| A tell | At most |
|---|---|
| With neither `full` nor `queue` | 4,096 bytes |
| With `full` | 70,000 bytes. Its queue takes entries until they come to 60,000 bytes or there are 200 of them, and counts the rest in `more`. |
| With `queue` | 210,000 bytes. Its songs are cut from the far end to fit, with 4,096 bytes left for the rest of the tell, and the rest counted in `more`. |

**What makes a state fresh.** Only an answer whose `re` is the nonce of a `look` the
device is waiting on, from the place it asked. Each waiting nonce is used once. Its
age is counted from when that look was sent. An answer to a command settles that
command and nothing else: it never replaces a newer state on the screen. (An answer
that says a command was carried out is also the state the **next** command is made
against: 5.6.) Anything else that opens is dropped.

**News.** A device says, unasked, that what its brief state says has changed: a tell
with no `re`, sent to every other place in the room.

- **News is only ever a reason to look.** The device that receives it asks, and shows
  the answer. News is never shown and never acted on itself, because anything on the
  way can deliver it again later; so it never makes a state fresh.
- **Bounded at the sender:** only when the state has changed since the last news, and
  no more often than once in two seconds, or in 400 ms for each other place if that
  is longer. So news to a full room is at most two and a half frames a second, half
  what the relay lets one place send. A change inside the gap is told when the gap is
  over, once, as the state then stands.
- **Bounded at the receiver:** news from one place prompts a look at it at most once a
  second, and the answering device still handles at most 20 asks a second.

**Looking.** The device being steered is looked at every five seconds, with `full`.
Every other device is looked at every thirty seconds, with `brief`; in between, its
news prompts a look. A state on the screen that is 50 seconds old says how old it is.

An answering device handles at most 20 asks a second and drops the rest.

### 5.6 The rule for commands

**The rule exists for one purpose:** so that a command from another device is judged
against what is true on this one. It never judges what the person does at the device
itself. Nothing they press there is refused, delayed or dropped because of it, with
linking on or off: every press does exactly what it would do without linking.

**A device has a generation,** a number that starts at 1 with the app and moves on
whenever what a command would act on changes:

- **a choice made at the device being carried out:** play, pause, next, a new queue,
  a seek, mute and volume, an edit to the queue, a sleep timer set or cancelled; a
  guest's action in a jam; a command from another device. A press that changes
  nothing (Next with nothing to go to, Pause when paused) takes no place;
- **by itself:** the next song starting (at the sound's own boundary: see below),
  autoplay adding songs, a followed playlist changing the queue, a timer's pause, the
  sound output being lost. Not the position ticking.

**Busy.** A choice whose work is still to do (a Play whose songs are being looked up)
marks the device **busy** from the press until the work is done or has come to
nothing. Its state says so, and a command from another device is refused `busy`: the
state it was made against is about to change. Several choices can be waiting at once,
each as it would be without this; none displaces another.

**Every wait has an end, and the end of a session ends its waits.** A choice's wait
ends by itself after a minute. A command that has been accepted is given half a
minute for its work (its songs looked up, a player on another thread saying what it
did): after that the device gives it up, is busy with it no longer, tells its sender
it took too long, and takes nothing from what the work then brings back. A queue
taken from another device keeps the device busy only until its sound starts or its
time is up. And when the session ends, whatever ends it, everything that was waiting
under it is let go there and then. Nothing can leave a device busy for ever.

What marks a device busy: Play, Play Next and Add to Queue on anything whose songs
have to be looked up; a radio being started; a guest's request in a jam, and one from
a device linked the old way, from the moment it is let in until it has been carried
out or refused; the wait for autoplay's songs when a queue has run out and they are to
start as they land; a command from another device from being accepted until it is
carried out; and a queue taken from another device, until its sound has started (5.7).

**And a minute:** whole minutes since the app started, on a clock that goes on
counting while the device sleeps (Windows: interrupt time; Android:
`elapsedRealtime`).

**The rule.** A command quotes the `at` of the fresh state it was made against. When
it arrives the receiving device checks, before any work:

- it is not busy (`busy`);
- the generation quoted is its generation now (`changed`);
- its minute now is the minute quoted, or the one after (`late`).

If any fails it does nothing and answers with its state. Otherwise the command is
accepted, and is the one open command: another is refused `busy` until it is done.
**While it is open the device's state says `busy` and names it in `by`**, so that its
sender, looking because no answer has come, sees that it arrived (and only while
busy: a state that named it and was not busy would say it was done).
Whatever the command needs is then found (the entry it names, the songs it brings,
by asking the music server if this device's copy of the library lacks them: for no
longer than the command's half minute, looked at between one song and the next, and
what is found is kept only if the command is still waiting for it under the sign-in
it was started for). If it
turns out to be one this device cannot carry out (the entry is gone, it has none of
the songs, nothing is in the player to act on) it is let go and answered `cannot`:
nothing was changed, so nothing moves.
**At the moment the player is changed** the device checks that it is still the open
command and inside its half minute, that the generation has not moved since it was
accepted, the minute, and that the session it arrived under is still the live session
and inside its time (5.1). That is leave to change the player, and the player is
asked. **The player says which of three things came of it**, and the device does not
work that out from "nothing changed":

- **it changed something**: the generation moves, once, `by` becomes the command's
  nonce, and the answer says `ok`;
- **it was so already** (a `play` when it was playing, a `pause` when paused, the
  volume, shuffle or repeat set to what they are, a queue cleared of nothing, an entry
  put down where it was): nothing moves, `by` is as it was, and the answer says `ok`
  with `why` `nothing`. Its sender takes it as settled and says nothing;
- **it refused, or could not** (the device's own rules would not start playing just
  now, as a phone's will not while a call is on; a seek on a radio; `next` with
  nothing to go to): nothing moves, and the answer is `cannot`. What its sender had
  waiting behind it is dropped, as after any refusal.

**The stop that ends a move (`moved`) is judged by one thing: the player says it is
not playing.** Nothing is recorded for it before that is asked. If the device is still
playing, whatever it changed on the way is counted as a change no command is named
for, its queue is its own still, and the answer is `cannot`: so the answer that says
it could not never also names the command in `by` with `done`. (The one answer that
refuses a command and names it as done is the refusal of a second copy of a frame
whose first copy was carried out, and there both are true.)

**A call into the player cannot fail.** There is no answer for "it could not be
asked", on either app, and nothing stands in for an answer that was not got. On the
phone, where the player is across a boundary, a call that does fail is a fault
(section 8): linking stops on that phone until the app is next started, and it
answers nothing more.

A command is finished once. It carries out its action on this device only: a Pause
never goes on to a jam the device has joined. **The player is asked, and has answered
when the call returns**, on both apps: the phone's on the thread it lives on (section
8). Nothing waits on a player.

So a repeat fails (its first delivery moved the generation), a command held back
fails once anything changes there or two minutes pass, and a
command overtaken by the person at the device fails. **A Pause that crosses the start
of the next song is refused once like anything else,** and the sender looks again.
Nothing is remembered about past commands but `by`.

**The sending device:**

- sends a command only against a fresh state (5.5) that is not busy and is less than
  45 seconds old; otherwise it looks first;
- sends one command to a device at a time. A press made while one is unanswered waits
  (up to eight; a volume slider keeps only its latest), and is sent only after an `ok`,
  against the state in that answer. A refusal or an unknown outcome discards them.
  **A refusal as `busy` that names the command itself in `by`** is no refusal: the
  command was taken and its work is under way (a queue that has not sounded yet), and
  what was refused was a repeat of it made on the way. The sender goes on waiting;
- with nothing unanswered, sends a press at once or not at all: it never waits for a
  look and then goes by itself, against a state the person had not seen. After an
  `ok`, the state in that answer serves for the next press too, until a look's answer
  replaces it or it is 45 seconds old, counted from when its command was sent;
- **takes for "done" only what the device says is done:** the answer's own `ok`, or
  `by` equal to its command's nonce **with `done`**, whatever else the tell says. `by`
  with `busy` is "taken, and its work is still under way" (its songs are being looked
  up, or the queue it brought has not begun to sound): it waits on, and asks again
  every 12 seconds, four times at the most, and never says "hadn't reached" of it.
  `by` with neither `busy` nor `done`, found at the check, is "it could not": the work
  came to nothing. (A device that took a queue and ran past its wait without sounding
  names the command and is idle, and its sender must not stop its own music for that.)
- after 12 seconds with no answer, looks once and says what is true: `by` is its
  nonce with `done`, done; the generation is as it was, "That hadn't reached Study PC
  when checked" (it may still, within its two minutes); anything else, "Study PC changed.
  Check what is playing before you press again." It never sends a command again by
  itself, and an answer that arrives after this is still believed. A success is
  believed whenever it comes, once, even after a refusal of the same command (a repeat
  the relay's path made can be refused first); a refusal is never believed over a
  success.

**What this costs.** Two people steering one device at the same instant: one is
refused and shown the result. A device someone is busy with refuses commands from
elsewhere while each choice lands: whoever is at the device wins. A command sent as
a song ends is refused once. After "not known" the person may press again and get
two.

**The sound's own boundary.** The sound rolls into the next song on the audio thread,
and the window is told a moment later. A command that arrived in between would be
judged against a generation the sound has already left: Pause would do what was meant,
but `seek` and `previous` act on "the current song", and would have acted on the one
that had just ended. So the generation moves **at the sound's boundary**: before
anything from another device is judged, answered or carried out, the app reads what is
sounding, and if the sound has rolled on it brings the window up to it there and then
(the cursor and the generation move exactly as they will when the audio thread's own
word arrives, and that word is then not acted on a second time). Every command that
crosses the start of a song is therefore refused `changed`, once, whichever side of
the window's catching up it lands on.

**Accepted, and not fixed now.**

- A Pause pressed at a device that is already paused changes nothing and takes no
  place, so it does not stop a Play another device sent a moment before and the path
  held back.
- The old linking and the new can run side by side on one PC while both are switched
  on, each as it is. "Link my devices" off ends both.

**What still needs time:** the minute; the ten minutes a secret is used; how long a
sender waits before it stops waiting; and the minute a look-up is given before a
device stops counting as busy for it. No clock is compared between devices and no wall
clock is used.

#### 5.6.1 Every way into the player on Windows

Each way in, where it counts, and where it was seen to. "The same press" means it goes
through the same function as the row it names, and is counted there. The walk is
`ways.py` beside the other walks: one headless copy that writes down each move of its
generation, each time it is busy and each time its queue is claimed, driven through
each way in turn. Thirty-seven ways, each as the table says.

**Each way in says one thing, once: who chose it and what it touches** (`core::ways`;
on Windows, `way_in`, `came_in` and `for_another` in `src/ways.rs`). Two rules are
worked out from that statement, and neither keeps a list of its own: this section's
order of choices, and whose the queue is when devices share it through the server's
saved queue (`core::handover`, where a queue the person has made their own is never
replaced by itself). Who chose it is the person at this PC, unless a function the app
runs by itself says nobody did, or whoever carries out another's choice says whose it
is first: a command from another of the person's devices, or what a guest asked for.
What is under way is one choice, counted once and claimed once, and is whoever's the
outermost is: the sleep timer pauses by running Pause's function, and that is the
app's own doing all the same.

The queue is this PC's own by a way in when a person chose it (here, at another of
their devices, or as a guest) and it touches what a saved queue holds: what is in the
player and how it is playing. The volume, mute, repeat and setting the sleep timer are
counted and claim nothing. **A command from another of the person's devices claims as
the same press here would**, by the old linking and the new alike.

| Way in | Enters the player at | Who chose it | Moves the generation | Busy while it waits | The queue is this PC's own by it | Seen |
|---|---|---|---|---|---|---|
| Play, Pause: the window's button, Space, the tray, the taskbar's buttons, the mini player, the media keys | `toggle_play_here` | The person here | As it toggles. With nothing in the player: no. |  | Yes | The walk: the button, Space |
| Next, Previous: the buttons, Ctrl+Right and Ctrl+Left, the tray, the taskbar, the mini player, the media keys | `next_track_here`, `previous_track_here` | The person here | As it goes. With nothing to go to: no. (Previous that starts the song again is a seek, and on a radio station with nothing before it is nothing.) |  | Yes | The walk: the buttons, the keys |
| Seek: the bar, the keys, the media keys | `seek_here` | The person here | Yes. Not on a radio, which has nowhere to seek to. |  | Yes | The walk |
| Volume; Mute | `set_volume_here`; the mute button's own handler | The person here | Yes |  | No: a saved queue holds neither | The walk |
| Shuffle, Repeat | `set_shuffle`, `set_repeat` | The person here | Yes |  | Shuffle: yes. Repeat: no | The walk |
| The queue's own edits: double-click an entry, Remove from queue, drag, Clear | `jump_to_upcoming_here`, `remove_upcoming_here`, `move_upcoming_here`, `clear_upcoming_here` | The person here | Yes. Naming a place that is not there, or putting an entry down where it was as what it was: no. |  | Yes | The walk |
| Playing something: an album, playlist, artist or mix; a page's Play; a song in a list; a suggestion; Recently played; a radio station; a song's radio | `play_songs` | The person here | As the queue is replaced. If nothing in it can play (offline): no. | From the press until the songs arrive, where they are looked up | Yes, as the queue is replaced: where the songs are looked up, when they arrive | The walk: an album (looked up), a song in a list, a look-up the music server never answers |
| Play Next, Add to Queue: a song, songs dragged onto the queue, an album, a playlist | `enqueue_here` | The person here | Yes | From the choice until the songs arrive, where they are looked up | Yes, as the songs are queued: where they are looked up, when they arrive | The walk: a song; an album, looked up |
| A guest in a jam, and a device linked the old way | `jam_asked`, then the same functions as the presses above | A guest; or the person at another of their devices (the old linking) | As each is carried out | From being let in until carried out or refused | Yes, for what touches what plays; a guest's volume or repeat, no | The walk: a guest's add, skip and play, and a search (which changes nothing). The second slice's walk: a device steered the old way while the new runs |
| A command from another device | `core::link::device` (judged), then the `_here` function of the same press under `for_another` | The person at another of their devices; nobody, for `moved` | Once, by the rule, if the player changed anything for it; what the player does for it is not counted again | From being accepted until carried out; a queue taken over, until its sound starts | As the same press would: yes, but for volume and repeat; and not for `moved`, which is nobody's | The second slice's walk: every command, and its repeat refused `changed` |
| The sound rolling into the next song | The engine's word, or the app reading what is sounding before it judges another device's command | Nobody | At the sound's boundary |  | No | The walk |
| A song ending; the queue running out | The engine's word | Nobody | Yes |  | No | The walk: the queue running out, autoplay off |
| Autoplay | `autoplay_loaded` | Nobody | As the songs are added: ahead of time, while the last song plays | Only if they have not arrived when the queue runs out: from then until they land | No | The walk: both |
| A song that will not play being passed over | `pass_over` | Nobody | Yes |  | No | The walk |
| The sleep timer: set, cancelled, its pause | `set_sleep`, `check_sleep`, the end of a song | Set, cancelled: the person here. Its pause: nobody | Yes |  | No: setting it touches nothing a saved queue holds, and its pause is the app's own | By reading |
| The sound output being lost | The engine's word | Nobody | If it was playing |  | No | By reading: it cannot be made to happen in a headless window |
| A followed playlist changing the queue | `source_list_now` | Nobody | Yes |  | No | By reading |
| Sync's "Continue here" | `sync.rs`, then `replace_queue` | The person here | Yes |  | Yes, once the queue is in (said by `continue_here` itself: putting it in claims nothing) | By reading |
| The queue restored as the app starts | `restore_queue` | Nobody | No: it is put back before the app has told anyone a state, so there is no command it could be made against | | No: a queue taken up from another device is still that device's after a restart | By reading |
| Sign-out emptying the player | `stop_playback` | Nobody | Yes | | No | By reading |
| "Play here": another device's music brought to this PC | `core::link::device`, then `load_queue_here` | The person here | As the queue is taken | Until its sound starts | Yes, as "Continue here" does | The second slice's walk |
| The pause that ends a move of this PC's music to another device | `core::link::device`, then `pause_here` | Nobody | Yes |  | No: the person chose that the queue go there, and that device claimed it as it took it. From just before the pause the queue here is that device's (5.7) | The second slice's walk |

Not counted, because no command acts on them: the position ticking, a radio's song title
arriving, a song being liked, the sound settings and the output device being changed (the
same song goes on).

### 5.7 Moving music

Two ordinary commands, each judged by the one rule. **The other starts, then this one
stops,** so that anything that gets in the way leaves music playing somewhere, and the
person told where.

- **To the other device** (it is picked in the list while this one is playing). This
  device, the source, notes its own generation, then sends `adopt` with its queue from
  the song that is playing, and where in it. The other checks that its library has
  the songs before changing anything (`cannot` if it has none), takes the queue,
  starts it, and **answers `ok` only when its sound has started**: the sound of the
  queue it took, and of no other (below). Until then its
  state says `busy` and names the command in `by`. On that answer, or on a state that
  names the command and is no longer busy, the source pauses and the player in its
  window becomes the other device's, **only if** its own generation is still the one
  it noted **and** the press was no more than 30 seconds ago.
- **From the other device** ("Play here", while steering it). This device looks with
  `want: queue`, takes the queue and starts. Once its own sound has started, if
  nothing else has been chosen here since and the press was no more than 30 seconds
  ago, it tells the other the music has `moved`, **quoting the `at` of the state it
  took the queue from.** From loading the queue until then it is busy to the other
  devices, as a device that was sent a queue is. If its sound has not started when the
  30 seconds are up, **it is stopped**, not left to start later with the other still
  playing.
- **Whose the queue is afterwards** (5.6.1). The device that takes the music claims
  its queue as it takes it, by `adopt` or by "Play here". The device that goes quiet
  claims nothing by it: its stop is the app's own doing (the pause that ends a move
  to another device) or `moved` (the stop that ends "Play here"). So the device that
  is playing is the one whose saved queue is the newest.
- **A queue that has been handed to another device is that device's.** Not claiming
  is not enough: the stop itself, on a device that has played on since its last save,
  is a change to save, and saved a moment after the taker's first save it would make
  the silent device the newest. So, in the terms of `core/src/handover.rs`: from the
  moment the music has moved (`moved` carried out, or the other device started with
  what this one gave it), and **before** this device's sound is stopped, its queue is
  in the same state as one it took up from that device and has not touched. **It is
  not saved, and it follows** (what that device saves is put in its place, paused),
  until the person uses it here. A move that did not happen hands nothing on. Until a
  device is told, it is a device that is playing, and saves as one: with the word held
  on the way both sound, and both save.
- **Sound has started** when the song is playing and has moved on a fifth of a second
  from where it was first heard: a song that has been opened is not yet sound.
- **The queue's number.** The player numbers the queue it holds: **a new number each
  time a whole queue is in the player in place of what was there** (a list started
  there, a queue from another device or the server, the last run's queue put back,
  the player emptied), at that one point and no other. Changing a queue in place
  keeps its number, and is a way into the player, which moves the generation before
  another command can act. Whether a queue that was to be taken **is in the player**
  is the number having moved, and nothing else: only then is it counted and claimed,
  and the sound a move waits for is the sound **of that queue**. So a queue that never
  went in (the player refused it, the app could not build it) is said ("This PC
  couldn't load that"), and the queue that was there before, still playing, does not
  stand in for it and stop the other device. And a move that gives up waiting for its
  queue's sound stops **that queue**: if another whole queue is in the player by then,
  it is left playing.
- **The mark of a queue handed on belongs to that queue,** by its number
  (`core::handover::Mark`). It is put on before the stop, on a device that was quiet
  already too, and written at once where the next start of the app will find it. If
  the device then did not stop, it is taken back, from the queue it was put on and no
  other.
- **A device that took a queue** gives its sound 20 seconds. If something else is
  chosen on it first, it answers `changed` and does whatever was chosen. If no sound
  comes, it pauses and answers `cannot`. Twenty is less than thirty so that its
  answer can still complete the move.

**What the person is told,** on the device they pressed at:

| What happened | Said | Playing |
|---|---|---|
| The other started in time; nothing was chosen here meanwhile | "Playing on Study PC." | There |
| The other started; something was chosen here meanwhile | "Study PC has started. This PC was given something else to do meanwhile, so it was left as it is: both may be playing." | Both, possibly |
| The other started, more than 30 seconds after the press | "Study PC has started, too late for this PC to stop by itself. Both are playing." | Both |
| The other refused: `busy`, `changed`, `late` | "Study PC was in the middle of something" (or: was given something else to do before it started; that took too long to reach it) "…Still playing here." | Here |
| The other could not play it | "Study PC couldn't play that. Still playing here." | Here |
| No answer; the other looked at and found as it was | "That hadn't reached Study PC when checked. Still playing here; Study PC may yet start." | Here; and there too if it arrives within the rule's two minutes, when the first or third row is then said |
| No answer, and the other changed or could not be looked at | "Study PC didn't answer. Still playing here; check whether Study PC started too." | Here; there not known |
| "Play here": the other's queue never came, or came after its 12 seconds (it is not taken then, whether or not a tick has yet said so) | "Study PC didn't answer, so nothing was moved. It may still be playing." | There, as it was |
| "Play here": this device has none of the songs | "This PC's library has none of those songs. Study PC is still playing." | There |
| "Play here": the queue could not be put in this device's player | "This PC couldn't load that. Study PC is still playing." | There, and whatever was playing here before |
| "Play here": started here; the other was told and did not stop | "Playing here. Study PC couldn't be stopped, so it is still playing too." | Both |
| "Play here": started here; the other left the room while this one was starting | "Playing here. Study PC is no longer there to be told, so it may still be playing." Nothing is sent to the place it had. | Here; there not known |
| Giving: the other started, and this device did not stop when it should have | "Study PC has started, and this PC couldn't be stopped. Both are playing." | Both |
| "Play here": this device's sound did not start in 30 seconds | "This PC didn't start in time, so Study PC was left playing." | There; this device is stopped |
| "Play here": something else was chosen here before the sound started | "Something else was chosen here, so Study PC was left as it is: it is still playing." | There, and whatever was chosen here |
| "Play here": started here, and the other paused | "Playing here. Study PC is paused." | Here |
| "Play here": started here; the other refused the pause | "Playing here. Study PC was given something else to do before it could be paused, so it may still be playing." | Both, possibly |
| "Play here": started here; the pause was not answered, or its answer can no longer come (the other left the room, this device has a new place in it, linking was reset here or the session ended) | "Playing here. Study PC didn't answer, so it may still be playing." | Here; there not known |
| "Play here": started here; the other is on a version that does not know `moved` (it answers `invalid`) | "Playing here. Study PC couldn't be told to stop, so it may still be playing." | Both |
| "Play here": linking ended before the other could be told (the secret reset, the sign-in changed, "Link my devices" off, no place at the relay) | "Playing here. Linking stopped before Study PC could be told, so it may still be playing." | Here; there not known |

**Still not promised.** That a device which cannot play is left as it was. It checks
that its library has the songs before it changes anything; but if the music server
then fails to give it the song, the queue it had is gone, and it is left paused on
the one it was sent. Keeping the promise needs the new queue made ready beside the
old one before the old is stopped, which the Windows player does not do.

**While steering.** The player and the queue panel on the steering device are the
other's, and what is pressed is asked of it. If the device being steered has had no
place in the room for 45 seconds, the player goes back to the steering device and
the person is told. That happens after "Reset linking" too, when the other device
takes longer than that to move to the new secret: it is steered again by picking it.

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
list shows them at once next time: the sixteen seen most lately, each for thirty days
after it was last seen. **A remembered device belongs to the account it was seen
under**: it is kept with that sign-in and listed for it only, so another account
signed in on the same PC or phone is not shown the names of somebody else's devices,
and they are there again when the first signs back in (sixteen for an account,
sixty-four together). One that is remembered and has no place in the room is listed
as not being there, and cannot be picked. A device started again is in the room twice
for a while; it is shown once, and what is asked of it goes to the place that answered
last. After "Reset linking", a device still on the old
secret stays in the old room, steerable by any holder of that secret, for up to two
minutes (ten if it cannot reach the music server). Then it leaves.

### 5.10 Devices of different generations in one room

**Built, in the library both apps share, switched off with the rest.** The rule is
one sentence, and it is held at every point where one device acts on another, each by
itself (the table below).

What devices say to each other has changed while linking has been switched off: a
state now says `done`, where a reader once worked "done" out from `by` and `busy`
(5.6). A device that reads `done` and one that does not disagree about the same tell:
the older takes a queue that was taken and never sounded for one that started, and
stops the only device that is playing; the newer takes the older's carried-out
command, which names it in `by` without `done`, for one that came to nothing. So two
devices may steer each other only if they read a tell the same way, and each has to
know that of the other before it sends anything.

- **Every tell and every ask says how its sender speaks**: one number, `speaks`, 2
  for the first build that is ever switched on. A message without it is from a device
  that speaks 1: every build released while linking was switched off. The number moves
  only when the meaning of something already said changes, not when something is
  added that an older reader can ignore.
- **A device steers, gives music to and takes music from only a device that speaks
  as it does.** One that does not is still **listed**, with its name and what it is
  playing, which every generation says the same way, and under it "Update Nexus Music
  on Study PC to control it from here" (or "…on this PC…", when it is this one that is
  behind and knows it, because the other's number is higher than its own). It cannot
  be picked, and nothing is sent to it but looks.
- **A device carries out a command only from a sender that speaks as it does.** Any
  other is refused as `invalid`, the word every generation already has for "not one
  this version carries out", so a device that knows nothing of `speaks` is told
  something it can say ("couldn't do that") and changes nothing.
- **Nothing is negotiated**, and no device speaks down to another: one rule to test,
  and both apps update themselves. The cost is that linking between two devices waits
  until both are up to date.

**The number as it is written and read.** `"speaks": 2`, a whole number from 1, in every
look, every command and every tell (an answer, and news). Left out, it is 1. There and
not such a number (0, a fraction, text), the message breaks its shape: an ask is answered
`invalid` and a tell is dropped, like any other field a version knows and finds wrong
(5.5). It fits inside the sizes every message already had, the 64 bytes of a look
included, so a device that knows nothing of it reads the rest as before.

**Where the rule is held.** A device that speaks otherwise is one whose number is not
this device's own, lower or higher.

| | What this device does |
|---|---|
| A command arrives from one that speaks otherwise | Refused `invalid`, **before the player is touched**: before the window is brought up to the sound, before the order of choices is asked to admit it, before anything is looked up for it. Nothing is counted and no place is kept for it. Its sender is answered, as any sender is. |
| A look arrives from one that speaks otherwise | Answered as any look is: that is what lists this device there. **But a look for the queue is answered with no queue**, "nothing here to carry on with", which every generation reads alike. Otherwise a device that knows nothing of the rule would take this device's queue, play it, and then tell this one to stop in a command this one refuses, and both would play. |
| Something is to be asked of one that speaks otherwise | **No command is sealed for it.** Every command this device sends (a press while steering, one that waited its turn, a queue given, the stop that ends "Play here") goes out through one function, which sends only to a device whose last fresh state said it speaks as this one does. One that has not been seen is sent nothing but looks either. The person is told which of the two to update. |
| One is picked in the list | Not steered, whatever the list on the screen showed when it was pressed. An app that gives something up for a pick (Windows leaves a device it steers the old way) asks first whether the pick will be taken, and gives it up only then. |
| The device being steered turns out to speak otherwise (it was updated and has started again) | Let go of as soon as a fresh state from it says so. What is pressed after that is this device's own. |
| "Play here", and the queue comes back from one that speaks otherwise | Not taken. Nothing is loaded, nothing is stopped anywhere. |
| An answer comes from one that speaks otherwise: to a command, or to the look that asks what became of one | **It never says the command was carried out**, whatever its `ok`, `by` and `done` say: two generations read those differently, which is what makes them two. What became of the command is "not known", and this device does what it does for not known: it does not stop, and it does not mark its queue as handed on. |
| The stop that ends "Play here", when the device the queue came from has since said it speaks otherwise | Not sent. This device plays; the person is told the other could not be told to stop. |

**An answer from a place its device has left** is held to the same, and for the same
reason: it cannot be taken for what it says. A device that is started again (an
update, or the person closing and opening it) comes back at a new place in the room,
its queue restored paused, and its old place is listed until the relay gives it up.
What its last run said may still arrive from the old place: "the queue you gave me has
started". So when a tell arrives, the device asks where the device that says it is
now, which is the place that last answered as it; if that is another place, the tell
settles nothing as carried out, nothing is made against it, and a queue it brings is
not taken. What became of the command is "not known". If the new place has not been
seen yet the device cannot tell, and the thirty seconds a move is given still bound
what it does.

**The note under the list is of two kinds.** Most notes say what became of the last
thing asked ("Paused Study PC."), and stay until there is something else to say. One
says how things are for as long as a device is steered: that the music is playing on it
("Playing on Study PC."). That one goes when the device is let go of, however that comes
about, so the line under the list never says the music is somewhere it is not.

**In the list**, a row for a device that speaks otherwise has what any row has, its name
and what it is playing, and one thing more: the sentence saying which of the two to
update. It is not to be picked and has no Pause. The sentence is under the row ("Update
Nexus Music on Study PC to control it from here"; "Update Nexus Music on this PC to
control it from here" when this one is behind), and where it is said away from the row
it names the other device ("…on this phone to control Study PC from here").

**What this does not cover.** A device that knows nothing of the rule (one that speaks
1) holds to none of it: it lists a newer device as any other, lets the person pick it,
and shows them "couldn't do that" for each thing they then press. Nothing is changed on
either device by that. And the rule is about what devices say to each other, not about
the relay or the secret: a change to either of those is 5.1's and 5.3's to carry, by
their own version numbers.

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

A guest's action counts in the generation as it is carried out, as a press does, and
like a press it displaces nothing the host has chosen. From the moment it is let in
until it is carried out or refused, the host's device is busy to the person's other
devices (5.6): what they would act on is about to change. A removal is applied before any
question collected in the same poll; a guest removed while their add is being looked
up can still have it applied, as today (accepted; it goes with the jam work). The
cases each app must pass, and the relay's changes for jams, are in version 2,
sections 7 and 8.

## 8. What goes in `core/`, and what it uses

**Everything that decides is in `core::link`,** so that the phone runs the same
deciding and writes none of it again. An app keeps only what is its own: threads and
timers, making the requests, its player, its window.

| In `core::link` | What it is |
|---|---|
| `secret`, `frame`, `text`, `message`, `command` | The record and its rules; the derivations; sealing and opening a frame; every schema and size; every command as data and the one function that reads it. |
| `rule`, `steer`, `moving`, `news` | The rule of 5.6 on both sides (with the presses that wait, and what the sender concludes after no answer); the decisions of a move (5.7) on all three of its sides; the bounds on news. |
| `find` | Asking the music server for songs a command brought that this device's copy of the library lacks: each request is given what is left of the command's half minute, counted from when the command arrived, as its own limit, the last one too. The device asks only for the songs it lacks, and puts together what the command is carried out with itself. |
| `look` | The look of 5.1 as one function over an `Http`: the list, the choice, the fetch, the write and the read back, with what it has learnt kept across its own retries, reset, and stopping the moment the sign-in ends. |
| `relay` | The stay at the relay as one function over an `Http`: the hello, entering, the long poll, entering again when the relay forgets the place, backing off, leaving. And a send. |
| `device` | **One device among a person's devices, as plain state.** Each thing that happens (a tick, a look's answer, what the relay brought, a press, a device picked, "Play here", "Reset linking", the setting, the sign-in ending) is a function that takes it and the time and gives back a list of things to do: start a look, begin or end a session, send this frame, say this, keep this list of devices, draw. It holds the session, each peer, a move in either direction, the news, the list. |
| `view` | The device being steered as the player shows it, and what a press means given what is shown (a toggle becomes Play or Pause; an entry is named by its place and its song). |
| `words` | Every sentence the person is told, for a PC and for a phone. |
| `seen` | The devices seen before: sixteen, thirty days, by name. |
| `turn` | One thing at a time on the thread the player lives on: what becomes of something that arrives while the deciding is inside a call to the player (below). |
| `bind` | The same deciding for an app whose player and library are not in Rust (below), with the one place what its requests bring waits for the player's thread. |
| `core::handover::Mark` (beside `link`) | The mark of a queue handed to another device: which queue it is a mark of, by number, and whose that queue was before. |
| `core::ways` (beside `link`) | Who chose a way into the player and what it touches, and what is owed for it: a place in the order of choices, and the queue being this device's own (5.6.1). |

It owns no thread, clock, file, socket or source of randomness. An app hands in:

- **the time** (milliseconds on a clock that counts through sleep) and the day;
- **random bytes** from the system, or the word that there are none;
- **an `Http`** (`core/src/http.rs`, the boundary the requests rules use): the app
  makes each request, and says when one is to stop. The look is given one that stops
  with the sign-in; the stay, one whose waits end when the session does;
- **its player,** as `device::Player`, **asked and answered in the one call**: what
  is playing in brief and in full, what could be handed over, the number of the queue
  it holds, where the sound has got to and in which queue, the songs to come and how
  many they are; carry this command out, and say which of three things came of it
  (it changed the player, it was so already, it refused); pause; take this queue;
  "your queue has been handed on", and "it was not after all" (5.7); and the order of
  choices kept beside the player (the generation, busy, `by`), because every way into
  the player counts there (5.6.1). **Each of these is a total function of the player's
  state: none can fail**, and there is no answer for one that did;
- **its library,** as `core::requests::Library` (`has_songs`): which of these songs
  it has.

**Windows** (`src/link.rs`) is then: the switch; the seam every way into the player
calls (`way_in`, `came_in`, `looking_up`, and `for_another` for a command or a guest's
ask, which says whose choice it is and is not counted twice); two `Http`s over the app's own agents; `Player` over the
`_here` functions of the player; the thread that keeps the place and the pool that
sends; and the window (the list, the other device's player and queue, the toasts).

**The phone** calls `bind::Phone`, and **its player is asked exactly as the Windows
app's is: on its own thread, with the answer back before the call returns.** Media3's
player lives on one thread (the main one), and the deciding lives there with it: in the
playback service, with the player itself (never through a `MediaController`, which
answers later). A frame arrives on a network thread, is handed to that thread, and
there the library opens it, decides, and calls up into Kotlin for what it needs. There
are no reports of what the player holds, no numbered acts, and nothing to cancel.

- **The app supplies** one object (`bind::PhonePlayer`): the questions and the acts of
  `device::Player` above, and `claim` (the queue is the phone's own from now, said once
  the change it is for is in the player). The order of choices and who chose what are
  kept in the library for it. **A call up that throws is a fault**, not an answer:
  nothing is put in its place (below).
- **The app calls** `Phone::happened`, on that thread: the tick; each way into the
  player as it is taken (`Event::Changed`, with who chose it and what it touches as
  in 5.6.1, whose every row has its like on a phone, with the notification, a
  headset, the car and other apps' controls added; `LookingUp` and `LookedUp` around
  a look-up for something to play); and what the person presses. **What the requests
  bring back** (a look's answer, what the relay handed over, songs found, the phone's
  copy of the library) is put, from their own threads, where it waits for the
  player's thread (`bind::Inbox`), and `Phone::turn` works through it there: the one
  place such things wait, and the one drain of it. What to show comes back as data.
- **A player tells its listeners of a change before the call that made it returns**,
  and the app's listener would tell the library, which is still inside that call. The
  rule for that is in the library (`core::link::turn`), not in each app: a change heard
  **while the library is itself changing the player** is that act, heard back, and is
  dropped (it is counted once, as the command, by its result); **anything else that
  waits for no answer** is kept and decided when the call returns, in the order it
  arrived; **a press**, which needs its answer at once and never comes from a listener,
  is refused. An app only calls, from wherever it hears.
- **What a frame costs that thread** (opened, decided, answered), built as the phone's
  library is and timed on this project's build machine (a desktop: a phone is several
  times slower): a look answered in brief, 8 microseconds; Next carried out, 10; an
  Add of fifty songs, 53; a tick, under 1. The dear ones are dear by their size: a look
  answered with everything the player shows (two hundred songs to come, which is all a tell carries, whatever the length of the queue), 0.5 ms, asked
  every few seconds by a device that is steering this one; with the queue to carry on
  with, 0.2 ms; and a queue to take over as long as one message carries (7,000 songs,
  280,000 characters), 7 ms, once, when a person sends one. What is not small stays
  where it was: the look, the stay at the relay, each send and each look-up of songs
  run on threads Kotlin starts, over the `Http` the phone's native side already gives
  the library, and hand what they bring to the player's thread; the phone's copy of
  the library is made into a set where it is read. **The secret never crosses to
  Kotlin.**
- **A sign-out or another account replaces everything** (the device, the order of
  choices, the look-ups under way, the copy of the library); only the numbers that
  looks, sessions and tickets had reached carry over, so that an answer still on its
  way to the last sign-in matches nothing in the next. **It is never refused and never
  waits behind frames:** arriving while a call is under way, it is applied before
  anything else, what was kept is dropped unread, and what is left of the call in
  flight asks nothing more of a player that may be the next account's already: **the
  fence is looked at before every call into the player and again after it**, so that
  nothing is done on the strength of an answer the old sign-in's player gave while
  the sign-in was ending, and **all that the call had come to is dropped**, not only
  the part that was under way. **Each sign-in has a number**, a new one from the
  moment the app says the last is over, whoever signs in next: a copy of the library
  is handed over with the number of the sign-in it was read for and is dropped if
  that is not the sign-in now, so the same account signing in again does not get the
  copy an old worker was still reading.
- **A fault.** If anything inside a call stops unexpectedly (a call into the player
  that failed, or a fault of the library's own), it is not left half-way and it is
  not gone on with: everything is replaced, as at a sign-out, one line is said of it,
  the app is told, and **linking is off on that phone until the app is next
  started**. Nothing in that run brings it back: not a sign-out, not another account,
  not a playback service made again in the same process. (Coming back at the next
  sign-in was built first and taken out: it needed a fault to be told from a fresh
  start by everything that starts, and the person's remedy is the same either way.)
  Until then the player is asked nothing, nothing is looked for and nothing is sent,
  so a fault that is still there cannot come round at every tick. The person is told
  once, in the library's words for both apps: "Linking has stopped on this phone
  until the app is next opened."
- **Every list has a bound, and nothing is dropped at the one that matters:** what
  the requests bring waits in one place (256 things, a megabyte, each counted at what
  it holds), and what does not fit is kept by the thread that brought it and offered
  again: the relay's frames are not counted as handed over until they have been
  taken. What the player's listeners tell from inside a call is kept the same way
  (256, a megabyte) and refused past that. One turn works through 64 things beyond
  what was waiting when something began it, and says when there is more. Look-ups
  counted as under way: 16, none past its minute. From one answer of the relay's: 17
  places, 64 frames and half a megabyte, chosen before any of it is copied, the rest
  left with the relay. Devices remembered: 16 for an account, forgotten by the
  calendar after thirty days. Songs to come that a command from another device may
  leave in a queue: 10,000.

Everything that crosses has serde, so each call is JSON in and JSON out, as the
phone's other calls are.

### 8.1 What the phone's adapter owes

The brief for whoever builds the Kotlin side, **complete by itself**: with this section
and the native layer (`native/src/link.rs` in the Android repository, which says the
same thing in code), the adapter is built without opening `core/src/link/`. The library
decides; the adapter is the only thing that knows the phone's player, and each line
here is something no rule in `core/` can check for it. File names are the Android
repository's, under `app/src/main/kotlin/org/medianexus/music/playback/`.

**Where it lives.** In `PlaybackService`, with the player the session exposes
(`QueuePlayer`, over ExoPlayer), on the player's looper (the main one). Every call into
the library (`linkNew`, `linkHappened`, `linkTurn`) is made on that looper, and every
call the library makes into the player arrives on it, inside one of those calls. A
`linkHappened` or `linkTurn` from another thread is an error, does nothing, and takes
nothing with it. With linking switched off none of this exists: nothing is created,
no listener is added, no tick runs.

**One linking for each playback service, and nothing of it is the process's.**
`linkNew` answers `{"link","sign_in"}`: the number of this linking, which **every
other call names** (`link`), and the number of its first sign-in. The sentence for the
person if it stops for a fault is asked for by itself, before anything is started
(`linkStoppedWords`, which needs nothing to have been), since a fault may be the
library no longer answering. The way up into the player, the place where what the
requests bring waits, and the library's half all belong to that linking and are let go
of by `linkClose(link)`, which the adapter calls as the service goes and after a
fault, and which may be called twice (`linkClose(0)` closes whatever there is: for a
start whose answer could not be read, which has no number to name). A call that names a linking that has been
closed, or that another has taken the place of, does nothing and fails; a request
still under way for it finds nowhere to put what it brought and lets it go. So a
service made again in the same process starts with nothing of the one before, and
nothing of the one before can reach it, whatever numbers the two give their sign-ins,
looks and sessions.

**The phone is in the room only while its playback service lives.** Android ends the
service when nothing plays and no screen of the app is connected to it, and the app
ends it at a sign-out; to the person's other devices the phone is then "not here just
now", and it is back when the service is next made (the app opened, something
played). So on a phone the same account signing in again is always a new linking,
never the old one carried on.

**The start leaves everything or nothing.** The library is asked first (`linkNew`
reads what it is given before it keeps anything, and puts the linking, the way up and
the waiting place in together, last); only when it has answered does the adapter read
the player, add its listeners, point the two lines of its requests (below) and begin
to tick. If any step fails, what the steps before it set up is taken down again, **and that is
a fault like any other**: the person is told, and linking is not started again in
that run of the app. It is started on the player's thread and nowhere else.

**What crosses.** UTF-8 bytes holding JSON, both ways, as with the phone's other calls
into the library. **Units:** a place in a song is **seconds, as a decimal number**
(`position`, `at`, a seek's `to`); a song's `duration` is whole seconds; a moment
(`now`, `since`, `until`, `since_ms`, `until_ms`) is **milliseconds on
`SystemClock.elapsedRealtime()`**, which goes on counting while the phone sleeps;
`today` is whole days since 1970 by the calendar clock
(`System.currentTimeMillis() / 86_400_000`), used only to forget devices not seen for
thirty days.

#### The player it supplies

One object, handed over once (`linkNew`). **Each of these is a total function of the
player's state at that moment. None can fail, and there is no answer for "could not be
asked".** If one throws, the native layer puts nothing in its place: it stops the
library's call, and that is a fault (below). So an adapter never catches an exception
to answer with a default; it lets it out.

There is no "unavailable" among the answers either. A player that holds nothing says
so in each answer's own terms (the last column); one that is paused answers as it
would playing, with `playing` false.

| Call | It answers | With nothing in the player |
|---|---|---|
| `brief()` | `{"playing","title","artist"}`. **`playing` is "sounding, or about to"**: set to play (`exo.playWhenReady`), with a song the player has made ready or is making ready (`STATE_READY` or `STATE_BUFFERING`), and nothing holding the sound back (`playbackSuppressionReason` none). Not `isPlaying` (false while a song loads), and **not the intent alone**: a player set to play that has reached the end, has stopped for an error, or was never made ready in this run is not playing, and Play is what starts it. For a radio station the title is the station's name. | `playing` false, both texts empty |
| `full()` | `{"position","now","coming","in_all","context","shuffle","repeat","volume"}`. `now` and each of `coming` is an entry: `{"id","title","artist","album","duration","queued"}` (`queued`: put in "Next in queue" by hand). `coming` is **the first 200 entries to come and no more**, in the order they would play; `in_all` is how many are to come altogether (`QueuePlayer.ahead`), known without walking the queue. The library cuts `coming` to what one message holds and counts the rest from `in_all`. `repeat` is 0 off, 1 the whole queue, 2 one song. `volume` is 0 to 1. | `now` null, `coming` empty, `in_all` 0 |
| `handed()` | `{"songs","queued","position","context"}`: what another device could carry on with. `songs` is the ids **from the song playing on**, in the order they would play, with **radio stations left out** (another device cannot be handed one); `queued` how many straight after the first were queued by hand, **counted along this very list** (from the entry that is current as ExoPlayer has it, and only entries that are given: one left out is not counted). A song this phone cannot play just now (not downloaded, offline) is handed on by its id all the same: whether it plays is the taker's to find. The library cuts the list to one message. | `{"songs":[]}`; **and the same when what is playing is a radio station** |
| `queue()` | **The number of the queue in the player.** It moves at one point: when a whole queue is in the player in place of what was there, which is the end of `QueuePlayer.load` (the model adopted **and** Media3 given its items; before anything is asked to play), and when the player is emptied (`clear`). So it moves for a list started here, a queue from another device or the server, the last run's queue put back, and the first songs added to an empty player; it does **not** move for an edit in place (adding, removing, moving, shuffling, following its list) or for the window of items sliding along. If building or loading throws part-way, the number has not moved. **The queue that is already in the player, handed to the session to be carried on with and come back to be loaded again, is not another queue**, and does not move it. Never used twice in a run. | the number, as ever |
| `sounding()` | `{"queue","at"}` **only while audio is advancing for the entry that was current when a queue was put in for a move of music** (`take`, `adopt`), in that queue; `null` otherwise, and for good once that entry is no longer the one playing. The rules: (1) when `take` or `adopt` has put a queue in, the adapter notes the queue's number and **the entry's own number** (`SongTag.uid`, never used twice in a run, so it is that queue's entry and no other's); (2) "advancing" is the audio track's own word, `AnalyticsListener.onAudioPositionAdvancing`, **which Media3 posts**: by the time it arrives another queue may be in the player, so it counts only if **the entry its `eventTime` belongs to** (the media item of `eventTime.timeline` at `eventTime.windowIndex`) is the noted one, never by what is current when it arrives; **and only if the event has a period of its own** (`eventTime.mediaPeriodId`): one without is filled in by Media3 from whatever is current as it is made, and proves nothing; (3) what was heard is let go of when the player stops playing (`isPlaying` false) or is given a new place to play from, **and is not believed again on a later word of the same loading of the entry** (the same period, by `windowSequenceNumber`): only of one that began after; (4) **one song ending and the next beginning, or the same one beginning again, ends it for good**: the library measures how far the sound has gone by subtracting places in one entry. `playWhenReady`, `isPlaying` and a position that has only been set do not show that a queue which was taken has sounded; and the service's own `heardThisTime` is a latch for its diagnostics and does not say which queue was heard. `at` is seconds into the entry. A queue the person started on the phone is waited on by no move and is never said to sound. | `null` |
| `hasSong()` | Whether there is a song in the player. | false |
| `ahead()` | The ids of **every** entry to come (radio stations too), in the order they would play, as a JSON list: the first is what Next goes to, and the same entry as `full().coming[0]`. | `[]` |
| `entryAt(n)` | The id of the entry `n` places after the song playing: what `ahead()[n]` is, without the list being made. **Asked first for a command that names an entry**; `ahead()` is asked only if this is not the song the command names (the queue has moved since its sender saw it). | null: the queue does not reach so far |
| `toCome()` | How many are to come: the length of `ahead()`, without making it. | 0 |

Acts, each done before the call returns:

| Call | It does |
|---|---|
| `carryOut(bytes)` | One command, below. Answers **1: it changed the player; 2: it was so already; 0: it refused, or could not.** |
| `take(bytes)` | `{"songs":[[id,queued],…],"begin","position","context"}`: this queue built and put in the player, **playing from** entry `begin` (zero-based) at `position` ("Installing a queue", below). It answers nothing: **whether the queue is in the player is what `queue()` says afterwards.** |
| `pause()` | The phone's own sound stopped, through the player's own pause (so that it takes its place in `Choices` once, as a pause there does). **It answers whether the phone is then not set to play** (`!exo.playWhenReady`): the intent, not whether anything sounds. A phone whose sound a call holds back, still set to play, has not stopped. |
| `claim()` | **The queue is the phone's own from now**: what `claimQueue` does (`queueTaken` cleared, `queueStamp` now, the save's revision moved, a save owed). Called by the library once the change it is for is in the player, for a command another device sent. |
| `handedOn()` | **The queue has been handed to another device**: `queueTaken` set to `HANDED` (`core::handover`), the save's revision moved, and the queue's place written. **It answers whether the queue's file holds the mark: this write's own result.** Each thing asked of the app's one writer of saved queues has a number, and the writer says the number of the last ask whose place reached the disk; the adapter waits on the player's thread for up to 250 ms and answers yes only if that number is the one it asked under (so an earlier write that once put the same mark there, for another queue, answers for nothing). **If not (the write failed, nothing is saved for this sign-in yet, or it did not get there in time), nothing is marked**: `queueTaken` is what it was, and the library refuses the hand-over before the phone is stopped (the device that asked is told this one could not; the phone plays on). Called before the stop that goes with it, and when the phone was already quiet too. |
| `handedBack()` | The phone did not stop: `queueTaken` is what it was, the revision moved, written the same way. **If that cannot be written it throws, which is a fault** (the clean-up has the queue the phone's own in memory, and a run without linking reads the mark as that). **Only if the queue in the player is still the one that was marked** (by number); if another whole queue is in the player, whose that is was settled when it was put there. |

**What a queue was before it was marked is kept from `handedOn` to the end of the call
into the library that made it, and no longer.** That is as long as the library can
take the mark back (it does so, if at all, straight after the stop that follows, in
the same call). So the record is never out of date: a queue marked, used here (its own
again) and handed on once more goes back to what it was at *this* hand-over, and
nothing goes back to "handed on". And there is nothing of it to keep through a
restart: a start finds the mark (it is kept with the queue's place), nothing is under
way to take back, and the adapter notes that the queue put back is the one that is
marked. **With linking switched off, a `HANDED` mark found at the start is read as the
queue being the phone's own** (`core::handover::restored`, and the same on Windows):
nothing in such a run hands a queue on or takes a mark back.

`handedOn` and `handedBack` are the two that must be on disk before they return;
`claim` is saved as any change the person makes is; the others change only the player. None of them can fail: an adapter that cannot do one lets the
exception out.

Three more, **from any thread**, which answer for themselves: `random(n)`: that many
bytes from `SecureRandom`, or null; `elapsed()`: the clock above; `wake()`: post one
`linkTurn` to the player's looper (never run it on the calling thread).

#### A command, as `carryOut` is handed it

`{"command":{"do":name,"with":{…}},"ready":…}`. The command is spelt as it is in a
message (5.5). **`ready` is what the library found for it against the player as it
stands now, and is what the adapter goes by**: `"nothing"`; `{"entry":n}`, the entry
it means, as **its zero-based place after the song playing, in `ahead()`'s order**
(the place the command itself names was the sender's view and may have moved: it is
not used); `{"songs":[id…]}`, the songs to queue, in the command's order, each one
playable here (in the phone's copy of the library, or brought by a `find` and already
in the adapter's hands); `{"queue":[[[id,queued],…],begin]}`, the queue to install and
where in it to start.

Each is carried out **by the same function a press on the phone reaches**, so that it
is the same act to the player, the car and the notification.

**The answer is the player after the call against the player before it**, and each
command is judged by the one thing it is for:

- **`play`: done is the player being about to sound** (`brief().playing`) after the
  call, and nothing less. That something about it moved is not enough.
- **`pause`, `moved`: done is the intent to play being cleared** (`!exo.playWhenReady`),
  not nothing sounding. The library takes a `moved` that was refused as not stopped,
  whatever is or is not sounding.
- **`next`, `previous`, `seek`, `jump`: done is another entry being current, or the
  place in the entry being somewhere the song playing on would not have taken it** in
  the time the call took (before where it was, or further on than that time and 150
  ms). So a Next with nowhere to go, a seek to where the player already is, and a
  Previous at the top of the first song are refusals, and a song simply playing on
  during the call is not a change.

Nothing is said to have changed on the strength of having been asked. **A command
that starts playing (`play`, `jump`, `adopt`, and `take`) asks the service's
`mayPlay()` first**, before anything is made ready, put in or moved to: one that may
not start leaves the player exactly as it was. **A phone that is already sounding or
about to is asked another question, one that touches nothing**: whether a start would
be allowed now, by what the service's gate goes by that can be read without doing
anything (its player is not showing another device's music, and the service is in the
foreground). The service's own "no" to a start stops the player, which is right for a
start that was refused and would stop music the person is listening to; so that gate
is never reached for a refusal. Refused, the phone plays on exactly as it was. (The
answer is a reading taken at that moment: if what it reads changes before the player
starts, the command can still meet the service's gate. Accepted.) A
command from another device never goes through a path that stops a phone that was
already playing. What each answer means:

| `do` | `with` | `ready` | 1, changed | 2, so already | 0, refused |
|---|---|---|---|---|---|
| `play` | | nothing | the player was set to play, **and made ready if it was not** (what the play button does: a queue put back from the last run has never been made ready, and asked only to play it sits set to play and silent until the service gives up on it) | **it was sounding or about to** (`brief().playing`); being set to play is not enough | **`mayPlay()` said no** (asked first; nothing was made ready), nothing to play, or nothing came of it |
| `pause`, `moved` | | nothing | it was set to pause | it was not set to play | it is still set to play |
| `next` | | nothing | another entry is current, or the same one started again | | nothing moved |
| `previous` | | nothing | as a press | | nothing moved |
| `seek` | `to` | nothing | the place is another | | **a radio station** (nowhere to seek to), or it was there already |
| `volume` | `to` | nothing | | | **always, on a phone**: its volume is the phone's own buttons', and `full()` says 1 |
| `jump`, `remove` | `at`, `id` | entry | it is the entry playing, or was taken out | | the entry is not there; for `jump`, **`mayPlay()` said no** (asked first: the queue has not moved to it) |
| `move` | `from`, `to`, `queued`, `id` | entry | it was moved to `to` places after the song playing, into "Next in queue" or out of it as `queued` says | it is there already, as that | the entry is not there |
| `clear` | `queued` | nothing | "Next in queue" emptied (`queued`), or the rest | nothing to clear | |
| `shuffle` | `on` | nothing | set | it was so | |
| `repeat` | `mode` | nothing | set | it was so | |
| `add` | `ids`, `next` | songs | queued by hand, at the top with `next` | | none of them could be made into a song |
| `adopt` | `songs`, `index`, `queued`, `position`, `context` | queue | **read from `queue()`, not from this answer** | | as `take` |

**Installing a queue (`adopt`, `take`).** The queue is built from the ids (each a song
the adapter can make: from the library's copy, or brought by a `find`), given a place
of its own in `Choices` as any start is, and put in with `QueuePlayer.playFor`. That
place is the newest, so the start stands, and every start still being looked up for
something else is overtaken by it. The rules, in full:

- **`mayPlay()` is asked before anything is put in.** If the phone may not start
  playing now, nothing is put in and `queue()` has not moved.
- **A song the adapter cannot make is left out**, and the rest keep their order. If
  none can be made, nothing is put in.
- **The `queued` flag comes in with each entry and goes out with it.** The entries
  marked so in the run straight after the one to begin with are put in "Next in queue"
  here, in that order (a mark further on, outside that run, is not one: the queue's
  model has one run). `handed()` says the same run again.
- **It begins with entry `begin`, at `position`**; from the top instead if that is in
  the song's last second, as on Windows. If that entry could not be made, it begins
  with the next that could (failing that, the last before it), **from its top**: a
  place in one song is never applied to another.
- **When `queue()` has moved, the queue is in the player whole**: the model has it and
  ExoPlayer has its items (`QueuePlayer.load`). `playFor` adopts the model before it
  gives ExoPlayer the items, as 0.5.1 does; if the second step throws, the exception
  goes out, the number has not moved, and that is a fault.
- The adapter then notes the queue's number and the entry it began with, for
  `sounding()`. Building a queue as long as one message carries (7,000 songs) costs the
library 7 ms on a desktop, **without** the call across, the Kotlin objects, or Media3
taking the items; that is measured on a phone build.

**A start under way is cancelled as a press would cancel it.** `play`, `pause`,
`moved`, `adopt` and `take` take a place in `Choices` through the player's own
functions; nothing else is needed for a start the car or a voice is still looking up
to give way to them, or to stand against what does not take one.

#### What the adapter tells the library

**`linkHappened({"link","event":…,"at":{"now","today"}})`**, with these events:

| Event | When |
|---|---|
| `tick` `{"account","wanted"}` | **Every 250 ms** while the service is alive, linking is switched on **and somebody is signed in**, from the main looper. `account` names the sign-in; `wanted` is a relay set up and "Link my devices" on. (With nobody signed in there is no tick: a tick that named nobody, and then somebody, would be read as one sign-in ending and another beginning.) |
| `changed` `{"who","what"}` | **Something changed what plays, or how, that the library did not do itself.** `who`: `here` (the person, on the phone, the notification, a headset, the car), `nobody` (the app by itself), `guest`; `what`: `playing` or `manner`. Below. |
| `press`, `add`, `play`, `play_here` | The person pressed something **while the phone's screens show another device's player**. Answered with `{"act":"handled","by_another":…}` as the last act: **false, and the press is the phone's own**, carried out as it always was; true, and it was asked of the other device and nothing is done here. |
| `choose` `{"place","in_a_jam"}`, `let_go`, `pause` `{"place"}`, `reset`, `setting` `{"on"}`, `named` `{"name"}` | The list of devices, and its settings. |
| `looking_up`, `looked_up` `{"ticket"}` | Around a look-up for something to play. `looking_up` is answered with `{"act":"ticket","ticket":n}`; the phone is busy to other devices until `looked_up` with that ticket, or a minute, whichever is first, and no more than sixteen are counted at once. No ticket comes back while linking is off, and then there is nothing to end. |
| `stop` | **The sign-in has ended**: a sign-out, a change of account, the service going. |

**What the phone's screens are handed** (built in the adapter; nothing draws a list
yet). The list the library says after each tick is kept where a screen can collect it,
with what the list calls (pick, pause, "Play here", stop steering, reset); a row for a
device of another generation (5.10) carries its sentence and is not to be picked. The
device being steered is put in the same place a jam's host is, which the phone's player,
Now Playing and queue already draw another player from: so they show it, and what is
pressed in them is told as `press`, `add` and `play`. Two rules of the adapter's own:

- **One thing is in the phone's player at a time.** If a friend's jam is there when a
  view of the device arrives, the jam stays and the library is told `let_go`.
- **The device is out of the phone's player before the phone is asked to take its
  queue.** The phone's service starts nothing of its own while its player shows
  another's; the library has let go of the device by the time it calls `take`, and
  says so only in what follows the call. So the adapter stops showing the device as
  `take` is called, not when it is told. (While the phone steers a device it carries
  out no command that would start its own music: a third device that gives it a queue
  is told it could not play it. That is the service's rule, from before linking.)

**`changed` is never said from a Media3 listener for something a command did.**
ExoPlayer tells its listeners of some things later (`onEvents`, posted to the looper),
and `ForwardingSimpleBasePlayer` brings its state up to date from that; a `changed`
said from one of those, after a command from another device has returned, turns one
remote Next into two choices: the library has recorded the command's nonce, and the
posted `changed` moves the order of choices again, forgets which command made the
state, and on a queue being taken makes the wait for its sound read as "something else
was chosen", leaving both devices playing. The library drops a change it hears
**inside** its own call into the player; it cannot know that one posted afterwards was
its own. So the adapter goes by **what the player holds, not by what it is told**:

- It keeps **what the library last knew of the player**: the entry that is current as
  ExoPlayer has it (`exo.currentMediaItem`, which a seek or a Next sets at once, not
  the queue's cursor, which follows later), `exo.playWhenReady`, **whether playing
  is under way** (set to play, with a song made ready or being made ready: so playback
  that ends, by an error or by reaching the end, is a change; **the sound being held
  back for a call is not**, though `brief().playing` says the phone is not playing
  while it is: counted as a change, a call arriving refused the stop that ends a move
  of music, and both devices played when it ended, which the walk found), the queue's `revision` (which moves with every edit of the list and with
  nothing else), repeat, the queue's number, the newest place in `Choices`, and **how
  many times the current entry has begun again by itself** (a song on repeat:
  `onMediaItemTransition` with `REASON_REPEAT`, counted and never told from). Every one
  of those is set before the call that changes it returns, and none is moved later by
  a command's echoes.
- **Before every call into the library** (a tick, a turn) it compares; if any differs,
  it says `changed` with `who: nobody` first, and the order of choices has moved
  before any command in that call is judged. **After every call into the library** it
  notes them again: what the library's own commands did is then known, and their
  later echoes change none of them.
- **The person's own press and what it did are one change.** The press is **noted**
  where it is made (`claimQueue`, the one place the person's ways in pass), and said to
  the library **once**, as `changed` with `who: here`, at the next comparison: which is
  before anything else is asked of the library, and at the latest as soon as the
  press's own work on the player's thread is done (the adapter posts one comparison for
  it). What that comparison finds changed is the press's doing and is not said again
  as a change nobody chose. (A seek, or Play and Pause inside a quarter of a second,
  changes none of the things above: the press is said all the same.) **A press heard
  from inside a call into the library is the library's own act coming round** (a
  command from another device being carried out through the same ways in) and is
  nothing.

The phone's own rule for whose the queue is stays where it is: `claimQueue` and `save`
do what they did. The library's `claim` is for a command another device sent, and
does the same thing by the same function; a `claim` that arrives while the adapter is
itself saying `changed` for a press is that press's own and is not done twice.
**`save` does not clear a queue marked `HANDED`** for a change nobody chose.

**`linkTurn({"link","now","today"})`**: when woken, and **again whenever an answer says
`more`**. It works through what the library kept from the calls before and then what
the requests have brought, a bounded number of things, and has no loop of its own; the
adapter has none either. It posts the next one; it does not call it from inside.

**`linkLibrary({"link","sign_in","ids"})`**, from whichever thread read the phone's
copy of the library: the ids of every song in it, with **the number of the sign-in
that was current when the reading began**. **Called only once the adapter's own index
of that copy is in place** (what `carryOut` finds a song by), so that the library
never counts as held a song the player cannot yet find. For the moment between, the
library goes by the copy before: the only song still counted that the new index lacks
is one that has left the person's library, which cannot be played either way. The number is in every answer (`sign_in`), and is
the next sign-in's from the moment `stop` is said, whatever that was answered with. A
copy read for a sign-in that is over is dropped by the library.

#### What the library tells the adapter

Every `linkHappened` and `linkTurn` answers
`{"acts":[…],"asks":[…],"more":…,"sign_in":…,"refused":…}`. **Every field is the
library's, and the adapter has no value of its own for one that is missing**: an
answer it cannot read whole is a fault. `refused` is true where the call was made
from inside one of the library's own calls with something that needs an answer, or
with something too large: nothing was done, and it is noted.

**`acts`**, for the app: `{"act":"do","ask":{"say":text}}` (a toast);
`{"ask":{"remember":[…]}}` (the devices seen: keep this list, and give it back to
`linkNew` at the next start); `{"ask":{"forget":{"playlist":id}}}` (a page of that
playlist kept on disk holds the secret: a PC deletes it; **a phone notes it and
deletes nothing yet**, see 5.1); `{"ask":{"steering":name|null}}`
(the phone's player is another device's, or its own again); `{"act":"shown","shown":…}`
(the list, after a tick); `{"act":"view","view":…}` (the device being steered);
`{"act":"handled",…}`, `{"act":"ticket",…}`; and **`{"act":"fault"}`**.

**`asks`**, each a request the adapter makes off the player's thread, under an
**operation number** of its own (as every request the library makes for the app has),
and each given the number of the linking it is for (`link`). **They run on six threads
kept for linking and nothing else, with room for thirty-two more to wait**; one that
finds no room is not begun, and the diagnostics say so (the library gives up on what
it was for in its own time). At rest one is in use, the stay at the relay; sends are
made by one worker at a time for each session, which is given back when nothing is
waiting.

| Ask | The adapter | Ended by |
|---|---|---|
| `{"look":{"number","used","reset"}}` | `linkLook` with the music server's sign-in, `since` (now) and the ask's fields. One at a time. | `"stop_look"`: end that operation |
| `{"begin":{"session","room","since_ms"}}` | Read the relay's address and host key (off the player's thread), **as one thing: the pair one save put there, never an address from one save with a key from another** (the phone keeps them sealed together, `link-relay.sealed`, apart from jams' own), then `linkStay` with them: it runs for as long as the session lasts. **A save of the pair ends the session**: at the next tick the adapter says linking is not wanted, which ends everything asked for under it, and at the one after that it is wanted again and begins with what is saved now. Note `since_ms` for the session. **A relay whose address is the music server's, Lidarr's or Lidarr's sign-in's is not used**: its key would go to whatever else is there; the diagnostics say so, and no session begins. | `{"end":{"session"}}` |
| `{"renew":{"session","since_ms"}}` | Note the new `since_ms` for the session. | |
| `{"end":{"session"}}` | End the stay's operation, and **send nothing more that was queued under that session**. | |
| `{"send":{"session","token","to","frame"}}` | (`to` is the place as text, as it is handed back.) `linkSend`, in the order asked, with the session's `since` and `now`: it sends nothing if the session's secret has run out of its ten minutes by the time its turn comes. Sent once; whatever comes of it, it is not sent again. | the session's end |
| `{"find":{"ticket","ids","until_ms"}}` | `linkFind` with the ids and `until` (it gives each request what is then left, and asks nothing once the moment has passed). **Its answer is the songs**: keep them where `carryOut` will find them, and **then** call `linkFound({"operation","ticket","ids"})` with the ids kept. Until `linkFound`, the library has heard nothing. | the sign-in's end |
| `{"log":text}` | The diagnostics log. Never a secret, a key or a token. | |
| `{"later":{…}}` | For a test only. | |

**Before the first of these requests** the adapter sees that the library has been told
which object makes its requests (the phone's `Core` does that the first time anything
of its asks for the network; linking may be the first). **And nothing the library says
can bring the app down**: an act or an ask the adapter cannot read is a fault, like
anything else that fails on the way in or out (below).

**The two lines.** The library's requests for linking go out on two lines of their
own: "music", to the music server (the person's sign-in goes with every request), and
"relay" (the host key and a place's token go with every request). **Each reaches one
place, at every step**: the music server's own address for the first and the relay's
for the second, as scheme, host and port; a request on either for anywhere else is not
sent; **neither is ever sent on** (an answer that says "go there instead" is the
answer, and the look or the stay fails and says so); and the relay's place is never
the music server's. The relay's line has a place only while a session has a stay
there.

**Ending an operation is how anything here is cancelled, and a stop always stops.** A
look, a stay and a look-up each stop at their next request or wait once their
operation is ended; what they had brought and not yet handed over is let go. A look
tells the transport to "keep going" before its first write, which for a request made
for the app's screens means a later stop does nothing; **for one of linking's it means
only that the write in flight is let finish**: nothing more is sent after it, and its
waits end. At `stop`, and at a fault, the adapter ends every one of them.

**Every asker gets an end.** A look, a session's stay or a look-up of songs that could
not even be begun (no room in linking's threads, the relay's key unreadable, a relay
that is refused, a failure of its own on the way) is told to the library as that
request having failed, at once: `linkLookFailed` (a look that could not ask the
server), `linkStayFailed` (said under the list: this device could not begin to keep
its place), `linkFindFailed` (it found none). Nothing is left waiting for a request
that was never made. A session's sender clears its own flag on every way out, so one
that failed does not leave the session unable to send.

**Nothing started for a sign-in outlives it.** Everything the adapter starts (a look,
the reading of the relay's key, a stay, a session's sends, a look-up of songs) is
started under a number that moves on whenever all of it is ended (`stop`, a fault,
the service going). What one of them brings back to the player's thread is used only
if that number is still what it began under; and for a session, only if it is still
the reading that session is waiting for (an `end` for it, or another `begin`, and it
is not). Songs a look-up found are kept, and the library told, only under the number
they were asked under. The library's own fence (the sign-in's number, in every
answer) stands behind all of it.

#### The end of a sign-in, and a fault

**`stop` is said before the player is switched**, at a sign-out or a change of account,
before another account's queue goes in. The library applies it at once, even from
inside one of its own calls, asks nothing more of the player for the old sign-in, and
drops everything the call in flight had come to. The adapter then ends every operation
and lets go of what it held for the old sign-in (songs found, sessions' times). A tick
that names another account ends the sign-in too, if `stop` was never said: that is a
net, not the way.

**A fault: linking has stopped on this phone, until the app is next started.** A call
into the player failed, or something inside the library did (`{"act":"fault"}`: the
library has replaced everything it held and will ask nothing more of the player); or
something failed on the adapter's own side of the boundary. **The fault path has no
outside**: every call into the library, the reading of the player that goes before and
after each (the comparison above), the reading of each act and ask that comes back,
and the start, are inside one catch, and whatever is thrown there is a fault. The
adapter then:

- **calls nothing more**: not the library, not the player, at no tick, press or
  sign-out, for as long as the process lives. A playback service made again in the
  same process does not start linking (the adapter keeps one word for the process:
  that it has faulted);
- lets go of everything, by a clean-up **no step of which can stop another, and which
  may be run twice**: its ticks and turns, every operation, its listeners on the
  player, the two lines, and the library's linking (`linkClose`);
- **takes back a `HANDED` mark if the phone is the one playing** (to what the queue
  was, if the fault came in the very call that marked it; to the phone's own
  otherwise): the library can no longer ask, and a queue left marked would not be
  saved while it played. This is the one thing only the adapter can put right;
- **tells the person once**, with the library's sentence (asked for before anything
  is started; the phone has the same words of its own for when the library's cannot
  be had): "Linking has
  stopped on this phone until the app is next opened."

A fault in the middle of a move of music can leave both devices playing. It does not
leave neither.

#### Bounds, and who is told at them

- **What the requests bring** waits in the library, 256 things and a megabyte, each
  counted at what it holds. **Nothing is dropped there**: a look's answer and found
  songs are kept by their own thread and offered again each second until taken or
  their operation ends; frames from the relay that were not taken are not counted as
  handed over, and the relay is asked for them again. Nobody need be told.
- **What the player's listeners tell from inside a call** is kept by the library, 256
  things and a megabyte. Past that a `changed` is refused (the call throws): the
  adapter says nothing from listeners, so this is not reached.
- **A turn** hands over 64 things beyond what was waiting; `more` says there are more.
- **One copy of the library** waits, the newest sign-in's.
- **An `add` from another device** is refused when it would leave more than 10,000
  songs to come. What the person adds on the phone has the phone's own bound.

| Crate | Pin | Checked (manifests, repositories, RustSec) |
|---|---|---|
| `chacha20poly1305` | `=0.10.1` | RustCrypto. Has `XChaCha20Poly1305`. Released 2022 and widely used; no advisory against it. NCC Group reviewed an earlier release (0.3, 2020), not this one. 0.11.0 came out in June 2026; the older line is chosen for its years of use. |
| `hkdf` | `=0.13.0` | RustCrypto, March 2026. Goes with the `sha2` 0.11 the app has. |
| `zeroize` | as brought in | The sealing key and the secret are wiped when replaced. |

**The secret is not stored on any device,** so there is no key to protect in
Credential Manager or the Keystore. It is in the app's memory, in Rust, while linking
is on, on a PC and on a phone alike.

**Built for Android** (arm64 and x86_64, API 31, the phone app's own native crate and
build, with the layer of 8.1, `native/src/link.rs`, which reaches all of the above and
calls up into one Kotlin object for the player): it builds as it is, with nothing
changed for it. With linking in, the phone's library is 1,625,144 bytes on arm64 and
1,983,808 on x86_64, built for size as that library is. Linking's share of that, when
it was last measured against the same library without it (the round before this one),
was about 338 KiB on arm64 and 421 KiB on x86_64; this round's layer is some 12 and
19 KB more than that round's. Not checked: iOS.

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
| Review 2's sequence 14: a held Pause; meanwhile Play pressed at B on a list still being looked up | `busy` while the list is looked up, `changed` after: the Play being carried out moved the generation. The Play goes ahead. |
| A look during that look-up; a second device presses Pause | The state said busy, so the sender does not send against it and looks again. One that sent anyway is refused `busy`. |
| Two things chosen at B, one after the other, the first still being looked up (Play then Next; Play A then Play B; a guest's add) | Each does exactly what it does without linking. The rule decides nothing between them; B is busy to other devices until both look-ups are over. |
| A look-up that never answers | B stops counting as busy for it after a minute. |
| B carries a Pause out, is started again by hand, and refuses a repeat; the refusal reaches A before the first answer | A says "nothing was done", and corrects it to "Paused" when the success arrives. |
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
| Moving: the other cannot play; the answer is lost, held, or late; the source is touched first; the other is touched before it sounds; its sound never starts | As in the table of 5.7. |
| Three presses while a command is unanswered | They wait, in order. Each goes after the `ok` before it, against the state in that answer. A refusal lets go of the rest. |
| A slider dragged while a command is unanswered | Only its last place waits. |
| A queue is taken over and has not begun to sound; the sender looks | `by` names its command and the state says `busy`: not done yet. It asks again in twelve seconds. |
| An Add is accepted and its songs are still being looked up; the sender looks | The same: `busy`, and `by` names its command. Accepted, not done yet; it is not said to have not arrived. If the songs are never found the device gives the command up at its half minute and answers `late`. |
| `moved` is held on the way past the taker's first save | Until it arrives both sound and both save. When it arrives the source's queue is handed on before its sound stops: the stop is not saved, and the source follows the taker's queue. |
| News delivered again an hour later | A look is sent. The answer to it is what is shown. |
| A command lands as the sound rolls into the next song, before the window has caught up | `changed`: the window is brought up to the sound first. |
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
still looking its songs up, presses that wait, work under way, the three sides of a
move) are tests of `core/`: of each part by itself, and of two whole devices with a
path between them that holds, repeats, hands back, throws away and delays what it
carries (`core/src/link/device/tests.rs`). That the app is wired to it rightly is what
the walks beside the kit show, with two copies of the app and a real relay.

## 10. Moving from today's scheme

A new version never works out the password's hashes, never sends or accepts the owner
word, and opens no private session.

**Before the route is switched on at a relay behind Cloudflare,** the proxy host needs
`real_ip_header CF-Connecting-IP;` in its Advanced box. Without it Nginx Proxy Manager
believes a caller's own `X-Real-IP`, and the relay's limits by address count a
Cloudflare address or whatever the caller claimed. That holds for the jams' port
today, not only for the new route.

1. **The relay,** with the private route and old linking still on.
2. **The Windows app,** then **the phone,** each linking only where the relay says
   `"link": 4`.
3. **The owner's steps** of section 2, in that order: old linking off on every
   device, the password, then "Reset linking". An old PC goes on sending the
   password's fingerprint in an address, its name and what it plays to the old room,
   and keeping a session anyone with the logs can join and watch, until it is updated
   or its linking is switched off; the relay cannot stop any of that.

## 11. The slices

**The first** (built, and released switched off in 0.40.8):

1. **The relay's route** (5.3) with its limits and bounds. Jams untouched; old
   linking still works and can be refused.
2. **`core::link`:** the secret's record and the look; the derivations; the frame;
   the two messages; the rule as a small state machine (reserve, busy, carry out,
   refuse, the minute, `by`), with the byte vectors and every sequence of section 9.
3. **Two Windows copies,** behind a switch with no setting: each shows what the other
   is playing, and Pause on one pauses the other, with the generation reserved at
   every way into the player.

**The second** (built): everything on Windows that the old linking does between two
PCs. Every command of 5.5, each carried out as the same press would be; every way
into the player counted (5.6.1); news; moving music both ways (5.7); steering, with
the other device's player and queue in the window; "Reset linking" under Settings,
Jam; the list as section 2 describes it. Still behind the switch.

Each was seen working in two headless copies against a stand-in for a hostile relay
path.

**Then, before the phone:** the deciding moved out of the Windows app into
`core::link` (section 8), where it is tested without an app: every command carried
out, repeated, handed back, after a change, while busy and late; the ways in; every
ending of a move in both directions; reset with a command in flight; every failure of
the look. The walks were run again on the app as it then stood, and read as before.

**And after the third review:** machinery was taken out, not added. The phone's player
is asked on its own thread and answers at once, as the Windows app's is (section 8);
the record is set up by the plain rule of 5.1, with no second name; and a queue that
has been handed to another device is that device's (5.7).

**The last round on the library by itself** gave every act on the player an outcome
(a queue that went in or did not, a stop that stopped or did not, a call that cannot
say), made "done" something a device says and a sender does not infer, bounded what
the phone's binding keeps and works through, and wrote down what the phone's adapter
owes (8.1).

**And after its review: kinds of failure were removed, by rule.** A call into the
player cannot fail, and one that does is a fault that stops linking on that device
until the app is next started (so "not known", and every true-or-false answer that
stood for "it threw", went); the player says which of three things came of a command; nothing is
recorded for the stop that ends a move until the player says it has stopped; the
queue's number moves at one point; the handed mark belongs to a queue's number; the
sign-in has a number, and the fence is looked at after every call; what the requests
bring waits in one bounded place with one drain, in the library; an Add's time runs
from its arrival. Section 8.1 was made complete by itself.

Then: the phone's adapter, on `bind::Phone`, from 8.1. One review covers both.

**Before linking is switched on for anyone** (the list): the review of the library's
changes and the adapter together; **devices of different generations in one room**
(5.10: built in the library, to be reviewed by itself, and then walked between a build
that says how it speaks and one that does not); the walk of the phone against a PC; and **a phone's
volume**. A phone refuses `volume` today and says 1 (weakness 19). The question for
the owner is whether a PC should be able to turn a phone's media volume up and down
at all. Whatever the answer, a device that says it has no volume of its own is to be
**shown without a volume slider**, not with one that springs back: that needs the
device to say so (a field in its full state), which no build does yet.

**A tester's pair** (one PC and one phone of the owner's, before any of the list
above): on Windows, the released app with the environment variable
`NEXUS_MUSIC_LINK=1` set for that one PC; on the phone, the released source built with
`-PlinkExists=true` (`BuildConfig.LINK`, off in every other build) and signed with the
release key, which adds two rows under Settings, Account ("Link my devices", and the
relay's host link, kept sealed as one pair of address and key) and nothing else: the
phone has no list of devices and no way to steer or ask for the music yet, so
everything is done from the PC. The relay is the owner's, with its devices' route
served. What the tester is told to expect is the reviews' list: the phone is in the
room only while the app is open or playing; music cannot be taken from the phone
while a call holds it; "Play here" replaces the taker's queue; a slow disk or a very
short first song can cost a hand-over; the record's playlist shows in a few lists and
on the other PCs and is to be left alone; after a fault the phone says so once and
linking is off until the app is closed and opened.

## 12. Where it is weakest

1. **The secret is in plain sight:** every app signed in to the account shows the
   playlist's description, Navidrome's page does, and its database and backups hold
   it. I see no better place on a Subsonic server.
2. **Signing in once is joining,** and reset is everyone's, a stolen device's too.
3. **One nonce reused breaks the seal for everyone;** recorded traffic is readable
   later by whoever gets the secret.
4. **The generation is only as good as the app's count of changes.** A way into the
   player that does not count is a hole. Every one has to be found and tested, on both
   apps. Windows' are listed in 5.6.1. On the phone the count is told across a
   boundary (`Event::Changed`), so a way in that is not told is the same hole. And on
   both apps there is the moment between reading what is sounding and changing the
   player, in which the sound can roll into the next song: accepted.
5. **After a lost answer, if anyone chose since, the outcome is not known.**
6. **A busy device starves steering from elsewhere.**
7. **The minute and the ten minutes need a clock that counts sleep.**
8. **`moved` needs no proof of a take.** Any of the person's own devices may tell
   another its music has moved, as any may pause another: the device told stops, and
   treats its queue as handed on (5.7). It is one more thing a device signed in to the
   account can do, not a way in for anyone else.
9. **A command that changed nothing may be sent again**, and again changes nothing: it
   made no state, so the state it was made against still stands.
10. **Setting the record up** (5.1): a playlist of exactly the reserved name that the
    person made and left empty and undescribed is used; and two devices set up at the
    same moment leave a second playlist of the name behind for good. So does one
    device whose sign-in ends while it is writing: the write under way is let finish
    (it cannot be known not to have reached the server), nothing more is asked for
    that sign-in, and the next writes its own. Walked on a phone: an empty playlist of
    the name was left beside the one that holds the secret, and both devices joined
    through that one.
11. **A fault in the middle of a move of music can leave both devices playing**
    (5.6, 8): the phone that faults stops linking and answers nothing more, so the
    other device is never told what became of its command, and says so. It does not
    leave neither playing. Linking then stays off on that phone until the app is next
    started, which the person is told once, as it happens. **What that costs:** a
    person who signs out and in again after a fault still has no linking, until the
    app has been closed and started again; and **on a phone "the app is next opened"
    means its process starting again**: closing the screen and opening it while the
    player is still alive in the background is not that. The sentence says "opened",
    which is what a person does; it does not say "sign in again", which would not
    help.
12. **A person editing the linking playlist's description in the moment between the
    app's read and its write loses the edit**, or finds the playlist made private: the
    music server has no update that is conditional on what was read.
13. **A save already on its way when a queue is handed on can still land after the
    taker's first save.** It is put right by the taker's next save, seconds later.
    **A source whose taker never saves** (its sharing of the queue is off) goes on
    following, with a queue it does not save, until the person uses it there.
14. **Putting a queue entry down in the same place with the other flag** (into "Next
    in queue" or out of it) counts as a change, as any move does.
15. **A queue's place in its song is clamped** to the song's stated length where that
    is known, and to a day where it is not: a length the server has wrong, or rounded
    down, can move a resume point back to it.
16. **The whole-document check of an XML answer is not a strict parser**: it takes some
    malformed XML one would refuse (the same attribute twice, a reference it does not
    know). No XML library is added for that.
17. **The longest queue one message carries costs the phone's main thread several
    milliseconds** in the library alone (7 ms on a desktop), once, when someone sends
    one. **And with the phone's own side, far more than a frame.** Measured on the
    emulator (a debug build, 5,000 songs in the phone's queue, the whole of one call
    into the library on the player's thread): a full look 7 ms, a seek 8, a queue
    handed on 17, Next 25, Remove 29, Move 33 to 41, Play 89, a queue of 4,997 taken
    from another device 119. Nothing has been changed for it yet; what is proposed is
    in the round's report (the entries to come asked for only by the commands that
    name one, and handed over as one text; a queue from another device built off the
    player's thread and put in as a second step). Frames are opened on the player's thread all the same: opening them elsewhere
    would put the session's keys on two threads.
18. **The audit of borrows on Windows** (`scripts/borrow-audit.py`, run by the check)
    does not look for three shapes of the fault it exists for: a borrow held while a
    closure made in the same function is called there and then; a borrow handed back
    by a helper; and a cell borrowed again directly inside the statement that holds
    it. None was found by reading. They are said at the top of the script.

19. **A phone's volume is not steered.** A phone has no volume of its own apart from
    the phone's buttons, so it refuses `volume` and says 1: a PC that steers a phone
    cannot turn it down.
20. **What the phone's adapter finds by comparing, and what it does not** (8.1).
    Nothing is told from the player's listeners, which is what keeps a command from
    another device from being counted twice. A press and what it did are one choice
    where the press's work is done on the player's thread at once; **a press whose
    effect arrives later** (a start that must look songs up first, a list chosen in
    the car) is two: the press, and the change found when it lands. Found at the next
    quarter-second and not before: the queue running on to another entry, an edit, the
    sound taken away for good, a sleep timer, playback ending by an error or at the
    end of the queue, the sound held back for a call and let go again, a song on repeat
    beginning again. **Not found at all:** a song that stalls while loading, a change of
    stream or of downloaded copy for the same entry, and two changes that cancel
    inside one quarter-second. Repeat and volume set from the car or a watch do not
    pass `claimQueue` and are found (repeat) or not (volume) by comparing alone. And
    `claimQueue` counts some presses before the player refuses them: such a press is a
    choice that changed nothing.
21. **An Add that was given up, whose refusal is lost,** can be said by its sender as
    "hadn't reached": the device it was sent to is by then idle at the generation the
    command was made against.
22. **A move of music to a phone is proved by the sound of one entry.** If the song it
    begins with is a second long or less, or ends before its sound has gone on for
    long enough, the proof never comes: the library gives up after twenty seconds,
    pauses the phone and says it did not start, and the device the music was on goes
    on playing. The same if the phone was handed a queue in which only a song on
    repeat-one would have proved it after its first time round.
23. **The mark of a queue handed on waits for the disk on the player's thread**, for
    up to 250 ms on a phone and five seconds on a PC, once for each hand-over. **If
    the disk does not take it in that time the hand-over is refused**: the source
    plays on, the device that took the music is told it could not be stopped, and
    both play until the person stops one. A slow disk therefore costs a hand-over,
    not a queue.
24. **A phone that may not start playing refuses a queue outright.** It is asked
    before anything is put in, so a queue sent to a phone that Android will not let
    start (or whose player is showing another device) is not there to be started by
    hand afterwards.

25. **Moving music proves that the taker sounded, once.** If the taker then stops by
    itself (an error, the end of a very short queue) before the word that it has
    started reaches the source, the source stops too and both are silent: the person
    presses play. And a queue taken onto a device replaces that device's queue before
    the proof: if the proof fails, the source keeps playing and the taker keeps the
    new queue, paused.
26. **On a phone a press and what it did can still be two choices** when Media3 puts
    the command behind other work of its own (weakness 20 says when they are one).
27. **While a phone steers another device it will play nothing of its own for anyone.**
    The phone's service starts none of its own music while its player shows another's
    (its rule from jams, and not linking's to change). Seen from linking: a third
    device that gives the phone its music, or presses Play for it, while the phone is
    steering, is told the phone could not. The person stops steering first.
28. **A look-up before something plays does not make a phone busy.** The library has
    the tickets for it and the adapter hands them on (`looking_up`, `looked_up`), but
    nothing on the phone calls them: the look-ups that run before a start (a voice
    request, a list a car picks) are the playback service's own, where a start that is
    being looked up is already ordered against what other devices ask by the service's
    order of choices. What is lost is only that another device is told "busy" a moment
    early; it is told "changed" instead, once the start lands.
29. **A sentence said on a phone with none of the app's screens alive is not seen.**
    Linking speaks through the app's own notes, which wait for the person while the
    app is behind and are shown by its screens. A phone whose service is alive with no
    screen made (played from a car, or steered from a PC, with the app closed) says the
    sentence to nobody. It is still in the list's note when the list is next drawn, and
    a phone on which linking has stopped says so there.

**For everyone, not for a tester's pair** (added to the list above): at set-up, an
empty playlist of the reserved name with an empty comment, of the person's own, found
beside one that holds the record, is removed (weakness 10 leaves it there today).
**The record's playlist in every copy** (5.1) is ruled on: it is treated as the sync
playlist is, by name, and `linking-playlist-everywhere.md` lists the lines that change.
Two things about set-up that the review of the earlier design found: setting up takes an
existing empty playlist of the name for its own **even when it is public**, which it
should not; and a set-up that is interrupted leaves a blank row, a playlist of the name
with nothing in it. And **taking a long queue from another device**: on an emulator's
debug build it held a phone's player for about 80 ms with 5,000 songs; building the
queue away from the player's thread would change what the player owes the library, so
it is to be measured on a real phone with a real large queue before anything is done.

The relay's path can still hold a command up to two minutes, show a stale state until
the next look, and stop linking altogether.
