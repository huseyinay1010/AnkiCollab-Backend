// Duplicated code from the website. but since we have auto-approve (unfortunately) we have to handle this in the backend as well.
use regex::Regex;
use std::collections::HashSet;

use bb8_postgres::bb8::PooledConnection;
use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::Error as PgError;

type SharedConn = PooledConnection<'static, PostgresConnectionManager<tokio_postgres::NoTls>>;

/// Extract all media references from a field content string as anki does
pub fn extract_media_references(field_content: &str) -> HashSet<String> {
    let mut references = HashSet::new();
    
    // Sound references [sound:filename.mp3]
    let sound_regex = Regex::new(r"\[sound:(.*?)\]").unwrap();
    for cap in sound_regex.captures_iter(field_content) {
        if let Some(filename) = cap.get(1) {
            references.insert(filename.as_str().to_string());
        }
    }
    
    // HTML img src
    let img_regex = Regex::new(r#"<img[^>]*src=["']([^"']*)["'][^>]*>"#).unwrap();
    for cap in img_regex.captures_iter(field_content) {
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
    let css_regex = Regex::new(r#"url\(["']?([^"')]+)["']?\)"#).unwrap();
    for cap in css_regex.captures_iter(field_content) {
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
    let src_regex = Regex::new(r#"(?i)(?:src|xlink:href)=["']([^"']+)["']"#).unwrap();
    for cap in src_regex.captures_iter(field_content) {
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
    let latex_regex = Regex::new(r"latex-image-\w+\.png").unwrap();
    for cap in latex_regex.captures_iter(field_content) {
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