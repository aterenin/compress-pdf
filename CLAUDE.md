# compress-pdf

Pure-Rust command-line PDF compressor. It reads a PDF, runs a fixed sequence of
independent stages over the document (image recompression, font reduction,
stripping of non-visual data, structural cleanup), and writes a smaller PDF
plus a report of what each stage did.

All behavior is driven by a `Config` value; three named presets (`less`,
`standard`, `more`) are predefined `Config`s and every field can be
overridden from the command line.

## Toolchain

`cargo` lives in `~/.cargo/bin`, which is not on PATH in non-interactive shells.

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
kiss check
cargo run -- some.pdf --dry-run -v
```

Rust edition 2024. A change is done when `cargo test`, `clippy -D warnings`,
`fmt --check`, and `kiss check` all pass. Iterate until they do.

### kiss

[kiss](https://github.com/dsweet99/kiss) (`cargo install kiss-ai`) gives
whole-codebase static feedback: per-function and per-file complexity limits,
module dependency shape, duplicate-code detection, and orphan-module
detection. Only `kiss check` is used; tests are plain `cargo test`.

- `kiss check` is fast; run it after every edit and fix what it reports by
  simplifying code.
- `kiss rules` prints the live rule catalog with thresholds; `kiss stats`
  shows the metric distribution; `kiss viz graph.md` writes the module graph.
- Thresholds live in `.kissconfig` and start at kiss's defaults, with the
  deviations commented inline. Raising a threshold is a design decision to be
  discussed, not a way to make a check pass.

Any image-heavy PDF works as a smoke input. Evaluation data lives under
`evals/`, which is gitignored and empty on a fresh checkout until
`cargo evals fetch` populates it.

## Design

### How compression happens

Scope rule: a technique or option exists in this crate only if at least one
preset uses it. Everything a preset enables is implemented in full; nothing
else is exposed, even as a flag.

Size reduction comes from the techniques below. Each is applied only where its
preconditions hold, and each keeps the original bytes when its result is not
smaller. The Done column is `[x]` implemented as described, `[~]` partly
(details under Implementation status), `[ ]` not started. Libraries marked "own" are implemented in this crate on top of the
object model; "Rust" is a pure-Rust crate; "C" is a C/C++ library reached
through Rust bindings.

| Technique | What it does | Applies to | Library | Done |
|---|---|---|---|---|
| Image decoding | Turn any PDF image stream into a raster so the techniques below can act on it: raw and Flate samples at 1, 2, 4, 8 and 16 bits, `Decode` arrays, PNG and TIFF predictors, every color space (Device*, ICCBased, Indexed, Lab, CalRGB/CalGray, Separation, DeviceN with their tint-transform functions), and every filter (DCT, JPX, CCITT G3/G4, JBIG2, LZW, RunLength, ASCII85/Hex). | every image | `image` and `zune-jpeg` (Rust) for DCT, `hayro-jpeg2000` (Rust) for JPX, `hayro-ccitt` (Rust) for CCITT input and `fax` (Rust) for G4 output, `hayro-jbig2` (Rust) for JBIG2, all three already compiled in through `hayro-syntax`; own for sample unpacking, color spaces, and PDF function evaluation | [~] |
| Downsampling | Resample an image to a target resolution when its effective resolution on the page exceeds a threshold. Effective resolution comes from the rendered size of every placement. | bitonal, gray, color images, and their masks | `fast_image_resize` (Rust); placement analysis is own code over lopdf's content-stream parser | [x] |
| Image clipping | Crop an image to the bounding box, in image space, of the union of the clip regions in effect at each of its placements, so pixels that can never be visible are discarded before re-encoding. Pixels inside the box but outside a non-rectangular path are kept. Pre-blended images (`/Matte`) are not clipped. Clipping implies resource optimization, since the cropped image is a new object. | every image with a clip narrower than its placement | own; clip tracking is part of the placement analysis | [ ] |
| Lossy re-encoding | Re-encode continuous-tone images as JPEG at a preset quality. | gray and color images | `mozjpeg` (C) | [x] |
| Lossless re-encoding | Re-encode with Flate, choosing the PNG predictor per image. | any image, indexed images especially | `flate2` (Rust, miniz_oxide backend) | [x] |
| Bitonal encoding | Encode 1-bit images with CCITT G4 or JBIG2. JBIG2 output is generic-region coding (lossless) today; symbol mode with one shared globals stream per document is planned once a lossless symbol encoder is available. | bitonal images and stencil masks | `fax` (Rust), `jbig2enc-rust` (Rust) | [~] |
| Best-of selection | Encode with every codec allowed for the image's class plus the original bytes; keep the smallest. Images never grow. | every image touched | own | [x] |
| Color complexity reduction | RGB or CMYK with all channels equal becomes gray; two-level gray becomes bitonal; flat images become 1x1; opaque soft masks are removed; two-level soft masks become stencil masks. | images with device color spaces, and indexed images with a device base | own, on raster buffers | [ ] |
| Color conversion | Convert images to RGB through ICC profiles: the image's embedded ICCBased profile when present; otherwise synthesized defaults, sRGB for RGB and gray, and for CMYK a LUT profile generated from the Neugebauer model with the published default coefficients. No third-party profile is bundled. | color images, when a preset asks | `moxcms` (Rust); own Neugebauer LUT generator | [ ] |
| Font subsetting | Reduce embedded TrueType and CFF programs to the glyphs actually used. | embedded fonts | `fontcull` / `klippa` (Rust) | [ ] |
| Font merging | Merge embedded font programs that originate from the same font (same type, name, and encoding) into one, repointing every font dictionary that used them. | duplicate TrueType and Type 1 embeddings | own, over the font parsers | [ ] |
| Type 1 to CFF conversion | Convert embedded Type 1 (`/FontFile`) programs to CFF (`/FontFile3`), which is more compact. | Type 1 fonts | `hayro-font` (Rust) to parse Type 1 charstrings; own CFF v1 writer (see open decisions) | [ ] |
| Standard-font unembedding | Drop the program of an embedded standard-14 font when its Unicode mapping is clean, so viewers substitute. | Helvetica, Times, Courier, Symbol, ZapfDingbats families | own | [ ] |
| Stripping | Remove non-visual parts: article threads, metadata, piece info, structure tree, thumbnails, spider info, alternate images, output intents. | document and page dictionaries | own, over lopdf | [x] |
| Structural cleanup | Flate-compress uncompressed streams, deduplicate identical objects by content hash, drop unused resources and unreferenced objects, renumber, raise the PDF version to what the output needs, write object streams and an xref stream. | whole file | `lopdf` (Rust) plus own dedupe | [~] |

`lopdf` provides the object model, parser, and writer for everything above.

### Data flow

```
main -> Cli -> Config -> pipeline::run(doc, config, report) -> save_modern -> verify -> write
                             |
             usage -> images -> fonts -> strip -> structure
