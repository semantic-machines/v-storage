use v_individual_model::onto::individual::Individual;
use v_individual_model::onto::parser::parse_raw;
use crate::common::{Storage, StorageId, StorageMode, StorageResult};
use lmdb_rs_m::core::EnvCreateFlags;
use lmdb_rs_m::{DbFlags, DbHandle, EnvBuilder, Environment, MdbError};
use lmdb_rs_m::{FromMdbValue, ToMdbValue};
use std::iter::Iterator;

pub struct LMDBStorage {
    individuals_db: LmdbInstance,
    tickets_db: LmdbInstance,
    az_db: LmdbInstance,
}

pub struct LmdbInstance {
    max_read_counter: u64,
    path: String,
    mode: StorageMode,
    db_handle: Result<DbHandle, MdbError>,
    db_env: Result<Environment, MdbError>,
    read_counter: u64,
}

impl Default for LmdbInstance {
    fn default() -> Self {
        LmdbInstance {
            max_read_counter: 1000,
            path: String::default(),
            mode: StorageMode::ReadOnly,
            db_handle: Err(MdbError::Panic),
            db_env: Err(MdbError::Panic),
            read_counter: 0,
        }
    }
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
    pub fn new(path: &str, mode: StorageMode) -> Self {
        LmdbInstance {
            max_read_counter: 1000,
            path: path.to_string(),
            mode,
            db_handle: Err(MdbError::Panic),
            db_env: Err(MdbError::Panic),
            read_counter: 0,
        }
    }

    pub fn iter(&mut self) -> Box<dyn Iterator<Item = Vec<u8>>> {
        if self.db_env.is_err() {
            self.open();
        }

        match &self.db_env {
            Ok(env) => match &self.db_handle {
                Ok(handle) => match env.get_reader() {
                    Ok(txn) => {
                        let db = txn.bind(handle);
                        let cursor_result = db.new_cursor();
                        match cursor_result {
                            Ok(mut cursor) => {
                                let mut keys = Vec::new();
                                while let Ok(()) = cursor.to_next_item() {
                                    if let Ok(key) = cursor.get_key::<Vec<u8>>() {
                                        keys.push(key);
                                    }
                                }
                                Box::new(LmdbIterator {
                                    keys,
                                    index: 0,
                                })
                            },
                            Err(_) => Box::new(std::iter::empty()),
                        }
                    },
                    Err(_) => Box::new(std::iter::empty()),
                },
                Err(_) => Box::new(std::iter::empty()),
            },
            Err(_) => Box::new(std::iter::empty()),
        }
    }

    pub fn open(&mut self) {
        let flags = if self.mode == StorageMode::ReadOnly {
            // MDB_NOLOCK (0x20000000) | MDB_RDONLY (0x20000) | MDB_NOMETASYNC (0x40000) | MDB_NOSYNC (0x10000)
            EnvCreateFlags::from_bits_truncate(0x20000000 | 0x20000 | 0x40000 | 0x10000)
        } else {
            // MDB_NOLOCK (0x20000000) | MDB_NOMETASYNC (0x40000) | MDB_NOSYNC (0x10000)
            EnvCreateFlags::from_bits_truncate(0x20000000 | 0x40000 | 0x10000)
        };
        
        let env_builder = EnvBuilder::new().flags(flags);

        let db_env = env_builder.open(&self.path, 0o644);

        let db_handle = match &db_env {
            Ok(env) => env.get_default_db(DbFlags::empty()),
            Err(e) => {
                error!("LMDB: fail opening read only environment, path=[{}], err={:?}", self.path, e);
                Err(MdbError::Corrupted)
            },
        };

        self.db_handle = db_handle;
        self.db_env = db_env;
        self.read_counter = 0;
    }

    fn get_individual(&mut self, uri: &str, iraw: &mut Individual) -> StorageResult<()> {
        if let Some(val) = self.get::<&[u8]>(uri) {
            iraw.set_raw(val);

            return if parse_raw(iraw).is_ok() {
                StorageResult::Ok(())
            } else {
                error!("LMDB: fail parse binobj, path=[{}], len={}, uri=[{}]", self.path, iraw.get_raw_len(), uri);
                StorageResult::UnprocessableEntity
            };
        }

        StorageResult::NotFound
    }

