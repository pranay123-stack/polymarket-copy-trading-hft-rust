//! Strongly-typed configuration with fail-closed defaults.
//!
//! Three rules shape this module:
//!
//! 1. **Live trading requires two independent switches.** `APP_MODE=live` alone is not
//!    enough; `LIVE_TRADING_ENABLED=true` must also be set. A single forgotten variable
//!    can therefore never arm real execution.
//! 2. **Every limit has a conservative default.** A missing risk variable yields a
//!    *tight* limit, never an unlimited one.
//! 3. **Secrets are never stored in a `Debug`-printable field.** The private key lives
//!    behind [`Secret`], whose `Debug` and `Display` are redacted.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use domain::{Address, AppMode, MarketId, SizingMode, TargetWallet, Usd};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{key} is not valid: {detail}")]
    Invalid { key: &'static str, detail: String },
    #[error("{key} is required in {mode} mode but was not set")]
    Missing { key: &'static str, mode: &'static str },
    #[error(
        "refusing to start LIVE execution: APP_MODE=live requires LIVE_TRADING_ENABLED=true \
         (both must be set independently; this guard exists so a single missing variable \
         cannot arm real trading)"
    )]
    LiveNotArmed,
    #[error("target wallet {0} is invalid: {1}")]
    BadWallet(String, String),
    #[error("risk limit inconsistency: {0}")]
    InconsistentLimits(String),
}

/// A value that must never reach a log line.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
    /// Deliberately verbose — every call site is an auditable point where a secret is used.
    pub fn expose(&self) -> &str { &self.0 }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() { f.write_str("Secret(<unset>)") } else { f.write_str("Secret(<redacted>)") }
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("<redacted>") }
}

/// Where the system gets its data and where orders go.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub gamma_url: String,
    pub clob_url: String,
    pub data_api_url: String,
    /// RTDS activity feed — the wallet-attributed trade source.
    pub rtds_ws_url: String,
    /// CLOB market channel, for order books.
    pub market_ws_url: String,
    pub http_timeout_ms: u64,
    pub ws_connect_timeout_ms: u64,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            gamma_url: "https://gamma-api.polymarket.com".into(),
            clob_url: "https://clob.polymarket.com".into(),
            data_api_url: "https://data-api.polymarket.com".into(),
            rtds_ws_url: "wss://ws-live-data.polymarket.com".into(),
            market_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            http_timeout_ms: 10_000,
            ws_connect_timeout_ms: 10_000,
        }
    }
}

/// Global risk limits. Every one of these is enforced before an order can be submitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_trade_usd: Usd,
    pub min_trade_usd: Usd,
    pub max_position_usd: Usd,
    pub max_market_exposure_usd: Usd,
    pub max_portfolio_exposure_usd: Usd,
    pub max_daily_loss_usd: Usd,
    pub max_open_orders: u32,
    pub max_slippage_bps: u32,
    /// Minimum resting notional within our limit price before we will trade.
    pub min_liquidity_usd: Usd,
    /// Books older than this are refused as a basis for pricing.
    pub max_market_data_age_ms: i64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        // Conservative on purpose: a missing env var must tighten, never loosen.
        Self {
            max_trade_usd: Usd::new(dec!(100)),
            min_trade_usd: Usd::new(dec!(5)),
            max_position_usd: Usd::new(dec!(1000)),
            max_market_exposure_usd: Usd::new(dec!(1000)),
            max_portfolio_exposure_usd: Usd::new(dec!(5000)),
            max_daily_loss_usd: Usd::new(dec!(100)),
            max_open_orders: 20,
            max_slippage_bps: 50,
            min_liquidity_usd: Usd::new(dec!(50)),
            max_market_data_age_ms: 30_000,
        }
    }
}

