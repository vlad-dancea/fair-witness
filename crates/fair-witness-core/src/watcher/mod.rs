mod change;
mod filter;
mod set;
mod summary;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::mem;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use notify_debouncer_full::file_id::{self, FileId};
use notify_debouncer_full::notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::notify::{
    self, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _, WatcherKind,
};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer_opt,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub use change::{Change, Kind};
use filter::CACHE_TAG;
pub use filter::Filter;
use set::{Held, LOOKS, extend_set, take_under, written_at};
pub use summary::{Listed, summarize};

/// How long the folder stays quiet before its changes count as one change set, unless a
/// caller asks for another time.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// The debouncer checks for quiet every quarter of the wait, so a wait of zero would have it
/// spin.
const SHORTEST_DEBOUNCE: Duration = Duration::from_millis(10);
/// What the watcher reports when the system dropped events, before the change set that holds
/// what it found by looking through the folder again.
const MISSED: &str = "too many changes came at once and the system dropped some of their \
                      events, so the folder was looked through again, and the next change set \
                      lists what was created, deleted or written meanwhile; a change undone in \
                      between is not seen";

/// What the watcher reports instead of a change set.
#[derive(Debug)]
pub enum WatchError {
    /// Watching failed for some folders, or the system dropped events, and goes on. Each
    /// message says what goes unseen.
    Failed(Vec<String>),
    /// The project folder was moved or deleted, so nothing is watched any more.
    Gone,
    /// The project folder could not be watched again after the system dropped events, for the
    /// reason given, so nothing is watched any more. The changes found before come first.
    Stopped(String),
}

type Batch = (Instant, DebounceEventResult);

/// Something about a known path that one batch cannot tell, because the event that tells it can
/// come in a later one. The debouncer hands on each event once it is a debounce old, a tick at a
/// time, so it can split one sequence of changes over batches anywhere.
struct Doubt {
    /// Whether a rename landed on the file, as `Doubt::replaced` says, rather than the path
    /// missing from its folder, as `Doubt::missing` says.
    replaced: bool,
    /// When the events that raised it happened, which puts what it finds in their set.
    at: Instant,
    /// A debounce and a half after the batch that raised it was handled, as for a set's end, by
    /// when the debouncer has handed on every event from before then.
    due: Instant,
}

impl Doubt {
    /// A rename landed on the file at the path, which it replaced unless the file there was
    /// renamed away within the debounce. The debouncer joins the steps of a swap through a
    /// temporary name into renames that seem to land on known files, each in its own batch or
    /// both in one.
    fn replaced(at: Instant, debounce: Duration) -> Self {
        Self {
            replaced: true,
            at,
            due: Instant::now() + debounce * 3 / 2,
        }
    }

    /// The path was missing from its folder after a batch, which deleted it unless its remove or
    /// rename is still to come. The debouncer drops the remove of a file that a rename lands on
    /// and that moves on within the debounce, unless it had events of its own, so no event may
    /// ever say.
    fn missing(at: Instant, debounce: Duration) -> Self {
        Self {
            replaced: false,
            ..Self::replaced(at, debounce)
        }
    }
}

pub struct FolderWatcher {
    /// None only once it could not be started again.
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
    /// When `debouncer` had watched every folder.
    watched: Instant,
    /// The debouncer's batches, each with when it handed the batch on.
    events: UnboundedReceiver<Batch>,
    sender: UnboundedSender<Batch>,
    debounce: Duration,
    held: Option<Held>,
    root: PathBuf,
    /// The project folder's id when watching started, which tells it from a folder that took its
    /// place since.
    root_id: FileId,
    known: BTreeSet<PathBuf>,
    /// What a batch left open about the known paths, until a later event or the time settles it.
    doubts: BTreeMap<PathBuf, Doubt>,
    filter: Filter,
    /// What the last batch showed went wrong, which `next` reports before anything else.
    pending: Vec<String>,
    /// Why watching stopped, which `next` reports once it has handed on the set it holds.
    stopped: Option<String>,
}

impl FolderWatcher {
    /// Watches `root`, grouping changes until the folder has been quiet for `debounce`, or
    /// for 10 milliseconds when that is shorter.
    pub fn new(root: &Path, debounce: Duration) -> Result<Self> {
        let filter = Filter::new(root)?;
        let (sender, events) = mpsc::unbounded_channel();
        let debounce = debounce.max(SHORTEST_DEBOUNCE);
        let root_id = file_id::get_file_id(root)
            .with_context(|| format!("could not read {}", root.display()))?;
        let debouncer = start(root, debounce, sender.clone()).map_err(|e| not_started(e, root))?;
        let watched = Instant::now();
        let mut known = BTreeSet::new();
        walk(root, &filter, &mut |path| {
            known.insert(path);
        });
        Ok(Self {
            debouncer: Some(debouncer),
            watched,
            events,
            sender,
            debounce,
            held: None,
            root: root.to_path_buf(),
            root_id,
            known,
            doubts: BTreeMap::new(),
            filter,
            pending: Vec::new(),
            stopped: None,
        })
    }

