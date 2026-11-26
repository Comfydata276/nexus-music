# The files the app keeps

**The rule: a write made because the person just did something, or one the app must have on
disk before it acts, is flushed before it takes its name; a write made by a timer, or by
playback moving on, is not** (it is renamed into place, as 0.40.10 wrote everything), and
its row says what a power cut within a few seconds of it can cost. Either way it is written
whole, beside its place, and renamed into place (`config::write_whole`, which is told which
of the two it is: `Durable::Flushed` or `Durable::InPassing`), so a kill leaves the old file
or the new one. A file that is there and can't be read is never taken for one that isn't, or
for an empty one; whatever can be fetched again may be written more cheaply, if a file of it
cut short is read as not there, never as content or as an error, and each such reader has a
test that shows it. Rows marked *not yet* are files whose reader still takes one that can't
be read for none, or that a power cut can still leave cut short.

## How a file is written

- **Whole** (`config::write_whole`, through `write_surely`): written beside its place under a
  name of that write's own (the file's whole name, the process's number, a count, then
  `.tmp`), flushed to the disk, renamed over the old file, and on Linux its folder synced.
  Each write renames and removes only its own file. What a killed run left beside a file is
  never read, and is cleared by exact name when the app next starts, before anything is
  written (the last row below).
  - A kill leaves the old file or the new one.
  - A power cut: on Linux, the old file or the new one (if the folder's sync failed, the write
    still counts as made, and the name may come back as the old file). On Windows the
    contents are on the disk before the rename, and the rename is made without write-through:
    the file system *should* keep the old file or the new one, which is Windows' promise and
    no more.
- **Whole, once** (`config::write_whole` with `config::ONCE`): the same, with a move that
  never replaces a file of that name (Windows: `MoveFileExW` without
  `MOVEFILE_REPLACE_EXISTING`, with `MOVEFILE_WRITE_THROUGH`; Linux: `link(2)`, then the first
  name removed, then the folder synced). A kill or a power cut leaves the whole file at its
  name, or nothing there. On Linux, on a drive that can't make hard links, nothing is made,
  and the person is told once ever.
- **In passing**: written whole as above, but renamed into place without a flush, and its
  folder not synced. A kill leaves the old file or the new one; a power cut within a few
  seconds of the write can leave the name empty, or holding part of the new file.
- **By the queue's saver**: the queue's two files only, in passing, on a thread of their own
  (`src/saver.rs`), the newest content winning; a write that failed is not said to be done.
  Every other file is written by the thread that changes it, when it changes.
- **Appended**: lines added to the end of the file and flushed. A kill or a power cut can leave the start of a line at the end: `plays::ending` is
  the one rule, used when reading and when adding to it, for where the last record ends.
- **In place**: written straight over the old file. A kill leaves it cut short. A power cut
  can leave it cut short, empty, or holding what was never flushed.
- **Renamed, unflushed**: written beside its place and renamed, with no flush. A kill leaves
  the old file or the new one. A power cut can leave the name empty, or holding part of the
  new file.

## Order, retries, and two copies at once

- **One writer for each file, in order.** Each file is written by the thread that changes
  it, when it changes: the settings, the log of plays, what waits to be sent and the rest on
  the window's thread; the queue's two files by the queue's saver, in the order asked, the
  newest winning; the batch's file while the batch is held, by whichever thread changed it.
- **Retries.** A write that fails is never said to be done, and is not tried again by
  itself, with these exceptions: the list of downloads stays due, and is tried again a
  couple of seconds later and when the batch is over; plays that couldn't be added to the
  log are kept, and added with the next play, and before any are sent; the queue's saver
  says a failed write wasn't done (`Saver::flush`), and writes the next version when the
  queue changes. Every other file is written again at its next change.
- **Two copies of the app on one profile** (only possible by pointing both at one folder
  with `NEXUS_MUSIC_HOME`) are not supported. Each writes its files as if alone: one can
  write over the other's settings, queue and lists, and plays can be lost (one copy writes
  the log again from its own memory, over lines the other added). The README says the same
  of keeping devices in step.

"Settings folder" is `%APPDATA%\Nexus Music\config`; "cache" is
`%LOCALAPPDATA%\Nexus Music\cache`; "data" is `%LOCALAPPDATA%\Nexus Music\data`.

## What can't be had again

