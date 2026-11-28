//! Remember the server's batch number and the original input. The server
//! remains the source of progress and owns every Lidarr change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use nexus_core::bulk::Want;
use nexus_core::nexus::account::RequestId;
use nexus_core::nexus::requests::{
    app_batch_step, AppBatchStep, AppBatchResolution as Resolution, BATCH_MAX_LINES, BackendLidarr, BatchLine, BatchLineProgress, BatchLineView, Change, JobId, MusicBrainzId, POLL_INTERVAL,
    Progress, Refusal, RequestError,
};
use serde::{Deserialize, Serialize};

use super::{Connection, Context, calls};

#[derive(Serialize, Deserialize)]
struct Saved {
    request: u64,
    submitting_session: u64,
    job: Option<JobId>,
    singles: bool,
    lines: Vec<BatchLine>,
    resolution: BTreeMap<u16, Resolution>,
    #[serde(default)]
    fills: Vec<Vec<crate::asked::Fill>>,
    #[serde(default)]
    exported_fills: BTreeSet<u16>,
}

#[derive(Clone)]
pub(crate) struct Line {
    pub title: String,
    pub detail: String,
}

#[derive(Clone, Default)]
pub(crate) struct View {
    pub notice: String,
    pub lines: Vec<Line>,
}

static VIEW: Mutex<Option<(Context, View)>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) fn view(context: Context) -> View {
    VIEW.lock().ok().and_then(|view| view.as_ref().filter(|(owner, _)| owner.same_session(context)).map(|(_, view)| view.clone())).unwrap_or_default()
}

pub(crate) fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

fn path(folder: &Path, context: Context) -> PathBuf {
    folder.join(format!("requests-{}-{}-{}.json", context.backend.to_hex(), context.account.get(), context.restore))
}

fn read(file: &Path) -> Result<Option<Saved>, String> {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| "Couldn't read this PC's saved batch.".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Couldn't read this PC's saved batch.".into()),
    }
}

fn record(file: &Path, saved: &Saved) -> Result<(), String> {
    let failed = |_| "Couldn't save the batch on this PC.".to_string();
    let bytes = serde_json::to_vec(saved).map_err(failed)?;
    crate::config::write_surely(file, &bytes).map_err(|_| "Couldn't save the batch on this PC.".into())
}

fn show(context: Context, view: View) {
    if !calls::active(context) {
        return;
    }
    if let Ok(mut current) = VIEW.lock() {
        *current = Some((context, view));
    }
    crate::controller::post(|controller| controller.server_batch_changed());
}

/// A new batch is stored before begin. Retrying that begin uses the same request
/// ID and body. A restart may also pick up a batch whose begin already answered.
pub(crate) fn run(connection: Connection, folder: PathBuf, input: Option<(Vec<Want>, bool)>) {
    let Connection::Server { context, client, .. } = &connection else { return };
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let context = *context;
    let client = client.clone();
    std::thread::spawn(move || {
        let file = path(&folder, context);
        let fill_file = super::fill::file(&folder, context);
        let result = run_batch(&connection, context, &client, &file, &fill_file, input);
        RUNNING.store(false, Ordering::Release);
        if let Err(notice) = result {
            let mut last = view(context);
            last.notice = notice;
            show(context, last);
        }
        if !connection.still_selected() {
            crate::controller::post(move |controller| {
                if let Some(connection) = super::current() {
                    run(connection, folder, None);
                }
            });
        }
    });
}