    fn get_v(&mut self, key: &str) -> Option<String> {
        self.get::<String>(key)
    }

    fn get_raw(&mut self, key: &str) -> Option<Vec<u8>> {
        self.get::<Vec<u8>>(key)
    }

    pub fn get<T: FromMdbValue>(&mut self, key: &str) -> Option<T> {
        if self.db_env.is_err() {
            self.open();
        }

        for _it in 0..2 {
            let mut is_need_reopen = false;

            self.read_counter += 1;
            if self.read_counter > self.max_read_counter {
                is_need_reopen = true;
            }

            match &self.db_env {
                Ok(env) => match &self.db_handle {
                    Ok(handle) => match env.get_reader() {
                        Ok(txn) => {
                            let db = txn.bind(handle);

                            match db.get::<T>(&key) {
                                Ok(val) => {
                                    return Some(val);
                                },
                                Err(e) => match e {
                                    MdbError::NotFound => {
                                        return None;
                                    },
                                    _ => {
                                        error!("LMDB: db.get failed for key=[{}], path=[{}], err={:?}", key, self.path, e);
                                        return None;
                                    },
                                },
                            }
                        },
                        Err(e) => match e {
                            MdbError::Other(c, _) => {
                                if c == -30785 {
                                    is_need_reopen = true;
                                } else {
                                    error!("LMDB: failed to create transaction for key=[{}], path=[{}], err={}", key, self.path, e);
                                    return None;
                                }
                            },
                            _ => {
                                error!("LMDB: failed to create transaction for key=[{}], path=[{}], err={}", key, self.path, e);
                            },
                        },
                    },
                    Err(e) => {
                        error!("LMDB: db handle error for key=[{}], path=[{}], err={}", key, self.path, e);
                        return None;
                    },
                },
                Err(e) => match e {
                    MdbError::Panic => {
                        is_need_reopen = true;
                    },
                    _ => {
                        error!("LMDB: db environment error for key=[{}], path=[{}], err={}", key, self.path, e);
                        return None;
                    },
                },
            }

            if is_need_reopen {
                warn!("db {} reopen for key=[{}]", self.path, key);
                self.open();
            }
        }

        None
    }

    pub fn count(&mut self) -> usize {
        if self.db_env.is_err() {
            self.open();
        }

        for _it in 0..2 {
            let mut is_need_reopen = false;

            match &self.db_env {
                Ok(env) => match env.stat() {
                    Ok(stat) => {
                        return stat.ms_entries;
                    },
                    Err(e) => match e {
                        MdbError::Other(c, _) => {
                            if c == -30785 {
                                is_need_reopen = true;
                            } else {
                                error!("LMDB: fail read stat for path=[{}], err={}", self.path, e);
                                return 0;
                            }
                        },
                        _ => {
                            error!("LMDB: fail to create transaction for stat read, path=[{}], err={}", self.path, e);
                        },
                    },
                },
                Err(e) => match e {
                    MdbError::Panic => {
                        is_need_reopen = true;
                    },
                    _ => {
                        error!("LMDB: db environment error while reading stat, path=[{}], err={}", self.path, e);
                        return 0;
                    },
                },
            }

            if is_need_reopen {
                warn!("db {} reopen for stat read", self.path);
                self.open();
            }
        }

        0
    }

    pub fn remove(&mut self, key: &str) -> bool {
        if self.db_env.is_err() {
            self.open();
        }
        remove_from_lmdb(&self.db_env, &self.db_handle, key, &self.path)
    }

    pub fn put<T: ToMdbValue>(&mut self, key: &str, val: T) -> bool {
        if self.db_env.is_err() {
            self.open();
        }
        put_kv_lmdb(&self.db_env, &self.db_handle, key, val, &self.path)
    }
}

