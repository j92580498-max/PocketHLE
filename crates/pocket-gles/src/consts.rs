//! OpenGL ES 1.1 and EGL 1.0 enumerant values.
//!
//! These are the numeric constants defined by the Khronos headers
//! (`GLES/gl.h`, `EGL/egl.h`). A guest passes them in as plain integers,
//! so the values must match the specification exactly — they are part of
//! the ABI, not an implementation choice.

// ---- error codes --------------------------------------------------------
pub const GL_NO_ERROR: u32 = 0;
pub const GL_INVALID_ENUM: u32 = 0x0500;
pub const GL_INVALID_VALUE: u32 = 0x0501;
pub const GL_INVALID_OPERATION: u32 = 0x0502;
pub const GL_STACK_OVERFLOW: u32 = 0x0503;
pub const GL_STACK_UNDERFLOW: u32 = 0x0504;
pub const GL_OUT_OF_MEMORY: u32 = 0x0505;

// ---- booleans -----------------------------------------------------------
pub const GL_FALSE: u32 = 0;
pub const GL_TRUE: u32 = 1;

// ---- matrix modes -------------------------------------------------------
pub const GL_MODELVIEW: u32 = 0x1700;
pub const GL_PROJECTION: u32 = 0x1701;
pub const GL_TEXTURE: u32 = 0x1702;

// ---- primitive types ----------------------------------------------------
pub const GL_POINTS: u32 = 0x0000;
pub const GL_LINES: u32 = 0x0001;
pub const GL_LINE_LOOP: u32 = 0x0002;
pub const GL_LINE_STRIP: u32 = 0x0003;
pub const GL_TRIANGLES: u32 = 0x0004;
pub const GL_TRIANGLE_STRIP: u32 = 0x0005;
pub const GL_TRIANGLE_FAN: u32 = 0x0006;

// ---- data types ---------------------------------------------------------
pub const GL_BYTE: u32 = 0x1400;
pub const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub const GL_SHORT: u32 = 0x1402;
pub const GL_UNSIGNED_SHORT: u32 = 0x1403;
pub const GL_FLOAT: u32 = 0x1406;
pub const GL_FIXED: u32 = 0x140C;

// ---- capabilities (glEnable / glDisable) --------------------------------
pub const GL_CULL_FACE: u32 = 0x0B44;
pub const GL_DEPTH_TEST: u32 = 0x0B71;
pub const GL_STENCIL_TEST: u32 = 0x0B90;
pub const GL_BLEND: u32 = 0x0BE2;
pub const GL_DITHER: u32 = 0x0BD0;
pub const GL_SCISSOR_TEST: u32 = 0x0C11;
pub const GL_ALPHA_TEST: u32 = 0x0BC0;
pub const GL_FOG: u32 = 0x0B60;
pub const GL_LIGHTING: u32 = 0x0B50;
pub const GL_TEXTURE_2D: u32 = 0x0DE1;
pub const GL_NORMALIZE: u32 = 0x0BA1;
pub const GL_COLOR_MATERIAL: u32 = 0x0B57;
pub const GL_POLYGON_OFFSET_FILL: u32 = 0x8037;

// ---- client-state arrays ------------------------------------------------
pub const GL_VERTEX_ARRAY: u32 = 0x8074;
pub const GL_NORMAL_ARRAY: u32 = 0x8075;
pub const GL_COLOR_ARRAY: u32 = 0x8076;
pub const GL_TEXTURE_COORD_ARRAY: u32 = 0x8078;

// ---- buffer bits (glClear) ---------------------------------------------
pub const GL_DEPTH_BUFFER_BIT: u32 = 0x0000_0100;
pub const GL_STENCIL_BUFFER_BIT: u32 = 0x0000_0400;
pub const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;

// ---- pixel formats and types -------------------------------------------
pub const GL_ALPHA: u32 = 0x1906;
pub const GL_RGB: u32 = 0x1907;
pub const GL_RGBA: u32 = 0x1908;
pub const GL_LUMINANCE: u32 = 0x1909;
pub const GL_LUMINANCE_ALPHA: u32 = 0x190A;
pub const GL_UNSIGNED_SHORT_4_4_4_4: u32 = 0x8033;
pub const GL_UNSIGNED_SHORT_5_5_5_1: u32 = 0x8034;
pub const GL_UNSIGNED_SHORT_5_6_5: u32 = 0x8363;

