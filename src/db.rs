//! PostgreSQL access: one pool, one fixed `search_path`, one statement timeout.
//!
//! The pool is created without touching the network: a database that is down does not prevent
//! the MCP server from starting and answering `doctor` honestly.

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};

use crate::config::Config;

/// SQLSTATE for a unique violation; used by the M1 write path.
#[allow(dead_code)]
pub const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("pool unavailable: {0}")]
    Pool(String),
    #[error("query failed: {0}")]
    Query(#[from] tokio_postgres::Error),
}

pub struct Db {
    pool: Pool,
}

impl Db {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let mut pg: tokio_postgres::Config = config.dsn.parse()?;
        pg.application_name("phylactery-mcp");
        pg.connect_timeout(std::time::Duration::from_secs(5));
        pg.options(format!(
            "-c statement_timeout={}",
            config.statement_timeout_ms
        ));
        let manager = Manager::from_config(
            pg,
            tokio_postgres::NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(config.pool_max)
            .runtime(Runtime::Tokio1)
            .build()?;
        Ok(Self { pool })
    }

    pub async fn client(&self) -> Result<deadpool_postgres::Object, DbError> {
        self.pool
            .get()
            .await
            .map_err(|error| DbError::Pool(error.to_string()))
    }

    pub fn pool_status(&self) -> String {
        let status = self.pool.status();
        format!(
            "size {} / available {} / waiting {}",
            status.size, status.available, status.waiting
        )
    }
}
