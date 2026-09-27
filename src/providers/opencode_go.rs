use serde_json::Value;

use crate::model::{Account, DetectedCredential, ProviderId, ProviderOutcome, RateLimitWindow};
use crate::providers::{request_json, HttpRequest};
use crate::util::parse_reset_at;

const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
// Cloudflare на opencode.ai режет незнакомых клиентов; этот UA пропускают.
const USER_AGENT: &str = "opencode/1.0";

fn auth_candidates() -> Vec<String> {
    let Some(home) = std::env::var("HOME")
        .ok()
        .map(|home| home.trim().to_string())
        .filter(|home| !home.is_empty())
    else {
        return Vec::new();
    };
    vec![
        format!("{home}/.local/share/opencode/auth.json"),
        // Родной путь macOS раньше XDG-config: синхронизированные dotfiles с протухшим ключом не должны его перебить.
        format!("{home}/Library/Application Support/opencode/auth.json"),
        format!("{home}/.config/opencode/auth.json"),
    ]
}

fn read_auth_key_with_source() -> Option<(String, String)> {
    for path in auth_candidates() {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(key) = json
            .get("opencode-go")
            .and_then(|entry| entry.get("key"))
            .and_then(|value| value.as_str())
        {
            let key = key.trim();
            if !key.is_empty() {
                return Some((key.to_string(), path));
            }
        }
    }
    None
}

fn read_auth_key() -> Option<String> {
    read_auth_key_with_source().map(|(key, _)| key)
}

/// Сброс дальше года — мусор в ответе (как у Custom и Devin): иначе окно навсегда «не прокрутилось»
/// и держит ключ в хвосте пула.
fn reset_at(payload: &Value) -> Option<i64> {
    let now = crate::store::now_ms();
    payload
        .get("resetsAt")
        .and_then(parse_reset_at)
        .filter(|at| *at > now && *at - now <= 366 * 86_400_000)
}

fn window(payload: Option<&Value>, key: &str, minutes: i64) -> Option<RateLimitWindow> {
    let payload = payload?;
    // OpenCode сам помечает исчерпанное окно — верим пометке, даже если процента нет или он меньше 100.
    if payload.get("status").and_then(Value::as_str).is_some_and(|s| {
        let s = s.replace(['_', '-'], "").to_ascii_lowercase();
        s == "ratelimited" || s == "limited"
    }) {
        return Some(RateLimitWindow {
            key: key.to_string(),
            label: crate::util::window_label(minutes),
            used_percent: 100.0,
            window_minutes: minutes,
            resets_at: reset_at(payload),
            note: Some("лимит исчерпан".to_string()),
        });
    }
    // Явный минус — мусор: нулём он поставил бы ключ первым в пуле прокси; safe_percent его отсекает.
    let raw = payload.get("percent").and_then(crate::providers::num)?;
    let used = crate::providers::safe_percent(raw)?;
    Some(RateLimitWindow {
        key: key.to_string(),
        label: crate::util::window_label(minutes),
        used_percent: used,
        window_minutes: minutes,
        resets_at: reset_at(payload),
        note: None,
    })
}

pub fn detect() -> Vec<DetectedCredential> {
    let Some((key, source)) = read_auth_key_with_source() else {
        return Vec::new();
    };
    vec![DetectedCredential {
        provider: ProviderId::OpenCodeGo,
        label: "OpenCode Go".to_string(),
        credentials: vec![("apiKey".to_string(), key)],
        options: Vec::new(),
        source,
    }]
}

pub fn fetch(account: &Account) -> ProviderOutcome {
    let api_key = account
        .credentials
        .get("apiKey")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(read_auth_key);

    let Some(api_key) = api_key else {
        return ProviderOutcome::unavailable("Не задан API key OpenCode Go");
    };
    // Непечатный символ в ключе — запрос упал бы с невнятной ошибкой заголовка; скажем прямо.
    if reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}")).is_err() {
        return ProviderOutcome::fail("Ключ OpenCode Go повреждён (лишние символы) — вставь его заново");
    }

    let payload = match request_json(
        HttpRequest::get(USAGE_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .user_agent(USER_AGENT),
    ) {
        Ok(payload) => payload,
        Err(error) => {
            if error.is_unauthorized() {
                return ProviderOutcome::fail("OpenCode Go не принял ключ (401) — проверь API key");
            }
            if error.status == Some(403) {
                return ProviderOutcome::fail(
                    "OpenCode Go отказал (403): у ключа нет активной подписки Go или запрос блокирует Cloudflare",
                );
            }
            if error.status == Some(402) {
                return ProviderOutcome::fail("Нет оплаченной подписки OpenCode Go (402)");
            }
            if error.is_rate_limited() {
                return ProviderOutcome::fail("OpenCode Go: лимит исчерпан или запросы ограничены (429)");
            }
            if error.status.is_some_and(|s| (300..400).contains(&s)) {
                return ProviderOutcome::fail(format!(
                    "OpenCode Go ответил перенаправлением ({}) — адрес статистики сменился, нужна новая версия SubBar",
                    error.status.unwrap_or_default()
                ));
            }
            return ProviderOutcome::fail(error.message);
        }
    };

    let usage = payload.get("usage");
    let mut windows = Vec::new();
    for (name, key, minutes) in [("rolling", "session", 300), ("weekly", "weekly", 10080), ("monthly", "monthly", 43200)] {
        let part = usage.and_then(|u| u.get(name)).filter(|v| !v.is_null());
        match window(part, key, minutes) {
            Some(w) => windows.push(w),
            // Окно пришло, а числа в нём нет: молча показать два окна из трёх — значит отдать
            // прокси ключ с «запасом» по тем, что остались. Считаем опрос неудачным — прошлые цифры останутся.
            None if part.is_some() => {
                let what = if part.and_then(|p| p.get("percent")).is_some() { "с неверным процентом" } else { "без процента" };
                return ProviderOutcome::fail(format!(
                    "OpenCode Go прислал окно «{}» {what} — формат ответа сменился",
                    crate::util::window_label(minutes)
                ));
            }
            None => {}
        }
    }

    // Блок usage есть, но ни одного знакомого окна в нём нет — формат сменился, а не «пока не активны».
    let unknown_shape = usage.is_some_and(|u| {
        u.as_object().is_none_or(|o| !o.is_empty() && !o.keys().any(|k| matches!(k.as_str(), "rolling" | "weekly" | "monthly")))
    });
    if windows.is_empty() && (usage.is_none() || unknown_shape) {
        return ProviderOutcome::fail("OpenCode Go прислал лимиты в незнакомом формате — нужна новая версия SubBar");
    }
    if windows.is_empty() {
        // Все окна пустые — подписка ещё не активна, это не сбой (так же у Command Code).
        return ProviderOutcome::unavailable("Лимиты OpenCode Go пока не активны");
    }
    ProviderOutcome::ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_usage_is_clamped_to_full() {
        let raw = serde_json::json!({"percent":250.0});
        let parsed = window(Some(&raw), "weekly", 10080).unwrap();
        assert_eq!(parsed.used_percent, 100.0);
    }
}
