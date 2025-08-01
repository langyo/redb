use rand::Rng;
use rand::prelude::SliceRandom;
use redb::backends::FileBackend;
use redb::{
    AccessGuard, Builder, CompactionError, Database, Durability, Key, MultimapRange,
    MultimapTableDefinition, MultimapValue, Range, ReadableDatabase, ReadableTable,
    ReadableTableMetadata, SetDurabilityError, StorageBackend, TableDefinition, TableStats,
    TransactionError, Value,
};
use redb::{DatabaseError, ReadableMultimapTable, SavepointError, StorageError, TableError};
use std::borrow::Borrow;
use std::fs;
use std::io::{ErrorKind, Write};
use std::marker::PhantomData;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const ELEMENTS: usize = 100;

const SLICE_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("slice");
const SLICE_TABLE2: TableDefinition<&[u8], &[u8]> = TableDefinition::new("slice2");
const STR_TABLE: TableDefinition<&str, &str> = TableDefinition::new("x");
const U64_TABLE: TableDefinition<u64, u64> = TableDefinition::new("u64");

fn create_tempfile() -> tempfile::NamedTempFile {
    if cfg!(target_os = "wasi") {
        tempfile::NamedTempFile::new_in("/tmp").unwrap()
    } else {
        tempfile::NamedTempFile::new().unwrap()
    }
}

/// Returns pairs of key, value
fn random_data(count: usize, key_size: usize, value_size: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = vec![];

    for _ in 0..count {
        let key: Vec<u8> = (0..key_size).map(|_| rand::rng().random()).collect();
        let value: Vec<u8> = (0..value_size).map(|_| rand::rng().random()).collect();
        pairs.push((key, value));
    }

    pairs
}

#[test]
fn previous_io_error() {
    #[derive(Debug)]
    struct FailingBackend {
        inner: FileBackend,
        fail_flag: Arc<AtomicBool>,
    }

    impl FailingBackend {
        fn new(backend: FileBackend, fail_flag: Arc<AtomicBool>) -> Self {
            Self {
                inner: backend,
                fail_flag,
            }
        }
    }

    impl StorageBackend for FailingBackend {
        fn len(&self) -> Result<u64, std::io::Error> {
            self.inner.len()
        }

        fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), std::io::Error> {
            self.inner.read(offset, out)
        }

        fn set_len(&self, len: u64) -> Result<(), std::io::Error> {
            self.inner.set_len(len)
        }

        fn sync_data(&self) -> Result<(), std::io::Error> {
            if self.fail_flag.load(Ordering::SeqCst) {
                Err(std::io::Error::from(ErrorKind::Other))
            } else {
                self.inner.sync_data()
            }
        }

        fn write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
            self.inner.write(offset, data)
        }
    }

    let tmpfile = create_tempfile();

    let fail_flag = Arc::new(AtomicBool::new(false));
    let backend = FailingBackend::new(
        FileBackend::new(tmpfile.into_file()).unwrap(),
        fail_flag.clone(),
    );
    let db = Database::builder().create_with_backend(backend).unwrap();
    fail_flag.store(true, Ordering::SeqCst);
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&0, &0).unwrap();
    }
    assert!(txn.commit().is_err());

    assert!(matches!(
        db.begin_write().err().unwrap(),
        TransactionError::Storage(StorageError::PreviousIo)
    ));
}

#[test]
fn mixed_durable_commit() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(Durability::None).unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&0, &0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();
}

