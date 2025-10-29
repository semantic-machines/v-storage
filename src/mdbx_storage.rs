use v_individual_model::onto::individual::Individual;
use v_individual_model::onto::parser::parse_raw;
use crate::common::{Storage, StorageId, StorageMode, StorageResult};
use libmdbx::{Database, DatabaseOptions, Mode, ReadWriteOptions, SyncMode, WriteFlags, WriteMap};
use std::iter::Iterator;
use std::path::Path;
use std::fs;
use std::sync::{Arc, OnceLock};
use std::collections::HashMap;
use std::sync::Mutex;

// Global registry of shared databases by path.
// This is critical for MDBX: multiple instances in the same process must share
// the same database for a given database path to avoid conflicts.
// Each MdbxInstance holds an Arc<Database> clone, ensuring thread-safe shared access.
static GLOBAL_DBS: OnceLock<Mutex<HashMap<String, Arc<Database<WriteMap>>>>> = OnceLock::new();

pub struct MDBXStorage {
    individuals_db: MdbxInstance,
    tickets_db: MdbxInstance,
    az_db: MdbxInstance,
}

pub struct MdbxInstance {
    max_read_counter: u64,
    path: String,
    db: Arc<Database<WriteMap>>,
    read_counter: u64,
}

// Get or create a shared MDBX database for the given path.
// This function ensures that all MdbxInstance objects for the same path
// share a single Database, which is a requirement for correct MDBX operation
// when multiple readers exist in the same process.
fn get_or_create_db(path: &str) -> Arc<Database<WriteMap>> {
    let dbs = GLOBAL_DBS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut dbs_map = dbs.lock().unwrap();
    
    // Return existing database if already created
    if let Some(db) = dbs_map.get(path) {
        return db.clone();
    }
    
    // Create directory if it doesn't exist
    if let Err(e) = fs::create_dir_all(path) {
        error!("MDBX: failed to create directory path=[{}], err={:?}", path, e);
    }
    
    // Open new database with retry logic
    let db = loop {
        let options = DatabaseOptions {
            mode: Mode::ReadWrite(ReadWriteOptions {
                sync_mode: SyncMode::SafeNoSync,
                min_size: Some(0),
                max_size: Some(10 * 1024 * 1024 * 1024), // 10GB
                growth_step: Some(1024 * 1024 * 1024),   // 1GB growth step
                shrink_threshold: None,
            }),
            max_tables: Some(1),
            ..Default::default()
        };
        
        match Database::<WriteMap>::open_with_options(Path::new(path), options) {
            Ok(db) => break Arc::new(db),
            Err(e) => {
                error!("MDBX: failed to open database, path=[{}], err={:?}", path, e);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    };
    
    // Store database in global registry
    dbs_map.insert(path.to_string(), db.clone());
    db
}

struct MdbxIterator {
    keys: Vec<Vec<u8>>,
    index: usize,
}

impl Iterator for MdbxIterator {
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

impl MdbxInstance {
    /// Create a new MdbxInstance.
    /// The database is shared globally - multiple instances for the same path
    /// will use the same underlying MDBX database.
    pub fn new(path: &str, _mode: StorageMode) -> Self {
        let db = get_or_create_db(path);
        
        MdbxInstance {
            max_read_counter: 1000,
            path: path.to_string(),
            db,
            read_counter: 0,
        }
    }

    pub fn iter(&mut self) -> Box<dyn Iterator<Item = Vec<u8>>> {
        match self.db.begin_ro_txn() {
            Ok(txn) => {
                match txn.open_table(None) {
                    Ok(table) => {
                        let mut keys = Vec::new();
                        if let Ok(mut cursor) = txn.cursor(&table) {
                            while let Ok(Some((key, _))) = cursor.next::<Vec<u8>, Vec<u8>>() {
                                keys.push(key);
                            }
                        }
                        Box::new(MdbxIterator {
                            keys,
                            index: 0,
                        })
                    },
                    Err(e) => {
                        error!("MDBX: failed to open table for iterator, path=[{}], err={:?}", self.path, e);
                        Box::new(std::iter::empty())
                    }
                }
            },
            Err(e) => {
                error!("MDBX: failed to create read transaction for iterator, path=[{}], err={:?}", self.path, e);
                Box::new(std::iter::empty())
            },
        }
    }

    pub fn open(&mut self) {
        // Reset read counter - database is already open and shared
        self.read_counter = 0;
        info!("MDBXStorage: reset read counter for path=[{}]", self.path);
    }

    fn get_individual(&mut self, uri: &str, iraw: &mut Individual) -> StorageResult<()> {
        if let Some(val) = self.get_raw(uri) {
            iraw.set_raw(&val);

            return if parse_raw(iraw).is_ok() {
                StorageResult::Ok(())
            } else {
                error!("MDBX: fail parse binobj, path=[{}], len={}, uri=[{}]", self.path, iraw.get_raw_len(), uri);
                StorageResult::UnprocessableEntity
            };
        }

        StorageResult::NotFound
    }

    fn get_v(&mut self, key: &str) -> Option<String> {
        self.get_raw(key).and_then(|bytes| {
            String::from_utf8(bytes).ok()
        })
    }

    fn get_raw(&mut self, key: &str) -> Option<Vec<u8>> {
        for _it in 0..2 {
            self.read_counter += 1;
            if self.read_counter > self.max_read_counter {
                warn!("db {} reset counter for key=[{}] (max counter reached)", self.path, key);
                self.read_counter = 0;
            }

            match self.db.begin_ro_txn() {
                Ok(txn) => {
                    match txn.open_table(None) {
                        Ok(table) => {
                            match txn.get::<Vec<u8>>(&table, key.as_bytes()) {
                                Ok(Some(val)) => {
                                    return Some(val);
                                },
                                Ok(None) => {
                                    return None;
                                },
                                Err(e) => {
                                    error!("MDBX: get failed for key=[{}], path=[{}], err={:?}", key, self.path, e);
                                    return None;
                                },
                            }
                        },
                        Err(e) => {
                            error!("MDBX: failed to open table for key=[{}], path=[{}], err={:?}", key, self.path, e);
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                },
                Err(e) => {
                    error!("MDBX: failed to create read transaction for key=[{}], path=[{}], err={:?}", key, self.path, e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                },
            }
        }

        None
    }

    pub fn count(&mut self) -> usize {
        for _it in 0..2 {
            match self.db.begin_ro_txn() {
                Ok(txn) => {
                    match txn.open_table(None) {
                        Ok(table) => {
                            match txn.table_stat(&table) {
                                Ok(stat) => {
                                    return stat.entries();
                                },
                                Err(e) => {
                                    error!("MDBX: failed to get count, path=[{}], err={:?}", self.path, e);
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                },
                            }
                        },
                        Err(e) => {
                            error!("MDBX: failed to open table for count, path=[{}], err={:?}", self.path, e);
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                },
                Err(e) => {
                    error!("MDBX: failed to create transaction for count, path=[{}], err={:?}", self.path, e);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                },
            }
        }

        0
    }

    pub fn remove(&mut self, key: &str) -> bool {
        remove_from_mdbx(&self.db, key, &self.path)
    }

    pub fn put(&mut self, key: &str, val: &[u8]) -> bool {
        put_kv_mdbx(&self.db, key, val, &self.path)
    }
}

impl MDBXStorage {
    pub fn new(db_path: &str, mode: StorageMode, _max_read_counter_reopen: Option<u64>) -> MDBXStorage {
        MDBXStorage {
            individuals_db: MdbxInstance::new(
                &(db_path.to_owned() + "/mdbx-individuals/"),
                mode.clone()
            ),
            tickets_db: MdbxInstance::new(
                &(db_path.to_owned() + "/mdbx-tickets/"),
                mode.clone()
            ),
            az_db: MdbxInstance::new(
                &(db_path.to_owned() + "/acl-indexes/"),
                mode.clone()
            ),
        }
    }

    fn get_db_instance(&mut self, storage: &StorageId) -> &mut MdbxInstance {
        match storage {
            StorageId::Individuals => &mut self.individuals_db,
            StorageId::Tickets => &mut self.tickets_db,
            StorageId::Az => &mut self.az_db,
        }
    }

    pub fn open(&mut self, storage: StorageId) {
        let db_instance = self.get_db_instance(&storage);
        db_instance.open();

        info!("MDBXStorage: db {} open {:?}", db_instance.path, storage);
    }
}

impl Storage for MDBXStorage {
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
        if put_kv_mdbx(&db_instance.db, key, val.as_bytes(), &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::Error("Failed to put value".to_string())
        }
    }

    fn put_raw_value(&mut self, storage: StorageId, key: &str, val: Vec<u8>) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if put_kv_mdbx(&db_instance.db, key, val.as_slice(), &db_instance.path) {
            crate::common::StorageResult::Ok(())
        } else {
            crate::common::StorageResult::Error("Failed to put raw value".to_string())
        }
    }

    fn remove_value(&mut self, storage: StorageId, key: &str) -> crate::common::StorageResult<()> {
        let db_instance = self.get_db_instance(&storage);
        if remove_from_mdbx(&db_instance.db, key, &db_instance.path) {
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

fn remove_from_mdbx(db: &Arc<Database<WriteMap>>, key: &str, path: &str) -> bool {
    match db.begin_rw_txn() {
        Ok(txn) => {
            match txn.open_table(None) {
                Ok(table) => {
                    match txn.del(&table, key.as_bytes(), None) {
                        Ok(true) => {
                            match txn.commit() {
                                Ok(_) => true,
                                Err(e) => {
                                    error!("MDBX: failed to commit removal for key=[{}], path=[{}], err={:?}", key, path, e);
                                    false
                                }
                            }
                        },
                        Ok(false) => {
                            // Key not found
                            false
                        },
                        Err(e) => {
                            error!("MDBX: failed to remove key=[{}] from path=[{}], err={:?}", key, path, e);
                            false
                        }
                    }
                },
                Err(e) => {
                    error!("MDBX: failed to open table while removing key=[{}], path=[{}], err={:?}", key, path, e);
                    false
                }
            }
        },
        Err(e) => {
            error!("MDBX: failed to create write transaction while removing key=[{}], path=[{}], err={:?}", key, path, e);
            false
        }
    }
}

fn put_kv_mdbx(db: &Arc<Database<WriteMap>>, key: &str, val: &[u8], path: &str) -> bool {
    match db.begin_rw_txn() {
        Ok(txn) => {
            match txn.open_table(None) {
                Ok(table) => {
                    match txn.put(&table, key.as_bytes(), val, WriteFlags::empty()) {
                        Ok(_) => {
                            match txn.commit() {
                                Ok(_) => true,
                                Err(e) => {
                                    error!("MDBX: failed to commit put for key=[{}], path=[{}], err={:?}", key, path, e);
                                    false
                                }
                            }
                        },
                        Err(e) => {
                            error!("MDBX: failed to put key=[{}] into path=[{}], err={:?}", key, path, e);
                            false
                        }
                    }
                },
                Err(e) => {
                    error!("MDBX: failed to open table while putting key=[{}], path=[{}], err={:?}", key, path, e);
                    false
                }
            }
        },
        Err(e) => {
            error!("MDBX: failed to create write transaction while putting key=[{}], path=[{}], err={:?}", key, path, e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::StorageResult;

    #[test]
    fn test_mdbx_basic_operations() {
        let temp_dir = format!("/tmp/test-mdbx-basic-{}", std::process::id());
        let mut storage = MDBXStorage::new(&temp_dir, StorageMode::ReadWrite, None);

        // Test put and get
        assert!(storage.put_value(StorageId::Individuals, "test:key1", "value1").is_ok());
        
        let result = storage.get_value(StorageId::Individuals, "test:key1");
        assert!(result.is_ok());
        if let StorageResult::Ok(value) = result {
            assert_eq!(value, "value1");
        }

        // Test count
        let count_result = storage.count(StorageId::Individuals);
        assert!(count_result.is_ok());
        if let StorageResult::Ok(count) = count_result {
            assert_eq!(count, 1);
        }

        // Test remove
        assert!(storage.remove_value(StorageId::Individuals, "test:key1").is_ok());
        
        let removed_result = storage.get_value(StorageId::Individuals, "test:key1");
        assert_eq!(removed_result, StorageResult::NotFound);

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_mdbx_raw_operations() {
        let temp_dir = format!("/tmp/test-mdbx-raw-{}", std::process::id());
        let mut storage = MDBXStorage::new(&temp_dir, StorageMode::ReadWrite, None);

        let test_data = vec![1, 2, 3, 4, 5];
        
        // Test put_raw_value
        assert!(storage.put_raw_value(StorageId::Tickets, "raw:key1", test_data.clone()).is_ok());
        
        // Test get_raw_value
        let result = storage.get_raw_value(StorageId::Tickets, "raw:key1");
        assert!(result.is_ok());
        if let StorageResult::Ok(data) = result {
            assert_eq!(data, test_data);
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_mdbx_different_storage_types() {
        let temp_dir = format!("/tmp/test-mdbx-types-{}", std::process::id());
        let mut storage = MDBXStorage::new(&temp_dir, StorageMode::ReadWrite, None);

        // Test all StorageId types
        assert!(storage.put_value(StorageId::Individuals, "ind:1", "individual_data").is_ok());
        assert!(storage.put_value(StorageId::Tickets, "ticket:1", "ticket_data").is_ok());
        assert!(storage.put_value(StorageId::Az, "az:1", "az_data").is_ok());

        // Verify all are accessible
        assert!(storage.get_value(StorageId::Individuals, "ind:1").is_ok());
        assert!(storage.get_value(StorageId::Tickets, "ticket:1").is_ok());
        assert!(storage.get_value(StorageId::Az, "az:1").is_ok());

        // Check counts
        assert_eq!(storage.count(StorageId::Individuals), StorageResult::Ok(1));
        assert_eq!(storage.count(StorageId::Tickets), StorageResult::Ok(1));
        assert_eq!(storage.count(StorageId::Az), StorageResult::Ok(1));

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_mdbx_nonexistent_key() {
        let temp_dir = format!("/tmp/test-mdbx-notfound-{}", std::process::id());
        let mut storage = MDBXStorage::new(&temp_dir, StorageMode::ReadWrite, None);

        let result = storage.get_value(StorageId::Individuals, "nonexistent");
        assert_eq!(result, StorageResult::NotFound);

        let remove_result = storage.remove_value(StorageId::Individuals, "nonexistent");
        assert_eq!(remove_result, StorageResult::NotFound);

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_mdbx_individual_operations() {
        let temp_dir = format!("/tmp/test-mdbx-individual-{}", std::process::id());
        let mut storage = MDBXStorage::new(&temp_dir, StorageMode::ReadWrite, None);
        let mut individual = v_individual_model::onto::individual::Individual::default();

        // Test with non-existent individual
        let result = storage.get_individual(StorageId::Individuals, "test:nonexistent", &mut individual);
        assert_eq!(result, StorageResult::NotFound);

        // Test with valid JSON data
        let valid_data = r#"{"@":"test:ind1","rdf:type":[{"type":"Uri","data":"test:Person"}]}"#;
        assert!(storage.put_value(StorageId::Individuals, "test:ind1", valid_data).is_ok());

        // Try to get as individual
        let ind_result = storage.get_individual(StorageId::Individuals, "test:ind1", &mut individual);
        assert!(ind_result == StorageResult::Ok(()) || ind_result == StorageResult::UnprocessableEntity);

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_mdbx_empty_values() {
        let temp_dir = format!("/tmp/test-mdbx-empty-{}", std::process::id());
        let mut storage = MDBXStorage::new(&temp_dir, StorageMode::ReadWrite, None);

        // Test empty string values
        assert!(storage.put_value(StorageId::Individuals, "empty_key", "").is_ok());
        
        let result = storage.get_value(StorageId::Individuals, "empty_key");
        assert!(result.is_ok());
        if let StorageResult::Ok(value) = result {
            assert_eq!(value, "");
        }

        // Test empty raw data
        let empty_vec: Vec<u8> = vec![];
        assert!(storage.put_raw_value(StorageId::Individuals, "empty_raw", empty_vec.clone()).is_ok());
        
        let raw_result = storage.get_raw_value(StorageId::Individuals, "empty_raw");
        assert!(raw_result.is_ok());
        if let StorageResult::Ok(data) = raw_result {
            assert_eq!(data, empty_vec);
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
