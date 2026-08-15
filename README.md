<div align="center">
  <img src="assets/logo.svg" style="width: 128px; height: 128px;"/>

<h1>mq-mount</h1>
</div>

> [!WARNING]
> This project is under active development. Interfaces, behavior, and file layout may change without notice, and things may break.

NFS-mount one or more Markdown files as a virtual filesystem: each file gets a top-level directory named after it, headings become subdirectories, and each section's body becomes a `content.md` file. Browse and edit a document with `ls`, `cat`, `grep`, `mkdir`, `rm`, and any regular text editor; writes are parsed back into the original Markdown via [mq-markdown](https://github.com/harehare/mq).

Companion tool for [mq](https://github.com/harehare/mq), a jq-like CLI for Markdown.

## How it maps

```
a.md, b.md               ->  /a/...  and  /b/...   (one top-level dir per mounted file,
                                                      named after the file with its extension
                                                      stripped; duplicate stems get -2, -3, ...)

# Title (inside a.md)     ->  /a/content.md          (a.md's own preamble, if any)
                              /a/Title/content.md    (Title's own body)
## Sub A                  ->  /a/Title/Sub-A/content.md
## Sub A                  ->  /a/Title/Sub-A-2/content.md  (duplicate titles get -2, -3, ...)
---
front matter
---                       ->  /a/_frontmatter.yaml (or _frontmatter.toml)
```

A section's `content.md` holds only its own body: text up to the *next* heading of any depth, not its subsections' content. Nesting comes from heading depth and document order, not from any indentation convention; a `#` typed inside a deeply-nested section's `content.md` becomes a new top-level directory *within that file* on save, not a nested one.

The top-level per-file directories are fixed at mount time, one per file passed on the command line: `mkdir`/`rmdir`/`rename` at that level (or moving `content.md` between two different mounted files) are not supported (`EPERM`/`ENOENT`/`EOPNOTSUPP`).

## Read/write semantics

- Editing `content.md` and saving splices the new text back into the source file. Typing a new heading line into it creates a new subdirectory on the next `ls`.
- `mkdir NAME` under a directory adds a new (empty) subheading. Fails with `EEXIST` if a sibling already has that name.
- `rmdir` is POSIX-strict: it only removes an already-empty directory (no subdirectories, empty `content.md`). Plain `rm -r somedir` still deletes a whole section and everything nested inside it, since the shell already unlinks/rmdirs bottom-up.
- Renaming a directory renames the heading's title. Moving a directory to a *different* parent (reparenting) is not supported (`EOPNOTSUPP`) in this version. The top-level, per-file directories can't be created, removed, or renamed either (`EPERM`/`ENOENT`); the set of mounted files is fixed for the life of the mount.
- Editors that save via a temp-file-then-rename dance (common with vim's `backupcopy=auto`, VS Code, and other "atomic save" tools) are supported: renaming any file onto a canonical `content.md`/frontmatter path adopts its bytes as that section's new content.

## Use cases

- **Token-efficient reading for LLM agents.** An agent working against a large Markdown doc (a spec, a design doc, a long README) doesn't need to load the whole file into context: `grep -r` the mount to find the relevant heading, then `cat` just that section's `content.md`. `ls` at each level doubles as a table of contents, so the agent can also walk down to the right section without ever reading unrelated ones.
- **Section-scoped edits.** An agent (or a script) can rewrite one section's `content.md` in isolation — no need to fetch the whole document, locate the section by string/regex matching, splice in new text, and write the whole thing back; `mq-mount` does that splice on save.
- **Ad-hoc exploration with standard tools.** `find`, `grep`, `fzf`, and any text editor work against the mount as-is, which is useful for skimming or searching through a document's structure without a Markdown-aware parser on hand.

## Known limitations

- **Not byte-exact.** Every save re-renders the *whole* document through mq-markdown. Mounting a file and saving without any edits can still normalize whitespace, blank-line counts, list markers, and table padding; mq-markdown's renderer doesn't guarantee a byte-identical round trip. mq-mount skips the rewrite when the render is unchanged from what it last wrote, to avoid *spurious* rewrites, but a first save after mount may differ from the original bytes even with no logical edit.
- **No external change detection.** If the source file is edited by something else while mounted, that change is not detected or merged; whichever write lands last (through the mount, or externally) wins.
- **Linux and macOS only**, no Windows.
- Cross-directory `mv` (reparenting a heading) is not implemented.

## Installation

No system packages are required; mounting uses the OS's built-in NFS client.

```sh
git clone https://github.com/harehare/mq-mount
cd mq-mount
cargo build --release
```

The `mount` feature is enabled by default. Building without it (`cargo build --no-default-features`) still compiles and tests the core section-tree logic, but produces a binary that refuses to run.

## Usage

```sh
mkdir /tmp/doc-mount
mq-mount README.md CHANGELOG.md /tmp/doc-mount

ls /tmp/doc-mount
cat /tmp/doc-mount/README/Installation/content.md
echo "more text" >> /tmp/doc-mount/README/Installation/content.md
mkdir /tmp/doc-mount/README/"New Section"

# Unmount (Ctrl-C in the mq-mount process also does this):
diskutil unmount /tmp/doc-mount # macOS
umount /tmp/doc-mount           # Linux
```

### Options

```
Usage: mq-mount [OPTIONS] <PATHS>...

Arguments:
  <PATHS>...  Markdown files to mount, followed by the mount directory as the
              last argument (e.g. `a.md b.md /mnt`)

Options:
      --readonly      Mount read-only; all writes are rejected
      --allow-other    Allow other users on the machine to access the mount
  -v, --verbose        Enable verbose (debug) logging
  -h, --help           Print help
  -V, --version        Print version
```

## Development

```sh
# Core logic tests
cargo test --no-default-features

# Full build
cargo build
cargo clippy
```

## License

Licensed under the MIT License.
