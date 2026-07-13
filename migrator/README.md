# NOTCH Classic migrator

Operator daemon. Watches every Classic pool; when one graduates (40 SOL raised)
it calls the on-chain `Migrate` instruction, creates a Raydium CPMM pool at the
exact price the curve closed at (~35 SOL : ~200M tokens, so there is no arb gap),
and burns 100% of the LP to permanently lock the liquidity.

The ~0.4 SOL Raydium pool-creation cost is pulled from the platform fee wallet
(from the 5 SOL ops carve-out: 4 SOL DexScreener listing + 1 SOL migration
mechanics + market making), so the entire bonding-curve SOL goes into the pool
as depth.

`DRY_RUN=1` by default (simulates only). Run with `DRY_RUN=0` to arm.
Env: `RPC`, `MIGRATOR_KP`, `PLATFORM_KP`, `POOL_STORE`, `POLL_MS`.