/// Paper-execution realism knobs. Defaults are pessimistic so paper results are not
/// flattering — a paper fill should be *harder* to get than a live one, not easier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub latency_ms: u64,
    pub latency_jitter_ms: u64,
    pub fee_bps: u32,
    /// Extra adverse slippage applied on top of walking the real book.
    pub slippage_bps: u32,
    pub partial_fill_enabled: bool,
    /// Probability a resting limit order gets filled when the price trades through it.
    pub fill_probability: f64,
    /// Probability the venue rejects an order outright.
    pub reject_probability: f64,
    /// Seed for the simulator RNG — fixing it makes paper runs reproducible.
    pub rng_seed: u64,
    pub starting_cash_usd: Usd,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            latency_ms: 45,
            latency_jitter_ms: 20,
            fee_bps: 0, // observed live fills reported fee_rate_bps "0"; see POLYMARKET_API.md §6
            slippage_bps: 10,
            partial_fill_enabled: true,
            fill_probability: 0.92,
            reject_probability: 0.01,
            rng_seed: 42,
            starting_cash_usd: Usd::new(dec!(10_000)),
        }
    }
}

/// Live-execution credentials and the arming interlock.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveConfig {
    /// Second switch, independent of `APP_MODE`.
    pub trading_enabled: bool,
    /// EIP-712 signing key (L1). Never logged.
    pub private_key: Secret,
    /// Our own proxy wallet, used for reconciliation.
    pub funder_address: Option<Address>,
    /// L2 HMAC credentials, derived from L1.
    pub api_key: Secret,
    pub api_secret: Secret,
    pub api_passphrase: Secret,
    /// Hard cap on live order notional, applied on top of `RiskConfig`.
    pub max_live_order_usd: Usd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub port: u16,
    /// Bearer token for mutating endpoints. Empty disables auth — refused in live mode.
    pub api_token: Secret,
    pub cors_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".into(),
            port: 8080,
            api_token: Secret::default(),
            cors_origins: vec!["http://localhost:5173".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub database_url: Secret,
    pub redis_url: Option<String>,
    pub max_connections: u32,
    /// Run without Postgres — state is in-memory only, and crash recovery is disabled.
    pub ephemeral: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_url: Secret::new("postgres://copytrader:copytrader@localhost:5432/copytrader"),
            redis_url: None,
            max_connections: 10,
            ephemeral: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub mode: AppMode,
    pub endpoints: EndpointConfig,
    pub risk: RiskConfig,
    pub simulation: SimulationConfig,
    pub live: LiveConfig,
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub wallets: Vec<TargetWallet>,
    /// Emit synthetic DEMO activity so the dashboard is populated without credentials.
    pub demo_data: bool,
    /// Replay input file, when `mode == Replay`.
    pub replay_file: Option<String>,
    pub log_format_json: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mode: AppMode::Paper,
            endpoints: EndpointConfig::default(),
            risk: RiskConfig::default(),
            simulation: SimulationConfig::default(),
            live: LiveConfig::default(),
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            wallets: Vec::new(),
            demo_data: false,
            replay_file: None,
            log_format_json: false,
        }
    }
}

