# The linking playlist in every copy: the ruling

**Nothing here is built, and nothing here is to be built before linking is switched on
for everyone.** This page replaces a design that proposed hiding the playlist by what it
holds, at the two requests that bring playlists in. That design was reviewed as a change
to everybody's playlists and the lead ruled against building it, in that form or a
corrected one. This is the ruling, where it will land, and what it leaves.

## Why not by what it holds

The review found that to hide a playlist by its contents properly the rule has to reach
about twenty places across the two apps: every change made by a remembered id (Add to
playlist, rename, delete, Undo), the Spotify import and its deferred fill, Stats, export,
the download top-up, the car and the watch offline, the caches and their clean-up, sync's
date notes, and the sources of saved queues. Even then it cannot be kept on a device that
has not updated, and it cannot tell an interrupted set-up from an empty playlist of the
person's own, because the music server does not say enough. That is a large change to
everybody's playlists to keep one row out of sight.

It also found what takes most of the urgency away: **neither app shows a playlist's
description anywhere.** What a person sees, with no hiding at all, is a playlist called
"Nexus Music link" with no songs in it.

## The ruling

**"Nexus Music link" is treated exactly as "Nexus Music sync" is treated today: by the
code that already treats that name specially, and by nothing else.** The apps have left
the sync playlist out by its exact name since keeping in step shipped, with a sentence in
Settings saying it is there and to leave it be, and that has been enough.

At general switch-on, and not before:

- **Windows:** the two names are unlisted and reserved alike, whatever the switch. The
  library already has the shape: `secret::unlisted(name, linking)` and
  `secret::kept_name(name, linking)` take a flag for the second name, and the flag goes.
- **The phone:** each place that names the sync playlist gains the second name, the same
  way, and `link/LinkShown.kt` (the in-memory leaving-out used while linking is being
  tried) goes.
- **Settings:** "Link my devices" says what the sync setting says: it is kept on your
  server in a playlist named "Nexus Music link", which other apps will show; leave it be.
- **Nothing by content.** No classifier at the cache beyond what is there today, no
  clean-up, no rewriting of anything kept, and nothing in the import, Stats, downloads or
  the car beyond what the sync name already has there.

**Until then the rule is as it is** (`linking-design.md`, 5.1): switched off means
nothing changes; with linking on, the name is left out of the lists shown, in memory.

## Where the sync name is treated specially today

So that the change, when it comes, is a list of lines.

**The shared library** (`core/src/link/secret.rs`, `core/src/sync.rs`, `core/src/step.rs`)

| | |
|---|---|
| `sync::PLAYLIST_NAME`, `link::secret::PLAYLIST_NAME` | The two names. |
| `secret::unlisted(name, linking)` | The sync name always; linking's only with the flag. |
| `secret::kept_name(name, linking)` | The sync name is refused for a playlist of the person's own, with its own words (`step::reserved`); linking's only with the flag. |
| `secret::page_may_be_kept(name, comment)` | Never the sync playlist's page; never a page whose description is a linking record. Already so in every copy, and stays. |

**Windows**

| | |
|---|---|
| `src/controller.rs`, `playlists_arrived` | The list as it arrives: `unlisted` names are taken out. The sidebar, the playlists page, "Add to playlist" and the jam's picker read what is left. |
| `src/import.rs` | Its own reading of the server's playlists leaves `unlisted` names out; a playlist to import with a kept name is refused (`kept_name`). |
| `src/actions.rs` | A new playlist, and a rename: `kept_name`. |
| `src/controller.rs`, the page cache (two places) | `page_may_be_kept`, on writing and on reading. |
| `src/link.rs` | `unlisted` and `kept_name` pass the switch as the flag: the two lines that change. |

**The phone**

| | |
|---|---|
| `sync/Names.kt` | The name. |
| `library/Library.kt`, `playlists()` | The list as it is fetched: the sync playlist is filtered out, so what is kept on disk is the list without it. |
| `library/Library.kt`, the kept list's reader | The same filter as an old kept list is read. |
| `library/Library.kt`, `playlist(id)` | Fetched by id, it is "not found". |
| `library/Library.kt`, a kept page's reader | A kept page of it is not read. |
| `library/Playlists.kt`, `canEdit` | Never editable. |
| `library/Playlists.kt`, `reserved` | The name is not one a playlist of the person's own can take. |
| `downloads/DownloadedViews.kt` | Left out of the downloaded playlists. |

## What it leaves

What the sync playlist has always left. From the review's findings, as the ruling gives
them (the review's thread has each in full):

- **Reachable by a remembered id, for writing too.** Something that kept the playlist's
  id before it was left out (a saved queue's source, an Undo, a link) can still name it;
  and a menu or prompt opened while the row was still listed (Add to playlist, rename,
  delete) still goes through, because the id is what is sent.
- **A renamed record shows.** A playlist renamed somewhere else is an ordinary playlist
  to every list; its description is still the record. Its page is still never written to
  a cache.
- **An older build shows it.** A copy from before the change lists it like any other.

## Two things the review found about set-up, not hiding

For the "for everyone" list in `linking-design.md`, section 12:

- **`judge` adopts an existing empty playlist of the name that is public.** Setting up
  should not take a public playlist for its own.
- **An interrupted set-up leaves a blank row**: a playlist of the name, empty, with no
  description, made and not yet written.
