//! The only crate that talks to Windows.
//!
//! Every rule the Python implementation learned the hard way lives here, each with the test that
//! proves it. The list is in docs/superpowers/specs/2026-10-06-rust-electron-rewrite-design.md and
//! it is the actual deliverable: the code is only where the rules are written down.

pub mod audio;
pub mod capture;
pub mod display;
pub mod hotkeys;
pub mod input;
pub mod ocr;
pub mod speech;
pub mod stt;
pub mod uia;
pub mod winlist;
