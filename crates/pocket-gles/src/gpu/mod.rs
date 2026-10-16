//! OpenGL 2.1 / GLES 2 offscreen rasterizer. Guest pointers never reach a driver.
mod platform;
use crate::{
    consts::*,
    matrix::{Matrix4, IDENTITY},
    raster::{
        BlendFactor, CompareFunc, CullMode, FogMode, FrontFace, PipelineState, RenderTarget,
        TexEnvMode, TextureStage, Vertex,
    },
    texture::{Filter, Texture, Wrap},
};
use platform::Host;
use std::{
    collections::{HashMap, HashSet},
    ffi::{c_char, c_void, CStr, CString},
};
pub struct VertexTransforms {
    pub lighting: crate::lighting::LightingState,
    pub normal_matrix: Matrix4,
    pub projection: Matrix4,
    pub modelview: Matrix4,
    pub textures: [Matrix4; 4],
}
/// Host-owned snapshots only. Guest addresses never reach the native driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamAttribute {
    pub size: i32,
    pub ty: u32,
    pub normalized: u8,
    pub stride: i32,
    pub offset: usize,
}
#[derive(Default)]
pub(crate) struct VertexStream {
    pub bytes: Vec<u8>,
    pub attributes: [Option<StreamAttribute>; 8],
    pub constants: [[f32; 4]; 8],
    pub count: usize,
    pub indices: Vec<u16>,
    pub coverage: Vec<u8>,
}

