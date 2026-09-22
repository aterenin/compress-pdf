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
Building needs a C compiler for mozjpeg and a C++ compiler plus libclang
(bindgen) for the bundled HarfBuzz; on macOS the Xcode command line tools
provide all three.

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
kiss check
cargo run -- some.pdf --dry-run -v
```

The crate is MIT licensed; dependencies carry permissive licenses (MIT,
Apache, BSD, Zlib, IJG for mozjpeg, Unicode for a few ICU tables), and no
code is copied from other projects.

Rust edition 2024, `rust-version` 1.92 (the oldest the hayro crates
accept; checked by a CI job on that toolchain). The README doubles as the
crate-level documentation through `include_str!` in `src/lib.rs`, and
`Cargo.toml` excludes CI and the evaluation tooling from the published
package; AGENTS.md ships with it. A change is done when `cargo test`, `clippy -D warnings`,
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
| Image clipping | Crop an image to the bounding box, in image space, of the union of the clip regions in effect at each of its placements, so pixels that can never be visible are discarded before re-encoding. Pixels inside the box but outside a non-rectangular path are kept. Pre-blended images (`/Matte`) are not clipped, and a crop that removes less than a sixteenth of the area, or saves fewer bytes than its wrapper form costs, is not applied. Clipping implies resource optimization, since the cropped image is a new object. | every image with a clip narrower than its placement | own; clip tracking is part of the placement analysis | [x] |
| Lossy re-encoding | Re-encode continuous-tone images as JPEG at a preset quality. | gray and color images | `mozjpeg` (C) | [x] |
| Lossless re-encoding | Re-encode with Flate, choosing the PNG predictor per image. | any image, indexed images especially | `flate2` (Rust, miniz_oxide backend) | [x] |
| Bitonal encoding | Encode 1-bit images with CCITT G4 or JBIG2. JBIG2 output is generic-region coding (lossless) today; symbol mode with one shared globals stream per document is planned once a lossless symbol encoder is available. | bitonal images and stencil masks | `fax` (Rust), `jbig2enc-rust` (Rust) | [~] |
| Best-of selection | Encode with every codec allowed for the image's class plus the original bytes; keep the smallest. Images never grow. | every image touched | own | [x] |
| Color complexity reduction | RGB or CMYK with all channels equal becomes gray; two-level gray becomes bitonal; flat images become 1x1; opaque soft masks are removed; two-level soft masks become stencil masks. | images with device color spaces, and indexed images with a device base | own, on raster buffers | [x] |
| Color conversion | Convert images to RGB through ICC profiles: the image's embedded ICCBased profile when present; otherwise synthesized defaults, sRGB for RGB and gray, and for CMYK a LUT profile generated from the Neugebauer model with the published default coefficients. No third-party profile is bundled. | color images, when a preset asks | `moxcms` (Rust); own Neugebauer LUT generator | [x] |
| Font subsetting | Reduce embedded TrueType, CFF and OpenType programs to the glyphs the content streams use. Glyph IDs are retained, so nothing that refers to a glyph (content strings, CMaps, `CIDToGIDMap`, the font's `cmap`) changes; unused glyphs keep their ID and lose their outline. Hinting and layout tables are dropped, except that a CFF program whose charstrings place stem hints after drawing has begun (Ghostscript's Type 1 conversions do) keeps its hints, since HarfBuzz's hint removal cuts such outlines. Which glyphs a simple font's codes reach is computed as the union over every rule a viewer may apply (symbol, Macintosh and Unicode `cmap` subtables, `post` and CFF glyph names, built-in encodings). | embedded TrueType, CFF, OpenType; Type 1 after conversion | `hb-subset` (HarfBuzz, C++, bundled) for the subsetting; `read-fonts` (Rust) to read `cmap`, `post` and CFF charsets and encodings; own code for the PDF side (encodings, embedded CMaps, the OpenType wrapper HarfBuzz needs around bare CFF) | [x] |
| Font merging | Merge embedded font programs that are byte-identical into one, repointing every font dictionary that used them. Merging different subsets of the same font would need glyph-level comparison of programs whose glyph IDs were renumbered independently, and is not attempted. | duplicate embeddings | the structure stage's deduplication | [x] |
| Type 1 to CFF conversion | Convert embedded Type 1 (`/FontFile`) programs to CFF (`/FontFile3`, `Type1C`), which is more compact and which the subsetter can then reduce. Charstrings are translated to Type 2 with subroutines expanded, flex and seac carried over, and hints dropped; the built-in encoding and private values are kept. A glyph that fails to translate keeps the whole program unconverted. | Type 1 fonts | own: Type 1 reader, charstring translator and CFF writer (`font/{type1,charstring,cff}.rs`), whose handling of malformed programs was learned from pdf.js and implemented independently; `read-fonts` parses the result back in tests, `hayro-font` renders the Type 1 original as the test oracle | [x] |
| Standard-font unembedding | Drop the program of an embedded standard-14 font when its encoding stands on its own (a standard encoding name, a differences dictionary with known glyph names, or a non-symbolic descriptor; Symbol and ZapfDingbats only with their built-in encoding), so viewers substitute. Name aliases are folded to the canonical 14 and the font is renamed. | Helvetica, Times, Courier, Symbol, ZapfDingbats families | own | [x] |
| Stripping | Remove non-visual parts: article threads, metadata, piece info, structure tree, thumbnails, spider info, alternate images, output intents. | document and page dictionaries | own, over lopdf | [x] |
| Structural cleanup | Flate-compress uncompressed streams, rewrite content streams in a canonical token form (single spaces, no comments, numbers without redundant digits) when that compresses smaller, deduplicate identical objects by content hash, drop unused resources (an AcroForm's default resources count as one more resource owner, whose users are the default appearance strings) and unreferenced objects, renumber, raise the PDF version to what the output needs, write object streams and an xref stream. | whole file | `lopdf` (Rust) plus own dedupe and content lexer | [x] |

`lopdf` provides the object model, parser, and writer for everything above.

### Data flow

```
main -> Cli -> Config -> compress::compress(bytes, config, verify) -> write
                              |
            pipeline::run -> pipeline::serialize -> verify (structural, render)
                             |
             usage -> images -> fonts -> strip -> structure