#[test]
fn non_durable_commit_persistence() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(Durability::None).unwrap();
    let pairs = random_data(100, 16, 20);
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..ELEMENTS {
            let (key, value) = &pairs[i % pairs.len()];
            table.insert(key.as_slice(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    // Check that cleanly closing the database persists the non-durable commit
    drop(db);
    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_read().unwrap();
    let table = txn.open_table(SLICE_TABLE).unwrap();

    let mut key_order: Vec<usize> = (0..ELEMENTS).collect();
    key_order.shuffle(&mut rand::rng());

    {
        for i in &key_order {
            let (key, value) = &pairs[*i % pairs.len()];
            assert_eq!(table.get(key.as_slice()).unwrap().unwrap().value(), value);
        }
    }
}

fn test_persistence(durability: Durability) {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    let pairs = random_data(100, 16, 20);
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..ELEMENTS {
            let (key, value) = &pairs[i % pairs.len()];
            table.insert(key.as_slice(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    drop(db);
    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_read().unwrap();
    let table = txn.open_table(SLICE_TABLE).unwrap();

    let mut key_order: Vec<usize> = (0..ELEMENTS).collect();
    key_order.shuffle(&mut rand::rng());

    {
        for i in &key_order {
            let (key, value) = &pairs[*i % pairs.len()];
            assert_eq!(table.get(key.as_slice()).unwrap().unwrap().value(), value);
        }
    }
}

#[test]
fn immediate_persistence() {
    test_persistence(Durability::Immediate);
}

#[test]
fn immediate_free() {
    test_free(Durability::Immediate);
}

#[test]
fn nondurable_free() {
    test_free(Durability::None);
}

fn test_free(durability: Durability) {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    {
        let _table = txn.open_table(SLICE_TABLE).unwrap();
        let mut table = txn.open_table(SLICE_TABLE2).unwrap();
        table.insert([].as_slice(), [].as_slice()).unwrap();
    }
    txn.commit().unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    {
        let mut table = txn.open_table(SLICE_TABLE2).unwrap();
        table.remove([].as_slice()).unwrap();
    }
    txn.commit().unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    let allocated_pages = txn.stats().unwrap().allocated_pages();

    let key = vec![0; 100];
    let value = vec![0u8; 1024];
    let target_db_size = 8 * 1024 * 1024;
    // Write 10% of db space each iteration
    let num_writes = target_db_size / 10 / (key.len() + value.len());
    // Make sure an internal index page is required
    assert!(num_writes > 64);

    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..num_writes {
            let mut mut_key = key.clone();
            mut_key.extend_from_slice(&(i as u64).to_le_bytes());
            table.insert(mut_key.as_slice(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    {
        let key_range: Vec<usize> = (0..num_writes).collect();
        // Delete in chunks to be sure that we don't run out of pages due to temp allocations
        for chunk in key_range.chunks(10) {
            let mut txn = db.begin_write().unwrap();
            txn.set_durability(durability).unwrap();
            {
                let mut table = txn.open_table(SLICE_TABLE).unwrap();
                for i in chunk {
                    let mut mut_key = key.clone();
                    mut_key.extend_from_slice(&(*i as u64).to_le_bytes());
                    table.remove(mut_key.as_slice()).unwrap();
                }
            }
            txn.commit().unwrap();
        }
    }

    // Extra commit to finalize the cleanup of the freed pages
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    txn.commit().unwrap();
    let mut txn = db.begin_write().unwrap();
    txn.set_durability(durability).unwrap();
    assert_eq!(allocated_pages, txn.stats().unwrap().allocated_pages());
    txn.abort().unwrap();
}

#[test]
fn nondurable_live_and_free() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(0).unwrap();
    }
    txn.commit().unwrap();
    // Process frees
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();
    let txn = db.begin_write().unwrap();
    let allocated_pages = txn.stats().unwrap().allocated_pages();
    txn.abort().unwrap();

    let mut txn = db.begin_write().unwrap();
    txn.set_durability(Durability::None).unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 1).unwrap();
    }
    txn.commit().unwrap();
    let read_txn = db.begin_read().unwrap();

    for i in 0..5 {
        let mut txn = db.begin_write().unwrap();
        txn.set_durability(Durability::None).unwrap();
        {
            let mut table = txn.open_table(U64_TABLE).unwrap();
            table.insert(0, i).unwrap();
        }
        txn.commit().unwrap();
    }

    {
        let table = read_txn.open_table(U64_TABLE).unwrap();
        assert_eq!(table.get(0).unwrap().unwrap().value(), 1);
    }
    drop(read_txn);

    let mut txn = db.begin_write().unwrap();
    txn.set_durability(Durability::None).unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(0).unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_write().unwrap();
    txn.set_durability(Durability::None).unwrap();
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    // allocated * 2, because we can't free the original persisted pages
    // + 2, because now we need freed trees to store those original pages to be freed
    assert!(txn.stats().unwrap().allocated_pages() <= allocated_pages * 2 + 2);
}

#[test]
fn large_values() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();

    let mut key = vec![0u8; 1024];
    let value = vec![0u8; 2_000_000];
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..5 {
            key[0] = i;
            table.insert(key.as_slice(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..5 {
            key[0] = i;
            table.remove(key.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
}

// Note: this test requires > 3GiB of memory
#[test]
#[cfg(target_pointer_width = "64")]
fn value_too_large() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();

    let small_value = vec![0u8; 1024];
    let too_big_value = vec![0u8; 3 * 1024 * 1024 * 1024 + 1];
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        assert!(matches!(
            table.insert(small_value.as_slice(), too_big_value.as_slice()),
            Err(StorageError::ValueTooLarge(_))
        ));
        assert!(matches!(
            table.insert(too_big_value.as_slice(), small_value.as_slice()),
            Err(StorageError::ValueTooLarge(_))
        ));
        assert!(matches!(
            table.insert(too_big_value.as_slice(), too_big_value.as_slice()),
            Err(StorageError::ValueTooLarge(_))
        ));
        drop(too_big_value);
        let almost_big_value = vec![0u8; 2 * 1024 * 1024 * 1024];
        assert!(matches!(
            table.insert(almost_big_value.as_slice(), almost_big_value.as_slice()),
            Err(StorageError::ValueTooLarge(_))
        ));
    }
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = txn.open_table(SLICE_TABLE).unwrap();
    assert!(table.is_empty().unwrap());
}

#[test]
fn small_db_is_small_file() {
    let tmpfile = create_tempfile();
    const TABLE: TableDefinition<u32, u32> = TableDefinition::new("TABLE");

    let mut db = Database::create(tmpfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let mut table = wtx.open_table(TABLE).unwrap();
    table.insert(0, 0).unwrap();
    drop(table);
    wtx.commit().unwrap();

    db.compact().unwrap();

    drop(db);
    let metadata = tmpfile.as_file().metadata().unwrap();
    assert!(
        metadata.len() < 40 * 1024,
        "File size: {:?}",
        metadata.len()
    );
}

#[test]
fn many_pairs() {
    let tmpfile = create_tempfile();
    const TABLE: TableDefinition<u32, u32> = TableDefinition::new("TABLE");

    let db = Database::create(tmpfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let mut table = wtx.open_table(TABLE).unwrap();

    for i in 0..200_000 {
        table.insert(i, i).unwrap();

        if i % 10_000 == 0 {
            eprintln!("{i}");
        }
    }

    drop(table);

    wtx.commit().unwrap();
}

#[test]
fn explicit_close() {
    let tmpfile = create_tempfile();
    const TABLE: TableDefinition<u32, u32> = TableDefinition::new("TABLE");
    let db = Database::create(tmpfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    wtx.open_table(TABLE).unwrap();
    wtx.commit().unwrap();

    let tx = db.begin_read().unwrap();
    let table = tx.open_table(TABLE).unwrap();
    assert!(matches!(
        tx.close(),
        Err(TransactionError::ReadTransactionStillInUse(_))
    ));
    drop(table);

    let tx2 = db.begin_read().unwrap();
    tx2.close().unwrap();
}

#[test]
fn large_keys() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();

    let mut key = vec![0u8; 1024];
    let value = vec![0u8; 1];
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..100 {
            key[0] = i;
            table.insert(key.as_slice(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..100 {
            key[0] = i;
            table.remove(key.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
}

#[test]
fn dynamic_growth() {
    let tmpfile = create_tempfile();
    let table_definition: TableDefinition<u64, &[u8]> = TableDefinition::new("x");
    let big_value = vec![0u8; 1024];

    let expected_size = 10 * 1024 * 1024;
    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table_definition).unwrap();
        table.insert(&0, big_value.as_slice()).unwrap();
    }
    txn.commit().unwrap();

    let initial_file_size = tmpfile.as_file().metadata().unwrap().len();
    assert!(initial_file_size < (expected_size / 2) as u64);

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table_definition).unwrap();
        for i in 0..2048 {
            table.insert(&i, big_value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let file_size = tmpfile.as_file().metadata().unwrap().len();

    assert!(file_size > initial_file_size);
}

#[test]
fn multi_page_kv() {
    let tmpfile = create_tempfile();
    let elements = 4;
    let page_size = 4096;

    let db = Builder::new().create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();

    let mut key = vec![0u8; page_size + 1];
    let mut value = vec![0; page_size + 1];
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..elements {
            key[0] = i;
            value[0] = i;
            table.insert(key.as_slice(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = txn.open_table(SLICE_TABLE).unwrap();
    for i in 0..elements {
        key[0] = i;
        value[0] = i;
        assert_eq!(&value, table.get(key.as_slice()).unwrap().unwrap().value());
    }

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..elements {
            key[0] = i;
            table.remove(key.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
}

#[test]
// Test for a bug in the deletion code, where deleting a key accidentally deleted other keys
fn regression() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&1, &1).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&6, &9).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&12, &10).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&18, &27).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&24, &33).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(&30, &14).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(&30).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = txn.open_table(U64_TABLE).unwrap();
    let v = table.get(&6).unwrap().unwrap().value();
    assert_eq!(v, 9);
}

#[test]
// Test for a bug in table creation code, where multiple tables could end up with the same id
fn regression2() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let tx = db.begin_write().unwrap();

    let a_def: TableDefinition<&str, &str> = TableDefinition::new("a");
    let b_def: TableDefinition<&str, &str> = TableDefinition::new("b");
    let c_def: TableDefinition<&str, &str> = TableDefinition::new("c");

    let _c = tx.open_table(c_def).unwrap();
    let b = tx.open_table(b_def).unwrap();
    let mut a = tx.open_table(a_def).unwrap();
    a.insert("hi", "1").unwrap();
    assert!(b.get("hi").unwrap().is_none());
}

#[test]
// Test for a bug in deletion code, where deletions could delete neighboring keys in a leaf,
// due to the partial leaf entries being dropped
fn regression3() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(SLICE_TABLE).unwrap();
        let big_value = vec![0u8; 1000];
        for i in 0..20u8 {
            t.insert([i].as_slice(), big_value.as_slice()).unwrap();
        }
        for i in (10..20u8).rev() {
            t.remove([i].as_slice()).unwrap();
            for j in 0..i {
                assert!(t.get([j].as_slice()).unwrap().is_some());
            }
        }
    }
    tx.commit().unwrap();
}

#[test]
fn regression7() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let big_value = vec![0u8; 4063];
        t.insert(&35723, big_value.as_slice()).unwrap();
        t.remove(&145278).unwrap();
        t.remove(&145227).unwrap();
    }
    tx.commit().unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::None).unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 47];
        t.insert(&66469, v.as_slice()).unwrap();
        let v = vec![0u8; 2414];
        t.insert(&146255, v.as_slice()).unwrap();
        let v = vec![0u8; 159];
        t.insert(&153701, v.as_slice()).unwrap();
        let v = vec![0u8; 1186];
        t.insert(&145227, v.as_slice()).unwrap();
        let v = vec![0u8; 223];
        t.insert(&118749, v.as_slice()).unwrap();

        t.remove(&145227).unwrap();

        let mut iter = t.range(138763..(138763 + 232359)).unwrap().rev();
        assert_eq!(iter.next().unwrap().unwrap().0.value(), 153701);
        assert_eq!(iter.next().unwrap().unwrap().0.value(), 146255);
        assert!(iter.next().is_none());
    }
    tx.commit().unwrap();
}

#[test]
fn regression8() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::None).unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 1186];
        t.insert(&145227, v.as_slice()).unwrap();
        let v = vec![0u8; 1585];
        t.insert(&565922, v.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 2040];
        t.insert(&94937, v.as_slice()).unwrap();
        let v = vec![0u8; 2058];
        t.insert(&130571, v.as_slice()).unwrap();
        t.remove(&145227).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 947];
        t.insert(&118749, v.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let t = tx.open_table(table_def).unwrap();
        let mut iter = t.range(118749..142650).unwrap();
        assert_eq!(iter.next().unwrap().unwrap().0.value(), 118749);
        assert_eq!(iter.next().unwrap().unwrap().0.value(), 130571);
        assert!(iter.next().is_none());
    }
    tx.commit().unwrap();
}

