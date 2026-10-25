mod filter;
mod summary;

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify_debouncer_full::notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::notify::{self, EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use filter::{CACHE_TAG, Filter};
pub use summary::{Listed, summarize};

/// How long the folder stays quiet before its changes count as one change set, unless a
/// caller asks for another time.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// A folder that never stays quiet for the debounce, such as during a long copy, still gets a
/// change set once its first change is this many debounces old.
const LONGEST_SET: u32 = 30;
/// The debouncer checks for quiet every quarter of the wait, so a wait of zero would have it
/// spin.
const SHORTEST_DEBOUNCE: Duration = Duration::from_millis(10);

/// A change in the watched folder, with paths relative to its root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Created(#[serde(with = "lossy")] PathBuf),
    Changed(#[serde(with = "lossy")] PathBuf),
    Renamed {
        #[serde(with = "lossy")]
        from: PathBuf,
        #[serde(with = "lossy")]
        to: PathBuf,
    },
    Deleted(#[serde(with = "lossy")] PathBuf),
}

/// Writes a path that is not valid UTF-8 with replacement characters, as the screen shows it,
/// so that every change can be serialized.
mod lossy {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&path.to_string_lossy())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        PathBuf::deserialize(deserializer)
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

pub struct FolderWatcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    events: UnboundedReceiver<DebounceEventResult>,
    debounce: Duration,
    held: Option<Held>,
    root: PathBuf,
    known: BTreeSet<PathBuf>,
    filter: Filter,
}

impl FolderWatcher {
    /// Watches `root`, grouping changes until the folder has been quiet for `debounce`, or
    /// for 10 milliseconds when that is shorter.
    pub fn new(root: &Path, debounce: Duration) -> Result<Self> {
        let filter = Filter::new(root)?;
        let (tx, events) = mpsc::unbounded_channel();
        let debounce = debounce.max(SHORTEST_DEBOUNCE);
        let mut debouncer = new_debouncer(debounce, None, move |result| {
            let _ = tx.send(result);
        })?;
        debouncer.watch(root, RecursiveMode::Recursive)?;
        let mut known = BTreeSet::new();
        walk(root, &filter, &mut |path| {
            known.insert(path);
        });
        Ok(Self {
            _debouncer: debouncer,
            events,
            debounce,
            held: None,
            root: root.to_path_buf(),
            known,
            filter,
        })
    }

    fn apply(&mut self, event: &DebouncedEvent, changes: &mut Vec<Change>) {
        let paths = &event.paths;
        match event.kind {
            EventKind::Create(_) => paths.iter().for_each(|path| self.discover(path, changes)),
            EventKind::Remove(_) => paths.iter().for_each(|path| self.forget(path, changes)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                self.rename(&paths[0], &paths[1], changes)
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                for path in paths {
                    if fs::symlink_metadata(path).is_ok() {
                        self.discover(path, changes);
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

    fn rename(&mut self, from: &Path, to: &Path, changes: &mut Vec<Change>) {
        let to_dir = fs::symlink_metadata(to).is_ok_and(|metadata| metadata.is_dir());
        // A move out of the project is a deletion, as when only its source is
        // reported.
        if !to.starts_with(&self.root) || self.filter.skips(to, to_dir) {
            self.forget(from, changes);
            return;
        }
        let moved = self.take_subtree(from);
        if moved.is_empty() {
            self.discover(to, changes);
            return;
        }
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
        for event in &events {
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
        changes
            .into_iter()
            .map(|change| change.relative_to(&self.root))
            .collect()
    }

    fn take_subtree(&mut self, path: &Path) -> Vec<PathBuf> {
        let subtree: Vec<PathBuf> = self
            .known
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .take_while(|known| known.starts_with(path))
            .cloned()
            .collect();
        for known in &subtree {
            self.known.remove(known);
        }
        subtree
    }

    /// The next change set, once the folder has been quiet for the debounce or the set's first
    /// change is 30 debounces old.
    pub async fn next(&mut self) -> Option<Result<Vec<Change>, Vec<notify::Error>>> {
        loop {
            let received = match &self.held {
                // `timeout_at` polls the channel first, so batches that waited there while the
                // session was busy still go into sets by when their events happened.
                Some(held) => {
                    let complete = held.complete(self.debounce).into();
                    tokio::time::timeout_at(complete, self.events.recv())
                        .await
                        .unwrap_or(None)
                }
                None => self.events.recv().await,
            };
            let events = match received {
                Some(Ok(events)) => events,
                Some(Err(errors)) => return Some(Err(errors)),
                // The set is complete, or the debouncer stopped.
                None => return self.held.take().map(|held| Ok(held.changes)),
            };
            // Only events that can make a change count, so that reading a file bridges no
            // pause, and a batch that made none, such as in a folder the filter leaves out, keeps
            // no set open.
            let times = events
                .iter()
                .filter(|event| can_change(&event.kind))
                .map(|event| event.time);
            let (Some(first), Some(last)) = (times.clone().min(), times.max()) else {
                continue;
            };
            let changes = self.handle(events);
            if changes.is_empty() {
                continue;
            }
            let batch = Held {
                changes,
                first,
                last,
            };
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
}

impl Held {
    /// Whether changes from events that began at `first` join the set, which they do unless the
    /// folder was quiet for `debounce` before them or the set has grown for its longest.
    fn takes(&self, first: Instant, debounce: Duration) -> bool {
        first <= self.last + debounce && first < self.first + debounce * LONGEST_SET
    }

    fn add(&mut self, batch: Held) {
        // A file written again is still one change.
        let changed: HashSet<&Path> = self
            .changes
            .iter()
            .filter(|change| change.kind() == Kind::Changed)
            .map(Change::path)
            .collect();
        let new: Vec<Change> = batch
            .changes
            .into_iter()
            .filter(|change| change.kind() != Kind::Changed || !changed.contains(change.path()))
            .collect();
        self.changes.extend(new);
        self.last = self.last.max(batch.last);
    }

    /// When the debouncer has handed on every event that could still join the set.
    fn complete(&self, debounce: Duration) -> Instant {
        let end = (self.last + debounce).min(self.first + debounce * LONGEST_SET);
        // It hands on an event once the event is a debounce old and checks for such events
        // every quarter debounce, so this leaves a quarter to spare for a late check.
        end + debounce * 3 / 2
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

        // The session reads a batch only after the review before it, so the
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
    #[test]
    fn serializes_a_path_that_is_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let change = Change::Renamed {
            from: PathBuf::from(OsStr::from_bytes(b"data/\xffa.csv")),
            to: PathBuf::from("data/a.csv"),
        };

        assert_eq!(
            serde_json::to_value(&change).unwrap(),
            serde_json::json!({ "renamed": { "from": "data/\u{fffd}a.csv", "to": "data/a.csv" } })
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
    async fn sorts_changes_into_sets_by_when_the_files_were_written() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["a.csv", "b.csv", "c.csv"] {
            fs::write(root.join(name), "a").unwrap();
        }
        let mut watcher = watch(root);
        let (batches, events) = mpsc::unbounded_channel();
        watcher.events = events;

        // The session reads batches only between reviews, so these all waited for it.
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
            batches.send(Ok(batch)).unwrap();
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
}
