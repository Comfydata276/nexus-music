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

**On the window's thread a write waits for the disk only inside the handler of a press on a
discrete control** (a button, a toggle, a choice, a menu item, a confirmed dialog), and once
per press. Everything else on that thread is in passing: timers, playback moving on, an
answer arriving from the network, a scan's result, a download finishing, and continuous
controls (a slider, a drag). Shuffle or Repeat arriving from a jam's guest or a linked device
is no press here, and goes in passing. Worker threads flush wherever the rule above says. With
`NEXUS_TRACE` set, every file written whole and flushed on the window's thread is printed
(`[trace] flushed on the window's thread …`), with the app's functions that asked for it; a
folder's sync after a rename (Linux) and a damaged file's move aside (written through) are not
printed. **Flushed there outside a press, by choice:** the waiting-changes file whenever it
holds a like or a rating not yet sent (a play counted, or an answer from the network, writes
it again: its row); the settings once as the app ends, after the window has gone; the settings
when a move of the downloads completes; the installer finishing, which quits; the play log's
trim; a batch of albums' file while it holds a way back (its row); `asked.json` for a request's
note and a take-off-the-list's answer, once each when Lidarr answers (the one record that it
was done), and for a batch's notes once when it ends or is stopped, while what a scan finds
(an arrival, a fill, an expiry), sign-in's tidy and a batch's notes as it runs are written in
passing (the next look finds them again). Each is the end of something the person started,
or a repair at the start. **Not yet so** (they
are flushed on the window's thread outside a press, and moving them would take more than
this change): what an exchange with the server must have on the disk before it sends (the
kept copy of the shared record, the shared choices saved before a write, the plays checked
on the disk before they are sent), and a play's line added to the log as it is counted.

## How a file is written

- **Whole** (`config::write_whole`, through `write_surely`): written beside its place under a
  name of that write's own (the file's whole name, the process's number, a count, then
  `.tmp`), flushed to the disk, renamed over the old file, and on Linux its folder synced.
  Each write renames and removes only its own file. What a killed run left beside a file is
  never read, and is cleared by exact name when the app next starts, once its settings are
  read and before any of these files is written (the last row below).
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
  newest winning; the batch's file by whichever thread changed it, copied while the batch is
  held and written after it is let go, writers taking turns, so that an older copy is never
  written over a newer.