impl LMDBStorage {
    pub fn new(db_path: &str, mode: StorageMode, max_read_counter_reopen: Option<u64>) -> LMDBStorage {
        LMDBStorage {
            individuals_db: LmdbInstance {
                max_read_counter: max_read_counter_reopen.unwrap_or(u32::MAX as u64),
                path: db_path.to_owned() + "/lmdb-individuals/",
                mode: mode.clone(),
                ..Default::default()
            },
            tickets_db: LmdbInstance {
                max_read_counter: max_read_counter_reopen.unwrap_or(u32::MAX as u64),
                path: db_path.to_owned() + "/lmdb-tickets/",
                mode: mode.clone(),
                ..Default::default()
            },
            az_db: LmdbInstance {
                max_read_counter: max_read_counter_reopen.unwrap_or(u32::MAX as u64),
                path: db_path.to_owned() + "/acl-indexes/",
                mode: mode.clone(),
                ..Default::default()
            },
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
        if put_kv_lmdb(&db_instance.db_env, &db_instance.db_handle, key, val.as_bytes(), &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::Error("Failed to put value".to_string())
        }
    }

    fn put_raw_value(&mut self, storage: StorageId, key: &str, val: Vec<u8>) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if put_kv_lmdb(&db_instance.db_env, &db_instance.db_handle, key, val.as_slice(), &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::Error("Failed to put raw value".to_string())
        }
    }

    fn remove_value(&mut self, storage: StorageId, key: &str) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if remove_from_lmdb(&db_instance.db_env, &db_instance.db_handle, key, &db_instance.path) {
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

fn remove_from_lmdb(db_env: &Result<Environment, MdbError>, db_handle: &Result<DbHandle, MdbError>, key: &str, path: &str) -> bool {
    match db_env {
        Ok(env) => match env.new_transaction() {
            Ok(txn) => match db_handle {
                Ok(handle) => {
                    let db = txn.bind(handle);
                    if let Err(e) = db.del(&key) {
                        error!("LMDB: failed to remove key=[{}] from path=[{}], err={}", key, path, e);
                        return false;
                    }

                    if let Err(e) = txn.commit() {
                        if let MdbError::Other(c, _) = e {
                            if c == -30792 && grow_db(db_env, path) {
                                return remove_from_lmdb(db_env, db_handle, key, path);
                            }
                        }
                        error!("LMDB: failed to commit removal for key=[{}], path=[{}], err={}", key, path, e);
                        return false;
                    }
                    true
                },
                Err(e) => {
                    error!("LMDB: db handle error while removing key=[{}], path=[{}], err={}", key, path, e);
                    false
                },
            },
            Err(e) => {
                error!("LMDB: failed to create transaction while removing key=[{}], path=[{}], err={}", key, path, e);
                false
            },
        },
        Err(e) => {
            error!("LMDB: db environment error while removing key=[{}], path=[{}], err={}", key, path, e);
            false
        },
    }
}

fn put_kv_lmdb<T: ToMdbValue>(db_env: &Result<Environment, MdbError>, db_handle: &Result<DbHandle, MdbError>, key: &str, val: T, path: &str) -> bool {
    match db_env {
        Ok(env) => match env.new_transaction() {
            Ok(txn) => match db_handle {
                Ok(handle) => {
                    let db = txn.bind(handle);
                    if let Err(e) = db.set(&key, &val) {
                        error!("LMDB: failed to put key=[{}] into path=[{}], err={}", key, path, e);
                        return false;
                    }

                    if let Err(e) = txn.commit() {
                        if let MdbError::Other(c, _) = e {
                            if c == -30792 && grow_db(db_env, path) {
                                return put_kv_lmdb(db_env, db_handle, key, val, path);
                            }
                        }
                        error!("LMDB: failed to commit put for key=[{}], path=[{}], err={}", key, path, e);
                        return false;
                    }
                    true
                },
                Err(e) => {
                    error!("LMDB: db handle error while putting key=[{}], path=[{}], err={}", key, path, e);
                    false
                },
            },
            Err(e) => {
                error!("LMDB: failed to create transaction while putting key=[{}], path=[{}], err={}", key, path, e);
                false
            },
        },
        Err(e) => {
            error!("LMDB: db environment error while putting key=[{}], path=[{}], err={}", key, path, e);
            false
        },
    }
}

fn grow_db(db_env: &Result<Environment, MdbError>, path: &str) -> bool {
    match db_env {
        Ok(env) => {
            if let Ok(stat) = env.info() {
                let new_size = stat.me_mapsize + 100 * 10_048_576;
                if env.set_mapsize(new_size).is_ok() {
                    info!("success grow db, new size = {}", new_size);
                    return true;
                }
            }
        },
        Err(e) => {
            error!("LMDB: db environment error while growing db, path=[{}], err={}", path, e);
        },
    }
    false
}
