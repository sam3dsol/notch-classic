# NOTCH Classic

A constant product bonding curve launchpad on Solana, the second curve on
[notch.fund](https://notch.fund) beside the Protected curve. Full price
discovery, no floor. The bonding phase runs through $NOTCH: creating a pool
requires holding 0.1 $NOTCH and buying on the curve requires holding 0.05
$NOTCH. Balances are checked on chain and never debited. Sells are never
gated. Graduated pools migrate to Raydium and the liquidity is locked.

## Fixed economics

Every pool uses the same parameters. There are no per launch knobs.

| Parameter | Value |
|---|---|
| Total supply | 1,000,000,000 tokens (9 decimals), minted once to the pool vault at create, then the mint authority is set to None |
| Virtual reserves | 16 SOL and 1.12B tokens, so a 40 SOL raise sells about 800M tokens and the final curve price equals the Raydium listing price |
| Start price | 0.0000000143 SOL per token (14.3 SOL market cap) |
| Graduation | 40 SOL raised: trading freezes, price 0.000000175 SOL (175 SOL market cap), 12.25x over the curve |
| Buy fee | 1% to the platform wallet |
| Sell fee | 1% to the pool creator |
| Wallet cap | no wallet may hold more than 4% of the supply from the curve |
| Migration | 5 SOL to the platform operations wallet (exchange listing and market making budget), the remaining raise plus all unsold tokens to the Raydium pool, liquidity locked |

The curve math is the standard constant product with virtual reserves:
`tokens out = y - ceil(k / (x + sol in))`, rounding always in the pool's
favor. A final buy may overshoot the 40 SOL target; the overshoot flows to
the Raydium pool, so the listing can only be at or above the final curve
price.

## Layout

- `program/` native Rust on-chain program, no framework
- `client/` instruction builders and the local validator integration suite

## Build and test

```
cd program
cargo test                          # native curve property tests
cargo build-sbf --features dev-mint # local test build
cargo build-sbf                     # release build (real NOTCH mint)
```

The `dev-mint` feature swaps the flagship NOTCH mint for the committed
throwaway keypair `dev-notch-mint.json` so the hold gates are testable on a
local validator. Release builds use default features.

```
solana-test-validator --reset
solana program deploy program/target/deploy/notch_classic.so --program-id <fresh keypair> -u localhost -k <payer>
cd client
RPC=http://127.0.0.1:8899 PROGRAM=<id> PAYER=<payer.json> cargo run --bin pool-test
```

The suite covers the full lifecycle: creation gates, fixed supply, buy gates,
the wallet cap, exact fees, sell math, the graduation freeze and migration
payouts.

## License

MIT
