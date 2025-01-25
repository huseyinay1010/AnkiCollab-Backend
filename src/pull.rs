use std::sync::Arc;
use std::fmt::Write;

use crate::database;
use crate::media::*;
use crate::notetypes::*;
use crate::structs::*;
use async_recursion::async_recursion;
use std::collections::HashMap;

use database::SharedConn;

#[async_recursion]
async fn fill_deck(
    client: &SharedConn,
    parent: i64,
    timestamp: &str,
) -> Result<Vec<AnkiDeck>, Box<dyn std::error::Error>> {
    let stmt = client
        .prepare("
            SELECT id, name, description, crowdanki_uuid, owner FROM decks 
            WHERE parent = $1 AND last_update > to_timestamp( $2 , 'YYYY-MM-DD hh24:mi:ss')::timestamptz")
        .await?;

    let rows = client.query(&stmt, &[&parent, &timestamp]).await?;
    let mut decks = Vec::with_capacity(rows.len());

    for row in rows {
        let id: i64 = row.get(0);
        let name: String = row.get(1);
        let description: String = row.get(2);
        let crowdanki_uuid: String = row.get(3);
        let owner: i32 = row.get(4);

        let note_vec = (get_changed_notes(client, id, timestamp).await).unwrap_or_default();
        let children = fill_deck(client, id, timestamp).await?;
        let notetypes = get_notetypes(client, &note_vec, owner).await;
        let media = get_media_files(client, id).await?;

        decks.push(AnkiDeck {
            crowdanki_uuid,
            children,
            desc: description,
            name,
            note_models: Some(notetypes),
            notes: note_vec,
            media_files: media,
        });
    }

    Ok(decks)
}

#[async_recursion]
async fn discover_deleted_notes(
    client: &SharedConn,
    deck_id: i64,
    deleted_notes: &mut Vec<String>,
    timestamp: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let deleted_notes_query = client.prepare("
        SELECT guid
        FROM notes
        WHERE deck = $1 AND deleted = true AND last_update > to_timestamp($2, 'YYYY-MM-DD hh24:mi:ss')::timestamptz
    ").await?;

    let deleted_notes_rows = client
        .query(&deleted_notes_query, &[&deck_id, &timestamp])
        .await?;

    for deleted_notes_row in deleted_notes_rows {
        let guid: String = deleted_notes_row.get("guid");
        deleted_notes.push(guid);
    }

    let decks_query = client
        .prepare(
            "
        SELECT id
        FROM decks
        WHERE parent = $1 AND last_update > to_timestamp( $2 , 'YYYY-MM-DD hh24:mi:ss')::timestamptz
    ",
        )
        .await?;

    let decks_rows = client.query(&decks_query, &[&deck_id, &timestamp]).await?;

    for deck_row in decks_rows {
        let child_deck_id: i64 = deck_row.get("id");
        discover_deleted_notes(client, child_deck_id, deleted_notes, timestamp).await?;
    }

    Ok(())
}

async fn get_changed_notes(
    client: &SharedConn,
    deck_id: i64,
    input_date: &str,
) -> Result<Vec<Note>, Box<dyn std::error::Error>> {
    let mut notes = Vec::new();
    let mut current_cursor:Option<i64> = None;

    // 1000 limit idk just a random number i picked, no research was done tbbh
    let notes_query = client
        .prepare(
        "
            SELECT n.id, n.guid, nt.guid, nt.id
            FROM notes AS n
            LEFT JOIN notetype AS nt ON n.notetype = nt.id
            WHERE n.deck = $1 AND n.reviewed = true AND n.deleted = false
            AND n.last_update > to_timestamp($2, 'YYYY-MM-DD hh24:mi:ss')::timestamptz
            AND ($3::bigint IS NULL OR n.id > $3)
            ORDER BY n.id
            LIMIT 1000
        ",
        )
        .await?;

    let fields_query = client
        .prepare(
        "
            SELECT note, content, position
            FROM fields
            WHERE note = ANY($1) AND reviewed = true AND content <> ''
            ORDER BY note, position
        ",
        )
        .await?;

    let tags_query = client
        .prepare(
        "
            SELECT note, content
            FROM tags
            WHERE note = ANY($1) AND reviewed = true
        ",
        )
        .await?;

    let notetype_size_query = client
        .prepare("SELECT count(1)::int from notetype_field where notetype = $1")
        .await?;

    let mut notetype_sizes: HashMap<i64, i32> = HashMap::new();

    loop {
        let notes_batch = client
            .query(
                &notes_query,
                &[
                    &deck_id,
                    &input_date,
                    &current_cursor,
                ],
            )
            .await?;

        if notes_batch.is_empty() {
            break;
        }

        let note_ids: Vec<i64> = notes_batch.iter().map(|row| row.get(0)).collect();

        // Fetch fields and tags for the entire batch
        let fields_rows = client.query(&fields_query, &[&note_ids]).await?;
        let tags_rows = client.query(&tags_query, &[&note_ids]).await?;

        // Process fields
        let mut fields_map: HashMap<i64, Vec<(String, u32)>> = HashMap::new();
        for row in fields_rows {
            let note_id: i64 = row.get(0);
            let content: String = row.get(1);
            let position: u32 = row.get(2);
            fields_map
                .entry(note_id)
                .or_default()
                .push((content, position));
        }

        // Process tags
        let mut tags_map: HashMap<i64, Vec<String>> = HashMap::new();
        for row in tags_rows {
            let note_id: i64 = row.get(0);
            let content: String = row.get(1);
            tags_map
                .entry(note_id)
                .or_default()
                .push(content);
        }

        // Process notes
        for note_row in &notes_batch {
            let note_id: i64 = note_row.get(0);
            let guid: String = note_row.get(1);
            let note_model_uuid: String = note_row.get(2);
            let note_model_id: i64 = note_row.get(3);

            let notetype_size = match notetype_sizes.get(&note_model_id) {
                Some(&size) => size,
                None => {
                    let notetype_size_rows = client
                        .query(&notetype_size_query, &[&note_model_id])
                        .await?;
                    let notetype_size: i32 = notetype_size_rows[0].get(0);
                    notetype_sizes.insert(note_model_id, notetype_size);
                    notetype_size
                }
            };

            let mut fields = vec![String::new(); notetype_size as usize];
            if let Some(note_fields) = fields_map.get(&note_id) {
                for (content, position) in note_fields {
                    if (*position as usize) < fields.len() {
                        fields[*position as usize] = content.to_string();
                    } else {
                        println!(
                            "Invalid field position: {}; note_id: {}; model id: {}",
                            position, note_id, note_model_id
                        );
                    }
                }
            } else {
                println!("No fields found for note: {}", note_id);
                continue;
            }

            let tags = tags_map.get(&note_id).cloned().unwrap_or_default();

            notes.push(Note {
                fields,
                guid,
                note_model_uuid,
                tags,
            });
        }

        current_cursor = notes_batch.last().map(|row| row.get::<_, i64>(0));
    }

    Ok(notes)
}

pub async fn pull_changes(
    db_state: &Arc<database::AppState>,
    deck_hash: &String,
    timestamp: &String,
) -> std::result::Result<UpdateInfoResponse, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get_owned().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        },
    };

    let stmt = client.prepare("
        SELECT id, name, description, crowdanki_uuid, owner, stats_enabled
        FROM decks
        WHERE human_hash = $1 AND last_update > to_timestamp( $2 , 'YYYY-MM-DD hh24:mi:ss')::timestamptz
    ").await?;

    let nt = pull_protected_fields(&client, deck_hash).await?;
    let rows = client.query(&stmt, &[&deck_hash, &timestamp]).await?;
    if let Some(row) = rows.first() {
        let id: i64 = row.get("id");
        let note_vec = get_changed_notes(&client, id, timestamp).await?;
        let name: String = row.get("name");
        let description: String = row.get("description");
        let crowdanki_uuid: String = row.get("crowdanki_uuid");
        let stats_enabled: bool = row.get("stats_enabled");
        let note_models = Some(get_notetypes(&client, &note_vec, row.get("owner")).await);
        let media = get_media_files(&client, id).await?;
        let children = fill_deck(&client, id, timestamp).await?;
        let optional_tags = get_optional_tag_groups(&client, id).await?;

        let daddy = AnkiDeck {
            crowdanki_uuid,
            children,
            desc: description,
            name,
            note_models,
            notes: note_vec,
            media_files: media,
        };

        let mut gdrive: GDriveInfo = Default::default();

        let query_gdrive = client
            .query(
                "select google_data, folder_id FROM service_accounts WHERE deck = $1 LIMIT 1",
                &[&id],
            )
            .await?;
        if let Some(row) = query_gdrive.first() {
            let cred_json: serde_json::Value = row.get("google_data");
            gdrive.service_account = serde_json::from_value(cred_json)?;
            gdrive.folder_id = row.get("folder_id");
        }

        let mut deleted_notes = Vec::new();
        discover_deleted_notes(&client, id, &mut deleted_notes, timestamp).await?;

        let res = UpdateInfoResponse {
            gdrive,
            protected_fields: nt,
            deck: daddy,
            changelog: get_changelog_info(&client, id, timestamp)
                .await
                .expect("Failed to retrieve changelog info"),
            deck_hash: deck_hash.to_string(),
            optional_tags,
            deleted_notes,
            stats_enabled,
        };
        Ok(res)
    } else {
        // Return error
        Err("No deck found".into())
    }
}

