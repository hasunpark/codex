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

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CHATGPT_MODEL: &str = "gpt-5-codex";
const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const DEFAULT_INSTRUCTIONS: &str = include_str!("../../core/gpt_5_codex_prompt.md");

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
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug)]
enum Credential {
    ApiKey(String),
    ChatGpt(TokenBundle),
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

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
    scope: &'static str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: String,
    access_token: Option<String>,
    refresh_token: Option<String>,
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

#[derive(Debug)]
struct HttpResponseError {
    status: StatusCode,
    body: String,
}

fn main() -> Result<()> {
    let credential = read_credentials()?;
    let prompt = read_prompt()?;

    let reply = match credential {
        Credential::ApiKey(key) => call_openai(&key, &prompt)?,
        Credential::ChatGpt(tokens) => call_chatgpt(tokens, &prompt)?,
    };

    println!("\n어시스턴트 > {reply}");
    Ok(())
}

fn read_credentials() -> Result<Credential> {
    let auth_path = resolve_auth_path()?;
    let raw = fs::read_to_string(&auth_path)
        .with_context(|| format!("auth.json 읽기 실패: {}", auth_path.display()))?;
    let auth: AuthJson = serde_json::from_str(&raw).context("auth.json 파싱 실패")?;

    if let Some(api_key) = auth.openai_api_key.filter(|value| !value.trim().is_empty()) {
        return Ok(Credential::ApiKey(api_key));
    }

    if let Some(tokens) = auth.tokens {
        if tokens.access_token.is_some() || tokens.refresh_token.is_some() {
            return Ok(Credential::ChatGpt(tokens));
        }
    }

    Err(anyhow!(
        "auth.json에서 사용할 수 있는 ChatGPT 토큰이나 OPENAI_API_KEY를 찾지 못했습니다."
    ))
}

fn resolve_auth_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CODEX_HOME") {
        let candidate = PathBuf::from(path).join("auth.json");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let candidate = home.join(".codex/auth.json");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "auth.json을 찾을 수 없습니다. CODEX_HOME 환경 변수를 설정하거나 ~/.codex/auth.json 파일을 준비해주세요."
    ))
}

fn read_prompt() -> Result<String> {
    print!("사용자 입력 > ");
    io::stdout().flush().ok();

    let mut buffer = String::new();
    io::stdin()
        .read_line(&mut buffer)
        .context("입력 읽기 실패")?;
    let prompt = buffer.trim().to_string();

    if prompt.is_empty() {
        Err(anyhow!("빈 프롬프트입니다."))
    } else {
        Ok(prompt)
    }
}

fn call_openai(api_key: &str, prompt: &str) -> Result<String> {
    let client = Client::new();
    let request_body = ChatRequest {
        model: CHATGPT_MODEL,
        input: vec![ChatInput {
            role: "user",
            content: vec![ChatContent {
                kind: "input_text",
                text: prompt,
            }],
        }],
        instructions: DEFAULT_INSTRUCTIONS,
        stream: false,
        store: false,
    };

    let response = client
        .post(OPENAI_RESPONSES_URL)
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .context("OpenAI API 요청 실패")?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(anyhow!(
            "인증이 거부되었습니다. OPENAI_API_KEY를 확인하세요."
        ));
    }

    let status = response.status();
    let body = response.text().context("응답 본문 읽기 실패")?;

    if !status.is_success() {
        return Err(anyhow!("API 오류 ({status}): {body}"));
    }

    parse_response_text(&body)
}

fn call_chatgpt(mut tokens: TokenBundle, prompt: &str) -> Result<String> {
    println!("⚙️  auth.json의 토큰을 이용해 ChatGPT 백엔드를 호출합니다...");
    let mut refreshed = false;

    loop {
        match send_chatgpt_request(&tokens, prompt) {
            Ok(text) => return Ok(text),
            Err(err) if err.status == StatusCode::UNAUTHORIZED => {
                if refreshed {
                    return Err(anyhow!(
                        "갱신된 토큰으로도 인증이 실패했습니다. codex login을 다시 실행해 토큰을 재발급하세요."
                    ));
                }
                tokens = refresh_tokens(&tokens)?;
                refreshed = true;
            }
            Err(err) => {
                return Err(anyhow!(
                    "ChatGPT 백엔드 호출 실패 (status: {}): {}",
                    err.status,
                    err.body
                ));
            }
        }
    }
}

