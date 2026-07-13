//! NOTCH Classic — constant-product bonding curve launchpad (v2, beside the
//! up-only Protected curve). Full price discovery, no floor: a classic curve.
//!
//! The whole platform runs through $NOTCH:
//!   * CREATE requires the creator to HOLD 0.1 $NOTCH (checked, never debited).
//!   * BUY on the bonding curve requires the buyer to HOLD 0.05 $NOTCH.
//!     Sells are never gated — holders can always exit.
//!   * After graduation the pool migrates to Raydium, which is permissionless:
//!     holding NOTCH buys early access, not a permanent wall.
//!
//! Fixed economics (no per-launch knobs):
//!   * Supply 1B (9 decimals), minted once to the pool vault at create, then
//!     the mint authority is set to None — supply is provably fixed forever.
//!   * Constant product on virtual reserves: x0 = 16 SOL, y0 = 1.12B tokens,
//!     chosen so a 40 SOL raise sells exactly ~800M tokens and the final curve
//!     price equals the Raydium listing price (35 SOL / ~200M tokens), so
//!     there is no price gap at migration. Rounding always favors the pool.
//!   * BUY fee 1% -> the platform wallet. SELL fee 1% -> the pool's creator.
//!     No other fees.
//!   * No wallet may hold more than 4% of the supply from the curve (the
//!     buyer's balance after a buy is capped). A SOL-sized cap would be
//!     toothless early — 5 SOL at genesis would sweep ~26% of supply — while
//!     the wallet cap binds hardest exactly there: a full 4% position costs
//!     ~0.59 SOL at launch, ~8 SOL near graduation; filling the curve takes
//!     at least 20 wallets.
//!   * Graduation at 40 SOL real: trading freezes; Migrate pays 5 SOL to the
//!     platform ops wallet (DexScreener listing + market making budget) and
//!     hands the remaining SOL plus all unsold tokens to the migrator, which
//!     creates the Raydium pool and locks the liquidity.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};

