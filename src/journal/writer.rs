//! The journal's sole writer. This thread owns ordering, retention and recovery.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::config::clear_leftovers;

use super::{Account, Counters, Message, Scope, events, new_id, now_ms, reader::{Update, Waiting}};
use events::{EventId, FileTail, JournalFile, KeptFile, Loss, LossCounts, Record};

const RESERVATION: u64 = 64;
const POLL: Duration = Duration::from_millis(150);

#[derive(Clone, Default, Deserialize, Serialize)]
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
    unwritten: u64,
    persisted_queue_loss: u64,
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
        let mut state: State = read_json(&root.join("state.json"))?.unwrap_or_default();
        if state.reserved == 0 {
            state.reserved = kept_high_water(&root)?;
        }
        let mut writer = Self {
            root, sequence: state.reserved, state, unwritten: 0, persisted_queue_loss: 0,
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
            remove(&directory.checkpoint())?;
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
            self.append(&mut current, events::end_open_start(start, position, id, now_ms()).map_err(invalid)?)?;
            self.prune(&mut current)?;
        }
        self.recovered.insert(scope);
        self.current = Some(current);
        Ok(())
    }

    fn append(&mut self, current: &mut Current, record: Record) -> io::Result<()> {
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
            output.write_all(&append.bytes)?;
            output.sync_data()?;
            crate::config::flushed(&path, started);
            if let Some(first) = current.first_waiting.last_mut() {
                first.get_or_insert(sequence);
            }
            file.bytes += append.bytes.len() as u64;
            file.events += 1;
            file.pending += 1;
            file.final_at_ms = Some(at);
        }
        Ok(())
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
                if new_scope {
                    let directory = JournalDirectory::new(&self.root, scope.account);
                    let known_life: Option<u64> = read_json(&directory.life())?;
                    if known_life.is_some_and(|life| life > scope.life) {
                        return Ok(());
                    }
                    self.activate(scope)?;
                }
                let Some(mut current) = self.current.take() else { return Err(invalid(())) };
                let record = Record::new(observation.id, observation.at, observation.kind).map_err(invalid);
                let result = record.and_then(|record| self.append(&mut current, record));
                // On an uncertain write, rescan rather than append after a torn line.
                if result.is_ok() && self.prune(&mut current).is_ok() {
                    self.current = Some(current);
                }
                result
            }
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
            Update::ScopeChanged(Some(scope)) => {
                if self.current.as_ref().is_none_or(|current| current.scope != scope) {
                    self.activate(scope)?;
                }
            }
            Update::ScopeChanged(None) => {}
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
        let line_full = seen.saturating_sub(self.persisted_queue_loss);
        if line_full != 0 || self.unwritten != 0 {
            let mut state = self.state.clone();
            state.loss.add(Loss::LineFull, line_full);
            state.unwritten = state.unwritten.saturating_add(self.unwritten);
            self.save(state)?;
            self.persisted_queue_loss = seen;
            self.unwritten = 0;
        }
        counters.files_full.store(self.state.loss.files_full, Ordering::Relaxed);
        counters.refused.store(self.state.loss.refused, Ordering::Relaxed);
        Ok(())
    }
}

pub(super) fn run(
    root: PathBuf,
    line: Receiver<Message>,
    updates: Receiver<Update>,
    current_waiting: Arc<Mutex<Option<Waiting>>>,
    counters: Arc<Counters>,
) {
    let mut writer = Writer::open(root.clone()).ok();
    let mut unavailable = 0u64;
    let mut last_totals = Instant::now();
    let mut last_retention = Instant::now();
    let mut last_notice = None::<Instant>;
    let mut visible = None::<Scope>;
    let mut dirty = false;
    let mut first_open = true;
    let mut queued_updates = std::collections::VecDeque::new();
    loop {
        let message = line.recv_timeout(POLL);
        let closing = counters.closing.load(Ordering::Acquire);
        let done = matches!(message, Err(RecvTimeoutError::Disconnected))
            || (closing && matches!(message, Err(RecvTimeoutError::Timeout)));
        if writer.is_none() {
            writer = Writer::open(root.clone()).ok();
        }
        if let Some(writer) = &mut writer {
            if first_open {
                counters.line_full.store(writer.state.loss.line_full, Ordering::Relaxed);
                let _ = writer.keep_losses(&counters);
                first_open = false;
            }
            queued_updates.extend(updates.try_iter());
            while let Some(update) = queued_updates.front() {
                if let Update::ScopeChanged(scope) = update {
                    visible = *scope;
                }
                if writer.update(update).is_err() {
                    break;
                }
                queued_updates.pop_front();
                dirty = true;
            }
            if let Ok(message) = message {
                let event = matches!(message, Message::Event(..));
                if writer.handle(message).is_err() && event {
                    writer.unwritten = writer.unwritten.saturating_add(1);
                }
                dirty |= event;
            }
            if last_retention.elapsed() >= Duration::from_secs(60) {
                if let Some(mut current) = writer.current.take() {
                    if writer.prune(&mut current).is_ok() {
                        writer.current = Some(current);
                    }
                }
                last_retention = Instant::now();
                dirty = true;
            }
            if done || last_totals.elapsed() >= POLL {
                writer.unwritten = writer.unwritten.saturating_add(std::mem::take(&mut unavailable));
                let _ = writer.keep_losses(&counters);
                last_totals = Instant::now();
            }
            if dirty && (done || last_notice.is_none_or(|last| last.elapsed() >= Duration::from_secs(1))) {
                if let Some(scope) = visible {
                    if writer.current.as_ref().is_none_or(|current| current.scope != scope) {
                        let _ = writer.activate(scope);
                    }
                }
                if let Ok(mut waiting) = current_waiting.lock() {
                    *waiting = writer.waiting().filter(|waiting| Some(waiting.scope) == visible);
                }
                dirty = false;
                last_notice = Some(Instant::now());
            }
        } else if matches!(message, Ok(Message::Event(..))) {
            unavailable = unavailable.saturating_add(1);
        }
        if done {
            break;
        }
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
        let root = folder.0.clone();
        std::thread::spawn(move || {
            let mut writer = Writer::open(root.clone()).unwrap();
            writer.handle(receive.try_recv().unwrap()).unwrap();
            writer.keep_losses(&counters).unwrap();
            writer.keep_losses(&counters).unwrap();
            assert_eq!(Writer::open(root).unwrap().state.loss.line_full, 2);
        }).join().unwrap();
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
        let mut again = Writer::open(folder.0.clone()).unwrap();
        again.activate(scope).unwrap();
        assert_eq!(read(&folder.0, scope, 0, 10, usize::MAX).len(), 4);
        assert_eq!(again.state.damaged, 1);
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