#[test]
fn regression9() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 118665];
        t.insert(&452, v.as_slice()).unwrap();
        t.len().unwrap();
    }
    tx.commit().unwrap();
}

#[test]
fn regression10() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 1043];
        t.insert(&118749, v.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 952];
        t.insert(&118757, v.as_slice()).unwrap();
    }
    tx.abort().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let t = tx.open_table(table_def).unwrap();
        t.get(&829513).unwrap();
    }
    tx.abort().unwrap();
}

#[test]
fn regression11() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 1204];
        t.insert(&118749, v.as_slice()).unwrap();
        let v = vec![0u8; 2062];
        t.insert(&153697, v.as_slice()).unwrap();
        let v = vec![0u8; 2980];
        t.insert(&110557, v.as_slice()).unwrap();
        let v = vec![0u8; 1999];
        t.insert(&677853, v.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let v = vec![0u8; 691];
        t.insert(&103591, v.as_slice()).unwrap();
        let v = vec![0u8; 952];
        t.insert(&118757, v.as_slice()).unwrap();
    }
    tx.abort().unwrap();

    let tx = db.begin_write().unwrap();
    tx.commit().unwrap();
}

#[test]
// Test that for stale read bug when re-opening a table during a write
fn regression12() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, u64> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        t.insert(&0, &0).unwrap();
        assert_eq!(t.get(&0).unwrap().unwrap().value(), 0);
        drop(t);

        let t2 = tx.open_table(table_def).unwrap();
        assert_eq!(t2.get(&0).unwrap().unwrap().value(), 0);
    }
    tx.commit().unwrap();
}

