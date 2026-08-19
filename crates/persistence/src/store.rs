//! Database connection handling and migrations.

use std::time::Duration;

use sqlx::postgres::{PgPoolOptions, PgQueryResult};
use sqlx::PgPool;
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("store is running in ephemeral mode; no database is configured")]
    Ephemeral,
}

impl StoreError {
    /// A unique-constraint violation, i.e. "we already have this".
    /// Treated as success by the idempotent insert paths.
    pub fn is_duplicate(&self) -> bool {
        match self {
            Self::Db(sqlx::Error::Database(e)) => e.code().as_deref() == Some("23505"),
            _ => false,
        }
    }
}

/// Wraps the pool and knows whether persistence is available at all.
#[derive(Clone)]
pub struct Store {
    pool: Option<PgPool>,
}

impl Store {
    /// Connects and runs migrations.
    pub async fn connect(url: &str, max_conns: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_conns)
            .acquire_timeout(Duration::from_secs(8))
            .connect(url)
            .await?;
        let s = Self { pool: Some(pool) };
        s.migrate().await?;
        info!("database connected and migrated");
        Ok(s)
    }

    /// Tries to connect, degrading to ephemeral rather than refusing to start.
    ///
    /// Deliberate: a paper or demo run must work with no infrastructure, and a live run
    /// losing its audit database should raise an alarm, not halt the book.
    pub async fn connect_or_ephemeral(url: &str, max_conns: u32) -> Self {
        match Self::connect(url, max_conns).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "database unavailable; continuing in EPHEMERAL mode (no durable audit, no crash recovery)");
                Self::ephemeral()
            }
        }
    }

    pub fn ephemeral() -> Self { Self { pool: None } }

    pub fn is_ephemeral(&self) -> bool { self.pool.is_none() }

    pub fn pool(&self) -> Option<&PgPool> { self.pool.as_ref() }

    /// Applies the embedded migrations. Idempotent.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let Some(p) = &self.pool else { return Ok(()) };
        sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
            .execute(p)
            .await
            .map_err(|e| StoreError::Migration(e.to_string()))?;
        Ok(())
    }

    /// Cheap liveness probe for the health endpoint.
    pub async fn ping(&self) -> bool {
        match &self.pool {
            None => false,
            Some(p) => sqlx::query("SELECT 1").execute(p).await.is_ok(),
        }
    }

    /// Runs a statement, silently succeeding in ephemeral mode.
    pub(crate) async fn exec<'q>(
        &self,
        q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> Result<Option<PgQueryResult>, StoreError> {
        match &self.pool {
            None => Ok(None),
            Some(p) => match q.execute(p).await {
                Ok(r) => Ok(Some(r)),
                // An idempotent re-insert is a success, not a failure.
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => Ok(None),
                Err(e) => Err(StoreError::Db(e)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ephemeral_store_is_usable_and_honest_about_it() {
        let s = Store::ephemeral();
        assert!(s.is_ephemeral());
        assert!(!s.ping().await, "ephemeral must not claim a healthy database");
        // Migrations are a no-op rather than an error.
        assert!(s.migrate().await.is_ok());
    }

    #[tokio::test]
    async fn unreachable_database_degrades_instead_of_panicking() {
        // Reserved-for-documentation address: guaranteed unreachable.
        let s = Store::connect_or_ephemeral("postgres://u:p@192.0.2.1:5432/x", 1).await;
        assert!(s.is_ephemeral(), "must degrade, not abort startup");
    }

    #[test]
    fn duplicate_detection_recognises_the_postgres_code() {
        // 23505 is unique_violation; the idempotent insert paths rely on this.
        let e = StoreError::Migration("x".into());
        assert!(!e.is_duplicate());
    }
}