/// Per-draw attribute layout. Disabled stages carry no payload. Smooth
/// lighting aliases the position attribute instead of uploading it twice.
#[derive(Clone, Copy, PartialEq, Eq)]
struct VertexLayout {
    attributes: [Option<(i32, usize)>; 8],
    floats: usize,
}
impl VertexLayout {
    fn new(
        state: &PipelineState,
        stages: &[TextureStage],
        transforms: Option<&VertexTransforms>,
    ) -> Self {
        let lighting = transforms.is_some_and(|t| t.lighting.enabled);
        let mut layout = Self {
            attributes: [None; 8],
            floats: 8,
        };
        layout.attributes[0] = Some((4, 0));
        layout.attributes[1] = Some((4, 4));
        for stage in stages {
            layout.attributes[stage.unit + 2] = Some((2, layout.floats));
            layout.floats += 2;
        }
        if lighting || (transforms.is_none() && state.fog) {
            layout.attributes[6] = Some((4, layout.floats));
            layout.floats += 4;
        }
        if lighting {
            if state.smooth_shading {
                layout.attributes[7] = Some((4, 0));
            } else {
                layout.attributes[7] = Some((4, layout.floats));
                layout.floats += 4;
            }
        }
        layout
    }
    fn pack(&self, out: &mut Vec<f32>, vertex: Vertex, provoking: Vertex, smooth: bool) {
        out.extend(vertex.pos);
        out.extend(if smooth {
            vertex.color
        } else {
            provoking.color
        });
        for unit in 0..4 {
            if self.attributes[unit + 2].is_some() {
                out.extend(vertex.texcoord[unit]);
            }
        }
        if self.attributes[6].is_some() {
            out.extend(if smooth {
                vertex.normal
            } else {
                provoking.normal
            });
            out.push(vertex.fog_depth);
        }
        if self.attributes[7].is_some_and(|(_, offset)| offset != 0) {
            out.extend(provoking.pos);
        }
    }
}
const TEXTURE_MATRICES: [&str; 4] = [
    "textureMatrix0",
    "textureMatrix1",
    "textureMatrix2",
    "textureMatrix3",
];
const SAMPLERS: [&str; 4] = ["sampler0", "sampler1", "sampler2", "sampler3"];
const ENVIRONMENTS: [&str; 4] = ["env0", "env1", "env2", "env3"];
const LOD_LIMITS: [&str; 4] = ["lodLimit0", "lodLimit1", "lodLimit2", "lodLimit3"];
const LOD_BIASES: [&str; 4] = ["lodBias0", "lodBias1", "lodBias2", "lodBias3"];
const ENV_COLORS: [&str; 4] = ["envColor0", "envColor1", "envColor2", "envColor3"];
#[allow(non_snake_case)]
struct Gl {
    CompressedTexImage2D:
        Option<unsafe extern "system" fn(u32, i32, u32, i32, i32, i32, i32, *const c_void)>,
    GenerateMipmap: Option<unsafe extern "system" fn(u32)>,
    GetString: unsafe extern "system" fn(u32) -> *const u8,
    GetError: unsafe extern "system" fn() -> u32,
    GetIntegerv: unsafe extern "system" fn(u32, *mut i32),
    GetFloatv: unsafe extern "system" fn(u32, *mut f32),
    LineWidth: unsafe extern "system" fn(f32),
    CreateShader: unsafe extern "system" fn(u32) -> u32,
    ShaderSource: unsafe extern "system" fn(u32, i32, *const *const c_char, *const i32),
    CompileShader: unsafe extern "system" fn(u32),
    GetShaderiv: unsafe extern "system" fn(u32, u32, *mut i32),
    GetShaderInfoLog: unsafe extern "system" fn(u32, i32, *mut i32, *mut c_char),
    DeleteShader: unsafe extern "system" fn(u32),
    CreateProgram: unsafe extern "system" fn() -> u32,
    AttachShader: unsafe extern "system" fn(u32, u32),
    BindAttribLocation: unsafe extern "system" fn(u32, u32, *const c_char),
    LinkProgram: unsafe extern "system" fn(u32),
    GetProgramiv: unsafe extern "system" fn(u32, u32, *mut i32),
    GetProgramInfoLog: unsafe extern "system" fn(u32, i32, *mut i32, *mut c_char),
    DeleteProgram: unsafe extern "system" fn(u32),
    UseProgram: unsafe extern "system" fn(u32),
    GetUniformLocation: unsafe extern "system" fn(u32, *const c_char) -> i32,
    Uniform1i: unsafe extern "system" fn(i32, i32),
    Uniform1f: unsafe extern "system" fn(i32, f32),
    Uniform2f: unsafe extern "system" fn(i32, f32, f32),
    UniformMatrix4fv: unsafe extern "system" fn(i32, i32, u8, *const f32),
    Uniform4fv: unsafe extern "system" fn(i32, i32, *const f32),
    GenBuffers: unsafe extern "system" fn(i32, *mut u32),
    DeleteBuffers: unsafe extern "system" fn(i32, *const u32),
    BindBuffer: unsafe extern "system" fn(u32, u32),
    BufferData: unsafe extern "system" fn(u32, isize, *const c_void, u32),
    DisableVertexAttribArray: unsafe extern "system" fn(u32),
    EnableVertexAttribArray: unsafe extern "system" fn(u32),
    VertexAttrib4fv: unsafe extern "system" fn(u32, *const f32),
    VertexAttribPointer: unsafe extern "system" fn(u32, i32, u32, u8, i32, *const c_void),
    PolygonOffset: unsafe extern "system" fn(f32, f32),
    DrawElements: unsafe extern "system" fn(u32, i32, u32, *const c_void),
    DrawArrays: unsafe extern "system" fn(u32, i32, i32),
    GenFramebuffers: unsafe extern "system" fn(i32, *mut u32),
    DeleteFramebuffers: unsafe extern "system" fn(i32, *const u32),
    BindFramebuffer: unsafe extern "system" fn(u32, u32),
    CheckFramebufferStatus: unsafe extern "system" fn(u32) -> u32,
    FramebufferTexture2D: unsafe extern "system" fn(u32, u32, u32, u32, i32),
    GenRenderbuffers: unsafe extern "system" fn(i32, *mut u32),
    DeleteRenderbuffers: unsafe extern "system" fn(i32, *const u32),
    BindRenderbuffer: unsafe extern "system" fn(u32, u32),
    RenderbufferStorage: unsafe extern "system" fn(u32, u32, i32, i32),
    FramebufferRenderbuffer: unsafe extern "system" fn(u32, u32, u32, u32),
    GenTextures: unsafe extern "system" fn(i32, *mut u32),
    DeleteTextures: unsafe extern "system" fn(i32, *const u32),
    BindTexture: unsafe extern "system" fn(u32, u32),
    TexImage2D: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    TexParameteri: unsafe extern "system" fn(u32, u32, i32),
    ActiveTexture: unsafe extern "system" fn(u32),
    PixelStorei: unsafe extern "system" fn(u32, i32),
    Enable: unsafe extern "system" fn(u32),
    Disable: unsafe extern "system" fn(u32),
    Viewport: unsafe extern "system" fn(i32, i32, i32, i32),
    Scissor: unsafe extern "system" fn(i32, i32, i32, i32),
    DepthFunc: unsafe extern "system" fn(u32),
    DepthMask: unsafe extern "system" fn(u8),
    BlendFunc: unsafe extern "system" fn(u32, u32),
    CullFace: unsafe extern "system" fn(u32),
    FrontFace: unsafe extern "system" fn(u32),
    ColorMask: unsafe extern "system" fn(u8, u8, u8, u8),
    ClearColor: unsafe extern "system" fn(f32, f32, f32, f32),
    Clear: unsafe extern "system" fn(u32),
    ReadPixels: unsafe extern "system" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
}
impl Gl {
    unsafe fn load(host: &Host) -> Result<Self, String> {
        Ok(Self {
            CompressedTexImage2D: {
                let mut p = host.load(c"glCompressedTexImage2D");
                if p.is_null() {
                    p = host.load(c"glCompressedTexImage2DARB");
                }
                if p.is_null() {
                    None
                } else {
                    Some(std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "system" fn(u32, i32, u32, i32, i32, i32, i32, *const c_void),
                    >(p))
                }
            },
            GenerateMipmap: {
                let mut p = host.load(c"glGenerateMipmap");
                if p.is_null() {
                    p = host.load(c"glGenerateMipmapEXT");
                }
                if p.is_null() {
                    None
                } else {
                    Some(std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "system" fn(u32),
                    >(p))
                }
            },
            GetString: {
                let mut p = host.load(c"glGetString");
                if p.is_null() {
                    p = host.load(c"glGetStringEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetString".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32) -> *const u8>(p)
            },
            GetError: {
                let mut p = host.load(c"glGetError");
                if p.is_null() {
                    p = host.load(c"glGetErrorEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetError".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn() -> u32>(p)
            },
            GetFloatv: {
                let p = host.load(c"glGetFloatv");
                if p.is_null() {
                    return Err("driver lacks glGetFloatv".into());
                }
                std::mem::transmute::<*const c_void, unsafe extern "system" fn(u32, *mut f32)>(p)
            },
            LineWidth: {
                let p = host.load(c"glLineWidth");
                if p.is_null() {
                    return Err("driver lacks glLineWidth".into());
                }
                std::mem::transmute::<*const c_void, unsafe extern "system" fn(f32)>(p)
            },
            GetIntegerv: {
                let mut p = host.load(c"glGetIntegerv");
                if p.is_null() {
                    p = host.load(c"glGetIntegervEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetIntegerv".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, *mut i32)>(p)
            },
            CreateShader: {
                let mut p = host.load(c"glCreateShader");
                if p.is_null() {
                    p = host.load(c"glCreateShaderEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glCreateShader".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32) -> u32>(p)
            },
            ShaderSource: {
                let mut p = host.load(c"glShaderSource");
                if p.is_null() {
                    p = host.load(c"glShaderSourceEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glShaderSource".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, i32, *const *const c_char, *const i32),
                >(p)
            },
            CompileShader: {
                let mut p = host.load(c"glCompileShader");
                if p.is_null() {
                    p = host.load(c"glCompileShaderEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glCompileShader".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            GetShaderiv: {
                let mut p = host.load(c"glGetShaderiv");
                if p.is_null() {
                    p = host.load(c"glGetShaderivEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetShaderiv".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, *mut i32)>(p)
            },
            GetShaderInfoLog: {
                let mut p = host.load(c"glGetShaderInfoLog");
                if p.is_null() {
                    p = host.load(c"glGetShaderInfoLogEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetShaderInfoLog".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, i32, *mut i32, *mut c_char),
                >(p)
            },
            DeleteShader: {
                let mut p = host.load(c"glDeleteShader");
                if p.is_null() {
                    p = host.load(c"glDeleteShaderEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDeleteShader".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            CreateProgram: {
                let mut p = host.load(c"glCreateProgram");
                if p.is_null() {
                    p = host.load(c"glCreateProgramEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glCreateProgram".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn() -> u32>(p)
            },
            AttachShader: {
                let mut p = host.load(c"glAttachShader");
                if p.is_null() {
                    p = host.load(c"glAttachShaderEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glAttachShader".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32)>(p)
            },
            BindAttribLocation: {
                let mut p = host.load(c"glBindAttribLocation");
                if p.is_null() {
                    p = host.load(c"glBindAttribLocationEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBindAttribLocation".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, *const c_char)>(
                    p,
                )
            },
            LinkProgram: {
                let mut p = host.load(c"glLinkProgram");
                if p.is_null() {
                    p = host.load(c"glLinkProgramEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glLinkProgram".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            GetProgramiv: {
                let mut p = host.load(c"glGetProgramiv");
                if p.is_null() {
                    p = host.load(c"glGetProgramivEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetProgramiv".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, *mut i32)>(p)
            },
            GetProgramInfoLog: {
                let mut p = host.load(c"glGetProgramInfoLog");
                if p.is_null() {
                    p = host.load(c"glGetProgramInfoLogEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetProgramInfoLog".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, i32, *mut i32, *mut c_char),
                >(p)
            },
            DeleteProgram: {
                let mut p = host.load(c"glDeleteProgram");
                if p.is_null() {
                    p = host.load(c"glDeleteProgramEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDeleteProgram".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            UseProgram: {
                let mut p = host.load(c"glUseProgram");
                if p.is_null() {
                    p = host.load(c"glUseProgramEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glUseProgram".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            GetUniformLocation: {
                let mut p = host.load(c"glGetUniformLocation");
                if p.is_null() {
                    p = host.load(c"glGetUniformLocationEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGetUniformLocation".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, *const c_char) -> i32,
                >(p)
            },
            Uniform1i: {
                let mut p = host.load(c"glUniform1i");
                if p.is_null() {
                    p = host.load(c"glUniform1iEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glUniform1i".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, i32)>(p)
            },
            Uniform1f: {
                let mut p = host.load(c"glUniform1f");
                if p.is_null() {
                    p = host.load(c"glUniform1fEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glUniform1f".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, f32)>(p)
            },
            Uniform2f: {
                let mut p = host.load(c"glUniform2f");
                if p.is_null() {
                    p = host.load(c"glUniform2fEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glUniform2f".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, f32, f32)>(p)
            },
            UniformMatrix4fv: {
                let p = host.load(c"glUniformMatrix4fv");
                if p.is_null() {
                    return Err("driver lacks glUniformMatrix4fv".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(i32, i32, u8, *const f32),
                >(p)
            },
            Uniform4fv: {
                let mut p = host.load(c"glUniform4fv");
                if p.is_null() {
                    p = host.load(c"glUniform4fvEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glUniform4fv".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, i32, *const f32)>(
                    p,
                )
            },
            GenBuffers: {
                let mut p = host.load(c"glGenBuffers");
                if p.is_null() {
                    p = host.load(c"glGenBuffersEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGenBuffers".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *mut u32)>(p)
            },
            DeleteBuffers: {
                let mut p = host.load(c"glDeleteBuffers");
                if p.is_null() {
                    p = host.load(c"glDeleteBuffersEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDeleteBuffers".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *const u32)>(p)
            },
            BindBuffer: {
                let mut p = host.load(c"glBindBuffer");
                if p.is_null() {
                    p = host.load(c"glBindBufferEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBindBuffer".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32)>(p)
            },
            BufferData: {
                let mut p = host.load(c"glBufferData");
                if p.is_null() {
                    p = host.load(c"glBufferDataEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBufferData".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, isize, *const c_void, u32),
                >(p)
            },
            DisableVertexAttribArray: {
                let p = host.load(c"glDisableVertexAttribArray");
                if p.is_null() {
                    return Err("driver lacks glDisableVertexAttribArray".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            EnableVertexAttribArray: {
                let mut p = host.load(c"glEnableVertexAttribArray");
                if p.is_null() {
                    p = host.load(c"glEnableVertexAttribArrayEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glEnableVertexAttribArray".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            VertexAttrib4fv: {
                let p = host.load(c"glVertexAttrib4fv");
                if p.is_null() {
                    return Err("driver lacks glVertexAttrib4fv".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, *const f32)>(p)
            },
            VertexAttribPointer: {
                let mut p = host.load(c"glVertexAttribPointer");
                if p.is_null() {
                    p = host.load(c"glVertexAttribPointerEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glVertexAttribPointer".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, i32, u32, u8, i32, *const c_void),
                >(p)
            },
            PolygonOffset: {
                let p = host.load(c"glPolygonOffset");
                if p.is_null() {
                    return Err("driver lacks glPolygonOffset".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(f32, f32)>(p)
            },
            DrawElements: {
                let p = host.load(c"glDrawElements");
                if p.is_null() {
                    return Err("driver lacks glDrawElements".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(u32, i32, u32, *const c_void),
                >(p)
            },
            DrawArrays: {
                let mut p = host.load(c"glDrawArrays");
                if p.is_null() {
                    p = host.load(c"glDrawArraysEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDrawArrays".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, i32, i32)>(p)
            },
            GenFramebuffers: {
                let mut p = host.load(c"glGenFramebuffers");
                if p.is_null() {
                    p = host.load(c"glGenFramebuffersEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGenFramebuffers".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *mut u32)>(p)
            },
            DeleteFramebuffers: {
                let mut p = host.load(c"glDeleteFramebuffers");
                if p.is_null() {
                    p = host.load(c"glDeleteFramebuffersEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDeleteFramebuffers".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *const u32)>(p)
            },
            BindFramebuffer: {
                let mut p = host.load(c"glBindFramebuffer");
                if p.is_null() {
                    p = host.load(c"glBindFramebufferEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBindFramebuffer".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32)>(p)
            },
            CheckFramebufferStatus: {
                let mut p = host.load(c"glCheckFramebufferStatus");
                if p.is_null() {
                    p = host.load(c"glCheckFramebufferStatusEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glCheckFramebufferStatus".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32) -> u32>(p)
            },
            FramebufferTexture2D: {
                let mut p = host.load(c"glFramebufferTexture2D");
                if p.is_null() {
                    p = host.load(c"glFramebufferTexture2DEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glFramebufferTexture2D".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, u32, u32, i32)>(
                    p,
                )
            },
            GenRenderbuffers: {
                let mut p = host.load(c"glGenRenderbuffers");
                if p.is_null() {
                    p = host.load(c"glGenRenderbuffersEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGenRenderbuffers".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *mut u32)>(p)
            },
            DeleteRenderbuffers: {
                let mut p = host.load(c"glDeleteRenderbuffers");
                if p.is_null() {
                    p = host.load(c"glDeleteRenderbuffersEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDeleteRenderbuffers".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *const u32)>(p)
            },
            BindRenderbuffer: {
                let mut p = host.load(c"glBindRenderbuffer");
                if p.is_null() {
                    p = host.load(c"glBindRenderbufferEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBindRenderbuffer".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32)>(p)
            },
            RenderbufferStorage: {
                let mut p = host.load(c"glRenderbufferStorage");
                if p.is_null() {
                    p = host.load(c"glRenderbufferStorageEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glRenderbufferStorage".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, i32, i32)>(p)
            },
            FramebufferRenderbuffer: {
                let mut p = host.load(c"glFramebufferRenderbuffer");
                if p.is_null() {
                    p = host.load(c"glFramebufferRenderbufferEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glFramebufferRenderbuffer".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, u32, u32)>(p)
            },
            GenTextures: {
                let mut p = host.load(c"glGenTextures");
                if p.is_null() {
                    p = host.load(c"glGenTexturesEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glGenTextures".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *mut u32)>(p)
            },
            DeleteTextures: {
                let mut p = host.load(c"glDeleteTextures");
                if p.is_null() {
                    p = host.load(c"glDeleteTexturesEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDeleteTextures".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, *const u32)>(p)
            },
            BindTexture: {
                let mut p = host.load(c"glBindTexture");
                if p.is_null() {
                    p = host.load(c"glBindTextureEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBindTexture".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32)>(p)
            },
            TexImage2D: {
                let mut p = host.load(c"glTexImage2D");
                if p.is_null() {
                    p = host.load(c"glTexImage2DEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glTexImage2D".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(
                        u32,
                        i32,
                        i32,
                        i32,
                        i32,
                        i32,
                        u32,
                        u32,
                        *const c_void,
                    ),
                >(p)
            },
            TexParameteri: {
                let mut p = host.load(c"glTexParameteri");
                if p.is_null() {
                    p = host.load(c"glTexParameteriEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glTexParameteri".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32, i32)>(p)
            },
            ActiveTexture: {
                let mut p = host.load(c"glActiveTexture");
                if p.is_null() {
                    p = host.load(c"glActiveTextureEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glActiveTexture".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            PixelStorei: {
                let mut p = host.load(c"glPixelStorei");
                if p.is_null() {
                    p = host.load(c"glPixelStoreiEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glPixelStorei".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, i32)>(p)
            },
            Enable: {
                let mut p = host.load(c"glEnable");
                if p.is_null() {
                    p = host.load(c"glEnableEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glEnable".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            Disable: {
                let mut p = host.load(c"glDisable");
                if p.is_null() {
                    p = host.load(c"glDisableEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDisable".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            Viewport: {
                let mut p = host.load(c"glViewport");
                if p.is_null() {
                    p = host.load(c"glViewportEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glViewport".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, i32, i32, i32)>(p)
            },
            Scissor: {
                let mut p = host.load(c"glScissor");
                if p.is_null() {
                    p = host.load(c"glScissorEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glScissor".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(i32, i32, i32, i32)>(p)
            },
            DepthFunc: {
                let mut p = host.load(c"glDepthFunc");
                if p.is_null() {
                    p = host.load(c"glDepthFuncEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDepthFunc".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            DepthMask: {
                let mut p = host.load(c"glDepthMask");
                if p.is_null() {
                    p = host.load(c"glDepthMaskEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glDepthMask".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u8)>(p)
            },
            BlendFunc: {
                let mut p = host.load(c"glBlendFunc");
                if p.is_null() {
                    p = host.load(c"glBlendFuncEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glBlendFunc".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32, u32)>(p)
            },
            CullFace: {
                let mut p = host.load(c"glCullFace");
                if p.is_null() {
                    p = host.load(c"glCullFaceEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glCullFace".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            FrontFace: {
                let mut p = host.load(c"glFrontFace");
                if p.is_null() {
                    p = host.load(c"glFrontFaceEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glFrontFace".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            ColorMask: {
                let mut p = host.load(c"glColorMask");
                if p.is_null() {
                    p = host.load(c"glColorMaskEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glColorMask".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u8, u8, u8, u8)>(p)
            },
            ClearColor: {
                let mut p = host.load(c"glClearColor");
                if p.is_null() {
                    p = host.load(c"glClearColorEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glClearColor".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(f32, f32, f32, f32)>(p)
            },
            Clear: {
                let mut p = host.load(c"glClear");
                if p.is_null() {
                    p = host.load(c"glClearEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glClear".into());
                }
                std::mem::transmute::<*mut c_void, unsafe extern "system" fn(u32)>(p)
            },
            ReadPixels: {
                let mut p = host.load(c"glReadPixels");
                if p.is_null() {
                    p = host.load(c"glReadPixelsEXT");
                }
                if p.is_null() {
                    return Err("driver lacks glReadPixels".into());
                }
                std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
                >(p)
            },
        })
    }
}
fn native_compressed_format(format: u32) -> u32 {
    // Preserve the software HLE's DXT1 RGB selector-3 alpha contract.
    if format == GL_COMPRESSED_RGB_S3TC_DXT1_EXT {
        GL_COMPRESSED_RGBA_S3TC_DXT1_EXT
    } else {
        format
    }
}
#[derive(Debug, PartialEq, Eq)]
enum UploadRoute {
    Compressed,
    Generated,
    Rgba,
}
unsafe fn upload_error(gl: &Gl) -> Result<u32, String> {
    let mut first = 0;
    for _ in 0..16 {
        let error = (gl.GetError)();
        if error == 0 {
            break;
        }
        if error == GL_OUT_OF_MEMORY {
            return Err("native texture allocation failed".into());
        }
        if first == 0 {
            first = error;
        }
    }
    Ok(first)
}
unsafe fn upload_texture(
    gl: &Gl,
    formats: &HashSet<u32>,
    tex: &Texture,
    mips: bool,
) -> Result<UploadRoute, String> {
    let levels = tex.width.max(tex.height).ilog2() + 1;
    let has_explicit = !tex.mipmaps.is_empty();
    if let Some(base) = tex.compressed.get(&0) {
        let format = native_compressed_format(base.format);
        let complete = (0..if mips { levels } else { 1 }).all(|l| {
            tex.compressed.get(&l).is_some_and(|c| {
                c.width == (tex.width >> l).max(1)
                    && c.height == (tex.height >> l).max(1)
                    && native_compressed_format(c.format) == format
            })
        });
        if formats.contains(&format) && (!mips || complete || !has_explicit) {
            if let Some(upload) = gl.CompressedTexImage2D {
                for l in 0..if mips && complete { levels } else { 1 } {
                    let c = &tex.compressed[&l];
                    upload(
                        GL_TEXTURE_2D,
                        l as i32,
                        format,
                        c.width as i32,
                        c.height as i32,
                        0,
                        c.bytes.len() as i32,
                        c.bytes.as_ptr().cast(),
                    );
                }
                let mut ok = upload_error(gl)? == 0;
                if ok && mips && !complete {
                    if let Some(generate) = gl.GenerateMipmap {
                        generate(GL_TEXTURE_2D);
                        ok = upload_error(gl)? == 0;
                    } else {
                        ok = false;
                    }
                }
                if ok {
                    return Ok(UploadRoute::Compressed);
                }
            }
        }
    }
    let base = tex.rgba_level(0).unwrap();
    (gl.TexImage2D)(
        GL_TEXTURE_2D,
        0,
        GL_RGBA as i32,
        tex.width as i32,
        tex.height as i32,
        0,
        GL_RGBA,
        GL_UNSIGNED_BYTE,
        base.as_ptr().cast(),
    );
    let error = upload_error(gl)?;
    if error != 0 {
        return Err(format!("native RGBA texture upload failed: 0x{error:04x}"));
    }
    if mips && !has_explicit {
        if let Some(generate) = gl.GenerateMipmap {
            generate(GL_TEXTURE_2D);
            if upload_error(gl)? == 0 {
                return Ok(UploadRoute::Generated);
            }
        }
    }
    if mips {
        for (l, (w, h, pixels)) in tex.upload_levels().iter().enumerate().skip(1) {
            (gl.TexImage2D)(
                GL_TEXTURE_2D,
                l as i32,
                GL_RGBA as i32,
                *w as i32,
                *h as i32,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
        }
    }
    Ok(UploadRoute::Rgba)
}

pub struct Renderer {
    host: Host,
    gl: Gl,
    program: u32,
    buffer: u32,
    index_buffer: u32,
    indices: Vec<u16>,
    packed_scratch: Vec<f32>,
    vertex_keys: HashMap<[u32; 24], u16>,
    uniform_values: HashMap<i32, Vec<u32>>,
    pipeline: Option<PipelineState>,
    texture_params: HashMap<u32, (Filter, Filter, Wrap, Wrap)>,
    texture_anisotropy: HashMap<u32, u8>,
    anisotropy: u8,
    max_anisotropy: u8,
    fbo: u32,
    color: u32,
    depth: u32,
    depth_format: u32,
    white: u32,
    width: u32,
    height: u32,
    textures: HashMap<u32, (u32, u64)>,
    uniforms: HashMap<String, i32>,
    scratch: Vec<f32>,
    readback: Vec<u8>,
    dirty: bool,
    full_npot: bool,
    native_max_level: bool,
    lod_derivatives: bool,
    compressed_formats: HashSet<u32>,
    attribute_layout: Option<VertexLayout>,
    stream_layout: Option<[Option<StreamAttribute>; 8]>,
    native_fixed: bool,
    pub point_range: [f32; 2],
    pub line_range: [f32; 2],
}
impl Renderer {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        Self::with_host(Host::new()?, width, height)
    }
    #[cfg(all(test, unix))]
    pub(crate) fn new_es_for_test(width: u32, height: u32) -> Result<Self, String> {
        Self::with_host(Host::es_for_test()?, width, height)
    }
    fn with_host(host: Host, width: u32, height: u32) -> Result<Self, String> {
        unsafe {
            let gl = Gl::load(&host)?;
            let mut units = 0;
            let mut attributes = 0;
            (gl.GetIntegerv)(0x8872, &mut units);
            (gl.GetIntegerv)(0x8869, &mut attributes);
            if units < 4 || attributes < 8 {
                return Err("OpenGL driver lacks required texture/attribute units".into());
            }
            let mut point_range = [1.; 2];
            let mut line_range = [1.; 2];
            (gl.GetFloatv)(0x846d, point_range.as_mut_ptr());
            (gl.GetFloatv)(0x846e, line_range.as_mut_ptr());
            for range in [&mut point_range, &mut line_range] {
                range[0] = range[0].max(1.);
                range[1] = range[1].clamp(range[0], 64.);
            }
            let full_npot = if host.es {
                let extensions = (gl.GetString)(0x1f03);
                let version = (gl.GetString)(0x1f02);
                (!extensions.is_null()
                    && CStr::from_ptr(extensions.cast())
                        .to_string_lossy()
                        .split_whitespace()
                        .any(|s| s == "GL_OES_texture_npot"))
                    || (!version.is_null()
                        && CStr::from_ptr(version.cast())
                            .to_string_lossy()
                            .starts_with("OpenGL ES 3."))
            } else {
                true
            };
            // Software retains float depth. A forced 16-bit attachment makes
            // nearby terrain/decal layers collapse at distance (RBR Android).
            let depth24 = if host.es {
                let extensions = (gl.GetString)(0x1f03);
                let version = (gl.GetString)(0x1f02);
                (!extensions.is_null()
                    && CStr::from_ptr(extensions.cast())
                        .to_string_lossy()
                        .split_whitespace()
                        .any(|s| s == "GL_OES_depth24"))
                    || (!version.is_null()
                        && CStr::from_ptr(version.cast())
                            .to_string_lossy()
                            .starts_with("OpenGL ES 3."))
            } else {
                true
            };
            let extensions_ptr = (gl.GetString)(0x1f03);
            let extensions = if extensions_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(extensions_ptr.cast())
                    .to_string_lossy()
                    .into_owned()
            };
            let version_ptr = (gl.GetString)(0x1f02);
            let es3 = !version_ptr.is_null()
                && CStr::from_ptr(version_ptr.cast())
                    .to_string_lossy()
                    .starts_with("OpenGL ES 3.");
            let native_max_level = !host.es
                || es3
                || extensions
                    .split_whitespace()
                    .any(|e| e == "GL_APPLE_texture_max_level");
            let lod_derivatives = !host.es
                || extensions
                    .split_whitespace()
                    .any(|e| e == "GL_OES_standard_derivatives");
            let native_fixed = host.es
                || extensions
                    .split_whitespace()
                    .any(|e| e == "GL_ARB_ES2_compatibility");
            let mut max_anisotropy = 1f32;
            if extensions.split_whitespace().any(|e| {
                matches!(
                    e,
                    "GL_EXT_texture_filter_anisotropic" | "GL_ARB_texture_filter_anisotropic"
                )
            }) {
                (gl.GetFloatv)(0x84ff, &mut max_anisotropy);
            }
            let max_anisotropy = if max_anisotropy.is_finite() {
                max_anisotropy.clamp(1., 8.).floor() as u8
            } else {
                1
            };
            let mut count = 0;
            (gl.GetIntegerv)(GL_NUM_COMPRESSED_TEXTURE_FORMATS, &mut count);
            let mut formats = vec![0i32; count.clamp(0, 128) as usize];
            if count > 0 && count <= 128 {
                (gl.GetIntegerv)(GL_COMPRESSED_TEXTURE_FORMATS, formats.as_mut_ptr());
            }
            let compressed_formats = formats.into_iter().map(|n| n as u32).collect();
            let mut r = Self {
                host,
                gl,
                program: 0,
                buffer: 0,
                index_buffer: 0,
                indices: Vec::new(),
                packed_scratch: Vec::new(),
                vertex_keys: HashMap::new(),
                uniform_values: HashMap::new(),
                pipeline: None,
                texture_params: HashMap::new(),
                texture_anisotropy: HashMap::new(),
                anisotropy: 1,
                max_anisotropy,
                fbo: 0,
                color: 0,
                depth: 0,
                depth_format: if depth24 { 0x81a6 } else { 0x81a5 },
                white: 0,
                width: 0,
                height: 0,
                textures: HashMap::new(),
                uniforms: HashMap::new(),
                scratch: Vec::new(),
                readback: Vec::new(),
                dirty: true,
                full_npot,
                native_max_level,
                lod_derivatives,
                compressed_formats,
                attribute_layout: None,
                stream_layout: None,
                native_fixed,
                point_range,
                line_range,
            };
            r.program = (r.gl.CreateProgram)();
            if r.program == 0 {
                return Err("OpenGL program allocation failed".into());
            }
            let prefix = if r.host.es {
                "#version 100\nprecision mediump float;\nprecision mediump int;\n#define VARYING varying mediump\n"
            } else {
                "#version 120\n#define VARYING varying\n"
            };
            let vs = r.shader(
                0x8b31,
                &format!(
                    "{}{VERTEX}",
                    prefix.replace("precision mediump float", "precision highp float")
                ),
            )?;
            let fragment_prefix = if r.lod_derivatives {
                if r.host.es {
                    prefix.replace("#version 100\n", "#version 100\n#extension GL_OES_standard_derivatives : enable\n#define LOD_DERIVATIVES\n")
                } else {
                    format!("{prefix}#define LOD_DERIVATIVES\n")
                }
            } else {
                prefix.to_owned()
            };
            let fs = match r.shader(0x8b30, &format!("{fragment_prefix}{FRAGMENT}")) {
                Ok(s) => s,
                Err(e) => {
                    (r.gl.DeleteShader)(vs);
                    return Err(e);
                }
            };
            (r.gl.AttachShader)(r.program, vs);
            (r.gl.AttachShader)(r.program, fs);
            for (i, name) in [
                "position",
                "color",
                "tex0",
                "tex1",
                "tex2",
                "tex3",
                "normalAndFog",
                "lightPosition",
            ]
            .iter()
            .enumerate()
            {
                (r.gl.BindAttribLocation)(
                    r.program,
                    i as u32,
                    CString::new(*name).unwrap().as_ptr(),
                );
            }
            (r.gl.LinkProgram)(r.program);
            (r.gl.DeleteShader)(vs);
            (r.gl.DeleteShader)(fs);
            let mut ok = 0;
            (r.gl.GetProgramiv)(r.program, 0x8b82, &mut ok);
            if ok == 0 {
                let mut msg = vec![0u8; 4096];
                (r.gl.GetProgramInfoLog)(
                    r.program,
                    4096,
                    std::ptr::null_mut(),
                    msg.as_mut_ptr().cast(),
                );
                return Err(String::from_utf8_lossy(&msg).trim_end_matches('\0').into());
            }
            (r.gl.GenBuffers)(1, &mut r.buffer);
            (r.gl.GenBuffers)(1, &mut r.index_buffer);
            (r.gl.GenFramebuffers)(1, &mut r.fbo);
            (r.gl.GenTextures)(1, &mut r.color);
            (r.gl.GenRenderbuffers)(1, &mut r.depth);
            (r.gl.GenTextures)(1, &mut r.white);
            (r.gl.ActiveTexture)(GL_TEXTURE0);
            (r.gl.BindTexture)(GL_TEXTURE_2D, r.white);
            (r.gl.TexImage2D)(
                GL_TEXTURE_2D,
                0,
                GL_RGBA as i32,
                1,
                1,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                [255u8; 4].as_ptr().cast(),
            );
            r.params(
                Filter::Nearest,
                Filter::Nearest,
                Wrap::ClampToEdge,
                Wrap::ClampToEdge,
            );
            r.resize(width, height)?;
            for name in [
                "lightingEnabled",
                "normalMatrix",
                "normalMode",
                "normalRescale",
                "colorMaterial",
                "twoSided",
                "modelAmbient",
                "materialAmbient",
                "materialDiffuse",
                "materialSpecular",
                "materialEmission",
                "shininess",
                "lightAmbient[0]",
                "lightDiffuse[0]",
                "lightSpecular[0]",
                "lightPositions[0]",
                "lightSpot[0]",
                "lightAttenuation[0]",
                "projectionMatrix",
                "modelviewMatrix",
                "objectSpace",
                "textureMatrix0",
                "textureMatrix1",
                "textureMatrix2",
                "textureMatrix3",
                "depthRange",
                "pointSize",
                "alphaMode",
                "alphaRef",
                "fogMode",
                "fogColor",
                "fogStart",
                "fogEnd",
                "fogDensity",
            ] {
                r.cache_uniform(name);
            }
            for i in 0..4 {
                for name in ["sampler", "env", "envColor", "lodBias", "lodLimit"] {
                    r.cache_uniform(&format!("{name}{i}"));
                }
            }
            let p = (r.gl.GetString)(0x1f01);
            let label = if p.is_null() {
                "unknown".into()
            } else {
                CStr::from_ptr(p.cast()).to_string_lossy().into_owned()
            };
            log::info!("GLES host OpenGL renderer: {label}");
            r.check()?;
            Ok(r)
        }
    }
    unsafe fn shader(&self, ty: u32, source: &str) -> Result<u32, String> {
        let s = (self.gl.CreateShader)(ty);
        if s == 0 {
            return Err("shader allocation failed".into());
        }
        let source = CString::new(source).unwrap();
        (self.gl.ShaderSource)(s, 1, &source.as_ptr(), std::ptr::null());
        (self.gl.CompileShader)(s);
        let mut ok = 0;
        (self.gl.GetShaderiv)(s, 0x8b81, &mut ok);
        if ok == 0 {
            let mut msg = vec![0u8; 4096];
            (self.gl.GetShaderInfoLog)(s, 4096, std::ptr::null_mut(), msg.as_mut_ptr().cast());
            (self.gl.DeleteShader)(s);
            Err(String::from_utf8_lossy(&msg).trim_end_matches('\0').into())
        } else {
            Ok(s)
        }
    }
    unsafe fn cache_uniform(&mut self, n: &str) {
        let l = (self.gl.GetUniformLocation)(self.program, CString::new(n).unwrap().as_ptr());
        self.uniforms.insert(n.into(), l);
    }
    fn changed_uniform(&mut self, location: i32, value: &[u32]) -> bool {
        if location < 0 {
            return false;
        }
        if self
            .uniform_values
            .get(&location)
            .is_some_and(|old| old == value)
        {
            return false;
        }
        let cached = self.uniform_values.entry(location).or_default();
        cached.clear();
        cached.extend_from_slice(value);
        true
    }
    unsafe fn uniform_1i(&mut self, l: i32, v: i32) {
        if self.changed_uniform(l, &[v as u32]) {
            (self.gl.Uniform1i)(l, v);
        }
    }
    unsafe fn uniform_1f(&mut self, l: i32, v: f32) {
        if self.changed_uniform(l, &[v.to_bits()]) {
            (self.gl.Uniform1f)(l, v);
        }
    }
    unsafe fn uniform_2f(&mut self, l: i32, a: f32, b: f32) {
        if self.changed_uniform(l, &[a.to_bits(), b.to_bits()]) {
            (self.gl.Uniform2f)(l, a, b);
        }
    }
    unsafe fn uniform_4fv(&mut self, l: i32, n: i32, p: *const f32) {
        let mut bits = [0u32; 32];
        let len = n as usize * 4;
        for (dst, v) in bits[..len]
            .iter_mut()
            .zip(std::slice::from_raw_parts(p, len))
        {
            *dst = v.to_bits();
        }
        if self.changed_uniform(l, &bits[..len]) {
            (self.gl.Uniform4fv)(l, n, p);
        }
    }
    unsafe fn uniform_matrix4fv(&mut self, l: i32, n: i32, transpose: u8, p: *const f32) {
        let mut bits = [0u32; 16];
        for (dst, v) in bits.iter_mut().zip(std::slice::from_raw_parts(p, 16)) {
            *dst = v.to_bits();
        }
        if self.changed_uniform(l, &bits) {
            (self.gl.UniformMatrix4fv)(l, n, transpose, p);
        }
    }
    fn loc(&self, n: &str) -> i32 {
        self.uniforms[n]
    }
    unsafe fn check(&self) -> Result<(), String> {
        let e = (self.gl.GetError)();
        if e == 0 {
            Ok(())
        } else {
            Err(format!("host OpenGL error 0x{e:04x}"))
        }
    }
    unsafe fn toggle(&self, cap: u32, on: bool) {
        if on {
            (self.gl.Enable)(cap)
        } else {
            (self.gl.Disable)(cap)
        }
    }
    unsafe fn scissor(&self, s: Option<(i32, i32, i32, i32)>) {
        self.toggle(GL_SCISSOR_TEST, s.is_some());
        if let Some((x, y, w, h)) = s {
            (self.gl.Scissor)(x, y, w, h);
        }
    }
    unsafe fn params(&self, min: Filter, mag: Filter, s: Wrap, t: Wrap) {
        for (p, v) in [
            (GL_TEXTURE_MIN_FILTER, filter(min)),
            (GL_TEXTURE_MAG_FILTER, filter(mag)),
            (GL_TEXTURE_WRAP_S, wrap(s)),
            (GL_TEXTURE_WRAP_T, wrap(t)),
        ] {
            (self.gl.TexParameteri)(GL_TEXTURE_2D, p, v as i32);
        }
    }
    pub fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        if (w, h) == (self.width, self.height) {
            return Ok(());
        }
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            return Err("unsupported GPU target size".into());
        }
        self.host.make_current()?;
        unsafe {
            (self.gl.ActiveTexture)(GL_TEXTURE0);
            (self.gl.BindTexture)(GL_TEXTURE_2D, self.color);
            (self.gl.TexImage2D)(
                GL_TEXTURE_2D,
                0,
                GL_RGBA as i32,
                w as i32,
                h as i32,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                std::ptr::null(),
            );
            self.params(
                Filter::Nearest,
                Filter::Nearest,
                Wrap::ClampToEdge,
                Wrap::ClampToEdge,
            );
            self.check()?;
            (self.gl.BindRenderbuffer)(0x8d41, self.depth);
            (self.gl.BindFramebuffer)(0x8d40, self.fbo);
            (self.gl.FramebufferTexture2D)(0x8d40, 0x8ce0, GL_TEXTURE_2D, self.color, 0);
            (self.gl.FramebufferRenderbuffer)(0x8d40, 0x8d00, 0x8d41, self.depth);
            loop {
                (self.gl.RenderbufferStorage)(0x8d41, self.depth_format, w as i32, h as i32);
                let error = upload_error(&self.gl)?;
                let complete = (self.gl.CheckFramebufferStatus)(0x8d40) == 0x8cd5;
                if error == 0 && complete {
                    break;
                }
                if self.depth_format == 0x81a6 {
                    // Keep ES2 drivers lacking a usable 24-bit attachment usable.
                    self.depth_format = 0x81a5;
                } else {
                    return Err(format!("GPU depth attachment failed: 0x{error:04x}"));
                }
            }
            self.pipeline = None;
            self.scissor(None);
            (self.gl.ColorMask)(1, 1, 1, 1);
            (self.gl.DepthMask)(1);
            (self.gl.ClearColor)(0., 0., 0., 0.);
            (self.gl.Clear)(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
            self.width = w;
            self.height = h;
            self.dirty = true;
            self.check()
        }
    }
    pub fn clear(
        &mut self,
        w: u32,
        h: u32,
        state: &PipelineState,
        color: [f32; 4],
        depth: f32,
        mask: u32,
    ) -> Result<(), String> {
        self.resize(w, h)?;
        self.host.make_current()?;
        self.pipeline = None;
        unsafe {
            (self.gl.BindFramebuffer)(0x8d40, self.fbo);
            self.scissor(state.scissor);
            (self.gl.ColorMask)(
                state.color_mask[0] as u8,
                state.color_mask[1] as u8,
                state.color_mask[2] as u8,
                state.color_mask[3] as u8,
            );
            // Match the software HLE contract: explicit depth clears reset depth
            // even after a HUD pass left depth writes disabled. Keep draw masks intact.
            (self.gl.DepthMask)(1);
            (self.gl.ClearColor)(color[0], color[1], color[2], color[3]);
            let p = self.host.load(if self.host.es {
                c"glClearDepthf"
            } else {
                c"glClearDepth"
            });
            if p.is_null() {
                return Err("driver lacks depth clear".into());
            }
            if self.host.es {
                let f: unsafe extern "system" fn(f32) = std::mem::transmute(p);
                f(depth);
            } else {
                let f: unsafe extern "system" fn(f64) = std::mem::transmute(p);
                f(depth as f64);
            }
            (self.gl.Clear)(mask & (GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT));
            (self.gl.DepthMask)(state.depth_write as u8);
            self.dirty = true;
            self.check()
        }
    }
    /// The choice is retained in Context; each native surface clamps independently.
    pub fn set_anisotropic_filtering(&mut self, level: u8) {
        self.anisotropy = match level {
            2 | 4 | 8 => level,
            _ => 1,
        }
        .min(self.max_anisotropy);
    }
    pub fn supports_compressed(&self, format: u32) -> bool {
        let format = native_compressed_format(format);
        self.gl.CompressedTexImage2D.is_some() && self.compressed_formats.contains(&format)
    }
    pub fn delete_textures(&mut self, names: &[u32]) {
        if self.host.make_current().is_err() {
            return;
        }
        for n in names {
            // Guest name zero is the persistent default texture, not native GL zero.
            if *n == 0 {
                continue;
            }
            if let Some((id, _)) = self.textures.remove(n) {
                self.texture_params.remove(&id);
                self.texture_anisotropy.remove(&id);
                unsafe {
                    (self.gl.DeleteTextures)(1, &id);
                }
            }
        }
    }
    pub fn draw(
        &mut self,
        target: &RenderTarget,
        state: &PipelineState,
        mode: u32,
        vertices: &[Vertex],
        stages: &[TextureStage],
        textures: &HashMap<u32, Texture>,
        names: [u32; 4],
    ) -> Result<(), String> {
        self.draw_inner(
            target, state, mode, vertices, stages, textures, names, None, false, None, None,
        )
    }

    pub fn draw_object_space(
        &mut self,
        target: &RenderTarget,
        state: &PipelineState,
        mode: u32,
        vertices: &[Vertex],
        stages: &[TextureStage],
        textures: &HashMap<u32, Texture>,
        names: [u32; 4],
        transforms: &VertexTransforms,
    ) -> Result<(), String> {
        self.draw_inner(
            target,
            state,
            mode,
            vertices,
            stages,
            textures,
            names,
            Some(transforms),
            false,
            None,
            None,
        )
    }

    pub fn draw_arrays_object_space(
        &mut self,
        target: &RenderTarget,
        state: &PipelineState,
        mode: u32,
        vertices: &[Vertex],
        stages: &[TextureStage],
        textures: &HashMap<u32, Texture>,
        names: [u32; 4],
        transforms: &VertexTransforms,
    ) -> Result<(), String> {
        self.draw_inner(
            target,
            state,
            mode,
            vertices,
            stages,
            textures,
            names,
            Some(transforms),
            true,
            None,
            None,
        )
    }

    /// Guest indices are already remapped to unique, validated vertices by Context.
    pub fn draw_indexed_object_space(
        &mut self,
        target: &RenderTarget,
        state: &PipelineState,
        mode: u32,
        vertices: &[Vertex],
        indices: &[u16],
        stages: &[TextureStage],
        textures: &HashMap<u32, Texture>,
        names: [u32; 4],
        transforms: &VertexTransforms,
    ) -> Result<(), String> {
        if indices.iter().any(|i| *i as usize >= vertices.len()) {
            return Err("invalid native index".into());
        }
        self.draw_inner(
            target,
            state,
            mode,
            vertices,
            stages,
            textures,
            names,
            Some(transforms),
            true,
            Some(indices),
            None,
        )
    }

    pub(crate) fn supports_fixed_vertices(&self) -> bool {
        self.native_fixed
    }

    pub(crate) fn draw_stream_object_space(
        &mut self,
        target: &RenderTarget,
        state: &PipelineState,
        mode: u32,
        stream: &VertexStream,
        stages: &[TextureStage],
        textures: &HashMap<u32, Texture>,
        names: [u32; 4],
        transforms: &VertexTransforms,
    ) -> Result<(), String> {
        if !state.smooth_shading
            || stream.count == 0
            || stream.count > 65536
            || stream.indices.len() > i32::MAX as usize
            || stream.indices.iter().any(|i| *i as usize >= stream.count)
        {
            return Err("invalid vertex stream".into());
        }
        for attr in stream.attributes.iter().flatten() {
            let width = match attr.ty {
                GL_BYTE | GL_UNSIGNED_BYTE => 1,
                GL_SHORT | GL_UNSIGNED_SHORT => 2,
                GL_FLOAT => 4,
                GL_FIXED if self.native_fixed => 4,
                _ => return Err("unsupported stream format".into()),
            };
            if !(1..=4).contains(&attr.size)
                || attr.stride <= 0
                || attr.normalized > 1
                || attr.offset % width != 0
                || attr.stride as usize % width != 0
            {
                return Err("invalid stream attribute".into());
            }
            let end = (stream.count - 1)
                .checked_mul(attr.stride as usize)
                .and_then(|n| n.checked_add(attr.offset))
                .and_then(|n| n.checked_add(attr.size as usize * width));
            if end.is_none_or(|n| n > stream.bytes.len()) {
                return Err("stream attribute outside host snapshot".into());
            }
        }
        if stream.attributes[0].is_none() {
            return Err("missing stream positions".into());
        }
        self.draw_inner(
            target,
            state,
            mode,
            &[],
            stages,
            textures,
            names,
            Some(transforms),
            true,
            None,
            Some(stream),
        )
    }

    fn draw_inner(
        &mut self,
        target: &RenderTarget,
        state: &PipelineState,
        mode: u32,
        vertices: &[Vertex],
        stages: &[TextureStage],
        textures: &HashMap<u32, Texture>,
        names: [u32; 4],
        transforms: Option<&VertexTransforms>,
        sequential: bool,
        guest_indices: Option<&[u16]>,
        stream: Option<&VertexStream>,
    ) -> Result<(), String> {
        self.resize(target.width, target.height)?;
        self.host.make_current()?;
        self.scratch.clear();
        let layout = VertexLayout::new(state, stages, transforms);
        let direct = sequential
            && state.smooth_shading
            && matches!(
                mode,
                GL_TRIANGLES | GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN | GL_POINTS | GL_LINES
            );
        let draw_mode = if direct {
            mode
        } else {
            match mode {
                GL_POINTS => GL_POINTS,
                GL_LINES | GL_LINE_STRIP | GL_LINE_LOOP => GL_LINES,
                _ => GL_TRIANGLES,
            }
        };
        let mut indexed = stream.is_some_and(|s| !s.indices.is_empty());
        if stream.is_none() {
            let mut primitive = |v: &[Vertex]| {
                let provoking = *v.last().unwrap();
                for &vertex in v {
                    layout.pack(&mut self.scratch, vertex, provoking, state.smooth_shading);
                }
            };
            if direct {
                for vertex in vertices {
                    primitive(&[*vertex]);
                }
            } else {
                match mode {
                    GL_TRIANGLES => {
                        for v in vertices.chunks_exact(3) {
                            primitive(&[v[0], v[1], v[2]]);
                        }
                    }
                    GL_TRIANGLE_FAN => {
                        for i in 2..vertices.len() {
                            primitive(&[vertices[0], vertices[i - 1], vertices[i]]);
                        }
                    }
                    GL_TRIANGLE_STRIP => {
                        for i in 2..vertices.len() {
                            primitive(&if i % 2 == 0 {
                                [vertices[i - 2], vertices[i - 1], vertices[i]]
                            } else {
                                [vertices[i - 1], vertices[i - 2], vertices[i]]
                            });
                        }
                    }
                    GL_POINTS => {
                        for v in vertices {
                            primitive(&[*v]);
                        }
                    }
                    GL_LINES => {
                        for v in vertices.chunks_exact(2) {
                            primitive(v);
                        }
                    }
                    GL_LINE_STRIP | GL_LINE_LOOP => {
                        for v in vertices.windows(2) {
                            primitive(v);
                        }
                        if mode == GL_LINE_LOOP && vertices.len() > 1 {
                            primitive(&[*vertices.last().unwrap(), vertices[0]]);
                        }
                    }
                    _ => return Err("unsupported GPU primitive".into()),
                }
            }
            self.indices.clear();
            let mut unique = std::mem::take(&mut self.vertex_keys);
            let mut packed = std::mem::take(&mut self.packed_scratch);
            packed.clear();
            indexed = !direct;
            if let Some(indices) = guest_indices {
                self.indices.extend_from_slice(indices);
                indexed = true;
            }
            if !direct {
                unique.clear();
                for record in self.scratch.chunks_exact(layout.floats) {
                    let key = std::array::from_fn(|i| record.get(i).map_or(0, |v| v.to_bits()));
                    let index = if let Some(&index) = unique.get(&key) {
                        index
                    } else {
                        if unique.len() == 65536 {
                            indexed = false;
                            break;
                        }
                        let index = unique.len() as u16;
                        unique.insert(key, index);
                        packed.extend_from_slice(record);
                        index
                    };
                    self.indices.push(index);
                }
            }
            // Do not increase transfer size for draws with little vertex reuse.
            if guest_indices.is_none() {
                indexed &= packed.len() * 4 + self.indices.len() * 2 < self.scratch.len() * 4;
            }
            if indexed && guest_indices.is_none() {
                std::mem::swap(&mut self.scratch, &mut packed);
            }
            self.vertex_keys = unique;
            self.packed_scratch = packed;
        }
        unsafe {
            (self.gl.BindFramebuffer)(0x8d40, self.fbo);
            (self.gl.UseProgram)(self.program);
            self.uniform_1f(
                self.loc("pointSize"),
                state
                    .point_size
                    .clamp(self.point_range[0], self.point_range[1]),
            );
            if self.pipeline.as_ref() != Some(state) {
                (self.gl.LineWidth)(
                    state
                        .line_width
                        .clamp(self.line_range[0], self.line_range[1]),
                );
                if !self.host.es {
                    self.toggle(0x8642, true);
                }
                let (x, y, w, h) = state.viewport;
                (self.gl.Viewport)(x, y, w, h);
                self.scissor(state.scissor);
                self.toggle(GL_DEPTH_TEST, state.depth_test);
                (self.gl.DepthFunc)(compare(state.depth_func));
                (self.gl.DepthMask)(state.depth_write as u8);
                self.toggle(GL_BLEND, state.blend);
                (self.gl.BlendFunc)(blend(state.blend_src), blend(state.blend_dst));
                self.toggle(GL_CULL_FACE, state.cull.is_some());
                if let Some(c) = state.cull {
                    (self.gl.CullFace)(match c {
                        CullMode::Front => GL_FRONT,
                        CullMode::Back => GL_BACK,
                        CullMode::FrontAndBack => GL_FRONT_AND_BACK,
                    });
                }
                (self.gl.FrontFace)(if state.front_face == FrontFace::Ccw {
                    GL_CCW
                } else {
                    GL_CW
                });
                (self.gl.ColorMask)(
                    state.color_mask[0] as u8,
                    state.color_mask[1] as u8,
                    state.color_mask[2] as u8,
                    state.color_mask[3] as u8,
                );
                self.toggle(GL_POLYGON_OFFSET_FILL, state.polygon_offset);
                (self.gl.PolygonOffset)(state.polygon_factor, state.polygon_units);
                self.pipeline = Some(state.clone());
            }
            let lighting = transforms.filter(|t| t.lighting.enabled);
            self.uniform_1i(self.loc("lightingEnabled"), lighting.is_some() as i32);
            self.uniform_1i(
                self.loc("twoSided"),
                (matches!(
                    draw_mode,
                    GL_TRIANGLES | GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN
                ) && lighting.is_some_and(|t| t.lighting.two_sided)) as i32,
            );
            if let Some(t) = lighting {
                let l = &t.lighting;
                self.uniform_matrix4fv(self.loc("normalMatrix"), 1, 0, t.normal_matrix.as_ptr());
                self.uniform_1i(
                    self.loc("normalMode"),
                    if l.normalize {
                        1
                    } else if l.rescale {
                        2
                    } else {
                        0
                    },
                );
                let scale = (t.normal_matrix[8] * t.normal_matrix[8]
                    + t.normal_matrix[9] * t.normal_matrix[9]
                    + t.normal_matrix[10] * t.normal_matrix[10])
                    .sqrt();
                self.uniform_1f(
                    self.loc("normalRescale"),
                    if scale > 0. { 1. / scale } else { 1. },
                );
                self.uniform_1i(self.loc("colorMaterial"), l.color_material as i32);
                self.uniform_1f(self.loc("shininess"), l.shininess);
                for (name, value) in [
                    ("modelAmbient", l.model_ambient),
                    ("materialAmbient", l.ambient),
                    ("materialDiffuse", l.diffuse),
                    ("materialSpecular", l.specular),
                    ("materialEmission", l.emission),
                ] {
                    self.uniform_4fv(self.loc(name), 1, value.as_ptr());
                }
                let ambient: [[f32; 4]; 8] = std::array::from_fn(|i| {
                    let mut c = l.lights[i].ambient;
                    c[3] = l.lights[i].enabled as u8 as f32;
                    c
                });
                let diffuse: [[f32; 4]; 8] = std::array::from_fn(|i| l.lights[i].diffuse);
                let specular: [[f32; 4]; 8] = std::array::from_fn(|i| l.lights[i].specular);
                let positions: [[f32; 4]; 8] = std::array::from_fn(|i| l.lights[i].position);
                let spot: [[f32; 4]; 8] = std::array::from_fn(|i| {
                    let a = l.lights[i];
                    [
                        a.spot_direction[0],
                        a.spot_direction[1],
                        a.spot_direction[2],
                        if a.spot_cutoff == 180. {
                            -1.
                        } else {
                            a.spot_cutoff.to_radians().cos().max(0.)
                        },
                    ]
                });
                let attenuation: [[f32; 4]; 8] = std::array::from_fn(|i| {
                    let a = l.lights[i];
                    [
                        a.attenuation[0],
                        a.attenuation[1],
                        a.attenuation[2],
                        a.spot_exponent,
                    ]
                });
                for (name, values) in [
                    ("lightAmbient[0]", &ambient),
                    ("lightDiffuse[0]", &diffuse),
                    ("lightSpecular[0]", &specular),
                    ("lightPositions[0]", &positions),
                    ("lightSpot[0]", &spot),
                    ("lightAttenuation[0]", &attenuation),
                ] {
                    self.uniform_4fv(self.loc(name), 8, values.as_ptr().cast());
                }
            }
            self.uniform_1i(self.loc("objectSpace"), transforms.is_some() as i32);
            let projection = transforms.map(|t| &t.projection).unwrap_or(&IDENTITY);
            let modelview = transforms.map(|t| &t.modelview).unwrap_or(&IDENTITY);
            self.uniform_matrix4fv(self.loc("projectionMatrix"), 1, 0, projection.as_ptr());
            self.uniform_matrix4fv(self.loc("modelviewMatrix"), 1, 0, modelview.as_ptr());
            for (i, name) in TEXTURE_MATRICES.iter().enumerate() {
                let matrix = transforms.map(|t| &t.textures[i]).unwrap_or(&IDENTITY);
                self.uniform_matrix4fv(self.loc(name), 1, 0, matrix.as_ptr());
            }
            self.uniform_2f(
                self.loc("depthRange"),
                state.depth_range.0,
                state.depth_range.1,
            );
            self.uniform_1i(
                self.loc("alphaMode"),
                if state.alpha_test {
                    compare(state.alpha_func) as i32 - GL_NEVER as i32
                } else {
                    7
                },
            );
            // The software pipeline compares alpha/ref at 8-bit fixed precision.
            // Upload the rounded reference as an exactly representable integer.
            self.uniform_1f(
                self.loc("alphaRef"),
                (state.alpha_ref.clamp(0., 1.) * 255. + 0.5).floor(),
            );
            self.uniform_1i(
                self.loc("fogMode"),
                if state.fog {
                    match state.fog_mode {
                        FogMode::Linear => 1,
                        FogMode::Exp => 2,
                        FogMode::Exp2 => 3,
                    }
                } else {
                    0
                },
            );
            self.uniform_4fv(self.loc("fogColor"), 1, state.fog_color.as_ptr());
            for (n, v) in [
                ("fogStart", state.fog_start),
                ("fogEnd", state.fog_end),
                ("fogDensity", state.fog_density),
            ] {
                self.uniform_1f(self.loc(n), v);
            }
            (self.gl.PixelStorei)(0x0cf5, 1);
            for (i, name) in names.into_iter().enumerate() {
                (self.gl.ActiveTexture)(GL_TEXTURE0 + i as u32);
                let stage = stages.iter().find(|s| s.unit == i);
                let mut id = self.white;
                let mut lod_limit = [1., 1., 0., 0.];
                if stage.is_some() {
                    if let Some(tex) = textures.get(&name).filter(|t| t.is_complete()) {
                        let item = self.textures.entry(name).or_insert_with(|| {
                            let mut id = 0;
                            (self.gl.GenTextures)(1, &mut id);
                            (id, u64::MAX)
                        });
                        id = item.0;
                        (self.gl.BindTexture)(GL_TEXTURE_2D, id);
                        let last_level = tex.width.max(tex.height).ilog2();
                        let restricted = tex.max_level < last_level;
                        let base_only = tex.max_level == 0
                            || (restricted && !self.native_max_level && !self.lod_derivatives);
                        lod_limit = [
                            tex.width as f32,
                            tex.height as f32,
                            tex.max_level.min(last_level) as f32,
                            (restricted && !self.native_max_level && !base_only) as u8 as f32,
                        ];
                        let needs_mips = !base_only
                            && tex.min_filter.uses_mipmaps()
                            && (self.full_npot
                                || (tex.width.is_power_of_two() && tex.height.is_power_of_two()));
                        let newly_mipped = needs_mips
                            && self
                                .texture_params
                                .get(&id)
                                .is_none_or(|p| !p.0.uses_mipmaps());
                        if item.1 != tex.revision || newly_mipped {
                            if self.native_max_level {
                                (self.gl.TexParameteri)(
                                    GL_TEXTURE_2D,
                                    GL_TEXTURE_MAX_LEVEL_SGIS,
                                    tex.max_level.min(last_level) as i32,
                                );
                            }
                            upload_texture(&self.gl, &self.compressed_formats, tex, needs_mips)?;
                            item.1 = tex.revision;
                        }
                        let params = if base_only {
                            (
                                match tex.min_filter {
                                    Filter::Nearest
                                    | Filter::NearestMipmapNearest
                                    | Filter::NearestMipmapLinear => Filter::Nearest,
                                    _ => Filter::Linear,
                                },
                                tex.mag_filter,
                                tex.wrap_s,
                                tex.wrap_t,
                            )
                        } else if !self.full_npot
                            && (!tex.width.is_power_of_two() || !tex.height.is_power_of_two())
                        {
                            (
                                match tex.min_filter {
                                    Filter::Nearest
                                    | Filter::NearestMipmapNearest
                                    | Filter::NearestMipmapLinear => Filter::Nearest,
                                    _ => Filter::Linear,
                                },
                                tex.mag_filter,
                                Wrap::ClampToEdge,
                                Wrap::ClampToEdge,
                            )
                        } else {
                            (tex.min_filter, tex.mag_filter, tex.wrap_s, tex.wrap_t)
                        };
                        if self.max_anisotropy > 1
                            && self.texture_anisotropy.get(&id) != Some(&self.anisotropy)
                        {
                            // The EXT/ARB parameter accepts integer values too.
                            (self.gl.TexParameteri)(GL_TEXTURE_2D, 0x84fe, self.anisotropy as i32);
                            self.texture_anisotropy.insert(id, self.anisotropy);
                        }
                        if self.texture_params.get(&id) != Some(&params) {
                            self.params(params.0, params.1, params.2, params.3);
                            self.texture_params.insert(id, params);
                        }
                    }
                }
                (self.gl.BindTexture)(GL_TEXTURE_2D, id);
                self.uniform_1i(self.loc(SAMPLERS[i]), i as i32);
                self.uniform_1i(
                    self.loc(ENVIRONMENTS[i]),
                    stage.map_or(-1, |s| match s.env {
                        TexEnvMode::Modulate => 0,
                        TexEnvMode::Replace => 1,
                        TexEnvMode::Decal => 2,
                        TexEnvMode::Blend => 3,
                        TexEnvMode::Add => 4,
                    }),
                );
                self.uniform_4fv(self.loc(LOD_LIMITS[i]), 1, lod_limit.as_ptr());
                self.uniform_1f(self.loc(LOD_BIASES[i]), stage.map_or(0.0, |s| s.lod_bias));
                if let Some(s) = stage {
                    self.uniform_4fv(self.loc(ENV_COLORS[i]), 1, s.env_color.as_ptr());
                }
            }
            (self.gl.BindBuffer)(GL_ARRAY_BUFFER, self.buffer);
            let (data, length) = stream
                .map(|s| (s.bytes.as_ptr().cast::<c_void>(), s.bytes.len()))
                .unwrap_or((self.scratch.as_ptr().cast(), self.scratch.len() * 4));
            (self.gl.BufferData)(GL_ARRAY_BUFFER, length as isize, data, 0x88e0);
            if let Some(stream) = stream {
                if self.stream_layout != Some(stream.attributes) {
                    for (i, attr) in stream.attributes.iter().enumerate() {
                        if let Some(attr) = attr {
                            (self.gl.EnableVertexAttribArray)(i as u32);
                            (self.gl.VertexAttribPointer)(
                                i as u32,
                                attr.size,
                                attr.ty,
                                attr.normalized,
                                attr.stride,
                                attr.offset as *const c_void,
                            );
                        } else {
                            (self.gl.DisableVertexAttribArray)(i as u32);
                        }
                    }
                    self.stream_layout = Some(stream.attributes);
                }
                for (i, attr) in stream.attributes.iter().enumerate() {
                    if attr.is_none() {
                        (self.gl.VertexAttrib4fv)(i as u32, stream.constants[i].as_ptr());
                    }
                }
                self.attribute_layout = None;
            } else {
                if self.attribute_layout != Some(layout) {
                    for (i, attribute) in layout.attributes.iter().enumerate() {
                        if let Some((size, offset)) = attribute {
                            (self.gl.EnableVertexAttribArray)(i as u32);
                            (self.gl.VertexAttribPointer)(
                                i as u32,
                                *size,
                                GL_FLOAT,
                                0,
                                (layout.floats * 4) as i32,
                                (*offset * 4) as *const c_void,
                            );
                        } else {
                            (self.gl.DisableVertexAttribArray)(i as u32);
                        }
                    }
                    self.attribute_layout = Some(layout);
                }
                self.stream_layout = None;
            }
            if indexed {
                let indices = stream
                    .map(|s| s.indices.as_slice())
                    .unwrap_or(&self.indices);
                (self.gl.BindBuffer)(GL_ELEMENT_ARRAY_BUFFER, self.index_buffer);
                (self.gl.BufferData)(
                    GL_ELEMENT_ARRAY_BUFFER,
                    (indices.len() * 2) as isize,
                    indices.as_ptr().cast(),
                    0x88e0,
                );
                (self.gl.DrawElements)(
                    draw_mode,
                    indices.len() as i32,
                    GL_UNSIGNED_SHORT,
                    std::ptr::null(),
                );
            } else {
                (self.gl.DrawArrays)(
                    draw_mode,
                    0,
                    stream.map_or(self.scratch.len() / layout.floats, |s| s.count) as i32,
                );
            }
            self.dirty = true;
            self.check()
        }
    }
    pub fn read(&mut self, target: &mut RenderTarget) -> Result<(), String> {
        self.read_inner(target, None)
    }
    pub fn read_present(
        &mut self,
        target: &mut RenderTarget,
        rgb565: &mut [u8],
    ) -> Result<(), String> {
        self.read_inner(target, Some(rgb565))
    }
    fn read_inner(
        &mut self,
        target: &mut RenderTarget,
        mut rgb565: Option<&mut [u8]>,
    ) -> Result<(), String> {
        self.resize(target.width, target.height)?;
        if !self.dirty {
            if let Some(out) = rgb565 {
                target.to_rgb565(out);
            }
            return Ok(());
        }
        self.host.make_current()?;
        unsafe {
            self.readback
                .resize((self.width * self.height * 4) as usize, 0);
            (self.gl.BindFramebuffer)(0x8d40, self.fbo);
            (self.gl.PixelStorei)(0x0d05, 1);
            (self.gl.ReadPixels)(
                0,
                0,
                self.width as i32,
                self.height as i32,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                self.readback.as_mut_ptr().cast(),
            );
            self.check()?;
            let stride = self.width as usize * 4;
            for (row, out) in target.color.chunks_exact_mut(stride).enumerate() {
                let start = (self.height as usize - 1 - row) * stride;
                let source = &self.readback[start..start + stride];
                let rgb_row = rgb565.as_deref_mut().and_then(|pixels| {
                    let offset = row * self.width as usize * 2;
                    let end = (offset + self.width as usize * 2).min(pixels.len());
                    pixels.get_mut(offset..end)
                });
                if let Some(rgb_row) = rgb_row.filter(|row| row.len() == stride / 2) {
                    // One traversal produces both caches, preserving the RGBA image
                    // needed by glReadPixels and copy-to-texture calls after a swap.
                    for ((src, rgba), rgb) in source
                        .chunks_exact(4)
                        .zip(out.chunks_exact_mut(4))
                        .zip(rgb_row.chunks_exact_mut(2))
                    {
                        rgba.copy_from_slice(src);
                        let value = ((src[0] as u16 >> 3) << 11)
                            | ((src[1] as u16 >> 2) << 5)
                            | (src[2] as u16 >> 3);
                        rgb.copy_from_slice(&value.to_le_bytes());
                    }
                } else {
                    out.copy_from_slice(source);
                    // Match the existing converter when a caller supplies a short output.
                    if let Some(pixels) = rgb565.as_deref_mut() {
                        let offset = row * self.width as usize * 2;
                        if let Some(tail) = pixels.get_mut(offset..) {
                            for (src, rgb) in source.chunks_exact(4).zip(tail.chunks_exact_mut(2)) {
                                let value = ((src[0] as u16 >> 3) << 11)
                                    | ((src[1] as u16 >> 2) << 5)
                                    | (src[2] as u16 >> 3);
                                rgb.copy_from_slice(&value.to_le_bytes());
                            }
                        }
                    }
                }
            }
            self.dirty = false;
            Ok(())
        }
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        if self.host.make_current().is_err() {
            return;
        }
        unsafe {
            for (id, _) in self.textures.values() {
                (self.gl.DeleteTextures)(1, id);
            }
            for id in [self.color, self.white] {
                (self.gl.DeleteTextures)(1, &id);
            }
            (self.gl.DeleteRenderbuffers)(1, &self.depth);
            (self.gl.DeleteFramebuffers)(1, &self.fbo);
            (self.gl.DeleteBuffers)(1, &self.buffer);
            (self.gl.DeleteBuffers)(1, &self.index_buffer);
            (self.gl.DeleteProgram)(self.program);
        }
    }
}
fn compare(v: CompareFunc) -> u32 {
    GL_NEVER
        + match v {
            CompareFunc::Never => 0,
            CompareFunc::Less => 1,
            CompareFunc::Equal => 2,
            CompareFunc::Lequal => 3,
            CompareFunc::Greater => 4,
            CompareFunc::Notequal => 5,
            CompareFunc::Gequal => 6,
            CompareFunc::Always => 7,
        }
}
fn blend(v: BlendFactor) -> u32 {
    match v {
        BlendFactor::Zero => GL_ZERO,
        BlendFactor::One => GL_ONE,
        BlendFactor::SrcColor => GL_SRC_COLOR,
        BlendFactor::OneMinusSrcColor => GL_ONE_MINUS_SRC_COLOR,
        BlendFactor::SrcAlpha => GL_SRC_ALPHA,
        BlendFactor::OneMinusSrcAlpha => GL_ONE_MINUS_SRC_ALPHA,
        BlendFactor::DstAlpha => GL_DST_ALPHA,
        BlendFactor::OneMinusDstAlpha => GL_ONE_MINUS_DST_ALPHA,
        BlendFactor::DstColor => GL_DST_COLOR,
        BlendFactor::OneMinusDstColor => GL_ONE_MINUS_DST_COLOR,
        BlendFactor::SrcAlphaSaturate => GL_SRC_ALPHA_SATURATE,
    }
}
fn filter(v: Filter) -> u32 {
    match v {
        Filter::Nearest => GL_NEAREST,
        Filter::Linear => GL_LINEAR,
        Filter::NearestMipmapNearest => GL_NEAREST_MIPMAP_NEAREST,
        Filter::LinearMipmapNearest => GL_LINEAR_MIPMAP_NEAREST,
        Filter::NearestMipmapLinear => GL_NEAREST_MIPMAP_LINEAR,
        Filter::LinearMipmapLinear => GL_LINEAR_MIPMAP_LINEAR,
    }
}
fn wrap(v: Wrap) -> u32 {
    match v {
        Wrap::Repeat => GL_REPEAT,
        Wrap::ClampToEdge => GL_CLAMP_TO_EDGE,
    }
}
const VERTEX: &str = r#"
attribute vec4 position,color;attribute vec2 tex0,tex1,tex2,tex3;attribute vec4 normalAndFog,lightPosition;
VARYING vec4 vColor,vBackColor;VARYING vec2 uv0,uv1,uv2,uv3;VARYING float vFog;uniform vec2 depthRange;
uniform mat4 projectionMatrix,modelviewMatrix,textureMatrix0,textureMatrix1,textureMatrix2,textureMatrix3;
uniform int objectSpace,lightingEnabled,normalMode,colorMaterial,twoSided;
uniform mat4 normalMatrix;
uniform float normalRescale,shininess,pointSize;
uniform vec4 modelAmbient,materialAmbient,materialDiffuse,materialSpecular,materialEmission;
uniform vec4 lightAmbient[8],lightDiffuse[8],lightSpecular[8],lightPositions[8],lightSpot[8],lightAttenuation[8];
vec3 unit(vec3 v){float l=length(v);return l>0.0?v/l:vec3(0.0);}
vec4 illuminate(vec3 n,vec3 point){
 vec4 ma=colorMaterial!=0?color:materialAmbient;
 vec4 md=colorMaterial!=0?color:materialDiffuse;
 vec3 result=materialEmission.rgb+modelAmbient.rgb*ma.rgb;
 for(int i=0;i<8;i++){
  if(lightAmbient[i].a>0.5){
   vec3 delta=lightPositions[i].w==0.0?lightPositions[i].xyz:lightPositions[i].xyz/lightPositions[i].w-point;
   float distance=length(delta);vec3 l=unit(delta);float attenuation=1.0;
   if(lightPositions[i].w!=0.0){vec3 k=lightAttenuation[i].xyz;attenuation=1.0/max(k.x+k.y*distance+k.z*distance*distance,0.000001);}
   float spot=dot(unit(lightSpot[i].xyz),-l);
   if(lightSpot[i].w>=0.0){if(spot<lightSpot[i].w){attenuation=0.0;}else if(lightAttenuation[i].w>0.0){attenuation*=pow(max(spot,0.0),lightAttenuation[i].w);}}
   float diffuse=max(dot(n,l),0.0);float spec=0.0;
   if(diffuse>0.0){float nh=max(dot(n,unit(l+vec3(0.0,0.0,1.0))),0.0);spec=shininess==0.0?1.0:pow(nh,shininess);}
   result+=attenuation*(lightAmbient[i].rgb*ma.rgb+diffuse*lightDiffuse[i].rgb*md.rgb+spec*lightSpecular[i].rgb*materialSpecular.rgb);
  }
 }
 return vec4(clamp(result,0.0,1.0),clamp(md.a,0.0,1.0));
}
void main(){
 gl_PointSize=pointSize;
 vec4 eye=modelviewMatrix*position;
 vec4 clip=projectionMatrix*eye;
 gl_Position=clip;
 gl_Position.z=(depthRange.y-depthRange.x)*clip.z+(depthRange.x+depthRange.y-1.0)*clip.w;
 vColor=color;vBackColor=color;
 if(lightingEnabled!=0){
  vec3 n=(normalMatrix*vec4(normalAndFog.xyz,0.0)).xyz;
  if(normalMode==1){n=unit(n);}else if(normalMode==2){n*=normalRescale;}
  vec4 lightEye=modelviewMatrix*lightPosition;
  vec3 point=lightEye.w==0.0?lightEye.xyz:lightEye.xyz/lightEye.w;
  vColor=illuminate(n,point);vBackColor=twoSided!=0?illuminate(-n,point):vColor;
 }
 uv0=(textureMatrix0*vec4(tex0,0.0,1.0)).xy;
 uv1=(textureMatrix1*vec4(tex1,0.0,1.0)).xy;
 uv2=(textureMatrix2*vec4(tex2,0.0,1.0)).xy;
 uv3=(textureMatrix3*vec4(tex3,0.0,1.0)).xy;
 vFog=objectSpace!=0?abs(eye.z):normalAndFog.w;
}
"#;
const FRAGMENT: &str = r#"
VARYING vec4 vColor,vBackColor;VARYING vec2 uv0,uv1,uv2,uv3;VARYING float vFog;
uniform vec4 lodLimit0,lodLimit1,lodLimit2,lodLimit3;
uniform float lodBias0,lodBias1,lodBias2,lodBias3;uniform sampler2D sampler0,sampler1,sampler2,sampler3;uniform int env0,env1,env2,env3;uniform vec4 envColor0,envColor1,envColor2,envColor3;
uniform int alphaMode,fogMode,twoSided;uniform float alphaRef,fogStart,fogEnd,fogDensity;uniform vec4 fogColor;
vec4 stage(vec4 c,vec4 t,int e,vec4 k){if(e==0)return c*t;if(e==1)return t;if(e==2)return vec4(mix(c.rgb,t.rgb,t.a),c.a);if(e==3)return vec4(mix(c.rgb,k.rgb,t.rgb),c.a*t.a);if(e==4)return vec4(min(c.rgb+t.rgb,vec3(1.0)),c.a*t.a);return c;}
float limitedBias(vec2 uv,float bias,vec4 limit){
#ifdef LOD_DERIVATIVES
 if(limit.w>0.5){
  vec2 dx=dFdx(uv)*limit.xy;vec2 dy=dFdy(uv)*limit.xy;
  float rho=max(length(dx),length(dy));
  float lod=log2(max(rho,0.000001));
  bias=min(bias,limit.z-lod);
 }
#endif
 return bias;
}
void main(){vec4 c=twoSided!=0&&!gl_FrontFacing?vBackColor:vColor;if(env0>=0)c=stage(c,texture2D(sampler0,uv0,limitedBias(uv0,lodBias0,lodLimit0)),env0,envColor0);if(env1>=0)c=stage(c,texture2D(sampler1,uv1,limitedBias(uv1,lodBias1,lodLimit1)),env1,envColor1);if(env2>=0)c=stage(c,texture2D(sampler2,uv2,limitedBias(uv2,lodBias2,lodLimit2)),env2,envColor2);if(env3>=0)c=stage(c,texture2D(sampler3,uv3,limitedBias(uv3,lodBias3,lodLimit3)),env3,envColor3);
float alpha=floor(clamp(c.a,0.0,1.0)*255.0+0.5);
if(alphaMode==0||(alphaMode==1&&alpha>=alphaRef)||(alphaMode==2&&alpha!=alphaRef)||(alphaMode==3&&alpha>alphaRef)||(alphaMode==4&&alpha<=alphaRef)||(alphaMode==5&&alpha==alphaRef)||(alphaMode==6&&alpha<alphaRef))discard;
// Carmageddon uses decreasing negative linear-fog endpoints. Preserve the
// signed span, and match software's unfogged result for a degenerate range.
float f=1.0;if(fogMode==1){float span=fogEnd-fogStart;if(span!=0.0 && abs(span)>=0.000000001)f=(fogEnd-vFog)/span;}if(fogMode==2)f=exp(-fogDensity*vFog);if(fogMode==3){float d=fogDensity*vFog;f=exp(-d*d);}c.rgb=mix(fogColor.rgb,c.rgb,clamp(f,0.0,1.0));gl_FragColor=c;}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster;
    #[cfg(unix)]
    #[test]
    #[ignore = "requires native desktop GL and GLES2; upload routes and driver fallbacks"]
    fn host_upload_routes_and_fallbacks() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 8, 8).unwrap()
            } else {
                Renderer::new(8, 8).unwrap()
            };
            gpu.host.make_current().unwrap();
            unsafe {
                let mut name = 0;
                (gpu.gl.GenTextures)(1, &mut name);
                (gpu.gl.BindTexture)(GL_TEXTURE_2D, name);
                let mut tex = Texture::default();
                tex.set_level(0, 4, 4, [255, 0, 0, 255].repeat(16));
                assert!(matches!(
                    upload_texture(&gpu.gl, &gpu.compressed_formats, &tex, true).unwrap(),
                    UploadRoute::Generated
                ));
                let generate = gpu.gl.GenerateMipmap.take();
                assert!(matches!(
                    upload_texture(&gpu.gl, &gpu.compressed_formats, &tex, true).unwrap(),
                    UploadRoute::Rgba
                ));
                gpu.gl.GenerateMipmap = generate;
                tex.set_level(1, 2, 2, [0, 255, 0, 255].repeat(4));
                assert!(matches!(
                    upload_texture(&gpu.gl, &gpu.compressed_formats, &tex, true).unwrap(),
                    UploadRoute::Rgba
                ));
                let bytes = [0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0];
                tex = Texture::default();
                tex.set_compressed(
                    0,
                    4,
                    4,
                    GL_COMPRESSED_RGBA_S3TC_DXT1_EXT,
                    &bytes,
                    Vec::new(),
                );
                assert!(tex.rgba.is_empty());
                assert_eq!(&*tex.rgba_level(0).unwrap(), &[255, 0, 0, 255].repeat(16));
                if gpu.supports_compressed(GL_COMPRESSED_RGBA_S3TC_DXT1_EXT) {
                    assert!(matches!(
                        upload_texture(&gpu.gl, &gpu.compressed_formats, &tex, false).unwrap(),
                        UploadRoute::Compressed
                    ));
                }
                assert!(matches!(
                    upload_texture(&gpu.gl, &HashSet::new(), &tex, false).unwrap(),
                    UploadRoute::Rgba
                ));
                let upload = gpu.gl.CompressedTexImage2D.take();
                assert!(matches!(
                    upload_texture(&gpu.gl, &gpu.compressed_formats, &tex, false).unwrap(),
                    UploadRoute::Rgba
                ));
                gpu.gl.CompressedTexImage2D = upload;
                // Force an advertised format the driver does not actually accept.
                // A refused upload must fall back without leaving a GL error.
                if !gpu.supports_compressed(GL_ATC_RGB_AMD) {
                    let mut refused = Texture::default();
                    refused.set_compressed(0, 4, 4, GL_ATC_RGB_AMD, &bytes, Vec::new());
                    let formats = HashSet::from([GL_ATC_RGB_AMD]);
                    assert!(matches!(
                        upload_texture(&gpu.gl, &formats, &refused, false).unwrap(),
                        UploadRoute::Rgba
                    ));
                    assert_eq!((gpu.gl.GetError)(), 0);
                }
                tex.ensure_rgba();
                assert_eq!(tex.rgba, [255, 0, 0, 255].repeat(16));
                tex.set_level(0, 4, 4, [0, 0, 255, 255].repeat(16));
                assert!(tex.compressed.is_empty());
                assert_eq!((gpu.gl.GetError)(), 0);
                (gpu.gl.DeleteTextures)(1, &name);
            }
        }
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires native GL/GLES2; anisotropy cap, cache and reset"]
    fn host_anisotropy_respects_driver_limit_and_returns_to_off() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 8, 8).unwrap()
            } else {
                Renderer::new(8, 8).unwrap()
            };
            let mut texture = Texture::default();
            texture.set_level(0, 4, 4, [255, 0, 0, 255].repeat(16));
            let textures = HashMap::from([(1, texture)]);
            let stage = TextureStage {
                unit: 0,
                env: TexEnvMode::Replace,
                env_color: [0.; 4],
                lod_bias: 0.,
            };
            let target = RenderTarget::new(8, 8);
            let state = PipelineState {
                viewport: (0, 0, 8, 8),
                ..Default::default()
            };
            for level in [0, 2, 4, 8, 0, 255] {
                gpu.set_anisotropic_filtering(level);
                let expected = match level {
                    2 | 4 | 8 => level,
                    _ => 1,
                }
                .min(gpu.max_anisotropy);
                assert_eq!(gpu.anisotropy, expected);
                gpu.draw(
                    &target,
                    &state,
                    GL_TRIANGLES,
                    &triangle([1.; 4], 0.),
                    &[stage],
                    &textures,
                    [1, 0, 0, 0],
                )
                .unwrap();
                let id = gpu.textures[&1].0;
                if gpu.max_anisotropy > 1 {
                    assert_eq!(gpu.texture_anisotropy[&id], expected);
                    unsafe {
                        let ptr = gpu.host.load(c"glGetTexParameterfv");
                        assert!(!ptr.is_null());
                        let get: unsafe extern "system" fn(u32, u32, *mut f32) =
                            std::mem::transmute(ptr);
                        (gpu.gl.ActiveTexture)(GL_TEXTURE0);
                        (gpu.gl.BindTexture)(GL_TEXTURE_2D, id);
                        let mut actual = 0.;
                        get(GL_TEXTURE_2D, 0x84fe, &mut actual);
                        assert_eq!(actual, expected as f32);
                        gpu.check().unwrap();
                    }
                } else {
                    assert!(gpu.texture_anisotropy.is_empty());
                }
            }
            gpu.delete_textures(&[1]);
            assert!(gpu.texture_anisotropy.is_empty());
            // Simulate an unsupported adapter: no extension parameter is emitted.
            gpu.max_anisotropy = 1;
            gpu.set_anisotropic_filtering(8);
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &triangle([1.; 4], 0.),
                &[stage],
                &textures,
                [1, 0, 0, 0],
            )
            .unwrap();
            assert_eq!(gpu.anisotropy, 1);
            assert!(gpu.texture_anisotropy.is_empty());
        }
    }
    fn identity_transforms() -> VertexTransforms {
        VertexTransforms {
            lighting: Default::default(),
            normal_matrix: IDENTITY,
            projection: IDENTITY,
            modelview: IDENTITY,
            textures: [IDENTITY; 4],
        }
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires native desktop GL and GLES2; mip/LOD and direct native indices"]
    fn host_mips_lod_bias_and_direct_guest_indices() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            let mut target = RenderTarget::new(32, 32);
            let state = PipelineState {
                viewport: (0, 0, 32, 32),
                ..Default::default()
            };
            let positions = [
                [-1., -1., 0., 1.],
                [1., -1., 0., 1.],
                [-1., 1., 0., 1.],
                [1., 1., 0., 1.],
            ];
            let uv = [[0., 0.], [1., 0.], [0., 1.], [1., 1.]];
            let vertices: Vec<_> = positions
                .iter()
                .zip(uv)
                .map(|(p, t)| {
                    let mut v = triangle([1.; 4], 0.)[0];
                    v.pos = *p;
                    v.texcoord[0] = t;
                    v
                })
                .collect();
            let mut texture = Texture::default();
            texture.set_level(0, 64, 64, [255, 0, 0, 255].repeat(64 * 64));
            texture.set_level(1, 32, 32, [0, 255, 0, 255].repeat(32 * 32));
            texture.set_level(2, 16, 16, [0, 0, 255, 255].repeat(16 * 16));
            texture.min_filter = Filter::NearestMipmapNearest;
            texture.mag_filter = Filter::Nearest;
            let mut textures = HashMap::from([(1, texture)]);
            let mut stage = TextureStage {
                unit: 0,
                env: TexEnvMode::Replace,
                env_color: [0.; 4],
                lod_bias: 0.,
            };
            for (bias, expected) in [
                (0., [0, 255, 0, 255]),
                (1., [0, 0, 255, 255]),
                (-1., [255, 0, 0, 255]),
            ] {
                stage.lod_bias = bias;
                gpu.draw_indexed_object_space(
                    &target,
                    &state,
                    GL_TRIANGLE_STRIP,
                    &vertices,
                    &[0, 1, 2, 3],
                    &[stage],
                    &textures,
                    [1, 0, 0, 0],
                    &identity_transforms(),
                )
                .unwrap();
                assert_eq!(gpu.scratch.len(), 4 * 10);
                assert_eq!(gpu.indices, [0, 1, 2, 3]);
                gpu.read(&mut target).unwrap();
                assert_eq!(
                    &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                    &expected,
                    "es={es} bias={bias}"
                );
            }
            // Native max-level and GLES2 derivative fallback must cap the
            // selected authored level, and permit it again when the cap grows.
            let native_max_level = gpu.native_max_level;
            for native_cap in [true, false] {
                gpu.native_max_level = native_cap;
                if !native_cap {
                    let id = gpu.textures[&1].0;
                    unsafe {
                        (gpu.gl.ActiveTexture)(GL_TEXTURE0);
                        (gpu.gl.BindTexture)(GL_TEXTURE_2D, id);
                        (gpu.gl.TexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL_SGIS, 1000);
                    }
                }
                for min_filter in [Filter::NearestMipmapNearest, Filter::LinearMipmapLinear] {
                    for (cap, expected) in [
                        (0, [255, 0, 0, 255]),
                        (1, [0, 255, 0, 255]),
                        (1000, [0, 0, 255, 255]),
                        (1, [0, 255, 0, 255]),
                    ] {
                        let texture = textures.get_mut(&1).unwrap();
                        texture.min_filter = min_filter;
                        texture.set_parameter(GL_TEXTURE_MAX_LEVEL_SGIS, cap);
                        stage.lod_bias = 1.;
                        gpu.draw_indexed_object_space(
                            &target,
                            &state,
                            GL_TRIANGLE_STRIP,
                            &vertices,
                            &[0, 1, 2, 3],
                            &[stage],
                            &textures,
                            [1, 0, 0, 0],
                            &identity_transforms(),
                        )
                        .unwrap();
                        gpu.read(&mut target).unwrap();
                        let i = (16 * 32 + 16) * 4;
                        for (actual, expected) in target.color[i..i + 4].iter().zip(expected) {
                            assert!(
                                actual.abs_diff(expected) <= 1,
                                "es={es} native={native_cap} cap={cap} filter={min_filter:?}"
                            );
                        }
                    }
                }
            }
            // Old ES2 without derivatives retains base filtering rather than
            // sampling beyond a cap it cannot represent.
            let derivatives = gpu.lod_derivatives;
            gpu.lod_derivatives = false;
            gpu.draw_indexed_object_space(
                &target,
                &state,
                GL_TRIANGLE_STRIP,
                &vertices,
                &[0, 1, 2, 3],
                &[stage],
                &textures,
                [1, 0, 0, 0],
                &identity_transforms(),
            )
            .unwrap();
            gpu.read(&mut target).unwrap();
            let i = (16 * 32 + 16) * 4;
            assert_eq!(&target.color[i..i + 4], &[255, 0, 0, 255]);
            gpu.lod_derivatives = derivatives;
            gpu.native_max_level = native_max_level;
            textures.get_mut(&1).unwrap().min_filter = Filter::NearestMipmapNearest;
            textures
                .get_mut(&1)
                .unwrap()
                .set_parameter(GL_TEXTURE_MAX_LEVEL_SGIS, 1000);
            textures
                .get_mut(&1)
                .unwrap()
                .set_level(1, 32, 32, [255, 255, 0, 255].repeat(32 * 32));
            stage.lod_bias = 0.;
            gpu.draw_indexed_object_space(
                &target,
                &state,
                GL_TRIANGLE_STRIP,
                &vertices,
                &[0, 1, 2, 3],
                &[stage],
                &textures,
                [1, 0, 0, 0],
                &identity_transforms(),
            )
            .unwrap();
            gpu.read(&mut target).unwrap();
            assert_eq!(
                &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                &[255, 255, 0, 255]
            );
            let tex = textures.get_mut(&1).unwrap();
            tex.min_filter = Filter::Nearest;
            tex.set_level(1, 32, 32, [0, 255, 255, 255].repeat(32 * 32));
            gpu.draw_indexed_object_space(
                &target,
                &state,
                GL_TRIANGLE_STRIP,
                &vertices,
                &[0, 1, 2, 3],
                &[stage],
                &textures,
                [1, 0, 0, 0],
                &identity_transforms(),
            )
            .unwrap();
            gpu.read(&mut target).unwrap();
            assert_eq!(
                &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                &[255, 0, 0, 255]
            );
            textures.get_mut(&1).unwrap().min_filter = Filter::NearestMipmapNearest;
            gpu.draw_indexed_object_space(
                &target,
                &state,
                GL_TRIANGLE_STRIP,
                &vertices,
                &[0, 1, 2, 3],
                &[stage],
                &textures,
                [1, 0, 0, 0],
                &identity_transforms(),
            )
            .unwrap();
            gpu.read(&mut target).unwrap();
            assert_eq!(
                &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                &[0, 255, 255, 255]
            );
            if es {
                gpu.full_npot = false; // Exercise core-ES2 fallback even on a capable Mesa driver.
                textures
                    .get_mut(&1)
                    .unwrap()
                    .set_level(0, 3, 5, [255, 0, 0, 255].repeat(15));
                gpu.draw_indexed_object_space(
                    &target,
                    &state,
                    GL_TRIANGLE_STRIP,
                    &vertices,
                    &[0, 1, 2, 3],
                    &[stage],
                    &textures,
                    [1, 0, 0, 0],
                    &identity_transforms(),
                )
                .unwrap();
                let id = gpu.textures[&1].0;
                assert_eq!(
                    gpu.texture_params[&id],
                    (
                        Filter::Nearest,
                        Filter::Nearest,
                        Wrap::ClampToEdge,
                        Wrap::ClampToEdge
                    )
                );
                gpu.read(&mut target).unwrap();
                assert_eq!(
                    &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                    &[255, 0, 0, 255]
                );
            }
            assert!(gpu
                .draw_indexed_object_space(
                    &target,
                    &state,
                    GL_TRIANGLES,
                    &vertices,
                    &[9],
                    &[],
                    &textures,
                    [0; 4],
                    &identity_transforms()
                )
                .is_err());
        }
    }

    #[test]
    fn compact_layout_keeps_only_active_attributes_and_flat_lighting_data() {
        let mut state = PipelineState::default();
        let mut transforms = identity_transforms();
        assert_eq!(
            VertexLayout::new(&state, &[], Some(&transforms)).floats * 4,
            32
        );
        let stage = TextureStage {
            unit: 2,
            env: TexEnvMode::Modulate,
            env_color: [0.; 4],
            lod_bias: 0.0,
        };
        let layout = VertexLayout::new(&state, &[stage], Some(&transforms));
        assert_eq!(layout.floats * 4, 40);
        assert!(layout.attributes[2].is_none());
        assert_eq!(layout.attributes[4], Some((2, 8)));
        transforms.lighting.enabled = true;
        let layout = VertexLayout::new(&state, &[stage], Some(&transforms));
        assert_eq!(layout.floats * 4, 56);
        assert_eq!(layout.attributes[7], Some((4, 0)));
        state.smooth_shading = false;
        let layout = VertexLayout::new(&state, &[stage], Some(&transforms));
        assert_eq!(layout.floats * 4, 72);
        let vertex = Vertex {
            pos: [1., 2., 3., 4.],
            normal: [1., 0., 0.],
            color: [1.; 4],
            ..Default::default()
        };
        let provoking = Vertex {
            pos: [5., 6., 7., 8.],
            normal: [0., 1., 0.],
            color: [0.5; 4],
            ..vertex
        };
        let mut packed = Vec::new();
        layout.pack(&mut packed, vertex, provoking, false);
        assert_eq!(&packed[4..8], &provoking.color);
        assert_eq!(&packed[10..13], &provoking.normal);
        assert_eq!(&packed[14..18], &provoking.pos);
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires host desktop GL and GLES2; direct strips/fans and fused display readback"]
    fn host_direct_arrays_compact_layout_and_fused_present_match_reference() {
        let transforms = identity_transforms();
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            let mut target = RenderTarget::new(32, 32);
            let positions = [
                [-0.75, -0.75, 0., 1.],
                [0.75, -0.75, 0., 1.],
                [-0.75, 0.75, 0., 1.],
                [0.75, 0.75, 0., 1.],
            ];
            let colors = [
                [1., 0., 0., 1.],
                [0., 1., 0., 1.],
                [0., 0., 1., 1.],
                [1., 1., 0., 1.],
            ];
            let vertices: [Vertex; 4] = std::array::from_fn(|i| Vertex {
                pos: positions[i],
                color: colors[i],
                ..Default::default()
            });
            let mut state = PipelineState {
                viewport: (0, 0, 32, 32),
                ..Default::default()
            };
            for smooth in [true, false] {
                state.smooth_shading = smooth;
                for mode in [GL_TRIANGLE_STRIP, GL_TRIANGLE_FAN] {
                    let vertices = if mode == GL_TRIANGLE_FAN {
                        [vertices[0], vertices[1], vertices[3], vertices[2]]
                    } else {
                        vertices
                    };
                    gpu.clear(32, 32, &state, [0.; 4], 1., GL_COLOR_BUFFER_BIT)
                        .unwrap();
                    gpu.draw_object_space(
                        &target,
                        &state,
                        mode,
                        &vertices,
                        &[],
                        &HashMap::new(),
                        [0; 4],
                        &transforms,
                    )
                    .unwrap();
                    gpu.read(&mut target).unwrap();
                    let reference = target.color.clone();
                    gpu.clear(32, 32, &state, [0.; 4], 1., GL_COLOR_BUFFER_BIT)
                        .unwrap();
                    gpu.draw_arrays_object_space(
                        &target,
                        &state,
                        mode,
                        &vertices,
                        &[],
                        &HashMap::new(),
                        [0; 4],
                        &transforms,
                    )
                    .unwrap();
                    if smooth {
                        assert_eq!(gpu.scratch.len() * 4, 128);
                        assert!(gpu.indices.is_empty());
                    }
                    let mut rgb = vec![0; 32 * 32 * 2];
                    gpu.read_present(&mut target, &mut rgb).unwrap();
                    for (a, b) in reference.iter().zip(&target.color) {
                        assert!(a.abs_diff(*b) <= 1, "es={es} mode={mode} smooth={smooth}");
                    }
                    let mut expected = vec![0; rgb.len()];
                    target.to_rgb565(&mut expected);
                    assert_eq!(rgb, expected);
                    rgb.fill(0);
                    gpu.read_present(&mut target, &mut rgb).unwrap(); // Clean-cache path.
                    assert_eq!(rgb, expected);
                    gpu.read(&mut target).unwrap(); // Canonical cache remains valid.
                }
            }
            state.smooth_shading = true;
            let stage = TextureStage {
                unit: 2,
                env: TexEnvMode::Modulate,
                env_color: [0.; 4],
                lod_bias: 0.0,
            };
            gpu.draw_arrays_object_space(
                &target,
                &state,
                GL_TRIANGLE_STRIP,
                &vertices,
                &[stage],
                &HashMap::new(),
                [0; 4],
                &transforms,
            )
            .unwrap();
            assert_eq!(gpu.scratch.len() * 4, 160); // Four 40-byte records, no indices.
            assert!(gpu.indices.is_empty());
            gpu.draw_arrays_object_space(
                &target,
                &state,
                GL_TRIANGLE_STRIP,
                &vertices,
                &[],
                &HashMap::new(),
                [0; 4],
                &transforms,
            )
            .unwrap();
            assert_eq!(gpu.scratch.len() * 4, 128); // Drop the texture attribute again.
            state.viewport = (0, 0, 7, 5);
            state.scissor = None;
            gpu.clear(7, 5, &state, [1., 0., 0., 1.], 1., GL_COLOR_BUFFER_BIT)
                .unwrap();
            state.scissor = Some((0, 0, 7, 2));
            gpu.clear(7, 5, &state, [0., 0., 1., 1.], 1., GL_COLOR_BUFFER_BIT)
                .unwrap();
            target.resize(7, 5);
            let mut short = vec![0x55; 7 * 5 * 2 - 1];
            gpu.read_present(&mut target, &mut short).unwrap();
            let mut expected = vec![0x55; short.len()];
            target.to_rgb565(&mut expected);
            assert_eq!(short, expected);
            assert_eq!(short.last(), Some(&0x55));
            assert_eq!(&target.color[..4], &[255, 0, 0, 255]);
            assert_eq!(&target.color[7 * 4 * 4..7 * 4 * 4 + 4], &[0, 0, 255, 255]);
        }
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires desktop GL and GLES2; indexed transfers and cache invalidation"]
    fn host_indexed_transfers_uniform_cache_and_polygon_offset() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            let mut target = RenderTarget::new(32, 32);
            let mut state = PipelineState {
                viewport: (0, 0, 32, 32),
                depth_test: true,
                ..Default::default()
            };
            let tri = triangle([1., 0., 0., 1.], 0.);
            let vertices: Vec<_> = tri.into_iter().cycle().take(36).collect();
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &vertices,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            assert_eq!(gpu.scratch.len(), 3 * 8);
            assert_eq!(gpu.indices.len(), 36);
            let colors_indices = [
                0, 2, 1, 2, 0, 3, 1, 5, 4, 5, 1, 2, 5, 3, 6, 3, 5, 2, 3, 7, 6, 7, 3, 0, 6, 4, 5, 4,
                6, 7, 1, 7, 0, 7, 1, 4,
            ];
            let cube: [Vertex; 8] = std::array::from_fn(|i| Vertex {
                pos: [
                    if i & 1 == 0 { -0.5 } else { 0.5 },
                    if i & 2 == 0 { -0.5 } else { 0.5 },
                    if i & 4 == 0 { -0.3 } else { 0.3 },
                    1.,
                ],
                color: [1., 0., 0., 1.],
                ..Default::default()
            });
            let mesh: Vec<_> = colors_indices.iter().map(|&i| cube[i]).collect();
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &mesh,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            assert_eq!(gpu.scratch.len() * 4 + gpu.indices.len() * 2, 328);
            gpu.clear(32, 32, &state, [0.; 4], 1., GL_DEPTH_BUFFER_BIT)
                .unwrap();
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &vertices,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            let uniforms = gpu.uniform_values.clone();
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &vertices,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            assert_eq!(gpu.uniform_values, uniforms);
            assert!(!gpu.changed_uniform(gpu.loc("pointSize"), &[1f32.to_bits()]));
            gpu.read(&mut target).unwrap();
            assert_eq!(
                &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                &[255, 0, 0, 255]
            );
            state.color_mask = [false; 4];
            gpu.clear(32, 32, &state, [0.; 4], 1., GL_COLOR_BUFFER_BIT)
                .unwrap();
            assert!(gpu.pipeline.is_none());
            state.color_mask = [true; 4];
            state.polygon_offset = true;
            state.polygon_units = -2.;
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &triangle([0., 1., 0., 1.], 0.),
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            gpu.read(&mut target).unwrap();
            assert_eq!(
                &target.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                &[0, 255, 0, 255]
            );
            gpu.resize(16, 16).unwrap();
            assert!(gpu.pipeline.is_none());
        }
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires host OpenGL; checks points and line modes with both shader dialects"]
    fn host_points_lines_match_software_and_preserve_triangle_state() {
        use crate::raster;
        let vertices = [
            Vertex {
                pos: [-0.65, 0.4, 0., 1.],
                color: [1., 0.2, 0.1, 0.5],
                ..Default::default()
            },
            Vertex {
                pos: [0.3, 0.4, 0., 1.],
                color: [0.1, 1., 0.2, 0.5],
                ..Default::default()
            },
            Vertex {
                pos: [0.3, -0.6, 0., 1.],
                color: [0.2, 0.1, 1., 0.5],
                ..Default::default()
            },
        ];
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            for mode in [GL_POINTS, GL_LINES, GL_LINE_STRIP, GL_LINE_LOOP] {
                for (size, smooth) in [1f32, 2., 3., 5.]
                    .into_iter()
                    .flat_map(|size| [(size, true), (size, false)])
                {
                    let mut state = PipelineState {
                        viewport: (0, 0, 32, 32),
                        smooth_shading: smooth,
                        point_size: size,
                        line_width: size.min(gpu.line_range[1]),
                        cull: Some(CullMode::FrontAndBack),
                        ..Default::default()
                    };
                    let mut draw_vertices = vertices;
                    // Wide-line edge interpolation differs across conforming drivers.
                    // Validate coverage/constant color for wide primitives and gradients at width 1.
                    if size > 1. {
                        for v in &mut draw_vertices {
                            v.color = [0.8, 0.4, 0.2, 0.5];
                        }
                    }
                    let mut cpu = RenderTarget::new(32, 32);
                    let mut native = RenderTarget::new(32, 32);
                    let sample = |_: usize, _: f32, _: f32| None;
                    gpu.clear(
                        32,
                        32,
                        &state,
                        [0.; 4],
                        1.,
                        GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
                    )
                    .unwrap();
                    match mode {
                        GL_POINTS => {
                            for v in draw_vertices {
                                raster::draw_point(&mut cpu, &state, &sample, &[], v);
                            }
                        }
                        GL_LINES => {
                            raster::draw_line(
                                &mut cpu,
                                &state,
                                &sample,
                                &[],
                                [draw_vertices[0], draw_vertices[1]],
                            );
                        }
                        _ => {
                            for v in draw_vertices.windows(2) {
                                raster::draw_line(&mut cpu, &state, &sample, &[], [v[0], v[1]]);
                            }
                            if mode == GL_LINE_LOOP {
                                raster::draw_line(
                                    &mut cpu,
                                    &state,
                                    &sample,
                                    &[],
                                    [draw_vertices[2], draw_vertices[0]],
                                );
                            }
                        }
                    }
                    gpu.draw(
                        &native,
                        &state,
                        mode,
                        &draw_vertices,
                        &[],
                        &HashMap::new(),
                        [0; 4],
                    )
                    .unwrap();
                    gpu.read(&mut native).unwrap();
                    let count =
                        |t: &RenderTarget| t.color.chunks_exact(4).filter(|p| p[3] > 0).count();
                    let a = count(&cpu);
                    let b = count(&native);
                    assert!(a > 0 && b > 0, "es={es}, mode={mode}, size={size}: {a}/{b}");
                    assert!(
                        a.abs_diff(b) <= vertices.len() * size as usize * 2,
                        "es={es}, mode={mode}, size={size}: coverage {a}/{b}"
                    );
                    for (a, b) in cpu
                        .color
                        .chunks_exact(4)
                        .zip(native.color.chunks_exact(4))
                        .filter(|(a, b)| a[3] > 0 && b[3] > 0)
                    {
                        for (&a, &b) in a.iter().zip(b) {
                            assert!(
                                a.abs_diff(b) <= 4,
                                "es={es},mode={mode},size={size}: color {a}/{b}"
                            );
                        }
                    }
                    // Point/line culling must not affect their drawing, while triangles still cull.
                    state.point_size = 1.;
                    state.line_width = 1.;
                    gpu.clear(32, 32, &state, [0.; 4], 1., GL_COLOR_BUFFER_BIT)
                        .unwrap();
                    gpu.draw(
                        &native,
                        &state,
                        GL_TRIANGLES,
                        &draw_vertices,
                        &[],
                        &HashMap::new(),
                        [0; 4],
                    )
                    .unwrap();
                    gpu.read(&mut native).unwrap();
                    assert_eq!(count(&native), 0);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires native desktop GL/GLES2; signed linear fog must match software"]
    fn host_linear_fog_descending_and_degenerate_ranges_match_software() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            for (start, end, depth) in [
                (-100.5, -343.28522, 100.0), // Carmageddon's actual range.
                (10.0, 2.0, 6.0),
                (10.0, 2.0, 0.0),
                (10.0, 2.0, 20.0),
                (0.0, 10.0, 5.0),
                (2.0, 2.0, 6.0),
            ] {
                let state = PipelineState {
                    viewport: (0, 0, 32, 32),
                    fog: true,
                    fog_mode: FogMode::Linear,
                    fog_start: start,
                    fog_end: end,
                    fog_color: [0.0, 0.0, 0.8, 1.0],
                    ..Default::default()
                };
                let mut vertices = triangle([1.0, 0.4, 0.2, 1.0], 0.0);
                for v in &mut vertices {
                    v.fog_depth = depth;
                }
                let mut cpu = RenderTarget::new(32, 32);
                let mut native = RenderTarget::new(32, 32);
                raster::draw_triangle(&mut cpu, &state, &|_, _, _| None, &[], vertices);
                gpu.clear(32, 32, &state, [0.0; 4], 1.0, GL_COLOR_BUFFER_BIT)
                    .unwrap();
                gpu.draw(
                    &native,
                    &state,
                    GL_TRIANGLES,
                    &vertices,
                    &[],
                    &HashMap::new(),
                    [0; 4],
                )
                .unwrap();
                gpu.read(&mut native).unwrap();
                for (a, b) in center(&cpu).into_iter().zip(center(&native)) {
                    assert!(
                        a.abs_diff(b) <= 2,
                        "es={es}, fog={start}..{end}, depth={depth}: software={a}, GPU={b}"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires native GL/GLES2; strict alpha tests must match software precision"]
    fn host_strict_alpha_equal_matches_software_quantization() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            let state = PipelineState {
                viewport: (0, 0, 32, 32),
                alpha_test: true,
                alpha_func: CompareFunc::Equal,
                alpha_ref: 1.,
                ..Default::default()
            };
            let vertices = triangle([0.8, 0.4, 0.2, 0.9999], 0.);
            let mut cpu = RenderTarget::new(32, 32);
            let mut native = RenderTarget::new(32, 32);
            raster::draw_triangle(&mut cpu, &state, &|_, _, _| None, &[], vertices);
            gpu.clear(32, 32, &state, [0.; 4], 1., GL_COLOR_BUFFER_BIT)
                .unwrap();
            gpu.draw(
                &native,
                &state,
                GL_TRIANGLES,
                &vertices,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            gpu.read(&mut native).unwrap();
            let i = (16 * 32 + 16) * 4;
            assert!(cpu.color[i] > 0);
            for c in 0..4 {
                assert!(
                    cpu.color[i + c].abs_diff(native.color[i + c]) <= 1,
                    "es={es}, channel={c}: software {}, GPU {}",
                    cpu.color[i + c],
                    native.color[i + c]
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires native GL/GLES2; HLE depth clear compatibility with writes disabled"]
    fn host_depth_clear_matches_software_with_depth_writes_disabled() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            let mut state = PipelineState {
                viewport: (0, 0, 32, 32),
                depth_test: true,
                ..Default::default()
            };
            let mut cpu = RenderTarget::new(32, 32);
            let mut native = RenderTarget::new(32, 32);
            gpu.clear(
                32,
                32,
                &state,
                [0.; 4],
                1.,
                GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
            )
            .unwrap();
            let near = triangle([1., 0., 0., 1.], 0.);
            gpu.draw(
                &native,
                &state,
                GL_TRIANGLES,
                &near,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            raster::draw_triangle(&mut cpu, &state, &|_, _, _| None, &[], near);
            state.depth_write = false;
            cpu.clear_color([0; 4], None);
            cpu.clear_depth(1., None);
            gpu.clear(
                32,
                32,
                &state,
                [0.; 4],
                1.,
                GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
            )
            .unwrap();
            let far = triangle([0., 1., 0., 1.], 0.5);
            gpu.draw(
                &native,
                &state,
                GL_TRIANGLES,
                &far,
                &[],
                &HashMap::new(),
                [0; 4],
            )
            .unwrap();
            gpu.read(&mut native).unwrap();
            raster::draw_triangle(&mut cpu, &state, &|_, _, _| None, &[], far);
            let i = (16 * 32 + 16) * 4;
            assert_eq!(&cpu.color[i..i + 4], &native.color[i..i + 4], "es={es}");
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires native GL/GLES2; closely spaced terrain depth layers"]
    fn host_depth24_separates_layers_that_depth16_collapses() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            assert_eq!(gpu.depth_format, 0x81a6);
            let state = PipelineState {
                viewport: (0, 0, 32, 32),
                depth_test: true,
                ..Default::default()
            };
            let mut native = RenderTarget::new(32, 32);
            let mut cpu = RenderTarget::new(32, 32);
            let far = triangle([1., 0., 0., 1.], 0.75);
            let near = triangle([0., 1., 0., 1.], 0.75 - 0.000001);
            for format in [0x81a5, 0x81a6] {
                gpu.depth_format = format;
                // Force a real attachment reallocation, as on orientation changes.
                gpu.resize(16, 16).unwrap();
                gpu.resize(32, 32).unwrap();
                gpu.clear(
                    32,
                    32,
                    &state,
                    [0.; 4],
                    1.,
                    GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
                )
                .unwrap();
                for vertices in [far, near] {
                    gpu.draw(
                        &native,
                        &state,
                        GL_TRIANGLES,
                        &vertices,
                        &[],
                        &HashMap::new(),
                        [0; 4],
                    )
                    .unwrap();
                }
                gpu.read(&mut native).unwrap();
                let i = (16 * 32 + 16) * 4;
                if format == 0x81a5 {
                    assert_eq!(&native.color[i..i + 4], &[255, 0, 0, 255]);
                } else {
                    for vertices in [far, near] {
                        raster::draw_triangle(&mut cpu, &state, &|_, _, _| None, &[], vertices);
                    }
                    assert_eq!(&native.color[i..i + 4], &[0, 255, 0, 255]);
                    assert_eq!(&native.color[i..i + 4], &cpu.color[i..i + 4]);
                }
            }
        }
    }

    fn triangle(color: [f32; 4], z: f32) -> [Vertex; 3] {
        [[-0.9, -0.9, z, 1.], [0.9, -0.9, z, 1.], [0., 0.9, z, 1.]].map(|pos| Vertex {
            pos,
            color,
            ..Vertex::default()
        })
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires host OpenGL and GLES2 drivers"]
    fn host_vertex_shader_matches_cpu_transforms() {
        use crate::matrix;
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            for perspective in [false, true] {
                let transforms = VertexTransforms {
                    lighting: Default::default(),
                    normal_matrix: IDENTITY,
                    projection: if perspective {
                        matrix::frustum(-1., 1., -1., 1., 1., 10.)
                    } else {
                        IDENTITY
                    },
                    modelview: matrix::multiply(
                        &matrix::translate(0.125, -0.125, if perspective { -2. } else { -0.25 }),
                        &matrix::multiply(
                            &matrix::rotate(23., 0., 0., 1.),
                            &matrix::scale(0.75, 0.625, 1.),
                        ),
                    ),
                    textures: std::array::from_fn(|i| {
                        matrix::multiply(
                            &matrix::translate(0.125 * i as f32, 0.25, 0.),
                            &matrix::scale(0.5, 0.75, 1.),
                        )
                    }),
                };
                let mut raw = triangle([0.9, 0.75, 0.5, 1.], 0.);
                raw[0].texcoord = [[0.1, 0.1]; 4];
                raw[1].texcoord = [[0.8, 0.1]; 4];
                raw[2].texcoord = [[0.4, 0.9]; 4];
                let mut cpu = raw;
                let mvp = matrix::multiply(&transforms.projection, &transforms.modelview);
                for vertex in &mut cpu {
                    vertex.fog_depth =
                        matrix::transform(&transforms.modelview, vertex.pos)[2].abs();
                    vertex.pos = matrix::transform(&mvp, vertex.pos);
                    for i in 0..4 {
                        let uv = vertex.texcoord[i];
                        let t = matrix::transform(&transforms.textures[i], [uv[0], uv[1], 0., 1.]);
                        vertex.texcoord[i] = [t[0], t[1]];
                    }
                }
                let texture = Texture {
                    width: 4,
                    height: 4,
                    rgba: (0..16)
                        .flat_map(|i| [(i % 4 * 64) as u8, (i / 4 * 64) as u8, 128, 255])
                        .collect(),
                    min_filter: Filter::Linear,
                    mag_filter: Filter::Linear,
                    revision: 1,
                    ..Default::default()
                };
                let textures = HashMap::from([(1, texture)]);
                for unit in 0..4 {
                    let stages = [TextureStage {
                        unit,
                        env: TexEnvMode::Modulate,
                        env_color: [0.; 4],
                        lod_bias: 0.0,
                    }];
                    let mut names = [0; 4];
                    names[unit] = 1;
                    for mode in [GL_TRIANGLES, GL_TRIANGLE_STRIP, GL_TRIANGLE_FAN] {
                        let state = PipelineState {
                            viewport: (0, 0, 32, 32),
                            fog: true,
                            fog_mode: FogMode::Linear,
                            fog_start: 0.,
                            fog_end: 5.,
                            fog_color: [0.1, 0.2, 0.3, 1.],
                            depth_test: true,
                            ..Default::default()
                        };
                        let mut target = RenderTarget::new(32, 32);
                        gpu.clear(
                            32,
                            32,
                            &state,
                            [0., 0., 0., 1.],
                            1.,
                            GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
                        )
                        .unwrap();
                        gpu.draw(&target, &state, mode, &cpu, &stages, &textures, names)
                            .unwrap();
                        gpu.read(&mut target).unwrap();
                        let expected = target.color.clone();
                        gpu.clear(
                            32,
                            32,
                            &state,
                            [0., 0., 0., 1.],
                            1.,
                            GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
                        )
                        .unwrap();
                        gpu.draw_object_space(
                            &target,
                            &state,
                            mode,
                            &raw,
                            &stages,
                            &textures,
                            names,
                            &transforms,
                        )
                        .unwrap();
                        gpu.read(&mut target).unwrap();
                        assert!(expected.chunks_exact(4).any(|p| p[0] != 0));
                        let differences = expected
                            .iter()
                            .zip(&target.color)
                            .filter(|(a, b)| a.abs_diff(**b) > 2)
                            .count();
                        assert!(differences <= 12, "es={es} perspective={perspective} unit={unit} mode={mode} differences={differences}");
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires host OpenGL and GLES2 lighting shaders"]
    fn host_fixed_lighting_materials_normals_spots_and_flat_shading() {
        for es in [false, true] {
            let mut gpu = if es {
                Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap()
            } else {
                Renderer::new(32, 32).unwrap()
            };
            let mut target = RenderTarget::new(32, 32);
            let mut state = PipelineState {
                viewport: (0, 0, 32, 32),
                ..Default::default()
            };
            let textures = HashMap::new();
            let raw = triangle([1., 0., 0., 1.], 0.);
            let mut transform = VertexTransforms {
                projection: IDENTITY,
                modelview: IDENTITY,
                textures: [IDENTITY; 4],
                normal_matrix: IDENTITY,
                lighting: Default::default(),
            };
            transform.lighting.enabled = true;
            transform.lighting.model_ambient = [0.; 4];
            transform.lighting.ambient = [0.; 4];
            transform.lighting.diffuse = [0.8, 0.4, 0.2, 0.7];
            transform.lighting.lights[0].enabled = true;
            let mut render = |state: &PipelineState, vertices: &[Vertex], t: &VertexTransforms| {
                gpu.clear(
                    32,
                    32,
                    state,
                    [0., 0., 0., 1.],
                    1.,
                    GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
                )
                .unwrap();
                gpu.draw_object_space(
                    &target,
                    state,
                    GL_TRIANGLES,
                    vertices,
                    &[],
                    &textures,
                    [0; 4],
                    t,
                )
                .unwrap();
                gpu.read(&mut target).unwrap();
                center(&target)
            };
            let lit = render(&state, &raw, &transform);
            for (actual, expected) in lit.into_iter().zip([204u8, 102, 51, 178]) {
                assert!(actual.abs_diff(expected) <= 1);
            }
            transform.lighting.lights[0].enabled = false;
            transform.lighting.model_ambient = [0.2, 0.3, 0.4, 1.];
            transform.lighting.ambient = [0.5, 0.25, 0.1, 1.];
            transform.lighting.emission = [0.1, 0.2, 0.3, 1.];
            for (actual, expected) in render(&state, &raw, &transform)
                .into_iter()
                .zip([51u8, 70, 87, 178])
            {
                assert!(actual.abs_diff(expected) <= 1);
            }
            transform.lighting.model_ambient = [0.; 4];
            transform.lighting.ambient = [0.; 4];
            transform.lighting.emission = [0.; 4];
            transform.lighting.lights[0].enabled = true;
            let mut away = raw;
            for v in &mut away {
                v.normal = [0., 0., -1.];
            }
            assert_eq!(&render(&state, &away, &transform)[..3], &[0, 0, 0]);
            transform.normal_matrix =
                crate::lighting::normal_matrix(&crate::matrix::scale(2., 2., 2.));
            let dim = render(&state, &raw, &transform);
            assert!(dim[0].abs_diff(102) <= 1);
            transform.lighting.normalize = true;
            assert_eq!(render(&state, &raw, &transform), lit);
            transform.lighting.normalize = false;
            transform.lighting.rescale = true;
            assert_eq!(render(&state, &raw, &transform), lit);
            transform.normal_matrix = IDENTITY;
            transform.lighting.rescale = false;
            transform.lighting.color_material = true;
            assert_eq!(render(&state, &raw, &transform), [255, 0, 0, 255]);
            transform.lighting.color_material = false;
            transform.lighting.diffuse = [0., 0., 0., 0.7];
            transform.lighting.specular = [0.5, 0.25, 0., 1.];
            transform.lighting.shininess = 32.;
            let spec = render(&state, &raw, &transform);
            assert!(spec[0].abs_diff(128) <= 1 && spec[1].abs_diff(64) <= 1);
            transform.lighting.specular = [0.; 4];
            transform.lighting.diffuse = [0.8, 0.4, 0.2, 0.7];
            transform.lighting.lights[0].enabled = false;
            transform.lighting.lights[7].enabled = true;
            transform.lighting.lights[7].diffuse = [1.; 4];
            assert_eq!(render(&state, &raw, &transform), lit);
            transform.lighting.lights[7].position = [0., 0., 2., 1.];
            transform.lighting.lights[7].attenuation = [1., 0., 0.];
            let near = render(&state, &raw, &transform);
            transform.lighting.lights[7].attenuation = [1., 1., 0.];
            let attenuated = render(&state, &raw, &transform);
            assert!(near[0] > attenuated[0] && attenuated[0] > 0);
            transform.lighting.lights[7].spot_cutoff = 30.;
            transform.lighting.lights[7].spot_direction = [0., 0., 1.];
            assert_eq!(&render(&state, &raw, &transform)[..3], &[0, 0, 0]);
            transform.lighting.lights[7].spot_direction = [0., 0., -1.];
            assert!(render(&state, &raw, &transform)[0] > 0);
            transform.lighting.lights[7].spot_cutoff = 180.;
            transform.lighting.lights[7].position = [0., 0., -1., 0.];
            let back = [raw[1], raw[0], raw[2]];
            assert_eq!(&render(&state, &back, &transform)[..3], &[0, 0, 0]);
            transform.lighting.two_sided = true;
            assert_eq!(render(&state, &back, &transform), lit);
            transform.lighting.two_sided = false;
            transform.lighting.lights[7].position = [0., 0., 1., 0.];
            let mut mixed = raw;
            mixed[2].normal = [0., 0., -1.];
            state.smooth_shading = false;
            assert_eq!(&render(&state, &mixed, &transform)[..3], &[0, 0, 0]);
            state.smooth_shading = true;
            assert!(render(&state, &mixed, &transform)[0] > 0);
            transform.lighting.enabled = false;
            assert_eq!(render(&state, &raw, &transform), [255, 0, 0, 255]);
        }
    }

    fn center(t: &RenderTarget) -> [u8; 4] {
        t.color[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4]
            .try_into()
            .unwrap()
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires a host EGL GLES2 driver; run explicitly"]
    fn host_es2_shaders_fog_scissor_masks_and_readback_orientation() {
        let mut gpu = Renderer::with_host(Host::es_for_test().unwrap(), 32, 32).unwrap();
        let mut target = RenderTarget::new(32, 32);
        let mut state = PipelineState {
            viewport: (0, 0, 32, 32),
            ..Default::default()
        };
        let textures = HashMap::new();
        gpu.clear(
            32,
            32,
            &state,
            [0., 0., 1., 1.],
            1.,
            GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
        )
        .unwrap();
        state.scissor = Some((0, 0, 32, 16));
        gpu.clear(32, 32, &state, [1., 0., 0., 1.], 1., GL_COLOR_BUFFER_BIT)
            .unwrap();
        gpu.read(&mut target).unwrap();
        assert_eq!(&target.color[..4], &[0, 0, 255, 255]);
        assert_eq!(
            &target.color[31 * 32 * 4..31 * 32 * 4 + 4],
            &[255, 0, 0, 255]
        );
        state.scissor = None;
        state.fog = true;
        state.fog_mode = FogMode::Linear;
        state.fog_start = 0.;
        state.fog_end = 2.;
        state.fog_color = [0., 0., 1., 1.];
        let mut vertices = triangle([1., 0., 0., 1.], 0.);
        for v in &mut vertices {
            v.fog_depth = 1.;
        }
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &vertices,
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        let p = center(&target);
        assert!((126..=129).contains(&p[0]) && (126..=129).contains(&p[2]));
        state.fog = false;
        state.color_mask = [false, true, false, false];
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([0., 1., 0., 1.], 0.),
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        let q = center(&target);
        assert_eq!(q[0], p[0]);
        assert_eq!(q[1], 255);
        assert_eq!(q[2], p[2]);
    }
    #[test]
    #[ignore = "requires a host OpenGL driver; run explicitly"]
    fn host_gpu_renders_depth_blend_alpha_textures_and_survives_context_switches() {
        let mut target = RenderTarget::new(32, 32);
        let mut state = PipelineState {
            viewport: (0, 0, 32, 32),
            ..Default::default()
        };
        let mut gpu = Renderer::new(32, 32).expect("host OpenGL initialization");
        let mut textures = HashMap::new();
        gpu.clear(
            32,
            32,
            &state,
            [0., 0., 0., 1.],
            1.,
            GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
        )
        .unwrap();
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([1., 0., 0., 1.], 0.),
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        assert_eq!(center(&target), [255, 0, 0, 255]);
        state.depth_test = true;
        gpu.clear(
            32,
            32,
            &state,
            [0., 0., 0., 1.],
            1.,
            GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT,
        )
        .unwrap();
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([0., 1., 0., 1.], -0.5),
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([1., 0., 0., 1.], 0.5),
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        assert_eq!(center(&target), [0, 255, 0, 255]);
        state.depth_test = false;
        state.alpha_test = true;
        state.alpha_func = CompareFunc::Greater;
        state.alpha_ref = 0.5;
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([1., 0., 0., 0.25], 0.),
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        assert_eq!(center(&target), [0, 255, 0, 255]);
        state.alpha_test = false;
        state.blend = true;
        state.blend_src = BlendFactor::SrcAlpha;
        state.blend_dst = BlendFactor::OneMinusSrcAlpha;
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([1., 0., 0., 0.5], 0.),
            &[],
            &textures,
            [0; 4],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        let p = center(&target);
        assert!((126..=129).contains(&p[0]) && (126..=129).contains(&p[1]));
        state.blend = false;
        textures.insert(
            1,
            Texture {
                width: 1,
                height: 1,
                rgba: vec![0, 0, 255, 255],
                revision: 1,
                ..Default::default()
            },
        );
        let stages = [TextureStage {
            unit: 0,
            env: TexEnvMode::Replace,
            env_color: [0.; 4],
            lod_bias: 0.0,
        }];
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([1.; 4], 0.),
            &stages,
            &textures,
            [1, 0, 0, 0],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        assert_eq!(center(&target), [0, 0, 255, 255]);
        textures.get_mut(&1).unwrap().rgba = vec![255, 255, 0, 255];
        textures.get_mut(&1).unwrap().revision += 1;
        gpu.draw(
            &target,
            &state,
            GL_TRIANGLES,
            &triangle([1.; 4], 0.),
            &stages,
            &textures,
            [1, 0, 0, 0],
        )
        .unwrap();
        gpu.read(&mut target).unwrap();
        assert_eq!(center(&target), [255, 255, 0, 255]);
        // A child renderer uses the same EGL display; dropping it must not
        // terminate the display or delete any parent resources.
        for _ in 0..10 {
            let child = Renderer::new(16, 16).unwrap();
            drop(child);
            gpu.draw(
                &target,
                &state,
                GL_TRIANGLES,
                &triangle([1.; 4], 0.),
                &stages,
                &textures,
                [1, 0, 0, 0],
            )
            .unwrap();
            gpu.read(&mut target).unwrap();
            assert_eq!(center(&target), [255, 255, 0, 255]);
        }
        gpu.resize(64, 48).unwrap();
        target.resize(64, 48);
        gpu.read(&mut target).unwrap();
        assert_eq!(target.color.len(), 64 * 48 * 4);
    }
}
