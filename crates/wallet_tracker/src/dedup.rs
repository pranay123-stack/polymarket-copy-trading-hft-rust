//! Idempotency for source trades.
//!
//! # The problem
//!
//! Polymarket exposes **no unique identifier for a fill**. Measured against production
//! over 1466 consecutive RTDS trades: 587 distinct `transactionHash` values, one
//! transaction carrying 16 fill rows, and — decisively — *byte-identical* rows within a
//! single transaction:
//!
//! ```text
//! tx 0x5acd4332…  wallet 0xe6F7E1Ab…  BUY  price 0.98  size 5
//! tx 0x5acd4332…  wallet 0xe6F7E1Ab…  BUY  price 0.98  size 5
//! ```
//!
//! So `txHash`, `(txHash, wallet)`, `(txHash, wallet, asset, side)` and even the full
//! content tuple are all non-unique. There is nothing to deduplicate *on*.
//!
//! # The resolution
//!
//! Identity is `H(content ‖ occurrence)`, where `occurrence` is the ordinal of this fill
//! among identical fills **within the same transaction**. Because `txHash` is part of
//! the content, ordinals are naturally scoped to one transaction and stay small
//! (max 16 observed) — they cannot grow without bound.
//!
//! Two delivery paths then need different treatment, and conflating them is the bug this
//! design exists to prevent:
//!
//! * **Live feed** ([`observe_live`]): each arrival is a *new* fill. The Nth identical
//!   row in a transaction takes ordinal N-1.
//! * **REST backfill** ([`observe_backfill`]): rows may *overlap* what the live feed
//!   already recorded. Backfill claims ordinals from 0 upward, and any ordinal already
//!   held by the live feed is recognised as the same fill and dropped. This is multiset
//!   reconciliation: if the live feed saw 2 identical fills and backfill reports 2, they
//!   are the same 2 — not 4.
//!
//! Assigning ordinals by probing for a free slot in *both* paths would be wrong: a
//! re-delivered fill would take a fresh ordinal and be copied twice.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use domain::{Address, MarketId, Price, Qty, Side, SourceEventId, TokenId, TxHash};
use sha2::{Digest, Sha256};

/// The content tuple that defines a fill's identity, before the occurrence ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentKey {
    pub tx_hash: TxHash,
    pub trader: Address,
    pub token_id: TokenId,
    pub side: Side,
    /// Canonical decimal text — `0.98` and `0.980` must not hash differently.
    pub price: String,
    pub size: String,
}

impl ContentKey {
    pub fn new(
        tx_hash: &TxHash,
        trader: &Address,
        token_id: &TokenId,
        side: Side,
        price: Price,
        size: Qty,
    ) -> Self {
        Self {
            tx_hash: tx_hash.clone(),
            trader: trader.clone(),
            token_id: token_id.clone(),
            side,
            price: price.get().normalize().to_string(),
            size: size.get().normalize().to_string(),
        }
    }

    /// Deterministic identity. Field-separated so that concatenation cannot be
    /// ambiguous between different field splits.
    pub fn event_id(&self, occurrence: u32) -> SourceEventId {
        let mut h = Sha256::new();
        h.update(self.tx_hash.as_str().as_bytes());
        h.update(b"\x1f");
        h.update(self.trader.as_str().as_bytes());
        h.update(b"\x1f");
        h.update(self.token_id.as_str().as_bytes());
        h.update(b"\x1f");
        h.update(self.side.as_str().as_bytes());
        h.update(b"\x1f");
        h.update(self.price.as_bytes());
        h.update(b"\x1f");
        h.update(self.size.as_bytes());
        h.update(b"\x1f");
        h.update(occurrence.to_be_bytes());
        SourceEventId::from_digest(hex::encode(h.finalize()))
    }
}

/// The result of offering a trade to the index.
#[derive(Debug, Clone, PartialEq)]
pub enum DedupVerdict {
    /// Not seen before. Carries the identity to use.
    New { event_id: SourceEventId, occurrence: u32 },
    /// Already processed — must not produce another copy order.
    Duplicate { event_id: SourceEventId, occurrence: u32 },
}

