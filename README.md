<div align="center">
  <img src="assets/logo.svg" style="width: 128px; height: 128px;"/>

<h1>mq-mount</h1>
</div>

> [!WARNING]
> This project is under active development. Interfaces, behavior, and file layout may change without notice, and things may break.

Mount one or more Markdown files (or directories of them) as a virtual filesystem: each file gets a top-level directory named after it, headings become subdirectories, and each section's body becomes a `content.md` file. Browse and edit a document with `ls`, `cat`, `grep`, `mkdir`, `rm`, and any regular text editor; writes are parsed back into the original Markdown via [mq-markdown](https://github.com/harehare/mq). Mounting uses the OS's built-in NFS client on macOS/Linux, and [WinFSP](https://winfsp.dev) on Windows (see [Installation](#installation)).

Companion tool for [mq](https://github.com/harehare/mq), a jq-like CLI for Markdown.

<img src="assets/demo.gif" alt="mq-mount demo: mounting a Markdown file in the background, then ls/cat-ing and editing a section" />

## How it maps

```
a.md, b.md               ->  /a/...  and  /b/...   (one top-level dir per mounted file,
                                                      named after the file with its extension
                                                      stripped; duplicate stems get -2, -3, ...)

docs/ (a directory        ->  /docs/guide/a/...  and  /docs/api/b/...
  guide/a.md,                 (a directory argument mirrors its own layout under its own
  api/b.md)                    name: every .md file found under it, recursively, skipping
                                dotfiles/dot-directories such as .git)

# Title (inside a.md)     ->  /a/content.md          (a.md's own preamble, if any)
                              /a/Title/content.md    (Title's own body)
## Sub A                  ->  /a/Title/Sub-A/content.md
## Sub A                  ->  /a/Title/Sub-A-2/content.md  (duplicate titles get -2, -3, ...)
---
front matter
---                       ->  /a/_frontmatter.yaml (or _frontmatter.toml)
```

A section's `content.md` holds only its own body: text up to the *next* heading of any depth, not its subsections' content. Nesting comes from heading depth and document order, not from any indentation convention; a `#` typed inside a deeply-nested section's `content.md` becomes a new top-level directory *within that file* on save, not a nested one.

The directories mirroring the command-line arguments (the top-level per-file directories, and any intermediate directories contributed by a directory argument) are fixed at mount time: `mkdir`/`rmdir`/`rename` at that level (or moving `content.md` between two different mounted files) are not supported (`EPERM`/`ENOENT`/`EOPNOTSUPP`).

## Read/write semantics

- Editing `content.md` and saving splices the new text back into the source file. Typing a new heading line into it creates a new subdirectory on the next `ls`.
- `mkdir NAME` under a directory adds a new (empty) subheading. Fails with `EEXIST` if a sibling already has that name.
- `rmdir` is POSIX-strict: it only removes an already-empty directory (no subdirectories, empty `content.md`). Plain `rm -r somedir` still deletes a whole section and everything nested inside it, since the shell already unlinks/rmdirs bottom-up.
- Renaming a directory renames the heading's title. Moving a directory to a *different* parent within the same mounted file reparents the heading (and everything nested under it) there, renumbering its heading depth — and its descendants' — to fit; it's rejected (`EINVAL`) if that would nest a heading past level 6, the deepest Markdown headings go, or if the destination is inside the directory's own subtree. Reparenting across two different mounted files, and moving/renaming the top-level, per-file directories themselves, are still not supported (`EOPNOTSUPP`/`EPERM`/`ENOENT`); the set of mounted files is fixed for the life of the mount.
- Editors that save via a temp-file-then-rename dance (common with vim's `backupcopy=auto`, VS Code, and other "atomic save" tools) are supported: renaming any file onto a canonical `content.md`/frontmatter path adopts its bytes as that section's new content.

## Use cases

- **Token-efficient reading for LLM agents.** An agent working against a large Markdown doc (a spec, a design doc, a long README) doesn't need to load the whole file into context: `grep -r` the mount to find the relevant heading, then `cat` just that section's `content.md`. `ls` at each level doubles as a table of contents, so the agent can also walk down to the right section without ever reading unrelated ones.
- **Section-scoped edits.** An agent (or a script) can rewrite one section's `content.md` in isolation — no need to fetch the whole document, locate the section by string/regex matching, splice in new text, and write the whole thing back; `mq-mount` does that splice on save.
- **Ad-hoc exploration with standard tools.** `find`, `grep`, `fzf`, and any text editor work against the mount as-is, which is useful for skimming or searching through a document's structure without a Markdown-aware parser on hand.

## Known limitations

- **Not byte-exact.** Every save re-renders the *whole* document through mq-markdown. Mounting a file and saving without any edits can still normalize whitespace, blank-line counts, list markers, and table padding; mq-markdown's renderer doesn't guarantee a byte-identical round trip. mq-mount skips the rewrite when the render is unchanged from what it last wrote, to avoid *spurious* rewrites, but a first save after mount may differ from the original bytes even with no logical edit.
- **External changes are detected but not merged.** If a source file's mtime advances outside the mount while mounted, the next write-through refuses instead of overwriting it (`ESTALE`/`STATUS_FILE_LOCK_CONFLICT`) — there's no diff/merge, so no working set of changes ever gets silently discarded, but the mount also can't reconcile the two versions for you. Unmount and remount to pick up the external edit and re-apply your change on top of it.
- **`--watch` only picks up additions.** A `.md` file added under a mounted directory argument appears in the mount without a restart, but deleting or renaming an already-mounted source file outside the mount is still unnoticed, same as without `--watch`.
- **Background mounts notice an external unmount within a couple of seconds, not instantly.** macOS/Linux poll for that every 2s. On Windows, `--stop` has no graceful cross-process signal to send, so it forcibly terminates the process; WinFSP's driver still unmounts cleanly when its hosting process dies, same as the README already notes for Ctrl-C.
- **The Windows (WinFSP) backend is unverified.** It's written against WinFSP's documented API and an example filesystem from its own repository, but hasn't yet been built or run on an actual Windows machine — see [Installation](#installation).

## Installation

**macOS/Linux**: no system packages are required; mounting uses the OS's built-in NFS client.

**Windows**: install [WinFSP](https://winfsp.dev) first (a separate driver, like macFUSE on macOS — there's no lighter-weight option, since Windows' own built-in NFS client is gated to Pro/Enterprise/Server editions and doesn't support the custom ports this tool needs). This backend hasn't been verified on a real Windows machine yet; see [Known limitations](#known-limitations).

### Install script

```sh
curl -fsSL https://raw.githubusercontent.com/harehare/mq-mount/main/bin/install.sh | bash
```

Downloads the latest release for your OS/architecture into `~/.local/bin`, verifying it against the release's checksums file. Pass `--bin-dir <dir>` to install elsewhere or `--no-modify-path` to skip touching your shell profile; see `--help` for details.

### cargo-binstall

```sh
cargo binstall mq-mount
```

### From a release binary

Download the binary for your platform from the [releases page](https://github.com/harehare/mq-mount/releases) and put it on your `PATH`.

### From source

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
# or mount every .md file under a directory tree, structure mirrored:
mq-mount docs/ /tmp/doc-mount

ls /tmp/doc-mount
cat /tmp/doc-mount/README/Installation/content.md
echo "more text" >> /tmp/doc-mount/README/Installation/content.md
mkdir /tmp/doc-mount/README/"New Section"

# Unmount (Ctrl-C in the mq-mount process also does this):
diskutil unmount /tmp/doc-mount # macOS
umount /tmp/doc-mount           # Linux
# Windows: Ctrl-C in the mq-mount process; WinFSP unmounts automatically.

# Auto-mount new .md files added under docs/, without a restart:
mq-mount docs/ /tmp/doc-mount --watch

# Run detached from the terminal (like `docker-compose up -d`), and stop it later:
mq-mount docs/ /tmp/doc-mount -d
mq-mount --stop /tmp/doc-mount
# A background mount also exits on its own if the volume is unmounted some
# other way (Finder/Explorer eject, diskutil/umount) — no orphaned process.
```

### Options

```
Usage: mq-mount [OPTIONS] [PATHS]...

Arguments:
  [PATHS]...  Markdown files and/or directories to mount, followed by the
              mount directory as the last argument (e.g. `a.md docs/ /mnt`).
              A directory contributes every `.md` file found under it
              (recursively, skipping dotfiles/dot-directories), mirroring
              its own layout under the directory's own name in the mount.
              Omit when using `--stop`

Options:
      --readonly           Mount read-only; all writes are rejected
      --allow-other        Loosen file permission bits so other local users
                            can read/write the mount (the underlying NFS
                            server has no per-caller ACL to restrict access
                            to the mounting user; no effect on Windows)
      --watch              Auto-mount new .md files added under a mounted
                            directory (additions only; see Known
                            limitations)
  -d, --background         Run detached from the terminal; the child keeps
                            running once this process exits
      --stop <MOUNTPOINT>  Stop a running mount (background or foreground)
                            at this mountpoint and exit
  -v, --verbose            Enable verbose (debug) logging
  -h, --help               Print help
  -V, --version            Print version
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
