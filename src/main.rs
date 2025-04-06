
pub mod database;
pub mod structs;
pub mod human_hash;
pub mod notetypes;
pub mod media;
pub mod pull;
pub mod push;
pub mod subscription;
pub mod suggestion;
pub mod changelog;
pub mod note_removal;
pub mod stats;
pub mod auth;
pub mod cleanser;
pub mod media_manager;
pub mod rate_limiter;
pub mod media_reference_manager;
pub mod media_logger;

use std::{collections::HashMap, env, io::Read, time::Duration};

use database::AppState;

use rate_limiter::RateLimiter;
use tokio::signal;

use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, Router},
    Json,
};
use axum_client_ip::{InsecureClientIp, SecureClientIpSource};

use std::net::SocketAddr;
use std::fs;
use std::sync::Arc;

use flate2::Compression;
use flate2::write::GzEncoder;
use flate2::read::GzDecoder;
use std::io::Write;
use base64::{Engine as _, engine::general_purpose};

use tower_governor::{governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor, GovernorLayer};

use aws_sdk_s3::Client as S3Client;

use axum_macros::debug_handler;

fn read_cached_json(file_name: &str) -> Option<String> {
    let path = format!("/home/cached_files/{}", file_name);
    match fs::read_to_string(path) {
        Ok(data) => Some(data),
        Err(_) => None
    }
}

fn decompress_data(engine: &general_purpose::GeneralPurpose, data: &str) -> String {
    let compressed_data = engine.decode(data).unwrap();
    let mut decoder = GzDecoder::new(&compressed_data[..]);
    let mut decoded_data = String::new();
    decoder.read_to_string(&mut decoded_data).unwrap();
    decoded_data
}

