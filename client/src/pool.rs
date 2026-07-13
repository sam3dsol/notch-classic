//! NOTCH Classic client: state mirror + instruction builders + SPL helpers.
//! Constants and math MUST match program/src/lib.rs exactly.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_instruction, system_program,
};
use std::str::FromStr;

pub const POOL_SEED: &[u8] = b"pool";
pub const POOL_SIZE: usize = 91;
pub const MINT_SIZE: usize = 82;

pub const TOTAL_SUPPLY: u64 = 1_000_000_000_000_000_000;
pub const VIRT_SOL0: u64 = 16_000_000_000;
pub const VIRT_TOK0: u64 = 1_120_000_000_000_000_000;
pub const GRAD_SOL: u64 = 40_000_000_000;
pub const OPS_SOL: u64 = 5_000_000_000;
pub const MAX_WALLET_SUPPLY: u64 = TOTAL_SUPPLY / 25;
pub const FEE_BPS: u64 = 100;
pub const MIN_CREATE_NOTCH: u64 = 100_000_000;
pub const MIN_BUY_NOTCH: u64 = 50_000_000;

pub fn platform_wallet() -> Pubkey {
    Pubkey::from_str("4Dz6JuP3M4LMCH9mandbULfZ8nt3S2LvWtwn489vEwuL").unwrap()
}
pub fn migrator() -> Pubkey {
    Pubkey::from_str("3tUp4eSggj6PmHkL4jY1JmBrQUGzbdMdcZx3N7g1UiEM").unwrap()
}
/// The dev-mint feature build's NOTCH stand-in (dev-notch-mint.json).
pub fn dev_notch_mint() -> Pubkey {
    Pubkey::from_str("H943cnr3iWYd8557JS8LHxp9u9YX1994ewEUG6yuJ3VB").unwrap()
}

pub fn token_program() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}
pub fn ata_program() -> Pubkey {
    Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
}

// Curve math mirror (program/src/lib.rs).
pub fn buy_out(x: u64, y: u64, net: u64) -> u64 {
    let k = x as u128 * y as u128;
    let y1 = k.div_ceil(x as u128 + net as u128);
    (y as u128 - y1) as u64
}
pub fn sell_out(x: u64, y: u64, dt: u64) -> u64 {
    let k = x as u128 * y as u128;
    let x1 = k.div_ceil(y as u128 + dt as u128);
    (x as u128 - x1) as u64
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Default, Clone)]
pub struct Pool {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub virt_sol: u64,
    pub virt_tok: u64,
    pub real_sol: u64,
    pub frozen: bool,
    pub migrated: bool,
    pub bump: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum PoolInstruction {
    Create,
    Buy { lamports: u64, min_out: u64 },
    Sell { units: u64, min_out: u64 },
    Migrate,
}

pub fn pool_pda(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], program_id)
}

pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program().as_ref(), mint.as_ref()],
        &ata_program(),
    )
    .0
}

// ---------------------------------------------------------------------------
// Program instructions
// ---------------------------------------------------------------------------

pub fn create(
    program_id: &Pubkey,
    creator: &Pubkey,
    mint: &Pubkey,
    notch_ta: &Pubkey,
) -> Instruction {
    let (pda, _) = pool_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*creator, true),
            AccountMeta::new(pda, false),
            AccountMeta::new(*mint, false),
            AccountMeta::new(ata(&pda, mint), false),
            AccountMeta::new_readonly(*notch_ta, false),
            AccountMeta::new_readonly(token_program(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: borsh::to_vec(&PoolInstruction::Create).unwrap(),
    }
}

pub fn buy(
    program_id: &Pubkey,
    buyer: &Pubkey,
    mint: &Pubkey,
    notch_ta: &Pubkey,
    lamports: u64,
    min_out: u64,
) -> Instruction {
    let (pda, _) = pool_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*buyer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(ata(&pda, mint), false),
            AccountMeta::new(ata(buyer, mint), false),
            AccountMeta::new_readonly(*notch_ta, false),
            AccountMeta::new(platform_wallet(), false),
            AccountMeta::new_readonly(token_program(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: borsh::to_vec(&PoolInstruction::Buy { lamports, min_out }).unwrap(),
    }
}

pub fn payout_pda(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"payout"], program_id).0
}