solana_security_txt::security_txt! {
    name: "NOTCH Classic",
    project_url: "https://notch.fund",
    contacts: "email:sam3dsol@gmail.com,link:https://notch.fund/.well-known/security.txt",
    policy: "https://notch.fund/safety",
    preferred_languages: "en",
    source_code: "https://github.com/sam3dsol/notch-classic"
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const POOL_SEED: &[u8] = b"pool";
/// Zero-data, system-owned PDA the Sell and Migrate payouts hop through so the
/// seller, creator, platform and migrator legs are real System transfers
/// (explorers render those; a direct lamport move on the data-carrying pool
/// PDA would not). It never holds lamports across instructions.
pub const PAYOUT_SEED: &[u8] = b"payout";
/// mint(32) creator(32) virt_sol(8) virt_tok(8) real_sol(8) frozen(1)
/// migrated(1) bump(1)
pub const POOL_SIZE: usize = 32 + 32 + 8 + 8 + 8 + 1 + 1 + 1; // = 91

const TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// 1B tokens at 9 decimals, minted once at create; authority then set to None.
pub const TOTAL_SUPPLY: u64 = 1_000_000_000_000_000_000;
/// Virtual reserves: x0 = 16 SOL, y0 = 1.12B tokens — the closed form for
/// "raising GRAD_SOL sells ~800M tokens and the final curve price equals the
/// Raydium listing price (GRAD_SOL - OPS_SOL) / ~200M tokens". Both land on
/// exact integers at a 40 SOL raise.
pub const VIRT_SOL0: u64 = 16_000_000_000;
pub const VIRT_TOK0: u64 = 1_120_000_000_000_000_000;
/// Bonding target: net SOL in the curve at which trading freezes.
pub const GRAD_SOL: u64 = 40_000_000_000;
/// Paid to the platform ops wallet at migration (DexScreener listing 4 SOL +
/// market making 1 SOL). Everything above it goes to the Raydium pool.
pub const OPS_SOL: u64 = 5_000_000_000;
/// Wallet cap: no wallet may hold more than 4% of the supply from the curve
/// (the buyer's token balance after a buy must stay within this).
pub const MAX_WALLET_SUPPLY: u64 = TOTAL_SUPPLY / 25;
/// 1% buy fee -> platform; 1% sell fee -> the pool's creator.
pub const FEE_BPS: u64 = 100;

/// Platform wallet: receives every buy fee and the migration ops budget.
/// Dedicated fee address (keypair in vault) so platform income stays separate
/// from every other wallet's traffic.
pub const PLATFORM_WALLET: Pubkey =
    pubkey!("4Dz6JuP3M4LMCH9mandbULfZ8nt3S2LvWtwn489vEwuL");
/// The only key allowed to run Migrate (dedicated hot key, not the upgrade
/// authority). It creates the Raydium pool and locks the liquidity.
pub const MIGRATOR: Pubkey = pubkey!("3tUp4eSggj6PmHkL4jY1JmBrQUGzbdMdcZx3N7g1UiEM");

/// Hold-gates: balances are checked, never debited.
pub const MIN_CREATE_NOTCH: u64 = 100_000_000; // 0.1 NOTCH
pub const MIN_BUY_NOTCH: u64 = 50_000_000; // 0.05 NOTCH
#[cfg(not(feature = "dev-mint"))]
pub const NOTCH_MINT: Pubkey = pubkey!("LT4z98vU6bLXhfrSH4wXgUq98gocjWfoxYw85fNotCH");
/// Local-validator stand-in (keypair committed at dev-notch-mint.json).
#[cfg(feature = "dev-mint")]
pub const NOTCH_MINT: Pubkey = pubkey!("H943cnr3iWYd8557JS8LHxp9u9YX1994ewEUG6yuJ3VB");

// Custom errors.
const E_BAD_PARAMS: u32 = 1;
const E_BAD_PDA: u32 = 2;
const E_BAD_MINT: u32 = 3;
const E_BAD_WALLET: u32 = 4;
const E_SLIPPAGE: u32 = 5;
const E_WALLET_CAP: u32 = 6;
const E_INSUFFICIENT: u32 = 7;
const E_OVERFLOW: u32 = 8;
const E_ALREADY_INIT: u32 = 9;
const E_ZERO_AMOUNT: u32 = 10;
const E_BAD_TOKEN_ACCOUNT: u32 = 11;
const E_NEED_NOTCH: u32 = 13;
const E_FROZEN: u32 = 14;
const E_NOT_FROZEN: u32 = 15;

fn err(code: u32) -> ProgramError {
    ProgramError::Custom(code)
}

// ---------------------------------------------------------------------------
// Curve math: constant product on virtual reserves. Rounding favors the pool.
// ---------------------------------------------------------------------------

/// Tokens out for `net` lamports in: dt = y - ceil(x*y / (x + net)).
pub fn buy_out(x: u64, y: u64, net: u64) -> Option<u64> {
    let k = (x as u128).checked_mul(y as u128)?;
    let x1 = (x as u128).checked_add(net as u128)?;
    let y1 = k.div_ceil(x1);
    u64::try_from((y as u128).checked_sub(y1)?).ok()
}

/// Lamports out for `dt` tokens in: out = x - ceil(x*y / (y + dt)).
pub fn sell_out(x: u64, y: u64, dt: u64) -> Option<u64> {
    let k = (x as u128).checked_mul(y as u128)?;
    let y1 = (y as u128).checked_add(dt as u128)?;
    let x1 = k.div_ceil(y1);
    u64::try_from((x as u128).checked_sub(x1)?).ok()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(BorshSerialize, BorshDeserialize, Debug, Default)]
pub struct Pool {
    pub mint: Pubkey,
    /// Receives the 1% sell fee (recorded at create).
    pub creator: Pubkey,
    /// Virtual SOL reserve (x). Starts at VIRT_SOL0; +net on buys, -out on sells.
    pub virt_sol: u64,
    /// Virtual token reserve (y).
    pub virt_tok: u64,
    /// Net real SOL in the pool (payouts come only from this).
    pub real_sol: u64,
    /// True once real_sol reached GRAD_SOL: trading closed, awaiting Migrate.
    pub frozen: bool,
    /// True once Migrate ran. Terminal.
    pub migrated: bool,
    pub bump: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum PoolInstruction {
    /// Create the pool for `mint`. The mint must be: decimals 9, supply 0,
    /// mint_authority == pool PDA, freeze_authority == None. The program mints
    /// the full fixed supply to the pool vault and sets the mint authority to
    /// None in the same instruction. The creator must hold >= 0.1 $NOTCH.
    /// Accounts: [creator (signer, writable), pool PDA (writable),
    ///            mint (writable), pool_vault (writable, token acct owned by
    ///            the pool PDA), creator_notch_ta, token_program,
    ///            system_program]
    Create,
    /// Swap `lamports` (gross) for tokens; the buyer's balance after the buy
    /// may not exceed 4% of the supply. 1% fee to the platform wallet; the
    /// buyer must hold >= 0.05 $NOTCH. Freezes the pool at the raise target.
    /// Accounts: [buyer (signer, writable), pool PDA (writable), mint,
    ///            pool_vault (writable), buyer_token_account (writable),
    ///            buyer_notch_ta, platform_wallet (writable), token_program,
    ///            system_program]
    Buy { lamports: u64, min_out: u64 },
    /// Swap `units` tokens back for SOL at the curve. 1% of the SOL out goes
    /// to the pool's creator. Never gated.
    /// Accounts: [seller (signer, writable), pool PDA (writable), mint,
    ///            pool_vault (writable), seller_token_account (writable),
    ///            creator (writable), token_program,
    ///            payout PDA (writable, optional), system_program (optional)]
    /// With the two optional accounts the seller/creator legs are real System
    /// transfers (explorer-visible); without them, direct lamport moves.
    Sell { units: u64, min_out: u64 },
    /// After graduation, hand the raise to migration: OPS_SOL to the platform
    /// ops wallet, the remaining real SOL plus the entire unsold token balance
    /// to the migrator (which creates the Raydium pool and locks the LP).
    /// Only the hardcoded MIGRATOR may call, exactly once.
    /// Accounts: [migrator (signer, writable), pool PDA (writable), mint,
    ///            pool_vault (writable), migrator_token_account (writable),
    ///            platform_wallet (writable), token_program,
    ///            payout PDA (writable, optional), system_program (optional)]
    /// With the two optional accounts the ops/migrator legs are real System
    /// transfers (explorer-visible); without them, direct lamport moves.
    Migrate,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn pool_pda(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], program_id)
}

struct MintView {
    authority: Option<Pubkey>,
    supply: u64,
    decimals: u8,
    initialized: bool,
    freeze_authority: Option<Pubkey>,
}

fn parse_mint(data: &[u8]) -> Result<MintView, ProgramError> {
    if data.len() < 82 {
        return Err(err(E_BAD_MINT));
    }
    let opt = |tag_off: usize, key_off: usize| -> Option<Pubkey> {
        let tag = u32::from_le_bytes(data[tag_off..tag_off + 4].try_into().unwrap());
        if tag == 1 {
            Some(Pubkey::new_from_array(data[key_off..key_off + 32].try_into().unwrap()))
        } else {
            None
        }
    };
    Ok(MintView {
        authority: opt(0, 4),
        supply: u64::from_le_bytes(data[36..44].try_into().unwrap()),
        decimals: data[44],
        initialized: data[45] == 1,
        freeze_authority: opt(46, 50),
    })
}

/// (mint, owner, amount) of an SPL token account.
fn token_view(data: &[u8]) -> Result<(Pubkey, Pubkey, u64), ProgramError> {
    if data.len() < 72 {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }
    Ok((
        Pubkey::new_from_array(data[0..32].try_into().unwrap()),
        Pubkey::new_from_array(data[32..64].try_into().unwrap()),
        u64::from_le_bytes(data[64..72].try_into().unwrap()),
    ))
}

/// Hold-gate: `ta` must be an SPL token account of the NOTCH mint, owned by
/// `holder`, with at least `min` units. Read-only — nothing is debited.
fn require_notch(ta: &AccountInfo, holder: &Pubkey, min: u64) -> ProgramResult {
    if *ta.owner != TOKEN_PROGRAM {
        return Err(err(E_NEED_NOTCH));
    }
    let (mint, owner, amount) = token_view(&ta.data.borrow())?;
    if mint != NOTCH_MINT || owner != *holder || amount < min {
        return Err(err(E_NEED_NOTCH));
    }
    Ok(())
}

fn rent_floor() -> Result<u64, ProgramError> {
    Ok(Rent::get()?.minimum_balance(POOL_SIZE))
}

fn load_pool(
    program_id: &Pubkey,
    pool_ai: &AccountInfo,
    mint_ai: &AccountInfo,
) -> Result<Pool, ProgramError> {
    if pool_ai.owner != program_id {
        return Err(err(E_BAD_PDA));
    }
    let pool = Pool::try_from_slice(&pool_ai.data.borrow())?;
    let (expect, _) = pool_pda(program_id, &pool.mint);
    if expect != *pool_ai.key || pool.mint != *mint_ai.key {
        return Err(err(E_BAD_PDA));
    }
    Ok(pool)
}

fn store_pool(pool: &Pool, pool_ai: &AccountInfo) -> ProgramResult {
    pool.serialize(&mut &mut pool_ai.data.borrow_mut()[..])?;
    Ok(())
}

/// The pool vault: any token account of `mint` owned by the pool PDA.
fn require_vault(vault_ai: &AccountInfo, mint: &Pubkey, pda: &Pubkey) -> Result<u64, ProgramError> {
    if *vault_ai.owner != TOKEN_PROGRAM {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }
    let (m, o, amount) = token_view(&vault_ai.data.borrow())?;
    if m != *mint || o != *pda {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }
    Ok(amount)
}

fn spl_mint_to(mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(7u8); // MintTo
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// SetAuthority(MintTokens -> None): fixes the supply forever.
fn spl_revoke_mint_authority(mint: &Pubkey, current: &Pubkey) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new_readonly(*current, true),
        ],
        data: vec![6u8, 0u8, 0u8], // SetAuthority, MintTokens, COption::None
    }
}

