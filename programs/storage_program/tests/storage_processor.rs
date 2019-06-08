use assert_matches::assert_matches;
use bincode::deserialize;
use log::*;
use solana_runtime::bank::Bank;
use solana_runtime::bank_client::BankClient;
use solana_sdk::account::{create_keyed_accounts, Account, KeyedAccount};
use solana_sdk::account_utils::State;
use solana_sdk::client::SyncClient;
use solana_sdk::genesis_block::create_genesis_block;
use solana_sdk::hash::{hash, Hash};
use solana_sdk::instruction::{Instruction, InstructionError};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil, Signature};
use solana_sdk::syscall::tick_height;
use solana_sdk::syscall::tick_height::TickHeight;
use solana_sdk::timing::DEFAULT_TICKS_PER_SLOT;
use solana_storage_api::storage_contract::StorageAccount;
use solana_storage_api::storage_contract::{
    CheckedProof, Proof, ProofStatus, StorageContract, STORAGE_ACCOUNT_SPACE,
    TOTAL_REPLICATOR_REWARDS, TOTAL_VALIDATOR_REWARDS,
};
use solana_storage_api::storage_instruction;
use solana_storage_api::storage_processor::process_instruction;
use solana_storage_api::SLOTS_PER_SEGMENT;
use solana_storage_api::{get_segment_from_slot, id};
use std::collections::HashMap;
use std::sync::Arc;

const TICKS_IN_SEGMENT: u64 = SLOTS_PER_SEGMENT * DEFAULT_TICKS_PER_SLOT;

fn test_instruction(
    ix: &Instruction,
    program_accounts: &mut [Account],
) -> Result<(), InstructionError> {
    let mut keyed_accounts: Vec<_> = ix
        .accounts
        .iter()
        .zip(program_accounts.iter_mut())
        .map(|(account_meta, account)| {
            KeyedAccount::new(&account_meta.pubkey, account_meta.is_signer, account)
        })
        .collect();

    let ret = process_instruction(&id(), &mut keyed_accounts, &ix.data);
    info!("ret: {:?}", ret);
    ret
}

#[test]
fn test_account_owner() {
    let account_owner = Pubkey::new_rand();
    let validator_storage_pubkey = Pubkey::new_rand();
    let replicator_storage_pubkey = Pubkey::new_rand();

    let (genesis_block, mint_keypair) = create_genesis_block(1000);
    let mut bank = Bank::new(&genesis_block);
    let mint_pubkey = mint_keypair.pubkey();
    bank.add_instruction_processor(id(), process_instruction);
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    let message = Message::new(storage_instruction::create_validator_storage_account(
        &mint_pubkey,
        &account_owner,
        &validator_storage_pubkey,
        1,
    ));
    bank_client
        .send_message(&[&mint_keypair], message)
        .expect("failed to create account");
    let account = bank
        .get_account(&validator_storage_pubkey)
        .expect("account not found");
    let storage_contract = account.state().expect("couldn't unpack account data");
    if let StorageContract::ValidatorStorage { owner, .. } = storage_contract {
        assert_eq!(owner, account_owner);
    } else {
        assert!(false, "wrong account type found")
    }

    let message = Message::new(storage_instruction::create_replicator_storage_account(
        &mint_pubkey,
        &account_owner,
        &replicator_storage_pubkey,
        1,
    ));
    bank_client
        .send_message(&[&mint_keypair], message)
        .expect("failed to create account");
    let account = bank
        .get_account(&replicator_storage_pubkey)
        .expect("account not found");
    let storage_contract = account.state().expect("couldn't unpack account data");
    if let StorageContract::ReplicatorStorage { owner, .. } = storage_contract {
        assert_eq!(owner, account_owner);
    } else {
        assert!(false, "wrong account type found")
    }
}

#[test]
fn test_proof_bounds() {
    let account_owner = Pubkey::new_rand();
    let pubkey = Pubkey::new_rand();
    let mut account = Account {
        data: vec![0; STORAGE_ACCOUNT_SPACE as usize],
        ..Account::default()
    };
    {
        let mut storage_account = StorageAccount::new(&mut account);
        storage_account
            .initialize_replicator_storage(account_owner)
            .unwrap();
    }

    let ix = storage_instruction::mining_proof(
        &pubkey,
        Hash::default(),
        SLOTS_PER_SEGMENT,
        Signature::default(),
    );
    // the proof is for slot 16, which is in segment 0, need to move the tick height into segment 2
    let ticks_till_next_segment = TICKS_IN_SEGMENT * 2;
    let mut tick_account = tick_height::create_account(1);
    TickHeight::to(ticks_till_next_segment, &mut tick_account);

    assert_eq!(test_instruction(&ix, &mut [account, tick_account]), Ok(()));
}

