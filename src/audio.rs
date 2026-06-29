use anyhow::Result;
use reqwest::{multipart, Client};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}

pub struct SpeachesClient {
    http: Client,
    base_url: String,
}

impl SpeachesClient {
    pub fn new(base_url: String) -> Self {
        SpeachesClient {
            http: Client::new(),
            base_url,
        }
    }

    /// Transcribe raw audio bytes (ogg/wav/mp3 etc.) via the Whisper-compatible API.
    pub async fn transcribe(&self, audio_bytes: Vec<u8>, filename: &str) -> Result<String> {
        let url = format!(
            "{}/v1/audio/transcriptions",
            self.base_url.trim_end_matches('/')
        );

        let part = multipart::Part::bytes(audio_bytes)
            .file_name(filename.to_string())
            .mime_str("audio/ogg")?;

        let form = multipart::Form::new()
            .part("file", part)
            .text("model", "Systran/faster-whisper-small");

        let resp = self.http.post(&url).multipart(form).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("speaches transcription failed {}: {}", status, text);
        }

        let result: TranscriptionResponse = resp.json().await?;
        Ok(result.text.trim().to_string())
    }
}
