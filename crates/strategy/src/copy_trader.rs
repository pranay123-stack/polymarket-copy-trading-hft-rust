//! The copy-trading strategy.
//!
//! Turns a detected [`SourceTrade`] into a [`CopySignal`]. The strategy has **no
//! knowledge of execution mode** — it emits an instruction, and whether that instruction
//! reaches a simulator or the real CLOB is decided further down the pipeline by the
//! `ExecutionAdapter` implementation. That separation is what makes paper results
//! meaningful: the identical code path produced them.

use chrono::Utc;
use domain::{
    CopySignal, LatencyStamps, OrderBook, Price, SignalId, SourceTrade, TargetWallet, TradeSource,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::sizing::{SizedOrder, SizingContext, SizingEngine, SizingRefusal};

/// Why no signal was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalRefusal {
    Sizing(SizingRefusal),
    /// The book was too stale to price against.
    StaleBook { age_ms: i64, max_age_ms: i64 },
}

#[derive(Debug, Clone)]
pub struct StrategyConfig {
    pub max_slippage_bps: u32,
    pub max_book_age_ms: i64,
    /// Fall back to the source trader's own price when we have no book at all.
    /// The resulting signal is marked with lower confidence.
    pub allow_pricing_without_book: bool,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self { max_slippage_bps: 50, max_book_age_ms: 30_000, allow_pricing_without_book: true }
    }
}

pub struct CopyTrader {
    cfg: StrategyConfig,
}

impl CopyTrader {
    pub fn new(cfg: StrategyConfig) -> Self { Self { cfg } }

    /// Builds a copy signal from an observed source trade.
    ///
    /// `book` is the current book for the *same* token, when available. It is used for
    /// the reference price and for liquidity-aware sizing; without it we fall back to
    /// the source trader's fill price and lower the signal's confidence accordingly.
    pub fn on_source_trade(
        &self,
        trade: &SourceTrade,
        wallet: &TargetWallet,
        book: Option<&OrderBook>,
        mut ctx: SizingContext,
        tick_size: Decimal,
    ) -> Result<CopySignal, SignalRefusal> {
        let now = Utc::now();

        // --- reference price ---
        // Prefer the live touch on the side we would trade: that is the price actually
        // obtainable now. The source trader's fill is already in the past, and on a
        // ~400ms-delayed feed it can be materially stale.
        let (reference, confidence) = match book {
            Some(b) => {
                let age = (now - b.timestamp).num_milliseconds();
                if age > self.cfg.max_book_age_ms {
                    return Err(SignalRefusal::StaleBook { age_ms: age, max_age_ms: self.cfg.max_book_age_ms });
                }
                let touch = match trade.side {
                    domain::Side::Buy => b.best_ask().map(|l| l.price),
                    domain::Side::Sell => b.best_bid().map(|l| l.price),
                };
                match touch {
                    Some(p) => (p, Self::confidence(trade, Some(age))),
                    None if self.cfg.allow_pricing_without_book => (trade.price, 0.4),
                    None => return Err(SignalRefusal::Sizing(SizingRefusal::NoLiquidity)),
                }
            }
            None if self.cfg.allow_pricing_without_book => (trade.price, 0.5),
            None => return Err(SignalRefusal::Sizing(SizingRefusal::NoLiquidity)),
        };

        let limit_price =
            SizingEngine::limit_price(reference, trade.side, self.cfg.max_slippage_bps, tick_size);

        // Liquidity within our limit, for risk-adjusted sizing.
        if let Some(b) = book {
            ctx.available_liquidity = b.notional_within(trade.side, limit_price);
        }
        ctx.min_order_size = if ctx.min_order_size > Decimal::ZERO {
            ctx.min_order_size
        } else {
            book.map(|b| b.min_order_size).unwrap_or(dec!(1))
        };

        let sized: SizedOrder =
            SizingEngine::size(wallet, trade.notional(), limit_price, &ctx).map_err(SignalRefusal::Sizing)?;

        let mut latency = LatencyStamps::from_source(trade.source_ts, false, trade.detected_ts);
        latency.signal = Some(now);

        Ok(CopySignal {
            signal_id: SignalId::new(),
            correlation_id: trade.correlation_id,
            source_event_id: trade.event_id.clone(),
            target_wallet: trade.trader.clone(),
            market_id: trade.market_id.clone(),
            token_id: trade.token_id.clone(),
            outcome: trade.outcome.clone(),
            side: trade.side,
            target_price: trade.price,
            target_quantity: trade.quantity,
            target_notional: trade.notional(),
            copy_quantity: sized.quantity,
            copy_notional: sized.notional,
            limit_price,
            sizing_mode: sized.mode.to_string(),
            confidence,
            source_ts: trade.source_ts,
            detection_ts: trade.detected_ts,
            signal_ts: now,
            latency,
            metadata: serde_json::json!({
                "reference_price": reference.get().to_string(),
                "binding_constraint": sized.binding_constraint,
                "source_feed": trade.source.as_str(),
                "occurrence": trade.occurrence,
                "tx_hash": trade.tx_hash.as_str(),
                "target_nickname": wallet.nickname,
            }),
        })
    }

