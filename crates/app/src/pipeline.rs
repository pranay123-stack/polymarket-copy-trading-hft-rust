//! The trading pipeline.
//!
//! ```text
//! source trade -> wallet match -> dedup -> strategy -> risk -> OMS -> execution
//!                                                                        |
//!                                                          portfolio <- fills
//! ```
//!
//! One function, [`Pipeline::on_source_trade`], carries a detected trade through every
//! stage and stamps latency as it goes. It is mode-agnostic: paper, replay and live all
//! run this exact code, differing only in which `ExecutionAdapter` sits behind the OMS.
//!
//! Every early exit is explicit and observable. A trade that is skipped, refused by
//! sizing, or rejected by risk emits an event saying so — the system never silently
//! drops a target's trade.

use std::sync::Arc;

use chrono::Utc;
use domain::{
    CopySignal, LatencyStamps, OrderRequest, OrderType, SignalSkipReason, SourceTrade, SystemEvent,
    TimeInForce, Usd,
};
use execution::{BookCache, SubmitOutcome};
use market_data::{PolymarketRest, TokenSubscriptions};
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicU64, Ordering};
use strategy::{CopyTrader, SignalRefusal, SizingContext};
use tracing::{debug, info, warn};

use api::state::{AppState, CopyRow};

pub struct Pipeline {
    pub state: Arc<AppState>,
    pub trader: Arc<CopyTrader>,
    pub books: Arc<BookCache>,
    /// Used to fetch a book on demand when a target trades a token we have not cached.
    rest: Option<Arc<PolymarketRest>>,
    /// Tokens the market stream follows. A token traded once is subscribed, so every
    /// later trade in that market is priced from the live book with no network call.
    subs: Option<Arc<TokenSubscriptions>>,
    book_seq: AtomicU64,
    /// Observability for the on-demand path: how often we had to pay a REST round trip
    /// versus pricing straight from the stream.
    pub books_on_demand: AtomicU64,
    pub books_unavailable: AtomicU64,
    pub books_from_stream: AtomicU64,
}

impl Pipeline {
    pub fn new(state: Arc<AppState>, trader: Arc<CopyTrader>, books: Arc<BookCache>) -> Self {
        Self {
            state,
            trader,
            books,
            rest: None,
            subs: None,
            book_seq: AtomicU64::new(1_000_000),
            books_on_demand: AtomicU64::new(0),
            books_unavailable: AtomicU64::new(0),
            books_from_stream: AtomicU64::new(0),
        }
    }

    /// Enables on-demand book fetching.
    pub fn with_rest(mut self, rest: Arc<PolymarketRest>) -> Self {
        self.rest = Some(rest);
        self
    }

    /// Enables streaming books, which removes the REST round trip from the hot path
    /// after the first trade in a market.
    pub fn with_subscriptions(mut self, subs: Arc<TokenSubscriptions>) -> Self {
        self.subs = Some(subs);
        self
    }

    /// Returns the book for `token`, fetching it if it is not cached.
    ///
    /// This is load-bearing for real trading, not an optimisation. Target wallets trade
    /// across the *entire* platform — well over a thousand markets — while the background
    /// market-data task can only keep a bounded working set warm. Measured against the
    /// live feed with six real target wallets, **every** copy was rejected with "no order
    /// book cached" because the token that was just traded had never been seeded.
    ///
    /// So the book for a token we are about to trade is fetched at signal time. It costs
    /// one REST round trip on the first touch of a token (~100-300ms, on top of the
    /// venue's own ~400ms publish delay) and is cached for subsequent trades in the same
    /// market. That latency is real and is measured like everything else — it is not
    /// hidden, and `books_on_demand` counts how often the path is taken.
    async fn book_for(&self, token: &domain::TokenId) -> Option<domain::OrderBook> {
        if let Some(b) = self.books.get(token) {
            // Only reuse a cached book while it is fresh enough to price against.
            let age = (Utc::now() - b.timestamp).num_milliseconds();
            if age <= self.state.config.risk.max_market_data_age_ms {
                self.books_from_stream.fetch_add(1, Ordering::Relaxed);
                return Some(b);
            }
        }
        // Follow this token from now on, so the next trade in the same market is priced
        // from the stream instead of another REST call.
        if let Some(s) = &self.subs {
            if s.add(token.clone()) {
                info!(token = %token, followed = s.len(), "following a newly-seen token");
            }
        }
        let rest = self.rest.as_ref()?;
        let seq = self.book_seq.fetch_add(1, Ordering::Relaxed);
        match rest.book(token, seq).await {
            Ok(b) => {
                self.books_on_demand.fetch_add(1, Ordering::Relaxed);
                self.books.put(b.clone());
                Some(b)
            }
            Err(e) => {
                self.books_unavailable.fetch_add(1, Ordering::Relaxed);
                warn!(token = %token, error = %e, "on-demand book fetch failed");
                None
            }
        }
    }

