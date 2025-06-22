
use std::collections::HashMap;

use crate::database;
use crate::media_reference_manager::update_media_references_for_note;
use crate::push;
use crate::structs::{AnkiDeck, Note};
use crate::notetypes::does_notetype_exist;
use crate::cleanser;

use async_recursion::async_recursion;
use regex::Regex;
use std::sync::Arc;

use bb8_postgres::bb8::PooledConnection;
use bb8_postgres::PostgresConnectionManager;
type SharedConn = PooledConnection<'static, PostgresConnectionManager<tokio_postgres::NoTls>>;

/*
type DeckTreeCache = Arc<Mutex<Vec<i32>>>;

async fn precompute_deck_tree(client: &mut SharedConn, deck_id: i64, cache: DeckTreeCache) -> Result<(), Box<dyn std::error::Error>> {
    let mut deck_tree = Vec::new();
    let mut to_visit = vec![deck_id];

    while let Some(current_deck_id) = to_visit.pop() {
        deck_tree.push(current_deck_id);

        // Find parent
        if let Some(row) = client.query_opt("SELECT parent FROM decks WHERE id = $1", &[&current_deck_id]).await? {
            if let Some(parent_id) = row.get::<_, Option<i64>>(0) {
                if !deck_tree.contains(&parent_id) {
                    to_visit.push(parent_id);
                }
            }
        }

        // Find children
        let rows = client.query("SELECT id FROM decks WHERE parent = $1", &[&current_deck_id]).await?;
        for row in rows {
            let child_deck_id: i64 = row.get(0);
            if !deck_tree.contains(&child_deck_id) {
                to_visit.push(child_deck_id);
            }
        }
    }

    // Cache the deck tree
    let mut cache_lock = cache.lock().await;
    *cache_lock = deck_tree;

    Ok(())
}

*/

pub async fn update_note_timestamp(tx: &tokio_postgres::Transaction<'_>, note_id: i64)  -> Result<(), Box<dyn std::error::Error>> { 
    let query1 = "
    WITH RECURSIVE tree AS (
        SELECT id, last_update, parent FROM decks
        WHERE id = (SELECT deck FROM notes WHERE id = $1)
        UNION ALL
        SELECT d.id, d.last_update, d.parent FROM decks d
        JOIN tree t ON d.id = t.parent
    )
    UPDATE decks
    SET last_update = NOW()
    WHERE id IN (SELECT id FROM tree)";

    let query2 = "UPDATE notes SET last_update = NOW() WHERE id = $1";

    tx.query(query1, &[&note_id]).await?;
    tx.query(query2, &[&note_id]).await?;

    Ok(())
}

pub async fn is_valid_optional_tag(tx: &tokio_postgres::Transaction<'_>, deck: &i64, tag: &str) -> std::result::Result<bool, Box<dyn std::error::Error>> {
    let re = Regex::new(r"AnkiCollab_Optional::(?P<tag_group>[^:]+)(::(?P<subtag>[^:]+))?")?;
    if let Some(caps) = re.captures(tag) {
        let tag_group = match caps.name("tag_group") {
            Some(t) => t.as_str(),
            None => return Ok(false), 
        };

        //let subtag = caps.name("subtag"); Unused atm

        let rows = tx.query("SELECT id FROM optional_tags WHERE deck = $1 AND tag_group = $2", &[&deck, &tag_group]).await?;

        Ok(!rows.is_empty())
    } else {
        Ok(false)
    }
}

