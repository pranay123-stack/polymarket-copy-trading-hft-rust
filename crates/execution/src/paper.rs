//! Paper execution adapter.
//!
//! Implements [`ExecutionAdapter`] against the [`simulator`] matching engine and a live
//! book cache. It is a *simulation of a venue*, not a rubber stamp: orders can rest, be
//! partially filled, be rejected, and fail for want of liquidity, exactly as they can
//! live.
//!
//! Simulated latency is applied as a real `sleep`, so the latency metrics the dashboard
//! displays in paper mode are measured the same way as in live mode rather than being
//! substituted with a constant.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use domain::{
    Fill, FillId, OrderBook, OrderId, OrderRequest, Qty, TimeInForce, TokenId, Usd,
};
use parking_lot::{Mutex, RwLock};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rust_decimal::Decimal;
use simulator::{MatchParams, MatchingEngine, SimOutcome};
use tracing::{debug, warn};

use crate::adapter::{
    Acknowledgement, AdapterCapabilities, ExecutionAdapter, ExecutionError, VenuePosition,
};

/// Shared cache of the latest book per token, written by the market-data task.
#[derive(Default)]
pub struct BookCache {
    books: RwLock<HashMap<TokenId, OrderBook>>,
}

impl BookCache {
    pub fn new() -> Self { Self::default() }
    pub fn put(&self, b: OrderBook) { self.books.write().insert(b.token_id.clone(), b); }
    pub fn get(&self, t: &TokenId) -> Option<OrderBook> { self.books.read().get(t).cloned() }
    pub fn len(&self) -> usize { self.books.read().len() }
    pub fn is_empty(&self) -> bool { self.books.read().is_empty() }
}

/// An order the simulator is holding as resting.
#[derive(Debug, Clone)]
struct RestingOrder {
    request: OrderRequest,
    remaining: Qty,
}

pub struct PaperExecution {
    books: Arc<BookCache>,
    params: MatchParams,
    latency_ms: u64,
    latency_jitter_ms: u64,
    rng: Mutex<ChaCha8Rng>,
    resting: RwLock<HashMap<OrderId, RestingOrder>>,
    positions: RwLock<HashMap<TokenId, Decimal>>,
    pending_fills: Mutex<Vec<Fill>>,
    seq: Mutex<u64>,
}

impl PaperExecution {
    pub fn new(books: Arc<BookCache>, params: MatchParams, latency_ms: u64, latency_jitter_ms: u64, seed: u64) -> Self {
        Self {
            books,
            params,
            latency_ms,
            latency_jitter_ms,
            rng: Mutex::new(ChaCha8Rng::seed_from_u64(seed)),
            resting: RwLock::new(HashMap::new()),
            positions: RwLock::new(HashMap::new()),
            pending_fills: Mutex::new(Vec::new()),
            seq: Mutex::new(0),
        }
    }

    pub fn from_config(books: Arc<BookCache>, cfg: &config::SimulationConfig) -> Self {
        Self::new(
            books,
            MatchParams {
                fee_bps: cfg.fee_bps,
                slippage_bps: cfg.slippage_bps,
                partial_fill_enabled: cfg.partial_fill_enabled,
                fill_probability: cfg.fill_probability,
                reject_probability: cfg.reject_probability,
            },
            cfg.latency_ms,
            cfg.latency_jitter_ms,
            cfg.rng_seed,
        )
    }

    fn next_venue_id(&self) -> String {
        let mut s = self.seq.lock();
        *s += 1;
        format!("PAPER-{:012}", *s)
    }

    /// Sleeps for the configured round-trip, so measured latency is real.
    async fn simulate_latency(&self) {
        let jitter = if self.latency_jitter_ms > 0 {
            self.rng.lock().gen_range(0..=self.latency_jitter_ms)
        } else { 0 };
        tokio::time::sleep(Duration::from_millis(self.latency_ms + jitter)).await;
    }

