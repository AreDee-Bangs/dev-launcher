use std::time::Duration;

use crate::config::DevConfig;

#[derive(Clone, Debug)]
pub enum LlmProvider {
    Anthropic,
    OpenAICompatible,
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub provider: LlmProvider,
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

/// Escape a string for JSON.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Extract the first JSON string value after `"key":` in a flat response body.
pub fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\":", key);
    let start = body.find(&needle)? + needle.len();
    let rest = body[start..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let inner = &rest[1..];
    let mut out = String::new();
    let mut chars = inner.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

pub fn call_anthropic(cfg: &LlmConfig, prompt: &str) -> Option<String> {
    let url = format!("{}/messages", cfg.base_url);
    let body = format!(
        "{{\"model\":{},\"max_tokens\":256,\"messages\":[{{\"role\":\"user\",\"content\":{}}}]}}",
        json_string(&cfg.model),
        json_string(prompt),
    );
    let resp = ureq::post(&url)
        .set("x-api-key", &cfg.api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(15))
        .send_string(&body)
        .ok()?;
    let text = resp.into_string().ok()?;
    extract_json_string(&text, "text")
}

pub fn call_openai_compatible(cfg: &LlmConfig, prompt: &str) -> Option<String> {
    let url = format!("{}/chat/completions", cfg.base_url);
    let body = format!(
        "{{\"model\":{},\"max_tokens\":256,\"messages\":[{{\"role\":\"user\",\"content\":{}}}]}}",
        json_string(&cfg.model),
        json_string(prompt),
    );
    let mut req = ureq::post(&url)
        .set("content-type", "application/json")
        .timeout(Duration::from_secs(15));
    if !cfg.api_key.is_empty() {
        req = req.set("authorization", &format!("Bearer {}", cfg.api_key));
    }
    let resp = req.send_string(&body).ok()?;
    let text = resp.into_string().ok()?;
    let choices_pos = text.find("\"choices\"")?;
    extract_json_string(&text[choices_pos..], "content")
}

pub fn llm_diagnose(cfg: &LlmConfig, log_tail: &str) -> Option<String> {
    let prompt = format!(
        "You are a dev-tools assistant. A local development service crashed. \
         The last lines of its log are below. In one short sentence (max 120 chars), \
         state the most likely cause and the single best fix. \
         Be direct — no preamble.\n\nLog tail:\n{log_tail}"
    );
    match cfg.provider {
        LlmProvider::Anthropic => call_anthropic(cfg, &prompt),
        LlmProvider::OpenAICompatible => call_openai_compatible(cfg, &prompt),
    }
}

pub fn resolve_llm_config(dev_cfg: Option<&DevConfig>) -> Option<LlmConfig> {
    let api_key = dev_cfg
        .and_then(|c| c.llm_api_key.as_deref())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("FILIGRAN_LLM_KEY").ok())
        .unwrap_or_default();

    let custom_url = dev_cfg.and_then(|c| c.llm_url.as_deref());
    if api_key.trim().is_empty() && custom_url.is_none() {
        return None;
    }

    let provider_hint = dev_cfg.and_then(|c| c.llm_provider.as_deref());

    let is_anthropic = match provider_hint {
        Some(p) => p.eq_ignore_ascii_case("anthropic"),
        None => {
            custom_url.map(|u| u.contains("anthropic")).unwrap_or(false)
                || (custom_url.is_none() && api_key.starts_with("sk-ant-"))
        }
    };

    let (provider, default_url, default_model) = if is_anthropic {
        (
            LlmProvider::Anthropic,
            "https://api.anthropic.com/v1",
            "claude-haiku-4-5-20251001",
        )
    } else {
        (
            LlmProvider::OpenAICompatible,
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        )
    };

    let base_url = custom_url
        .unwrap_or(default_url)
        .trim_end_matches('/')
        .to_string();

    let model = dev_cfg
        .and_then(|c| c.llm_model.as_deref())
        .unwrap_or(default_model)
        .to_string();

    Some(LlmConfig {
        provider,
        api_key,
        model,
        base_url,
    })
}
