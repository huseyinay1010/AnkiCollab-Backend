use std::sync::Arc;
use serde::Deserialize;

use crate::database;

#[derive(Deserialize)]
pub struct Login {
    pub username: String,
    pub password: String,
}

use database::SharedConn;

pub async fn login(db_state: &Arc<database::AppState>, form: &Login) -> Result<String, String> {
    let client = match db_state.db_pool.get_owned().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        },
    };
    let normalized_username = form.username.to_lowercase();
    
    let stmt = client
        .prepare("SELECT id, username, password FROM users WHERE username = $1")
        .await
        .map_err(|e| e.to_string())?;

    let row = client
        .query_one(&stmt, &[&normalized_username])
        .await
        .map_err(|_| "Invalid username or password".to_string())?;

    let username:String = row.get(1);
    let password:String = row.get(2);

    if verify_password(&form.password, &password)? {
        let token = request_token(&client, &username).await.map_err(|e| e.to_string())?;
        Ok(token)
    } else {
        Err("Invalid username or password".to_string())
    }
}

fn verify_password(password: &str, hash: &str) -> Result<bool, String> {
    argon2::verify_encoded(hash, password.as_bytes()).map_err(|e| e.to_string())
}

pub async fn request_token(client: &SharedConn, username: &String) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let normalized_username = username.to_lowercase();
    let result = client.query("
        WITH user_info AS (
            SELECT id
            FROM users
            WHERE username = $1
        )
        INSERT INTO auth_tokens (user_id, token)
        SELECT id, md5(random()::text)
        FROM user_info
        ON CONFLICT (user_id) 
        DO UPDATE SET token = md5(random()::text)
        RETURNING token
    ", &[&normalized_username]).await;

    match result {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                let token: String = row.get(0);
                Ok(token)
            } else {
                Err("No token generated".into())
            }
        },
        Err(err) => {
            println!("Error requesting token: {}", err);
            Err("Cannot request token".into())
        }
    }
}


pub async fn remove_token(db_state: &Arc<database::AppState>, token: &String) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let client = match db_state.db_pool.get().await {
        Ok(pool) => pool,
        Err(err) => {
            println!("Error getting pool: {}", err);
            return Err("Failed to retrieve a pooled connection".into());
        },
    };

    let result = client.query("DELETE FROM auth_tokens WHERE token = $1", &[&token]).await;
    if let Err(err) = result {
        println!("Error removing token: {}", err);
        return Err("Cannot remove token".into());
    }
    Ok("Done.".into())
}