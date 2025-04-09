use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use aws_sdk_s3::{
    presigning::PresigningConfig, primitives::ByteStream
};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use axum_client_ip::SecureClientIp;
use base64::Engine;
use chrono::{Duration, Utc};
use std::sync::Arc;
use tokio_postgres::{Client, Error as PgError};
use tokio::time;
use uuid::Uuid;

use md5::{Md5, Digest};
use futures::stream::StreamExt;
use std::collections::{HashSet, HashMap};
use tokio::io::AsyncReadExt;
use bytes::BytesMut;
use crate::structs::*;
use crate::{auth, AppState};
use crate::media_logger::{self, MediaOperationType};

use bb8_postgres::bb8::PooledConnection;
use bb8_postgres::PostgresConnectionManager;
type SharedConn = PooledConnection<'static, PostgresConnectionManager<tokio_postgres::NoTls>>;
use lazy_static::lazy_static;
use std::env::var;

// Media file type constants
const MAX_FILE_SIZE_BYTES: usize = 2 * 1024 * 1024; // 2 MB

// S3 bucket configuration
lazy_static! {
    static ref MEDIA_BUCKET: String = var("S3_BUCKET_NAME").unwrap();
}

const PRESIGNED_URL_EXPIRATION: u64 = 15; // minutes
const SIGNATURE_CHECK_BYTES: usize = 128; // Maximum bytes needed for file signature validation

