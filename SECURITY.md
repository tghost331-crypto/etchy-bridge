# Security policy

## Reporting a vulnerability

Email **security@etchy.fun** with:

- a description of the issue and its impact,
- the steps or a proof of concept to reproduce it,
- the affected program address and transaction signatures, if any.

Please do not open a public GitHub issue for a security problem, and do not
exploit an issue beyond what is needed to show it exists. We will confirm
receipt, keep you informed while we fix it, and credit you if you wish.

## Scope

- The Etchy bridge program on Solana mainnet:
  `BGEwwP6EcQAvBqZhYQXPk5jPKpiBXFXqWKag9GsZpDx2`
- The source code in this repository.

Issues that matter most: anything that moves tokens out of a vault other than
a release to a deposit's recipient or a refund to the original requester,
a second release for the same Bitcoin output, getting around the pause or the
daily release cap, or changing a coin's Rune ID after it is set.
