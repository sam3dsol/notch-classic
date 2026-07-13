// NOTCH Classic migrator.
//
// Watches every Classic pool. When one graduates (real_sol >= 40 SOL, so the
// program has set frozen=true, migrated=false) it:
//   1. calls the on-chain Migrate instruction with the MIGRATOR key: 5 SOL ops
//      -> platform wallet, the remaining ~35 SOL + all unsold tokens -> this
//      migrator wallet (real System/SPL transfers, explorer-visible).
//   2. pulls the Raydium pool-creation cost (~0.4 SOL: 0.15 create fee + rents
//      + gas) FROM the platform fee wallet, so the whole bonding SOL becomes
//      pool depth. The 5 SOL ops = 4 DexScreener listing + 1 migration + MM.
//   3. creates a Raydium CPMM pool priced at the graduation price
//      (~35 SOL : ~200M tokens, i.e. the exact price the curve closed at, so
//      there is no arb gap), depositing all the received bonding SOL + tokens.
//   4. permanently LOCKS 100% of the LP via Raydium's Burn & Earn program as
//      TWO 50/50 positions: the liquidity can never be withdrawn (approved lock
//      exception, users' liquidity), and the pool's trading fees split 50/50
//      via two Fee Key NFTs — one to the platform wallet, one to the token's
//      creator — both claimable forever.
//   5. records the migration + WhatsApp-alerts.
//
// HARVEST=1 mode: MANUAL ONLY (operator runs it by hand, never auto/cron).
// Claims accrued fees from each pool's PLATFORM Fee Key NFT to the platform
// wallet, then exits. (The creator harvests their own half themselves.) Fees arrive as BOTH token and SOL (constant-
// product pools skim the fee from each swap's input side). Claiming does NOT
// swap or dump — it just credits the wallet; the operator keeps the SOL and
// decides separately whether to hold or convert the token portion.
//
// Safety: DRY_RUN=1 by default (simulates, never signs a live migrate/pool).
// Arm with DRY_RUN=0 once a real pool is close to graduating.

import { Connection, Keypair, PublicKey, Transaction, TransactionInstruction, SystemProgram, ComputeBudgetProgram } from '@solana/web3.js';
import { getAssociatedTokenAddressSync, TOKEN_PROGRAM_ID } from '@solana/spl-token';
import fs from 'fs';

const RPC = process.env.RPC;
if (!RPC) { console.error('FATAL: RPC not set'); process.exit(1); }
const DRY = process.env.DRY_RUN !== '0';
const POLL_MS = Number(process.env.POLL_MS || 60000);
const STORE = process.env.POOL_STORE || '/root/notch-classic-pools.json';
const DONE = '/root/notch-classic-migrated.json';
const conn = new Connection(RPC, 'confirmed');

const PROGRAM = new PublicKey('rqPbThPVCPKgoK823z6gFbn2EcP7QmQRqn297LC1ass');
const TOKEN = TOKEN_PROGRAM_ID;
const WSOL = new PublicKey('So11111111111111111111111111111111111111112');
const PLATFORM = new PublicKey('4Dz6JuP3M4LMCH9mandbULfZ8nt3S2LvWtwn489vEwuL');
const INCINERATOR = new PublicKey('1nc1nerator11111111111111111111111111111111');
const OPS_SOL = 5_000_000_000n;

const kp = (p) => Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(p, 'utf8'))));
const migrator = kp(process.env.MIGRATOR_KP || '/root/vault/notch-classic/migrator-keypair.json');
if (migrator.publicKey.toBase58() !== '3tUp4eSggj6PmHkL4jY1JmBrQUGzbdMdcZx3N7g1UiEM') {
  console.error('FATAL: migrator key mismatch'); process.exit(1);
}
// The platform fee wallet receives the 5 SOL ops carve-out at each migration
// (4 SOL DexScreener listing + 1 SOL migration mechanics + market making). The
// migrator draws its Raydium pool-creation cost from this wallet, so the whole
// bonding-curve SOL goes into the pool as depth.
const platform = kp(process.env.PLATFORM_KP || '/root/vault/notch-classic/platform-fee-keypair.json');
if (platform.publicKey.toBase58() !== PLATFORM.toBase58()) {
  console.error('FATAL: platform key mismatch'); process.exit(1);
}
// Migration mechanical cost pulled from the platform wallet per graduation:
// Raydium CPMM creation fee (0.15) + pool account rents (~0.25) + gas margin.
const MIGRATION_COST = 500_000_000n; // 0.5 SOL

