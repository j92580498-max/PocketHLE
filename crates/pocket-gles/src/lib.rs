//! Clean-room OpenGL ES 1.x / EGL 1.0 implementation for PocketHLE.
//!
//! This crate provides the entry points that `libGLES_CM.dll` and
//! `libGLES_CL.dll` export. Windows Mobile games link these libraries
//! by ordinal only, so the ordinal tables in `data/` are the ABI
//! contract — the names exist for human debugging, not for the guest.
//!
//! Software rasterization is the default. The experimental host OpenGL
//! backend keeps guest state and vertex decoding here, then runs texture,
//! fragment, depth and blending operations in an offscreen native context.
//! Desktop uses WGL/OpenGL; Android uses EGL/OpenGL ES 2 shaders. Readback
//! preserves the existing framebuffer presentation and capture contracts.

#![allow(clippy::chunks_exact_to_as_chunks)]

pub mod consts;
pub mod context;
pub mod fixed;
pub mod gpu;
pub mod lighting;
pub mod matrix;
pub mod ordinals;
pub mod raster;
pub mod texture;

pub use consts::*;
pub use fixed::{to_f32 as fixed_to_f32, word_to_f32, word_to_f32_bits};
pub use matrix::{Matrix4, MatrixMode, MatrixStack, IDENTITY};
pub use ordinals::{entry_count, is_gles_dll, lookup, names_for};
pub use texture::Texture;
