//! Shared admission checks for folder and ZIP staging. No graph or filesystem writes.
use caseless::Caseless;
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

/// Comparison only: never use this key as a source path or document title.
pub(crate) fn path_key(path: &str) -> String {
    path.split('/')
        .map(|part| part.nfc().default_case_fold().nfc().collect::<String>())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug)]
struct PathRecord {
    original: String,
    kind: Kind,
    explicit: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub entries: usize,
    pub expanded_bytes: u64,
    pub note_bytes: u64,
    pub attachment_bytes: u64,
    pub depth: usize,
    pub path_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 10_000,
            expanded_bytes: 1 << 30,
            note_bytes: 32 << 20,
            attachment_bytes: 256 << 20,
            depth: 32,
            path_bytes: 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Directory,
    Note,
    Attachment,
}

#[derive(Debug)]
pub(crate) struct Admission {
    limits: Limits,
    paths: BTreeMap<String, PathRecord>,
    entries: usize,
    expanded: u64,
}

impl Admission {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            paths: BTreeMap::new(),
            entries: 0,
            expanded: 0,
        }
    }

    // Call before opening or allocating an entry. Symlinks/special files must
    // already have been rejected by the source adapter, not followed here.
    pub fn admit(&mut self, raw: &str, directory: bool) -> Result<(String, Kind), String> {
        if self.entries >= self.limits.entries {
            return Err("entry limit exceeded".into());
        }
        let path = raw.strip_suffix('/').filter(|_| directory).unwrap_or(raw);
        if path.is_empty()
            || path.len() > self.limits.path_bytes
            || path.contains('\\')
            || path.chars().any(|c| c.is_control() || c == ':')
        {
            return Err("invalid relative path".into());
        }
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() > self.limits.depth
            || parts.iter().any(|p| {
                p.is_empty() || *p == "." || *p == ".." || p.ends_with(' ') || p.ends_with('.')
            })
        {
            return Err("unsafe path component or excessive depth".into());
        }
        let key = path_key(path);
        let kind = if directory {
            Kind::Directory
        } else if key.ends_with(".md") || key.ends_with(".markdown") {
            Kind::Note
        } else {
            Kind::Attachment
        };
        // Reserve implicit directories too: A/x and a/y cannot silently merge.
        // Validate the entire entry before mutating the index.
        let mut pending = Vec::new();
        for end in path
            .match_indices('/')
            .map(|(i, _)| i)
            .chain(std::iter::once(path.len()))
        {
            let original = &path[..end];
            let explicit = end == path.len();
            let entry_kind = if explicit { kind } else { Kind::Directory };
            let entry_key = if explicit {
                key.clone()
            } else {
                path_key(original)
            };
            if let Some(previous) = self.paths.get(&entry_key) {
                if previous.original != original
                    || previous.kind != entry_kind
                    || (explicit && previous.explicit)
                {
                    return Err(format!(
                        "portable path collision: {:?} conflicts with {:?}",
                        previous.original, original
                    ));
                }
            }
            pending.push((
                entry_key,
                PathRecord {
                    original: original.to_owned(),
                    kind: entry_kind,
                    explicit,
                },
            ));
        }
        for (key, record) in pending {
            self.paths
                .entry(key)
                .and_modify(|old| old.explicit |= record.explicit)
                .or_insert(record);
        }
        self.entries += 1;
        Ok((path.to_owned(), kind))
    }

    // Account actual streamed bytes, never the archive's claimed expanded size.
    // Caller owns per-entry count and stops reading immediately on any error.
    pub fn account(
        &mut self,
        kind: Kind,
        entry_bytes: &mut u64,
        chunk: usize,
    ) -> Result<(), String> {
        let next = entry_bytes
            .checked_add(chunk as u64)
            .ok_or("entry size overflow")?;
        let total = self
            .expanded
            .checked_add(chunk as u64)
            .ok_or("total size overflow")?;
        let cap = match kind {
            Kind::Directory => 0,
            Kind::Note => self.limits.note_bytes,
            Kind::Attachment => self.limits.attachment_bytes,
        };
        if next > cap || total > self.limits.expanded_bytes {
            return Err("expanded byte limit exceeded".into());
        }
        *entry_bytes = next;
        self.expanded = total;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_collision_keys_preserve_original_names() {
        for (left, right) in [
            ("caf\u{e9}.md", "cafe\u{301}.md"),
            ("Stra\u{df}e.md", "STRASSE.md"),
            ("\u{3c2}.md", "\u{3c3}.md"),
        ] {
            let mut a = Admission::new(Limits::default());
            assert_eq!(a.admit(left, false).unwrap().0, left);
            let error = a.admit(right, false).unwrap_err();
            assert!(
                error.contains(&format!("{left:?}")) && error.contains(&format!("{right:?}")),
                "{error}"
            );
        }
    }
    #[test]
    fn distinct_scripts_and_compatibility_characters_remain_distinct() {
        let mut a = Admission::new(Limits::default());
        for path in [
            "a.md",
            "\u{430}.md",
            "\u{ff41}.md",
            "1.md",
            "\u{2460}.md",
            "A/same.md",
            "B/same.md",
        ] {
            a.admit(path, false).unwrap();
        }
    }
    #[test]
    fn implicit_folder_aliases_are_rejected_without_poisoning_index() {
        let mut a = Admission::new(Limits::default());
        a.admit("A/one.md", false).unwrap();
        assert!(a.admit("a/two.md", false).is_err());
        a.admit("A/two.md", false).unwrap();
        a.admit("Caf\u{e9}/one.md", false).unwrap();
        assert!(a.admit("Cafe\u{301}/two.md", false).is_err());
        a.admit("Caf\u{e9}/two.md", false).unwrap();
        a.admit("Caf\u{e9}/", true).unwrap();
        assert!(a.admit("Caf\u{e9}/", true).is_err());
        assert!(a.admit("CAF\u{c9}", false).is_err());
    }
    #[test]
    fn unsafe_paths() {
        for path in [
            "",
            "/absolute",
            "../escape",
            "a/../b",
            "./a",
            "a//b",
            "a\\b",
            "C:/x",
            "a\0b",
            "a./b",
            "a /b",
        ] {
            assert!(
                Admission::new(Limits::default())
                    .admit(path, false)
                    .is_err(),
                "{path:?}"
            );
        }
    }
    #[test]
    fn duplicate_and_tree_collisions() {
        let mut a = Admission::new(Limits::default());
        a.admit("Notes/", true).unwrap();
        a.admit("Notes/Hello.md", false).unwrap();
        assert!(a.admit("notes/HELLO.md", false).is_err());
        assert!(a.admit("Notes/Hello.md/child", false).is_err());
        let mut a = Admission::new(Limits::default());
        a.admit("implicit/child.md", false).unwrap();
        assert!(a.admit("implicit", false).is_err());
        assert!(a.admit("implicit/", true).is_ok());
    }
    #[test]
    fn bounds_and_classification() {
        let limits = Limits {
            entries: 2,
            depth: 2,
            path_bytes: 10,
            ..Limits::default()
        };
        let mut a = Admission::new(limits);
        assert!(a.admit("a/b/c", false).is_err());
        assert!(a.admit("longfilename", false).is_err());
        assert_eq!(a.admit("a.MD", false).unwrap().1, Kind::Note);
        assert_eq!(a.admit("b.pdf", false).unwrap().1, Kind::Attachment);
        assert!(a.admit("c", false).is_err());
    }
    #[test]
    fn actual_bytes_and_failed_accounting_are_bounded() {
        let mut a = Admission::new(Limits {
            note_bytes: 4,
            attachment_bytes: 6,
            expanded_bytes: 7,
            ..Limits::default()
        });
        let mut note = 0;
        a.account(Kind::Note, &mut note, 4).unwrap();
        assert!(a.account(Kind::Note, &mut note, 1).is_err());
        assert_eq!(note, 4);
        let mut attachment = 0;
        a.account(Kind::Attachment, &mut attachment, 3).unwrap();
        assert!(a.account(Kind::Attachment, &mut attachment, 1).is_err());
        assert!(a.account(Kind::Directory, &mut 0, 1).is_err());
    }
}
