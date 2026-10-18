//! A tiny in-memory Windows CE registry.
//!
//! Pocket PC games keep more than preferences in `HKLM` / `HKCU`: the
//! CAB installer writes the paths a title later reads back to find its
//! own data. Astraware Bejeweled, for example, refuses to start (it
//! calls `ExitProcess(0x42)`) unless
//! `HKLM\SOFTWARE\Apps\Astraware Bejeweled\SaveDir` exists, because
//! that is where its `_setup.xml` told the installer to put saves.
//!
//! The store is deliberately simple:
//!
//! * Keys are canonical strings such as
//!   `HKLM\SOFTWARE\Apps\Astraware Bejeweled`, compared
//!   case-insensitively (WinCE registry keys are case-insensitive).
//! * Values are `REG_SZ`, `REG_DWORD` or `REG_BINARY`.
//! * `RegOpenKeyEx` hands out integer handles that map back to the
//!   canonical path, so `RegQueryValueEx` can resolve them without the
//!   guest holding a pointer into host memory.

use std::collections::HashMap;

/// Value types we model — the subset Pocket PC titles actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryValue {
    /// `REG_SZ` (stored as UTF-8, handed to the guest as UTF-16).
    Sz(String),
    /// `REG_DWORD`.
    Dword(u32),
    /// `REG_BINARY`.
    Binary(Vec<u8>),
}

impl RegistryValue {
    /// The `REG_*` type code reported through `RegQueryValueEx`.
    pub fn type_code(&self) -> u32 {
        match self {
            RegistryValue::Sz(_) => 1,
            RegistryValue::Binary(_) => 3,
            RegistryValue::Dword(_) => 4,
        }
    }

    /// The value encoded exactly as the guest expects to receive it.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            RegistryValue::Sz(text) => text
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect(),
            RegistryValue::Dword(value) => value.to_le_bytes().to_vec(),
            RegistryValue::Binary(bytes) => bytes.clone(),
        }
    }
}

/// First handle handed out by [`Registry::open`]. Chosen to be
/// obviously not a predefined `HKEY_*` constant (`0x8000_000n`) and not
/// to collide with the VFS or GDI fake-handle ranges.
const HANDLE_BASE: u32 = 0xDEAD_9100;

#[derive(Debug, Default)]
pub struct Registry {
    /// Key path (lowercased) -> value name (lowercased) -> value.
    keys: HashMap<String, HashMap<String, RegistryValue>>,
    /// Key path -> lowercased value name -> first spelling written.
    value_names: HashMap<String, HashMap<String, String>>,
    /// Key path (lowercased) -> the path as first written, for logs.
    display: HashMap<String, String>,
    /// Open handle -> key path (lowercased).
    handles: HashMap<u32, String>,
    next_handle: u32,
}

/// Map a predefined `HKEY_*` constant to its canonical prefix.
fn root_prefix(root: u32) -> Option<&'static str> {
    match root {
        0x8000_0000 => Some("HKCR"),
        0x8000_0001 => Some("HKCU"),
        0x8000_0002 => Some("HKLM"),
        0x8000_0003 => Some("HKU"),
        _ => None,
    }
}

/// Normalise the textual form of a key path: single backslashes, no
/// leading or trailing separator, `HKEY_LOCAL_MACHINE` style prefixes
/// folded to the short form used internally.
pub fn canonical_key(path: &str) -> String {
    let mut parts: Vec<&str> = path
        .split('\\')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if let Some(first) = parts.first_mut() {
        *first = match first.to_ascii_uppercase().as_str() {
            "HKEY_CLASSES_ROOT" => "HKCR",
            "HKEY_CURRENT_USER" => "HKCU",
            "HKEY_LOCAL_MACHINE" => "HKLM",
            "HKEY_USERS" => "HKU",
            _ => *first,
        };
    }
    parts.join("\\")
}

