//! In-memory Windows CE object-store database state for CEDB API handlers.
//!
//! This models the database, record, property and seek-handle behavior needed by native Pocket PC titles. It is process-scoped; it does not claim to emulate mounted database volumes or persist data between emulator runs.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

pub const SEEK_CEOID: u32 = 0x0001;
pub const SEEK_BEGINNING: u32 = 0x0002;
pub const SEEK_END: u32 = 0x0004;
pub const SEEK_CURRENT: u32 = 0x0008;
pub const SEEK_VALUE_SMALLER: u32 = 0x0010;
pub const SEEK_VALUE_FIRST_EQUAL: u32 = 0x0020;
pub const SEEK_VALUE_GREATER: u32 = 0x0040;
pub const SEEK_VALUE_NEXT_EQUAL: u32 = 0x0080;

pub const SORT_DESCENDING: u32 = 0x0001;
pub const SORT_CASE_INSENSITIVE: u32 = 0x0002;
pub const SORT_UNKNOWN_FIRST: u32 = 0x0004;
pub const OPEN_AUTOINCREMENT: u32 = 0x0001;

#[derive(Clone, Debug, PartialEq)]
pub enum PropertyValue {
    I2(i16),
    I4(i32),
    R8(u64),
    Bool(i32),
    Ui2(u16),
    Ui4(u32),
    String(String),
    FileTime(u64),
    Blob(Vec<u8>),
    Raw { type_code: u16, bits: u64 },
}

