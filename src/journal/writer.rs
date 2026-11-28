//! The journal's sole writer. This thread owns ordering, retention and recovery.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::config::clear_leftovers;

use super::{Account, Counters, Message, Scope, events, new_id, now_ms, reader::{ChangeRequest, Update, Waiting}};
use events::{EventId, FileTail, JournalFile, KeptFile, Loss, LossCounts, Record};

const RESERVATION: u64 = 64;
const POLL: Duration = Duration::from_millis(150);
const FIRST_RETRY: Duration = Duration::from_millis(500);
const MAX_RETRY: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Retry {
    at: Option<Instant>,
    delay: Option<Duration>,
}

impl Retry {
    fn ready(&self, now: Instant) -> bool {
        self.at.is_none_or(|at| now >= at)
    }

    fn failed(&mut self, now: Instant) {
        let delay = self.delay.map_or(FIRST_RETRY, |delay| (delay * 2).min(MAX_RETRY));
        self.delay = Some(delay);
        self.at = Some(now + delay);
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct State {
    reserved: u64,
    loss: LossCounts,
    damaged: u64,
    unwritten: u64,
    pending: Option<Pending>,
    resending: Option<[u8; 16]>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Pending {
    account: Account,
    change: Change,
}

#[derive(Clone, Deserialize, Serialize)]
enum Change {
    Remove(Vec<u64>),
    Repair(u64),
    Settle(Vec<FilePositions>, Settlement),
}

#[derive(Clone, Deserialize, Serialize)]
struct FilePositions {
    file: u64,
    sequences: Vec<u64>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum Settlement {
    Taken(u64),
    Refused(u64),
}

impl Settlement {
    fn outcome(self) -> events::Outcome {
        match self {
            Self::Taken(_) => events::Outcome::Taken,
            Self::Refused(_) => events::Outcome::Refused,
        }
    }

    fn at_ms(self) -> u64 {
        match self { Self::Taken(at) | Self::Refused(at) => at }
    }
}

type Settled = BTreeMap<u64, Settlement>;

#[derive(Deserialize, Serialize)]
struct Checkpoint {
    life: u64,
    start: EventId,
    position: u32,
}

struct Writer {
    root: PathBuf,
    state: State,
    sequence: u64,
    persisted_drops: u64,
    prior_line_full: u64,
    current: Option<Current>,
    recovered: HashSet<Scope>,
}

struct Current {
    scope: Scope,
    directory: JournalDirectory,
    numbers: Vec<u64>,
    files: Vec<KeptFile>,
    first_waiting: Vec<Option<u64>>,
    can_append: bool,
}

pub(super) struct JournalDirectory(PathBuf);

fn backend_folder(root: &Path, id: [u8; 16]) -> PathBuf {
    let backend = nexus_core::nexus::proto::BackendId::from_bytes(id);
    root.join(backend.to_hex())
}

impl JournalDirectory {
    pub(super) fn new(root: &Path, account: Account) -> Self {
        Self(backend_folder(root, account.backend).join(account.number.to_string()))
    }

    fn journal(&self, number: u64) -> PathBuf {
        self.0.join(format!("{number:020}.journal"))
    }

    pub(super) fn exists(&self, number: u64) -> bool {
        self.journal(number).is_file()
    }

    fn outcomes(&self, number: u64) -> PathBuf {
        self.0.join(format!("{number:020}.settled"))
    }

    fn clear_leftovers(&self) {
        clear_leftovers(&self.0, |name| {
            matches!(name, "position.json" | "life.json") ||
                name.strip_suffix(".settled").or_else(|| name.strip_suffix(".journal"))
                    .is_some_and(|number| parse_number(number).is_some())
        }, &[]);
    }

    fn checkpoint(&self) -> PathBuf {
        self.0.join("position.json")
    }

    fn life(&self) -> PathBuf {
        self.0.join("life.json")
    }

    pub(super) fn numbers(&self) -> io::Result<Vec<u64>> {
        let mut numbers = Vec::new();
        for entry in list(&self.0)? {
            if let Some(number) = file_number(&entry?.path()) {
                numbers.push(number);
            }
        }
        numbers.sort_unstable();
        Ok(numbers)
    }

    pub(super) fn read(&self, number: u64) -> io::Result<(usize, Option<JournalFile>)> {
        let mut bytes = Vec::new();
        match File::open(self.journal(number)) {
            Ok(file) => file.take(events::FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, None)),
            Err(error) => return Err(error),
        };
        Ok((bytes.len(), events::read_file(&bytes).ok()))
    }

    pub(super) fn settled(&self, number: u64) -> io::Result<Settled> {
        Ok(read_json(&self.outcomes(number))?.unwrap_or_default())
    }
}

fn invalid(_: impl core::fmt::Debug) -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    match File::open(path) {
        Ok(file) => file.take(events::FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if bytes.len() > events::FILE_BYTES {
        return Err(invalid(()));
    }
    serde_json::from_slice(&bytes).map(Some).map_err(invalid)
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    crate::config::write_surely(path, &serde_json::to_vec(value).map_err(invalid)?)
}

fn list(path: &Path) -> io::Result<impl Iterator<Item = io::Result<fs::DirEntry>>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries).into_iter().flatten()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None.into_iter().flatten()),
        Err(error) => Err(error),
    }
}

fn parse_number(stem: &str) -> Option<u64> {
    (stem.len() == 20 && stem.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| stem.parse().ok()).flatten().filter(|&number| number != 0)
}

fn file_number(path: &Path) -> Option<u64> {
    (path.extension()?.to_str()? == "journal").then_some(())?;
    parse_number(path.file_stem()?.to_str()?)
}

fn account_folders(path: &Path, backend: [u8; 16]) -> io::Result<Vec<Account>> {
    let mut accounts = Vec::new();
    for entry in list(&backend_folder(path, backend))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(number) = entry.file_name().to_str().and_then(|name| name.parse().ok()) else { continue };
        accounts.push(Account { backend, number });
    }
    Ok(accounts)
}