// Initialize media tables in database
pub async fn init_media_tables(db: &Client) -> std::result::Result<(), PgError> {
    db.batch_execute(
        "
        CREATE TABLE IF NOT EXISTS media_files (
            id BIGINT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
            hash VARCHAR(64) NOT NULL UNIQUE,
            file_size BIGINT NOT NULL DEFAULT 0,  -- Add file size column
            created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
        );

        CREATE TABLE IF NOT EXISTS media_references (
            id BIGINT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
            media_id BIGINT NOT NULL REFERENCES media_files(id) ON DELETE CASCADE,
            note_id BIGINT REFERENCES notes(id) ON DELETE CASCADE,
            file_name VARCHAR(255) NOT NULL,
            UNIQUE(media_id, note_id)
        );

        -- Bulk upload metadata
        CREATE TABLE IF NOT EXISTS media_bulk_uploads (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            metadata JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );

        CREATE TABLE IF NOT EXISTS media_operations_log (
            id BIGINT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
            operation_type INTEGER NOT NULL,
            user_id INTEGER,
            ip_address VARCHAR(45) NOT NULL,
            timestamp TIMESTAMPTZ NOT NULL,
            file_hash VARCHAR(64),
            file_name VARCHAR(255),
            file_size BIGINT,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE SET NULL
        );

        CREATE TABLE user_quotas (
            user_id INTEGER NOT NULL PRIMARY KEY,
            storage_used BIGINT NOT NULL DEFAULT 0,
            upload_count INTEGER NOT NULL DEFAULT 0,
            download_count INTEGER NOT NULL DEFAULT 0,
            last_reset TIMESTAMP WITH TIME ZONE NOT NULL,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS media_files_hash_idx ON media_files(hash);
        CREATE INDEX IF NOT EXISTS media_references_note_id_idx ON media_references(note_id);
        CREATE INDEX IF NOT EXISTS media_references_media_idx ON media_references(media_id);
        CREATE INDEX IF NOT EXISTS media_references_file_name_idx ON media_references(file_name);
        CREATE INDEX IF NOT EXISTS notes_guid_idx ON notes(guid);

        DROP TABLE auth_tokens;
        CREATE TABLE auth_tokens (
            id INT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            token_hash BYTEA NOT NULL,
            refresh_token_hash BYTEA,
            expires_at TIMESTAMP WITH TIME ZONE NOT NULL,
            refresh_expires_at TIMESTAMP WITH TIME ZONE,
            created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
            last_used_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
        );

        CREATE UNIQUE INDEX idx_auth_tokens_user_id ON auth_tokens(user_id);
        CREATE INDEX idx_auth_tokens_token_hash ON auth_tokens(token_hash);
        CREATE INDEX idx_auth_tokens_refresh_token_hash ON auth_tokens(refresh_token_hash);
        CREATE INDEX idx_auth_tokens_expires_at ON auth_tokens(expires_at);        
        "
    ).await
}

// Get all objects from s3, see which ones aren't references in the media table (should be impossible to have this happen, but just in case) and remove them.
pub async fn cleanup_orphaned_media_s3(
    state: Arc<AppState>,
    dry_run: bool,
) -> Result<usize, (StatusCode, String)> {
    let db_hashes = {
        let conn = state.db_pool.get().await.map_err(|err| {
            println!("Error getting pool: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error getting database connection".to_string())
        })?;
        
        let rows = conn
            .query("SELECT hash FROM media_files", &[])
            .await.map_err(|err| {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
            })?;

        rows.into_iter()
            .map(|row| row.get::<usize, String>(0))
            .collect::<HashSet<String>>()
    };
    if db_hashes.is_empty() && !dry_run {
        println!("No media hashes found in the database.");
        return Ok(0);
    }

    let mut orphaned_s3_keys: Vec<String> = Vec::new(); // Store the *full S3 keys* of orphans
    let mut continuation_token: Option<String> = None;
    let mut total_s3_objects_scanned = 0;
    let mut invalid_key_format_count = 0;
    let mut prefix_mismatch_count = 0;

    loop {
        let mut request_builder = state
            .s3_client
            .list_objects_v2()
            .bucket(MEDIA_BUCKET.as_str());

        if let Some(token) = &continuation_token { // Borrow token
            request_builder = request_builder.continuation_token(token.clone());
        }

        let resp = request_builder.send().await.unwrap(); // Propagate S3 list error

        // Process the objects in the current page
        if let Some(objects) = resp.contents {
             let count = objects.len();
             total_s3_objects_scanned += count;

            for object in objects {
                if let Some(s3_key) = object.key {
                    // Expect key format: "ab/abcdef123..."
                    let parts: Vec<&str> = s3_key.splitn(2, '/').collect();

                    // Basic format check
                    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                        println!("Invalid S3 key format found (expected 'prefix/hash'). Skipping.");
                        invalid_key_format_count += 1;
                        continue;
                    }
                    let s3_prefix = parts[0];
                    let s3_hash = parts[1];

                    // Check prefix length and that it matches the start of the hash
                    if s3_prefix.len() != 2 || !s3_hash.starts_with(s3_prefix) {
                        println!("S3 key prefix validation failed (length != 2 or mismatch with hash start). Skipping.");
                        prefix_mismatch_count += 1;
                        continue;
                    }

                    // Check if the extracted hash exists in our set of DB hashes
                    if !db_hashes.contains(s3_hash) {
                        orphaned_s3_keys.push(s3_key.clone());
                    }
                } else {
                    println!("S3 object found with no key. Skipping.");
                }
            }
        }

        // Check if the response is truncated (more pages exist)
        if resp.is_truncated.unwrap_or(false) {
             continuation_token = resp.next_continuation_token;
             if continuation_token.is_none() {
                 // This shouldn't happen if is_truncated is true, but handle defensively
                 println!("S3 response is truncated but no continuation token provided. Stopping pagination prematurely.");
                 break;
             }
        } else {
            break;
        }
    }

    println!(
        "Scan complete. Scanned {} objects in S3. Found {} potential orphans.",
        total_s3_objects_scanned,
        orphaned_s3_keys.len(),
    );
    if invalid_key_format_count > 0 {
        println!("Skipped {} keys due to invalid format.", invalid_key_format_count);
    }
    if prefix_mismatch_count > 0 {
        println!("Skipped {} keys due to prefix/hash mismatch.", prefix_mismatch_count);
    }

    if orphaned_s3_keys.is_empty() {
        println!("No orphaned S3 objects found requiring action.");
        return Ok(0);
    }

    if dry_run {
        println!("[DRY RUN] Identified orphaned S3 objects (not deleting): {}", orphaned_s3_keys.len());
        // Log first few samples for verification
        for (i, key) in orphaned_s3_keys.iter().take(10).enumerate() {
            println!("[DRY RUN] Orphan sample {}: s3://{}/{}", i + 1, MEDIA_BUCKET.as_str(), key);
        }
        if orphaned_s3_keys.len() > 10 {
            println!("[DRY RUN] ... (and {} more)", orphaned_s3_keys.len() - 10);
        }
        Ok(0)
    } else {
        let mut total_deleted_count = 0;
        let mut deletion_had_errors = false;

        for keys_chunk in orphaned_s3_keys.chunks(1000) {
            if keys_chunk.is_empty() { continue; } // Should not happen, but safe check

            let objects_to_delete: Vec<ObjectIdentifier> = keys_chunk
                .iter()
                .map(|k| ObjectIdentifier::builder().key(k).build().expect("Key must be valid UTF-8")) // Build ObjectIdentifier for each key
                .collect();

            let delete_builder = Delete::builder()
                .set_objects(Some(objects_to_delete)) // Use set_objects for Vec<ObjectIdentifier>
                .quiet(false); // Set quiet=false to get results for each key

            let delete_request = delete_builder.build().expect("Delete request structure is valid");

            println!("Attempting to delete batch of {} objects...", keys_chunk.len());

            match state
                .s3_client
                .delete_objects()
                .bucket(MEDIA_BUCKET.as_str())
                .delete(delete_request)
                .send()
                .await
            {
                Ok(output) => {
                    if let Some(deleted_objects) = output.deleted {
                        let batch_deleted_count = deleted_objects.len();
                        println!("Successfully deleted batch of {} objects.", batch_deleted_count);
                        total_deleted_count += batch_deleted_count;
                    }
                    if let Some(errors) = output.errors {
                        deletion_had_errors = true; // Mark that at least one error occurred
                        println!("Errors occurred during batch deletion:");
                        for error in errors {
                            println!(
                                " - Failed to delete key '{}': Code={}, Message={}",
                                error.key.as_deref().unwrap_or("N/A"),
                                error.code.as_deref().unwrap_or("N/A"),
                                error.message.as_deref().unwrap_or("N/A")
                            );
                        }
                    }
                }
                Err(err) => {
                    // Error sending the entire batch delete request
                    deletion_had_errors = true;
                    println!("Failed to send batch delete request: {}", err);
                }
            }
        } // end batch deletion loop

        println!("Orphan cleanup finished. Total objects deleted: {}", total_deleted_count);

        // Return an error if any part of the deletion failed
        if deletion_had_errors {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "Error during deletion".to_string()));
        } else {
            Ok(total_deleted_count)
        }
    }
}