fn env_opt(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn parse<T: FromStr>(key: &'static str, raw: &str) -> Result<T, ConfigError>
where
    T::Err: fmt::Display,
{
    raw.parse::<T>().map_err(|e| ConfigError::Invalid { key, detail: e.to_string() })
}

fn env_usd(key: &'static str, default: Usd) -> Result<Usd, ConfigError> {
    match env_opt(key) {
        None => Ok(default),
        Some(v) => {
            let d: Decimal = parse(key, &v)?;
            if d < Decimal::ZERO {
                return Err(ConfigError::Invalid { key, detail: "must not be negative".into() });
            }
            Ok(Usd::new(d))
        }
    }
}

fn env_num<T: FromStr + PartialOrd>(key: &'static str, default: T) -> Result<T, ConfigError>
where
    T::Err: fmt::Display,
{
    match env_opt(key) {
        None => Ok(default),
        Some(v) => parse(key, &v),
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    match env_opt(key) {
        None => default,
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    }
}

impl AppConfig {
    /// Loads `.env` (if present) then the environment, and validates the result.
    pub fn from_env() -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();
        Self::from_env_inner()
    }

    fn from_env_inner() -> Result<Self, ConfigError> {
        let mode = match env_opt("APP_MODE")
            .unwrap_or_else(|| "paper".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "paper" => AppMode::Paper,
            "live" => AppMode::Live,
            "replay" => AppMode::Replay,
            other => {
                return Err(ConfigError::Invalid {
                    key: "APP_MODE",
                    detail: format!("expected paper|live|replay, got {other:?}"),
                })
            }
        };
        let mut cfg = AppConfig { mode, ..Default::default() };

        let e = &mut cfg.endpoints;
        if let Some(v) = env_opt("POLYMARKET_GAMMA_URL") { e.gamma_url = v; }
        if let Some(v) = env_opt("POLYMARKET_CLOB_URL") { e.clob_url = v; }
        if let Some(v) = env_opt("POLYMARKET_DATA_API_URL") { e.data_api_url = v; }
        if let Some(v) = env_opt("POLYMARKET_RTDS_WS_URL") { e.rtds_ws_url = v; }
        if let Some(v) = env_opt("POLYMARKET_MARKET_WS_URL") { e.market_ws_url = v; }
        e.http_timeout_ms = env_num("HTTP_TIMEOUT_MS", e.http_timeout_ms)?;

        cfg.risk = RiskConfig {
            max_trade_usd: env_usd("MAX_TRADE_USD", cfg.risk.max_trade_usd)?,
            min_trade_usd: env_usd("MIN_TRADE_USD", cfg.risk.min_trade_usd)?,
            max_position_usd: env_usd("MAX_POSITION_USD", cfg.risk.max_position_usd)?,
            max_market_exposure_usd: env_usd("MAX_MARKET_EXPOSURE_USD", cfg.risk.max_market_exposure_usd)?,
            max_portfolio_exposure_usd: env_usd("MAX_PORTFOLIO_EXPOSURE_USD", cfg.risk.max_portfolio_exposure_usd)?,
            max_daily_loss_usd: env_usd("MAX_DAILY_LOSS_USD", cfg.risk.max_daily_loss_usd)?,
            max_open_orders: env_num("MAX_OPEN_ORDERS", cfg.risk.max_open_orders)?,
            max_slippage_bps: env_num("MAX_SLIPPAGE_BPS", cfg.risk.max_slippage_bps)?,
            min_liquidity_usd: env_usd("MIN_LIQUIDITY_USD", cfg.risk.min_liquidity_usd)?,
            max_market_data_age_ms: env_num("MAX_MARKET_DATA_AGE_MS", cfg.risk.max_market_data_age_ms)?,
        };

        let s = &mut cfg.simulation;
        s.latency_ms = env_num("SIMULATED_LATENCY_MS", s.latency_ms)?;
        s.latency_jitter_ms = env_num("SIMULATED_LATENCY_JITTER_MS", s.latency_jitter_ms)?;
        s.fee_bps = env_num("SIMULATED_FEE_BPS", s.fee_bps)?;
        s.slippage_bps = env_num("SIMULATED_SLIPPAGE_BPS", s.slippage_bps)?;
        s.partial_fill_enabled = env_bool("PARTIAL_FILL_ENABLED", s.partial_fill_enabled);
        s.fill_probability = env_num("FILL_PROBABILITY", s.fill_probability)?;
        s.reject_probability = env_num("REJECT_PROBABILITY", s.reject_probability)?;
        s.rng_seed = env_num("SIM_RNG_SEED", s.rng_seed)?;
        s.starting_cash_usd = env_usd("PAPER_STARTING_CASH_USD", s.starting_cash_usd)?;
        for (k, v) in [("FILL_PROBABILITY", s.fill_probability), ("REJECT_PROBABILITY", s.reject_probability)] {
            if !(0.0..=1.0).contains(&v) {
                return Err(ConfigError::Invalid { key: k, detail: format!("must be within 0..=1, got {v}") });
            }
        }

        cfg.live = LiveConfig {
            trading_enabled: env_bool("LIVE_TRADING_ENABLED", false),
            private_key: Secret::new(env_opt("POLYMARKET_PRIVATE_KEY").unwrap_or_default()),
            funder_address: match env_opt("POLYMARKET_FUNDER_ADDRESS") {
                Some(a) => Some(Address::new(&a).map_err(|e| ConfigError::Invalid {
                    key: "POLYMARKET_FUNDER_ADDRESS", detail: e.to_string() })?),
                None => None,
            },
            api_key: Secret::new(env_opt("POLYMARKET_API_KEY").unwrap_or_default()),
            api_secret: Secret::new(env_opt("POLYMARKET_API_SECRET").unwrap_or_default()),
            api_passphrase: Secret::new(env_opt("POLYMARKET_API_PASSPHRASE").unwrap_or_default()),
            max_live_order_usd: env_usd("MAX_LIVE_ORDER_USD", Usd::new(dec!(50)))?,
        };

        cfg.server.port = env_num("SERVER_PORT", cfg.server.port)?;
        if let Some(v) = env_opt("SERVER_BIND_ADDR") { cfg.server.bind_addr = v; }
        cfg.server.api_token = Secret::new(env_opt("API_AUTH_TOKEN").unwrap_or_default());
        if let Some(v) = env_opt("CORS_ORIGINS") {
            cfg.server.cors_origins = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }

        if let Some(v) = env_opt("DATABASE_URL") { cfg.storage.database_url = Secret::new(v); }
        cfg.storage.redis_url = env_opt("REDIS_URL");
        cfg.storage.max_connections = env_num("DB_MAX_CONNECTIONS", cfg.storage.max_connections)?;
        cfg.storage.ephemeral = env_bool("EPHEMERAL_STORAGE", false);

        cfg.demo_data = env_bool("DEMO_DATA", false);
        cfg.replay_file = env_opt("REPLAY_FILE");
        cfg.log_format_json = env_bool("LOG_JSON", false);
        cfg.wallets = parse_target_wallets(&env_opt("TARGET_WALLETS").unwrap_or_default())?;

        cfg.validate()?;
        Ok(cfg)
    }

    /// Every cross-field rule the system depends on.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // --- the live interlock ---
        if self.mode.is_live() && !self.live.trading_enabled {
            return Err(ConfigError::LiveNotArmed);
        }
        if self.mode.is_live() {
            if self.live.private_key.is_empty() {
                return Err(ConfigError::Missing { key: "POLYMARKET_PRIVATE_KEY", mode: "LIVE" });
            }
            if self.live.api_key.is_empty() || self.live.api_secret.is_empty() || self.live.api_passphrase.is_empty() {
                return Err(ConfigError::Missing { key: "POLYMARKET_API_KEY/SECRET/PASSPHRASE", mode: "LIVE" });
            }
            if self.live.funder_address.is_none() {
                return Err(ConfigError::Missing { key: "POLYMARKET_FUNDER_ADDRESS", mode: "LIVE" });
            }
            // An unauthenticated kill switch on a live book is not acceptable.
            if self.server.api_token.is_empty() {
                return Err(ConfigError::Missing { key: "API_AUTH_TOKEN", mode: "LIVE" });
            }
            // Demo data must never be mixed into a live book.
            if self.demo_data {
                return Err(ConfigError::InconsistentLimits(
                    "DEMO_DATA=true is refused in LIVE mode: synthetic trades must never enter a real book".into(),
                ));
            }
        }
        if matches!(self.mode, AppMode::Replay) && self.replay_file.is_none() {
            return Err(ConfigError::Missing { key: "REPLAY_FILE", mode: "REPLAY" });
        }

        // --- limit coherence ---
        let r = &self.risk;
        if r.min_trade_usd > r.max_trade_usd {
            return Err(ConfigError::InconsistentLimits(format!(
                "MIN_TRADE_USD ({}) exceeds MAX_TRADE_USD ({})", r.min_trade_usd, r.max_trade_usd)));
        }
        if r.max_trade_usd > r.max_position_usd {
            return Err(ConfigError::InconsistentLimits(format!(
                "MAX_TRADE_USD ({}) exceeds MAX_POSITION_USD ({}): a single order could breach the position cap",
                r.max_trade_usd, r.max_position_usd)));
        }
        if r.max_position_usd > r.max_portfolio_exposure_usd {
            return Err(ConfigError::InconsistentLimits(format!(
                "MAX_POSITION_USD ({}) exceeds MAX_PORTFOLIO_EXPOSURE_USD ({})",
                r.max_position_usd, r.max_portfolio_exposure_usd)));
        }
        if r.max_open_orders == 0 {
            return Err(ConfigError::InconsistentLimits("MAX_OPEN_ORDERS must be at least 1".into()));
        }

        // --- per-wallet limits must sit inside global limits ---
        for w in &self.wallets {
            if w.max_trade_usd > r.max_trade_usd {
                return Err(ConfigError::InconsistentLimits(format!(
                    "wallet {} max_trade {} exceeds global MAX_TRADE_USD {}",
                    w.nickname, w.max_trade_usd, r.max_trade_usd)));
            }
        }
        Ok(())
    }

    /// True when orders may actually reach the venue.
    pub fn live_execution_armed(&self) -> bool {
        self.mode.is_live() && self.live.trading_enabled
    }

    /// A redacted view safe to return from `GET /api/config`.
    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode.as_str(),
            "live_execution_armed": self.live_execution_armed(),
            "demo_data": self.demo_data,
            "endpoints": {
                "rtds_ws_url": self.endpoints.rtds_ws_url,
                "clob_url": self.endpoints.clob_url,
                "data_api_url": self.endpoints.data_api_url,
            },
            "risk": {
                "max_trade_usd": self.risk.max_trade_usd.get(),
                "min_trade_usd": self.risk.min_trade_usd.get(),
                "max_position_usd": self.risk.max_position_usd.get(),
                "max_market_exposure_usd": self.risk.max_market_exposure_usd.get(),
                "max_portfolio_exposure_usd": self.risk.max_portfolio_exposure_usd.get(),
                "max_daily_loss_usd": self.risk.max_daily_loss_usd.get(),
                "max_open_orders": self.risk.max_open_orders,
                "max_slippage_bps": self.risk.max_slippage_bps,
                "min_liquidity_usd": self.risk.min_liquidity_usd.get(),
            },
            "simulation": {
                "latency_ms": self.simulation.latency_ms,
                "latency_jitter_ms": self.simulation.latency_jitter_ms,
                "fee_bps": self.simulation.fee_bps,
                "slippage_bps": self.simulation.slippage_bps,
                "partial_fill_enabled": self.simulation.partial_fill_enabled,
                "fill_probability": self.simulation.fill_probability,
                "reject_probability": self.simulation.reject_probability,
                "starting_cash_usd": self.simulation.starting_cash_usd.get(),
            },
            "wallet_count": self.wallets.len(),
        })
    }
}