#[test]
fn regression13() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new("x");

    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::None).unwrap();
    {
        let mut t = tx.open_multimap_table(table_def).unwrap();
        let value = vec![0; 1026];
        t.insert(&539717, value.as_slice()).unwrap();
        let value = vec![0; 530];
        t.insert(&539717, value.as_slice()).unwrap();
    }
    tx.abort().unwrap();
}

#[test]
fn regression14() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let table_def: MultimapTableDefinition<u64, &[u8]> = MultimapTableDefinition::new("x");

    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::None).unwrap();
    {
        let mut t = tx.open_multimap_table(table_def).unwrap();
        let value = vec![0; 1424];
        t.insert(&539749, value.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::None).unwrap();
    {
        let mut t = tx.open_multimap_table(table_def).unwrap();
        let value = vec![0; 2230];
        t.insert(&776971, value.as_slice()).unwrap();

        let mut iter = t.range(514043..(514043 + 514043)).unwrap().rev();
        {
            let (key, mut value_iter) = iter.next().unwrap().unwrap();
            assert_eq!(key.value(), 776971);
            assert_eq!(value_iter.next().unwrap().unwrap().value(), &[0; 2230]);
        }
        {
            let (key, mut value_iter) = iter.next().unwrap().unwrap();
            assert_eq!(key.value(), 539749);
            assert_eq!(value_iter.next().unwrap().unwrap().value(), &[0; 1424]);
        }
    }
    tx.abort().unwrap();
}

#[test]
fn regression17() {
    let tmpfile = create_tempfile();

    let db = Database::builder().create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::None).unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let value = vec![0; 4578];
        t.insert(&671325, value.as_slice()).unwrap();

        let mut value = t.insert_reserve(&723904, 2246).unwrap();
        value.as_mut().fill(0xFF);
    }
    tx.abort().unwrap();
}

#[test]
fn regression18() {
    let tmpfile = create_tempfile();

    let db = Database::builder().create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    let savepoint0 = tx.ephemeral_savepoint().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let mut value = t.insert_reserve(&118749, 817).unwrap();
        value.as_mut().fill(0xFF);
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    let savepoint1 = tx.ephemeral_savepoint().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let mut value = t.insert_reserve(&65373, 1807).unwrap();
        value.as_mut().fill(0xFF);
    }
    tx.commit().unwrap();

    let mut tx = db.begin_write().unwrap();
    let savepoint2 = tx.ephemeral_savepoint().unwrap();

    tx.restore_savepoint(&savepoint2).unwrap();
    tx.commit().unwrap();

    drop(savepoint0);

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let mut value = t.insert_reserve(&118749, 2494).unwrap();
        value.as_mut().fill(0xFF);
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    let savepoint4 = tx.ephemeral_savepoint().unwrap();
    tx.abort().unwrap();
    drop(savepoint1);

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let mut value = t.insert_reserve(&429469, 667).unwrap();
        value.as_mut().fill(0xFF);
        drop(value);
        let mut value = t.insert_reserve(&266845, 1614).unwrap();
        value.as_mut().fill(0xFF);
    }
    tx.commit().unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.restore_savepoint(&savepoint4).unwrap();
    tx.commit().unwrap();

    drop(savepoint2);
    drop(savepoint4);
}

#[test]
fn regression19() {
    let tmpfile = create_tempfile();

    let db = Database::builder().create(tmpfile.path()).unwrap();

    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new("x");

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let value = vec![0xFF; 100];
        t.insert(&1, value.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    let savepoint0 = tx.ephemeral_savepoint().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let value = vec![0xFF; 101];
        t.insert(&1, value.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    {
        let mut t = tx.open_table(table_def).unwrap();
        let value = vec![0xFF; 102];
        t.insert(&1, value.as_slice()).unwrap();
    }
    tx.commit().unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.restore_savepoint(&savepoint0).unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    tx.open_table(table_def).unwrap();
}

#[test]
fn regression20() {
    let tmpfile = create_tempfile();

    let table_def: MultimapTableDefinition<'static, u128, u128> =
        MultimapTableDefinition::new("some-table");

    for _ in 0..3 {
        let mut db = Database::builder().create(tmpfile.path()).unwrap();
        db.check_integrity().unwrap();

        let txn = db.begin_write().unwrap();
        let mut table = txn.open_multimap_table(table_def).unwrap();

        for i in 0..1024 {
            table.insert(0, i).unwrap();
        }
        drop(table);

        txn.commit().unwrap();
    }
}

#[test]
fn regression21() {
    let tmpfile = create_tempfile();

    let mut db = Database::create(tmpfile.path()).unwrap();

    let write_tx = db.begin_write().unwrap();
    let read_tx = db.begin_read().unwrap();

    let mut write_table = write_tx
        .open_table::<&str, &str>(TableDefinition::new("example"))
        .unwrap();

    write_table.insert("example", "example").unwrap();

    drop(write_table);

    write_tx.commit().unwrap();
    assert!(matches!(
        db.compact().unwrap_err(),
        CompactionError::TransactionInProgress
    ));
    drop(read_tx);
}

#[test]
fn regression22() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(0).unwrap();
    }
    txn.commit().unwrap();

    // Extra commit to finalize the cleanup of the freed pages
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    let allocated_pages = txn.stats().unwrap().allocated_pages();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(0).unwrap();
    }
    txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();

    // Extra commit to finalize the cleanup of the freed pages. The read transaction should not
    // block the freeing, but there was a bug where it did.
    db.begin_write().unwrap().commit().unwrap();

    drop(read_txn);

    let txn = db.begin_write().unwrap();
    assert_eq!(allocated_pages, txn.stats().unwrap().allocated_pages());
}

