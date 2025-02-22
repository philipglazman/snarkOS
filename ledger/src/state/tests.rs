// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    storage::{rocksdb::RocksDB, Storage},
    LedgerState,
};
use snarkvm::dpc::{prelude::*, testnet2::Testnet2};

use rand::thread_rng;
use std::sync::atomic::AtomicBool;

fn temp_dir() -> std::path::PathBuf {
    tempfile::tempdir().expect("Failed to open temporary directory").into_path()
}

/// Initializes a new instance of the ledger.
fn new_ledger<N: Network, S: Storage>() -> LedgerState<N> {
    LedgerState::open::<S, _>(temp_dir(), false).expect("Failed to initialize ledger")
}

#[test]
fn test_genesis() {
    // Initialize a new ledger.
    let ledger = new_ledger::<Testnet2, RocksDB>();

    // Retrieve the genesis block.
    let genesis = Testnet2::genesis_block();

    // Initialize a new ledger tree.
    let mut ledger_tree = LedgerTree::<Testnet2>::new().expect("Failed to initialize ledger tree");
    ledger_tree.add(&genesis.hash()).expect("Failed to add to ledger tree");

    // Ensure the ledger is at the genesis block.
    assert_eq!(0, ledger.latest_block_height());
    assert_eq!(genesis.height(), ledger.latest_block_height());
    assert_eq!(genesis.hash(), ledger.latest_block_hash());
    assert_eq!(genesis.timestamp(), ledger.latest_block_timestamp());
    assert_eq!(genesis.difficulty_target(), ledger.latest_block_difficulty_target());
    assert_eq!(genesis, &ledger.latest_block());
    assert_eq!(Some(&(genesis.hash(), None)), ledger.latest_block_locators().get(&genesis.height()));
    assert_eq!(ledger_tree.root(), ledger.latest_ledger_root());
}

#[test]
fn test_add_next_block() {
    let rng = &mut thread_rng();
    let terminator = AtomicBool::new(false);

    // Initialize a new ledger.
    let mut ledger = new_ledger::<Testnet2, RocksDB>();
    assert_eq!(0, ledger.latest_block_height());

    // Initialize a new ledger tree.
    let mut ledger_tree = LedgerTree::<Testnet2>::new().expect("Failed to initialize ledger tree");
    ledger_tree
        .add(&Testnet2::genesis_block().hash())
        .expect("Failed to add to ledger tree");

    // Initialize a new account.
    let account = Account::<Testnet2>::new(&mut thread_rng());
    let address = account.address();

    // Mine the next block.
    let block = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block).expect("Failed to add next block to ledger");
    ledger_tree.add(&block.hash()).expect("Failed to add hash to ledger tree");

    // Ensure the ledger is at block 1.
    assert_eq!(1, ledger.latest_block_height());
    assert_eq!(block.height(), ledger.latest_block_height());
    assert_eq!(block.hash(), ledger.latest_block_hash());
    assert_eq!(block.timestamp(), ledger.latest_block_timestamp());
    assert_eq!(block.difficulty_target(), ledger.latest_block_difficulty_target());
    assert_eq!(block, ledger.latest_block());
    assert_eq!(ledger_tree.root(), ledger.latest_ledger_root());

    // Retrieve the genesis block.
    let genesis = Testnet2::genesis_block();

    // Ensure the block locators are correct.
    let block_locators = ledger.latest_block_locators();
    assert_eq!(2, block_locators.len());
    assert_eq!(
        Some(&(block.hash(), Some(block.header().clone()))),
        block_locators.get(&block.height())
    );
    assert_eq!(Some(&(genesis.hash(), None)), block_locators.get(&genesis.height()));
}

#[test]
fn test_remove_last_block() {
    let rng = &mut thread_rng();
    let terminator = AtomicBool::new(false);

    // Initialize a new ledger.
    let mut ledger = new_ledger::<Testnet2, RocksDB>();
    assert_eq!(0, ledger.latest_block_height());

    // Initialize a new ledger tree.
    let mut ledger_tree = LedgerTree::<Testnet2>::new().expect("Failed to initialize ledger tree");
    ledger_tree
        .add(&Testnet2::genesis_block().hash())
        .expect("Failed to add to ledger tree");

    // Initialize a new account.
    let account = Account::<Testnet2>::new(&mut thread_rng());
    let address = account.address();

    // Mine the next block.
    let block = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block).expect("Failed to add next block to ledger");
    assert_eq!(1, ledger.latest_block_height());

    // Remove the last block.
    let blocks = ledger
        .revert_to_block_height(ledger.latest_block_height() - 1)
        .expect("Failed to remove the last block");
    assert_eq!(vec![block], blocks);

    // Retrieve the genesis block.
    let genesis = Testnet2::genesis_block();

    // Ensure the ledger is back at the genesis block.
    assert_eq!(0, ledger.latest_block_height());
    assert_eq!(genesis.height(), ledger.latest_block_height());
    assert_eq!(genesis.hash(), ledger.latest_block_hash());
    assert_eq!(genesis.timestamp(), ledger.latest_block_timestamp());
    assert_eq!(genesis.difficulty_target(), ledger.latest_block_difficulty_target());
    assert_eq!(genesis, &ledger.latest_block());
    assert_eq!(Some(&(genesis.hash(), None)), ledger.latest_block_locators().get(&genesis.height()));
    assert_eq!(ledger_tree.root(), ledger.latest_ledger_root());
}