    /// What the watcher leaves out as not research data.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    fn apply(&mut self, event: &DebouncedEvent, changes: &mut Vec<Change>) {
        let paths = &event.paths;
        match event.kind {
            EventKind::Create(_) => paths.iter().for_each(|path| self.arrive(path, changes)),
            EventKind::Remove(_) => paths.iter().for_each(|path| self.forget(path, changes)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                self.rename(&paths[0], &paths[1], event.time, changes)
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                for path in paths {
                    if fs::symlink_metadata(path).is_ok() {
                        self.arrive(path, changes);
                    } else {
                        self.forget(path, changes);
                    }
                }
            }
            EventKind::Modify(ModifyKind::Metadata(_)) => {}
            EventKind::Modify(_) => {
                for path in paths {
                    if !self.known.contains(path) {
                        self.discover(path, changes);
                    } else if self.filter.skips(path, false) {
                        // Its folder was tagged as a cache since, as in
                        // `forget`.
                        self.known.remove(path);
                    } else if path.is_file() {
                        changes.push(Change::Changed(path.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    /// A path that was created or moved in from outside. A file already there was replaced, as
    /// a program that saves by renaming a new file over the old one does, which the debouncer
    /// hands on as a create of the old name, so it changed.
    fn arrive(&mut self, path: &Path, changes: &mut Vec<Change>) {
        // Without following a link, which can replace a file as well and point nowhere.
        let folder = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
        if !self.known.contains(path) || folder {
            self.discover(path, changes);
        } else if !self.filter.skips(path, false) {
            changes.push(Change::Changed(path.to_path_buf()));
        }
    }

    fn discover(&mut self, path: &Path, changes: &mut Vec<Change>) {
        walk(path, &self.filter, &mut |found| {
            if self.known.insert(found.clone()) {
                changes.push(Change::Created(found));
            }
        });
    }

    fn forget(&mut self, path: &Path, changes: &mut Vec<Change>) {
        // A folder can be tagged as a cache after the event, too late for the
        // tag's own event to be in this batch.
        if !self.take_subtree(path).is_empty() && !self.filter.skips(path, false) {
            changes.push(Change::Deleted(path.to_path_buf()));
        }
    }

    /// Applies a rename whose event happened `at`.
    fn rename(&mut self, from: &Path, to: &Path, at: Instant, changes: &mut Vec<Change>) {
        let to_dir = fs::symlink_metadata(to).is_ok_and(|metadata| metadata.is_dir());
        // A move out of the project is a deletion, as when only its source is
        // reported.
        if !to.starts_with(&self.root) || self.filter.skips(to, to_dir) {
            self.forget(from, changes);
            return;
        }
        let moved = self.take_subtree(from);
        if moved.is_empty() {
            self.arrive(to, changes);
            return;
        }
        // A file renamed over another replaces it, which the rename alone does not say, and
        // whether it did can wait for a later batch.
        if self.known.contains(to) && !to_dir {
            let doubt = Doubt::replaced(at, self.debounce);
            self.doubts.insert(to.to_path_buf(), doubt);
        }
        let moved_to = |old: &Path| {
            let rest = old
                .strip_prefix(from)
                .expect("subtree paths start with its root");
            if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            }
        };
        for old in moved {
            let new = moved_to(&old);
            let is_dir = fs::symlink_metadata(&new).is_ok_and(|metadata| metadata.is_dir());
            if !self.filter.skips(&new, is_dir) {
                self.known.insert(new);
            }
        }
        // The doubts about what is in a renamed folder go with it. One about the folder itself
        // was settled by this event.
        let doubted: Vec<PathBuf> = self
            .doubts
            .range::<Path, _>((Bound::Excluded(from), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(from))
            .map(|(path, _)| path.clone())
            .collect();
        for old in doubted {
            if let Some(doubt) = self.doubts.remove(&old) {
                self.doubts.insert(moved_to(&old), doubt);
            }
        }
        changes.push(Change::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        self.discover(to, changes);
    }

    /// Settles the doubts about the paths `event` names, before it is applied. A replaced file
    /// that the event renames away within the debounce of the rename that landed on it was in a
    /// swap, and any other event comes after the file was replaced, so it changed, also when the
    /// event itself reports nothing, as a write to a path that is gone by now. A missing path is
    /// settled by an event that creates, removes or renames it.
    fn settle_by(&mut self, event: &DebouncedEvent, changes: &mut Vec<Change>) {
        let renames = matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)));
        let moves = renames || event.kind.is_create() || event.kind.is_remove();
        for (index, path) in event.paths.iter().enumerate() {
            let Some(doubt) = self.doubts.get(path) else {
                continue;
            };
            if doubt.replaced {
                let swapped = renames && index == 0 && event.time <= doubt.at + self.debounce;
                if !swapped {
                    changes.push(Change::Changed(path.clone()));
                }
            } else if !moves {
                continue;
            }
            self.doubts.remove(path);
        }
    }

    /// Doubts each path the watcher knows right inside `folder` that is not there, as
    /// `Doubt::missing` says, with when the events that touched the folder happened.
    fn doubt_missing(&mut self, folder: &Path, at: Instant) {
        let Ok(entries) = fs::read_dir(folder) else {
            return;
        };
        let names: HashSet<OsString> = entries.flatten().map(|entry| entry.file_name()).collect();
        for path in children(&self.known, folder) {
            if !path.file_name().is_some_and(|name| names.contains(name)) {
                let doubt = Doubt::missing(at, self.debounce);
                self.doubts.entry(path).or_insert(doubt);
            }
        }
    }

    /// Settles the doubts that are due by `now`, which no event settled: a path still known and
    /// gone is deleted, and a replaced file still there changed. Gives back what they found as a
    /// batch, with when the events that raised them happened.
    fn settle(&mut self, now: Instant) -> Option<Held> {
        let due: Vec<(PathBuf, Doubt)> = self
            .doubts
            .extract_if(.., |_, doubt| doubt.due <= now)
            .collect();
        let mut changes = Vec::new();
        let mut times = Vec::new();
        for (path, doubt) in due {
            if !self.known.contains(&path) {
                continue;
            }
            // Nothing is reported for a path in a folder tagged as a cache since, as in `forget`.
            let before = changes.len();
            if fs::symlink_metadata(&path).is_err_and(|e| missing(&e)) {
                self.forget(&path, &mut changes);
            } else if doubt.replaced && !self.filter.skips(&path, false) {
                changes.push(Change::Changed(path));
            }
            if changes.len() > before {
                times.push(doubt.at);
            }
        }
        let (first, last) = (*times.iter().min()?, *times.iter().max()?);
        let changes = changes
            .into_iter()
            .map(|change| change.relative_to(&self.root))
            .collect();
        Some(Held::new(changes, first, last, now + self.debounce / LOOKS))
    }

    fn handle(&mut self, events: Vec<DebouncedEvent>) -> Vec<Change> {
        // Opening a tag to read it is an event too, so only writes count, or
        // every read would cause another.
        let tagged: Vec<&Path> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(
                            ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Name(_)
                        )
                )
            })
            .flat_map(|event| &event.paths)
            .filter(|path| path.file_name() == Some(OsStr::new(CACHE_TAG)))
            .filter_map(|path| path.parent())
            .collect();
        // Cargo creates `target/` before it tags it, so files in a folder can
        // be known before the folder counts as a cache. Forgetting them before
        // the events are applied sends every later event there through the
        // filter. Nothing is reported, because nothing was deleted.
        for folder in &tagged {
            if self.filter.skips(folder, true) {
                self.take_subtree(folder);
            }
        }
        // The debouncer puts a remove marked "override" before the create of a path or a rename
        // onto it when the path has events of its own, as `sed -i`'s has after it reads the file,
        // and keeps both in one batch. The create or rename says on its own that the file was
        // replaced, as it does without such events, so the remove counts only when nothing came
        // to the path after it, as when the new file moved on within the debounce.
        let arrivals: HashMap<PathBuf, usize> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| Some((arrived_at(event)?, index)))
            .collect();
        let mut changes = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let replaced = event.kind.is_remove()
                && event.info() == Some("override")
                && event.paths.first().and_then(|path| arrivals.get(path)) > Some(&index);
            if replaced {
                continue;
            }
            if can_change(&event.kind) {
                self.settle_by(event, &mut changes);
            }
            self.apply(event, &mut changes);
        }
        // A folder whose tag is gone or lost its signature holds files that
        // were never reported, so they count as created, like the files of a
        // folder that appears.
        for folder in &tagged {
            if !self.filter.skips(folder, true) {
                self.discover(folder, &mut changes);
            }
        }
        // A file can go without an event of its own, so the folders whose entries the events
        // changed are looked into. Only the folders, since a walk of the whole project after each
        // batch would take long in a large one.
        let touched: BTreeSet<&Path> = events
            .iter()
            .filter(|event| {
                event.kind.is_create()
                    || event.kind.is_remove()
                    || matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)))
            })
            .flat_map(|event| &event.paths)
            .filter_map(|path| path.parent())
            .filter(|folder| *folder == self.root || self.known.contains(*folder))
            .collect();
        if let Some(at) = events.iter().map(|event| event.time).max() {
            for folder in touched {
                self.doubt_missing(folder, at);
            }
        }
        // inotify watches a new folder only once the watcher has added it, so a file written
        // into it right away can come with its write but without its create. Walking the folder
        // reports the file as created, and its write then finds it known.
        let mut set = Vec::new();
        extend_set(
            &mut set,
            changes
                .into_iter()
                .map(|change| change.relative_to(&self.root)),
        );
        set
    }

    /// Looks at the files of the held set if it is time to.
    fn look_if_due(&mut self) {
        let Some(held) = &mut self.held else {
            return;
        };
        if Instant::now() < held.next_look {
            return;
        }
        held.note_writes(&self.root, &self.known, self.debounce);
        held.next_look = Instant::now() + self.debounce / LOOKS;
    }

    /// When the file that a create or a rename put at `path`, or a file the watcher knows in
    /// the folder it put there, was last written while the debouncer still held that event,
    /// which it handed on at `sent`. The debouncer drops the writes to a new file until it hands
    /// the create on, and holds a rename back for a debounce, so without this a file still being
    /// copied would seem quiet for that long and the rest of the copy would start a set of its
    /// own. Only files the watcher reports count, so an ignored file being written keeps no set
    /// open.
    fn written_while_held(&self, path: &Path, sent: Instant) -> Option<Instant> {
        let prompt = Instant::now().saturating_duration_since(sent) <= self.debounce / LOOKS;
        self.known
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .take_while(|known| known.starts_with(path))
            .filter_map(|file| written_at(file))
            .filter_map(|written| {
                if written <= sent {
                    Some(written)
                } else {
                    // A write after `sent` came as an event of its own and hides when the file
                    // was written before. Right after `sent`, it shows that the copy was still
                    // running when the event was handed on. Later, it could also follow a
                    // pause, which its own event shows.
                    prompt.then_some(sent)
                }
            })
            .max()
    }

    fn take_subtree(&mut self, path: &Path) -> Vec<PathBuf> {
        take_under(&mut self.known, path)
    }

    /// Looks through the folder again after the system dropped events, and gives back what
    /// differs from the paths the watcher knows: what was created or deleted, and the files it
    /// knows that were written since `since`, which the dropped events may have been about.
    /// The system said so at `noticed`. When the folder cannot be watched again, watching stops.
    fn rescan(&mut self, since: Instant, noticed: Instant) -> Vec<Change> {
        let mut changes = self.look_again(since);
        // notify opens every folder on the thread that reads the events when it starts watching,
        // which can overflow the queue by itself in a large project. Another watcher would do
        // the same, also while a file that is still written makes every look find a change.
        let own = noticed <= self.watched + self.debounce;
        if own || RecommendedWatcher::kind() != WatcherKind::Inotify {
            return changes;
        }
        // inotify watches a folder only once its create comes, so a folder made while events
        // were dropped is not watched, also one made again where the watcher knows one, and a
        // dropped rename leaves notify's list of watches wrong, so that a later event can remove
        // a watch still in use. A new watcher, with an inotify instance of its own, puts that
        // right. The old one goes first, since both count against the same limits, and what it
        // still holds happened before the look above. Looking once more finds what was made in
        // a folder before its new watch.
        self.debouncer = None;
        // Watching stops then, as it does when it cannot start, and is not tried again, since
        // notify leaves a thread running when it cannot make the watcher.
        match start(&self.root, self.debounce, self.sender.clone()) {
            Ok(fresh) => {
                self.debouncer = Some(fresh);
                self.watched = Instant::now();
                let found = self.look_again(since);
                extend_set(&mut changes, found);
            }
            Err(e) => {
                let (folder, cause) = explain(e);
                self.stopped = Some(format!(
                    "could not watch {} again after the system dropped events, so the project \
                     folder is no longer watched: {cause}",
                    folder.as_deref().unwrap_or(&self.root).display()
                ));
            }
        }
        changes
    }

    /// Walks the folder and gives back what differs from the paths the watcher knows, as
    /// `rescan` says, and knows the paths it found from then on.
    fn look_again(&mut self, since: Instant) -> Vec<Change> {
        let mut found = BTreeSet::new();
        walk(&self.root, &self.filter, &mut |path| {
            found.insert(path);
        });
        let mut changes = Vec::new();
        let mut kept = Vec::new();
        // Paths come in order, each folder right before what is in it, so a deleted folder is
        // reported once, as `forget` does.
        let mut deleted: Option<&PathBuf> = None;
        for path in self.known.difference(&found) {
            if deleted.is_some_and(|folder| path.starts_with(folder)) {
                continue;
            }
            match fs::symlink_metadata(path) {
                Err(e) if missing(&e) => {
                    if !self.filter.skips(path, false) {
                        changes.push(Change::Deleted(path.clone()));
                    }
                    deleted = Some(path);
                }
                // In a folder tagged as a cache since, which `handle` forgets without a word.
                Ok(metadata) if self.filter.skips(path, metadata.is_dir()) => {}
                // The walk could not look into a folder above it, which does not make it gone.
                _ => kept.push(path.clone()),
            }
        }
        changes.extend(found.difference(&self.known).cloned().map(Change::Created));
        changes.extend(
            self.known
                .intersection(&found)
                .filter(|path| written_at(path).is_some_and(|written| written >= since))
                .cloned()
                .map(Change::Changed),
        );
        found.extend(kept);
        self.known = found;
        changes
            .into_iter()
            .map(|change| change.relative_to(&self.root))
            .collect()
    }

    /// Whether the project folder was moved or deleted, which the events say when it was the
    /// folder itself. When a folder above it moved, no event says so, but the folder is no
    /// longer where the later events name it, or another one is. A folder that cannot be looked
    /// at, as when a folder above it is closed to the user for a while, is still there.
    fn gone(&self, events: &[DebouncedEvent]) -> bool {
        let moved_or_removed = |kind: &EventKind| {
            kind.is_remove() || matches!(kind, EventKind::Modify(ModifyKind::Name(_)))
        };
        let on_root = events
            .iter()
            .any(|event| moved_or_removed(&event.kind) && event.paths.contains(&self.root));
        on_root
            || match file_id::get_file_id(&self.root) {
                Ok(id) => id != self.root_id,
                Err(e) => matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ),
            }
    }

    /// The next change set, once the folder has been quiet for the debounce or the set's first
    /// change is 30 debounces old.
    pub async fn next(&mut self) -> Option<Result<Vec<Change>, WatchError>> {
        loop {
            if !self.pending.is_empty() {
                return Some(Err(WatchError::Failed(mem::take(&mut self.pending))));
            }
            // No more events come, so the set is complete.
            if let Some(stopped) = &self.stopped {
                return Some(match self.held.take() {
                    Some(held) => Ok(held.changes),
                    None => Err(WatchError::Stopped(stopped.clone())),
                });
            }
            // By the clock, since batches of other files can keep coming while a file is copied.
            self.look_if_due();
            let doubts_due = self.doubts.values().map(|doubt| doubt.due).min();
            let held = self.held.as_ref();
            let received = match held.map(|held| (self.complete(held), held.next_look)) {
                // `timeout_at` polls the channel first, so batches that waited there while the
                // watcher handled others still go into sets by when their events happened.
                Some((complete, next_look)) => {
                    let until = doubts_due
                        .into_iter()
                        .fold(complete.min(next_look), Instant::min);
                    match tokio::time::timeout_at(until.into(), self.events.recv()).await {
                        Ok(received) => received,
                        // A look or a doubt is due, or the set is complete unless the look finds a
                        // write.
                        Err(_) => {
                            self.look_if_due();
                            if let Some(found) = self.settle(Instant::now())
                                && let Some(done) = self.hold(found)
                            {
                                return Some(Ok(done));
                            }
                            let held = self.held.as_ref();
                            if held.is_some_and(|held| Instant::now() < self.complete(held)) {
                                continue;
                            }
                            None
                        }
                    }
                }
                // Windows sends no event when the project folder itself is deleted, and no system
                // sends one when a folder above it moves, so a quiet folder is looked at once a
                // debounce.
                None => {
                    let quiet = Instant::now() + self.debounce;
                    let until = doubts_due.map_or(quiet, |due| due.min(quiet));
                    match tokio::time::timeout_at(until.into(), self.events.recv()).await {
                        Ok(received) => received,
                        Err(_) if self.gone(&[]) => return Some(Err(WatchError::Gone)),
                        Err(_) => {
                            if let Some(found) = self.settle(Instant::now()) {
                                self.held = Some(found);
                            }
                            continue;
                        }
                    }
                }
            };
            let (sent, events) = match received {
                Some((sent, Ok(events))) => (sent, events),
                Some((_, Err(errors))) => {
                    let errors = errors.into_iter().map(unwatched).collect();
                    return Some(Err(WatchError::Failed(errors)));
                }
                // The set is complete, or the debouncer stopped. A folder can go without an event
                // while the set is held, and the set goes with it then, as below.
                None if self.gone(&[]) => {
                    self.held = None;
                    return Some(Err(WatchError::Gone));
                }
                None => return self.held.take().map(|held| Ok(held.changes)),
            };
            // The watches stay on the folder wherever it went, or on nothing, and their events
            // name paths that are not the project's any more. The held set goes too, since
            // nothing in it can be read where it was.
            if self.gone(&events) {
                self.held = None;
                return Some(Err(WatchError::Gone));
            }
            // The system dropped events, which notify says with an event of its own that names
            // no path and makes no change, so the folder has to be looked through again.
            let rescan = events
                .iter()
                .filter(|event| event.need_rescan())
                .map(|event| event.time)
                .max();
            // Only events that can make a change count, so that reading a file bridges no
            // pause, and a batch that made none, such as in a folder the filter leaves out, keeps
            // no set open. The events that were dropped made changes, though.
            let times = events
                .iter()
                .filter(|event| can_change(&event.kind) || event.need_rescan())
                .map(|event| event.time);
            let (Some(mut first), Some(last)) = (times.clone().min(), times.max()) else {
                continue;
            };
            let mut arrived: Vec<PathBuf> = events.iter().filter_map(arrived_at).collect();
            let mut changes = self.handle(events);
            if let Some(noticed) = rescan {
                // The dropped events came after the set held now began, or with this batch when
                // none is held, so the files written since then count as changed, which can list
                // a file of the held set again in the next. Events wait in the system's queue
                // until notify reads them, longest when it overflows or is busy, so a dropped
                // write can be older still, and a debounce more allows for some of that.
                let start = self
                    .held
                    .as_ref()
                    .map_or(first, |held| held.first.min(first));
                let since = start.checked_sub(self.debounce).unwrap_or(start);
                let found = self.rescan(since, noticed);
                if found.is_empty() {
                    // Nothing the watcher reports changed, as when the dropped events were reads,
                    // which notify's opening every folder to watch it at the start can be.
                    tracing::info!("the system dropped events, and none of them made a change");
                } else {
                    self.pending.insert(0, MISSED.to_string());
                }
                // What the dropped events wrote counts as written while they were held, as for
                // a create, so that a copy the walk found still running keeps its set open.
                arrived.extend(
                    found
                        .iter()
                        .filter(|change| !matches!(change, Change::Deleted(_)))
                        .map(|change| self.root.join(change.path())),
                );
                extend_set(&mut changes, found);
            }
            // The debouncer handed this batch on after every event that could settle these
            // doubts, and they go with the batches that raised them.
            if let Some(found) = self.settle(sent) {
                extend_set(&mut changes, found.changes);
                first = first.min(found.first);
            }
            if changes.is_empty() {
                continue;
            }
            let last = arrived
                .iter()
                .filter_map(|path| self.written_while_held(path, sent))
                .fold(last, Instant::max);
            let batch = Held::new(changes, first, last, Instant::now() + self.debounce / LOOKS);
            if let Some(done) = self.hold(batch) {
                return Some(Ok(done));
            }
        }
    }

    /// Adds `batch` to the held set, or holds it instead and gives back the set it ends.
    fn hold(&mut self, batch: Held) -> Option<Vec<Change>> {
        match &mut self.held {
            Some(held) if held.takes(batch.first, self.debounce) => {
                held.add(batch);
                None
            }
            held => held.replace(batch).map(|done| done.changes),
        }
    }

    /// When the held set is complete: when `Held::complete` says, and not before every doubt is
    /// settled, since what a doubt finds goes with the batch that raised it.
    fn complete(&self, held: &Held) -> Instant {
        self.doubts
            .values()
            .map(|doubt| doubt.due)
            .fold(held.complete(self.debounce), Instant::max)
    }
}

