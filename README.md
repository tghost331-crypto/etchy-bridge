# Etchy bridge

The Solana program behind [Etchy](https://etchy.fun): a **lock vault** that
moves an Etchy coin between Solana and Bitcoin, 1:1.

Every Etchy coin exists twice: as an SPL token on Solana and as a real Bitcoin
**Rune** whose whole premine sits in Etchy custody. Both have the same fixed
supply (1,000,000,000 at 6 decimals) and the token has no mint authority, so
this program never creates tokens. Coins only move in and out of the vault:

> Rune in Etchy custody = tokens circulating on Solana = supply − vault balance

| | |
| --- | --- |
| Program (mainnet) | [`BGEwwP6EcQAvBqZhYQXPk5jPKpiBXFXqWKag9GsZpDx2`](https://solscan.io/account/BGEwwP6EcQAvBqZhYQXPk5jPKpiBXFXqWKag9GsZpDx2) |
| Framework | Anchor 1.0.2, Solana 3.1.10 |
| Interface (IDL) | [`idl/etchy_bridge.json`](idl/etchy_bridge.json) |
| Security contact | security@etchy.fun ([policy](SECURITY.md)) |

## How it works

**Solana → Bitcoin.** The holder locks tokens in the coin's vault and names a
Bitcoin Taproot address (`bc1p…`), paying a small SOL fee that covers the
Bitcoin network fee. Etchy sends the Rune from custody and records the Bitcoin
transaction on the request, or refunds the tokens to the requester's own
wallet. A request ends exactly one way: _Sent_ or _Refunded_.

**Bitcoin → Solana.** When a Rune deposit to Etchy custody is final on Bitcoin,
the same amount is released from the vault to the depositor's token account.
Each Bitcoin output can be released only once (a receipt account per output).

## Roles

| Role | Can | Cannot |
| --- | --- | --- |
| Holder | lock tokens for a Bitcoin address; close its own finished requests | touch anyone else's tokens |
| Attester | mark a request sent; refund it to the requester; release for a final deposit (once per output, under a daily cap) | send vault tokens anywhere else; go over the cap; release while paused |
| Guardian | pause everything | unpause; change anything |
| Admin | register coins, link a coin's Rune ID once, set limits and the fee (at most 1 SOL), rotate keys (two-step admin handover), pause and unpause | move tokens |

## Safety rules enforced on chain

- Only Etchy-shaped coins: 6 decimals, no mint authority, no freeze authority,
  supply at most the 10^15 Rune premine.
- Nothing can be bridged until the coin's Rune ID is linked, and the link can
  never change afterwards.
- Only the program's upgrade authority can initialize it.
- A per-coin daily release cap limits the damage if the attester key is lost;
  the guardian can pause instantly.
- Refunds go only to the requester; releases only to the recipient's own
  token account. All arithmetic is checked.

## Build and verify

The deployed binary is built reproducibly with
[solana-verify](https://github.com/solana-foundation/solana-verifiable-build):

```sh
solana-verify build --library-name etchy_bridge
solana-verify get-executable-hash target/deploy/etchy_bridge.so
solana-verify get-program-hash -u mainnet BGEwwP6EcQAvBqZhYQXPk5jPKpiBXFXqWKag9GsZpDx2
```

Run the program tests (LiteSVM, against the compiled program) after a build:

```sh
cargo test -p etchy-bridge
```