/// Parses `TARGET_WALLETS`.
///
/// Format: `addr:nickname:ratio:max_trade:max_exposure` entries separated by `,`.
/// Only the address is mandatory. Example:
/// `0xabc…:Whale:0.25:100:1000,0xdef…:Sharp:0.10:50:500`
pub fn parse_target_wallets(raw: &str) -> Result<Vec<TargetWallet>, ConfigError> {
    let mut out = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = entry.split(':').map(str::trim).collect();
        let addr = Address::new(parts[0])
            .map_err(|e| ConfigError::BadWallet(parts[0].to_string(), e.to_string()))?;
        if seen.insert(addr.to_string(), ()).is_some() {
            return Err(ConfigError::BadWallet(addr.to_string(), "duplicate target wallet".into()));
        }
        let nickname = parts.get(1).filter(|s| !s.is_empty()).map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}…{}", &addr.as_str()[..6], &addr.as_str()[38..]));
        let mut w = TargetWallet::new(addr, nickname);
        if let Some(r) = parts.get(2).filter(|s| !s.is_empty()) {
            let ratio: Decimal = r.parse().map_err(|_| {
                ConfigError::BadWallet(entry.to_string(), format!("copy ratio {r:?} is not a number"))
            })?;
            if ratio <= Decimal::ZERO || ratio > dec!(10) {
                return Err(ConfigError::BadWallet(entry.to_string(),
                    format!("copy ratio {ratio} must be within (0, 10]")));
            }
            w.sizing = SizingMode::FixedRatio { ratio };
        }
        if let Some(v) = parts.get(3).filter(|s| !s.is_empty()) {
            w.max_trade_usd = Usd::new(v.parse().map_err(|_| {
                ConfigError::BadWallet(entry.to_string(), format!("max trade {v:?} is not a number"))
            })?);
        }
        if let Some(v) = parts.get(4).filter(|s| !s.is_empty()) {
            w.max_exposure_usd = Usd::new(v.parse().map_err(|_| {
                ConfigError::BadWallet(entry.to_string(), format!("max exposure {v:?} is not a number"))
            })?);
        }
        out.push(w);
    }
    Ok(out)
}

