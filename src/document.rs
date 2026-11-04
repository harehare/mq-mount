//! FUSE-independent core: derives a directory/file view (heading -> directory,
//! section body -> `content.md`) from `mq_markdown`'s flat `Vec<Node>`, and
//! applies mutations by splicing that list and re-deriving the view. Nothing
//! here knows about FUSE, which keeps it unit-testable without mounting.

use std::ops::Range;

use mq_markdown::{Heading, HeadingDepth, Markdown, Node, Point, Position, Text};
use rustc_hash::FxHashMap;

/// The canonical file name for a section's own body content.
pub const CONTENT_FILE: &str = "content.md";

/// How a heading's text becomes its directory name (`--slug`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum SlugStyle {
    /// The heading text as-is: spaces and punctuation kept, only path
    /// separators and control characters replaced (`Getting Started!`).
    #[default]
    Raw,
    /// Lowercased, whitespace turned into `-`, and anything but letters,
    /// digits, `-`, `_`, and `.` dropped (`getting-started`). Non-ASCII
    /// letters are kept.
    Kebab,
}

impl SlugStyle {
    /// The directory name for a heading titled `title`. Always a fixed
    /// point (`apply(apply(x)) == apply(x)`), which is what lets
    /// `mkdir`/`rename` accept exactly the names this would produce.
    pub fn apply(self, title: &str) -> String {
        match self {
            SlugStyle::Raw => slugify(title),
            SlugStyle::Kebab => kebab_slug(title),
        }
    }
}

fn kebab_slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || matches!(c, '_' | '.') {
            out.push(c);
        } else if (c.is_whitespace() || c == '-' || c == '/' || c == '\\') && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let truncated: String = out.chars().take(200).collect();
    match truncated.trim_end_matches('-') {
        "" | "." | ".." => "untitled".to_string(),
        other => other.to_string(),
    }
}

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

    /// This section with its children's own children dropped: enough to list
    /// a directory or check a sibling name without cloning the whole subtree.
    pub fn shallow(&self) -> Section {
        Section {
            children: self.children.iter().map(Section::clone_header).collect(),
            ..self.clone_header()
        }
    }

    fn clone_header(&self) -> Section {
        Section {
            name: self.name.clone(),
            depth: self.depth,
            heading_index: self.heading_index,
            own_content_range: self.own_content_range.clone(),
            children: Vec::new(),
        }
    }

    /// Applies a node-count change of `delta` made to the content range
    /// `[start, end)` of the section owning it: everything at or after `end`
    /// moves, and the owner's range grows or shrinks at its end.
    fn shift_after_edit(&mut self, start: usize, end: usize, delta: isize) {
        let shift = |i: usize| i.saturating_add_signed(delta);
        if self.own_content_range.start == start && self.own_content_range.end == end {
            self.own_content_range.end = shift(end);
        } else {
            if self.own_content_range.start >= end {
                self.own_content_range.start = shift(self.own_content_range.start);
            }
            if self.own_content_range.end >= end {
                self.own_content_range.end = shift(self.own_content_range.end);
            }
        }
        if let Some(h) = &mut self.heading_index
            && *h >= end
        {
            *h = shift(*h);
        }
        for child in &mut self.children {
            child.shift_after_edit(start, end, delta);
        }
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
    pub fn build_with(nodes: &[Node], slug: SlugStyle) -> Self {
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
            let depth = depth.get();

            // Close whichever section is currently accumulating content: the
            // very next heading (any depth) always ends its own_content_range.
            stack.last_mut().unwrap().section.own_content_range.end = idx;

            // Pop sections that this heading is not nested under (depth <= their own).
            while stack.len() > 1 && stack.last().unwrap().section.depth >= depth {
                let closed = stack.pop().unwrap().section;
                stack.last_mut().unwrap().section.children.push(closed);
            }

            let name = unique_name_with(&mut stack.last_mut().unwrap().seen_names, &nodes[idx].value(), slug);
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

    /// Updates every node index after a body-only edit (no headings added or
    /// removed) that replaced `[start, end)` with `delta` more nodes, without
    /// rebuilding the tree.
    pub fn shift_after_edit(&mut self, start: usize, end: usize, delta: isize) {
        if let Some((_, i)) = &mut self.front_matter
            && *i >= end
        {
            *i = i.saturating_add_signed(delta);
        }
        self.root.shift_after_edit(start, end, delta);
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
    unique_name_with(seen, title, SlugStyle::Raw)
}

fn unique_name_with(seen: &mut FxHashMap<String, u32>, title: &str, slug: SlugStyle) -> String {
    let base = slug.apply(title);
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

/// What [`Document::replace_section_content`] did to the node list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentEdit {
    /// How many nodes were added (negative: removed) in the section.
    pub delta: isize,
    /// Whether the new body contains a heading, i.e. the directory tree's
    /// shape changed. If not, [`SectionTree::shift_after_edit`] is enough.
    pub structural: bool,
}

/// Owns the flat, parsed node list. [`SectionTree`] is always a fresh view
/// recomputed on demand, never mutated independently.
pub struct Document {
    markdown: Markdown,
    /// The source text this document was parsed from, split into the same
    /// chunks [`chunk_starts`] cuts the node list into. Lets [`Self::render`]
    /// write back an unedited chunk's original bytes verbatim instead of
    /// mq-markdown's normalized rendering of it. `None` if the parse didn't
    /// carry usable positions, in which case every render is a full one.
    baseline: Option<Baseline>,
    /// Exact text for chunks whose content the user wrote (or whose heading
    /// was renamed/moved), keyed by the chunk's normalized rendering. Lets an
    /// edited section keep the formatting it was typed with (`*` bullets,
    /// table padding, ...) instead of going through mq-markdown's renderer.
    /// Two edited chunks that render identically share an entry.
    verbatim: FxHashMap<String, String>,
    /// For each node, the index of the original chunk it was parsed in, or
    /// [`NO_ORIGIN`] once it was created or changed. A chunk whose nodes all
    /// still point at the same original chunk (and are all there) is
    /// unedited, which [`Self::render`] can see without rendering it.
    /// Empty when there is no baseline.
    origin: Vec<u32>,
    slug: SlugStyle,
}

const NO_ORIGIN: u32 = u32::MAX;

/// See [`Document::baseline`].
struct Baseline {
    source: String,
    /// Byte range in `source` each original chunk occupies, in order. Chunk
    /// `i` ends where chunk `i + 1` starts, so the blank lines between two
    /// chunks belong to the earlier one, and the first chunk always starts
    /// at byte 0 (keeping any leading blank lines).
    chunk_bytes: Vec<Range<usize>>,
    /// How many nodes each original chunk has.
    chunk_lens: Vec<usize>,
}

impl Baseline {
    fn new(source: &str, nodes: &[Node]) -> Option<Self> {
        let mut line_starts = vec![0usize];
        line_starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
        let line_offset = |line: usize| line_starts.get(line.checked_sub(1)?).copied();

        let starts = chunk_starts(nodes);
        let mut chunk_bytes = Vec::with_capacity(starts.len());
        let mut chunk_lens = Vec::with_capacity(starts.len());
        for (i, range) in chunk_ranges(&starts, nodes.len()).enumerate() {
            let start = if i == 0 {
                0
            } else {
                line_offset(nodes[range.start].position()?.start.line as usize)?
            };
            let end = match starts.get(i + 1) {
                Some(&next) => line_offset(nodes[next].position()?.start.line as usize)?,
                None => source.len(),
            };
            if start > end {
                return None;
            }
            chunk_bytes.push(start..end);
            chunk_lens.push(range.len());
        }
        Some(Self {
            source: source.to_string(),
            chunk_bytes,
            chunk_lens,
        })
    }
}

/// Node indices at which the document is cut into independently
/// re-renderable chunks: the front matter, the preamble before the first
/// heading, and each heading (with its own content, up to the next heading
/// of any depth). Headings always start a fresh block, so a chunk never
/// splits a list, table, or paragraph.
fn chunk_starts(nodes: &[Node]) -> Vec<usize> {
    let mut starts = Vec::new();
    if !nodes.is_empty() {
        starts.push(0);
    }
    if matches!(nodes.first(), Some(Node::Yaml(_) | Node::Toml(_))) && nodes.len() > 1 {
        starts.push(1);
    }
    starts.extend(
        nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| *i > 0 && matches!(n, Node::Heading(_)))
            .map(|(i, _)| i),
    );
    starts.dedup();
    starts
}

fn chunk_ranges(starts: &[usize], len: usize) -> impl Iterator<Item = Range<usize>> + '_ {
    starts
        .iter()
        .enumerate()
        .map(move |(i, &start)| start..starts.get(i + 1).copied().unwrap_or(len))
}

