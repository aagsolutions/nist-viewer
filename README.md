# NIST Biometric Viewer

A desktop viewer and field editor for **ANSI/NIST-ITL** biometric transaction files (`.an2`, `.nst`, `.eft`, `.nist`), written in Rust with an [eframe](https://github.com/emilk/egui/tree/master/crates/eframe)/egui UI.

It parses traditional encoding, lists all logical records in the file, shows record metadata in field tables, and decodes the embedded images — including a pure-Rust, pixel-exact **WSQ** fingerprint decoder.

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
  - Editable field tables for text records and legacy binary record headers
  - Recalculate every record's byte length (`LEN`) and the transaction contents directory (`CNT`), including updated IDC values
  - Save and Save As for NIST files, with automatic recalculation before saving
  - Replace record images from JPEG, PNG, WSQ, BMP, or TIFF files, with updated dimensions and encoding metadata
  - Resize embedded images with pixel dimensions, an aspect-ratio lock, and a preview
  - Unsaved changes indicator and prompts before opening another file or closing
  - Async (background-thread) image decoding so the UI stays responsive
  - Automatic image previews on selection, retained through metadata edits and saving
  - Zoom in/out/fit, scrollable image panes
  - Export the decoded image as PNG (full resolution)
  - Keyboard shortcuts: `⌘/Ctrl+O` open, `⌘/Ctrl+S` save NIST file, `⌘/Ctrl+Shift+S` save NIST file as

## Building & Running

Requires a recent stable Rust toolchain. The WSQ encoder and decoder are implemented in safe Rust, with no WSQ C library, FFI, or native build step.

```bash
cargo run --release
```

Then use **File ▸ Open…** and pick an `.an2`/`.nst` file. Sample reference files are bundled in [`test_data/`](test_data/).

## Editing & Saving

Select a record and edit its values in the field table. For image records, expand **Record fields (edit)** below the image. Use `\x1e` for the record separator between subfields, `\x1f` for the unit separator between items, and `\\` for a literal backslash.

Click **Recalculate fields** to apply and validate the edits and refresh `LEN` and `CNT`. **File ▸ Save NIST file** and **Save NIST file as…** also recalculate automatically. `LEN` includes the encoded byte length of the entire record, and `CNT` lists each record's type and IDC in file order, as specified by [ANSI/NIST-ITL](https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.500-290e3.pdf). These calculated fields are read-only.

Saving preserves images you have not replaced, uninterpreted binary header bytes, and unchanged encoded field values. Writes use a temporary file in the destination directory before replacing the destination. Invalid edits keep their drafts for correction. Files with incomplete parsing remain viewable but cannot be saved.

The character set declaration (`DCS`) and detected binary compression are read-only. Type-1 values must remain ASCII, and edits in other records must fit the file's declared character set. Image export remains available through **File ▸ Save image as PNG…**.

## Replacing Images

Select an image record and click **Replace image…**, or use **Image ▸ Replace image…**. Choose a JPEG, PNG, WSQ, BMP, or TIFF file. The replacement updates the preview, dimensions (`HLL`/`VLL`), compression (`CGA`/`GCA`), bit depth and color space where applicable, and record lengths. Pending field edits are applied together with the image. Save the NIST file to keep the replacement; importing does not modify the source image file.

- Type-10 and Type-13–17 records retain compatible JPEG/PNG images. Other raster formats, transparency, and required color conversions are normalized to an 8-bit PNG. Fingerprint and palm images are converted to grayscale; transparency is composited onto white.
- Type-3/4 records store raster imports as uncompressed 8-bit grayscale. Imported WSQ files remain WSQ in Type-3/4 and Type-13–16 records.
- Type-5/6 and Type-8 records store uncompressed one-bit images using a grayscale threshold of 128. Signature representation (`SRT`) is updated accordingly. Binary image dimensions must fit the 16-bit header.

Replacement supports Type-3–6, Type-8, Type-10, and Type-13–17 image records. Custom Type-7 payloads and other record layouts are not replaced. Existing scan resolution, capture details, quality scores, annotations, and related minutiae records are retained; review these when using a different image, since they are not inferred or recomputed from its pixels.

## Resizing Images

Select an image record and click **Resize image…**, or choose **Image ▸ Resize image…**. Enter the target width or height in pixels. **Keep aspect ratio** adjusts the other dimension automatically; turn it off to set both independently. The dialog shows the resulting proportions and output format. **Reset dimensions** restores the original size, and **Cancel** leaves the image and pending edits unchanged.

Click **Apply resize** to resample the embedded pixels, refresh the preview, and recalculate image metadata and `LEN`/`CNT`. Save or Save As to write the resized image into the NIST file. Smooth images use Lanczos resampling; one-bit fingerprint and signature images use nearest-neighbor resampling to retain black/white pixels.

**WSQ images remain WSQ after resizing**, in tagged records (`CGA=WSQ20`) and legacy Type-3/4 records (binary `GCA=1`). The [pure Rust WSQ encoder](src/wsq/encoder.rs) retains the source PPI and usable compression bitrate, defaulting to 0.75 bits/pixel when no bitrate is available. It implements wavelet decomposition, scalar quantization, canonical Huffman coding, and WSQ serialization directly in Rust. No external WSQ installation is needed. Re-encoding WSQ is lossy. Supported dimensions are 128–65,535 pixels per axis; unsupported dimensions or encoding errors leave the original image unchanged and never fall back to PNG.

Other resized Type-10/13–17 images are stored as lossless 8-bit PNG. Non-WSQ legacy Type-3/4 images store raw grayscale; Type-5/6/8 records store packed one-bit pixels. Keeping the original dimensions preserves the original encoded image. Scan resolution and image annotations are retained and should be reviewed after resizing.

## Testing

```bash
cargo test
```

The test suite includes golden-value verification of the WSQ decoder: `test_data/sample.wsq` must decode **pixel-identically** to `test_data/sample.png` (the reference output produced by the NBIS-compatible [jnbis](https://github.com/mhshams/jnbis) Java decoder). Additional tests parse and decode the bundled Type-3, Type-4, Type-10 and Type-14 reference files.

Editing tests also verify byte-for-byte round trips of every bundled NIST file, recalculation after metadata edits, UTF-8/UTF-16/Latin-1 encoding, binary header preservation, and rejection of invalid edits or incomplete input.

Image replacement tests save and reopen edited records, check payloads and encoding metadata, cover grayscale/bi-level conversion and transparency, and verify failed imports preserve the original image and pending edits.

There is also a small debugging helper that dumps every decodable image of a file to `/tmp`:

```bash
cargo run --example debug_decode -- test_data/type-14-amp-nqm-utf8.an2
```

## License

The Rust WSQ codec follows the public-domain NIST NBIS algorithms; its original notice is retained in [`src/wsq/NOTICE`](src/wsq/NOTICE). Independently decoded compatibility fixtures and their provenance are in [`test_data/wsq_encoder/`](test_data/wsq_encoder/README.md). Builds and tests do not compile or invoke native WSQ code.
