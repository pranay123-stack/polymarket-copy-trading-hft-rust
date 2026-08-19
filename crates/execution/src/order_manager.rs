//! Order lifecycle management.
//!
//! Owns the mapping from our order ids to venue ids and drives every order through the
//! state machine. All transitions go through [`domain::Order::transition`], so an
//! illegal one is an error rather than a silent state corruption.
//!
//! On an ambiguous submission outcome — a timeout after the request went out — the order
//! is moved to `UNKNOWN` rather than `FAILED`. That distinction matters: `FAILED` says
//! "nothing exists at the venue", and being wrong about that loses a real position.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use domain::{
    Fill, Order, OrderId, OrderRequest, OrderState, Qty, SystemEvent, TokenId, Usd,
};
use parking_lot::RwLock;
use tracing::{error, info, warn};

use crate::adapter::{ExecutionAdapter, ExecutionError};

/// Outcome of trying to place an order.
#[derive(Debug)]
pub enum SubmitOutcome {
    Acknowledged { order_id: OrderId, fills: Vec<Fill> },
    Rejected { order_id: OrderId, reason: String },
    /// Requires reconciliation before the position can be trusted.
    Ambiguous { order_id: OrderId, detail: String },
}

pub struct OrderManager {
    adapter: Arc<dyn ExecutionAdapter>,
    orders: RwLock<HashMap<OrderId, Order>>,
    events: tokio::sync::broadcast::Sender<SystemEvent>,
}

impl OrderManager {
    pub fn new(
        adapter: Arc<dyn ExecutionAdapter>,
        events: tokio::sync::broadcast::Sender<SystemEvent>,
    ) -> Self {
        Self { adapter, orders: RwLock::new(HashMap::new()), events }
    }

    pub fn adapter_name(&self) -> &'static str { self.adapter.name() }
    pub fn is_real_money(&self) -> bool { self.adapter.capabilities().is_real_money }

    fn emit(&self, e: SystemEvent) { let _ = self.events.send(e); }

    pub fn get(&self, id: OrderId) -> Option<Order> { self.orders.read().get(&id).cloned() }

    pub fn all(&self) -> Vec<Order> {
        let mut v: Vec<_> = self.orders.read().values().cloned().collect();
        v.sort_by_key(|o| std::cmp::Reverse(o.request.created_at));
        v
    }

    pub fn open_orders(&self) -> Vec<Order> {
        self.orders.read().values().filter(|o| o.state.is_open()).cloned().collect()
    }

    pub fn open_count(&self) -> u32 { self.open_orders().len() as u32 }

    /// Orders whose outcome is unresolved. Reconciliation must clear these.
    pub fn unknown_orders(&self) -> Vec<Order> {
        self.orders.read().values().filter(|o| o.state == OrderState::Unknown).cloned().collect()
    }

    /// Net signed exposure per token from *filled* quantity only.
    pub fn filled_exposure(&self) -> HashMap<TokenId, rust_decimal::Decimal> {
        let mut m = HashMap::new();
        for o in self.orders.read().values() {
            if o.filled_qty.is_zero() { continue; }
            *m.entry(o.request.token_id.clone()).or_insert(rust_decimal::Decimal::ZERO) +=
                o.request.side.sign() * o.filled_qty.get();
        }
        m
    }

    /// Registers a risk-approved order. The `Validated` state is what makes it eligible
    /// for submission at all.
    pub fn register_validated(&self, req: OrderRequest, latency: domain::LatencyStamps) -> Result<OrderId, String> {
        let mut o = Order::new(req, latency).map_err(|e| e.to_string())?;
        o.transition(OrderState::Validated, Utc::now()).map_err(|e| e.to_string())?;
        let id = o.id();
        self.orders.write().insert(id, o);
        Ok(id)
    }