#[test]
fn test_storage_tx() {
    let pubkey = Pubkey::new_rand();
    let mut accounts = [(pubkey, Account::default())];
    let mut keyed_accounts = create_keyed_accounts(&mut accounts);
    assert!(process_instruction(&id(), &mut keyed_accounts, &[]).is_err());
}

#[test]
fn test_serialize_overflow() {
    let pubkey = Pubkey::new_rand();
    let tick_pubkey = Pubkey::new_rand();
    let mut keyed_accounts = Vec::new();
    let mut user_account = Account::default();
    let mut tick_account = tick_height::create_account(1);
    keyed_accounts.push(KeyedAccount::new(&pubkey, true, &mut user_account));
    keyed_accounts.push(KeyedAccount::new(&tick_pubkey, false, &mut tick_account));

    let ix = storage_instruction::advertise_recent_blockhash(
        &pubkey,
        Hash::default(),
        SLOTS_PER_SEGMENT,
    );

    assert_eq!(
        process_instruction(&id(), &mut keyed_accounts, &ix.data),
        Err(InstructionError::InvalidAccountData)
    );
}

#[test]
fn test_invalid_accounts_len() {
    let pubkey = Pubkey::new_rand();
    let mut accounts = [Account::default()];

    let ix = storage_instruction::mining_proof(&pubkey, Hash::default(), 0, Signature::default());
    // move tick height into segment 1
    let ticks_till_next_segment = TICKS_IN_SEGMENT + 1;
    let mut tick_account = tick_height::create_account(1);
    TickHeight::to(ticks_till_next_segment, &mut tick_account);

    assert!(test_instruction(&ix, &mut accounts).is_err());

    let mut accounts = [Account::default(), tick_account, Account::default()];

    assert!(test_instruction(&ix, &mut accounts).is_err());
}

#[test]
fn test_submit_mining_invalid_slot() {
    solana_logger::setup();
    let pubkey = Pubkey::new_rand();
    let mut accounts = [Account::default(), Account::default()];
    accounts[0].data.resize(STORAGE_ACCOUNT_SPACE as usize, 0);
    accounts[1].data.resize(STORAGE_ACCOUNT_SPACE as usize, 0);

    let ix = storage_instruction::mining_proof(&pubkey, Hash::default(), 0, Signature::default());

    // submitting a proof for a slot in the past, so this should fail
    assert!(test_instruction(&ix, &mut accounts).is_err());
}

#[test]
fn test_submit_mining_ok() {
    solana_logger::setup();
    let account_owner = Pubkey::new_rand();
    let pubkey = Pubkey::new_rand();
    let mut account = Account::default();
    account.data.resize(STORAGE_ACCOUNT_SPACE as usize, 0);
    {
        let mut storage_account = StorageAccount::new(&mut account);
        storage_account
            .initialize_replicator_storage(account_owner)
            .unwrap();
    }

    let ix = storage_instruction::mining_proof(&pubkey, Hash::default(), 0, Signature::default());
    // move tick height into segment 1
    let ticks_till_next_segment = TICKS_IN_SEGMENT + 1;
    let mut tick_account = tick_height::create_account(1);
    TickHeight::to(ticks_till_next_segment, &mut tick_account);

    assert_matches!(test_instruction(&ix, &mut [account, tick_account]), Ok(_));
}

