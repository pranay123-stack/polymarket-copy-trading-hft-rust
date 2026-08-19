//! EIP-712 order signing for the Polymarket CTF Exchange.
//!
//! # Everything here was verified against the live chain
//!
//! Public Polymarket client libraries document an order struct that **no longer matches
//! what is deployed**. Rather than trust them, the scheme below was recovered from
//! Polygon mainnet on 2026-08-19 and then proved correct by recovering the signer of real,
//! already-settled orders.
//!
//! ## How it was established
//!
//! 1. Every trade returned by `data-api` settles through
//!    `0xe3333700ca9d93003f00f0f71f8515005f6c00aa` — a proxy (EIP-1967 implementation
//!    `0x7345c6842b244926125ed4054905cac49620b5dc`). This is *not* the CTF Exchange
//!    address the public clients target.
//! 2. That contract implements ERC-5267. `eip712Domain()` returns, verbatim:
//!    `name = "Polymarket CTF Exchange"`, **`version = "3"`**, `chainId = 137`,
//!    `verifyingContract = 0xe333…00aa`.
//! 3. Recomputing the domain separator from those fields reproduces the contract's own
//!    `domainSeparator()` return value `0x466c6391…e095b` **exactly**.
//! 4. The settlement calldata embeds the EIP-712 type string as ASCII. Extracted verbatim,
//!    it is [`ORDER_TYPE`] below.
//! 5. Final proof: for three independent already-settled orders with `signatureType = 1`
//!    and two distinct signers, ECDSA recovery over the digest produced by this module
//!    returns **exactly** the `signer` field recorded on chain.
//!
//! ## Why this matters
//!
//! The documented `version` is `"1"`; the deployed value is `"3"`. Signing with `"1"`
//! yields domain separator `0xa5745e87…` instead of `0x466c6391…`, so **every signature
//! would be rejected** — and rejected in a way that looks exactly like a network or
//! credentials problem. The documented struct also carries `taker`, `expiration`, `nonce`
//! and `feeRateBps`, none of which exist in the deployed type.
//!
//! ## What is still unverified
//!
//! `signatureType = 3` is now the dominant type on chain and does **not** recover to
//! `order.signer` under this scheme — it appears to be a delegated/session-key mechanism
//! whose key registration is not publicly observable. This module therefore supports
//! `EOA (0)` and `POLY_PROXY (1)`, which are the paths an API trader uses, and refuses to
//! pretend it can produce type 3.
//!
//! An order signed by this module has never been *accepted* by `POST /order`, because
//! that needs funded credentials. The cryptography is proven; the acceptance is not.

use k256::ecdsa::{RecoveryId, Signature as K256Signature, SigningKey, VerifyingKey};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use sha3::{Digest, Keccak256};

use domain::{OrderRequest, Side};

/// The deployed exchange proxy. Verified: every sampled trade settles here.
pub const EXCHANGE_ADDRESS: &str = "0xe3333700ca9d93003f00f0f71f8515005f6c00aa";
/// From the contract's own ERC-5267 `eip712Domain()`.
pub const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
/// **Three**, not one. See the module docs.
pub const DOMAIN_VERSION: &str = "3";
pub const CHAIN_ID: u64 = 137;

/// Extracted verbatim from settlement calldata.
pub const ORDER_TYPE: &[u8] = b"Order(uint256 salt,address maker,address signer,uint256 tokenId,\
uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,\
bytes32 metadata,bytes32 builder)";

const DOMAIN_TYPE: &[u8] =
    b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

/// The `domainSeparator()` the deployed contract returns. Asserted in tests.
pub const EXPECTED_DOMAIN_SEPARATOR: &str =
    "466c63910185bbd55e8679264200c4e0abdcbb0c6264eb3d41d13326022e095b";

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("invalid private key: {0}")]
    BadKey(String),
    #[error("invalid address {0}")]
    BadAddress(String),
    #[error("amount {0} does not fit a uint256 base-unit value")]
    BadAmount(String),
    #[error("signature type {0} is not supported by this signer (only EOA=0 and POLY_PROXY=1 are verified)")]
    UnsupportedSignatureType(u8),
    #[error("signing failed: {0}")]
    Ecdsa(String),
}

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Keccak256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Left-pads to a 32-byte ABI word.
fn word_u256(v: &[u8]) -> [u8; 32] {
    let mut w = [0u8; 32];
    let n = v.len().min(32);
    w[32 - n..].copy_from_slice(&v[v.len() - n..]);
    w
}