impl DedupVerdict {
    pub fn is_new(&self) -> bool { matches!(self, Self::New { .. }) }
    pub fn event_id(&self) -> &SourceEventId {
        match self { Self::New { event_id, .. } | Self::Duplicate { event_id, .. } => event_id }
    }
    pub fn occurrence(&self) -> u32 {
        match self { Self::New { occurrence, .. } | Self::Duplicate { occurrence, .. } => *occurrence }
    }
}

/// Bounded, restart-safe dedup index.
///
/// Entries older than the retention window are evicted. The window must comfortably
/// exceed the longest disconnect we intend to backfill, otherwise an evicted fill could
/// be re-copied; [`DEFAULT_RETENTION_HOURS`] is sized for that.
pub struct DedupIndex {
    /// content → number of occurrences recorded
    counts: HashMap<ContentKey, u32>,
    /// content → when it was last touched, for eviction
    last_seen: HashMap<ContentKey, DateTime<Utc>>,
    retention: Duration,
    evictions: u64,
    suspicious: u64,
}

pub const DEFAULT_RETENTION_HOURS: i64 = 6;

/// Hard ceiling on occurrences of one identical fill within a single transaction.
///
/// The multiset rule that reconciles backfill against the live feed is arithmetically
/// correct only while the upstream data is trustworthy. A malformed backfill — a paging
/// loop re-emitting the same rows, say — would otherwise translate one-for-one into
/// copied orders, because each repeat legitimately claims the next ordinal.
///
/// Production was measured at **16 fills per transaction at the very worst**, so this
/// bound is roughly 4× the observed maximum: high enough never to reject real activity,
/// low enough that a runaway is capped at a handful of orders instead of tens of
/// thousands. Breaching it is reported via [`DedupIndex::suspicious_suppressions`]
/// rather than silently swallowed, because it means an upstream assumption broke.
pub const MAX_OCCURRENCES_PER_CONTENT: u32 = 64;

impl Default for DedupIndex {
    fn default() -> Self { Self::new(Duration::hours(DEFAULT_RETENTION_HOURS)) }
}

impl DedupIndex {
    pub fn new(retention: Duration) -> Self {
        Self {
            counts: HashMap::new(),
            last_seen: HashMap::new(),
            retention,
            evictions: 0,
            suspicious: 0,
        }
    }

    pub fn tracked_contents(&self) -> usize { self.counts.len() }
    pub fn evictions(&self) -> u64 { self.evictions }

    /// Fills refused because they exceeded [`MAX_OCCURRENCES_PER_CONTENT`].
    /// A non-zero value means upstream data is behaving in a way we did not expect.
    pub fn suspicious_suppressions(&self) -> u64 { self.suspicious }

    /// Total recorded fills across all contents.
    pub fn recorded_fills(&self) -> u64 {
        self.counts.values().map(|c| *c as u64).sum()
    }

    /// Restores state after a restart, so a replayed fill is still recognised.
    pub fn restore(&mut self, key: ContentKey, occurrences: u32, at: DateTime<Utc>) {
        let e = self.counts.entry(key.clone()).or_insert(0);
        *e = (*e).max(occurrences);
        self.last_seen.insert(key, at);
    }

    /// Records a fill from the **live feed**: always a new occurrence.
    pub fn observe_live(&mut self, key: ContentKey, at: DateTime<Utc>) -> DedupVerdict {
        let n = self.counts.entry(key.clone()).or_insert(0);
        let occurrence = *n;
        if occurrence >= MAX_OCCURRENCES_PER_CONTENT {
            // Far beyond anything production has ever produced: treat as corrupt input
            // and refuse, rather than minting unbounded new identities.
            self.suspicious += 1;
            return DedupVerdict::Duplicate { event_id: key.event_id(occurrence), occurrence };
        }
        *n += 1;
        self.last_seen.insert(key.clone(), at);
        DedupVerdict::New { event_id: key.event_id(occurrence), occurrence }
    }