#[test]
fn test_validate_mining() {
    solana_logger::setup();
    let (genesis_block, mint_keypair) = create_genesis_block(1000);
    let mint_pubkey = mint_keypair.pubkey();

    let replicator_1_storage_keypair = Keypair::new();
    let replicator_1_storage_id = replicator_1_storage_keypair.pubkey();

    let replicator_2_storage_keypair = Keypair::new();
    let replicator_2_storage_id = replicator_2_storage_keypair.pubkey();

    let validator_storage_keypair = Keypair::new();
    let validator_storage_id = validator_storage_keypair.pubkey();

    let mining_pool_keypair = Keypair::new();
    let mining_pool_pubkey = mining_pool_keypair.pubkey();

    let mut bank = Bank::new(&genesis_block);
    bank.add_instruction_processor(id(), process_instruction);
    let bank = Arc::new(bank);
    let slot = 0;
    let bank_client = BankClient::new_shared(&bank);

    init_storage_accounts(
        &bank_client,
        &mint_keypair,
        &[&validator_storage_id],
        &[&replicator_1_storage_id, &replicator_2_storage_id],
        10,
    );
    let message = Message::new(storage_instruction::create_mining_pool_account(
        &mint_pubkey,
        &mining_pool_pubkey,
        100,
    ));
    bank_client.send_message(&[&mint_keypair], message).unwrap();

    // tick the bank up until it's moved into storage segment 2 because the next advertise is for segment 1
    let next_storage_segment_tick_height = TICKS_IN_SEGMENT * 2;
    for _ in 0..next_storage_segment_tick_height {
        bank.register_tick(&bank.last_blockhash());
    }

    // advertise for storage segment 1
    let message = Message::new_with_payer(
        vec![storage_instruction::advertise_recent_blockhash(
            &validator_storage_id,
            Hash::default(),
            SLOTS_PER_SEGMENT,
        )],
        Some(&mint_pubkey),
    );
    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &validator_storage_keypair], message),
        Ok(_)
    );

    // submit proofs 5 proofs for each replicator for segment 0
    let mut checked_proofs: HashMap<_, Vec<_>> = HashMap::new();
    for slot in 0..5 {
        checked_proofs
            .entry(replicator_1_storage_id)
            .or_default()
            .push(submit_proof(
                &mint_keypair,
                &replicator_1_storage_keypair,
                slot,
                &bank_client,
            ));
        checked_proofs
            .entry(replicator_2_storage_id)
            .or_default()
            .push(submit_proof(
                &mint_keypair,
                &replicator_2_storage_keypair,
                slot,
                &bank_client,
            ));
    }
    let message = Message::new_with_payer(
        vec![storage_instruction::advertise_recent_blockhash(
            &validator_storage_id,
            Hash::default(),
            SLOTS_PER_SEGMENT * 2,
        )],
        Some(&mint_pubkey),
    );

    let next_storage_segment_tick_height = TICKS_IN_SEGMENT;
    for _ in 0..next_storage_segment_tick_height {
        bank.register_tick(&bank.last_blockhash());
    }

    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &validator_storage_keypair], message),
        Ok(_)
    );

    let message = Message::new_with_payer(
        vec![storage_instruction::proof_validation(
            &validator_storage_id,
            get_segment_from_slot(slot) as u64,
            checked_proofs,
        )],
        Some(&mint_pubkey),
    );

    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &validator_storage_keypair], message),
        Ok(_)
    );

    let message = Message::new_with_payer(
        vec![storage_instruction::advertise_recent_blockhash(
            &validator_storage_id,
            Hash::default(),
            SLOTS_PER_SEGMENT * 3,
        )],
        Some(&mint_pubkey),
    );

    let next_storage_segment_tick_height = TICKS_IN_SEGMENT;
    for _ in 0..next_storage_segment_tick_height {
        bank.register_tick(&bank.last_blockhash());
    }

    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &validator_storage_keypair], message),
        Ok(_)
    );

    assert_eq!(bank_client.get_balance(&validator_storage_id).unwrap(), 10);

    let message = Message::new_with_payer(
        vec![storage_instruction::claim_reward(
            &validator_storage_id,
            &mining_pool_pubkey,
            slot,
        )],
        Some(&mint_pubkey),
    );
    assert_matches!(bank_client.send_message(&[&mint_keypair], message), Ok(_));
    assert_eq!(
        bank_client.get_balance(&validator_storage_id).unwrap(),
        10 + (TOTAL_VALIDATOR_REWARDS * 10)
    );

    // tick the bank into the next storage epoch so that rewards can be claimed
    for _ in 0..=TICKS_IN_SEGMENT {
        bank.register_tick(&bank.last_blockhash());
    }

    assert_eq!(
        bank_client.get_balance(&replicator_1_storage_id).unwrap(),
        10
    );

    let message = Message::new_with_payer(
        vec![storage_instruction::claim_reward(
            &replicator_1_storage_id,
            &mining_pool_pubkey,
            slot,
        )],
        Some(&mint_pubkey),
    );
    assert_matches!(bank_client.send_message(&[&mint_keypair], message), Ok(_));

    let message = Message::new_with_payer(
        vec![storage_instruction::claim_reward(
            &replicator_2_storage_id,
            &mining_pool_pubkey,
            slot,
        )],
        Some(&mint_pubkey),
    );
    assert_matches!(bank_client.send_message(&[&mint_keypair], message), Ok(_));

    // TODO enable when rewards are working
    assert_eq!(
        bank_client.get_balance(&replicator_1_storage_id).unwrap(),
        10 + (TOTAL_REPLICATOR_REWARDS * 5)
    );
}

