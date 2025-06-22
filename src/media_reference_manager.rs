// Duplicated code from the website. but since we have auto-approve (unfortunately) we have to handle this in the backend as well.
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use once_cell::sync::Lazy;

use rayon::prelude::*;

use bb8_postgres::bb8::PooledConnection;
use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::Error as PgError;

type SharedConn = PooledConnection<'static, PostgresConnectionManager<tokio_postgres::NoTls>>;

static SOUND_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[sound:(.*?)\]").unwrap());
static IMG_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<img[^>]*src=["']([^"']*)["'][^>]*>"#).unwrap());
static CSS_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r#"url\(["']?([^"')]+)["']?\)"#).unwrap());
static SRC_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?i)(?:src|xlink:href)=["']([^"']+)["']"#).unwrap());
static LATEX_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"latex-image-\w+\.png").unwrap());

/// Extract all media references from a field content string as anki does
#[must_use] pub fn extract_media_references(field_content: &str) -> HashSet<String> {
    let mut references = HashSet::new();
    
    // Sound references [sound:filename.mp3]
    for cap in SOUND_REGEX.captures_iter(field_content) {
        if let Some(filename) = cap.get(1) {
            references.insert(filename.as_str().to_string());
        }
    }
    
    // HTML img src
    for cap in IMG_REGEX.captures_iter(field_content) {
        if let Some(filename) = cap.get(1) {
            let src = filename.as_str();
            // Only consider local media files (not URLs)
            if !src.starts_with("http://") && 
               !src.starts_with("https://") && 
               !src.starts_with("data:") {
                references.insert(src.to_string());
            }
        }
    }
    
    // CSS url() references 
    for cap in CSS_REGEX.captures_iter(field_content) {
        if let Some(filename) = cap.get(1) {
            let src = filename.as_str();
            if !src.starts_with("http://") && 
               !src.starts_with("https://") && 
               !src.starts_with("data:") {
                references.insert(src.to_string());
            }
        }
    }
    
    // Other HTML elements with src attribute
    for cap in SRC_REGEX.captures_iter(field_content) {
        if let Some(filename) = cap.get(1) {
            let src = filename.as_str();
            if !src.starts_with("http://") && 
               !src.starts_with("https://") && 
               !src.starts_with("data:") {
                references.insert(src.to_string());
            }
        }
    }
    
    // LaTeX image references
    for cap in LATEX_REGEX.captures_iter(field_content) {
        references.insert(cap.get(0).unwrap().as_str().to_string());
    }
    
    references
}

/// Get all fields of a note
pub async fn get_note_fields(
    client: &SharedConn, 
    note_id: i64
) -> Result<Vec<String>, PgError> {
    let rows = client
        .query(
            "SELECT content FROM fields WHERE note = $1",
            &[&note_id]
        )
        .await?;
    
    let fields = rows.iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    
    Ok(fields)
}

/// Get all media references currently saved for a note
pub async fn get_existing_references(
    client: &SharedConn,
    note_id: i64
) -> Result<HashSet<String>, PgError> {
    let rows = client
        .query(
            "SELECT file_name FROM media_references WHERE note_id = $1",
            &[&note_id]
        )
        .await?;
    
    let refs = rows.iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    
    Ok(refs)
}

/// Update media references for a single note
pub async fn update_media_references_for_note(
    client: &mut SharedConn,
    note_id: i64
) -> Result<(), Box<dyn std::error::Error>> {    
    // Get all field content for the note
    let fields = get_note_fields(client, note_id).await?;
    
    // Extract all media references from fields
    let mut all_references = HashSet::new();
    for field in &fields {
        let refs = extract_media_references(field);
        all_references.extend(refs);
    }
        
    // Get existing references
    let existing_refs = get_existing_references(client, note_id).await?;
    
    // Calculate differences
    let to_add: HashSet<_> = all_references.difference(&existing_refs).cloned().collect();
    let to_remove: HashSet<_> = existing_refs.difference(&all_references).cloned().collect();
    
    if to_add.is_empty() && to_remove.is_empty() {
        return Ok(());
    }

    let tx = client.transaction().await?;
    
    // to_add should be empty, bc the media gets uploaded upon submission. We can't do anything about missing media here, so we'll just log it.

    // Remove old references
    for filename in &to_remove {
        tx.execute(
            "DELETE FROM media_references 
            WHERE note_id = $1 AND file_name = $2",
            &[&note_id, &filename]
        ).await?;
    }
    
    tx.commit().await?;
    
    
    Ok(())
}

pub async fn get_missing_media(client: &SharedConn, deck_hash: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let notes_query = client.query(
        "WITH RECURSIVE deck_tree AS (
            SELECT id FROM decks WHERE human_hash = $1
            UNION ALL
            SELECT d.id FROM decks d
            JOIN deck_tree dt ON d.parent = dt.id
        )
        SELECT id FROM notes WHERE deck IN (SELECT id FROM deck_tree)",
        &[&deck_hash]
    ).await?;
    
    let note_ids: Vec<i64> = notes_query.iter().map(|row| row.get(0)).collect();
    if note_ids.is_empty() {
        return Ok(Vec::new());
    }

    const BATCH_SIZE: usize = 1000;
    let mut missing_media = HashSet::new();
    
    // Process notes in batches
    for chunk in note_ids.chunks(BATCH_SIZE) {
        let chunk_vec = chunk.to_vec();
        
        // Use JSON aggregation to fetch all fields and references in a single query per batch
        let query_rows = client.query(
            "WITH note_fields AS (
                SELECT note, json_agg(content) as fields_json
                FROM fields
                WHERE note = ANY($1)
                GROUP BY note
            ),
            note_refs AS (
                SELECT note_id, json_agg(file_name) as refs_json
                FROM media_references
                WHERE note_id = ANY($1)
                GROUP BY note_id
            )
            SELECT
                nf.fields_json,
                COALESCE(nr.refs_json, '[]'::json) as refs_json
            FROM note_fields nf
            LEFT JOIN note_refs nr ON nf.note = nr.note_id",
            &[&chunk_vec]
        ).await?;
        
        let batch_missing_media: HashSet<String> = query_rows
            .par_iter()
            .flat_map(|row| {
                let fields_val: Value = row.get(0);
                let refs_val: Value = row.get(1);

                let mut content_references = HashSet::new();
                if let Value::Array(fields) = fields_val {
                    for field_val in fields {
                        if let Value::String(content) = field_val {
                            content_references.extend(extract_media_references(&content));
                        }
                    }
                }

                let mut existing_references = HashSet::new();
                if let Value::Array(refs) = refs_val {
                    for ref_val in refs {
                        if let Value::String(file_name) = ref_val {
                            existing_references.insert(file_name);
                        }
                    }
                }

                content_references
                    .difference(&existing_references)
                    .cloned()
                    .collect::<HashSet<_>>()
            })
            .collect();

        missing_media.extend(batch_missing_media);
    }

    // Convert HashSet to Vec for the final result to make serde stop bitching
    let missing_media_vec: Vec<String> = missing_media.into_iter().collect();

    Ok(missing_media_vec)
}