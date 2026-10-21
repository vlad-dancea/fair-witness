use std::collections::BTreeSet;
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::Result;
use notify_debouncer_full::notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::notify::{self, EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};

const DEBOUNCE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created(PathBuf),
    Changed(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
    Deleted(PathBuf),
}

impl Change {
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Change::Created(path) | Change::Changed(path) | Change::Deleted(path) => vec![path],
            Change::Renamed { from, to } => vec![from, to],
        }
    }
}

pub struct FolderWatcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    events: Receiver<DebounceEventResult>,
    known: BTreeSet<PathBuf>,
}

impl FolderWatcher {
    pub fn new(root: &Path) -> Result<Self> {
        let (tx, events) = mpsc::channel();
        let mut debouncer = new_debouncer(DEBOUNCE_TIMEOUT, None, tx)?;
        debouncer.watch(root, RecursiveMode::Recursive)?;
        let mut known = BTreeSet::new();
        walk(root, &mut |path| {
            known.insert(path);
        });
        Ok(Self {
            _debouncer: debouncer,
            events,
            known,
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
                    } else if path.is_file() {
                        changes.push(Change::Changed(path.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    fn discover(&mut self, path: &Path, changes: &mut Vec<Change>) {
        walk(path, &mut |found| {
            if self.known.insert(found.clone()) {
                changes.push(Change::Created(found));
            }
        });
    }

    fn forget(&mut self, path: &Path, changes: &mut Vec<Change>) {
        if !self.take_subtree(path).is_empty() {
            changes.push(Change::Deleted(path.to_path_buf()));
        }
    }

    fn rename(&mut self, from: &Path, to: &Path, changes: &mut Vec<Change>) {
        let moved = self.take_subtree(from);
        if moved.is_empty() {
            self.discover(to, changes);
            return;
        }
        for old in moved {
            let rest = old
                .strip_prefix(from)
                .expect("subtree paths start with its root");
            self.known.insert(if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            });
        }
        changes.push(Change::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        self.discover(to, changes);
    }

    fn handle(&mut self, result: DebounceEventResult) -> Result<Vec<Change>, Vec<notify::Error>> {
        let mut changes = Vec::new();
        for event in &result? {
            self.apply(event, &mut changes);
        }
        Ok(changes)
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
}

impl Iterator for FolderWatcher {
    type Item = Result<Vec<Change>, Vec<notify::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        let result = self.events.recv().ok()?;
        Some(self.handle(result))
    }
}

fn walk(path: &Path, visit: &mut impl FnMut(PathBuf)) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    visit(path.to_path_buf());
    if metadata.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            walk(&entry.path(), visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_secs(4);

    fn drain(watcher: &mut FolderWatcher) -> Vec<Change> {
        let mut changes = Vec::new();
        while let Ok(result) = watcher.events.recv_timeout(QUIET) {
            changes.extend(watcher.handle(result).expect("watching failed"));
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

    #[test]
    fn reports_files_written_into_a_new_folder_once_each() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut watcher = FolderWatcher::new(root).unwrap();

        fs::create_dir_all(root.join("data/raw")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        fs::write(root.join("data/raw/b.csv"), "b").unwrap();

        assert_eq!(
            created(&drain(&mut watcher)),
            ["data", "data/a.csv", "data/raw", "data/raw/b.csv"].map(|p| root.join(p)),
        );
    }

    #[test]
    fn reports_a_folder_moved_in_from_outside_with_its_contents() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(outside.path().join("survey")).unwrap();
        fs::write(outside.path().join("survey/answers.csv"), "x").unwrap();
        let root = root.path();
        let mut watcher = FolderWatcher::new(root).unwrap();

        fs::rename(outside.path().join("survey"), root.join("survey")).unwrap();

        assert_eq!(
            created(&drain(&mut watcher)),
            ["survey", "survey/answers.csv"].map(|p| root.join(p)),
        );
    }

    #[test]
    fn reports_a_renamed_folder_as_one_rename() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        let mut watcher = FolderWatcher::new(root).unwrap();

        fs::rename(root.join("data"), root.join("dataset")).unwrap();

        assert_eq!(
            drain(&mut watcher),
            [Change::Renamed {
                from: root.join("data"),
                to: root.join("dataset"),
            }],
        );
    }

    #[test]
    fn reports_a_deleted_folder() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(root.join("data/a.csv"), "a").unwrap();
        let mut watcher = FolderWatcher::new(root).unwrap();

        fs::remove_dir_all(root.join("data")).unwrap();

        let changes = drain(&mut watcher);
        assert!(
            changes.contains(&Change::Deleted(root.join("data"))),
            "{changes:?}"
        );
        assert!(created(&changes).is_empty(), "{changes:?}");
    }

    #[test]
    fn reports_an_edited_file_as_changed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::write(root.join("notes.txt"), "a").unwrap();
        let mut watcher = FolderWatcher::new(root).unwrap();

        fs::write(root.join("notes.txt"), "b").unwrap();

        assert_eq!(
            drain(&mut watcher),
            [Change::Changed(root.join("notes.txt"))]
        );
    }
}
