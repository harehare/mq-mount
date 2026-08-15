//! Assigns stable FUSE inode numbers to entries derived from a
//! [`crate::document::SectionTree`] and keeps them stable across rebuilds:
//! renamed sections keep their inode, and freed inodes are never reused
//! (stale references must not resolve to an unrelated newer entry). Since
//! `Node` has no identity of its own, children are matched to their previous
//! incarnation by exact name, falling back to positional pairing only when
//! exactly one child is left unmatched on each side (a rename).

use rustc_hash::FxHashMap;

use crate::document::{FrontMatterKind, Section, SectionTree};

pub const ROOT_INO: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MountPath {
    Dir(Vec<String>),
    Content(Vec<String>),
    FrontMatter(FrontMatterKind),
}

#[derive(Debug, Clone, Default)]
struct ShadowNode {
    name: String,
    dir_ino: u64,
    content_ino: u64,
    children: Vec<ShadowNode>,
}

pub struct InodeTable {
    shadow: ShadowNode,
    frontmatter: Option<(FrontMatterKind, u64)>,
    path_to_ino: FxHashMap<MountPath, u64>,
    ino_to_path: FxHashMap<u64, MountPath>,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

fn alloc(next_ino: &mut u64) -> u64 {
    let ino = *next_ino;
    *next_ino += 1;
    ino
}

impl InodeTable {
    pub fn new() -> Self {
        Self {
            shadow: ShadowNode::default(),
            frontmatter: None,
            path_to_ino: FxHashMap::default(),
            ino_to_path: FxHashMap::default(),
        }
    }

    pub fn ino_for(&self, path: &MountPath) -> Option<u64> {
        self.path_to_ino.get(path).copied()
    }

    pub fn path_for(&self, ino: u64) -> Option<&MountPath> {
        self.ino_to_path.get(&ino)
    }

    /// This file's own root directory inode — stable across resyncs, but only
    /// meaningful once at least one `sync` has run.
    pub fn root_dir_ino(&self) -> u64 {
        self.shadow.dir_ino
    }

    /// All currently-live `(ino, path)` pairs for this file, used to rebuild
    /// the cross-file `ino -> file` reverse index after a resync.
    pub fn entries(&self) -> impl Iterator<Item = (u64, &MountPath)> {
        self.ino_to_path.iter().map(|(&ino, path)| (ino, path))
    }

    /// Re-derives inode assignments from a freshly built `SectionTree`,
    /// carrying forward inodes for sections that still exist. `next_ino` is a
    /// counter shared across every mounted file, so inode numbers stay
    /// globally unique across the whole mount.
    pub fn sync(&mut self, next_ino: &mut u64, tree: &SectionTree) {
        self.path_to_ino.clear();
        self.ino_to_path.clear();

        let mut old_root = std::mem::take(&mut self.shadow);
        if old_root.content_ino == 0 {
            old_root.content_ino = alloc(next_ino);
        }
        if old_root.dir_ino == 0 {
            old_root.dir_ino = alloc(next_ino);
        }

        self.record(MountPath::Dir(Vec::new()), old_root.dir_ino);
        self.record(MountPath::Content(Vec::new()), old_root.content_ino);

        let children = self.sync_children(next_ino, old_root.children, &tree.root.children, &[]);
        self.shadow = ShadowNode {
            name: String::new(),
            dir_ino: old_root.dir_ino,
            content_ino: old_root.content_ino,
            children,
        };

        match (tree.front_matter, self.frontmatter) {
            (Some((kind, _)), Some((old_kind, ino))) if old_kind == kind => {
                self.record(MountPath::FrontMatter(kind), ino);
            }
            (Some((kind, _)), _) => {
                let ino = alloc(next_ino);
                self.frontmatter = Some((kind, ino));
                self.record(MountPath::FrontMatter(kind), ino);
            }
            (None, _) => self.frontmatter = None,
        }
    }

    fn record(&mut self, path: MountPath, ino: u64) {
        self.path_to_ino.insert(path.clone(), ino);
        self.ino_to_path.insert(ino, path);
    }

