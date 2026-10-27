use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

    pub(crate) fn relative_to(self, root: &Path) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