    /// Submits a registered order through the execution adapter.
    pub async fn submit(&self, id: OrderId) -> SubmitOutcome {
        let Some(mut order) = self.get(id) else {
            return SubmitOutcome::Rejected { order_id: id, reason: "unknown order".into() };
        };

        let now = Utc::now();
        if let Err(e) = order.transition(OrderState::Submitted, now) {
            return SubmitOutcome::Rejected { order_id: id, reason: e.to_string() };
        }
        order.latency.submission = Some(now);
        self.store(order.clone());
        self.emit(SystemEvent::OrderSubmitted {
            order_id: id, correlation_id: order.request.correlation_id, at: now });

        match self.adapter.submit(&order.request).await {
            Ok(ack) => {
                let at = Utc::now();
                order.latency.ack = Some(at);
                order.venue_order_id = ack.venue_order_id.clone();
                let _ = order.transition(OrderState::Acknowledged, at);
                self.emit(SystemEvent::OrderAcknowledged {
                    order_id: id,
                    correlation_id: order.request.correlation_id,
                    venue_order_id: ack.venue_order_id.clone(),
                    at,
                });

                let mut applied = Vec::new();
                for f in &ack.immediate_fills {
                    if order.latency.fill.is_none() { order.latency.fill = Some(f.filled_at); }
                    match order.apply_fill(f.quantity, f.price, f.fee, f.filled_at) {
                        Ok(()) => applied.push(f.clone()),
                        Err(e) => {
                            // An overfill is an integrity failure, not something to absorb.
                            error!(order = %id, error = %e, "venue fill rejected by order state machine");
                            let _ = order.transition(OrderState::Unknown, Utc::now());
                        }
                    }
                }
                let state = order.state;
                self.store(order.clone());
                for f in &applied {
                    let ev = if state == OrderState::Filled {
                        SystemEvent::OrderFilled {
                            order_id: id, correlation_id: order.request.correlation_id, fill: Box::new(f.clone()) }
                    } else {
                        SystemEvent::OrderPartiallyFilled {
                            order_id: id, correlation_id: order.request.correlation_id, fill: Box::new(f.clone()) }
                    };
                    self.emit(ev);
                }
                info!(order = %id, state = %state, fills = applied.len(), "order acknowledged");
                SubmitOutcome::Acknowledged { order_id: id, fills: applied }
            }
            Err(e) if e.requires_reconciliation() => {
                // Do NOT mark this failed: an order may exist at the venue.
                warn!(order = %id, error = %e, "ambiguous submission; entering UNKNOWN for reconciliation");
                let at = Utc::now();
                let _ = order.transition(OrderState::Unknown, at);
                order.reject_reason = Some(e.to_string());
                self.store(order.clone());
                SubmitOutcome::Ambiguous { order_id: id, detail: e.to_string() }
            }
            Err(e) => {
                let at = Utc::now();
                let _ = order.transition(OrderState::Rejected, at);
                order.reject_reason = Some(e.to_string());
                self.store(order.clone());
                self.emit(SystemEvent::OrderRejected {
                    order_id: id, correlation_id: order.request.correlation_id,
                    reason: e.to_string(), at });
                SubmitOutcome::Rejected { order_id: id, reason: e.to_string() }
            }
        }
    }

    /// Applies an asynchronously-delivered fill (resting order hit, or a push feed).
    pub fn apply_external_fill(&self, f: &Fill) -> Result<(), String> {
        let Some(mut o) = self.get(f.order_id) else {
            return Err(format!("fill for unknown order {}", f.order_id));
        };
        if o.latency.fill.is_none() { o.latency.fill = Some(f.filled_at); }
        o.apply_fill(f.quantity, f.price, f.fee, f.filled_at).map_err(|e| e.to_string())?;
        let state = o.state;
        let corr = o.request.correlation_id;
        self.store(o);
        let ev = if state == OrderState::Filled {
            SystemEvent::OrderFilled { order_id: f.order_id, correlation_id: corr, fill: Box::new(f.clone()) }
        } else {
            SystemEvent::OrderPartiallyFilled { order_id: f.order_id, correlation_id: corr, fill: Box::new(f.clone()) }
        };
        self.emit(ev);
        Ok(())
    }