/// Sell WITHOUT the payout route (direct lamport payout, 7 accounts).
pub fn sell(
    program_id: &Pubkey,
    seller: &Pubkey,
    mint: &Pubkey,
    creator: &Pubkey,
    units: u64,
    min_out: u64,
) -> Instruction {
    let (pda, _) = pool_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*seller, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(ata(&pda, mint), false),
            AccountMeta::new(ata(seller, mint), false),
            AccountMeta::new(*creator, false),
            AccountMeta::new_readonly(token_program(), false),
        ],
        data: borsh::to_vec(&PoolInstruction::Sell { units, min_out }).unwrap(),
    }
}

/// Sell WITH the payout route: appends the payout PDA + system program so the
/// seller/creator legs are real System transfers (explorer-visible).
pub fn sell_via_payout(
    program_id: &Pubkey,
    seller: &Pubkey,
    mint: &Pubkey,
    creator: &Pubkey,
    units: u64,
    min_out: u64,
) -> Instruction {
    let mut ix = sell(program_id, seller, mint, creator, units, min_out);
    ix.accounts.push(AccountMeta::new(payout_pda(program_id), false));
    ix.accounts.push(AccountMeta::new_readonly(system_program::ID, false));
    ix
}

/// Migrate WITH the payout route (the operator always uses this): ops SOL to
/// the platform and the LP SOL to the migrator as real System transfers.
pub fn migrate(program_id: &Pubkey, migrator_key: &Pubkey, mint: &Pubkey) -> Instruction {
    let (pda, _) = pool_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*migrator_key, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(ata(&pda, mint), false),
            AccountMeta::new(ata(migrator_key, mint), false),
            AccountMeta::new(platform_wallet(), false),
            AccountMeta::new_readonly(token_program(), false),
            AccountMeta::new(payout_pda(program_id), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: borsh::to_vec(&PoolInstruction::Migrate).unwrap(),
    }
}

/// Migrate WITHOUT the payout route (direct lamport payout, 7 accounts).
pub fn migrate_direct(program_id: &Pubkey, migrator_key: &Pubkey, mint: &Pubkey) -> Instruction {
    let (pda, _) = pool_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*migrator_key, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(ata(&pda, mint), false),
            AccountMeta::new(ata(migrator_key, mint), false),
            AccountMeta::new(platform_wallet(), false),
            AccountMeta::new_readonly(token_program(), false),
        ],
        data: borsh::to_vec(&PoolInstruction::Migrate).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// SPL setup helpers
// ---------------------------------------------------------------------------

/// create_account + InitializeMint2 { decimals 9, authority = pool PDA, no
/// freeze } — the shape Create expects (the program mints + revokes itself).
pub fn create_pool_mint_ixs(
    program_id: &Pubkey,
    payer: &Pubkey,
    mint: &Pubkey,
    mint_rent: u64,
) -> Vec<Instruction> {
    let (pda, _) = pool_pda(program_id, mint);
    plain_mint_ixs(payer, mint, &pda, mint_rent)
}

/// Plain SPL mint with an explicit authority (e.g. the dev NOTCH stand-in).
pub fn plain_mint_ixs(
    payer: &Pubkey,
    mint: &Pubkey,
    authority: &Pubkey,
    mint_rent: u64,
) -> Vec<Instruction> {
    let mut data = Vec::with_capacity(35);
    data.push(20u8); // InitializeMint2
    data.push(9u8); // decimals
    data.extend_from_slice(authority.as_ref());
    data.push(0u8); // freeze_authority = None
    vec![
        system_instruction::create_account(payer, mint, mint_rent, MINT_SIZE as u64, &token_program()),
        Instruction {
            program_id: token_program(),
            accounts: vec![AccountMeta::new(*mint, false)],
            data,
        },
    ]
}

pub fn mint_to_ix(mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(7u8); // MintTo
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: token_program(),
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// Associated token account CreateIdempotent (works for off-curve owners like
/// the pool PDA).
pub fn create_ata_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: ata_program(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(token_program(), false),
        ],
        data: vec![1u8], // CreateIdempotent
    }
}

// ---------------------------------------------------------------------------
// Account readers
// ---------------------------------------------------------------------------

pub fn parse_pool(data: &[u8]) -> Option<Pool> {
    Pool::try_from_slice(data).ok()
}

pub fn mint_supply(data: &[u8]) -> u64 {
    if data.len() < 44 {
        return 0;
    }
    u64::from_le_bytes(data[36..44].try_into().unwrap())
}

/// Mint authority COption tag (0 = None — supply fixed forever).
pub fn mint_authority_tag(data: &[u8]) -> u32 {
    if data.len() < 4 {
        return u32::MAX;
    }
    u32::from_le_bytes(data[0..4].try_into().unwrap())
}

pub fn token_amount(data: &[u8]) -> u64 {
    if data.len() < 72 {
        return 0;
    }
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}