```

- `src/lib.rs` exposes `config`, `pipeline`, `report`, and `stages`; the
  binary and the test harnesses are clients of it.
- `src/cli.rs` turns flags into a `Config`. Nothing below `main` sees clap.
- `src/config.rs` is the whole knob set, as data. Presets are `Config` values.
- `src/pipeline.rs` defines `Stage` and runs the fixed stage list, measuring
  serialized size after each stage for the report.
- `src/report.rs` is the only output. Per-image rows, per-stage sizes, notes.
- `src/stages/*.rs` one file per stage, each a unit struct implementing `Stage`.

### Stage contract

A stage gets `&mut lopdf::Document` and `&mut Context { config, report, usage }`.
It may mutate the document freely and must append what it did to the report.
Rules:

1. Never make the file larger. If the encoded result is not smaller, keep the
   original bytes. This is the `Codecs::SOURCE` candidate in the image stage,
   and `pipeline::serialize` applies the same rule to the whole file: it
   writes the smaller of lopdf's two writers, and if neither beats the input
   it writes the input bytes unchanged and says so in the report.
2. Anything the stage does not fully understand is left byte-for-byte untouched
   and gets a `report.note(...)` saying why. Silent skips are bugs.
3. Stages do not call each other. Shared analysis goes through `Context`.
4. Stages are independently testable: build a tiny PDF with lopdf in a unit
   test, run the one stage, assert on the document and the report.
5. `enabled(config)` decides whether a stage runs. Do not put config checks
   deep inside `run`; make the stage a no-op at the top level instead.
6. Per-item isolation. Work on one image or one font at a time and never
   hold more than one decoded raster in memory; a 600 dpi color scan page is
   hundreds of megabytes raw. A decoder panic on one item (corrupt or
   adversarial input is normal in the corpus) is caught with
   `catch_unwind`, reported as "kept: decoder panic", and does not abort the
   run.
7. Determinism. The same input and configuration produce byte-identical
   output. No timestamps, random IDs, or hash-map iteration order reach the
   file; the trailer `/ID` is derived from a hash of the content. This is
   what lets the evals harness cache and compare runs.

### Stages

Pipeline order. Analysis first, then the lossy image work, then fonts, then
the cheap structural passes that clean up whatever the earlier stages left
behind.

| Stage | File | Reads | Writes | Responsibility | Done |
|---|---|---|---|---|---|
| usage | `stages/usage.rs` | page, form XObject, pattern, and annotation appearance content streams | `Context.usage` | Walk each content stream tracking the CTM and the current clip through `q`/`Q`/`cm`/`W n`/`re` and form `/Matrix` and `/BBox`; at every image `Do` record the rendered size in points and the clip region in image space. `ImageUsage` holds, per image object, its pixel size and all placements; effective DPI is the minimum over placements on the tighter axis; the crop box is the union of placement clips. Images reached only through paths the walker does not follow get no entry and are reported as unknown. Does not mutate the document. | [x] |
| images | `stages/images.rs` | image XObjects, `Context.usage` | image streams and dictionaries | Classify, decode to raster, transform (clip to crop box, color conversion, color-complexity reduction, downsampling), encode with every allowed codec plus the original bytes, keep the smallest, rewrite the stream keeping SMask/Mask consistent. Split into `images/{classify,decode,transform,encode,rewrite}.rs` when it grows. | [~] |
| fonts | `stages/fonts.rs` | font dictionaries, content streams | font programs and dictionaries | Unembed the 14 standard fonts when the font's Unicode mapping is trustworthy; convert Type 1 programs to CFF; merge duplicate embeddings of the same font; subset embedded TrueType/CFF programs to the glyphs referenced by content streams. In that order, so subsetting runs once on the merged result. | [ ] |
| strip | `stages/strip.rs` | catalog, pages, XObjects | dictionary entries only | Remove the parts selected by `Strip` flags: threads, metadata streams, piece info, structure tree, thumbnails, spider info, alternate images, output intents. Removed objects become unreferenced and are collected by `structure`. | [x] |
| structure | `stages/structure.rs` | whole object map | whole object map | Flate-compress uncompressed streams, deduplicate identical streams and dictionaries by content hash and repoint references, drop unused entries from `/Resources` dictionaries, drop unreferenced objects, renumber, raise the header version to the minimum the output needs (1.4 for JBIG2; lopdf raises to 1.5 itself when it writes object streams). Object streams and the xref stream are written by lopdf's `save_with_options`, configured in `pipeline::serialize` for maximum packing (level 9, up to 5,000 objects per stream). | [~] |

### Output verification

After serialization and before the file is written, `verify` checks the
output with a parser that shares no code with the writer, so a bug in our
object handling or in lopdf's writer is caught rather than shipped.
`hayro-syntax` (pure Rust, the parser behind the `hayro` rasterizer used by
Typst) is the independent reader; `hayro` is the rasterizer. Two levels:

1. **Structural, always on.** Parse the output bytes with `hayro-syntax`.
   Check: the xref and trailer resolve; the page count equals the input's;
   every page's content stream decodes; every stream, images included,
   decodes with its declared filters. Problems are grouped into categories
   (parse, page count, content stream, stream, image). When the output has
   any, the input is verified the same way as a baseline: problems the
   input already had are reported as warnings and the output is still
   written; a category with more problems than the input is a **bug, not a
   warning**: the output is not written, the process exits non-zero with
   the finding, and the report is still printed. Cost is one parse of the
   output, plus one of the input only on the failure path.

2. **Visual, opt-in (`--verify render`).** Rasterize every page of input and
   output with `hayro` at a fixed low resolution (72 dpi) and compare with
   SSIM. A page below the per-preset floor produces a **warning** naming the
   page and score; the output is still written; `--strict` turns it into a
   failure. Off by default in the CLI because it costs a full render of both
   documents; on in the evals harness and in `cargo evals score`, which
   reuse the same function.

The `Verify` result is part of the report so a dry run shows what would
have been checked. External validators (`mutool`, `qpdf --check`,
Ghostscript) are not runtime dependencies; the evals harness may call them
when present as an extra cross-check, never the binary.

### Image stage details

- Classes: bitonal (1 bpc, or ImageMask), indexed (Indexed color space),
  continuous (everything else). Each class has its own codec set and DPI rule.
- Decoding covers every color space and filter listed in the techniques table.
  A stream that still fails to decode (corrupt data, unsupported JPX feature)
  is kept byte-for-byte with a note naming the reason; that is the only
  "kept" path.
- A JPEG that needs no transform (no clip, no downsample, no color change) is
  passed through: the original bytes are a `source` candidate and no decode
  happens unless another candidate codec is in the set.
- SMask and Mask are clipped and resampled with the same factor as their
  parent and never dropped unless `reduce_color_complexity` proves them opaque.
- Downsample only when `usage` reports an effective DPI above the class
  threshold. Unknown DPI means do not downsample and do not clip.
- CMYK JPEGs may carry an Adobe APP14 inversion marker; honor it on decode
  and on re-encode.
- Color conversion goes through `moxcms`. Source profile: the image's
  ICCBased stream when present, else a synthesized default for its device
  space: sRGB for DeviceRGB, sRGB gray for DeviceGray, and for DeviceCMYK a
  CMYK-to-Lab LUT profile computed from the Neugebauer model with a
  16-primary coefficient table that lives in the code. Target: synthesized
  sRGB. Lab and Cal* spaces convert through their defined transforms to the
  target. No ICC file is bundled or loaded from disk.
- Bitonal candidates: G4 (`fax` crate) and JBIG2 (`jbig2enc-rust`, generic region, lossless). Continuous: JPEG (`mozjpeg`)
  and Flate with predictor selection. Indexed: Flate.

### Strip flag mapping

| Flag | Removes |
|---|---|
| THREADS | catalog `/Threads`, page `/B` |
| METADATA | catalog and per-object `/Metadata` streams |
| PIECE_INFO | `/PieceInfo` on catalog, pages and XObjects |
| STRUCT_TREE | catalog `/StructTreeRoot`, `/MarkInfo`; page `/StructParents`; marked-content operators stay in place |
| THUMBNAILS | page `/Thumb` |
| SPIDER | catalog `/SpiderInfo` |
| ALTERNATES | image `/Alternates` |
| OUTPUT_INTENTS | catalog `/OutputIntents` |

Annotations and form fields are never stripped or flattened; no preset asks
for it (scope rule).

### Presets

`standard` and `more` share a common base (`Config::heavy_base()`),
which enables the lossless passes (resource cleanup, font subsetting, stripping)
and image recompression; the presets differ in resolution, quality, codec
choice and color handling. `less` starts from the all-off baseline instead.

| Setting | Less | Standard | More |
|---|---|---|---|
| bitonal codecs | jbig2 (+source) | g4, source | jbig2 (+source) |
| continuous codecs | jpeg (+source) | jpeg, flate, source | jpeg (+source) |
| indexed codecs | none (untouched) | flate, source | flate, source |
| JPEG quality | 75 | 60 | 60 |
| downsample to / if above | 200 / 400 dpi | 150 / 150 dpi | 72 / 110 dpi |
| color conversion | none | none | to RGB |
| clip images | no | yes | yes |
| reduce color complexity | no | yes | yes |
| fonts | subset, unembed std 14 | subset, merge, CFF | subset, merge, CFF, unembed std 14 |
| strip | none | threads, metadata, piece info, thumbs, spider, alternates, output intents | same, minus output intents, plus struct tree |

`source` is present in every non-empty codec set so that rule 1 above holds
for all presets: an image is only rewritten when a candidate encoding is
strictly smaller than the bytes already in the file. There is no
force-recompress option.

Under the scope rule, `Config` carries only what the presets use. Codec sets
are drawn from jpeg, flate, g4, jbig2, source. Color conversion is none or
RGB. Strip flags are the eight in the mapping table. The command line exposes
the preset plus overrides for the fields in the table above; there is no
grayscale option.

### Testing

The crate is a library (`src/lib.rs`: config, pipeline, report, stages) plus
a thin binary (`src/main.rs`), so every test layer calls the pipeline
in-process rather than shelling out.

Three layers, all run by `cargo test`:

1. **Unit tests** next to the code. Each stage has at least one test that
   builds a minimal document with lopdf, runs that stage alone, and asserts
   on both the document and the report. No binary fixtures are checked in.

2. **Probe tests** in `tests/probes.rs`. Synthetic PDFs generated in code by
   `probes::` helpers (one variable each: a CMYK patch image, a 1-bit line
   pattern at 600 dpi, an indexed image at a chosen dpi, an already-subset
   font, a file with duplicate streams, and so on). Each probe asserts the
   design's stated behavior. The same generators are used to produce probe
   files on disk for running through a reference tool, so the answers to the
   open questions and the regression tests come from one source.

3. **Corpus evals** in `tests/evals.rs`, a custom harness (`harness = false`,
   built on `libtest-mimic`) that discovers every PDF under
   `evals/corpus/` at runtime and registers one test per file per preset.
   `cargo test --test evals` lists and filters them like ordinary tests, so
   `cargo test --test evals issue1001` runs one file. If the corpus directory
   is absent the harness registers nothing and passes, with a message saying
   how to fetch it. Per file the invariants checked are:
   - the pipeline completes without error;
   - the output is not larger than the input;
   - the output re-parses with lopdf and has the same page count;
   - every image row in the report has `bytes_out <= bytes_in`;
   - when `pdftoppm` is on PATH and `EVALS_RENDER=1`, each page rasterized
     before and after has SSIM at or above a per-preset floor.
   Known failures are listed in `evals-expectations.toml` with a reason and
   are reported as ignored, so the suite stays green and the list is the
   backlog.

   The corpus is 4,000+ files and 1 GB, so `cargo test` does not run all of
   it. `evals.toml` defines named subsets under `[subsets]`; `quick` is an
   explicit list of about 25 small files chosen so that every encoding and
   feature we handle appears at least twice, plus a few realistic
   multi-page documents, totalling a few MB. The harness runs `quick` by
   default. `EVALS_SUBSET=full` (or `=scoring`, or any other subset name)
   selects a different one; the files outside the selected subset are
   registered as ignored tests, so `cargo test --test evals -- --ignored`
   also runs everything.

**Evals tooling** is one example binary, `examples/evals.rs`, exposed
through a cargo alias in `.cargo/config.toml` so it reads as a cargo
subcommand:

```bash
cargo evals fetch            # clone/download everything in evals.toml (idempotent)
cargo evals fetch pdfjs      # one source only
cargo evals status           # what is present, missing, or stale vs. the pins
cargo evals score            # each preset vs. the references named for it
cargo evals score --preset more --reference app-a-more --subset scoring
cargo evals probes out/      # write the synthetic probe PDFs to a directory
```

`evals.toml` at the repository root lists each source with its git URL,
pinned commit, license, and the subdirectory to take, plus the hand-picked
scoring subset by path. `fetch` performs shallow, sparse clones at the pinned
commits into `evals/corpus/<name>/` using the `git` on PATH, resolves
pdf.js `.link` files by downloading their URLs into
`evals/corpus/pdfjs-linked/` (failures are recorded, not fatal), and
writes `evals/corpus/MANIFEST.json` with what was fetched and at which
commit. `status` compares the manifest against `evals.toml`. `score` runs
the scoring subset and compares against reference outputs. A reference is
a directory `evals/reference/<name>/` holding an external tool's output for
the same inputs, matched by file name; there can be any number. A reference
is just a directory, so a tool run with several settings is several
references, named by convention `<tool>-<preset>` (for example
`app-a-standard`). `score --preset standard` compares our `standard` output
against every reference, one column each, with sizes and ratios per file
and in total; without `--preset` it runs each of our presets against the
references whose name ends in that preset. `--reference <name>` narrows to
one. It is the number to watch, not a pass/fail. A reference with outputs
for only some inputs still gets a column, with blanks and a coverage count.
A reference directory may contain a `tool.toml` (name, version, date, and
which of the tool's settings were used); when present, `score` prints it in
the column header. `probes` writes the same
synthetic PDFs the probe tests use, for running through a reference tool.
Sources:

| Name | Source | Files | Role |
|---|---|---|---|
| py-pdf | github.com/py-pdf/sample-files (CC-BY-SA 4.0) | 35 | scoring and soak |
| pdfjs | github.com/mozilla/pdf.js, `test/pdfs` (mixed licenses) | 982 in repo, 459 linked | soak; selected files in scoring |
| verapdf | github.com/veraPDF/veraPDF-corpus | 2,904 | soak only; color space, metadata and output-intent edge cases |
| hand-picked | arXiv papers (Type 1 fonts), Internet Archive scans (JBIG2, JPX) | about 10 | scoring |

Everything under `evals/` is gitignored and is never a build input.
Third-party material placed there for local comparison must never be linked,
loaded, or copied into the crate.

### Out of scope

Excluded by the scope rule (no preset uses them): MRC segmentation, JPEG 2000
output, LZW and RunLength output, CMYK as a conversion target, annotation and
form stripping or flattening, linearization, force-recompress.

Excluded for v1 regardless: inline images (`BI ... EI`), encrypted input,
PDF/A conformance preservation.

### Open decisions

- `lopdf` vs `qpdf` bindings. Starting with lopdf: pure Rust, no C++ build
  step, direct access to the object map. Known limits from the full-corpus
  run: its writer is a few percent less compact than good producers even
  with object streams packed at level 9 (the file-level never-grow rule
  absorbs this); it loads some damaged-xref files into a broken graph
  without error; it cannot load 11 of 4,368 corpus files and hangs on one.
  Revisit if repair of damaged files becomes a goal; the `Stage` trait would
  not change.
- JPX decoding uses `hayro-jpeg2000` (pure Rust), which arrives with
  `hayro-syntax` and is exercised on every image the verify step checks. It
  decoded all 51 JPX-bearing corpus files without regressions. With this the
  only C dependency is mozjpeg. Revisit if a corpus file exposes a codestream
  feature it lacks; OpenJPEG through bindings is the fallback.
- CFF writing: `write-fonts` 0.53 ships `ps::cff::v2` with only `Cff2Header`
  and `Index` (CFF2 container primitives); its generated CFF v1 types are not
  compiled into the crate, and neither version has dict or charstring
  writers. PDF `/FontFile3` needs CFF v1, so the conversion needs an own
  writer for the subset a converted Type 1 font requires: header, INDEXes,
  top and private dict encoding, Type 2 charstrings, standard encoding, no
  CID keying. `read-fonts::tables::cff` can parse the result in tests.
  Revisit when write-fonts exposes CFF v1.

### Provisional choices

Fixed for v1 and expected to be revisited against evals results.

- Effective DPI: the minimum over placements on the tighter axis, from a
  content-stream walk.
- Flate predictor selection: try each PNG predictor per image and keep the
  smallest.
- JBIG2 mode: generic-region coding, lossless. The available encoder's symbol
  mode substitutes glyphs (lossy) and has no refinement, so it is not used.
  Revisit when a lossless symbol mode exists or the evals show generic
  coding far behind the reference outputs on scanned text.
- Content stream rebuild: re-serialize each content stream from its parsed
  operator list, which normalizes formatting and makes resource references
  exact; nothing else is changed.

## Implementation status

The crate is split into `src/lib.rs` (config, pipeline, report, stages) and
a thin `src/main.rs`. It runs end to end and re-reads its own output.
`main`, `cli`, `config`, `pipeline`, and `report` are complete for the
design above; `config` and `usage` have unit tests. `tests/evals.rs` is in
place: `cargo test --test evals` runs the `quick` subset under all three
presets (78 trials, about two seconds) with the four structural invariants;
the render-based check is not wired in yet. `src/verify.rs` implements the
structural level with `hayro-syntax`, with the input as baseline; `main`
refuses to write on regressions and the harness fails on them. Encrypted
input is refused by `pipeline::run` and the harness treats that refusal as
the expected outcome. The full corpus (`EVALS_SUBSET=full`, standard
preset, release build) runs in about 20 seconds; `evals-expectations.toml`
lists 39 files: ones lopdf cannot load, loads into a broken graph, or hangs
on, plus two where its loader drops a stream with a wrong `/Length` and
verification blocks the resulting content loss. Stages:

| Stage | Status | Done when |
|---|---|---|
| usage | done: CTM and bounding-box clip walk over page content, form XObjects (with `/Matrix` and `/BBox`), tiling patterns, and annotation appearance streams; `ImageUsage` holds pixels, placements with rendered size and visible fraction, `min_dpi()` and `crop_box()`; tested for two placements, rectangular clip, `Q` restoring the clip, form matrices, and undrawn images | |
| images | second milestone: classify; decode raw/Flate/LZW samples at 1 to 16 bits in Device, ICCBased (by N), CalRGB/CalGray and Indexed spaces with identity or inverting `Decode`; decode DCT (Gray, RGB) including Flate-wrapped JPEGs; decode CCITT (all K modes, `BlackIs1`, indirect `DecodeParms`), embedded JBIG2 with globals, and JPX (codestream color space and depth win; alpha channels skipped) via hayro's decoders; downsample per class rule (Lanczos3, nearest for indices, gray-then-threshold for bitonal); encode Flate with per-row PNG predictor choice, JPEG via mozjpeg, CCITT G4 via `fax`, JBIG2 generic region via `jbig2enc-rust`; unwrapped-JPEG passthrough candidate; best-of with never-grow; soft masks resized with their parent; per-image report rows | Remaining, in order: CMYK JPEG (Adobe inversion) and CMYK JPEG output; color conversion (moxcms, Neugebauer LUT); color complexity reduction; clipping via a wrapping form XObject; stencil `/Mask` resampling; Separation/DeviceN/Lab; non-trivial `Decode` arrays; JBIG2 symbol mode with shared globals once a lossless symbol encoder is available. |
| fonts | stub | Unembed standard 14, Type 1 to CFF, merge, subset, in that order. |
| strip | done: every flag removes the keys in the mapping table; catalog keys on the catalog, the rest on any object | |
| structure | done except content-stream re-serialization: unused resource entries removed (pages, form XObjects, tiling patterns, Type 3 fonts; owners that do not decode, inherited resources, and Type 3 fonts without resources are left alone), streams compressed, duplicate objects merged by canonical form, unreferenced objects pruned, renumbered, version raised for JBIG2 | Content streams re-serialized from parsed operators under the never-grow rule. |

Implementation order was structure, strip, usage, then images, then fonts.
This differs from pipeline order on purpose: structure and strip are cheap
and verify the harness; usage must exist before images can downsample or
clip safely.

Present: `evals.toml` with the three sources pinned, and `examples/evals.rs`
behind the `cargo evals` alias with `fetch` (shallow sparse clones, pdf.js
link resolution with recorded failures, manifest) and `status` working;
`score` and `probes` are stubs that exit with "not implemented". A full
fetch takes about five minutes and 1.1 GB; re-running retries only failed
links. `evals.toml` defines the `quick` subset (26 files, 4.7 MB, every
handled feature at least twice plus seven realistic documents) and an empty
`scoring` subset; `status` reports each subset's presence on disk.

Not yet present: the visual level of `verify` (`hayro` rendering and SSIM),
`tests/probes.rs` and its generators,
`tests/evals.rs` with `libtest-mimic`, `evals-expectations.toml`, the
`score` and `probes` subcommands, and the rasterize-and-compare check.