fn init_storage_accounts(
    client: &BankClient,
    mint: &Keypair,
    validator_accounts_to_create: &[&Pubkey],
    replicator_accounts_to_create: &[&Pubkey],
    lamports: u64,
) {
    let mut ixs: Vec<_> = validator_accounts_to_create
        .into_iter()
        .flat_map(|account| {
            storage_instruction::create_validator_storage_account(
                &mint.pubkey(),
                &Pubkey::default(),
                account,
                lamports,
            )
        })
        .collect();
    replicator_accounts_to_create
        .into_iter()
        .for_each(|account| {
            ixs.append(&mut storage_instruction::create_replicator_storage_account(
                &mint.pubkey(),
                &Pubkey::default(),
                account,
                lamports,
            ))
        });
    let message = Message::new(ixs);
    client.send_message(&[mint], message).unwrap();
}

fn get_storage_slot<C: SyncClient>(client: &C, account: &Pubkey) -> u64 {
    match client.get_account_data(&account).unwrap() {
        Some(storage_system_account_data) => {
            let contract = deserialize(&storage_system_account_data);
            if let Ok(contract) = contract {
                match contract {
                    StorageContract::ValidatorStorage { slot, .. } => {
                        return slot;
                    }
                    _ => info!("error in reading slot"),
                }
            }
        }
        None => {
            info!("error in reading slot");
        }
    }
    0
}

fn submit_proof(
    mint_keypair: &Keypair,
    storage_keypair: &Keypair,
    slot: u64,
    bank_client: &BankClient,
) -> CheckedProof {
    let sha_state = Hash::new(Pubkey::new_rand().as_ref());
    let message = Message::new_with_payer(
        vec![storage_instruction::mining_proof(
            &storage_keypair.pubkey(),
            sha_state,
            slot,
            Signature::default(),
        )],
        Some(&mint_keypair.pubkey()),
    );

    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &storage_keypair], message),
        Ok(_)
    );
    CheckedProof {
        proof: Proof {
            signature: Signature::default(),
            sha_state,
        },
        status: ProofStatus::Valid,
    }
}

fn get_storage_blockhash<C: SyncClient>(client: &C, account: &Pubkey) -> Hash {
    if let Some(storage_system_account_data) = client.get_account_data(&account).unwrap() {
        let contract = deserialize(&storage_system_account_data);
        if let Ok(contract) = contract {
            match contract {
                StorageContract::ValidatorStorage { hash, .. } => {
                    return hash;
                }
                _ => (),
            }
        }
    }
    Hash::default()
}

#[test]
fn test_bank_storage() {
    let (genesis_block, mint_keypair) = create_genesis_block(1000);
    let mint_pubkey = mint_keypair.pubkey();
    let replicator_keypair = Keypair::new();
    let replicator_pubkey = replicator_keypair.pubkey();
    let validator_keypair = Keypair::new();
    let validator_pubkey = validator_keypair.pubkey();

    let mut bank = Bank::new(&genesis_block);
    bank.add_instruction_processor(id(), process_instruction);
    // tick the bank up until it's moved into storage segment 2
    let next_storage_segment_tick_height = TICKS_IN_SEGMENT * 2;
    for _ in 0..next_storage_segment_tick_height {
        bank.register_tick(&bank.last_blockhash());
    }
    let bank_client = BankClient::new(bank);

    let x = 42;
    let x2 = x * 2;
    let storage_blockhash = hash(&[x2]);

    bank_client
        .transfer(10, &mint_keypair, &replicator_pubkey)
        .unwrap();

    let message = Message::new(storage_instruction::create_replicator_storage_account(
        &mint_pubkey,
        &Pubkey::default(),
        &replicator_pubkey,
        1,
    ));
    bank_client.send_message(&[&mint_keypair], message).unwrap();

    let message = Message::new(storage_instruction::create_validator_storage_account(
        &mint_pubkey,
        &Pubkey::default(),
        &validator_pubkey,
        1,
    ));
    bank_client.send_message(&[&mint_keypair], message).unwrap();

    let message = Message::new_with_payer(
        vec![storage_instruction::advertise_recent_blockhash(
            &validator_pubkey,
            storage_blockhash,
            SLOTS_PER_SEGMENT,
        )],
        Some(&mint_pubkey),
    );

    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &validator_keypair], message),
        Ok(_)
    );

    let slot = 0;
    let message = Message::new_with_payer(
        vec![storage_instruction::mining_proof(
            &replicator_pubkey,
            Hash::default(),
            slot,
            Signature::default(),
        )],
        Some(&mint_pubkey),
    );
    assert_matches!(
        bank_client.send_message(&[&mint_keypair, &replicator_keypair], message),
        Ok(_)
    );

    assert_eq!(
        get_storage_slot(&bank_client, &validator_pubkey),
        SLOTS_PER_SEGMENT
    );
    assert_eq!(
        get_storage_blockhash(&bank_client, &validator_pubkey),
        storage_blockhash
    );
}