//! Target-wallet matching on the firehose hot path.
//!
//! The RTDS feed delivers **every** trade on the platform (~33/sec sustained, and
//! bursty well above that), and offers no server-side wallet filter. Every frame
//! therefore hits this matcher, and the overwhelming majority are not ours.
//!
//! The design consequence: matching must be a single O(1) hash lookup on an
//! already-normalised address, performed *before* any further work. Address
//! normalisation happens in `Address::new` at parse time — the RTDS feed sends EIP-55
//! mixed case while `data-api` sends lowercase, and comparing those raw would silently
//! match nothing at all.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use domain::{Address, SignalSkipReason, SourceTrade, TargetWallet, Usd};
use parking_lot::RwLock;
use market_data::ParsedTrade;

use crate::dedup::{BatchOrdinals, ContentKey, DedupIndex, DedupVerdict};

/// What happened to one observed trade.
#[derive(Debug, Clone, PartialEq)]
pub enum Detection {
    /// A tracked, enabled wallet traded and this is a fill we have not processed.
    Actionable(Box<SourceTrade>),
    /// A tracked wallet traded but the trade was deliberately not copied.
    Skipped { trader: Address, reason: SignalSkipReason },
    /// Not one of our wallets. The common case; costs one hash lookup.
    NotTracked,
}

/// Hot-path counters, surfaced as metrics.
#[derive(Debug, Default, Clone, Copy)]
pub struct TrackerStats {
    pub frames_examined: u64,
    pub wallet_matches: u64,
    pub duplicates_suppressed: u64,
    pub actionable: u64,
    pub skipped: u64,
}

/// Registry of tracked wallets plus the dedup index.
pub struct WalletTracker {
    wallets: Arc<RwLock<HashMap<Address, TargetWallet>>>,
    dedup: RwLock<DedupIndex>,
    stats: RwLock<TrackerStats>,
}

impl Default for WalletTracker {
    fn default() -> Self { Self::new(Vec::new()) }
}

impl WalletTracker {
    pub fn new(wallets: Vec<TargetWallet>) -> Self {
        let map = wallets.into_iter().map(|w| (w.address.clone(), w)).collect();
        Self {
            wallets: Arc::new(RwLock::new(map)),
            dedup: RwLock::new(DedupIndex::default()),
            stats: RwLock::new(TrackerStats::default()),
        }
    }

    pub fn stats(&self) -> TrackerStats { *self.stats.read() }
    pub fn wallet_count(&self) -> usize { self.wallets.read().len() }
    pub fn dedup_size(&self) -> usize { self.dedup.read().tracked_contents() }

    /// Fills refused for exceeding the per-transaction occurrence ceiling. Non-zero
    /// means upstream data is malformed and should be investigated.
    pub fn suspicious_suppressions(&self) -> u64 { self.dedup.read().suspicious_suppressions() }

    pub fn list_wallets(&self) -> Vec<TargetWallet> {
        let mut v: Vec<_> = self.wallets.read().values().cloned().collect();
        v.sort_by(|a, b| a.nickname.cmp(&b.nickname));
        v
    }

    pub fn get_wallet(&self, a: &Address) -> Option<TargetWallet> {
        self.wallets.read().get(a).cloned()
    }

    pub fn upsert_wallet(&self, w: TargetWallet) { self.wallets.write().insert(w.address.clone(), w); }

    pub fn remove_wallet(&self, a: &Address) -> Option<TargetWallet> { self.wallets.write().remove(a) }

    pub fn set_enabled(&self, a: &Address, enabled: bool) -> bool {
        match self.wallets.write().get_mut(a) {
            Some(w) => { w.enabled = enabled; true }
            None => false,
        }
    }

    /// Is this address one we copy? The only work done for the ~99% of frames that
    /// are not ours.
    #[inline]
    pub fn is_tracked(&self, a: &Address) -> bool { self.wallets.read().contains_key(a) }

    /// Processes one live-feed trade.
    pub fn observe_live(&self, t: ParsedTrade) -> Detection {
        self.observe(t, None)
    }