#[test]
fn regression23() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        // List the savepoints to ensure the system table is created and occupies a page
        #[allow(unused_must_use)]
        {
            txn.list_persistent_savepoints().unwrap();
        }
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(0).unwrap();
    }
    txn.commit().unwrap();

    // Extra commit to finalize the cleanup of the freed pages
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    let allocated_pages = txn.stats().unwrap().allocated_pages();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.remove(0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    let savepoint = txn.ephemeral_savepoint().unwrap();
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_write().unwrap();
    txn.restore_savepoint(&savepoint).unwrap();
    txn.commit().unwrap();
    drop(savepoint);

    // Extra commit to finalize the cleanup of the freed pages.
    // There was a bug where the restoration of the savepoint would leak pages
    db.begin_write().unwrap().commit().unwrap();
    db.begin_write().unwrap().commit().unwrap();

    let txn = db.begin_write().unwrap();
    assert_eq!(allocated_pages, txn.stats().unwrap().allocated_pages());
}

#[test]
fn regression24() {
    let tmpfile = create_tempfile();

    let table_def: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("x");

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    {
        // Touch the savepoints tables to be sure they get created, so that they occupy pages
        let id = txn.persistent_savepoint().unwrap();
        txn.delete_persistent_savepoint(id).unwrap();
        // List the savepoints to ensure the system table is created and occupies a page
        #[allow(unused_must_use)]
        {
            txn.list_persistent_savepoints().unwrap();
        }
        let mut table = txn.open_table(U64_TABLE).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        txn.delete_table(U64_TABLE).unwrap();
    }
    txn.commit().unwrap();

    // Extra commit to finalize the cleanup of the freed pages
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    let allocated_pages = txn.stats().unwrap().allocated_pages();
    {
        let mut table = txn.open_multimap_table(table_def).unwrap();
        table.insert(0, 0).unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        txn.delete_multimap_table(table_def).unwrap();
    }
    txn.commit().unwrap();

    // Extra commit to finalize the cleanup of the freed pages.
    // There was a bug where deleting a multimap table leaked pages
    db.begin_write().unwrap().commit().unwrap();

    let txn = db.begin_write().unwrap();
    assert_eq!(allocated_pages, txn.stats().unwrap().allocated_pages());
}

#[test]
fn check_integrity_clean() {
    let tmpfile = create_tempfile();

    let table_def: TableDefinition<'static, u64, u64> = TableDefinition::new("x");

    let mut db = Database::builder().create(tmpfile.path()).unwrap();
    assert!(db.check_integrity().unwrap());

    let txn = db.begin_write().unwrap();
    let mut table = txn.open_table(table_def).unwrap();

    for i in 0..10 {
        table.insert(0, i).unwrap();
    }
    drop(table);

    txn.commit().unwrap();
    assert!(db.check_integrity().unwrap());
    drop(db);

    let mut db = Database::builder().create(tmpfile.path()).unwrap();
    assert!(db.check_integrity().unwrap());
    drop(db);

    let mut db = Database::builder().open(tmpfile.path()).unwrap();
    assert!(db.check_integrity().unwrap());
}

#[test]
fn multimap_stats() {
    let tmpfile = create_tempfile();
    let db = Database::builder().create(tmpfile.path()).unwrap();

    let table_def: MultimapTableDefinition<u128, u128> = MultimapTableDefinition::new("x");

    let mut last_size = 0;
    for i in 0..1000 {
        let mut txn = db.begin_write().unwrap();
        txn.set_durability(Durability::None).unwrap();
        let mut table = txn.open_multimap_table(table_def).unwrap();
        table.insert(0, i).unwrap();
        drop(table);
        txn.commit().unwrap();

        let txn = db.begin_write().unwrap();
        let bytes = txn.stats().unwrap().stored_bytes();
        assert!(bytes > last_size, "{i}");
        last_size = bytes;
    }
}

#[test]
fn no_downgrade_durability_with_savepoint() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.persistent_savepoint().unwrap();
    assert!(matches!(
        tx.set_durability(Durability::None),
        Err(SetDurabilityError::PersistentSavepointModified)
    ));
    assert!(matches!(tx.set_durability(Durability::Immediate), Ok(())));
}

#[test]
fn no_savepoint_resurrection() {
    let tmpfile = create_tempfile();

    let db = Database::builder()
        .set_cache_size(41178283)
        .create(tmpfile.path())
        .unwrap();

    let tx = db.begin_write().unwrap();
    let persistent_savepoint = tx.persistent_savepoint().unwrap();
    tx.commit().unwrap();

    let tx = db.begin_write().unwrap();
    let savepoint2 = tx.ephemeral_savepoint().unwrap();
    tx.delete_persistent_savepoint(persistent_savepoint)
        .unwrap();
    tx.commit().unwrap();

    let mut tx = db.begin_write().unwrap();
    tx.restore_savepoint(&savepoint2).unwrap();
    tx.delete_persistent_savepoint(persistent_savepoint)
        .unwrap();
    tx.commit().unwrap();
}