pub async fn get_changelog_info(
    client: &SharedConn,
    deck_id: i64,
    last_timestamp: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let rows = client.query(
        "SELECT message, TO_CHAR(timestamp, 'MM/DD/YYYY HH24:MI') AS timestamp FROM changelogs WHERE deck = $1 AND timestamp > to_timestamp($2, 'YYYY-MM-DD hh24:mi:ss')::timestamptz ORDER BY timestamp DESC",
        &[&deck_id, &last_timestamp],
    ).await?;

    let changelog_string = rows.iter()
        .fold(String::new(), |mut acc, row| {
            let message: String = row.get(0);
            let timestamp: String = row.get(1);
            write!(acc, "--- Changes from {}: ---\n{}\n\n", timestamp, message).unwrap();
            acc
        });

    Ok(changelog_string)
}
async fn get_optional_tag_groups(
    client: &SharedConn,
    deck: i64,
) -> std::result::Result<Vec<String>, Box<dyn std::error::Error>> {
    let rows = client
        .query(
            "SELECT tag_group FROM optional_tags WHERE deck = $1",
            &[&deck],
        )
        .await?
        .into_iter()
        .map(|row| row.get::<_, String>("tag_group"))
        .collect::<Vec<String>>();

    Ok(rows)
}

pub async fn get_id_from_username(
    db_state: &Arc<database::AppState>,
    username: String,
) -> std::result::Result<i32, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        }
    };
    let normalized_username = username.to_lowercase();
    let rows = client
        .query("SELECT id FROM users WHERE username = $1", &[&normalized_username])
        .await?;

    if rows.is_empty() {
        return Err("User doesnt exist".to_string().into());
    }

    Ok(rows[0].get(0))
}

