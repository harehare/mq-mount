//! FUSE-independent core: derives a directory/file view (heading -> directory,
//! section body -> `content.md`) from `mq_markdown`'s flat `Vec<Node>`, and
//! applies mutations by splicing that list and re-deriving the view. Nothing
//! here knows about FUSE, which keeps it unit-testable without mounting.

use std::ops::Range;

use mq_markdown::{Heading, Markdown, Node, Text};
use rustc_hash::FxHashMap;

/// The canonical file name for a section's own body content.
pub const CONTENT_FILE: &str = "content.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrontMatterKind {
    Yaml,
    Toml,
}

impl FrontMatterKind {
    pub fn file_name(self) -> &'static str {
        match self {
            FrontMatterKind::Yaml => "_frontmatter.yaml",
            FrontMatterKind::Toml => "_frontmatter.toml",
        }
    }
}

/// A directory in the mounted view: either the document root or a heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Slug used as the directory name. Empty for the root.
    pub name: String,
    /// Heading depth; 0 for the (synthetic) root.
    pub depth: u8,
    /// Index of the `Node::Heading` this section was built from, `None` for root.
    pub heading_index: Option<usize>,
    /// Node range that becomes this section's `content.md`: everything after
    /// the heading up to (not including) the very next heading of ANY depth.
    pub own_content_range: Range<usize>,
    pub children: Vec<Section>,
}

