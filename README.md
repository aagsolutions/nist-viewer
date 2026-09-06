# NIST Biometric Viewer

A desktop viewer for **ANSI/NIST-ITL** biometric transaction files (`.an2`, `.nst`, `.eft`, `.nist`), written in Rust with an [eframe](https://github.com/emilk/egui/tree/master/crates/eframe)/egui UI.

It parses the traditional (SEMI-separated) encoding, lists all logical records in the file, shows record metadata in field tables, and decodes the embedded images — including a pure-Rust, pixel-exact **WSQ** fingerprint decoder.

![Type](https://img.shields.io/badge/format-ANSI%2FNIST--ITL-blue) ![Lang](https://img.shields.io/badge/lang-Rust-orange)

## Features

- **Full file parsing** of ANSI/NIST-ITL traditional encoding:
  - Type-1 transaction record, Type-2 user-defined text, Type-9 minutiae, Type-10 facial/SMT, Type-13–15 fingerprint/palm, Type-17 iris, and legacy binary Type-3/4/5/6 records
  - Text records with the binary `999` image field extracted by declared record length (separator bytes inside image payloads are handled correctly)
  - Character sets: ASCII/Latin-1 default, UTF-8, UTF-16 (per the Type-1 `DCS` field)
  - Lenient recovery: parsing errors in later records don't discard the whole file
- **Image decoding**:
  - **WSQ** (FBI Wavelet Scalar Quantization 3.1) — complete in-tree decoder
  - JPEG / PNG / and other formats via the [`image`](https://crates.io/crates/image) crate
  - Uncompressed raw 8-bit grayscale (legacy Type-3/4/5/6 records with GCA = NONE), using the parsed `HLL`×`VLL` dimensions
  - Compression detection by payload magic bytes (`FF A0` WSQ, `FF D8` JPEG, PNG signature) — robust against misleading 1-byte `GCA` codes in legacy binary records
- **GUI**:
  - Record browser (type + IDC) in a side panel
  - Field table view for every record
  - Async (background-thread) image decoding so the UI stays responsive
  - Zoom in/out/fit, scrollable image panes
  - Export the decoded image as PNG (full resolution)
  - Keyboard shortcuts: `⌘/Ctrl+O` open, `⌘/Ctrl+S` save image

## Building & Running

Requires a recent stable Rust toolchain.

```bash
cargo run --release
```

Then use **File ▸ Open…** and pick an `.an2`/`.nst` file. Sample reference files are bundled in [`test_data/`](test_data/).

## Testing

```bash
cargo test
```

The test suite includes golden-value verification of the WSQ decoder: `test_data/sample.wsq` must decode **pixel-identically** to `test_data/sample.png` (the reference output produced by the NBIS-compatible [jnbis](https://github.com/mhshams/jnbis) Java decoder). Additional tests parse and decode the bundled Type-3, Type-4, Type-10 and Type-14 reference files.

There is also a small debugging helper that dumps every decodable image of a file to `/tmp`:

```bash
cargo run --example debug_decode -- test_data/type-14-amp-nqm-utf8.an2
```

## License

The WSQ decode algorithm is derived from the public-domain NIST NBIS reference implementation.
