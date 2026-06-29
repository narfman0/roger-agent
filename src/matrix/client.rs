use anyhow::Result;
use matrix_sdk::{
    config::SyncSettings,
    matrix_auth::MatrixSession,
    Client,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::info;

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSession {
    homeserver: String,
    access_token: String,
    user_id: String,
    device_id: String,
}

pub async fn build_client(homeserver: &str, data_dir: &Path) -> Result<Client> {
    std::fs::create_dir_all(data_dir)?;

    let client = Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(data_dir, None)
        .build()
        .await?;

    Ok(client)
}

pub async fn login(client: &Client, user: &str, password: &str, data_dir: &Path) -> Result<()> {
    let session_file = data_dir.join("session.json");

    // Try restoring a saved session first
    if session_file.exists() {
        let json = std::fs::read_to_string(&session_file)?;
        if let Ok(saved) = serde_json::from_str::<PersistedSession>(&json) {
            let session = MatrixSession {
                tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
                    access_token: saved.access_token,
                    refresh_token: None,
                },
                meta: matrix_sdk::SessionMeta {
                    user_id: saved.user_id.parse()?,
                    device_id: saved.device_id.into(),
                },
            };
            match client.restore_session(session).await {
                Ok(_) => {
                    info!("restored session for {}", user);
                    client.sync_once(SyncSettings::default()).await?;
                    info!("initial sync complete");
                    return Ok(());
                }
                Err(e) => {
                    info!("session restore failed ({}), doing fresh login", e);
                    let _ = std::fs::remove_file(&session_file);
                }
            }
        }
    }

    // Fresh login
    let resp = client
        .matrix_auth()
        .login_username(user, password)
        .initial_device_display_name("roger")
        .await?;

    info!("logged in as {} (device {})", resp.user_id, resp.device_id);

    // Persist session so next restart restores instead of re-logging in
    let saved = PersistedSession {
        homeserver: client.homeserver().to_string(),
        access_token: resp.access_token,
        user_id: resp.user_id.to_string(),
        device_id: resp.device_id.to_string(),
    };
    std::fs::write(&session_file, serde_json::to_string_pretty(&saved)?)?;
    info!("session saved to {}", session_file.display());

    client.sync_once(SyncSettings::default()).await?;
    info!("initial sync complete");

    Ok(())
}
