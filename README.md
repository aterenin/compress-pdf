# compress-pdf

This is a Rust-based command line tool which tries to [do one thing, and do it well](https://en.wikipedia.org/wiki/Unix_philosophy): compress PDFs to reduce file size while maintaining content, using a large array of techniques for images, fonts, metadata, and other 

This code is 100% AI-generated: this file is the only human-written one in the codebase.
The high-level compression pipeline described in [CLAUDE.md](/CLAUDE.md) is human-codesigned to ensure the code's overall structure is sane and makes sense.
We use [KISS](https://github.com/dsweet99/kiss) as a linter to guard against bad patterns that AI has a tendency to use.
Correctness is verified not by reading the code, but  by actually running it on a large array of reference PDFs, and checking that no content is lost, while file size is smaller than a reference implementation provided by [ILovePDF](https://ilovepdf.com).