async fn post_login(
    State(db_state): State<Arc<AppState>>,
    form: axum::Form<auth::Login>
) -> impl IntoResponse {
    match auth::login(&db_state, &form).await {
        Ok(res) => {
            let json = serde_json::to_string(&res).unwrap();
            (StatusCode::OK, json)
        },
        Err(error) => {
            println!("Error occurred: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        },
    }
}

pub async fn remove_token(
    State(db_state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    match auth::remove_token(&db_state, &token).await {
        Ok(res) => (StatusCode::OK, res.to_string()),
        Err(error) => {
            println!("Error deleting token: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error".to_string())
        }
    }
}

pub async fn upload_deck_stats(
    State(state): State<Arc<AppState>>,
    deck: String
) -> impl IntoResponse {
    let decompressed_data = decompress_data(&state.base64_engine, &deck);
    let info: structs::StatsInfo = match serde_json::from_str(&decompressed_data) {
        Ok(data) => data,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid data".to_string()),
    };

    let db_state_clone = Arc::clone(&state);

    tokio::spawn(async move {
        match stats::new(&db_state_clone, info).await {
            Ok(_) => {},
            Err(error) => { println!("Error inserting deck stats: {}", error); },
        }
    });

    (StatusCode::OK, "Thanks for sharing!".to_string())
}

pub async fn confirm_media_bulk_async(
    State(state): State<Arc<AppState>>,
    Json(req): Json<structs::MediaBulkConfirmRequest>,
) -> impl IntoResponse {
    // we handle the confirmation in a seperate thread, so the user doesn't have to wait for it to finish, lacking information on which files failed to upload, trading off speed for user experience
    let state_copy = state.clone();
    tokio::spawn(async move {
        match media_manager::confirm_media_bulk_upload(state_copy, req).await {
            Ok(_) => {},
            Err(error) => { println!("Error confirming media bulk upload: {:?}", error); },
        }
    });
    (StatusCode::OK, "Thanks for sharing!".to_string())
}

pub async fn check_for_update(
    InsecureClientIp(_iip): InsecureClientIp,
    State(state): State<Arc<AppState>>,
    Json(input): Json<HashMap<String, structs::UpdateInfo>>,
) -> impl IntoResponse {
    let mut responses = Vec::with_capacity(input.iter().len());

    for (deck_hash, update_info) in input.iter() {
        if update_info.timestamp == "2022-12-31 23:59:59" {
            // Check if the result is already cached
            let file_name = format!("{}.json", deck_hash);
            let json_data = read_cached_json(&file_name);

            if let Some(data) = json_data {
                let mut decompressed_data = decompress_data(&state.base64_engine, &data);

                // Hacky, but its stored as a map because that's what pullChanges returned, but the map only contains 1 item and we are looking for that one item here, so we remove the unnecessary brackets
                decompressed_data.pop(); // remove last bracket
                decompressed_data.remove(0); // remove first bracket
                // Now it's deserializable
                let json = serde_json::from_str(&decompressed_data).expect("JSON was not well-formatted");
                responses.push(json);
                continue;
            }
        }

        match pull::pull_changes(&state, deck_hash, &update_info.timestamp).await {
            Ok(val) => responses.push(val),
            Err(_error) => (),
        };
    }

    let json_bytes = serde_json::to_vec(&responses).unwrap();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json_bytes).unwrap();
    let compressed_bytes = encoder.finish().unwrap();
    
    let encoded = state.base64_engine.encode(compressed_bytes);

    (StatusCode::OK, encoded)
}

pub async fn post_data(
    InsecureClientIp(iip): InsecureClientIp,
    State(state): State<Arc<AppState>>,
    deck: String,
) -> impl IntoResponse {
    let decompressed_data = decompress_data(&state.base64_engine, &deck);
    let info: structs::CreateDeckReq = match serde_json::from_str(&decompressed_data) {
        Ok(data) => data,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid data".to_string()),
    };

    let owner_id = (pull::get_id_from_username(&state, info.username).await).unwrap_or_default();

    if owner_id == 0 {
        return (StatusCode::OK, r#"{ "status": 0, "message": "Unknown username" }"#.to_string());
    }

    let anki_deck = structs::AnkiDeck::from_json_string(&info.deck).unwrap();
    let ip = iip.to_string();

    let commit_text = String::new();
    let commit_id = match suggestion::create_new_commit(&state, 1, &commit_text, &ip, Some(owner_id)).await {
        Ok(val) => val,
        Err(error) => {
            println!("Error: {}", error);
            return (StatusCode::OK, r#"{ "status": 0, "message": "An error occurred processing your request" }"#.to_string());
        }
    };

    let mut client: database::SharedConn = match state.db_pool.get_owned().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return (StatusCode::OK, r#"{ "status": 0, "message": "Server Error. Please notify us! (752)" }"#.to_string());
        },
    };

    let deck_status = match push::check_deck_exists(&client, &anki_deck.name, &anki_deck.crowdanki_uuid, owner_id, None).await {
        Ok(res) => format!(r#"{{ "status": 1, "message": "{}" }}"#, res),
        Err(error) => r#"{ "status": 0, "message": "Deck does not exist" }"#.to_string(),
    };

    
    tokio::spawn(async move {
        let deck_tree: Vec<i64> = Vec::new(); // The tree is yet to be created. This is a new deck
        let mut notetype_cache = HashMap::new();
        match push::unpack_deck_json(&mut client, &anki_deck, &mut notetype_cache, owner_id, &ip, None, true, commit_id, &deck_tree).await {
            Ok(res) => {
                println!("Success: {}", res);
                format!(r#"{{ "status": 1, "message": "{}" }}"#, res)
            }
            Err(error) => {
                println!("Error: {}", error);
                r#"{ "status": 0, "message": "An error occurred processing your request" }"#.to_string()
            }
        }
    });

    (StatusCode::OK, deck_status)
}

pub async fn request_removal(
    InsecureClientIp(iip): InsecureClientIp,
    State(db_state): State<Arc<AppState>>,
    Json(form): Json<structs::NoteRemovalReq>,
) -> impl IntoResponse {
    let info = form;

    let ip = iip.to_string();

    let access_token = (auth::is_valid_user_token(&db_state, &info.token, &info.remote_deck).await).unwrap_or_default();

    let mut force_overwrite = false;
    if access_token {
        force_overwrite = info.force_overwrite;
    }

    let committing_user:Option<i32> = match auth::get_user_from_token(&db_state, &info.token).await {
        Ok(user) => Some(user),
        Err(_error) => None,
    };

    match note_removal::new(&db_state, info.note_guids, info.commit_text, info.remote_deck, ip, force_overwrite, committing_user).await {
        Ok(_res) => (StatusCode::OK, "Success".to_string()),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, "Could not create removal request.".to_string()),
    }
}

#[debug_handler]
pub async fn process_card(
    InsecureClientIp(iip): InsecureClientIp,    
    State(state): State<Arc<AppState>>,
    deck: String,
) -> impl IntoResponse {
    let decompressed_data = decompress_data(&state.base64_engine, &deck);
    let info: structs::SubmitCardReq = match serde_json::from_str(&decompressed_data) {
        Ok(data) => data,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid data".to_string()),
    };

    let mut anki_deck = match structs::AnkiDeck::from_json_string(&info.deck) {
        Ok(deck) => deck,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid deck data".to_string()),
    };

    let ip = iip.to_string();
    
    let committing_user:Option<i32> = match auth::get_user_from_token(&state, &info.token).await {
        Ok(user) => Some(user),
        Err(_error) => None,
    };
    
    let commit_id = match suggestion::create_new_commit(&state, info.rationale, &info.commit_text, &ip, committing_user).await {
        Ok(val) => val,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Error occurred. Damn!".to_string()),
    };

    // Check if they are authorized to decide whether they want to force overwrite or not
    let access_token = (auth::is_valid_user_token(&state, &info.token, &info.remote_deck).await).unwrap_or_default();

    let mut force_overwrite = false;
    if access_token {
        force_overwrite = info.force_overwrite;
    }

    let deck_path = match suggestion::fix_deck_name(&state, &info.deck_path, &info.new_name, &info.remote_deck).await {
        Ok(val) => val,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, "Invalid Deck Name".to_string()),
    };

    for deck in &mut anki_deck.children {
        suggestion::update_deck_names(deck).await;
    }

    let mut client: database::SharedConn = match state.db_pool.get_owned().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, r#"{ "status": 0, "message": "Server Error. Please notify us! (752)" }"#.to_string());
        },
    };

    let mut notetype_cache = HashMap::new();

    match suggestion::sanity_check_notetypes(&client, &mut notetype_cache, & info.remote_deck, &anki_deck).await {
        Ok(_res) => (),
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Notetype Error: {}", error)),
    };

    let (deck_id, owner) = match suggestion::sanity_check(&client, &info.remote_deck, &deck_path, commit_id).await {
        Ok((deck_id, owner)) => (deck_id, owner),
        Err(error) => {
            println!("Error getting pool: {}", error);
            return (StatusCode::INTERNAL_SERVER_ERROR, r#"{ "status": 0, "message": "Error submitting your suggestion" }"#.to_string());
        },
    };
    let deck_tree = {
        client.query(
            "
            WITH RECURSIVE up_tree AS (
                SELECT id, parent
                FROM decks
                WHERE id = $1
                UNION ALL
                SELECT d.id, d.parent
                FROM decks d
                JOIN up_tree ut ON d.id = ut.parent
            ),
            down_tree AS (
                SELECT id, parent
                FROM up_tree
                WHERE parent IS NULL
                UNION ALL
                SELECT d.id, d.parent
                FROM decks d
                JOIN down_tree dt ON d.parent = dt.id
            )
            SELECT DISTINCT id
            FROM down_tree
            ",
            &[&deck_id.unwrap()]
        ).await.unwrap().iter().map(|row| row.get::<_, i64>(0)).collect::<Vec<i64>>()
    };
    match suggestion::make(&mut client, &info.remote_deck, &mut notetype_cache, &deck_path, &anki_deck, &ip, commit_id, force_overwrite, deck_id, owner, &deck_tree).await {
        Ok(_res) => (StatusCode::OK, "Success".to_string()),
        Err(error) => { 
            println!("Error occurred in make suggestion: {}", error); 
            (StatusCode::INTERNAL_SERVER_ERROR, r#"{ "status": 0, "message": "Error submitting your suggestion" }"#.to_string())
        },
    }
}

pub async fn check_deck_alive(
    State(db_state): State<Arc<AppState>>,
    Json(request): Json<structs::CheckDeckAliveRequest>,
) -> impl IntoResponse {
    let deck_hashes = request.deck_hashes;

    match pull::check_deck_alive(&db_state, deck_hashes).await {
        Ok(res) => {
            let json = serde_json::to_string(&res).unwrap();
            (StatusCode::OK, json)
        },
        Err(error) => {
            println!("Error occurred: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error".to_string())
        },
    }
}

pub async fn add_subscription(
    State(db_state): State<Arc<AppState>>,
    Json(request): Json<structs::SubscriptionRequest>,
) -> impl IntoResponse {
    let deck_hash = request.deck_hash;
    let user_hash = request.user_hash;
    match subscription::add(&db_state, deck_hash, user_hash).await {
        Ok(res) => (StatusCode::OK, res),
        Err(_error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, ())
        },
    }
}
pub async fn remove_subscription(
    State(db_state): State<Arc<AppState>>,
    Json(request): Json<structs::SubscriptionRequest>,
) -> impl IntoResponse {
    let deck_hash = request.deck_hash;
    let user_hash = request.user_hash;
    match subscription::remove(&db_state, deck_hash, user_hash).await {
        Ok(res) => (StatusCode::OK, res),
        Err(_error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, ())
        },
    }
}

pub async fn get_deck_timestamp(
    State(db_state): State<Arc<AppState>>,
    Path(deck_hash): Path<String>,
) -> impl IntoResponse {
    match pull::get_deck_last_update_unix(&db_state, &deck_hash).await {
        Ok(res) => (StatusCode::OK, format!("{}", res)),
        Err(error) => {
            println!("Error retrieving deck timestamp: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, "0.0".to_string())
        },
    }
}

pub async fn get_large_decks(State(db_state): State<Arc<AppState>>) -> impl IntoResponse {

    let client: database::SharedConn = match db_state.db_pool.get_owned().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "[]".to_string());
        },
    };

    notetypes::delete_unused_notetypes(&client).await.unwrap(); // Run it every 24h to keep the notetypes clean since they dont get auto removed

    // Run it every 24h to keep the /decks page on the website up2date
    client.execute("REFRESH MATERIALIZED VIEW deck_stats", &[]).await.expect("Error executing large decks statement 2");

    let rows = client
        .query("SELECT id, human_hash FROM decks WHERE parent IS NULL", &[])
        .await.expect("Error executing large decks statement");

    let mut large_decks: Vec<String> = Vec::new();

    let query = client.prepare("
        WITH RECURSIVE cte AS (
            SELECT $1::bigint as id
            UNION ALL
            SELECT d.id
            FROM cte JOIN decks d ON d.parent = cte.id
        )
        SELECT COUNT(*) as num FROM notes WHERE deck IN (SELECT id FROM cte)
    ").await.unwrap();
    
    for row in rows {
        let deck_id: i64 = row.get(0);
        let xx = client.query(&query, &[&deck_id]).await.unwrap();
        let count: i64 = xx[0].get(0);
        if count > 5000 {
            large_decks.push(row.get(1));
        }
    }
    let json = serde_json::to_string(&large_decks).unwrap();
    (StatusCode::OK, json)
}

