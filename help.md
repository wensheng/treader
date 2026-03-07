# Treader Help

## Synopsis

```
treader [options] <file>
```

Open a PDF or EPUB file in the terminal. Requires a terminal that supports
the Kitty graphics protocol (Kitty, Ghostty).

## PDF vs EPUB

Treader handles PDF and EPUB differently:

- **PDF** uses fixed page geometry from the document itself.
- **EPUB** is reflowed by MuPDF into terminal-sized, book-like pages.

Key differences:

- `z`, `o`, `O` are **PDF-oriented zoom controls**. They do not apply to EPUB.
- `<` and `>` are **EPUB-only font-size controls**. They do not apply to PDF.
- EPUB remembers the chosen font size per document.
- Scrolling is still available whenever the rendered page is taller than the
  visible area, regardless of format.

## Options

```
-i, --invert              Start with inverted colours
-b, --black-color <css>   Custom black colour (CSS colour string, e.g. '#1a1a2e')
-w, --white-color <css>   Custom white colour (CSS colour string, e.g. '#e0e0e0')
    --version             Print version and exit
```

Colour values are any CSS colour format accepted by the `csscolorparser`
crate: hex (`#rgb`, `#rrggbb`), `rgb(r, g, b)`, `hsl(h, s%, l%)`, etc.
Named colours (e.g. `red`) are not supported.

## Keyboard Reference

### Quitting

| Key | Action |
|-----|--------|
| `q` | Quit |
| `Esc` | Quit (or close overlay if one is open) |
| `Ctrl-C` | Quit |

### Page Navigation

| Key | Action |
|-----|--------|
| `l`, `Right` | Next page |
| `h`, `Left` | Previous page |
| `j` | Next page (always turns, ignores scroll position) |
| `k` | Previous page (always turns, ignores scroll position) |
| `g` then digits then `Enter` | Go to page number (1-indexed) |
| `t` | Open table of contents |

`j`/`k` always turn the page immediately. `Left`/`Right` also turn pages
in normal mode, but pan horizontally when zoom mode is active.

### Scrolling

| Key | Action |
|-----|--------|
| `Down`, `Space` | Scroll down within page; turn to next page at bottom |
| `Up` | Scroll up within page; turn to previous page at top |

Each scroll step moves by 3 cell rows of pixels. When the page fits
entirely on screen, `Down`/`Up` act as page turns instead.

When scrolling up past the top of a page, the previous page appears
scrolled to its bottom, so continuous upward reading is seamless.

For PDF, scrolling is most useful when zoomed in or when the page is taller
than the terminal. For EPUB, scrolling only matters when the reflowed page is
taller than the visible area.

### Zoom

| Key | Action |
|-----|--------|
| `z` | Toggle zoom mode on/off (**PDF only**) |
| `o` | Zoom in (**PDF only**) |
| `O` | Zoom out (**PDF only**) |
| `Left`/`Right` | Pan horizontally (zoom mode only) |
| `Up`/`Down` | Scroll vertically (zoom mode only) |

Zoom re-renders the page through MuPDF at higher resolution, so text and
vector graphics stay sharp at any zoom level. This is not pixel scaling.

Leaving zoom mode (`z` again) resets zoom level, pan, and scroll to
defaults.

EPUB does not use this zoom model. Reflowable documents use font-size controls
instead:

### EPUB Font Size

| Key | Action |
|-----|--------|
| `<` | Decrease EPUB font size |
| `>` | Increase EPUB font size |

These controls relayout the EPUB and repaginate it. The selected font size is
saved per document and restored on the next open.

### Search

| Key | Action |
|-----|--------|
| `/` then text then `Enter` | Search for text across all pages |
| `n` | Jump to next page with results |
| `N` | Jump to previous page with results |
| `/` then `Enter` (empty) | Clear search and remove highlights |

Search results are highlighted with a yellow-ish tint on matching regions.
The search scans all pages in the background; `n`/`N` navigate between
pages that have at least one match.

### Display Transformations

| Key | Action |
|-----|--------|
| `i` | Toggle colour inversion |
| `r` | Rotate 90 degrees clockwise |
| `d` | Toggle warm tint (sepia reading mode) |
| `c` | Toggle auto-crop (remove whitespace margins) |
| `R`, `F5` | Manual refresh (re-render all pages) |