pub async fn get_deck_last_update_unix(
    db_state: &Arc<database::AppState>,
    deck_hash: &str,
) -> Result<f64, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        }
    };

    let result = client.query_opt(
        "SELECT CAST(EXTRACT(EPOCH FROM last_update) AS numeric)::text FROM decks WHERE human_hash = $1",
        &[&deck_hash],
    ).await?;
    let result = result.map_or(0.0, |row| {
        row.get::<usize, String>(0).parse().unwrap_or(0.0)
    });
    Ok(result)
}

pub async fn check_deck_alive(
    db_state: &Arc<database::AppState>,
    deck_hashes: Vec<String>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        }
    };

    let stmt = client
        .prepare(
            "
        SELECT human_hash
        FROM decks
        WHERE human_hash = ANY($1)
    ",
        )
        .await?;

    let rows = client.query(&stmt, &[&deck_hashes]).await?;
    let existing_hashes: Vec<String> = rows.iter().map(|row| row.get("human_hash")).collect();
    let missing_hashes: Vec<String> = deck_hashes
        .into_iter()
        .filter(|hash| !existing_hashes.contains(hash))
        .collect();

    Ok(missing_hashes)
}

pub async fn get_google_drive_data(
    db_state: &Arc<database::AppState>,
    deck_hash: String,
) -> Result<GDriveInfo, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        }
    };

    let mut gdrive: GDriveInfo = Default::default();

    let query_gdrive = client.query("select google_data, folder_id FROM service_accounts WHERE deck = (select id from decks where human_hash = $1)", &[&deck_hash]).await?;
    if let Some(row) = query_gdrive.first() {
        let cred_json: serde_json::Value = row.get("google_data");
        gdrive.service_account = serde_json::from_value(cred_json)?;
        gdrive.folder_id = row.get("folder_id");
    }
    Ok(gdrive)
}