    /// Records a fill from **REST backfill**, reconciling against what the live feed
    /// already holds.
    ///
    /// `index_within_batch` is this row's position among identical rows in the same
    /// backfill response. Ordinals are claimed from 0 upward, so any ordinal the live
    /// feed already holds is recognised as the same fill and reported as a duplicate.
    pub fn observe_backfill(
        &mut self,
        key: ContentKey,
        index_within_batch: u32,
        at: DateTime<Utc>,
    ) -> DedupVerdict {
        if index_within_batch >= MAX_OCCURRENCES_PER_CONTENT {
            self.suspicious += 1;
            return DedupVerdict::Duplicate {
                event_id: key.event_id(index_within_batch),
                occurrence: index_within_batch,
            };
        }
        let existing = *self.counts.get(&key).unwrap_or(&0);
        if index_within_batch < existing {
            // The live feed already has this exact occurrence.
            return DedupVerdict::Duplicate {
                event_id: key.event_id(index_within_batch),
                occurrence: index_within_batch,
            };
        }
        self.counts.insert(key.clone(), index_within_batch + 1);
        self.last_seen.insert(key.clone(), at);
        DedupVerdict::New { event_id: key.event_id(index_within_batch), occurrence: index_within_batch }
    }

    /// Drops entries outside the retention window.
    pub fn evict_older_than(&mut self, now: DateTime<Utc>) {
        let cutoff = now - self.retention;
        let stale: Vec<ContentKey> = self
            .last_seen
            .iter()
            .filter(|(_, t)| **t < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.counts.remove(&k);
            self.last_seen.remove(&k);
            self.evictions += 1;
        }
    }
}

/// Counts identical rows within one backfill batch so each gets its own ordinal.
pub struct BatchOrdinals {
    seen: HashMap<ContentKey, u32>,
}

impl Default for BatchOrdinals {
    fn default() -> Self { Self::new() }
}

impl BatchOrdinals {
    pub fn new() -> Self { Self { seen: HashMap::new() } }
    pub fn next(&mut self, key: &ContentKey) -> u32 {
        let e = self.seen.entry(key.clone()).or_insert(0);
        let v = *e;
        *e += 1;
        v
    }
}