| File | What it holds | How it is written | A kill mid-write leaves | A power cut mid-write leaves | Found unreadable |
|---|---|---|---|---|---|
| `settings.json` (settings folder) | Every setting, the server and name signed in with, what this PC last agreed with the others | Flushed after a change the person made; a shared choice is a checked write, taken back if it isn't made. In passing when an exchange or a new queue starting saves it | The old or the new | Flushed: as for "whole". In passing: within a few seconds, possibly an empty file, which is then read from the backup as below (losing what changed since the last start) | The copy from the last good start is used, and the person is told. It is copied back over `settings.json` in place: a kill during that leaves `settings.json` cut short, and the next start does the same again from the copy. With no good copy, it is moved aside as `settings.damaged-<date and time>.json` and the note names it; if it can't be moved, the note says so, and the defaults the app starts with take its place when they are next saved |
| `settings.backup.json` | The settings as the last start found them good | Copied in place at each start | A copy cut short | A copy cut short or empty | Only read when `settings.json` can't be: then it reads as no copy, and `settings.json` is moved aside as above |
| `plays.log` | Every play counted here, one line each | Started whole, once; appended; written again whole, flushed, when a source is told late, and when the oldest go (playback moving on, but flushed all the same: in passing, a power cut could leave the whole log empty) | Started: no log, and it is started again at the next start, from the same list. Appended: at most the start of a line, which is no play to the reader or the writer | As for a kill | Left exactly as it is and not written to; plays go to `plays.log.new` and are joined to it the next time it reads; the person is told. Damaged lines are passed over, and the log is then only ever added to. One that an earlier version left cut short as it was started is read by these rules and no more: the plays it was carrying over are not put back |
| `plays.log.new` | Plays counted while the log couldn't be read or started | Appended | As for the log | As for the log | Its lines that read are joined, once each |
| `plays.json` | The plays as versions before 0.26 kept them | Not written. Removed only by the run that starts the log from it, right after the log is in place and its folder synced | — | — | Unreadable: no log is started without it, and the person is told. Found beside a log that is already there: never read and never removed. It is no part of the log, and may be deleted by hand |
| `shared-record-kept-<account>-<version>.json` | The record devices share, as this version first found it | Whole, once, and its folder synced before anything is sent | Nothing at its name | Nothing at its name, or the whole file | Never read by the app: it is for putting the record back by hand. One made by 0.40.5 to 0.40.7 can be cut short, and can't be told from a whole one |
| `asked.json` | What was asked for from Lidarr, to announce when it arrives and put into playlists | Whole | The old or the new | As for "whole" | Empty: said on the Requests page. Cut short: moved aside as `asked.damaged-<date and time>.json`, and said, naming it; if it can't be moved, as one that can't be read. Can't be read just now: left as it is, not written over, read again when something is next asked for, and said |
| `asking.json` | A batch of albums being asked for; and what the "new releases" setting of an artist is to be put back to, for the batch's requests and for one asked for by itself | Its progress in passing. What is to be put back is a checked write, flushed and made while the batch is held, before Lidarr is changed, on every path that changes it: if it fails, Lidarr isn't changed, and the request says why. Put back, it is crossed off. The batch is always written while it is held, so an older copy never overtakes a newer | The old or the new | Progress: within a few seconds, possibly an empty file (said, as below). What is to be put back: as for "whole" | Can't be read: left as it is, nothing saved over it, read again at the next start, and said on the Requests page. Cut short: moved aside as `asking.damaged-<date and time>.json`, and said, naming it; if it can't be moved, left, nothing saved over it, and said. Empty: removed, and said. While one is left, nothing that would have to be put back is changed on Lidarr |
| `pending-<account>.json` | Likes, ratings and plays waiting to be sent to the server | Whole | The old or the new | As for "whole" | *Not yet*: taken for nothing waiting |
| `queue.json`, `queue-place.json` | The queue, and where in it playback had got to | By the queue's saver | The old or the new | Within a few seconds of a write: possibly an empty file, and then no queue | *Not yet*: taken for no queue |
| `history.json` | Recently played | In passing, as a song starts or an exchange brings plays | The old or the new | Within a few seconds of a write: possibly an empty file, and then no Recently played until other devices share theirs | *Not yet*: taken for none; what other devices share comes back with the next exchange |
| `playlist-added.json` | When songs were added to playlists, as this PC saw it | Flushed after an edit, an import, a playlist made or deleted here; in passing when a look at a playlist or an exchange notes something | The old or the new | Flushed: as for "whole". In passing: within a few seconds, possibly an empty file, and then no dates | *Not yet*: taken for none; what devices share comes back with the next exchange |
| `index-<account>.json`, `collections-<account>.json` (in `downloads\`) | Which songs are downloaded, and which lists are kept downloaded | In passing while a batch arrives; flushed when the batch is over, after a removal, and for the playlists kept downloaded | The old or the new | In passing: within a few seconds, possibly an empty list, and then the songs in the folder aren't listed | *Not yet*: taken for none. The songs stay in the folder but aren't listed, and are fetched again if downloaded again |
| Downloaded songs (`downloads\` in data, or the folder chosen in Settings) | Songs saved for listening without the server. Moving the folder (Settings, Storage) is decided before anything moves: the lists that read as the app's (a list of playlists only beside a list of downloads of the same account), and the songs they name by a plain file name that is there. Nothing else in the folder is touched. If any of those names is had in the new folder already, nothing moves, and the person is told how many and where. A file that can't move has what moved put back, by the move's own list; if some can't come back, the person is told how many are in which folder, and the app stays on the old one | Renamed, unflushed (from `<id>.<extension>.<attempt>.part`). Moved by rename on one drive; across drives copied under a temporary name of the app's in the new folder, flushed, given its name, and then removed from the old | The old or nothing; what a kill left as `<id>.<extension>.<attempt>.part` is cleared at the next start (a bare `<file>.part` only if the list names that file; any other `.part` is the person's, and left). A kill in the middle of a move is not resumed: each song is whole in one folder or the other, and the app keeps to the folder its settings name (the old one, until the move had finished). To put it right, move the songs and the lists (`index-…json`, `collections-…json`) by hand into the folder Settings, Storage names, and start the app again. A copy across drives cut short leaves `<song file>.<process>.<count>.tmp` in the new folder, which may be deleted | *Not yet*: a song cut short, or empty, which is listed and plays short | — |
| `credential`, `secret-<name>` (Linux development build only) | The password and other secrets | In place | A cut-short secret, which is a wrong one | The same, or an empty one | As none |

## What can be had again

| File | What it holds | How it is written | A kill mid-write leaves | A power cut mid-write leaves | Found unreadable |
|---|---|---|---|---|---|
| `crash.log` (data) | What went wrong when the app stopped by itself | Appended, unflushed | At most a line cut short | Lines lost, or cut short | Read as text |
| `crash.seen` (data) | How much of the crash log has been mentioned | Whole | The old or the new | As for "whole" | Taken for nothing mentioned, so the note is shown once more |
| `diagnostics.txt` (data) | What "Save diagnostics" writes, opened as soon as it is written | In place | A file cut short, which isn't opened: no "Saved" is shown | The same, or an empty file | — |
| Exported playlists (`.csv`, where the person chose, or `exports\` in data), `Nexus Music - not found.txt` beside files brought in | What the person asked to have written, where they chose | In place | A file cut short, and no "Exported" or report shown. (Not written beside first: a name beside it, in the person's own folder, could be one of theirs) | The same, or an empty file | — |
| `discover.json` (cache) | Artists worth a look, as last worked out | In place | A file cut short | A file cut short or empty | As none kept: looked for again when Home is next opened |
| `pages\…\*.json` (cache) | Pages as last fetched, shown while they are fetched again | In place | A file cut short | A file cut short or empty | As no page: fetched as if none was kept |
| `pages\…\library.json` | A copy of the library, for search | Renamed, unflushed | The old or the new | Possibly nothing, or part of it | As no copy: fetched again |
| `pages\…\lyrics\*.json` | Lyrics, and that a song has none | Renamed, unflushed | The old or the new | Possibly nothing, or part of it | As nothing kept (not "none"): asked again |
| `covers\*` (cache) | Cover pictures | Renamed, unflushed (from `….part`) | The old or the new | Possibly nothing, or part of a picture | As no picture: not shown, removed, and fetched again |
| `songs\*` (cache) | Songs kept as they're played and fetched ahead | Renamed, unflushed (from `….fetch`, or a second name for the streamed file) | The old or nothing | Possibly an empty file, or part of a song | An empty one is not kept, and is fetched again. Any file with something in it is taken for the song: one a power cut left cut short plays short |
| `lyrics\` (data) | Where version 0.22 kept lyrics it fetched itself | Not written | — | — | Removed at the start |
| `buffers\*` (cache) | What is streaming now | In place, while it streams | — | — | Cleared at the start |
| `<file>.<process>.<count>.tmp`, and the names 0.40.10 and before could leave (`settings.tmp`, `queue.tmp`, `queue-place.tmp`, `history.tmp`, `playlist-added.tmp`, `plays.tmp`, `asking.json.part`, `pending-<account>.tmp`, `shared-record-kept-<account>-<0.40.8 to 0.40.10>.tmp`; `index-<account>.tmp`, `collections-<account>.tmp` in a downloads folder) | A write under way, or left by a run killed during one | (The first step of "whole") | — | — | Never read. Cleared when the app starts, once, before anything is written, and only by exact name: in the settings and data folders, an app file's own name with this version's ending, or one of the old names above, for the account signed in; in a downloads folder, which may be the person's own, only that account's two lists'. Another account's are left until it is the one signed in when the app starts |
