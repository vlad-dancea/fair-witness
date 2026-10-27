use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::mem;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::Change;

/// A folder that never stays quiet for the debounce, such as during a long copy, still gets a
/// change set once its first change is this many debounces old.
pub const LONGEST_SET: u32 = 30;
/// How often per debounce the watcher looks at the files of a held set.
pub const LOOKS: u32 = 4;

/// Changes held back until no more can join them, with when the first and the last of the
/// events behind them happened.
pub struct Held {
    pub changes: Vec<Change>,
    pub first: Instant,
    last: Instant,
    /// When the watcher looks at the set's files next.
    pub next_look: Instant,
    /// The paths the set created, changed or renamed something to, under the names its later
    /// renames gave them, without what it deleted.
    paths: BTreeSet<PathBuf>,
    /// Each file as the watcher last looked at it.
    seen: HashMap<PathBuf, Seen>,
}

impl Held {
    pub fn new(changes: Vec<Change>, first: Instant, last: Instant, next_look: Instant) -> Self {
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
    pub fn takes(&self, first: Instant, debounce: Duration) -> bool {
        first <= self.last + debounce && first < self.first + debounce * LONGEST_SET
    }

    pub fn add(&mut self, batch: Held) {
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
    pub fn note_writes(&mut self, root: &Path, known: &BTreeSet<PathBuf>, debounce: Duration) {
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
    pub fn complete(&self, debounce: Duration) -> Instant {
        let end = (self.last + debounce).min(self.first + debounce * LONGEST_SET);
        // It hands on an event once the event is a debounce old and checks for such events
        // every quarter debounce, so this leaves a quarter to spare for a late check.
        end + debounce * 3 / 2
    }
}

/// Adds `new` to the changes of a set, leaving out a `changed` for a file the set already has as
/// created or changed, because the model would read it as one more change.
pub fn extend_set(set: &mut Vec<Change>, new: impl IntoIterator<Item = Change>) {
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
pub fn take_under(paths: &mut BTreeSet<PathBuf>, root: &Path) -> Vec<PathBuf> {
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

/// When the file at `path` was last written, as an instant, or none for a folder, a path that is
/// gone or a time ahead of the clock, which says nothing about when the file was written.
pub fn written_at(path: &Path) -> Option<Instant> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        held.note_writes(
            Path::new("/nonexistent"),
            &BTreeSet::new(),
            Duration::from_millis(50),
        );

        assert_eq!(held.paths.len(), 10_000);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
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
}