**Invert** swaps black and white (and everything in between). Useful for
dark-mode reading of light-background documents.

**Rotate** cycles through 0, 90, 180, 270 degrees. The rotation is saved
per document.

**Tint** applies a warm sepia tone (dark brown on light cream) for
comfortable extended reading. The tint colours are `#704214` (black) and
`#F5E6C8` (white).

**Auto-crop** detects the content bounding box on each page and removes
surrounding whitespace margins. The cropped content is then scaled up to
fill the terminal, giving you a larger effective reading area. Especially
useful for academic papers with wide margins.

### Document Information

| Key | Action |
|-----|--------|
| `M` | Show document metadata (title, author, dates, etc.) |
| `f` | Show links on the current page |
| `?` | Show a one-line help summary in the status bar |

### Table of Contents (`t`)

| Key | Action |
|-----|--------|
| `j`, `Down` | Move selection down |
| `k`, `Up` | Move selection up |
| `Enter` | Jump to selected entry's page |
| Left click | Jump to the clicked entry's page |
| `Esc`, `t`, `q` | Close table of contents |

The current page's entry is marked with `>`. Entries are indented to
reflect the document's heading hierarchy.

### Metadata (`M`)

Displays a full-screen overlay with document properties: format, title,
author, subject, keywords, creator application, producer, creation date,
and modification date. Press `Esc`, `M`, or `q` to close.

### Links (`f`)

Shows a numbered list of all hyperlinks on the current page. Type a
number and press `Enter` to follow:

- **Internal links** (cross-references) jump to the target page.
- **External links** (URLs) open in your system browser (`open` on macOS,
  `xdg-open` on Linux).

Press `Esc` to cancel. Use `Backspace` to correct the number.

## Custom Colours

The `-b` and `-w` flags let you set custom black and white points for the
document renderer. MuPDF maps the document's black to your `-b` colour and
white to your `-w` colour, with everything in between interpolated.

Examples:

```
# Dark blue background with light grey text
treader -b '#1a1a2e' -w '#e0e0e0' paper.pdf

# Green on black (terminal hacker aesthetic)
treader -b '#00ff00' -w '#000000' --invert book.epub

# Subtle warm tones without using the built-in sepia
treader -b '#2c1810' -w '#f5e6d0' novel.pdf
```

Note: custom colours and the `-i` (invert) flag interact — invert swaps
the effective black and white, so `--invert -b '#000' -w '#fff'` produces
white-on-black.

## State Persistence

Treader remembers per-document settings across sessions:

- Current page
- Rotation (0/90/180/270)
- Colour inversion on/off
- Auto-crop on/off
- Warm tint on/off
- EPUB font size (EPUB only)

State is saved to `~/.cache/treader/` (or your platform's XDG cache
directory) as JSON files named by an MD5 hash of the document's absolute
path. To reset a document's state, delete its cache file or toggle the
settings back manually.

## Logging

Set the `RUST_LOG` environment variable to enable debug logging:

```
RUST_LOG=debug treader document.pdf
```

Logs are written to `./treader.log` in the current working directory. The
log includes render thread timing, Kitty protocol messages, page
dimensions, and channel activity.

## Terminal Requirements

Treader requires a terminal that supports the **Kitty graphics protocol**:

- [Kitty](https://sw.kovidgoyal.net/kitty/) (full support including SHM)
- [Ghostty](https://ghostty.org/) (full support including SHM)

Other terminals (iTerm2, WezTerm, Alacritty, etc.) do not support the
Kitty graphics protocol. Treader detects this at startup and exits with a
clear error instead of trying to draw image escape sequences into an
unsupported terminal.

Treader auto-detects shared memory (SHM) support at startup. SHM transfer
is faster than base64 encoding, especially for large images at high zoom.
If SHM is not available, it falls back to direct (base64) transfer
transparently.

## Supported Formats

- **PDF** — all versions supported by MuPDF
- **EPUB** — reflowed into terminal-sized pages by MuPDF; supports saved font
  size via `<` / `>`
- **XPS, CBZ, FB2** — also supported via MuPDF (untested)