    /// Requests cancellation of one order.
    pub async fn cancel(&self, id: OrderId) -> Result<(), String> {
        let Some(mut o) = self.get(id) else { return Err("unknown order".into()) };
        let vid = o.venue_order_id.clone();
        o.transition(OrderState::CancelRequested, Utc::now()).map_err(|e| e.to_string())?;
        self.store(o.clone());
        match self.adapter.cancel(id, vid.as_deref()).await {
            Ok(()) => {
                let at = Utc::now();
                // May legitimately fail if a fill won the race — that is not an error.
                if o.transition(OrderState::Cancelled, at).is_ok() {
                    self.store(o.clone());
                    self.emit(SystemEvent::OrderCancelled {
                        order_id: id, correlation_id: o.request.correlation_id, at });
                }
                Ok(())
            }
            Err(ExecutionError::UnknownOrder(_)) => {
                // Already gone at the venue.
                let at = Utc::now();
                if o.transition(OrderState::Cancelled, at).is_ok() { self.store(o); }
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Cancels every open order. Used by the kill switch.
    pub async fn cancel_all(&self) -> (u32, u32) {
        let open = self.open_orders();
        let (mut ok, mut fail) = (0, 0);
        for o in open {
            match self.cancel(o.id()).await {
                Ok(()) => ok += 1,
                Err(e) => { warn!(order = %o.id(), error = %e, "cancel failed"); fail += 1; }
            }
        }
        (ok, fail)
    }

    /// Rehydrates orders from persistence on startup.
    pub fn restore(&self, orders: Vec<Order>) {
        let mut g = self.orders.write();
        for o in orders { g.insert(o.id(), o); }
    }

    /// Total fees paid across all orders.
    pub fn total_fees(&self) -> Usd {
        self.orders.read().values().map(|o| o.fees_paid).sum()
    }

    /// Total filled quantity, for load-test assertions.
    pub fn total_filled(&self) -> Qty {
        self.orders.read().values().map(|o| o.filled_qty).sum()
    }

    fn store(&self, o: Order) { self.orders.write().insert(o.id(), o); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Acknowledgement, AdapterCapabilities, VenuePosition};
    use crate::paper::{BookCache, PaperExecution};
    use async_trait::async_trait;
    use domain::{
        CorrelationId, FillId, LatencyStamps, Level, MarketId, OrderType, Price, Side, TimeInForce,
    };
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn token() -> TokenId { TokenId::new("8320847481581361120679688919767116680249870957184742").unwrap() }

    fn req(qty: Decimal) -> OrderRequest {
        OrderRequest {
            order_id: OrderId::new(),
            correlation_id: CorrelationId::new(),
            signal_id: None,
            market_id: MarketId::new(format!("0x{:064x}", 1)).unwrap(),
            token_id: token(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            quantity: Qty::new(qty).unwrap(),
            limit_price: Price::new(dec!(0.60)).unwrap(),
            reference_price: Price::new(dec!(0.60)).unwrap(),
            tick_size: dec!(0.01),
            created_at: Utc::now(),
        }
    }

    fn book(ask: Decimal, size: Decimal) -> domain::OrderBook {
        domain::OrderBook {
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

    fn paper_om(ask: Decimal, size: Decimal) -> (OrderManager, tokio::sync::broadcast::Receiver<SystemEvent>) {
        let c = Arc::new(BookCache::new());
        c.put(book(ask, size));
        let p = PaperExecution::new(c, simulator::MatchParams {
            fee_bps: 0, slippage_bps: 0, partial_fill_enabled: true,
            fill_probability: 1.0, reject_probability: 0.0 }, 0, 0, 1);
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        (OrderManager::new(Arc::new(p), tx), rx)
    }

    /// An adapter that always times out — for the ambiguity path.
    struct TimeoutAdapter;
    #[async_trait]
    impl ExecutionAdapter for TimeoutAdapter {
        fn name(&self) -> &'static str { "timeout" }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities { supports_cancel: false, supports_position_query: false, is_real_money: false }
        }
        async fn is_ready(&self) -> bool { true }
        async fn submit(&self, o: &OrderRequest) -> Result<Acknowledgement, ExecutionError> {
            Err(ExecutionError::Ambiguous(format!("timeout on {}", o.order_id)))
        }
        async fn cancel(&self, _: OrderId, _: Option<&str>) -> Result<(), ExecutionError> { Ok(()) }
        async fn positions(&self) -> Result<Vec<VenuePosition>, ExecutionError> { Ok(vec![]) }
    }

    /// An adapter that reports a fill larger than requested.
    struct OverfillAdapter;
    #[async_trait]
    impl ExecutionAdapter for OverfillAdapter {
        fn name(&self) -> &'static str { "overfill" }
        fn capabilities(&self) -> AdapterCapabilities {
            AdapterCapabilities { supports_cancel: false, supports_position_query: false, is_real_money: false }
        }
        async fn is_ready(&self) -> bool { true }
        async fn submit(&self, o: &OrderRequest) -> Result<Acknowledgement, ExecutionError> {
            Ok(Acknowledgement {
                order_id: o.order_id,
                venue_order_id: Some("V1".into()),
                accepted_at: Utc::now(),
                immediate_fills: vec![Fill {
                    fill_id: FillId::new(), order_id: o.order_id, correlation_id: o.correlation_id,
                    market_id: o.market_id.clone(), token_id: o.token_id.clone(), side: o.side,
                    quantity: Qty::new(o.quantity.get() * dec!(2)).unwrap(), // twice what we asked
                    price: o.limit_price, fee: Usd::ZERO, venue_fill_id: None,
                    is_maker: false, filled_at: Utc::now(),
                }],
                terminal: true,
            })
        }
        async fn cancel(&self, _: OrderId, _: Option<&str>) -> Result<(), ExecutionError> { Ok(()) }
        async fn positions(&self) -> Result<Vec<VenuePosition>, ExecutionError> { Ok(vec![]) }
    }

    #[tokio::test]
    async fn happy_path_walks_the_full_state_machine() {
        let (om, _rx) = paper_om(dec!(0.50), dec!(10_000));
        let id = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        assert_eq!(om.get(id).unwrap().state, OrderState::Validated);
        let out = om.submit(id).await;
        assert!(matches!(out, SubmitOutcome::Acknowledged { .. }), "{out:?}");
        let o = om.get(id).unwrap();
        assert_eq!(o.state, OrderState::Filled);
        assert_eq!(o.filled_qty.get(), dec!(100));
        assert!(o.venue_order_id.is_some());
        // Latency stages were genuinely recorded.
        assert!(o.latency.submission.is_some() && o.latency.ack.is_some() && o.latency.fill.is_some());
    }

    #[tokio::test]
    async fn ambiguous_submission_becomes_unknown_not_failed() {
        // The property that prevents losing a real position on a timeout.
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let om = OrderManager::new(Arc::new(TimeoutAdapter), tx);
        let id = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        let out = om.submit(id).await;
        assert!(matches!(out, SubmitOutcome::Ambiguous { .. }), "{out:?}");
        let o = om.get(id).unwrap();
        assert_eq!(o.state, OrderState::Unknown);
        assert!(o.state.may_have_executed(), "UNKNOWN must be treated as possibly-executed");
        assert_eq!(om.unknown_orders().len(), 1);
    }

    #[tokio::test]
    async fn venue_overfill_is_refused_and_flagged() {
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let om = OrderManager::new(Arc::new(OverfillAdapter), tx);
        let id = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        om.submit(id).await;
        let o = om.get(id).unwrap();
        // The bogus fill must not be booked.
        assert_eq!(o.filled_qty, Qty::ZERO, "an overfill must never enter the position");
        assert_eq!(o.state, OrderState::Unknown, "and must be escalated for reconciliation");
    }

    #[tokio::test]
    async fn rejection_is_terminal_and_reported() {
        // Empty book -> paper adapter refuses.
        let c = Arc::new(BookCache::new());
        let p = PaperExecution::new(c, simulator::MatchParams::default(), 0, 0, 1);
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let om = OrderManager::new(Arc::new(p), tx);
        let id = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        let out = om.submit(id).await;
        assert!(matches!(out, SubmitOutcome::Rejected { .. }), "{out:?}");
        let o = om.get(id).unwrap();
        assert!(o.state.is_terminal());
        assert!(o.reject_reason.is_some());
    }

    #[tokio::test]
    async fn open_order_count_reflects_only_working_orders() {
        let (om, _rx) = paper_om(dec!(0.80), dec!(10_000)); // ask above our 0.60 limit -> rests
        let a = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        om.submit(a).await;
        assert_eq!(om.open_count(), 1);
        om.cancel(a).await.unwrap();
        assert_eq!(om.open_count(), 0);
        assert_eq!(om.get(a).unwrap().state, OrderState::Cancelled);
    }

    #[tokio::test]
    async fn cancel_all_clears_the_book_for_the_kill_switch() {
        let (om, _rx) = paper_om(dec!(0.80), dec!(10_000));
        for _ in 0..5 {
            let id = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
            om.submit(id).await;
        }
        assert_eq!(om.open_count(), 5);
        let (ok, fail) = om.cancel_all().await;
        assert_eq!((ok, fail), (5, 0));
        assert_eq!(om.open_count(), 0);
    }

    #[tokio::test]
    async fn events_are_emitted_for_the_whole_lifecycle() {
        let (om, mut rx) = paper_om(dec!(0.50), dec!(10_000));
        let id = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        om.submit(id).await;
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() { kinds.push(e.kind()); }
        assert!(kinds.contains(&"order_submitted"));
        assert!(kinds.contains(&"order_acknowledged"));
        assert!(kinds.contains(&"order_filled"));
    }

    #[tokio::test]
    async fn exposure_is_computed_from_filled_quantity_only() {
        let (om, _rx) = paper_om(dec!(0.50), dec!(10_000));
        let a = om.register_validated(req(dec!(100)), LatencyStamps::begin(Utc::now())).unwrap();
        om.submit(a).await;
        let e = om.filled_exposure();
        assert_eq!(e.get(&token()).copied().unwrap(), dec!(100));
    }

    #[tokio::test]
    async fn restored_orders_are_visible_after_restart() {
        let (om, _rx) = paper_om(dec!(0.50), dec!(10_000));
        let mut o = Order::new(req(dec!(50)), LatencyStamps::begin(Utc::now())).unwrap();
        o.transition(OrderState::Validated, Utc::now()).unwrap();
        o.transition(OrderState::Submitted, Utc::now()).unwrap();
        let id = o.id();
        om.restore(vec![o]);
        assert_eq!(om.get(id).unwrap().state, OrderState::Submitted);
        assert_eq!(om.open_count(), 1);
    }
}