fn render_nodes(nodes: &[Node]) -> String {
    Markdown::new(nodes.to_vec()).to_string()
}

/// Splits `text` (a chunk starting at a heading) into the heading's own
/// lines and everything after. `None` if it doesn't start with a heading.
fn split_heading(text: &str) -> Option<(&str, &str)> {
    let line_end = |from: usize| text[from..].find('\n').map_or(text.len(), |i| from + i + 1);
    let first_end = line_end(0);
    if text[..first_end].trim_start_matches(' ').starts_with('#') {
        return Some(text.split_at(first_end));
    }
    // Setext heading: text line(s) up to an `===` / `---` underline.
    let mut end = first_end;
    while end < text.len() {
        let next = line_end(end);
        let line = text[end..next].trim();
        end = next;
        if line.is_empty() {
            return None;
        }
        if line.chars().all(|c| c == '=') || line.chars().all(|c| c == '-') {
            return Some(text.split_at(end));
        }
    }
    None
}

/// A chunk's text taken apart. For a body that isn't empty, `blanks` is the
/// number of blank lines after its last line; for an empty one, the number
/// of newlines after the heading (so it reads the same either way).
struct ChunkParts<'a> {
    head: &'a str,
    gap: &'a str,
    core: &'a str,
    blanks: usize,
}

impl<'a> ChunkParts<'a> {
    fn split(text: &'a str, has_heading: bool) -> Option<Self> {
        let (head, rest) = if has_heading { split_heading(text)? } else { ("", text) };
        if rest.trim().is_empty() {
            return Some(Self {
                head,
                gap: "",
                core: "",
                blanks: rest.matches('\n').count(),
            });
        }
        let lead = rest
            .split_inclusive('\n')
            .take_while(|l| l.trim().is_empty())
            .map(str::len)
            .sum::<usize>();
        let (gap, body) = rest.split_at(lead);
        let core = body.trim_end_matches('\n');
        Some(Self {
            head,
            gap,
            core,
            blanks: (body.len() - core.len()).saturating_sub(1),
        })
    }

