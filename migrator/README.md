# NOTCH Classic migrator

Operator daemon. Watches every Classic pool; when one graduates (40 SOL raised)
it calls the on-chain `Migrate` instruction, creates a Raydium CPMM pool at the
exact price the curve closed at (~35 SOL : ~200M tokens, so there is no arb gap),
and permanently locks 100% of the LP via Raydium's Burn & Earn program — the liquidity can never be withdrawn, and a Fee Key NFT is minted to the platform wallet so the platform harvests the pool's trading fees forever.

The ~0.4 SOL Raydium pool-creation cost is pulled from the platform fee wallet
(from the 5 SOL ops carve-out: 4 SOL DexScreener listing + 1 SOL migration
mechanics + market making), so the entire bonding-curve SOL goes into the pool
as depth.

`DRY_RUN=1` by default (simulates only). Run with `DRY_RUN=0` to arm.
`HARVEST=1` claims accrued fees from every locked pool's Fee Key NFT to the
platform wallet, then exits. **Manual only** — run by hand, never on a cron.
Fees arrive as both the token and SOL (constant-product pools skim the fee from
each swap's input side); claiming is not a swap and never dumps on the pool. The
operator keeps the SOL and decides separately whether to hold or convert the
token portion.
Env: `RPC`, `MIGRATOR_KP`, `PLATFORM_KP`, `POOL_STORE`, `POLL_MS`, `HARVEST`.
