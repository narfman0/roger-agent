use crate::history::ChatMessage;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    max_tokens: u32,
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: String,
}

pub struct LlmClient {
    http: Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    max_tokens: u32,
    temperature: f32,
}

impl LlmClient {
    pub fn new(
        base_url: String,
        model: String,
        api_key: Option<String>,
        max_tokens: u32,
        temperature: f32,
    ) -> Self {
        LlmClient {
            http: Client::new(),
            base_url,
            model,
            api_key,
            max_tokens,
            temperature,
        }
    }

    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = ChatRequest {
            model: &self.model,
            messages,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        };

        let mut req = self.http.post(&url).json(&body);

        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM request failed {}: {}", status, text);
        }

        let chat_resp: ChatResponse = resp.json().await?;
        let content = chat_resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_else(|| "(no response)".to_string());

        Ok(content)
    }
}