impl PropertyValue {
    fn compare(&self, other: &Self, sort_flags: u32) -> Option<Ordering> {
        let order = match (self, other) {
            (Self::I2(a), Self::I2(b)) => a.cmp(b),
            (Self::I4(a), Self::I4(b)) => a.cmp(b),
            (Self::R8(a), Self::R8(b)) => f64::from_bits(*a).total_cmp(&f64::from_bits(*b)),
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Ui2(a), Self::Ui2(b)) => a.cmp(b),
            (Self::Ui4(a), Self::Ui4(b)) => a.cmp(b),
            (Self::String(a), Self::String(b)) if sort_flags & SORT_CASE_INSENSITIVE != 0 => {
                a.to_lowercase().cmp(&b.to_lowercase())
            }
            (Self::String(a), Self::String(b)) => a.cmp(b),
            (Self::FileTime(a), Self::FileTime(b)) => a.cmp(b),
            (
                Self::Raw {
                    type_code: at,
                    bits: a,
                },
                Self::Raw {
                    type_code: bt,
                    bits: b,
                },
            ) if at == bt => a.cmp(b),
            _ => return None,
        };
        Some(if sort_flags & SORT_DESCENDING != 0 {
            order.reverse()
        } else {
            order
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    pub propid: u32,
    pub value: PropertyValue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteProperty {
    pub propid: u32,
    pub value: Option<PropertyValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub oid: u32,
    pub properties: BTreeMap<u32, PropertyValue>,
}

#[derive(Clone, Debug)]
pub struct Database {
    pub oid: u32,
    pub name: String,
    pub database_type: u32,
    pub sort_orders: Vec<(u32, u32)>,
    pub records: Vec<Record>,
}

#[derive(Clone, Debug)]
struct DatabaseHandle {
    database_oid: u32,
    sort_propid: u32,
    sort_flags: u32,
    flags: u32,
    cursor_oid: Option<u32>,
}

pub struct DatabaseStore {
    databases: HashMap<u32, Database>,
    database_names: HashMap<String, u32>,
    handles: HashMap<u32, DatabaseHandle>,
    next_database_oid: u32,
    next_record_oid: u32,
    next_handle: u32,
}

impl Default for DatabaseStore {
    fn default() -> Self {
        Self {
            databases: HashMap::new(),
            database_names: HashMap::new(),
            handles: HashMap::new(),
            next_database_oid: 0xCE00_0001,
            next_record_oid: 0xCE10_0001,
            next_handle: 0xDEAD_D000,
        }
    }
}

impl DatabaseStore {
    pub fn create_database(
        &mut self,
        name: String,
        database_type: u32,
        sort_orders: Vec<(u32, u32)>,
    ) -> Option<u32> {
        let key = name.to_lowercase();
        if name.is_empty() || self.database_names.contains_key(&key) {
            return None;
        }
        let oid = take_id(&mut self.next_database_oid);
        self.database_names.insert(key, oid);
        self.databases.insert(
            oid,
            Database {
                oid,
                name,
                database_type,
                sort_orders,
                records: Vec::new(),
            },
        );
        Some(oid)
    }

    pub fn open_database(
        &mut self,
        requested_oid: u32,
        name: Option<&str>,
        propid: u32,
        flags: u32,
    ) -> Option<(u32, u32)> {
        let database_oid = if requested_oid != 0 {
            requested_oid
        } else {
            let key = name?.to_lowercase();
            *self.database_names.get(&key)?
        };
        let database = self.databases.get(&database_oid)?;
        let selected = if propid == 0 {
            database.sort_orders.first().copied().unwrap_or((0, 0))
        } else {
            database
                .sort_orders
                .iter()
                .copied()
                .find(|(sort_propid, _)| *sort_propid == propid)
                .unwrap_or((propid, 0))
        };
        let handle = take_id(&mut self.next_handle);
        self.handles.insert(
            handle,
            DatabaseHandle {
                database_oid,
                sort_propid: selected.0,
                sort_flags: selected.1,
                flags,
                cursor_oid: None,
            },
        );
        Some((handle, database_oid))
    }

    pub fn close_database(&mut self, handle: u32) -> bool {
        self.handles.remove(&handle).is_some()
    }

    pub fn write_record(
        &mut self,
        handle: u32,
        record_oid: u32,
        properties: Vec<WriteProperty>,
    ) -> Option<u32> {
        let database_oid = self.handles.get(&handle)?.database_oid;
        let new_oid = if record_oid == 0 {
            take_id(&mut self.next_record_oid)
        } else {
            record_oid
        };
        let database = self.databases.get_mut(&database_oid)?;
        let record = if let Some(record) = database.records.iter_mut().find(|r| r.oid == new_oid) {
            record
        } else if record_oid == 0 {
            database.records.push(Record {
                oid: new_oid,
                properties: BTreeMap::new(),
            });
            database.records.last_mut()?
        } else {
            return None;
        };
        for property in properties {
            if let Some(value) = property.value {
                record.properties.insert(property.propid, value);
            } else {
                record.properties.remove(&property.propid);
            }
        }
        let oid = record.oid;
        self.handles.get_mut(&handle)?.cursor_oid = Some(oid);
        Some(oid)
    }

    pub fn delete_record(&mut self, handle: u32, record_oid: u32) -> bool {
        let Some(database_oid) = self.handles.get(&handle).map(|h| h.database_oid) else {
            return false;
        };
        let Some(database) = self.databases.get_mut(&database_oid) else {
            return false;
        };
        let Some(index) = database.records.iter().position(|r| r.oid == record_oid) else {
            return false;
        };
        database.records.remove(index);
        for open_handle in self.handles.values_mut() {
            if open_handle.database_oid == database_oid
                && open_handle.cursor_oid == Some(record_oid)
            {
                open_handle.cursor_oid = None;
            }
        }
        true
    }

    pub fn delete_database(&mut self, database_oid: u32) -> bool {
        if self
            .handles
            .values()
            .any(|handle| handle.database_oid == database_oid)
        {
            return false;
        }
        let Some(database) = self.databases.remove(&database_oid) else {
            return false;
        };
        self.database_names.remove(&database.name.to_lowercase());
        true
    }

    pub fn seek(
        &mut self,
        handle: u32,
        seek_type: u32,
        value: u32,
        query: Option<&Property>,
    ) -> Option<(u32, u32)> {
        let open_handle = self.handles.get(&handle)?;
        let database = self.databases.get(&open_handle.database_oid)?;
        let ordered = ordered_records(database, open_handle.sort_propid, open_handle.sort_flags);
        let current_index = open_handle
            .cursor_oid
            .and_then(|oid| ordered.iter().position(|record| *record == oid));
        let found_index = match seek_type {
            SEEK_CEOID => ordered.iter().position(|record| *record == value),
            SEEK_BEGINNING => index_if_in_range(value as u64, ordered.len()),
            SEEK_END => ordered.len().checked_sub(value as usize + 1),
            SEEK_CURRENT => {
                let current = current_index? as i64;
                index_if_in_range((current + value as i32 as i64) as u64, ordered.len()).or_else(
                    || {
                        let target = current + value as i32 as i64;
                        (target >= 0 && target < ordered.len() as i64).then_some(target as usize)
                    },
                )
            }
            SEEK_VALUE_SMALLER
            | SEEK_VALUE_FIRST_EQUAL
            | SEEK_VALUE_GREATER
            | SEEK_VALUE_NEXT_EQUAL => {
                let query = query?;
                if open_handle.sort_propid != 0 && query.propid != open_handle.sort_propid {
                    return None;
                }
                let mut candidates: Vec<usize> = (0..ordered.len()).collect();
                if seek_type == SEEK_VALUE_NEXT_EQUAL {
                    let current = current_index?;
                    candidates.retain(|index| *index > current);
                }
                if seek_type == SEEK_VALUE_SMALLER {
                    candidates.reverse();
                }
                candidates.into_iter().find(|index| {
                    let Some(record) = database.records.iter().find(|r| r.oid == ordered[*index])
                    else {
                        return false;
                    };
                    let Some(value) = record.properties.get(&query.propid) else {
                        return false;
                    };
                    let Some(order) = value.compare(&query.value, open_handle.sort_flags) else {
                        return false;
                    };
                    match seek_type {
                        SEEK_VALUE_SMALLER => order == Ordering::Less,
                        SEEK_VALUE_FIRST_EQUAL | SEEK_VALUE_NEXT_EQUAL => order == Ordering::Equal,
                        SEEK_VALUE_GREATER => order == Ordering::Greater,
                        _ => false,
                    }
                })
            }
            _ => None,
        }?;
        let record_oid = *ordered.get(found_index)?;
        self.handles.get_mut(&handle)?.cursor_oid = Some(record_oid);
        Some((record_oid, found_index as u32))
    }

    pub fn current_record(&self, handle: u32) -> Option<Record> {
        let open_handle = self.handles.get(&handle)?;
        let database = self.databases.get(&open_handle.database_oid)?;
        let record_oid = open_handle.cursor_oid?;
        database
            .records
            .iter()
            .find(|record| record.oid == record_oid)
            .cloned()
    }

    pub fn advance_cursor(&mut self, handle: u32) {
        let Some(open_handle) = self.handles.get(&handle).cloned() else {
            return;
        };
        if open_handle.flags & OPEN_AUTOINCREMENT == 0 {
            return;
        }
        let Some(database) = self.databases.get(&open_handle.database_oid) else {
            return;
        };
        let ordered = ordered_records(database, open_handle.sort_propid, open_handle.sort_flags);
        let next = open_handle
            .cursor_oid
            .and_then(|oid| ordered.iter().position(|candidate| *candidate == oid))
            .and_then(|index| ordered.get(index + 1).copied());
        if let Some(handle) = self.handles.get_mut(&handle) {
            handle.cursor_oid = next;
        }
    }

    pub fn database(&self, oid: u32) -> Option<&Database> {
        self.databases.get(&oid)
    }
}

fn take_id(next: &mut u32) -> u32 {
    let id = *next;
    *next = (*next).wrapping_add(1).max(1);
    id
}

fn index_if_in_range(index: u64, len: usize) -> Option<usize> {
    (index < len as u64).then_some(index as usize)
}

fn ordered_records(database: &Database, propid: u32, sort_flags: u32) -> Vec<u32> {
    let mut records: Vec<&Record> = database.records.iter().collect();
    if propid != 0 {
        records.sort_by(|left, right| {
            let left_value = left.properties.get(&propid);
            let right_value = right.properties.get(&propid);
            match (left_value, right_value) {
                (Some(a), Some(b)) => a
                    .compare(b, sort_flags)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| left.oid.cmp(&right.oid)),
                (Some(_), None) => {
                    if sort_flags & SORT_UNKNOWN_FIRST != 0 {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (None, Some(_)) => {
                    if sort_flags & SORT_UNKNOWN_FIRST != 0 {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (None, None) => left.oid.cmp(&right.oid),
            }
        });
    }
    records.into_iter().map(|record| record.oid).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_open_write_and_seek_round_trip() {
        let mut store = DatabaseStore::default();
        let propid = 0x0001_0003;
        let database_oid = store
            .create_database("Scores".to_string(), 7, vec![(propid, 0)])
            .unwrap();
        let (handle, resolved_oid) = store.open_database(0, Some("scores"), propid, 0).unwrap();
        assert_eq!(resolved_oid, database_oid);
        let record_oid = store
            .write_record(
                handle,
                0,
                vec![WriteProperty {
                    propid,
                    value: Some(PropertyValue::I4(42)),
                }],
            )
            .unwrap();
        assert_eq!(
            store.seek(handle, SEEK_BEGINNING, 0, None),
            Some((record_oid, 0))
        );
        assert_eq!(
            store.seek(handle, SEEK_CEOID, record_oid, None),
            Some((record_oid, 0))
        );
        assert_eq!(
            store.current_record(handle).unwrap().properties[&propid],
            PropertyValue::I4(42)
        );
        assert!(store.close_database(handle));
        assert!(store.current_record(handle).is_none());
    }

    #[test]
    fn seek_uses_zero_based_offsets_and_keeps_cursor_on_failure() {
        let mut store = DatabaseStore::default();
        let propid = 0x0001_0003;
        let database_oid = store
            .create_database("scores".to_string(), 0, vec![(propid, 0)])
            .unwrap();
        let (handle, _) = store.open_database(database_oid, None, propid, 0).unwrap();
        let mut ids = Vec::new();
        for score in [9, 3, 6] {
            ids.push(
                store
                    .write_record(
                        handle,
                        0,
                        vec![WriteProperty {
                            propid,
                            value: Some(PropertyValue::I4(score)),
                        }],
                    )
                    .unwrap(),
            );
        }
        assert_eq!(
            store.seek(handle, SEEK_BEGINNING, 0, None),
            Some((ids[1], 0))
        );
        assert_eq!(store.seek(handle, SEEK_END, 0, None), Some((ids[0], 2)));
        assert_eq!(store.seek(handle, SEEK_CEOID, 0x1234, None), None);
        assert_eq!(store.current_record(handle).unwrap().oid, ids[0]);
    }

    #[test]
    fn value_seek_respects_the_selected_sort_order() {
        let mut store = DatabaseStore::default();
        let propid = 0x0001_0003;
        let database_oid = store
            .create_database("scores".to_string(), 0, vec![(propid, 0)])
            .unwrap();
        let (handle, _) = store.open_database(database_oid, None, propid, 0).unwrap();
        let mut ids = Vec::new();
        for score in [30, 10, 20] {
            ids.push(
                store
                    .write_record(
                        handle,
                        0,
                        vec![WriteProperty {
                            propid,
                            value: Some(PropertyValue::I4(score)),
                        }],
                    )
                    .unwrap(),
            );
        }
        let query = Property {
            propid,
            value: PropertyValue::I4(15),
        };
        assert_eq!(
            store.seek(handle, SEEK_VALUE_GREATER, 0, Some(&query)),
            Some((ids[2], 1))
        );
        assert_eq!(
            store.seek(handle, SEEK_VALUE_SMALLER, 0, Some(&query)),
            Some((ids[1], 0))
        );
    }
}
