use std::sync::mpsc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use rocksdb::{Options, DB};
use sha2::{Digest, Sha256};
use tempdir::TempDir;

use lisk_db::batch::PrefixWriteBatch;
use lisk_db::consts;
use lisk_db::database::types::{DbMessage, Kind};
use lisk_db::database::DB as LDB;
use lisk_db::sparse_merkle_tree::smt::{SparseMerkleTree, UpdateData};
use lisk_db::sparse_merkle_tree::smt_db;
use lisk_db::types::{Cache, KeyLength, NestedVec, SharedKVPair};

const DATA_LEN: usize = 100_000;
const QUERY_LEN: usize = 1_000;

fn get_data() -> UpdateData {
    let mut data = UpdateData::new_from(Cache::new());

    for i in 0..DATA_LEN {
        let mut hasher = Sha256::new();
        hasher.update(i.to_string());
        let hash = hasher.finalize();
        let hash: String = format!("{hash:X}");

        data.insert(SharedKVPair(
            &hex::decode(&hash).unwrap(),
            &hex::decode(&hash).unwrap(),
        ));
    }

    data
}

fn get_query_keys() -> NestedVec {
    let mut query_keys: NestedVec = vec![];

    for i in 0..QUERY_LEN {
        let mut hasher = Sha256::new();
        hasher.update(i.to_string());
        let hash = hasher.finalize();
        let hash: String = format!("{hash:X}");

        query_keys.push(hash.as_bytes().to_vec());
    }

    query_keys
}

fn test_smt(data: &UpdateData, query_keys: &[Vec<u8>]) {
    let mut tree = SparseMerkleTree::new(&[], KeyLength(32), Default::default());

    let temp_dir = TempDir::new("bench_db_").unwrap();
    let mut opts = Options::default();
    opts.create_if_missing(true);
    let rocks_db = DB::open(&opts, temp_dir.path()).unwrap();
    let (tx, _) = mpsc::channel::<DbMessage>();
    let common_db = LDB::new(rocks_db, tx, Kind::Normal);
    let mut db = smt_db::SmtDB::new(&common_db);

    let root = tree.commit(&mut db, data).unwrap();

    // write batch to RocksDB database
    let mut write_batch = PrefixWriteBatch::new();
    write_batch.set_prefix(&consts::Prefix::SMT);
    db.batch.iterate(&mut write_batch);
    common_db.write(write_batch.batch).unwrap();

    let proof = tree
        .prove(
            &mut db,
            &query_keys
                .iter()
                .map(|k| hex::decode(k).unwrap())
                .collect::<NestedVec>(),
        )
        .unwrap();

    let is_valid = SparseMerkleTree::verify(
        &query_keys
            .iter()
            .map(|k| hex::decode(k).unwrap())
            .collect::<NestedVec>(),
        &proof,
        &root.lock().unwrap(),
        KeyLength(32),
    )
    .unwrap();

    assert!(is_valid);
}

fn criterion_benchmark(c: &mut Criterion) {
    let data = get_data();
    let query_keys = get_query_keys();

    c.bench_function("sparse merkle tree", |b| {
        b.iter(|| test_smt(&data, &query_keys))
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50).measurement_time(Duration::from_secs(40));
    targets = criterion_benchmark
}
criterion_main!(benches);
