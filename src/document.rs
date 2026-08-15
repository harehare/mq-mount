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
        // Strip position: `body`'s line numbers restart at 0, and the
        // renderer's line-delta spacing can saturate to 0 against a later
        // node in the full document, gluing them together with no separator.
        let mut new_nodes = body
            .parse::<Markdown>()
            .map_err(|e| MutationError::Parse(e.to_string()))?
            .nodes;
        for node in &mut new_nodes {
            node.set_position(None);
        }
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

    #[test]
    fn replace_section_content_does_not_glue_onto_the_next_heading() {
        let mut doc = Document::parse("# Title\n\n## A\n\nold\n\n## B\n\nb body\n").unwrap();
        let tree = doc.tree();
        let a = tree.find(&["Title", "A"]).unwrap();
        doc.replace_section_content(a.own_content_range.clone(), "new\n")
            .unwrap();

        let reparsed = Document::parse(&doc.render()).unwrap();
        let tree = reparsed.tree();
        let a = tree.find(&["Title", "A"]).expect("A survives re-parse");
        assert_eq!(reparsed.render_range(a.own_content_range.clone()).trim(), "new");
        assert!(tree.find(&["Title", "B"]).is_some(), "B survives re-parse");
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    struct HeadingSpec {
        name: String,
        body: String,
        children: Vec<HeadingSpec>,
    }

    const NAME_POOL: [&str; 6] = ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot"];

    fn heading_tree(max_depth: u32) -> impl Strategy<Value = Vec<HeadingSpec>> {
        let leaf = (
            proptest::sample::subsequence(NAME_POOL.to_vec(), 0..=4),
            proptest::collection::vec("[a-zA-Z]{1,8}", 0..3),
        )
            .prop_map(|(names, words)| {
                names
                    .into_iter()
                    .map(|name| HeadingSpec {
                        name: name.to_string(),
                        body: words.join(" "),
                        children: Vec::new(),
                    })
                    .collect::<Vec<_>>()
            });

        leaf.prop_recursive(max_depth, 20, 4, |inner| {
            (
                proptest::sample::subsequence(NAME_POOL.to_vec(), 0..=4),
                proptest::collection::vec("[a-zA-Z]{1,8}", 0..3),
                proptest::collection::vec(inner, 0..3),
            )
                .prop_map(|(names, words, mut child_groups)| {
                    names
                        .into_iter()
                        .map(|name| HeadingSpec {
                            name: name.to_string(),
                            body: words.join(" "),
                            children: child_groups.pop().unwrap_or_default(),
                        })
                        .collect::<Vec<_>>()
                })
        })
    }

    fn render_specs(specs: &[HeadingSpec], depth: usize, out: &mut String) {
        for spec in specs {
            out.push_str(&"#".repeat(depth));
            out.push(' ');
            out.push_str(&spec.name);
            out.push_str("\n\n");
            if !spec.body.is_empty() {
                out.push_str(&spec.body);
                out.push_str("\n\n");
            }
            render_specs(&spec.children, depth + 1, out);
        }
    }

    fn assert_matches_spec(specs: &[HeadingSpec], sections: &[Section]) {
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            sections.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        );
        for (spec, section) in specs.iter().zip(sections) {
            assert_matches_spec(&spec.children, &section.children);
        }
    }

    /// Every path (in document order) to a heading in `specs`, as the name
    /// chain `find` expects.
    fn all_paths(specs: &[HeadingSpec], prefix: &[String], out: &mut Vec<Vec<String>>) {
        for spec in specs {
            let path: Vec<String> = prefix.iter().cloned().chain([spec.name.clone()]).collect();
            out.push(path.clone());
            all_paths(&spec.children, &path, out);
        }
    }

    proptest! {
        /// Regression coverage for the position-based line-delta bug: no
        /// combination of heading nesting/order should render into something
        /// that fails to reparse back into the same structure.
        #[test]
        fn heading_structure_round_trips_through_render_and_reparse(specs in heading_tree(3)) {
            let mut src = String::new();
            render_specs(&specs, 1, &mut src);
            prop_assume!(!src.trim().is_empty());

            let doc = Document::parse(&src).unwrap();
            assert_matches_spec(&specs, &doc.tree().root.children);

            let reparsed = Document::parse(&doc.render()).unwrap();
            assert_matches_spec(&specs, &reparsed.tree().root.children);
        }

        /// `replace_section_content` on an arbitrary section, with an
        /// arbitrary plain-text body, must never disturb sibling/ancestor
        /// heading structure or fail to reparse.
        #[test]
        fn replace_section_content_preserves_sibling_headings(
            specs in heading_tree(3),
            path_index in any::<proptest::sample::Index>(),
            new_body in "[a-zA-Z]{1,8}( [a-zA-Z]{1,8}){0,3}",
        ) {
            let mut paths = Vec::new();
            all_paths(&specs, &[], &mut paths);
            prop_assume!(!paths.is_empty());
            let path = &paths[path_index.index(paths.len())];

            let mut src = String::new();
            render_specs(&specs, 1, &mut src);
            let mut doc = Document::parse(&src).unwrap();

            let refs: Vec<&str> = path.iter().map(String::as_str).collect();
            let range = doc.tree().find(&refs).unwrap().own_content_range.clone();
            doc.replace_section_content(range, &format!("{new_body}\n")).unwrap();

            let reparsed = Document::parse(&doc.render()).unwrap();
            assert_matches_spec(&specs, &reparsed.tree().root.children);
            let section = reparsed.tree().find(&refs).unwrap().clone();
            assert_eq!(reparsed.render_range(section.own_content_range).trim(), new_body.trim());
        }
    }
}