#[test]
fn non_durable_read_isolation() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let mut write_txn = db.begin_write().unwrap();
    write_txn.set_durability(Durability::None).unwrap();
    {
        let mut table = write_txn.open_table(STR_TABLE).unwrap();
        table.insert("hello", "world").unwrap();
    }
    write_txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    let read_table = read_txn.open_table(STR_TABLE).unwrap();
    assert_eq!("world", read_table.get("hello").unwrap().unwrap().value());

    let mut write_txn = db.begin_write().unwrap();
    write_txn.set_durability(Durability::None).unwrap();
    {
        let mut table = write_txn.open_table(STR_TABLE).unwrap();
        table.remove("hello").unwrap();
        table.insert("hello2", "world2").unwrap();
        table.insert("hello3", "world3").unwrap();
    }
    write_txn.commit().unwrap();

    let read_txn2 = db.begin_read().unwrap();
    let read_table2 = read_txn2.open_table(STR_TABLE).unwrap();
    assert!(read_table2.get("hello").unwrap().is_none());
    assert_eq!(
        "world2",
        read_table2.get("hello2").unwrap().unwrap().value()
    );
    assert_eq!(
        "world3",
        read_table2.get("hello3").unwrap().unwrap().value()
    );
    assert_eq!(read_table2.len().unwrap(), 2);

    assert_eq!("world", read_table.get("hello").unwrap().unwrap().value());
    assert!(read_table.get("hello2").unwrap().is_none());
    assert!(read_table.get("hello3").unwrap().is_none());
    assert_eq!(read_table.len().unwrap(), 1);
}

#[test]
fn range_query() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(U64_TABLE).unwrap();
        for i in 0..10 {
            table.insert(&i, &i).unwrap();
        }
    }
    write_txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    let table = read_txn.open_table(U64_TABLE).unwrap();
    let mut iter = table.range(3..7).unwrap();
    for i in 3..7u64 {
        let (key, value) = iter.next().unwrap().unwrap();
        assert_eq!(i, key.value());
        assert_eq!(i, value.value());
    }
    assert!(iter.next().is_none());

    let mut iter = table.range(3..=7).unwrap();
    for i in 3..=7u64 {
        let (key, value) = iter.next().unwrap().unwrap();
        assert_eq!(i, key.value());
        assert_eq!(i, value.value());
    }
    assert!(iter.next().is_none());

    let total: u64 = table
        .range(1..=3)
        .unwrap()
        .map(|item| item.unwrap().1.value())
        .sum();
    assert_eq!(total, 6);
}

#[test]
fn range_query_reversed() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let write_txn = db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(U64_TABLE).unwrap();
        for i in 0..10 {
            table.insert(&i, &i).unwrap();
        }
    }
    write_txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    let table = read_txn.open_table(U64_TABLE).unwrap();
    let mut iter = table.range(3..7).unwrap().rev();
    for i in (3..7u64).rev() {
        let (key, value) = iter.next().unwrap().unwrap();
        assert_eq!(i, key.value());
        assert_eq!(i, value.value());
    }
    assert!(iter.next().is_none());

    // Test reversing multiple times
    let mut iter = table.range(3..7).unwrap();
    let (key, _) = iter.next().unwrap().unwrap();
    assert_eq!(3, key.value());

    let mut iter = iter.rev();
    let (key, _) = iter.next().unwrap().unwrap();
    assert_eq!(6, key.value());
    let (key, _) = iter.next().unwrap().unwrap();
    assert_eq!(5, key.value());

    let mut iter = iter.rev();
    let (key, _) = iter.next().unwrap().unwrap();
    assert_eq!(4, key.value());

    assert!(iter.next().is_none());
}

#[test]
fn alias_table() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();

    let write_txn = db.begin_write().unwrap();
    let table = write_txn.open_table(STR_TABLE).unwrap();
    let result = write_txn.open_table(STR_TABLE);
    assert!(matches!(
        result.err().unwrap(),
        TableError::TableAlreadyOpen(_, _)
    ));
    drop(table);
}

#[test]
fn delete_table() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();

    let y_def: MultimapTableDefinition<&str, &str> = MultimapTableDefinition::new("y");

    let write_txn = db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(STR_TABLE).unwrap();
        table.insert("hello", "world").unwrap();
        let mut multitable = write_txn.open_multimap_table(y_def).unwrap();
        multitable.insert("hello2", "world2").unwrap();
    }
    write_txn.commit().unwrap();

    let write_txn = db.begin_write().unwrap();
    assert!(write_txn.delete_table(STR_TABLE).unwrap());
    assert!(!write_txn.delete_table(STR_TABLE).unwrap());
    assert!(write_txn.delete_multimap_table(y_def).unwrap());
    assert!(!write_txn.delete_multimap_table(y_def).unwrap());
    write_txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    let result = read_txn.open_table(STR_TABLE);
    assert!(result.is_err());
    let result = read_txn.open_multimap_table(y_def);
    assert!(result.is_err());
}

#[test]
fn delete_all_tables() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();

    let x_def: TableDefinition<&str, &str> = TableDefinition::new("x");
    let y_def: TableDefinition<&str, &str> = TableDefinition::new("y");

    let write_txn = db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(x_def).unwrap();
        table.insert("hello", "world").unwrap();
        let mut table = write_txn.open_table(y_def).unwrap();
        table.insert("hello", "world").unwrap();
    }
    write_txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    assert_eq!(2, read_txn.list_tables().unwrap().count());

    let write_txn = db.begin_write().unwrap();
    for table in write_txn.list_tables().unwrap() {
        write_txn.delete_table(table).unwrap();
    }
    write_txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    assert_eq!(0, read_txn.list_tables().unwrap().count());
}

#[test]
fn dropped_write() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();

    let write_txn = db.begin_write().unwrap();
    {
        let mut table = write_txn.open_table(STR_TABLE).unwrap();
        table.insert("hello", "world").unwrap();
    }
    drop(write_txn);
    let read_txn = db.begin_read().unwrap();
    let result = read_txn.open_table(STR_TABLE);
    assert!(matches!(result, Err(TableError::TableDoesNotExist(_))));
}

#[test]
fn non_page_size_multiple() {
    let tmpfile = create_tempfile();

    let db = Database::create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();
    let key = vec![0u8; 1024];
    let value = vec![0u8; 1];
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        table.insert(key.as_slice(), value.as_slice()).unwrap();
    }
    txn.commit().unwrap();

    let read_txn = db.begin_read().unwrap();
    let table = read_txn.open_table(SLICE_TABLE).unwrap();
    assert_eq!(table.len().unwrap(), 1);
}