const enc = (s) => new TextEncoder().encode(s);
const poolPda = (mint) => PublicKey.findProgramAddressSync([enc('pool'), mint.toBytes()], PROGRAM)[0];
const payoutPda = () => PublicKey.findProgramAddressSync([enc('payout')], PROGRAM)[0];
const ata = (owner, mint) => getAssociatedTokenAddressSync(mint, owner, true);
const readLE = (a, o, n) => { let x = 0n; for (let i = n - 1; i >= 0; i--) x = (x << 8n) | BigInt(a[o + i]); return x; };

function parsePool(data) {
  const d = new Uint8Array(data);
  if (d.length < 91) return null;
  return {
    mint: new PublicKey(d.slice(0, 32)),
    creator: new PublicKey(d.slice(32, 64)),
    virtSol: readLE(d, 64, 8), virtTok: readLE(d, 72, 8), realSol: readLE(d, 80, 8),
    frozen: d[88] === 1, migrated: d[89] === 1,
  };
}

function ixMigrate(mint) {
  const pda = poolPda(mint);
  return new TransactionInstruction({ programId: PROGRAM, keys: [
    { pubkey: migrator.publicKey, isSigner: true, isWritable: true },
    { pubkey: pda, isSigner: false, isWritable: true },
    { pubkey: mint, isSigner: false, isWritable: false },
    { pubkey: ata(pda, mint), isSigner: false, isWritable: true },
    { pubkey: ata(migrator.publicKey, mint), isSigner: false, isWritable: true },
    { pubkey: PLATFORM, isSigner: false, isWritable: true },
    { pubkey: TOKEN, isSigner: false, isWritable: false },
    { pubkey: payoutPda(), isSigner: false, isWritable: true },
    { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
  ], data: new Uint8Array([3]) });
}

async function alert(text) {
  try {
    await fetch('http://127.0.0.1:8787/send', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text: 'NOTCH Classic migrator: ' + text }),
    });
  } catch (e) {}
}

const loadDone = () => { try { return JSON.parse(fs.readFileSync(DONE, 'utf8')); } catch (e) { return {}; } };
const saveDone = (o) => { try { fs.writeFileSync(DONE + '.tmp', JSON.stringify(o)); fs.renameSync(DONE + '.tmp', DONE); } catch (e) {} };

async function sendTx(ixs, signers, feePayer) {
  const tx = new Transaction();
  ixs.forEach((i) => tx.add(i));
  tx.feePayer = (feePayer || migrator).publicKey;
  tx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
  tx.sign(...signers);
  const sig = await conn.sendRawTransaction(tx.serialize(), { skipPreflight: false });
  await conn.confirmTransaction(sig, 'confirmed');
  return sig;
}