// ---- texture parameters -------------------------------------------------
pub const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
pub const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
pub const GL_TEXTURE_WRAP_S: u32 = 0x2802;
pub const GL_TEXTURE_WRAP_T: u32 = 0x2803;
pub const GL_NEAREST: u32 = 0x2600;
pub const GL_LINEAR: u32 = 0x2601;
pub const GL_NEAREST_MIPMAP_NEAREST: u32 = 0x2700;
pub const GL_LINEAR_MIPMAP_NEAREST: u32 = 0x2701;
pub const GL_NEAREST_MIPMAP_LINEAR: u32 = 0x2702;
pub const GL_LINEAR_MIPMAP_LINEAR: u32 = 0x2703;
pub const GL_REPEAT: u32 = 0x2901;
pub const GL_CLAMP_TO_EDGE: u32 = 0x812F;

// ---- glGetString names --------------------------------------------------
pub const GL_VENDOR: u32 = 0x1F00;
pub const GL_RENDERER: u32 = 0x1F01;
pub const GL_VERSION: u32 = 0x1F02;
pub const GL_EXTENSIONS: u32 = 0x1F03;

// ---- glGetIntegerv queries ---------------------------------------------
pub const GL_MAX_TEXTURE_SIZE: u32 = 0x0D33;
pub const GL_MAX_LIGHTS: u32 = 0x0D31;
pub const GL_MAX_TEXTURE_UNITS: u32 = 0x84E2;
pub const GL_MAX_MODELVIEW_STACK_DEPTH: u32 = 0x0D36;
pub const GL_MAX_PROJECTION_STACK_DEPTH: u32 = 0x0D38;
pub const GL_MAX_TEXTURE_STACK_DEPTH: u32 = 0x0D39;
pub const GL_DEPTH_BITS: u32 = 0x0D56;
pub const GL_STENCIL_BITS: u32 = 0x0D57;
pub const GL_RED_BITS: u32 = 0x0D52;
pub const GL_GREEN_BITS: u32 = 0x0D53;
pub const GL_BLUE_BITS: u32 = 0x0D54;
pub const GL_ALPHA_BITS: u32 = 0x0D55;
pub const GL_VIEWPORT: u32 = 0x0BA2;
pub const GL_COMPRESSED_TEXTURE_FORMATS: u32 = 0x86A3;
pub const GL_NUM_COMPRESSED_TEXTURE_FORMATS: u32 = 0x86A2;

// ---- OES compressed paletted texture formats ----------------------------
pub const GL_PALETTE4_RGB8_OES: u32 = 0x8B90;
pub const GL_PALETTE4_RGBA8_OES: u32 = 0x8B91;
pub const GL_PALETTE4_R5_G6_B5_OES: u32 = 0x8B92;
pub const GL_PALETTE4_RGBA4_OES: u32 = 0x8B93;
pub const GL_PALETTE4_RGB5_A1_OES: u32 = 0x8B94;
pub const GL_PALETTE8_RGB8_OES: u32 = 0x8B95;
pub const GL_PALETTE8_RGBA8_OES: u32 = 0x8B96;
pub const GL_PALETTE8_R5_G6_B5_OES: u32 = 0x8B97;
pub const GL_PALETTE8_RGBA4_OES: u32 = 0x8B98;
pub const GL_PALETTE8_RGB5_A1_OES: u32 = 0x8B99;

// ---- ATITC (AMD ATC) compressed texture formats ------------------------
//
// `GL_AMD_compressed_ATC_texture`. Qualcomm's Adreno was the dominant
// GPU in Windows Mobile handsets, so titles that ship compressed art
// ship it as ATC: Xtrakt uploads its entire atlas set this way and
// checks `glGetError` afterwards, so a driver that rejects the format
// stops the game rather than merely losing the texture.
pub const GL_ATC_RGB_AMD: u32 = 0x8C92;
pub const GL_ATC_RGBA_EXPLICIT_ALPHA_AMD: u32 = 0x8C93;
pub const GL_ATC_RGBA_INTERPOLATED_ALPHA_AMD: u32 = 0x87EE;
pub const GL_COMPRESSED_RGB_S3TC_DXT1_EXT: u32 = 0x83F0;
pub const GL_COMPRESSED_RGBA_S3TC_DXT1_EXT: u32 = 0x83F1;
pub const GL_MODELVIEW_MATRIX: u32 = 0x0BA6;