/// Recover if a course of writes left journal files but no device high water.
fn kept_high_water(root: &Path) -> io::Result<u64> {
    let mut high = 0;
    for entry in list(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(backend) = entry.file_name().to_str().and_then(nexus_core::nexus::proto::BackendId::from_hex) else { continue };
        for account in account_folders(root, *backend.as_bytes())? {
            let directory = JournalDirectory::new(root, account);
            if let Some(last) = directory.numbers()?.last() {
                high = high.max(*last);
                if let Some(file) = directory.read(*last)?.1 {
                    if let Some(sequence) = file.events.iter().map(|row| row.sequence()).max() {
                        high = high.max(sequence);
                    }
                }
            }
        }
    }
    Ok(high)
}

fn remove(path: &Path) -> io::Result<()> {
    fs::remove_file(path).or_else(|error| {
        if error.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(error) }
    })
}

fn retention(directory: &JournalDirectory, life: u64) -> io::Result<events::RemoveFiles> {
    let mut files = Vec::new();
    for number in directory.numbers()? {
        let (bytes, file) = directory.read(number)?;
        let outcomes = directory.settled(number)?;
        let (life, count, pending) = file.map_or((life, 0, 0), |file| {
            let pending = file.events.iter().filter(|event| !outcomes.contains_key(&event.sequence())).count();
            (file.life, file.events.len(), pending)
        });
        files.push(KeptFile {
            life, bytes: bytes as u64, events: count as u64, pending: pending as u64,
            final_at_ms: outcomes.values().map(|settled| settled.at_ms()).max(),
        });
    }
    Ok(events::remove_files(&files, life, now_ms()))
}

impl Writer {
    fn open(root: PathBuf) -> io::Result<Self> {
        clear_leftovers(&root, |name| name == "state.json", &[]);
        let path = root.join("state.json");
        let mut state: State = match read_json(&path) {
            Ok(state) => state.unwrap_or_default(),
            Err(error) if error.kind() == io::ErrorKind::InvalidData && crate::config::set_aside(&path).is_some() => State::default(),
            Err(error) => return Err(error),
        };
        if state.reserved == 0 {
            state.reserved = kept_high_water(&root)?;
        }
        let mut writer = Self {
            root, sequence: state.reserved, prior_line_full: state.loss.line_full, state, persisted_drops: 0,
            current: None, recovered: HashSet::new(),
        };
        writer.complete_change()?;
        writer.complete_resend()?;
        Ok(writer)
    }