// Create the Raydium CPMM pool, then permanently LOCK the LP via Raydium's
// Burn & Earn program. Locking (not burning) keeps the liquidity un-withdrawable
// forever AND mints a Fee Key NFT — sent to the platform wallet — that harvests
// the pool's trading fees perpetually. Loaded lazily so the daemon starts even
// if the (heavy) Raydium SDK is absent; only needed at migration.
async function raydiumListAndLock(mint, tokenAmount, solAmount, creator) {
  const { Raydium, TxVersion, CREATE_CPMM_POOL_PROGRAM, CREATE_CPMM_POOL_FEE_ACC, getCpmmPdaPoolId } = await import('@raydium-io/raydium-sdk-v2');
  const { default: BN } = await import('bn.js');
  const raydium = await Raydium.load({ connection: conn, owner: migrator, cluster: 'mainnet' });
  const ammConfigs = await raydium.api.getCpmmConfigs();
  const feeConfig = ammConfigs[0];

  // SAFETY: only ever CREATE a fresh pool, never deposit into an existing one.
  // CPMM pool ids are deterministic from (program, config, sorted mints); if
  // our canonical pool already exists (e.g. a squatter front-ran the listing),
  // abort and leave it for the operator — never add liquidity to someone
  // else's pool.
  const [a, b] = [mint, WSOL].sort((x, y) => (x.toBuffer().compare(y.toBuffer())));
  const poolId = getCpmmPdaPoolId(CREATE_CPMM_POOL_PROGRAM, new PublicKey(feeConfig.id), a, b).publicKey;
  if (await conn.getAccountInfo(poolId)) {
    throw new Error(`Raydium pool ${poolId.toBase58()} already exists for ${mint.toBase58()} — refusing to add liquidity, operator must handle`);
  }

  const { execute, extInfo } = await raydium.cpmm.createPool({
    programId: CREATE_CPMM_POOL_PROGRAM,
    poolFeeAccount: CREATE_CPMM_POOL_FEE_ACC,
    mintA: { address: mint.toBase58(), programId: TOKEN.toBase58(), decimals: 9 },
    mintB: { address: WSOL.toBase58(), programId: TOKEN.toBase58(), decimals: 9 },
    mintAAmount: BigInt(tokenAmount),
    mintBAmount: BigInt(solAmount),
    startTime: 0,
    feeConfig,
    associatedOnly: false,
    ownerInfo: { useSOLBalance: true },
    txVersion: TxVersion.V0,
  });
  const { txId } = await execute({ sendAndConfirm: true });
  const lpMint = new PublicKey(extInfo.address.lpMint);

  // permanently LOCK 100% of the LP via Burn & Earn, as TWO 50/50 positions:
  // half's Fee Key NFT -> the platform wallet, half -> the token's creator. So
  // the graduated pool's trading fees split 50/50 between platform and creator,
  // forever, while the liquidity itself can never be withdrawn.
  const lpAta = ata(migrator.publicKey, lpMint);
  const lpBal = BigInt((await conn.getTokenAccountBalance(lpAta)).value.amount);
  const halfPlatform = lpBal / 2n;
  const halfCreator = lpBal - halfPlatform; // creator gets the odd unit, negligible
  const { poolInfo } = await raydium.cpmm.getPoolInfoFromRpc(extInfo.address.poolId);
  const lock = async (amount, owner) => {
    const { execute: ex, extInfo: li } = await raydium.cpmm.lockLp({
      poolInfo, lpAmount: new BN(amount.toString()), withMetadata: true, feeNftOwner: owner, txVersion: TxVersion.V0,
    });
    const { txId: t } = await ex({ sendAndConfirm: true });
    return { tx: t, nft: li?.nftMint?.toBase58?.() || null };
  };
  const plat = await lock(halfPlatform, PLATFORM);
  const crea = await lock(halfCreator, creator);
  return {
    poolId: extInfo.address.poolId, lpMint: lpMint.toBase58(), createTx: txId, lpLocked: lpBal.toString(),
    lockTxPlatform: plat.tx, feeNftPlatform: plat.nft, feeOwnerPlatform: PLATFORM.toBase58(),
    lockTxCreator: crea.tx, feeNftCreator: crea.nft, feeOwnerCreator: creator.toBase58(),
  };
}

