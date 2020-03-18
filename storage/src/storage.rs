// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Error, Result};
use crypto::HashValue;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;

/// Type alias to improve readability.
pub type ColumnFamilyName = &'static str;

/// Use for batch commit
pub trait WriteBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()>;
    fn delete(&mut self, key: Vec<u8>) -> Result<()>;
}

pub trait Repository: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()>;
    fn contains_key(&self, key: Vec<u8>) -> Result<bool>;
    fn remove(&self, key: Vec<u8>) -> Result<()>;
    fn get_len(&self) -> Result<u64>;
    fn keys(&self) -> Result<Vec<Vec<u8>>>;
}

pub trait InnerRepository: Send + Sync {
    fn get(&self, prefix_name: &str, key: Vec<u8>) -> Result<Option<Vec<u8>>>;
    fn put(&self, prefix_name: &str, key: Vec<u8>, value: Vec<u8>) -> Result<()>;
    fn contains_key(&self, prefix_name: &str, key: Vec<u8>) -> Result<bool>;
    fn remove(&self, prefix_name: &str, key: Vec<u8>) -> Result<()>;
    fn get_len(&self) -> Result<u64>;
    fn keys(&self) -> Result<Vec<Vec<u8>>>;
}

pub struct StorageDelegated {
    repository: Arc<dyn InnerRepository>,
    pub prefix_name: ColumnFamilyName,
}
impl StorageDelegated {
    pub fn new(repository: Arc<dyn InnerRepository>, prefix_name: ColumnFamilyName) -> Self {
        Self {
            repository,
            prefix_name,
        }
    }
}

impl Repository for StorageDelegated {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        self.repository.clone().get(self.prefix_name, key.to_vec())
    }

    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), Error> {
        self.repository.clone().put(self.prefix_name, key, value)
    }

    fn contains_key(&self, key: Vec<u8>) -> Result<bool, Error> {
        self.repository.clone().contains_key(self.prefix_name, key)
    }

    fn remove(&self, key: Vec<u8>) -> Result<(), Error> {
        self.repository.clone().remove(self.prefix_name, key)
    }

    fn get_len(&self) -> Result<u64, Error> {
        self.repository.clone().get_len()
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, Error> {
        self.repository.clone().keys()
    }
}

/// two level storage package
pub struct Storage {
    cache: Arc<dyn InnerRepository>,
    db: Arc<dyn InnerRepository>,
    pub prefix_name: ColumnFamilyName,
}

impl Storage {
    pub fn new(
        cache: Arc<dyn InnerRepository>,
        db: Arc<dyn InnerRepository>,
        prefix_name: ColumnFamilyName,
    ) -> Self {
        Storage {
            cache,
            db,
            prefix_name,
        }
    }
}

impl Repository for Storage {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        // first get from cache
        let key_vec = key.to_vec();
        match self.cache.clone().get(self.prefix_name, key_vec.clone()) {
            Ok(v) => Ok(v),
            _ => self.db.clone().get(self.prefix_name, key_vec.clone()),
        }
    }

    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), Error> {
        self.db
            .clone()
            .put(self.prefix_name, key.clone(), value.clone())
            .unwrap();
        self.cache.clone().put(self.prefix_name, key, value)
    }

    fn contains_key(&self, key: Vec<u8>) -> Result<bool, Error> {
        self.cache.clone().contains_key(self.prefix_name, key)
    }

    fn remove(&self, key: Vec<u8>) -> Result<(), Error> {
        match self.db.clone().remove(self.prefix_name, key.clone()) {
            Ok(_) => self.cache.clone().remove(self.prefix_name, key),
            Err(err) => bail!("remove persistence error: {}", err),
        }
    }

    fn get_len(&self) -> Result<u64, Error> {
        self.cache.get_len()
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, Error> {
        self.cache.keys()
    }
}

pub trait KeyCodec: Sized + PartialEq + Debug {
    /// Converts `self` to bytes to be stored in DB.
    fn encode_key(&self) -> Result<Vec<u8>>;
    /// Converts bytes fetched from DB to `Self`.
    fn decode_key(data: &[u8]) -> Result<Self>;
}

pub trait ValueCodec: Sized + PartialEq + Debug {
    /// Converts `self` to bytes to be stored in DB.
    fn encode_value(&self) -> Result<Vec<u8>>;
    /// Converts bytes fetched from DB to `Self`.
    fn decode_value(data: &[u8]) -> Result<Self>;
}

pub struct CodecStorage<K, V>
where
    K: KeyCodec,
    V: ValueCodec,
{
    store: Arc<dyn Repository>,
    k: PhantomData<K>,
    v: PhantomData<V>,
}

impl<K, V> CodecStorage<K, V>
where
    K: KeyCodec,
    V: ValueCodec,
{
    pub fn new(store: Arc<dyn Repository>) -> Self {
        Self {
            store,
            k: PhantomData,
            v: PhantomData,
        }
    }

    pub fn get(&self, key: K) -> Result<Option<V>> {
        match self.store.get(key.encode_key()?.as_slice())? {
            Some(v) => Ok(Some(V::decode_value(v.as_slice())?)),
            None => Ok(None),
        }
    }
    pub fn put(&self, key: K, value: V) -> Result<()> {
        self.store.put(key.encode_key()?, value.encode_value()?)
    }
    pub fn contains_key(&self, key: K) -> Result<bool> {
        self.store.contains_key(key.encode_key()?)
    }
    pub fn remove(&self, key: K) -> Result<()> {
        self.store.remove(key.encode_key()?)
    }

    pub fn get_len(&self) -> Result<u64> {
        self.store.get_len()
    }
    pub fn keys(&self) -> Result<Vec<Vec<u8>>> {
        self.store.keys()
    }
}

impl KeyCodec for HashValue {
    fn encode_key(&self) -> Result<Vec<u8>> {
        Ok(self.to_vec())
    }

    fn decode_key(data: &[u8]) -> Result<Self, Error> {
        Ok(HashValue::from_slice(data)?)
    }
}

impl ValueCodec for HashValue {
    fn encode_value(&self) -> Result<Vec<u8>> {
        Ok(self.to_vec())
    }

    fn decode_value(data: &[u8]) -> Result<Self, Error> {
        Ok(HashValue::from_slice(data)?)
    }
}
