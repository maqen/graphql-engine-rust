//! YAML output byte-for-byte like the official CLI (go-yaml v3 with CUE's quoting).

pub mod emitter;
pub mod node;
pub mod resolve;

pub use node::Node;