async function migrateOne(rec) {
  const mint = new PublicKey(rec.mint);
  const pda = poolPda(mint);
  const acc = await conn.getAccountInfo(pda);
  if (!acc) return;
  const st = parsePool(acc.data);
  if (!st || !st.frozen || st.migrated) return;

  console.log(`[${new Date().toISOString()}] ${rec.ticker} (${rec.mint}) graduated, real=${Number(st.realSol) / 1e9} SOL`);
  await alert(`${rec.ticker} graduated at ${(Number(st.realSol) / 1e9).toFixed(2)} SOL, migrating…`);

  if (DRY) {
    // simulate the migrate ix only
    const tx = new Transaction().add(ixMigrate(mint));
    tx.feePayer = migrator.publicKey;
    tx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
    const sim = await conn.simulateTransaction(tx);
    console.log(`  DRY_RUN migrate sim err: ${JSON.stringify(sim.value.err)}`);
    return;
  }

  // 1. on-chain migrate: 5 SOL ops -> platform wallet, the remaining bonding SOL
  //    + all unsold tokens -> migrator wallet.
  const solBefore = await conn.getBalance(migrator.publicKey);
  const migSig = await sendTx([ComputeBudgetProgram.setComputeUnitLimit({ units: 120000 }), ixMigrate(mint)], [migrator]);
  console.log(`  migrated on-chain: ${migSig}`);
  const tokBal = (await conn.getTokenAccountBalance(ata(migrator.publicKey, mint))).value.amount;
  const solAfterMigrate = await conn.getBalance(migrator.publicKey);
  // Pure bonding SOL the migrator received; ALL of it goes into the pool as depth.
  const solForPool = BigInt(Math.max(0, solAfterMigrate - solBefore));

  // 2. cover the Raydium pool-creation cost from the platform fee wallet (from
  //    the 5 SOL ops it just received), so no bonding SOL is spent on mechanics.
  const topSig = await sendTx(
    [SystemProgram.transfer({ fromPubkey: platform.publicKey, toPubkey: migrator.publicKey, lamports: Number(MIGRATION_COST) })],
    [platform], platform);
  console.log(`  pulled ${Number(MIGRATION_COST) / 1e9} SOL migration cost from platform wallet: ${topSig}`);

  // 3+4. Raydium pool at the graduation price + lock LP 50/50 (platform + creator)
  const r = await raydiumListAndLock(mint, tokBal, solForPool, st.creator);
  console.log(`  Raydium pool ${r.poolId} created ${r.createTx}; LP locked 50/50 — platform NFT ${r.feeNftPlatform}, creator NFT ${r.feeNftCreator} -> ${r.feeOwnerCreator}`);

  const done = loadDone();
  done[rec.mint] = { ...rec, migrateTx: migSig, ...r, ts: Math.floor(Date.now() / 1000) };
  saveDone(done);
  await alert(`${rec.ticker} live on Raydium (pool ${r.poolId}), LP locked. Fees split 50/50 platform + creator. ${solForPool / 1_000_000_000n} SOL + tokens deposited.`);
}

// HARVEST mode: claim accrued trading fees from every locked pool's Fee Key NFT
// to the platform wallet. MANUAL ONLY — the operator runs this by hand; it is
// never auto-scheduled. Fees arrive as both token and SOL; claiming is not a
// swap, so it never touches the pool price.
async function harvestAll() {
  const { Raydium, TxVersion } = await import('@raydium-io/raydium-sdk-v2');
  const raydium = await Raydium.load({ connection: conn, owner: platform, cluster: 'mainnet' });
  const done = loadDone();
  let claimed = 0;
  for (const rec of Object.values(done)) {
    // only the PLATFORM's half — the creator owns their own NFT and harvests it
    // themselves from their Raydium portfolio.
    if (!rec.feeNftPlatform) continue;
    try {
      const { execute } = await raydium.cpmm.harvestLockLp({ nftMint: new PublicKey(rec.feeNftPlatform), lpFeeAmount: null, txVersion: TxVersion.V0 });
      const { txId } = await execute({ sendAndConfirm: true });
      console.log(`  harvested ${rec.ticker} platform-half fees: ${txId}`);
      claimed++;
    } catch (e) { console.error(`  harvest ${rec.ticker} failed:`, e.message); }
  }
  console.log(`harvest done: ${claimed} pool(s) claimed to platform ${platform.publicKey.toBase58()}`);
}

async function tick() {
  let pools = [];
  try { pools = JSON.parse(fs.readFileSync(STORE, 'utf8')); } catch (e) { return; }
  const done = loadDone();
  for (const rec of pools) {
    if (done[rec.mint]) continue;
    try { await migrateOne(rec); } catch (e) { console.error(`  migrate ${rec.ticker} failed:`, e.message); await alert(`FAILED migrating ${rec.ticker}: ${e.message}`); }
  }
}

if (process.env.HARVEST === '1') {
  console.log('NOTCH Classic migrator: HARVEST mode');
  harvestAll().then(() => process.exit(0)).catch((e) => { console.error(e); process.exit(1); });
} else {
  console.log(`NOTCH Classic migrator up. DRY_RUN=${DRY ? 1 : 0}, poll ${POLL_MS}ms, migrator ${migrator.publicKey.toBase58()}`);
  tick();
  setInterval(tick, POLL_MS);
}
