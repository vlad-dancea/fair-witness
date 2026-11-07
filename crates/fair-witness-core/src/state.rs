//! What the project folder looked like when its changes were last decided, kept from one run to
//! the next so that a run can report what changed while none was watching.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Result, bail};
use jiff::Timestamp;
use notify_debouncer_full::file_id::{self, FileId};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::escape;
use crate::project::Project;
use crate::watcher::{Change, Filter, missing, walk};

/// How often at most the state is written while `run` goes on. Rewriting it takes time in a large
/// folder, and a write lost to a crash only means reporting a change again.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// How long the look at a set's paths may hold up its review. Such a look can block, as on a
/// network drive, and a review has to stay limited by its timeout for the next set to get its turn.
const LOOK_WITHIN: Duration = Duration::from_secs(60);

/// How a path looks from outside, which tells that it changed without reading it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Look {
    /// A folder, whose own size and time change with what is in it.
    Folder,
    /// A file or a link, which counts as a file wherever it leads.
    File {
        size: u64,
        /// None for a time the system or `Timestamp` cannot give.
        modified: Option<Timestamp>,
    },
}

impl Look {
    fn of(metadata: &Metadata) -> Self {
        if metadata.is_dir() {
            return Self::Folder;
        }
        Self::File {
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| Timestamp::try_from(time).ok()),
        }
    }

    fn is_folder(&self) -> bool {
        matches!(self, Self::Folder)
    }
}

/// What each path looked like, by its path relative to the project folder.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FolderState {
    looks: BTreeMap<PathBuf, Look>,
}

/// `FolderState` as `.fair-witness/state.json` holds it, with paths written as events write them.
#[derive(Serialize, Deserialize)]
struct SavedState {
    paths: BTreeMap<String, Look>,
}

impl FolderState {
    pub(crate) fn add(&mut self, relative: PathBuf, metadata: &Metadata) {
        self.looks.insert(relative, Look::of(metadata));
    }

    /// The state as `state.json` holds it.
    fn to_json(&self) -> Result<Vec<u8>> {
        let paths = self
            .looks
            .iter()
            .map(|(path, look)| (escape::path(path), look.clone()))
            .collect();
        Ok(serde_json::to_vec(&SavedState { paths })?)
    }

    /// Reads what `to_json` wrote. A path in it that leads anywhere but into the folder fails, since
    /// no walk through the folder gives one.
    pub(crate) fn from_json(text: &[u8]) -> Result<Self> {
        let saved: SavedState = serde_json::from_slice(text)?;
        let mut looks = BTreeMap::new();
        for (text, look) in saved.paths {
            let path = escape::unescape(&text).map_err(anyhow::Error::msg)?;
            if !path.components().all(|c| matches!(c, Component::Normal(_))) {
                bail!("it names {text}, which is not a path in the project folder");
            }
            looks.insert(path, look);
        }
        Ok(Self { looks })
    }

    /// What differs in `now`, as the folder under `root` looks, in the order a look through the
    /// folder reports it. A deleted folder comes once, without what was in it. Forgets the paths
    /// that the rules skip now, which were not deleted. A path that only the walk did not reach, as
    /// under a folder it could not read, stays as it is.
    fn compare(&mut self, now: &FolderState, root: &Path, filter: &Filter) -> Vec<Change> {
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();
        let mut deleted_folder: Option<&PathBuf> = None;
        for (path, saved) in &self.looks {
            if deleted_folder.is_some_and(|folder| path.starts_with(folder)) {
                continue;
            }
            match now.looks.get(path) {
                Some(look) if look.is_folder() == saved.is_folder() => continue,
                Some(_) => {}
                None => {
                    let full = root.join(path);
                    match fs::symlink_metadata(&full) {
                        Err(e) if missing(&e) => {
                            if filter.skips(&full, saved.is_folder()) {
                                skipped.push(path.clone());
                                continue;
                            }
                        }
                        Ok(metadata) if filter.skips(&full, metadata.is_dir()) => {
                            skipped.push(path.clone());
                            continue;
                        }
                        _ => continue,
                    }
                }
            }
            deleted.push(Change::Deleted(path.clone()));
            deleted_folder = Some(path);
        }
        let mut created = Vec::new();
        let mut changed = Vec::new();
        for (path, look) in &now.looks {
            match self.looks.get(path) {
                Some(saved) if saved.is_folder() != look.is_folder() => {
                    created.push(Change::Created(path.clone()));
                }
                Some(saved) if saved != look => changed.push(Change::Changed(path.clone())),
                Some(_) => {}
                None => created.push(Change::Created(path.clone())),
            }
        }
        for path in skipped {
            self.looks.remove(&path);
        }
        deleted.extend(created);
        deleted.extend(changed);
        deleted
    }

