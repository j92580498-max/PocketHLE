//! Private offscreen contexts, confined to the emulation thread.
#[cfg(unix)]
use std::ffi::CString;
use std::{
    ffi::{c_char, c_void, CStr},
    marker::PhantomData,
    rc::Rc,
};

type Ptr = *mut c_void;
pub struct Library(Ptr);
// A loader handle is process-wide and symbol lookup does not mutate it.
// GL contexts themselves remain !Send/!Sync through Host's Rc marker.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}
impl Library {
    pub fn open(name: &str) -> Result<Self, String> {
        #[cfg(unix)]
        unsafe {
            let name = CString::new(name).unwrap();
            let p = dlopen(name.as_ptr(), 2);
            if p.is_null() {
                Err(format!("cannot load {name:?}"))
            } else {
                Ok(Self(p))
            }
        }
        #[cfg(windows)]
        unsafe {
            let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let p = LoadLibraryW(name.as_ptr());
            if p.is_null() {
                Err("cannot load OpenGL driver".into())
            } else {
                Ok(Self(p))
            }
        }
    }
    pub fn symbol(&self, name: &CStr) -> Ptr {
        #[cfg(unix)]
        unsafe {
            dlsym(self.0, name.as_ptr())
        }
        #[cfg(windows)]
        unsafe {
            GetProcAddress(self.0, name.as_ptr())
        }
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            #[cfg(unix)]
            {
                dlclose(self.0);
            }
            #[cfg(windows)]
            {
                FreeLibrary(self.0);
            }
        }
    }
}
#[cfg(unix)]
#[link(name = "dl")]
extern "C" {
    fn dlopen(n: *const c_char, flags: i32) -> Ptr;
    fn dlsym(l: Ptr, n: *const c_char) -> Ptr;
    fn dlclose(l: Ptr) -> i32;
}
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(n: *const u16) -> Ptr;
    fn GetProcAddress(l: Ptr, n: *const c_char) -> Ptr;
    fn FreeLibrary(l: Ptr) -> i32;
}