    fn record_fill(&self, o: &OrderRequest, qty: Qty, price: domain::Price, fee: Usd, is_maker: bool) -> Fill {
        let mut p = self.positions.write();
        let e = p.entry(o.token_id.clone()).or_insert(Decimal::ZERO);
        *e += o.side.sign() * qty.get();
        Fill {
            fill_id: FillId::new(),
            order_id: o.order_id,
            correlation_id: o.correlation_id,
            market_id: o.market_id.clone(),
            token_id: o.token_id.clone(),
            side: o.side,
            quantity: qty,
            price,
            fee,
            venue_fill_id: Some(format!("PAPERFILL-{}", FillId::new())),
            is_maker,
            filled_at: Utc::now(),
        }
    }

    /// Re-examines resting orders against a new book. Called by the market-data loop so
    /// resting orders fill when the market genuinely trades through them.
    pub fn on_book_update(&self, book: &OrderBook) -> Vec<Fill> {
        let candidates: Vec<(OrderId, RestingOrder)> = self
            .resting
            .read()
            .iter()
            .filter(|(_, r)| r.request.token_id == book.token_id)
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        let mut fills = Vec::new();
        for (id, r) in candidates {
            if !MatchingEngine::is_marketable(&r.request, book) {
                continue;
            }
            let mut probe = r.request.clone();
            probe.quantity = r.remaining;
            let outcome = {
                let mut rng = self.rng.lock();
                MatchingEngine::match_order(&probe, book, &self.params, &mut *rng)
            };
            if let SimOutcome::Filled { quantity, price, fee, .. } = outcome {
                // A resting order that gets hit was providing liquidity, so it is always a
                // maker regardless of what the matcher reported for the taker path.
                const RESTING_IS_MAKER: bool = true;
                let f = self.record_fill(&r.request, quantity, price, fee, RESTING_IS_MAKER);
                let left = r.remaining.saturating_sub(quantity);
                if left.is_zero() {
                    self.resting.write().remove(&id);
                } else if let Some(e) = self.resting.write().get_mut(&id) {
                    e.remaining = left;
                }
                debug!(order = %id, qty = %quantity, "paper resting order filled");
                fills.push(f);
            }
        }
        if !fills.is_empty() {
            self.pending_fills.lock().extend(fills.iter().cloned());
        }
        fills
    }

    pub fn resting_count(&self) -> usize { self.resting.read().len() }

    /// Clears all simulated state — backs `POST /api/paper/reset`.
    pub fn reset(&self) {
        self.resting.write().clear();
        self.positions.write().clear();
        self.pending_fills.lock().clear();
    }
}

