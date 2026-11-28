//! The backend's files are the owner's alone (the review of S3, finding 8): the data holds
//! email addresses, device details and password verifiers, its copies the same, and the
//! store kept apart its keys.
//!
//! - **From the moment each is made**: a file is made by this program, empty, readable and
//!   writable by its owner only (0600), before SQLite or anything else writes a byte to it;
//!   a folder the program makes is its owner's only (0700). SQLite makes the files it writes
//!   beside the data (`-wal`, `-shm`) with the data file's own mode, whatever the umask.
//! - **And those made before**, by an older program or by hand: at every start, and before
//!   either command does anything, what is wider is narrowed. Nothing is ever widened: a file
//!   the owner made read-only stays read-only.
//! - **The two folders the owner gives** (`NEXUS_DATA`, `NEXUS_STATE`) are theirs: only the
//!   right of others to write in them is taken away, so that no one else can put a file there
//!   or take one away; who may look in them is the owner's choice.

use std::fs::{self, File};
use std::io;
use std::path::Path;

/// A new file at `path`, empty and the owner's alone; refused if anything is there already.
pub fn create(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// The folder at `path`, made the owner's alone if it is not there; narrowed if it is.
pub fn folder(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(why) if why.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => narrow(path, 0o077),
        Err(why) => Err(why),
    }
}

/// Takes the bits in `take` away from what is at `path`, if anything is there and has any of
/// them. Never adds one.
#[cfg(unix)]
fn narrow(path: &Path, take: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match fs::symlink_metadata(path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(why) => return Err(why),
        Ok(meta) => meta,
    };
    // A link is not followed: what it points at is not the backend's to change.
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mode = meta.permissions().mode();
    if mode & take == 0 {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777 & !take))
}

#[cfg(not(unix))]
fn narrow(_path: &Path, _take: u32) -> io::Result<()> {
    Ok(())
}

/// What a file of the backend's own may not be: anything for anyone but its owner.
pub fn file(path: &Path) -> io::Result<()> {
    narrow(path, 0o077)
}

/// Before anything else is read: the data's own files, the copies and the store kept apart
/// are files, not links (the re-review's 8): a link is refused, never followed, and what it
/// points at is never narrowed.
pub fn no_links(state: &Path, data: &Path) -> Result<(), String> {
    let mut names: Vec<std::path::PathBuf> = ["", "-wal", "-shm", "-journal"].iter().map(|suffix| data.join(format!("{}{suffix}", crate::store::data::FILE))).collect();
    names.push(crate::store::data::incoming(data));
    names.push(crate::store::copies::folder(data));
    names.push(crate::store::apart::place(state));
    names.push(state.join(crate::identity::FILE));
    if let Ok(entries) = fs::read_dir(crate::store::copies::folder(data)) {
        names.extend(entries.flatten().map(|entry| entry.path()));
    }
    for path in names {
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(format!(
                "{} is a link. The backend keeps its data, its copies and its store kept apart as files of their own, and follows no link to them: nothing is served and nothing was changed. Put the file itself there.",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Before anything is read for use: every file the backend keeps in `state` and `data`, and
/// its copies, narrowed to the owner's alone; the two folders closed to others' writing.
/// What cannot be narrowed is said, and stops the program: a file others can read is not
/// served from.
pub fn at_start(state: &Path, data: &Path) -> Result<(), String> {
    let failed = |path: &Path, why: io::Error| format!("{} can't be made the owner's alone ({}). Nothing is served.", path.display(), why.kind());
    for mount in [state, data] {
        narrow(mount, 0o022).map_err(|why| failed(mount, why))?;
    }
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for folder in [state, data] {
        for entry in fs::read_dir(folder).map_err(|why| failed(folder, why))?.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                files.push(entry.path());
            }
        }
    }
    let copies = crate::store::copies::folder(data);
    if copies.is_dir() {
        folder(&copies).map_err(|why| failed(&copies, why))?;
        for entry in fs::read_dir(&copies).map_err(|why| failed(&copies, why))?.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                files.push(entry.path());
            }
        }
    }
    for path in files {
        file(&path).map_err(|why| failed(&path, why))?;
    }
    Ok(())
}
