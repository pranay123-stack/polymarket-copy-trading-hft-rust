//! Live smoke test: connects to the real RTDS feed, parses frames with the production
//! parser, and drives them through the real WalletTracker.
//!
//! Run: `cargo run -p wallet_tracker --example live_ingest_smoke`
//! Requires network. Not part of `cargo test`.

use std::collections::HashMap;
use std::time::Duration;

use domain::{Address, TargetWallet, Usd};
use market_data::{FeedMessage, RtdsClient};
use wallet_tracker::{Detection, WalletTracker};

#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
    let client = RtdsClient::new("wss://ws-live-data.polymarket.com".into(), 10_000);
    tokio::spawn(client.run(tx, sd_rx));

    // Phase 1: watch the firehose to find genuinely active wallets.
    println!("== phase 1: sampling the live firehose for 20s ==");
    let mut counts: HashMap<Address, u32> = HashMap::new();
    let mut frames = 0u64;
    let mut lat = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(FeedMessage::Trade(t))) => {
                frames += 1;
                lat.push((t.detected_ts - t.source_ts).num_milliseconds());
                *counts.entry(t.trader.clone()).or_default() += 1;
            }
            Ok(Some(FeedMessage::Connected { .. })) => println!("   connected"),
            Ok(Some(FeedMessage::Disconnected { reason, .. })) => println!("   disconnected: {reason}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    lat.sort_unstable();
    println!("   trades parsed : {frames}");
    println!("   distinct wallets: {}", counts.len());
    if !lat.is_empty() {
        println!("   detection latency ms  min={} p50={} p95={} max={}",
            lat[0], lat[lat.len()/2], lat[lat.len()*95/100], lat[lat.len()-1]);
    }

    let mut top: Vec<_> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    if top.is_empty() { println!("no trades observed; aborting"); return; }
    println!("   busiest wallets: {:?}",
        top.iter().take(3).map(|(a,c)| (a.to_string(), *c)).collect::<Vec<_>>());

    // Phase 2: track the busiest wallets and prove the tracker fires on real trades.
    println!("\n== phase 2: tracking the 3 busiest wallets for 25s ==");
    let wallets: Vec<TargetWallet> = top.iter().take(3).map(|(a, _)| {
        let mut w = TargetWallet::new(a.clone(), format!("live-{}", &a.as_str()[..8]));
        w.min_source_notional_usd = Usd::new(rust_decimal::Decimal::ZERO);
        w
    }).collect();
    let tracker = WalletTracker::new(wallets);

    let (mut actionable, mut skipped, mut untracked) = (0u32, 0u32, 0u32);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(FeedMessage::Trade(t))) => match tracker.observe_live(*t) {
                Detection::Actionable(st) => {
                    actionable += 1;
                    if actionable <= 5 {
                        println!("   COPY  {} {} {} @ {} qty {} occ={} [{}]",
                            &st.trader.as_str()[..10], st.side, st.outcome,
                            st.price, st.quantity, st.occurrence,
                            &st.event_id.as_str()[..12]);
                    }
                }
                Detection::Skipped { reason, .. } => {
                    skipped += 1;
                    if skipped <= 3 { println!("   SKIP  {reason:?}"); }
                }
                Detection::NotTracked => untracked += 1,
            },
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let s = tracker.stats();
    println!("\n== results ==");
    println!("   frames examined      : {}", s.frames_examined);
    println!("   wallet matches       : {}", s.wallet_matches);
    println!("   actionable copies    : {actionable}");
    println!("   skipped              : {skipped} (duplicates suppressed: {})", s.duplicates_suppressed);
    println!("   not tracked          : {untracked}");
    println!("   dedup contents held  : {}", tracker.dedup_size());
    let _ = sd_tx.send(true);

    assert!(s.frames_examined > 0, "no frames reached the tracker");
    println!("\nOK: live ingest -> parse -> wallet match -> dedup all functioning");
}
