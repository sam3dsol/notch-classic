// NOTCH Classic migrator.
//
// Watches every Classic pool. When one graduates (real_sol >= 40 SOL, so the
// program has set frozen=true, migrated=false) it:
//   1. calls the on-chain Migrate instruction with the MIGRATOR key: 5 SOL ops
//      -> platform wallet, the remaining ~35 SOL + all unsold tokens -> this
//      migrator wallet (real System/SPL transfers, explorer-visible).
//   2. creates a Raydium CPMM pool priced at the graduation price
//      (~35 SOL : ~200M tokens, i.e. the exact price the curve closed at, so
//      there is no arb gap), depositing the received SOL + tokens.
//   3. burns 100% of the LP to the incinerator, permanently locking the
//      liquidity (the approved exception to the never-lock-LP rule: this is
//      users' liquidity, not ours).
//   4. records the migration + WhatsApp-alerts.
//
// Safety: DRY_RUN=1 by default (simulates, never signs a live migrate/pool).
// Arm with DRY_RUN=0 once a real pool is close to graduating.

import { Connection, Keypair, PublicKey, Transaction, TransactionInstruction, SystemProgram, ComputeBudgetProgram } from '@solana/web3.js';
import { getAssociatedTokenAddressSync, createBurnInstruction, TOKEN_PROGRAM_ID } from '@solana/spl-token';
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

async function sendTx(ixs, signers) {
  const tx = new Transaction();
  ixs.forEach((i) => tx.add(i));
  tx.feePayer = migrator.publicKey;
  tx.recentBlockhash = (await conn.getLatestBlockhash()).blockhash;
  tx.sign(...signers);
  const sig = await conn.sendRawTransaction(tx.serialize(), { skipPreflight: false });
  await conn.confirmTransaction(sig, 'confirmed');
  return sig;
}

// Create the Raydium CPMM pool and burn the LP. Loaded lazily so the daemon
// starts even if the (heavy) Raydium SDK is absent; only needed at migration.
async function raydiumListAndLock(mint, tokenAmount, solAmount) {
  const { Raydium, TxVersion, CREATE_CPMM_POOL_PROGRAM, CREATE_CPMM_POOL_FEE_ACC, getCpmmPdaAmmConfigId } = await import('@raydium-io/raydium-sdk-v2');
  const raydium = await Raydium.load({ connection: conn, owner: migrator, cluster: 'mainnet' });
  const ammConfigs = await raydium.api.getCpmmConfigs();
  const feeConfig = ammConfigs[0];
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
  // burn 100% of our LP to lock the liquidity
  const lpAta = ata(migrator.publicKey, lpMint);
  const lpBal = (await conn.getTokenAccountBalance(lpAta)).value.amount;
  let burnSig = null;
  if (BigInt(lpBal) > 0n) {
    burnSig = await sendTx([createBurnInstruction(lpAta, lpMint, migrator.publicKey, BigInt(lpBal))], [migrator]);
  }
  return { poolId: extInfo.address.poolId, lpMint: lpMint.toBase58(), createTx: txId, burnTx: burnSig, lpBurned: lpBal };
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

  // 1. on-chain migrate: SOL + tokens land in the migrator wallet
  const solBefore = await conn.getBalance(migrator.publicKey);
  const migSig = await sendTx([ComputeBudgetProgram.setComputeUnitLimit({ units: 120000 }), ixMigrate(mint)], [migrator]);
  console.log(`  migrated on-chain: ${migSig}`);
  const tokBal = (await conn.getTokenAccountBalance(ata(migrator.publicKey, mint))).value.amount;
  const solAfter = await conn.getBalance(migrator.publicKey);
  // the LP SOL is what the migrator received (minus a little tx fee); pool it all
  const solForPool = BigInt(Math.max(0, solAfter - solBefore));

  // 2+3. Raydium pool at the graduation price + burn LP
  const r = await raydiumListAndLock(mint, tokBal, solForPool);
  console.log(`  Raydium pool ${r.poolId} created ${r.createTx}, LP burned ${r.burnTx}`);

  const done = loadDone();
  done[rec.mint] = { ...rec, migrateTx: migSig, ...r, ts: Math.floor(Date.now() / 1000) };
  saveDone(done);
  await alert(`${rec.ticker} live on Raydium (pool ${r.poolId}), LP burned. ${solForPool / 1_000_000_000n} SOL + tokens deposited.`);
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

console.log(`NOTCH Classic migrator up. DRY_RUN=${DRY ? 1 : 0}, poll ${POLL_MS}ms, migrator ${migrator.publicKey.toBase58()}`);
tick();
setInterval(tick, POLL_MS);
