# The files the app keeps

**The rule: a file the app must not lose is written through `config::write_whole` (or
`write_surely`, which is it for a file written again and again), so that a kill or a power
cut leaves the file as it was or as it is now; and a file that is there and can't be read is
never taken for one that isn't, or for an empty one.** Whatever can be fetched again may be
written more cheaply, if a file of it cut short is read as not there, never as content or as
an error; each such reader has a test that shows it. The rows marked *not yet* below are
files whose reader still takes one that can't be read for none.

How each is written:

- **Whole** is `config::write_whole`: written beside its place (the same name ending `.tmp`),
  flushed to the disk, renamed into place, and on Linux its folder synced. A kill or a power
  cut leaves the old file or the new one. Whatever a stopped write left beside it is cleared
  by the next and never read.
- **Whole, once** is the same with a move that never replaces a file of that name (on
  Windows `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, with `MOVEFILE_WRITE_THROUGH`;
  on Linux `link(2)`, then removing the first name). The file is there whole, or not at all.
- **Appended** is a line added to the end and flushed. A kill can leave the start of a line
  at the end: `plays::ending` is the one rule, used when reading and when adding to it, for
  where the last record ends.
- **In place** is written straight over the old file. A kill leaves it cut short.
- **Renamed, unflushed** is written beside its place and renamed, with no flush. A kill
  leaves the old file or the new one; a power cut can leave the name empty or holding part of
  the new one.

"Settings folder" is `%APPDATA%\Nexus Music\config`; "cache" is
`%LOCALAPPDATA%\Nexus Music\cache`; "data" is `%LOCALAPPDATA%\Nexus Music\data`.

## What can't be had again

| File | What it holds | How it is written | A kill mid-write leaves | Found unreadable |
|---|---|---|---|---|
| `settings.json` (settings folder) | Every setting, the server and name signed in with, what this PC last agreed with the others | Whole | The old or the new | The copy from the last good start (`settings.backup.json`) is used, and the person is told; with no good copy, it is kept as `settings.damaged.json`, the app starts with defaults, and says so |
| `settings.backup.json` | The settings as the last start found them good | Copied in place at each start | A copy cut short, which is only read if the settings themselves can't be: it then reads as no copy, and the damaged settings are kept aside as above | As no copy |
| `plays.log` | Every play counted here, one line each | Started whole, once; appended; written again whole when the oldest go or a source is told late | Started: no log (it is started again next time, from the same list); appended: at most the start of a line, which is no play to the reader or the writer | Left exactly as it is and not written to; plays go to `plays.log.new` and are joined to it the next time it reads; the person is told. Damaged lines are passed over, and the log is then only ever added to |
| `plays.log.new` | Plays counted while the log couldn't be read | Appended | As for the log | Its lines that read are joined, once each |
| `plays.json` | The plays as versions before 0.26 kept them | Not written; removed once the log has them all | (Nothing: a start cut short leaves it there, and its plays are put back into the log, and the person told) | Left where it is, and the log starts from Recently played |
| `shared-record-kept-<account>-<version>.json` | The record devices share, as this version first found it | Whole, once | Nothing at its name | Never read by the app: it is for putting the record back by hand. One made by 0.40.5 to 0.40.7 can be cut short, and can't be told from a whole one |
| `asked.json` | What was asked for from Lidarr, to announce when it arrives and put into playlists | Whole | The old or the new | Empty: said on the Requests page. Cut short: kept as `asked.damaged.json`, and said. Can't be read just now: left as it is, not written over, read again when something is next asked for, and said |
| `asking.json` | A batch of albums being asked for, and the "new releases" settings it has to put back on Lidarr | Whole | The old or the new | Kept as `asking.damaged.json` (an empty one is removed), and the Requests page says the batch wasn't carried on and what may not have been put back |
| `pending-<account>.json` | Likes, ratings and plays waiting to be sent to the server | Whole | The old or the new | *Not yet*: taken for nothing waiting. (Only a power cut on 0.40.10 or earlier could leave it empty, and nothing is left in such a file to get back) |
| `queue.json`, `queue-place.json` | The queue, and where in it playback had got to | Whole, on a thread of its own | The old or the new | *Not yet*: taken for no queue |
| `history.json` | Recently played | Whole | The old or the new | *Not yet*: taken for none; what other devices share comes back with the next exchange |
| `playlist-added.json` | When songs were added to playlists, as this PC saw it | Whole | The old or the new | *Not yet*: taken for none; what devices share comes back with the next exchange |
| `downloads\index-<account>.json`, `collections-<account>.json` | Which songs are downloaded, and which lists are kept downloaded | Whole | The old or the new | *Not yet*: taken for none. The songs stay in the folder but aren't listed, and are fetched again if downloaded again |
| Downloaded songs (`downloads\` in data, or the folder chosen in Settings) | Songs saved for listening without the server | Renamed, unflushed (`….part`) | The old or nothing; what a kill left as `.part` is cleared at the next start. A power cut can leave a song cut short, which plays short | — |
| `credential`, `secret-*` (Linux development build only) | The password and other secrets | In place | A cut-short secret, which is a wrong one | As none |

## What can be had again

| File | What it holds | How it is written | A kill mid-write leaves | Found unreadable |
|---|---|---|---|---|
| `crash.log` (data), `crash.seen` | What went wrong when the app stopped by itself; how much of that has been mentioned | Appended; whole | At most a line cut short; the old or the new | Read as text; a mark that can't be read is taken for nothing mentioned, so the note is shown once more |
| `diagnostics.txt` (data) | What "Save diagnostics" writes, opened as soon as it is written | In place | A file cut short, which isn't opened: no "Saved" is shown | — |
| Exported playlists (`.csv`, where the person chose, or `exports\` in data), `Nexus Music - not found.txt` beside files brought in | What the person asked to have written, where they chose | In place | A file cut short, and no "Exported" or report shown. (Not written beside first: that name, in the person's own folder, could be one of theirs) | — |
| `discover.json` (cache) | Artists worth a look, as last worked out | In place | A file cut short | As none kept: looked for again when Home is opened |
| `pages\…\*.json` (cache) | Pages as last fetched, shown while they are fetched again | In place | A file cut short | As no page: fetched as if none was kept |
| `pages\…\library.json` | A copy of the library, for search | Renamed, unflushed | The old or the new; after a power cut, possibly nothing | As no copy: fetched again |
| `pages\…\lyrics\*.json` | Lyrics, and that a song has none | Renamed, unflushed | The old or the new; after a power cut, possibly nothing | As nothing kept (not "none"): asked again |
| `covers\*` (cache) | Cover pictures | Renamed, unflushed (`….part`) | The old or the new; after a power cut, possibly nothing | As no picture: not shown, removed, and fetched again |
| `songs\*` (cache) | Songs kept as they're played and fetched ahead | Renamed, unflushed (`….fetch`, or a second name for the streamed file) | The old or nothing | An empty one is not kept, and is fetched again; after a power cut one can hold part of a song, which plays short |
| `lyrics\` (data) | Where version 0.22 kept lyrics it fetched itself | Not written | — | Removed at the start |
| `buffers\*` (cache) | What is streaming now | In place, while it streams | — | Cleared at the start |