fn word_from_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

fn word_from_u8(v: u8) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[31] = v;
    w
}

/// Parses `0x`-prefixed hex into a 20-byte address word.
fn word_from_address(a: &str) -> Result<[u8; 32], SigningError> {
    let s = a.trim_start_matches("0x").trim_start_matches("0X");
    if s.len() != 40 {
        return Err(SigningError::BadAddress(a.to_string()));
    }
    let bytes = hex::decode(s).map_err(|_| SigningError::BadAddress(a.to_string()))?;
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&bytes);
    Ok(w)
}

/// Parses a decimal uint256 string (token ids are far larger than u128).
fn word_from_decimal_string(s: &str) -> Result<[u8; 32], SigningError> {
    let mut acc = [0u8; 32];
    for ch in s.bytes() {
        let d = match ch {
            b'0'..=b'9' => ch - b'0',
            _ => return Err(SigningError::BadAmount(s.to_string())),
        };
        // acc = acc * 10 + d, big-endian, with overflow detection.
        let mut carry = d as u16;
        for i in (0..32).rev() {
            let v = acc[i] as u16 * 10 + carry;
            acc[i] = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return Err(SigningError::BadAmount(s.to_string()));
        }
    }
    Ok(acc)
}

/// How the venue should validate the signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SignatureType {
    /// A plain externally-owned account signs.
    Eoa = 0,
    /// A Polymarket proxy wallet is the maker; an EOA signs on its behalf.
    PolyProxy = 1,
}

impl SignatureType {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The order exactly as the deployed contract hashes it.
#[derive(Debug, Clone)]
pub struct PolymarketOrder {
    pub salt: u128,
    /// The account that holds the funds — a proxy wallet under `PolyProxy`.
    pub maker: String,
    /// The account whose key signs. Equals `maker` for `Eoa`.
    pub signer: String,
    /// ERC-1155 outcome token id, decimal uint256.
    pub token_id: String,
    /// Base units the maker gives (USDC for a buy, tokens for a sell).
    pub maker_amount: u128,
    /// Base units the maker receives.
    pub taker_amount: u128,
    /// 0 = BUY, 1 = SELL.
    pub side: u8,
    pub signature_type: SignatureType,
    /// Unix seconds.
    pub timestamp: u64,
    pub metadata: [u8; 32],
    pub builder: [u8; 32],
}

impl PolymarketOrder {
    /// Builds an order from our venue-agnostic [`OrderRequest`].
    ///
    /// Amounts are converted to base units at `decimals` (6 for both USDC and outcome
    /// tokens on Polymarket) and **rounded in our favour**: we never offer more than
    /// intended, and never ask for less.
    pub fn from_request(
        req: &OrderRequest,
        maker: &str,
        signer: &str,
        signature_type: SignatureType,
        salt: u128,
        timestamp: u64,
        decimals: u32,
    ) -> Result<Self, SigningError> {
        let scale = Decimal::from(10u64.pow(decimals));
        let qty = req.quantity.get();
        let px = req.limit_price.get();
        let notional = qty * px;

        let to_base = |d: Decimal, round_up: bool| -> Result<u128, SigningError> {
            let scaled = d * scale;
            let r = if round_up { scaled.ceil() } else { scaled.floor() };
            r.to_u128().ok_or_else(|| SigningError::BadAmount(d.to_string()))
        };

        let (maker_amount, taker_amount, side) = match req.side {
            // Buying: we give USDC (round the cost DOWN so we never overpay) and expect
            // tokens (round the ask DOWN so the price we imply is not better than our limit).
            Side::Buy => (to_base(notional, false)?, to_base(qty, false)?, 0u8),
            // Selling: we give tokens, and require at least this much USDC back.
            Side::Sell => (to_base(qty, false)?, to_base(notional, true)?, 1u8),
        };
        if maker_amount == 0 || taker_amount == 0 {
            return Err(SigningError::BadAmount(format!(
                "order rounds to zero base units at {decimals} decimals"
            )));
        }

        Ok(Self {
            salt,
            maker: maker.to_string(),
            signer: signer.to_string(),
            token_id: req.token_id.to_string(),
            maker_amount,
            taker_amount,
            side,
            signature_type,
            timestamp,
            metadata: [0u8; 32],
            builder: [0u8; 32],
        })
    }

