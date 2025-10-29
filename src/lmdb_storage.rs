use v_individual_model::onto::individual::Individual;
use v_individual_model::onto::parser::parse_raw;
use crate::common::{Storage, StorageId, StorageMode, StorageResult, ZeroCopyStorage};
use heed::{Env, EnvOpenOptions};
use heed::types::*;
use std::borrow::Cow;
use std::iter::Iterator;
use std::path::Path;
use std::fs;
use std::sync::{Arc, OnceLock};
use std::collections::HashMap;
use std::sync::Mutex;

// Trait for types that can be deserialized from MDB value
// Similar to heed's BytesDecode but simpler for our use case
pub trait FromMdbValue: Sized {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self>;
}

// Implement FromMdbValue for common types
impl FromMdbValue for Vec<u8> {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self> {
        Some(bytes.to_vec())
    }
}

impl FromMdbValue for String {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self> {
        String::from_utf8(bytes.to_vec()).ok()
    }
}

impl FromMdbValue for i64 {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self> {
        let arr: &[u8; 8] = bytes.try_into().ok()?;
        Some(i64::from_le_bytes(*arr))
    }
}

impl FromMdbValue for u64 {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self> {
        let arr: &[u8; 8] = bytes.try_into().ok()?;
        Some(u64::from_le_bytes(*arr))
    }
}

impl FromMdbValue for i32 {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self> {
        let arr: &[u8; 4] = bytes.try_into().ok()?;
        Some(i32::from_le_bytes(*arr))
    }
}

impl FromMdbValue for u32 {
    fn from_mdb_value(bytes: &[u8]) -> Option<Self> {
        let arr: &[u8; 4] = bytes.try_into().ok()?;
        Some(u32::from_le_bytes(*arr))
    }
}

// Global registry of shared environments by path.
// This is critical for LMDB: multiple instances in the same process must share
// the same environment for a given database path to avoid conflicts.
// Each LmdbInstance holds an Arc<Env> clone, ensuring thread-safe shared access.
static GLOBAL_ENVS: OnceLock<Mutex<HashMap<String, Arc<Env>>>> = OnceLock::new();

pub struct LMDBStorage {
    individuals_db: LmdbInstance,
    tickets_db: LmdbInstance,
    az_db: LmdbInstance,
}

pub struct LmdbInstance {
    max_read_counter: u64,
    path: String,
    env: Arc<Env>,
    read_counter: u64,
}