```

- `src/lib.rs` exposes `compress`, `config`, `error`, `pipeline`, `report`
  and `verify`; that is the crate's API, and the binary and the test harnesses
  are clients of it. `stages`, `font` and `content` are private
  implementation. The `Stage` trait and `Context` are crate-private too,
  so stages can change without an API break. Public structs that the
  library fills in (`Config`, `Report` and its rows, `Compressed`,
  `Verification`, `Comparison`) are `#[non_exhaustive]`, so a field can be
  added in a minor release; so are the enums `Preset`, `ColorConversion`,
  `Verify` and `Category`, so a variant can be.
- `src/compress.rs` is the one-call entry point: what the command line does
  for a document, as a library function. It loads the bytes, runs the
  pipeline, serializes, applies the structural check with the input as
  baseline, and the render check when asked, returning the output and the
  report or a rejection carrying the report. The result also carries the
  structural check and the page comparison as values, so a program does
  not parse notes. `main` only reads files, calls it, and writes or prints.
- `src/error.rs` is `Refusal`, the one error type: why a document was not
  compressed or its output not accepted, as variants a program can match
  on, with `Display` giving the message the command prints. Anything a
  stage or the writer fails with is `Internal`. Refusal reasons are typed
  because a caller decides on them; report actions, kept reasons and notes
  stay strings because they exist to be read.
- `src/content.rs` is the content-stream lexer that produces the canonical
  form the structure stage stores. It never reads `Config`.
- `src/font/` is font-program machinery independent of the pipeline:
  standard-14 recognition, and the CFF, TrueType and Type 1 parsing and
  writing the font stage builds on. It never reads `Config`.
