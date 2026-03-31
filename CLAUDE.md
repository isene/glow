# Glow

Rust feature clone of [termpix](https://github.com/isene/termpix), a terminal image display library.

Supports kitty graphics protocol, sixel, and w3m image display. Used by Scroll, Pointer, and Kastrup for inline images.

## Build

```bash
PATH="/usr/bin:$PATH" cargo build --release
```

Note: `PATH` prefix needed to avoid `~/bin/cc` (Claude Code sessions) shadowing the C compiler.