// Housekeeping: Delete orphaned media files based on the postgres table
pub async fn cleanup_orphaned_media(state: Arc<AppState>) -> Result<(), (StatusCode, String)>{
    //info!("Starting orphaned media cleanup job");

    let mut db_client = state.db_pool.get().await.map_err(|err| {
        println!("Error getting pool: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Error getting database connection".to_string())
    })?;

    let tx = db_client.transaction().await.map_err(|err| {
        println!("Error starting transaction: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string())
    })?;

    // Find orphaned media files (not referenced in media_references)
    let orphaned_files = tx.query(
        "SELECT id, hash FROM media_files m
         WHERE NOT EXISTS (SELECT 1 FROM media_references r WHERE r.media_id = m.id)",
        &[]
    ).await.map_err(|err| {
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;
    
    //println!("Found {} orphaned media files to delete", orphaned_files.len());
    
    for row in orphaned_files {
        let id: i64 = row.get(0);
        let hash: String = row.get(1);
        
        let prefix = &hash[0..2];
        let s3_key = format!("{}/{}", prefix, hash);
        
        // Delete from S3
        match state.s3_client.delete_object()
            .bucket(MEDIA_BUCKET.as_str())
            .key(&s3_key)
            .send()
            .await
        {
            Ok(_) => {
                // Delete from database
                if let Err(e) = tx.execute("DELETE FROM media_files WHERE id = $1", &[&id]).await {
                    println!("Failed to delete media file {} from database: {}", hash, e);
                    continue;
                }                
            },
            Err(e) => {
                println!("Failed to delete media file {} from S3: {}", hash, e);
                continue;
            }
        }
    }

    tx.commit().await.map_err(|err| {
        println!("Error committing transaction: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;
    
    //info!("Completed orphaned media cleanup job");
    Ok(())
}

// Spawn a background task to periodically clean up orphaned media
pub async fn start_cleanup_task(state: Arc<AppState>) {
    let cleanup_interval = Duration::hours(4).to_std().unwrap();
    
    let orphan_clone = state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(cleanup_interval);
        
        loop {
            interval.tick().await;
            
            if let Err(e) = cleanup_orphaned_media(orphan_clone.clone()).await {
                println!("Media cleanup task failed: {:?}", e);
            }
        }
    });
    
    //info!("Media cleanup task scheduled");

    // Add bulk upload cleanup
    let bulk_state = state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(tokio::time::Duration::from_secs(3600 * 24)); // 24 hour
        
        loop {
            interval.tick().await;
            
            if let Ok(client) = bulk_state.db_pool.get().await {
                match client.query(
                    "SELECT id, metadata FROM media_bulk_uploads WHERE created_at < NOW() - INTERVAL '1 day'",
                    &[]
                ).await {
                    Ok(rows) => {
                        for row in rows {
                            // Check if there are any media references for this upload, delete all files that have no notes associated with them
                            let upload_id: Uuid = row.get(0);
                            let metadata: serde_json::Value = row.get(1);
                            let files: Vec<MediaMissingFile> = serde_json::from_value(metadata).unwrap();

                            let mut hash_vec = Vec::new();
                            for file in &files {
                                hash_vec.push(file.hash.clone());
                            }

                            let mut files_to_delete = Vec::new();
                            let mut files_to_keep = Vec::new();
                            let media_ids = client.query(
                                "SELECT id, hash FROM media_files WHERE hash = ANY($1)",
                                &[&hash_vec]
                            ).await.unwrap();

                            for row in media_ids {
                                let id: i64 = row.get(0);
                                let hash: String = row.get(1);
                                let note_id = client.query_opt(
                                    "SELECT id FROM media_references WHERE media_id = $1 LIMIT 1",
                                    &[&id]
                                ).await.unwrap();
                                if note_id.is_none() {
                                    files_to_delete.push(hash);
                                } else {
                                    files_to_keep.push(hash);
                                }
                            }

                            // Find orphans that dont exist anywhere in the database but are somehow related to this bulk
                            for file in &files {
                                if !files_to_keep.contains(&file.hash) && !files_to_delete.contains(&file.hash) {
                                    files_to_delete.push(file.hash.clone());
                                }
                            }
                            
                            // Delete the upload record
                            let _ = client.execute(
                                "DELETE FROM media_bulk_uploads WHERE id = $1",
                                &[&upload_id]
                            ).await;

                            if files_to_delete.is_empty() {
                                continue;
                            }

                            // Delete the files from the database
                            let _ = client.execute(
                                "DELETE FROM media_files WHERE hash = ANY($1)",
                                &[&files_to_delete]
                            ).await;

                            // Delete the files from S3
                            for hash in files_to_delete {
                                let prefix = &hash[0..2];
                                let s3_key = format!("{}/{}", prefix, hash);
                                let _ = bulk_state.s3_client.delete_object()
                                    .bucket(MEDIA_BUCKET.as_str())
                                    .key(&s3_key)
                                    .send()
                                    .await;
                            }

                        }
                    },
                    Err(e) => {
                        println!("Error cleaning up expired bulk uploads: {}", e);
                    }
                }
            }
        }
    });

}

pub async fn check_media_bulk(
    State(state): State<Arc<AppState>>,
    client_ip: SecureClientIp,
    Json(req): Json<MediaBulkCheckRequest>,
) -> Result<Json<MediaBulkCheckResponse>, (StatusCode, String)> {

    let user_id = auth::get_user_from_token(&state, &req.token).await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Error authenticating user".to_string()))?;
    
    if user_id == 0 {
        return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
    }
    let ip_address = client_ip.0;

    // Check if request is valid
    if req.files.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No files provided".to_string()));
    }

    if req.files.len() > 100 {
        return Err((StatusCode::BAD_REQUEST, "Too many files in single request".to_string()));
    }

    let mut invalid_files = Vec::new();
    let mut valid_files = Vec::new();
    // basic check if the files are valid with is_allowed_extension
    for file in req.files {
        if is_allowed_extension(&file.filename) {
            valid_files.push(file);
        } else {
            //println!("Invalid file extension: {}", file.filename.clone());
            invalid_files.push(file);
        }
    }
   
    let mut db_client = state.db_pool.get().await.map_err(|err| {
        println!("Error getting pool: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    let tx = db_client.transaction().await.map_err(|err| {
        println!("Error starting transaction: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    // Get all note IDs that are in the deck or its descendants
    let note_guids: Vec<&str> = valid_files.iter().map(|file| file.note_guid.as_str()).collect();
    let note_ids = tx.query(
        "WITH RECURSIVE deck_tree AS (
            SELECT id, parent
            FROM decks
            WHERE human_hash = $1            
            UNION ALL
            SELECT d.id, d.parent
            FROM decks d
            INNER JOIN deck_tree dt ON dt.id = d.parent
        )
        SELECT n.id, n.guid
        FROM notes n
        INNER JOIN deck_tree dt ON n.deck = dt.id
        WHERE n.deleted = false AND n.guid = ANY($2)",
        &[&req.deck_hash, &note_guids]
    ).await.map_err(|err| {
        println!("Error querying notes: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    // Create a HashMap to store guid -> id mapping
    let mut note_id_map: HashMap<String, i64> = HashMap::new();
    for row in note_ids {
        let id: i64 = row.get(0);
        let guid: String = row.get(1);
        note_id_map.insert(guid, id);
    }

    // Get a list of all file hashes for checking existence
    let hashes: Vec<String> = valid_files.iter().map(|file| file.hash.clone()).collect();
    
    // Check which files already exist in the database
    let existing_files = tx.query(
        "SELECT id, hash FROM media_files WHERE hash = ANY($1)",
        &[&hashes]
    ).await.map_err(|err| {
        println!("Error querying database: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    // Create a map of hash -> media_id for existing files
    let mut existing_media_ids_map = HashMap::new();
    for row in existing_files {
        let id: i64 = row.get(0);
        let hash: String = row.get(1);
        existing_media_ids_map.insert(hash, id);
    }

    // Insert references for existing files
    for (hash, media_id) in &existing_media_ids_map {
        for file in &valid_files {
            if file.hash == *hash {
                let try_note_id = note_id_map.get(&file.note_guid);
                if try_note_id.is_none() {
                    continue;
                }
                let note_id = try_note_id.unwrap();

                tx.execute(
                    "INSERT INTO media_references (media_id, note_id, file_name) VALUES ($1, $2, $3) 
                     ON CONFLICT (media_id, note_id) DO NOTHING",
                    &[media_id, note_id, &file.filename]
                ).await.map_err(|err| {
                    println!("Error inserting reference: {}", err);
                    (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
                })?;
            }
        }
    }

    // Prepare lists of existing and missing files
    let mut existing_files_response = Vec::new();
    let mut missing_files_response = Vec::new();

    let mut invalid_hash_set = HashSet::new();

    
    // Calculate total potential storage cost
    let total_potential_bytes: u64 = valid_files.iter()
    .map(|f| // file size of the file.hash not in existing_media_ids_map
        if existing_media_ids_map.contains_key(&f.hash) {
            0
        } else {
            f.file_size as u64
        })
    .sum();
    let potential_file_count = valid_files.len() - existing_media_ids_map.len();

    // check user-specific quota
    if !state.rate_limiter.check_user_upload_allowed(user_id, ip_address, potential_file_count as u32, total_potential_bytes).await {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "You reached the upload limit. Please try again tomorrow.".to_string()
        ));
    }
    
    // Generate presigned URLs for each missing file
    for file in &valid_files {
        if let Some(&media_id) = existing_media_ids_map.get(&file.hash) {
            existing_files_response.push(MediaExistingFile {
                hash: file.hash.clone(),
                media_id,
            });
        } else {
            let note_id = match note_id_map.get(&file.note_guid) {
                Some(nid) => nid,
                None => {
                    invalid_hash_set.insert(file.hash.clone());
                    //println!("Note ID not found for file: {}", file.filename);
                    continue; // Skip if note ID is not found
                }
            };

            if file.file_size <= 0 || file.file_size > MAX_FILE_SIZE_BYTES as i64 {
                invalid_hash_set.insert(file.hash.clone());
                //println!("Invalid file size for file: {}", file.filename);
                continue; // Skip if file size is invalid
            }        

            let file_content_type = match determine_content_type_by_name(&file.filename) {
                Some(content_type) => content_type,
                None => {
                    invalid_hash_set.insert(file.hash.clone());
                    //println!("Invalid file content type for file: {}", file.filename);
                    continue; // Skip if content type is invalid
                }
            };

            // Create the file location in S3
            let prefix = &file.hash[0..2];
            let s3_key = format!("{}/{}", prefix, file.hash);

            // Log attempted file upload
            media_logger::log_media_operation(
                &tx,
                MediaOperationType::Upload, 
                Some(user_id), 
                ip_address, 
                Some(file.hash.clone()), 
                Some(file.filename.clone()), 
                Some(file.file_size),
            ).await.unwrap();

            let hash_bytes = hex::decode(&file.hash).unwrap_or_default();
            let base64_md5 = base64::engine::general_purpose::STANDARD.encode(&hash_bytes);

            // Generate a presigned URL for this specific file
            let presigned_req = match state.s3_client.put_object()
                .bucket(MEDIA_BUCKET.as_str())
                .key(&s3_key)
                .content_length(file.file_size)
                .content_type(file_content_type)
                .content_md5(base64_md5)
                .presigned(
                    PresigningConfig::expires_in(std::time::Duration::from_secs(PRESIGNED_URL_EXPIRATION * 60)).unwrap()
                )
                .await {
                    Ok(req) => req,
                    Err(err) => {
                        println!("Error generating presigned URL for {}: {}", file.hash, err);
                        invalid_hash_set.insert(file.hash.clone());
                        continue;
                    }
                };
            
            let missing_file = MediaMissingFile {
                hash: file.hash.clone(),
                filename: file.filename.clone(),
                note_id: *note_id,
                file_size: file.file_size, // Store the file size
                presigned_url: Some(presigned_req.uri().to_string()),
            };
            
            missing_files_response.push(missing_file);
        }
    }

    for file in &valid_files {
        if invalid_hash_set.contains(&file.hash) {
            invalid_files.push(file.clone());
        }
    }
    valid_files.retain(|file| !invalid_hash_set.contains(&file.hash));

    state.rate_limiter.track_user_upload(user_id, ip_address, total_potential_bytes, missing_files_response.len() as u32).await;    

    // Store metadata about missing files for later confirmation
    let mut batch_id = None;
    if !missing_files_response.is_empty() {
        // Generate a unique ID for this batch
        batch_id = Some(Uuid::new_v4());
        
        // Convert to JSON
        let batch_metadata = serde_json::to_value(&missing_files_response).map_err(|err| {
            println!("Error serializing metadata: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error processing request".to_string())
        })?;
        
        // Store in database with expiration
        tx.execute(
            "INSERT INTO media_bulk_uploads (id, metadata, created_at) VALUES ($1, $2, NOW())",
            &[&batch_id, &batch_metadata]
        ).await.map_err(|err| {
            println!("Error storing bulk upload metadata: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error storing upload metadata".to_string())
        })?;
    }

    tx.commit().await.unwrap();

    if batch_id.is_none() {
        return Ok(Json(MediaBulkCheckResponse {
            existing_files: existing_files_response,
            missing_files: missing_files_response,
            failed_files: invalid_files,
            batch_id: None,
        }));
    }
    let batch_id_str = Some(batch_id.unwrap().to_string());

    // If there are no missing files, just return existing ones
    Ok(Json(MediaBulkCheckResponse {
        existing_files: existing_files_response,
        missing_files: missing_files_response,
        failed_files: invalid_files,
        batch_id: batch_id_str,
    }))
}

pub async fn confirm_media_bulk_upload(
    state: Arc<AppState>,
    req: MediaBulkConfirmRequest,
) -> Result<Json<MediaBulkConfirmResponse>, (StatusCode, String)> {

    if req.confirmed_files.is_empty() || req.confirmed_files.len() > 100 {
        return Err((StatusCode::BAD_REQUEST, "Invalid Format".to_string()));
    }

    let mut db_client = state.db_pool.get().await.map_err(|err| {
        println!("Error getting pool: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    let batch_id_uuid = Uuid::parse_str(&req.batch_id).map_err(|_| {
        (StatusCode::BAD_REQUEST, "Invalid batch ID".to_string())
    })?;  
    // Retrieve the bulk upload metadata
    let metadata_row = db_client.query_opt(
        "SELECT metadata FROM media_bulk_uploads WHERE id = $1",
        &[&batch_id_uuid]
    ).await.map_err(|err| {
        println!("Error querying database: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Error retrieving upload metadata".to_string())
    })?;

    let metadata_json: serde_json::Value = match metadata_row {
        Some(row) => row.get(0),
        None => return Err((StatusCode::NOT_FOUND, "Batch not found or expired".to_string())),
    };

    let files: Vec<MediaMissingFile> = serde_json::from_value(metadata_json).map_err(|err| {
        println!("Error parsing metadata: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Error parsing upload metadata".to_string())
    })?;

    // Find files that client claims were uploaded
    let confirmed_hashes: HashSet<String> = req.confirmed_files.into_iter().collect();
    let confirmed_files: Vec<&MediaMissingFile> = files.iter()
        .filter(|f| confirmed_hashes.contains(&f.hash) && is_allowed_extension(&f.filename))
        .collect();

    let unconfirmed_files: Vec<&MediaMissingFile> = files.iter()
        .filter(|f| !confirmed_hashes.contains(&f.hash))
        .collect();

    // Process the uploaded files
    let mut processed_files = Vec::new();
    let tx = db_client.transaction().await.map_err(|err| {
        println!("Error starting transaction: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    let note_ids: Vec<i64> = confirmed_files.iter()
    .map(|f| f.note_id)
    .collect();

    // validate notes
    let valid_notes = tx.query(
    "SELECT id FROM notes WHERE id = ANY($1) AND deleted = false",
    &[&note_ids]
    ).await.map_err(|err| {
        println!("Error querying database: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    let valid_note_ids: HashSet<i64> = valid_notes.iter()
    .map(|row| row.get::<_, i64>(0))
    .collect();

    for file in &confirmed_files {
        let prefix = &file.hash[0..2];
        let s3_key = format!("{}/{}", prefix, file.hash);
        
        // Verify the file exists in S3 and check its properties
        match state.s3_client.head_object()
            .bucket(MEDIA_BUCKET.as_str())
            .key(&s3_key)
            .send()
            .await {
                Ok(head_response) => {
                    if !valid_note_ids.contains(&file.note_id) {
                        // Delete invalid file
                        let _ = state.s3_client.delete_object()
                            .bucket(MEDIA_BUCKET.as_str())
                            .key(&s3_key)
                            .send()
                            .await;
                            
                        processed_files.push(MediaProcessedFile {
                            hash: file.hash.clone(),
                            media_id: 0i64,
                            success: false,
                            error: Some("Invalid file".to_string()),
                        });
                        continue;
                    }

                    // Ensure file size matches what was specified (within small tolerance)
                    let actual_size = head_response.content_length.unwrap_or(0);
                    let actual_size_f64 = actual_size as f64;
                    let file_size_f64 = file.file_size as f64;
                    if actual_size > MAX_FILE_SIZE_BYTES as i64 || actual_size == 0 ||
                    (actual_size_f64 > file_size_f64 * 1.01) || // Allow 1% tolerance
                    (actual_size_f64 < file_size_f64 * 0.99) {  // Allow 1% tolerance
                        //println!("File size mismatch: expected {}, got {}", file.file_size, actual_size);
                        // Delete invalid file
                        let _ = state.s3_client.delete_object()
                            .bucket(MEDIA_BUCKET.as_str())
                            .key(&s3_key)
                            .send()
                            .await;
                            
                        processed_files.push(MediaProcessedFile {
                            hash: file.hash.clone(),
                            media_id: 0i64,
                            success: false,
                            error: Some("Invalid file".to_string()),
                        });
                        continue;
                    }
                                
                    let get_object_response = state.s3_client.get_object()
                        .bucket(MEDIA_BUCKET.as_str())
                        .key(&s3_key)
                        .send()
                        .await;
                        
                    match get_object_response {
                        Ok(response) => {
                            // Validate the stream and calculate hash in one pass
                            let validation_result = validate_media_stream(response.body, &file.hash)
                                .await
                                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Invalid data".to_string()))?;
                            
                            if !validation_result {
                                // Delete invalid file
                                let _ = state.s3_client.delete_object()
                                    .bucket(MEDIA_BUCKET.as_str())
                                    .key(&s3_key)
                                    .send()
                                    .await;
                                    
                                processed_files.push(MediaProcessedFile {
                                    hash: file.hash.clone(),
                                    media_id: 0i64,
                                    success: false,
                                    error: Some("Invalid file".to_string()),
                                });

                                //println!("File validation (stream) failed for: {}", file.filename);
                                continue;
                            }
                            // File is valid, add to database
                            match tx.query_one(
                                "INSERT INTO media_files (hash, file_size) 
                                    VALUES ($1, $2) 
                                    ON CONFLICT (hash) DO UPDATE 
                                    SET file_size = EXCLUDED.file_size 
                                    RETURNING id",
                                &[&file.hash, &actual_size]
                            ).await {
                                Ok(row) => {
                                    let media_id: i64 = row.get(0);
                                    // Create reference
                                    match tx.execute(
                                        "INSERT INTO media_references (media_id, note_id, file_name) 
                                        VALUES ($1, $2, $3)
                                        ON CONFLICT (media_id, note_id) DO NOTHING",
                                        &[&media_id, &file.note_id, &file.filename]
                                    ).await {
                                        Ok(_) => {
                                            processed_files.push(MediaProcessedFile {
                                                hash: file.hash.clone(),
                                                media_id,
                                                success: true,
                                                error: None,
                                            });
                                        },
                                        Err(e) => {
                                            processed_files.push(MediaProcessedFile {
                                                hash: file.hash.clone(),
                                                media_id,
                                                success: false,
                                                error: Some(format!("Error creating reference: {}", e)),
                                            });
                                        }
                                    }
                                },
                                Err(e) => {
                                    processed_files.push(MediaProcessedFile {
                                        hash: file.hash.clone(),
                                        media_id: 0i64,
                                        success: false,
                                        error: Some(format!("Error inserting media file: {}", e)),
                                    });
                                }
                            }
                        },
                        Err(e) => {
                            processed_files.push(MediaProcessedFile {
                                hash: file.hash.clone(),
                                media_id: 0i64,
                                success: false,
                                error: Some(format!("Error retrieving file: {}", e)),
                            });
                        }
                    }
                },
                Err(e) => {
                    processed_files.push(MediaProcessedFile {
                        hash: file.hash.clone(),
                        media_id: 0i64,
                        success: false,
                        error: Some(format!("File not found: {}", e)),
                    });
                }
            }
    }

    // handle the unconfirmed files (delete from S3) even though they likely dont even exist
    // delete the files from S3
    for file in unconfirmed_files {
        let prefix = &file.hash[0..2];
        let s3_key = format!("{}/{}", prefix, file.hash);
        let _ = state.s3_client.delete_object()
            .bucket(MEDIA_BUCKET.as_str())
            .key(&s3_key)
            .send()
            .await;
    }

    // Track how much storage was uesd
    let actual_bytes_stored: u64 = processed_files.iter()
        .filter(|f| f.success)
        .map(|f| {
            // Find the original file size from the confirmed_files list
            confirmed_files.iter()
                .find(|cf| cf.hash == f.hash)
                .map(|cf| cf.file_size as u64)
                .unwrap_or(0)
        })
        .sum();
    
    // Track the global storage used
    state.rate_limiter.storage_monitor().track_operation(actual_bytes_stored);
    
    let batch_id_uuid = Uuid::parse_str(&req.batch_id).map_err(|_| {
        (StatusCode::BAD_REQUEST, "Invalid batch ID".to_string())
    })?;
    // Clean up the batch record
    let _ = tx.execute(
        "DELETE FROM media_bulk_uploads WHERE id = $1",
        &[&batch_id_uuid]
    ).await;
    
    tx.commit().await.map_err(|err| {
        println!("Error committing transaction: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    Ok(Json(MediaBulkConfirmResponse {
        processed_files,
    }))
}

use svg_hush::*;
fn sanitize_svg(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if data.len() > MAX_FILE_SIZE_BYTES || data.len() < 10 {
        return out;
    }

    // Check if it's actually an SVG
    if !is_svg(data) {
        return out;
    }
    
    let mut input = data;
    let mut filter = Filter::new();
    filter.set_data_url_filter(data_url_filter::allow_standard_images);
    filter.filter(&mut input, &mut out).unwrap_or_default();
    out
}

fn is_svg(bytes: &[u8]) -> bool {
    const SVG_TAG: [u8; 4] = [0x3C, 0x73, 0x76, 0x67]; // "<svg"
    const XML_DECLARATION: [u8; 4] = [0x3C, 0x3F, 0x78, 0x6D]; // "<?xm"
    
    if bytes.len() >= 4 {
        // Direct SVG tag
        if bytes[0..4] == SVG_TAG {
            return true;
        }
        
        // XML declaration followed by SVG tag
        if bytes[0..4] == XML_DECLARATION {
            // Look for "<svg" in the first 100 bytes (or less if the file is smaller)
            let search_limit = bytes.len().min(100);
            for i in 0..search_limit.saturating_sub(4) {
                if bytes[i..i+4] == SVG_TAG {
                    return true;
                }
            }
        }
    }
    false
}

async fn validate_media_stream(
    byte_stream: ByteStream,
    expected_hash: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut reader = byte_stream.into_async_read();
    let mut buffer = [0u8; 8192];
    let mut hasher = Md5::new();
    let mut total_size: u64 = 0;
    let mut content_type = None;
    let mut is_valid = false;
    let mut signature_buffer = BytesMut::with_capacity(SIGNATURE_CHECK_BYTES);
    let mut svg_content = Vec::new();
    
    // First read to check signature/content type
    let initial_bytes = match reader.read(&mut buffer).await? {
        0 => return Ok(false),
        bytes_read => {
            total_size = bytes_read as u64;
            
            // Store initial bytes for signature detection
            let signature_bytes = std::cmp::min(bytes_read, SIGNATURE_CHECK_BYTES);
            signature_buffer.extend_from_slice(&buffer[..signature_bytes]);
            
            // Determine content type and validity early
            content_type = determine_content_type(&signature_buffer);
            is_valid = is_valid_media_file(&signature_buffer);
            
            // Handle SVG specially
            if content_type == Some("image/svg+xml".to_string()) {
                svg_content.extend_from_slice(&buffer[..bytes_read]);
            } else {
                hasher.update(&buffer[..bytes_read]);
            }
            
            bytes_read
        }
    };
    
    // Early return if first chunk exceeds max size
    if total_size > MAX_FILE_SIZE_BYTES as u64 {
        return Ok(false);
    }
    
    // Continue reading the rest of the stream
    if initial_bytes > 0 {
        loop {
            let bytes_read = reader.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            
            total_size += bytes_read as u64;
            
            // Check size limit
            if total_size > MAX_FILE_SIZE_BYTES as u64 {
                return Ok(false);
            }
            
            // Process chunk based on content type
            if content_type == Some("image/svg+xml".to_string()) {
                svg_content.extend_from_slice(&buffer[..bytes_read]);
            } else {
                hasher.update(&buffer[..bytes_read]);
            }
        }
    }
    
    // Special handling for SVG content
    if content_type == Some("image/svg+xml".to_string()) {
        let sanitized = sanitize_svg(&svg_content);

        if sanitized.is_empty() {
            is_valid = false;
        } else {
            hasher = Md5::new();
            hasher.update(&sanitized);
        }
    }
    
    // Calculate final hash and validate
    let actual_hash = format!("{:x}", hasher.finalize());

    is_valid = is_valid && actual_hash == expected_hash;
    
    Ok(is_valid)
}

fn determine_content_type_by_name(file_name: &str) -> Option<String> {
    let ext = file_name.split('.').last().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "png" => Some("image/png".to_string()),
        "gif" => Some("image/gif".to_string()),
        "webp" => Some("image/webp".to_string()),
        "svg" => Some("image/svg+xml".to_string()),
        "bmp" => Some("image/bmp".to_string()),
        "tif" | "tiff" => Some("image/tiff".to_string()),
        _ => None,
    }
}

fn determine_content_type(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 8 {
        return None;
    }

    match &bytes[0..4] {
        [0xFF, 0xD8, 0xFF, _] => Some("image/jpeg".to_string()),
        [0x89, 0x50, 0x4E, 0x47] => Some("image/png".to_string()),
        [0x47, 0x49, 0x46, 0x38] => Some("image/gif".to_string()),
        [0x52, 0x49, 0x46, 0x46] if bytes.len() >= 12 && &bytes[8..12] == b"WEBP" => {
            Some("image/webp".to_string())
        }
        [0x3C, 0x73, 0x76, 0x67] | [0x3C, 0x3F, 0x78, 0x6D] => Some("image/svg+xml".to_string()),
        [0x42, 0x4D, _, _] => Some("image/bmp".to_string()),
        _ => None,
    }
}

// Function to validate file signatures https://en.wikipedia.org/wiki/List_of_file_signatures
fn is_valid_media_file(bytes: &[u8]) -> bool {
    const JPEG_MARKER: [u8; 3] = [0xFF, 0xD8, 0xFF];
    const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    const GIF87A_SIGNATURE: [u8; 6] = [0x47, 0x49, 0x46, 0x38, 0x37, 0x61]; // GIF87a
    const GIF89A_SIGNATURE: [u8; 6] = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61]; // GIF89a
    const WEBP_HEADER: [u8; 4] = [0x52, 0x49, 0x46, 0x46]; // 'R', 'I', 'F', 'F'
    const WEBP_MARKER: [u8; 4] = [0x57, 0x45, 0x42, 0x50]; // "WEBP"
    const BMP_MARKER: [u8; 2] = [0x42, 0x4D]; // "BM"
    const TIFF_LE_SIGNATURE: [u8; 4] = [0x49, 0x49, 0x2A, 0x00]; // "II*\0"
    const TIFF_BE_SIGNATURE: [u8; 4] = [0x4D, 0x4D, 0x00, 0x2A]; // "MM\0*"
    
    // Minimum bytes needed for signature validation
    if bytes.len() < 8 {
        return false;
    }

    // JPEG validation
    if bytes.len() >= 3 && bytes[0..3] == JPEG_MARKER {
        return true;
    }
    
    // PNG validation (complete 8-byte signature)
    if bytes.len() >= 8 && bytes[0..8] == PNG_SIGNATURE {
        return true;
    }
    
    // GIF validation
    if bytes.len() >= 6 && (bytes[0..6] == GIF87A_SIGNATURE || bytes[0..6] == GIF89A_SIGNATURE) {
        return true;
    }
    
    // WebP validation
    if bytes.len() >= 12 && bytes[0..4] == WEBP_HEADER && bytes[8..12] == WEBP_MARKER { // Bytes 4-8 are the filesize btw if anybody ever reads this and wonders :)
        return true;
    }
    
    // BMP validation
    if bytes.len() >= 2 && bytes[0..2] == BMP_MARKER {
        return true;
    }
    
    // TIFF validation
    if bytes.len() >= 4 && (bytes[0..4] == TIFF_LE_SIGNATURE || bytes[0..4] == TIFF_BE_SIGNATURE) {
        return true;
    }
    
    if is_svg(bytes) {
        return true;
    }
    
    // No valid signature found
    false
}

fn is_allowed_extension(filename: &str) -> bool {
    if filename.is_empty() || filename.len() > 255 || filename.len() < 5 {
        return false;
    }

    if filename.contains('/') || filename.contains('\\') || filename.contains("..") || filename.ends_with('.') || filename.ends_with(' ') {
        return false;
    }

    if filename.trim().is_empty() {
        return false;
    }

    if !filename.chars().all(|c| {
        !c.is_control() &&
        (c.is_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ' ' || c == '(' || c == ')' || c == '+' || c == ',' || c == '%' || c == '&')
    }) {
        return false;
    }

    // Name must start with a letter or number (to avoid leading dots/spaces)
    if !filename.chars().next().map_or(false, |c| c.is_alphanumeric()) {
        return false;
    }
    
    // Name must contain at least one letter or number (to avoid only dots/spaces)
    if !filename.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }

    let path = std::path::Path::new(filename);
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => ext.to_lowercase(),
        None => return false,
    };

    const ALLOWED_EXTENSIONS: [&str; 9] = [
        "jpg", "jpeg", "png", "gif", "webp", "svg", "bmp", "tif", "tiff"
    ];

    ALLOWED_EXTENSIONS.contains(&ext.as_str())
}

pub async fn get_media_manifest(
    State(state): State<Arc<AppState>>,
    client_ip: SecureClientIp,
    Json(req): Json<MediaManifestRequest>,
) -> Result<Json<MediaManifestResponse>, (StatusCode, String)> {
    
    let user_id = auth::get_user_from_token(&state, &req.user_token).await
    .map_err(|_| (StatusCode::UNAUTHORIZED, "Error authenticating user (1)".to_string()))?;
    
    if user_id == 0 {
        return Err((StatusCode::UNAUTHORIZED, "Error authenticating user (2)".to_string()));
    }

    if req.filenames.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No filenames provided".to_string()));
    }

    if req.filenames.len() > 500 {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "Too many files requested.".to_string()
        ));
    }

    let ip_address = client_ip.0;

    let db_client = state.db_pool.get_owned().await.map_err(|err| {
        println!("Error getting pool: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    // find the media file hashes based on the filenames and the deck hash
    let media_query = 
            "
            WITH RECURSIVE deck_tree AS (
                SELECT id FROM decks WHERE human_hash = $1
                UNION ALL
                SELECT d.id FROM decks d
                INNER JOIN deck_tree dt ON d.parent = dt.id
            )
            SELECT DISTINCT mf.hash, mr.file_name, mf.file_size
            FROM media_references mr
            JOIN media_files mf ON mf.id = mr.media_id
            JOIN notes n ON mr.note_id = n.id
            WHERE n.deck IN (SELECT id FROM deck_tree)
            AND mr.file_name = ANY($2)
            ";

    let media_rows = db_client.query(media_query, &[&req.deck_hash, &req.filenames]).await.map_err(|err| {
        println!("Error querying media files: {}", err);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string())
    })?;

    // Calculate total size for download tracking
    let total_size: i64 = media_rows.iter()
        .map(|row| row.get::<_, i64>(2))
        .sum();

    // Create the manifest data
    let now = Utc::now();
    let expires_at = now + Duration::minutes(PRESIGNED_URL_EXPIRATION as i64);
    
    let length_vec = media_rows.len() as u32;
    let mut file_entries: Vec<MediaDownloadItem> = Vec::with_capacity(length_vec.try_into().unwrap() );

    // Check download limits for this IP using actual size
    if !state.rate_limiter.check_user_download_allowed(user_id, ip_address, length_vec).await {
        println!("Download limit exceeded for {:?}", client_ip);
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "Download limit exceeded. Please try again tomorrow.".to_string()
        ));
    }


    for row in media_rows {
        let hash: String = row.get(0);
        let filename: String = row.get(1);

        let prefix = &hash[0..2];
        let s3_key = format!("{}/{}", prefix, hash);
        
        // Generate a presigned URL for each file
        let presigned_req = state.s3_client.get_object()
            .bucket(MEDIA_BUCKET.as_str())
            .key(&s3_key)
            .presigned(
                PresigningConfig::expires_in(std::time::Duration::from_secs(PRESIGNED_URL_EXPIRATION * 60)).unwrap()
            )
            .await.map_err(|err| {
                println!("Error generating presigned URL for {}: {}", hash, err);
                (StatusCode::INTERNAL_SERVER_ERROR, "Error generating download link".to_string())
            })?;
        
        file_entries.push(MediaDownloadItem {
            filename,
            download_url: presigned_req.uri().to_string()
        });
    }

    state.rate_limiter.track_user_download(user_id, ip_address, length_vec).await;
        
    let response =  MediaManifestResponse {
        files: file_entries,
        file_count: length_vec as i32,
        expires_at: expires_at.to_rfc3339(),
    };

    Ok(Json(response))
}

pub async fn sanitize_svg_batch(
    Json(req): Json<SvgSanitizeRequest>,
) -> Result<Json<SvgSanitizeResponse>, (StatusCode, String)> {
    // Validate request
    if req.svg_files.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No SVG files provided".to_string()));
    }
    
    // Limit batch size
    const MAX_BATCH_SIZE: usize = 250;
    const MAX_TOTAL_SIZE: usize = 1 * 1024 * 1024; // 1 MB total batch size
    
    if req.svg_files.len() > MAX_BATCH_SIZE {
        return Err((StatusCode::BAD_REQUEST, 
                   format!("Too many files in single request")));
    }
    
    // Calculate total size
    let total_size: usize = req.svg_files.iter()
        .map(|f| f.content.len())
        .sum();
        
    if total_size > MAX_TOTAL_SIZE {
        return Err((StatusCode::BAD_REQUEST, 
                   format!("Total batch size too large")));
    }
    
    let mut sanitized_files = Vec::with_capacity(req.svg_files.len());
    
    for file in req.svg_files {
        let content_bytes = file.content.as_bytes();
        if !is_svg(content_bytes) {
            continue; // Skip non-SVG files
        }
        
        let sanitized_bytes = sanitize_svg(content_bytes);
        if sanitized_bytes.is_empty() {
            continue;
        }
        
        // Convert back to string
        let sanitized_content = match String::from_utf8(sanitized_bytes) {
            Ok(content) => content,
            Err(_) => continue, // Skip if invalid UTF-8
        };
        
        let mut hasher = Md5::new();
        hasher.update(sanitized_content.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        
        sanitized_files.push(SanitizedSvgItem {
            content: sanitized_content,
            filename: file.filename,
            hash,
        });
    }
    
    Ok(Json(SvgSanitizeResponse {
        sanitized_files,
    }))
}