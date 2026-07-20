# bokai

A format-agnostic ebook processing engine (EPUB, KFX, AZW3, MOBI, PDF) with CJK
typography and Kindle-format depth. Self-contained: it knows nothing about
whatever application embeds it.

The name is a contraction of **boko** + **改** (*kai*, "revised/altered") — a
diverged fork of [boko](https://github.com/zacharydenton/boko). The crate, the
library, and the CLI binary are all called `bokai`.

Forked from upstream commit `a3df89622afa2e413a9b70f71a4822c8262ee22f` (master
HEAD as of 2026-05-17). To diff against upstream:

```sh
git clone https://github.com/zacharydenton/boko.git /tmp/boko-upstream
diff -r /tmp/boko-upstream/src bokai/src
```
