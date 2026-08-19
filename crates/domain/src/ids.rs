//! Identifier newtypes.
//!
//! Every identifier that crosses a module boundary is a distinct type. Polymarket
//! exposes three different opaque hex/decimal strings (`conditionId`, `asset`,
//! `transactionHash`) that are trivially swappable at a call site if they are all
//! `String`; making them distinct types turns that class of bug into a compile error.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    #[error("address must be 0x + 40 hex chars, got {0:?}")]
    BadAddress(String),
    #[error("condition id must be 0x + 64 hex chars, got {0:?}")]
    BadConditionId(String),
    #[error("tx hash must be 0x + 64 hex chars, got {0:?}")]
    BadTxHash(String),
    #[error("token id must be a non-empty decimal uint256 string, got {0:?}")]
    BadTokenId(String),
}

fn is_hex_body(s: &str, want: usize) -> bool {
    s.len() == want && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A Polymarket proxy wallet address.
///
/// Always stored lowercase. The RTDS feed emits mixed-case (EIP-55) addresses while
/// `data-api` emits lowercase — comparing them raw silently fails to match target
/// wallets, so normalisation happens once, here, at construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Address(String);

impl Address {
    pub fn new(s: impl AsRef<str>) -> Result<Self, IdError> {
        let s = s.as_ref().trim();
        let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"));
        match body {
            Some(b) if is_hex_body(b, 40) => Ok(Self(format!("0x{}", b.to_ascii_lowercase()))),
            _ => Err(IdError::BadAddress(s.to_string())),
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Address::new(&s).map_err(serde::de::Error::custom)
    }
}

/// `conditionId` — identifies a *market* (the question), not an outcome leg.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct MarketId(String);

impl MarketId {
    pub fn new(s: impl AsRef<str>) -> Result<Self, IdError> {
        let s = s.as_ref().trim();
        match s.strip_prefix("0x") {
            Some(b) if is_hex_body(b, 64) => Ok(Self(format!("0x{}", b.to_ascii_lowercase()))),
            _ => Err(IdError::BadConditionId(s.to_string())),
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for MarketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl<'de> Deserialize<'de> for MarketId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        MarketId::new(&s).map_err(serde::de::Error::custom)
    }
}

/// `asset` — the ERC-1155 token id of one outcome leg. A uint256 in decimal form,
/// far too large for u128, so it stays a validated string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TokenId(String);

impl TokenId {
    pub fn new(s: impl AsRef<str>) -> Result<Self, IdError> {
        let s = s.as_ref().trim();
        if !s.is_empty() && s.len() <= 78 && s.bytes().all(|b| b.is_ascii_digit()) {
            Ok(Self(s.to_string()))
        } else {
            Err(IdError::BadTokenId(s.to_string()))
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl<'de> Deserialize<'de> for TokenId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        TokenId::new(&s).map_err(serde::de::Error::custom)
    }
}

/// A Polygon transaction hash. Note: **not unique per trade** — see
/// `docs/POLYMARKET_API.md` §3. Never use this alone as an idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TxHash(String);

impl TxHash {
    pub fn new(s: impl AsRef<str>) -> Result<Self, IdError> {
        let s = s.as_ref().trim();
        match s.strip_prefix("0x") {
            Some(b) if is_hex_body(b, 64) => Ok(Self(format!("0x{}", b.to_ascii_lowercase()))),
            _ => Err(IdError::BadTxHash(s.to_string())),
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for TxHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl<'de> Deserialize<'de> for TxHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        TxHash::new(&s).map_err(serde::de::Error::custom)
    }
}

macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            /// v7 so ids sort by creation time — makes DB indexes and log greps ordered.
            pub fn new() -> Self { Self(Uuid::now_v7()) }
            pub fn from_uuid(u: Uuid) -> Self { Self(u) }
            pub fn as_uuid(&self) -> Uuid { self.0 }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
        }
    };
}

uuid_id!(/// Correlates every artefact produced by one source trade, end to end.
    CorrelationId);
uuid_id!(/// Our internal order identity, assigned before submission.
    OrderId);
uuid_id!(/// One fill against one of our orders.
    FillId);
uuid_id!(/// A generated copy signal.
    SignalId);

/// Deterministic identity of an observed *source* trade.
///
/// Derived by hashing content + occurrence ordinal (see `wallet_tracker::dedup`),
/// because Polymarket exposes no unique key for a fill. Deterministic derivation is
/// what makes restart-safety and WS/REST overlap dedup work: the same underlying fill
/// always hashes to the same id, whichever path observed it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceEventId(String);

impl SourceEventId {
    /// Wraps an already-computed digest. Construction lives in `wallet_tracker::dedup`
    /// so the hashing rule has exactly one definition.
    pub fn from_digest(hex: impl Into<String>) -> Self { Self(hex.into()) }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for SourceEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_normalises_case() {
        let mixed = Address::new("0x8a5152d056aDB066C9E4Dc65164620cDD82CeB6f").unwrap();
        let lower = Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap();
        // This is the whole point: RTDS sends EIP-55, data-api sends lowercase.
        assert_eq!(mixed, lower);
        assert_eq!(mixed.as_str(), "0x8a5152d056adb066c9e4dc65164620cdd82ceb6f");
    }

    #[test]
    fn address_rejects_malformed() {
        for bad in ["", "0x", "8a5152d056adb066c9e4dc65164620cdd82ceb6f", "0xZZ", "0x123"] {
            assert!(Address::new(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn ids_reject_wrong_length() {
        assert!(MarketId::new("0x1d8720896546460a48d4f7fd5b3f0705a8d427921222de498d1cd334d4890c52").is_ok());
        assert!(MarketId::new("0x1d87208965").is_err());
        assert!(TxHash::new("0xb6acf6859bc84216f4b3e2567fb392a2eae19d275340ad96ea17218ccfec27b7").is_ok());
        assert!(TxHash::new("0xb6acf685").is_err());
    }

    #[test]
    fn token_id_accepts_uint256_decimal() {
        assert!(TokenId::new("72551024098258542594534683942523606143014690620243023298497729957846870197074").is_ok());
        assert!(TokenId::new("0xdeadbeef").is_err(), "token ids are decimal, not hex");
        assert!(TokenId::new("").is_err());
    }

    #[test]
    fn uuid_ids_are_time_ordered() {
        let a = OrderId::new();
        let b = OrderId::new();
        assert!(a < b, "v7 ids must sort by creation time");
    }
}
