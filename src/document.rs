//! FUSE-independent core: derives a directory/file view (heading -> directory,
//! section body -> `content.md`) from `mq_markdown`'s flat `Vec<Node>`, and
//! applies mutations by splicing that list and re-deriving the view. Nothing
//! here knows about FUSE, which keeps it unit-testable without mounting.

use std::ops::Range;

use mq_markdown::{Heading, Markdown, Node, Point, Position, Text};
use rustc_hash::FxHashMap;

/// The canonical file name for a section's own body content.
pub const CONTENT_FILE: &str = "content.md";

/// A predicate tested against a single heading node to decide whether the
/// section it opens should be exposed in the mount (`--filter`). Kept as a
/// generic closure type, rather than referencing the query engine that
/// builds one, so this FUSE-independent core never depends on it — only the
/// `mount`-feature-gated CLI layer knows about `mq-lang`.
pub type HeadingFilter = dyn Fn(&Node) -> bool + Send + Sync;

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

    /// End of this section's entire subtree, vs. [`Section::own_content_range`] which stops at the first child heading.
    fn subtree_end(&self) -> usize {
        self.children
            .last()
            .map_or(self.own_content_range.end, Section::subtree_end)
    }

    /// The deepest heading depth in this section's subtree, including itself.
    fn max_depth(&self) -> u8 {
        self.children
            .iter()
            .map(Section::max_depth)
            .max()
            .unwrap_or(self.depth)
            .max(self.depth)
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
    #[error("cannot move a heading into its own subtree")]
    WouldCreateCycle,
    #[error("heading would be nested deeper than level 6, which Markdown headings can't express")]
    TooDeep,
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

    /// Like [`Self::tree`], but prunes out any section that neither matches
    /// `filter` itself nor has a descendant that does (`--filter`). A
    /// section's ancestors stay visible even when they don't match
    /// themselves, so a match remains reachable by path; the synthetic root
    /// always survives.
    pub fn tree_filtered(&self, filter: &HeadingFilter) -> SectionTree {
        let tree = self.tree();
        SectionTree {
            front_matter: tree.front_matter,
            root: self.pruned(tree.root, filter),
        }
    }

    fn pruned(&self, section: Section, filter: &HeadingFilter) -> Section {
        let children = section
            .children
            .into_iter()
            .filter_map(|child| self.pruned_child(child, filter))
            .collect();
        Section { children, ..section }
    }

    fn pruned_child(&self, section: Section, filter: &HeadingFilter) -> Option<Section> {
        let self_matches = section
            .heading_index
            .is_some_and(|idx| filter(&self.markdown.nodes[idx]));
        let section = self.pruned(section, filter);
        (self_matches || !section.children.is_empty()).then_some(section)
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
        self.splice_with_reflow(range, new_nodes);
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
        let at = parent.own_content_range.end;
        self.splice_with_reflow(at..at, vec![heading]);
        Ok(())
    }

    /// Splices `new_nodes` into `range`, touching line positions only at the
    /// two seams (old content <-> `new_nodes` <-> old content) instead of
    /// renumbering the whole document — mq-markdown infers list/table/
    /// paragraph grouping from position gaps, so a blanket renumbering
    /// corrupts those anywhere in the document, not just the edited range.
    fn splice_with_reflow(&mut self, range: Range<usize>, mut new_nodes: Vec<Node>) -> Vec<Node> {
        // Hand-built nodes (e.g. `insert_heading`'s new `Heading`) have no
        // position; give them one so the shift below has something to use.
        Self::normalize_missing_positions(&mut new_nodes);

        let left_end_line = range
            .start
            .checked_sub(1)
            .and_then(|i| self.markdown.nodes.get(i))
            .and_then(Node::position)
            .map(|p| p.end.line);

        if let Some(first_line) = new_nodes.first().and_then(Node::position).map(|p| p.start.line) {
            let target = left_end_line.map_or(1, |l| l + 2);
            Self::shift_top_level_positions(&mut new_nodes, target as isize - first_line as isize);
        }

        let anchor_line = new_nodes
            .last()
            .and_then(Node::position)
            .map(|p| p.end.line)
            .or(left_end_line);

        if let Some(anchor) = anchor_line
            && let Some(next_start) = self
                .markdown
                .nodes
                .get(range.end)
                .and_then(Node::position)
                .map(|p| p.start.line)
        {
            let target = anchor + 2;
            Self::shift_top_level_positions(
                &mut self.markdown.nodes[range.end..],
                target as isize - next_start as isize,
            );
        }

        self.markdown.nodes.splice(range, new_nodes).collect()
    }

    fn normalize_missing_positions(nodes: &mut [Node]) {
        let mut prev_end_line: Option<usize> = None;
        for node in nodes {
            if node.position().is_none() {
                let line = prev_end_line.map_or(1, |l| l + 2);
                node.set_position(Some(Position {
                    start: Point { line, column: 1 },
                    end: Point { line, column: 1 },
                }));
            }
            prev_end_line = node.position().map(|p| p.end.line);
        }
    }

    fn shift_top_level_positions(nodes: &mut [Node], delta: isize) {
        if delta == 0 {
            return;
        }
        for node in nodes {
            if let Some(mut pos) = node.position() {
                pos.start.line = pos.start.line.saturating_add_signed(delta);
                pos.end.line = pos.end.line.saturating_add_signed(delta);
                node.set_position(Some(pos));
            }
        }
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
        self.splice_with_reflow(idx..idx + 1, Vec::new());
        Ok(())
    }

    /// Reparents `section` (and its subtree) under `new_parent`, renaming it to `new_name`. Both must be from the same, not-yet-mutated `tree()` snapshot.
    pub fn move_heading(
        &mut self,
        section: &Section,
        new_parent: &Section,
        new_name: &str,
    ) -> Result<(), MutationError> {
        if new_parent.children.iter().any(|c| c.name == new_name) {
            return Err(MutationError::AlreadyExists(new_name.to_string()));
        }
        let start = section.heading_index.ok_or(MutationError::NotADirectory)?;
        let end = section.subtree_end();
        if let Some(new_parent_idx) = new_parent.heading_index
            && (start..end).contains(&new_parent_idx)
        {
            return Err(MutationError::WouldCreateCycle);
        }

        let depth_shift = new_parent.depth as i16 + 1 - section.depth as i16;
        if section.max_depth() as i16 + depth_shift > 6 {
            return Err(MutationError::TooDeep);
        }

        let mut moved = self.splice_with_reflow(start..end, Vec::new());
        for node in &mut moved {
            if let Node::Heading(h) = node {
                h.depth = (h.depth as i16 + depth_shift) as u8;
            }
        }
        if let Some(Node::Heading(h)) = moved.first_mut() {
            h.values = vec![Node::Text(Text {
                value: new_name.to_string(),
                position: None,
            })];
        }

        let removed_len = end - start;
        let insert_at = if new_parent.own_content_range.end <= start {
            new_parent.own_content_range.end
        } else {
            new_parent.own_content_range.end - removed_len
        };
        self.splice_with_reflow(insert_at..insert_at, moved);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn insert_heading_keeps_tight_reference_definitions_tight() {
        let mut doc = Document::parse("# A\n\n[a]: http://a\n[b]: http://b\n[c]: http://c\n\n# B\n").unwrap();
        let tree = doc.tree();
        let a = tree.find(&["A"]).unwrap();
        doc.insert_heading(a, "New").unwrap();
        assert_eq!(
            doc.render(),
            "# A\n\n[a]: http://a\n[b]: http://b\n[c]: http://c\n\n## New\n\n# B\n"
        );
    }

    #[test]
    fn insert_heading_keeps_a_formatted_paragraph_as_one_paragraph() {
        // mq-markdown stores each inline run (plain text, bold, link, ...) of
        // one paragraph as its own flat top-level node; an edit elsewhere in
        // the document must not blow them apart into separate paragraphs.
        let mut doc = Document::parse("# A\n\nHello **bold** and [a link](http://x) here.\n\n# B\n").unwrap();
        let tree = doc.tree();
        let a = tree.find(&["A"]).unwrap();
        doc.insert_heading(a, "New").unwrap();
        assert_eq!(
            doc.render(),
            "# A\n\nHello **bold** and [a link](http://x) here.\n\n## New\n\n# B\n"
        );
    }

    #[test]
    fn replace_section_content_leaves_an_unrelated_sections_list_and_formatting_intact() {
        let mut doc = Document::parse("# A\n\nHello **bold** [x](http://x).\n\n- a\n- b\n\n# B\n\nold\n").unwrap();
        let tree = doc.tree();
        let b = tree.find(&["B"]).unwrap();
        doc.replace_section_content(b.own_content_range.clone(), "new\n")
            .unwrap();
        assert_eq!(
            doc.render(),
            "# A\n\nHello **bold** [x](http://x).\n\n- a\n- b\n\n# B\n\nnew\n"
        );
    }

    #[test]
    fn replace_section_content_preserves_the_blank_line_after_the_heading() {
        let mut doc = Document::parse("# Title\n\nline1\nline2\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        let mut body = doc.render_range(title.own_content_range.clone());
        body.push_str("line3\n");
        doc.replace_section_content(title.own_content_range.clone(), &body)
            .unwrap();
        assert_eq!(doc.render(), "# Title\n\nline1\nline2\nline3\n");
    }

    #[test]
    fn replace_section_content_keeps_a_tight_list_tight_after_appending_an_item() {
        let mut doc = Document::parse("# Ideas\n\n- ship the watch mode\n\n# Todo\n\n- record a demo\n").unwrap();
        let tree = doc.tree();
        let ideas = tree.find(&["Ideas"]).unwrap();
        let mut body = doc.render_range(ideas.own_content_range.clone());
        body.push_str("- also ship the demo gif\n");
        doc.replace_section_content(ideas.own_content_range.clone(), &body)
            .unwrap();
        assert_eq!(
            doc.render(),
            "# Ideas\n\n- ship the watch mode\n- also ship the demo gif\n\n# Todo\n\n- record a demo\n"
        );
    }

    #[test]
    fn insert_heading_keeps_a_nested_list_intact() {
        let mut doc = Document::parse("# Title\n\n- a\n  - a1\n  - a2\n- b\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        doc.insert_heading(title, "New").unwrap();
        assert_eq!(doc.render(), "# Title\n\n- a\n  - a1\n  - a2\n- b\n\n## New\n");
    }

    #[test]
    fn replace_section_content_preserves_blank_lines_between_paragraphs() {
        let mut doc = Document::parse("# Title\n\npara one\n\npara two\n\npara three\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        let body = doc.render_range(title.own_content_range.clone());
        doc.replace_section_content(title.own_content_range.clone(), &body)
            .unwrap();
        assert_eq!(doc.render(), "# Title\n\npara one\n\npara two\n\npara three\n");
    }

    #[test]
    fn insert_heading_preserves_blank_lines_around_the_new_heading() {
        let mut doc = Document::parse("# Title\n\nintro\n\n## A\n\na body\n").unwrap();
        let tree = doc.tree();
        let title = tree.find(&["Title"]).unwrap();
        doc.insert_heading(title, "New").unwrap();
        assert_eq!(doc.render(), "# Title\n\nintro\n\n## New\n\n## A\n\na body\n");
    }

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

    fn depth_filter(depth: u8) -> impl Fn(&Node) -> bool {
        move |node: &Node| matches!(node, Node::Heading(h) if h.depth == depth)
    }

    #[test]
    fn tree_filtered_keeps_only_matching_sections() {
        let doc = Document::parse("# A\n\n## B\n\nb\n\n# C\n\nc\n").unwrap();
        let tree = doc.tree_filtered(&depth_filter(1));
        assert_eq!(names(&tree.root.children), vec!["A", "C"]);
        assert!(tree.find(&["A", "B"]).is_none(), "a non-matching child is pruned");
    }

    #[test]
    fn tree_filtered_keeps_non_matching_ancestors_of_a_match() {
        let doc = Document::parse("# A\n\n## B\n\n### C\n\nleaf\n").unwrap();
        // Only depth-3 headings match; A and B don't match themselves but
        // must stay visible so C remains reachable by path.
        let tree = doc.tree_filtered(&depth_filter(3));
        let c = tree
            .find(&["A", "B", "C"])
            .expect("matching leaf survives under its ancestors");
        assert_eq!(doc.render_range(c.own_content_range.clone()).trim(), "leaf");
        assert!(
            tree.find(&["A"]).unwrap().children.len() == 1,
            "A keeps only the path to the match"
        );
    }

    #[test]
    fn tree_filtered_with_no_match_collapses_to_an_empty_root() {
        let doc = Document::parse("# A\n\n## B\n\nb\n").unwrap();
        let tree = doc.tree_filtered(&depth_filter(9));
        assert!(tree.root.children.is_empty());
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
    fn move_heading_keeps_a_tight_list_in_the_moved_subtree_tight() {
        let mut doc = Document::parse("# A\n\n## B\n\n- a\n- b\n\n# D\n\nd\n").unwrap();
        let tree = doc.tree();
        let b = tree.find(&["A", "B"]).unwrap();
        let d = tree.find(&["D"]).unwrap();
        doc.move_heading(b, d, "Moved").unwrap();
        assert_eq!(doc.render(), "# A\n\n# D\n\nd\n\n## Moved\n\n- a\n- b\n");
    }

    #[test]
    fn move_heading_reparents_a_section_and_its_subtree() {
        let mut doc = Document::parse("# A\n\nintro\n\n## B\n\nb\n\n### C\n\nc\n\n# D\n\nd\n\n## E\n\ne\n").unwrap();
        let tree = doc.tree();
        let b = tree.find(&["A", "B"]).unwrap();
        let e = tree.find(&["D", "E"]).unwrap();
        doc.move_heading(b, e, "Moved").unwrap();

        let tree = doc.tree();
        assert!(tree.find(&["A", "B"]).is_none(), "B no longer under A");
        assert!(tree.find(&["A"]).unwrap().children.is_empty(), "A has no children left");

        let moved = tree.find(&["D", "E", "Moved"]).expect("B now lives under D/E as Moved");
        assert_eq!(doc.render_range(moved.own_content_range.clone()).trim(), "b");
        let child = tree
            .find(&["D", "E", "Moved", "C"])
            .expect("C moved along with its parent");
        assert_eq!(doc.render_range(child.own_content_range.clone()).trim(), "c");

        // Depths must stay valid ATX headings (round-trips through render+reparse).
        let reparsed = Document::parse(&doc.render()).unwrap();
        assert!(reparsed.tree().find(&["D", "E", "Moved", "C"]).is_some());
    }

    #[test]
    fn move_heading_rejects_duplicate_name_at_destination() {
        let mut doc = Document::parse("# A\n\n## X\n\nax\n\n# B\n\n## X\n\nbx\n").unwrap();
        let tree = doc.tree();
        let a_x = tree.find(&["A", "X"]).unwrap();
        let b = tree.find(&["B"]).unwrap();
        let err = doc.move_heading(a_x, b, "X").unwrap_err();
        assert!(matches!(err, MutationError::AlreadyExists(_)));
    }

    #[test]
    fn move_heading_rejects_moving_into_its_own_subtree() {
        let mut doc = Document::parse("# A\n\n## B\n\nb\n").unwrap();
        let tree = doc.tree();
        let a = tree.find(&["A"]).unwrap();
        let b = tree.find(&["A", "B"]).unwrap();
        let err = doc.move_heading(a, b, "A").unwrap_err();
        assert!(matches!(err, MutationError::WouldCreateCycle));
    }

    #[test]
    fn move_heading_rejects_when_it_would_exceed_heading_depth_6() {
        let mut doc = Document::parse(
            "# 1\n\n## 2\n\n### 3\n\n#### 4\n\n##### 5\n\n###### 6\n\nleaf\n\n\
             # Other\n\n## OA\n\n### OB\n\n#### OC\n\n##### OD\n\n###### OE\n\noe leaf\n",
        )
        .unwrap();
        let tree = doc.tree();
        // "2" carries its depth-6 leaf; under depth-6 "OE" that needs depth 7+.
        let two = tree.find(&["1", "2"]).unwrap();
        let oe = tree.find(&["Other", "OA", "OB", "OC", "OD", "OE"]).unwrap();
        let err = doc.move_heading(two, oe, "Moved").unwrap_err();
        assert!(matches!(err, MutationError::TooDeep));
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

        /// Writing a section's own unmodified body back into itself (as every
        /// save does, edited or not) must not disturb the full-document
        /// render beyond whatever a plain parse-and-render already
        /// normalizes away: regression coverage for blank lines collapsing
        /// because spliced-in nodes lost their position.
        #[test]
        fn replace_section_content_with_unchanged_body_is_a_full_document_no_op(
            specs in heading_tree(3),
            path_index in any::<proptest::sample::Index>(),
        ) {
            let mut paths = Vec::new();
            all_paths(&specs, &[], &mut paths);
            prop_assume!(!paths.is_empty());
            let path = &paths[path_index.index(paths.len())];
            let refs: Vec<&str> = path.iter().map(String::as_str).collect();

            let mut src = String::new();
            render_specs(&specs, 1, &mut src);
            let baseline = Document::parse(&src).unwrap().render();

            let mut doc = Document::parse(&src).unwrap();
            let range = doc.tree().find(&refs).unwrap().own_content_range.clone();
            let unchanged_body = doc.render_range(range.clone());
            doc.replace_section_content(range, &unchanged_body).unwrap();

            assert_eq!(doc.render(), baseline);
        }
    }
}