- `src/cli.rs` turns flags into a `Config`. Nothing below `main` sees clap.
  Logging shows our warnings by default and lopdf's one verbosity level
  later, since lopdf warns once per item it cannot handle and some files
  have hundreds of thousands; `RUST_LOG` overrides both.
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
| usage | `stages/usage.rs` | page, form XObject, pattern, and annotation appearance content streams | `Context.usage` | Walk each content stream tracking the CTM and the current clip through `q`/`Q`/`cm`/`W n`/`re` and form `/Matrix` and `/BBox`; at every image `Do` record the rendered size in points and the clip region in image space. `ImageUsage` holds, per image object, its pixel size and all placements; effective DPI is the minimum over placements on the tighter axis; the crop box is the union of placement clips. The same walk records, per font, the strings shown with it (`Tj`, `TJ`, `'`, `"`), with `Tf` tracked as part of the saved graphics state. Images and fonts reached only through paths the walker does not follow get no entry. Does not mutate the document. | [x] |
| images | `stages/images.rs` | image XObjects, `Context.usage` | image streams and dictionaries | Classify, decode to raster, transform (clip to crop box, color conversion, color-complexity reduction, downsampling), encode with every allowed codec plus the original bytes, keep the smallest, rewrite the stream keeping SMask/Mask consistent. Split into `images/{classify,decode,transform,encode,bitonal,function}.rs`: dictionary reading and color-space mapping, decoding, raster type and transforms, output codecs, bitonal codecs, PDF functions with the type 4 calculator. | [~] |
| fonts | `stages/fonts.rs` | font dictionaries, content streams | font programs and dictionaries | Unembed the 14 standard fonts when the font's encoding stands on its own; convert Type 1 programs to CFF; subset embedded TrueType/CFF/OpenType programs to the glyphs referenced by content streams, glyph IDs retained. Byte-identical programs are merged by the structure stage. | [x] |
| strip | `stages/strip.rs` | catalog, pages, XObjects | dictionary entries only | Remove the parts selected by `Strip` flags: threads, metadata streams, piece info, structure tree, thumbnails, spider info, alternate images, output intents. Removed objects become unreferenced and are collected by `structure`. | [x] |
| structure | `stages/structure.rs` | whole object map | whole object map | Rewrite content streams in canonical form where smaller, Flate-compress uncompressed streams, deduplicate identical streams and dictionaries by content hash and repoint references, drop unused entries from `/Resources` dictionaries, drop unreferenced objects, renumber, raise the header version to the minimum the output needs (1.4 for JBIG2; lopdf raises to 1.5 itself when it writes object streams). Object streams and the xref stream are written by lopdf's `save_with_options`, configured in `pipeline::serialize` for maximum packing (level 9, up to 5,000 objects per stream). | [x] |

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
the preset as one of `--less`, `--standard` (the default) and `--more`,
plus an override for every `Config` field: `--dpi` and
`--threshold-dpi` (all classes at once), `--quality`, the three codec lists,
`--color-conversion`, `--strip`, and a `true`/`false` flag for each boolean
(clipping, color reduction, the four font passes, resource cleanup,
deduplication, content-stream rebuild). There is no grayscale option.
Several inputs are compressed one after another with the same settings;
`-o` names the output file for one input, or the output directory for
several, and without it each output lands next to its input as
`<name>-compressed.pdf`. One file's failure is reported and the rest still
run; the exit status is then non-zero.

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
   - with `EVALS_RENDER=1`, each page rasterized before and after with
     `hayro` has SSIM at or above the preset's floor.
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
the same inputs, laid out like the corpus: the reference for
`evals/corpus/pdfjs/test/pdfs/tracemonkey.pdf` is
`evals/reference/<name>/pdfjs/test/pdfs/tracemonkey.pdf`, so files that
share a name in different folders (veraPDF has 218 such names) stay
distinct. There can be any number of references. `score` also writes
our own outputs the same way, to `evals/output/<preset>/` followed by the
corpus-relative path, so they can be opened next to the originals and the
references. A reference
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

**CI** is two GitHub Actions workflows. `.github/workflows/checks.yml`
runs the four gates on every branch push and on pull requests from forks
(one from a branch of this repository already ran on its push):
`fmt --check`, clippy,
`kiss check` (version pinned in the workflow) and `cargo test`, which
without a corpus is the unit and probe tests.
`.github/workflows/evals.yml` does not run on ordinary pushes, since the
corpus is a 1.1 GB download; it runs when a tag is pushed and on manual
dispatch, where the subset (`full` by default) and the render check (on by
default) can be chosen. It calls the checks workflow first, then fetches
the corpus, cached under a key derived
from `evals.toml` so a pin bump refetches, and runs the evals harness in a
release build with a longer per-file timeout than the local default. The
log is uploaded as an artifact and failures are listed in the run summary.

Everything under `evals/` is gitignored and is never a build input.
Third-party material placed there for local comparison must never be linked,
loaded, or copied into the crate.

### Out of scope