    /// The text of a chunk with this heading, gap, body and trailing blank lines.
    fn assemble(head: &str, gap: &str, core: &str, blanks: usize) -> String {
        let mut out = String::from(head);
        if (!core.is_empty() || blanks > 0) && !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if core.is_empty() {
            out.push_str(&"\n".repeat(blanks));
        } else {
            out.push_str(gap);
            out.push_str(core);
            out.push_str(&"\n".repeat(1 + blanks));
        }
        out
    }
}

/// What [`Document::replace_section_content`] needs to know about the chunk
/// it is about to replace.
struct OldChunk {
    head: String,
    gap: String,
    empty: bool,
    core_matches: bool,
    blanks: usize,
}

/// Cuts `body` at the lines its headings start on (as parsed in `nodes`),
/// so each piece becomes one chunk's text. `None` if positions don't line up.
fn split_body_pieces<'a>(body: &'a str, nodes: &[Node]) -> Option<Vec<&'a str>> {
    let lines: Vec<u32> = nodes
        .iter()
        .filter(|n| matches!(n, Node::Heading(_)))
        .map(|n| n.position().map(|p| p.start.line))
        .collect::<Option<_>>()?;
    let mut line_starts = vec![0usize];
    line_starts.extend(body.match_indices('\n').map(|(i, _)| i + 1));
    let mut cuts = vec![0usize];
    for line in lines {
        let at = *line_starts.get((line as usize).checked_sub(1)?)?;
        if at < *cuts.last()? || at > body.len() {
            return None;
        }
        cuts.push(at);
    }
    cuts.push(body.len());
    Some(cuts.windows(2).map(|w| &body[w[0]..w[1]]).collect())
}

/// A heading node as one source line.
fn heading_line(node: &Node) -> String {
    format!("{}\n", render_nodes(std::slice::from_ref(node)).trim_end())
}

/// Appends whatever newlines `out` still needs to end in a blank line, so
/// the next chunk starts a fresh block.
fn push_block_separator(out: &mut String) {
    if out.is_empty() || out.ends_with("\n\n") {
        return;
    }
    out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
}

impl Document {
    pub fn parse(source: &str) -> Result<Self, MutationError> {
        let markdown = source
            .parse::<Markdown>()
            .map_err(|e| MutationError::Parse(e.to_string()))?;
        let baseline = Baseline::new(source, &markdown.nodes);
        let mut origin = Vec::new();
        if baseline.is_some() {
            origin = vec![NO_ORIGIN; markdown.nodes.len()];
            let starts = chunk_starts(&markdown.nodes);
            for (k, range) in chunk_ranges(&starts, markdown.nodes.len()).enumerate() {
                origin[range].fill(k as u32);
            }
        }
        Ok(Self {
            markdown,
            baseline,
            verbatim: FxHashMap::default(),
            origin,
            slug: SlugStyle::Raw,
        })
    }

    /// Sets how [`Self::tree`] names each heading's directory.
    pub fn with_slug(mut self, slug: SlugStyle) -> Self {
        self.slug = slug;
        self
    }

