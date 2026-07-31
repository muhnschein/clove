//! Terminal primitives for `clove(1)`.
//!
//! A library only so that [`term`]'s escape-sequence decoder can carry a fuzz
//! target like every other parser in this project (`fuzz/README.md`). The
//! commands, the renderers and everything else live in the binary.

pub mod term;