    /// Processes one backfilled trade. `batch` supplies ordinals for identical rows
    /// within the same response so overlap with the live feed reconciles exactly.
    pub fn observe_backfill(&self, t: ParsedTrade, batch: &mut BatchOrdinals) -> Detection {
        let key = Self::key_of(&t);
        let ordinal = batch.next(&key);
        self.observe(t, Some(ordinal))
    }

    fn key_of(t: &ParsedTrade) -> ContentKey {
        ContentKey::new(&t.tx_hash, &t.trader, &t.token_id, t.side, t.price, t.quantity)
    }

    fn observe(&self, t: ParsedTrade, backfill_ordinal: Option<u32>) -> Detection {
        {
            let mut s = self.stats.write();
            s.frames_examined += 1;
        }

        // --- hot path: one hash lookup, then out for non-targets ---
        let wallet = match self.wallets.read().get(&t.trader) {
            Some(w) => w.clone(),
            None => return Detection::NotTracked,
        };
        {
            let mut s = self.stats.write();
            s.wallet_matches += 1;
        }

        // Wallet-level admission (enabled, market lists, dust floor).
        let notional: Usd = t.quantity.notional(t.price);
        if let Err(reason) = wallet.admits(&t.market_id, notional) {
            self.stats.write().skipped += 1;
            return Detection::Skipped { trader: t.trader.clone(), reason };
        }

        // --- idempotency ---
        let key = Self::key_of(&t);
        let verdict = {
            let mut d = self.dedup.write();
            match backfill_ordinal {
                Some(o) => d.observe_backfill(key, o, t.detected_ts),
                None => d.observe_live(key, t.detected_ts),
            }
        };
        let (event_id, occurrence) = match verdict {
            DedupVerdict::New { event_id, occurrence } => (event_id, occurrence),
            DedupVerdict::Duplicate { .. } => {
                let mut s = self.stats.write();
                s.duplicates_suppressed += 1;
                s.skipped += 1;
                return Detection::Skipped {
                    trader: t.trader.clone(),
                    reason: SignalSkipReason::DuplicateEvent,
                };
            }
        };

        self.stats.write().actionable += 1;
        Detection::Actionable(Box::new(t.into_source_trade(event_id, occurrence)))
    }

    /// Restores dedup state on startup so persisted fills are not re-copied.
    pub fn restore_dedup(&self, entries: Vec<(ContentKey, u32, DateTime<Utc>)>) {
        let mut d = self.dedup.write();
        for (k, n, at) in entries {
            d.restore(k, n, at);
        }
    }

