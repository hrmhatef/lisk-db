use std::cmp;
use std::collections::HashMap;
use std::sync::Arc;

use neon::prelude::*;
use thiserror::Error;

use crate::batch;
use crate::common_db::{
    DatabaseKind, JsArcMutex, JsNewWithArcMutex, Kind as DBKind, NewDBWithKeyLength,
};
use crate::diff;
use crate::options::IterationOption;
use crate::types::{Cache, KVPair, KeyLength, SharedKVPair, VecOption};
use crate::utils;

pub type SendableStateWriter = JsArcMutex<StateWriter>;

trait Batch {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>);
    fn delete(&mut self, key: Box<[u8]>);
}

#[derive(Error, Debug)]
pub enum StateWriterError {
    #[error("Invalid usage")]
    InvalidUsage,
}

#[derive(Clone, Debug)]
pub struct StateCache {
    init: VecOption,
    value: Vec<u8>,
    dirty: bool,
    deleted: bool,
}

#[derive(Default)]
pub struct StateWriter {
    counter: u32,
    pub backup: HashMap<u32, HashMap<Vec<u8>, StateCache>>,
    pub cache: HashMap<Vec<u8>, StateCache>,
}

impl DatabaseKind for StateWriter {
    fn db_kind() -> DBKind {
        DBKind::StateWriter
    }
}

impl Clone for StateWriter {
    fn clone(&self) -> Self {
        let mut cloned = StateWriter::default();
        cloned.cache.clone_from(&self.cache);
        cloned
    }
}

impl NewDBWithKeyLength for StateWriter {
    fn new_db_with_key_length(_: Option<KeyLength>) -> Self {
        Self::default()
    }
}

impl JsNewWithArcMutex for StateWriter {}
impl Finalize for StateWriter {}

impl StateCache {
    fn new(val: &[u8]) -> Self {
        Self {
            init: None,
            value: val.to_vec(),
            dirty: false,
            deleted: false,
        }
    }

    fn new_existing(val: &[u8]) -> Self {
        Self {
            init: Some(val.to_vec()),
            value: val.to_vec(),
            dirty: false,
            deleted: false,
        }
    }
}

impl StateWriter {
    pub fn cache_new(&mut self, pair: &SharedKVPair) {
        let cache = StateCache::new(pair.value());
        self.cache.insert(pair.key_as_vec(), cache);
    }

    pub fn cache_existing(&mut self, pair: &SharedKVPair) {
        let cache = StateCache::new_existing(pair.value());
        self.cache.insert(pair.key_as_vec(), cache);
    }

    pub fn get(&self, key: &[u8]) -> (Vec<u8>, bool, bool) {
        let val = self.cache.get(key);
        if val.is_none() {
            return (vec![], false, false);
        }
        let val = val.unwrap();
        if val.deleted {
            return (vec![], true, true);
        }
        (val.value.clone(), false, true)
    }

    pub fn is_cached(&self, key: &[u8]) -> bool {
        self.cache.get(key).is_some()
    }

    pub fn get_range(&self, options: &IterationOption) -> Cache {
        let start = options.gte.as_ref().unwrap();
        let end = options.lte.as_ref().unwrap();
        self.cache
            .iter()
            .filter_map(|(k, v)| {
                if utils::compare(k, start) != cmp::Ordering::Less
                    && utils::compare(k, end) != cmp::Ordering::Greater
                    && !v.deleted
                {
                    Some((k.to_vec(), v.value.to_vec()))
                } else {
                    None
                }
            })
            .collect::<Cache>()
    }

    pub fn update(&mut self, pair: &KVPair) -> Result<(), StateWriterError> {
        let mut cached = self
            .cache
            .get_mut(pair.key())
            .ok_or(StateWriterError::InvalidUsage)?;
        cached.value = pair.value_as_vec();
        cached.dirty = true;
        cached.deleted = false;
        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) {
        let cached = self.cache.get_mut(key);
        if cached.is_none() {
            return;
        }
        let mut cached = cached.unwrap();
        if cached.init.is_none() {
            self.cache.remove(key);
            return;
        }
        cached.deleted = true;
    }

    fn snapshot(&mut self) -> u32 {
        self.backup.insert(self.counter, self.cache.clone());
        let index = self.counter;
        self.counter += 1;
        index
    }

    fn restore_snapshot(&mut self, index: u32) -> Result<(), StateWriterError> {
        let backup = self
            .backup
            .get(&index)
            .ok_or(StateWriterError::InvalidUsage)?;
        self.cache.clone_from(backup);
        self.backup = HashMap::new();
        Ok(())
    }

    pub fn get_updated(&self) -> Cache {
        let mut result = Cache::new();
        for (key, value) in self.cache.iter() {
            if value.init.is_none() || value.dirty {
                result.insert(key.clone(), value.value.clone());
                continue;
            }
            if value.deleted {
                result.insert(key.clone(), vec![]);
            }
        }
        result
    }

    pub fn commit(&self, batch: &mut impl batch::BatchWriter) -> diff::Diff {
        let mut created = vec![];
        let mut updated = vec![];
        let mut deleted = vec![];
        for (key, value) in self.cache.iter() {
            let kv = KVPair::new(key, &value.value);
            if value.init.is_none() {
                created.push(key.to_vec());
                batch.put(&kv);
                continue;
            }
            if value.deleted {
                deleted.push(KVPair::new(key, &value.value));
                batch.delete(key);
                continue;
            }
            if value.dirty {
                updated.push(KVPair::new(key, value.init.as_ref().unwrap()));
                batch.put(&kv);
                continue;
            }
        }
        diff::Diff::new(created, updated, deleted)
    }
}

impl StateWriter {
    pub fn js_snapshot(mut ctx: FunctionContext) -> JsResult<JsNumber> {
        let writer = ctx
            .this()
            .downcast_or_throw::<SendableStateWriter, _>(&mut ctx)?;

        let batch = Arc::clone(&writer.borrow());
        let mut inner_writer = batch.lock().unwrap();

        let index = inner_writer.snapshot();

        Ok(ctx.number(index))
    }

    pub fn js_restore_snapshot(mut ctx: FunctionContext) -> JsResult<JsUndefined> {
        let writer = ctx
            .this()
            .downcast_or_throw::<SendableStateWriter, _>(&mut ctx)?;

        let batch = Arc::clone(&writer.borrow());
        let mut inner_writer = batch.lock().unwrap();
        let index = ctx.argument::<JsNumber>(0)?.value(&mut ctx) as u32;

        match inner_writer.restore_snapshot(index) {
            Ok(()) => Ok(ctx.undefined()),
            Err(error) => ctx.throw_error(error.to_string())?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache() {
        let mut writer = StateWriter::default();

        writer.cache_new(&SharedKVPair::new(&[0, 0, 2], &[1, 2, 3]));
        writer.cache_existing(&SharedKVPair::new(&[0, 0, 3], &[1, 2, 4]));

        let (value, deleted, exists) = writer.get(&[0, 0, 2]);
        assert_eq!(value, &[1, 2, 3]);
        assert!(!deleted);
        assert!(exists);

        let (value, deleted, exists) = writer.get(&[0, 0, 3]);
        assert_eq!(value, &[1, 2, 4]);
        assert!(!deleted);
        assert!(exists);

        let (value, deleted, exists) = writer.get(&[0, 0, 1]);
        assert_eq!(value, &[]);
        assert!(!deleted);
        assert!(!exists)
    }
}
