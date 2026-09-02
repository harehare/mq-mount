<div align="center">
  <img src="assets/logo.svg" width="128" height="128" alt="mq-mount logo"/>

<h1>mq-mount</h1>

[![CI](https://github.com/harehare/mq-mount/actions/workflows/ci.yml/badge.svg)](https://github.com/harehare/mq-mount/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mq-mount.svg)](https://crates.io/crates/mq-mount)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
</div>

> [!WARNING]
> This project is under active development. Interfaces, behavior, and file layout may change without notice, and things may break.

Mount Markdown files as a virtual filesystem: headings become directories, section bodies become `content.md` files. Browse and edit with `ls`, `cat`, `grep`, `mkdir`, `rm`, or any text editor — writes are parsed back into the original Markdown via [mq-markdown](https://github.com/harehare/mq).

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

- With `--toc`, every mounted file also gets a read-only `/a/_toc.md`: a linked Markdown list of its whole heading tree (e.g. `- [Sub A](Title/Sub A/content.md)`). Lets an agent see the structure in one read instead of walking every directory with `ls`. The mount root also gets a read-only `/_toc.md` aggregating every mounted file's heading tree in one place, under a `## <file>` heading each.
- A section's `content.md` holds only its own body — text up to the *next* heading of any depth, not its subsections'.
- Nesting follows heading depth and document order, not indentation. Typing `#` inside a deeply-nested section's `content.md` creates a new top-level directory *within that file* on save, not a nested one.
- The top-level, per-file/per-directory layout is fixed at mount time: `mkdir`/`rmdir`/`rename` there, or moving `content.md` between two mounted files, aren't supported (`EPERM`/`ENOENT`/`EOPNOTSUPP`).

## Read/write semantics

- Mounts are read-only by default; pass `--write` to allow edits to reach the source file(s). `--filter` always mounts read-only, even with `--write` (a warning is logged if both are given).
- Saving `content.md` updates the in-memory document immediately (reads see it right away, e.g. a new heading becomes a subdirectory on the next `ls`). The write to disk is batched — flushed within ~150ms, or before unmount/Ctrl-C.
- `_toc.md` (per file and at the mount root) is always computed fresh, never stale.
- `mkdir NAME` adds an empty subheading. Fails with `EEXIST` if a sibling already has that name.
- `rmdir` is POSIX-strict: only an already-empty directory (no subdirectories, empty `content.md`) can be removed. `rm -r` still deletes a whole section, since the shell unlinks bottom-up.
- Renaming a directory renames the heading. Moving it to a different parent in the same mounted file reparents the heading and its subtree, adjusting heading depth to fit.
  - Rejected (`EINVAL`) if that would nest past depth 6, or move a directory into its own subtree.
  - Reparenting across two mounted files, and moving/renaming the top-level per-file directories, aren't supported (`EOPNOTSUPP`/`EPERM`/`ENOENT`) — the set of mounted files is fixed for the mount's life.
- Atomic-save editors (vim `backupcopy=auto`, VS Code, etc.) work: renaming a file onto a canonical `content.md`/frontmatter path adopts its bytes as the new content.

## Use cases

- **Token-efficient reading for LLM agents.** Don't load a whole doc into context — `grep -r` the mount to find a heading, then `cat` just that section's `content.md`. `ls` at each level doubles as a table of contents.
- **Section-scoped edits.** Rewrite one section's `content.md` in isolation; `mq-mount` splices it back into the full document on save.
- **Ad-hoc exploration.** `find`, `grep`, `fzf`, and any text editor work against the mount as-is — no Markdown-aware parser needed.

## Known limitations

- **Not byte-exact.** Every flush re-renders the whole document, which can normalize whitespace, blank lines, list markers, and table padding. mq-mount skips the rewrite when the render is unchanged, but the first save after mount may still differ from the original bytes even with no logical edit. Pass `--backup` to snapshot each source file's pre-edit bytes to a sibling `<file>.orig` before its first write.
- **External changes are auto-reloaded, not merged.** An external edit is picked up on the next tick (~150ms) if there's no pending local edit. If there is, the mount refuses to overwrite it (`ESTALE`/`STATUS_FILE_LOCK_CONFLICT`) instead of silently discarding either version — unmount and remount to reconcile by hand.
- **A deferred write can be lost on a hard kill.** Writes are batched rather than flushed on every `write()`; a crash, `kill -9`, or forced termination can lose up to ~150ms of unflushed edits. A graceful stop (Ctrl-C, SIGTERM, `--stop`) always flushes first.
- **External unmounts are noticed within a couple of seconds, not instantly.** macOS/Linux poll every 2s. On Windows, `--stop` forcibly terminates the process (no graceful cross-process signal); WinFSP still unmounts cleanly when its host process dies.
- **The Windows (WinFSP) backend is unverified.** Built against WinFSP's documented API, but not yet run on a real Windows machine — see [Installation](#installation).

## Installation

**macOS/Linux**: no system packages required; mounting uses the OS's built-in NFS client.

**Windows**: install [WinFSP](https://winfsp.dev) first — Windows' own built-in NFS client is Pro/Enterprise/Server-only and doesn't support the custom ports this tool needs. Backend unverified on a real Windows machine; see [Known limitations](#known-limitations).

### Install script

```sh
curl -fsSL https://raw.githubusercontent.com/harehare/mq-mount/main/bin/install.sh | bash
```

Installs the latest release for your OS/architecture into `~/.local/bin`, verified against the release's checksums. `--bin-dir <dir>` to install elsewhere, `--no-modify-path` to skip touching your shell profile; see `--help` for details.

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

`mount` is a default feature. Without it (`cargo build --no-default-features`), the core section-tree logic still builds and tests, but the binary refuses to run.

## Usage

```sh
mkdir /tmp/doc-mount
# Mounts are read-only by default; pass --write to edit the source file(s).
mq-mount README.md CHANGELOG.md /tmp/doc-mount --write
# or mount every .md file under a directory tree, structure mirrored:
mq-mount docs/ /tmp/doc-mount --write

ls /tmp/doc-mount
cat /tmp/doc-mount/README/Installation/content.md
echo "more text" >> /tmp/doc-mount/README/Installation/content.md
mkdir /tmp/doc-mount/README/"New Section"

# Unmount (Ctrl-C in the mq-mount process also does this):
diskutil unmount /tmp/doc-mount # macOS
umount /tmp/doc-mount           # Linux
# Windows: Ctrl-C in the mq-mount process; WinFSP unmounts automatically.

# Auto-mount/unmount .md files added, deleted, or renamed under docs/, without a restart:
mq-mount docs/ /tmp/doc-mount --watch

# Only mount files matching a glob (relative to the directory argument), skipping drafts:
mq-mount docs/ /tmp/doc-mount --include "guide/**/*.md" --exclude "**/draft-*.md"

# Add a read-only _toc.md per file listing its whole heading tree:
mq-mount docs/ /tmp/doc-mount --toc
cat /tmp/doc-mount/docs/guide/a/_toc.md

# Run detached from the terminal (like `docker-compose up -d`), and stop it later:
mq-mount docs/ /tmp/doc-mount -d
mq-mount --stop /tmp/doc-mount
# A background mount also exits on its own if the volume is unmounted some
# other way (Finder/Explorer eject, diskutil/umount) — no orphaned process.

# List every currently running background mount (like `docker ps`):
mq-mount --list

# Snapshot each source file's pre-edit bytes to <file>.orig before its first write:
mq-mount README.md /tmp/doc-mount --write --backup

# Only expose sections whose heading matches an mq query; always read-only:
mq-mount README.md /tmp/doc-mount --filter '.h2'
ls /tmp/doc-mount/README   # only depth-2 headings (and their ancestors) appear
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
      --write              Allow writes to the source Markdown file(s)
                            (default: read-only)
      --allow-other        Loosen file permission bits so other local users
                            can read/write the mount (the underlying NFS
                            server has no per-caller ACL to restrict access
                            to the mounting user; no effect on Windows)
      --watch              Auto-mount new .md files added under a mounted
                            directory; also unmounts a file deleted or
                            renamed away outside the mount
      --toc                Expose a read-only `_toc.md` at each mounted
                            file's root, listing its whole heading tree with
                            links to each section's content.md
      --include <GLOB>     Only mount .md files under a directory argument
                            whose path (relative to that argument) matches
                            this glob; repeatable
      --exclude <GLOB>     Skip .md files under a directory argument whose
                            path (relative to that argument) matches this
                            glob; repeatable, applied after --include
      --filter <QUERY>     Only expose sections whose heading matches this
                            mq query (e.g. `.h1`, `select(contains("TODO"))`);
                            ancestors of a match stay visible so it remains
                            reachable by path. Always mounts read-only,
                            regardless of --write
      --backup             Before the first write to each source file, save
                            its pre-edit bytes to a sibling `<file>.orig`
                            (skipped if one already exists)
  -d, --background         Run detached from the terminal; the child keeps
                            running once this process exits
      --stop <MOUNTPOINT>  Stop a running mount (background or foreground)
                            at this mountpoint and exit
      --list               List currently running background mounts and exit
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
