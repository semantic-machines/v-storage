use v_storage::{StorageId, StorageMode, MDBXStorage, Storage, StorageProvider};

fn main() {
    // Example 1: Direct MDBX storage creation
    println!("=== Example 1: Direct MDBX Storage ===");
    let mut storage = MDBXStorage::new("/tmp/test-mdbx", StorageMode::ReadWrite, None);
    
    // Put value
    let _ = storage.put_value(StorageId::Individuals, "test:key1", "value1");
    
    // Get value
    match storage.get_value(StorageId::Individuals, "test:key1") {
        v_storage::StorageResult::Ok(value) => {
            println!("Retrieved value: {}", value);
        },
        _ => println!("Value not found"),
    }
    
    // Count entries
    match storage.count(StorageId::Individuals) {
        v_storage::StorageResult::Ok(count) => {
            println!("Total entries: {}", count);
        },
        _ => println!("Failed to count"),
    }
    
    println!();
    
    // Example 2: Using StorageProvider
    println!("=== Example 2: Using StorageProvider ===");
    let mut storage2 = StorageProvider::mdbx("/tmp/test-mdbx-2", StorageMode::ReadWrite, None);
    
    let _ = storage2.put_value(StorageId::Individuals, "user:001", "John Doe");
    
    match storage2.get_value(StorageId::Individuals, "user:001") {
        v_storage::StorageResult::Ok(value) => {
            println!("User name: {}", value);
        },
        _ => println!("User not found"),
    }
    
    println!();
    
    // Example 3: Using VStorage with MDBX
    println!("=== Example 3: VStorage with MDBX ===");
    let mut vstorage = StorageProvider::vstorage_mdbx("/tmp/test-mdbx-3", StorageMode::ReadWrite, None);
    
    let _ = vstorage.put_value(StorageId::Individuals, "product:001", "Laptop");
    
    match vstorage.get_value(StorageId::Individuals, "product:001") {
        v_storage::StorageResult::Ok(value) => {
            println!("Product: {}", value);
        },
        _ => println!("Product not found"),
    }
    
    println!();
    
    // Example 4: Generic MDBX storage (static dispatch, better performance)
    println!("=== Example 4: Generic MDBX Storage ===");
    let mut generic_storage = StorageProvider::mdbx_generic("/tmp/test-mdbx-4", StorageMode::ReadWrite, None);
    
    let _ = generic_storage.put_value(StorageId::Individuals, "item:001", "Book");
    
    match generic_storage.get_value(StorageId::Individuals, "item:001") {
        v_storage::StorageResult::Ok(value) => {
            println!("Item: {}", value);
        },
        _ => println!("Item not found"),
    }
    
    println!();
    
    // Example 5: Builder pattern
    println!("=== Example 5: Builder Pattern ===");
    let storage_result = v_storage::StorageBuilder::new()
        .mdbx("/tmp/test-mdbx-5", StorageMode::ReadWrite, None)
        .build();
    
    if let Ok(storage_box) = storage_result {
        let mut vstorage = v_storage::VStorage::new(storage_box);
        let _ = vstorage.put_value(StorageId::Individuals, "config:db", "mdbx");
        
        match vstorage.get_value(StorageId::Individuals, "config:db") {
            v_storage::StorageResult::Ok(value) => {
                println!("Config DB: {}", value);
            },
            _ => println!("Config not found"),
        }
    }
    
    println!();
    
    // Example 6: Enum-based storage (best performance)
    println!("=== Example 6: Enum-based Storage ===");
    let mut enum_storage = v_storage::VStorageEnum::mdbx("/tmp/test-mdbx-6", StorageMode::ReadWrite, None);
    
    let _ = enum_storage.put_value(StorageId::Individuals, "session:001", "active");
    
    match enum_storage.get_value(StorageId::Individuals, "session:001") {
        v_storage::StorageResult::Ok(value) => {
            println!("Session status: {}", value);
        },
        _ => println!("Session not found"),
    }
    
    println!("\nAll examples completed successfully!");
}

