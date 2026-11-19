# Roadmap

What's planned for Nexus Music, in the order it will be done. It comes from a review of
version 0.23.1 by four independent reviewers in November 2026. Each release below takes one
theme from that review. A ticked item has shipped; the version it shipped in is beside the
heading.

## Where 0.23.1 stood

Average of the four reviewers' scores, out of 10. The yardstick was "an app you'd happily use
every day in place of Spotify's desktop client".

| Category | Score |
|---|---|
| Features and completeness | 7.9 |
| UI and UX design | 7.5 |
| Audio playback | 7.3 |
| Performance and resource use | 7.0 |
| Code quality and maintainability | 6.5 |
| Security and privacy | 6.4 |
| Correctness and reliability | 6.0 |
| Testing and release process | 5.4 |
| **Overall** | **6.8** |

The feature set scored highest. What held it back was how the app behaves when something
goes wrong, and how little is checked automatically before a release. The order below
follows from that.

## 1. Reliability (0.24.0)

Things that lose data or interrupt the music.

- [x] A server that answers with an error no longer signs you out and stops the music; only
      a refused sign-in does. Signing in again keeps the queue.
- [x] Likes, ratings and plays made offline are kept until the server has accepted them. A
      proxy's error page counts as "try again", not as sent.
- [x] Changing Lidarr's address to a different host forgets the saved key, so it is never
      sent somewhere it wasn't meant for.
- [x] An update is installed only if it matches the checksum GitHub publishes; with no
      checksum it isn't installed.
- [x] MP3 and Ogg Vorbis songs are no longer trimmed twice where one song joins the next.
- [x] Seeking in a song that is being converted no longer counts the play twice.
- [x] Artwork that failed to load is tried again once the connection is back.
- [x] `scripts/check` fails when the build fails, and `scripts/release` runs the tests
      before it publishes.

## 2. Streaming that survives a bad connection (0.25.0)

- [x] A stalled stream doesn't freeze the player: Next, Stop and seeking act at once.
- [x] A stream that has gone quiet is reconnected and carries on from the same place, instead
      of skipping to the next song.
- [x] A lost connection is noticed while music plays, not only when a page is opened, and
      the player says it is buffering or reconnecting.
- [x] The audio buffer is shared without a lock, and running dry fades out instead of
      clicking.
- [x] If there is no sound device the player pauses and says why, instead of moving through
      the queue in silence and counting plays. One plugged in later is used when play is
      pressed.
- [x] Internet radio rejoins a station that dropped.
- [x] A converted song that was cut short isn't kept in the cache.
- [x] Fetching songs ahead of time gives up on a transfer that has gone quiet, instead of
      waiting on it for the rest of the session.

## 3. Smooth with a big queue and a big library (0.26.0)

- [x] The queue is written to disk only when it changes, by a thread of its own, and where
      playback has got to is saved by itself.
- [x] The play log is added to, not rewritten, on each play.
- [x] The window opens with the saved theme at once; a newer synced theme is applied when it
      arrives.
- [x] Download progress updates only the rows that changed, and the list of downloads is
      written every couple of seconds during a batch, not once per song.
- [x] Search while typing has a thread of its own and skips searches that are out of date.
      Lidarr requests have their own threads too.
- [x] "Up next" on Now Playing builds only the rows it shows. (The queue panel already did.)
- [x] Offline, the app knows which songs it has without asking the disk for every row.
- [x] Large stored pages are read away from the UI thread.

## 4. Keyboard (0.27.0)

- [x] Arrow keys, Page Up and Down, Home and End move through the songs on a page, Enter
      plays, Delete removes from a playlist, and Shift with the arrows selects.
- [x] The same in the queue panel, reached with Tab or F6: Enter plays, Delete removes,
      Alt with the arrows moves a song up or down.
- [x] Left and Right to seek, M to mute.
- [x] The menu key opens the menu for what's selected; menus work with the arrows and Enter,
      and a long "Add to playlist" menu scrolls.
- [x] Links and the "Up next" rows can be named and used by a screen reader.
- [ ] On search results and artist pages the list doesn't yet scroll to follow the keyboard.

## 5. Playlists you can trust (0.28.0)

- [x] Removing or moving a song is carried out on the playlist as the server has it at that
      moment, so a change made on another device isn't undone and the wrong song is never
      removed. Changes to playlists are sent one at a time, in the order they were made.
- [x] Undo for the last removal or move, on the toast or with Ctrl+Z.
- [x] Songs from a requested album reach the imported playlist that was waiting for them even
      if the first attempt fails, or the album turns up before all its songs do.
- [x] Save the queue, or a "Made for you" mix, as a playlist.
- [x] Export a playlist as CSV.

## 6. Downloads (0.29.0)

- [x] Cancelling a download and starting it again works.
- [x] Downloads that failed are listed with the reason and a "Retry failed" button.
- [x] The format shown for a downloaded song is the format of the downloaded copy.
- [x] A playback error doesn't delete the downloaded copy.
- [x] A choice of folder for downloads, which moves them, with a check for free space before
      moving and before downloading.
- [x] Downloads and held changes are kept per account.
- [ ] A choice of folder for the songs kept as they play.

## 7. Audio (0.30.0)

- [x] Songs that have no ReplayGain tags are turned down by a typical amount while levelling
      is on, so they don't jump out. How much is a setting.
- [x] No crossfade between consecutive tracks of the same album.
- [x] Exclusive mode reopens the output when a song has more bits than the output is
      carrying, not only when the sample rate changes.
- [x] An equaliser preamp, with an automatic setting, and presets you can save.
- [x] No extra silence when a resampled song ends.
- [x] Lyrics honour the offset a `.lrc` file can carry, and lyrics that aren't timed say so.

## 8. Finding music (0.31.0, 0.32.0)

- [x] A Songs page: every song, newest first, sortable and filterable. (0.31.0)
- [x] Every artist credited on a song has a "Go to" entry in the song's menu. (0.31.0)
- [x] A details view for a song: format, bit depth, sample rate, levelling, play count. (0.31.0)
- [x] Search that is less forgiving on short words. (0.31.0)
- [x] Mixes from your own rules, such as "rated four or more, not played this year". (0.32.0)
- [x] "Continue here": pick up on this PC what another one was playing. (0.32.0)
- [x] Stats for the last four weeks and six months that cover every PC. (0.32.0)

## Alongside every release

- [ ] Every push is compiled and tested automatically (GitHub Actions).
- [ ] Tests for the audio engine using its recorded output (gapless, seeking, crossfade), for
      the offline queue and for the streaming source.
- [ ] A crash log, and "Copy diagnostics" in Settings.
- [ ] A damaged settings file is recovered from a backup, never reset without saying.
- [ ] Releases are built from the tagged commit with a pinned toolchain and build image.
- [ ] A server address typed without `https://` tries HTTPS first and warns before using
      plain HTTP outside the home network.
- [ ] Updates are signed with the project's own key, so they don't depend on GitHub alone.
- [ ] The controller is split up area by area as each is worked on.

## Suggested, not yet decided

- A contact address in the requests to MusicBrainz and ListenBrainz, which ask for one.
- A licence for this repository.
- Ranking songs that aren't in the library by popularity, which would send what you search
  for to one more service.
- Discord presence, share links for playlists, browsing by folder, a choice of Navidrome
  library, playlist folders, and playing to another device.

## Not planned

These were decided earlier and the review didn't change them.

- A browser engine inside the app.
- Fetching lyrics from the internet in the app: they come from the music server.
- Downloading music in the app: requests go to Lidarr.
- Supporting systems other than Windows.
