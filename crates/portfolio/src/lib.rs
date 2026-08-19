//! Portfolio, position and PnL accounting.
//!
//! One `Portfolio` owns all positions, cash and realised PnL. Every fill flows through
//! [`Portfolio::apply_fill`], which is the single place position state changes — there
//! is no other write path, so the book cannot drift through a side door.
//!
//! Marks are applied separately from fills, and **unrealised PnL is `None` while any
//! held position is unmarked**. An unmarked book must never masquerade as a flat one.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};
use domain::{
    Address, Fill, MarketId, PnlSnapshot, Position, Price, Qty, Side, TokenId, Usd,
};
use parking_lot::RwLock;
use rust_decimal::Decimal;

/// Attribution keys, so PnL can be sliced per wallet and per market.
#[derive(Debug, Clone, Default)]
struct Attribution {
    by_wallet: HashMap<Address, Usd>,
    by_market: HashMap<MarketId, Usd>,
}

pub struct Portfolio {
    cash: RwLock<Usd>,
    starting_cash: Usd,
    positions: RwLock<HashMap<TokenId, Position>>,
    realized: RwLock<Usd>,
    fees: RwLock<Usd>,
    peak_equity: RwLock<Usd>,
    /// (UTC ordinal day, realised PnL booked that day)
    daily: RwLock<(u32, Usd)>,
    attribution: RwLock<Attribution>,
    /// Which wallet's signal produced which order, for attribution.
    order_wallet: RwLock<HashMap<domain::OrderId, Address>>,
}

impl Portfolio {
    pub fn new(starting_cash: Usd) -> Self {
        Self {
            cash: RwLock::new(starting_cash),
            starting_cash,
            positions: RwLock::new(HashMap::new()),
            realized: RwLock::new(Usd::ZERO),
            fees: RwLock::new(Usd::ZERO),
            peak_equity: RwLock::new(starting_cash),
            daily: RwLock::new((Utc::now().ordinal(), Usd::ZERO)),
            attribution: RwLock::new(Attribution::default()),
            order_wallet: RwLock::new(HashMap::new()),
        }
    }

    pub fn cash(&self) -> Usd { *self.cash.read() }
    pub fn realized_pnl(&self) -> Usd { *self.realized.read() }
    pub fn fees_paid(&self) -> Usd { *self.fees.read() }

    /// Records which target wallet a copy order came from, for per-wallet PnL.
    pub fn attribute_order(&self, order: domain::OrderId, wallet: Address) {
        self.order_wallet.write().insert(order, wallet);
    }

    pub fn positions(&self) -> Vec<Position> {
        let mut v: Vec<_> = self.positions.read().values().cloned().collect();
        v.sort_by(|a, b| b.exposure().cmp(&a.exposure()));
        v
    }

    pub fn position(&self, t: &TokenId) -> Option<Position> { self.positions.read().get(t).cloned() }

    pub fn active_positions(&self) -> Vec<Position> {
        self.positions.read().values().filter(|p| !p.is_flat()).cloned().collect()
    }

    /// Signed quantity per token, for reconciliation.
    pub fn exposure_map(&self) -> HashMap<TokenId, Decimal> {
        self.positions.read().iter().map(|(t, p)| (t.clone(), p.net_quantity)).collect()
    }

    /// Absolute notional held in one token.
    pub fn token_exposure(&self, t: &TokenId) -> Usd {
        self.positions.read().get(t).map(|p| p.exposure()).unwrap_or(Usd::ZERO)
    }

    /// Absolute notional across every token in a market (both legs).
    pub fn market_exposure(&self, m: &MarketId) -> Usd {
        self.positions.read().values().filter(|p| &p.market_id == m).map(|p| p.exposure()).sum()
    }

    pub fn gross_exposure(&self) -> Usd {
        self.positions.read().values().map(|p| p.exposure()).sum()
    }

    pub fn wallet_pnl(&self, w: &Address) -> Usd {
        self.attribution.read().by_wallet.get(w).copied().unwrap_or(Usd::ZERO)
    }

    pub fn market_pnl(&self, m: &MarketId) -> Usd {
        self.attribution.read().by_market.get(m).copied().unwrap_or(Usd::ZERO)
    }