pub const GL_PROJECTION_MATRIX: u32 = 0x0BA7;
pub const GL_TEXTURE_MATRIX: u32 = 0x0BA8;
pub const GL_CURRENT_COLOR: u32 = 0x0B00;
pub const GL_DEPTH_RANGE: u32 = 0x0B70;

// ---- buffer objects ----------------------------------------------------
pub const GL_ARRAY_BUFFER: u32 = 0x8892;
pub const GL_ELEMENT_ARRAY_BUFFER: u32 = 0x8893;
pub const GL_ARRAY_BUFFER_BINDING: u32 = 0x8894;
pub const GL_ELEMENT_ARRAY_BUFFER_BINDING: u32 = 0x8895;
pub const GL_STATIC_DRAW: u32 = 0x88E4;
pub const GL_DYNAMIC_DRAW: u32 = 0x88E8;
pub const GL_BUFFER_SIZE: u32 = 0x8764;
pub const GL_BUFFER_USAGE: u32 = 0x8765;

// ---- depth / alpha comparison functions --------------------------------
pub const GL_NEVER: u32 = 0x0200;
pub const GL_LESS: u32 = 0x0201;
pub const GL_EQUAL: u32 = 0x0202;
pub const GL_LEQUAL: u32 = 0x0203;
pub const GL_GREATER: u32 = 0x0204;
pub const GL_NOTEQUAL: u32 = 0x0205;
pub const GL_GEQUAL: u32 = 0x0206;
pub const GL_ALWAYS: u32 = 0x0207;

// ---- blend factors ------------------------------------------------------
pub const GL_ZERO: u32 = 0;
pub const GL_ONE: u32 = 1;
pub const GL_SRC_COLOR: u32 = 0x0300;
pub const GL_ONE_MINUS_SRC_COLOR: u32 = 0x0301;
pub const GL_SRC_ALPHA: u32 = 0x0302;
pub const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
pub const GL_DST_ALPHA: u32 = 0x0304;
pub const GL_ONE_MINUS_DST_ALPHA: u32 = 0x0305;
pub const GL_DST_COLOR: u32 = 0x0306;
pub const GL_ONE_MINUS_DST_COLOR: u32 = 0x0307;
pub const GL_SRC_ALPHA_SATURATE: u32 = 0x0308;

// ---- face culling / winding --------------------------------------------
pub const GL_FRONT: u32 = 0x0404;
pub const GL_BACK: u32 = 0x0405;
pub const GL_FRONT_AND_BACK: u32 = 0x0408;
pub const GL_CW: u32 = 0x0900;
pub const GL_CCW: u32 = 0x0901;

// ---- shading model ------------------------------------------------------
pub const GL_FLAT: u32 = 0x1D00;
pub const GL_SMOOTH: u32 = 0x1D01;

// ---- texture environment ------------------------------------------------
pub const GL_TEXTURE_ENV: u32 = 0x2300;
pub const GL_TEXTURE_ENV_MODE: u32 = 0x2200;
pub const GL_TEXTURE_ENV_COLOR: u32 = 0x2201;
pub const GL_MODULATE: u32 = 0x2100;
pub const GL_DECAL: u32 = 0x2101;
pub const GL_ADD: u32 = 0x0104;
pub const GL_REPLACE: u32 = 0x1E01;
pub const GL_COMBINE: u32 = 0x8570;

// ---- fog ----------------------------------------------------------------
pub const GL_FOG_MODE: u32 = 0x0B65;
pub const GL_FOG_DENSITY: u32 = 0x0B62;
pub const GL_FOG_START: u32 = 0x0B63;
pub const GL_FOG_END: u32 = 0x0B64;
pub const GL_FOG_COLOR: u32 = 0x0B66;
pub const GL_EXP: u32 = 0x0800;
pub const GL_EXP2: u32 = 0x0801;
pub const GL_LINEAR_FOG: u32 = 0x2601; // same value as GL_LINEAR

// ---- hints --------------------------------------------------------------
pub const GL_DONT_CARE: u32 = 0x1100;
pub const GL_FASTEST: u32 = 0x1101;
pub const GL_NICEST: u32 = 0x1102;
pub const GL_PERSPECTIVE_CORRECTION_HINT: u32 = 0x0C50;
pub const GL_FOG_HINT: u32 = 0x0C54;

// ---- texture units ------------------------------------------------------
pub const GL_TEXTURE0: u32 = 0x84C0;

