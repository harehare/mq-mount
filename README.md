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
## Sub A                  ->  /a/Title/Sub A/content.md    (the heading text, as-is: spaces kept,
                                                             only `/` and `\` replaced with `-`)
## Sub A                  ->  /a/Title/Sub A-2/content.md  (duplicate titles get -2, -3, ...)
---
front matter
---                       ->  /a/_frontmatter.yaml (or _frontmatter.toml)
```

- With `--toc`, every mounted file also gets a read-only `/a/_toc.md`: a linked Markdown list of its whole heading tree (e.g. `- [Sub A](<Title/Sub A/content.md>)`). Lets an agent see the structure in one read instead of walking every directory with `ls`. The mount root also gets a read-only `/_toc.md` aggregating every mounted file's heading tree in one place, under a `## <file>` heading each.
- With `--full`, every section directory (and each file's root) also gets a read-only `_full.md`: the section's heading, its own body, and all its subsections, in one read. Under `--filter`, it includes only the subsections the filter leaves visible. A file root's `_full.md` is the whole document minus its front matter.
- With `--slug kebab`, directory names are lowercased kebab-case (`## Getting Started!` -> `getting-started/`), which is easier to type in a shell than the default `--slug raw`. Letters outside ASCII (e.g. Japanese) are kept. `mkdir`/`rename` then only accept names already in that form (anything else fails with `EINVAL`), and the name becomes the heading text as-is.
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

- **Token-efficient reading for LLM agents.** Don't load a whole doc into context — `grep -r` the mount to find a heading, then `cat` just that section's `content.md` (or, with `--full`, its `_full.md` to get the whole chapter including subsections). `ls` at each level, or `--toc`'s `_toc.md`, doubles as a table of contents.
- **Section-scoped edits.** Rewrite one section's `content.md` in isolation; `mq-mount` splices it back into the full document on save.
- **Ad-hoc exploration.** `find`, `grep`, `fzf`, and any text editor work against the mount as-is — no Markdown-aware parser needed.

## Known limitations

- **Edited sections are normalized; everything else is byte-exact.** A flush rewrites only the chunks of the file whose content actually changed (a chunk is the front matter, the preamble before the first heading, or one heading plus its own body). Every untouched chunk is copied from the original file byte for byte, so an unedited file is never rewritten, and saving one section leaves the rest of the file untouched. An edited chunk goes through mq-markdown's renderer, which can normalize its whitespace, list markers, and table padding. Pass `--backup` to snapshot each source file's pre-edit bytes to a sibling `<file>.orig` before its first write.
- **External changes are auto-reloaded, not merged.** An external edit is picked up on the next tick (~150ms) if there's no pending local edit. If there is, nothing is silently discarded: by default (`--on-conflict keep-both`) the mount's version is saved next to the source as `<file>.conflict-<unix time>`, the version on disk is loaded in its place, and the operation that hit the conflict fails with `ESTALE`/`STATUS_FILE_LOCK_CONFLICT`. Merge the two by hand, then keep working — no remount needed. The conflict file deliberately doesn't end in `.md`, so a directory mount never picks it up. `--on-conflict reject` restores the older behavior: keep refusing to write until you unmount and remount.
- **A deferred write can be lost on a hard kill.** Writes are batched rather than flushed on every `write()`; a crash, `kill -9`, or forced termination can lose up to ~150ms of unflushed edits. A graceful stop (Ctrl-C, SIGTERM, `--stop`) always flushes first.
- **External unmounts are noticed within a couple of seconds, not instantly.** macOS/Linux poll every 2s. On Windows, `--stop` forcibly terminates the process (no graceful cross-process signal); WinFSP still unmounts cleanly when its host process dies.
- **The Windows (WinFSP) backend is unverified.** Built against WinFSP's documented API, but not yet run on a real Windows machine — see [Installation](#installation).

## Installation

**macOS**: no system packages required; mounting uses the OS's built-in NFS client.

**Linux**: mounting uses the kernel's NFS client, which needs the `mount.nfs` helper (`nfs-common` on Debian/Ubuntu, `nfs-utils` on Fedora/Arch) and **root**: run `mq-mount` with `sudo`. Files written through the mount are still written by the `mq-mount` process, so they end up owned by root only if they didn't exist before.

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
# The mount directory is created if it doesn't exist yet.
# On Linux, prefix each mounting command with `sudo` (see Installation).
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

# Add a read-only _full.md per section (the section plus all its subsections):
mq-mount README.md /tmp/doc-mount --full
cat /tmp/doc-mount/README/Usage/_full.md

# Shell-friendly kebab-case directory names (`## Getting Started` -> getting-started/):
mq-mount README.md /tmp/doc-mount --slug kebab

# Add a read-only _toc.md per file listing its whole heading tree:
mq-mount docs/ /tmp/doc-mount --toc
cat /tmp/doc-mount/docs/guide/a/_toc.md

# Run detached from the terminal (like `docker-compose up -d`), and stop it later:
mq-mount docs/ /tmp/doc-mount -d
mq-mount --stop /tmp/doc-mount
# A background mount also exits on its own if the volume is unmounted some
# other way (Finder/Explorer eject, diskutil/umount) — no orphaned process.

# List every currently running mount and its sources (like `docker ps`):
mq-mount --list
# ...or as JSON, for scripts and agents:
mq-mount --list --json
# [{"pid":4242,"mountpoint":"/tmp/doc-mount","sources":["/home/me/docs"],"log":"/tmp/mq-mount/….log"}]

# Snapshot each source file's pre-edit bytes to <file>.orig before its first write:
mq-mount README.md /tmp/doc-mount --write --backup

# Only expose sections whose heading matches an mq query; always read-only:
mq-mount README.md /tmp/doc-mount --filter '.h2'
ls /tmp/doc-mount/README   # only depth-2 headings (and their ancestors) appear
```

### Config file

Options you always use can live in a `.mq-mount.toml`, looked up in the current directory, then in the first source's directory (or passed explicitly with `--config <path>`; `--no-config` skips it). Keys mirror the long flags; command-line flags override the file:

```toml
write = true
toc = true
full = true
slug = "kebab"           # raw | kebab
on-conflict = "keep-both" # keep-both | reject
exclude = ["**/draft-*.md"]
# Also: allow-other, watch, backup, filter, include
```

A boolean the file turns on can't be turned off from the command line; use `--no-config` for that. Unknown keys are an error, so typos don't go unnoticed.

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
      --full               Expose a read-only `_full.md` in every section
                            directory: the section's heading, its own
                            content, and all its subsections
      --slug <STYLE>       How a heading's text becomes its directory name:
                            `raw` keeps it as-is (spaces and punctuation
                            included); `kebab` lowercases it, turns
                            whitespace into `-`, and drops other punctuation
                            [default: raw] [possible values: raw, kebab]
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
      --config <PATH>      Read default options from this TOML file instead
                            of looking for `.mq-mount.toml` in the current
                            directory, then in the first source's directory.
                            Command-line flags override it
      --no-config          Don't read any `.mq-mount.toml`
      --on-conflict <POLICY>
                           What to do when a pending edit can't be written
                            because the source file changed on disk in the
                            meantime: `keep-both` saves the mount's version
                            to a sibling `<file>.conflict-<time>` and loads
                            the one on disk; `reject` keeps failing until a
                            remount [default: keep-both] [possible values:
                            keep-both, reject]
  -d, --background         Run detached from the terminal; the child keeps
                            running once this process exits
      --stop <MOUNTPOINT>  Stop a running mount (background or foreground)
                            at this mountpoint and exit
      --list               List currently running mounts (background or
                            foreground) and exit
      --json               With --list, print the mounts as a JSON array of
                            `{pid, mountpoint, sources, log}` objects
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