Excluded by the scope rule (no preset uses them): MRC segmentation, JPEG 2000
output, LZW and RunLength output, CMYK as a conversion target, annotation and
form stripping or flattening, linearization, force-recompress.

Excluded for v1 regardless: inline images (`BI ... EI`), input that needs
a password to open, PDF/A conformance preservation. Encrypted input that
opens without a password (the usual way permission restrictions are set)
is decrypted and written unencrypted, with a report note saying the
restrictions no longer apply.

### Open decisions

- `lopdf` vs `qpdf` bindings. Starting with lopdf: pure Rust, no C++ build
  step, direct access to the object map. Known limits from the full-corpus
  run: its writer is a few percent less compact than good producers even
  with object streams packed at level 9 (the file-level never-grow rule
  absorbs this); it loads some damaged-xref files into a broken graph
  without error; it cannot load 11 of 4,368 corpus files and hangs on one;
  it decodes a stream with an empty `Filter` array to nothing, reads a
  stream without `Length` as empty, and its lenient content parser stops
  silently at a malformed token (each worked around in `pipeline` and by
  strict parsing). Revisit if repair of damaged files becomes a goal; the
  `Stage` trait would not change.
- JPX decoding uses `hayro-jpeg2000` (pure Rust), which arrives with
  `hayro-syntax` and is exercised on every image the verify step checks. It
  decoded all 51 JPX-bearing corpus files without regressions. With this the
  only C dependency is mozjpeg. Revisit if a corpus file exposes a codestream
  feature it lacks; OpenJPEG through bindings is the fallback.
- Font subsetting library: HarfBuzz through `hb-subset` (bundled, so a
  C++ compiler and libclang are needed to build). Decided after finding
  that none of the published Rust subsetters fits an existing PDF:
  `fontcull` selects by character through the font's cmap rather than by
  glyph ID; `subsetter` (Typst) renumbers glyphs, drops the cmap and
  requires the font to be rewritten as a CID font; the `klippa` crate on
  crates.io is an unrelated rectangle-clipping library. HarfBuzz retains
  glyph IDs and is the most exercised subsetter there is; with it and
  mozjpeg the crate has two C/C++ dependencies.
- HarfBuzz version: the `hb-subset` crate (0.3.0, last released in
  2023) bundles HarfBuzz 8.2.2, and no newer crate exists. Built against
  HarfBuzz 14.4 in a scratch copy, 20 more CFF and CID-keyed CFF programs
  in 12 corpus files subset (bare CFF and `CIDFontType0C` that 8.2.2
  refuses), the render check unchanged; the saving is modest because those
  programs are already subsets (a few KB each, up to 10 KB on one file).
  Getting there means vendoring a fork of the crate with the 8 MB HarfBuzz
  source tree, or a git dependency on such a fork, and 35 seconds more
  compile time. Left as is for now; revisit if reference comparisons show
  CFF-heavy documents trailing.
- CFF writing: own writer (`font/cff.rs`), since `write-fonts` 0.53 has no
  CFF v1 dict or charstring writers. It covers exactly what a converted
  Type 1 font needs: one font, no subroutines, custom encoding with
  supplements, private dict values. Revisit when write-fonts exposes CFF
  v1.

### Provisional choices

Fixed for v1 and expected to be revisited against evals results.

- Effective DPI: the minimum over placements on the tighter axis, from a
  content-stream walk.
- Flate predictor selection: try each PNG predictor per image and keep the
  smallest.
- Tiny images: images under 10,000 pixels (icons, bullets, rules) are
  neither downsampled nor re-encoded lossily. The bytes at stake are
  negligible and JPEG or a 2x reduction visibly damages a 16-pixel glyph.
  Lossless re-encoding still applies.
- Bitonal downsampling: area-average the bits to gray, then give a pixel
  the ink color (the minority color of the image) when the ink covers at
  least 30 percent of it. Mid-gray thresholding erased one-pixel features
  in a 2x reduction; this keeps them at the cost of thickening. Halftone
  fill patterns (regular dot screens at 600 dpi) alias under a 4x
  reduction either way; converting such regions to gray would be MRC-style
  segmentation, which is out of scope. What a reference does here is
  unknown.