    /// Takes `moves` in order, so that what a set did last counts.
    fn apply(&mut self, moves: Vec<Move>) {
        for step in moves {
            match step {
                Move::See(path, look) => {
                    self.looks.insert(path, look);
                }
                Move::Forget(path) => {
                    self.take_under(&path);
                }
                // What was in a folder goes with it as it was last decided, so that a file edited
                // during the review still differs. A path that looked otherwise before the review
                // is not what was decided, as when another folder took the old name since, and
                // its look could match what comes later, so it is reported again instead.
                Move::Rename { from, to, now } => {
                    for (path, look) in self.take_under(&from) {
                        let rest = path
                            .strip_prefix(&from)
                            .expect("paths under it start with it");
                        let moved = if rest.as_os_str().is_empty() {
                            to.clone()
                        } else {
                            to.join(rest)
                        };
                        if now.get(&moved) == Some(&look) {
                            self.looks.insert(moved, look);
                        } else {
                            self.looks.remove(&moved);
                        }
                    }
                }
            }
        }
    }

    /// Removes `root` and what is under it and gives them back.
    fn take_under(&mut self, root: &Path) -> Vec<(PathBuf, Look)> {
        let under: Vec<PathBuf> = self
            .looks
            .range::<Path, _>((Bound::Included(root), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(root))
            .map(|(path, _)| path.clone())
            .collect();
        under
            .into_iter()
            .filter_map(|path| {
                let look = self.looks.remove(&path)?;
                Some((path, look))
            })
            .collect()
    }
}

/// One step of how the state moves past a decided set.
#[derive(Debug, PartialEq)]
enum Move {
    See(PathBuf, Look),
    /// The path and what is under it are gone.
    Forget(PathBuf),
    Rename {
        from: PathBuf,
        to: PathBuf,
        /// How `to` and what is under it looked before the review.
        now: BTreeMap<PathBuf, Look>,
    },
}

/// How the state moves past `changes`, with what they created, changed or renamed something to as
/// it looks now. A path they deleted or renamed away is only forgotten, also when something is
/// there again by now, since the review is not told about that.
fn moves(changes: &[Change], root: &Path, filter: &Filter) -> Vec<Move> {
    let look = |path: &Path| {
        let full = root.join(path);
        match fs::symlink_metadata(&full) {
            Ok(metadata) if !filter.skips(&full, metadata.is_dir()) => {
                Move::See(path.to_path_buf(), Look::of(&metadata))
            }
            _ => Move::Forget(path.to_path_buf()),
        }
    };
    let mut moves = Vec::with_capacity(changes.len());
    for change in changes {
        match change {
            Change::Created(path) | Change::Changed(path) => moves.push(look(path)),
            Change::Deleted(path) => moves.push(Move::Forget(path.clone())),
            Change::Renamed { from, to } => {
                let mut now = BTreeMap::new();
                walk(&root.join(to), filter, &mut |path, metadata| {
                    if let Ok(relative) = path.strip_prefix(root) {
                        now.insert(relative.to_path_buf(), Look::of(metadata));
                    }
                });
                moves.push(Move::Rename {
                    from: from.clone(),
                    to: to.clone(),
                    now,
                });
                moves.push(look(to));
            }
        }
    }
    moves
}

/// The state of a project folder during a session: it moves past each set that is decided and
/// goes to `.fair-witness/state.json` now and then.
pub struct StateKeeper {
    project: Project,
    moving: Mutex<Moving>,
    /// Held from before the contents are taken until they are written, so that the newest contents
    /// are written last.
    saving: Mutex<()>,
    unsaved: Notify,
}

#[derive(Default)]
struct Moving {
    state: FolderState,
    /// The set under review, with how the state moves past it if it is decided.
    under_review: Option<(usize, Vec<Move>)>,
    unsaved: bool,
    /// Whether the folder may not be where it was, so that nothing is saved any more.
    stopped: bool,
    /// The project folder's id, which tells it from a folder that took its place before the watcher
    /// noticed.
    root_id: Option<FileId>,
}

/// What a session reports when it starts.
#[derive(Debug, Default)]
pub(crate) struct Started {
    /// What changed since the last run.
    pub changes: Vec<Change>,
    /// What went wrong with the state, for the researcher.
    pub problems: Vec<String>,
}

impl StateKeeper {
    pub fn new(project: Project) -> Self {
        Self {
            project,
            moving: Mutex::default(),
            saving: Mutex::new(()),
            unsaved: Notify::new(),
        }
    }

