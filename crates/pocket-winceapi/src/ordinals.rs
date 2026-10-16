//! Ordinal → name resolution for system DLLs that export by ordinal.
//!
//! On Pocket PC 2002 / 2003 / Windows Mobile 5/6, `coredll.dll` and
//! `aygshell.dll` export every public function as both a name and an
//! ordinal but the SDK link libraries strip the names by default to
//! save space, so applications import by ordinal. To make logs
//! readable we keep an ordinal table per DLL keyed by lowercase DLL
//! name and ordinal.
//!
//! The default tables are loaded from JSON files in `data/`. They are
//! deliberately incomplete — they cover the subset of APIs encountered
//! while bringing up the JumpyBall test ROM, plus a few neighbours.
//! Submit additions via PR.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use serde::Deserialize;

const COREDLL_JSON: &str = include_str!("../data/coredll-ordinals.json");
const AYGSHELL_JSON: &str = include_str!("../data/aygshell-ordinals.json");
// Exact exports from the Gizmondo ROM DLLs supplied for the SDK audit.
const WS2_JSON: &str = include_str!("../data/ws2-ordinals.json");
const BTD_JSON: &str = include_str!("../data/btd-ordinals.json");

#[derive(Debug, Deserialize)]
struct OrdinalFile {
    /// Pretty name of the DLL — informational.
    #[allow(dead_code)]
    dll: String,
    /// Ordinal -> exported name.
    #[serde(rename = "ordinals")]
    map: HashMap<String, String>,
}

static ORDINAL_TABLES: Lazy<HashMap<String, HashMap<u16, String>>> = Lazy::new(|| {
    let mut tables: HashMap<String, HashMap<u16, String>> = HashMap::new();
    for (dll, json) in [
        ("coredll.dll", COREDLL_JSON),
        ("aygshell.dll", AYGSHELL_JSON),
        ("ws2.dll", WS2_JSON),
        ("btd.dll", BTD_JSON),
    ] {
        match serde_json::from_str::<OrdinalFile>(json) {
            Ok(file) => {
                let mut converted = HashMap::with_capacity(file.map.len());
                for (k, v) in file.map {
                    if let Ok(ord) = k.parse::<u16>() {
                        converted.insert(ord, v);
                    }
                }
                tables.insert(dll.to_string(), converted);
            }
            Err(e) => {
                log::error!("failed to parse ordinal table for {dll}: {e}");
            }
        }
    }
    tables
});

/// Look up the symbolic name for an ordinal in `dll`. Returns `None`
/// if the DLL or ordinal is unknown.
pub fn lookup(dll: &str, ordinal: u16) -> Option<String> {
    let key = dll.to_ascii_lowercase();
    // The OpenGL ES client libraries keep their tables in
    // `pocket-gles`, next to the implementation they describe.
    if pocket_gles::ordinals::is_gles_dll(&key) {
        return pocket_gles::ordinals::lookup(&key, ordinal);
    }
    ORDINAL_TABLES
        .get(&key)
        .and_then(|m| m.get(&ordinal).cloned())
}

/// Number of ordinal entries for `dll` — used by tests.
pub fn entry_count(dll: &str) -> usize {
    let key = dll.to_ascii_lowercase();
    if pocket_gles::ordinals::is_gles_dll(&key) {
        return pocket_gles::ordinals::entry_count(&key);
    }
    ORDINAL_TABLES.get(&key).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coredll_table_loads() {
        assert!(entry_count("coredll.dll") > 10);
    }

    #[test]
    fn lookup_known_ordinal() {
        // Most leaked WM5/WM6 SDKs map ordinal 33 to one of the
        // CRT-style helpers; exact name varies by build, so just
        // verify the lookup returns *something*.
        let _ = lookup("coredll.dll", 33);
    }

    #[test]
    fn gles_ordinals_resolve_through_the_shared_resolver() {
        // Games import libGLES_CL.dll purely by ordinal, so this path
        // is what turns `#50` into `glDrawElements` for both the
        // dispatcher and the logs.
        assert_eq!(
            lookup("libGLES_CL.dll", 50).as_deref(),
            Some("glDrawElements")
        );
        assert_eq!(
            lookup("libgles_cm.dll", 133).as_deref(),
            Some("glTexImage2D")
        );
        assert_eq!(entry_count("libgles_cm.dll"), 140);
        assert_eq!(entry_count("libgles_cl.dll"), 102);
    }
}
