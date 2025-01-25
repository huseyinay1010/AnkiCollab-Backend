use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// Anki Structs from the json export
/* Notetypes */
#[derive(Deserialize, Serialize)]
pub struct NotetypeField {
    pub description: String,
    pub font: String,
    pub id: Option<i64>,
    pub name: String,
    pub ord: i32,
    //pub preventDeletion: bool,
    pub rtl: bool,
    pub size: i32,
    pub sticky: bool,
    pub tag: Option<i32>,
}

#[derive(Deserialize, Serialize)]
pub struct NotetypeTemplate {
    pub afmt: String,
    pub bafmt: String,
    pub bfont: String,
    pub bqfmt: String,
    pub bsize: i32,
    pub id: Option<i64>,
    pub name: String,
    pub ord: i32,
    pub qfmt: String,
}

#[derive(Deserialize, Serialize)]
pub struct CardRequirement {
    pub card_ord: i32,
    pub kind: String,
    pub field_ords: Vec<u32>,
}

#[derive(Deserialize, Serialize)]
pub struct Notetype {
    pub crowdanki_uuid: String,
    pub css: String,
    pub flds: Vec<NotetypeField>,
    pub latexPost: String,
    pub latexPre: String,
    pub latexsvg: bool,
    pub name: String,
    pub originalStockKind: Option<i32>,
    pub req: Vec<CardRequirement>,
    pub sortf: i32,
    pub tmpls: Vec<NotetypeTemplate>,
    #[serde(rename = "type")]
    pub type_: i32,
}

/* Notes */
#[derive(Deserialize, Serialize)]
pub struct Note {
    pub fields: Vec<String>,
    pub guid: String,
    pub note_model_uuid: String, // this string equals Notetype::crowdanki_uuid
    pub tags: Vec<String>,
}

/* Decks */
#[derive(Deserialize, Serialize)]
pub struct AnkiDeck {
    pub crowdanki_uuid: String,
    pub children: Vec<AnkiDeck>,
    pub desc: String,
    pub name: String,
    pub note_models: Option<Vec<Notetype>>,
    pub notes: Vec<Note>,
    pub media_files: Vec<String>,
}

impl AnkiDeck {
    pub fn from_json_string(json_string: &str) -> Result<AnkiDeck, serde_json::Error> {
        serde_json::from_str(json_string)
    }
}

#[derive(Deserialize, Serialize)]
pub struct UpdateInfo {
    pub timestamp: String,
}

pub struct DeckUpdateInfo {
    pub deck_id: u64,
    pub deck_guid: String,
    pub deck_update: chrono::NaiveDateTime,
    pub parent: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct NoteModelFieldInfo {
    pub id: i64,
    pub name: String,
    pub protected: bool,
}

#[derive(Serialize, Deserialize)]
pub struct NoteModel {
    pub id: i64,
    pub fields:Vec<NoteModelFieldInfo>,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub struct GoogleServiceAccount {
    pub r#type: String,
    pub project_id: String,
    pub private_key_id: String,
    pub private_key: String,
    pub client_email: String,
    pub client_id: String,
    pub auth_uri: String,
    pub token_uri: String,
    pub auth_provider_x509_cert_url: String,
    pub client_x509_cert_url: String,
}

#[derive(Serialize, Deserialize)]
pub struct GDriveInfo {
    pub service_account: GoogleServiceAccount,
    pub folder_id: String,
}

impl Default for GDriveInfo {
    fn default() -> Self {
        GDriveInfo {
            service_account: GoogleServiceAccount {
                r#type: String::new(),
                project_id: String::new(),
                private_key_id: String::new(),
                private_key: String::new(),
                client_email: String::new(),
                client_id: String::new(),
                auth_uri: String::new(),
                token_uri: String::new(),
                auth_provider_x509_cert_url: String::new(),
                client_x509_cert_url: String::new(),
            },
            folder_id: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct UpdateInfoResponse {
    pub gdrive: GDriveInfo,
    pub protected_fields: Vec<NoteModel>,
    pub deck: AnkiDeck,
    pub changelog: String,
    pub deck_hash: String,
    pub optional_tags: Vec<String>,
    pub deleted_notes: Vec<String>,
    pub stats_enabled: bool,
}


#[derive(Deserialize, Serialize)]
pub struct CreateDeckReq {
    pub deck: String,
    pub username: String,
}

#[derive(Deserialize, Serialize)]
pub struct SubmitCardReq {
    pub remote_deck: String, // Hash of the deck
    pub deck_path: String, // Path to the deck in Anki Format
    pub new_name: String, // Users can rename the top most deck. This helps us to keep track of the original name
    pub deck: String, // JSON String of the actual deck content
    pub rationale: i32, // Enum. See elsewhere
    pub commit_text: String, // (optional) additional info
    pub token: String,
    pub force_overwrite: bool,
}

#[derive(Deserialize, Serialize)]
pub struct NoteRemovalReq {
    pub remote_deck: String, // Topmost deck that contains all the notes with the guids. This is required because multiple decks might use a card
    pub note_guids: Vec<String>,
    pub commit_text: String,
    pub token: String,
    pub force_overwrite: bool,
}

#[derive(Deserialize, Serialize)]
pub struct SubmitChangelog {
    pub deck_hash: String,
    pub changelog: String,
    pub token: String,
}

#[derive(Deserialize)]
pub struct CheckDeckAliveRequest {
    pub deck_hashes: Vec<String>,
}

#[derive(Deserialize)]
pub struct NoteStatsInfo {
    pub retention: i32,
    pub reps: i32,
    pub lapses: i32,
}

type NoteReview = HashMap<String, NoteStatsInfo>; // HashMap<NoteGuid, NoteStatsInfo
type DeckReview = HashMap<String, NoteReview>; // HashMap<DeckName, NoteReview>

#[derive(Deserialize)]
pub struct StatsInfo {
    pub user_hash: String,
    pub deck_hash: String,
    pub review_history: DeckReview,
}

#[derive(Deserialize)]
pub struct TokenInfo {
    pub token: String,
    pub deck_hash: String,
}