impl Section {
    /// POSIX-strict emptiness check for `rmdir`: no subdirectories and an
    /// already-empty `content.md`. `rm -r` still deletes whole subtrees since
    /// it walks bottom-up.
    pub fn is_removable(&self) -> bool {
        self.children.is_empty() && self.own_content_range.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionTree {
    pub front_matter: Option<(FrontMatterKind, usize)>,
    pub root: Section,
}

impl SectionTree {
    pub fn build(nodes: &[Node]) -> Self {
        let mut idx = 0usize;
        let front_matter = match nodes.first() {
            Some(Node::Yaml(_)) => {
                idx = 1;
                Some((FrontMatterKind::Yaml, 0))
            }
            Some(Node::Toml(_)) => {
                idx = 1;
                Some((FrontMatterKind::Toml, 0))
            }
            _ => None,
        };
        let root_content_start = idx;
        while idx < nodes.len() && !matches!(nodes[idx], Node::Heading(_)) {
            idx += 1;
        }

        struct Open {
            section: Section,
            seen_names: FxHashMap<String, u32>,
        }

        let root = Section {
            name: String::new(),
            depth: 0,
            heading_index: None,
            own_content_range: root_content_start..idx,
            children: Vec::new(),
        };
        let mut stack: Vec<Open> = vec![Open {
            section: root,
            seen_names: FxHashMap::default(),
        }];

        while idx < nodes.len() {
            let Node::Heading(Heading { depth, .. }) = &nodes[idx] else {
                unreachable!("loop invariant: idx always lands on a Heading node")
            };
            let depth = *depth;

            // Close whichever section is currently accumulating content: the
            // very next heading (any depth) always ends its own_content_range.
            stack.last_mut().unwrap().section.own_content_range.end = idx;

            // Pop sections that this heading is not nested under (depth <= their own).
            while stack.len() > 1 && stack.last().unwrap().section.depth >= depth {
                let closed = stack.pop().unwrap().section;
                stack.last_mut().unwrap().section.children.push(closed);
            }

            let name = unique_name(&mut stack.last_mut().unwrap().seen_names, &nodes[idx].value());
            stack.push(Open {
                section: Section {
                    name,
                    depth,
                    heading_index: Some(idx),
                    own_content_range: (idx + 1)..(idx + 1),
                    children: Vec::new(),
                },
                seen_names: FxHashMap::default(),
            });

            idx += 1;
            while idx < nodes.len() && !matches!(nodes[idx], Node::Heading(_)) {
                idx += 1;
            }
        }

        stack.last_mut().unwrap().section.own_content_range.end = nodes.len();
        while stack.len() > 1 {
            let closed = stack.pop().unwrap().section;
            stack.last_mut().unwrap().section.children.push(closed);
        }

        SectionTree {
            front_matter,
            root: stack.pop().unwrap().section,
        }
    }

    /// Finds the section whose directory path (slug components, root-relative)
    /// matches `path`. An empty path returns the root.
    pub fn find(&self, path: &[&str]) -> Option<&Section> {
        let mut current = &self.root;
        for component in path {
            current = current.children.iter().find(|c| c.name == *component)?;
        }
        Some(current)
    }
}

/// Slugifies a heading title (or, reused by `fs.rs` for a file stem) and
/// disambiguates against sibling names already seen under the same parent
/// (duplicates get `-2`, `-3`, ...).
pub(crate) fn unique_name(seen: &mut FxHashMap<String, u32>, title: &str) -> String {
    let base = slugify(title);
    let count = seen.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 { base } else { format!("{base}-{count}") }
}

pub(crate) fn slugify(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    let trimmed = match trimmed {
        "" | "." | ".." => "untitled",
        other => other,
    };
    trimmed.chars().take(200).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum MutationError {
    #[error("a heading named {0:?} already exists in this directory")]
    AlreadyExists(String),
    #[error("directory is not empty")]
    NotEmpty,
    #[error("not a directory")]
    NotADirectory,
    #[error("failed to parse markdown: {0}")]
    Parse(String),
}

/// Owns the flat, parsed node list. [`SectionTree`] is always a fresh view
/// recomputed on demand, never mutated independently.
pub struct Document {
    markdown: Markdown,
}

impl Document {
    pub fn parse(source: &str) -> Result<Self, MutationError> {
        Ok(Self {
            markdown: source
                .parse::<Markdown>()
                .map_err(|e| MutationError::Parse(e.to_string()))?,
        })
    }

    pub fn tree(&self) -> SectionTree {
        SectionTree::build(&self.markdown.nodes)
    }

    /// Full-document render, used both to serve reads of a fresh mount and to
    /// persist every mutation back to the source file.
    pub fn render(&self) -> String {
        self.markdown.to_string()
    }

    pub fn render_range(&self, range: Range<usize>) -> String {
        Markdown::new(self.markdown.nodes[range].to_vec()).to_string()
    }

    pub fn frontmatter_value(&self, node_index: usize) -> &str {
        match &self.markdown.nodes[node_index] {
            Node::Yaml(y) => &y.value,
            Node::Toml(t) => &t.value,
            _ => "",
        }
    }

    pub fn set_frontmatter(&mut self, node_index: usize, value: String) {
        match &mut self.markdown.nodes[node_index] {
            Node::Yaml(y) => y.value = value,
            Node::Toml(t) => t.value = value,
            _ => {}
        }
    }

    /// Reparses `body` and splices it into a section's content range. Heading
    /// lines typed into `body` become new subdirectories on the next rebuild.
    pub fn replace_section_content(&mut self, range: Range<usize>, body: &str) -> Result<(), MutationError> {
        let new_nodes = body
            .parse::<Markdown>()
            .map_err(|e| MutationError::Parse(e.to_string()))?
            .nodes;
        self.markdown.nodes.splice(range, new_nodes);
        Ok(())
    }

    pub fn insert_heading(&mut self, parent: &Section, name: &str) -> Result<(), MutationError> {
        if parent.children.iter().any(|c| c.name == name) {
            return Err(MutationError::AlreadyExists(name.to_string()));
        }
        let heading = Node::Heading(Heading {
            depth: parent.depth.saturating_add(1).max(1),
            values: vec![Node::Text(Text {
                value: name.to_string(),
                position: None,
            })],
            position: None,
        });
        self.markdown.nodes.insert(parent.own_content_range.end, heading);
        Ok(())
    }

    pub fn rename_heading(&mut self, section: &Section, new_name: &str) -> Result<(), MutationError> {
        let idx = section.heading_index.ok_or(MutationError::NotADirectory)?;
        if let Node::Heading(h) = &mut self.markdown.nodes[idx] {
            h.values = vec![Node::Text(Text {
                value: new_name.to_string(),
                position: None,
            })];
        }
        Ok(())
    }

    pub fn remove_heading(&mut self, section: &Section) -> Result<(), MutationError> {
        if !section.is_removable() {
            return Err(MutationError::NotEmpty);
        }
        let idx = section.heading_index.ok_or(MutationError::NotADirectory)?;
        self.markdown.nodes.remove(idx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn names(sections: &[Section]) -> Vec<&str> {
        sections.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn flat_document_has_no_headings() {
        let doc = Document::parse("just a paragraph\n").unwrap();
        let tree = doc.tree();
        assert!(tree.root.children.is_empty());
        assert_eq!(doc.render_range(tree.root.own_content_range), "just a paragraph\n");
    }

    #[test]
    fn nested_headings_build_expected_tree() {
        let doc = Document::parse("# A\n\nintro\n\n## B\n\nb body\n\n### C\n\nc body\n\n## D\n\nd body\n").unwrap();
        let tree = doc.tree();
        assert_eq!(names(&tree.root.children), vec!["A"]);
        let a = &tree.root.children[0];
        assert_eq!(names(&a.children), vec!["B", "D"]);
        let b = &a.children[0];
        assert_eq!(names(&b.children), vec!["C"]);
        assert_eq!(doc.render_range(a.own_content_range.clone()).trim(), "intro");
        assert_eq!(doc.render_range(b.own_content_range.clone()).trim(), "b body");
        assert_eq!(
            doc.render_range(b.children[0].own_content_range.clone()).trim(),
            "c body"
        );
    }

    #[test]
    fn duplicate_sibling_titles_get_suffixed() {
        let doc = Document::parse("## Foo\n\na\n\n## Foo\n\nb\n\n## Foo\n\nc\n").unwrap();
        let tree = doc.tree();
        assert_eq!(names(&tree.root.children), vec!["Foo", "Foo-2", "Foo-3"]);
    }

    #[test]
    fn heading_shallower_than_container_becomes_root_sibling() {
        // A `#` typed inside a `###`-deep section's content is a document-order
        // event, not indentation: it closes out to a top-level section.
        let doc = Document::parse("### Deep\n\n# Shallow\n").unwrap();
        let tree = doc.tree();
        assert_eq!(names(&tree.root.children), vec!["Deep", "Shallow"]);
    }

    #[rstest]
    #[case("---\nkey: value\n---\n# H\n", FrontMatterKind::Yaml)]
    fn frontmatter_is_extracted(#[case] src: &str, #[case] kind: FrontMatterKind) {
        let doc = Document::parse(src).unwrap();
        let tree = doc.tree();
        let (found_kind, idx) = tree.front_matter.expect("front matter present");
        assert_eq!(found_kind, kind);
        assert_eq!(doc.frontmatter_value(idx).trim(), "key: value");
    }

    #[test]
    fn insert_rename_and_remove_heading_round_trip() {
        let mut doc = Document::parse("# Title\n\nintro\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        doc.insert_heading(title, "Sub").unwrap();

        let tree = doc.tree();
        let sub = tree.find(&["Title", "Sub"]).unwrap();
        assert_eq!(sub.depth, 2);
        assert!(sub.is_removable());

        doc.rename_heading(sub, "Renamed").unwrap();
        let tree = doc.tree();
        assert!(tree.find(&["Title", "Sub"]).is_none());
        let renamed = tree.find(&["Title", "Renamed"]).unwrap();
        assert!(renamed.is_removable());

        doc.remove_heading(renamed).unwrap();
        let tree = doc.tree();
        assert!(tree.find(&["Title", "Renamed"]).is_none());
        assert!(doc.render().contains("# Title"));
        assert!(!doc.render().contains("Renamed"));
    }

    #[test]
    fn insert_heading_rejects_duplicate_sibling_name() {
        let mut doc = Document::parse("# Title\n\n## Sub\n\nbody\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        let err = doc.insert_heading(title, "Sub").unwrap_err();
        assert!(matches!(err, MutationError::AlreadyExists(_)));
    }

    #[test]
    fn remove_heading_rejects_nonempty_section() {
        let mut doc = Document::parse("# Title\n\nintro\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        let err = doc.remove_heading(title).unwrap_err();
        assert!(matches!(err, MutationError::NotEmpty));
    }

    #[test]
    fn replace_section_content_with_new_heading_creates_subdirectory() {
        let mut doc = Document::parse("# Title\n\nintro\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        doc.replace_section_content(title.own_content_range.clone(), "intro\n\n## New\n\nchild body\n")
            .unwrap();

        let tree = doc.tree();
        let new_section = tree
            .find(&["Title", "New"])
            .expect("typed heading became a subdirectory");
        assert_eq!(
            doc.render_range(new_section.own_content_range.clone()).trim(),
            "child body"
        );
    }
}