- JBIG2 mode: generic-region coding, lossless. The available encoder's symbol
  mode substitutes glyphs (lossy) and has no refinement, so it is not used.
  Revisit when a lossless symbol mode exists or the evals show generic
  coding far behind the reference outputs on scanned text. Possible
  improvement: contribute refinement coding to `jbig2enc-rust`, whose
  decoder already reads refinement regions and whose encoder has the
  configuration fields but not the implementation; each glyph instance
  would then be its dictionary symbol plus the exact pixel difference,
  which is what makes a symbol dictionary lossless and is what optimizers
  of the reference's class ship. Whether the reference's own outputs use
  symbol coding, refinement and a shared globals stream can be read from
  their JBIG2 segment headers once they exist.
- Content stream rebuild: rewrite each content stream at the token level
  (single spaces, comments dropped, numbers without redundant digits,
  strings and names verbatim) rather than from lopdf's parsed operators,
  which hold reals as `f32` and would round coordinates. The stream must
  parse strictly before and after and yield the same operations, streams
  holding inline images are left alone, and the result is kept only when
  it compresses smaller than what is stored. Nothing else is changed.
- Unused default-resource fonts: fonts in an AcroForm's `/DR` that no
  default appearance string names are dropped when there is no XFA entry,
  as unreachable data (XFA-style forms often embed several full fonts
  there). The `/DR` is handled as one more owner in the unused-resource
  pass, so when it is the same object as a page's resources (a producer
  shortcut seen in the corpus) the page's usage and the appearance strings'
  usage are unioned and nothing either needs is lost. Whether a reference
  tool does the same is unknown; to be checked once reference outputs
  exist.
- SSIM floors: less 0.95, standard 0.93, more 0.90, at 72 dpi (pages
  under 128 px on a side are scaled up to that) with 8x8-block SSIM on
  2x2-averaged gray. Set from the corpus and probes; to be revisited
  against reference outputs.
- Color conversion scope: only color images (CMYK, and RGB or CMYK with an
  embedded profile) are converted to RGB; gray images are left in gray,
  since converting them would triple their size for no visual gain. Whether
  the reference converts gray is unknown.

## Implementation status

The crate is split into `src/lib.rs` (config, pipeline, report, stages) and
a thin `src/main.rs`. It runs end to end and re-reads its own output.
`main`, `cli`, `config`, `pipeline`, and `report` are complete for the
design above, and every module carries unit tests. `tests/evals.rs` is in
place: `cargo test --test evals` runs the `quick` subset under all three
presets (78 trials, about twenty seconds) with the four structural
invariants, plus the render check when asked. `src/verify.rs` implements the
structural level with `hayro-syntax`, with the input as baseline; `main`
refuses to write on regressions and the harness fails on them. Input that
needs a password is refused by `pipeline::run` (lopdf tries the empty
password on load; when that fails it loads nothing and keeps the trailer's
`Encrypt` entry, which is what the refusal checks). Encrypted input that
opens with the empty password was decrypted on load and is compressed and
written unencrypted, with a note, unless the file names crypt filters
lopdf could not read: lopdf reads them only when written inline, and
otherwise leaves the data encrypted without an error, so such files are
refused. Refused as well is damaged input
the parser loaded silently short: a page tree with kids it could not load,
a `Contents` or resource-category entry that refers to an object it could
not load, or a content stream without a `Length` that it read as empty
(writing such files would drop a page, an image, a font or the page's
text while the page count still added up; viewers that rebuild the
cross-reference table recover them). The harness treats these refusals as
the expected outcome. Content streams are parsed strictly everywhere
(`Content::decode_strict`): lopdf's lenient parser stops silently at a
malformed token (`-.` as a number has been seen), and the usage walk and
the resource pruning would both read a truncated operation list as "nothing
after this point is used". A stream whose `Filter` is an empty array is
normalized to no filter before the stages run, since lopdf decodes it to
nothing, and strings holding non-printable bytes are marked for hexadecimal
output, since hayro's literal-string lexer reads some of lopdf's escaped
binary literals differently (an Indexed palette rendered as gray indices
until then). The full corpus (`EVALS_SUBSET=full`, standard preset, release
build) runs in about 20 seconds without rendering and about 12 minutes
with `EVALS_RENDER=1`; `evals-expectations.toml` lists 48 files: ones lopdf
cannot load, loads into a broken graph, or hangs on, two where its loader
drops a stream with a wrong `/Length` and verification blocks the
resulting content loss, four where the render comparison flags a
difference that is not content loss (halftone patterns aliased by
downsampling, a 16-bit CMYK image the rasterizers mishandle in the input,
a font hayro cannot draw), and five that hayro cannot render within
reason (a Type 3 glyph cycle it recurses on without bound; tiling patterns
and inline-image floods that take 14 to 19 GB, which starve a parallel
corpus run even though they pass alone). Rendered pages are capped at four
million pixels so a huge media box cannot do the same. Stages:

| Stage | Status | Done when |
|---|---|---|
| usage | done: CTM and bounding-box clip walk over page content, form XObjects (with `/Matrix` and `/BBox`), tiling patterns, and annotation appearance streams; `ImageUsage` holds pixels, placements with rendered size and visible fraction, `min_dpi()` and `crop_box()`; tested for two placements, rectangular clip, `Q` restoring the clip, form matrices, and undrawn images | |
| images | second milestone: classify; decode raw/Flate/LZW samples at 1 to 16 bits in Device, ICCBased (by N), CalRGB/CalGray and Indexed spaces with any `Decode` array; Separation, DeviceN and Lab samples mapped into their device alternate through PDF functions of types 0, 2, 3 and 4 (own evaluator with a PostScript calculator, memoized per distinct sample tuple), with the dictionary rewritten to the alternate space, and indexed images over such a base get their palette mapped the same way; indirect `Filter` entries resolved; decode DCT (Gray, RGB, CMYK, YCCK) including Flate-wrapped JPEGs, honoring the `ColorTransform` decode parameter when the codestream has no Adobe marker; decode CCITT (all K modes, `BlackIs1`, indirect `DecodeParms`), embedded JBIG2 with globals, and JPX (codestream color space and depth win; alpha channels, and indexed or mapped dictionary spaces, skipped) via hayro's decoders; CMYK rasters resized with alpha handling off, since the resizer would otherwise premultiply by the K channel; downsample per class rule (Lanczos3; nearest for indices; bitonal by area averaging then a threshold toward the ink color at 30 percent coverage, so thin lines thicken rather than vanish); encode Flate with per-row PNG predictor choice, JPEG (Gray, RGB, CMYK) via mozjpeg, CCITT G4 via `fax`, JBIG2 generic region via `jbig2enc-rust`; unwrapped-JPEG passthrough candidate; color complexity reduction (flat images to one pixel, gray RGB/CMYK to gray, two-level gray to bitonal; opaque soft masks removed, two-level soft masks turned into stencil masks); color conversion to RGB for the `more` preset (embedded ICC profiles through moxcms, DeviceCMYK through the Neugebauer model; gray stays gray); best-of with never-grow; soft and stencil masks resized with their parent; clipping to the crop box from `usage`, applied only when the invisible fraction of the source bytes outweighs the wrapper form, with the cropped image placed behind a form XObject that keeps every existing placement valid and masks cropped alongside (`Matte` masks refuse); per-image report rows | Remaining: JPX and CCITT/JBIG2 data in mapped spaces; JBIG2 symbol mode with shared globals once a lossless symbol encoder is available. |
| fonts | third milestone: every embedded program gets a report row; Type 1 programs converted to CFF (`Type1C`) under the never-grow rule, then subset like any CFF; subsetting of TrueType, CFF, CIDFontType0C and OpenType programs with glyph IDs retained (HarfBuzz), driven by the strings the usage walk recorded, with the union of glyphs over every font dictionary sharing a program (a TrueType or OpenType program is normalized before anything reads it, without changing any table's content: directory sorted by tag with records pointing outside the file dropped, `head` version set to 1.0, `maxp` glyph count capped to what `loca` holds; each of these was seen in the corpus and each makes HarfBuzz and read-fonts reject the whole font; for an OpenType program with CFF outlines under a CID font, both the `CIDToGIDMap` and the CFF charset readings are kept, since the subtype is not consulted; embedded CMaps may use `bfchar` and `bfrange`, read as CIDs); programs of fonts a default appearance string names in the AcroForm default resources, of fonts in Type 3 resources, and of fonts the walk never saw, are left alone; hints are removed unless read-fonts finds a stem hint after a path operator in any charstring, which HarfBuzz's removal mishandles; the program stream is rewritten Flate-compressed under the never-grow rule and untagged fonts get a subset tag derived from the glyph set; standard-14 unembedding for simple fonts (name aliases folded to the canonical 14, encoding must stand on its own: a standard encoding name, a differences dictionary with known glyph names, or a non-symbolic descriptor; Symbol and ZapfDingbats only with their built-in encoding), renaming font and descriptor to the canonical name; the usage stage records the strings shown with each font | Only Type 1 programs whose conversion fails stay unconverted; those are reported. Subsetting still fails on 13 corpus programs: CFF programs the bundled HarfBuzz 8.2.2 refuses (see open decisions), TrueType programs whose `glyf` is empty (nothing to gain; fonts used to hide OCR text look like this, so they are left whole rather than given a substitute), and three single-file oddities. |
| strip | done: every flag removes the keys in the mapping table; catalog keys on the catalog, the rest on any object | |
| structure | done: content streams (page contents, forms, patterns, Type 3 glyph procedures) rewritten in canonical token form and re-compressed when smaller, under the never-grow rule per stream; unused resource entries removed (pages, form XObjects, tiling patterns, Type 3 fonts; owners whose content does not decode or parse strictly, inherited resources, and owners whose resources list a Type 3 font, form XObject or pattern without resources of its own, which draws with the owner's, are left alone), streams compressed, duplicate objects merged by canonical form, unreferenced objects pruned, the AcroForm default resources treated as an owner whose content is the set of default appearance strings (fonts only; other categories kept whole; left alone entirely with an XFA entry), references to missing objects removed so renumbering cannot rebind them, renumbered, version raised for JBIG2 | |

A second-pass check ran the standard preset with render verification over
1,856 documents (2.9 GB, papers, books and articles) that a Ghostscript-based
tool had already compressed: every file completed, none was refused, no
page fell below the floor, and the files shrank a further 18 percent
overall (papers 25, books 14, articles 55 percent). The render check
found three defects on the way that the structural check cannot see,
each fixed: HarfBuzz's hint removal cutting outlines in Ghostscript's
charstrings, the strip stage removing a font resource named `/B`, and
hayro misreading lopdf's escaped binary literals.

Implementation order was structure, strip, usage, then images, then fonts.
This differs from pipeline order on purpose: structure and strip are cheap
and verify the harness; usage must exist before images can downsample or
clip safely.

Present: `evals.toml` with the three sources pinned, and `examples/evals.rs`
behind the `cargo evals` alias with `fetch` (shallow sparse clones, pdf.js
link resolution with recorded failures, manifest) and `status` working;
`probes` writes the synthetic documents; `score` runs a subset through
the pipeline in-process and prints, per preset, one row per file with
input size, our size and ratio, and one column per reference directory
with its size and ratio, then totals (a reference's total over the files
it covers, with our total on the same files and the coverage count).
No reference outputs exist yet. A full
fetch takes about five minutes and 1.1 GB; re-running retries only failed
links. `evals.toml` defines the `quick` subset (26 files, 4.7 MB, every
handled feature at least twice plus seven realistic documents) and the
`scoring` subset (34 py-pdf files, all but the password-protected one,
plus eight realistic pdf.js documents: papers with Type 1 and TrueType
fonts, a slide deck, scans with JPX and CCITT, a CID-keyed CFF; about 28
MB); `status` reports each subset's presence on disk.

Present: `tests/probes.rs` with its generators in
`tests/probes/generators.rs` (eighteen one-variable documents: CMYK, gray
RGB, bitonal, indexed, JPEG, clipped, opaque soft mask, Lab, duplicate and
unused resources, a partly used Type 1 font, an embedded Arial, metadata
and thumbnail, AcroForm default resources with and without XFA and shared
with the page's resources, a verbose content stream, an inline image), each
asserted against the presets' stated behavior and all run through every
preset with the structural verifier; `cargo evals probes <dir>` writes the
same files with a README listing them.

The visual level of `verify` is present: `verify::render` rasterizes
every page of input and output with `hayro` (with its bundled standard
fonts and CMaps, so unembedded text still renders) at 72 dpi and scores
each page with block SSIM on gray; `--verify render` reports it, `--strict`
fails on a page under the preset's floor, `EVALS_RENDER=1` applies it in
the corpus harness, the probe tests apply it to every probe under every
preset, and `score` prints the minimum page SSIM per file unless
`--no-render`. Floors are provisional (less 0.95, standard 0.93, more
0.90).