#[test]
fn does_not_exist() {
    let tmpfile = create_tempfile();
    fs::remove_file(tmpfile.path()).unwrap();
    let result = Database::open(tmpfile.path());
    if let Err(DatabaseError::Storage(StorageError::Io(e))) = result {
        assert!(matches!(e.kind(), ErrorKind::NotFound));
    } else {
        panic!();
    }

    let tmpfile = create_tempfile();

    let result = Database::open(tmpfile.path());
    if let Err(DatabaseError::Storage(StorageError::Io(e))) = result {
        assert!(matches!(e.kind(), ErrorKind::InvalidData));
    } else {
        panic!();
    }
}

#[test]
fn invalid_database_file() {
    let mut tmpfile = create_tempfile();
    tmpfile.write_all(b"hi").unwrap();
    let result = Database::open(tmpfile.path());
    if let Err(DatabaseError::Storage(StorageError::Io(e))) = result {
        assert!(matches!(e.kind(), ErrorKind::InvalidData));
    } else {
        panic!();
    }

    let result = Database::create(tmpfile.path());
    if let Err(DatabaseError::Storage(StorageError::Io(e))) = result {
        assert!(matches!(e.kind(), ErrorKind::InvalidData));
    } else {
        panic!();
    }
}

#[test]
fn wrong_types() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();

    let definition: TableDefinition<u32, u32> = TableDefinition::new("x");
    let wrong_definition: TableDefinition<u64, u64> = TableDefinition::new("x");

    let txn = db.begin_write().unwrap();
    txn.open_table(definition).unwrap();
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    assert!(matches!(
        txn.open_table(wrong_definition),
        Err(TableError::TableTypeMismatch { .. })
    ));
    txn.abort().unwrap();

    let txn = db.begin_read().unwrap();
    txn.open_table(definition).unwrap();
    assert!(matches!(
        txn.open_table(wrong_definition),
        Err(TableError::TableTypeMismatch { .. })
    ));
}

#[test]
fn tree_balance() {
    const EXPECTED_ORDER: usize = 9;
    fn expected_height(mut elements: usize) -> u32 {
        // Root may have only 2 entries
        let mut height = 1;
        elements /= 2;

        // Leaves may have only a single entry
        height += 1;

        // Each internal node half-full, plus 1 to round up
        height += (elements as f32).log((EXPECTED_ORDER / 2) as f32) as usize + 1;

        height.try_into().unwrap()
    }

    let tmpfile = create_tempfile();

    // One for the last table id counter, and one for the "x" -> TableDefinition entry
    let num_internal_entries = 2;

    // Pages are 4kb, so use a key size such that 9 keys will fit
    let key_size = 410;
    let db = Database::builder().create(tmpfile.path()).unwrap();
    let txn = db.begin_write().unwrap();

    let elements = (EXPECTED_ORDER / 2).pow(2) - num_internal_entries;

    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in (0..elements).rev() {
            let mut key = vec![0u8; key_size];
            key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            table.insert(key.as_slice(), b"".as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let expected = expected_height(elements + num_internal_entries);
    let txn = db.begin_write().unwrap();
    let height = txn.stats().unwrap().tree_height();
    assert!(height <= expected, "height={height} expected={expected}",);

    let reduce_to = EXPECTED_ORDER / 2 - num_internal_entries;
    {
        let mut table = txn.open_table(SLICE_TABLE).unwrap();
        for i in 0..(elements - reduce_to) {
            let mut key = vec![0u8; key_size];
            key[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            table.remove(key.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let expected = expected_height(reduce_to + num_internal_entries);
    let txn = db.begin_write().unwrap();
    let height = txn.stats().unwrap().tree_height();
    txn.abort().unwrap();
    assert!(height <= expected, "height={height} expected={expected}",);
}

#[cfg(not(target_os = "wasi"))] // TODO remove this line once WASI gets flock
#[test]
fn database_lock() {
    let tmpfile = create_tempfile();
    let result = Database::create(tmpfile.path());
    assert!(result.is_ok());
    let result2 = Database::open(tmpfile.path());
    assert!(
        matches!(result2, Err(DatabaseError::DatabaseAlreadyOpen)),
        "{result2:?}",
    );
    drop(result);
    let result = Database::open(tmpfile.path());
    assert!(result.is_ok());
}

#[test]
fn persistent_savepoint() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let definition: TableDefinition<u32, &str> = TableDefinition::new("x");

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        table.insert(&0, "hello").unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    let savepoint_id = txn.persistent_savepoint().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        table.remove(&0).unwrap();
    }
    txn.commit().unwrap();

    drop(db);
    let db = Database::create(tmpfile.path()).unwrap();
    // Make sure running the GC doesn't invalidate the savepoint
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_write().unwrap();
    let savepoint = txn.get_persistent_savepoint(savepoint_id).unwrap();

    txn.restore_savepoint(&savepoint).unwrap();
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = txn.open_table(definition).unwrap();
    assert_eq!(table.get(&0).unwrap().unwrap().value(), "hello");
}

#[test]
fn savepoint() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let definition: TableDefinition<u32, &str> = TableDefinition::new("x");

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        table.insert(&0, "hello").unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    let savepoint = txn.ephemeral_savepoint().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        table.remove(&0).unwrap();
    }
    txn.commit().unwrap();

    let mut txn = db.begin_write().unwrap();
    let savepoint2 = txn.ephemeral_savepoint().unwrap();

    txn.restore_savepoint(&savepoint).unwrap();

    assert!(matches!(
        txn.restore_savepoint(&savepoint2).err().unwrap(),
        SavepointError::InvalidSavepoint
    ));
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = txn.open_table(definition).unwrap();
    assert_eq!(table.get(&0).unwrap().unwrap().value(), "hello");

    // Test that savepoints can be used multiple times
    let mut txn = db.begin_write().unwrap();
    txn.restore_savepoint(&savepoint).unwrap();
    txn.commit().unwrap();
}

#[test]
fn compaction() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let definition: TableDefinition<u32, &[u8]> = TableDefinition::new("x");

    let big_value = vec![0u8; 100 * 1024];

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        // Insert 10MiB of data
        for i in 0..100 {
            table.insert(&i, big_value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        // Delete 90% of it
        for i in 0..90 {
            table.remove(&i).unwrap();
        }
    }
    txn.commit().unwrap();
    // Second commit to trigger dynamic compaction
    let txn = db.begin_write().unwrap();
    txn.commit().unwrap();

    // The values are > 1 page, so shouldn't get relocated. Therefore there should be a bunch of fragmented space,
    // since we left the last 100 values in the db.
    drop(db);
    let file_size = tmpfile.as_file().metadata().unwrap().len();
    let mut db = Database::open(tmpfile.path()).unwrap();

    assert!(db.compact().unwrap());
    drop(db);
    let file_size2 = tmpfile.as_file().metadata().unwrap().len();
    assert!(file_size2 < file_size);
}

fn require_send<T: Send>(_: &T) {}
fn require_sync<T: Sync + Send>(_: &T) {}

#[test]
fn is_send() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let definition: TableDefinition<u32, &[u8]> = TableDefinition::new("x");

    let txn = db.begin_write().unwrap();
    {
        let table = txn.open_table(definition).unwrap();
        require_send(&table);
        require_sync(&txn);
    }
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = txn.open_table(definition).unwrap();
    require_sync(&table);
    require_sync(&txn);
}

struct DelegatingTable<K: Key + 'static, V: Value + 'static, T: ReadableTable<K, V>> {
    inner: T,
    _key: PhantomData<K>,
    _value: PhantomData<V>,
}

impl<K: Key + 'static, V: Value + 'static, T: ReadableTable<K, V>> ReadableTable<K, V>
    for DelegatingTable<K, V, T>
{
    fn get<'a>(
        &self,
        key: impl Borrow<K::SelfType<'a>>,
    ) -> redb::Result<Option<AccessGuard<'_, V>>> {
        self.inner.get(key)
    }

    fn range<'a, KR>(&self, range: impl RangeBounds<KR> + 'a) -> redb::Result<Range<'_, K, V>>
    where
        KR: Borrow<K::SelfType<'a>> + 'a,
    {
        self.inner.range(range)
    }

    fn first(&self) -> redb::Result<Option<(AccessGuard<'_, K>, AccessGuard<'_, V>)>> {
        self.inner.first()
    }

    fn last(&self) -> redb::Result<Option<(AccessGuard<'_, K>, AccessGuard<'_, V>)>> {
        self.inner.last()
    }
}