    fn emit(&self, e: SystemEvent) {
        let _ = self.state.events.send(e.clone());
        self.state.recent.add_event(e);
    }

    /// Builds the sizing context from live portfolio state.
    fn sizing_context(&self, t: &SourceTrade) -> SizingContext {
        let s = self.state.portfolio.snapshot(self.state.orders.open_count());
        let limits = self.state.risk.read().limits().clone();
        // Remaining daily loss budget: the limit less what has already been lost today.
        let lost_today = (-s.daily_pnl).max(Usd::ZERO);
        SizingContext {
            equity: s.equity,
            current_market_exposure: self.state.portfolio.market_exposure(&t.market_id),
            max_position_usd: limits.max_position_usd,
            max_trade_usd: limits.max_trade_usd,
            min_trade_usd: limits.min_trade_usd,
            available_liquidity: Usd::ZERO, // filled in by the strategy from the book
            remaining_daily_risk: (limits.max_daily_loss_usd - lost_today).max(Usd::ZERO),
            min_order_size: Decimal::ZERO,
        }
    }

    /// Carries one detected source trade through the whole pipeline.
    pub async fn on_source_trade(&self, trade: SourceTrade) {
        let st = &self.state;
        st.metrics.source_trades_total.inc();
        st.recent.add_source_trade(trade.clone());
        let _ = st.repos.insert_source_event(&trade).await;
        self.emit(SystemEvent::SourceTradeDetected(Box::new(trade.clone())));

        // Detection latency is measurable only when the venue stamp was precise.
        let mut stamps = LatencyStamps::from_source(trade.source_ts, false, trade.detected_ts);
        st.metrics.latency.record_all(&stamps);

        let Some(wallet) = st.tracker.get_wallet(&trade.trader) else {
            return; // wallet removed between detection and here
        };

        // Fetch the book for this token if we do not already have a fresh one. Without
        // this the copy is priced blind and the paper adapter has nothing to fill against.
        let book = self.book_for(&trade.token_id).await;
        let tick = book.as_ref().map(|b| b.tick_size).unwrap_or(Decimal::new(1, 2));
        let ctx = self.sizing_context(&trade);

        // ---- strategy ----
        let signal: CopySignal = match self.trader.on_source_trade(&trade, &wallet, book.as_ref(), ctx, tick) {
            Ok(s) => s,
            Err(refusal) => {
                let reason = match &refusal {
                    SignalRefusal::StaleBook { .. } => SignalSkipReason::SizedToZero,
                    SignalRefusal::Sizing(_) => SignalSkipReason::SizedToZero,
                };
                debug!(wallet = %trade.trader, ?refusal, "no signal generated");
                st.metrics.source_trades_skipped_total.inc();
                self.emit(SystemEvent::SourceTradeSkipped {
                    event_id: trade.event_id.clone(),
                    correlation_id: trade.correlation_id,
                    reason,
                    at: Utc::now(),
                });
                return;
            }
        };

        stamps.signal = signal.latency.signal;
        st.metrics.copy_signals_total.inc();
        st.recent.add_signal(signal.clone());
        let _ = st.repos.insert_signal(&signal).await;
        self.emit(SystemEvent::CopySignalGenerated(Box::new(signal.clone())));

        // Dashboard row, updated in place as the order progresses.
        st.recent.add_copy(CopyRow {
            correlation_id: signal.correlation_id.to_string(),
            source_event_id: signal.source_event_id.to_string(),
            wallet: signal.target_wallet.to_string(),
            wallet_nickname: wallet.nickname.clone(),
            market_title: trade.market_title.clone(),
            outcome: signal.outcome.clone(),
            side: signal.side.as_str().to_string(),
            source_notional: signal.target_notional.to_string(),
            copy_notional: signal.copy_notional.to_string(),
            source_price: signal.target_price.to_string(),
            copy_price: None,
            slippage_bps: None,
            status: "SIGNAL".into(),
            detection_latency_ms: stamps.detection_us().map(|u| u as f64 / 1000.0),
            execution_latency_ms: None,
            end_to_end_latency_ms: None,
            at: Utc::now(),
        });

        // ---- build the order ----
        let order = OrderRequest {
            order_id: domain::OrderId::new(),
            correlation_id: signal.correlation_id,
            signal_id: Some(signal.signal_id),
            market_id: signal.market_id.clone(),
            token_id: signal.token_id.clone(),
            side: signal.side,
            // Marketable-limit: cross the spread, but never worse than the slippage budget.
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Ioc,
            quantity: signal.copy_quantity,
            limit_price: signal.limit_price,
            reference_price: signal.target_price,
            tick_size: tick,
            created_at: Utc::now(),
        };

        // ---- risk ----
        let verdict = {
            let engine = st.risk.read();
            let snap = st.portfolio.snapshot(st.orders.open_count());
            let rs = risk::RiskSnapshot {
                daily_pnl: snap.daily_pnl,
                gross_exposure: snap.gross_exposure,
                market_exposure: st.portfolio.market_exposure(&order.market_id),
                wallet_exposure: Usd::ZERO,
                token_exposure: st.portfolio.token_exposure(&order.token_id),
                open_orders: snap.open_orders,
                equity: snap.equity,
            };
            let status = risk::SystemStatus {
                market_data_healthy: true,
                source_feed_healthy: true,
                execution_ready: true,
                database_healthy: !st.repos.is_ephemeral(),
            };
            engine.check(&order, &rs, &status, &st.kill_switch, book.as_ref(),
                wallet.enabled, &wallet.nickname, None, None, Utc::now())
        };
        stamps.risk_check = Some(Utc::now());

        let rejection = match verdict {
            risk::RiskVerdict::Approved => None,
            risk::RiskVerdict::Rejected(r) => Some(r),
        };
        if let Some(r) = rejection {
            st.metrics.record_rejection(r.code());
            warn!(wallet = %signal.target_wallet, code = r.code(), "order refused by risk");
            let _ = st.repos.insert_risk_event(
                Some(signal.correlation_id.as_uuid()), Some(signal.signal_id.as_uuid()), &r).await;
            self.emit(SystemEvent::OrderRiskRejected {
                correlation_id: signal.correlation_id,
                signal_id: Some(signal.signal_id),
                rejection: r.clone(),
                at: Utc::now(),
            });
            // Systemic breaches halt everything; ordinary limit hits do not.
            if risk::RiskEngine::should_auto_engage_kill_switch(&r) {
                st.kill_switch.engage(format!("auto: {}", r.code()), "risk-engine");
                st.metrics.kill_switch_activations_total.inc();
                self.emit(SystemEvent::KillSwitchActivated {
                    reason: format!("auto: {}", r.code()), by: "risk-engine".into(), at: Utc::now() });
            }
            let corr = signal.correlation_id.to_string();
            st.recent.update_copy(&corr, |c| c.status = format!("REJECTED:{}", r.code()));
            return;
        }

        self.emit(SystemEvent::OrderRiskApproved {
            order_id: order.order_id,
            correlation_id: signal.correlation_id,
            signal_id: signal.signal_id,
            at: Utc::now(),
        });

        // ---- execution ----
        st.portfolio.attribute_order(order.order_id, signal.target_wallet.clone());
        let Ok(order_id) = st.orders.register_validated(order.clone(), stamps) else {
            warn!("order registration failed");
            return;
        };
        st.metrics.orders_submitted_total.inc();

        let outcome = st.orders.submit(order_id).await;
        let corr = signal.correlation_id.to_string();

        match outcome {
            SubmitOutcome::Acknowledged { fills, .. } => {
                st.metrics.orders_acknowledged_total.inc();

                // The order row must exist before its fills: `fills.order_id` is a
                // foreign key into `orders`. Writing the fill first fails the constraint,
                // and because the result was discarded the failure was invisible — the
                // database ended up with filled orders and zero fills.
                let order_row = st.orders.get(order_id);
                if let Some(o) = &order_row {
                    st.metrics.latency.record_all(&o.latency);
                    if let Err(e) = st.repos.upsert_order(o, st.mode.as_str()).await {
                        warn!(order = %order_id, error = %e, "failed to persist order");
                    }
                    // Persist the per-stage measurements alongside the order, so latency
                    // can be analysed historically and correlated back to a single trade,
                    // not just observed as live percentiles.
                    for sample in o.latency.samples() {
                        let _ = st.repos.insert_latency(
                            Some(signal.correlation_id.as_uuid()),
                            sample.stage.as_str(),
                            sample.micros,
                        ).await;
                    }
                }

                for f in &fills {
                    st.portfolio.apply_fill(f, &signal.outcome);
                    if let Err(e) = st.repos.insert_fill(f).await {
                        warn!(order = %order_id, error = %e, "failed to persist fill");
                    }
                    st.metrics.orders_filled_total.inc();
                }

                if let Some(o) = order_row {
                    let avg = o.avg_fill_price();
                    let slip = avg.map(|p| domain::Bps::slippage(signal.target_price, p, signal.side));
                    st.recent.update_copy(&corr, |c| {
                        c.status = o.state.as_str().to_string();
                        c.copy_price = avg.map(|p| p.to_string());
                        c.slippage_bps = slip.and_then(|s| s.round().try_into().ok());
                        c.execution_latency_ms = o.latency.execution_us().map(|u| u as f64 / 1000.0);
                        c.end_to_end_latency_ms = o.latency.end_to_end_us().map(|u| u as f64 / 1000.0);
                    });
                    info!(order = %order_id, state = %o.state, wallet = %signal.target_wallet, "copy executed");
                }
                if let Some(p) = st.portfolio.position(&signal.token_id) {
                    self.emit(SystemEvent::PositionUpdated { position: Box::new(p), at: Utc::now() });
                }
                let snap = st.portfolio.snapshot(st.orders.open_count());
                let _ = st.repos.insert_pnl_snapshot(&snap).await;
                self.emit(SystemEvent::PnlUpdated { snapshot: Box::new(snap) });
            }
            SubmitOutcome::Rejected { reason, .. } => {
                st.metrics.orders_rejected_total.inc();
                st.recent.update_copy(&corr, |c| c.status = "REJECTED".into());
                debug!(order = %order_id, %reason, "execution rejected");
                if let Some(o) = st.orders.get(order_id) {
                    let _ = st.repos.upsert_order(&o, st.mode.as_str()).await;
                }
            }
            SubmitOutcome::Ambiguous { detail, .. } => {
                // Never treated as "no order": reconciliation must resolve it.
                warn!(order = %order_id, %detail, "AMBIGUOUS submission — reconciliation required");
                st.recent.update_copy(&corr, |c| c.status = "UNKNOWN".into());
                if let Some(o) = st.orders.get(order_id) {
                    let _ = st.repos.upsert_order(&o, st.mode.as_str()).await;
                }
            }
        }
    }
}