// Get or create a shared LMDB environment for the given path.
// This function ensures that all LmdbInstance objects for the same path
// share a single Environment, which is a requirement for correct LMDB operation
// when multiple readers exist in the same process.
fn get_or_create_env(path: &str) -> Arc<Env> {
    let envs = GLOBAL_ENVS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut envs_map = envs.lock().unwrap();
    
    // Return existing environment if already created
    if let Some(env) = envs_map.get(path) {
        return env.clone();
    }
    
    // Create directory if it doesn't exist
    if let Err(e) = fs::create_dir_all(path) {
        error!("LMDB: failed to create directory path=[{}], err={:?}", path, e);
    }
    
    // Open new environment with retry logic
    let env = loop {
        match unsafe {
            EnvOpenOptions::new()
                .map_size(10 * 1024 * 1024 * 1024) // 10GB initial size
                .max_dbs(1)
                .open(Path::new(path))
        } {
            Ok(env) => break Arc::new(env),
            Err(e) => {
                error!("LMDB: failed to open environment, path=[{}], err={:?}", path, e);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    };
    
    // Store environment in global registry
    envs_map.insert(path.to_string(), env.clone());
    env
}

struct LmdbIterator {
    keys: Vec<Vec<u8>>,
    index: usize,
}

impl Iterator for LmdbIterator {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.keys.len() {
            None
        } else {
            let key = self.keys[self.index].clone();
            self.index += 1;
            Some(key)
        }
    }
}

impl LmdbInstance {
    /// Create a new LmdbInstance.
    /// The environment is shared globally - multiple instances for the same path
    /// will use the same underlying LMDB environment.
    /// Database handle is NOT stored - it's opened per-transaction for thread safety.
    pub fn new(path: &str, _mode: StorageMode) -> Self {
        let env = get_or_create_env(path);
        
        // Try to initialize database (create_database is idempotent - succeeds if already exists)
        if let Ok(mut wtxn) = env.write_txn() {
            if let Ok(_db) = env.create_database::<Bytes, Bytes>(&mut wtxn, None) {
                let _ = wtxn.commit();
            }
        }
        
        LmdbInstance {
            max_read_counter: 1000,
            path: path.to_string(),
            env,
            read_counter: 0,
        }
    }

    pub fn iter(&mut self) -> Box<dyn Iterator<Item = Vec<u8>>> {
        match self.env.read_txn() {
            Ok(txn) => {
                match self.env.open_database::<Bytes, Bytes>(&txn, None) {
                    Ok(Some(db)) => {
                        let mut keys = Vec::new();
                        if let Ok(iter) = db.iter(&txn) {
                            for item in iter {
                                if let Ok((key, _)) = item {
                                    keys.push(key.to_vec());
                                }
                            }
                        }
                        Box::new(LmdbIterator {
                            keys,
                            index: 0,
                        })
                    },
                    Ok(None) => {
                        error!("LMDB: database not found, path=[{}]", self.path);
                        Box::new(std::iter::empty())
                    },
                    Err(e) => {
                        error!("LMDB: failed to open database for iterator, path=[{}], err={:?}", self.path, e);
                        Box::new(std::iter::empty())
                    }
                }
            },
            Err(e) => {
                error!("LMDB: failed to create read transaction for iterator, path=[{}], err={:?}", self.path, e);
                Box::new(std::iter::empty())
            },
        }
    }

    pub fn open(&mut self) {
        // Reset read counter - environment is already open and shared
        self.read_counter = 0;
        info!("LMDBStorage: reset read counter for path=[{}]", self.path);
    }

    /// Create a read-only transaction for zero-copy operations
    /// Use this with get_with_txn to avoid data copying
    pub fn begin_ro_txn(&self) -> heed::Result<heed::RoTxn<'_, heed::WithTls>> {
        self.env.read_txn()
    }

    /// Get data with zero-copy using existing transaction
    /// Returns Cow::Borrowed (reference without copying, valid while transaction lives)
    pub fn get_with_txn<'tx>(&self, txn: &'tx heed::RoTxn<heed::WithTls>, key: &str) -> Option<Cow<'tx, [u8]>> {
        match self.env.open_database::<Bytes, Bytes>(txn, None) {
            Ok(Some(db)) => {
                match db.get(txn, key.as_bytes()) {
                    Ok(Some(val)) => Some(Cow::Borrowed(val)),  // Zero-copy! Returns Cow::Borrowed
                    Ok(None) => None,
                    Err(e) => {
                        error!("LMDB: get_with_txn failed for key=[{}], path=[{}], err={:?}", key, self.path, e);
                        None
                    },
                }
            },
            Ok(None) => {
                error!("LMDB: database not found in get_with_txn for key=[{}], path=[{}]", key, self.path);
                None
            },
            Err(e) => {
                error!("LMDB: failed to open database in get_with_txn for key=[{}], path=[{}], err={:?}", key, self.path, e);
                None
            }
        }
    }

    pub fn get_individual(&mut self, uri: &str, iraw: &mut Individual) -> StorageResult<()> {
        if let Some(val) = self.get_raw(uri) {
            iraw.set_raw(&val);

            return if parse_raw(iraw).is_ok() {
                StorageResult::Ok(())
            } else {
                error!("LMDB: fail parse binobj, path=[{}], len={}, uri=[{}]", self.path, iraw.get_raw_len(), uri);
                StorageResult::UnprocessableEntity
            };
        }

        StorageResult::NotFound
    }

    pub fn get_v(&mut self, key: &str) -> Option<String> {
        self.get::<String>(key)
    }

    pub fn get_raw(&mut self, key: &str) -> Option<Vec<u8>> {
        self.get::<Vec<u8>>(key)
    }

    pub fn get<T: FromMdbValue>(&mut self, key: &str) -> Option<T> {
        for _it in 0..2 {
            self.read_counter += 1;
            if self.read_counter > self.max_read_counter {
                warn!("db {} reset counter for key=[{}] (max counter reached)", self.path, key);
                self.read_counter = 0;
            }

            match self.env.read_txn() {
                Ok(txn) => {
                    match self.env.open_database::<Bytes, Bytes>(&txn, None) {
                        Ok(Some(db)) => {
                            match db.get(&txn, key.as_bytes()) {
                                Ok(Some(val)) => {
                                    return T::from_mdb_value(val);
                                },
                                Ok(None) => {
                                    return None;
                                },
                                Err(e) => {
                                    error!("LMDB: db.get failed for key=[{}], path=[{}], err={:?}", key, self.path, e);
                                    return None;
                                },
                            }
                        },
                        Ok(None) => {
                            error!("LMDB: database not found for key=[{}], path=[{}]", key, self.path);
                            return None;
                        },
                        Err(e) => {
                            error!("LMDB: failed to open database for key=[{}], path=[{}], err={:?}", key, self.path, e);
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                },
                Err(e) => {
                    error!("LMDB: failed to create read transaction for key=[{}], path=[{}], err={:?}", key, self.path, e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                },
            }
        }

        None
    }

    pub fn count(&mut self) -> usize {
        for _it in 0..2 {
            match self.env.read_txn() {
                Ok(txn) => {
                    match self.env.open_database::<Bytes, Bytes>(&txn, None) {
                        Ok(Some(db)) => {
                            match db.len(&txn) {
                                Ok(count) => {
                                    return count as usize;
                                },
                                Err(e) => {
                                    error!("LMDB: failed to get count, path=[{}], err={:?}", self.path, e);
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                },
                            }
                        },
                        Ok(None) => {
                            error!("LMDB: database not found for count, path=[{}]", self.path);
                            return 0;
                        },
                        Err(e) => {
                            error!("LMDB: failed to open database for count, path=[{}], err={:?}", self.path, e);
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                },
                Err(e) => {
                    error!("LMDB: failed to create transaction for count, path=[{}], err={:?}", self.path, e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                },
            }
        }

        0
    }

    pub fn remove(&mut self, key: &str) -> bool {
        remove_from_lmdb(&self.env, key, &self.path)
    }

    pub fn put(&mut self, key: &str, val: &[u8]) -> bool {
        put_kv_lmdb(&self.env, key, val, &self.path)
    }
}

// Implement ZeroCopyStorage trait for LmdbInstance
impl ZeroCopyStorage for LmdbInstance {
    type Transaction<'tx> = heed::RoTxn<'tx, heed::WithTls>;
    
    fn begin_ro_txn(&self) -> Result<Self::Transaction<'_>, Box<dyn std::error::Error>> {
        self.env.read_txn().map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }
    
    fn get_with_txn<'tx>(&self, txn: &'tx Self::Transaction<'tx>, key: &str) -> Option<Cow<'tx, [u8]>> {
        match self.env.open_database::<Bytes, Bytes>(txn, None) {
            Ok(Some(db)) => {
                match db.get(txn, key.as_bytes()) {
                    Ok(Some(val)) => Some(Cow::Borrowed(val)),
                    _ => None,
                }
            },
            _ => None,
        }
    }
    
    fn put(&mut self, key: &str, val: &[u8]) -> bool {
        put_kv_lmdb(&self.env, key, val, &self.path)
    }
}

impl LMDBStorage {
    pub fn new(db_path: &str, mode: StorageMode, _max_read_counter_reopen: Option<u64>) -> LMDBStorage {
        LMDBStorage {
            individuals_db: LmdbInstance::new(
                &(db_path.to_owned() + "/lmdb-individuals/"),
                mode.clone()
            ),
            tickets_db: LmdbInstance::new(
                &(db_path.to_owned() + "/lmdb-tickets/"),
                mode.clone()
            ),
            az_db: LmdbInstance::new(
                &(db_path.to_owned() + "/acl-indexes/"),
                mode.clone()
            ),
        }
    }

    fn get_db_instance(&mut self, storage: &StorageId) -> &mut LmdbInstance {
        match storage {
            StorageId::Individuals => &mut self.individuals_db,
            StorageId::Tickets => &mut self.tickets_db,
            StorageId::Az => &mut self.az_db,
        }
    }

    pub fn open(&mut self, storage: StorageId) {
        let db_instance = self.get_db_instance(&storage);
        db_instance.open();

        info!("LMDBStorage: db {} open {:?}", db_instance.path, storage);
    }
}

impl Storage for LMDBStorage {
    fn get_individual(&mut self, storage: StorageId, uri: &str, iraw: &mut Individual) -> StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        db_instance.get_individual(uri, iraw)
    }

    fn get_value(&mut self, storage: StorageId, key: &str) -> crate::common::StorageResult<String> {
        let db_instance = self.get_db_instance(&storage);
        match db_instance.get_v(key) {
            Some(value) => crate::common::StorageResult::Ok(value),
            None => crate::common::StorageResult::NotFound,
        }
    }

    fn get_raw_value(&mut self, storage: StorageId, key: &str) -> crate::common::StorageResult<Vec<u8>> {
        let db_instance = self.get_db_instance(&storage);
        match db_instance.get_raw(key) {
            Some(value) => crate::common::StorageResult::Ok(value),
            None => crate::common::StorageResult::NotFound,
        }
    }

    fn put_value(&mut self, storage: StorageId, key: &str, val: &str) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if put_kv_lmdb(&db_instance.env, key, val.as_bytes(), &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::Error("Failed to put value".to_string())
        }
    }

    fn put_raw_value(&mut self, storage: StorageId, key: &str, val: Vec<u8>) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if put_kv_lmdb(&db_instance.env, key, val.as_slice(), &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::Error("Failed to put raw value".to_string())
        }
    }

    fn remove_value(&mut self, storage: StorageId, key: &str) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if remove_from_lmdb(&db_instance.env, key, &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::NotFound
        }
    }

    fn count(&mut self, storage: StorageId) -> crate::common::StorageResult<usize> {
        let db_instance = self.get_db_instance(&storage);
        crate::common::StorageResult::Ok(db_instance.count())
    }
}

