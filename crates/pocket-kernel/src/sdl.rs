//! Session-owned SDL 1.2 software video and input resources.
use std::collections::{HashMap, VecDeque};
#[derive(Clone)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bytes: u32,
    pub pixels: u32,
    pub format: u32,
    pub masks: [u32; 4],
}
pub struct SdlState {
    pub initialized: u32,
    pub screen: u32,
    pub surfaces: HashMap<u32, Surface>,
    pub events: VecDeque<[u8; 20]>,
    pub keys: [u8; 323],
    pub key_ptr: u32,
    pub error: String,
    pub error_ptr: u32,
    pub version_ptr: u32,
    pub mouse: (u16, u16, bool),
}
impl Default for SdlState {
    fn default() -> Self {
        Self {
            initialized: 0,
            screen: 0,
            surfaces: HashMap::new(),
            events: VecDeque::new(),
            keys: [0; 323],
            key_ptr: 0,
            error: String::new(),
            error_ptr: 0,
            version_ptr: 0,
            mouse: (0, 0, false),
        }
    }
}
