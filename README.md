# Compress PDF

This is a Rust-based command line tool which tries to [do one thing, and do it well](https://en.wikipedia.org/wiki/Unix_philosophy): compress PDFs to reduce file size while maintaining content, using a large array of techniques for images, fonts, metadata, and other components whose size can be reduced.

**Development style.** This code is built using evaluation-driven development, and is 100% AI-generated: in particular, this file is the only human-written one in the codebase.
The high-level compression pipeline is described in [CLAUDE.md](/CLAUDE.md), and is human-codesigned to ensure the code's overall structure is sane and makes sense.
We use [KISS](https://github.com/dsweet99/kiss) as a linter to guard against bad patterns that AI systems sometime have a tendency to use.

**Verification.** Correctness is verified by running the code it on 4368 reference PDFs originating from various open-source PDF library test suites, which are downloaded on-demand by the evaluation code: we check that no content is lost by rendering the original and compressed PDFs and comparing the resulting images, and that file size is reduced in comparison to a reference implementation provided by [ILovePDF](https://ilovepdf.com).