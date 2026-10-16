//! Shared kernel object storage. A guard retains the lock while accessing state.
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};

pub struct SharedObjects<T>(Arc<Mutex<HashMap<u32, T>>>);
impl<T> Clone for SharedObjects<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Default for SharedObjects<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}
pub struct ObjectGuard<'a, T> {
    map: MutexGuard<'a, HashMap<u32, T>>,
    key: u32,
}
impl<T> Deref for ObjectGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.map.get(&self.key).unwrap()
    }
}
impl<T> DerefMut for ObjectGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.map.get_mut(&self.key).unwrap()
    }
}
impl<T> SharedObjects<T> {
    pub fn get(&self, key: &u32) -> Option<ObjectGuard<'_, T>> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(key) {
            return None;
        }
        Some(ObjectGuard { map, key: *key })
    }
    pub fn get_mut(&self, key: &u32) -> Option<ObjectGuard<'_, T>> {
        self.get(key)
    }
    pub fn insert(&self, key: u32, object: T) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, object)
    }
    pub fn insert_if_absent(&self, key: u32, object: T) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert(object);
    }
    pub fn remove(&self, key: &u32) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key)
    }
    pub fn contains_key(&self, key: &u32) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn retain(&self, mut predicate: impl FnMut(&u32, &mut T) -> bool) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, value| predicate(key, value));
    }
}