    fn moving(&self) -> MutexGuard<'_, Moving> {
        self.moving.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes in how the folder looks now, once the watches are up, and gives what differs from the
    /// saved state. The first time, or when the state cannot be read, there is nothing to compare
    /// with, so it reports no change and starts from `now` at once: a run killed later still has
    /// something to come back to. `root_id` is the id of the folder `now` was found in, and when
    /// another has taken its place since, it does nothing, and the watcher says so soon.
    pub(crate) fn start(&self, now: FolderState, filter: &Filter, root_id: FileId) -> Started {
        self.moving().root_id = Some(root_id);
        let mut started = Started::default();
        if file_id::get_file_id(self.project.root()).ok() != Some(root_id) {
            return started;
        }
        match self.project.folder_state() {
            Ok(Some(mut state)) => {
                let saved_paths = state.looks.len();
                started.changes = state.compare(&now, self.project.root(), filter);
                let mut moving = self.moving();
                moving.unsaved = state.looks.len() != saved_paths;
                moving.state = state;
                let unsaved = moving.unsaved;
                drop(moving);
                if unsaved {
                    self.unsaved.notify_one();
                }
                return started;
            }
            Ok(None) => {}
            Err(e) => {
                let problem = format!(
                    "could not read what the folder looked like when `run` last stopped, so what \
                     changed since then is not reported and this run starts over: {e:#}"
                );
                tracing::warn!("{problem}");
                started.problems.push(problem);
            }
        }
        {
            let mut moving = self.moving();
            moving.state = now;
            moving.unsaved = true;
        }
        if let Err(e) = self.save() {
            started.problems.push(not_saved(&e));
        }
        started
    }

    /// Looks at the paths of `changes` right before the review of `set` starts, so that an edit
    /// during the review is seen as new. When the folder does not answer in time, the state stays
    /// where it is and the set comes back at the next start.
    pub(crate) async fn before_review(&self, set: usize, changes: &[Change], filter: &Filter) {
        self.moving().under_review = None;
        let (changes, filter) = (changes.to_vec(), filter.clone());
        let root = self.project.root().to_path_buf();
        let looked = tokio::task::spawn_blocking(move || moves(&changes, &root, &filter));
        match tokio::time::timeout(LOOK_WITHIN, looked).await {
            Ok(Ok(moves)) => self.moving().under_review = Some((set, moves)),
            Ok(Err(e)) => tracing::warn!("could not look at the paths of set {set}: {e}"),
            Err(_) => tracing::warn!("the folder did not answer about the paths of set {set}"),
        }
    }

    /// Moves the state past `set`. A set a newer verdict settled has nothing to move, since that
    /// verdict moved what it covered, with how those paths looked later.
    pub(crate) fn decided(&self, set: usize) {
        {
            let mut moving = self.moving();
            let Some((_, moves)) = moving.under_review.take_if(|(of, _)| *of == set) else {
                return;
            };
            moving.state.apply(moves);
            moving.unsaved = true;
        }
        self.unsaved.notify_one();
    }

