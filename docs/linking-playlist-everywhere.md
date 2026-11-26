# The linking playlist, out of sight in every copy

**A design, not built. For review as a change to everybody's playlists, before any of it
is written.** It is the last item of "for everyone, not for a tester's pair" in
`linking-design.md`, section 12, and it replaces the hiding that was built for both apps
and withdrawn (5.1).

## What it is for

The secret a person's devices trust each other by is kept in the description of a
playlist of theirs, "Nexus Music link" (5.1). A playlist's description is on show
wherever the playlist is: in the list, on its page, in the files an app keeps of both.
Today a copy hides it only where linking is switched on in that copy. When linking is
switched on for everyone, every copy a person has is one of their linked devices or is
about to be, and the playlist should then be what the sync playlist ("Nexus Music sync")
already is in every copy of both apps: **never listed, never opened, never kept on
disk, never offered as a place to put songs, and its name not one a playlist of the
person's own can be given.**

## Why the first attempt was withdrawn, and what this does differently

It went by what a playlist holds, everywhere, in copies with linking switched off, and
reached into three things every person relies on:

1. **A kept list was rewritten, or thrown away**, when it was found to hold the row.
2. **The import resolved names against a filtered list**, so a playlist it could not see
   could be made a second time.
3. **Pages still opened by id**, from a link kept somewhere else, past the filter.

This design has one rule, applied **where playlists come in from the music server**,
which is two requests in each app, and nowhere after. Nothing downstream of those two
is changed, so nothing that already holds a list is rewritten: the list is simply
right the next time it arrives.

## The rule

A playlist is **linking's** if, as the music server describes it,

- its description **reads as a linking record** (the library's `is_record`: the mark,
  with a layout this version knows and a well-formed secret, or a later layout),
  **whatever it is called**; or
- it has linking's name, **no description, no songs, is private and is the account's
  own**: one being set up, in the moment between being made and being written.

Anything else is the person's own, including a playlist that only has the name: one
with songs in it, or a description of their own, or that is somebody else's. Linking
already refuses to use such a playlist and tells the person to rename it (5.1); it has
to stay in sight for them to do that.

The rule is one function in the shared library, which both apps call. It does not ask
whether linking is on in this copy.

## Where it is applied

| | Windows | Phone |
|---|---|---|
| **The list of playlists arrives** | `playlists_arrived` (`src/controller.rs`), where "Nexus Music sync" is taken out today. The sidebar, the playlists page, "Add to playlist" and the jam's picker read the list it keeps, which is in memory and not on disk. | `Library.playlists()` (`library/Library.kt`), the one fetch behind the Library page, search, the menu's picker, the car's tree and Downloads, where the sync playlist is taken out today. What is kept on disk is the list after the rule. |
| **One playlist is fetched by id** | The page loader (`src/pages.rs`). A playlist that is linking's is "not found", as one that has been deleted is. | `Library.playlist(id)`, which already answers "not found" for the sync playlist. |
| **A kept file from before this version is read** | The page cache already refuses to write, and throws away unread, a page whose description is a record (`page_may_be_kept`), in every copy today. | The kept list is filtered as it is read, as it already is for the sync playlist; a kept page that is linking's is thrown away unread, as the sync playlist's is. **Neither file is rewritten for it.** |
| **The import reads the server's playlists itself** | `src/import.rs` filters its own reading by the same function, and refuses the name with linking's words. It resolves against the same list the person sees, so nothing is made twice. | (No import.) |
| **The name** | `kept_name`: refused for a new playlist and for a rename, in every copy. | `Playlists.reserved`: the same. |

What reads its playlists **outside** those two requests is not touched and must not be:
linking's own look at the secret, and keeping in step, which reads the sync playlist.
Each has its own request already.

## What a person could notice

- **Nothing, for nearly everyone**: the playlist was made by the app, has no songs, and
  disappears from one list.
- **Someone who gave a playlist of their own that exact name, with nothing in it and no
  description**, private: it reads as one being set up, and is hidden; linking then
  writes its record into it. This is the one case where a playlist of the person's own
  is taken. It is the same today wherever linking is on (5.1, weakness 10), and the
  first write is refused unless the playlist is empty, private and theirs.
- **Sync's notes** (a pin, a sort, a shuffle kept for a playlist by id) can name the
  playlist only if the person pinned or sorted it while a copy showed it. Such a note is
  left where it is and matches nothing, as a note for a deleted playlist does.
- **A download of it** cannot exist: it has no songs. A phone's Downloads leaves the
  sync playlist out by name today; nothing is added there.
- **Recently played and "Your stats"** list what was played from; nothing can be played
  from an empty playlist.

## What stays on disk until it is next replaced

A phone's kept list of playlists, written by a version from before this one, holds this
playlist's description with every other's until the list is next fetched and written,
which is the first time the app is opened online. It is not shown (it is filtered as
it is read) and it is not rewritten for this. The secret is the owner's own, and
anything signed in to the account can read it from the music server (weakness 1); a
copy of it in the app's private files for one more start is not a new exposure. If the
review wants it gone at once, the smallest way is to have the first start of the new
version fetch the list before showing it, and that is a change to how every phone
starts.

## What has to ship together

The rule is useful only once it is in **every** copy a person has, and harmless before
linking is on anywhere. So it goes out in the release that switches linking on for
everyone, or in one before it, never after: a copy that has linking on while another
of the person's copies still lists the playlist is where the secret is on show. The
update check already says which copies are behind; 5.10 already keeps a copy that is
behind from being steered.

## Tests it would need

- The rule, as a table: a record under its name, renamed, in a later layout; one being
  set up; the name with songs, with a description, public, somebody else's; a
  description that only looks like a record. One table, in the library, which both
  apps' tests name.
- Each app: the list with such a playlist in it, as each screen that draws from it gets
  it; the page by id; a kept file from before read once and not rewritten (its bytes
  unchanged); the import, with a playlist of the name on the server.
- The switch-off walk of each app's playlists, before and after, with such a playlist
  left on the test server on purpose.

## Questions for the review

1. Is "no description, no songs, private, the account's own, and the name" too wide for
   "being set up"? The alternative is to hide only a playlist that already holds a
   record, and let one being set up show for the second it takes.
2. Should the kept list on a phone be left to be replaced at its next fetch (above), or
   fetched before it is first shown?
3. Search on a phone and "Your stats" were left listing the playlist in the trial. With
   the rule at the fetch they are covered without being touched; is there any other
   reader of the server's playlists in either app that this table has missed?
