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

## Use cases

- **Token-efficient reading for LLM agents.** `grep -r` the mount to find a heading, then `cat` just that section instead of loading the whole doc.
- **Section-scoped edits.** Rewrite one section's `content.md`; `mq-mount` splices it back into the document on save.
- **Ad-hoc exploration.** `find`, `grep`, `fzf`, and any editor work as-is.

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/harehare/mq-mount/main/bin/install.sh | bash
# or
cargo binstall mq-mount
```

Other options: download a binary from the [releases page](https://github.com/harehare/mq-mount/releases), or build from source with `cargo build --release`.

Platform requirements:

- **macOS**: none; uses the built-in NFS client.
- **Linux**: needs the `mount.nfs` helper (`nfs-common` on Debian/Ubuntu, `nfs-utils` on Fedora/Arch) and **root** (run with `sudo`).
- **Windows**: install [WinFSP](https://winfsp.dev). This backend is unverified on a real Windows machine.

## Usage

```sh
# Mount files and/or directories (mount dir is created if missing).
# Read-only by default; add --write to let edits reach the source files.
mq-mount README.md CHANGELOG.md /tmp/doc-mount --write
mq-mount docs/ /tmp/doc-mount --write     # every .md under docs/, layout mirrored

ls /tmp/doc-mount
cat /tmp/doc-mount/README/Installation/content.md
echo "more text" >> /tmp/doc-mount/README/Installation/content.md
mkdir /tmp/doc-mount/README/"New Section"

# Unmount (or Ctrl-C in the mq-mount process)
diskutil unmount /tmp/doc-mount   # macOS
umount /tmp/doc-mount             # Linux

# Useful flags
mq-mount docs/ /tmp/doc-mount --watch                    # follow added/removed files
mq-mount docs/ /tmp/doc-mount --include "guide/**/*.md" --exclude "**/draft-*.md"
mq-mount README.md /tmp/doc-mount --toc --full           # _toc.md and _full.md
mq-mount README.md /tmp/doc-mount --slug kebab           # `## Getting Started` -> getting-started/
mq-mount README.md /tmp/doc-mount --write --backup       # keep <file>.orig before first write
mq-mount README.md /tmp/doc-mount --filter '.h2'         # only matching sections; read-only

# Background mounts
mq-mount docs/ /tmp/doc-mount -d
mq-mount --list [--json]
mq-mount --stop /tmp/doc-mount
```

Run `mq-mount --help` for the full list of options.

### Config file

Options you always use can live in `.mq-mount.toml` (looked up in the current directory, then the first source's directory; or `--config <path>`, `--no-config` to skip). Keys mirror the long flags and command-line flags override the file. Unknown keys are an error.

```toml
write = true
toc = true
slug = "kebab"            # raw | kebab
on-conflict = "keep-both" # keep-both | reject
exclude = ["**/draft-*.md"]
```

## How it maps

```
a.md, b.md               ->  /a/...  and  /b/...      (one dir per file, extension stripped;
                                                       duplicate stems get -2, -3, ...)
docs/ (guide/a.md, ...)  ->  /docs/guide/a/...         (a directory mirrors its own layout,
                                                       skipping dotfiles)

# Title (in a.md)        ->  /a/content.md             (preamble, if any)
                             /a/Title/content.md       (Title's own body)
## Sub A                 ->  /a/Title/Sub A/content.md (duplicates get -2, -3, ...)
front matter             ->  /a/_frontmatter.yaml (or .toml)
```

- `content.md` holds only a section's own body — text up to the *next* heading of any depth.
- `--toc` adds a read-only `_toc.md` per file (and one at the mount root) listing the heading tree.
- `--full` adds a read-only `_full.md` per section: its heading, body, and all subsections.
- `--slug kebab` makes directory names lowercase kebab-case (non-ASCII letters are kept); `mkdir`/`rename` then only accept names already in that form.
- The per-file layout is fixed at mount time: `mkdir`/`rmdir`/`rename` at the top level, or moving sections between mounted files, aren't supported.

## Read/write semantics

- Mounts are read-only unless `--write` is given. `--filter` is always read-only.
- Saves update the in-memory document immediately and are flushed to disk within ~150ms (or on unmount/Ctrl-C).
- `mkdir NAME` adds an empty subheading (`EEXIST` if a sibling has that name).
- `rmdir` only removes an empty directory; `rm -r` deletes a whole section.
- Renaming a directory renames the heading. Moving it within the same file reparents the subtree and adjusts depth; moves past depth 6 or into its own subtree fail with `EINVAL`.
- Atomic-save editors (vim, VS Code, etc.) work.

## Known limitations

- **Edits are saved as written.** Only changed chunks (front matter, preamble, or a heading plus its body) are rewritten; everything else is copied byte for byte. `mkdir` and front matter edits go through mq-mount's renderer and may be normalized. Style-only edits count as changes. Use `--backup` to keep the original.
- **External changes are reloaded, not merged.** With a pending local edit, `--on-conflict keep-both` (default) saves the mount's version as `<file>.conflict-<unix time>`, loads the disk version, and fails the operation with `ESTALE`. `reject` refuses writes until you remount.
- **A hard kill can lose up to ~150ms of edits.** Ctrl-C, SIGTERM, and `--stop` always flush first.
- **External unmounts are noticed within ~2s** (macOS/Linux). On Windows, `--stop` terminates the process forcibly.
- **The Windows (WinFSP) backend is unverified.**

## Development

```sh
cargo test --no-default-features   # core logic tests
cargo build && cargo clippy        # full build

# End-to-end tests against a real mount (Linux needs nfs-common and root):
sudo -E env "PATH=$PATH" MQ_MOUNT_E2E=1 cargo test --test e2e -- --test-threads=1
```

## License

Licensed under the MIT License.
