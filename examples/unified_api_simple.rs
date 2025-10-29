// Example: Unified API for LMDB and MDBX
// Shows that both have identical interface via their instance methods

use v_storage::lmdb_storage::LmdbInstance;
use v_storage::mdbx_storage::MdbxInstance;
use v_storage::common::StorageMode;
use std::borrow::Cow;

fn main() {
    println!("=== Unified API: LMDB vs MDBX ===\n");
    
    // Both instances have identical API!
    let mut lmdb = LmdbInstance::new("/tmp/api-lmdb/", StorageMode::ReadWrite);
    let mut mdbx = MdbxInstance::new("/tmp/api-mdbx/", StorageMode::ReadWrite);
    
    println!("1. LMDB:");
    demo_database(&mut lmdb);
    
    println!("\n2. MDBX:");
    demo_database(&mut mdbx);
    
    println!("\n=== Key Point: Both use IDENTICAL zero-copy API ===");
    println!("   - begin_ro_txn() - creates transaction");
    println!("   - get_with_txn(&txn, key) - returns Cow<[u8]>");
    println!("   - Cow::Borrowed = zero-copy!");
    println!("   - Cow::Owned = copied only when necessary");
    
    // Cleanup
    let _ = std::fs::remove_dir_all("/tmp/api-lmdb");
    let _ = std::fs::remove_dir_all("/tmp/api-mdbx");
}

// This function shows that API is identical - works with both LMDB and MDBX
fn demo_database(storage: &mut dyn UnifiedAPI) {
    // Put
    storage.put_data("key1", b"Test data!");
    
    // Zero-copy read
    storage.read_with_zero_copy("key1");
}

// Trait to show that API is unified
trait UnifiedAPI {
    fn put_data(&mut self, key: &str, val: &[u8]);
    fn read_with_zero_copy(&self, key: &str);
}

impl UnifiedAPI for LmdbInstance {
    fn put_data(&mut self, key: &str, val: &[u8]) {
        self.put(key, val);
        println!("   Stored {} bytes", val.len());
    }
    
    fn read_with_zero_copy(&self, key: &str) {
        if let Ok(txn) = self.begin_ro_txn() {
            if let Some(data) = self.get_with_txn(&txn, key) {
                let cow_type = match data {
                    Cow::Borrowed(_) => "Borrowed (zero-copy!)",
                    Cow::Owned(_) => "Owned (copied)",
                };
                println!("   Read {} bytes via Cow::{}", data.len(), cow_type);
            }
        }
    }
}

impl UnifiedAPI for MdbxInstance {
    fn put_data(&mut self, key: &str, val: &[u8]) {
        self.put(key, val);
        println!("   Stored {} bytes", val.len());
    }
    
    fn read_with_zero_copy(&self, key: &str) {
        if let Ok(txn) = self.begin_ro_txn() {
            if let Some(data) = self.get_with_txn(&txn, key) {
                let cow_type = match data {
                    Cow::Borrowed(_) => "Borrowed (zero-copy!)",
                    Cow::Owned(_) => "Owned (copied)",
                };
                println!("   Read {} bytes via Cow::{}", data.len(), cow_type);
            }
        }
    }
}