pub async fn submit_changelog(
    State(db_state): State<Arc<AppState>>,
    Json(changelog_data): Json<structs::SubmitChangelog>,
) -> impl IntoResponse {
    // check if they are authorized to add a changelog message to this deck
    let access_token = (auth::is_valid_user_token(&db_state, &changelog_data.token, &changelog_data.deck_hash).await).unwrap_or_default();
    if !access_token {
        return (StatusCode::UNAUTHORIZED, "You are not authorized to add a changelog message to this deck".to_string());
    }

    match changelog::insert_new_changelog(&db_state, &changelog_data.deck_hash, &changelog_data.changelog).await {
        Ok(_res) => (StatusCode::OK, "Changelog published successfully!".to_string()),
        Err(_error) => (StatusCode::INTERNAL_SERVER_ERROR, "An error occurred while publishing the changelog.".to_string()),
    }
}

pub async fn check_user_token(
    State(db_state): State<Arc<AppState>>,
    Json(info): Json<structs::TokenInfo>
) -> impl IntoResponse {
    let quer = auth::get_user_from_token(&db_state, &info.token).await.unwrap_or_default();
    let res = quer > 0;
    (StatusCode::OK, serde_json::to_string(&res).unwrap())
}

pub async fn refresh_auth_token(
    State(db_state): State<Arc<AppState>>,
    Json(refresh_req): Json<auth::TokenRefresh>,
) -> impl IntoResponse {
    match auth::refresh_token(&db_state, &refresh_req).await {
        Ok(res) => {
            let json = serde_json::to_string(&res).unwrap();
            (StatusCode::OK, json)
        },
        Err(error) => {
            println!("Error occurred: {}", error);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error".to_string())
        },
    }
}

