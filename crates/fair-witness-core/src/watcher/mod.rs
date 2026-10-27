mod filter;
mod summary;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::mem;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use notify_debouncer_full::file_id::{self, FileId};
use notify_debouncer_full::notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::notify::{
    self, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _, WatcherKind,
};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer_opt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use filter::CACHE_TAG;
pub use filter::Filter;
pub use summary::{Listed, summarize};

/// How long the folder stays quiet before its changes count as one change set, unless a
/// caller asks for another time.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// A folder that never stays quiet for the debounce, such as during a long copy, still gets a
/// change set once its first change is this many debounces old.
const LONGEST_SET: u32 = 30;
/// How often per debounce the watcher looks at the files of a held set.
const LOOKS: u32 = 4;
/// The debouncer checks for quiet every quarter of the wait, so a wait of zero would have it
/// spin.
const SHORTEST_DEBOUNCE: Duration = Duration::from_millis(10);
/// What the watcher reports when the system dropped events, before the change set that holds
/// what it found by looking through the folder again.
const MISSED: &str = "too many changes came at once and the system dropped some of their \
                      events, so the folder was looked through again, and the next change set \
                      lists what was created, deleted or written meanwhile; a change undone in \
                      between is not seen";

/// A change in the watched folder, with paths relative to its root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Created(#[serde(with = "escaped")] PathBuf),
    Changed(#[serde(with = "escaped")] PathBuf),
    Renamed {
        #[serde(with = "escaped")]
        from: PathBuf,
        #[serde(with = "escaped")]
        to: PathBuf,
    },
    Deleted(#[serde(with = "escaped")] PathBuf),
}

/// Writes a path as the screen and the model show it, so that every change can be serialized,
/// also one whose name is not valid UTF-8, and read back unchanged.
mod escaped {
    use std::path::{Path, PathBuf};

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::escape;

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&escape::path(path))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        let text = String::deserialize(deserializer)?;
        escape::unescape(&text).map_err(|e| D::Error::custom(format!("{text}: {e}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Created,
    Changed,
    Renamed,
    Deleted,
}

impl Kind {
    pub fn verb(self) -> &'static str {
        match self {
            Kind::Created => "created",
            Kind::Changed => "changed",
            Kind::Renamed => "renamed",
            Kind::Deleted => "deleted",
        }
    }
}

impl Change {
    pub fn kind(&self) -> Kind {
        match self {
            Change::Created(_) => Kind::Created,
            Change::Changed(_) => Kind::Changed,
            Change::Renamed { .. } => Kind::Renamed,
            Change::Deleted(_) => Kind::Deleted,
        }
    }

    /// Where the change left things: the new name of a renamed path.
    pub fn path(&self) -> &Path {
        match self {
            Change::Created(path) | Change::Changed(path) | Change::Deleted(path) => path,
            Change::Renamed { to, .. } => to,
        }
    }

    fn relative_to(self, root: &Path) -> Self {
        let relative = |path: PathBuf| match path.strip_prefix(root) {
            Ok(rest) => rest.to_path_buf(),
            Err(_) => path,
        };
        match self {
            Change::Created(path) => Change::Created(relative(path)),
            Change::Changed(path) => Change::Changed(relative(path)),
            Change::Renamed { from, to } => Change::Renamed {
                from: relative(from),
                to: relative(to),
            },
            Change::Deleted(path) => Change::Deleted(relative(path)),
        }
    }
}

/// What the watcher reports instead of a change set.
#[derive(Debug)]
pub enum WatchError {
    /// Watching failed for some folders, or the system dropped events, and goes on. Each
    /// message says what goes unseen.
    Failed(Vec<String>),
    /// The project folder was moved or deleted, so nothing is watched any more.
    Gone,
    /// The project folder could not be watched again after the system dropped events, for the
    /// reason given, so nothing is watched any more.
    Stopped(String),
}

type Batch = (Instant, DebounceEventResult);

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
    filter: Filter,
    /// What the last batch showed went wrong, which `next` reports before anything else.
    pending: Vec<String>,
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
            filter,
            pending: Vec::new(),
        })
    }

    /// What the watcher leaves out as not research data.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// Applies `event`, whose batch makes the `moves`.
    fn apply(&mut self, event: &DebouncedEvent, moves: &Moves, changes: &mut Vec<Change>) {
        let paths = &event.paths;
        match event.kind {
            EventKind::Create(_) => paths.iter().for_each(|path| self.arrive(path, changes)),
            EventKind::Remove(_) => paths.iter().for_each(|path| self.forget(path, changes)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                self.rename(&paths[0], &paths[1], moves, changes)
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

    fn rename(&mut self, from: &Path, to: &Path, moves: &Moves, changes: &mut Vec<Change>) {
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
        // A file renamed over another replaces it, which the rename alone does not say. The
        // debouncer joins the steps of a swap through a temporary name into renames that seem to
        // land on known files, but there the file that was there moves on in the same batch, or
        // the source exists again when the swap's renames come in different batches, while a
        // source that a create in the batch filled again is not a swap.
        let replaced = self.known.contains(to)
            && !to_dir
            && !moves.renamed_away.contains(to)
            && (fs::symlink_metadata(from).is_err() || moves.created.contains(from));
        for old in moved {
            let rest = old
                .strip_prefix(from)
                .expect("subtree paths start with its root");
            let new = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            };
            let is_dir = fs::symlink_metadata(&new).is_ok_and(|metadata| metadata.is_dir());
            if !self.filter.skips(&new, is_dir) {
                self.known.insert(new);
            }
        }
        changes.push(Change::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        if replaced {
            changes.push(Change::Changed(to.to_path_buf()));
        }
        self.discover(to, changes);
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
        let mut changes = Vec::new();
        let moves = Moves::of(&events);
        let mut replaced = Vec::new();
        for event in &events {
            // The debouncer adds such a remove when a file is renamed over a path that has
            // events of its own, as `sed -i` does after reading the file.
            if event.kind.is_remove() && event.info() == Some("override") {
                replaced.extend(event.paths.iter().cloned());
            }
            self.apply(event, &moves, &mut changes);
        }
        // A replaced path comes back as created, or as where a rename went, when the create or
        // rename of it follows the remove, so it changed rather than went. When the new file
        // moved on, or the folder it is in did, before it came back, what comes there later is
        // another file, and the path stays deleted.
        for path in replaced {
            let Some(deleted) = changes
                .iter()
                .position(|change| *change == Change::Deleted(path.clone()))
            else {
                continue;
            };
            let back = changes
                .iter()
                .enumerate()
                .skip(deleted + 1)
                .find_map(|(index, change)| match change {
                    Change::Created(created) if *created == path => Some(Some((index, true))),
                    Change::Renamed { to, .. } if *to == path => Some(Some((index, false))),
                    Change::Renamed { from: gone, .. } | Change::Deleted(gone)
                        if path.starts_with(gone) =>
                    {
                        Some(None)
                    }
                    _ => None,
                })
                .flatten();
            match back {
                Some((created, true)) => {
                    changes[created] = Change::Changed(path.clone());
                    changes.remove(deleted);
                }
                Some((renamed, false)) => {
                    changes.insert(renamed + 1, Change::Changed(path.clone()));
                    changes.remove(deleted);
                }
                None => {}
            }
        }
        // A folder whose tag is gone or lost its signature holds files that
        // were never reported, so they count as created, like the files of a
        // folder that appears.
        for folder in &tagged {
            if !self.filter.skips(folder, true) {
                self.discover(folder, &mut changes);
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
    /// The system said so at `noticed`. Fails when the folder cannot be watched again.
    fn rescan(&mut self, since: Instant, noticed: Instant) -> Result<Vec<Change>, String> {
        let mut changes = self.look_again(since);
        // notify opens every folder on the thread that reads the events when it starts watching,
        // which can overflow the queue by itself in a large project. Another watcher would do
        // the same, also while a file that is still written makes every look find a change.
        let own = noticed <= self.watched + self.debounce;
        if own || RecommendedWatcher::kind() != WatcherKind::Inotify {
            return Ok(changes);
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
        let fresh = start(&self.root, self.debounce, self.sender.clone()).map_err(|e| {
            let (folder, cause) = explain(e);
            format!(
                "could not watch {} again after the system dropped events, so the project \
                 folder is no longer watched: {cause}",
                folder.as_deref().unwrap_or(&self.root).display()
            )
        })?;
        self.debouncer = Some(fresh);
        self.watched = Instant::now();
        let found = self.look_again(since);
        extend_set(&mut changes, found);
        Ok(changes)
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
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
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
            // By the clock, since batches of other files can keep coming while a file is copied.
            self.look_if_due();
            let held = self.held.as_ref();
            let received = match held.map(|held| (held.complete(self.debounce), held.next_look)) {
                // `timeout_at` polls the channel first, so batches that waited there while the
                // watcher handled others still go into sets by when their events happened.
                Some((complete, next_look)) => {
                    let until = complete.min(next_look).into();
                    match tokio::time::timeout_at(until, self.events.recv()).await {
                        Ok(received) => received,
                        // A look is due, or the set is complete unless the look finds a write.
                        Err(_) => {
                            self.look_if_due();
                            let held = self.held.as_ref();
                            if held
                                .is_some_and(|held| Instant::now() < held.complete(self.debounce))
                            {
                                continue;
                            }
                            None
                        }
                    }
                }
                // Windows sends no event when the project folder itself is deleted, and no system
                // sends one when a folder above it moves, so a quiet folder is looked at once a
                // debounce.
                None => match tokio::time::timeout(self.debounce, self.events.recv()).await {
                    Ok(received) => received,
                    Err(_) if self.gone(&[]) => return Some(Err(WatchError::Gone)),
                    Err(_) => continue,
                },
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
            let (Some(first), Some(last)) = (times.clone().min(), times.max()) else {
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
                let found = match self.rescan(since, noticed) {
                    Ok(found) => found,
                    Err(e) => {
                        self.held = None;
                        return Some(Err(WatchError::Stopped(e)));
                    }
                };
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
            if changes.is_empty() {
                continue;
            }
            let last = arrived
                .iter()
                .filter_map(|path| self.written_while_held(path, sent))
                .fold(last, Instant::max);
            let batch = Held::new(changes, first, last, Instant::now() + self.debounce / LOOKS);
            match &mut self.held {
                Some(held) if held.takes(first, self.debounce) => held.add(batch),
                held => {
                    if let Some(done) = held.replace(batch) {
                        return Some(Ok(done.changes));
                    }
                }
            }
        }
    }
}

/// Changes held back until no more can join them, with when the first and the last of the
/// events behind them happened.
struct Held {
    changes: Vec<Change>,
    first: Instant,
    last: Instant,
    /// When the watcher looks at the set's files next.
    next_look: Instant,
    /// The paths the set created, changed or renamed something to, under the names its later
    /// renames gave them, without what it deleted.
    paths: BTreeSet<PathBuf>,
    /// Each file as the watcher last looked at it.
    seen: HashMap<PathBuf, Seen>,
}

impl Held {
    fn new(changes: Vec<Change>, first: Instant, last: Instant, next_look: Instant) -> Self {
        let mut paths = BTreeSet::new();
        for change in &changes {
            follow(&mut paths, change);
        }
        Self {
            changes,
            first,
            last,
            next_look,
            paths,
            seen: HashMap::new(),
        }
    }

    /// Whether changes from events that began at `first` join the set, which they do unless the
    /// folder was quiet for `debounce` before them or the set has grown for its longest.
    fn takes(&self, first: Instant, debounce: Duration) -> bool {
        first <= self.last + debounce && first < self.first + debounce * LONGEST_SET
    }

    fn add(&mut self, batch: Held) {
        // The debouncer drops a write after a create only while it still holds the create, so
        // a file written again comes in a later batch.
        for change in &batch.changes {
            follow(&mut self.paths, change);
        }
        extend_set(&mut self.changes, batch.changes);
        self.last = self.last.max(batch.last);
    }

    /// Moves the set's last event to the latest write to a file the set holds, under the name
    /// it has now and also in a folder it holds, when that write came within a debounce of it.
    /// The debouncer holds back writes of one kind to a file until they pause, as a program
    /// that keeps the file open writes them, so a copy that is still running sends no events
    /// and would otherwise seem quiet. The watcher looks only every `LOOKS`th of a debounce, and
    /// a write that bridged the pause can be hidden behind a later one by then, so that much
    /// later still counts. Only files the watcher still knows count, so not those in a folder
    /// tagged as a cache since, and only when `Seen::written_since` says they were written.
    fn note_writes(&mut self, root: &Path, known: &BTreeSet<PathBuf>, debounce: Duration) {
        let mut files: BTreeSet<&PathBuf> = BTreeSet::new();
        for path in &self.paths {
            let path = root.join(path);
            files.extend(
                known
                    .range::<Path, _>((Bound::Included(path.as_path()), Bound::Unbounded))
                    .take_while(|known| known.starts_with(&path)),
            );
        }
        let mut written = Vec::new();
        for file in files {
            let Some((at, now)) = file_written(file) else {
                continue;
            };
            let before = self.seen.insert(file.clone(), now);
            if before.is_some_and(|before| now.written_since(&before)) {
                written.push(at);
            }
        }
        written.sort();
        for written in written {
            if written > self.last && written <= self.last + debounce + debounce / LOOKS {
                self.last = written;
            }
        }
    }

    /// When the debouncer has handed on every event that could still join the set.
    fn complete(&self, debounce: Duration) -> Instant {
        let end = (self.last + debounce).min(self.first + debounce * LONGEST_SET);
        // It hands on an event once the event is a debounce old and checks for such events
        // every quarter debounce, so this leaves a quarter to spare for a late check.
        end + debounce * 3 / 2
    }
}

/// Adds `new` to the changes of a set, leaving out a `changed` for a file the set already has as
/// created or changed, because the model would read it as one more change.
fn extend_set(set: &mut Vec<Change>, new: impl IntoIterator<Item = Change>) {
    let mut written = BTreeSet::new();
    // Going through the set again keeps all of it, because a `changed` left out never changes
    // `written`.
    for change in mem::take(set).into_iter().chain(new) {
        match &change {
            Change::Changed(path) if written.contains(path) => continue,
            Change::Created(path) | Change::Changed(path) => {
                written.insert(path.clone());
            }
            // A path deleted or moved away comes back only as created or as where a rename
            // went, and a rename brings another file there than the set has written.
            Change::Renamed { to, .. } => {
                take_under(&mut written, to);
            }
            Change::Deleted(_) => {}
        }
        set.push(change);
    }
}

/// Follows `change` in `paths`, the names the set's files and folders have now. A rename moves
/// only the paths under it, so that a set of thousands of renamed files stays quick to follow.
fn follow(paths: &mut BTreeSet<PathBuf>, change: &Change) {
    match change {
        Change::Created(path) | Change::Changed(path) => {
            paths.insert(path.clone());
        }
        Change::Renamed { from, to } => {
            for path in take_under(paths, from) {
                let rest = path
                    .strip_prefix(from)
                    .expect("paths under it start with it");
                paths.insert(if rest.as_os_str().is_empty() {
                    to.clone()
                } else {
                    to.join(rest)
                });
            }
            paths.insert(to.clone());
        }
        Change::Deleted(path) => {
            take_under(paths, path);
        }
    }
}

/// Removes `root` and the paths under it from `paths` and gives them back, in a time that grows
/// with how many there are, not with all of `paths`.
fn take_under(paths: &mut BTreeSet<PathBuf>, root: &Path) -> Vec<PathBuf> {
    let under: Vec<PathBuf> = paths
        .range::<Path, _>((Bound::Included(root), Bound::Unbounded))
        .take_while(|path| path.starts_with(root))
        .cloned()
        .collect();
    for path in &under {
        paths.remove(path);
    }
    under
}

/// What a batch of events renames away and creates, which tells a rename in it that lands on a
/// known file from one that swaps it through a temporary name.
struct Moves {
    renamed_away: HashSet<PathBuf>,
    created: HashSet<PathBuf>,
}

impl Moves {
    fn of(events: &[DebouncedEvent]) -> Self {
        let first = |kind: fn(&EventKind) -> bool| {
            events
                .iter()
                .filter(|event| kind(&event.kind))
                .filter_map(|event| event.paths.first().cloned())
                .collect()
        };
        Self {
            renamed_away: first(|kind| {
                *kind == EventKind::Modify(ModifyKind::Name(RenameMode::Both))
            }),
            created: first(|kind| kind.is_create()),
        }
    }
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

/// When the file at `path` was last written, as an instant, or none for a folder, a path that is
/// gone or a time ahead of the clock, which says nothing about when the file was written.
fn written_at(path: &Path) -> Option<Instant> {
    file_written(path).map(|(at, _)| at)
}

/// When the file at `path` was last written, as `written_at` gives it, and how it looked then.
fn file_written(path: &Path) -> Option<(Instant, Seen)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    let seen = Seen {
        size: metadata.len(),
        modified,
        accessed: metadata.accessed().ok(),
    };
    Some((Instant::now().checked_sub(age)?, seen))
}

/// A file's size and times as the watcher saw them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    size: u64,
    modified: SystemTime,
    accessed: Option<SystemTime>,
}

impl Seen {
    /// Whether the file was written since it looked like `before`: its modification time moved
    /// on and its size changed, or its access time is not the new modification time. `touch`
    /// sets both times to the same instant and writes nothing, while a program that sized the
    /// file in advance, as some downloads do, writes without changing its size.
    fn written_since(&self, before: &Seen) -> bool {
        self.modified > before.modified
            && (self.size != before.size || self.accessed != Some(self.modified))
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

        assert_eq!(found, Ok(Vec::new()));
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
        let dropped = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let later = watcher.watched + DEBOUNCE * 2;
        tokio::time::sleep_until(later.into()).await;
        let batch = vec![DebouncedEvent::new(dropped, later)];
        batches.send((Instant::now(), Ok(batch))).unwrap();
        let next = tokio::time::timeout(FIRST_BATCH, watcher.next()).await;
        fs::set_permissions(&raw, fs::Permissions::from_mode(0o755)).unwrap();

        let Ok(Some(Err(WatchError::Stopped(error)))) = next else {
            panic!("{next:?}");
        };
        assert_eq!(
            error,
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
            Ok(vec![Change::Created(PathBuf::from("notes.md"))])
        );
        assert_eq!(watcher.watched, started);
        let later = started + DEBOUNCE * 2;
        assert_eq!(watcher.rescan(Instant::now(), later), Ok(Vec::new()));
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
            Ok(vec![
                Change::Deleted(path("old/a.csv")),
                Change::Created(path("old/b.csv")),
                Change::Created(path("raw")),
                Change::Created(path("raw/a.csv")),
            ])
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

    #[cfg(unix)]
    #[test]
    fn serializes_a_path_that_is_not_utf8_and_reads_it_back() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let change = Change::Renamed {
            from: PathBuf::from(OsStr::from_bytes(b"data/\xffa\n.csv")),
            to: PathBuf::from("data/a.csv"),
        };

        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "renamed": { "from": "data/\\xffa\\n.csv", "to": "data/a.csv" } })
        );
        assert_eq!(serde_json::from_value::<Change>(json).unwrap(), change);
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

    #[test]
    fn counts_a_write_that_a_later_one_hid_between_two_looks() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let debounce = Duration::from_millis(400);
        let start = Instant::now() - debounce * 10;
        let names = ["bridged.csv", "paused.csv"];
        let known: BTreeSet<PathBuf> = names.map(|name| root.join(name)).into();
        let at = |after: Duration| SystemTime::now() - (Instant::now() - (start + after));
        for name in names {
            let file = fs::File::create(root.join(name)).unwrap();
            std::io::Write::write_all(&mut &file, b"a").unwrap();
            file.set_modified(at(Duration::ZERO)).unwrap();
        }
        let mut held = names.map(|name| {
            let changes = vec![Change::Created(PathBuf::from(name))];
            Held::new(changes, start, start, start)
        });
        // The first look only learns how the files look.
        for held in &mut held {
            held.note_writes(root, &known, debounce);
        }

        // A write 1.1 debounces after the last one can follow one in between that the watcher
        // did not see before it, but one 1.5 debounces after it follows a pause.
        let write = |name: &str, after: Duration| {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(root.join(name))
                .unwrap();
            std::io::Write::write_all(&mut file, b"b").unwrap();
            file.set_modified(at(after)).unwrap();
        };
        write("bridged.csv", debounce * 11 / 10);
        write("paused.csv", debounce * 3 / 2);
        for held in &mut held {
            held.note_writes(root, &known, debounce);
        }

        let [bridged, paused] = held;
        assert!(
            bridged.last > start + debounce,
            "{:?}",
            bridged.last - start
        );
        assert_eq!(paused.last, start);
    }

    #[test]
    fn follows_thousands_of_renames_quickly() {
        let start = Instant::now();
        let path = |name: String| PathBuf::from(name);
        let mut held = Held::new(Vec::new(), start, start, start);
        // As a script that renames 10,000 photos, a batch at a time.
        for batch in (0..10_000).collect::<Vec<_>>().chunks(100) {
            let changes = batch
                .iter()
                .flat_map(|i| {
                    [
                        Change::Created(path(format!("photos/IMG_{i:05}.JPG"))),
                        Change::Renamed {
                            from: path(format!("photos/IMG_{i:05}.JPG")),
                            to: path(format!("photos/site-1-{i:05}.jpg")),
                        },
                    ]
                })
                .collect();
            held.add(Held::new(changes, start, start, start));
        }
        held.note_writes(Path::new("/nonexistent"), &BTreeSet::new(), DEBOUNCE);

        assert_eq!(held.paths.len(), 10_000);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
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
        fs::write(root.join("b.csv"), "b").unwrap();
        fs::write(root.join("a.csv~"), "a").unwrap();
        let mut watcher = watch(root);

        // A backup is renamed over a file the watcher knows, after something read it, and moves
        // on to another backup name, all within a debounce. Both backup names are ignored, so
        // only the file the watcher knew is left to report, and it is gone.
        fs::read(root.join("b.csv")).unwrap();
        fs::rename(root.join("a.csv~"), root.join("b.csv")).unwrap();
        fs::rename(root.join("b.csv"), root.join("c.csv~")).unwrap();

        assert_eq!(
            drain(&mut watcher).await,
            [Change::Deleted(PathBuf::from("b.csv"))]
        );
    }

    #[tokio::test]
    async fn reports_a_replaced_file_where_its_folder_went() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(root, "data/b.csv", "b");
        write(root, "data/a.csv~", "a");
        let mut watcher = watch(root);

        // A backup is renamed over a file the watcher knows, after something read it, and the
        // folder gets another name, all within a debounce.
        fs::read(root.join("data/b.csv")).unwrap();
        fs::rename(root.join("data/a.csv~"), root.join("data/b.csv")).unwrap();
        fs::rename(root.join("data"), root.join("renamed")).unwrap();

        let mut changes = drain(&mut watcher).await;
        changes.sort_by_key(|change| format!("{change:?}"));
        let path = PathBuf::from;
        assert_eq!(
            changes,
            [
                Change::Created(path("renamed/b.csv")),
                Change::Deleted(path("data/b.csv")),
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
        let mut watcher = watch(root);
        fs::rename(root.join("site-a.csv"), root.join("swap.tmp")).unwrap();
        fs::rename(root.join("site-b.csv"), root.join("site-a.csv")).unwrap();
        fs::rename(root.join("swap.tmp"), root.join("site-b.csv")).unwrap();
        fs::rename(root.join("site-a.csv"), root.join("site-c.csv")).unwrap();

        // The debouncer joins those steps into a rename from site-a.csv to site-b.csv and one
        // from site-b.csv to site-c.csv. Handed on together, as here, they show that nothing was
        // written over site-b.csv, though site-a.csv, the first rename's source, is gone. Its
        // tick can also hand them on one at a time, and then site-b.csv shows up as changed too.
        let rename = |from: &str, to: &str| {
            let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(root.join(from))
                .add_path(root.join(to));
            DebouncedEvent::new(event, Instant::now())
        };
        let changes = watcher.handle(vec![
            rename("site-a.csv", "site-b.csv"),
            rename("site-b.csv", "site-c.csv"),
        ]);

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
                    to: path("site-c.csv"),
                },
            ]
        );
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
    fn lists_an_edit_to_a_file_moved_onto_a_path_the_set_wrote() {
        let path = PathBuf::from;
        let changes = vec![
            Change::Created(path("a.csv")),
            Change::Renamed {
                from: path("a.csv"),
                to: path("b.csv"),
            },
            Change::Renamed {
                from: path("c.csv"),
                to: path("a.csv"),
            },
            Change::Changed(path("a.csv")),
            Change::Created(path("data/x.csv")),
            Change::Deleted(path("data")),
            Change::Renamed {
                from: path("archive"),
                to: path("data"),
            },
            Change::Changed(path("data/x.csv")),
        ];
        let mut set = Vec::new();

        // Each in a batch of its own, so that every batch is checked against the ones before.
        for change in changes.clone() {
            extend_set(&mut set, [change]);
        }

        assert_eq!(set, changes);
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