/// Parses a comma-separated market allow/block list.
pub fn parse_market_list(raw: &str) -> Result<Vec<MarketId>, ConfigError> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty())
        .map(|s| MarketId::new(s).map_err(|e| ConfigError::Invalid {
            key: "MARKET_LIST", detail: e.to_string() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_base() -> AppConfig {
        let mut c = AppConfig { mode: AppMode::Live, ..Default::default() };
        c.live.trading_enabled = true;
        c.live.private_key = Secret::new("0xkey");
        c.live.api_key = Secret::new("k");
        c.live.api_secret = Secret::new("s");
        c.live.api_passphrase = Secret::new("p");
        c.live.funder_address = Some(Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap());
        c.server.api_token = Secret::new("t");
        c
    }

    #[test]
    fn live_mode_alone_does_not_arm_trading() {
        let c = AppConfig { mode: AppMode::Live, ..Default::default() };
        // LIVE_TRADING_ENABLED defaults to false, so this must refuse.
        assert!(matches!(c.validate(), Err(ConfigError::LiveNotArmed)));
        assert!(!c.live_execution_armed());
    }

    #[test]
    fn trading_enabled_alone_does_not_arm_trading() {
        let mut c = AppConfig::default(); // mode = Paper
        c.live.trading_enabled = true;
        assert!(c.validate().is_ok(), "paper mode stays valid");
        assert!(!c.live_execution_armed(), "paper mode must never be armed");
    }

    #[test]
    fn both_switches_arm_trading() {
        let c = live_base();
        c.validate().unwrap();
        assert!(c.live_execution_armed());
    }

    #[test]
    fn live_requires_every_credential() {
        for clear in ["key", "api", "funder", "token"] {
            let mut c = live_base();
            match clear {
                "key" => c.live.private_key = Secret::default(),
                "api" => c.live.api_secret = Secret::default(),
                "funder" => c.live.funder_address = None,
                _ => c.server.api_token = Secret::default(),
            }
            assert!(c.validate().is_err(), "live must refuse to start without {clear}");
        }
    }

    #[test]
    fn demo_data_is_refused_in_live() {
        let mut c = live_base();
        c.demo_data = true;
        assert!(matches!(c.validate(), Err(ConfigError::InconsistentLimits(_))));
    }

    #[test]
    fn incoherent_limits_are_rejected() {
        let mut c = AppConfig::default();
        c.risk.max_trade_usd = Usd::new(dec!(2000)); // > max_position 1000
        assert!(c.validate().is_err());

        let mut c = AppConfig::default();
        c.risk.min_trade_usd = Usd::new(dec!(500)); // > max_trade 100
        assert!(c.validate().is_err());

        let mut c = AppConfig::default();
        c.risk.max_open_orders = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn defaults_are_conservative_not_unlimited() {
        let r = RiskConfig::default();
        assert!(r.max_trade_usd.get() <= dec!(100));
        assert!(r.max_daily_loss_usd.get() <= dec!(100));
        assert!(r.max_open_orders <= 20);
        assert!(r.max_slippage_bps <= 50);
    }

    #[test]
    fn secrets_never_render_their_value() {
        let s = Secret::new("0xdeadbeefprivatekey");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        assert!(!format!("{s:?}{s}").contains("deadbeef"));
    }

    #[test]
    fn public_config_view_leaks_no_secrets() {
        let c = live_base();
        let v = serde_json::to_string(&c.public_view()).unwrap();
        for leak in ["0xkey", "\"k\"", "\"s\"", "\"p\"", "\"t\"", "postgres://"] {
            assert!(!v.contains(leak), "public view leaked {leak}: {v}");
        }
    }

    #[test]
    fn wallet_spec_parses_all_fields() {
        let ws = parse_target_wallets(
            "0x8a5152d056adb066c9e4dc65164620cdd82ceb6f:Whale:0.25:100:1000").unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].nickname, "Whale");
        assert_eq!(ws[0].sizing, SizingMode::FixedRatio { ratio: dec!(0.25) });
        assert_eq!(ws[0].max_trade_usd.get(), dec!(100));
        assert_eq!(ws[0].max_exposure_usd.get(), dec!(1000));
    }

    #[test]
    fn wallet_spec_accepts_address_only() {
        let ws = parse_target_wallets("0x8a5152d056aDB066C9E4Dc65164620cDD82CeB6f").unwrap();
        assert_eq!(ws.len(), 1);
        assert!(ws[0].nickname.starts_with("0x8a51"));
        // mixed-case input must normalise
        assert_eq!(ws[0].address.as_str(), "0x8a5152d056adb066c9e4dc65164620cdd82ceb6f");
    }

    #[test]
    fn duplicate_wallets_are_rejected() {
        // Two entries for one trader would double every copy.
        let e = parse_target_wallets(
            "0x8a5152d056adb066c9e4dc65164620cdd82ceb6f,0x8A5152D056ADB066C9E4DC65164620CDD82CEB6F");
        assert!(matches!(e, Err(ConfigError::BadWallet(_, _))));
    }

    #[test]
    fn absurd_copy_ratio_is_rejected() {
        assert!(parse_target_wallets("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f:X:50").is_err());
        assert!(parse_target_wallets("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f:X:0").is_err());
        assert!(parse_target_wallets("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f:X:abc").is_err());
    }

    #[test]
    fn bad_address_is_rejected_loudly() {
        assert!(parse_target_wallets("not-an-address").is_err());
        assert!(parse_target_wallets("0x123").is_err());
    }

    #[test]
    fn empty_wallet_list_is_valid() {
        assert!(parse_target_wallets("").unwrap().is_empty());
        assert!(parse_target_wallets("  , ,  ").unwrap().is_empty());
    }

    #[test]
    fn replay_requires_a_file() {
        let c = AppConfig { mode: AppMode::Replay, ..Default::default() };
        assert!(matches!(c.validate(), Err(ConfigError::Missing { key: "REPLAY_FILE", .. })));
    }

    #[test]
    fn per_wallet_limits_cannot_exceed_global() {
        let mut c = AppConfig::default();
        let mut w = TargetWallet::new(
            Address::new("0x8a5152d056adb066c9e4dc65164620cdd82ceb6f").unwrap(), "W");
        w.max_trade_usd = Usd::new(dec!(999)); // global default is 100
        c.wallets = vec![w];
        assert!(c.validate().is_err());
    }
}