fn run_batch(
    connection: &Connection, context: Context, client: &BackendLidarr<'_>, file: &Path, fill_file: &Path,
    input: Option<(Vec<Want>, bool)>,
) -> Result<(), String> {
    let previous = read(file)?;
    let mut saved = match (previous, input) {
        (Some(saved), None) => saved,
        (None, None) => return Ok(()),
        (previous, Some((wants, singles))) => {
            if let Some(previous) = previous {
                let unfinished = match previous.job {
                    Some(job) => match client.after(job) {
                        Ok(Progress::Working) => true,
                        Ok(_) | Err(RequestError::Refused(Refusal::JobNotFound)) => false,
                        Err(error) => return Err(error.to_string()),
                    },
                    None => true,
                };
                if unfinished {
                    return Err("Your Nexus server is still handling the previous batch.".into());
                }
            }
            let (lines, fills): (Vec<_>, Vec<_>) = wants.into_iter().flat_map(|want| {
                want.songs.into_iter().map(move |song| {
                    let fills = want.fills.iter().filter(|fill| fill.title == song).cloned().collect();
                    (BatchLine { artist: want.artist.clone(), album: want.album.clone(), song }, fills)
                })
            }).unzip();
            if lines.len() > BATCH_MAX_LINES {
                return Err(Refusal::TooLarge.sentence(None).into_owned());
            }
            let request = calls::new_id().map_err(|error| RequestError::from(error).to_string())?.get();
            let batch = Saved {
                request, submitting_session: context.session.get(), job: None, singles, lines, resolution: BTreeMap::new(),
                fills, exported_fills: BTreeSet::new(),
            };
            record(file, &batch)?;
            batch
        }
    };

    if !connection.still_selected() {
        return Ok(());
    }
    let job = match saved.job {
        Some(job) => job,
        None => {
            if saved.submitting_session != context.session.get() {
                std::fs::remove_file(file).map_err(|_| "Couldn't clear the previous sign-in's batch.".to_string())?;
                return Err("The previous sign-in ended before this PC could confirm the batch number.".into());
            }
            let request = RequestId::new(saved.request).ok_or_else(|| "The saved batch number couldn't be read.".to_string())?;
            let change = Change::Batch { singles: saved.singles, lines: saved.lines.clone() };
            match client.begin(request, change) {
                Ok(job) => {
                    saved.job = Some(job);
                    record(file, &saved)?;
                    job
                }
                Err(error @ (RequestError::Transport(_) | RequestError::Refused(Refusal::Busy))) => return Err(error.to_string()),
                Err(error) => {
                    std::fs::remove_file(file).map_err(|_| "Couldn't clear the refused batch on this PC.".to_string())?;
                    return Err(error.to_string());
                }
            }
        }
    };

    while connection.still_selected() {
        let rows = client.batch_lines(job).map_err(|error| error.to_string())?;
        let progress = client.after(job).map_err(|error| error.to_string())?;
        export_fills(file, fill_file, &mut saved, &rows)?;
        let notice = match &progress {
            Progress::Working => "Your Nexus server is working through this batch.".into(),
            Progress::Done(result) => result.sentence().into(),
            Progress::Failed(reason) => reason.sentence(None).into_owned(),
        };
        let lines = rows.iter().filter_map(|row| {
            let input = saved.lines.get(usize::from(row.index))?;
            let detail = match &row.progress {
                BatchLineProgress::Waiting => "Waiting".into(),
                BatchLineProgress::NeedsResolution => "Finding the song's release".into(),
                BatchLineProgress::Found { .. } => "Release found".into(),
                BatchLineProgress::Asked { .. } => "Asked".into(),
                BatchLineProgress::Done { .. } => "On the list".into(),
                BatchLineProgress::NotFound => "Couldn't find a release".into(),
                BatchLineProgress::Failed { reason, .. } => reason.sentence(None).into_owned(),
            };
            Some(Line { title: input.song.clone(), detail: format!("{} · {detail}", input.artist) })
        }).collect();
        show(context, View { notice, lines });
        if progress != Progress::Working {
            return Ok(());
        }
        let submitted_here = saved.submitting_session == context.session.get();
        for row in rows.into_iter().filter(|row| submitted_here && row.progress == BatchLineProgress::NeedsResolution) {
            if !connection.still_selected() {
                return Ok(());
            }
            settle(file, &mut saved, row.index, |line, singles| {
                line.resolve_on_device(crate::musicbrainz::service(), singles).ok().flatten()
            }, |release| client.resolve_batch_line(job, row.index, release))?;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

fn export_fills(file: &Path, fills: &Path, saved: &mut Saved, rows: &[BatchLineView]) -> Result<(), String> {
    let mut exported = Vec::new();
    let mut notes = Vec::new();
    let now = chrono::Utc::now().timestamp();
    for row in rows {
        let album = match &row.progress {
            BatchLineProgress::Asked { album } | BatchLineProgress::Done { album } => album,
            _ => continue,
        };
        let Some(fill) = saved.fills.get(usize::from(row.index)).filter(|fills| !fills.is_empty()) else { continue };
        if saved.exported_fills.contains(&row.index) {
            continue;
        }
        notes.push(crate::asked::Asked {
            album: album.offer.0.as_str().to_owned(), title: album.title.clone(), artist: album.artist.clone(), at: now,
            fills: fill.clone(), ..Default::default()
        });
        exported.push(row.index);
    }
    if exported.is_empty() {
        return Ok(());
    }
    super::fill::remember(fills, notes)?;
    saved.exported_fills.extend(exported);
    record(file, saved)
}

/// Save every irreversible step before starting it. An interrupted lookup
/// becomes an empty result. A lost response can replay that result once.
fn settle(
    file: &Path, saved: &mut Saved, index: u16,
    lookup: impl FnOnce(&BatchLine, bool) -> Option<MusicBrainzId>,
    send: impl FnOnce(Option<MusicBrainzId>) -> Result<(), RequestError>,
) -> Result<(), String> {
    let (release, sends) = match app_batch_step(saved.resolution.get(&index)) {
        AppBatchStep::Wait => return Ok(()),
        AppBatchStep::Send { release, sends } => (release, sends),
        AppBatchStep::Lookup => {
            saved.resolution.insert(index, Resolution::Started);
            record(file, saved)?;
            let line = saved.lines.get(usize::from(index)).ok_or_else(|| RequestError::UnexpectedAnswer.to_string())?;
            (lookup(line, saved.singles), 1)
        }
    };
    saved.resolution.insert(index, Resolution::Ready { release: release.clone(), sends });
    record(file, saved)?;
    match send(release) {
        Ok(()) => {
            saved.resolution.insert(index, Resolution::Accepted);
            record(file, saved)
        }
        Err(RequestError::Transport(_)) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::nexus::requests::TransportError;

    #[test]
    fn interrupted_batch_replays_a_saved_resolution_at_most_once() {
        let number = calls::new_id().unwrap();
        let dir = std::env::temp_dir().join(format!("nexus-batch-{}", number.get()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("batch.json");
        let mut saved = Saved {
            request: number.get(),
            submitting_session: 3,
            job: JobId(std::num::NonZeroU64::new(6).unwrap()).into(),
            singles: true,
            lines: vec![BatchLine { artist: "Artist".into(), album: "".into(), song: "Song".into() }],
            resolution: BTreeMap::new(),
            fills: Vec::new(),
            exported_fills: BTreeSet::new(),
        };
        record(&file, &saved).unwrap();
        let expected = MusicBrainzId::new("cea5d1a3-baac-4978-9ff7-c9ce27576cd5").unwrap();
        settle(&file, &mut saved, 0, |line, singles| {
            assert_eq!(&line.song, "Song");
            assert!(singles);
            let disk = read(&file).unwrap().unwrap();
            assert!(matches!(disk.resolution.get(&0), Some(Resolution::Started)));
            Some(expected.clone())
        }, |result| {
            assert_eq!(result, Some(expected.clone()));
            Err(RequestError::Transport(TransportError::Unreachable))
        }).unwrap();

        let mut saved = read(&file).unwrap().unwrap();
        assert_eq!(saved.job.unwrap().0.get(), 6);
        assert_eq!(saved.request, number.get());
        settle(&file, &mut saved, 0, |_, _| panic!("lookup repeated"), |result| {
            assert_eq!(result, Some(expected));
            Err(RequestError::Transport(TransportError::Unreachable))
        }).unwrap();
        let mut saved = read(&file).unwrap().unwrap();
        settle(&file, &mut saved, 0, |_, _| panic!("lookup repeated"), |_| panic!("too many sends")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
