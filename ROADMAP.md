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

## 2. Streaming that survives a bad connection

- [ ] A stalled stream doesn't freeze the player: Next, Stop and seeking act at once.
- [ ] A stream that has gone quiet is reconnected and carries on from the same place, instead
      of skipping to the next song.
- [ ] A lost connection is noticed while music plays, not only when a page is opened, and
      the player says it is reconnecting.
- [ ] The audio buffer is shared without a lock, and running dry fades out instead of
      clicking.
- [ ] If the sound device can't be opened the player pauses and says why, instead of moving
      through the queue in silence and counting plays. A device plugged in mid-song is used.
- [ ] Internet radio rejoins a station that dropped.
- [ ] A converted song that was cut short isn't kept in the cache.

## 3. Smooth with a big queue and a big library

- [ ] The queue is saved as song ids and a position, away from the UI thread, and the
      position is saved by itself.
- [ ] The play log is added to, not rewritten, on each play.
- [ ] The window opens with the saved theme at once; a newer synced theme is applied when it
      arrives.
- [ ] Download progress updates only the rows that changed.
- [ ] Search while typing waits for a pause, drops searches that are out of date, and doesn't
      share threads with network calls. Lidarr requests get a thread of their own.
- [ ] The queue panel and "Up next" build only the rows on show.
- [ ] Offline, the app knows which songs it has without asking the disk for every row.
- [ ] Cached pages are read away from the UI thread.

## 4. Keyboard

- [ ] Arrow keys, Enter to play, Delete to remove and Shift with the arrows to select, in
      song lists and the queue.
- [ ] Left and Right to seek, M to mute.
- [ ] Menus work from the keyboard, and a long "Add to playlist" menu scrolls.
- [ ] Accessibility labels on the controls that lack them.

## 5. Playlists you can trust

- [ ] Removing or moving a song checks the playlist against the server first, so a change
      made on another device isn't undone and the wrong song is never removed.
- [ ] Undo for the last removal or move.
- [ ] Songs from a requested album reach the imported playlist that was waiting for them even
      if the first attempt fails.
- [ ] Save the queue, or a "Made for you" mix, as a playlist.
- [ ] Export a playlist as CSV.

## 6. Downloads

- [ ] Cancelling a download and starting it again works.
- [ ] Downloads that failed are listed with the reason and a "Retry failed" button.
- [ ] The format shown for a downloaded song is the format of the downloaded copy.
- [ ] A playback error doesn't delete the downloaded copy.
- [ ] A choice of folder for downloads and the song cache, with a check for free space.
- [ ] Downloads and held changes are kept per account.

## 7. Audio

- [ ] A volume setting for songs that have no ReplayGain tags, so they don't jump out when
      levelling is on.
- [ ] No crossfade between consecutive tracks of the same album.
- [ ] Exclusive mode reopens the output when the bit depth changes, not only the sample rate.
- [ ] An equaliser preamp, and presets you can save.
- [ ] No extra silence when a resampled song ends.
- [ ] Lyrics honour the offset a `.lrc` file can carry, and lyrics that aren't timed say so.

## 8. Finding music

- [ ] A Songs page: every song, sortable, with "Recently added".
- [ ] Every artist credited on a song links to their page.
- [ ] Mixes from your own rules, such as "rated four or more, not played this year".
- [ ] "Continue here": pick up on this PC what another one was playing.
- [ ] Stats for the last four weeks and six months that cover every PC.
- [ ] A details view for a song: format, bit depth, sample rate, levelling, play count.
- [ ] Search that is less forgiving on short words.

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
