# Compress PDF

This is a Rust-based command line tool which tries to [do one thing, and do it well](https://en.wikipedia.org/wiki/Unix_philosophy): compress PDFs to reduce file size while maintaining content, using a large array of techniques for images, fonts, metadata, and other components whose size can be reduced.

**Installation.** 
Run `cargo install compress-pdf`.
Binaries for macOS and Linux will be made available when this package becomes a bit more mature.
On macOS and Linux, please ensure that you have the standard C/C++ toolchain and appropriate libraries installed, so that the crates we depend on compile correctly.
Windows should work but is currently untested, please open a GitHub issue if you encounter any issues.

**Usage.** 
To use this package, run `compress-pdf file_to_compress.pdf`. For more options, including presets and to configure various details regarding how compression takes place, use `compress-pdf --help`.

**Development style.** 
This code is built using evaluation-driven development, and is 100% AI-generated: in particular, this file is the only human-written one in the codebase.
The high-level compression pipeline is described in [CLAUDE.md](CLAUDE.md), and is human-codesigned to ensure the code's overall structure is sane and makes sense.
We use [KISS](https://github.com/dsweet99/kiss) as a linter to guard against bad patterns that AI systems sometime have a tendency to use.

**Verification.**
Correctness is verified by running the code on approximately 4400 reference PDFs originating from various open-source PDF library test suites, which are downloaded on-demand by the evaluation code. 
We check that no visible content is lost by rendering the original and compressed PDFs and comparing the resulting images, and that file size is reduced in comparison to a reference implementation provided by [ILovePDF](https://ilovepdf.com).
Due to the latter being a commercial implementation which prohibits programmatic use in its terms of service, comparisons were performed entirely by hand, on a small representative corpus assembled by the package author, which cannot be shared due to copyright reasons.
Performance, in most cases, was found to be competitive.
If you find a PDF on which we perform badly, please submit a GitHub issue.

**Documentation.**
This package does not have traditional documentation.
It is designed primarily for command-line use, which is documented by running `compress-pdf --help`.
The docs page include only auto-generated content and this readme.
The package should also be suitable for programmatic use: for this, we recommend you ask your AI agent to look at `main.rs` and `pipeline.rs`.
The code is structured in a manner where it should not be difficult to figure out how it works.
If you have a use case for which the APIs are poorly suited, please submit a GitHub issue.