/// Convenience for building a key straight from parsed fields.
pub fn content_key(
    tx: &TxHash,
    trader: &Address,
    token: &TokenId,
    side: Side,
    price: Price,
    size: Qty,
    _market: &MarketId,
) -> ContentKey {
    ContentKey::new(tx, trader, token, side, price, size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn tx(n: u8) -> TxHash { TxHash::new(format!("0x{:064x}", n)).unwrap() }
    fn addr(n: u8) -> Address { Address::new(format!("0x{:040x}", n)).unwrap() }
    fn tok() -> TokenId { TokenId::new("83208474815813611206796889197671166802498709571847428026387018914516677525784").unwrap() }
    fn key(t: u8, a: u8, price: rust_decimal::Decimal, size: rust_decimal::Decimal) -> ContentKey {
        ContentKey::new(&tx(t), &addr(a), &tok(), Side::Buy,
            Price::new(price).unwrap(), Qty::new(size).unwrap())
    }

    #[test]
    fn identical_fills_in_one_tx_get_distinct_identities() {
        // The real production case: 0xe6F7E1Ab BUY 0.98 size 5, twice, same tx.
        let mut idx = DedupIndex::default();
        let k = key(1, 1, dec!(0.98), dec!(5));
        let a = idx.observe_live(k.clone(), Utc::now());
        let b = idx.observe_live(k.clone(), Utc::now());
        assert!(a.is_new() && b.is_new(), "both are genuine fills and must both be copied");
        assert_ne!(a.event_id(), b.event_id(), "identical content must still get distinct ids");
        assert_eq!(a.occurrence(), 0);
        assert_eq!(b.occurrence(), 1);
    }

    #[test]
    fn backfill_overlapping_the_live_feed_is_recognised_as_duplicate() {
        let mut idx = DedupIndex::default();
        let k = key(1, 1, dec!(0.98), dec!(5));
        let live = idx.observe_live(k.clone(), Utc::now());
        // The same fill arrives again via REST backfill after a reconnect.
        let back = idx.observe_backfill(k.clone(), 0, Utc::now());
        assert!(!back.is_new(), "a re-delivered fill must NOT produce a second copy order");
        assert_eq!(back.event_id(), live.event_id(), "same fill -> same identity");
    }

    #[test]
    fn backfill_multiset_reconciliation_is_exact() {
        // Live feed saw 2 identical fills; backfill reports 3 of them.
        // Exactly one is new.
        let mut idx = DedupIndex::default();
        let k = key(1, 1, dec!(0.98), dec!(5));
        idx.observe_live(k.clone(), Utc::now());
        idx.observe_live(k.clone(), Utc::now());

        let mut batch = BatchOrdinals::new();
        let verdicts: Vec<_> = (0..3)
            .map(|_| idx.observe_backfill(k.clone(), batch.next(&k), Utc::now()))
            .collect();
        assert!(!verdicts[0].is_new());
        assert!(!verdicts[1].is_new());
        assert!(verdicts[2].is_new(), "the third really is a fill we never saw");
        assert_eq!(verdicts[2].occurrence(), 2);
    }

    #[test]
    fn backfill_during_a_gap_is_all_new() {
        // Nothing was seen live, so every backfilled row is genuinely new.
        let mut idx = DedupIndex::default();
        let k = key(2, 3, dec!(0.44), dec!(120));
        let mut batch = BatchOrdinals::new();
        for i in 0..3 {
            let v = idx.observe_backfill(k.clone(), batch.next(&k), Utc::now());
            assert!(v.is_new());
            assert_eq!(v.occurrence(), i);
        }
    }

    #[test]
    fn different_wallets_in_one_tx_are_independent() {
        // One tx routinely carries fills for many wallets (16 observed).
        let mut idx = DedupIndex::default();
        let a = idx.observe_live(key(1, 1, dec!(0.6), dec!(10)), Utc::now());
        let b = idx.observe_live(key(1, 2, dec!(0.6), dec!(10)), Utc::now());
        assert_ne!(a.event_id(), b.event_id());
        assert_eq!(a.occurrence(), 0);
        assert_eq!(b.occurrence(), 0, "each wallet has its own ordinal sequence");
    }

    #[test]
    fn price_formatting_does_not_change_identity() {
        // 0.98 and 0.980 are the same price and must hash identically.
        let k1 = ContentKey::new(&tx(1), &addr(1), &tok(), Side::Buy,
            Price::new(dec!(0.98)).unwrap(), Qty::new(dec!(5)).unwrap());
        let k2 = ContentKey::new(&tx(1), &addr(1), &tok(), Side::Buy,
            Price::new(dec!(0.980)).unwrap(), Qty::new(dec!(5.0)).unwrap());
        assert_eq!(k1, k2);
        assert_eq!(k1.event_id(0), k2.event_id(0));
    }

    #[test]
    fn side_is_part_of_identity() {
        let buy = ContentKey::new(&tx(1), &addr(1), &tok(), Side::Buy,
            Price::new(dec!(0.5)).unwrap(), Qty::new(dec!(5)).unwrap());
        let sell = ContentKey::new(&tx(1), &addr(1), &tok(), Side::Sell,
            Price::new(dec!(0.5)).unwrap(), Qty::new(dec!(5)).unwrap());
        assert_ne!(buy.event_id(0), sell.event_id(0));
    }

    #[test]
    fn identity_is_stable_across_process_restarts() {
        // Same inputs must hash the same in a fresh index — that is what makes
        // restart-safety and cross-path dedup work at all.
        let k = key(9, 9, dec!(0.33), dec!(77));
        let first = DedupIndex::default().observe_live(k.clone(), Utc::now());
        let second = DedupIndex::default().observe_live(k.clone(), Utc::now());
        assert_eq!(first.event_id(), second.event_id());
    }

    #[test]
    fn restore_prevents_recopying_after_restart() {
        let k = key(4, 4, dec!(0.21), dec!(9));
        let mut idx = DedupIndex::default();
        // Two occurrences were persisted before the crash.
        idx.restore(k.clone(), 2, Utc::now());
        let mut batch = BatchOrdinals::new();
        assert!(!idx.observe_backfill(k.clone(), batch.next(&k), Utc::now()).is_new());
        assert!(!idx.observe_backfill(k.clone(), batch.next(&k), Utc::now()).is_new());
        assert!(idx.observe_backfill(k.clone(), batch.next(&k), Utc::now()).is_new());
    }

    #[test]
    fn eviction_bounds_memory() {
        let mut idx = DedupIndex::new(Duration::hours(1));
        let now = Utc::now();
        for i in 0..50u8 {
            idx.observe_live(key(i, 1, dec!(0.5), dec!(1)), now - Duration::hours(3));
        }
        assert_eq!(idx.tracked_contents(), 50);
        idx.evict_older_than(now);
        assert_eq!(idx.tracked_contents(), 0);
        assert_eq!(idx.evictions(), 50);
    }

    #[test]
    fn recent_entries_survive_eviction() {
        let mut idx = DedupIndex::new(Duration::hours(6));
        let now = Utc::now();
        idx.observe_live(key(1, 1, dec!(0.5), dec!(1)), now);
        idx.evict_older_than(now);
        assert_eq!(idx.tracked_contents(), 1, "recent fills must not be evicted");
    }

    #[test]
    fn a_repeat_storm_is_capped_rather_than_copied_unbounded() {
        // A malformed backfill re-emitting one row thousands of times must not translate
        // into thousands of orders. Production maxes out at 16 fills per transaction.
        let mut idx = DedupIndex::default();
        let k = key(1, 1, dec!(0.5), dec!(10));
        let mut batch = BatchOrdinals::new();
        let mut new = 0;
        for _ in 0..10_000 {
            if idx.observe_backfill(k.clone(), batch.next(&k), Utc::now()).is_new() {
                new += 1;
            }
        }
        assert_eq!(new, MAX_OCCURRENCES_PER_CONTENT as usize,
            "must cap at the ceiling, not mint an identity per repeat");
        assert!(idx.suspicious_suppressions() > 0, "the breach must be observable");
    }

    #[test]
    fn the_live_path_is_capped_too() {
        let mut idx = DedupIndex::default();
        let k = key(2, 2, dec!(0.5), dec!(10));
        let mut new = 0;
        for _ in 0..1000 {
            if idx.observe_live(k.clone(), Utc::now()).is_new() { new += 1; }
        }
        assert_eq!(new, MAX_OCCURRENCES_PER_CONTENT as usize);
    }

    #[test]
    fn the_cap_sits_well_above_real_production_volume() {
        // 16 identical fills in one transaction is the worst ever observed live; the cap
        // must never reject genuine activity.
        let mut idx = DedupIndex::default();
        let k = key(3, 3, dec!(0.98), dec!(5));
        for i in 0..16 {
            let v = idx.observe_live(k.clone(), Utc::now());
            assert!(v.is_new(), "real fill {i} must not be refused");
        }
        assert_eq!(idx.suspicious_suppressions(), 0);
    }

    #[test]
    fn ordinals_stay_bounded_because_tx_scopes_them() {
        // Ordinals are scoped by txHash, so they cannot grow without bound across time.
        let mut idx = DedupIndex::default();
        for t in 0..30u8 {
            for _ in 0..3 {
                idx.observe_live(key(t, 1, dec!(0.5), dec!(1)), Utc::now());
            }
        }
        assert_eq!(idx.tracked_contents(), 30);
        assert_eq!(idx.recorded_fills(), 90);
        // Every content's ordinal count is small, regardless of total volume.
        assert!(idx.counts.values().all(|c| *c <= 3));
    }
}
