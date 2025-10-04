use std::fs;
use std::io::Write;
use std::io::{self};
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

const CHATGPT_MODEL: &str = "gpt-5-codex";
const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const CHATGPT_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CHATGPT_PROMPT: &str = include_str!("../../core/gpt_5_codex_prompt.md");

#[derive(Debug, Deserialize)]
struct AuthJson {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<TokenBundle>,
}

#[derive(Debug, Deserialize, Clone)]
struct TokenBundle {
    id_token: String,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    input: Vec<ChatInput<'a>>,
    instructions: &'a str,
    stream: bool,
    store: bool,
}

#[derive(Debug, Serialize)]
struct ChatInput<'a> {
    role: &'a str,
    content: Vec<ChatContent<'a>>,
}

#[derive(Debug, Serialize)]
struct ChatContent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

#[derive(Debug, Deserialize)]
struct ResponsesReply {
    output: Vec<OutputMessage>,
}

#[derive(Debug, Deserialize)]
struct OutputMessage {
    #[allow(dead_code)]
    role: Option<String>,
    content: Vec<OutputContent>,
}

#[derive(Debug, Deserialize)]
struct OutputContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}

#[derive(Debug, Deserialize)]
struct AuthClaims {
    chatgpt_account_id: Option<String>,
}

fn main() -> Result<()> {
    // --- auth.json 위치 찾기 ---
    let auth_path = if let Ok(path) = std::env::var("CODEX_HOME") {
        let candidate = PathBuf::from(path).join("auth.json");
        if candidate.exists() {
            candidate
        } else {
            return Err(anyhow!("CODEX_HOME/auth.json을 찾을 수 없습니다."));
        }
    } else {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("홈 디렉터리를 찾을 수 없습니다."))?;
        let candidate = home.join(".codex/auth.json");
        if candidate.exists() {
            candidate
        } else {
            return Err(anyhow!(
                "~/.codex/auth.json이 없습니다. codex login 후 다시 시도하세요."
            ));
        }
    };

    // --- auth.json 로드 ---
    let raw_auth = fs::read_to_string(&auth_path)
        .with_context(|| format!("auth.json 읽기 실패: {}", auth_path.display()))?;
    let parsed_auth: AuthJson = serde_json::from_str(&raw_auth).context("auth.json 파싱 실패")?;
    let openai_api_key = parsed_auth.openai_api_key.as_ref().and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value.trim())
        }
    });
    let chatgpt_tokens = parsed_auth.tokens.clone();

    // --- 사용자 입력 ---
    print!("사용자 입력 > ");
    io::stdout().flush().ok();
    let mut prompt = String::new();
    io::stdin()
        .read_line(&mut prompt)
        .context("입력 읽기 실패")?;
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(anyhow!("빈 프롬프트입니다."));
    }

    // --- OpenAI API 키가 있으면 OpenAI Responses 사용 ---
    let reply = if let Some(api_key) = openai_api_key {
        let client = Client::new();
        let body = ChatRequest {
            model: CHATGPT_MODEL,
            input: vec![ChatInput {
                role: "user",
                content: vec![ChatContent {
                    kind: "input_text",
                    text: &prompt,
                }],
            }],
            instructions: CHATGPT_PROMPT,
            stream: false,
            store: false,
        };

        let response = client
            .post(OPENAI_RESPONSES_URL)
            .bearer_auth(api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("OpenAI API 요청 실패")?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "인증이 거부되었습니다. OPENAI_API_KEY를 확인하세요."
            ));
        }

        let status = response.status();
        let text_body = response.text().context("응답 본문 읽기 실패")?;
        if !status.is_success() {
            return Err(anyhow!("API 오류 ({status}): {text_body}"));
        }

        let reply: ResponsesReply = serde_json::from_str(&text_body)?;
        reply
            .output
            .into_iter()
            .flat_map(|message| message.content)
            .find_map(|piece| (piece.kind == "output_text").then(|| piece.text).flatten())
            .ok_or_else(|| anyhow!("응답에서 output_text를 찾지 못했습니다."))?
    } else if let Some(tokens) = chatgpt_tokens.as_ref().filter(|t| {
        t.access_token
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
    }) {
        println!("⚙️  auth.json의 토큰을 이용해 ChatGPT 백엔드를 호출합니다...");
        let client = Client::builder()
            .user_agent("codex-simple-chatgpt-test/0.1")
            .build()
            .context("HTTP 클라이언트 생성 실패")?;

        // access_token
        let access_token = tokens
            .access_token
            .as_ref()
            .expect("checked above")
            .trim()
            .to_string();

        // chatgpt-account-id 추출
        let mut parts = tokens.id_token.split('.');
        let (_, payload, _) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => {
                (h, p, s)
            }
            _ => return Err(anyhow!("잘못된 JWT 형식")),
        };
        let payload_bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .context("JWT payload 디코딩 실패")?;
        let claims: IdClaims =
            serde_json::from_slice(&payload_bytes).context("JWT JSON 파싱 실패")?;
        let account_id = claims
            .auth
            .and_then(|auth| auth.chatgpt_account_id)
            .ok_or_else(|| anyhow!("chatgpt_account_id 클레임이 없습니다."))?;

        let conversation_id = Uuid::new_v4().to_string();
        let body = ChatRequest {
            model: CHATGPT_MODEL,
            input: vec![ChatInput {
                role: "user",
                content: vec![ChatContent {
                    kind: "input_text",
                    text: &prompt,
                }],
            }],
            instructions: CHATGPT_PROMPT,
            stream: true,
            store: false,
        };

        let response = client
            .post(CHATGPT_RESPONSES_URL)
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("OpenAI-Beta", "responses=experimental")
            .header("chatgpt-account-id", account_id)
            .header("conversation_id", &conversation_id)
            .header("session_id", &conversation_id)
            .json(&body)
            .send()
            .context("ChatGPT 백엔드 요청 실패")?;

        let status = response.status();
        let text_body = response
            .text()
            .unwrap_or_else(|_| "<본문 읽기 실패>".to_string());
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "ChatGPT 토큰이 만료되었거나 유효하지 않습니다. codex login으로 다시 로그인해주세요."
            ));
        }
        if !status.is_success() {
            return Err(anyhow!(
                "ChatGPT 백엔드 호출 실패 (status: {status}): {text_body}"
            ));
        }

        let mut collected = String::new();
        for block in text_body.split("\n\n") {
            for line in block.lines() {
                let line = line.trim();
                if !line.starts_with("data:") {
                    continue;
                }
                let data = line.trim_start_matches("data:").trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }

                let event: Value = serde_json::from_str(data)?;
                match event.get("type").and_then(|v| v.as_str()) {
                    Some("response.output_text.delta") => {
                        if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                            collected.push_str(delta);
                        }
                    }
                    Some("response.completed") if collected.is_empty() => {
                        if let Some(response_value) = event.get("response") {
                            let reply: ResponsesReply =
                                serde_json::from_value(response_value.clone())?;
                            if let Some(text) = reply
                                .output
                                .into_iter()
                                .flat_map(|message| message.content)
                                .find_map(|piece| {
                                    (piece.kind == "output_text").then(|| piece.text).flatten()
                                })
                            {
                                collected = text;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if collected.is_empty() {
            return Err(anyhow!("스트리밍 응답에서 텍스트를 찾지 못했습니다."));
        }
        collected
    } else {
        return Err(anyhow!(
            "auth.json에 OPENAI_API_KEY나 access_token이 없습니다."
        ));
    };

    println!("\n어시스턴트 > {reply}");
    Ok(())
}