/// The paths in `paths` right inside `folder`, found without going through what is in them, so
/// that a folder with a large tree under it stays quick to look into.
fn children(paths: &BTreeSet<PathBuf>, folder: &Path) -> Vec<PathBuf> {
    let mut children = Vec::new();
    let mut from = Bound::Excluded(folder.to_path_buf());
    while let Some(path) = paths.range((from, Bound::Unbounded)).next() {
        let Some(name) = path
            .strip_prefix(folder)
            .ok()
            .and_then(|rest| rest.iter().next())
        else {
            break;
        };
        let child = folder.join(name);
        // Paths sort by their names one at a time, so the child's name with a NUL byte added, which
        // no name holds, sorts after everything in the child and before the name after it.
        let mut past = child.clone().into_os_string();
        past.push("\0");
        from = Bound::Included(PathBuf::from(past));
        if *path == child {
            children.push(child);
        }
    }
    children
}

/// Whether `error`, from looking at a path, says that nothing is there.
fn missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Where a create or a rename put a file or a folder.
fn arrived_at(event: &DebouncedEvent) -> Option<PathBuf> {
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            event.paths.first().cloned()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => event.paths.get(1).cloned(),
        _ => None,
    }
}

/// A debouncer that watches `root` and sends each batch to `sender` with when it handed it on.
fn start(
    root: &Path,
    debounce: Duration,
    sender: UnboundedSender<Batch>,
) -> notify::Result<Debouncer<RecommendedWatcher, RecommendedCache>> {
    // A link can lead to a share with more folders than the system lets a user watch, and what
    // changes there is not the project's, which `walk` and the model's tools also stay out of.
    let config = notify::Config::default().with_follow_symlinks(false);
    let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher, _>(
        debounce,
        None,
        move |result| {
            let _ = sender.send((Instant::now(), result));
        },
        RecommendedCache::new(),
        config,
    )?;
    debouncer.watch(root, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

/// The folder `error` is about, if it names one, and what went wrong, with the setting and the
/// command that raise the limit when the system's limit on inotify was reached.
fn explain(mut error: notify::Error) -> (Option<PathBuf>, String) {
    let folder = error.paths.first().cloned();
    // notify's message would name the paths again.
    error.paths.clear();
    let limit = match &error.kind {
        notify::ErrorKind::MaxFilesWatch => Some(("max_user_watches", "watched folders", 524_288)),
        // Each program that watches folders opens at least one inotify instance, which also
        // counts as an open file, and fair-witness opens few others.
        #[cfg(target_os = "linux")]
        notify::ErrorKind::Io(e)
            if rustix::io::Errno::from_io_error(e) == Some(rustix::io::Errno::MFILE) =>
        {
            Some(("max_user_instances", "inotify instances", 1024))
        }
        _ => None,
    };
    let Some((setting, what, fallback)) = limit else {
        return (folder, error.to_string());
    };
    // Linux 5.11 and later size the limit on watches by the memory, so a fixed number could
    // lower it.
    let current = fs::read_to_string(format!("/proc/sys/fs/inotify/{setting}"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let (limit, raised) = match current {
        Some(current) => (format!("{current} {what}"), current.saturating_mul(2)),
        None => (what.to_string(), fallback),
    };
    let cause = format!(
        "the limit of {limit} per user (fs.inotify.{setting}) is reached. Raise it with \
         `sudo sysctl fs.inotify.{setting}={raised}` and start `fair-witness run` again"
    );
    (folder, cause)
}

/// Why watching `root` could not start.
fn not_started(error: notify::Error, root: &Path) -> anyhow::Error {
    let (folder, cause) = explain(error);
    anyhow!(
        "could not watch {}: {cause}",
        folder.as_deref().unwrap_or(root).display()
    )
}

/// What a watch error says while watching goes on, which leaves the folder it names unwatched.
fn unwatched(error: notify::Error) -> String {
    match explain(error) {
        (Some(folder), cause) => format!(
            "could not watch {}, so changes in it go unseen: {cause}",
            folder.display()
        ),
        (None, cause) => cause,
    }
}

/// Whether `FolderWatcher::apply` can make a change of an event of this kind, which it cannot
/// of a read or of a change to a file's metadata.
fn can_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Modify(ModifyKind::Metadata(_)))
        && (kind.is_create() || kind.is_remove() || kind.is_modify())
}

fn walk(path: &Path, filter: &Filter, visit: &mut impl FnMut(PathBuf)) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if filter.skips(path, metadata.is_dir()) {
        return;
    }
    visit(path.to_path_buf());
    if !metadata.is_dir() {
        return;
    }
    // Changes inside a folder that cannot be read go unnoticed, so the log says which.
    match fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                walk(&entry.path(), filter, visit);
            }
        }
        Err(e) => tracing::warn!("could not look into {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::SystemTime;

    use super::set::LONGEST_SET;
    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(50);
    /// Generous, so that a slow machine still delivers the first batch in time.
    const FIRST_BATCH: Duration = Duration::from_secs(5);
    /// Every change a test makes happens before it drains, so a batch that comes this long
    /// after the last one belongs to no test.
    const QUIET: Duration = Duration::from_millis(250);

    fn watch(root: &Path) -> FolderWatcher {
        FolderWatcher::new(root, DEBOUNCE).unwrap()
    }

    async fn drain(watcher: &mut FolderWatcher) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut wait = FIRST_BATCH;
        while let Ok(Some(batch)) = tokio::time::timeout(wait, watcher.next()).await {
            changes.extend(batch.expect("watching failed"));
            wait = QUIET;
        }
        changes
    }

    fn created(changes: &[Change]) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Created(path) => Some(path.clone()),
                _ => None,
            })
            .collect();
        paths.sort();
        paths
    }

    fn git(dir: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        // A git hook or `git rebase --exec` running the tests sets variables such as GIT_DIR,
        // which would point these commands at that repository instead of `dir`.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        let status = command
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.org"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn skips_a_commit_in_a_git_repository() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::write(root.join("notes.txt"), "a").unwrap();
        git(root, &["init", "-q"]);
        let mut watcher = watch(root);

        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "Add notes"]);
        fs::write(root.join("results.csv"), "x").unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Created(PathBuf::from("results.csv"))]
        );
    }

    #[tokio::test]
    async fn skips_folders_tools_fill_and_files_the_researcher_ignores() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join(".fair-witness")).unwrap();
        fs::write(root.join(".fair-witness/ignore"), "scratch/\n").unwrap();
        let mut watcher = watch(root);

        for path in [
            ".venv/lib/site.py",
            "node_modules/plot/index.js",
            "analysis/__pycache__/clean.pyc",
            "analysis/.ipynb_checkpoints/explore.ipynb",
            "data/.a.csv.swp",
            "data/a.csv",
            ".DS_Store",
            "scratch/tmp.csv",
            ".fair-witness/madmp.json",
            "data/.fair-witness/log/x",
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }

        assert_eq!(
            created(&drain(&mut watcher).await),
            ["analysis", "data", "data/a.csv"].map(PathBuf::from),
        );
    }

    const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
        # This file is a cache directory tag created by cargo.\n";

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn skips_folders_tools_tag_as_caches() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, ".pytest_cache/CACHEDIR.TAG", CARGO_TAG);
        let mut watcher = watch(root);

        write(root, ".pytest_cache/v/cache/lastfailed", "{}");
        // As `cargo build` does, in one change set.
        fs::create_dir(root.join("target")).unwrap();
        write(root, "target/CACHEDIR.TAG", CARGO_TAG);
        write(root, "target/debug/app", "x");
        write(root, "data/a.csv", "a");

        assert_eq!(
            created(&drain(&mut watcher).await),
            ["data", "data/a.csv"].map(PathBuf::from),
        );
    }

    #[tokio::test]
    async fn stops_reporting_a_folder_once_it_is_tagged_as_a_cache() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);

        write(root, "target/debug/app", "x");
        assert_eq!(
            created(&drain(&mut watcher).await),
            ["target", "target/debug", "target/debug/app"].map(PathBuf::from),
        );
        write(root, "target/CACHEDIR.TAG", CARGO_TAG);
        write(root, "target/debug/app", "y");
        write(root, "target/debug/build.log", "y");
        write(root, "results.csv", "x");

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Created(PathBuf::from("results.csv"))]
        );
    }

    #[tokio::test]
    async fn skips_changes_to_known_files_in_a_folder_tagged_since() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "target/app", "x");
        write(root, "target/build.log", "x");
        let mut watcher = watch(root);

        // The debouncer hands events on a debounce after they happen, so the
        // tag can be written before the batch with earlier events is read.
        fs::remove_file(root.join("target/build.log")).unwrap();
        write(root, "target/CACHEDIR.TAG", CARGO_TAG);
        let edit = notify::Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(root.join("target/app"));
        let removal = notify::Event::new(EventKind::Remove(notify::event::RemoveKind::File))
            .add_path(root.join("target/build.log"));
        let changes = watcher.handle(vec![
            DebouncedEvent::new(edit, Instant::now()),
            DebouncedEvent::new(removal, Instant::now()),
        ]);

        assert_eq!(changes, []);
    }

    #[tokio::test]
    async fn reports_nothing_for_a_file_gone_from_a_folder_tagged_since() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "target/app", "x");
        write(root, "target/build.log", "x");
        let mut watcher = watch(root);

        // A file goes without an event of its own, and the folder is tagged before the tag's
        // event is handed on, so a create beside them is all the watcher gets.
        fs::remove_file(root.join("target/build.log")).unwrap();
        write(root, "target/CACHEDIR.TAG", CARGO_TAG);
        write(root, "target/app.d", "x");
        let create = notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(root.join("target/app.d"));
        let changes = watcher.handle(vec![DebouncedEvent::new(create, Instant::now())]);
        let later = watcher.settle(Instant::now() + DEBOUNCE * 2);

        assert_eq!(changes, []);
        assert_eq!(later.map(|found| found.changes), None);
    }

    #[tokio::test]
    async fn reports_a_folder_whose_tag_is_removed_as_created() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "data/CACHEDIR.TAG", CARGO_TAG);
        write(root, "data/a.csv", "a");
        write(root, "data/b.csv", "b");
        let mut watcher = watch(root);

        fs::remove_file(root.join("data/CACHEDIR.TAG")).unwrap();
        assert_eq!(
            created(&drain(&mut watcher).await),
            ["data", "data/a.csv", "data/b.csv"].map(PathBuf::from),
        );
        fs::write(root.join("data/a.csv"), "c").unwrap();
        fs::remove_file(root.join("data/b.csv")).unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [
                Change::Changed(PathBuf::from("data/a.csv")),
                Change::Deleted(PathBuf::from("data/b.csv")),
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_file_moved_into_an_ignored_folder_as_deleted() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("data")).unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        let mut watcher = watch(root);

        fs::rename(root.join("data/a.csv"), root.join("node_modules/a.csv")).unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Deleted(PathBuf::from("data/a.csv"))]
        );
    }

    #[tokio::test]
    async fn reports_a_download_only_once_it_is_renamed_into_place() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("downloads")).unwrap();
        let mut watcher = watch(root);
        let part = root.join("downloads/era5-2026-10.nc.part");
        let done = root.join("downloads/era5-2026-10.nc");

        fs::write(&part, "x").unwrap();
        let started = watcher.handle(vec![DebouncedEvent::new(
            notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
                .add_path(part.clone()),
            Instant::now(),
        )]);
        fs::rename(&part, &done).unwrap();
        let finished = watcher.handle(vec![DebouncedEvent::new(
            notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(part)
                .add_path(done),
            Instant::now(),
        )]);

        assert_eq!(
            (started, finished),
            (
                vec![],
                vec![Change::Created(PathBuf::from("downloads/era5-2026-10.nc"))]
            )
        );
    }

    #[tokio::test]
    async fn reports_a_file_moved_out_of_the_project_as_deleted() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.join("a.csv"), "a").unwrap();
        let mut watcher = watch(root);

        // Some platforms report both ends of a move out of the watched folder.
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(root.join("a.csv"))
            .add_path(outside.path().join("a.csv"));
        let changes = watcher.handle(vec![DebouncedEvent::new(event, Instant::now())]);

        assert_eq!(changes, [Change::Deleted(PathBuf::from("a.csv"))]);
    }

    #[tokio::test]
    async fn stops_watching_files_a_renamed_folder_brings_under_a_rule() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join(".fair-witness")).unwrap();
        fs::write(root.join(".fair-witness/ignore"), "archive/*.csv\n").unwrap();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        fs::write(root.join("data/notes.txt"), "a").unwrap();
        let mut watcher = watch(root);

        fs::rename(root.join("data"), root.join("archive")).unwrap();
        assert_eq!(
            drain(&mut watcher).await,
            [Change::Renamed {
                from: PathBuf::from("data"),
                to: PathBuf::from("archive"),
            }]
        );
        fs::write(root.join("archive/a.csv"), "b").unwrap();
        fs::write(root.join("archive/notes.txt"), "b").unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Changed(PathBuf::from("archive/notes.txt"))]
        );
    }

    #[tokio::test]
    async fn reports_files_written_into_a_new_folder_once_each() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);

        fs::create_dir_all(root.join("data/raw")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        fs::write(root.join("data/raw/b.csv"), "b").unwrap();

        assert_eq!(
            created(&drain(&mut watcher).await),
            ["data", "data/a.csv", "data/raw", "data/raw/b.csv"].map(PathBuf::from),
        );
    }

    #[tokio::test]
    async fn reports_a_folder_moved_in_from_outside_with_its_contents() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("survey")).unwrap();
        fs::write(outside.path().join("survey/answers.csv"), "x").unwrap();
        let root = root.path();
        let mut watcher = watch(root);

        fs::rename(outside.path().join("survey"), root.join("survey")).unwrap();

        assert_eq!(
            created(&drain(&mut watcher).await),
            ["survey", "survey/answers.csv"].map(PathBuf::from),
        );
    }

    #[tokio::test]
    async fn reports_a_renamed_folder_as_one_rename() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        let mut watcher = watch(root);

        fs::rename(root.join("data"), root.join("dataset")).unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Renamed {
                from: PathBuf::from("data"),
                to: PathBuf::from("dataset"),
            }],
        );
    }

    #[tokio::test]
    async fn reports_a_deleted_folder() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        let mut watcher = watch(root);

        fs::remove_dir_all(root.join("data")).unwrap();

        let changes = drain(&mut watcher).await;
        assert!(
            changes.contains(&Change::Deleted(PathBuf::from("data"))),
            "{changes:?}"
        );
        assert!(created(&changes).is_empty(), "{changes:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_changes_behind_a_link_to_a_folder_outside_the_project() {
        let share = tempfile::tempdir().unwrap();
        let campaign = share.path().join("campaign");
        fs::create_dir_all(campaign.join("day-01")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::os::unix::fs::symlink(&campaign, root.join("raw")).unwrap();
        let mut watcher = watch(root);

        // In a folder that was there when watching started, and in one made since.
        fs::write(campaign.join("day-01/samples.csv"), "a").unwrap();
        fs::create_dir(campaign.join("day-02")).unwrap();
        fs::write(root.join("notes.md"), "Day 2 is on the share.").unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Created(PathBuf::from("notes.md"))]
        );
    }

    /// Asserts that the watcher of a project folder stops once `go`, called `way`, moved or
    /// deleted it.
    async fn assert_stops_once(way: &str, go: impl FnOnce(&Path)) {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("lab/project");
        fs::create_dir_all(&root).unwrap();
        let mut watcher = watch(&root);
        // Lets the events of reading the folder at the start pass, so that only `go` can stop it.
        assert!(
            tokio::time::timeout(QUIET, watcher.next()).await.is_err(),
            "{way}"
        );

        go(&root);

        let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        assert!(
            matches!(next, Ok(Some(Err(WatchError::Gone)))),
            "{way}: {next:?}"
        );
    }

    #[tokio::test]
    async fn stops_once_the_project_folder_is_moved_or_deleted() {
        assert_stops_once("moved", |root| {
            fs::rename(root, root.with_file_name("moved")).unwrap();
        })
        .await;
        assert_stops_once("deleted", |root| fs::remove_dir_all(root).unwrap()).await;
        // No event says that a folder above it moved.
        assert_stops_once("moved with its parent", |root| {
            let parent = root.parent().unwrap();
            fs::rename(parent, parent.with_file_name("moved")).unwrap();
        })
        .await;
        assert_stops_once("moved with its parent and then changed", |root| {
            let parent = root.parent().unwrap();
            let moved = parent.with_file_name("moved");
            fs::rename(parent, &moved).unwrap();
            fs::write(moved.join("project/notes.md"), "a").unwrap();
        })
        .await;
        assert_stops_once("moved with its parent and replaced", |root| {
            let parent = root.parent().unwrap();
            let moved = parent.with_file_name("moved");
            fs::rename(parent, &moved).unwrap();
            fs::create_dir_all(root).unwrap();
            fs::write(root.join("notes.md"), "new").unwrap();
            fs::write(moved.join("project/notes.md"), "a").unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn drops_the_set_it_holds_once_another_folder_takes_the_project_folders_place() {
        // Long enough that the set is surely held, and not yet complete, when the folder moves.
        let debounce = Duration::from_millis(400);
        let parent = tempfile::tempdir().unwrap();
        let lab = parent.path().join("lab");
        let root = lab.join("project");
        fs::create_dir_all(&root).unwrap();
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        fs::write(root.join("results.csv"), "a").unwrap();
        assert!(
            tokio::time::timeout(debounce * 2, watcher.next())
                .await
                .is_err()
        );
        // No event says so, since the watches went with the folder.
        fs::rename(&lab, parent.path().join("moved")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("results.csv"), "someone else's").unwrap();

        let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        assert!(matches!(next, Ok(Some(Err(WatchError::Gone)))), "{next:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keeps_watching_a_project_folder_closed_to_the_user_for_a_while() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let lab = parent.path().join("lab");
        let root = lab.join("project");
        fs::create_dir_all(&root).unwrap();
        let mut watcher = watch(&root);

        // The batch with the write comes while nobody can look into the project folder.
        fs::write(root.join("notes.md"), "a").unwrap();
        fs::set_permissions(&lab, fs::Permissions::from_mode(0o000)).unwrap();
        let next = tokio::time::timeout(QUIET, watcher.next()).await;
        fs::set_permissions(&lab, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!matches!(next, Ok(Some(Err(WatchError::Gone)))), "{next:?}");
    }

    #[tokio::test]
    async fn looks_through_the_folder_again_when_the_system_dropped_events() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for path in [
            "data/kept.csv",
            "data/edited.csv",
            "data/old/a.csv",
            "cache/a.csv",
        ] {
            write(root, path, "a");
        }
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        // So that the files are older than the set the dropped events belong to.
        tokio::time::sleep(DEBOUNCE * 4).await;

        // What the dropped events were about. A folder tagged as a cache is left out, as when
        // the tag's event comes, and nothing in it was deleted.
        write(root, "cache/CACHEDIR.TAG", CARGO_TAG);
        fs::remove_dir_all(root.join("data/old")).unwrap();
        write(root, "data/new/b.csv", "b");
        fs::write(root.join("data/edited.csv"), "b").unwrap();
        let dropped = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let batch = vec![DebouncedEvent::new(dropped, Instant::now())];
        batches.send((Instant::now(), Ok(batch))).unwrap();

        let mut next = async || {
            let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
            next.expect("nothing in time").unwrap()
        };
        let warning = next().await;
        assert!(
            matches!(&warning, Err(WatchError::Failed(errors)) if errors == &[MISSED]),
            "{warning:?}"
        );
        let path = PathBuf::from;
        assert_eq!(
            next().await.unwrap(),
            [
                Change::Deleted(path("data/old")),
                Change::Created(path("data/new")),
                Change::Created(path("data/new/b.csv")),
                Change::Changed(path("data/edited.csv")),
            ]
        );
        let known: BTreeSet<PathBuf> = ["data", "data/edited.csv", "data/kept.csv", "data/new"]
            .into_iter()
            .chain(["data/new/b.csv"])
            .map(|path| root.join(path))
            .chain([root.to_path_buf()])
            .collect();
        assert_eq!(watcher.known, known);

        // When nothing differs, as when the dropped events were reads, there is nothing to say.
        let dropped = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let batch = vec![DebouncedEvent::new(dropped, Instant::now())];
        batches.send((Instant::now(), Ok(batch))).unwrap();
        let next = tokio::time::timeout(QUIET, watcher.next()).await;
        assert!(next.is_err(), "{next:?}");
    }

    /// A copy whose create was dropped still keeps its set open while it is written, as one
    /// whose create came does.
    #[tokio::test]
    async fn keeps_a_copy_found_by_looking_again_in_one_set_while_it_is_written() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        let writer = std::thread::spawn({
            let path = root.join("interview.mp4");
            move || {
                let mut video = fs::File::create(path).unwrap();
                let started = Instant::now();
                while started.elapsed() < debounce * 5 {
                    video.write_all(&[0; 4096]).unwrap();
                    std::thread::sleep(debounce / 10);
                }
            }
        });
        tokio::time::sleep(debounce).await;
        // The debouncer hands the notice on a debounce after it came.
        let dropped = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let batch = vec![DebouncedEvent::new(dropped, Instant::now() - debounce)];
        batches.send((Instant::now(), Ok(batch))).unwrap();

        let mut sets = Vec::new();
        while let Ok(Some(next)) = tokio::time::timeout(debounce * 10, watcher.next()).await {
            match next {
                Ok(set) => sets.push((set, writer.is_finished())),
                Err(WatchError::Failed(errors)) => assert_eq!(errors, [MISSED]),
                Err(stopped) => panic!("{stopped:?}"),
            }
        }
        writer.join().unwrap();

        assert_eq!(
            sets,
            [(vec![Change::Created(PathBuf::from("interview.mp4"))], true)]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keeps_the_files_of_a_folder_it_cannot_look_into_when_it_looks_again() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "raw/a.csv", "a");
        let mut watcher = watch(root);
        let raw = root.join("raw");

        fs::set_permissions(&raw, fs::Permissions::from_mode(0o000)).unwrap();
        let found = watcher.rescan(Instant::now(), watcher.watched);
        fs::set_permissions(&raw, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(found, []);
        assert!(watcher.known.contains(&raw.join("a.csv")));
    }

    // Only on inotify does the watcher start anew.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stops_once_it_cannot_watch_the_folder_again_after_dropped_events() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "raw/a.csv", "a");
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        let raw = root.join("raw");

        // inotify cannot watch a folder the user cannot read.
        fs::set_permissions(&raw, fs::Permissions::from_mode(0o000)).unwrap();
        write(root, "notes.md", "a");
        let dropped = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let later = watcher.watched + DEBOUNCE * 2;
        tokio::time::sleep_until(later.into()).await;
        let batch = vec![DebouncedEvent::new(dropped, later)];
        batches.send((Instant::now(), Ok(batch))).unwrap();
        let mut reported = Vec::new();
        for _ in 0..3 {
            let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
            reported.push(next.expect("nothing in time").unwrap());
        }
        fs::set_permissions(&raw, fs::Permissions::from_mode(0o755)).unwrap();

        // What the look found still comes, with the warning before it.
        let [
            Err(WatchError::Failed(warning)),
            Ok(set),
            Err(WatchError::Stopped(error)),
        ] = &reported[..]
        else {
            panic!("{reported:?}");
        };
        assert_eq!(warning, &[MISSED]);
        assert_eq!(set, &[Change::Created(PathBuf::from("notes.md"))]);
        assert_eq!(
            *error,
            format!(
                "could not watch {} again after the system dropped events, so the project folder \
                 is no longer watched: Permission denied (os error 13)",
                raw.display()
            )
        );
        assert!(watcher.debouncer.is_none());
    }

    #[tokio::test]
    async fn counts_files_written_since_the_held_set_began_when_the_system_dropped_events() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["a.csv", "b.csv"] {
            write(root, name, "a");
        }
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        tokio::time::sleep(DEBOUNCE * 4).await;

        let began = Instant::now();
        // A write whose event was dropped.
        fs::write(root.join("b.csv"), "b").unwrap();
        tokio::time::sleep(DEBOUNCE * 2).await;
        // The notice comes more than a debounce after the held set's last event, as when notify
        // was busy, so its batch does not join the set.
        let edit =
            notify::Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.join("a.csv"));
        let dropped = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        for event in [
            DebouncedEvent::new(edit, began),
            DebouncedEvent::new(dropped, Instant::now()),
        ] {
            batches.send((Instant::now(), Ok(vec![event]))).unwrap();
        }

        let mut next = async || {
            let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
            next.expect("nothing in time").unwrap()
        };
        assert_eq!(
            next().await.unwrap(),
            [Change::Changed(PathBuf::from("a.csv"))]
        );
        let warning = next().await;
        assert!(
            matches!(&warning, Err(WatchError::Failed(errors)) if errors == &[MISSED]),
            "{warning:?}"
        );
        assert_eq!(
            next().await.unwrap(),
            [Change::Changed(PathBuf::from("b.csv"))]
        );
    }

    // Only inotify watches each folder, so only there can one go unwatched.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watches_anew_when_the_dropped_events_changed_nothing_it_sees() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "raw/a.csv", "a");
        let mut watcher = watch(root);
        // As if the folder had been deleted and restored with its old names and times while the
        // events were dropped, which leaves it unwatched.
        let debouncer = watcher.debouncer.as_mut().unwrap();
        debouncer.unwatch(root).unwrap();
        debouncer.watch(root, RecursiveMode::NonRecursive).unwrap();
        write(root, "notes.md", "a");

        // As when notify's own first look at every folder overflowed the queue, also when the
        // look finds a change.
        let started = watcher.watched;
        assert_eq!(
            watcher.rescan(Instant::now(), started),
            [Change::Created(PathBuf::from("notes.md"))]
        );
        assert_eq!(watcher.watched, started);
        let later = started + DEBOUNCE * 2;
        assert_eq!(watcher.rescan(Instant::now(), later), []);
        assert!(watcher.watched > started);
        fs::write(root.join("raw/a.csv"), "b").unwrap();
        assert_eq!(
            drain(&mut watcher).await,
            [Change::Changed(PathBuf::from("raw/a.csv"))]
        );
    }

    // Only inotify watches each folder, so only there can a new one go unwatched.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watches_the_folders_made_while_the_system_dropped_events() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "old/a.csv", "a");
        let mut watcher = watch(root);
        // As if the events of these changes had been dropped, and with them those that would
        // have had the new folders watched.
        let debouncer = watcher.debouncer.as_mut().unwrap();
        debouncer.unwatch(root).unwrap();
        write(root, "raw/a.csv", "a");
        // A folder made again where the watcher knows one.
        fs::remove_dir_all(root.join("old")).unwrap();
        write(root, "old/b.csv", "b");
        let debouncer = watcher.debouncer.as_mut().unwrap();
        debouncer.watch(root, RecursiveMode::NonRecursive).unwrap();

        let path = PathBuf::from;
        assert_eq!(
            watcher.rescan(Instant::now(), Instant::now() + DEBOUNCE * 2),
            [
                Change::Deleted(path("old/a.csv")),
                Change::Created(path("old/b.csv")),
                Change::Created(path("raw")),
                Change::Created(path("raw/a.csv")),
            ]
        );
        assert_eq!(watcher.pending, Vec::<String>::new());
        fs::write(root.join("raw/a.csv"), "b").unwrap();
        fs::write(root.join("old/b.csv"), "c").unwrap();
        assert_eq!(
            drain(&mut watcher).await,
            [
                Change::Changed(path("raw/a.csv")),
                Change::Changed(path("old/b.csv")),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn names_the_folder_the_limit_and_the_command_when_the_system_allows_no_more_watches() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        let raw = root.join("raw");
        let full = notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(raw.clone());
        batches.send((Instant::now(), Err(vec![full]))).unwrap();
        let reached = |setting: &str, what: &str| {
            let current: u64 = fs::read_to_string(format!("/proc/sys/fs/inotify/{setting}"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            format!(
                "the limit of {current} {what} per user (fs.inotify.{setting}) is reached. Raise \
                 it with `sudo sysctl fs.inotify.{setting}={}` and start `fair-witness run` again",
                current * 2
            )
        };

        let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        let Ok(Some(Err(WatchError::Failed(errors)))) = next else {
            panic!("{next:?}");
        };
        assert_eq!(
            errors,
            [format!(
                "could not watch {}, so changes in it go unseen: {}",
                raw.display(),
                reached("max_user_watches", "watched folders")
            )]
        );
        // As starting to watch fails when the system allows no more inotify instances.
        let too_many = notify::Error::io(io::Error::from_raw_os_error(24));
        assert_eq!(
            not_started(too_many, root).to_string(),
            format!(
                "could not watch {}: {}",
                root.display(),
                reached("max_user_instances", "inotify instances")
            )
        );
    }

    #[tokio::test]
    async fn reports_an_edited_file_as_changed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::write(root.join("notes.txt"), "a").unwrap();
        let mut watcher = watch(root);

        fs::write(root.join("notes.txt"), "b").unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Changed(PathBuf::from("notes.txt"))]
        );
    }

    #[tokio::test]
    async fn keeps_changes_less_than_the_debounce_apart_in_one_set() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let debounce = Duration::from_millis(300);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();

        // The debouncer hands on the first files before the last ones are written.
        let names: Vec<String> = (1..=10).map(|run| format!("run-{run}.csv")).collect();
        for name in &names {
            fs::write(root.join(name), "x").unwrap();
            tokio::time::sleep(debounce / 4).await;
        }

        let set = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        assert_eq!(
            set.unwrap().unwrap().unwrap(),
            names
                .iter()
                .map(|name| Change::Created(PathBuf::from(name)))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn ends_a_set_in_a_folder_that_keeps_changing() {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::write(root.join("run.log"), "").unwrap();
        // Long enough that a busy machine does not pause the writes for that long.
        let debounce = Duration::from_millis(100);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();

        let writing = Arc::new(AtomicBool::new(true));
        let writer = std::thread::spawn({
            let log = root.join("run.log");
            let writing = writing.clone();
            // The debouncer holds back the writes to a file that stays open until they pause, so
            // each write opens it again, as `>>` in a shell loop does.
            move || {
                while writing.load(Ordering::Relaxed) {
                    let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
                    file.write_all(b"step\n").unwrap();
                    std::thread::sleep(debounce / 10);
                }
            }
        });
        let started = Instant::now();
        let set = tokio::time::timeout(Duration::from_secs(10), watcher.next()).await;
        let waited = started.elapsed();
        writing.store(false, Ordering::Relaxed);
        writer.join().unwrap();

        assert_eq!(
            set.expect("no set while the folder kept changing")
                .unwrap()
                .unwrap(),
            [Change::Changed(PathBuf::from("run.log"))]
        );
        assert!(waited >= debounce * LONGEST_SET, "{waited:?}");
    }

    #[tokio::test]
    async fn keeps_a_new_file_in_one_set_while_it_is_written() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        // Long enough that a busy machine does not pause the writes for that long.
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();

        // As a copy writes a large file, for five debounces, opening it for each write while the
        // session waits for the set. The debouncer drops the writes while it still holds the
        // file's create.
        let writer = std::thread::spawn({
            let video = root.join("interview.mp4");
            move || {
                let started = Instant::now();
                while started.elapsed() < debounce * 5 {
                    let mut file = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&video)
                        .unwrap();
                    file.write_all(&[0; 4096]).unwrap();
                    drop(file);
                    std::thread::sleep(debounce / 10);
                }
            }
        });
        // Long enough for the set to come a few debounces after the writes, and a second one.
        let mut sets = Vec::new();
        while let Ok(Some(set)) = tokio::time::timeout(debounce * 10, watcher.next()).await {
            sets.push(set.unwrap());
        }
        writer.join().unwrap();

        assert_eq!(sets, [[Change::Created(PathBuf::from("interview.mp4"))]]);
    }

    #[tokio::test]
    async fn keeps_a_new_file_held_open_in_one_set_while_it_is_written() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();

        // As `cp` writes, keeping the file open for five debounces. The debouncer then hands on
        // none of the writes until they pause, while the session waits for the set.
        let writer = std::thread::spawn({
            let path = root.join("interview.mp4");
            move || {
                let mut video = fs::File::create(path).unwrap();
                let started = Instant::now();
                while started.elapsed() < debounce * 5 {
                    video.write_all(&[0; 4096]).unwrap();
                    std::thread::sleep(debounce / 10);
                }
            }
        });
        let mut sets = Vec::new();
        // Long enough for the set to come a few debounces after the writes, and a second one.
        while let Ok(Some(set)) = tokio::time::timeout(debounce * 10, watcher.next()).await {
            sets.push(set.unwrap());
        }
        writer.join().unwrap();

        assert_eq!(sets, [[Change::Created(PathBuf::from("interview.mp4"))]]);
    }

    #[tokio::test]
    async fn counts_writes_to_a_new_file_only_until_its_create_is_handed_on() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        fs::write(root.join("results.csv"), "x").unwrap();

        // The watcher handles both batches late, once the file holds the second write. That
        // write came after a pause longer than the debounce, though less than a debounce after
        // the debouncer handed the create on, so it starts a set of its own.
        let start = Instant::now() - DEBOUNCE * 10;
        let event = |kind, time| {
            DebouncedEvent::new(
                notify::Event::new(kind).add_path(root.join("results.csv")),
                time,
            )
        };
        let create = event(EventKind::Create(notify::event::CreateKind::File), start);
        let edit = event(EventKind::Modify(ModifyKind::Any), start + DEBOUNCE * 3 / 2);
        batches.send((start + DEBOUNCE, Ok(vec![create]))).unwrap();
        batches
            .send((start + DEBOUNCE * 3, Ok(vec![edit])))
            .unwrap();

        let first = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        let second = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        assert_eq!(
            [
                first.unwrap().unwrap().unwrap(),
                second.unwrap().unwrap().unwrap()
            ],
            [
                [Change::Created(PathBuf::from("results.csv"))],
                [Change::Changed(PathBuf::from("results.csv"))],
            ]
        );
    }

    #[tokio::test]
    async fn ends_a_set_with_a_file_dated_in_the_future_in_time() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);

        // As an archive extracted with the dates it was made with, by a clock that ran ahead.
        let file = fs::File::create(root.join("results.csv")).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        drop(file);
        let started = Instant::now();
        let set = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;

        assert_eq!(
            set.unwrap().unwrap().unwrap(),
            [Change::Created(PathBuf::from("results.csv"))]
        );
        assert!(started.elapsed() < DEBOUNCE * 10, "{:?}", started.elapsed());
    }

    /// The sets `watcher` gives while `write` runs on a thread, until none comes for `wait`.
    async fn sets_while(
        watcher: &mut FolderWatcher,
        wait: Duration,
        write: impl FnOnce() + Send + 'static,
    ) -> Vec<Vec<Change>> {
        let writer = std::thread::spawn(write);
        let mut sets = Vec::new();
        while let Ok(Some(set)) = tokio::time::timeout(wait, watcher.next()).await {
            sets.push(set.unwrap());
        }
        writer.join().unwrap();
        sets
    }

    #[tokio::test]
    async fn keeps_a_copy_in_one_set_while_ignored_files_keep_coming() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        // Batches of the ignored files keep coming, so the watcher must look at the copy by the
        // clock, not when the batches pause.
        let sets = sets_while(&mut watcher, debounce * 15, move || {
            let mut copy = fs::File::create(root.join("video.mp4")).unwrap();
            let started = Instant::now();
            let mut swap = 0;
            while started.elapsed() < debounce * 10 {
                copy.write_all(&[0; 4096]).unwrap();
                swap += 1;
                fs::write(root.join(format!(".notes-{swap}.swp")), "x").unwrap();
                std::thread::sleep(debounce / 20);
            }
        })
        .await;

        assert_eq!(sets, [[Change::Created(PathBuf::from("video.mp4"))]]);
    }

    #[tokio::test]
    async fn ends_a_set_once_its_growing_file_is_in_a_cache() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        // The log's folder is tagged as a cache while the log grows, so the log stops counting
        // and the second file, after a pause longer than the debounce, starts a set of its own.
        let sets = sets_while(&mut watcher, debounce * 15, move || {
            fs::write(root.join("results.csv"), "x").unwrap();
            fs::create_dir(root.join("data")).unwrap();
            let mut log = fs::File::create(root.join("data/run.log")).unwrap();
            let started = Instant::now();
            let (mut tagged, mut second) = (false, false);
            while started.elapsed() < debounce * 10 {
                if !tagged && started.elapsed() > debounce * 5 / 2 {
                    fs::write(root.join("data/CACHEDIR.TAG"), CARGO_TAG).unwrap();
                    tagged = true;
                }
                if !second && started.elapsed() > debounce * 6 {
                    fs::write(root.join("notes.txt"), "y").unwrap();
                    second = true;
                }
                log.write_all(b"step\n").unwrap();
                std::thread::sleep(debounce / 10);
            }
        })
        .await;

        assert_eq!(sets.len(), 2, "{sets:?}");
        assert_eq!(sets[1], [Change::Created(PathBuf::from("notes.txt"))]);
    }

    #[tokio::test]
    async fn keeps_a_file_in_one_set_when_its_folder_is_renamed_while_it_is_written() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        // The writes go on through the open file after its folder gets another name.
        let sets = sets_while(&mut watcher, debounce * 15, move || {
            fs::create_dir(root.join("data")).unwrap();
            let mut log = fs::File::create(root.join("data/run.log")).unwrap();
            let started = Instant::now();
            let mut renamed = false;
            while started.elapsed() < debounce * 10 {
                if !renamed && started.elapsed() > debounce * 5 / 2 {
                    fs::rename(root.join("data"), root.join("runs")).unwrap();
                    renamed = true;
                }
                log.write_all(b"step\n").unwrap();
                std::thread::sleep(debounce / 10);
            }
        })
        .await;

        assert_eq!(sets.len(), 1, "{sets:?}");
    }

    #[tokio::test]
    async fn keeps_a_file_in_one_set_when_its_existing_folder_is_renamed_while_it_grows() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/run.log"), "start\n").unwrap();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        // The log grows through its open file before and after its folder gets another name.
        let sets = sets_while(&mut watcher, debounce * 15, move || {
            let mut log = fs::OpenOptions::new()
                .append(true)
                .open(root.join("data/run.log"))
                .unwrap();
            let started = Instant::now();
            let mut renamed = false;
            while started.elapsed() < debounce * 10 {
                if !renamed && started.elapsed() > debounce * 5 / 2 {
                    fs::rename(root.join("data"), root.join("runs")).unwrap();
                    renamed = true;
                }
                log.write_all(b"step\n").unwrap();
                std::thread::sleep(debounce / 10);
            }
        })
        .await;

        assert_eq!(sets.len(), 1, "{sets:?}");
    }

    #[tokio::test]
    async fn keeps_a_file_sized_in_advance_in_one_set_while_it_is_written() {
        use std::io::{Seek, SeekFrom, Write};

        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        // As a download that sets the file's size first and then fills it in, so the size never
        // changes while it is written.
        let sets = sets_while(&mut watcher, debounce * 15, move || {
            let mut file = fs::File::create(root.join("reads.fastq.gz")).unwrap();
            file.set_len(1 << 20).unwrap();
            let started = Instant::now();
            let mut offset = 0;
            while started.elapsed() < debounce * 8 {
                file.seek(SeekFrom::Start(offset)).unwrap();
                file.write_all(&[1; 4096]).unwrap();
                offset = (offset + 4096) % (1 << 20);
                std::thread::sleep(debounce / 10);
            }
        })
        .await;

        assert_eq!(sets, [[Change::Created(PathBuf::from("reads.fastq.gz"))]]);
    }

    #[tokio::test]
    async fn keeps_no_set_open_for_a_file_that_is_only_touched() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().to_path_buf();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(&root, debounce).unwrap();

        // A new time alone writes nothing, so the second file, after a pause longer than the
        // debounce, starts a set of its own.
        let sets = sets_while(&mut watcher, debounce * 15, move || {
            fs::write(root.join("notes.txt"), "x").unwrap();
            let started = Instant::now();
            let mut second = false;
            while started.elapsed() < debounce * 8 {
                if !second && started.elapsed() > debounce * 6 {
                    fs::write(root.join("results.csv"), "y").unwrap();
                    second = true;
                }
                // Both times, as `touch` sets them, which the kernel reports as a change to
                // the file's metadata, not as a write.
                let now = SystemTime::now();
                let times = fs::FileTimes::new().set_accessed(now).set_modified(now);
                let notes = fs::File::open(root.join("notes.txt")).unwrap();
                notes.set_times(times).unwrap();
                std::thread::sleep(debounce / 10);
            }
        })
        .await;

        assert_eq!(
            sets,
            [
                [Change::Created(PathBuf::from("notes.txt"))],
                [Change::Created(PathBuf::from("results.csv"))],
            ]
        );
    }

    #[tokio::test]
    async fn starts_a_new_set_for_a_write_after_a_pause() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let debounce = Duration::from_millis(200);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();

        // The second write comes while the first set is still held, but after the folder was
        // quiet for longer than a debounce, so looking at the file must not join them.
        let writer = std::thread::spawn({
            let path = root.join("notes.txt");
            move || {
                fs::write(&path, "a").unwrap();
                std::thread::sleep(debounce * 3 / 2);
                fs::write(&path, "b").unwrap();
            }
        });
        let mut sets = Vec::new();
        while let Ok(Some(set)) = tokio::time::timeout(debounce * 10, watcher.next()).await {
            sets.push(set.unwrap());
        }
        writer.join().unwrap();

        assert_eq!(
            sets,
            [
                [Change::Created(PathBuf::from("notes.txt"))],
                [Change::Changed(PathBuf::from("notes.txt"))],
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_file_replaced_by_a_rename_as_changed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let outside = tempfile::tempdir().unwrap();
        for name in ["saved.csv", "moved.csv"] {
            fs::write(root.join(name), "a").unwrap();
        }
        let mut watcher = watch(root);

        // As `sed -i` saves: it reads the old file, writes a new one and renames that over it.
        fs::read(root.join("saved.csv")).unwrap();
        fs::write(root.join(".saved.csv.tmp"), "b").unwrap();
        fs::rename(root.join(".saved.csv.tmp"), root.join("saved.csv")).unwrap();
        // As a file manager replaces a file with one moved in from elsewhere.
        fs::write(outside.path().join("moved.csv"), "b").unwrap();
        fs::rename(outside.path().join("moved.csv"), root.join("moved.csv")).unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        assert_eq!(
            changes,
            ["moved.csv", "saved.csv"].map(|name| Change::Changed(PathBuf::from(name)))
        );
    }

    #[tokio::test]
    async fn reports_a_file_renamed_over_another_as_replacing_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["draft.csv", "final.csv", "old.csv", "new.csv", "notes.csv"] {
            fs::write(root.join(name), name).unwrap();
        }
        let mut watcher = watch(root);

        // Once with events of its own on the file that is replaced, and once without.
        fs::read(root.join("final.csv")).unwrap();
        fs::rename(root.join("draft.csv"), root.join("final.csv")).unwrap();
        fs::rename(root.join("new.csv"), root.join("old.csv")).unwrap();
        // From a name the watcher leaves out, as an editor's backup.
        fs::write(root.join("notes.csv~"), "b").unwrap();
        tokio::time::sleep(DEBOUNCE * 2).await;
        fs::rename(root.join("notes.csv~"), root.join("notes.csv")).unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Changed(path("final.csv")),
                Change::Changed(path("notes.csv")),
                Change::Changed(path("old.csv")),
                Change::Renamed {
                    from: path("draft.csv"),
                    to: path("final.csv"),
                },
                Change::Renamed {
                    from: path("new.csv"),
                    to: path("old.csv"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_swap_through_a_temporary_name_as_two_renames() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["site-a.csv", "site-b.csv"] {
            fs::write(root.join(name), name).unwrap();
        }
        let mut watcher = watch(root);

        fs::rename(root.join("site-a.csv"), root.join("swap.tmp")).unwrap();
        fs::rename(root.join("site-b.csv"), root.join("site-a.csv")).unwrap();
        fs::rename(root.join("swap.tmp"), root.join("site-b.csv")).unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Renamed {
                    from: path("site-a.csv"),
                    to: path("site-b.csv"),
                },
                Change::Renamed {
                    from: path("site-b.csv"),
                    to: path("site-a.csv"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn keeps_a_new_folder_in_one_set_while_it_fills() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let debounce = Duration::from_millis(100);
        let mut watcher = FolderWatcher::new(root, debounce).unwrap();

        // As a slow unzip or a download into a new folder. Walking the folder when it appears
        // finds the first files before their creates come, and those creates still show that
        // the folder is busy.
        let writer = std::thread::spawn({
            let folder = root.join("stream");
            move || {
                fs::create_dir(&folder).unwrap();
                for i in 0..80 {
                    fs::write(folder.join(format!("f-{i:02}.csv")), "x").unwrap();
                    std::thread::sleep(debounce / 8);
                }
            }
        });
        let mut sets = Vec::new();
        // Long enough for the set to come after ten debounces of writes, and a second one.
        while let Ok(Some(set)) = tokio::time::timeout(debounce * 20, watcher.next()).await {
            sets.push(set.unwrap());
        }
        writer.join().unwrap();

        assert_eq!(sets.len(), 1, "{sets:?}");
        assert_eq!(created(&sets[0]).len(), 81);
    }

    #[tokio::test]
    async fn reports_a_file_as_deleted_when_the_file_renamed_over_it_moves_on() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        // A folder whose name sorts before the files, which the look for what is missing skips.
        write(root, "a/notes.csv", "n");
        for name in ["read.csv", "unread.csv"] {
            fs::write(root.join(name), "old").unwrap();
            fs::write(root.join(format!("{name}.new~")), "new").unwrap();
        }
        let mut watcher = watch(root);

        // A backup is renamed over a file the watcher knows and moves on to another backup name,
        // all within a debounce, once after something read the file and once without. Both
        // backup names are ignored, so only the file the watcher knew is left to report, and it
        // is gone. The debouncer joins the renames into one between the backup names and says
        // that the file is gone only when it has events of its own, from a read in the same
        // debounce, which can also come in an earlier batch.
        fs::read(root.join("read.csv")).unwrap();
        for name in ["read.csv", "unread.csv"] {
            fs::rename(root.join(format!("{name}.new~")), root.join(name)).unwrap();
            fs::rename(root.join(name), root.join(format!("{name}.old~"))).unwrap();
        }

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        assert_eq!(
            changes,
            ["read.csv", "unread.csv"].map(|name| Change::Deleted(PathBuf::from(name)))
        );
    }

    #[tokio::test]
    async fn reports_a_replaced_file_where_its_folder_went() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["read.csv", "unread.csv"] {
            write(root, &format!("data/{name}"), "old");
            write(root, &format!("data/{name}~"), "new");
        }
        let mut watcher = watch(root);

        // A backup is renamed over a file the watcher knows, once after something read it and
        // once without, and the folder gets another name, all within a debounce. Either way the
        // file changed where it was, and the folder's rename takes it along.
        fs::read(root.join("data/read.csv")).unwrap();
        for name in ["read.csv", "unread.csv"] {
            let file = root.join("data").join(name);
            fs::rename(file.with_file_name(format!("{name}~")), file).unwrap();
        }
        fs::rename(root.join("data"), root.join("renamed")).unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Changed(path("data/read.csv")),
                Change::Changed(path("data/unread.csv")),
                Change::Renamed {
                    from: path("data"),
                    to: path("renamed"),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_a_file_replaced_by_a_link_that_points_nowhere_as_changed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["read.csv", "unread.csv"] {
            fs::write(root.join(name), "x").unwrap();
        }
        for name in ["read.csv~", "unread.csv~"] {
            std::os::unix::fs::symlink(root.join("nowhere"), root.join(name)).unwrap();
        }
        let mut watcher = watch(root);

        // Once with events of its own on the file that is replaced, and once without.
        fs::read(root.join("read.csv")).unwrap();
        fs::rename(root.join("read.csv~"), root.join("read.csv")).unwrap();
        fs::rename(root.join("unread.csv~"), root.join("unread.csv")).unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        assert_eq!(
            changes,
            ["read.csv", "unread.csv"].map(|name| Change::Changed(PathBuf::from(name)))
        );
    }

    #[tokio::test]
    async fn reports_a_swap_and_a_rename_after_it_as_renames() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["site-a.csv", "site-b.csv"] {
            fs::write(root.join(name), name).unwrap();
        }
        let [mut together, mut apart] = [watch(root), watch(root)];
        fs::rename(root.join("site-a.csv"), root.join("swap.tmp")).unwrap();
        fs::rename(root.join("site-b.csv"), root.join("site-a.csv")).unwrap();
        fs::rename(root.join("swap.tmp"), root.join("site-b.csv")).unwrap();
        fs::rename(root.join("site-a.csv"), root.join("site-c.csv")).unwrap();

        // The debouncer joins those steps into a rename from site-a.csv to site-b.csv and one
        // from site-b.csv to site-c.csv, and its tick can hand them on together or one at a time.
        // Either way nothing was written over site-b.csv, though site-a.csv, the first rename's
        // source, is gone.
        let rename = |from: &str, to: &str| {
            let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(root.join(from))
                .add_path(root.join(to));
            DebouncedEvent::new(event, Instant::now())
        };
        let (first, second) = (
            rename("site-a.csv", "site-b.csv"),
            rename("site-b.csv", "site-c.csv"),
        );
        let mut sets = [
            together.handle(vec![first.clone(), second.clone()]),
            [apart.handle(vec![first]), apart.handle(vec![second])].concat(),
        ];
        // Once every event that could settle them has come.
        let later = Instant::now() + DEBOUNCE * 2;
        for (watcher, changes) in [&mut together, &mut apart].into_iter().zip(&mut sets) {
            changes.extend(
                watcher
                    .settle(later)
                    .into_iter()
                    .flat_map(|found| found.changes),
            );
        }

        let path = PathBuf::from;
        let renames = [
            Change::Renamed {
                from: path("site-a.csv"),
                to: path("site-b.csv"),
            },
            Change::Renamed {
                from: path("site-b.csv"),
                to: path("site-c.csv"),
            },
        ];
        assert_eq!(sets, [renames.clone(), renames]);
    }

    #[tokio::test]
    async fn keeps_a_new_file_where_a_replaced_one_moved_away_from() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "data/b.csv", "b");
        write(root, "data/a.csv~", "a");
        let mut watcher = watch(root);

        // The replaced file moves away with its folder, and a new folder of that name gets a new
        // file of that name, all within a debounce. The new file is another one.
        fs::read(root.join("data/b.csv")).unwrap();
        fs::rename(root.join("data/a.csv~"), root.join("data/b.csv")).unwrap();
        fs::rename(root.join("data"), root.join("renamed")).unwrap();
        write(root, "data/b.csv", "new");

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Changed(path("data/b.csv")),
                Change::Created(path("data")),
                Change::Created(path("data/b.csv")),
                Change::Renamed {
                    from: path("data"),
                    to: path("renamed"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_file_renamed_over_another_as_replacing_it_when_its_name_is_reused() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["draft.csv", "final.csv"] {
            fs::write(root.join(name), name).unwrap();
        }
        let mut watcher = watch(root);

        // The draft replaces the final version, and a new draft starts right away.
        fs::rename(root.join("draft.csv"), root.join("final.csv")).unwrap();
        fs::write(root.join("draft.csv"), "next").unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Changed(path("final.csv")),
                Change::Created(path("draft.csv")),
                Change::Renamed {
                    from: path("draft.csv"),
                    to: path("final.csv"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn tells_a_replacement_from_a_swap_whatever_batches_their_renames_come_in() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["draft.csv", "final.csv", "site-a.csv", "site-b.csv"] {
            fs::write(root.join(name), name).unwrap();
        }
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;

        // The draft replaces the final version and a new draft starts, and two sites swap names
        // through a temporary one.
        fs::rename(root.join("draft.csv"), root.join("final.csv")).unwrap();
        fs::write(root.join("draft.csv"), "next").unwrap();
        fs::rename(root.join("site-a.csv"), root.join("swap.tmp")).unwrap();
        fs::rename(root.join("site-b.csv"), root.join("site-a.csv")).unwrap();
        fs::rename(root.join("swap.tmp"), root.join("site-b.csv")).unwrap();
        // The debouncer joins the swap into two renames. Its tick can hand on the rename onto the
        // final version before the new draft's create, and the swap's renames one at a time, so
        // that the first batch alone cannot tell the replacement from the swap.
        let start = Instant::now() - DEBOUNCE * 10;
        let event = |kind, names: &[&str], quarters: u32| {
            let event = names.iter().fold(notify::Event::new(kind), |event, name| {
                event.add_path(root.join(name))
            });
            DebouncedEvent::new(event, start + DEBOUNCE * quarters / 4)
        };
        let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        let create = EventKind::Create(notify::event::CreateKind::File);
        for batch in [
            vec![
                event(rename, &["draft.csv", "final.csv"], 0),
                event(rename, &["site-a.csv", "site-b.csv"], 0),
            ],
            vec![
                event(create, &["draft.csv"], 1),
                event(rename, &["site-b.csv", "site-a.csv"], 1),
            ],
        ] {
            batches.send((Instant::now(), Ok(batch))).unwrap();
        }

        let path = PathBuf::from;
        assert_eq!(
            watcher.next().await.unwrap().unwrap(),
            [
                Change::Renamed {
                    from: path("draft.csv"),
                    to: path("final.csv"),
                },
                Change::Renamed {
                    from: path("site-a.csv"),
                    to: path("site-b.csv"),
                },
                Change::Created(path("draft.csv")),
                Change::Renamed {
                    from: path("site-b.csv"),
                    to: path("site-a.csv"),
                },
                Change::Changed(path("final.csv")),
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_replaced_file_as_changed_before_a_later_rename_moves_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["notes.csv", "notes-new.csv"] {
            fs::write(root.join(name), name).unwrap();
        }
        let mut watcher = watch(root);
        fs::rename(root.join("notes-new.csv"), root.join("notes.csv")).unwrap();
        fs::rename(root.join("notes.csv"), root.join("archive.csv")).unwrap();

        // More than a debounce apart, so the second rename is no step of a swap.
        let start = Instant::now() - DEBOUNCE * 10;
        let rename = |from: &str, to: &str, after: Duration| {
            let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(root.join(from))
                .add_path(root.join(to));
            DebouncedEvent::new(event, start + after)
        };
        let changes = [
            watcher.handle(vec![rename("notes-new.csv", "notes.csv", Duration::ZERO)]),
            watcher.handle(vec![rename("notes.csv", "archive.csv", DEBOUNCE * 3 / 2)]),
        ]
        .concat();

        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Renamed {
                    from: path("notes-new.csv"),
                    to: path("notes.csv"),
                },
                Change::Changed(path("notes.csv")),
                Change::Renamed {
                    from: path("notes.csv"),
                    to: path("archive.csv"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_replaced_file_as_changed_where_a_renamed_folder_took_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "draft.csv", "draft");
        write(root, "data/final.csv", "final");
        let mut watchers = [(); 4].map(|_| watch(root)).into_iter();
        fs::rename(root.join("draft.csv"), root.join("data/final.csv")).unwrap();
        write(root, "data/final.csv", "edited");
        fs::rename(root.join("data"), root.join("archive")).unwrap();

        // Once without the write in between and once with it, which names the file where it no
        // longer is. Each is handed on together or an event at a time, with nothing renamed away
        // from data/final.csv.
        let event = |kind, names: &[&str]| {
            let event = names.iter().fold(notify::Event::new(kind), |event, name| {
                event.add_path(root.join(name))
            });
            DebouncedEvent::new(event, Instant::now())
        };
        let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        let (first, write, second) = (
            event(rename, &["draft.csv", "data/final.csv"]),
            event(EventKind::Modify(ModifyKind::Any), &["data/final.csv"]),
            event(rename, &["data", "archive"]),
        );
        let later = Instant::now() + DEBOUNCE * 2;
        let mut sets = Vec::new();
        for events in [
            vec![first.clone(), second.clone()],
            vec![first, write, second],
        ] {
            let one_at_a_time = events.iter().map(|event| vec![event.clone()]).collect();
            for batches in [vec![events], one_at_a_time] {
                let mut watcher = watchers.next().unwrap();
                let mut changes = Vec::new();
                for batch in batches {
                    changes.extend(watcher.handle(batch));
                }
                changes.extend(
                    watcher
                        .settle(later)
                        .into_iter()
                        .flat_map(|found| found.changes),
                );
                sets.push(changes);
            }
        }

        let path = PathBuf::from;
        let renamed = |from: &str, to: &str| Change::Renamed {
            from: PathBuf::from(from),
            to: PathBuf::from(to),
        };
        let plain = [
            renamed("draft.csv", "data/final.csv"),
            renamed("data", "archive"),
            Change::Changed(path("archive/final.csv")),
        ];
        let written = [
            renamed("draft.csv", "data/final.csv"),
            Change::Changed(path("data/final.csv")),
            renamed("data", "archive"),
        ];
        assert_eq!(
            sets,
            [&plain, &plain, &written, &written].map(|set| set.to_vec())
        );
    }

    #[tokio::test]
    async fn keeps_a_rename_of_a_file_found_before_its_create_came() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);
        let event = |kind, names: &[&str]| {
            let event = names.iter().fold(notify::Event::new(kind), |event, name| {
                event.add_path(root.join(name))
            });
            DebouncedEvent::new(event, Instant::now())
        };
        let create = EventKind::Create(notify::event::CreateKind::Any);

        // Walking the new folder finds the file. It is renamed after its create was handed on
        // and before the watcher handles it, so the create names a path that is gone, and the
        // rename's event comes later.
        write(root, "data/a.csv", "a");
        let mut changes = watcher.handle(vec![event(create, &["data"])]);
        fs::rename(root.join("data/a.csv"), root.join("data/b.csv")).unwrap();
        changes.extend(watcher.handle(vec![event(create, &["data/a.csv"])]));
        let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        changes.extend(watcher.handle(vec![event(rename, &["data/a.csv", "data/b.csv"])]));

        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Created(path("data")),
                Change::Created(path("data/a.csv")),
                Change::Changed(path("data/a.csv")),
                Change::Renamed {
                    from: path("data/a.csv"),
                    to: path("data/b.csv"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn keeps_a_rename_whose_event_comes_after_a_change_beside_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::write(root.join("notes.csv"), "a").unwrap();
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;

        fs::write(root.join("results.csv"), "x").unwrap();
        fs::rename(root.join("notes.csv"), root.join("notes-2026.csv")).unwrap();
        // The new file's create comes in a batch before the rename, when notes.csv is already
        // gone from the folder.
        let start = Instant::now() - DEBOUNCE * 10;
        let create = notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(root.join("results.csv"));
        let rename = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(root.join("notes.csv"))
            .add_path(root.join("notes-2026.csv"));
        for batch in [
            DebouncedEvent::new(create, start),
            DebouncedEvent::new(rename, start + DEBOUNCE / 4),
        ] {
            batches.send((Instant::now(), Ok(vec![batch]))).unwrap();
        }

        assert_eq!(
            watcher.next().await.unwrap().unwrap(),
            [
                Change::Created(PathBuf::from("results.csv")),
                Change::Renamed {
                    from: PathBuf::from("notes.csv"),
                    to: PathBuf::from("notes-2026.csv"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn sorts_changes_into_sets_by_when_the_files_were_written() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["a.csv", "b.csv", "c.csv"] {
            fs::write(root.join(name), "a").unwrap();
        }
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;

        // These all waited in the channel, as while the watcher handles a large batch.
        let start = Instant::now() - DEBOUNCE * 10;
        let event = |kind, name: &str, quarters: u32| {
            let event = notify::Event::new(kind).add_path(root.join(name));
            DebouncedEvent::new(event, start + DEBOUNCE * quarters / 4)
        };
        let edit = EventKind::Modify(ModifyKind::Any);
        let read = EventKind::Access(notify::event::AccessKind::Read);
        for batch in [
            vec![event(edit, "a.csv", 0)],
            vec![event(edit, "b.csv", 2)],
            // A debounce and a half after the last edit, so c.csv starts the next set even
            // though b.csv was read before a debounce had passed.
            vec![event(read, "b.csv", 5), event(edit, "c.csv", 8)],
        ] {
            batches.send((Instant::now(), Ok(batch))).unwrap();
        }

        assert_eq!(
            watcher.next().await.unwrap().unwrap(),
            ["a.csv", "b.csv"].map(|name| Change::Changed(PathBuf::from(name)))
        );
        assert_eq!(
            watcher.next().await.unwrap().unwrap(),
            [Change::Changed(PathBuf::from("c.csv"))]
        );
    }

    #[tokio::test]
    async fn reports_a_file_written_into_a_new_folder_only_as_created() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = watch(root);

        write(root, "analysis/plot_ph.py", "x");
        // As inotify can report `mkdir -p analysis` and a file written into it right after,
        // without the file's create.
        let folder = notify::Event::new(EventKind::Create(notify::event::CreateKind::Folder))
            .add_path(root.join("analysis"));
        let edit = notify::Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(root.join("analysis/plot_ph.py"));
        let changes = watcher.handle(vec![
            DebouncedEvent::new(folder, Instant::now()),
            DebouncedEvent::new(edit, Instant::now()),
        ]);

        assert_eq!(
            changes,
            ["analysis", "analysis/plot_ph.py"].map(|path| Change::Created(PathBuf::from(path)))
        );
    }

    #[test]
    fn finds_the_paths_right_inside_a_folder_whatever_their_names() {
        let paths: BTreeSet<PathBuf> = [
            "data",
            "data/x",
            "data/x/deep/a.csv",
            "data/x-y.csv",
            "data/x.csv",
            "data/x0/b.csv",
            "data/y",
            "data2/c.csv",
        ]
        .map(PathBuf::from)
        .into();

        // A name that starts with another sorts after what is in the other, and a folder only
        // known by what is in it is no path of its own.
        assert_eq!(
            children(&paths, Path::new("data")),
            ["data/x", "data/x-y.csv", "data/x.csv", "data/y"].map(PathBuf::from)
        );
    }

    #[tokio::test]
    async fn lists_a_file_written_again_in_a_set_once() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::write(root.join("notes.txt"), "a").unwrap();
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;
        fs::write(root.join("results.csv"), "x").unwrap();

        let start = Instant::now() - DEBOUNCE * 10;
        let event = |kind, name: &str, time| {
            DebouncedEvent::new(notify::Event::new(kind).add_path(root.join(name)), time)
        };
        let create = EventKind::Create(notify::event::CreateKind::File);
        let edit = EventKind::Modify(ModifyKind::Any);
        for batch in [
            vec![
                event(create, "results.csv", start),
                event(edit, "notes.txt", start),
            ],
            vec![
                event(edit, "results.csv", start + DEBOUNCE / 2),
                event(edit, "notes.txt", start + DEBOUNCE / 2),
            ],
        ] {
            batches.send((Instant::now(), Ok(batch))).unwrap();
        }

        assert_eq!(
            watcher.next().await.unwrap().unwrap(),
            [
                Change::Created(PathBuf::from("results.csv")),
                Change::Changed(PathBuf::from("notes.txt")),
            ]
        );
    }
}
