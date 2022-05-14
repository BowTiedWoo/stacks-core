use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::thread;

use stacks::burnchains::Burnchain;
use stacks::chainstate::stacks::db::StacksChainState;
use stacks::chainstate::stacks::StacksBlockHeader;
use stacks::types::chainstate::StacksAddress;
use stacks::vm::types::PrincipalData;

use crate::config::Config;
use crate::config::EventKeyType;
use crate::config::EventObserverConfig;
use crate::config::InitialBalance;
use crate::neon;
use crate::neon::RunLoopCounter;
use crate::tests::bitcoin_regtest::BitcoinCoreController;
use crate::tests::neon_integrations::*;
use crate::tests::*;
use crate::BitcoinRegtestController;
use crate::BurnchainController;
use crate::Keychain;

use stacks::core;

use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::burn::distribution::BurnSamplePoint;
use stacks::chainstate::burn::operations::leader_block_commit::BURN_BLOCK_MINED_AT_MODULUS;
use stacks::chainstate::burn::operations::BlockstackOperationType;
use stacks::chainstate::burn::operations::LeaderBlockCommitOp;

use stacks::burnchains::PoxConstants;
use stacks::burnchains::Txid;

use crate::stacks_common::types::chainstate::BlockHeaderHash;
use crate::stacks_common::types::chainstate::VRFSeed;
use crate::stacks_common::types::Address;
use crate::stacks_common::util::hash::bytes_to_hex;
use crate::stacks_common::util::hash::hex_bytes;

use stacks_common::types::chainstate::BurnchainHeaderHash;
use stacks_common::util::hash::Hash160;
use stacks_common::util::secp256k1::Secp256k1PublicKey;

use stacks::chainstate::coordinator::comm::CoordinatorChannels;

use stacks::core::STACKS_EPOCH_2_1_MARKER;

use clarity::vm::ClarityVersion;
use stacks::clarity_cli::vm_execute as execute;