fn send_chatgpt_request(tokens: &TokenBundle, prompt: &str) -> Result<String, HttpResponseError> {
    let access_token = tokens
        .access_token
        .as_ref()
        .ok_or_else(|| HttpResponseError {
            status: StatusCode::UNAUTHORIZED,
            body: "access_token이 없습니다".to_string(),
        })?;

    let account_id =
        extract_chatgpt_account_id(&tokens.id_token).map_err(|err| HttpResponseError {
            status: StatusCode::BAD_REQUEST,
            body: format!("id_token 파싱 실패: {err}"),
        })?;

    let request_body = ChatRequest {
        model: CHATGPT_MODEL,
        input: vec![ChatInput {
            role: "user",
            content: vec![ChatContent {
                kind: "input_text",
                text: prompt,
            }],
        }],
        instructions: DEFAULT_INSTRUCTIONS,
        stream: true,
        store: false,
    };

    let conversation_id = Uuid::new_v4().to_string();

    let client = Client::builder()
        .user_agent("codex-simple-chatgpt-test/0.1")
        .build()
        .map_err(|err| HttpResponseError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: format!("HTTP 클라이언트 생성 실패: {err}"),
        })?;

    let response = client
        .post(format!("{CHATGPT_BASE_URL}/codex/responses"))
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("OpenAI-Beta", "responses=experimental")
        .header("chatgpt-account-id", account_id)
        .header("conversation_id", &conversation_id)
        .header("session_id", &conversation_id)
        .json(&request_body)
        .send()
        .map_err(|err| HttpResponseError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: format!("요청 전송 실패: {err}"),
        })?;

    let status = response.status();
    let body = response
        .text()
        .unwrap_or_else(|_| "<본문 읽기 실패>".to_string());

    if !status.is_success() {
        return Err(HttpResponseError { status, body });
    }

    parse_sse_body(&body).map_err(|err| HttpResponseError {
        status,
        body: format!("스트리밍 응답 파싱 실패: {err} | raw={body}"),
    })
}

fn parse_response_text(body: &str) -> Result<String> {
    let reply: ResponsesReply = serde_json::from_str(body)?;
    extract_output(reply)
}

fn parse_sse_body(body: &str) -> Result<String> {
    let mut collected = String::new();

    for block in body.split("\n\n") {
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
            let Some(event_type) = event.get("type").and_then(|v| v.as_str()) else {
                continue;
            };

            match event_type {
                "response.output_text.delta" => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        collected.push_str(delta);
                    }
                }
                "response.completed" => {
                    if collected.is_empty() {
                        if let Some(response) = event.get("response") {
                            let reply: ResponsesReply = serde_json::from_value(response.clone())?;
                            return extract_output(reply);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if collected.is_empty() {
        Err(anyhow!("스트리밍 응답에서 텍스트를 찾지 못했습니다."))
    } else {
        Ok(collected)
    }
}

fn extract_output(reply: ResponsesReply) -> Result<String> {
    for message in reply.output {
        for piece in message.content {
            if piece.kind == "output_text" {
                if let Some(text) = piece.text {
                    return Ok(text);
                }
            }
        }
    }
    Err(anyhow!("응답에서 output_text를 찾지 못했습니다."))
}

fn refresh_tokens(tokens: &TokenBundle) -> Result<TokenBundle> {
    let refresh_token = tokens
        .refresh_token
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("refresh_token이 없습니다. codex login으로 다시 로그인해주세요."))?;

    println!("🔄 refresh_token으로 토큰을 갱신합니다...");
    let request = RefreshRequest {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token,
        scope: "openid profile email",
    };

    let client = Client::new();
    let response = client
        .post("https://auth.openai.com/oauth/token")
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .context("토큰 갱신 요청 실패")?;

    let status = response.status();
    let body = response.text().context("토큰 갱신 응답 읽기 실패")?;

    if !status.is_success() {
        return Err(anyhow!("토큰 갱신 실패 (status: {status}): {body}"));
    }

    let updated: RefreshResponse =
        serde_json::from_str(&body).context("토큰 갱신 응답 파싱 실패")?;

    Ok(TokenBundle {
        id_token: updated.id_token,
        access_token: updated.access_token.or_else(|| tokens.access_token.clone()),
        refresh_token: updated
            .refresh_token
            .or_else(|| tokens.refresh_token.clone()),
    })
}

fn extract_chatgpt_account_id(id_token: &str) -> Result<String> {
    let mut parts = id_token.split('.');
    let (_header, payload, _sig) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => return Err(anyhow!("잘못된 JWT 형식")),
    };

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("JWT payload 디코딩 실패")?;
    let claims: IdClaims = serde_json::from_slice(&payload_bytes).context("JWT JSON 파싱 실패")?;
    claims
        .auth
        .and_then(|auth| auth.chatgpt_account_id)
        .ok_or_else(|| anyhow!("chatgpt_account_id 클레임이 없습니다."))
}
