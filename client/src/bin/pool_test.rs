//! NOTCH Classic integration suite, against a local validator running the
//! dev-mint build. Proves on-chain: create gates + fixed supply, buy gates +
//! wallet cap + exact math + exact fees, sell math + creator fee, graduation
//! freeze, and migration payouts.
//!
//! Env: RPC (default localhost), PROGRAM, PAYER, MIGRATOR_KP (default
//! ~/vault/notch-classic/migrator-keypair.json), DEV_NOTCH_KP (default
//! dev-notch-mint.json at repo root or cwd).

use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use notch_classic_client::{pool, rpc::Rpc};

const SOL: u64 = 1_000_000_000;

fn load_kp(path: &str) -> Keypair {
    let bytes: Vec<u8> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    Keypair::from_bytes(&bytes).unwrap()
}

async fn send(rpc: &Rpc, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair]) -> Result<String, String> {
    let bh = rpc.blockhash().await;
    let msg = solana_sdk::message::Message::new(ixs, Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    tx.sign(signers, bh);
    rpc.send(&tx).await
}

/// Largest gross a fresh wallet can spend within the 4% cap (mirror of the
/// frontend sizing; margin for rounding).
fn max_gross(x: u64, y: u64) -> u64 {
    let k = x as u128 * y as u128;
    let net = (k / (y - pool::MAX_WALLET_SUPPLY) as u128).saturating_sub(x as u128) as u64;
    ((net as u128 * 10_000 / (10_000 - pool::FEE_BPS as u128)) as u64).saturating_sub(10_000).max(1)
}

#[tokio::main]
async fn main() {
    let url = std::env::var("RPC").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
    let program = Pubkey::from_str(&std::env::var("PROGRAM").expect("PROGRAM env required")).unwrap();
    let rpc = Rpc::new(&url);
    let payer = load_kp(&std::env::var("PAYER").expect("PAYER env required"));
    let migrator_kp = load_kp(&std::env::var("MIGRATOR_KP").unwrap_or_else(|_| {
        format!("{}/vault/notch-classic/migrator-keypair.json", std::env::var("HOME").unwrap())
    }));
    assert_eq!(migrator_kp.pubkey(), pool::migrator(), "MIGRATOR_KP must match the hardcoded MIGRATOR");

    let mut pass = 0u32;
    let mut fail = 0u32;
    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond { pass += 1; println!("PASS  {}", $name); } else { fail += 1; println!("FAIL  {}", $name); }
        };
    }

    // --- setup ---------------------------------------------------------------
    if rpc.balance(&payer.pubkey()).await < 700 * SOL {
        rpc.airdrop(&payer.pubkey(), 1_000 * SOL).await.expect("airdrop");
    }
    let mint_rent = rpc.min_balance(pool::MINT_SIZE).await;

    // Dev NOTCH stand-in mint (authority = payer).
    let notch_kp = load_kp(&std::env::var("DEV_NOTCH_KP").unwrap_or_else(|_| {
        if std::path::Path::new("dev-notch-mint.json").exists() { "dev-notch-mint.json".into() } else { "../dev-notch-mint.json".into() }
    }));
    let notch_mint = notch_kp.pubkey();
    assert_eq!(notch_mint, pool::dev_notch_mint(), "dev keypair must match the dev-mint const");
    if rpc.account_data(&notch_mint).await.is_none() {
        send(&rpc, &pool::plain_mint_ixs(&payer.pubkey(), &notch_mint, &payer.pubkey(), mint_rent), &payer, &[&payer, &notch_kp])
            .await.expect("create dev NOTCH");
    }
    // Fund a wallet with SOL and `notch` units of NOTCH; returns its keypair.
    let fund = |sol: u64, notch: u64| {
        let rpc = Rpc::new(&url);
        let payer = load_kp(&std::env::var("PAYER").unwrap());
        async move {
            let w = Keypair::new();
            send(&rpc, &[solana_sdk::system_instruction::transfer(&payer.pubkey(), &w.pubkey(), sol)], &payer, &[&payer]).await.expect("fund sol");
            if notch > 0 {
                let ta = pool::ata(&w.pubkey(), &pool::dev_notch_mint());
                send(&rpc, &[pool::create_ata_ix(&payer.pubkey(), &w.pubkey(), &pool::dev_notch_mint()),
                             pool::mint_to_ix(&pool::dev_notch_mint(), &ta, &payer.pubkey(), notch)], &payer, &[&payer]).await.expect("fund notch");
            }
            w
        }
    };

    let creator = fund(3 * SOL, pool::MIN_CREATE_NOTCH).await; // exactly 0.1
    let buyer = fund(5 * SOL, pool::MIN_BUY_NOTCH).await; // exactly 0.05
    let poor = fund(3 * SOL, pool::MIN_BUY_NOTCH - 10_000_000).await; // 0.04
    let whale = fund(30 * SOL, pool::MIN_BUY_NOTCH).await;
    let creator_notch = pool::ata(&creator.pubkey(), &notch_mint);
    let buyer_notch = pool::ata(&buyer.pubkey(), &notch_mint);
    let poor_notch = pool::ata(&poor.pubkey(), &notch_mint);
    let whale_notch = pool::ata(&whale.pubkey(), &notch_mint);

    // Token mint candidate (authority = pool PDA) + pool vault ATA.
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let (pda, _) = pool::pool_pda(&program, &mint);
    send(&rpc, &pool::create_pool_mint_ixs(&program, &payer.pubkey(), &mint, mint_rent), &payer, &[&payer, &mint_kp]).await.expect("mint");
    send(&rpc, &[pool::create_ata_ix(&payer.pubkey(), &pda, &mint)], &payer, &[&payer]).await.expect("vault ata");
    let _vault = pool::ata(&pda, &mint);

    let pool_state = |mint: Pubkey| {
        let rpc = Rpc::new(&url);
        let program = program;
        async move {
            let (pda, _) = pool::pool_pda(&program, &mint);
            pool::parse_pool(&rpc.account_data(&pda).await.unwrap_or_default())
        }
    };
    let bag = |owner: Pubkey, m: Pubkey| {
        let rpc = Rpc::new(&url);
        async move { match rpc.account_data(&pool::ata(&owner, &m)).await { Some(d) => pool::token_amount(&d), None => 0 } }
    };

    // --- 1) CREATE hold-gate -------------------------------------------------
    let r = send(&rpc, &[pool::create(&program, &poor.pubkey(), &mint, &poor_notch)], &poor, &[&poor]).await;
    check!("CREATE: 0.04 NOTCH rejected (< 0.1)", r.is_err());
    let r = send(&rpc, &[pool::create(&program, &buyer.pubkey(), &mint, &buyer_notch)], &buyer, &[&buyer]).await;
    check!("CREATE: 0.05 NOTCH rejected (< 0.1)", r.is_err());
    let r = send(&rpc, &[pool::create(&program, &creator.pubkey(), &mint, &creator_notch)], &creator, &[&creator]).await;
    check!("CREATE: 0.1 NOTCH creates the pool", r.is_ok());
    let p0 = pool_state(mint).await;
    check!("CREATE: pool state correct", matches!(&p0, Some(p) if p.mint == mint && p.creator == creator.pubkey()
        && p.virt_sol == pool::VIRT_SOL0 && p.virt_tok == pool::VIRT_TOK0 && p.real_sol == 0 && !p.frozen && !p.migrated));
    let vault_bal = bag(pda, mint).await;
    check!("CREATE: full 1B supply preminted to the vault", vault_bal == pool::TOTAL_SUPPLY);
    let md = rpc.account_data(&mint).await.unwrap();
    check!("CREATE: supply fixed forever (mint authority None)", pool::mint_authority_tag(&md) == 0 && pool::mint_supply(&md) == pool::TOTAL_SUPPLY);
    let r = send(&rpc, &[pool::create(&program, &creator.pubkey(), &mint, &creator_notch)], &creator, &[&creator]).await;
    check!("CREATE: re-create rejected", r.is_err());
    check!("CREATE: creator NOTCH held not spent", { let d = rpc.account_data(&creator_notch).await.unwrap(); pool::token_amount(&d) == pool::MIN_CREATE_NOTCH });

    // --- 2) BUY hold-gate + exact math + exact fee ----------------------------
    send(&rpc, &[pool::create_ata_ix(&poor.pubkey(), &poor.pubkey(), &mint)], &poor, &[&poor]).await.expect("poor ata");
    let r = send(&rpc, &[pool::buy(&program, &poor.pubkey(), &mint, &poor_notch, SOL / 2, 0)], &poor, &[&poor]).await;
    check!("BUY: 0.04 NOTCH rejected (< 0.05)", r.is_err());

    send(&rpc, &[pool::create_ata_ix(&buyer.pubkey(), &buyer.pubkey(), &mint)], &buyer, &[&buyer]).await.expect("buyer ata");
    let st = pool_state(mint).await.unwrap();
    let gross = SOL / 2;
    let fee = gross * pool::FEE_BPS / 10_000;
    let exp_out = pool::buy_out(st.virt_sol, st.virt_tok, gross - fee);
    let plat0 = rpc.balance(&pool::platform_wallet()).await;
    let pda0 = rpc.balance(&pda).await;
    let r = send(&rpc, &[pool::buy(&program, &buyer.pubkey(), &mint, &buyer_notch, gross, exp_out)], &buyer, &[&buyer]).await;
    check!("BUY: 0.05 NOTCH buys", r.is_ok());
    check!("BUY: exact token out (mirror match)", bag(buyer.pubkey(), mint).await == exp_out);
    check!("BUY: exact 1% fee to the platform wallet", rpc.balance(&pool::platform_wallet()).await == plat0 + fee);
    check!("BUY: net landed in the pool", rpc.balance(&pda).await == pda0 + (gross - fee));
    let st = pool_state(mint).await.unwrap();
    check!("BUY: state advanced (x+net, y-out, real+net)", st.virt_sol == pool::VIRT_SOL0 + (gross - fee) && st.virt_tok == pool::VIRT_TOK0 - exp_out && st.real_sol == gross - fee);
    let r = send(&rpc, &[pool::buy(&program, &buyer.pubkey(), &mint, &buyer_notch, SOL / 10, u64::MAX)], &buyer, &[&buyer]).await;
    check!("BUY: min_out too high rejected", r.is_err());

    // --- 3) 4% wallet cap ------------------------------------------------------
    send(&rpc, &[pool::create_ata_ix(&whale.pubkey(), &whale.pubkey(), &mint)], &whale, &[&whale]).await.expect("whale ata");
    let r = send(&rpc, &[pool::buy(&program, &whale.pubkey(), &mint, &whale_notch, 5 * SOL, 0)], &whale, &[&whale]).await;
    check!("CAP: genesis 5 SOL buy (would sweep ~26%) rejected", r.is_err());
    let st = pool_state(mint).await.unwrap();
    let g = max_gross(st.virt_sol, st.virt_tok);
    let r = send(&rpc, &[pool::buy(&program, &whale.pubkey(), &mint, &whale_notch, g, 0)], &whale, &[&whale]).await;
    check!("CAP: cap-sized buy accepted", r.is_ok());
    let wb = bag(whale.pubkey(), mint).await;
    check!("CAP: whale holds <= 4% of supply", wb <= pool::MAX_WALLET_SUPPLY && wb > pool::MAX_WALLET_SUPPLY * 99 / 100);
    let r = send(&rpc, &[pool::buy(&program, &whale.pubkey(), &mint, &whale_notch, SOL / 2, 0)], &whale, &[&whale]).await;
    check!("CAP: topping past the wallet cap rejected", r.is_err());

    // --- 4) SELL exact math + creator fee --------------------------------------
    let st = pool_state(mint).await.unwrap();
    let units = bag(buyer.pubkey(), mint).await / 2;
    let exp_gross = pool::sell_out(st.virt_sol, st.virt_tok, units);
    let exp_fee = exp_gross * pool::FEE_BPS / 10_000;
    let cre0 = rpc.balance(&creator.pubkey()).await;
    let buy0 = rpc.balance(&buyer.pubkey()).await;
    let r = send(&rpc, &[pool::sell(&program, &buyer.pubkey(), &mint, &creator.pubkey(), units, exp_gross - exp_fee)], &buyer, &[&buyer]).await;
    check!("SELL: succeeds", r.is_ok());
    check!("SELL: exact payout (gross - 1%)", rpc.balance(&buyer.pubkey()).await + 5_000 - buy0 == exp_gross - exp_fee);
    check!("SELL: exact 1% to the pool creator", rpc.balance(&creator.pubkey()).await == cre0 + exp_fee);
    let st2 = pool_state(mint).await.unwrap();
    check!("SELL: state reversed (x-gross, y+units, real-gross)", st2.virt_sol == st.virt_sol - exp_gross && st2.virt_tok == st.virt_tok + units && st2.real_sol == st.real_sol - exp_gross);
    let r = send(&rpc, &[pool::sell(&program, &buyer.pubkey(), &mint, &whale.pubkey(), 100, 0)], &buyer, &[&buyer]).await;
    check!("SELL: wrong creator account rejected", r.is_err());

    // --- 4b) SELL via the payout PDA route (explorer-visible System transfers) --
    {
        let st = pool_state(mint).await.unwrap();
        let units = bag(buyer.pubkey(), mint).await / 2;
        let exp_gross = pool::sell_out(st.virt_sol, st.virt_tok, units);
        let exp_fee = exp_gross * pool::FEE_BPS / 10_000;
        let cre0 = rpc.balance(&creator.pubkey()).await;
        let buy0 = rpc.balance(&buyer.pubkey()).await;
        let sig = send(&rpc, &[pool::sell_via_payout(&program, &buyer.pubkey(), &mint, &creator.pubkey(), units, exp_gross - exp_fee)], &buyer, &[&buyer]).await;
        check!("PAYOUT-ROUTE sell succeeds", sig.is_ok());
        check!("PAYOUT-ROUTE seller paid exact 94%… (gross - 1%)", rpc.balance(&buyer.pubkey()).await + 5_000 - buy0 == exp_gross - exp_fee);
        check!("PAYOUT-ROUTE creator paid exact 1%", rpc.balance(&creator.pubkey()).await == cre0 + exp_fee);
        check!("PAYOUT-ROUTE payout PDA left empty", rpc.balance(&pool::payout_pda(&program)).await == 0);
        // The System transfers are visible in the tx: assert the log shows a System transfer program invocation.
        if let Ok(s) = sig {
            let tx = rpc.get_transaction(&s).await;
            check!("PAYOUT-ROUTE tx carries System transfer(s)", tx.contains("11111111111111111111111111111111"));
        }
    }

    // --- 5) fill to graduation --------------------------------------------------
    let mut fillers: Vec<Keypair> = Vec::new();
    let mut frozen = false;
    for i in 0..40 {
        let st = pool_state(mint).await.unwrap();
        if st.frozen { frozen = true; break; }
        let g = max_gross(st.virt_sol, st.virt_tok);
        let w = fund(g + SOL, pool::MIN_BUY_NOTCH).await;
        let wn = pool::ata(&w.pubkey(), &notch_mint);
        send(&rpc, &[pool::create_ata_ix(&w.pubkey(), &w.pubkey(), &mint), pool::buy(&program, &w.pubkey(), &mint, &wn, g, 0)], &w, &[&w])
            .await.unwrap_or_else(|e| panic!("fill buy {i}: {e}"));
        fillers.push(w);
    }
    let st = pool_state(mint).await.unwrap();
    check!("GRAD: pool froze at the 40 SOL raise", (frozen || st.frozen) && st.real_sol >= pool::GRAD_SOL);
    println!("      raised {} SOL over {} wallets (overshoot {})", st.real_sol as f64 / 1e9, fillers.len() + 2, (st.real_sol - pool::GRAD_SOL) as f64 / 1e9);
    let last = fillers.last().unwrap();
    let ln = pool::ata(&last.pubkey(), &notch_mint);
    let r = send(&rpc, &[pool::buy(&program, &last.pubkey(), &mint, &ln, SOL / 10, 0)], last, &[last]).await;
    check!("GRAD: buys closed after freeze", r.is_err());
    let lb = bag(last.pubkey(), mint).await;
    let r = send(&rpc, &[pool::sell(&program, &last.pubkey(), &mint, &creator.pubkey(), lb / 2, 0)], last, &[last]).await;
    check!("GRAD: sells closed after freeze", r.is_err());

    // --- 6) MIGRATE ---------------------------------------------------------------
    send(&rpc, &[solana_sdk::system_instruction::transfer(&payer.pubkey(), &migrator_kp.pubkey(), SOL)], &payer, &[&payer]).await.expect("fund migrator");
    send(&rpc, &[pool::create_ata_ix(&migrator_kp.pubkey(), &migrator_kp.pubkey(), &mint)], &migrator_kp, &[&migrator_kp]).await.expect("migrator ata");
    let r = send(&rpc, &[pool::migrate(&program, &payer.pubkey(), &mint)], &payer, &[&payer]).await;
    check!("MIGRATE: non-migrator signer rejected", r.is_err());
    let st = pool_state(mint).await.unwrap();
    let vault_tokens = bag(pda, mint).await;
    let plat0 = rpc.balance(&pool::platform_wallet()).await;
    let mig0 = rpc.balance(&migrator_kp.pubkey()).await;
    let r = send(&rpc, &[pool::migrate(&program, &migrator_kp.pubkey(), &mint)], &migrator_kp, &[&migrator_kp]).await;
    check!("MIGRATE: succeeds", r.is_ok());
    check!("MIGRATE: exact 5 SOL ops to the platform wallet", rpc.balance(&pool::platform_wallet()).await == plat0 + pool::OPS_SOL);
    check!("MIGRATE: remaining raise to the migrator", rpc.balance(&migrator_kp.pubkey()).await + 5_000 - mig0 == st.real_sol - pool::OPS_SOL);
    check!("MIGRATE: all unsold tokens to the migrator", bag(migrator_kp.pubkey(), mint).await == vault_tokens && bag(pda, mint).await == 0);
    let st = pool_state(mint).await.unwrap();
    check!("MIGRATE: pool terminal (migrated, real 0)", st.migrated && st.real_sol == 0);
    let r = send(&rpc, &[pool::migrate(&program, &migrator_kp.pubkey(), &mint)], &migrator_kp, &[&migrator_kp]).await;
    check!("MIGRATE: double-migrate rejected", r.is_err());

    // --- 7) MIGRATE before freeze rejected (fresh pool) ----------------------------
    let mk2 = Keypair::new();
    let m2 = mk2.pubkey();
    let (pda2, _) = pool::pool_pda(&program, &m2);
    send(&rpc, &pool::create_pool_mint_ixs(&program, &payer.pubkey(), &m2, mint_rent), &payer, &[&payer, &mk2]).await.expect("mint2");
    send(&rpc, &[pool::create_ata_ix(&payer.pubkey(), &pda2, &m2)], &payer, &[&payer]).await.expect("vault2");
    send(&rpc, &[pool::create(&program, &creator.pubkey(), &m2, &creator_notch)], &creator, &[&creator]).await.expect("create2");
    send(&rpc, &[pool::create_ata_ix(&migrator_kp.pubkey(), &migrator_kp.pubkey(), &m2)], &migrator_kp, &[&migrator_kp]).await.expect("migrator ata2");
    let r = send(&rpc, &[pool::migrate(&program, &migrator_kp.pubkey(), &m2)], &migrator_kp, &[&migrator_kp]).await;
    check!("MIGRATE: before freeze rejected", r.is_err());

    println!("\n==== {} passed, {} failed ====", pass, fail);
    if fail > 0 {
        std::process::exit(1);
    }
}
