//! Deterministic DEMO data generator.
//!
//! Lets the whole system be demonstrated with no credentials, no database and no network.
//!
//! **Demo data is never mixed with real data.** Every generated trade carries
//! `TradeSource::Demo`, which reports `is_real() == false`, and config validation refuses
//! `DEMO_DATA=true` in LIVE mode outright. Markets and wallets are prefixed `DEMO` so a
//! screenshot can never be mistaken for real activity.

use chrono::Utc;
use domain::{
    Address, CorrelationId, Level, MarketId, OrderBook, Price, Qty, Side, SourceEventId,
    SourceTrade, TargetWallet, TokenId, TradeSource, TxHash, Usd,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A synthetic market with a moving mid.
#[derive(Clone)]
pub struct DemoMarket {
    pub market_id: MarketId,
    pub yes_token: TokenId,
    pub no_token: TokenId,
    pub title: String,
    pub slug: String,
    pub mid: Decimal,
    pub tick: Decimal,
}

pub struct DemoGenerator {
    rng: ChaCha8Rng,
    pub markets: Vec<DemoMarket>,
    pub wallets: Vec<TargetWallet>,
    seq: u64,
}

impl DemoGenerator {
    pub fn new(seed: u64) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let titles = [
            ("DEMO: Will BTC close above $120k this month?", "demo-btc-120k"),
            ("DEMO: Will the Fed cut rates in Q4?", "demo-fed-cut-q4"),
            ("DEMO: Will Team A win the championship?", "demo-team-a-champ"),
            ("DEMO: Will it rain in London tomorrow?", "demo-london-rain"),
            ("DEMO: Will the bill pass before December?", "demo-bill-pass"),
        ];
        // The `unwrap()`s below are on values this function itself formats — `{:064x}`
        // always yields 64 hex chars, and the token ids are decimal by construction — so
        // they cannot fail. They are the only unwraps in this file.
        let markets = titles
            .iter()
            .enumerate()
            .map(|(i, (t, s))| DemoMarket {
                market_id: MarketId::new(format!("0x{:064x}", 0xDE0000 + i)).unwrap(),
                yes_token: TokenId::new(format!("{}", 900_000_000_000u64 + (i as u64 * 2))).unwrap(),
                no_token: TokenId::new(format!("{}", 900_000_000_001u64 + (i as u64 * 2))).unwrap(),
                title: (*t).to_string(),
                slug: (*s).to_string(),
                mid: Decimal::from(30 + rng.gen_range(0..40)) / dec!(100),
                tick: if i % 2 == 0 { dec!(0.01) } else { dec!(0.001) },
            })
            .collect();

        let wallets = [
            ("DEMO Whale", dec!(0.25), 300u32),
            ("DEMO Sharp", dec!(0.50), 200),
            ("DEMO Scalper", dec!(0.10), 100),
        ]
        .iter()
        .enumerate()
        .map(|(i, (name, ratio, max))| {
            let mut w = TargetWallet::new(
                Address::new(format!("0x{:040x}", 0xDEA0 + i)).unwrap(),
                (*name).to_string(),
            );
            w.sizing = domain::SizingMode::FixedRatio { ratio: *ratio };
            w.max_trade_usd = Usd::new(Decimal::from(*max));
            w.max_exposure_usd = Usd::new(dec!(1000));
            w.min_source_notional_usd = Usd::new(dec!(10));
            w
        })
        .collect();

        Self { rng, markets, wallets, seq: 0 }
    }

    /// Random-walks a market's mid so books and prices move realistically.
    pub fn step_prices(&mut self) {
        for m in self.markets.iter_mut() {
            let drift = Decimal::from(self.rng.gen_range(-3i32..=3)) / dec!(1000);
            m.mid = (m.mid + drift).clamp(dec!(0.05), dec!(0.95));
        }
    }

    /// A two-sided book around the current mid, with realistic depth.
    pub fn book(&mut self, market_idx: usize, yes_leg: bool) -> OrderBook {
        let m = self.markets[market_idx].clone();
        let mid = if yes_leg { m.mid } else { Decimal::ONE - m.mid };
        let tick = m.tick;
        let mut bids = Vec::new();
        let mut asks = Vec::new();
        for i in 1..=5i32 {
            let off = tick * Decimal::from(i);
            let size = Decimal::from(self.rng.gen_range(200..3000));
            if let (Ok(bp), Ok(sz)) = (Price::new((mid - off).max(dec!(0.001))), Qty::new(size)) {
                bids.push(Level { price: bp, size: sz });
            }
            let size = Decimal::from(self.rng.gen_range(200..3000));
            if let (Ok(ap), Ok(sz)) = (Price::new((mid + off).min(dec!(0.999))), Qty::new(size)) {
                asks.push(Level { price: ap, size: sz });
            }
        }
        self.seq += 1;
        OrderBook {
            market_id: m.market_id.clone(),
            token_id: if yes_leg { m.yes_token.clone() } else { m.no_token.clone() },
            bids,
            asks,
            tick_size: tick,
            min_order_size: dec!(5),
            timestamp: Utc::now(),
            seq: self.seq,
        }
    }

    /// A synthetic source trade from one of the demo wallets.
    pub fn source_trade(&mut self) -> SourceTrade {
        let mi = self.rng.gen_range(0..self.markets.len());
        let wi = self.rng.gen_range(0..self.wallets.len());
        let yes_leg = self.rng.gen_bool(0.5);
        let m = self.markets[mi].clone();
        let w = self.wallets[wi].clone();
        let side = if self.rng.gen_bool(0.6) { Side::Buy } else { Side::Sell };
        let mid = if yes_leg { m.mid } else { Decimal::ONE - m.mid };
        let px = Price::saturating(mid).round_to_tick(m.tick, side);
        let qty = Qty::new(Decimal::from(self.rng.gen_range(50..2000))).unwrap_or(Qty::ZERO);

        self.seq += 1;
        let now = Utc::now();
        // Mirrors the real feed's ~400ms publish delay so demo latency is plausible.
        let source_ts = now - chrono::Duration::milliseconds(self.rng.gen_range(300..500));
        let tx = TxHash::new(format!("0x{:064x}", 0xDE_0000_0000u64 + self.seq)).unwrap();

        SourceTrade {
            // Demo ids are clearly labelled and cannot collide with real SHA-256 digests.
            event_id: SourceEventId::from_digest(format!("demo-{:016x}", self.seq)),
            correlation_id: CorrelationId::new(),
            trader: w.address.clone(),
            market_id: m.market_id.clone(),
            token_id: if yes_leg { m.yes_token.clone() } else { m.no_token.clone() },
            outcome: if yes_leg { "Yes".into() } else { "No".into() },
            side,
            price: px,
            quantity: qty,
            tx_hash: tx,
            occurrence: 0,
            source_ts,
            detected_ts: now,
            source: TradeSource::Demo,
            market_title: m.title.clone(),
            market_slug: m.slug.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_data_is_labelled_and_never_counts_as_real() {
        let mut g = DemoGenerator::new(1);
        let t = g.source_trade();
        assert_eq!(t.source, TradeSource::Demo);
        assert!(!t.source.is_real(), "demo trades must never be treated as real");
        assert!(t.market_title.starts_with("DEMO"));
        assert!(t.event_id.as_str().starts_with("demo-"));
        assert!(g.wallets.iter().all(|w| w.nickname.starts_with("DEMO")));
    }

    #[test]
    fn generation_is_deterministic_for_a_given_seed() {
        let mut a = DemoGenerator::new(99);
        let mut b = DemoGenerator::new(99);
        for _ in 0..20 {
            let (x, y) = (a.source_trade(), b.source_trade());
            assert_eq!(x.price, y.price);
            assert_eq!(x.quantity, y.quantity);
            assert_eq!(x.trader, y.trader);
        }
    }

    #[test]
    fn generated_books_are_well_formed() {
        let mut g = DemoGenerator::new(7);
        for i in 0..5 {
            for leg in [true, false] {
                let b = g.book(i, leg);
                assert!(b.is_well_formed(), "demo book {i} leg {leg} is malformed");
                assert!(b.best_bid().is_some() && b.best_ask().is_some());
            }
        }
    }

    #[test]
    fn prices_random_walk_within_bounds() {
        let mut g = DemoGenerator::new(3);
        for _ in 0..1000 { g.step_prices(); }
        for m in &g.markets {
            assert!(m.mid >= dec!(0.05) && m.mid <= dec!(0.95), "mid escaped bounds: {}", m.mid);
        }
    }

    #[test]
    fn generated_prices_respect_the_market_tick() {
        let mut g = DemoGenerator::new(11);
        for _ in 0..50 {
            let t = g.source_trade();
            let m = g.markets.iter().find(|m| m.market_id == t.market_id).unwrap();
            let steps = t.price.get() / m.tick;
            assert_eq!(steps.fract(), Decimal::ZERO, "price {} is off tick {}", t.price, m.tick);
        }
    }

    #[test]
    fn demo_trades_carry_a_plausible_publish_delay() {
        let mut g = DemoGenerator::new(5);
        for _ in 0..20 {
            let t = g.source_trade();
            let lag = (t.detected_ts - t.source_ts).num_milliseconds();
            // Matches the ~392ms median measured on the real feed.
            assert!((300..=500).contains(&lag), "implausible demo lag {lag}ms");
        }
    }
}