fn advance_to_2_1(
    mut initial_balances: Vec<InitialBalance>,
) -> (
    Config,
    BitcoinCoreController,
    BitcoinRegtestController,
    RunLoopCounter,
    CoordinatorChannels,
) {
    let epoch_2_05 = 210;
    let epoch_2_1 = 215;

    test_observer::spawn();

    let (mut conf, miner_account) = neon_integration_test_conf();

    conf.initial_balances.append(&mut initial_balances);
    conf.events_observers.push(EventObserverConfig {
        endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut epochs = core::STACKS_EPOCHS_REGTEST.to_vec();
    epochs[1].end_height = epoch_2_05;
    epochs[2].start_height = epoch_2_05;
    epochs[2].end_height = epoch_2_1;
    epochs[3].start_height = epoch_2_1;

    conf.burnchain.epochs = Some(epochs);

    let mut burnchain_config = Burnchain::regtest(&conf.get_burn_db_path());

    let reward_cycle_len = 2000;
    let prepare_phase_len = 100;
    let pox_constants = PoxConstants::new(
        reward_cycle_len,
        prepare_phase_len,
        4 * prepare_phase_len / 5,
        5,
        15,
        u32::max_value(),
    );
    burnchain_config.pox_constants = pox_constants.clone();

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller
        .start_bitcoind()
        .map_err(|_e| ())
        .expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::with_burnchain(
        conf.clone(),
        None,
        Some(burnchain_config.clone()),
        None,
    );
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    // give one coinbase to the miner, and then burn the rest
    btc_regtest_controller.bootstrap_chain(1);

    let mining_pubkey = btc_regtest_controller.get_mining_pubkey().unwrap();
    btc_regtest_controller.set_mining_pubkey(
        "03dc62fe0b8964d01fc9ca9a5eec0e22e557a12cc656919e648f04e0b26fea5faa".to_string(),
    );

    // bitcoin chain starts at epoch 2.05 boundary, minus 5 blocks to go
    btc_regtest_controller.bootstrap_chain(epoch_2_05 - 6);

    // only one UTXO for our mining pubkey
    let utxos = btc_regtest_controller
        .get_all_utxos(&Secp256k1PublicKey::from_hex(&mining_pubkey).unwrap());
    assert_eq!(utxos.len(), 1);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf.clone());
    let blocks_processed = run_loop.get_blocks_processed_arc();

    let channel = run_loop.get_coordinator_channel().unwrap();

    let runloop_burnchain = burnchain_config.clone();
    thread::spawn(move || run_loop.start(Some(runloop_burnchain), 0));

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tip_info = get_chain_info(&conf);
    assert_eq!(tip_info.burn_block_height, epoch_2_05 - 4);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // cross the epoch 2.05 boundary
    for _i in 0..3 {
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    let tip_info = get_chain_info(&conf);
    assert_eq!(tip_info.burn_block_height, epoch_2_05 + 1);

    // these should all succeed across the epoch 2.1 boundary
    for _i in 0..5 {
        let tip_info = get_chain_info(&conf);

        // this block is the epoch transition?
        let (chainstate, _) = StacksChainState::open(
            false,
            conf.burnchain.chain_id,
            &conf.get_chainstate_path_str(),
            None,
        )
        .unwrap();
        let res = StacksChainState::block_crosses_epoch_boundary(
            &chainstate.db(),
            &tip_info.stacks_tip_consensus_hash,
            &tip_info.stacks_tip,
        )
        .unwrap();
        debug!(
            "Epoch transition at {} ({}/{}) height {}: {}",
            &StacksBlockHeader::make_index_block_hash(
                &tip_info.stacks_tip_consensus_hash,
                &tip_info.stacks_tip
            ),
            &tip_info.stacks_tip_consensus_hash,
            &tip_info.stacks_tip,
            tip_info.burn_block_height,
            res
        );

        if tip_info.burn_block_height >= epoch_2_1 {
            if tip_info.burn_block_height == epoch_2_1 {
                assert!(res);
            }

            // pox-2 should be initialized now
            let _ = get_contract_src(
                &http_origin,
                StacksAddress::from_string("ST000000000000000000002AMW42H").unwrap(),
                "pox-2".to_string(),
                true,
            )
            .unwrap();
        } else {
            assert!(!res);

            // pox-2 should NOT be initialized
            let e = get_contract_src(
                &http_origin,
                StacksAddress::from_string("ST000000000000000000002AMW42H").unwrap(),
                "pox-2".to_string(),
                true,
            )
            .unwrap_err();
            eprintln!("No pox-2: {}", &e);
        }

        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    let tip_info = get_chain_info(&conf);
    assert_eq!(tip_info.burn_block_height, epoch_2_1 + 1);

    let account = get_account(&http_origin, &miner_account);
    assert_eq!(account.nonce, 9);

    eprintln!("Begin Stacks 2.1");
    return (
        conf,
        btcd_controller,
        btc_regtest_controller,
        blocks_processed,
        channel,
    );
}

#[test]
#[ignore]
fn transition_fixes_utxo_chaining() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    // very simple test to verify that the miner will keep making valid (empty) blocks after the
    // transition.  Really tests that the block-commits are well-formed before and after the epoch
    // transition.
    let (conf, _btcd_controller, mut btc_regtest_controller, blocks_processed, coord_channel) =
        advance_to_2_1(vec![]);

    // post epoch 2.1 -- UTXO chaining should be fixed
    for i in 0..10 {
        let tip_info = get_chain_info(&conf);

        if i % 2 == 1 {
            std::env::set_var(
                "STX_TEST_LATE_BLOCK_COMMIT",
                format!("{}", tip_info.burn_block_height + 1),
            );
        }

        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    let sortdb = btc_regtest_controller.sortdb_mut();
    let tip = SortitionDB::get_canonical_burn_chain_tip(sortdb.conn()).unwrap();
    let burn_sample: Vec<BurnSamplePoint> = sortdb
        .conn()
        .query_row(
            "SELECT data FROM snapshot_burn_distributions WHERE sortition_id = ?",
            &[tip.sortition_id],
            |row| {
                let data_str: String = row.get_unwrap(0);
                Ok(serde_json::from_str(&data_str).unwrap())
            },
        )
        .unwrap();

    // if UTXO linking is fixed, then our median burn will be int((20,000 + 1) / 2).
    // Otherwise, it will be 1.
    assert_eq!(burn_sample.len(), 1);
    assert_eq!(burn_sample[0].burns, 10_000);

    test_observer::clear();
    coord_channel.stop_chains_coordinator();
}

#[test]
#[ignore]
fn transition_adds_burn_block_height() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    // very simple test to verify that after the 2.1 transition, get-burn-block-info? works as
    // expected

    let spender_sk = StacksPrivateKey::new();
    let spender_addr = PrincipalData::from(to_addr(&spender_sk));
    let spender_addr_c32 = StacksAddress::from(to_addr(&spender_sk));

    let (conf, _btcd_controller, mut btc_regtest_controller, blocks_processed, coord_channel) =
        advance_to_2_1(vec![InitialBalance {
            address: spender_addr.clone(),
            amount: 200_000_000,
        }]);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    // post epoch 2.1 -- we should be able to query any/all burnchain headers after the first
    // burnchain block height
    let contract = "
    (define-private (test-burn-headers-cls (height uint) (base uint))
        (begin
            (print { height: (+ base height), hash: (get-burn-block-info? header-hash (+ base height)) })
            base
        )
    )
    (define-public (test-burn-headers)
        (begin
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u0
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u25
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u50
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u75
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u100
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u125
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u150
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u175
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u200
            )
            (fold test-burn-headers-cls
                (list u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24)
                u225
            )
            (ok u0)
        )
    )
    ";

    let tx = make_contract_publish(
        &spender_sk,
        0,
        (2 * contract.len()) as u64,
        "test-burn-headers",
        contract,
    );
    submit_tx(&http_origin, &tx);

    // mine it
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tx = make_contract_call(
        &spender_sk,
        1,
        (2 * contract.len()) as u64,
        &spender_addr_c32,
        "test-burn-headers",
        "test-burn-headers",
        &[],
    );
    submit_tx(&http_origin, &tx);

    let cc_txid = StacksTransaction::consensus_deserialize(&mut &tx[..])
        .unwrap()
        .txid();

    // mine it
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // check it
    let mut header_hashes: HashMap<u64, Option<BurnchainHeaderHash>> = HashMap::new();
    let blocks = test_observer::get_blocks();
    for block in blocks {
        let transactions = block.get("transactions").unwrap().as_array().unwrap();
        let events = block.get("events").unwrap().as_array().unwrap();

        for tx in transactions.iter() {
            let raw_tx = tx.get("raw_tx").unwrap().as_str().unwrap();
            if raw_tx == "0x00" {
                continue;
            }
            let tx_bytes = hex_bytes(&raw_tx[2..]).unwrap();
            let parsed = StacksTransaction::consensus_deserialize(&mut &tx_bytes[..]).unwrap();
            if parsed.txid() == cc_txid {
                // check events for this block
                for event in events.iter() {
                    if let Some(cev) = event.get("contract_event") {
                        // strip leading `0x`
                        eprintln!("{:#?}", &cev);
                        let clarity_serialized_value = hex_bytes(
                            &String::from_utf8(
                                cev.get("raw_value").unwrap().as_str().unwrap().as_bytes()[2..]
                                    .to_vec(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                        let clarity_value =
                            Value::deserialize_read(&mut &clarity_serialized_value[..], None)
                                .unwrap();
                        let pair = clarity_value.expect_tuple();
                        let height = pair.get("height").unwrap().clone().expect_u128() as u64;
                        let bhh_opt =
                            pair.get("hash")
                                .unwrap()
                                .clone()
                                .expect_optional()
                                .map(|inner_buff| {
                                    let buff_bytes_vec = inner_buff.expect_buff(32);
                                    let mut buff_bytes = [0u8; 32];
                                    buff_bytes.copy_from_slice(&buff_bytes_vec[0..32]);
                                    BurnchainHeaderHash(buff_bytes)
                                });

                        header_hashes.insert(height, bhh_opt);
                    }
                }
            }
        }
    }

    let sortdb = btc_regtest_controller.sortdb_mut();
    let all_snapshots = sortdb.get_all_snapshots().unwrap();
    let tip = SortitionDB::get_canonical_burn_chain_tip(sortdb.conn()).unwrap();

    // all block headers are accounted for
    for sn in all_snapshots.iter() {
        match header_hashes.get(&sn.block_height) {
            Some(Some(bhh)) => {
                assert_eq!(&sn.burn_header_hash, bhh);

                // only defined up to the tip
                assert!(sn.block_height < tip.block_height);
            }
            Some(None) => {
                // must exceed the tip
                assert!(sn.block_height >= tip.block_height);
            }
            None => {
                panic!("Missing header for {}", sn.block_height);
            }
        }
    }

    test_observer::clear();
    coord_channel.stop_chains_coordinator();
}

#[test]
#[ignore]
fn transition_caps_discount_mining_upside() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return;
    }

    let epoch_2_05 = 210;
    let epoch_2_1 = 215;

    let spender_sk = StacksPrivateKey::new();
    let spender_addr = PrincipalData::from(to_addr(&spender_sk));
    let spender_addr_c32 = StacksAddress::from(to_addr(&spender_sk));

    let pox_pubkey = Secp256k1PublicKey::from_hex(
        "02f006a09b59979e2cb8449f58076152af6b124aa29b948a3714b8d5f15aa94ede",
    )
    .unwrap();
    let pox_pubkey_hash160 = Hash160::from_node_public_key(&pox_pubkey);
    let pox_pubkey_hash = bytes_to_hex(&pox_pubkey_hash160.to_bytes().to_vec());

    test_observer::spawn();

    let (mut conf, miner_account) = neon_integration_test_conf();
    let keychain = Keychain::default(conf.node.seed.clone());

    conf.initial_balances = vec![InitialBalance {
        address: spender_addr.clone(),
        amount: 200_000_000_000_000,
    }];
    conf.events_observers.push(EventObserverConfig {
        endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
        events_keys: vec![EventKeyType::AnyEvent],
    });

    let mut epochs = core::STACKS_EPOCHS_REGTEST.to_vec();
    epochs[1].end_height = epoch_2_05;
    epochs[2].start_height = epoch_2_05;
    epochs[2].end_height = epoch_2_1;
    epochs[3].start_height = epoch_2_1;

    conf.burnchain.epochs = Some(epochs);

    let mut burnchain_config = Burnchain::regtest(&conf.get_burn_db_path());

    let reward_cycle_len = 20;
    let prepare_phase_len = 10;
    let pox_constants = PoxConstants::new(
        reward_cycle_len,
        prepare_phase_len,
        4 * prepare_phase_len / 5,
        5,
        1,
        230,
    );
    burnchain_config.pox_constants = pox_constants.clone();

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller
        .start_bitcoind()
        .map_err(|_e| ())
        .expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::with_burnchain(
        conf.clone(),
        None,
        Some(burnchain_config.clone()),
        None,
    );
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    // give one coinbase to the miner, and then burn the rest
    btc_regtest_controller.bootstrap_chain(1);

    let mining_pubkey = btc_regtest_controller.get_mining_pubkey().unwrap();
    btc_regtest_controller.set_mining_pubkey(
        "03dc62fe0b8964d01fc9ca9a5eec0e22e557a12cc656919e648f04e0b26fea5faa".to_string(),
    );

    // bitcoin chain starts at epoch 2.05 boundary, minus 5 blocks to go
    btc_regtest_controller.bootstrap_chain(epoch_2_05 - 6);

    // only one UTXO for our mining pubkey
    let utxos = btc_regtest_controller
        .get_all_utxos(&Secp256k1PublicKey::from_hex(&mining_pubkey).unwrap());
    assert_eq!(utxos.len(), 1);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf.clone());
    let blocks_processed = run_loop.get_blocks_processed_arc();

    let channel = run_loop.get_coordinator_channel().unwrap();

    let runloop_burnchain = burnchain_config.clone();
    thread::spawn(move || run_loop.start(Some(runloop_burnchain), 0));

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    let tip_info = get_chain_info(&conf);
    assert_eq!(tip_info.burn_block_height, epoch_2_05 - 4);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    let key_block_ptr = tip_info.burn_block_height as u32;
    let key_vtxindex = 1; // nothing else here but the coinbase

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // cross the epoch 2.05 boundary
    for _i in 0..3 {
        debug!("Burnchain block height is {}", tip_info.burn_block_height);
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    // cross the epoch 2.1 boundary
    for _i in 0..5 {
        debug!("Burnchain block height is {}", tip_info.burn_block_height);
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    // wait until the next reward cycle starts
    let mut tip_info = get_chain_info(&conf);
    while tip_info.burn_block_height < 221 {
        debug!("Burnchain block height is {}", tip_info.burn_block_height);
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
        tip_info = get_chain_info(&conf);
    }

    // stack all the tokens
    let tx = make_contract_call(
        &spender_sk,
        0,
        300,
        &StacksAddress::from_string("ST000000000000000000002AMW42H").unwrap(),
        "pox-2",
        "stack-stx",
        &[
            Value::UInt(200_000_000_000_000 - 1_000),
            execute(
                &format!("{{ hashbytes: 0x{}, version: 0x00 }}", pox_pubkey_hash),
                ClarityVersion::Clarity2,
            )
            .unwrap()
            .unwrap(),
            Value::UInt((tip_info.burn_block_height + 3).into()),
            Value::UInt(12),
        ],
    );

    submit_tx(&http_origin, &tx);

    // wait till next reward phase
    while tip_info.burn_block_height < 241 {
        debug!("Burnchain block height is {}", tip_info.burn_block_height);
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
        tip_info = get_chain_info(&conf);
    }

    // verify stacking worked
    let pox_info = get_pox_info(&http_origin);

    assert_eq!(
        pox_info.current_cycle.stacked_ustx,
        200_000_000_000_000 - 1_000
    );
    assert_eq!(pox_info.current_cycle.is_pox_active, true);

    // try to send a PoX payout that over-spends
    assert!(!burnchain_config.is_in_prepare_phase(tip_info.burn_block_height + 1));

    let mut bitcoin_controller = BitcoinRegtestController::new_dummy(conf.clone());

    // allow using 0-conf utxos
    bitcoin_controller.set_allow_rbf(false);
    let mut bad_commits = HashSet::new();

    for i in 0..10 {
        let burn_fee_cap = 100000000; // 1 BTC
        let commit_outs = vec![
            StacksAddress {
                version: C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
                bytes: pox_pubkey_hash160.clone(),
            },
            StacksAddress {
                version: C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
                bytes: pox_pubkey_hash160.clone(),
            },
        ];

        // let's commit an oversized commit
        let burn_parent_modulus = (tip_info.burn_block_height % BURN_BLOCK_MINED_AT_MODULUS) as u8;
        let block_header_hash = BlockHeaderHash([0xff - (i as u8); 32]);

        let op = BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
            block_header_hash: block_header_hash.clone(),
            burn_fee: burn_fee_cap,
            destroyed: 0,
            input: (Txid([0; 32]), 0),
            apparent_sender: keychain.get_burnchain_signer(),
            key_block_ptr,
            key_vtxindex,
            memo: vec![STACKS_EPOCH_2_1_MARKER],
            new_seed: VRFSeed([0x11; 32]),
            parent_block_ptr: tip_info.burn_block_height.try_into().unwrap(),
            parent_vtxindex: 1,
            // to be filled in
            vtxindex: 0,
            txid: Txid([0u8; 32]),
            block_height: 0,
            burn_header_hash: BurnchainHeaderHash::zero(),
            burn_parent_modulus,
            commit_outs,
        });

        let mut op_signer = keychain.generate_op_signer();
        let res = bitcoin_controller.submit_operation(op, &mut op_signer, 1);
        assert!(res, "Failed to submit block-commit");

        bad_commits.insert(block_header_hash);
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    // commit not even accepted
    // (NOTE: there's no directly way to confirm here that it was rejected due to
    // BlockCommitPoxOverpay)

    let sortdb = btc_regtest_controller.sortdb_mut();
    let all_snapshots = sortdb.get_all_snapshots().unwrap();

    for i in (all_snapshots.len() - 2)..all_snapshots.len() {
        let commits =
            SortitionDB::get_block_commits_by_block(sortdb.conn(), &all_snapshots[i].sortition_id)
                .unwrap();
        assert_eq!(commits.len(), 1);
        assert!(!bad_commits.contains(&commits[0].block_header_hash));
    }

    test_observer::clear();
    channel.stop_chains_coordinator();
}