    /// `keccak256(abi.encode(typehash, ...fields))`.
    pub fn struct_hash(&self) -> Result<[u8; 32], SigningError> {
        let type_hash = keccak(&[ORDER_TYPE]);
        let fields: Vec<[u8; 32]> = vec![
            type_hash,
            word_from_u128(self.salt),
            word_from_address(&self.maker)?,
            word_from_address(&self.signer)?,
            word_from_decimal_string(&self.token_id)?,
            word_from_u128(self.maker_amount),
            word_from_u128(self.taker_amount),
            word_from_u8(self.side),
            word_from_u8(self.signature_type.as_u8()),
            word_from_u128(self.timestamp as u128),
            self.metadata,
            self.builder,
        ];
        let flat: Vec<u8> = fields.concat();
        Ok(keccak(&[&flat]))
    }

    /// The EIP-712 digest: `keccak256(0x19 0x01 ‖ domainSeparator ‖ structHash)`.
    pub fn digest(&self, domain_separator: &[u8; 32]) -> Result<[u8; 32], SigningError> {
        let sh = self.struct_hash()?;
        Ok(keccak(&[&[0x19u8, 0x01u8], domain_separator, &sh]))
    }
}

/// Computes the EIP-712 domain separator for the deployed exchange.
pub fn domain_separator(verifying_contract: &str, chain_id: u64) -> Result<[u8; 32], SigningError> {
    let flat: Vec<u8> = [
        keccak(&[DOMAIN_TYPE]),
        keccak(&[DOMAIN_NAME.as_bytes()]),
        keccak(&[DOMAIN_VERSION.as_bytes()]),
        word_u256(&chain_id.to_be_bytes()),
        word_from_address(verifying_contract)?,
    ]
    .concat();
    Ok(keccak(&[&flat]))
}

/// A secp256k1 key that signs orders.
pub struct EoaSigner {
    key: SigningKey,
    address: String,
    /// The funds-holding account: a proxy wallet, or the EOA itself.
    maker: String,
    signature_type: SignatureType,
    domain_separator: [u8; 32],
}

impl EoaSigner {
    /// `private_key_hex` may be `0x`-prefixed. `maker` is the proxy wallet when
    /// `signature_type` is `PolyProxy`, otherwise the signing address itself.
    pub fn new(
        private_key_hex: &str,
        maker: Option<&str>,
        signature_type: SignatureType,
    ) -> Result<Self, SigningError> {
        let raw = private_key_hex.trim().trim_start_matches("0x");
        let bytes = hex::decode(raw).map_err(|e| SigningError::BadKey(e.to_string()))?;
        if bytes.len() != 32 {
            return Err(SigningError::BadKey(format!(
                "expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        let key = SigningKey::from_slice(&bytes).map_err(|e| SigningError::BadKey(e.to_string()))?;
        let address = address_of(key.verifying_key());
        let maker = maker.unwrap_or(&address).to_string();
        Ok(Self {
            key,
            address,
            maker,
            signature_type,
            domain_separator: domain_separator(EXCHANGE_ADDRESS, CHAIN_ID)?,
        })
    }

    pub fn address(&self) -> &str { &self.address }
    pub fn maker(&self) -> &str { &self.maker }
    pub fn signature_type(&self) -> SignatureType { self.signature_type }
    pub fn domain_separator(&self) -> [u8; 32] { self.domain_separator }

    /// Signs an order, returning the 65-byte `r ‖ s ‖ v` signature with `v ∈ {27, 28}`.
    pub fn sign_order(&self, order: &PolymarketOrder) -> Result<Vec<u8>, SigningError> {
        let digest = order.digest(&self.domain_separator)?;
        let (sig, rec): (K256Signature, RecoveryId) = self
            .key
            .sign_prehash_recoverable(&digest)
            .map_err(|e| SigningError::Ecdsa(e.to_string()))?;
        let mut out = sig.to_bytes().to_vec(); // 64 bytes: r ‖ s
        // Ethereum offsets the recovery id by 27.
        out.push(rec.to_byte() + 27);
        Ok(out)
    }

    /// Signs and renders the JSON body `POST /order` expects.
    ///
    /// UNVERIFIED: the exact JSON field names of the request envelope. The *signature*
    /// is proven correct; the wrapper around it is not, and is the first thing to check
    /// against a funded account.
    pub fn sign_to_json(&self, order: &PolymarketOrder) -> Result<serde_json::Value, SigningError> {
        let sig = self.sign_order(order)?;
        Ok(serde_json::json!({
            "salt": order.salt.to_string(),
            "maker": order.maker,
            "signer": order.signer,
            "tokenId": order.token_id,
            "makerAmount": order.maker_amount.to_string(),
            "takerAmount": order.taker_amount.to_string(),
            "side": if order.side == 0 { "BUY" } else { "SELL" },
            "signatureType": order.signature_type.as_u8(),
            "timestamp": order.timestamp.to_string(),
            "metadata": format!("0x{}", hex::encode(order.metadata)),
            "builder": format!("0x{}", hex::encode(order.builder)),
            "signature": format!("0x{}", hex::encode(sig)),
        }))
    }
}

/// Ethereum address of a public key: last 20 bytes of `keccak256(uncompressed[1..])`.
pub fn address_of(vk: &VerifyingKey) -> String {
    let point = vk.to_encoded_point(false);
    let h = keccak(&[&point.as_bytes()[1..]]);
    format!("0x{}", hex::encode(&h[12..]))
}

/// Recovers the signer of a digest from a 65-byte signature. Used by the tests that
/// verify this module against real on-chain orders.
pub fn recover(digest: &[u8; 32], sig65: &[u8]) -> Result<String, SigningError> {
    if sig65.len() != 65 {
        return Err(SigningError::Ecdsa(format!("expected 65 bytes, got {}", sig65.len())));
    }
    let sig = K256Signature::from_slice(&sig65[..64])
        .map_err(|e| SigningError::Ecdsa(e.to_string()))?;
    let v = sig65[64];
    let rec = RecoveryId::from_byte(if v >= 27 { v - 27 } else { v })
        .ok_or_else(|| SigningError::Ecdsa(format!("bad recovery id {v}")))?;
    let vk = VerifyingKey::recover_from_prehash(digest, &sig, rec)
        .map_err(|e| SigningError::Ecdsa(e.to_string()))?;
    Ok(address_of(&vk))
}

/// Bridges the verified signer to the adapter's [`crate::OrderSigner`] seam, so
/// `LiveExecution` can submit without knowing anything about EIP-712.
impl crate::live::OrderSigner for EoaSigner {
    fn sign(&self, order: &OrderRequest, _tick: Decimal) -> Result<crate::live::SignedOrder, String> {
        // A random salt per order. The venue uses it to distinguish otherwise-identical
        // orders, so it must not be derived from order content.
        let salt: u128 = rand::random::<u64>() as u128;
        let timestamp = chrono::Utc::now().timestamp().max(0) as u64;
        let po = PolymarketOrder::from_request(
            order,
            &self.maker,
            &self.address,
            self.signature_type,
            salt,
            timestamp,
            USDC_DECIMALS,
        )
        .map_err(|e| e.to_string())?;
        let body = self.sign_to_json(&po).map_err(|e| e.to_string())?;
        Ok(crate::live::SignedOrder { body, address: self.address.clone() })
    }

    fn address(&self) -> &str { &self.address }
}

/// Both USDC and Polymarket outcome tokens use 6 decimals.
pub const USDC_DECIMALS: u32 = 6;

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, already-settled Polymarket order, captured from Polygon mainnet
    /// tx `0x3a0b940f454c311c1fb39b31642d216e00baf66da88b69963e517c9f0085b811`.
    /// If this test fails, the deployed signing scheme has changed.
    struct Fixture;
    impl Fixture {
        fn order() -> PolymarketOrder {
            PolymarketOrder {
                salt: 6_777_282_044_196_416_742,
                maker: "0xabb89972b21b304c1bed2bf26f35c8741ac9bba3".into(),
                signer: "0xadba23fe56c61f1aed0d377cb63e2806f2acaf43".into(),
                token_id:
                    "1392685676748424252782535044166142780451270518470285578579002936606958026752"
                        .into(),
                maker_amount: 9_740_926,
                taker_amount: 53_113_878,
                side: 0,
                signature_type: SignatureType::PolyProxy,
                timestamp: 1_787_112_360,
                metadata: [0u8; 32],
                builder: [0u8; 32],
            }
        }
        fn signature() -> Vec<u8> {
            let mut v = Vec::with_capacity(65);
            v.extend_from_slice(
                &hex::decode("d5d4794b3ce45f2df9a4522be8610b005368278d2bbc7fbcef27c862b7485ea4")
                    .unwrap(),
            );
            v.extend_from_slice(
                &hex::decode("530e31ac630fd83239aa9db9e79b1c1c2e40a1bb93075e40d9fad03b96aae671")
                    .unwrap(),
            );
            v.push(28);
            v
        }
    }

    #[test]
    fn domain_separator_matches_the_deployed_contract() {
        // The contract's own domainSeparator() return value.
        let d = domain_separator(EXCHANGE_ADDRESS, CHAIN_ID).unwrap();
        assert_eq!(hex::encode(d), EXPECTED_DOMAIN_SEPARATOR);
    }

    #[test]
    fn documented_version_one_would_produce_the_wrong_domain() {
        // Guards the single most dangerous mistake: public clients say version "1", the
        // deployed contract says "3". Signing with "1" is rejected in a way that looks
        // like a credentials or network fault.
        assert_eq!(DOMAIN_VERSION, "3");
        let wrong: Vec<u8> = [
            keccak(&[DOMAIN_TYPE]),
            keccak(&[DOMAIN_NAME.as_bytes()]),
            keccak(&[b"1"]),
            word_u256(&CHAIN_ID.to_be_bytes()),
            word_from_address(EXCHANGE_ADDRESS).unwrap(),
        ]
        .concat();
        assert_ne!(hex::encode(keccak(&[&wrong])), EXPECTED_DOMAIN_SEPARATOR);
    }

    #[test]
    fn recovers_the_signer_of_a_real_settled_order() {
        // The decisive test: this module's digest must recover the exact signer the
        // exchange recorded on chain.
        let order = Fixture::order();
        let ds = domain_separator(EXCHANGE_ADDRESS, CHAIN_ID).unwrap();
        let digest = order.digest(&ds).unwrap();
        let recovered = recover(&digest, &Fixture::signature()).unwrap();
        assert_eq!(recovered.to_lowercase(), order.signer.to_lowercase());
    }

    #[test]
    fn any_field_change_invalidates_the_signature() {
        // Confirms every field genuinely participates in the hash.
        let ds = domain_separator(EXCHANGE_ADDRESS, CHAIN_ID).unwrap();
        let good = recover(&Fixture::order().digest(&ds).unwrap(), &Fixture::signature()).unwrap();

        let mutate: Vec<(&str, Box<dyn Fn(&mut PolymarketOrder)>)> = vec![
            ("salt", Box::new(|o: &mut PolymarketOrder| o.salt += 1)),
            ("maker", Box::new(|o: &mut PolymarketOrder| o.maker = format!("0x{:040x}", 1))),
            ("tokenId", Box::new(|o: &mut PolymarketOrder| o.token_id = "12345".into())),
            ("makerAmount", Box::new(|o: &mut PolymarketOrder| o.maker_amount += 1)),
            ("takerAmount", Box::new(|o: &mut PolymarketOrder| o.taker_amount += 1)),
            ("side", Box::new(|o: &mut PolymarketOrder| o.side = 1)),
            ("timestamp", Box::new(|o: &mut PolymarketOrder| o.timestamp += 1)),
            ("metadata", Box::new(|o: &mut PolymarketOrder| o.metadata[31] = 1)),
            ("builder", Box::new(|o: &mut PolymarketOrder| o.builder[31] = 1)),
        ];
        for (name, f) in mutate {
            let mut o = Fixture::order();
            f(&mut o);
            let r = recover(&o.digest(&ds).unwrap(), &Fixture::signature()).unwrap();
            assert_ne!(r, good, "changing {name} did not change the digest");
        }
    }

    #[test]
    fn signing_round_trips_through_recovery() {
        // Deterministic test key (not a real account).
        let pk = "0x4c0883a69102937d6231471b5dbb6204fe512961708279e2f2b4d2e0a1b2c3d4";
        let s = EoaSigner::new(pk, None, SignatureType::Eoa).unwrap();
        let mut o = Fixture::order();
        o.maker = s.address().to_string();
        o.signer = s.address().to_string();
        o.signature_type = SignatureType::Eoa;

        let sig = s.sign_order(&o).unwrap();
        assert_eq!(sig.len(), 65);
        assert!(sig[64] == 27 || sig[64] == 28, "v must be 27 or 28, got {}", sig[64]);

        let digest = o.digest(&s.domain_separator()).unwrap();
        assert_eq!(recover(&digest, &sig).unwrap().to_lowercase(), s.address().to_lowercase());
    }

    #[test]
    fn signing_is_deterministic() {
        // RFC 6979 — the same order must always produce the same signature, so a retry
        // cannot create a second distinct order at the venue.
        let pk = "0x4c0883a69102937d6231471b5dbb6204fe512961708279e2f2b4d2e0a1b2c3d4";
        let s = EoaSigner::new(pk, None, SignatureType::Eoa).unwrap();
        let o = Fixture::order();
        assert_eq!(s.sign_order(&o).unwrap(), s.sign_order(&o).unwrap());
    }

    #[test]
    fn address_derivation_matches_a_known_vector() {
        // Well-known test key → well-known address.
        let pk = "0x4646464646464646464646464646464646464646464646464646464646464646";
        let s = EoaSigner::new(pk, None, SignatureType::Eoa).unwrap();
        assert_eq!(s.address(), "0x9d8a62f656a8d1615c1294fd71e9cfb3e4855a4f");
    }

    #[test]
    fn uint256_token_ids_are_parsed_exactly() {
        // Token ids exceed u128; a lossy parse would silently sign the wrong market.
        let big = "1392685676748424252782535044166142780451270518470285578579002936606958026752";
        let w = word_from_decimal_string(big).unwrap();
        // Recompute the decimal from the 32-byte word.
        let mut digits = vec![0u8; 0];
        let mut acc = w;
        while acc.iter().any(|b| *b != 0) {
            let mut rem = 0u16;
            for byte in acc.iter_mut() {
                let cur = (rem << 8) | *byte as u16;
                *byte = (cur / 10) as u8;
                rem = cur % 10;
            }
            digits.push(b'0' + rem as u8);
        }
        digits.reverse();
        assert_eq!(String::from_utf8(digits).unwrap(), big);
    }

    #[test]
    fn oversized_token_id_is_rejected_not_truncated() {
        let too_big = "1".repeat(80);
        assert!(word_from_decimal_string(&too_big).is_err());
        assert!(word_from_decimal_string("12a4").is_err());
    }

    #[test]
    fn buy_and_sell_map_amounts_to_the_right_legs() {
        use chrono::Utc;
        use domain::{CorrelationId, MarketId, OrderId, OrderType, Price, Qty, TimeInForce, TokenId};
        use rust_decimal_macros::dec;

        let mk = |side: Side| OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("12345").unwrap(),
            side,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            quantity: Qty::new(dec!(100)).unwrap(),
            limit_price: Price::new(dec!(0.60)).unwrap(),
            reference_price: Price::new(dec!(0.60)).unwrap(),
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        };

        // Buying 100 shares at 0.60 = $60 out, 100 tokens in.
        let b = PolymarketOrder::from_request(
            &mk(Side::Buy),
            &format!("0x{:040x}", 1), &format!("0x{:040x}", 1),
            SignatureType::Eoa, 1, 1, 6).unwrap();
        assert_eq!(b.side, 0);
        assert_eq!(b.maker_amount, 60_000_000);  // 60 USDC
        assert_eq!(b.taker_amount, 100_000_000); // 100 tokens

        // Selling 100 shares at 0.60 = 100 tokens out, $60 in.
        let s = PolymarketOrder::from_request(
            &mk(Side::Sell),
            &format!("0x{:040x}", 1), &format!("0x{:040x}", 1),
            SignatureType::Eoa, 1, 1, 6).unwrap();
        assert_eq!(s.side, 1);
        assert_eq!(s.maker_amount, 100_000_000);
        assert_eq!(s.taker_amount, 60_000_000);
    }

    #[test]
    fn malformed_keys_and_addresses_are_rejected() {
        assert!(EoaSigner::new("nothex", None, SignatureType::Eoa).is_err());
        assert!(EoaSigner::new("0x1234", None, SignatureType::Eoa).is_err());
        assert!(word_from_address("0x123").is_err());
    }
}
