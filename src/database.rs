
use std::sync::Arc;
use std::env;

use tokio_postgres::NoTls;
use bb8_postgres::PostgresConnectionManager;
use bb8_postgres::bb8::Pool;

use base64::engine::general_purpose;

use aws_sdk_s3::Client as S3Client;
use crate::rate_limiter::RateLimiter;

pub struct AppState {
    pub db_pool: Arc<Pool<PostgresConnectionManager<NoTls>>>,
    pub base64_engine: Arc<general_purpose::GeneralPurpose>,
    pub s3_client: S3Client,
    pub rate_limiter: RateLimiter,
}

pub type SharedConn = bb8_postgres::bb8::PooledConnection<'static, PostgresConnectionManager<tokio_postgres::NoTls>>;

pub async fn establish_pool_connection() -> Result<Pool<PostgresConnectionManager<NoTls>>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    let conn_manager = PostgresConnectionManager::new_from_stringlike(
        env::var("DATABASE_URL").expect("Expected DATABASE_URL to exist in the environment"),
        NoTls,
    ).unwrap();

    let pool = Pool::builder()
                .min_idle(Some(1))
                .max_size(15)
                .build(conn_manager).await?;
    Ok(pool)
}