impl<K: Key + 'static, V: Value + 'static, T: ReadableTable<K, V>> ReadableTableMetadata
    for DelegatingTable<K, V, T>
{
    fn stats(&self) -> redb::Result<TableStats> {
        self.inner.stats()
    }

    fn len(&self) -> redb::Result<u64> {
        self.inner.len()
    }
}

struct DelegatingMultimapTable<K: Key + 'static, V: Key + 'static, T: ReadableMultimapTable<K, V>> {
    inner: T,
    _key: PhantomData<K>,
    _value: PhantomData<V>,
}

impl<K: Key + 'static, V: Key + 'static, T: ReadableMultimapTable<K, V>> ReadableMultimapTable<K, V>
    for DelegatingMultimapTable<K, V, T>
{
    fn get<'a>(&self, key: impl Borrow<K::SelfType<'a>>) -> redb::Result<MultimapValue<'_, V>> {
        self.inner.get(key)
    }

    fn range<'a, KR>(
        &self,
        range: impl RangeBounds<KR> + 'a,
    ) -> redb::Result<MultimapRange<'_, K, V>>
    where
        KR: Borrow<K::SelfType<'a>> + 'a,
    {
        self.inner.range(range)
    }
}

impl<K: Key + 'static, V: Key + 'static, T: ReadableMultimapTable<K, V>> ReadableTableMetadata
    for DelegatingMultimapTable<K, V, T>
{
    fn stats(&self) -> redb::Result<TableStats> {
        self.inner.stats()
    }

    fn len(&self) -> redb::Result<u64> {
        self.inner.len()
    }
}

#[test]
fn custom_table_type() {
    let tmpfile = create_tempfile();
    let db = Database::create(tmpfile.path()).unwrap();
    let definition: TableDefinition<u32, &str> = TableDefinition::new("x");
    let definition_multimap: MultimapTableDefinition<u32, &str> =
        MultimapTableDefinition::new("multi");

    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(definition).unwrap();
        table.insert(0, "hello").unwrap();
        let mut table = txn.open_multimap_table(definition_multimap).unwrap();
        table.insert(1, "world").unwrap();
    }
    txn.commit().unwrap();

    let txn = db.begin_read().unwrap();
    let table = DelegatingTable {
        inner: txn.open_table(definition).unwrap(),
        _key: Default::default(),
        _value: Default::default(),
    };
    assert_eq!("hello", table.get(0).unwrap().unwrap().value());
    let table = DelegatingMultimapTable {
        inner: txn.open_multimap_table(definition_multimap).unwrap(),
        _key: Default::default(),
        _value: Default::default(),
    };
    assert_eq!(
        "world",
        table.get(1).unwrap().next().unwrap().unwrap().value()
    );

    let txn = db.begin_write().unwrap();
    let table = DelegatingTable {
        inner: txn.open_table(definition).unwrap(),
        _key: Default::default(),
        _value: Default::default(),
    };
    assert_eq!("hello", table.get(0).unwrap().unwrap().value());
    let table = DelegatingMultimapTable {
        inner: txn.open_multimap_table(definition_multimap).unwrap(),
        _key: Default::default(),
        _value: Default::default(),
    };
    assert_eq!(
        "world",
        table.get(1).unwrap().next().unwrap().unwrap().value()
    );
}