    pub fn tree(&self) -> SectionTree {
        SectionTree::build_with(&self.markdown.nodes, self.slug)
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

    /// Full-document render, persisted back to the source file on every
    /// flush. Each chunk (see [`chunk_starts`]) none of whose nodes changed
    /// since parsing is copied from the original source byte for byte (found
    /// via `origin`, without rendering it); an edited one is written as the
    /// user wrote it (`verbatim`), and only chunks with neither — new
    /// headings, front matter — go through mq-markdown's normalizing
    /// renderer. So an unedited document renders to exactly its source, and
    /// an edit costs O(changed chunks) of rendering.
    pub fn render(&self) -> String {
        let Some(base) = &self.baseline else {
            return self.markdown.to_string();
        };
        let nodes = &self.markdown.nodes;
        let starts = chunk_starts(nodes);
        let mut out = String::new();
        // The original chunk emitted immediately before (reset by an edited
        // chunk).
        let mut prev: Option<usize> = None;
        for range in chunk_ranges(&starts, nodes.len()) {
            let hit = match self.untouched(range.clone()) {
                Some(k) => Some(k),
                None => {
                    let rendered = render_nodes(&nodes[range]);
                    if let Some(raw) = self.verbatim.get(&rendered) {
                        push_block_separator(&mut out);
                        out.push_str(raw);
                        prev = None;
                        continue;
                    }
                    if !rendered.is_empty() {
                        push_block_separator(&mut out);
                        out.push_str(&rendered);
                        prev = None;
                    }
                    None
                }
            };
            if let Some(k) = hit {
                // Adjacent original chunks already carry the exact gap
                // between them; anything else needs its own separator.
                if prev.is_none_or(|p| p + 1 != k) {
                    push_block_separator(&mut out);
                }
                out.push_str(&base.source[base.chunk_bytes[k].clone()]);
                prev = Some(k);
            }
        }
        // Only the original last chunk knows how the file originally ended;
        // anything else ends in exactly one newline.
        if prev != Some(base.chunk_bytes.len().wrapping_sub(1)) {
            let trimmed = out.trim_end_matches('\n').len();
            out.truncate(trimmed);
            if !out.is_empty() {
                out.push('\n');
            }
        }
        out
    }

    pub fn render_range(&self, range: Range<usize>) -> String {
        render_nodes(&self.markdown.nodes[range])
    }

    /// `section` in full: its heading, its own content, and every
    /// subsection in `section.children` (recursively) — so only the
    /// subsections a `--filter`ed tree still shows. For the root, that's
    /// the preamble plus every section, without the front matter.
    pub fn render_subtree(&self, section: &Section) -> String {
        let mut out = String::new();
        self.push_subtree(section, &mut out);
        out
    }

    fn push_subtree(&self, section: &Section, out: &mut String) {
        let start = section.heading_index.unwrap_or(section.own_content_range.start);
        let range = start..section.own_content_range.end;
        let text = if self.baseline.is_some() {
            self.chunk_text(range)
        } else {
            self.render_range(range)
        };
        let text = text.trim_matches('\n');
        if !text.is_empty() {
            push_block_separator(out);
            out.push_str(text);
            out.push('\n');
        }
        for child in &section.children {
            self.push_subtree(child, out);
        }
    }

    pub fn frontmatter_value(&self, node_index: usize) -> &str {
        match &self.markdown.nodes[node_index] {
            Node::Yaml(y) => &y.value,
            Node::Toml(t) => &t.value,
            _ => "",
        }
    }

    pub fn set_frontmatter(&mut self, node_index: usize, value: String) {
        if let Some(o) = self.origin.get_mut(node_index) {
            *o = NO_ORIGIN;
        }
        match &mut self.markdown.nodes[node_index] {
            Node::Yaml(y) => y.value = value,
            Node::Toml(t) => t.value = value,
            _ => {}
        }
    }

    /// Reparses `body` and splices it into a section's content range. Heading
    /// lines typed into `body` become new subdirectories on the next rebuild.
    ///
    /// `body` is kept as written: the section's chunk is saved with exactly
    /// these bytes (plus its original heading line and spacing), not
    /// mq-markdown's re-rendering of them.
    pub fn replace_section_content(&mut self, range: Range<usize>, body: &str) -> Result<ContentEdit, MutationError> {
        let new_nodes = body
            .parse::<Markdown>()
            .map_err(|e| MutationError::Parse(e.to_string()))?
            .nodes;
        let unchanged = ContentEdit {
            delta: 0,
            structural: false,
        };
        let (chunk_start, has_heading) = self.owner_chunk(range.start);
        let old = self.baseline.is_some().then(|| {
            let text = self.chunk_text(chunk_start..range.end);
            ChunkParts::split(&text, has_heading).map(|p| OldChunk {
                head: p.head.to_string(),
                gap: p.gap.to_string(),
                empty: p.core.is_empty(),
                core_matches: p.core == body.trim_end_matches('\n'),
                blanks: p.blanks,
            })
        });
        let old = old.flatten();
        let semantic_same = render_nodes(&new_nodes) == self.render_range(range.clone());
        // Rewriting a section with what it already says (an editor saving
        // unchanged content) changes nothing, so the original bytes stay.
        match &old {
            Some(o) if o.core_matches => return Ok(unchanged),
            None if semantic_same => return Ok(unchanged),
            _ => {}
        }

        let heading_at: Vec<usize> = new_nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n, Node::Heading(_)))
            .map(|(i, _)| i)
            .collect();
        let pieces = old.as_ref().and_then(|_| split_body_pieces(body, &new_nodes));
        let old_key = old
            .as_ref()
            .map(|_| render_nodes(&self.markdown.nodes[chunk_start..range.end]));
        let new_len = if semantic_same { range.len() } else { new_nodes.len() };
        let delta = new_len as isize - range.len() as isize;
        if !semantic_same {
            self.splice_with_reflow(range.clone(), new_nodes);
        }