    fn save(&mut self, state: State) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        write_json(&self.root.join("state.json"), &state)?;
        self.state = state;
        Ok(())
    }

    fn complete_change(&mut self) -> io::Result<()> {
        let Some(pending) = &self.state.pending else { return Ok(()) };
        let directory = JournalDirectory::new(&self.root, pending.account);
        match &pending.change {
            Change::Remove(files) => {
                for number in files {
                    remove(&directory.journal(*number))?;
                    remove(&directory.outcomes(*number))?;
                }
            }
            Change::Repair(number) => {
                let path = directory.journal(*number);
                if let Some(file) = directory.read(*number)?.1 {
                    if file.skipped_lines != 0 {
                        let mut bytes = events::file_header(file.life).map_err(invalid)?;
                        for event in file.events {
                            bytes.extend(event.line().map_err(invalid)?);
                        }
                        crate::config::write_surely(&path, &bytes)?;
                    }
                }
            }
            Change::Settle(files, settlement) => {
                for change in files {
                    let mut settled = directory.settled(change.file)?;
                    for sequence in &change.sequences {
                        settled.entry(*sequence).or_insert(*settlement);
                    }
                    write_json(&directory.outcomes(change.file), &settled)?;
                }
            }
        }
        self.save(State { pending: None, ..self.state.clone() })
    }

    fn change(&mut self, account: Account, change: Change, loss: Option<(Loss, u64)>, damaged: u64) -> io::Result<()> {
        self.complete_change()?;
        let mut state = self.state.clone();
        if let Some((kind, count)) = loss {
            state.loss.add(kind, count);
        }
        state.damaged = state.damaged.saturating_add(damaged);
        state.pending = Some(Pending { account, change });
        self.save(state)?;
        self.complete_change()
    }

    fn next_sequence(&mut self) -> io::Result<u64> {
        let sequence = events::next_sequence(self.sequence).ok_or_else(|| invalid(()))?;
        if sequence > self.state.reserved {
            // Reserve before using any number, even if the last event is later pruned.
            self.save(State {
                reserved: sequence.saturating_add(RESERVATION - 1),
                ..self.state.clone()
            })?;
        }
        self.sequence = sequence;
        Ok(sequence)
    }

    fn activate(&mut self, scope: Scope) -> io::Result<()> {
        self.complete_change()?;
        let directory = JournalDirectory::new(&self.root, scope.account);
        fs::create_dir_all(&directory.0)?;
        let prior_life: Option<u64> = read_json(&directory.life())?;
        if prior_life.is_some_and(|life| life > scope.life) {
            return Err(invalid(()));
        }
        if prior_life != Some(scope.life) {
            if prior_life.is_some() {
                remove(&directory.checkpoint())?;
            }
            write_json(&directory.life(), &scope.life)?;
        }
        let mut current = Current {
            scope, directory, numbers: Vec::new(), files: Vec::new(),
            first_waiting: Vec::new(), can_append: false,
        };
        let recover = !self.recovered.contains(&scope);
        if recover {
            current.directory.clear_leftovers();
        }
        let mut recover_files = Vec::new();
        for number in current.directory.numbers()? {
            let (mut bytes, file) = current.directory.read(number)?;
            let Some(file) = file else {
                self.change(scope.account, Change::Remove(vec![number]), None, 1)?;
                continue;
            };
            if file.skipped_lines != 0 {
                self.change(scope.account, Change::Repair(number), None, file.skipped_lines as u64)?;
                bytes = current.directory.journal(number).metadata()?.len() as usize;
            }
            let outcomes = current.directory.settled(number)?;
            let mut waiting = file.events.iter().filter(|event| !outcomes.contains_key(&event.sequence()));
            let first = waiting.next().map(|event| event.sequence());
            let pending = first.map_or(0, |_| 1 + waiting.count() as u64);
            let final_at_ms = outcomes.values().map(|settled| settled.at_ms()).max();
            current.numbers.push(number);
            current.first_waiting.push(first);
            current.files.push(KeptFile {
                life: file.life, bytes: bytes as u64, events: file.events.len() as u64,
                pending, final_at_ms,
            });
            if recover && file.life == scope.life {
                recover_files.push(file);
            }
        }
        // Compute recovery before pruning, so removing an old file containing a stop
        // cannot turn a previously closed start into an open one.
        let open = events::open_starts(recover_files.iter().flat_map(|file| &file.events));
        let position = read_json::<Checkpoint>(&current.directory.checkpoint()).ok().flatten()
            .filter(|saved| saved.life == scope.life)
            .map(|saved| (saved.start, saved.position));
        self.prune(&mut current)?;
        for start in open {
            let id = new_id().ok_or_else(|| io::Error::other("journal id unavailable"))?;
            let stop = events::end_open_start(start, position, id, now_ms()).map_err(invalid)?;
            if !self.append(&mut current, stop)? {
                return Err(io::Error::other("could not confirm the recovery append"));
            }
            self.prune(&mut current)?;
        }
        self.recovered.insert(scope);
        self.current = Some(current);
        Ok(())
    }

    // An error before a line reaches the file is a loss. If an append reached the
    // file but could not be confirmed, the next scan counts a torn line or keeps
    // a whole one. Counting it here as well would double-count a torn write.
    fn append(&mut self, current: &mut Current, record: Record) -> io::Result<bool> {
        let sequence = self.next_sequence()?;
        let at = record.at_ms();
        let event = record.place(sequence).map_err(invalid)?;
        let tail = current.can_append.then(|| current.files.last()).flatten().map(|file| FileTail {
            life: file.life, bytes: file.bytes as usize, last_byte: b'\n',
        });
        let append = events::append(current.scope.life, tail, &event).map_err(invalid)?;
        if append.new_file {
            crate::config::write_whole(
                &current.directory.journal(sequence), &append.bytes,
                crate::config::Durable::Flushed, &crate::config::ONCE,
            )?;
            current.numbers.push(sequence);
            current.first_waiting.push(Some(sequence));
            current.files.push(KeptFile {
                life: current.scope.life, bytes: append.bytes.len() as u64, events: 1,
                pending: 1, final_at_ms: Some(at),
            });
            current.can_append = true;
        } else if let (Some(&number), Some(file)) = (current.numbers.last(), current.files.last_mut()) {
            let path = current.directory.journal(number);
            let started = Instant::now();
            let mut output = OpenOptions::new().append(true).open(&path)?;
            if let Err(error) = output.write_all(&append.bytes) {
                return if output.metadata().is_ok_and(|metadata| metadata.len() == file.bytes) {
                    Err(error)
                } else {
                    Ok(false)
                };
            }
            if output.sync_data().is_err() {
                return Ok(false);
            }
            crate::config::flushed(&path, started);
            if let Some(first) = current.first_waiting.last_mut() {
                first.get_or_insert(sequence);
            }
            file.bytes += append.bytes.len() as u64;
            file.events += 1;
            file.pending += 1;
            file.final_at_ms = Some(at);
        }
        Ok(true)
    }

    fn prune(&mut self, current: &mut Current) -> io::Result<()> {
        let plan = events::remove_files(&current.files, current.scope.life, now_ms());
        if !plan.remove.is_empty() {
            let files = plan.remove.iter().map(|&index| current.numbers[index]).collect();
            self.change(current.scope.account, Change::Remove(files), Some((Loss::FilesFull, plan.dropped)), 0)?;
            for index in plan.remove.into_iter().rev() {
                if index + 1 == current.files.len() {
                    current.can_append = false;
                }
                current.files.remove(index);
                current.numbers.remove(index);
                current.first_waiting.remove(index);
            }
        }
        Ok(())
    }

    fn handle(&mut self, message: Message) -> io::Result<()> {
        self.complete_change()?;
        self.complete_resend()?;
        match message {
            Message::Event(scope, observation) => {
                let new_scope = self.current.as_ref().is_none_or(|current| current.scope != scope);
                if new_scope && !self.activate_current_life(scope)? {
                    return Ok(());
                }
                let Some(mut current) = self.current.take() else { return Err(invalid(())) };
                let record = Record::new(observation.id, observation.at, observation.kind).map_err(invalid);
                let result = record.and_then(|record| self.append(&mut current, record));
                // On an uncertain write, rescan rather than append after a torn line.
                if matches!(result, Ok(true)) && self.prune(&mut current).is_ok() {
                    self.current = Some(current);
                }
                result.map(drop)
            }
            Message::Wake => Ok(()),
            Message::Checkpoint(scope, start, position) => {
                if let Some(current) = self.current.as_ref().filter(|current| current.scope == scope) {
                    write_json(&current.directory.checkpoint(), &Checkpoint {
                        life: scope.life, start, position,
                    })?;
                }
                Ok(())
            }
        }
    }

    fn activate_current_life(&mut self, scope: Scope) -> io::Result<bool> {
        if self.current.as_ref().is_some_and(|current| current.scope == scope) {
            return Ok(true);
        }
        let directory = JournalDirectory::new(&self.root, scope.account);
        let known_life: Option<u64> = read_json(&directory.life())?;
        if known_life.is_some_and(|life| life > scope.life) {
            return Ok(false);
        }
        self.activate(scope)?;
        Ok(true)
    }

    fn recovery_pending(&self) -> bool {
        self.state.pending.is_some() || self.state.resending.is_some()
    }

    fn waiting(&self) -> Option<Waiting> {
        let current = self.current.as_ref()?;
        Some(Waiting {
            scope: current.scope,
            pending: current.files.iter().map(|file| file.pending).sum(),
            from: current.first_waiting.iter().filter_map(|first| *first).next().unwrap_or_default(),
        })
    }

    fn update(&mut self, update: &Update) -> io::Result<()> {
        self.complete_change()?;
        self.complete_resend()?;
        match *update {
            Update::ScopeChanged(_) => {}
            Update::LifeMoved(scope) => {
                let directory = JournalDirectory::new(&self.root, scope.account);
                let prior: Option<u64> = read_json(&directory.life())?;
                if prior.is_none_or(|life| life < scope.life) {
                    self.activate(scope)?;
                }
            }
            Update::Settle(scope, ref ids, outcome) => self.settle(scope, ids, outcome)?,
            Update::ResendAll(backend) => {
                self.complete_resend()?;
                self.save(State { resending: Some(backend), ..self.state.clone() })?;
                self.complete_resend()?;
            }
        }
        Ok(())
    }

    fn settle(&mut self, scope: Scope, ids: &[EventId], outcome: Settlement) -> io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let directory = JournalDirectory::new(&self.root, scope.account);
        let kept_life: Option<u64> = read_json(&directory.life())?;
        if kept_life != Some(scope.life) {
            return Ok(());
        }
        let mut ids: HashSet<_> = ids.iter().copied().collect();
        let mut files = Vec::new();
        let mut summaries = Vec::new();
        let mut newly_settled = 0;
        for number in directory.numbers()? {
            let Some(file) = directory.read(number)?.1.filter(|file| file.life == scope.life) else { continue };
            let mut settled = directory.settled(number)?;
            let sequences: Vec<_> = file.events.iter().filter_map(|event| {
                if !ids.remove(&event.record().id()) || settled.contains_key(&event.sequence()) {
                    return None;
                }
                let sequence = event.sequence();
                settled.insert(sequence, outcome);
                Some(sequence)
            }).collect();
            if !sequences.is_empty() {
                newly_settled += sequences.len() as u64;
                files.push(FilePositions { file: number, sequences });
                let mut waiting = file.events.iter().filter(|event| !settled.contains_key(&event.sequence()));
                let first = waiting.next().map(|event| event.sequence());
                let pending = first.map_or(0, |_| 1 + waiting.count() as u64);
                let final_at = settled.values().map(|status| status.at_ms()).max();
                summaries.push((number, first, pending, final_at));
            }
            if ids.is_empty() {
                break;
            }
        }
        if newly_settled != 0 {
            let loss = (outcome.outcome() == events::Outcome::Refused).then_some((Loss::Refused, newly_settled));
            let mut prior = self.current.take();
            self.change(scope.account, Change::Settle(files, outcome), loss, 0)?;
            if let Some(current) = prior.as_mut().filter(|current| current.scope == scope) {
                for (number, first, pending, final_at) in summaries {
                    if let Ok(index) = current.numbers.binary_search(&number) {
                        current.first_waiting[index] = first;
                        current.files[index].pending = pending;
                        current.files[index].final_at_ms = final_at;
                    }
                }
                self.prune(current)?;
            } else {
                let plan = retention(&directory, scope.life)?;
                self.remove_files(scope, &directory, plan)?;
            }
            self.current = prior;
        }
        Ok(())
    }

    fn complete_resend(&mut self) -> io::Result<()> {
        let Some(backend) = self.state.resending else { return Ok(()) };
        let active = self.current.take().map(|current| current.scope);
        for account in account_folders(&self.root, backend)? {
            let directory = JournalDirectory::new(&self.root, account);
            for file in directory.numbers()? {
                let mut settled = directory.settled(file)?;
                let old = settled.len();
                settled.retain(|_, status| !status.outcome().should_send(true));
                if settled.len() != old {
                    write_json(&directory.outcomes(file), &settled)?;
                }
            }
            // Use core's cap for every account, including ones not signed in.
            if let Some(life) = read_json(&directory.life())? {
                let scope = Scope { account, life };
                let plan = retention(&directory, life)?;
                self.remove_files(scope, &directory, plan)?;
            }
        }
        self.save(State { resending: None, ..self.state.clone() })?;
        if let Some(scope) = active {
            self.activate(scope)?;
        }
        Ok(())
    }

    fn remove_files(
        &mut self, scope: Scope, directory: &JournalDirectory, plan: events::RemoveFiles,
    ) -> io::Result<()> {
        if !plan.remove.is_empty() {
            let numbers = directory.numbers()?;
            let paths = plan.remove.iter().filter_map(|&index| numbers.get(index).copied()).collect();
            self.change(scope.account, Change::Remove(paths), Some((Loss::FilesFull, plan.dropped)), 0)?;
        }
        Ok(())
    }

    fn keep_losses(&mut self, counters: &Counters) -> io::Result<()> {
        let seen = counters.dropped.load(Ordering::Relaxed);
        let lost = seen.saturating_sub(self.persisted_drops);
        if lost != 0 {
            let mut state = self.state.clone();
            state.loss.add(Loss::LineFull, lost);
            self.save(state)?;
            self.persisted_drops = seen;
        }
        Ok(())
    }

    fn publish_losses(&self, counters: &Counters) {
        // dropped already includes losses made during this run, even if they have
        // since reached disk. Include the historical split-out write failures, too.
        counters.line_full.store(self.prior_line_full.saturating_add(self.state.unwritten)
            .saturating_add(self.state.damaged), Ordering::Relaxed);
        counters.files_full.store(self.state.loss.files_full, Ordering::Relaxed);
        counters.refused.store(self.state.loss.refused, Ordering::Relaxed);
    }
}