// ---- pixel store --------------------------------------------------------
pub const GL_PACK_ALIGNMENT: u32 = 0x0D05;
pub const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;

// =========================================================================
// EGL 1.0
// =========================================================================

// ---- boolean / handles --------------------------------------------------
pub const EGL_FALSE: u32 = 0;
pub const EGL_TRUE: u32 = 1;
pub const EGL_NO_SURFACE: u32 = 0;
pub const EGL_NO_CONTEXT: u32 = 0;
pub const EGL_NO_DISPLAY: u32 = 0;
pub const EGL_DEFAULT_DISPLAY: u32 = 0;

// ---- error codes --------------------------------------------------------
pub const EGL_SUCCESS: u32 = 0x3000;
pub const EGL_NOT_INITIALIZED: u32 = 0x3001;
pub const EGL_BAD_ACCESS: u32 = 0x3002;
pub const EGL_BAD_ALLOC: u32 = 0x3003;
pub const EGL_BAD_ATTRIBUTE: u32 = 0x3004;
pub const EGL_BAD_CONFIG: u32 = 0x3005;
pub const EGL_BAD_CONTEXT: u32 = 0x3006;
pub const EGL_BAD_CURRENT_SURFACE: u32 = 0x3007;
pub const EGL_BAD_DISPLAY: u32 = 0x3008;
pub const EGL_BAD_MATCH: u32 = 0x3009;
pub const EGL_BAD_NATIVE_PIXMAP: u32 = 0x300A;
pub const EGL_BAD_NATIVE_WINDOW: u32 = 0x300B;
pub const EGL_BAD_PARAMETER: u32 = 0x300C;
pub const EGL_BAD_SURFACE: u32 = 0x300D;

// ---- config attributes --------------------------------------------------
pub const EGL_BUFFER_SIZE: u32 = 0x3020;
pub const EGL_ALPHA_SIZE: u32 = 0x3021;
pub const EGL_BLUE_SIZE: u32 = 0x3022;
pub const EGL_GREEN_SIZE: u32 = 0x3023;
pub const EGL_RED_SIZE: u32 = 0x3024;
pub const EGL_DEPTH_SIZE: u32 = 0x3025;
pub const EGL_STENCIL_SIZE: u32 = 0x3026;
pub const EGL_CONFIG_CAVEAT: u32 = 0x3027;
pub const EGL_CONFIG_ID: u32 = 0x3028;
pub const EGL_LEVEL: u32 = 0x3029;
pub const EGL_MAX_PBUFFER_HEIGHT: u32 = 0x302A;
pub const EGL_MAX_PBUFFER_PIXELS: u32 = 0x302B;
pub const EGL_MAX_PBUFFER_WIDTH: u32 = 0x302C;
pub const EGL_NATIVE_RENDERABLE: u32 = 0x302D;
pub const EGL_NATIVE_VISUAL_ID: u32 = 0x302E;
pub const EGL_NATIVE_VISUAL_TYPE: u32 = 0x302F;
pub const EGL_SAMPLES: u32 = 0x3031;
pub const EGL_SAMPLE_BUFFERS: u32 = 0x3032;
pub const EGL_SURFACE_TYPE: u32 = 0x3033;
pub const EGL_TRANSPARENT_TYPE: u32 = 0x3034;
pub const EGL_NONE: u32 = 0x3038;
pub const EGL_RENDERABLE_TYPE: u32 = 0x3040;
pub const EGL_CONFIG_COUNT_QUERY: u32 = 0x3200;

// ---- surface attributes -------------------------------------------------
pub const EGL_HEIGHT: u32 = 0x3056;
pub const EGL_WIDTH: u32 = 0x3057;
pub const EGL_LARGEST_PBUFFER: u32 = 0x3058;

// ---- surface type bits --------------------------------------------------
pub const EGL_PBUFFER_BIT: u32 = 0x0001;
pub const EGL_PIXMAP_BIT: u32 = 0x0002;
pub const EGL_WINDOW_BIT: u32 = 0x0004;

// ---- eglQueryString names ----------------------------------------------
pub const EGL_VENDOR: u32 = 0x3053;
pub const EGL_VERSION: u32 = 0x3054;
pub const EGL_EXTENSIONS: u32 = 0x3055;

// ---- draw/read selectors -----------------------------------------------
pub const EGL_DRAW: u32 = 0x3059;
pub const EGL_READ: u32 = 0x305A;
