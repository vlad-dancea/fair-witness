use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{Change, Kind};

/// Above this many changes, a change set is listed per folder.
pub const MAX_LISTED: usize = 50;

/// One line of a change set as shown to the researcher and the model.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed<'a> {
    One(&'a Change),
    /// `count` changes of one kind under `folder`, relative to the project root.
    Many {
        kind: Kind,
        count: usize,
        folder: PathBuf,
    },
}

impl Listed<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            Listed::One(change) => change.kind(),
            Listed::Many { kind, .. } => *kind,
        }
    }

    /// What changed, with paths relative to `root`.
    pub fn describe(&self, root: &Path) -> String {
        match self {
            Listed::One(Change::Renamed { from, to }) => format!(
                "{} -> {}",
                relative(root, from).display(),
                relative(root, to).display()
            ),
            Listed::One(change) => relative(root, change.path()).display().to_string(),
            Listed::Many { count, folder, .. } if folder.as_os_str().is_empty() => {
                format!("{count} paths in the project folder")
            }
            Listed::Many { count, folder, .. } => {
                format!("{count} paths under {}/", folder.display())
            }
        }
    }
}

/// Lists a change set path by path, or, when that would take more than
/// [`MAX_LISTED`] lines, per kind of change and top-level folder, and failing
/// that per kind alone.
pub fn summarize<'a>(root: &Path, changes: &'a [Change]) -> Vec<Listed<'a>> {
    if changes.len() <= MAX_LISTED {
        return changes.iter().map(Listed::One).collect();
    }
    let by_folder = group(root, changes, true);
    if by_folder.len() <= MAX_LISTED {
        by_folder
    } else {
        group(root, changes, false)
    }
}

fn group<'a>(root: &Path, changes: &'a [Change], by_folder: bool) -> Vec<Listed<'a>> {
    let top = |path: &'a Path| relative(root, path).iter().next().unwrap_or_default();
    // A created folder joins the group of the paths created inside it.
    let folders: HashSet<&OsStr> = changes
        .iter()
        .filter(|change| relative(root, change.path()).components().count() > 1)
        .map(|change| top(change.path()))
        .collect();
    let mut groups: BTreeMap<(Kind, &OsStr), Vec<&'a Change>> = BTreeMap::new();
    for change in changes {
        let folder = Some(top(change.path()))
            .filter(|folder| by_folder && folders.contains(folder))
            .unwrap_or_default();
        groups
            .entry((change.kind(), folder))
            .or_default()
            .push(change);
    }
    groups
        .into_iter()
        .map(|((kind, _), members)| match members[..] {
            [one] => Listed::One(one),
            _ => Listed::Many {
                kind,
                count: members.len(),
                folder: common_folder(root, &members),
            },
        })
        .collect()
}

fn common_folder(root: &Path, members: &[&Change]) -> PathBuf {
    let paths: Vec<&Path> = members
        .iter()
        .map(|change| relative(root, change.path()))
        .collect();
    let mut common: PathBuf = paths[0].iter().collect();
    for path in &paths[1..] {
        common = common
            .iter()
            .zip(path.iter())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect();
    }
    // The same file changed twice is under its folder, not under itself.
    if paths.iter().all(|path| *path == common) {
        common.pop();
    }
    common
}

fn relative<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_a_long_change_set_per_folder() {
        let root = Path::new("/project");
        let mut changes = vec![
            Change::Created(root.join("data")),
            Change::Changed(root.join("notes.txt")),
        ];
        changes.extend((0..60).map(|i| Change::Created(root.join(format!("data/raw/{i}.csv")))));
        changes.extend((0..3).map(|i| Change::Deleted(root.join(format!("old/{i}.csv")))));

        let lines: Vec<String> = summarize(root, &changes)
            .iter()
            .map(|listed| format!("{}: {}", listed.kind().verb(), listed.describe(root)))
            .collect();

        assert_eq!(
            lines,
            [
                "created: 61 paths under data/",
                "changed: notes.txt",
                "deleted: 3 paths under old/",
            ]
        );
    }

    #[test]
    fn lists_per_kind_alone_when_there_are_too_many_folders() {
        let root = Path::new("/project");
        let changes: Vec<Change> = (0..60)
            .flat_map(|i| {
                [
                    Change::Created(root.join(format!("{i}/a.csv"))),
                    Change::Created(root.join(format!("{i}/b.csv"))),
                ]
            })
            .collect();

        assert_eq!(
            summarize(root, &changes),
            [Listed::Many {
                kind: Kind::Created,
                count: 120,
                folder: PathBuf::new(),
            }]
        );
    }
}
