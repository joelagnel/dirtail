# dirtail

> **⚠️ WARNING:** This project was quickly generated with AI assistance. It may contain bugs or
> unexpected behavior. Use at your own risk and review the code before deploying in any
> critical environment.

`dirtail` is a command-line tool that tails all files matching a glob pattern in a directory,
with live watching for new files and color-coded output per file.

## Rationale

When you have a directory full of log files (e.g., per-service or per-worker logs), you often
want to watch all of them simultaneously — similar to `tail -f`, but across many files at once.
`dirtail` fills this gap: it tails existing files, watches for new ones as they appear, and
handles log rotation (rename/remove events).

Each file gets a distinct color prefix so you can tell at a glance which file a line came from.

## Features

- Tail all files matching a glob pattern in a directory
- Automatically pick up new files as they are created
- Handles log rotation (rename/remove — re-tails the path when a new file appears)
- Color-coded filename prefix per file for easy visual scanning
- Supports both positional and named flag arguments

## Installation

```sh
cargo install --path .
```

Or build manually:

```sh
cargo build --release
cp target/release/dirtail ~/.local/bin/
```

## Usage

```sh
# Tail all files in the current directory
dirtail

# Tail all .log files in /var/log
dirtail /var/log '*.log'

# Using named flags
dirtail --dir /var/log --pattern '*.log'
```

### Arguments

| Argument            | Description                                      | Default |
|---------------------|--------------------------------------------------|---------|
| `DIR`               | Directory to watch                               | `.`     |
| `PATTERN`           | Glob pattern to match files                      | `*`     |
| `--dir <DIR>`       | Directory to watch (named flag)                  | `.`     |
| `--pattern <PAT>`   | Glob pattern to match files (named flag)         | `*`     |

## Architecture

- One **inotify watcher thread** monitors the directory for new/renamed/removed files.
- One **tail-poll thread per file** reads new lines and sends them to the printer.
- One **print-loop thread** receives lines from all tail threads and outputs them with color.
- A `FileRegistry` (HashMap) prevents double-tailing the same file and handles rotation.
- A small ring buffer (last 10 lines) is used for initial tail output — avoids reading entire files into memory.

## License

GPL-2.0