    /// Applies a fill: updates cash, the position, realised PnL, fees and attribution.
    /// This is the only path that mutates position state.
    pub fn apply_fill(&self, f: &Fill, outcome_name: &str) -> Position {
        // Cash moves by the signed notional, net of fees.
        *self.cash.write() += f.cash_delta();
        *self.fees.write() += f.fee;

        let mut positions = self.positions.write();
        let p = positions.entry(f.token_id.clone()).or_insert_with(|| {
            Position::new(f.market_id.clone(), f.token_id.clone(), outcome_name.to_string())
        });
        let realized = p.apply(f.side, f.quantity, f.price, f.fee, f.filled_at);
        let snapshot = p.clone();
        drop(positions);

        *self.realized.write() += realized;

        // Daily bucket, rolling over at the UTC day boundary.
        {
            let mut d = self.daily.write();
            let today = f.filled_at.ordinal();
            if d.0 != today { *d = (today, Usd::ZERO); }
            d.1 += realized;
        }

        // Attribution.
        {
            let mut a = self.attribution.write();
            *a.by_market.entry(f.market_id.clone()).or_insert(Usd::ZERO) += realized;
            if let Some(w) = self.order_wallet.read().get(&f.order_id) {
                *a.by_wallet.entry(w.clone()).or_insert(Usd::ZERO) += realized;
            }
        }

        self.refresh_peak();
        snapshot
    }

    /// Applies a mark to one token.
    pub fn mark(&self, t: &TokenId, price: Price, at: DateTime<Utc>) {
        if let Some(p) = self.positions.write().get_mut(t) {
            p.mark(price, at);
        }
        self.refresh_peak();
    }

    /// Mark-to-market value of open positions. `None` if any held position is unmarked.
    pub fn position_value(&self) -> Option<Usd> {
        let g = self.positions.read();
        let held: Vec<&Position> = g.values().filter(|p| !p.is_flat()).collect();
        if held.iter().any(|p| p.mark_price.is_none()) {
            return None;
        }
        Some(held.iter().map(|p| Usd::new(p.net_quantity * p.mark_price.map(|m| m.get()).unwrap_or(Decimal::ZERO))).sum())
    }

    pub fn unrealized_pnl(&self) -> Option<Usd> {
        let g = self.positions.read();
        let held: Vec<&Position> = g.values().filter(|p| !p.is_flat()).collect();
        if held.is_empty() { return Some(Usd::ZERO); }
        if held.iter().any(|p| p.mark_price.is_none()) { return None; }
        Some(held.iter().filter_map(|p| p.unrealized_pnl()).sum())
    }

    /// `cash + position value`. Falls back to cash + cost basis when unmarked, so
    /// equity is always defined; the ambiguity is exposed via `unrealized_pnl`.
    pub fn equity(&self) -> Usd {
        let pv = self.position_value().unwrap_or_else(|| {
            self.positions.read().values().map(|p| Usd::new(p.net_quantity * p.avg_entry)).sum()
        });
        self.cash() + pv
    }

    fn refresh_peak(&self) {
        let e = self.equity();
        let mut p = self.peak_equity.write();
        if e > *p { *p = e; }
    }

    pub fn peak_equity(&self) -> Usd { *self.peak_equity.read() }

    pub fn daily_pnl(&self) -> Usd {
        let d = self.daily.read();
        if d.0 != Utc::now().ordinal() { Usd::ZERO } else { d.1 }
    }

    pub fn total_pnl(&self) -> Usd {
        self.realized_pnl() + self.unrealized_pnl().unwrap_or(Usd::ZERO)
    }

    /// Return since inception, as a fraction of starting cash.
    pub fn return_pct(&self) -> Decimal {
        if self.starting_cash.get().is_zero() { return Decimal::ZERO; }
        (self.equity().get() - self.starting_cash.get()) / self.starting_cash.get()
    }

    pub fn snapshot(&self, open_orders: u32) -> PnlSnapshot {
        let equity = self.equity();
        PnlSnapshot {
            at: Utc::now(),
            cash: self.cash(),
            position_value: self.position_value().unwrap_or(Usd::ZERO),
            equity,
            realized_pnl: self.realized_pnl(),
            unrealized_pnl: self.unrealized_pnl(),
            fees_paid: self.fees_paid(),
            gross_exposure: self.gross_exposure(),
            daily_pnl: self.daily_pnl(),
            peak_equity: self.peak_equity(),
            open_orders,
            active_positions: self.active_positions().len() as u32,
        }
    }

    /// Rehydrates from persistence on startup.
    pub fn restore(&self, cash: Usd, positions: Vec<Position>, realized: Usd, fees: Usd) {
        *self.cash.write() = cash;
        *self.realized.write() = realized;
        *self.fees.write() = fees;
        let mut g = self.positions.write();
        g.clear();
        for p in positions { g.insert(p.token_id.clone(), p); }
        drop(g);
        self.refresh_peak();
    }