async fn get_protected_fields_from_deck(
    State(db_state): State<Arc<AppState>>,
    Path(deck_hash): Path<String>,
) -> impl IntoResponse {
    let client = db_state.db_pool.get_owned().await.map_err(|e| {
        println!("Error getting pool: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Error".to_string())
    }).unwrap();
    let quer = notetypes::pull_protected_fields(&client, &deck_hash).await.unwrap_or_default();
    (StatusCode::OK, serde_json::to_string(&quer).unwrap())
}

fn media_routes() -> Router<Arc<AppState>> {
    let media_api_ratelimit:u32 = std::env::var("MEDIA_API_RATE_LIMIT_PER_MINUTE").expect("MEDIA_API_RATE_LIMIT_PER_MINUTE must be set").parse().expect("Rate limit must be a valid number");
    let media_governor_conf = Arc::new(GovernorConfigBuilder::default()
        .per_second((media_api_ratelimit as f64 / 60.0) as u64)
        .burst_size(media_api_ratelimit / 2)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .unwrap());

    let governor_limiter = media_governor_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    // a separate background task to clean up
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        governor_limiter.retain_recent();
    });

    Router::new()
        .route("/check/bulk", post(media_manager::check_media_bulk))
        .route("/confirm/bulk", post(confirm_media_bulk_async))
        .route("/manifest", post(media_manager::get_media_manifest))
        .route("/sanitize/svg", post(media_manager::sanitize_svg_batch))        
        // strict rate limiting
        .layer(GovernorLayer {
            config: media_governor_conf,
        })
}