fn remove_from_lmdb(env: &Arc<Env>, key: &str, path: &str) -> bool {
    match env.write_txn() {
        Ok(mut txn) => {
            match env.open_database::<Bytes, Bytes>(&txn, None) {
                Ok(Some(db)) => {
                    match db.delete(&mut txn, key.as_bytes()) {
                        Ok(true) => {
                            match txn.commit() {
                                Ok(_) => true,
                                Err(e) => {
                                    error!("LMDB: failed to commit removal for key=[{}], path=[{}], err={:?}", key, path, e);
                                    false
                                }
                            }
                        },
                        Ok(false) => {
                            // Key not found
                            false
                        },
                        Err(e) => {
                            error!("LMDB: failed to remove key=[{}] from path=[{}], err={:?}", key, path, e);
                            false
                        }
                    }
                },
                Ok(None) => {
                    error!("LMDB: database not found while removing key=[{}], path=[{}]", key, path);
                    false
                },
                Err(e) => {
                    error!("LMDB: failed to open database while removing key=[{}], path=[{}], err={:?}", key, path, e);
                    false
                }
            }
        },
        Err(e) => {
            error!("LMDB: failed to create write transaction while removing key=[{}], path=[{}], err={:?}", key, path, e);
            false
        }
    }
}

fn put_kv_lmdb(env: &Arc<Env>, key: &str, val: &[u8], path: &str) -> bool {
    match env.write_txn() {
        Ok(mut txn) => {
            match env.open_database::<Bytes, Bytes>(&txn, None) {
                Ok(Some(db)) => {
                    match db.put(&mut txn, key.as_bytes(), val) {
                        Ok(_) => {
                            match txn.commit() {
                                Ok(_) => true,
                                Err(e) => {
                                    error!("LMDB: failed to commit put for key=[{}], path=[{}], err={:?}", key, path, e);
                                    false
                                }
                            }
                        },
                        Err(e) => {
                            error!("LMDB: failed to put key=[{}] into path=[{}], err={:?}", key, path, e);
                            false
                        }
                    }
                },
                Ok(None) => {
                    error!("LMDB: database not found while putting key=[{}], path=[{}]", key, path);
                    false
                },
                Err(e) => {
                    error!("LMDB: failed to open database while putting key=[{}], path=[{}], err={:?}", key, path, e);
                    false
                }
            }
        },
        Err(e) => {
            error!("LMDB: failed to create write transaction while putting key=[{}], path=[{}], err={:?}", key, path, e);
            false
        }
    }
}
