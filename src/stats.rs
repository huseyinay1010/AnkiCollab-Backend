
use crate::{database, structs::StatsInfo};

use std::sync::Arc;

pub async fn new(db_state: &Arc<database::AppState>, info: StatsInfo) -> Result<(), Box<dyn std::error::Error>> { 
    let mut client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {err}");
            return Err("Failed to retrieve a pooled connection".into());
        },
    };

    // Check if the deck exists
    let deck_id_rows = client.query("SELECT id FROM decks WHERE human_hash = $1", &[&info.deck_hash]).await?;
    if deck_id_rows.len() != 1 {
        return Err(format!("Expected one deck with hash {}, found {}", info.deck_hash, deck_id_rows.len()).into());
    }
    let deck_id: i64 = deck_id_rows[0].get(0);

    let deck_ids: Vec<i64> = client
        .query(
            "
                WITH RECURSIVE input_deck_cte AS (
                    SELECT id, name, parent
                    FROM decks
                    WHERE id = $1
                    UNION ALL
                    SELECT d.id, d.name, d.parent
                    FROM decks d
                    JOIN input_deck_cte s ON d.parent = s.id
                )
                SELECT id FROM input_deck_cte
            "
            , &[&deck_id])
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    
    let tx = client.transaction().await?;

    let note_id_stmt = tx.prepare("SELECT id FROM notes WHERE guid = $1 AND deleted = false AND deck = ANY($2)").await?;
    let execute_stmt = tx.prepare(
        "INSERT INTO note_stats (note_id, user_hash, retention, lapses, reps) VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (note_id, user_hash) DO UPDATE SET retention = $3, lapses = $4, reps = $5"
    ).await?;
    
    for notes in info.review_history.values() {
        for (guid, note_stats) in notes {
            let note_id_rows = tx.query(&note_id_stmt, &[&guid, &deck_ids]).await?;
            if note_id_rows.len() != 1 {
                // Skip invalid notes. User may have added a note to a deck that is not in the database.
                continue;
            }
            let note_id: i64 = note_id_rows[0].get(0);

            tx.execute(&execute_stmt,
                &[&note_id, &info.user_hash, &note_stats.retention, &note_stats.lapses, &note_stats.reps]
            ).await?;
        }
    }
    tx.commit().await?;

    Ok(())
    
}