#[async_trait]
impl ExecutionAdapter for PaperExecution {
    fn name(&self) -> &'static str { "paper" }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            supports_cancel: true,
            supports_position_query: true,
            is_real_money: false,
        }
    }

    async fn is_ready(&self) -> bool { true }

    async fn submit(&self, order: &OrderRequest) -> Result<Acknowledgement, ExecutionError> {
        // Real sleep: latency metrics in paper mode are measured, not fabricated.
        self.simulate_latency().await;

        let Some(book) = self.books.get(&order.token_id) else {
            // No market data for this token — refuse rather than invent a fill.
            warn!(token = %order.token_id, "paper submit with no book available");
            return Err(ExecutionError::NotReady(format!(
                "no order book cached for token {}", order.token_id)));
        };

        let outcome = {
            let mut rng = self.rng.lock();
            MatchingEngine::match_order(order, &book, &self.params, &mut *rng)
        };

        let now = Utc::now();
        match outcome {
            SimOutcome::Rejected { reason } => Err(ExecutionError::Rejected(reason)),
            SimOutcome::NoLiquidity => Err(ExecutionError::Rejected(
                "insufficient liquidity within limit price".into())),
            SimOutcome::Resting => {
                self.resting.write().insert(order.order_id, RestingOrder {
                    request: order.clone(), remaining: order.quantity });
                Ok(Acknowledgement {
                    order_id: order.order_id,
                    venue_order_id: Some(self.next_venue_id()),
                    accepted_at: now,
                    immediate_fills: Vec::new(),
                    terminal: false,
                })
            }
            SimOutcome::Filled { quantity, price, fee, is_maker } => {
                let fill = self.record_fill(order, quantity, price, fee, is_maker);
                let complete = quantity >= order.quantity;
                if !complete && order.time_in_force == TimeInForce::Gtc {
                    // Remainder rests, as it would at the venue.
                    self.resting.write().insert(order.order_id, RestingOrder {
                        request: order.clone(),
                        remaining: order.quantity.saturating_sub(quantity),
                    });
                }
                Ok(Acknowledgement {
                    order_id: order.order_id,
                    venue_order_id: Some(self.next_venue_id()),
                    accepted_at: now,
                    immediate_fills: vec![fill],
                    terminal: complete || order.time_in_force != TimeInForce::Gtc,
                })
            }
        }
    }

    async fn cancel(&self, order_id: OrderId, _venue: Option<&str>) -> Result<(), ExecutionError> {
        self.simulate_latency().await;
        match self.resting.write().remove(&order_id) {
            Some(_) => Ok(()),
            None => Err(ExecutionError::UnknownOrder(order_id)),
        }
    }

    async fn positions(&self) -> Result<Vec<VenuePosition>, ExecutionError> {
        Ok(self
            .positions
            .read()
            .iter()
            .filter(|(_, q)| !q.is_zero())
            .map(|(t, q)| VenuePosition {
                token_id: t.clone(),
                quantity: Qty::new(q.abs()).unwrap_or(Qty::ZERO),
            })
            .collect())
    }

    async fn poll_fills(&self) -> Result<Vec<Fill>, ExecutionError> {
        Ok(std::mem::take(&mut *self.pending_fills.lock()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{CorrelationId, Level, MarketId, OrderType, Price, Side};
    use rust_decimal_macros::dec;

    fn token() -> TokenId {
        TokenId::new("83208474815813611206796889197671166802498709571847428026387").unwrap()
    }

    fn book(ask: Decimal, size: Decimal) -> OrderBook {
        OrderBook {
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: token(),
            bids: vec![Level { price: Price::new(dec!(0.40)).unwrap(), size: Qty::new(size).unwrap() }],
            asks: vec![Level { price: Price::new(ask).unwrap(), size: Qty::new(size).unwrap() }],
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            timestamp: Utc::now(),
            seq: 1,
        }
    }

    fn order(side: Side, qty: Decimal, limit: Decimal, tif: TimeInForce) -> OrderRequest {
        OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: token(),
            side,
            order_type: OrderType::Limit,
            time_in_force: tif,
            quantity: Qty::new(qty).unwrap(),
            limit_price: Price::new(limit).unwrap(),
            reference_price: Price::new(limit).unwrap(),
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        }
    }

    fn certain_params() -> MatchParams {
        MatchParams { fee_bps: 0, slippage_bps: 0, partial_fill_enabled: true,
            fill_probability: 1.0, reject_probability: 0.0 }
    }

    fn paper(books: Arc<BookCache>) -> PaperExecution {
        PaperExecution::new(books, certain_params(), 0, 0, 42)
    }

    #[tokio::test]
    async fn adapter_reports_it_is_not_real_money() {
        let p = paper(Arc::new(BookCache::new()));
        assert_eq!(p.name(), "paper");
        assert!(!p.capabilities().is_real_money, "paper must never claim to be real money");
        assert!(p.is_ready().await);
    }

    #[tokio::test]
    async fn marketable_order_fills_against_the_cached_book() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.50), dec!(1000)));
        let p = paper(c);
        let ack = p.submit(&order(Side::Buy, dec!(100), dec!(0.60), TimeInForce::Gtc)).await.unwrap();
        assert_eq!(ack.immediate_fills.len(), 1);
        assert!(ack.terminal);
        assert_eq!(ack.immediate_fills[0].quantity.get(), dec!(100));
        assert!(ack.venue_order_id.is_some());
    }

    #[tokio::test]
    async fn submitting_without_market_data_refuses_instead_of_inventing_a_fill() {
        // The failure mode this guards against: a paper engine that "fills" everything.
        let p = paper(Arc::new(BookCache::new()));
        let e = p.submit(&order(Side::Buy, dec!(100), dec!(0.60), TimeInForce::Gtc)).await.unwrap_err();
        assert!(matches!(e, ExecutionError::NotReady(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn unmarketable_order_rests_and_can_be_cancelled() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.60), dec!(1000)));
        let p = paper(c);
        let o = order(Side::Buy, dec!(100), dec!(0.45), TimeInForce::Gtc);
        let ack = p.submit(&o).await.unwrap();
        assert!(ack.immediate_fills.is_empty());
        assert!(!ack.terminal);
        assert_eq!(p.resting_count(), 1);
        p.cancel(o.order_id, None).await.unwrap();
        assert_eq!(p.resting_count(), 0);
    }

    #[tokio::test]
    async fn cancelling_an_unknown_order_is_an_error() {
        let p = paper(Arc::new(BookCache::new()));
        assert!(matches!(p.cancel(OrderId::new(), None).await, Err(ExecutionError::UnknownOrder(_))));
    }

    #[tokio::test]
    async fn resting_order_fills_when_the_market_comes_to_it() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.60), dec!(1000)));
        let p = paper(c.clone());
        let o = order(Side::Buy, dec!(100), dec!(0.45), TimeInForce::Gtc);
        p.submit(&o).await.unwrap();
        assert_eq!(p.resting_count(), 1);

        // Market falls through our resting bid.
        let cheaper = book(dec!(0.44), dec!(1000));
        let fills = p.on_book_update(&cheaper);
        assert_eq!(fills.len(), 1);
        assert!(fills[0].is_maker, "a resting order that gets hit is a maker");
        assert_eq!(p.resting_count(), 0);
    }

    #[tokio::test]
    async fn partial_fill_leaves_the_remainder_resting() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.50), dec!(40))); // only 40 available
        let p = paper(c);
        let ack = p.submit(&order(Side::Buy, dec!(100), dec!(0.60), TimeInForce::Gtc)).await.unwrap();
        assert_eq!(ack.immediate_fills[0].quantity.get(), dec!(40));
        assert!(!ack.terminal, "60 remain to be filled");
        assert_eq!(p.resting_count(), 1);
    }

    #[tokio::test]
    async fn ioc_does_not_leave_a_remainder_resting() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.50), dec!(40)));
        let p = paper(c);
        let ack = p.submit(&order(Side::Buy, dec!(100), dec!(0.60), TimeInForce::Ioc)).await.unwrap();
        assert!(ack.terminal);
        assert_eq!(p.resting_count(), 0, "IOC must cancel its remainder");
    }

    #[tokio::test]
    async fn positions_track_signed_exposure() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.50), dec!(10_000)));
        let p = paper(c);
        p.submit(&order(Side::Buy, dec!(100), dec!(0.60), TimeInForce::Gtc)).await.unwrap();
        let pos = p.positions().await.unwrap();
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].quantity.get(), dec!(100));

        // Selling it back should flatten, and a flat position must not be reported.
        let mut sell = order(Side::Sell, dec!(100), dec!(0.30), TimeInForce::Gtc);
        sell.limit_price = Price::new(dec!(0.30)).unwrap();
        p.submit(&sell).await.unwrap();
        assert!(p.positions().await.unwrap().is_empty(), "flat positions must not linger");
    }

    #[tokio::test]
    async fn fills_are_drained_exactly_once() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.60), dec!(1000)));
        let p = paper(c);
        p.submit(&order(Side::Buy, dec!(100), dec!(0.45), TimeInForce::Gtc)).await.unwrap();
        p.on_book_update(&book(dec!(0.44), dec!(1000)));
        assert_eq!(p.poll_fills().await.unwrap().len(), 1);
        assert!(p.poll_fills().await.unwrap().is_empty(), "a fill must not be reported twice");
    }

    #[tokio::test]
    async fn simulated_latency_is_actually_elapsed_not_faked() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.50), dec!(1000)));
        let p = PaperExecution::new(c, certain_params(), 60, 0, 42);
        let t0 = std::time::Instant::now();
        p.submit(&order(Side::Buy, dec!(100), dec!(0.60), TimeInForce::Gtc)).await.unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(55),
            "latency must be real so measurements are real, elapsed {:?}", t0.elapsed());
    }

    #[tokio::test]
    async fn reset_clears_all_simulated_state() {
        let c = Arc::new(BookCache::new());
        c.put(book(dec!(0.60), dec!(1000)));
        let p = paper(c);
        p.submit(&order(Side::Buy, dec!(100), dec!(0.45), TimeInForce::Gtc)).await.unwrap();
        assert_eq!(p.resting_count(), 1);
        p.reset();
        assert_eq!(p.resting_count(), 0);
        assert!(p.positions().await.unwrap().is_empty());
    }
}
