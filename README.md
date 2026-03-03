# TurboFind

Fast file indexer and search for Windows. Indexes 780K+ files in ~12s, then fuzzy-searches them in ~10ms.

Built with Rust using rayon for parallel crawling, nucleo for fuzzy matching (same engine as Helix editor), and crossterm for the TUI.

## Benchmarks

Tested on 786,882 files (RTX 3060 / Ryzen system):

| Operation | Time |
|---|---|
| First index | ~12s (65K files/sec) |
| Cache reload | <1s |
| Fuzzy search (avg) | ~10ms |

Measured over 100 iterations per query using release build. Run `Ctrl+B` inside the TUI to benchmark on your machine.

## Build

```bash
git clone https://github.com/fulong97/turbofind.git
cd turbofind
cargo build --release
```

Binary ends up at `target/release/turbofind.exe`.

## Install

```bash
cargo install --path .
```

This copies `turbofind` to `~/.cargo/bin/` which is already in your PATH. Run `turbofind` from anywhere.

Or grab the pre-built `.exe` from [Releases](https://github.com/fulong97/turbofind/releases).

## Usage

```bash
# Index default paths
turbofind

# Index specific directories
turbofind C:\Projects D:\Documents
```

### Search filters

| Query | What it does |
|---|---|
| `budget` | Fuzzy match all files |
| `ext:rs config` | Only .rs files matching "config" |
| `ext:pdf invoice` | Only PDFs matching "invoice" |
| `dir:` | Only directories |

### Keys

| Key | Action |
|---|---|
| `Up/Down` | Navigate results |
| `Enter` | Open file |
| `Ctrl+O` | Open containing folder |
| `Ctrl+B` | Run benchmark |
| `Esc` | Quit |

## How it works

First run crawls the filesystem in parallel using all CPU cores and builds an in-memory index. The index gets serialized to a binary cache file so subsequent launches load in under a second. Search uses the nucleo fuzzy matching algorithm (same engine as Helix editor) with parallel scoring via rayon. Extension filters use a pre-built HashMap for O(1) lookup.

## Dependencies

`rayon` `walkdir` `nucleo` `crossterm` `bincode` `serde` `dirs`

## License

MIT