#[test]
fn test_remove_last_2_blocks() {
    let rng = &mut thread_rng();
    let terminator = AtomicBool::new(false);

    // Initialize a new ledger.
    let mut ledger = new_ledger::<Testnet2, RocksDB>();
    assert_eq!(0, ledger.latest_block_height());

    // Initialize a new ledger tree.
    let mut ledger_tree = LedgerTree::<Testnet2>::new().expect("Failed to initialize ledger tree");
    ledger_tree
        .add(&Testnet2::genesis_block().hash())
        .expect("Failed to add to ledger tree");

    // Initialize a new account.
    let account = Account::<Testnet2>::new(&mut thread_rng());
    let address = account.address();

    // Mine the next block.
    let block_1 = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block_1).expect("Failed to add next block to ledger");
    assert_eq!(1, ledger.latest_block_height());

    // Mine the next block.
    let block_2 = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block_2).expect("Failed to add next block to ledger");
    assert_eq!(2, ledger.latest_block_height());

    // Remove the last block.
    let blocks = ledger
        .revert_to_block_height(ledger.latest_block_height() - 2)
        .expect("Failed to remove the last two blocks");
    assert_eq!(vec![block_1, block_2], blocks);

    // Retrieve the genesis block.
    let genesis = Testnet2::genesis_block();

    // Ensure the ledger is back at the genesis block.
    assert_eq!(0, ledger.latest_block_height());
    assert_eq!(genesis.height(), ledger.latest_block_height());
    assert_eq!(genesis.hash(), ledger.latest_block_hash());
    assert_eq!(genesis.timestamp(), ledger.latest_block_timestamp());
    assert_eq!(genesis.difficulty_target(), ledger.latest_block_difficulty_target());
    assert_eq!(genesis, &ledger.latest_block());
    assert_eq!(Some(&(genesis.hash(), None)), ledger.latest_block_locators().get(&genesis.height()));
    assert_eq!(ledger_tree.root(), ledger.latest_ledger_root());
}

#[test]
fn test_get_block_locators() {
    let rng = &mut thread_rng();
    let terminator = AtomicBool::new(false);

    // Initialize a new ledger.
    let mut ledger = new_ledger::<Testnet2, RocksDB>();
    assert_eq!(0, ledger.latest_block_height());

    // Initialize a new ledger tree.
    let mut ledger_tree = LedgerTree::<Testnet2>::new().expect("Failed to initialize ledger tree");
    ledger_tree
        .add(&Testnet2::genesis_block().hash())
        .expect("Failed to add to ledger tree");

    // Initialize a new account.
    let account = Account::<Testnet2>::new(&mut thread_rng());
    let address = account.address();

    // Mine the next block.
    let block_1 = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block_1).expect("Failed to add next block to ledger");
    assert_eq!(1, ledger.latest_block_height());

    // Check the block locators.
    let block_locators = ledger
        .get_block_locators(ledger.latest_block_height())
        .expect("Failed to get block locators");
    assert!(
        ledger
            .check_block_locators(&block_locators)
            .expect("Failed to check block locators")
    );

    // Mine the next block.
    let block_2 = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block_2).expect("Failed to add next block to ledger");
    assert_eq!(2, ledger.latest_block_height());

    // Check the block locators.
    let block_locators = ledger
        .get_block_locators(ledger.latest_block_height())
        .expect("Failed to get block locators");
    assert!(
        ledger
            .check_block_locators(&block_locators)
            .expect("Failed to check block locators")
    );

    // Mine the next block.
    let block_3 = ledger.mine_next_block(address, &[], &terminator, rng).expect("Failed to mine");
    ledger.add_next_block(&block_3).expect("Failed to add next block to ledger");
    assert_eq!(3, ledger.latest_block_height());

    // Check the block locators.
    let block_locators = ledger
        .get_block_locators(ledger.latest_block_height())
        .expect("Failed to get block locators");
    assert!(
        ledger
            .check_block_locators(&block_locators)
            .expect("Failed to check block locators")
    );
}