        if let (Some(old), Some(pieces), Some(old_key)) = (old, pieces, old_key) {
            self.verbatim.remove(&old_key);
            let trailing = body.len() - body.trim_end_matches('\n').len();
            let last_blanks = if trailing <= 1 { old.blanks } else { trailing - 1 };
            let gap = if old.empty && has_heading {
                "\n"
            } else {
                old.gap.as_str()
            };
            let mut chunks: Vec<(Range<usize>, String)> = Vec::with_capacity(pieces.len());
            for (i, piece) in pieces.iter().enumerate() {
                let (from, head, gap) = match i {
                    0 => (chunk_start, old.head.as_str(), gap),
                    _ => (range.start + heading_at[i - 1], "", ""),
                };
                let to = range.start + heading_at.get(i).copied().unwrap_or(new_len);
                let core = piece.trim_end_matches('\n');
                let blanks = if i + 1 == pieces.len() {
                    last_blanks
                } else {
                    (piece.len() - core.len()).saturating_sub(1)
                };
                chunks.push((from..to, ChunkParts::assemble(head, gap, core, blanks)));
            }
            for (nodes, raw) in chunks {
                let key = render_nodes(&self.markdown.nodes[nodes]);
                self.set_raw(key, raw);
            }
        }
        Ok(ContentEdit {
            delta,
            structural: !heading_at.is_empty(),
        })
    }

    /// Start of the chunk that owns the content range starting at `start`,
    /// and whether that chunk begins with a heading.
    fn owner_chunk(&self, start: usize) -> (usize, bool) {
        match start.checked_sub(1).and_then(|i| self.markdown.nodes.get(i)) {
            Some(Node::Heading(_)) => (start - 1, true),
            _ => (start, false),
        }
    }

    /// The original chunk `range` still is, byte for byte, if none of its
    /// nodes was created or changed since parsing.
    fn untouched(&self, range: Range<usize>) -> Option<usize> {
        let base = self.baseline.as_ref()?;
        let &k = self.origin.get(range.start)?;
        (k != NO_ORIGIN && base.chunk_lens[k as usize] == range.len() && self.origin[range].iter().all(|&o| o == k))
            .then_some(k as usize)
    }

    /// The text this document would save for the chunk at `range`: what the
    /// user wrote, else the original bytes, else mq-markdown's rendering.
    fn chunk_text(&self, range: Range<usize>) -> String {
        if let (Some(k), Some(base)) = (self.untouched(range.clone()), &self.baseline) {
            return base.source[base.chunk_bytes[k].clone()].to_string();
        }
        let rendered = render_nodes(&self.markdown.nodes[range.clone()]);
        if let Some(raw) = self.verbatim.get(&rendered) {
            return raw.clone();
        }
        rendered
    }

    /// Remembers `raw` as the text of the chunk rendering as `key`.
    fn set_raw(&mut self, key: String, raw: String) {
        self.verbatim.insert(key, raw);
    }

    /// A section's body as it is written in the source, for `content.md`:
    /// the same bytes [`Self::replace_section_content`] takes back.
    pub fn body_text(&self, range: Range<usize>) -> String {
        if self.baseline.is_none() || range.is_empty() {
            return self.render_range(range);
        }
        let (chunk_start, has_heading) = self.owner_chunk(range.start);
        let text = self.chunk_text(chunk_start..range.end);
        match ChunkParts::split(&text, has_heading) {
            Some(p) if p.core.is_empty() => String::new(),
            Some(p) => format!("{}\n", p.core),
            None => self.render_range(range),
        }
    }

    pub fn insert_heading(&mut self, parent: &Section, name: &str) -> Result<(), MutationError> {
        if parent.children.iter().any(|c| c.name == name) {
            return Err(MutationError::AlreadyExists(name.to_string()));
        }
        let heading = Node::Heading(Heading {
            depth: HeadingDepth::saturating(i64::from(parent.depth) + 1),
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

    /// Splices `new_nodes` into `range`, giving them line positions that
    /// follow the node before `range` — mq-markdown infers list/table/
    /// paragraph grouping from position gaps, so those must make sense.
    /// Nodes after `range` are left alone: they start with a heading, which
    /// always begins a fresh block, and every chunk is rendered on its own,
    /// so nothing compares their lines to the new nodes'. That keeps an edit
    /// O(edited section), not O(document).
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

        // Without a baseline `render` falls back to rendering the whole node
        // list in one go, which does compare lines across chunks.
        if self.baseline.is_none() {
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
                Self::shift_top_level_positions(
                    &mut self.markdown.nodes[range.end..],
                    (anchor + 2) as isize - next_start as isize,
                );
            }
        }

        if !self.origin.is_empty() {
            self.origin
                .splice(range.clone(), std::iter::repeat_n(NO_ORIGIN, new_nodes.len()));
        }
        self.markdown.nodes.splice(range, new_nodes).collect()
    }

    fn normalize_missing_positions(nodes: &mut [Node]) {
        let mut prev_end_line: Option<u32> = None;
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
        let delta = i32::try_from(delta).unwrap_or(if delta < 0 { i32::MIN } else { i32::MAX });
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
        let end = section.own_content_range.end;
        // Only the heading line changes; keep the body's own formatting.
        let before = self
            .baseline
            .is_some()
            .then(|| (render_nodes(&self.markdown.nodes[idx..end]), self.chunk_text(idx..end)));
        if let Some(o) = self.origin.get_mut(idx) {
            *o = NO_ORIGIN;
        }
        if let Node::Heading(h) = &mut self.markdown.nodes[idx] {
            h.values = vec![Node::Text(Text {
                value: new_name.to_string(),
                position: None,
            })];
        }
        if let Some((old_key, old_text)) = before
            && let Some((_, rest)) = split_heading(&old_text)
        {
            self.verbatim.remove(&old_key);
            let key = render_nodes(&self.markdown.nodes[idx..end]);
            let raw = format!("{}{rest}", heading_line(&self.markdown.nodes[idx]));
            self.set_raw(key, raw);
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

        // Each moved chunk keeps its body text as written; only its heading
        // line (new depth, maybe a new name) is re-rendered.
        let saved_rests: Option<Vec<String>> = self.baseline.is_some().then(|| {
            let heads: Vec<usize> = (start..end)
                .filter(|&i| matches!(self.markdown.nodes[i], Node::Heading(_)))
                .collect();
            heads
                .iter()
                .enumerate()
                .map(|(k, &h)| {
                    let to = heads.get(k + 1).copied().unwrap_or(end);
                    let text = self.chunk_text(h..to);
                    split_heading(&text).map_or_else(String::new, |(_, rest)| rest.to_string())
                })
                .collect()
        });
        let mut moved = self.splice_with_reflow(start..end, Vec::new());
        for node in &mut moved {
            if let Node::Heading(h) = node {
                h.depth = HeadingDepth::saturating(i64::from(h.depth) + i64::from(depth_shift));
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
        if let Some(rests) = saved_rests {
            let moved_end = insert_at + removed_len;
            let heads: Vec<usize> = (insert_at..moved_end)
                .filter(|&i| matches!(self.markdown.nodes[i], Node::Heading(_)))
                .collect();
            for (k, (&h, rest)) in heads.iter().zip(&rests).enumerate() {
                let to = heads.get(k + 1).copied().unwrap_or(moved_end);
                let key = render_nodes(&self.markdown.nodes[h..to]);
                let raw = format!("{}{rest}", heading_line(&self.markdown.nodes[h]));
                self.set_raw(key, raw);
            }
        }
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

    /// Formatting mq-markdown's renderer would normalize: `*` bullets, a
    /// padless table, a run of blank lines, `+` bullets, and no trailing
    /// newline at the very end.
    const QUIRKY: &str = "---\ntitle:   x\n---\n\n# Title\n\n* one\n* two\n\n|a|b|\n|-|-|\n|1|2|\n\n\n\n## A\nold a\n\n## B\n\n+ b1\n+ b2\n\n## C\n\n1) c\n2) d";

    fn section_range(doc: &Document, path: &[&str]) -> Range<usize> {
        doc.tree().find(path).unwrap().own_content_range.clone()
    }

    #[rstest]
    #[case::grow("a\n\nb\n\nc\n")]
    #[case::shrink("")]
    #[case::same_count("x\n")]
    fn body_edit_shift_matches_a_full_rebuild(#[case] body: &str) {
        let src = "---\nt: 1\n---\n\npre\n\n# A\n\none\n\ntwo\n\n## B\n\n## C\n\nc body\n\n# D\n\nd\n";
        for path in [vec![], vec!["A"], vec!["A", "B"], vec!["A", "C"], vec!["D"]] {
            let mut doc = Document::parse(src).unwrap();
            let mut tree = doc.tree();
            let range = tree.find(&path).unwrap().own_content_range.clone();
            let edit = doc.replace_section_content(range.clone(), body).unwrap();
            assert!(!edit.structural);
            tree.shift_after_edit(range.start, range.end, edit.delta);
            assert_eq!(tree, doc.tree(), "editing {path:?} with {body:?}");
        }
    }

    #[test]
    #[ignore]
    fn bench_document_phases() {
        use std::time::Instant;
        let mut src = String::new();
        for i in 0..800 {
            src.push_str(&format!("# Chapter {i}\n\nIntro.\n\n"));
            for j in 0..50 {
                src.push_str(&format!("## Section {i}.{j}\n\nSome *body* text {j} with a [link](http://example.com).\n\n* item one\n* item two\n\n```rust\nfn f() {{}}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n"));
            }
        }
        eprintln!("{:.1} MB", src.len() as f64 / 1e6);
        let t = Instant::now();
        let md = src.parse::<Markdown>().unwrap();
        eprintln!("mq-markdown parse : {:?} ({} nodes)", t.elapsed(), md.nodes.len());
        let t = Instant::now();
        let _ = Baseline::new(&src, &md.nodes);
        eprintln!("Baseline::new     : {:?}", t.elapsed());
        let t = Instant::now();
        let _ = SectionTree::build_with(&md.nodes, SlugStyle::Raw);
        eprintln!("tree build        : {:?}", t.elapsed());
        let doc = Document::parse(&src).unwrap();
        let t = Instant::now();
        let _ = doc.render();
        eprintln!("render (unedited) : {:?}", t.elapsed());
        let t = Instant::now();
        let _ = md.to_string();
        eprintln!("markdown.to_string: {:?}", t.elapsed());
    }

    #[test]
    fn render_of_an_unedited_document_is_its_exact_source() {
        let doc = Document::parse(QUIRKY).unwrap();
        assert_eq!(doc.render(), QUIRKY);

        for src in [
            "",
            "\n\n# Leading blank lines\n",
            "plain text, no headings",
            "Setext\n======\n\nbody\n",
        ] {
            assert_eq!(Document::parse(src).unwrap().render(), src);
        }
    }

    #[test]
    fn editing_one_section_leaves_every_other_section_byte_identical() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        let range = section_range(&doc, &["Title", "B"]);
        doc.replace_section_content(range, "new b\n").unwrap();

        let out = doc.render();
        let expected = QUIRKY.replace("+ b1\n+ b2\n\n", "new b\n\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn editing_the_last_section_keeps_everything_before_it() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        let range = section_range(&doc, &["Title", "C"]);
        doc.replace_section_content(range, "changed\n").unwrap();

        let out = doc.render();
        let (before, _) = QUIRKY.split_once("## C").unwrap();
        assert_eq!(out, format!("{before}## C\n\nchanged\n"));
    }

    #[test]
    fn rewriting_a_section_with_its_own_content_is_a_byte_exact_no_op() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        for path in [&["Title"][..], &["Title", "A"], &["Title", "B"], &["Title", "C"]] {
            let range = section_range(&doc, path);
            let body = doc.body_text(range.clone());
            doc.replace_section_content(range, &body).unwrap();
        }
        assert_eq!(doc.render(), QUIRKY);
    }

    #[test]
    fn truncating_then_restoring_a_section_lands_back_on_the_original_bytes() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        let range = section_range(&doc, &["Title"]);
        let body = doc.body_text(range.clone());
        doc.replace_section_content(range, "").unwrap();
        assert_ne!(doc.render(), QUIRKY);

        let range = section_range(&doc, &["Title"]);
        doc.replace_section_content(range, &body).unwrap();
        assert_eq!(doc.render(), QUIRKY);
    }

    #[test]
    fn body_text_is_the_source_bytes_not_a_normalized_rendering() {
        let doc = Document::parse(QUIRKY).unwrap();
        assert_eq!(
            doc.body_text(section_range(&doc, &["Title"])),
            "* one\n* two\n\n|a|b|\n|-|-|\n|1|2|\n"
        );
        assert_eq!(doc.body_text(section_range(&doc, &["Title", "A"])), "old a\n");
        assert_eq!(doc.body_text(section_range(&doc, &["Title", "B"])), "+ b1\n+ b2\n");
        assert_eq!(doc.body_text(section_range(&doc, &["Title", "C"])), "1) c\n2) d\n");
    }

    #[test]
    fn an_edited_section_keeps_the_formatting_it_was_written_with() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        let range = section_range(&doc, &["Title"]);
        let body = doc.body_text(range.clone()).replace("* two", "* two!");
        doc.replace_section_content(range, &body).unwrap();
        // The bullets and the unpadded table survive; so do the three blank
        // lines after the section and everything else in the file.
        assert_eq!(doc.render(), QUIRKY.replace("* two", "* two!"));
        // ...and the edit reads back as written.
        assert_eq!(doc.body_text(section_range(&doc, &["Title"])), body);
    }

    #[test]
    fn an_edit_that_only_changes_markup_style_is_saved() {
        let mut doc = Document::parse("# T\n\n* a\n\n# U\n").unwrap();
        let range = section_range(&doc, &["T"]);
        // Renders the same as `* a`, but it is not the same text.
        let edit = doc.replace_section_content(range, "- a\n").unwrap();
        assert_eq!(edit.delta, 0);
        assert_eq!(doc.render(), "# T\n\n- a\n\n# U\n");
    }

    #[test]
    fn the_gap_after_the_heading_and_the_blank_lines_after_the_body_survive_an_edit() {
        let mut doc = Document::parse("# T\ntight\n\n\n# U\n\nloose\n").unwrap();
        let t = section_range(&doc, &["T"]);
        doc.replace_section_content(t, "tight!\n").unwrap();
        let u = section_range(&doc, &["U"]);
        doc.replace_section_content(u, "loose!\n").unwrap();
        assert_eq!(doc.render(), "# T\ntight!\n\n\n# U\n\nloose!\n");
    }

    #[test]
    fn writing_a_body_into_an_empty_section_puts_a_blank_line_after_the_heading() {
        let mut doc = Document::parse("# T\n\n# U\n").unwrap();
        let t = section_range(&doc, &["T"]);
        doc.replace_section_content(t, "* x\n").unwrap();
        assert_eq!(doc.render(), "# T\n\n* x\n\n# U\n");
    }

    #[test]
    fn new_headings_typed_into_a_body_keep_each_pieces_formatting() {
        let mut doc = Document::parse("# T\n\nold\n\n# U\n\n* u\n").unwrap();
        let t = section_range(&doc, &["T"]);
        doc.replace_section_content(t, "+ intro\n\n## Sub\n\n* sub\n").unwrap();
        assert_eq!(doc.render(), "# T\n\n+ intro\n\n## Sub\n\n* sub\n\n# U\n\n* u\n");
        let tree = doc.tree();
        let sub = tree.find(&["T", "Sub"]).unwrap();
        assert_eq!(doc.body_text(sub.own_content_range.clone()), "* sub\n");
    }

    #[test]
    fn renaming_a_heading_keeps_its_body_formatting() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        let tree = doc.tree();
        doc.rename_heading(tree.find(&["Title", "B"]).unwrap(), "Bee").unwrap();
        assert_eq!(doc.render(), QUIRKY.replace("## B", "## Bee"));
        let tree = doc.tree();
        doc.rename_heading(tree.find(&["Title", "Bee"]).unwrap(), "B").unwrap();
        assert_eq!(doc.render(), QUIRKY);
    }

    #[test]
    fn moving_a_heading_keeps_the_formatting_of_every_moved_body() {
        let src = "# P\n\n# Q\n\n* q\n\n## R\n\n+ r\n";
        let mut doc = Document::parse(src).unwrap();
        let tree = doc.tree();
        let (q, p) = (tree.find(&["Q"]).unwrap(), tree.find(&["P"]).unwrap());
        doc.move_heading(q, p, "Q").unwrap();
        assert_eq!(doc.render(), "# P\n\n## Q\n\n* q\n\n### R\n\n+ r\n");
    }

    #[test]
    fn full_view_of_a_subtree_uses_the_source_formatting() {
        let doc = Document::parse("# T\n\n* a\n\n## S\n\n+ b\n").unwrap();
        let tree = doc.tree();
        assert_eq!(
            doc.render_subtree(tree.find(&["T"]).unwrap()),
            "# T\n\n* a\n\n## S\n\n+ b\n"
        );
    }

    #[test]
    fn structural_edits_leave_unrelated_sections_byte_identical() {
        let title_chunk = "# Title\n\n* one\n* two\n\n|a|b|\n|-|-|\n|1|2|\n\n\n\n";
        let b_chunk = "## B\n\n+ b1\n+ b2\n\n";

        // Insert a new heading at the end of Title.
        let mut doc = Document::parse(QUIRKY).unwrap();
        let tree = doc.tree();
        doc.insert_heading(tree.find(&["Title"]).unwrap(), "New").unwrap();
        let out = doc.render();
        assert!(out.contains(title_chunk), "{out}");
        assert!(out.contains(b_chunk), "{out}");
        assert!(out.contains("|1|2|\n\n\n\n## New\n\n## A\nold a\n"), "{out}");

        // Rename B.
        let mut doc = Document::parse(QUIRKY).unwrap();
        let tree = doc.tree();
        doc.rename_heading(tree.find(&["Title", "B"]).unwrap(), "Bee").unwrap();
        let out = doc.render();
        assert!(out.contains(title_chunk), "{out}");
        assert!(out.contains("## Bee\n\n+ b1\n+ b2\n"), "{out}");
        assert!(out.ends_with("## C\n\n1) c\n2) d"), "{out}");

        // Delete C (empty it first: rmdir is POSIX-strict).
        let mut doc = Document::parse(QUIRKY).unwrap();
        let range = section_range(&doc, &["Title", "C"]);
        doc.replace_section_content(range, "").unwrap();
        let tree = doc.tree();
        doc.remove_heading(tree.find(&["Title", "C"]).unwrap()).unwrap();
        let out = doc.render();
        assert_eq!(out, QUIRKY.split_once("## C").unwrap().0.trim_end().to_string() + "\n");

        // Move A under B: A's depth changes (re-rendered), the rest is kept.
        let mut doc = Document::parse(QUIRKY).unwrap();
        let tree = doc.tree();
        let a = tree.find(&["Title", "A"]).unwrap();
        let b = tree.find(&["Title", "B"]).unwrap();
        doc.move_heading(a, b, "A").unwrap();
        let out = doc.render();
        assert!(out.contains(title_chunk), "{out}");
        assert!(out.contains("### A\n"), "{out}");
        assert!(out.ends_with("## C\n\n1) c\n2) d"), "{out}");
        assert!(
            Document::parse(&out)
                .unwrap()
                .tree()
                .find(&["Title", "B", "A"])
                .is_some()
        );
    }

    #[test]
    fn editing_front_matter_keeps_the_body_byte_identical() {
        let mut doc = Document::parse(QUIRKY).unwrap();
        doc.set_frontmatter(0, "title: y".to_string());
        let out = doc.render();
        let (_, body) = QUIRKY.split_once("---\n\n").unwrap();
        assert!(out.starts_with("---\ntitle: y\n---\n"), "{out}");
        assert!(out.ends_with(body), "{out}");
    }

    #[test]
    fn render_subtree_includes_the_heading_and_every_subsection() {
        let doc = Document::parse("intro\n\n# A\n\na\n\n## B\n\nb\n\n### C\n\nc\n\n# D\n\nd\n").unwrap();
        let tree = doc.tree();
        assert_eq!(
            doc.render_subtree(tree.find(&["A"]).unwrap()),
            "# A\n\na\n\n## B\n\nb\n\n### C\n\nc\n"
        );
        assert_eq!(doc.render_subtree(tree.find(&["A", "B", "C"]).unwrap()), "### C\n\nc\n");
        assert_eq!(
            doc.render_subtree(&tree.root),
            "intro\n\n# A\n\na\n\n## B\n\nb\n\n### C\n\nc\n\n# D\n\nd\n"
        );
    }

    #[test]
    fn render_subtree_of_a_filtered_tree_skips_hidden_subsections() {
        let doc = Document::parse("# A\n\na\n\n## Keep\n\nk\n\n## Drop\n\nd\n").unwrap();
        let filter = |n: &Node| n.value() != "Drop";
        let tree = doc.tree_filtered(&filter);
        assert_eq!(
            doc.render_subtree(tree.find(&["A"]).unwrap()),
            "# A\n\na\n\n## Keep\n\nk\n"
        );
    }

    #[rstest]
    #[case::raw(SlugStyle::Raw, "Getting Started!", "Getting Started!")]
    #[case::kebab_basic(SlugStyle::Kebab, "Getting Started!", "getting-started")]
    #[case::kebab_punctuation(SlugStyle::Kebab, "  API: v1.2 / `foo()` -- notes ", "api-v1.2-foo-notes")]
    #[case::kebab_non_ascii(SlugStyle::Kebab, "日本語 の 見出し", "日本語-の-見出し")]
    #[case::kebab_snake(SlugStyle::Kebab, "my_var", "my_var")]
    #[case::kebab_empty(SlugStyle::Kebab, "!!!", "untitled")]
    #[case::kebab_dot(SlugStyle::Kebab, ".", "untitled")]
    fn slug_styles(#[case] style: SlugStyle, #[case] title: &str, #[case] expected: &str) {
        assert_eq!(style.apply(title), expected);
        assert_eq!(style.apply(expected), expected, "must be a fixed point");
    }

    #[test]
    fn kebab_slugs_disambiguate_headings_that_collapse_to_the_same_name() {
        let doc = Document::parse("# Foo Bar\n\n# foo-bar\n\n# FOO BAR!\n")
            .unwrap()
            .with_slug(SlugStyle::Kebab);
        let names: Vec<String> = doc.tree().root.children.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec!["foo-bar", "foo-bar-2", "foo-bar-3"]);
    }

    #[test]
    fn duplicate_looking_sections_each_keep_their_own_bytes() {
        let src = "# X\n\n* same\n\n# Y\n\n# X\n\n- same\n";
        let mut doc = Document::parse(src).unwrap();
        let range = section_range(&doc, &["Y"]);
        doc.replace_section_content(range, "y\n").unwrap();
        assert_eq!(doc.render(), "# X\n\n* same\n\n# Y\n\ny\n\n# X\n\n- same\n");
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