    /// Resets to the starting state — backs `POST /api/paper/reset`.
    pub fn reset(&self) {
        *self.cash.write() = self.starting_cash;
        self.positions.write().clear();
        *self.realized.write() = Usd::ZERO;
        *self.fees.write() = Usd::ZERO;
        *self.peak_equity.write() = self.starting_cash;
        *self.daily.write() = (Utc::now().ordinal(), Usd::ZERO);
        *self.attribution.write() = Attribution::default();
        self.order_wallet.write().clear();
    }
}

/// Convenience: build a fill for tests and the demo generator.
#[allow(clippy::too_many_arguments)]
pub fn make_fill(
    order_id: domain::OrderId,
    correlation_id: domain::CorrelationId,
    market_id: MarketId,
    token_id: TokenId,
    side: Side,
    quantity: Qty,
    price: Price,
    fee: Usd,
    at: DateTime<Utc>,
) -> Fill {
    Fill {
        fill_id: domain::FillId::new(),
        order_id,
        correlation_id,
        market_id,
        token_id,
        side,
        quantity,
        price,
        fee,
        venue_fill_id: None,
        is_maker: false,
        filled_at: at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{CorrelationId, OrderId};
    use rust_decimal_macros::dec;

    fn tok(n: u8) -> TokenId { TokenId::new(format!("{}", 5000 + n as u32)).unwrap() }
    fn mkt(n: u8) -> MarketId { MarketId::new(format!("0x{:064x}", n)).unwrap() }

    fn fill(t: u8, m: u8, side: Side, qty: Decimal, px: Decimal, fee: Decimal) -> Fill {
        make_fill(OrderId::new(), CorrelationId::new(), mkt(m), tok(t), side,
            Qty::new(qty).unwrap(), Price::new(px).unwrap(), Usd::new(fee), Utc::now())
    }

    fn pf() -> Portfolio { Portfolio::new(Usd::new(dec!(10_000))) }

    #[test]
    fn buying_moves_cash_and_creates_a_position() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0.5)), "Yes");
        // 100 * 0.60 = 60, plus 0.50 fee.
        assert_eq!(p.cash().get(), dec!(9939.5));
        assert_eq!(p.position(&tok(1)).unwrap().net_quantity, dec!(100));
        assert_eq!(p.fees_paid().get(), dec!(0.5));
    }

    #[test]
    fn round_trip_realises_the_spread_net_of_fees() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0)), "Yes");
        p.apply_fill(&fill(1, 1, Side::Sell, dec!(100), dec!(0.70), dec!(0)), "Yes");
        assert_eq!(p.realized_pnl().get(), dec!(10)); // 100 * 0.10
        assert!(p.position(&tok(1)).unwrap().is_flat());
        assert_eq!(p.cash().get(), dec!(10_010));
    }

    #[test]
    fn cash_and_equity_stay_consistent_through_a_round_trip() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(1)), "Yes");
        p.mark(&tok(1), Price::new(dec!(0.60)).unwrap(), Utc::now());
        // Paid 60 + 1 fee; position worth 60. Equity = 9939 + 60 = 9999.
        assert_eq!(p.equity().get(), dec!(9999));
        assert_eq!(p.total_pnl().get(), dec!(-1), "only the fee has been lost so far");
    }

    #[test]
    fn unrealised_is_unknown_until_the_position_is_marked() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0)), "Yes");
        assert_eq!(p.unrealized_pnl(), None, "an unmarked book must not look flat");
        assert_eq!(p.position_value(), None);
        p.mark(&tok(1), Price::new(dec!(0.75)).unwrap(), Utc::now());
        assert_eq!(p.unrealized_pnl().unwrap().get(), dec!(15));
        assert_eq!(p.position_value().unwrap().get(), dec!(75));
    }

    #[test]
    fn a_flat_book_has_zero_not_unknown_unrealised() {
        let p = pf();
        assert_eq!(p.unrealized_pnl(), Some(Usd::ZERO));
    }

    #[test]
    fn partially_marked_book_reports_unknown() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0)), "Yes");
        p.apply_fill(&fill(2, 2, Side::Buy, dec!(100), dec!(0.30), dec!(0)), "No");
        p.mark(&tok(1), Price::new(dec!(0.65)).unwrap(), Utc::now());
        assert_eq!(p.unrealized_pnl(), None, "one unmarked position makes the total unknown");
    }

    #[test]
    fn exposure_aggregates_per_token_and_per_market() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0)), "Yes");
        p.apply_fill(&fill(2, 1, Side::Buy, dec!(100), dec!(0.40), dec!(0)), "No");
        p.apply_fill(&fill(3, 2, Side::Buy, dec!(50), dec!(0.50), dec!(0)), "Yes");
        assert_eq!(p.token_exposure(&tok(1)).get(), dec!(60));
        // Both legs of market 1.
        assert_eq!(p.market_exposure(&mkt(1)).get(), dec!(100));
        assert_eq!(p.gross_exposure().get(), dec!(125));
    }

    #[test]
    fn pnl_is_attributed_to_the_originating_wallet() {
        let p = pf();
        let w = Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap();
        let buy = fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0));
        let mut sell = fill(1, 1, Side::Sell, dec!(100), dec!(0.70), dec!(0));
        sell.order_id = buy.order_id;
        p.attribute_order(buy.order_id, w.clone());
        p.apply_fill(&buy, "Yes");
        p.apply_fill(&sell, "Yes");
        assert_eq!(p.wallet_pnl(&w).get(), dec!(10));
        assert_eq!(p.market_pnl(&mkt(1)).get(), dec!(10));
    }

    #[test]
    fn drawdown_is_measured_from_peak_equity() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.50), dec!(0)), "Yes");
        p.mark(&tok(1), Price::new(dec!(0.90)).unwrap(), Utc::now());
        let peak = p.peak_equity();
        assert!(peak.get() > dec!(10_000));
        p.mark(&tok(1), Price::new(dec!(0.20)).unwrap(), Utc::now());
        let s = p.snapshot(0);
        assert!(s.drawdown_pct() > Decimal::ZERO, "a fall from the peak must show as drawdown");
        assert_eq!(s.peak_equity, peak, "the peak must not fall with equity");
    }

    #[test]
    fn snapshot_reports_a_coherent_view() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(0.5)), "Yes");
        p.mark(&tok(1), Price::new(dec!(0.65)).unwrap(), Utc::now());
        let s = p.snapshot(3);
        assert_eq!(s.open_orders, 3);
        assert_eq!(s.active_positions, 1);
        assert_eq!(s.fees_paid.get(), dec!(0.5));
        assert_eq!(s.equity, p.cash() + s.position_value);
        assert_eq!(s.unrealized_pnl.unwrap().get(), dec!(5));
    }

    #[test]
    fn short_positions_are_handled() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Sell, dec!(100), dec!(0.60), dec!(0)), "Yes");
        assert_eq!(p.position(&tok(1)).unwrap().net_quantity, dec!(-100));
        // Selling gives us cash.
        assert_eq!(p.cash().get(), dec!(10_060));
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.50), dec!(0)), "Yes");
        assert_eq!(p.realized_pnl().get(), dec!(10));
    }

    #[test]
    fn restore_rebuilds_state_after_a_restart() {
        let p = pf();
        let mut pos = Position::new(mkt(1), tok(1), "Yes".into());
        pos.apply(Side::Buy, Qty::new(dec!(100)).unwrap(), Price::new(dec!(0.6)).unwrap(), Usd::ZERO, Utc::now());
        p.restore(Usd::new(dec!(5000)), vec![pos], Usd::new(dec!(42)), Usd::new(dec!(3)));
        assert_eq!(p.cash().get(), dec!(5000));
        assert_eq!(p.realized_pnl().get(), dec!(42));
        assert_eq!(p.position(&tok(1)).unwrap().net_quantity, dec!(100));
    }

    #[test]
    fn reset_returns_to_the_starting_state() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(100), dec!(0.60), dec!(1)), "Yes");
        p.reset();
        assert_eq!(p.cash().get(), dec!(10_000));
        assert!(p.positions().is_empty());
        assert_eq!(p.realized_pnl(), Usd::ZERO);
        assert_eq!(p.fees_paid(), Usd::ZERO);
    }

    #[test]
    fn return_pct_tracks_equity_against_inception() {
        let p = pf();
        p.apply_fill(&fill(1, 1, Side::Buy, dec!(1000), dec!(0.50), dec!(0)), "Yes");
        p.mark(&tok(1), Price::new(dec!(0.60)).unwrap(), Utc::now());
        // Paid 500, now worth 600 -> +100 on 10k = 1%.
        assert_eq!(p.return_pct(), dec!(0.01));
    }

    #[test]
    fn many_fills_keep_cash_and_positions_consistent() {
        // Property-ish sweep: cash + cost basis must always equal starting cash minus fees.
        let p = pf();
        for i in 1..=20u8 {
            p.apply_fill(&fill(i % 5, 1, Side::Buy, dec!(10), dec!(0.5), dec!(0)), "Yes");
        }
        let cost: Decimal = p.positions().iter().map(|x| x.net_quantity * x.avg_entry).sum();
        assert_eq!(p.cash().get() + cost, dec!(10_000));
    }
}