    /// Confidence in the signal, in `0..=1`.
    ///
    /// Degrades with book staleness and with feed provenance: a backfilled trade is
    /// older and carries a second-resolution timestamp, so it is inherently less
    /// actionable than one straight off the live feed.
    fn confidence(trade: &SourceTrade, book_age_ms: Option<i64>) -> f64 {
        let mut c: f64 = match trade.source {
            TradeSource::RtdsWebsocket => 0.95,
            TradeSource::RestBackfill => 0.6,
            TradeSource::Replay => 0.9,
            TradeSource::Demo => 0.5,
        };
        // Our own detection lag.
        let lag = (trade.detected_ts - trade.source_ts).num_milliseconds().max(0);
        if lag > 2_000 { c *= 0.7; } else if lag > 1_000 { c *= 0.85; }
        if let Some(age) = book_age_ms {
            if age > 10_000 { c *= 0.7; } else if age > 2_000 { c *= 0.9; }
        }
        c.clamp(0.0, 1.0)
    }
}

/// Price we could actually have obtained, for measuring copy quality against the source.
pub fn achievable_price(book: &OrderBook, side: domain::Side, qty: domain::Qty, limit: Price) -> Option<Price> {
    book.sweep_vwap(side, qty, limit).map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{
        Address, CorrelationId, Level, MarketId, Qty, Side, SourceEventId, TokenId, TxHash, Usd,
    };

    fn book(bid: Decimal, ask: Decimal, size: Decimal, age_ms: i64) -> OrderBook {
        OrderBook {
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("8320847481581361120679688919767116680249870957184742802638701891451667752578").unwrap(),
            bids: vec![Level { price: Price::new(bid).unwrap(), size: Qty::new(size).unwrap() }],
            asks: vec![Level { price: Price::new(ask).unwrap(), size: Qty::new(size).unwrap() }],
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            timestamp: Utc::now() - chrono::Duration::milliseconds(age_ms),
            seq: 1,
        }
    }

    fn src(side: Side, price: Decimal, qty: Decimal, source: TradeSource) -> SourceTrade {
        SourceTrade {
            event_id: SourceEventId::from_digest("abc"),
            correlation_id: CorrelationId::new(),
            trader: Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap(),
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: TokenId::new("8320847481581361120679688919767116680249870957184742802638701891451667752578").unwrap(),
            outcome: "Yes".into(),
            side,
            price: Price::new(price).unwrap(),
            quantity: Qty::new(qty).unwrap(),
            tx_hash: TxHash::new(format!("0x{:064x}", 2)).unwrap(),
            occurrence: 0,
            source_ts: Utc::now() - chrono::Duration::milliseconds(400),
            detected_ts: Utc::now(),
            source,
            market_title: "T".into(),
            market_slug: "t".into(),
        }
    }

    fn ctx() -> SizingContext {
        SizingContext {
            equity: Usd::new(dec!(10_000)),
            current_market_exposure: Usd::ZERO,
            max_position_usd: Usd::new(dec!(1000)),
            max_trade_usd: Usd::new(dec!(500)),
            min_trade_usd: Usd::new(dec!(5)),
            available_liquidity: Usd::new(dec!(10_000)),
            remaining_daily_risk: Usd::new(dec!(1000)),
            min_order_size: dec!(1),
        }
    }

    fn wallet() -> TargetWallet {
        let mut w = TargetWallet::new(
            Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap(), "Whale");
        w.max_trade_usd = Usd::new(dec!(500));
        w.min_trade_usd = Usd::new(dec!(5));
        w
    }

    #[test]
    fn produces_a_signal_with_the_full_audit_chain() {
        let t = CopyTrader::new(StrategyConfig::default());
        let trade = src(Side::Buy, dec!(0.60), dec!(1000), TradeSource::RtdsWebsocket);
        let b = book(dec!(0.59), dec!(0.61), dec!(10_000), 100);
        let s = t.on_source_trade(&trade, &wallet(), Some(&b), ctx(), dec!(0.01)).unwrap();

        // Identity is threaded end to end.
        assert_eq!(s.correlation_id, trade.correlation_id);
        assert_eq!(s.source_event_id, trade.event_id);
        assert_eq!(s.target_notional.get(), dec!(600));
        // Default ratio 0.25 of $600 = $150.
        assert!(s.copy_notional.get() <= dec!(150));
        assert_eq!(s.side, Side::Buy);
        assert!(s.latency.signal.is_some());
        assert!(s.latency.detection_us().is_some());
    }

    #[test]
    fn buy_prices_against_the_ask_not_the_source_fill() {
        let t = CopyTrader::new(StrategyConfig::default());
        // Source got 0.60, but the market has moved and the ask is now 0.65.
        let trade = src(Side::Buy, dec!(0.60), dec!(1000), TradeSource::RtdsWebsocket);
        let b = book(dec!(0.64), dec!(0.65), dec!(10_000), 100);
        let s = t.on_source_trade(&trade, &wallet(), Some(&b), ctx(), dec!(0.01)).unwrap();
        // We must price off the obtainable ask, not the stale source fill.
        assert!(s.limit_price.get() >= dec!(0.65),
            "limit {} should be at or above the live ask", s.limit_price);
        assert_eq!(s.metadata["reference_price"], "0.65");
    }

    #[test]
    fn sell_prices_against_the_bid() {
        let t = CopyTrader::new(StrategyConfig::default());
        let trade = src(Side::Sell, dec!(0.60), dec!(1000), TradeSource::RtdsWebsocket);
        let b = book(dec!(0.55), dec!(0.65), dec!(10_000), 100);
        let s = t.on_source_trade(&trade, &wallet(), Some(&b), ctx(), dec!(0.01)).unwrap();
        assert_eq!(s.metadata["reference_price"], "0.55");
        assert!(s.limit_price.get() <= dec!(0.55), "a sell limit must not sit above the bid");
    }

    #[test]
    fn stale_books_are_refused_rather_than_traded_on() {
        let t = CopyTrader::new(StrategyConfig { max_book_age_ms: 5_000, ..Default::default() });
        let trade = src(Side::Buy, dec!(0.60), dec!(1000), TradeSource::RtdsWebsocket);
        let b = book(dec!(0.59), dec!(0.61), dec!(10_000), 30_000);
        assert!(matches!(
            t.on_source_trade(&trade, &wallet(), Some(&b), ctx(), dec!(0.01)),
            Err(SignalRefusal::StaleBook { .. })
        ));
    }

    #[test]
    fn missing_book_lowers_confidence_but_still_signals() {
        let t = CopyTrader::new(StrategyConfig::default());
        let trade = src(Side::Buy, dec!(0.60), dec!(1000), TradeSource::RtdsWebsocket);
        let s = t.on_source_trade(&trade, &wallet(), None, ctx(), dec!(0.01)).unwrap();
        assert!(s.confidence <= 0.5, "no book -> reduced confidence, got {}", s.confidence);
    }

    #[test]
    fn backfilled_trades_are_less_confident_than_live_ones() {
        let t = CopyTrader::new(StrategyConfig::default());
        let b = book(dec!(0.59), dec!(0.61), dec!(10_000), 100);
        let live = t.on_source_trade(&src(Side::Buy, dec!(0.6), dec!(1000), TradeSource::RtdsWebsocket),
            &wallet(), Some(&b), ctx(), dec!(0.01)).unwrap();
        let back = t.on_source_trade(&src(Side::Buy, dec!(0.6), dec!(1000), TradeSource::RestBackfill),
            &wallet(), Some(&b), ctx(), dec!(0.01)).unwrap();
        assert!(back.confidence < live.confidence);
    }

    #[test]
    fn sizing_refusal_propagates_with_its_reason() {
        let t = CopyTrader::new(StrategyConfig::default());
        let trade = src(Side::Buy, dec!(0.60), dec!(10), TradeSource::RtdsWebsocket); // $6 source
        let mut w = wallet();
        w.sizing = domain::SizingMode::FixedRatio { ratio: dec!(0.01) }; // -> $0.06
        let b = book(dec!(0.59), dec!(0.61), dec!(10_000), 100);
        assert!(matches!(
            t.on_source_trade(&trade, &w, Some(&b), ctx(), dec!(0.01)),
            Err(SignalRefusal::Sizing(SizingRefusal::BelowMinimum { .. }))
        ));
    }

    #[test]
    fn liquidity_is_read_from_the_book_for_risk_adjusted_sizing() {
        let t = CopyTrader::new(StrategyConfig::default());
        let mut w = wallet();
        w.sizing = domain::SizingMode::RiskAdjusted { base_ratio: dec!(1), liquidity_cap_pct: dec!(0.5) };
        let trade = src(Side::Buy, dec!(0.60), dec!(10_000), TradeSource::RtdsWebsocket);
        // Only 100 shares on the ask at 0.61 -> ~$61 of liquidity, 50% cap -> ~$30.
        let b = book(dec!(0.59), dec!(0.61), dec!(100), 100);
        let s = t.on_source_trade(&trade, &w, Some(&b), ctx(), dec!(0.01)).unwrap();
        assert!(s.copy_notional.get() <= dec!(35),
            "must be constrained by the thin book, got {}", s.copy_notional);
    }

    #[test]
    fn achievable_price_walks_the_real_book() {
        let mut b = book(dec!(0.59), dec!(0.61), dec!(100), 0);
        b.asks.push(Level { price: Price::new(dec!(0.62)).unwrap(), size: Qty::new(dec!(100)).unwrap() });
        let p = achievable_price(&b, Side::Buy, Qty::new(dec!(200)).unwrap(), Price::new(dec!(0.63)).unwrap()).unwrap();
        assert_eq!(p.get(), dec!(0.615));
    }
}
