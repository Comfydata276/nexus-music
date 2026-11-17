<img src="assets/app-icon.png" width="64" height="64" alt="">

# Nexus Music

A native Windows music player for [Navidrome](https://www.navidrome.org/). It streams from
your own server and is laid out the way Spotify is: library on the left, what you're
browsing in the middle, the player along the bottom.

It is written in Rust with the [Slint](https://slint.dev) UI toolkit. There is no browser
engine inside it: the interface is drawn directly on the GPU, and audio is decoded and played
by the app itself.

## What it does

- Signs in to a Navidrome server; the password is kept in Windows Credential Manager.
- Home, search, albums (sortable), artists, genres, playlists and Liked songs.
- Search that forgives typing mistakes, with a preview of the best matches as you type. It runs on a copy of the library kept on your PC, so it's instant; a matching artist comes first, and songs you've liked rank above others.
- Gapless playback or crossfade, seeking, shuffle, repeat and a queue you can edit.
- A queue panel beside the page, with a play button on every row to skip straight to it.
  Drag rows to reorder the queue, or the songs in a playlist.
- Find and sort within a playlist or Liked songs: click a column heading to sort by it, including the date each song was added.
- Shuffle is set per playlist or album and remembered.
- Keeps your devices in step: the queue and your place in it, Recently played, pinned playlists, each list's shuffle and sort, the dates songs were added and the theme follow you to the app on another PC. The queue uses the server's saved-queue feature; the rest is kept in the comment of a playlist named "Nexus Music sync".
- Select several songs with Ctrl or Shift, then drag them onto a playlist or the queue, or
  right-click to act on them all.
- Recently played: the last hundred songs, each playable again by itself.
- Autoplay: when the queue ends it carries on with similar songs, falling back to the same
  genre when Navidrome has no similar-song source set up.
- A six-band equaliser with presets, mono audio, and volume levelling from ReplayGain tags
  at a quiet, normal or loud target.
- Streaming quality from 96 kbps to lossless, switchable mid-song.
- A choice of output device, and exclusive mode: the device runs at each song's own sample
  rate and receives the file's samples unchanged (bit-perfect at full volume with the
  equaliser and levelling off).
- Downloads: save songs, albums and playlists on the PC to play without the server, at a
  quality you choose, with progress shown per song and on the taskbar, and a cancel button.
- Offline mode, and automatic detection when the server can't be reached: downloads keep
  playing, and likes, ratings and plays are sent once the server is back.
- A Now Playing screen with large artwork, the song's format and lyrics or what's next.
- Internet radio stations set up in Navidrome, star ratings, and a sleep timer.
- Plays MP3, FLAC, AAC/M4A, ALAC, Ogg Vorbis, WAV and AIFF directly. Anything else (Opus,
  WMA, APE and so on) is converted to MP3 by Navidrome on the fly.
- Create, rename, pin and delete playlists; add and remove songs, with a warning before
  adding one that's already there; like songs and albums.
- Synced lyrics when Navidrome has them (embedded or `.lrc` files).
- Media keys and the Windows now-playing overlay.
- Reports plays to Navidrome, which keeps play counts current and feeds Last.fm or
  ListenBrainz if you linked them there.
- Remembers the queue and position between runs.
- Keeps playing from the notification area when the window is closed (optional), and
  opening it a second time brings back the copy that's already running.
- Checks GitHub for new versions and installs them on request.
- Its own title bar: navigation, search and the window's buttons in one strip, in the theme's
  colours. Six colour themes and an interface size setting.
- Previous, play and next buttons on the taskbar thumbnail.

Keyboard: Space plays or pauses, Ctrl+Left/Right changes track, Ctrl+Up/Down changes
volume, Ctrl+K jumps to search, Ctrl+, opens Settings, Alt+Left/Right goes back and forward,
F5 reloads the page.

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

## Building

Everything builds inside one Docker image, including the Windows cross-compile, so the host
needs only Docker.

```sh
docker build -f docker/dev.Dockerfile -t nexus-music-dev docker   # once
scripts/package-windows     # Windows .exe and installer, into dist/
scripts/check               # compile for Linux and print diagnostics
scripts/dx cargo test       # unit tests
scripts/release notes.md    # build and publish a GitHub release (bump Cargo.toml and push first)
```

`scripts/dx <command>` runs any command inside the image as your user.

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

Settings shows how long connecting to your server takes compared with a request on an open
connection.

## How the code is organised

| Path | What's there |
| --- | --- |
| `ui/` | The interface, in Slint. `types.slint` is the contract with the Rust side. |
| `src/subsonic.rs` | Client for the Subsonic API that Navidrome serves at `/rest`. |
| `src/audio/` | Playback engine: download buffer, decoding, resampling, output device. |
| `src/controller.rs` | Sign-in, navigation and the page on screen. |
| `src/pages.rs` | Turns server responses into page content (runs on worker threads). |
| `src/playback.rs` | Queue, transport, play reporting, lyrics. |
| `src/actions.rs` | Context menus, likes, playlist edits. |
| `src/covers.rs` | Cover art cache, on disk and in memory. |
| `src/media.rs` | Windows media keys and now-playing overlay. |
| `src/update.rs` | Finds, downloads and verifies new releases. |
| `src/downloads.rs` | Songs saved for offline listening. |
| `src/offline.rs` | Offline mode and changes held until the server is reachable. |
| `src/audio/exclusive.rs` | Exclusive-mode (WASAPI) output on Windows. |
| `src/instance.rs` | One running copy at a time, title bar colour, display scaling (Windows). |
| `src/taskbar.rs` | Buttons on the taskbar thumbnail (Windows). |
| `src/frame.rs` | Replaces Windows' title bar with the app's own top bar. |
| `installer/` | NSIS script for the Windows installer. |

## Licences of bundled work

- Slint is used under its royalty-free desktop licence, which asks for the attribution shown
  in Settings.
- Fonts: Bricolage Grotesque and Hanken Grotesk, SIL Open Font License (`ui/fonts/`).
- Icons: Lucide, ISC License (`ui/icons/`).