    /// Periodic maintenance: bounds dedup memory.
    pub fn evict_stale(&self, now: DateTime<Utc>) { self.dedup.write().evict_older_than(now); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{MarketId, Price, Qty, Side, TokenId, TradeSource, TxHash};
    use rust_decimal_macros::dec;

    fn addr(n: u8) -> Address { Address::new(format!("0x{:040x}", n)).unwrap() }

    fn trade(wallet: Address, tx: u8, size: rust_decimal::Decimal) -> ParsedTrade {
        ParsedTrade {
            trader: wallet,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("83208474815813611206796889197671166802498709571847428026387018914516677525784").unwrap(),
            outcome: "Down".into(),
            side: Side::Buy,
            price: Price::new(dec!(0.5)).unwrap(),
            quantity: Qty::new(size).unwrap(),
            tx_hash: TxHash::new(format!("0x{:064x}", tx)).unwrap(),
            source_ts: Utc::now(),
            source_is_coarse: false,
            detected_ts: Utc::now(),
            market_title: "T".into(),
            market_slug: "t".into(),
            source: TradeSource::RtdsWebsocket,
        }
    }

    fn tracker_with(n: u8) -> WalletTracker {
        let mut w = TargetWallet::new(addr(n), "Target");
        w.min_source_notional_usd = Usd::new(dec!(10));
        WalletTracker::new(vec![w])
    }

    #[test]
    fn untracked_wallets_are_dropped_immediately() {
        let t = tracker_with(1);
        assert_eq!(t.observe_live(trade(addr(2), 1, dec!(100))), Detection::NotTracked);
        let s = t.stats();
        assert_eq!(s.frames_examined, 1);
        assert_eq!(s.wallet_matches, 0, "non-targets must not do any further work");
    }

    #[test]
    fn tracked_wallet_produces_an_actionable_trade() {
        let t = tracker_with(1);
        match t.observe_live(trade(addr(1), 1, dec!(100))) {
            Detection::Actionable(st) => {
                assert_eq!(st.trader, addr(1));
                assert_eq!(st.occurrence, 0);
                assert_eq!(st.notional().get(), dec!(50));
            }
            other => panic!("expected actionable, got {other:?}"),
        }
        assert_eq!(t.stats().actionable, 1);
    }

    #[test]
    fn mixed_case_addresses_from_rtds_still_match() {
        // RTDS sends EIP-55; our config may hold lowercase. This must match.
        let w = TargetWallet::new(
            Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap(), "T");
        let t = WalletTracker::new(vec![w]);
        let mixed = Address::new("0x8a5152d056aDB066C9E4Dc65164620cDD82CeB6f").unwrap();
        assert!(t.is_tracked(&mixed));
        assert!(matches!(t.observe_live(trade(mixed, 1, dec!(1000))), Detection::Actionable(_)));
    }

    #[test]
    fn disabled_wallet_is_skipped_with_a_reason() {
        let t = tracker_with(1);
        t.set_enabled(&addr(1), false);
        match t.observe_live(trade(addr(1), 1, dec!(100))) {
            Detection::Skipped { reason, .. } => assert_eq!(reason, SignalSkipReason::WalletDisabled),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn dust_trades_are_skipped_not_copied() {
        let t = tracker_with(1);
        // notional = 1 * 0.5 = 0.50, below the 10 floor.
        match t.observe_live(trade(addr(1), 1, dec!(1))) {
            Detection::Skipped { reason, .. } => {
                assert!(matches!(reason, SignalSkipReason::BelowMinNotional { .. }));
            }
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn two_identical_live_fills_both_act_with_distinct_ids() {
        let t = tracker_with(1);
        let a = t.observe_live(trade(addr(1), 7, dec!(100)));
        let b = t.observe_live(trade(addr(1), 7, dec!(100)));
        let (ia, ib) = match (&a, &b) {
            (Detection::Actionable(x), Detection::Actionable(y)) => (x.event_id.clone(), y.event_id.clone()),
            _ => panic!("both should be actionable: {a:?} {b:?}"),
        };
        assert_ne!(ia, ib);
        assert_eq!(t.stats().duplicates_suppressed, 0);
    }

    #[test]
    fn backfill_overlap_is_suppressed() {
        let t = tracker_with(1);
        assert!(matches!(t.observe_live(trade(addr(1), 7, dec!(100))), Detection::Actionable(_)));
        let mut batch = BatchOrdinals::new();
        match t.observe_backfill(trade(addr(1), 7, dec!(100)), &mut batch) {
            Detection::Skipped { reason, .. } => assert_eq!(reason, SignalSkipReason::DuplicateEvent),
            other => panic!("re-delivered fill must be suppressed, got {other:?}"),
        }
        assert_eq!(t.stats().duplicates_suppressed, 1);
    }

    #[test]
    fn wallets_can_be_managed_at_runtime() {
        let t = tracker_with(1);
        assert_eq!(t.wallet_count(), 1);
        t.upsert_wallet(TargetWallet::new(addr(2), "Second"));
        assert_eq!(t.wallet_count(), 2);
        assert!(t.is_tracked(&addr(2)));
        assert!(t.remove_wallet(&addr(2)).is_some());
        assert!(!t.is_tracked(&addr(2)));
    }

    #[test]
    fn eviction_bounds_the_dedup_index() {
        let t = tracker_with(1);
        for i in 0..20u8 { t.observe_live(trade(addr(1), i, dec!(100))); }
        assert_eq!(t.dedup_size(), 20);
        t.evict_stale(Utc::now() + chrono::Duration::hours(24));
        assert_eq!(t.dedup_size(), 0);
    }
}