    /// Saves nothing from here on, for a folder that was moved or deleted, or that another may have
    /// taken the place of. What was not saved is reported again, at most.
    pub(crate) fn stop_saving(&self) {
        self.moving().stopped = true;
    }

    /// Writes the state if it moved since it was last written. `run` calls this as it stops, since
    /// it does not wait for what its tasks still do then.
    pub fn save(&self) -> Result<()> {
        let _saving = self.saving.lock().unwrap_or_else(PoisonError::into_inner);
        let contents = {
            let mut moving = self.moving();
            let same_folder = file_id::get_file_id(self.project.root()).ok() == moving.root_id;
            if !moving.unsaved || moving.stopped || !same_folder {
                return Ok(());
            }
            let contents = moving.state.to_json()?;
            moving.unsaved = false;
            contents
        };
        let saved = self.project.save_folder_state(&contents);
        if saved.is_err() {
            self.moving().unsaved = true;
        }
        saved
    }

    /// Saves what moved, at most once per `SAVE_EVERY`, and tells `report` when that stops working,
    /// once until it works again. It never ends.
    pub(crate) async fn save_when_due(self: Arc<Self>, report: impl Fn(String)) {
        let mut last = Instant::now();
        let mut failing = false;
        loop {
            self.unsaved.notified().await;
            tokio::time::sleep_until(last + SAVE_EVERY).await;
            let keeper = self.clone();
            let saved = tokio::task::spawn_blocking(move || keeper.save()).await;
            last = Instant::now();
            match saved {
                Ok(Ok(())) => failing = false,
                Ok(Err(e)) => {
                    let problem = not_saved(&e);
                    tracing::warn!("{problem}");
                    if !failing {
                        report(problem);
                    }
                    failing = true;
                }
                Err(e) => tracing::warn!("could not save the state of the folder: {e}"),
            }
        }
    }
}

fn not_saved(error: &anyhow::Error) -> String {
    format!(
        "could not save what the folder looks like, so the next start may report changes again: \
         {error:#}"
    )
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::SystemTime;

    use super::*;

    impl StateKeeper {
        pub(crate) fn has_moved_past(&self, relative: &Path) -> bool {
            self.moving().state.looks.contains_key(relative)
        }
    }

    fn project() -> (tempfile::TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = Project::new(root.path().to_path_buf());
        project.create().unwrap();
        (root, project)
    }

    /// The id of the folder at `root`.
    fn id_of(root: &Path) -> FileId {
        file_id::get_file_id(root).unwrap()
    }

    /// The folder as a watcher that starts now finds it.
    fn looked_through(root: &Path) -> FolderState {
        crate::watcher::FolderWatcher::new(root, Duration::from_millis(50))
            .unwrap()
            .take_first_look()
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// `state` compared with the folder under `root`.
    fn compared(state: &mut FolderState, root: &Path) -> Vec<Change> {
        let filter = Filter::new(root).unwrap();
        state.compare(&looked_through(root), root, &filter)
    }

    #[tokio::test]
    async fn reports_a_deleted_folder_once_and_a_path_that_became_another_kind_as_new() {
        let (root, _project) = project();
        let root = root.path();
        write(root, "data/a.csv", "a");
        write(root, "data/raw/b.csv", "b");
        write(root, "became-folder", "c");
        write(root, "became-file/d.csv", "d");
        write(root, "same.csv", "e");
        let mut state = looked_through(root);

        fs::remove_dir_all(root.join("data")).unwrap();
        fs::remove_file(root.join("became-folder")).unwrap();
        write(root, "became-folder/e.csv", "e");
        fs::remove_dir_all(root.join("became-file")).unwrap();
        write(root, "became-file", "f");

        assert_eq!(
            compared(&mut state, root),
            [
                Change::Deleted("became-file".into()),
                Change::Deleted("became-folder".into()),
                Change::Deleted("data".into()),
                Change::Created("became-file".into()),
                Change::Created("became-folder".into()),
                Change::Created("became-folder/e.csv".into()),
            ]
        );
    }

    #[tokio::test]
    async fn reports_a_file_replaced_by_one_of_the_same_size_and_an_older_time() {
        let (root, _project) = project();
        let root = root.path();
        write(root, "results.csv", "new");
        write(root, "touched.csv", "new");
        let mut state = looked_through(root);

        // As `cp -p` or `rsync -t` leave a file.
        let replacement = root.join("replacement");
        fs::write(&replacement, "old").unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(10 * 365 * 24 * 60 * 60))
            .unwrap();
        fs::rename(&replacement, root.join("results.csv")).unwrap();

        assert_eq!(
            compared(&mut state, root),
            [Change::Changed("results.csv".into())]
        );
    }

    #[tokio::test]
    async fn leaves_out_and_forgets_what_a_new_rule_skips_whether_or_not_it_is_there() {
        let (root, _project) = project();
        let root = root.path();
        write(root, "scratch/kept.csv", "a");
        write(root, "scratch/deleted.csv", "b");
        write(root, "old.log", "c");
        write(root, "tagged/a.csv", "d");
        write(root, "data.csv", "e");
        let mut state = looked_through(root);

        write(root, ".fair-witness/ignore", "scratch/\n*.log\n");
        fs::remove_file(root.join("scratch/deleted.csv")).unwrap();
        fs::remove_file(root.join("old.log")).unwrap();
        write(
            root,
            "tagged/CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );

        assert_eq!(compared(&mut state, root), []);
        // A rule taken back later finds the files again, as paths never reported.
        assert_eq!(
            state.looks.keys().collect::<Vec<_>>(),
            [&PathBuf::from("data.csv")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keeps_what_the_walk_could_not_reach_without_calling_it_deleted() {
        use std::os::unix::fs::PermissionsExt;

        let (root, _project) = project();
        let root = root.path();
        write(root, "closed/a.csv", "a");
        write(root, "unread/b.csv", "b");
        let mut state = looked_through(root);
        let saved = state.looks.len();
        // As a walk finds a folder it cannot look into, which it does not stop for.
        let mut now = looked_through(root);
        now.looks.remove(Path::new("closed/a.csv"));
        now.looks.remove(Path::new("unread/b.csv"));
        let closed = root.join("closed");

        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let filter = Filter::new(root).unwrap();
        let changes = state.compare(&now, root, &filter);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(changes, []);
        assert_eq!(state.looks.len(), saved);
    }

    #[cfg(unix)]
    #[test]
    fn reads_back_any_name_and_refuses_a_path_outside_the_folder() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut state = FolderState::default();
        let look = Look::File {
            size: 3,
            modified: Some("2026-11-07T10:11:12.123456789Z".parse().unwrap()),
        };
        // Sorted by name first, as a walk and `take_under` need, which text does not do.
        for name in [b"a/b".as_slice(), b"a-b", b"a", b"latin1-\xe9\n.csv"] {
            state
                .looks
                .insert(PathBuf::from(OsStr::from_bytes(name)), look.clone());
        }
        state.looks.insert(PathBuf::from("folder"), Look::Folder);

        let json = state.to_json().unwrap();

        assert_eq!(FolderState::from_json(&json).unwrap(), state);
        let text = String::from_utf8(json).unwrap();
        assert!(
            text.contains(r#""latin1-\\xe9\\n.csv":{"file":{"size":3,"#),
            "{text}"
        );
        assert!(text.contains(r#""folder":"folder""#), "{text}");
        for outside in ["../secret", "/etc/passwd", "data/../../secret"] {
            let text = format!(r#"{{"paths":{{"{outside}":"folder"}}}}"#);
            assert_eq!(
                FolderState::from_json(text.as_bytes())
                    .unwrap_err()
                    .to_string(),
                format!("it names {outside}, which is not a path in the project folder")
            );
        }
    }

    #[test]
    fn moves_past_a_set_with_what_was_in_a_renamed_folder_as_last_decided() {
        let file = |size| Look::File {
            size,
            modified: None,
        };
        let now = [
            ("new", Look::Folder),
            ("new/raw", Look::Folder),
            ("new/raw/c.csv", file(3)),
            ("new/raw/edited.csv", file(40)),
            ("new/raw/new.csv", file(5)),
        ]
        .map(|(path, look)| (PathBuf::from(path), look));
        let mut state = FolderState::default();
        for (path, look) in [
            ("a.csv", file(1)),
            ("b.csv", file(2)),
            ("old", Look::Folder),
            ("old/raw", Look::Folder),
            ("old/raw/c.csv", file(3)),
            ("old/raw/edited.csv", file(4)),
            ("older", Look::Folder),
        ] {
            state.looks.insert(path.into(), look);
        }

        state.apply(vec![
            Move::See("a.csv".into(), file(10)),
            Move::Forget("b.csv".into()),
            Move::Rename {
                from: "old".into(),
                to: "new".into(),
                now: now.into(),
            },
            Move::See("new".into(), Look::Folder),
        ]);

        assert_eq!(
            state.looks.into_iter().collect::<Vec<_>>(),
            [
                ("a.csv".into(), file(10)),
                ("new".into(), Look::Folder),
                ("new/raw".into(), Look::Folder),
                ("new/raw/c.csv".into(), file(3)),
                ("older".into(), Look::Folder),
            ]
        );
    }

    #[tokio::test]
    async fn gives_a_renamed_folder_no_look_from_what_took_its_old_name_since() {
        let (root, _project) = project();
        let root = root.path();
        write(root, "a/x.csv", "reviewed");
        write(root, "a/y.csv", "same");
        let mut state = looked_through(root);
        let filter = Filter::new(root).unwrap();
        fs::rename(root.join("a"), root.join("b")).unwrap();
        // A newer set's verdict decides a new folder of the old name while the rename waits.
        write(root, "a/x.csv", "another file");
        let newer = [
            Change::Created("a".into()),
            Change::Created("a/x.csv".into()),
        ];
        state.apply(moves(&newer, root, &filter));

        let rename = [Change::Renamed {
            from: "a".into(),
            to: "b".into(),
        }];
        let before_the_review = moves(&rename, root, &filter);
        // During the review, as `cp -p` does, which leaves the look the state has for a/x.csv.
        fs::copy(root.join("a/x.csv"), root.join("b/x.csv")).unwrap();
        File::options()
            .write(true)
            .open(root.join("b/x.csv"))
            .unwrap()
            .set_modified(
                fs::metadata(root.join("a/x.csv"))
                    .unwrap()
                    .modified()
                    .unwrap(),
            )
            .unwrap();
        state.apply(before_the_review);

        assert_eq!(
            compared(&mut state, root),
            [
                Change::Created("a".into()),
                Change::Created("a/x.csv".into()),
                Change::Created("b/x.csv".into()),
            ]
        );
    }

    #[tokio::test]
    async fn looks_at_the_paths_of_a_set_as_they_are_before_its_review() {
        let (root, _project) = project();
        let root = root.path();
        write(root, "edited.csv", "two");
        write(
            root,
            "cache/CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        write(root, "cache/a.csv", "a");
        let filter = Filter::new(root).unwrap();
        let changes = [
            Change::Changed("edited.csv".into()),
            Change::Created("gone.csv".into()),
            Change::Created("cache/a.csv".into()),
            Change::Deleted("deleted.csv".into()),
            Change::Renamed {
                from: "from.csv".into(),
                to: "gone-too.csv".into(),
            },
        ];

        let moves = moves(&changes, root, &filter);

        let [Move::See(edited, Look::File { size: 3, .. }), rest @ ..] = &moves[..] else {
            panic!("{moves:?}");
        };
        assert_eq!(edited, Path::new("edited.csv"));
        assert_eq!(
            rest,
            [
                Move::Forget("gone.csv".into()),
                Move::Forget("cache/a.csv".into()),
                Move::Forget("deleted.csv".into()),
                Move::Rename {
                    from: "from.csv".into(),
                    to: "gone-too.csv".into(),
                    now: BTreeMap::new(),
                },
                Move::Forget("gone-too.csv".into()),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn saves_at_most_once_a_minute_and_on_request() {
        let (root, project) = project();
        write(root.path(), "a.csv", "a");
        let keeper = Arc::new(StateKeeper::new(project.clone()));
        let filter = Filter::new(root.path()).unwrap();
        keeper.start(looked_through(root.path()), &filter, id_of(root.path()));
        let first = project.folder_state().unwrap().unwrap();
        let decide = async |set: usize, path: &str| {
            write(root.path(), path, "b");
            let changes = [Change::Created(path.into())];
            keeper.before_review(set, &changes, &filter).await;
            keeper.decided(set);
        };

        let saved = async {
            let saved_paths = || {
                let state = project.folder_state().unwrap().unwrap();
                state.looks.len()
            };
            decide(1, "b.csv").await;
            tokio::time::sleep(Duration::from_secs(59)).await;
            let before_a_minute = saved_paths();
            tokio::time::sleep(Duration::from_secs(2)).await;
            let after_a_minute = saved_paths();
            decide(2, "c.csv").await;
            tokio::time::sleep(Duration::from_secs(58)).await;
            let before_the_next_minute = saved_paths();
            tokio::time::sleep(Duration::from_secs(3)).await;
            let after_the_next_minute = saved_paths();
            decide(3, "d.csv").await;
            keeper.save().unwrap();
            let when_stopping = saved_paths();
            [
                before_a_minute,
                after_a_minute,
                before_the_next_minute,
                after_the_next_minute,
                when_stopping,
            ]
        };

        let report = |problem| panic!("{problem}");
        tokio::select! {
            () = keeper.clone().save_when_due(report) => panic!("it never ends"),
            saved = saved => assert_eq!(saved, [1, 2, 2, 3, 4]),
        }
        assert_eq!(first.looks.len(), 1);
    }

    #[test]
    fn neither_brings_back_a_folder_that_went_nor_saves_once_stopped() {
        let (root, project) = project();
        let keeper = StateKeeper::new(project.clone());
        keeper.moving().unsaved = true;
        keeper.stop_saving();

        keeper.save().unwrap();
        assert_eq!(project.folder_state().unwrap(), None);

        let moved = root.path().join("project");
        let gone = Project::new(moved.clone());
        let error = StateKeeper::new(gone.clone());
        error.moving().unsaved = true;
        assert!(error.save().is_err());
        assert!(!moved.exists());
    }

    #[tokio::test]
    async fn saves_nothing_over_a_folder_that_took_its_place_before_the_watcher_noticed() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        let project = Project::new(root.clone());
        project.create().unwrap();
        let keeper = StateKeeper::new(project.clone());
        let filter = Filter::new(&root).unwrap();
        keeper.start(looked_through(&root), &filter, id_of(&root));

        fs::rename(&root, parent.path().join("moved")).unwrap();
        project.create().unwrap();
        keeper.moving().unsaved = true;
        keeper.save().unwrap();

        assert_eq!(project.folder_state().unwrap(), None);
    }

    #[tokio::test]
    async fn takes_no_folder_that_came_after_the_walk_for_the_one_it_walked() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        let project = Project::new(root.clone());
        project.create().unwrap();
        write(&root, "walked.csv", "a");
        let walked = looked_through(&root);
        let walked_id = id_of(&root);

        fs::rename(&root, parent.path().join("moved")).unwrap();
        project.create().unwrap();
        write(&root, "other.csv", "b");
        let filter = Filter::new(&root).unwrap();
        let keeper = StateKeeper::new(project.clone());
        let started = keeper.start(walked, &filter, walked_id);
        keeper.moving().unsaved = true;
        keeper.save().unwrap();

        assert_eq!(started.changes, []);
        assert_eq!(started.problems, Vec::<String>::new());
        assert_eq!(project.folder_state().unwrap(), None);
    }
}