struct QueuedChange {
    request: ChangeRequest,
    retry: Retry,
}

fn changes_overlap(first: &Update, second: &Update) -> bool {
    fn target(update: &Update) -> Option<([u8; 16], Option<u64>)> {
        match *update {
            Update::Settle(scope, ..) | Update::LifeMoved(scope) =>
                Some((scope.account.backend, Some(scope.account.number))),
            Update::ResendAll(backend) => Some((backend, None)),
            Update::ScopeChanged(_) => None,
        }
    }

    match (target(first), target(second)) {
        (Some((a, first)), Some((b, second))) => {
            a == b && (first.is_none() || second.is_none() || first == second)
        }
        _ => false,
    }
}

fn apply_changes(writer: &mut Writer, queue: &mut VecDeque<QueuedChange>, now: Instant) -> bool {
    let mut blocked = VecDeque::<QueuedChange>::new();
    let mut changed = false;
    while let Some(mut next) = queue.pop_front() {
        if !writer.recovery_pending() && next.retry.ready(now) && !blocked.iter()
            .any(|earlier| changes_overlap(&earlier.request.update, &next.request.update))
        {
            if writer.update(&next.request.update).is_ok() {
                if let Some(finished) = next.request.finished {
                    let _ = finished.send(());
                }
                changed = true;
                continue;
            }
            next.retry.failed(now);
        }
        blocked.push_back(next);
    }
    *queue = blocked;
    changed
}

