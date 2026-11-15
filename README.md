# WorldPruner ![](https://user-images.githubusercontent.com/13237524/168894674-c2b8c07e-a4d6-4eb9-ab9d-b906b78dcb70.png)

A fast, multi-threaded command line app to prune chunks from a Minecraft Java Edition world, with an interactive
terminal interface and an optional desktop GUI.

This is a Rust port of [Querz/WorldPruner](https://github.com/Querz/WorldPruner). It keeps the original command
line, works with worlds from 1.13 up to 26.x, and is built for large server worlds.

> Make sure you have a backup of your world!
>
> We are not liable for data loss!

## What it does

WorldPruner deletes chunks that nobody spent time in, so the world shrinks and the terrain can regenerate with
newer world generation. A chunk is **kept** if

- its `InhabitedTime` (the accumulated time players spent in it) is greater than `--time`, or
- it lies within `--radius` chunks of such a chunk, or
- it is on the whitelist, or
- it belongs to a structure (village, stronghold, ...) that reaches into a kept chunk, so structures are never cut
  in half, or
- it could not be read (only with `--continue-on-error`; otherwise nothing is changed at all).

Everything else is removed from the `region`, `poi` and `entities` files of the dimension, including the
`.mcc` files of oversized chunks.

## Installation

Build it with a [Rust toolchain](https://rustup.rs) (1.89 or newer):

```sh
cargo build --release                          # command line and terminal interface
cargo build --release --features gui           # ... plus the desktop window
cargo build --release --no-default-features    # command line only
```

The binary is `target/release/worldpruner`. No Java is required.

## Usage

Close the world first: stop the server or leave the world in the game. WorldPruner refuses to touch a world whose
`session.lock` is held by another program.

| Argument(s)                 | Default Value | Description                                                                 |
|-----------------------------|---------------|-----------------------------------------------------------------------------|
| `--world` `-w`              | *Required*    | The path to the world folder, or to a dimension folder such as `world/DIM-1`. Optional with `--tui` |
| `--tui` `-i`                |               | Open the [interactive terminal interface](#terminal-interface)              |
| `--time` `-t`               | `0s`          | The minimum time a chunk should have to be kept                             |
| `--radius` `-r`             | `0`           | The radius of additional chunks preserved around matching chunks (max 128)  |
| `--white-list` `-W`         |               | The path to a whitelist CSV file                                            |
| `--white-list-only` `-l`    |               | Prune everything except the whitelist, requires `--white-list` to be set    |
| `--dimension` `-D`          | `overworld`   | The dimension to prune: `the_nether`, `the_end`, `mymod:mine`, or `all`. Can be repeated |
| `--continue-on-error` `-c`  |               | Continue if a file or chunk cannot be processed; unreadable chunks are kept |
| `--dry-run` `-n`            |               | Only report what would be pruned, without changing anything                 |
| `--threads` `-j`            | all cores     | The number of worker threads                                                |
| `--force`                   |               | Prune even if the world appears to be in use (this can corrupt the world)   |
| `--log-file`                |               | Write a log to this file                                                    |
| `--debug` `-d`              |               | Enables debug logging                                                       |
| `--help` `-h`               |               | Prints all available command line options                                   |

Durations are written like `90s`, `5 minutes`, `1h30m` or `1 day 12 hours`.

**Examples:**

*See what would happen when pruning all chunks that are not older than 1 minute*
```sh
worldpruner --world "/path/to/my/world/" --time "1 minute" --dry-run
```

*Prune all chunks that are not older than 5 minutes, keep a radius of two chunks and continue if an error occurs*
```sh
worldpruner --world "/path/to/my/world/" --time "5 minutes" --radius 2 --continue-on-error
```

*Prune the Nether and the End of a world*
```sh
worldpruner --world "/path/to/my/world/" --time "10 minutes" --dimension the_nether --dimension the_end
```

*Prune all chunks that are not older than 1 day, keep a radius of ten chunks and keep all chunks in the `whitelist.csv`*
```sh
worldpruner --world "/path/to/my/world/" --time "1 day" --radius 10 --white-list "/path/to/whitelist.csv"
```

The exit code is `0` on success, `1` on errors and `130` if the run was cancelled with Ctrl-C.

### Terminal interface

`worldpruner --tui` (or just `worldpruner` without arguments in a terminal) opens an interactive interface that
only needs a terminal, so it works over SSH on a headless server:

```
┌ WorldPruner 2.0.0 ───────────────────────────────────────────────────────────────┐
│ > World:         /srv/minecraft/world/                                           │
│   Dimensions:    the_nether, the_end                                             │
│   InhabitedTime: 5 minutes                                                       │
│   Radius:        2                                                               │
│   Whitelist:     (none)                                                          │
│   [ ] Prune everything except the whitelist                                      │
│   [x] Continue on errors                                                         │
│   Threads:       0 (all cores)                                                   │
│                                                                                  │
│   [ Preview (dry run) ]  [ Prune ]  [ Quit ]                                     │
└──────────────────────────────────────────────────────────────────────────────────┘
 Up/Down move  Tab complete  Enter select  F2 preview  F10 prune  Esc quit
```

- Fill in the form; invalid values are flagged as you type and `Tab` completes paths like a shell.
- **Preview** (`F2`) runs a dry run and shows what would be removed. Nothing is changed.
- **Prune** (`F10`, or `p` after a preview) asks for confirmation, then shows live progress. `Esc` cancels safely.
- Options given on the command line fill in the form, e.g. `worldpruner --tui -w /srv/minecraft/world -t 1h`.

### GUI Mode

When built with `--features gui`, starting `worldpruner` without arguments on a desktop opens a window with the
same options as the original Swing GUI: open your world folder, provide a minimum time, a radius and an
optional [whitelist](#whitelisting-chunks) containing all chunks that should be kept in any case. Without a
display (for example over SSH) the terminal interface opens instead.

## Supported worlds

| | |
|---|---|
| Folder layout | Classic (`region/`, `DIM-1/`, `DIM1/`) and the layout introduced in 26.1 (`dimensions/<namespace>/<name>/`). `--world` takes the world folder in both cases; a single dimension folder works too. |
| Chunk format | 1.18 and newer including 26.x (`InhabitedTime`, `structures`) and 1.13 - 1.17 (`Level.InhabitedTime`, `Level.Structures`). |
| Compression | zlib, gzip, uncompressed and LZ4 (`region-file-compression=lz4`), plus oversized chunks in `.mcc` files. Chunks written with a modded custom compression cannot be read and count as errors. |

The 26.x format was checked against the 26.3 game files. Kept chunks are copied byte for byte, so chunk data
that this tool does not understand is never altered.

## Whitelisting Chunks

You can create and export a whitelist from [MCA Selector](https://github.com/Querz/mcaselector).
More information on how to create a whitelist can be found [here](https://github.com/Querz/mcaselector/wiki/Selections#save-and-load-selections).

If you want to create your own whitelist read [this](https://github.com/Querz/mcaselector/wiki/Selections#selection-file-format).

With `--dimension` given several times (or `all`), the same whitelist is applied to every dimension.

## Safety

- **Nothing is modified until the whole world has been scanned successfully.** If a chunk cannot be read, the run
  stops before changing anything, unless `--continue-on-error` is set, in which case that chunk is kept.
- **Files are replaced atomically.** A region file is rewritten into a temporary file next to it, flushed to disk
  and renamed over the original. Killing the program or losing power at any moment leaves every file either in
  its old or its new state. Leftover temporary files are cleaned up by the next run.
- **Kept chunks are copied byte for byte**, together with their timestamps.
- **The world is locked** through `session.lock` while pruning, the same way the game locks it, so a server
  cannot open it halfway through.
- Ctrl-C finishes the files in progress and stops; the world stays loadable.
- The code contains no `unsafe`, and malformed files produce errors instead of crashes.

## Performance

The scan and the rewrite run in parallel over all region files. Chunks are decompressed as a stream and only as
far as necessary: reading usually stops after the first few kilobytes of a chunk. Memory use stays small
because chunks are read one at a time (about 45 MiB with 32 threads in the test below).

Measured on a synthetic 945 MiB world (256 region files, 166,464 chunks, made by duplicating real region files),
removing 94% of the chunks, on a 32-thread desktop:

| | Time |
|---|---|
| Original Java version 1.1.0 | 8.3 s |
| This version, 1 thread | 2.7 s |
| This version, 32 threads | 0.25 s |

The Java run used Windows/NTFS and the Rust runs Linux/ext4 on the same machine, so treat this as a rough guide.
Both left exactly the same set of chunks.

## Differences from the Java version

The result is the same chunk selection as the original, with these deliberate exceptions:

- The margin of one chunk around structure pieces is now always complete. The original missed the outermost
  column or row for some piece positions, so this version can keep a few more chunks next to structures.
- `--white-list-only` really ignores `InhabitedTime`: only structures touching whitelisted chunks are kept.
- Inverted whitelists are handled correctly (the original ignored their chunk entries and could drop inhabited
  chunks in excluded regions).
- Region files that have nothing to remove are left untouched instead of being rewritten.
- Oversized chunk files (`.mcc`) of removed chunks are deleted too.
- Read errors are reported per chunk; with `--continue-on-error` only the unreadable chunk is kept instead of the
  whole region file.
- No log file is written unless `--log-file` is given.

## Development

```sh
cargo test                                  # unit and end-to-end tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The library part of the crate (`worldpruner::prune`) can be used from other Rust programs.