/// Pay `to_recipient` lamports and (optionally) `to_creator` lamports out of the
/// pool. `total` (= sum of the legs) is first moved from the pool PDA. If the
/// caller passed the payout PDA + system program as the trailing two accounts,
/// the legs are routed through the transient payout PDA as real System transfers
/// (explorer-visible); otherwise they are direct lamport credits. The pool PDA
/// rides along as an extra (ignored) account on the first System transfer so the
/// runtime's pre-CPI balance check sees both sides of the pool->payout move.
#[allow(clippy::too_many_arguments)]
fn pay_out<'a, 'info>(
    program_id: &Pubkey,
    ai: &mut std::slice::Iter<'a, AccountInfo<'info>>,
    pool_ai: &AccountInfo<'info>,
    total: u64,
    recipient_ai: &AccountInfo<'info>,
    to_recipient: u64,
    creator_ai: &AccountInfo<'info>,
    to_creator: u64,
) -> ProgramResult {
    let via_payout = (|| -> Option<(AccountInfo, AccountInfo, u8)> {
        let payout_ai = next_account_info(ai).ok()?.clone();
        let system_ai = next_account_info(ai).ok()?.clone();
        if *system_ai.key != system_program::ID {
            return None;
        }
        let (expect, bump) = Pubkey::find_program_address(&[PAYOUT_SEED], program_id);
        if expect != *payout_ai.key || !payout_ai.data_is_empty() || payout_ai.owner != &system_program::ID {
            return None;
        }
        Some((payout_ai, system_ai, bump))
    })();

    **pool_ai.try_borrow_mut_lamports()? -= total;
    match via_payout {
        Some((payout_ai, system_ai, bump)) => {
            **payout_ai.try_borrow_mut_lamports()? += total;
            let seeds: &[&[u8]] = &[PAYOUT_SEED, &[bump]];
            let mut t = system_instruction::transfer(payout_ai.key, recipient_ai.key, to_recipient);
            t.accounts.push(AccountMeta::new(*pool_ai.key, false));
            invoke_signed(
                &t,
                &[payout_ai.clone(), recipient_ai.clone(), pool_ai.clone(), system_ai.clone()],
                &[seeds],
            )?;
            if to_creator > 0 {
                invoke_signed(
                    &system_instruction::transfer(payout_ai.key, creator_ai.key, to_creator),
                    &[payout_ai.clone(), creator_ai.clone(), system_ai.clone()],
                    &[seeds],
                )?;
            }
        }
        None => {
            **recipient_ai.try_borrow_mut_lamports()? += to_recipient;
            if to_creator > 0 {
                **creator_ai.try_borrow_mut_lamports()? += to_creator;
            }
        }
    }
    Ok(())
}