async fn get_bucket_size(s3_client: &S3Client, bucket: &str) -> i64 {
    let mut pages = s3_client
        .list_objects_v2()
        .bucket(bucket)
        .into_paginator()
        .send();
    let mut total_bytes: i64 = 0;

    while let Some(page) = pages.next().await {
        if page.is_err() {
            println!("Error listing objects: {:?}", page.err().unwrap());
            return 0;
        }
        total_bytes += page.unwrap()
            .contents()            
            .iter()
            .map(|obj| obj.size.unwrap_or_default())
            .sum::<i64>();
    }
    println!("Total bucket size in GB: {} GB", total_bytes / 1024 / 1024 / 1024);

    total_bytes
}


#[tokio::main]
async fn main() {
    dotenvy::dotenv().expect(
        "Expected .env file in the root directory containing the database connection string",
    );
    // Sentry setup
    let _guard = sentry::init((env::var("SENTRY_URL").expect("SENTRY_URL must be set"), sentry::ClientOptions {
        release: sentry::release_name!(),
        traces_sample_rate: 0.2,
        ..Default::default()
      }));

    let s3_access_key_id = std::env::var("S3_ACCESS_KEY_ID").expect("S3_ACCESS_KEY_ID must be set");
    let s3_secret_access_key = std::env::var("S3_SECRET_ACCESS_KEY").expect("S3_SECRET_ACCESS_KEY must be set");
    let s3_domain = std::env::var("S3_DOMAIN").expect("S3_DOMAIN must be set");
    let bucket_name = std::env::var("S3_BUCKET_NAME").expect("S3_BUCKET_NAME must be set");

    let credentials = aws_sdk_s3::config::Credentials::new(
        s3_access_key_id,
        s3_secret_access_key,
        None, None, "s3-credentials");
    
    let region_provider = aws_config::meta::region::RegionProviderChain::default_provider().or_else("eu-central-1"); // Europe (Frankfurt)
    let s3_config = aws_config::from_env()
        .region(region_provider)
        .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(credentials))
        .endpoint_url(&s3_domain)
        .load()
        .await;
    
    let s3_service_config = aws_sdk_s3::config::Builder::from(&s3_config)
    .force_path_style(true) // Contabo is <special>
    .build();
    
    let s3_client = S3Client::from_conf(s3_service_config);

    let pool = database::establish_pool_connection().await.expect("Failed to establish database connection pool");
    
    // Initialize rate limiter
    // Get current bucket size of bucket bucket_name
    let bucket_size = get_bucket_size(&s3_client, &bucket_name).await;
    
    let rate_limiter = RateLimiter::new(Arc::new(pool.clone()), bucket_size as u64);
    rate_limiter.load_user_quotas().await.expect("Failed to load user quotas");

    let state = Arc::new(database::AppState {
        db_pool: Arc::new(pool),
        base64_engine: Arc::new(general_purpose::STANDARD),
        s3_client,
        rate_limiter,
    });

    // IP RAte limiter 
    let global_api_ratelimit:u32 = std::env::var("STANDARD_API_RATE_LIMIT_PER_MINUTE").expect("STANDARD_API_RATE_LIMIT_PER_MINUTE must be set").parse().expect("Rate limit must be a valid number");
    let global_governor_conf = Arc::new(GovernorConfigBuilder::default()
            .per_second((global_api_ratelimit as f64 / 60.0) as u64)
            .burst_size(global_api_ratelimit / 2)
            .key_extractor(SmartIpKeyExtractor)
            .finish()
            .unwrap());

    let governor_limiter = global_governor_conf.limiter().clone();
    let interval = Duration::from_secs(60);
    // a separate background task to clean up
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        governor_limiter.retain_recent();
    });
    
    // start media cleanup task
    media_manager::start_cleanup_task(state.clone()).await;
   
   // Schedule periodic cleanup of expired tokens
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(86400)); // Daily
        loop {
            interval.tick().await;
            match auth::cleanup_expired_tokens(&cleanup_state).await {
                Ok(count) => {
                    if count > 0 {
                        println!("Cleaned up {} expired tokens", count);
                    }
                },
                Err(e) => println!("Error cleaning up tokens: {}", e),
            }
        }
    });

    // Enable tracing.
    let env_filter = if cfg!(debug_assertions) {
        // Debug build
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            format!(
                "{}=debug,tower_http=debug,axum=trace",
                env!("CARGO_CRATE_NAME")
            )
            .into()
        })
    } else {
        // Release build
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            format!(
                "{}=info,tower_http=info,axum=info",
                env!("CARGO_CRATE_NAME")
            )
            .into()
        })
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().without_time())
        .init();
    
    // Build our application with routes
    let app = Router::new()
        .route("/pullChanges", post(check_for_update))
        .route("/createDeck", post(post_data))
        .route("/submitCard", post(process_card))
        .route("/CheckDeckAlive", post(check_deck_alive))
        .route("/AddSubscription", post(add_subscription))
        .route("/RemoveSubscription", post(remove_subscription))
        .route("/GetDeckTimestamp/{deck_hash}", get(get_deck_timestamp))
        .route("/GetLargeDecks", get(get_large_decks))
        .route("/submitChangelog", post(submit_changelog))    
        .route("/login", post(post_login))
        .route("/removeToken/{token}", get(remove_token))
        .route("/UploadDeckStats", post(upload_deck_stats))
        .route("/requestRemoval", post(request_removal))
        .route("/CheckUserToken", post(check_user_token))
        .route("/refreshToken", post(refresh_auth_token))
        .route("/GetProtectedFields/{deck_hash}", get(get_protected_fields_from_deck))
        .nest("/media", media_routes())
        // standard rate limiting
        .layer(GovernorLayer {
            config: global_governor_conf,
        })
        .with_state(state)
        //.layer(axum::extract::DefaultBodyLimit::disable())
        .layer((
            TraceLayer::new_for_http(),
            // Graceful shutdown will wait for outstanding requests to complete. Add a timeout so
            // requests don't hang forever. Causes issues for streaming large decks that take more than 10secs to generate. hence i disabled it
            //TimeoutLayer::new(Duration::from_secs(10)),
        ))
        .layer(SecureClientIpSource::ConnectInfo.into_extension());

    // run it
    let listener = tokio::net::TcpListener::bind("localhost:5555").await.unwrap();
    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