impl Registry {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
            value_names: HashMap::new(),
            display: HashMap::new(),
            handles: HashMap::new(),
            next_handle: HANDLE_BASE,
        }
    }

    /// A registry pre-populated the way a Pocket PC device would be.
    ///
    /// `HKCU\ControlPanel\Owner` exists on every device (the "Owner
    /// Information" control panel), and games read the owner name to
    /// personalise menus. MetalStrike additionally reads its own
    /// licence pair, which earlier work established as `1739` / `0`.
    /// The `Drivers\BuiltIn` entries describe hardware the emulated
    /// device claims to have; a title that polls for a missing driver
    /// tends to give up and exit rather than fall back.
    pub fn with_device_defaults() -> Self {
        let mut reg = Self::new();
        reg.set_value(
            r"HKCU\ControlPanel\Owner",
            "Owner",
            RegistryValue::Sz("Argon".to_string()),
        );
        reg.set_value(
            r"HKLM\SOFTWARE\Greatelsoft.Com\MetalStrike",
            "SN-Key1",
            RegistryValue::Dword(1739),
        );
        reg.set_value(
            r"HKLM\SOFTWARE\Greatelsoft.Com\MetalStrike",
            "SN-Key2",
            RegistryValue::Dword(0),
        );
        // The g-sensor. Tilt-controlled titles find the accelerometer by
        // opening its driver key rather than by asking a sensor API, so
        // a device without this key reads as a device without a sensor —
        // Xtrakt polls for it (`RegOpenKeyEx`, `Sleep(10)`, retry) and
        // shuts down when the retries run out.
        //
        // `poll_delay` is the sampling interval in milliseconds and
        // `resolution` the counts-per-g the driver reports; both are the
        // values shipped on the HTC handsets these games targeted.
        reg.set_value(
            r"HKLM\Drivers\BuiltIn\Accelerometer",
            "poll_delay",
            RegistryValue::Dword(20),
        );
        reg.set_value(
            r"HKLM\Drivers\BuiltIn\Accelerometer",
            "resolution",
            RegistryValue::Dword(1000),
        );
        // Which way up the driver currently believes the device is.
        // `0` is the unrotated portrait orientation, which is what an
        // emulated screen always is.
        reg.set_value(
            r"HKLM\Drivers\BuiltIn\Accelerometer",
            "current_rotation",
            RegistryValue::Dword(0),
        );
        // The companion screen-rotation service. A game that rotates
        // itself reads `OverrideCounter` to suppress the shell's own
        // auto-rotation while it is in the foreground; zero means
        // nothing is currently overriding it.
        reg.set_value(
            r"HKLM\Services\MultiService\mods\Rotation",
            "OverrideCounter",
            RegistryValue::Dword(0),
        );
        reg
    }

    /// Resolve the `(root, subkey)` pair a `Reg*` call was given.
    ///
    /// `root` is either a predefined `HKEY_*` constant or a handle we
    /// previously returned from [`Registry::open`], which is how games
    /// walk down a tree one level at a time.
    pub fn resolve(&self, root: u32, subkey: &str) -> Option<String> {
        let base = match root_prefix(root) {
            Some(prefix) => prefix.to_string(),
            None => self.handles.get(&root)?.clone(),
        };
        let sub = canonical_key(subkey);
        if sub.is_empty() {
            Some(canonical_key(&base))
        } else {
            Some(canonical_key(&format!("{base}\\{sub}")))
        }
    }

    pub fn contains_key(&self, path: &str) -> bool {
        self.keys
            .contains_key(&canonical_key(path).to_ascii_lowercase())
    }

    pub fn create_key(&mut self, path: &str) {
        let canonical = canonical_key(path);
        let lower = canonical.to_ascii_lowercase();
        self.display.entry(lower.clone()).or_insert(canonical);
        self.keys.entry(lower).or_default();
    }

    pub fn set_value(&mut self, path: &str, name: &str, value: RegistryValue) {
        self.create_key(path);
        let lower_path = canonical_key(path).to_ascii_lowercase();
        let lower_name = name.to_ascii_lowercase();
        self.keys
            .entry(lower_path.clone())
            .or_default()
            .insert(lower_name.clone(), value);
        self.value_names
            .entry(lower_path)
            .or_default()
            .entry(lower_name)
            .or_insert_with(|| name.to_string());
    }

    pub fn value(&self, path: &str, name: &str) -> Option<&RegistryValue> {
        self.keys
            .get(&canonical_key(path).to_ascii_lowercase())?
            .get(&name.to_ascii_lowercase())
    }

    pub fn value_count(&self, path: &str) -> usize {
        self.keys
            .get(&canonical_key(path).to_ascii_lowercase())
            .map_or(0, HashMap::len)
    }

    pub fn value_at(&self, path: &str, index: usize) -> Option<(String, RegistryValue)> {
        let lower_path = canonical_key(path).to_ascii_lowercase();
        let values = self.keys.get(&lower_path)?;
        let mut names: Vec<_> = values.keys().collect();
        names.sort_unstable();
        let lower_name = *names.get(index)?;
        let name = self
            .value_names
            .get(&lower_path)
            .and_then(|value_names| value_names.get(lower_name))
            .unwrap_or(lower_name);
        Some((name.clone(), values.get(lower_name)?.clone()))
    }

    pub fn value_stats(&self, path: &str) -> (usize, usize, usize) {
        let lower_path = canonical_key(path).to_ascii_lowercase();
        let Some(values) = self.keys.get(&lower_path) else {
            return (0, 0, 0);
        };
        let max_name_len = values
            .keys()
            .map(|lower_name| {
                self.value_names
                    .get(&lower_path)
                    .and_then(|names| names.get(lower_name))
                    .unwrap_or(lower_name)
                    .encode_utf16()
                    .count()
            })
            .max()
            .unwrap_or(0);
        let max_data_len = values
            .values()
            .map(|value| value.to_bytes().len())
            .max()
            .unwrap_or(0);
        (values.len(), max_name_len, max_data_len)
    }

    pub fn delete_value(&mut self, path: &str, name: &str) -> bool {
        let lower_path = canonical_key(path).to_ascii_lowercase();
        let lower_name = name.to_ascii_lowercase();
        let removed = self
            .keys
            .get_mut(&lower_path)
            .and_then(|values| values.remove(&lower_name))
            .is_some();
        if removed {
            if let Some(names) = self.value_names.get_mut(&lower_path) {
                names.remove(&lower_name);
            }
        }
        removed
    }

    /// Hand out a handle for an existing key. Returns `None` when the
    /// key was never created, so `RegOpenKeyEx` can report
    /// `ERROR_FILE_NOT_FOUND` the way a real device does.
    pub fn open(&mut self, path: &str) -> Option<u32> {
        let canonical = canonical_key(path);
        let lower = canonical.to_ascii_lowercase();
        if !self.keys.contains_key(&lower) {
            return None;
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.handles.insert(handle, lower);
        Some(handle)
    }

    /// Create the key if needed, then hand out a handle for it.
    pub fn create_and_open(&mut self, path: &str) -> u32 {
        self.create_key(path);
        self.open(path).unwrap_or(HANDLE_BASE)
    }

    pub fn path_for(&self, handle: u32) -> Option<String> {
        if let Some(prefix) = root_prefix(handle) {
            return Some(prefix.to_string());
        }
        self.handles.get(&handle).cloned()
    }

    pub fn close(&mut self, handle: u32) {
        self.handles.remove(&handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalises_paths_and_roots() {
        assert_eq!(
            canonical_key(r"HKEY_LOCAL_MACHINE\SOFTWARE\Apps\"),
            r"HKLM\SOFTWARE\Apps"
        );
        assert_eq!(
            canonical_key(r"\ControlPanel\Owner\"),
            r"ControlPanel\Owner"
        );
    }

    #[test]
    fn stores_and_reads_values_case_insensitively() {
        let mut reg = Registry::new();
        reg.set_value(
            r"HKLM\SOFTWARE\Apps\Astraware Bejeweled",
            "SaveDir",
            RegistryValue::Sz(r"\My Documents\My Saved Games\Bejeweled".to_string()),
        );
        assert_eq!(
            reg.value(r"hklm\software\apps\astraware bejeweled", "savedir"),
            Some(&RegistryValue::Sz(
                r"\My Documents\My Saved Games\Bejeweled".to_string()
            ))
        );
    }

    #[test]
    fn open_only_succeeds_for_existing_keys() {
        let mut reg = Registry::new();
        assert!(reg.open(r"HKLM\SOFTWARE\Nope").is_none());
        reg.create_key(r"HKLM\SOFTWARE\Yes");
        let handle = reg.open(r"HKLM\SOFTWARE\Yes").expect("key exists");
        assert_eq!(reg.path_for(handle).as_deref(), Some(r"hklm\software\yes"));
        // A handle can be used as the root of a relative open.
        reg.create_key(r"HKLM\SOFTWARE\Yes\Child");
        assert_eq!(
            reg.resolve(handle, "Child").as_deref(),
            Some(r"hklm\software\yes\Child")
        );
        reg.close(handle);
        assert!(reg.path_for(handle).is_none());
    }

    #[test]
    fn enumerates_values_in_stable_order_and_preserves_their_names() {
        let mut reg = Registry::new();
        reg.set_value("HKLM\\Settings", "zeta", RegistryValue::Dword(9));
        reg.set_value(
            "HKLM\\Settings",
            "Alpha",
            RegistryValue::Sz("x".to_string()),
        );
        reg.set_value(
            "HKLM\\Settings",
            "ALPHA",
            RegistryValue::Sz("x".to_string()),
        );

        assert_eq!(reg.value_count("hklm\\settings"), 2);
        assert_eq!(
            reg.value_at("hklm\\settings", 0),
            Some(("Alpha".to_string(), RegistryValue::Sz("x".to_string())))
        );
        assert_eq!(
            reg.value_at("hklm\\settings", 1),
            Some(("zeta".to_string(), RegistryValue::Dword(9)))
        );
        assert_eq!(reg.value_at("hklm\\settings", 2), None);
        assert_eq!(reg.value_stats("hklm\\settings"), (2, 5, 4));
    }

    #[test]
    fn dword_and_string_encodings_match_win32() {
        assert_eq!(RegistryValue::Dword(1739).type_code(), 4);
        assert_eq!(RegistryValue::Dword(1739).to_bytes(), 1739u32.to_le_bytes());
        let sz = RegistryValue::Sz("AB".to_string());
        assert_eq!(sz.type_code(), 1);
        assert_eq!(sz.to_bytes(), vec![b'A', 0, b'B', 0, 0, 0]);
    }
}
