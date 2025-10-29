// Example demonstrating zero-copy API for LMDB and MDBX storage
// This API avoids copying data when reading from the database

use v_storage::lmdb_storage::LmdbInstance;
use v_storage::mdbx_storage::MdbxInstance;
use v_storage::common::StorageMode;

fn main() {
    println!("=== Zero-Copy API Example ===\n");
    
    // Example 1: LMDB zero-copy
    println!("1. LMDB zero-copy example:");
    lmdb_zero_copy_example();
    
    println!("\n2. MDBX zero-copy example:");
    mdbx_zero_copy_example();
    
    println!("\n=== Cleanup ===");
    let _ = std::fs::remove_dir_all("/tmp/zero-copy-lmdb");
    let _ = std::fs::remove_dir_all("/tmp/zero-copy-mdbx");
}

fn lmdb_zero_copy_example() {
    // Create LMDB instance
    let mut instance = LmdbInstance::new("/tmp/zero-copy-lmdb/", StorageMode::ReadWrite);
    
    // Put some data
    let test_data = b"Hello, LMDB! This is test data without copying!";
    instance.put("test:key1", test_data);
    println!("   Stored {} bytes", test_data.len());
    
    // Traditional API - copies data
    let _copied_data: Option<Vec<u8>> = instance.get_raw("test:key1");
    println!("   Traditional get: data copied to Vec<u8>");
    
    // Zero-copy API - returns reference!
    if let Ok(txn) = instance.begin_ro_txn() {
        if let Some(data) = instance.get_with_txn(&txn, "test:key1") {
            // 'data' is Cow<[u8]> - a reference to LMDB's memory (Borrowed)!
            // No copying happened here
            println!("   Zero-copy get: {} bytes via Cow::Borrowed", data.len());
            println!("   Data: {:?}", std::str::from_utf8(&data).unwrap());
            
            // You can work with the data as long as txn lives
            // Multiple reads without copying
            if let Some(data2) = instance.get_with_txn(&txn, "test:key1") {
                println!("   Second zero-copy read: still no copying! {} bytes", data2.len());
            }
        }
        // txn drops here, data references are no longer valid
    }
}

fn mdbx_zero_copy_example() {
    // Create MDBX instance
    let mut instance = MdbxInstance::new("/tmp/zero-copy-mdbx/", StorageMode::ReadWrite);
    
    // Put some data
    let test_data = b"Hello, MDBX! Smart copy-on-write with Cow!";
    instance.put("test:key1", test_data);
    println!("   Stored {} bytes", test_data.len());
    
    // Traditional API - always copies
    let _copied_data: Option<Vec<u8>> = instance.get_raw("test:key1");
    println!("   Traditional get: data copied to Vec<u8>");
    
    // Zero-copy API - uses Cow (Copy-on-Write)
    if let Ok(txn) = instance.begin_ro_txn() {
        if let Some(data) = instance.get_with_txn(&txn, "test:key1") {
            // 'data' is Cow<[u8]> - smart pointer that's usually Borrowed (no copy!)
            match data {
                std::borrow::Cow::Borrowed(slice) => {
                    println!("   Zero-copy get: {} bytes via Cow::Borrowed (no copying!)", slice.len());
                    println!("   Data: {:?}", std::str::from_utf8(slice).unwrap());
                },
                std::borrow::Cow::Owned(vec) => {
                    println!("   Zero-copy get: {} bytes via Cow::Owned (data was dirty, had to copy)", vec.len());
                }
            }
            
            // Multiple reads
            if let Some(data2) = instance.get_with_txn(&txn, "test:key1") {
                println!("   Second read: {} bytes", data2.len());
            }
        }
        // txn drops here
    }
}