fn retry_recording(recording: &mut Option<(Scope, Retry)>, scope: Scope) -> &mut (Scope, Retry) {
    if recording.as_ref().is_none_or(|(account, _)| *account != scope) {
        *recording = Some((scope, Retry::default()));
    }
    recording.as_mut().expect("the scope was just set")
}

pub(super) fn run(
    root: PathBuf,
    line: Receiver<Message>,
    updates: Receiver<ChangeRequest>,
    current_waiting: Arc<Mutex<Option<Waiting>>>,
    counters: &Counters,
) {
    let mut writer = Writer::open(root.clone()).ok();
    let mut opening = Retry::default();
    if writer.is_none() {
        opening.failed(Instant::now());
    }
    let mut recover = Retry::default();
    let mut recording = None::<(Scope, Retry)>;
    let mut totals = Retry::default();
    let mut last_retention = Instant::now();
    let mut last_notice = None::<Instant>;
    let mut visible = None::<Scope>;
    let mut dirty = false;
    let mut queued_updates = VecDeque::new();
    let mut message = Ok(Message::Wake);

    loop {
        let now = Instant::now();
        let closing = counters.closing.load(Ordering::Acquire);
        let done = matches!(message, Err(RecvTimeoutError::Disconnected))
            || (closing && matches!(message, Err(RecvTimeoutError::Timeout)));
        for request in updates.try_iter() {
            if let Update::ScopeChanged(scope) = request.update {
                visible = scope;
                dirty = true;
                if let Ok(mut waiting) = current_waiting.lock() {
                    *waiting = None;
                }
            } else {
                queued_updates.push_back(QueuedChange { request, retry: Retry::default() });
            }
        }

        let unkept = |writer: Option<&Writer>| {
            counters.dropped.load(Ordering::Relaxed) > writer.map_or(0, |writer| writer.persisted_drops)
        };
        if writer.is_none() && opening.ready(now) && (visible.is_some() || unkept(None)
            || !queued_updates.is_empty() || matches!(message, Ok(Message::Event(..))))
        {
            writer = Writer::open(root.clone()).ok();
            if writer.is_none() {
                opening.failed(Instant::now());
            }
        }

        if let Some(writer) = &mut writer {
            if writer.recovery_pending() && recover.ready(now) {
                if writer.complete_change().and_then(|_| writer.complete_resend()).is_ok() {
                    recover.clear();
                    dirty = true;
                } else {
                    recover.failed(Instant::now());
                }
            }
            if !writer.recovery_pending() {
                dirty |= apply_changes(writer, &mut queued_updates, now);
                if writer.recovery_pending() {
                    recover.failed(Instant::now());
                }
            }

            if let Ok(message @ (Message::Event(scope, _) | Message::Checkpoint(scope, ..))) = message {
                let event = matches!(message, Message::Event(..));
                let ready = recording.as_ref().is_none_or(|(account, retry)| *account != scope || retry.ready(now));
                if writer.recovery_pending() || !ready {
                    if event {
                        counters.drop_event();
                    }
                } else if writer.handle(message).is_err() {
                    if event {
                        counters.drop_event();
                    }
                    let (_, retry) = retry_recording(&mut recording, scope);
                    retry.failed(Instant::now());
                    if writer.recovery_pending() {
                        recover.failed(Instant::now());
                    }
                } else {
                    recording = recording.filter(|(account, _)| *account != scope);
                }
                dirty |= event;
            }

            if !writer.recovery_pending() && visible.is_some() && last_retention.elapsed() >= Duration::from_secs(60) {
                if let Some(mut current) = writer.current.take()
                    && writer.prune(&mut current).is_ok()
                {
                    writer.current = Some(current);
                }
                if writer.recovery_pending() {
                    recover.failed(Instant::now());
                }
                last_retention = Instant::now();
                dirty = true;
            }
            if unkept(Some(writer)) && (done || totals.ready(now)) {
                if writer.keep_losses(counters).is_ok() {
                    totals.clear();
                } else {
                    totals.failed(Instant::now());
                }
            }

            let notice_due = dirty && (done || last_notice.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)));
            if notice_due && !writer.recovery_pending() {
                if let Some(scope) = visible {
                    let ready = recording.as_ref().is_none_or(|(account, retry)| *account != scope || retry.ready(now));
                    if ready {
                        if writer.activate_current_life(scope).is_err() {
                            let (_, retry) = retry_recording(&mut recording, scope);
                            retry.failed(Instant::now());
                            if writer.recovery_pending() {
                                recover.failed(Instant::now());
                            }
                        } else {
                            recording = recording.filter(|(account, _)| *account != scope);
                            dirty = false;
                        }
                    }
                } else {
                    dirty = false;
                }
                if let Ok(mut waiting) = current_waiting.lock() {
                    *waiting = writer.waiting().filter(|waiting| Some(waiting.scope) == visible);
                }
                last_notice = Some(Instant::now());
            }
            writer.publish_losses(counters);
        } else if matches!(message, Ok(Message::Event(..))) {
            counters.drop_event();
        }

        if done {
            break;
        }
        message = if closing {
            line.try_recv().map_err(|_| RecvTimeoutError::Timeout)
        } else if visible.is_some() || !queued_updates.is_empty() || unkept(writer.as_ref())
            || writer.as_ref().is_some_and(Writer::recovery_pending)
        {
            line.recv_timeout(POLL)
        } else {
            line.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Observation, try_observe};
    use crate::journal::reader::Read;
    use events::{Began, Kind, PlayedFrom, Sound, Stopped, DeviceKind};
    use std::sync::mpsc;

    struct Folder(PathBuf);

    impl Folder {
        fn new() -> Self {
            Self(std::env::temp_dir().join(crate::config::temp_for(Path::new("nexus-listening"))))
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scope(backend: u8, number: u64, life: u64) -> Scope {
        Scope { account: Account { backend: [backend; 16], number }, life }
    }

    fn observation(id: u8, kind: Kind) -> Observation {
        Observation { id: EventId::from_bytes([id; 16]), at: 100, kind }
    }

    fn open(id: u8) -> Observation {
        observation(id, Kind::Open {
            version: nexus_core::nexus::proto::Version::parse("0.40.13").unwrap(),
            device: DeviceKind::Pc,
        })
    }

    fn start(id: u8) -> Observation {
        observation(id, Kind::Start {
            song_id: format!("fictional-song-{id}"), title: "Paper Moons".into(),
            artist: "The Quiet Satellites".into(), album: "Gold Telescope".into(),
            from: PlayedFrom::Playlist { id: "fictional-list".into() },
            began: Began::PressedHere, sound: Sound::Download, position_ms: 0,
        })
    }

    fn read(root: &Path, scope: Scope, from: u64, max_events: usize, max_bytes: usize) -> Vec<events::Placed> {
        let (send, receive) = mpsc::channel();
        Read {
            scope, from, max_events, max_bytes, answered: Box::new(move |rows| send.send(rows).unwrap()),
        }.run(root);
        receive.recv().unwrap()
    }

    #[test]
    fn a_full_line_skips_without_a_reader_and_persists_the_exact_count() {
        let folder = Folder::new();
        let (line, receive) = mpsc::sync_channel(1);
        let counters = Counters::default();
        let scope = scope(7, 1, 1);
        crate::config::mark_window_thread();
        let before = crate::config::window_flushes();
        assert!(try_observe(&line, &counters, scope, open(1)));
        // The receiver never runs. A blocking send would hang on the first overflow.
        assert!(!try_observe(&line, &counters, scope, open(2)));
        assert!(!try_observe(&line, &counters, scope, open(3)));
        assert_eq!(crate::config::window_flushes(), before);
        assert_eq!(counters.losses().line_full, 2);
        let root = folder.0.clone();
        std::thread::spawn(move || {
            let mut writer = Writer::open(root.clone()).unwrap();
            writer.handle(receive.try_recv().unwrap()).unwrap();
            writer.keep_losses(&counters).unwrap();
            writer.keep_losses(&counters).unwrap();
            writer.publish_losses(&counters);
            assert_eq!(counters.losses().line_full, 2);
            assert_eq!(Writer::open(root).unwrap().state.loss.line_full, 2);
        }).join().unwrap();
    }

    #[test]
    fn a_dead_writer_counts_each_later_event_once() {
        let (line, receive) = mpsc::sync_channel(1);
        let counters = Counters::default();
        drop(receive);
        for id in 1..=3 {
            assert!(!try_observe(&line, &counters, scope(7, 1, 1), open(id)));
            assert_eq!(counters.losses().line_full, u64::from(id));
        }
    }

    fn run_three_events(root: PathBuf) -> LossCounts {
        let (send, line) = mpsc::sync_channel(3);
        let (_update, updates) = mpsc::channel();
        let counters = Counters::default();
        for id in 1..=3 {
            assert!(try_observe(&send, &counters, scope(7, 1, 1), open(id)));
        }
        drop(send);
        run(root, line, updates, Arc::default(), &counters);
        counters.losses()
    }

    #[test]
    fn a_journal_that_never_opens_still_counts_losses() {
        let folder = Folder::new();
        fs::create_dir_all(&folder.0).unwrap();
        let root = folder.0.join("this-is-a-file");
        fs::write(&root, "").unwrap();
        assert!(Writer::open(root.clone()).is_err());
        assert_eq!(run_three_events(root), LossCounts { line_full: 3, ..LossCounts::default() });
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_profile_counts_each_failed_write() {
        use std::os::unix::fs::PermissionsExt;
        let folder = Folder::new();
        fs::create_dir_all(&folder.0).unwrap();
        fs::set_permissions(&folder.0, fs::Permissions::from_mode(0o555)).unwrap();
        // A privileged test runner can bypass the permission bits. In that case
        // make the account directory impossible to create for the same write path.
        let probe = folder.0.join("permission-check");
        if fs::write(&probe, "").is_ok() {
            fs::remove_file(probe).unwrap();
            let account = JournalDirectory::new(&folder.0, scope(7, 1, 1).account);
            fs::create_dir_all(account.0.parent().unwrap()).unwrap();
            fs::write(account.0, "").unwrap();
        }
        assert!(Writer::open(folder.0.clone()).is_ok());
        let losses = run_three_events(folder.0.clone());
        fs::set_permissions(&folder.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(losses, LossCounts { line_full: 3, ..LossCounts::default() });
    }

    #[test]
    fn a_torn_line_is_counted_once_and_only_the_matching_play_gets_a_position() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let first = start(1);
        let first_id = first.id;
        let second = start(2);
        let second_id = second.id;
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.handle(Message::Event(scope, first)).unwrap();
        writer.handle(Message::Event(scope, second)).unwrap();
        writer.handle(Message::Checkpoint(scope, second_id, 42_000)).unwrap();
        drop(writer);
        let directory = JournalDirectory::new(&folder.0, scope.account);
        let path = directory.journal(1);
        let valid = fs::read(&path).unwrap();
        // Even a correctly encoded record without the trailing LF is a torn write.
        let tail = open(3);
        let tail_id = tail.id;
        let event = Record::new(tail.id, tail.at, tail.kind).unwrap().place(3).unwrap().line().unwrap();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(&event[..event.len() - 1]).unwrap();

        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.activate(scope).unwrap();
        assert_eq!(fs::read(&path).unwrap(), valid);
        let rows = read(&folder.0, scope, 0, events::BATCH_EVENTS, events::BATCH_BYTES);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].record().id(), first_id);
        assert_eq!(rows[1].record().id(), second_id);
        assert!(rows.iter().all(|row| row.record().id() != tail_id));
        assert!(matches!(rows[2].record().kind(), Kind::Stop { start, position_ms: None, why: Stopped::Unknown, .. } if *start == first_id));
        assert!(matches!(rows[3].record().kind(), Kind::Stop { start, position_ms: Some(42_000), why: Stopped::Unknown, .. } if *start == second_id));
        assert!(rows[2].sequence() > 3);
        assert_eq!(writer.state.damaged, 1);
        let counters = Counters::default();
        writer.publish_losses(&counters);
        assert_eq!(counters.losses().line_full, 1);
        let mut again = Writer::open(folder.0.clone()).unwrap();
        again.activate(scope).unwrap();
        assert_eq!(read(&folder.0, scope, 0, 10, usize::MAX).len(), 4);
        assert_eq!(again.state.damaged, 1);
        again.publish_losses(&counters);
        assert_eq!(counters.losses().line_full, 1);
    }

    #[test]
    fn folders_and_lives_are_fenced_and_sequences_are_device_wide() {
        let folder = Folder::new();
        let first = scope(7, 1, 1);
        let new_life = scope(7, 1, 2);
        let another_account = scope(7, 2, 1);
        let another_backend = scope(8, 1, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        for (id, scope) in [first, another_account, another_backend].into_iter().enumerate() {
            writer.handle(Message::Event(scope, open(id as u8 + 1))).unwrap();
        }
        for (at, scope) in [first, another_account, another_backend].into_iter().enumerate() {
            let rows = read(&folder.0, scope, 0, 8, usize::MAX);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].sequence(), at as u64 + 1);
        }
        writer.update(&Update::LifeMoved(new_life)).unwrap();
        writer.handle(Message::Event(first, open(4))).unwrap();
        writer.handle(Message::Event(new_life, open(5))).unwrap();
        assert!(read(&folder.0, first, 0, 8, usize::MAX).is_empty());
        let rows = read(&folder.0, new_life, 0, 8, usize::MAX);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sequence(), 4);
        assert_eq!(writer.state.loss, LossCounts::default());
    }

    #[test]
    fn reader_bounds_and_terminal_outcomes_survive_restart_and_resend() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        let ids: Vec<_> = (1..=3).map(|id| {
            let event = open(id);
            let id = event.id;
            writer.handle(Message::Event(scope, event)).unwrap();
            id
        }).collect();
        let rows = read(&folder.0, scope, 2, 1, usize::MAX);
        assert_eq!(rows[0].record().id(), ids[1]);
        assert!(read(&folder.0, scope, 1, 3, 1).is_empty());
        writer.update(&Update::Settle(scope, vec![ids[0]], Settlement::Taken(now_ms()))).unwrap();
        for _ in 0..2 {
            writer.update(&Update::Settle(scope, vec![ids[1]], Settlement::Refused(now_ms()))).unwrap();
        }
        assert_eq!(writer.state.loss.refused, 1);
        assert_eq!(read(&folder.0, scope, 0, 3, usize::MAX)[0].record().id(), ids[2]);
        writer.update(&Update::ResendAll(scope.account.backend)).unwrap();
        let again = Writer::open(folder.0.clone()).unwrap();
        assert_eq!(again.state.loss.refused, 1);
        let rows = read(&folder.0, scope, 0, 3, usize::MAX);
        assert_eq!(rows.iter().map(|row| row.record().id()).collect::<Vec<_>>(), [ids[0], ids[2]]);
    }

    #[test]
    fn a_mutation_receipt_follows_its_persisted_result() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        let event = open(1);
        let id = event.id;
        writer.handle(Message::Event(scope, event)).unwrap();
        writer.update(&Update::Settle(scope, vec![id], Settlement::Taken(now_ms()))).unwrap();
        assert!(read(&folder.0, scope, 0, 1, usize::MAX).is_empty());
        drop(writer);

        let (sender, line) = mpsc::sync_channel(1);
        let (update, updates) = mpsc::channel();
        let (finished, receipt) = mpsc::channel();
        let root = folder.0.clone();
        let waiting = Arc::new(Mutex::new(None));
        let counters = Arc::new(Counters::default());
        let worker = counters.clone();
        update.send(ChangeRequest {
            update: Update::ResendAll(scope.account.backend), finished: Some(finished),
        }).unwrap();
        let thread = std::thread::spawn(move || run(root, line, updates, waiting, &worker));
        let result = receipt.recv_timeout(Duration::from_secs(5));
        let rows = read(&folder.0, scope, 0, 1, usize::MAX);
        drop(sender);
        thread.join().unwrap();
        result.unwrap();
        assert_eq!(rows[0].record().id(), id);
    }

    #[test]
    fn a_kill_keeps_flushed_events_and_discards_a_torn_append() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        const CHILD_HOME: &str = "NEXUS_W8_CRASH_TEST_CHILD_HOME";
        const READY: &str = "journal crash fixture ready";
        let scope = scope(7, 1, 1);
        let start_id = EventId::from_bytes([1; 16]);
        if let Some(root) = std::env::var_os(CHILD_HOME) {
            let root = PathBuf::from(root);
            let mut writer = Writer::open(root.clone()).unwrap();
            writer.handle(Message::Event(scope, start(1))).unwrap();
            writer.handle(Message::Checkpoint(scope, start_id, 1_250)).unwrap();
            let directory = JournalDirectory::new(&root, scope.account);
            let mut file = OpenOptions::new().append(true).open(directory.journal(1)).unwrap();
            file.write_all(b"{\"id\":").unwrap();
            eprintln!("{READY}");
            // Keep the file open at the point where a real append could have been cut.
            let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
            return;
        }

        let folder = Folder::new();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "journal::writer::tests::a_kill_keeps_flushed_events_and_discards_a_torn_append", "--nocapture"])
            .env(CHILD_HOME, &folder.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn().unwrap();
        let ready = BufReader::new(child.stderr.take().unwrap()).lines()
            .map_while(Result::ok).any(|line| line == READY);
        let killed = child.kill();
        let status = child.wait().unwrap();
        assert!(ready);
        killed.unwrap();
        assert!(!status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9));
        }

        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.activate(scope).unwrap();
        let events = read(&folder.0, scope, 0, 5, usize::MAX);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].record().id(), start_id);
        assert!(matches!(events[1].record().kind(), Kind::Stop { start, position_ms: Some(1_250), why: Stopped::Unknown, .. } if *start == start_id));
        assert_eq!(writer.state.damaged, 1);
    }

    #[test]
    fn a_failed_change_backs_off_without_blocking_another_account() {
        let folder = Folder::new();
        let first = scope(7, 1, 1);
        let other = scope(7, 2, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        for (scope, id) in [(first, 1), (other, 2)] {
            writer.handle(Message::Event(scope, open(id))).unwrap();
        }
        let path = JournalDirectory::new(&folder.0, first.account).outcomes(1);
        fs::write(&path, b"bad sidecar").unwrap();
        let mut queue = VecDeque::new();
        let mut receipts = Vec::new();
        for (scope, id) in [(first, 1), (first, 1), (other, 2)] {
            let (finished, receipt) = mpsc::channel();
            let update = Update::Settle(scope, vec![open(id).id], Settlement::Taken(now_ms()));
            queue.push_back(QueuedChange {
                request: ChangeRequest { update, finished: Some(finished) }, retry: Retry::default(),
            });
            receipts.push(receipt);
        }
        let now = Instant::now();
        assert!(apply_changes(&mut writer, &mut queue, now));
        assert_eq!(queue.len(), 2);
        assert!(receipts[2].try_recv().is_ok());
        assert!(receipts[0].try_recv().is_err() && receipts[1].try_recv().is_err());

        fs::remove_file(path).unwrap();
        assert!(!apply_changes(&mut writer, &mut queue, now + FIRST_RETRY - Duration::from_millis(1)));
        assert!(apply_changes(&mut writer, &mut queue, now + FIRST_RETRY));
        assert!(queue.is_empty());
        assert!(receipts[0].try_recv().is_ok() && receipts[1].try_recv().is_ok());
    }

    #[test]
    fn an_old_scope_cannot_block_the_current_scope() {
        let folder = Folder::new();
        let old = scope(7, 1, 1);
        let current = scope(7, 1, 2);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.activate(current).unwrap();
        assert!(!writer.activate_current_life(old).unwrap());
        assert!(writer.activate_current_life(current).unwrap());
        assert_eq!(writer.waiting().unwrap().scope.life, 2);
    }

    #[test]
    fn malformed_state_is_kept_aside_without_reusing_a_sequence() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.handle(Message::Event(scope, open(1))).unwrap();
        drop(writer);
        fs::write(folder.0.join("state.json"), b"bad state").unwrap();
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.handle(Message::Event(scope, open(2))).unwrap();
        let rows = read(&folder.0, scope, 0, 5, usize::MAX);
        assert_eq!(rows.iter().map(events::Placed::sequence).collect::<Vec<_>>(), [1, 2]);
        assert!(fs::read_dir(&folder.0).unwrap().flatten()
            .any(|entry| fs::read(entry.path()).is_ok_and(|bytes| bytes == b"bad state")));
    }

    #[test]
    fn a_batch_skips_files_wholly_before_its_cursor() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let directory = JournalDirectory::new(&folder.0, scope.account);
        fs::create_dir_all(directory.journal(1)).unwrap(); // fails if opened as a file
        let event = open(2);
        let mut contents = events::file_header(scope.life).unwrap();
        contents.extend(Record::new(event.id, event.at, event.kind).unwrap().place(50).unwrap().line().unwrap());
        fs::write(directory.journal(50), contents).unwrap();
        let rows = read(&folder.0, scope, 50, 5, usize::MAX);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sequence(), 50);
    }

    #[test]
    fn oversized_display_fields_still_keep_the_start() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        let mut observation = start(1);
        let Kind::Start { title, artist, album, .. } = &mut observation.kind else {
            panic!("expected a start");
        };
        for field in [title, artist, album] {
            *field = "🎷".repeat(events::TEXT_CHARS * 2);
        }
        let id = observation.id;
        writer.handle(Message::Event(scope, observation)).unwrap();
        let rows = read(&folder.0, scope, 0, 1, usize::MAX);
        assert_eq!(rows[0].record().id(), id);
        assert!(rows[0].line().unwrap().len() <= events::EVENT_BYTES + 1);
    }

    #[test]
    fn a_cut_after_recording_an_eviction_completes_it_without_double_counting() {
        let folder = Folder::new();
        let scope = scope(7, 1, 1);
        let mut writer = Writer::open(folder.0.clone()).unwrap();
        writer.handle(Message::Event(scope, open(1))).unwrap();
        let mut evidence = writer.state.clone();
        evidence.loss.add(Loss::FilesFull, 1);
        evidence.pending = Some(Pending { account: scope.account, change: Change::Remove(vec![1]) });
        writer.save(evidence).unwrap();
        for _ in 0..2 {
            let after = Writer::open(folder.0.clone()).unwrap();
            assert!(after.state.pending.is_none());
            assert_eq!(after.state.loss.files_full, 1);
            assert!(read(&folder.0, scope, 0, 8, usize::MAX).is_empty());
        }
    }
}