pub async fn get_topmost_deck_by_note_id(tx: &tokio_postgres::Transaction<'_> ,note_id: i64) -> std::result::Result<i64, Box<dyn std::error::Error>> {
    let rows = tx.query("
        WITH RECURSIVE deck_hierarchy AS (
            SELECT decks.id, parent
            FROM decks
            JOIN notes ON decks.id = notes.deck
            WHERE notes.id = $1
            UNION
            SELECT decks.id, decks.parent
            FROM decks
            JOIN deck_hierarchy ON deck_hierarchy.parent = decks.id
        )
        SELECT id
        FROM deck_hierarchy
        WHERE parent IS NULL      
    ", &[&note_id]).await?;

    if rows.is_empty() {
        return Err("No deck found for note".into());
    }

    Ok(rows[0].get(0))
}

async fn force_overwrite_tag(client: &mut SharedConn, note_id: i64, new_content: &[String], req_ip: &str, commit: i32, reviewed: bool) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let tx = client.transaction().await?;

    let query = "SELECT content from tags where note = $1 and reviewed = $2";
    let old_tags = tx.query(query, &[&note_id, &reviewed])
    .await?
    .into_iter()
    .map(|row| row.get::<_, String>("content"))
    .collect::<Vec<String>>();

    // HashSet could be faster in theory, but since our vecs are expected to be very small (< 100 tags on average) this is probably more performant
    let new_tags: Vec<_> = new_content.iter().filter(|item| !old_tags.contains(item)).collect();
    let removed_tags: Vec<_> = old_tags.into_iter().filter(|item| !new_content.contains(item)).collect();


    let insert_new_tag = tx.prepare("
        INSERT INTO tags (note, content, reviewed, creator_ip, action, commit) VALUES ($1, $2, $3, $4, true, $5)
    ").await?;

    let remove_new_tag = tx.prepare("
        DELETE FROM tags WHERE note = $1 and content = $2
    ").await?;

    let deck_id = get_topmost_deck_by_note_id(&tx, note_id).await?;

    for tag in new_tags {
        if tag.starts_with("AnkiCollab_Optional::") && !is_valid_optional_tag(&tx, &deck_id, tag).await?  {
            continue; // Invalid Optional tag!
        }
        let con = cleanser::clean(tag);
        tx.execute(&insert_new_tag, &[&note_id, &con, &reviewed, &req_ip, &commit],).await?;        
    }
    
    for tag in removed_tags {
        let con = cleanser::clean(&tag);
        tx.execute(&remove_new_tag, &[&note_id, &con],).await?;        
    }

    tx.commit().await?;
    
    Ok(())
}

async fn check_tag(client: &mut SharedConn, note_id: i64, new_content: &[String], req_ip: &str, commit: i32) -> std::result::Result<(), Box<dyn std::error::Error>> {  
    let tx = client.transaction().await?;

    let query = "SELECT content from tags where note = $1 and reviewed = true";
    let old_tags = tx.query(query, &[&note_id])
    .await?
    .into_iter()
    .map(|row| row.get::<_, String>("content"))
    .collect::<Vec<String>>();

    // HashSet could be faster in theory, but since our vecs are expected to be very small (< 100 tags on average) this is probably more performant
    let new_tags: Vec<_> = new_content.iter().filter(|item| !old_tags.contains(item)).collect();
    let removed_tags: Vec<_> = old_tags.into_iter().filter(|item| !new_content.contains(item)).collect();


    let insert_new_tag = tx.prepare("
        INSERT INTO tags (note, content, reviewed, creator_ip, action, commit) VALUES ($1, $2, false, $3, true, $4)
    ").await?;

    let remove_new_tag = tx.prepare("
        INSERT INTO tags (note, content, reviewed, creator_ip, action, commit) VALUES ($1, $2, false, $3, false, $4)
    ").await?;

    let deck_id = get_topmost_deck_by_note_id(&tx, note_id).await?;

    for tag in new_tags {
        if tag.starts_with("AnkiCollab_Optional::") && !is_valid_optional_tag(&tx, &deck_id, tag).await?  {
            continue; // Invalid Optional tag!
        }
        let con = cleanser::clean(tag);
        tx.execute(&insert_new_tag, &[&note_id, &con, &req_ip, &commit],).await?;        
    }
    
    for tag in removed_tags {
        let con = cleanser::clean(&tag);
        tx.execute(&remove_new_tag, &[&note_id, &con, &req_ip, &commit],).await?;        
    }

    tx.commit().await?;
    
    Ok(())
}

pub async fn overwrite_note(
    client: &mut SharedConn, 
    note: &Note, 
    n_id: i64, 
    req_ip: &str, 
    commit: i32,
    new_deck_id: i64,
    old_deck_id: i64,
) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let n_r_q = client.query("SELECT 1 FROM notes where id = $1 and deleted = false", &[&n_id],).await?;
    if n_r_q.is_empty() { 
        return Err("note not found or deleted".into()); // Because its been marked as deleted
    }

    let tx = client.transaction().await?;
    
    // I added the order, to make sure to only overwrite the reviewed field and not the unreviewed one (if there is one)
    let upsert_field_q = tx.prepare("
        INSERT INTO fields (note, position, content, creator_ip, commit, reviewed)
        SELECT $1, $2, $3, $4, $5, true
        FROM notetype_field
        WHERE notetype = (SELECT notetype FROM notes WHERE id = $1) AND position = $2 AND protected = false
        AND NOT EXISTS (
            SELECT 1
            FROM fields
            WHERE note = $1 AND position = $2 AND content = $3
        )
        ON CONFLICT (note, position) WHERE reviewed=true DO UPDATE
        SET content = $3, creator_ip = $4
        WHERE fields.note = $1
        AND fields.position = $2
        AND fields.content <> $3
        AND fields.commit <> $5
    ").await?;

    let delete_field_q = tx.prepare("
        DELETE FROM fields WHERE note = $1 AND position = $2 AND reviewed = true
    ").await?;

    // Check fields for changes
    for (i, field) in note.fields.iter().enumerate().map(|(i, field)| (i as u32, field)) {
        if field.is_empty() {
            tx.execute(&delete_field_q, &[&n_id, &i],).await?;        
        } else {            
            let content = cleanser::clean(field);
            tx.execute(&upsert_field_q, &[&n_id, &i, &content, &req_ip, &commit],).await?;        
        }
    }

    // Update note location if necessary
    if new_deck_id != old_deck_id {
        tx.execute("UPDATE notes SET deck = $1 WHERE id = $2", &[&new_deck_id, &n_id],).await?;
    }
    
    update_note_timestamp(&tx, n_id).await?;

    tx.commit().await?;

    // update media references
    update_media_references_for_note(client, n_id).await?;
    
    // force tag changes
    force_overwrite_tag(client, n_id, &note.tags, req_ip, commit, true).await?;

    Ok(format!("Updating the existing card {n_id:?} with the new information!"))
}

pub async fn update_note(
    client: &mut SharedConn,
    note: &Note,
    n_id: i64,
    req_ip: &str,
    commit: i32,
    new_deck_id: i64,
    old_deck_id: i64,
) -> std::result::Result<String, Box<dyn std::error::Error>> {    
    let n_r_q = client.query("SELECT reviewed FROM notes where id = $1 and deleted = false", &[&n_id],).await?;
    let note_reviewed = if n_r_q.is_empty() { 
        return Err("note not found or deleted".into()); // Because its been marked as deleted
    } else {
        n_r_q[0].get(0)
    };

    let tx = client.transaction().await?;

    let add_field_q = tx.prepare("
        INSERT INTO fields (note, position, content, creator_ip, commit)
        SELECT $1, $2, $3, $4, $5
        FROM notetype_field
        WHERE notetype = (SELECT notetype FROM notes WHERE id = $1) AND position = $2 AND protected = false
        AND NOT EXISTS (
            SELECT 1
            FROM fields
            WHERE note = $1 AND position = $2 AND content = $3
        )
        LIMIT 1
    ").await?;

    if note_reviewed {    
        for (i, field) in note.fields.iter().enumerate().map(|(i, field)| (i as u32, field)) {
            if !field.is_empty() || !tx.query("SELECT 1 FROM fields WHERE note = $1 AND position = $2", &[&n_id, &i]).await?.is_empty() {
                let content = cleanser::clean(field);
                tx.execute(&add_field_q, &[&n_id, &i, &content, &req_ip, &commit],).await?;
            }
        }
    } else {
        let does_field_exist = tx.prepare("SELECT 1 FROM fields WHERE note = $1 AND position = $2").await?;

        let update_field_q = tx.prepare("
            UPDATE fields SET content = $3, creator_ip = $4 WHERE note = $1 AND position = $2 AND content <> $3 and commit <> $5
        ").await?;

        let delete_field_q = tx.prepare("DELETE FROM fields WHERE note = $1 AND position = $2 AND reviewed = false").await?;

        // Check if the field exists, if it does, update it, else insert it. If the new content is empty, don't insert but delete it instead
        for (i, field) in note.fields.iter().enumerate().map(|(i, field)| (i as u32, field)) {
            if field.is_empty() {
                tx.execute(&delete_field_q, &[&n_id, &i],).await?;        
            } else {
                let rows = tx.query(&does_field_exist, &[&n_id, &i]).await?;
                let content = cleanser::clean(field);
                if rows.is_empty() {
                    tx.execute(&add_field_q, &[&n_id, &i, &content, &req_ip, &commit],).await?;
                } else {
                    tx.execute(&update_field_q, &[&n_id, &i, &content, &req_ip, &commit],).await?;
                }
            }
        }
    }

    tx.commit().await?;

    if note_reviewed {
        // Check tags for changes
        check_tag(client, n_id, &note.tags, req_ip, commit).await?;
    } else {// force tag changes, if the note is unreviewed but keep the old commit id so all changes are kept in the same commit        
        // Gamble that there is only one commit per unreviewed note. should be the case beecause who other than the creator would change it?
        let get_old_commit = client.query("SELECT commit FROM tags WHERE note = $1 AND reviewed = false ORDER BY commit ASC LIMIT 1", &[&n_id],).await?;
        let old_commit_id = if get_old_commit.is_empty() { 
            commit // No idea how this could happen, so we just use the current one as a fallback
        } else {
            get_old_commit[0].get(0)
        };
        force_overwrite_tag(client, n_id, &note.tags, req_ip, old_commit_id, false).await?;
    }

    if new_deck_id != old_deck_id {
        if note_reviewed { // Suggest move
            let tx = client.transaction().await?;
            let insert_q = tx.prepare(
                "INSERT INTO note_move_suggestions (original_deck, target_deck, note, creator_ip, commit) VALUES ($1, $2, $3, $4, $5)"
            ).await?;
            tx.execute(&insert_q, &[&old_deck_id, &new_deck_id, &n_id, &req_ip, &commit],).await?;
            tx.commit().await?;
        } else { // Force move
            // Update the deck of the note immediately because the note is in a pending state
            let tx = client.transaction().await?;
            let update_q = tx.prepare("UPDATE notes SET deck = $1 WHERE id = $2").await?;
            tx.execute(&update_q, &[&new_deck_id, &n_id],).await?;
            tx.commit().await?;
        }       
    }
    
    Ok(format!("Suggested the new information to the existing card {n_id:?}"))
}

async fn get_original_name(db_state: &Arc<database::AppState>, input_hash: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {err}");
            return Err("Failed to retrieve a pooled connection".into());
        },
    };

    let res = client.query("SELECT name FROM decks WHERE human_hash = $1 LIMIT 1", &[&input_hash],).await?;
    if res.is_empty() {
        Err("Deck not found".into())
    } else {
        Ok(res[0].get(0))
    }
}

async fn get_id_from_path(client: &SharedConn, input_hash: &str, deck_path: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let query = 
        "WITH RECURSIVE subdecks AS (
            SELECT id
            FROM decks
            WHERE human_hash = $1
            UNION ALL
            SELECT p.id
            FROM decks p
            JOIN subdecks s ON s.id = p.parent
        )
        SELECT d.id from decks d JOIN subdecks p on p.id = d.id
        WHERE d.full_path = $2 LIMIT 1";
    let result = client.query(query, &[&input_hash, &deck_path],).await?;

    if result.is_empty() { 
        Err("not found".into())
    } else {
        Ok(result[0].get(0))
    }
}
    
async fn try_suggest_note(
    client: &mut SharedConn,
    deck_hash: &String,
    notetype_cache: &mut HashMap<String, String>,
    deck_path: &String,
    deck_id: Option<i64>,
    deck: &AnkiDeck,
    req_ip: &String,
    commit: i32,
    force_overwrite: bool,
    deck_tree: &Vec<i64>,
) -> Result<String, Box<dyn std::error::Error>> {
    if deck_id.is_none() {
        return Err("Not found".into());
    }
    
    push::handle_notes_and_media_update(client, deck, notetype_cache, req_ip, deck_id, force_overwrite, commit, deck_tree).await?;

    for child in &deck.children {
        let child_path = format!("{}::{}", deck_path, child.name);
        let (san_deck_id, san_owner) = sanity_check(client, deck_hash, &child_path, commit).await?;
        make(client, deck_hash, notetype_cache, &child_path, child, req_ip, commit, force_overwrite, san_deck_id, san_owner, deck_tree).await?;
    }

    Ok("Success".to_string())
}
pub async fn remove_ankicollab_suffix(raw_deck_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    // Remove AnkiCollab suffix
    let re = Regex::new(r"\s?\(AnkiCollab\)(_\d+)?")?;
    let res = re.replace(raw_deck_path, "").into_owned();
    if res.is_empty() {
        return Err("Error occurred: Deckname is ill-formed.".into());
    }
    Ok(res)
}

#[async_recursion]
pub async fn update_deck_names(deck: &mut AnkiDeck) {
    deck.name = match remove_ankicollab_suffix(&deck.name).await {
        Ok(val) => val,
        Err(error) => {
            println!("Error occured in update_deck_names: {}, Deck name: {}", error, &deck.name);
            deck.name.clone()
        }
    };

    for child in &mut deck.children {
        update_deck_names(child).await;
    }
}

pub async fn fix_deck_name(db_state: &Arc<database::AppState>, raw_deck_path: &str, alternate_name: &str, deck_hash: &str) -> Result<String, Box<dyn std::error::Error>> {
    let res = remove_ankicollab_suffix(raw_deck_path).await?;
    // Replace the alternate name with the expected name    
    let original_name = get_original_name(db_state, deck_hash).await?;
    Ok(res.replacen(alternate_name, &original_name, 1))
}

pub async fn create_new_commit(db_state: &Arc<database::AppState>, rationale: i32, commit_text: &str, ip: &String, user_id: Option<i32>) -> Result<i32, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {err}");
            return Err("Failed to retrieve a pooled connection".into());
        },
    };
    let content = ammonia::clean(commit_text);
    let rows = client.query("INSERT INTO commits (rationale, info, ip, timestamp, user_id) VALUES ($1, $2, $3, NOW(), $4) RETURNING commit_id", &[&rationale, &content, &ip, &user_id]).await?;
    Ok(rows[0].get(0))
}

#[async_recursion]
pub async fn sanity_check_notetypes(
    client: &SharedConn, 
    cache: &mut HashMap<String, String>,
    deck_hash: &String,
    deck: &AnkiDeck
) -> Result<(), Box<dyn std::error::Error>> {    
    let deck_query = client.query("SELECT owner, restrict_notetypes from decks where human_hash = $1", &[&deck_hash]).await?;
    if deck_query.is_empty() {
        return Err("Deck does not exist".into());
    }
    let owner: i32 = deck_query[0].get(0);
    let restrict_notetypes: bool = deck_query[0].get(1);
    if !restrict_notetypes {
        return Ok(());
    }

    if let Some(nt) = &deck.note_models {
        for n in nt {
            if cache.contains_key(&n.crowdanki_uuid) {
                continue;
            }        
        
            let guid = does_notetype_exist(client, n, owner).await?;
            if guid.is_empty() {
                return Err(n.crowdanki_uuid.clone().into());
            }

            cache.insert(n.crowdanki_uuid.clone(), guid.clone());
        }
    }

    for child in &deck.children {
        sanity_check_notetypes(client, cache, deck_hash, child).await?;
    }
    
    Ok(())
}

pub async fn sanity_check(
    client: &SharedConn,
    deck_hash: &String,
    deck_path: &str,
    commit: i32
) -> Result<(Option<i64>, i32), Box<dyn std::error::Error>> {

    let mut deck_id = match get_id_from_path(client, deck_hash, deck_path).await {
        Ok(id) => Some(id),
        Err(_error) => None,
    };

    // Check if the parent exists and insert child if it does, else abort
    if deck_id.is_none() {
        let input_layers: Vec<&str> = deck_path.split("::").collect();
        let parent_path = input_layers[0..input_layers.len()-1].join("::");
        
        let owner = client.query("SELECT owner from decks where human_hash = $1", &[&deck_hash],).await?;
        if parent_path.is_empty() || owner.is_empty()  {
            return Err("Deck does not exist".into());
        } 

        match get_id_from_path(client, deck_hash, &parent_path).await {
            Ok(id) => { deck_id = Some(id) },
            Err(_error) => {
                return Err("Parent Deck does not exist".into())
            },
        };
        
        client.query("UPDATE commits SET deck = $1 WHERE commit_id = $2 AND deck is NULL", &[&deck_id, &commit]).await?;
        Ok((deck_id, owner[0].get(0)))
    } else {       
        client.query("UPDATE commits SET deck = $1 WHERE commit_id = $2 AND deck is NULL", &[&deck_id, &commit]).await?;
        Ok((deck_id, 0))
    }
}

#[async_recursion]
pub async fn make(
    client: &mut SharedConn,
    deck_hash: &String,
    notetype_cache: &mut HashMap<String, String>,
    deck_path: &String,
    deck: &AnkiDeck,
    req_ip: &String,
    commit: i32,
    force_overwrite: bool,
    deck_id: Option<i64>,
    owner: i32,
    deck_tree: &Vec<i64>,
) -> Result<String, Box<dyn std::error::Error>> {
    
    if deck_id.is_none() {
        return Err("Deck not found. Illegal code.".into());
    }
    // Check if the maintainer allows new subdecks to be created
    if owner != 0 {
        let deck_query = client.query("SELECT restrict_subdecks from decks where human_hash = $1", &[&deck_hash]).await?;
        if deck_query.is_empty() {
            return Err("Deck does not exist".into());
        }
        let restrict_subdecks: bool = deck_query[0].get(0);
        if restrict_subdecks {
            return Err("Subdecks are not allowed".into());
        }
    }
    if owner != 0 {
        push::unpack_deck_json(client, deck, notetype_cache, owner, req_ip, deck_id, force_overwrite, commit, deck_tree).await?;
    } else {
        // match try_suggest_note(client, deck_hash, notetype_cache, deck_path, deck_id, deck, req_ip, commit, force_overwrite, deck_tree).await {
        //     Ok(_res) => { },
        //     Err(error) => { println!("Error Submit Note: {error}") },
        // }; // Big Problem: Issues go unnoticed by the user.
        try_suggest_note(client, deck_hash, notetype_cache, deck_path, deck_id, deck, req_ip, commit, force_overwrite, deck_tree).await?;
    }

    Ok("Success".into())
}