    fn sync_children(
        &mut self,
        next_ino: &mut u64,
        old_children: Vec<ShadowNode>,
        new_children: &[Section],
        parent_path: &[String],
    ) -> Vec<ShadowNode> {
        let mut old_by_name: FxHashMap<&str, usize> = FxHashMap::default();
        for (i, c) in old_children.iter().enumerate() {
            old_by_name.insert(c.name.as_str(), i);
        }

        let mut matches: Vec<Option<usize>> = vec![None; new_children.len()];
        let mut consumed = vec![false; old_children.len()];
        for (ni, sec) in new_children.iter().enumerate() {
            if let Some(&oi) = old_by_name.get(sec.name.as_str())
                && !consumed[oi]
            {
                matches[ni] = Some(oi);
                consumed[oi] = true;
            }
        }

        let unmatched_old: Vec<usize> = (0..old_children.len()).filter(|&i| !consumed[i]).collect();
        let unmatched_new: Vec<usize> = (0..new_children.len()).filter(|&i| matches[i].is_none()).collect();
        if let (&[oi], &[ni]) = (unmatched_old.as_slice(), unmatched_new.as_slice()) {
            matches[ni] = Some(oi);
        }

        let mut old_children: Vec<Option<ShadowNode>> = old_children.into_iter().map(Some).collect();
        let mut result = Vec::with_capacity(new_children.len());

        for (ni, sec) in new_children.iter().enumerate() {
            let mut path = parent_path.to_vec();
            path.push(sec.name.clone());

            let (dir_ino, content_ino, old_grandchildren) = match matches[ni] {
                Some(oi) => {
                    let old = old_children[oi].take().unwrap();
                    (old.dir_ino, old.content_ino, old.children)
                }
                None => (alloc(next_ino), alloc(next_ino), Vec::new()),
            };

            self.record(MountPath::Dir(path.clone()), dir_ino);
            self.record(MountPath::Content(path.clone()), content_ino);

            let children = self.sync_children(next_ino, old_grandchildren, &sec.children, &path);
            result.push(ShadowNode {
                name: sec.name.clone(),
                dir_ino,
                content_ino,
                children,
            });
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn synced(table: &mut InodeTable, next_ino: &mut u64, src: &str) -> SectionTree {
        let doc = Document::parse(src).unwrap();
        let tree = doc.tree();
        table.sync(next_ino, &tree);
        tree
    }

    #[test]
    fn root_dir_ino_is_stable_across_resyncs() {
        let mut table = InodeTable::new();
        let mut next_ino = ROOT_INO + 1;
        synced(&mut table, &mut next_ino, "# A\n");
        let root_ino = table.ino_for(&MountPath::Dir(vec![])).unwrap();
        assert_eq!(table.root_dir_ino(), root_ino);

        synced(&mut table, &mut next_ino, "# A\n\n# B\n");
        assert_eq!(table.ino_for(&MountPath::Dir(vec![])), Some(root_ino));
        assert_eq!(table.root_dir_ino(), root_ino);
    }

    #[test]
    fn unrelated_edit_keeps_existing_inos_stable() {
        let mut table = InodeTable::new();
        let mut next_ino = ROOT_INO + 1;
        synced(&mut table, &mut next_ino, "# A\n\nx\n\n# B\n\ny\n");
        let a_ino = table.ino_for(&MountPath::Dir(vec!["A".into()])).unwrap();
        let b_ino = table.ino_for(&MountPath::Dir(vec!["B".into()])).unwrap();

        synced(&mut table, &mut next_ino, "# A\n\nx changed\n\n# B\n\ny\n\n# C\n\nz\n");
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["A".into()])), Some(a_ino));
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["B".into()])), Some(b_ino));
        assert!(table.ino_for(&MountPath::Dir(vec!["C".into()])).is_some());
    }

    #[test]
    fn rename_in_place_preserves_inode() {
        let mut table = InodeTable::new();
        let mut next_ino = ROOT_INO + 1;
        synced(&mut table, &mut next_ino, "# Old Name\n\nbody\n");
        let old_ino = table.ino_for(&MountPath::Dir(vec!["Old Name".into()])).unwrap();
        let old_content_ino = table.ino_for(&MountPath::Content(vec!["Old Name".into()])).unwrap();

        synced(&mut table, &mut next_ino, "# New Name\n\nbody\n");
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["Old Name".into()])), None);
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["New Name".into()])), Some(old_ino));
        assert_eq!(
            table.ino_for(&MountPath::Content(vec!["New Name".into()])),
            Some(old_content_ino)
        );
    }

    #[test]
    fn removing_a_sibling_does_not_disturb_the_others() {
        let mut table = InodeTable::new();
        let mut next_ino = ROOT_INO + 1;
        synced(&mut table, &mut next_ino, "# A\n\n# B\n\n# C\n");
        let a_ino = table.ino_for(&MountPath::Dir(vec!["A".into()])).unwrap();
        let c_ino = table.ino_for(&MountPath::Dir(vec!["C".into()])).unwrap();

        synced(&mut table, &mut next_ino, "# A\n\n# C\n");
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["A".into()])), Some(a_ino));
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["C".into()])), Some(c_ino));
        assert_eq!(table.ino_for(&MountPath::Dir(vec!["B".into()])), None);
    }

    #[test]
    fn removed_inode_is_never_reused_even_if_the_name_comes_back() {
        let mut table = InodeTable::new();
        let mut next_ino = ROOT_INO + 1;
        synced(&mut table, &mut next_ino, "# A\n\n# B\n\n# C\n");
        let original_b_ino = table.ino_for(&MountPath::Dir(vec!["B".into()])).unwrap();

        synced(&mut table, &mut next_ino, "# A\n\n# C\n");
        synced(&mut table, &mut next_ino, "# A\n\n# B\n\n# C\n");

        let new_b_ino = table.ino_for(&MountPath::Dir(vec!["B".into()])).unwrap();
        assert_ne!(
            original_b_ino, new_b_ino,
            "a re-added section must not reuse a retired inode"
        );
    }

    #[test]
    fn frontmatter_ino_persists_while_present_and_is_retired_when_removed() {
        let mut table = InodeTable::new();
        let mut next_ino = ROOT_INO + 1;
        synced(&mut table, &mut next_ino, "---\nkey: 1\n---\n# A\n");
        let fm_ino = table.ino_for(&MountPath::FrontMatter(FrontMatterKind::Yaml)).unwrap();

        synced(&mut table, &mut next_ino, "---\nkey: 2\n---\n# A\n");
        assert_eq!(
            table.ino_for(&MountPath::FrontMatter(FrontMatterKind::Yaml)),
            Some(fm_ino)
        );

        synced(&mut table, &mut next_ino, "# A\n");
        assert_eq!(table.ino_for(&MountPath::FrontMatter(FrontMatterKind::Yaml)), None);

        synced(&mut table, &mut next_ino, "---\nkey: 3\n---\n# A\n");
        let new_fm_ino = table.ino_for(&MountPath::FrontMatter(FrontMatterKind::Yaml)).unwrap();
        assert_ne!(fm_ino, new_fm_ino);
    }
}