- **Retries.** Most writes don't say whether they failed: a failed one leaves the file as it
  was, and the file is written again at its next change. These do: a checked write (a
  shared choice, which is taken back; the kept copy and the batch's way back, without which
  nothing is sent or changed on Lidarr); the list of downloads, which stays due and is
  tried again a couple of seconds later and when the batch is over; the log of plays, whose
  plays that couldn't be added are kept and added with the next play, and before any are
  sent; and the queue's saver, which says a failed write wasn't done (`Saver::flush`) and
  writes the next version when the queue changes.
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
| `settings.json` (settings folder) | Every setting, the server and name signed in with, what this PC last agreed with the others | Flushed in the handler of a press (a toggle, a choice, a confirmed dialog, signing out), once: Shuffle pressed while a list plays writes them once, with the list's choice; a shared choice is a checked write, taken back if it isn't made. Flushed once more as the app ends, after the window has gone (Quit itself doesn't write them), and before an update's installer is started, which can close the app before that. In passing otherwise: an exchange, a new queue starting, an answer from the network (signing in, Lidarr), the update check, a slider when its drag ends, an identity made on first use, and Shuffle or Repeat from a jam's guest or a linked device (the list's choice with it still checked, but not waited for) | The old or the new | Flushed: as for "whole". In passing: within a few seconds, possibly an empty file, which is then read from the backup as below (losing what changed since the last start) | The copy from the last good start is used, and the person is told. It is copied back over `settings.json` in place: a kill during that leaves `settings.json` cut short, and the next start does the same again from the copy. With no good copy, it is moved aside as `settings.damaged-<date and time>.json` and the note names it; if it can't be moved, the note says so, and the defaults the app starts with take its place when they are next saved |
| `settings.backup.json` | The settings as the last start found them good | Copied in place at each start | A copy cut short | A copy cut short or empty | Only read when `settings.json` can't be: then it reads as no copy, and `settings.json` is moved aside as above |
| `plays.log` | Every play counted here, one line each | Started whole, once; appended; written again whole, flushed, when a source is told late, and when the oldest go (playback moving on, but flushed all the same: in passing, a power cut could leave the whole log empty) | Started: no log, and it is started again at the next start, from the same list. Appended: at most the start of a line, which is no play to the reader or the writer | As for a kill | Left exactly as it is and not written to; plays go to `plays.log.new` and are joined to it the next time it reads; the person is told. Damaged lines are passed over, and the log is then only ever added to. One that an earlier version left cut short as it was started is read by these rules and no more: the plays it was carrying over are not put back |
| `plays.log.new` | Plays counted while the log couldn't be read or started | Appended | As for the log | As for the log | Its lines that read are joined, once each |
| `plays.json` | The plays as versions before 0.26 kept them | Not written. Removed only by the run that starts the log from it, right after the log is in place and its folder synced | — | — | Unreadable: no log is started without it, and the person is told. Found beside a log that is already there: never read and never removed. It is no part of the log, and may be deleted by hand |
| `shared-record-kept-<account>-<version>.json` | The record devices share, as this version first found it | Whole, once, and its folder synced before anything is sent | Nothing at its name | Nothing at its name, or the whole file | Never read by the app: it is for putting the record back by hand. One made by 0.40.5 to 0.40.7 can be cut short, and can't be told from a whole one |
| `asked.json` | What was asked for from Lidarr, to announce when it arrives and put into playlists | Whole: flushed for a request's note and a take-off-the-list's answer, and a batch's notes once at its end; in passing for what a scan finds, an expiry, and a batch's notes while it runs | The old or the new | Flushed: as for "whole". In passing (a scan's arrival, fill or expiry, sign-in's tidy, a batch's notes as it runs): within a few seconds of the write, possibly an empty file, and then every note is gone, not only that change: what was asked for before isn't announced when it arrives or put into its playlists, and the Requests page says so (as below) | Empty: said on the Requests page. Cut short: moved aside as `asked.damaged-<date and time>.json`, and said, naming it; if it can't be moved, as one that can't be read. Can't be read just now: left as it is, not written over, read again when something is next asked for, and said |
| `asking.json` | A batch of albums being asked for; and what the "new releases" setting of an artist is to be put back to, for the batch's requests and for one asked for by itself | Its progress in passing, while it holds no way back; flushed whenever it holds one. What is to be put back is a checked write, made before anything on Lidarr is changed (the profile made for it included), one step at a time with every other such change: if it fails, Lidarr isn't changed, and the request says why. Put back, it is crossed off, by the step that wrote it. Each way back names the Lidarr it was written for (its address as Settings has it, without a trailing slash) and is acted on only there: while another Lidarr is set up it is left in the file as it is, isn't work left to do and raises no notice, and it is put back once that Lidarr is set up again. One written by 0.40.10 names none, and is acted on at whichever Lidarr is set up, as 0.40.10 did; it is written back in 0.40.10's form. (0.40.10 can't read a file that holds a way back naming its Lidarr: it starts with no batch, and its next batch is written over it.) A copy is taken under the batch's lock and written after it is let go, writers in turn, so an older copy is never written over a newer. Start, Try again and Clear, and the tidy when a batch ends, write it on the window's thread, which can wait there for another writer's flush; the tidy flushes it if it holds a way back. (Every read of an artist's record on Lidarr that is written back, by the batch, a request or "Keep track of artist", is a step of its own in the same turn, so no record read before another step's change is written back over it.) | The old or the new | Progress: within a few seconds, possibly an empty file (said, as below). What is to be put back: as for "whole" | Can't be read: left as it is, nothing saved over it, read again at the next start, and said on the Requests page. Cut short: moved aside as `asking.damaged-<date and time>.json`, and said, naming it; if it can't be moved, left, nothing saved over it, and said. Empty: removed, and said. While one is left, nothing that would have to be put back is changed on Lidarr |
| `pending-<account>.json` | Likes, ratings and plays waiting to be sent to the server | Flushed when a like or a rating is pressed while offline (once for "Like all", however many songs), and at every write while it holds a like or a rating not yet sent, whatever made the write and on whichever thread: a play counted, a round of sending answered, a failed send. In passing when it holds only plays | The old or the new | Flushed: as for "whole". In passing (only plays held): within a few seconds of the write, possibly an empty file, and then those plays are not sent | *Not yet*: taken for nothing waiting |
| `queue.json`, `queue-place.json` | The queue, and where in it playback had got to | By the queue's saver | The old or the new | Within a few seconds of a write: possibly an empty file, and then no queue | *Not yet*: taken for no queue |
| `history.json` | Recently played | In passing, as a song starts or an exchange brings plays | The old or the new | Within a few seconds of a write: possibly an empty file, and then no Recently played until other devices share theirs | *Not yet*: taken for none; what other devices share comes back with the next exchange |
| `playlist-added.json` | When songs were added to playlists, as this PC saw it | In passing: it is written when the server has taken an edit, a look at a playlist or a scan notes something, or other devices' notes are taken in, never inside a press | The old or the new | Within a few seconds of a write: possibly an empty file, and then no dates until other devices share theirs | *Not yet*: taken for none; what devices share comes back with the next exchange |
| `index-<account>.json`, `collections-<account>.json` (in `downloads\`) | Which songs are downloaded, and which lists are kept downloaded | In passing while a batch arrives and when it ends (and at quitting); flushed once for a removal the person asked for, however many songs; the playlists kept downloaded flushed on the press that changes them (once for "Remove all") | The old or the new | In passing: within a few seconds, possibly an empty list, and then the songs in the folder aren't listed | *Not yet*: taken for none. The songs stay in the folder but aren't listed, and are fetched again if downloaded again. A move of the folder is refused while its list of downloads can't be read, and says so; the move never writes that list over. Once the list has been written whole again (a song arriving, a removal), the folder can be moved. The list is read again as the move is planned: if it can't be then, the move is refused the same way |
| Downloaded songs (`downloads\` in data, or the folder chosen in Settings) | Songs saved for listening without the server. Moving the folder (Settings, Storage) is decided before anything moves: the lists that read as the app's (a list of playlists only beside a list of downloads of the same account), and the songs they name by a plain file name that is there. Nothing else in the folder is touched. A list of downloads that couldn't be read when it was loaded is never written over: the move is refused, and the person is told the list in that folder couldn't be read. Otherwise the list is written first only if it has changes not yet written. If any of the names is had in the new folder already, nothing moves, and the person is told how many and where. A file that can't move has what moved put back, by the move's own list; if some can't come back, the person is told how many are in which folder, and the app stays on the old one. The settings are flushed when the move completes. Left as 0.40.10 had them, or narrower: a file of the person's that reads as one of the app's lists (`index-tax.json` holding `[]`) moves with the downloads; a link among the songs is followed; a name with a trailing dot, a stream name or a device name isn't refused for it; two names differing only in case, moved to a drive that ignores case, pass the look, and the second fails once moving has begun (then the put-back runs); after a put-back that couldn't finish, or a settings save that failed, the songs can be in one folder and the list in the other (the app says where); removing a download while a move is under way can bring the song back, and a cancelled download can finish during a move | Renamed, unflushed (from `<id>.<extension>.<attempt>.part`). Moved by rename on one drive; across drives copied under a temporary name of the app's in the new folder (a new file, never one there already: a name that is taken is passed over for the next, and left as it is), flushed, given its name, and then removed from the old. A copy that fails removes only the temporary file it made | The old or nothing; what a kill left as `<id>.<extension>.<attempt>.part` is cleared at the next start (a bare `<file>.part` only if the list names that file; any other `.part` is the person's, and left; still cleared: a `family.zip.7.part` of the person's, or an `a.flac.part` beside a listed `a.flac`; left for ever: partial downloads from before 0.29 the list has no entry for). A kill in the middle of a move is not resumed: each song is whole in one folder or the other, and the app keeps to the folder its settings name (the old one, until the move had finished). To put it right, move the songs and the lists (`index-…json`, `collections-…json`) by hand into the folder Settings, Storage names, and start the app again. A copy across drives cut short leaves `<song file>.<process>.<count>.tmp` in the new folder: cleared, by that exact shape and only for a song the folder's list names, when the app next starts with that folder as its downloads folder; until then it may be deleted. Across drives, a power cut (not a kill) can lose the new folder's name for a song whose old copy is already gone, as 0.40.10 could | *Not yet*: a song cut short, or empty, which is listed and plays short | — |
| `credential`, `secret-<name>` (Linux development build only) | The password and other secrets | In place | A cut-short secret, which is a wrong one | The same, or an empty one | As none |

## What can be had again

| File | What it holds | How it is written | A kill mid-write leaves | A power cut mid-write leaves | Found unreadable |
|---|---|---|---|---|---|
| `crash.log` (data) | What went wrong when the app stopped by itself | Appended, unflushed | At most a line cut short | Lines lost, or cut short | Read as text |
| `crash.seen` (data) | How much of the crash log has been mentioned | In passing | The old or the new | Within a few seconds of a write: possibly an empty file, and the note shown once more | Taken for nothing mentioned, so the note is shown once more |
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
| `<file>.<process>.<count>.tmp`, and the names 0.40.10 and before could leave (`settings.tmp`, `queue.tmp`, `queue-place.tmp`, `history.tmp`, `playlist-added.tmp`, `plays.tmp`, `asking.json.part`, `pending-<account>.tmp`, `shared-record-kept-<account>-<0.40.8 to 0.40.10>.tmp`; `index-<account>.tmp`, `collections-<account>.tmp` in a downloads folder) | A write under way, or left by a run killed during one | (The first step of "whole") | — | — | Never read. Cleared when the app starts (after the one-copy check, and once its settings are read, before any of these files is written), and only by exact name: in the settings and data folders, an app file's own name with this version's ending (the kept copy's only with a version of digits and dots), or one of the old names above, for the account signed in; in a downloads folder, which may be the person's own, only that account's two lists'. Another account's are left until it is the one signed in when the app starts |