fn spl_transfer(src: &Pubkey, dst: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8); // Transfer
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*src, false),
            AccountMeta::new(*dst, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match PoolInstruction::try_from_slice(data)? {
        PoolInstruction::Create => create(program_id, accounts),
        PoolInstruction::Buy { lamports, min_out } => buy(program_id, accounts, lamports, min_out),
        PoolInstruction::Sell { units, min_out } => sell(program_id, accounts, units, min_out),
        PoolInstruction::Migrate => migrate(program_id, accounts),
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

fn create(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let ai = &mut accounts.iter();
    let creator_ai = next_account_info(ai)?;
    let pool_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let vault_ai = next_account_info(ai)?;
    let notch_ta_ai = next_account_info(ai)?;
    let token_ai = next_account_info(ai)?;
    let system_ai = next_account_info(ai)?;

    if !creator_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_ai.key != TOKEN_PROGRAM || *system_ai.key != system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    require_notch(notch_ta_ai, creator_ai.key, MIN_CREATE_NOTCH)?;

    let (pda, bump) = pool_pda(program_id, mint_ai.key);
    if pda != *pool_ai.key {
        return Err(err(E_BAD_PDA));
    }
    if !pool_ai.data_is_empty() || pool_ai.owner == program_id {
        return Err(err(E_ALREADY_INIT));
    }

    // The mint must be fully under the pool's control with zero supply.
    if *mint_ai.owner != TOKEN_PROGRAM {
        return Err(err(E_BAD_MINT));
    }
    let mint = parse_mint(&mint_ai.data.borrow())?;
    if !mint.initialized
        || mint.decimals != 9
        || mint.supply != 0
        || mint.authority != Some(pda)
        || mint.freeze_authority.is_some()
    {
        return Err(err(E_BAD_MINT));
    }
    require_vault(vault_ai, mint_ai.key, &pda)?;

    let rent = Rent::get()?.minimum_balance(POOL_SIZE);
    let seeds: &[&[u8]] = &[POOL_SEED, mint_ai.key.as_ref(), &[bump]];
    if pool_ai.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(creator_ai.key, pool_ai.key, rent, POOL_SIZE as u64, program_id),
            &[creator_ai.clone(), pool_ai.clone(), system_ai.clone()],
            &[seeds],
        )?;
    } else {
        // Pre-funded PDA (griefing attempt or donation): allocate+assign path.
        if pool_ai.lamports() < rent {
            invoke(
                &system_instruction::transfer(creator_ai.key, pool_ai.key, rent - pool_ai.lamports()),
                &[creator_ai.clone(), pool_ai.clone(), system_ai.clone()],
            )?;
        }
        invoke_signed(&system_instruction::allocate(pool_ai.key, POOL_SIZE as u64), &[pool_ai.clone(), system_ai.clone()], &[seeds])?;
        invoke_signed(&system_instruction::assign(pool_ai.key, program_id), &[pool_ai.clone(), system_ai.clone()], &[seeds])?;
    }

    // Mint the entire fixed supply to the vault, then revoke the authority:
    // supply can never change again.
    invoke_signed(
        &spl_mint_to(mint_ai.key, vault_ai.key, &pda, TOTAL_SUPPLY),
        &[mint_ai.clone(), vault_ai.clone(), pool_ai.clone(), token_ai.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &spl_revoke_mint_authority(mint_ai.key, &pda),
        &[mint_ai.clone(), pool_ai.clone(), token_ai.clone()],
        &[seeds],
    )?;

    let pool = Pool {
        mint: *mint_ai.key,
        creator: *creator_ai.key,
        virt_sol: VIRT_SOL0,
        virt_tok: VIRT_TOK0,
        real_sol: 0,
        frozen: false,
        migrated: false,
        bump,
    };
    store_pool(&pool, pool_ai)?;
    msg!("classic: create mint={} creator={}", mint_ai.key, creator_ai.key);
    Ok(())
}

// ---------------------------------------------------------------------------
// Buy
// ---------------------------------------------------------------------------

fn buy(program_id: &Pubkey, accounts: &[AccountInfo], lamports: u64, min_out: u64) -> ProgramResult {
    let ai = &mut accounts.iter();
    let buyer_ai = next_account_info(ai)?;
    let pool_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let vault_ai = next_account_info(ai)?;
    let buyer_ta_ai = next_account_info(ai)?;
    let notch_ta_ai = next_account_info(ai)?;
    let platform_ai = next_account_info(ai)?;
    let token_ai = next_account_info(ai)?;
    let system_ai = next_account_info(ai)?;

    if !buyer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_ai.key != TOKEN_PROGRAM || *system_ai.key != system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *platform_ai.key != PLATFORM_WALLET {
        return Err(err(E_BAD_WALLET));
    }
    if lamports == 0 {
        return Err(err(E_ZERO_AMOUNT));
    }
    let mut pool = load_pool(program_id, pool_ai, mint_ai)?;
    if pool.frozen || pool.migrated {
        return Err(err(E_FROZEN));
    }
    require_notch(notch_ta_ai, buyer_ai.key, MIN_BUY_NOTCH)?;
    let vault_bal = require_vault(vault_ai, &pool.mint, pool_ai.key)?;
    let (bm, _, buyer_bal) = token_view(&buyer_ta_ai.data.borrow())?;
    if bm != pool.mint {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }

    let fee = lamports * FEE_BPS / 10_000;
    let net = lamports - fee;
    if net == 0 {
        return Err(err(E_ZERO_AMOUNT));
    }
    let out = buy_out(pool.virt_sol, pool.virt_tok, net).ok_or(err(E_OVERFLOW))?;
    if buyer_bal.saturating_add(out) > MAX_WALLET_SUPPLY {
        return Err(err(E_WALLET_CAP));
    }
    if out == 0 || out < min_out {
        return Err(err(E_SLIPPAGE));
    }
    if out > vault_bal {
        return Err(err(E_INSUFFICIENT));
    }

    invoke(
        &system_instruction::transfer(buyer_ai.key, pool_ai.key, net),
        &[buyer_ai.clone(), pool_ai.clone(), system_ai.clone()],
    )?;
    if fee > 0 {
        invoke(
            &system_instruction::transfer(buyer_ai.key, platform_ai.key, fee),
            &[buyer_ai.clone(), platform_ai.clone(), system_ai.clone()],
        )?;
    }
    invoke_signed(
        &spl_transfer(vault_ai.key, buyer_ta_ai.key, pool_ai.key, out),
        &[vault_ai.clone(), buyer_ta_ai.clone(), pool_ai.clone(), token_ai.clone()],
        &[&[POOL_SEED, pool.mint.as_ref(), &[pool.bump]]],
    )?;

    pool.virt_sol = pool.virt_sol.checked_add(net).ok_or(err(E_OVERFLOW))?;
    pool.virt_tok = pool.virt_tok.checked_sub(out).ok_or(err(E_OVERFLOW))?;
    pool.real_sol = pool.real_sol.checked_add(net).ok_or(err(E_OVERFLOW))?;
    if pool.real_sol >= GRAD_SOL {
        pool.frozen = true;
        msg!("classic: GRADUATED at {} lamports — awaiting migration", pool.real_sol);
    }
    store_pool(&pool, pool_ai)?;
    msg!("classic: buy {} -> {} units", lamports, out);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sell
// ---------------------------------------------------------------------------

fn sell(program_id: &Pubkey, accounts: &[AccountInfo], units: u64, min_out: u64) -> ProgramResult {
    let ai = &mut accounts.iter();
    let seller_ai = next_account_info(ai)?;
    let pool_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let vault_ai = next_account_info(ai)?;
    let seller_ta_ai = next_account_info(ai)?;
    let creator_ai = next_account_info(ai)?;
    let token_ai = next_account_info(ai)?;

    if !seller_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_ai.key != TOKEN_PROGRAM {
        return Err(ProgramError::IncorrectProgramId);
    }
    if units == 0 {
        return Err(err(E_ZERO_AMOUNT));
    }
    let mut pool = load_pool(program_id, pool_ai, mint_ai)?;
    if pool.frozen || pool.migrated {
        return Err(err(E_FROZEN));
    }
    if *creator_ai.key != pool.creator {
        return Err(err(E_BAD_WALLET));
    }
    require_vault(vault_ai, &pool.mint, pool_ai.key)?;
    let (sm, _, _) = token_view(&seller_ta_ai.data.borrow())?;
    if sm != pool.mint {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }

    let gross = sell_out(pool.virt_sol, pool.virt_tok, units).ok_or(err(E_OVERFLOW))?;
    if gross == 0 {
        return Err(err(E_SLIPPAGE));
    }
    if gross > pool.real_sol {
        return Err(err(E_INSUFFICIENT));
    }
    let fee = gross * FEE_BPS / 10_000;
    let to_seller = gross - fee;
    if to_seller < min_out {
        return Err(err(E_SLIPPAGE));
    }

    // Tokens back to the vault (seller signs), then pay out of the pool. The
    // token CPI runs BEFORE the payout moves so the runtime's pre-CPI balance
    // check on the later System transfers is not thrown off. With the optional
    // payout PDA + system program the seller/creator legs are real System
    // transfers (explorer-visible); without them, direct lamport moves.
    invoke(
        &spl_transfer(seller_ta_ai.key, vault_ai.key, seller_ai.key, units),
        &[seller_ta_ai.clone(), vault_ai.clone(), seller_ai.clone(), token_ai.clone()],
    )?;
    pay_out(program_id, ai, pool_ai, gross, seller_ai, to_seller, creator_ai, fee)?;
    if pool_ai.lamports() < rent_floor()? {
        return Err(err(E_INSUFFICIENT));
    }

    pool.virt_sol = pool.virt_sol.checked_sub(gross).ok_or(err(E_OVERFLOW))?;
    pool.virt_tok = pool.virt_tok.checked_add(units).ok_or(err(E_OVERFLOW))?;
    pool.real_sol = pool.real_sol.checked_sub(gross).ok_or(err(E_OVERFLOW))?;
    store_pool(&pool, pool_ai)?;
    msg!("classic: sell {} units -> {} lamports (creator fee {})", units, to_seller, fee);
    Ok(())
}

// ---------------------------------------------------------------------------
// Migrate
// ---------------------------------------------------------------------------

fn migrate(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let ai = &mut accounts.iter();
    let migrator_ai = next_account_info(ai)?;
    let pool_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let vault_ai = next_account_info(ai)?;
    let migrator_ta_ai = next_account_info(ai)?;
    let platform_ai = next_account_info(ai)?;
    let token_ai = next_account_info(ai)?;

    if !migrator_ai.is_signer || *migrator_ai.key != MIGRATOR {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_ai.key != TOKEN_PROGRAM {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *platform_ai.key != PLATFORM_WALLET {
        return Err(err(E_BAD_WALLET));
    }
    let mut pool = load_pool(program_id, pool_ai, mint_ai)?;
    if !pool.frozen {
        return Err(err(E_NOT_FROZEN));
    }
    if pool.migrated {
        return Err(err(E_ALREADY_INIT));
    }
    let vault_bal = require_vault(vault_ai, &pool.mint, pool_ai.key)?;
    let (mm, mo, _) = token_view(&migrator_ta_ai.data.borrow())?;
    if mm != pool.mint || mo != MIGRATOR {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }
    if pool.real_sol < OPS_SOL {
        return Err(err(E_INSUFFICIENT)); // unreachable: GRAD_SOL > OPS_SOL
    }

    // 5 SOL ops budget (DexScreener + market making) to the platform wallet;
    // the remaining raise + every unsold token to the migrator, which creates
    // the Raydium pool at the graduation price and locks the liquidity.
    // The token CPI runs BEFORE the direct lamport moves: the runtime's
    // pre-CPI balance check only sees the accounts inside the CPI frame, so
    // debiting the pool first makes that frame read as unbalanced (the
    // credited wallets are not part of it).
    invoke_signed(
        &spl_transfer(vault_ai.key, migrator_ta_ai.key, pool_ai.key, vault_bal),
        &[vault_ai.clone(), migrator_ta_ai.clone(), pool_ai.clone(), token_ai.clone()],
        &[&[POOL_SEED, pool.mint.as_ref(), &[pool.bump]]],
    )?;
    // OPS_SOL to the platform ops wallet, the rest to the migrator. With the
    // optional payout PDA + system program these are real System transfers
    // (explorer-visible); the migrator always passes them.
    let to_lp = pool.real_sol - OPS_SOL;
    pay_out(program_id, ai, pool_ai, pool.real_sol, platform_ai, OPS_SOL, migrator_ai, to_lp)?;
    if pool_ai.lamports() < rent_floor()? {
        return Err(err(E_INSUFFICIENT));
    }

    pool.real_sol = 0;
    pool.migrated = true;
    store_pool(&pool, pool_ai)?;
    msg!("classic: migrated — {} lamports + {} units to LP, {} ops", to_lp, vault_bal, OPS_SOL);
    Ok(())
}

// ---------------------------------------------------------------------------
// Native tests: curve properties (pure math, mirrors the on-chain paths).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod curve_tests {
    use super::*;

    const SOL: u64 = 1_000_000_000;

    struct Sim {
        x: u64,
        y: u64,
        real: u64,
        frozen: bool,
    }
    impl Sim {
        fn new() -> Self {
            Sim { x: VIRT_SOL0, y: VIRT_TOK0, real: 0, frozen: false }
        }
        /// Gross-lamports buy through the program path. Returns tokens out.
        fn buy(&mut self, gross: u64) -> u64 {
            assert!(!self.frozen);
            let net = gross - gross * FEE_BPS / 10_000;
            let out = buy_out(self.x, self.y, net).unwrap();
            // Sim chunks model fresh wallets: each single buy must fit the cap.
            assert!(out <= MAX_WALLET_SUPPLY, "buy exceeds the 4% wallet cap");
            self.x += net;
            self.y -= out;
            self.real += net;
            if self.real >= GRAD_SOL {
                self.frozen = true;
            }
            out
        }
        /// Largest gross spend a FRESH wallet can make (token out within the
        /// 4% wallet cap) at the current curve position (margin for rounding).
        fn max_gross(&self) -> u64 {
            let k = self.x as u128 * self.y as u128;
            let net = (k / (self.y - MAX_WALLET_SUPPLY) as u128).saturating_sub(self.x as u128) as u64;
            ((net as u128 * 10_000 / (10_000 - FEE_BPS as u128)) as u64).saturating_sub(10_000).max(1)
        }
        /// Token sell through the program path. Returns net lamports to seller.
        fn sell(&mut self, units: u64) -> u64 {
            assert!(!self.frozen);
            let gross = sell_out(self.x, self.y, units).unwrap();
            assert!(gross <= self.real, "payout must come from real SOL");
            self.x -= gross;
            self.y += units;
            self.real -= gross;
            gross - gross * FEE_BPS / 10_000
        }
        fn sold(&self) -> u64 {
            VIRT_TOK0 - self.y
        }
        fn price(&self) -> f64 {
            self.x as f64 / self.y as f64
        }
    }

    #[test]
    fn raise_35_sells_about_800m_and_price_matches_lp() {
        let mut s = Sim::new();
        // Buy in max-size chunks until graduation (fee-adjusted gross).
        while !s.frozen {
            s.buy(s.max_gross().min(((GRAD_SOL - s.real) as u128 * 10_000 / 9_900) as u64 + 1).max(1));
        }
        assert!(s.real >= GRAD_SOL && s.real < GRAD_SOL + 9 * SOL);
        let sold = s.sold() as f64 / 1e9;
        assert!((sold - 800_000_000.0).abs() / 800_000_000.0 < 0.01, "sold {sold}");
        // Final curve price ~= Raydium listing price (real - 5 SOL) / unsold.
        let unsold = (TOTAL_SUPPLY - s.sold()) as f64;
        let lp_price = (s.real - OPS_SOL) as f64 / unsold;
        let gap = (s.price() - lp_price).abs() / lp_price;
        assert!(gap < 0.01, "curve {} vs lp {} gap {:.4}", s.price(), lp_price, gap);
    }

    #[test]
    fn start_price_and_multiple() {
        let s = Sim::new();
        let p0 = s.price();
        assert!((p0 - 1.4286e-8).abs() / 1.4286e-8 < 0.01, "start {p0}");
        // Exactly-40 raise → 12.25x by construction (16 -> 56 virtual SOL).
        let mut s = Sim::new();
        while !s.frozen {
            s.buy(s.max_gross().min(((GRAD_SOL - s.real) as u128 * 10_000 / 9_900) as u64 + 1).max(1));
        }
        let exact = s.price() / p0;
        assert!(exact > 11.7 && exact < 12.8, "exact-raise multiple {exact}");
        // Max-chunk buys overshoot the target (the last cap-sized wallet lands
        // past it): higher final price, and the overshoot goes to the LP side.
        // Extreme case: a full 4% bought right at the boundary → exactly 16x.
        let mut s = Sim::new();
        while !s.frozen {
            s.buy(s.max_gross());
        }
        let mult = s.price() / p0;
        assert!(mult > 11.7 && mult < 16.1, "overshoot multiple {mult}");
        // Overshoot can only list the Raydium pool ABOVE the final curve
        // price (never below): no dump-arb at migration.
        let unsold = (TOTAL_SUPPLY - s.sold()) as f64;
        let lp_price = (s.real - OPS_SOL) as f64 / unsold;
        assert!(lp_price >= s.price() * 0.999, "lp {lp_price} below curve {}", s.price());
    }

    #[test]
    fn round_trip_never_profits() {
        let mut s = Sim::new();
        s.buy(s.max_gross() / 2); // someone before us
        for gross in [SOL / 100, SOL / 2, s.max_gross()] {
            let mut t = Sim { x: s.x, y: s.y, real: s.real, frozen: false };
            let out = t.buy(gross);
            let back = t.sell(out);
            assert!(back < gross, "profit on round trip: in {gross} out {back}");
            // ~2% fees + price impact; tiny buys lose ~2%, whales more.
            assert!((back as f64 / gross as f64) > 0.90);
        }
    }

    #[test]
    fn price_monotone_and_k_never_shrinks() {
        let mut s = Sim::new();
        let mut k_last = s.x as u128 * s.y as u128;
        let mut p_last = s.price();
        let mut bag = 0u64;
        for i in 0..200u64 {
            if i % 3 == 2 && bag > 1_000 {
                let units = bag / 2;
                bag -= units;
                s.sell(units);
                assert!(s.price() <= p_last, "sell must not raise price");
            } else {
                bag += s.buy(SOL / 10 + (i % 7) * SOL / 20);
                assert!(s.price() >= p_last, "buy must not lower price");
            }
            let k = s.x as u128 * s.y as u128;
            assert!(k >= k_last, "k shrank at step {i}");
            k_last = k;
            p_last = s.price();
            if s.frozen {
                break;
            }
        }
    }

    #[test]
    fn full_exit_returns_all_real_sol_minus_fees() {
        let mut s = Sim::new();
        let mut bag = 0u64;
        for _ in 0..10 {
            bag += s.buy(SOL / 2);
        }
        let real_before = s.real;
        let got = s.sell(bag);
        // Everything sellable came back out; pool keeps only rounding dust.
        assert!(s.real <= 10, "real stranded: {}", s.real);
        assert!(got <= real_before);
    }

    #[test]
    fn vault_always_covers_curve_sales() {
        // Selling the whole curve allocation must never exceed the premint.
        let mut s = Sim::new();
        let mut sold_total = 0u64;
        while !s.frozen {
            sold_total += s.buy(s.max_gross());
        }
        assert!(sold_total <= TOTAL_SUPPLY);
        assert!(sold_total == s.sold());
    }

    #[test]
    fn wallet_cap_binds_hardest_at_genesis() {
        // A SOL-sized cap would be toothless early: a genesis 5 SOL buy would
        // sweep ~26% of the supply. The 4% wallet cap forbids exactly that.
        let net = 5 * SOL - 5 * SOL * FEE_BPS / 10_000;
        let grab = buy_out(VIRT_SOL0, VIRT_TOK0, net).unwrap();
        assert!(grab > TOTAL_SUPPLY / 4, "genesis 5 SOL grabs {grab}");
        assert!(grab > MAX_WALLET_SUPPLY, "cap must reject the genesis whale");
        // A full 4% position costs ~0.59 SOL at genesis, ~8 SOL at the end.
        let s = Sim::new();
        let g0 = s.max_gross();
        assert!(g0 > SOL / 2 && g0 < SOL * 7 / 10, "genesis max {g0}");
        let mut s = Sim::new();
        while !s.frozen {
            s.buy(s.max_gross().min(((GRAD_SOL - s.real) as u128 * 10_000 / 9_900) as u64 + 1).max(1));
        }
        let mut t = Sim { x: s.x, y: s.y, real: 0, frozen: false };
        let g_end = t.max_gross();
        assert!(g_end > 15 * SOL / 2 && g_end < 17 * SOL / 2, "end max {g_end}");
        // 20 wallets minimum to fill the curve: 800M sold / 40M cap.
        assert_eq!(MAX_WALLET_SUPPLY, TOTAL_SUPPLY / 25);
    }
}
