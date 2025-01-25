use crate::database;
use crate::human_hash::humanize;
use tokio_postgres::types::ToSql;
use uuid::Uuid;



use crate::structs::*;
use crate::notetypes::*;
use crate::media::*;
use crate::suggestion::*;

use std::collections::HashMap;

use database::SharedConn;

use async_recursion::async_recursion;

pub async fn unpack_notes(client: &mut SharedConn, notes: Vec<&Note>, notetype_map: &HashMap<String, String>, opt_deck: Option<i64>, reviewed: bool, req_ip: &str, commit: i32) -> std::result::Result<String, Box<dyn std::error::Error>> {
    // Cache protected fields for all the notetypes used by the notes
    let deck = opt_deck.ok_or("Deck ID is None")?;
    let deck_owner_q = client.query("SELECT owner from decks where id = $1", &[&deck]).await?;
    let deck_owner: i32 = deck_owner_q[0].get(0);

    let mut protected_fields_cache: HashMap<String, Vec<u32>> = HashMap::new();
    for note in &notes {
        let notetype_guid = match notetype_map.get(&note.note_model_uuid) {
            Some(guid) => guid,
            None => return Err(format!("Note type not found in cache: {}", note.note_model_uuid).into()),
        };
        if !protected_fields_cache.contains_key(notetype_guid) {
            let protected_fields = client
                .query(
                    "SELECT position FROM notetype_field WHERE notetype = (SELECT id FROM notetype WHERE guid = $1 AND owner = $2) AND protected = true",
                    &[&notetype_guid, &deck_owner],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            protected_fields_cache.insert(notetype_guid.clone(), protected_fields);
        }
    }

    let batch = notes.len() > 5000;
    let batch_size = 250;
    let notes_batches = notes.chunks(batch_size);

    if batch {
        client.batch_execute("
            ALTER TABLE notes DISABLE TRIGGER ALL;
            ALTER TABLE fields DISABLE TRIGGER ALL;
            ALTER TABLE tags DISABLE TRIGGER ALL;
        ").await?;
    }

    let _empty_string = String::new();

    for batch in notes_batches {
        let tx = client.transaction().await?;

        let insert_note_stmt = tx.prepare("
            INSERT INTO notes (guid, notetype, deck, last_update, reviewed, creator_ip)
            SELECT $1, nt.id, $2, NOW(), $3, $4
            FROM notetype nt
            WHERE nt.guid = $5 AND nt.owner = $6
            RETURNING id
        ").await?;

        let max_field_stmt = tx.prepare("SELECT MAX(position) FROM notetype_field WHERE notetype = (SELECT id FROM notetype WHERE guid = $1 AND owner = $2)",).await?;

        for note in batch {
            let notetype_guid = match notetype_map.get(&note.note_model_uuid) {
                Some(guid) => guid,
                None => return Err(format!("Note type not found in cache: {}", note.note_model_uuid).into()),
            };
            let rows = tx.query(&insert_note_stmt, &[
                &note.guid,
                &deck,
                &reviewed,
                &req_ip,
                &notetype_guid,
                &deck_owner,
            ]).await?;

            if rows.is_empty() {
                return Err(format!("Error inserting note {}: notetype does not exist", &note.guid).into());
            }
            let id:i64 = rows[0].get(0);

            // get protected fields from notetype
            let protected_fields = protected_fields_cache
                .get(notetype_guid)
                .ok_or_else(|| format!("Notetype {} not found in cache", notetype_guid))?;

            
            let mut field_values: Vec<&(dyn ToSql + Sync)> = Vec::new();
            let mut tag_values: Vec<&(dyn ToSql + Sync)> = Vec::new();

            let mut tag_query = String::from("INSERT INTO tags (note, content, reviewed, creator_ip, action, commit) VALUES ");    
            let mut field_query = String::from("INSERT INTO fields (note, position, content, reviewed, creator_ip, commit) VALUES ");  

            let deck_id = get_topmost_deck_by_note_id(&tx, id).await?;

            let max_allowed_position: u32 = tx.query(&max_field_stmt, &[&notetype_guid, &deck_owner]).await?[0].get(0);
            // Truncate the fields list to match the allowed position
            // This is done to make sure the fields conform to the notetype. A mismatch between the notetype and the fields should not be allowed
            // It shouldn't really ever happen, but I have seen it happen in the wild (5 times in 150k notes). Not sure how users are doing it.
            let truncated_fields = if note.fields.len() > max_allowed_position as usize + 1 {
                &note.fields[0..(max_allowed_position as usize + 1)]
            } else {
                &note.fields
            };

            let indices: Vec<u32> = (0..=max_allowed_position).collect(); // Ugly, but done to avoid the temporary variable issue. #fixlater

            // i = position of the field
            // n = parameter counter for the bulk insert
            // should fix this and make it more readable later. 
            let mut n = 0;
            for (i, field) in truncated_fields.iter().enumerate() {
                let content = if protected_fields.contains(&(i as u32)) || field.is_empty() {
                    continue;
                } else {
                    field
                };

                field_query.push_str(&format!("(${}, ${}, ${}, ${}, ${}, ${}),", 6*n+1, 6*n+2, 6*n+3, 6*n+4, 6*n+5, 6*n+6));

                field_values.push(&id);
                field_values.push(&(indices[i]));
                field_values.push(content);
                field_values.push(&reviewed);
                field_values.push(&req_ip);
                field_values.push(&commit);
                n += 1;
            }

            for (i, tag) in note.tags.iter().enumerate() {
                if tag.starts_with("AnkiCollab_Optional::") && !is_valid_optional_tag(&tx, &deck_id, tag).await?  {
                    continue; // Invalid Optional tag!
                }

                tag_query.push_str(&format!("(${}, ${}, ${}, ${}, true, ${}),", 5*i+1, 5*i+2, 5*i+3, 5*i+4, 5*i+5));

                tag_values.push(&id);
                tag_values.push(tag);
                tag_values.push(&reviewed);
                tag_values.push(&req_ip);
                tag_values.push(&commit);
            }

            // Remove the trailing comma
            tag_query.pop();
            field_query.pop();

            if !truncated_fields.is_empty() && !field_values.is_empty() {
                let stmt2 = tx.prepare(field_query.as_str()).await?;
                tx.execute(&stmt2, &field_values[..]).await?;
            }           

            if reviewed {
                update_note_timestamp(&tx, id).await?; 
            }

            if !note.tags.is_empty() && !tag_values.is_empty() { // Ill-formed SQL query if no tags. Shouldn't be necessary to do this check on fields because it's not possible to have a note with no fields (somehow the users make it possible tho)
                let stmt = tx.prepare(tag_query.as_str()).await?;
                tx.execute(&stmt, &tag_values[..]).await?;
            }
        }
        
        tx.commit().await?;
    } 

    if batch {
        client.batch_execute("
            ALTER TABLE notes ENABLE TRIGGER ALL;
            ALTER TABLE fields ENABLE TRIGGER ALL;
            ALTER TABLE tags ENABLE TRIGGER ALL;
        ").await?;
    }

    Ok("Success".to_string())
}

pub async fn handle_notes_and_media_update(
    client: &mut SharedConn,
    deck: &AnkiDeck,
    cache: &mut HashMap<String, String>,
    req_ip: &str,
    deck_id: Option<i64>,
    approved: bool,
    commit: i32,
    deck_tree: &Vec<i64>
)
-> std::result::Result<(), Box<dyn std::error::Error>> {
        
        // fix me later
    if deck_id.is_none() {
        return Err("Deck ID is None".into());
    }
    let safe_deck_id = deck_id.ok_or("Deck ID is None")?; // This is dumb af. idek why I used to keep all that shit in Option<>. There is no scenario in which it should be allowed to have a none deckid.. but who is going to rewrite all that code?

    if let Some(nt) = &deck.note_models {
        for n in nt {
            if let Some(_guid) = cache.get(&n.crowdanki_uuid) {
                continue;
            }
        
            let guid = unpack_notetype(client, n, deck_id).await?;
        
            cache.insert(n.crowdanki_uuid.to_owned(), guid.to_owned());
        }
    }

    let guids: Vec<&str> = deck.notes.iter().map(|note| note.guid.as_str()).collect();
    let note_query = client.prepare("
        SELECT n.id, n.guid, n.deck
        FROM notes n
        WHERE n.deck = ANY($1) AND n.guid = ANY($2)
    ").await?;

    // cba to create a struct for this. 0 is the note id, 1 is the deck id where the note is currently stored
    let existing_notes: HashMap<String, (i64, i64)> = client.query(&note_query, &[&deck_tree, &guids])
        .await?
        .into_iter()
        .map(|row| (
            row.get::<_, String>("guid"),
            (row.get::<_, i64>("id"), row.get::<_, i64>("deck"))
        ))
        .collect();

    if existing_notes.is_empty() {
        // If there are no existing notes, unpack all notes directly
        unpack_notes(client, deck.notes.iter().collect(), cache, deck_id, approved, req_ip, commit).await?;
    } else {
        let mut new_notes = Vec::new();
    
        for note in &deck.notes {
            match existing_notes.get(&note.guid) {
                None => new_notes.push(note),
                Some(&val) => {
                    if approved {
                        overwrite_note(client, note, val.0, req_ip, commit, safe_deck_id, val.1).await?;
                    } else {
                        update_note(client, note, val.0, req_ip, commit, safe_deck_id, val.1).await?;
                    }
                }
            }
        }
    
        if !new_notes.is_empty() {
            unpack_notes(client, new_notes, cache, deck_id, approved, req_ip, commit).await?;
        }
    }

    unpack_media(client, &deck.media_files, deck_id).await?;

    Ok(())
}

#[async_recursion]
async fn unpack_deck_data(
    client: &mut SharedConn,
    deck: &AnkiDeck,
    notetype_cache: &mut HashMap<String, String>,
    owner: i32,
    req_ip: &String,
    deck_id: Option<i64>,
    approved: bool,
    commit: i32,
    deck_tree: &Vec<i64>,
) -> std::result::Result<String, Box<dyn std::error::Error>> {
    
    handle_notes_and_media_update(client, deck, notetype_cache, req_ip, deck_id, approved, commit, deck_tree).await?;

    for child in &deck.children {
        unpack_deck_json(client, child, notetype_cache, owner, req_ip, deck_id, approved, commit, deck_tree).await?;
    }
    
    Ok("Success".into())
}

pub async fn check_deck_exists(
    client: &SharedConn,
    deck_name: &String,
    deck_uuid: &str,
    owner: i32,
    parent: Option<i64>,
) -> Result<String, Box<dyn std::error::Error>> {
    let my_uuid = Uuid::parse_str(deck_uuid)?;
    let hum = humanize(&my_uuid, 5);

    let deck_exist_check = {
        client.query("
            SELECT 1 FROM decks WHERE (name = $1 AND owner = $2 AND full_path = (coalesce( (SELECT full_path FROM decks WHERE id = $3 ) || '::' , '') || $1)) OR human_hash = $4
            ", &[&deck_name, &owner, &parent, &hum]).await?
    };

    match deck_exist_check.first() {
        None => Ok(hum),
        Some(_row) => Err(format!("Deck {} already exists. Please submit suggestions instead.", &deck_name).into()),
    }
}

#[async_recursion]
pub async fn unpack_deck_json(
    client: &mut SharedConn,
    deck: &AnkiDeck,
    notetype_cache: &mut HashMap<String, String>,
    owner: i32,
    req_ip: &String,
    parent: Option<i64>,
    approved: bool,
    commit: i32,
    deck_tree: &Vec<i64>,
) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let hum = check_deck_exists(client, &deck.name, &deck.crowdanki_uuid, owner, parent).await?;

    let stmt = client.prepare("
        INSERT INTO decks (name, description, owner, last_update, parent, crowdanki_uuid, human_hash, creator_ip, full_path)
        VALUES ($1, $2, $3, NOW(), $4, $5, $6, $7, 
        coalesce( (SELECT full_path FROM decks WHERE id = $4 ) || '::' , '') || $1
        )
        RETURNING id
    ").await?;

    let rows = client.query(&stmt, &[
        &deck.name,
        &deck.desc,
        &owner,
        &parent,
        &deck.crowdanki_uuid,
        &hum,
        &req_ip,
    ]).await?;

    let id:Option<i64> = rows.first().map(|row| row.get(0));

    if id.is_none() {
        return Err("Deck already exists. Please suggests cards instead (2).".into());
    }

    client.query("UPDATE commits SET deck = $1 WHERE commit_id = $2 AND deck is NULL", &[&id, &commit]).await?;


    match unpack_deck_data(client, deck, notetype_cache, owner, req_ip, id, approved, commit, deck_tree).await {
        Ok(_res) => { },
        Err(error) => { println!("Error: {}", error) },
    }

    Ok(hum)
}
