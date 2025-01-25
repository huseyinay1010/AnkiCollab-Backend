
use std::sync::Arc;
use tokio_postgres::NoTls;
use bb8_postgres::PostgresConnectionManager;
use bb8_postgres::bb8::Pool;

use base64::engine::general_purpose;

pub struct AppState {
    pub db_pool: Arc<Pool<PostgresConnectionManager<NoTls>>>,
    pub base64_engine: Arc<general_purpose::GeneralPurpose>,
}

pub(crate) type SharedConn = bb8_postgres::bb8::PooledConnection<'static, PostgresConnectionManager<tokio_postgres::NoTls>>;

pub async fn establish_pool_connection() -> Result<Pool<PostgresConnectionManager<NoTls>>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let conn_manager = PostgresConnectionManager::new_from_stringlike(
        "postgresql://postgres:password@localhost/anki",
        NoTls,
    ).unwrap();

    let pool = Pool::builder()
                .connection_timeout(core::time::Duration::new(60, 0))
                .min_idle(Some(1))
                .max_size(15)
                .max_lifetime(Some(core::time::Duration::new(10, 0)))
                .idle_timeout(Some(core::time::Duration::new(5, 0)))
                .build(conn_manager).await?;
    Ok(pool)
}