<img src="assets/app-icon.png" width="64" height="64" alt="">

# Nexus Music

A native Windows music player for [Navidrome](https://www.navidrome.org/). It streams from
your own server and is laid out the way Spotify is: library on the left, what you're
browsing in the middle, the player along the bottom.

It is written in Rust with the [Slint](https://slint.dev) UI toolkit. There is no browser
engine inside it: the interface is drawn directly on the GPU, and audio is decoded and played
by the app itself.

What's planned next is in the [roadmap](ROADMAP.md).

## What it does

- Signs in to a Navidrome server; the password is kept in Windows Credential Manager. An address typed without `http://` or `https://` is tried over HTTPS; only one that looks like a home-network address (a private IP address, a bare name such as `nas`, or a name ending in `.local`, `.lan` and the like) is tried over plain HTTP as well. To use plain HTTP with any other server, type `http://` in front of the address; the app says when you sign in that it isn't encrypted.
- Home, search, albums (sortable), artists, songs (every song in the library, newest first, sortable and filterable), genres, playlists and Liked songs.
- Search that forgives typing mistakes, with a preview of the best matches as you type. It runs on a copy of the library kept on your PC, so it's instant; a matching artist comes first, and songs you've liked rank above others. A short word is only taken for a mistyping of one that starts with the same letter, so four letters don't match half the library.
- Gapless playback or crossfade, seeking, shuffle, repeat and a queue you can edit. Crossfade leaves an album alone: its tracks run into each other as recorded.
- A queue panel beside the page, with a play button on every row to skip straight to it.
  In a window too narrow for both, it opens over the page instead of squeezing it.
  Drag rows to reorder the queue, or the songs in a playlist.
- "Add to queue" and "Play next" put songs in "Next in queue", which plays straight after the
  current song and ahead of the rest of the album or playlist you were playing; the list
  carries on afterwards. A song that's already further down the list can be queued too, and
  still plays when its turn in the list comes. Drag a song across the heading to move it into
  the queue or back out; "Clear queue" empties just what you queued.
- Find and sort within a playlist or Liked songs: click a column heading to sort by it, including the date each song was added.
- Shuffle is set per playlist or album and remembered.
- Brings playlists over from Spotify: choose the files you exported (CSV from a tool such as Exportify, or Spotify's own data download) and it matches each song to your library, builds the playlists, and lists what it couldn't find.
- Optional, off by default: if you run Lidarr beside your music server, search results gain a "Not in your library" section. It lists songs (each with the album it's on), artists and albums; an artist opens to all their albums and an album to its track list. Asking for a song or album puts that album on Lidarr's list, "Add artist" has Lidarr fetch everything by them, and a Requests page shows what is on its way. A song that never came out on an album is offered on its single or EP (this can be turned off); otherwise whole albums. Soundtracks can be asked for as well: one credited to an artist is asked of that artist, and a film's soundtrack album goes on Lidarr's list under "Various Artists". Live albums, compilations and remixes aren't offered. Where Lidarr gets them from is set up in Lidarr. Songs, discographies and track lists are looked up at [MusicBrainz](https://musicbrainz.org), which is sent what you search for; covers come from the Cover Art Archive. How much each song is played comes from [ListenBrainz](https://listenbrainz.org), which is told which artists and songs came up: the most played go first, an artist's page opens with what they're most played for, and typing an artist's name lists those songs. If Lidarr sits behind an Authentik sign-in, you sign in with your browser and the app keeps only a token (see below).
- With requests on, Home gains "Artists you might like": artists you don't have that are like the ones you play most, each opening to their page. Your server is asked who is similar (it needs Last.fm or Deezer for that, and Navidrome uses Deezer unless told otherwise), and Lidarr says who each one is.
- A "Made for you" row on Home, built from your listening: On repeat, Your top songs, Rediscover and mixes of the genres you play most. You can add mixes of your own, made to rules you set: a genre, a rating, liked songs only, the years, how long since you played them, how lately they were added. Each has a page that stays up to date and can be saved as a playlist.
- After an import, one button asks Lidarr for everything the playlists were missing, however much that is. Each song's album is found by the name the playlist gives it ("Supernatural (Remastered)" is "Supernatural"), a song listed by its single is asked for as that single, and one that can't be found by name (or whose name Lidarr's search fails on) is found by the song itself, through MusicBrainz. Artists are handed to Lidarr one call each with just the releases wanted, so hundreds are accepted in a minute or two and Lidarr works through them in its own time. The Requests page shows how far it has got and lists anything that couldn't be asked for, with why. A batch is written down as it goes: closing the app, or Lidarr going away, doesn't lose it.
- Follows up what you ask for: when a requested album arrives it says so, lists it under Requests, and puts the songs an imported playlist was missing into that playlist.
- Keeps songs ready: each song is kept on this PC once it has played, and the next few in the queue are fetched ahead of time, so the music carries on through a dropped connection and replays start at once. You choose how much space that may take (2 GB unless changed), which folder it's in, and how far ahead to fetch; the songs played longest ago make way when it fills.
- Rides out a bad connection: a stream that stops arriving is asked for again from where it stopped, usually before the audio in hand runs out. If the server can't be reached at all the song waits where it is, the player says it's reconnecting, and the music carries on from the same place when the server is back. Skipping, stopping and seeking still act at once while a stream is stuck.
- Playing a song from a search starts its radio: that song, then others like it.
- Play on another of your devices: with a relay set up (see Jams, below), every PC running Nexus Music on your account shows up under “Connect to a device” in the player bar, which lists them in the side panel in the queue's place, with what each is playing. Pick one while music is playing and the music moves there, carrying on from the same place in the same queue; pick one while nothing is playing and you steer whatever it has on. Either way the player and queue panel on the PC in front of you then control the other one, volume included, and “Play here” brings the music back.
- Jams: one queue that friends add to while the music plays on your PC. They join from a link, in a browser (a phone works) or in Nexus Music on their own PC under Jam. In the app, a joined jam takes over the player and the queue panel: they show the host's song and queue, and pausing, skipping, dragging songs into a new order or taking them out happens for everyone. “Add to queue” anywhere adds to the jam, and so does playing a song. A strip along the bottom of the window says you're in a jam for as long as you are. The browser page does the same from a phone. You choose whether guests may steer or only add songs, see who's there, and can remove anyone. The queue shows who added what. Joining a jam needs nothing set up; hosting one is off until you switch it on, and needs a small relay service (see below).
- Keeps your devices in step: the queue and your place in it, Recently played, pinned playlists, each list's shuffle and sort, the dates songs were added and the theme follow you to the app on another PC. The queue uses the server's saved-queue feature; the rest is kept in the comment of a playlist named "Nexus Music sync". When another PC has been playing in the last half hour, the queue panel offers "Continue here", which picks its queue up where it had got to. The plays behind "Your stats" are shared too, so today, this week and this month cover every PC. See "Keeping devices in step", below, for where a copy of it is kept and what is known not to be perfect.
- Select several songs with Ctrl or Shift, then drag them onto a playlist or the queue, or
  right-click to act on them all.
- Recently played: the last hundred songs, each playable again by itself.
- Autoplay: when the queue ends it carries on with similar songs, falling back to the same
  genre when Navidrome has no similar-song source set up.
- A six-band equaliser with presets, presets of your own, and a preamp that leaves room for
  bands you've raised (automatically, if you like). Mono audio. Volume levelling from
  ReplayGain tags at a quiet, normal or loud target; songs with no tags are turned down by a
  typical amount so they don't stand out.
- Streaming quality from 96 kbps to lossless, switchable mid-song.
- A choice of output device, and exclusive mode: the device runs at each song's own sample
  rate and receives the file's samples unchanged (bit-perfect at full volume with the
  equaliser and levelling off).
- Downloads: save songs, albums and playlists on the PC to play without the server, at a
  quality you choose, with progress shown per song and on the taskbar, and a cancel button.
  Songs that couldn't be fetched are listed on the Downloads page with the reason and a
  "Retry failed" button. You choose the folder they're kept in (Settings, Storage); changing
  it moves them. A download that wouldn't fit on the drive isn't started. Each account has
  its own list of downloads.
- Offline mode, and automatic detection when the server can't be reached: downloads keep
  playing, and likes, ratings and plays are kept on the PC until the server is back and has
  accepted them. A server that answers with an error is treated the same way; only a
  sign-in the server refuses takes you back to the sign-in screen, and the queue is still
  there after signing in again.
- A Now Playing screen with large artwork, the song's format and lyrics or what's next.
- Internet radio stations set up in Navidrome (a station whose stream drops is rejoined), star ratings, and a sleep timer.
- Plays MP3, FLAC, AAC/M4A, ALAC, Ogg Vorbis, WAV and AIFF directly. Anything else (Opus,
  WMA, APE and so on) is converted to MP3 by Navidrome on the fly.
- Create, rename, pin and delete playlists; add and remove songs, with a warning before
  adding one that's already there; like songs and albums.
- Playlists are safe to edit from more than one place. Removing or moving a song is carried
  out on the playlist as the server has it at that moment, by the song and not by its
  position, so a change made on another device in the meantime is kept and the wrong song is
  never removed. "Undo" (or Ctrl+Z) takes the last removal or move back. A playlist that
  holds songs missing from your library (their files gone, or a disk offline when the server
  last looked) is the exception: the server hides those songs from the app but still counts
  them, so nothing is removed, moved or undone in it, and the app says why. Songs can still
  be added to it.
- Save the queue, or a "Made for you" mix, as a playlist. Export a playlist as a CSV file
  that this app, and other tools, can import again.
- Synced lyrics, from Navidrome: it serves the ones embedded in a song or in a `.lrc` file beside it. The app doesn't look lyrics up anywhere else, so every app that uses your server shows the same ones. It keeps a copy of what the server says, and fetches the lyrics of the next few songs in the queue ahead of time, so they show the instant you open them and are there when you're offline. To fill in a library that has none, put `.lrc` files beside the songs with a tool such as [LRCGET](https://github.com/tranxuanthang/lrcget).
- A mini player: a small window that stays on top of other apps, with the artwork, the song, and buttons to pause, skip and like.
- "Your stats": minutes listened and songs played, with your top songs, artists, albums, genres and playlists, for today, this week, this month or all time. Under "All time" you can narrow it to the last 3, 6 or 12 months, or to a calendar year. "All time" uses your server's play counts, which take in every app, and adds what's been played since the app last read them; the other periods use a log of plays this app keeps on your PC, which takes in the plays made on your other PCs as well. Playlists are counted from version 0.40 on.
- Recommended songs under each playlist: songs from your library like the ones in it, each with a button to add it.
- Media keys and the Windows now-playing overlay.
- Reports plays to Navidrome, which keeps play counts current and feeds Last.fm or
  ListenBrainz if you linked them there.
- Remembers the queue and position between runs.
- Keeps playing from the notification area when the window is closed (optional), and
  opening it a second time brings back the copy that's already running.
- Checks GitHub for new versions and installs them on request, once the download matches the checksum GitHub publishes for it.
- If it ever stops by itself it notes why, and "Save diagnostics" under Settings, About writes a text file about the app and its set-up (no server address or account) to help work out what went wrong. A settings file that gets damaged is recovered from a copy, or put aside; it is never silently replaced with defaults.
- Its own title bar: navigation, search and the window's buttons in one strip, in the theme's
  colours. Six colour themes and an interface size setting.
- Previous, play and next buttons on the taskbar thumbnail.

### Keyboard

| Keys | What they do |
|---|---|
| Space | Play or pause |
| Left, Right | Back or forward five seconds |
| Ctrl+Left, Ctrl+Right | Previous or next track |
| Ctrl+Up, Ctrl+Down | Volume |
| M | Mute |
| Up, Down, Page Up, Page Down, Home, End | Move through the songs on the page |
| Down, in the search box | Leave the box for the songs found |
| Shift with any of those | Select from where you started to where you've moved |
| Ctrl+A | Select every song on the page |
| Ctrl+Z | Undo the last removal from, or move within, a playlist |
| Enter | Play the song you're on |
| Delete | Take the selected songs out of the playlist |
| Menu key, or Shift+F10 | Open the menu for the selected songs; the arrows and Enter work in it |
| Tab, or F6 | Move the arrow keys to the queue panel and back |
| In the queue: Enter, Delete, Alt+Up, Alt+Down | Play that song, remove it, move it up or down |
| Ctrl+K | Search |
| Ctrl+, | Settings |
| Alt+Left, Alt+Right | Back and forward |
| F5 | Reload the page |
| Esc | Close a menu, clear the selection |

### Requests when Lidarr is behind Authentik

If Lidarr's address answers with an [Authentik](https://goauthentik.io) sign-in instead of
Lidarr, pressing **Connect** notices and opens that sign-in in your browser. You sign in on
Authentik's own page, with whatever second step it asks for, and the browser hands the app a
token. No name or password is typed into the app or kept by it. The token is kept in the
Windows credential store, is replaced each time it's used, and is handed back when you sign
out. It lasts for as long as the app is opened now and then within the lifetime Authentik
gives refresh tokens.

Authentik needs three things set up once:

1. An OAuth2/OpenID provider for the app: public client, client ID `nexus-music`, redirect
   URI `http://127\.0\.0\.1:\d+/callback` (regular expression), grant types Authorization
   Code and Refresh Token, scopes `openid`, `profile` and `offline_access`. Give it an
   application, limited to the people who may use Lidarr.
2. On the proxy provider in front of Lidarr, add that provider under Federated OIDC
   Providers. Who may use Lidarr is still decided by the Lidarr application's own bindings.
3. The reverse proxy has to pass the `Authorization` header on to Authentik's outpost for
   Lidarr's host. With that header passed, Authentik also accepts tokens made from a user's
   app password for that host, so only do this where that is acceptable.

### Jams

A jam's music plays on the host's PC. Everyone else steers. What a guest adds goes into
"Next in queue", behind anything already waiting there, so songs play in the order they were
added and ahead of whatever album or playlist the host had on. A song that's already waiting
isn't queued twice, and the first song added to an empty queue starts the music. With "Guests
can steer" on (it is unless the host turns it off), guests can also pause, skip, go back,
reorder the queue and take songs out; with it off they can add songs and take their own back
out.

Guests don't sign in to your music server and are never given its address or your sign-in.
What they search for is looked up by your copy of the app, in its own copy of the library,
and covers are passed on by it. No audio is sent to them.

**Joining.** Click Jam in the sidebar, paste the link the host sent, and press Join. From
then until you leave, the player at the bottom and the queue panel are the jam's: what was
playing on your PC pauses and is still there afterwards. Songs your own library also has are
shown as they are anywhere else in the app; the jam's page has a search box that looks in
the host's library, for when it isn't the same one. Someone without the app opens the same
link in a browser.

**Hosting.** A PC at home can't be reached from a phone on mobile data, so the app and the
guests meet at a relay: a small service you run somewhere that can be reached, such as beside
your music server. The app connects out to it; nothing has to be opened on your PC or your
router for the app itself. Once the relay is running:

1. In Settings, under Jam, switch on "Host jams".
2. Paste the relay's host link and press Connect. The link is the relay's address with a key
   after a `#`; the relay prints it when it starts. The key is kept with your password
   (Windows Credential Manager), not in the settings file, and is only ever sent to that relay.
3. Click Jam in the sidebar and "Start a jam". Send friends the link, or have them scan the code.

A jam ends when you end it, when the app closes, or half an hour after the app loses touch
with the relay.

**Your own devices.** The same relay links your PCs to each other. On each one, switch on
"Host jams" under Settings, Jam, and paste the host link; "Link my devices" is then on unless
you turn it off. Each PC keeps a private session open at the relay and says what it's
playing, and the others list it under “Connect to a device” in the player bar. Only copies of
the app signed in to the same account can find each other: the room they meet in, and the
word one shows another to be given the run of it, are both worked out from the account's
name and password (neither gives the password away, and it is never sent). A private
session does nothing for anyone else who reaches it, and the relay is told what the PC is
playing only while one of your devices is steering it. Starting a jam turns the PC's private
session into the jam; ending it opens a fresh private one, so an old jam link is of no
further use.

**Running the relay.** It is one small program in a container, keeps nothing on disk, and
holds jams in memory only: restarting it ends any jam in progress.

```sh
docker run -d --name nexus-music-jam --restart unless-stopped -p 8080:8080 \
  -e JAM_HOST_KEY="$(openssl rand -hex 24)" \
  -e JAM_PUBLIC_URL=https://jam.example.org \
  ghcr.io/comfydata276/nexus-music-jam:latest
docker logs nexus-music-jam    # prints the host link
```

| Setting | What it's for |
|---|---|
| `JAM_HOST_KEY` | What a copy of the app must show to start a jam: at least 16 characters, and worth making long and random. Without it the relay makes one up each time it starts, so the host link changes on every restart. |
| `JAM_PUBLIC_URL` | The address people reach the relay at. Only used to print the whole host link in the log. |
| `JAM_LISTEN` | Where it listens inside the container; `0.0.0.0:8080` unless set. |
| `JAM_MAX_JAMS` | How many sessions may be open at once; 20 unless set. Each linked device holds one, whether or not it's a jam. |

Put it behind the reverse proxy you already use, on an address of its own (or under a path),
with HTTPS. Three things matter there:

- Nothing may ask for a sign-in in front of it. Guests have no account anywhere; the jam's
  link, which is long and random, is what lets them in. The host key is what stops anyone
  else starting jams.
- It holds requests open for up to 20 seconds on purpose, waiting for something to say. A
  proxy that gives up on a quiet request sooner than that needs its read timeout raised.
- The `Authorization` header must be passed through to it.

The app refuses a host link or jam link that starts with `http://` unless it points at your
own network, since the key travels with every request.

The image is built and published by `.github/workflows/relay.yml` when the relay changes. To
build it yourself: `docker build -f relay/Dockerfile -t nexus-music-jam .` from the top of
this repository.

## Installing

Download `NexusMusic-Setup-<version>.exe` from the
[latest release](https://github.com/Comfydata276/nexus-music/releases/latest) and run it. It installs for the current user without an
administrator prompt. The build isn't code-signed, so Windows SmartScreen will ask before
running it the first time: choose **More info**, then **Run anyway**.

Installed copies update themselves from Settings, About. The portable `.exe` on the same
page needs no installation; it tells you when a new version is out and opens the download page.

The app needs the Microsoft Visual C++ runtime, which most PCs already have. If Windows
reports that `VCRUNTIME140.dll` or `MSVCP140.dll` is missing, install it from
<https://aka.ms/vs/17/release/vc_redist.x64.exe>.

## Keeping devices in step

What devices share (pins, each list's shuffle and sort, the theme, the dates songs were
added, Recently played, the latest plays) is text in the comment of a playlist named "Nexus
Music sync", which the app makes and doesn't list. Each item carries the time it last
changed, and the newer one wins on every device. The rules for it are in the library
(`core/src/step.rs`, with the steps an app follows numbered at its top), so that the Windows
app and the Android app do the same.

**A copy, in case.** Before a version of the app first changes that record for an account, it
keeps the text exactly as it found it, beside its settings (`%APPDATA%\Nexus Music\config`):
`shared-record-kept-<account>-<version>.json` (an empty file, if the record was empty). The
file is made once and never written again, by that version or a later one, and nothing is
sent to the server until it is there. The one with the lowest version in its name is the
record as it was before any of them changed it.

**Putting the record back.** Pasting that text into the playlist's comment is not enough by
itself. Each device remembers what it last agreed with the server, the newer of two values
always wins, and a device's own are newer than an old copy: so the first device to look
writes its own pins, sorts and theme straight back. To go back to the record as it was kept:

1. Quit the app on every device (from the tray, not only the window), and leave it closed.
2. In Navidrome, open the playlist "Nexus Music sync" and paste the file's text in as its
   comment.
3. On each PC, before starting the app again, open `settings.json` in the same folder and
   delete the whole `"choices"` entry (from `"choices": {` to its closing `}`, and the comma
   that goes with it). That is what the PC last agreed and what it still means to send; with
   it gone, the PC takes its pins, sorts, shuffles and theme from the record. To take the
   dates songs were added from the record as well, delete `playlist-added.json`. Leave
   `plays.log` alone: plays are only ever added to, and none of this touches them.
4. Start the app on one PC and look that it shows what the record holds. Then start the
   others, one at a time.

A PC that step 3 was not done on puts back whatever it holds that is newer than the copy.
Recently played and the plays are not gone back on either way: what has been played since
the copy was kept stays, on the PCs and in the record.

**Known, and accepted.**

- Two devices that read the record in the same moment both write, and the second write wins.
  The first still holds what it wrote and puts it back at its next look (a minute at most).
  The server has no write that is conditional on what was read.
- If the answer to a write is lost (the app is closed, the connection drops) and, before this
  device next looks, another device deliberately sets the opposite, this device makes its
  change again on top of that.
- Dates that 0.40.4 and earlier gave songs merely by looking at a playlist are already in the
  record, with nothing to tell them from real ones. They stay.
- Going back to 0.40.4 while changes are still waiting to be shared loses those changes:
  0.40.4 does not know of them.
- A song the server silently leaves out of an "add to playlist" (one the account may not see)
  is still dated as added.
- A play is timed by the PC's clock when it happens. While the clock is more than five
  minutes from the server's nothing is shared, but the times already put on plays are not
  put right afterwards.
- A second playlist named "Nexus Music sync" with a higher id (two devices each made one
  before either had seen the other's) holds data nobody reads: every device uses the one with
  the lowest id. To see it, look for two playlists of that name in Navidrome's own pages; to
  be rid of it, check its comment holds nothing wanted and delete it there.
- A change that is sent, held up on its way for more than ten minutes, and undone here in
  the meantime, is not undone again when it finally arrives. (Up to ten minutes, it is.)
- An edit made in the fraction of a second between the app deciding what to send and sending
  it goes as it was decided, and is put right by the next exchange.
- What arrives from other devices (pins, sorts, the theme, dates, Recently played) is written
  to the disk as the app writes everything else: a moment after it is shown, not before. If
  that write fails it is reported like any other, and the same things arrive again at the
  next exchange. (A change you make yourself is on the disk before it is shown.)
- A profile opened by two copies of the app at once (possible only by pointing both at one
  folder with `NEXUS_MUSIC_HOME`) is not protected: each writes its files as if alone.
- Some things are one set for the PC, not one for each account, as they were in 0.40.4. With
  two accounts signed in on one PC in turn:
  - pins, list choices and the theme that were agreed under one account are offered, at the
    times they were made, to the other (changes still waiting are not: they wait for their
    own account);
  - the dates songs were added (`playlist-added.json`) are one set, so dates noted under one
    account are shared into the other's record for playlists of the same id;
  - the plays (`plays.log`) are one log, so "Your stats" counts whoever listened on this PC,
    and the latest of them are shared into whichever account's record is exchanged;
  - likes, ratings and downloads waiting to be sent are kept per account under a name that
    turns punctuation into `_`, so two usernames on one server that differ only in
    punctuation share them.

  Signing out clears Recently played, the saved queue and the "Continue here" offer.

**Your plays.** `plays.log`, in the same folder, holds every play the app has counted, one to
a line, and is only ever added to. If it can't be read when the app starts (another program
has it open, the disk gave an error) it is left exactly as it is: the app says so once,
"Your stats" counts only what is played from then on and says that nothing is gone, and
those plays go to `plays.log.new` beside it, which is joined to the log the next time the
log reads. A log that is simply not there is started from Recently played.

## Building

Everything builds inside one Docker image, including the Windows cross-compile, so the host
needs only Docker.

```sh
docker build -f docker/dev.Dockerfile -t nexus-music-dev docker   # once
scripts/package-windows     # Windows .exe and installer, into dist/
scripts/check               # compile for Linux and print diagnostics
scripts/dx cargo test --workspace    # unit tests: the app's and the library's (core/)
scripts/release notes.md    # build and publish a GitHub release (bump Cargo.toml and push first)
scripts/dx cargo test --manifest-path relay/Cargo.toml    # the jam relay's tests
```

Every push to GitHub is compiled and tested there as well (`.github/workflows/check.yml`).
The Rust version is fixed in `docker/dev.Dockerfile` and in that workflow, so a release is
built with the same compiler as the one before unless it's raised on purpose.

`scripts/dx <command>` runs any command inside the image as your user.

The repository is a Cargo workspace of two packages: the app, at the top, and the library
it is built on, in `core/` (see "How the code is organised"). The library is compiled into
the app's one `.exe`; it isn't a separate file. `cargo build` and `cargo test` at the top
cover both, and `scripts/dx cargo test -p nexus-core` runs the library's tests alone. The
jam relay in `relay/` is not part of the workspace: it has its own lock file and is built
into its own image.

### Running it on Linux for development

The Linux build is for development and testing only. Slint's embedded
[MCP server](https://docs.slint.dev/latest/docs/slint/guide/tooling/ai-coding-assistants/)
lets a script or coding agent inspect the window, click and take screenshots, with no display:

```sh
scripts/dx bash -c 'SLINT_EMIT_DEBUG_INFO=1 cargo build --features slint/mcp'
DX_DOCKER_ARGS="-e SLINT_MCP_PORT=9315 -e SLINT_BACKEND=headless-skia -e NEXUS_MUSIC_HOME=/work/target/test-home" \
  scripts/dx target/debug/nexus-music
```

- `NEXUS_MUSIC_HOME` puts settings and caches in a folder of your choosing.
- `NEXUS_TRACE=1` prints how long each track took to open and to produce its first audio.
- `NEXUS_AUDIO_DUMP=<file>` (only when no sound card is present) records what would have
  been played as raw 32-bit float stereo at 48 kHz, so output can be checked numerically.

## What keeps it quick

Most waiting in a streaming client is connection setup and round trips, so the app is built
to avoid both:

- One HTTP agent serves API calls, artwork and audio, with connections kept open for minutes
  and warmed up at sign-in. Name lookups are cached, and when a name resolves to several
  addresses the one that answers first is used.
- A page is drawn from a single request; extras such as an artist's biography follow.
  Independent requests run in parallel.
- Clicking something draws its header at once from what the clicked item already shows.
- Every page is cached in memory and on disk. A cached page appears immediately and is
  checked against the server in the background. Resting the pointer on an album, artist or
  playlist fetches its page before the click.
- Each cover is requested from the server at one size; thumbnails are cut from it locally.
- FLAC and MP3 tracks start from a single request.

And the thread that draws the window is kept free of anything slow:

- Files are written by a thread of their own. The queue (which can be thousands of songs) is
  written only when it changes; where playback has got to is a few numbers kept beside it.
- Each play adds one line to the play log; the log is never rewritten.
- Searching the library as you type has its own thread and skips any search that more typing
  has already replaced. Asking Lidarr for something, which can take minutes, has its own too.
- A long playlist's stored copy is read back away from the window's thread.
- One song's rows are found by a lookup, not by going through the list on screen.

Settings shows how long connecting to your server takes compared with a request on an open
connection.

## How the code is organised

| Path | What's there |
| --- | --- |
| `core/` | The library (`nexus-core`): what every app on an account has to do identically, kept apart so the Android app can be built on the same code. No window, files, threads or connections of its own. See below. |
| `ui/` | The interface, in Slint. `types.slint` is the contract with the Rust side. |
| `src/subsonic.rs` | Client for the Subsonic API that Navidrome serves at `/rest`. |
| `src/audio/` | Playback engine: download buffer, decoding, resampling, output device. |
| `src/controller.rs` | Sign-in, navigation and the page on screen. |
| `src/pages.rs` | Turns server responses into page content (runs on worker threads). |
| `src/playback.rs` | Queue, transport, play reporting, lyrics. |
| `src/playlists.rs` | Describes a change to a playlist so it can be applied to the server's copy; CSV export. |
| `src/actions.rs` | Context menus, likes, playlist edits. |
| `src/covers.rs` | Cover art cache, on disk and in memory. |
| `src/media.rs` | Windows media keys and now-playing overlay. |
| `src/lidarr.rs` | Asking a Lidarr server for albums (optional): the connection, and what has been offered. The rules are `core/src/lidarr.rs`. |
| `src/asked.rs` | Remembers what was requested and spots when it arrives. |
| `src/bulk.rs` | Asks Lidarr for many albums at once: finds each, hands them over an artist at a time, and checks what Lidarr made of them. |
| `src/musicbrainz.rs` | Finds songs, discographies and track lists for music that isn't in the library. |
| `src/gate.rs` | Browser sign-in for a Lidarr that sits behind Authentik: the browser's part. The rules are `core/src/gate.rs`. |
| `src/jam.rs` | Jams and linked devices: being steered through a relay, steering someone else's playback, and handing a queue from one device to another. |
| `relay/` | The jam relay: a separate small program (its own `Cargo.toml`) with the page guests without the app use. |
| `src/update.rs` | Finds, downloads and verifies new releases. |
| `src/lyrics.rs` | The server's lyrics, kept so they show at once. |
| `src/plays.rs` | The log of plays behind "Your stats". |
| `src/saver.rs` | Writes files in the background, newest version only. |
| `src/listenbrainz.rs` | How much songs, releases and artists are played. |
| `src/discover.rs` | Artists worth a look that the library doesn't have. |
| `src/songcache.rs` | Songs kept as they're played and fetched ahead of the queue. |
| `src/downloads.rs` | Songs saved for offline listening. |
| `src/offline.rs` | Offline mode and changes held until the server is reachable. |
| `src/audio/exclusive.rs` | Exclusive-mode (WASAPI) output on Windows. |
| `src/instance.rs` | One running copy at a time, title bar colour, display scaling (Windows). |
| `src/taskbar.rs` | Buttons on the taskbar thumbnail (Windows). |
| `src/frame.rs` | Replaces Windows' title bar with the app's own top bar. |
| `installer/` | NSIS script for the Windows installer. |

### The library, `core/`

A rule that lives in `core/` is written once and gives the Windows app and the Android app
the same answer. Each module in `src/` that had such a rule passes it on under its old name
(`search::fold` is `nexus_core::text::fold`), and keeps what is the Windows app's own: the
window, the files, the connections.

| Path | What's there |
| --- | --- |
| `core/src/text.rs` | Comparing names: folding, words, what titles have tacked on. |
| `core/src/search.rs` | How well what was typed matches a name. |
| `core/src/model.rs` | What a server's answers are read into: a song, an album, a playlist. |
| `core/src/sync.rs`, `notes.rs` | The record devices share, and how two copies of it are combined; when a song's place in a playlist gets a date. |
| `core/src/record.rs` | That record as text: the layout number read first, what this version doesn't know carried back out. |
| `core/src/step.rs` | An exchange, decided: changes kept as changes, which playlist holds the record, what to keep, take in and write. |
| `core/src/handover.rs` | The saved queue between devices: whether to save this one's, take up another's, or offer it. |
| `core/src/time.rs` | The times a server sends, and a device's clock set beside the server's. |
| `core/src/playlists.rs` | A change to a playlist, described so it means the same if the playlist has changed. |
| `core/src/address.rs` | What a typed server address means, and when plain HTTP matters. |
| `core/src/jam.rs` | Reading a jam's link; what a jam says of a song; what a device does for whoever asks it something. |
| `core/src/levelling.rs` | When a song counts as playing with its album. |
| `core/src/queue.rs` | What becomes of the queue when the list it was started from changes. |
| `core/src/musicbrainz.rs`, `listenbrainz.rs`, `offer.rs` | The questions put to MusicBrainz and ListenBrainz and what is made of the answers. |
| `core/src/lidarr.rs` | The questions put to Lidarr and what is made of the answers: looking up, asking for an album or an artist, the profiles for singles and soundtracks, what is on its way. `src/lidarr.rs` keeps the Lidarr in use and what has been offered. |
| `core/src/gate.rs` | The sign-in in front of Lidarr: finding it, the link the browser is sent to, trading the code and renewing the token. `src/gate.rs` keeps the browser's part: somewhere on this PC for it to come back to, and opening it. |
| `core/src/http.rs` | How the library asks for a request: it asks the app. `src/corehttp.rs` is the Windows app's answer, on its own connections. |

Something moves into `core/` when it is free of the window, files and threads, and is moved
as it is written, tests with it.

## Licences of bundled work

- Slint is used under its royalty-free desktop licence, which asks for the attribution shown
  in Settings.
- Fonts: Bricolage Grotesque and Hanken Grotesk, SIL Open Font License (`ui/fonts/`).
- Icons: Lucide, ISC License (`ui/icons/`).
