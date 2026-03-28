# treader

A terminal-based PDF and EPUB viewer that renders documents using the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Written in Rust for speed and sharp rendering at any zoom level.

It runs in a terminal that supports the Kitty graphics protocol:
- [Ghostty](https://ghostty.org/)
- [Kitty](https://sw.kovidgoyal.net/kitty/)
- [WezTerm](https://wezterm.net/)
- [cmux](https://github.com/manaflow-ai/cmux)

## Installation

```
cargo install treader
```

Or `brew install wensheng/treader/treader` if you are using macOS and homebrew.

If you are on Linux, make sure `clang` and `libfontconfig` are installed.

```
sudo apt install clang libfontconfig1-dev
```

## Usage

```
treader [options] <file.pdf|epub>
```

### Options

| Flag | Description |
|------|-------------|
| `-i`, `--invert` | Start with inverted colours |
| `-b`, `--black-color <color>` | Custom black colour (CSS colour string) |
| `-w`, `--white-color <color>` | Custom white colour (CSS colour string) |
| `-h`, `--help` | Show usage and exit |
| `--version` | Print version and exit |

### Examples

```
treader document.pdf
treader --invert paper.pdf
treader -b '#1a1a2e' -w '#e0e0e0' book.epub
```

## Features

- **PDF and EPUB support** via MuPDF
- **Sharp zoom** — pages re-render at higher resolution, not just pixel scaling
- **Vertical scrolling** within pages, with automatic page turn at boundaries
- **Auto-crop** — detects content bounds and removes whitespace margins
- **Colour tinting** — warm sepia tone for comfortable reading
- **Search** with highlighted results across all pages
- **Table of contents** navigation
- **Internal and external link following**
- **State persistence** — remembers page, rotation, crop, tint, and invert
  settings per file (stored in XDG cache)
- **Shared memory transfer** — detects SHM support for faster image display
  on Kitty and Ghostty
- **Clean exit** — always cleans up Kitty graphics on quit, Ctrl-C, or panic

## Keyboard Shortcuts

### Navigation

| Key | Action |
|-----|--------|
| `Space` | Next page |
| `PageDown` | Next page (or move down by one viewport in zoom mode) |
| `PageUp` | Previous page (or move up by one viewport in zoom mode) |
| `j`, `Down` | Scroll down / next page at boundary |
| `k`, `Up` | Scroll up / previous page at boundary |
| `Right` | Next page |
| `Left` | Previous page |
| `l` | Next page (or pan right in zoom mode) |
| `h` | Previous page (or pan left in zoom mode) |
| `g` + number + `Enter` | Go to page |
| `t` | Table of contents |
| `/` + text + `Enter` | Search |
| `n` / `N` | Next / previous search result |
| `q`, `Esc` | Quit |
| `Ctrl-C` | Quit |

### Display

| Key | Action |
|-----|--------|
| `r` | Rotate 90° clockwise |
| `i` | Invert colours |
| `d` | Toggle warm tint (sepia) |
| `c` | Toggle auto-crop (remove whitespace margins) |
| `R`, `F5` | Refresh |

### Zoom

| Key | Action |
|-----|--------|
| `z` | Toggle zoom mode |
| `o` | Zoom in (1.2x per step) |
| `O` | Zoom out |
| `h` / `l` | Pan horizontally (in zoom mode) |
| `Up` / `Down` | Scroll vertically (in zoom mode) |

Zoom re-renders pages at higher resolution through MuPDF, so text stays
sharp at any zoom level.

### Document Info

| Key | Action |
|-----|--------|
| `M` | Show document metadata |
| `f` | Show links on current page |
| `?` | Help |

When viewing links, type a number and press `Enter` to follow. Internal
links jump to the target page. External URLs open in your system browser.

## License

This work is released under the AGPL-3.0 license.

## Acknowledgments

This project is derived from [tdf](https://github.com/itsjunetime/tdf)
