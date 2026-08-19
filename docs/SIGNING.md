# Order signing — recovered and verified from chain

Public Polymarket client libraries document an EIP-712 order scheme that **no longer
matches what is deployed**. Implementing from those docs produces signatures that are
silently rejected, in a way indistinguishable from a credentials or network fault.

Everything below was recovered from Polygon mainnet on 2026-08-19 and then *proved*.

## What is deployed

| Field | Deployed value | Public clients say |
|---|---|---|
| verifying contract | `0xe3333700ca9d93003f00f0f71f8515005f6c00aa` | the old CTF Exchange |
| domain name | `Polymarket CTF Exchange` | same |
| **domain version** | **`3`** | **`1`** |
| chainId | `137` | same |

The contract is a proxy (EIP-1967 implementation `0x7345c6842b244926125ed4054905cac49620b5dc`).

## How it was established

1. **Find the contract.** Every trade returned by `data-api` was traced on chain; all of
   them settle through `0xe333…00aa` as the transaction entrypoint.
2. **Read the domain.** The contract implements ERC-5267, so `eip712Domain()` returns the
   domain verbatim — including `version = "3"`.
3. **Check it.** Recomputing the separator from those fields reproduces the contract's own
   `domainSeparator()` return value `0x466c6391…e095b` exactly.
4. **Find the struct.** Settlement calldata embeds the EIP-712 type string as ASCII.
   Extracted verbatim:

```
Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,
      uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,
      bytes32 metadata,bytes32 builder)
```

   The documented struct instead carries `taker`, `expiration`, `nonce` and `feeRateBps` —
   **none of which exist** — and lacks `timestamp`, `metadata` and `builder`.

5. **Prove it.** For three independent already-settled orders with `signatureType = 1` and
   two distinct signers, ECDSA recovery over the digest produced by
   `crates/execution/src/signing.rs` returns **exactly** the `signer` recorded on chain.

## Why the version matters so much

```
version "3" -> domainSeparator 0x466c6391…   (matches the contract)
version "1" -> domainSeparator 0xa5745e87…   (every signature rejected)
```

Both are well-formed EIP-712. Only one is accepted. A test pins this
(`documented_version_one_would_produce_the_wrong_domain`).

## What is implemented

`EoaSigner` supports `signatureType`:

* **0 — EOA**: the signing key is the maker.
* **1 — POLY_PROXY**: the maker is a Polymarket proxy wallet and the key signs for it.
  This is the mode used when `POLYMARKET_FUNDER_ADDRESS` is set.

Signing is RFC 6979 deterministic, so a retry cannot create a second distinct order.

## What is still not verified

* **`signatureType = 3`** is now the dominant type on chain and does *not* recover to
  `order.signer` under this scheme. It appears to be a delegated/session-key mechanism
  whose key registration is not publicly observable. This module refuses to pretend it can
  produce it.
* **`POST /order` has never accepted an order signed by this module**, because that needs
  funded credentials. The *cryptography* is proven against real settled orders; the JSON
  envelope around it, and the venue's acceptance, are not.

## Regression safety

`cargo test -p execution --lib signing` — 11 tests, including:

* the domain separator matches the deployed contract byte for byte;
* a real settled order's signer is recovered exactly;
* mutating **any** of the 9 mutable fields changes the digest;
* signing round-trips through recovery and is deterministic;
* uint256 token ids are parsed exactly, and oversized input is rejected, not truncated.

If Polymarket redeploys, `recovers_the_signer_of_a_real_settled_order` is the test that
fails first.