#[cfg(unix)]
mod native {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, OnceLock},
    };
    type Display = Ptr;
    type GetCurrent = unsafe extern "system" fn() -> Ptr;
    type GetSurface = unsafe extern "system" fn(i32) -> Ptr;
    type MakeCurrent = unsafe extern "system" fn(Display, Ptr, Ptr, Ptr) -> u32;
    type Destroy = unsafe extern "system" fn(Display, Ptr) -> u32;
    type Terminate = unsafe extern "system" fn(Display) -> u32;
    type GetProc = unsafe extern "system" fn(*const c_char) -> Ptr;
    fn displays() -> &'static Mutex<HashMap<usize, usize>> {
        static D: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
        D.get_or_init(Default::default)
    }
    pub struct Host {
        egl: Arc<Library>,
        gl: Library,
        display: Display,
        surface: Ptr,
        context: Ptr,
        current: MakeCurrent,
        current_context: GetCurrent,
        current_display: GetCurrent,
        current_surface: GetSurface,
        destroy_context: Destroy,
        destroy_surface: Destroy,
        terminate: Terminate,
        get_proc: GetProc,
        pub es: bool,
        _thread: PhantomData<Rc<()>>,
    }
    impl Host {
        pub fn new() -> Result<Self, String> {
            Self::create(cfg!(target_os = "android"))
        }
        #[cfg(test)]
        pub fn es_for_test() -> Result<Self, String> {
            Self::create(true)
        }
        fn create(es: bool) -> Result<Self, String> {
            unsafe {
                #[cfg(target_os = "android")]
                let egl = {
                    static EGL: OnceLock<Result<Arc<Library>, String>> = OnceLock::new();
                    EGL.get_or_init(|| Library::open("libEGL.so").map(Arc::new))
                        .clone()?
                };
                #[cfg(not(target_os = "android"))]
                let egl = Arc::new(Library::open("libEGL.so.1")?);
                let gl = Library::open(if cfg!(target_os = "android") {
                    "libGLESv2.so"
                } else {
                    "libGL.so.1"
                })?;
                macro_rules! sym {
                    ($name:literal,$t:ty) => {{
                        let p = egl.symbol(
                            CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap(),
                        );
                        if p.is_null() {
                            return Err(concat!("missing ", $name).into());
                        }
                        std::mem::transmute::<Ptr, $t>(p)
                    }};
                }
                let get_display = sym!("eglGetDisplay", unsafe extern "system" fn(Ptr) -> Display);
                let initialize = sym!(
                    "eglInitialize",
                    unsafe extern "system" fn(Display, *mut i32, *mut i32) -> u32
                );
                let bind = sym!("eglBindAPI", unsafe extern "system" fn(u32) -> u32);
                let choose = sym!(
                    "eglChooseConfig",
                    unsafe extern "system" fn(Display, *const i32, *mut Ptr, i32, *mut i32) -> u32
                );
                let create_surface = sym!(
                    "eglCreatePbufferSurface",
                    unsafe extern "system" fn(Display, Ptr, *const i32) -> Ptr
                );
                let create_context = sym!(
                    "eglCreateContext",
                    unsafe extern "system" fn(Display, Ptr, Ptr, *const i32) -> Ptr
                );
                let current = sym!("eglMakeCurrent", MakeCurrent);
                let current_context = sym!("eglGetCurrentContext", GetCurrent);
                let current_display = sym!("eglGetCurrentDisplay", GetCurrent);
                let current_surface = sym!("eglGetCurrentSurface", GetSurface);
                let destroy_context = sym!("eglDestroyContext", Destroy);
                let destroy_surface = sym!("eglDestroySurface", Destroy);
                let terminate = sym!("eglTerminate", Terminate);
                let get_proc = sym!("eglGetProcAddress", GetProc);
                let mut display = get_display(std::ptr::null_mut());
                // Mesa's surfaceless platform is useful on headless Linux only.
                if !cfg!(target_os = "android") {
                    let p = get_proc(c"eglGetPlatformDisplayEXT".as_ptr());
                    if !p.is_null() {
                        let get: unsafe extern "system" fn(u32, Ptr, *const i32) -> Display =
                            std::mem::transmute(p);
                        let d = get(0x31dd, std::ptr::null_mut(), std::ptr::null());
                        if !d.is_null() {
                            display = d;
                        }
                    }
                }
                if display.is_null() {
                    return Err("EGL has no display".into());
                }
                {
                    let mut d = displays().lock().unwrap();
                    let first = !d.contains_key(&(display as usize));
                    if first && initialize(display, std::ptr::null_mut(), std::ptr::null_mut()) == 0
                    {
                        d.remove(&(display as usize));
                        return Err("EGL initialization failed".into());
                    }
                    *d.entry(display as usize).or_default() += 1;
                }
                let mut host = Self {
                    egl,
                    gl,
                    display,
                    surface: std::ptr::null_mut(),
                    context: std::ptr::null_mut(),
                    current,
                    current_context,
                    current_display,
                    current_surface,
                    destroy_context,
                    destroy_surface,
                    terminate,
                    get_proc,
                    es,
                    _thread: PhantomData,
                };
                if bind(if host.es { 0x30a0 } else { 0x30a2 }) == 0 {
                    return Err("EGL API binding failed".into());
                }
                let attrs = [
                    0x3033,
                    1,
                    0x3040,
                    if host.es { 4 } else { 8 },
                    0x3024,
                    8,
                    0x3023,
                    8,
                    0x3022,
                    8,
                    0x3038,
                ];
                let mut config = std::ptr::null_mut();
                let mut count = 0;
                if choose(display, attrs.as_ptr(), &mut config, 1, &mut count) == 0 || count == 0 {
                    return Err("EGL pbuffer configuration unavailable".into());
                }
                host.surface =
                    create_surface(display, config, [0x3057, 1, 0x3056, 1, 0x3038].as_ptr());
                let ca = if host.es {
                    vec![0x3098, 2, 0x3038]
                } else {
                    vec![0x3038]
                };
                host.context = create_context(display, config, std::ptr::null_mut(), ca.as_ptr());
                if host.surface.is_null() || host.context.is_null() {
                    return Err("EGL offscreen creation failed".into());
                }
                host.make_current()?;
                Ok(host)
            }
        }
        pub fn make_current(&self) -> Result<(), String> {
            unsafe {
                // Preserve actual native thread state across parent/child switches.
                if (self.current_context)() == self.context
                    && (self.current_display)() == self.display
                    && (self.current_surface)(0x3059) == self.surface
                    && (self.current_surface)(0x305a) == self.surface
                {
                    return Ok(());
                }
                if (self.current)(self.display, self.surface, self.surface, self.context) == 0 {
                    Err("EGL context lost".into())
                } else {
                    Ok(())
                }
            }
        }
        pub fn load(&self, n: &CStr) -> Ptr {
            unsafe {
                let p = (self.get_proc)(n.as_ptr());
                if p.is_null() {
                    self.gl.symbol(n)
                } else {
                    p
                }
            }
        }
    }
    impl Drop for Host {
        fn drop(&mut self) {
            unsafe {
                if !self.context.is_null() {
                    (self.current)(
                        self.display,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    (self.destroy_context)(self.display, self.context);
                }
                if !self.surface.is_null() {
                    (self.destroy_surface)(self.display, self.surface);
                }
                let mut ds = displays().lock().unwrap();
                if let Some(n) = ds.get_mut(&(self.display as usize)) {
                    *n -= 1;
                    // Android's UI shares EGL_DEFAULT_DISPLAY. Keep one
                    // process-wide initialization lease; destroy every game
                    // context and surface without terminating the UI display.
                    if *n == 0 && !cfg!(target_os = "android") {
                        (self.terminate)(self.display);
                        ds.remove(&(self.display as usize));
                    }
                }
                let _ = &self.egl;
            }
        }
    }
}
#[cfg(windows)]
mod native {
    use super::*;
    #[repr(C)]
    struct PixelFormat {
        size: u16,
        version: u16,
        flags: u32,
        ty: u8,
        color: u8,
        red: u8,
        redshift: u8,
        green: u8,
        greenshift: u8,
        blue: u8,
        blueshift: u8,
        alpha: u8,
        alphashift: u8,
        accum: u8,
        ar: u8,
        ag: u8,
        ab: u8,
        aa: u8,
        depth: u8,
        stencil: u8,
        aux: u8,
        layer: u8,
        reserved: u8,
        layermask: u32,
        visible: u32,
        damage: u32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn CreateWindowExW(
            ex: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: Ptr,
            menu: Ptr,
            instance: Ptr,
            param: Ptr,
        ) -> Ptr;
        fn DestroyWindow(w: Ptr) -> i32;
        fn GetDC(w: Ptr) -> Ptr;
        fn ReleaseDC(w: Ptr, d: Ptr) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn ChoosePixelFormat(d: Ptr, p: *const PixelFormat) -> i32;
        fn SetPixelFormat(d: Ptr, i: i32, p: *const PixelFormat) -> i32;
    }
    #[link(name = "opengl32")]
    extern "system" {
        fn wglCreateContext(d: Ptr) -> Ptr;
        fn wglMakeCurrent(d: Ptr, c: Ptr) -> i32;
        fn wglGetCurrentContext() -> Ptr;
        fn wglGetCurrentDC() -> Ptr;
        fn wglDeleteContext(c: Ptr) -> i32;
        fn wglGetProcAddress(n: *const c_char) -> Ptr;
    }
    pub struct Host {
        gl: Library,
        window: Ptr,
        dc: Ptr,
        context: Ptr,
        pub es: bool,
        _thread: PhantomData<Rc<()>>,
    }
    impl Host {
        pub fn new() -> Result<Self, String> {
            unsafe {
                let gl = Library::open("opengl32.dll")?;
                let mut h = Self {
                    gl,
                    window: std::ptr::null_mut(),
                    dc: std::ptr::null_mut(),
                    context: std::ptr::null_mut(),
                    es: false,
                    _thread: PhantomData,
                };
                let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
                h.window = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    class.as_ptr(),
                    0x80000000,
                    0,
                    0,
                    1,
                    1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if h.window.is_null() {
                    return Err("WGL hidden window unavailable".into());
                }
                h.dc = GetDC(h.window);
                if h.dc.is_null() {
                    return Err("WGL device context unavailable".into());
                }
                let mut p: PixelFormat = std::mem::zeroed();
                p.size = std::mem::size_of::<PixelFormat>() as u16;
                p.version = 1;
                p.flags = 0x24;
                p.color = 24;
                p.depth = 24;
                p.stencil = 8;
                let i = ChoosePixelFormat(h.dc, &p);
                if i == 0 || SetPixelFormat(h.dc, i, &p) == 0 {
                    return Err("WGL pixel format unavailable".into());
                }
                h.context = wglCreateContext(h.dc);
                if h.context.is_null() {
                    return Err("WGL context unavailable".into());
                }
                h.make_current()?;
                Ok(h)
            }
        }
        pub fn make_current(&self) -> Result<(), String> {
            unsafe {
                // Rebinding an already current WGL context can flush queued work.
                // Query native state so child/parent switches remain correct.
                if wglGetCurrentContext() == self.context && wglGetCurrentDC() == self.dc {
                    return Ok(());
                }
                if wglMakeCurrent(self.dc, self.context) == 0 {
                    Err("WGL context lost".into())
                } else {
                    Ok(())
                }
            }
        }
        pub fn load(&self, n: &CStr) -> Ptr {
            unsafe {
                let p = wglGetProcAddress(n.as_ptr());
                if p.is_null() || matches!(p as usize, 1 | 2 | 3 | usize::MAX) {
                    self.gl.symbol(n)
                } else {
                    p
                }
            }
        }
    }
    impl Drop for Host {
        fn drop(&mut self) {
            unsafe {
                if !self.context.is_null() {
                    wglMakeCurrent(std::ptr::null_mut(), std::ptr::null_mut());
                    wglDeleteContext(self.context);
                }
                if !self.dc.is_null() {
                    ReleaseDC(self.window, self.dc);
                }
                if !self.window.is_null() {
                    DestroyWindow(self.window);
                }
            }
        }
    }
}
pub use native::Host;
