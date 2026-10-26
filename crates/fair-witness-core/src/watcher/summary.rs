use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{Change, Kind};
use crate::escape;

/// Above this many changes, a change set is listed per folder.
pub const MAX_LISTED: usize = 50;

/// One line of a change set as shown to the researcher and the model.
#[derive(Debug, PartialEq, Eq)]
pub enum Listed<'a> {
    One(&'a Change),
    /// `count` changes of one kind under `folder`. Renames also name the
    /// folder they came `from`.
    Many {
        kind: Kind,
        count: usize,
        folder: PathBuf,
        from: Option<PathBuf>,
    },
}

impl Listed<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            Listed::One(change) => change.kind(),
            Listed::Many { kind, .. } => *kind,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Listed::One(Change::Renamed { from, to }) => {
                format!("{} -> {}", escape::path(from), escape::path(to))
            }
            Listed::One(change) => escape::path(change.path()),
            Listed::Many {
                count,
                folder,
                from: Some(from),
                ..
            } => format!("{count} paths from {} to {}", label(from), label(folder)),
            Listed::Many { count, folder, .. } if folder.as_os_str().is_empty() => {
                format!("{count} paths in the project folder")
            }
            Listed::Many { count, folder, .. } => {
                format!("{count} paths under {}/", escape::path(folder))
            }
        }
    }
}

/// Lists a change set path by path, or, when that would take more than
/// [`MAX_LISTED`] lines, per kind of change and top-level folder, and failing
/// that per kind alone.
pub fn summarize(changes: &[Change]) -> Vec<Listed<'_>> {
    if changes.len() <= MAX_LISTED {
        return changes.iter().map(Listed::One).collect();
    }
    let by_folder = group(changes, true);
    if by_folder.len() <= MAX_LISTED {
        by_folder
    } else {
        group(changes, false)
    }
}

fn group<'a>(changes: &'a [Change], by_folder: bool) -> Vec<Listed<'a>> {
    let top = |path: &'a Path| path.iter().next().unwrap_or_default();
    // A created folder joins the group of the paths created inside it.
    let folders: HashSet<&OsStr> = changes
        .iter()
        .filter(|change| change.path().components().count() > 1)
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
                folder: common_folder(members.iter().map(|change| change.path())),
                from: (kind == Kind::Renamed).then(|| {
                    common_folder(members.iter().filter_map(|change| match change {
                        Change::Renamed { from, .. } => Some(from.as_path()),
                        _ => None,
                    }))
                }),
            },
        })
        .collect()
}

fn common_folder<'a>(paths: impl Iterator<Item = &'a Path>) -> PathBuf {
    let paths: Vec<&Path> = paths.collect();
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

fn label(folder: &Path) -> String {
    if folder.as_os_str().is_empty() {
        "the project folder".to_string()
    } else {
        format!("{}/", escape::path(folder))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_a_long_change_set_per_folder() {
        let mut changes = vec![
            Change::Created(PathBuf::from("data")),
            Change::Changed(PathBuf::from("notes.txt")),
        ];
        changes
            .extend((0..60).map(|i| Change::Created(PathBuf::from(format!("data/raw/{i}.csv")))));
        changes.extend((0..3).map(|i| Change::Deleted(PathBuf::from(format!("old/{i}.csv")))));
        changes.extend((0..2).map(|i| Change::Renamed {
            from: PathBuf::from(format!("raw/{i}.csv")),
            to: PathBuf::from(format!("archive/2026/{i}.csv")),
        }));

        let lines: Vec<String> = summarize(&changes)
            .iter()
            .map(|listed| format!("{}: {}", listed.kind().verb(), listed.describe()))
            .collect();

        assert_eq!(
            lines,
            [
                "created: 61 paths under data/",
                "changed: notes.txt",
                "renamed: 2 paths from raw/ to archive/2026/",
                "deleted: 3 paths under old/",
            ]
        );
    }

    #[test]
    fn lists_per_kind_alone_when_there_are_too_many_folders() {
        let changes: Vec<Change> = (0..60)
            .flat_map(|i| {
                [
                    Change::Created(PathBuf::from(format!("{i}/a.csv"))),
                    Change::Created(PathBuf::from(format!("{i}/b.csv"))),
                ]
            })
            .collect();

        assert_eq!(
            summarize(&changes),
            [Listed::Many {
                kind: Kind::Created,
                count: 120,
                folder: PathBuf::new(),
                from: None,
            }]
        );
    }
}
