//! Clean-room OpenGL ES 1.x / EGL 1.0 implementation for PocketHLE.
//!
//! This crate provides the entry points that `libGLES_CM.dll` and
//! `libGLES_CL.dll` export. Windows Mobile games link these libraries
//! by ordinal only, so the ordinal tables in `data/` are the ABI
//! contract — the names exist for human debugging, not for the guest.
//!
//! The implementation strategy is software rasterization on the host.
//! This keeps the code simple, portable, and testable: no host GPU
//! context, no shader translation, no driver quirks. A future OpenGL 2.1
//! or 3.3 backend can reuse the same dispatch layer by swapping the
//! state machine for one that builds vertex buffers and issues host GL
//! calls.

#![allow(clippy::chunks_exact_to_as_chunks)]

pub mod consts;
pub mod context;
pub mod fixed;
pub mod matrix;
pub mod ordinals;
pub mod raster;
pub mod texture;

pub use consts::*;
pub use fixed::{to_f32 as fixed_to_f32, word_to_f32, word_to_f32_bits};
pub use matrix::{Matrix4, MatrixMode, MatrixStack, IDENTITY};
pub use ordinals::{entry_count, is_gles_dll, lookup, names_for};
pub use texture::Texture;
