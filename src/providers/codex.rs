use std::collections::HashSet;

use serde_json::Value;

use crate::model::{Account, DetectedCredential, ProviderOutcome, RateLimitWindow};
use crate::providers::{request_json, HttpRequest};
use crate::util::{parse_reset_at, window_label};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Больше года — это уже не окно лимита, а мусор в ответе (как у Claude и OpenCode Go):
/// сброс считался бы от далёкого будущего.
const MAX_WINDOW_SECONDS: f64 = 31_622_400.0;

fn codex_home(account: Option<&Account>) -> String {
    let configured = account
        .and_then(|a| {
            a.credentials
                .get("codexHome")
                .or_else(|| a.options.get("codexHome"))
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(path) = configured {
        return expand_tilde(&path);
    }
    if let Some(home) = std::env::var("CODEX_HOME")
        .ok()
        .map(|home| home.trim().to_string())
        .filter(|home| !home.is_empty())
    {
        return expand_tilde(&home);
    }
    home_dir()
        .map(|home| format!("{home}/.codex"))
        .unwrap_or_default()
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .map(|home| home.trim().to_string())
        .filter(|home| !home.is_empty())
}

pub(crate) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir()
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|| path.to_string());
    }
    if path == "~" {
        return home_dir().unwrap_or_else(|| path.to_string());
    }
    path.to_string()
}

fn read_auth_json(codex_home: &str) -> Option<Value> {
    if codex_home.is_empty() {
        return None;
    }
    let raw = std::fs::read_to_string(std::path::Path::new(codex_home).join("auth.json")).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

fn decode_email(id_token: Option<&str>) -> Option<String> {
    let token = id_token?;
    let payload = token.split('.').nth(1)?;
    let bytes = crate::util::base64url_decode(payload)?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    json.get("email")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn window_from_backend(
    raw: Option<&Value>,
    key: &str,
    fallback_minutes: i64,
) -> Option<RateLimitWindow> {
    let raw = raw?;
    let used = raw
        .get("used_percent")
        // Число могут прислать и строкой: окно не должно пропадать целиком.
        .and_then(super::num)
        .and_then(crate::providers::safe_percent)?;
    let fallback_seconds = if fallback_minutes > 0 {
        (fallback_minutes * 60) as f64
    } else {
        0.0
    };
    let reported = raw
        .get("limit_window_seconds")
        .and_then(super::num)
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= MAX_WINDOW_SECONDS);
    // Угаданная длина годится для подписи, но не как граница чужого срока сброса.
    let bound = reported.unwrap_or(MAX_WINDOW_SECONDS);
    let seconds = reported.unwrap_or(fallback_seconds);
    let minutes = if seconds > 0.0 {
        ((seconds / 60.0).round() as i64).max(1)
    } else {
        0
    };
    let now = crate::store::now_ms();
    // Сброс не дальше длины окна (без неё — года). reset_at — метка чужих часов: 5 минут запаса,
    // иначе сервер, спешащий на пару секунд, молча стирал отсчёт в начале окна.
    const CLOCK_SLACK_S: f64 = 300.0;
    let resets_at = raw.get("reset_at").and_then(parse_reset_at).filter(|at| *at > now && *at - now <= ((bound + CLOCK_SLACK_S) * 1000.0) as i64).or_else(|| {
        raw.get("reset_after_seconds")
            .and_then(super::num)
            // Сброс не дальше длины окна, если сервер её назвал; угаданная длина — только подпись.
            .filter(|after| *after > 0.0 && *after <= bound)
            .map(|after| now.saturating_add((after * 1000.0) as i64))
    });
    Some(RateLimitWindow {
        key: key.to_string(),
        label: if minutes > 0 {
            window_label(minutes)
        } else {
            "лимит".to_string()
        },
        used_percent: used,
        window_minutes: minutes,
        resets_at,
        note: None,
    })
}

fn append_additional_windows(payload: &Value, windows: &mut Vec<RateLimitWindow>) {
    let Some(limits) = payload
        .get("additional_rate_limits")
        .and_then(Value::as_array)
    else {
        return;
    };
    let mut keys: HashSet<String> = windows.iter().map(|window| window.key.clone()).collect();

    // Сотня лимитов от битого ответа растянула бы карточку на экраны.
    for (index, limit) in limits.iter().take(20).enumerate() {
        // Официальная модель Codex описывает эти поля строками без запрета
        // на пустоту — пустой limit_name не должен прятать id.
        let Some(name) = ["limit_name", "metered_feature"]
            .iter()
            .filter_map(|key| limit.get(*key).and_then(Value::as_str).map(str::trim))
            .find(|name| !name.is_empty())
        else {
            continue;
        };
        let safe_name: String = name
            .chars()
            .filter(|ch| !ch.is_control())
            .take(64)
            .collect();
        if safe_name.is_empty() {
            continue;
        }
        let display_name = safe_name.replace('_', " ").replace('-', " ");
        let rate_limit = limit.get("rate_limit");
        for (field, suffix) in [
            ("primary_window", "primary"),
            ("secondary_window", "secondary"),
        ] {
            let Some(mut window) = window_from_backend(
                rate_limit.and_then(|value| value.get(field)),
                &format!("{safe_name}:{suffix}"),
                0,
            ) else {
                continue;
            };
            // Одноимённые лимиты различаются номером и в подписи: иначе две строки неотличимы.
            let mut tag = String::new();
            if !keys.insert(window.key.clone()) {
                window.key = format!("{}:{index}", window.key);
                if !keys.insert(window.key.clone()) {
                    continue;
                }
                tag = format!(" #{}", index + 1);
            }
            window.label = format!("{display_name}{tag} · {}", window.label);
            windows.push(window);
        }
    }
}

pub fn detect() -> Vec<DetectedCredential> {
    let codex_home = codex_home(None);
    let Some(auth) = read_auth_json(&codex_home) else {
        return Vec::new();
    };
    let tokens = auth.get("tokens");
    let access_token = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default();
    if access_token.is_empty() {
        return Vec::new();
    }
    let email = decode_email(
        tokens
            .and_then(|t| t.get("id_token"))
            .and_then(|v| v.as_str()),
    );
    let account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        // Идёт в заголовок ChatGPT-Account-Id: управляющий знак уронил бы весь запрос.
        .filter(|v| !v.is_empty() && !v.chars().any(char::is_control))
        .map(str::to_string)
        .or_else(|| jwt_account_id(access_token))
        .unwrap_or_default();

    vec![DetectedCredential {
        provider: crate::model::ProviderId::Codex,
        label: match &email {
            Some(email) => format!("ChatGPT · {email}"),
            None => "ChatGPT (Codex CLI)".to_string(),
        },
        credentials: vec![
            ("accessToken".to_string(), access_token.to_string()),
            ("accountId".to_string(), account_id.to_string()),
            ("codexHome".to_string(), codex_home.clone()),
        ],
        options: Vec::new(),
        source: format!("{codex_home}/auth.json"),
    }]
}

/// Аккаунт ChatGPT из самого токена (claim `chatgpt_account_id`): он переживает обновление
/// токена, поэтому сверять вход надо по нему, а не по совпадению токенов.
fn jwt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = crate::util::base64url_decode(payload)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Срок жизни JWT (`exp`, секунды); None — если это не JWT.
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = crate::util::base64url_decode(payload)?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("exp")?.as_i64()
}

/// Каким токеном ходить. В карточке — снимок токена на момент добавления: он живёт ~10 дней и истекает,
/// а Codex CLI сам обновляет вход в auth.json. Поэтому токен того же аккаунта ChatGPT из auth.json,
/// который живёт дольше сохранённого, важнее — его и берём (и записываем в карточку). Токен другого
/// аккаунта (в CLI вошли под другим) не подставляем: это были бы чужие лимиты.
/// (токен, взят ли он из auth.json вместо сохранённого).
fn pick_token(stored: Option<&str>, file: Option<&str>, same_account: bool) -> Option<(String, bool)> {
    match (stored, file) {
        (None, file) => file.map(|f| (f.to_string(), false)),
        // Токен карточки не JWT (срок неизвестен) — файл побеждает, только если сам ещё жив:
        // иначе рабочий ручной токен вытеснялся мёртвым, а откат на него блокировался.
        (Some(s), Some(f))
            if same_account
                && f != s
                && jwt_exp(f).is_some_and(|fe| fe > jwt_exp(s).unwrap_or(crate::store::now_ms() / 1000)) =>
        {
            Some((f.to_string(), true))
        }
        (Some(s), _) => Some((s.to_string(), false)),
    }
}

/// Тот же аккаунт ChatGPT. Без сверки id токен из auth.json может принадлежать другому
/// входу, а он ещё и записался бы в карточку. Значит, сверяем всегда, когда есть что сверить:
/// - id не записан нигде: вход в этом CODEX_HOME один;
/// - id нет в файле: файл не подтверждает карточку, подставлять его токен нельзя;
/// - id нет в карточке: тот же вход отдаёт тот же токен, а чужой — другой.
fn same_account(
    stored_account: Option<&str>,
    file_account: Option<&str>,
    stored_token: Option<&str>,
    file_token: Option<&str>,
) -> bool {
    match (stored_account, file_account) {
        // Ни у кого нет id: вход в этом CODEX_HOME один, токен CLI — обновлённый токен карточки.
        (None, None) => true,
        (_, None) => false,
        (None, Some(_)) => stored_token.is_some() && stored_token == file_token,
        (Some(a), Some(b)) => a == b,
    }
}

/// Какой accountId слать в заголовке. Он принадлежит конкретному токену, поэтому id
/// из auth.json нельзя ставить в пару к снимку из карточки, если аккаунты разошлись.
fn account_id_for(
    token_from_card: bool,
    same_account: bool,
    stored: Option<&str>,
    file: Option<&str>,
) -> Option<String> {
    let id = match (token_from_card, same_account) {
        (true, true) => stored.or(file),
        (true, false) => stored,
        (false, _) => file,
    };
    id.map(str::to_string)
}

fn usage_request(token: &str, account_id: Option<&String>) -> HttpRequest {
    let mut request = HttpRequest::get(USAGE_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("OpenAI-Beta", "codex-1")
        .header("originator", "Codex Desktop")
        .user_agent("codex-cli");
    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id.clone());
    }
    request
}

pub fn fetch(account: &Account) -> ProviderOutcome {
    let codex_home = codex_home(Some(account));
    let auth = read_auth_json(&codex_home);
    let file_tokens = auth.as_ref().and_then(|a| a.get("tokens"));
    let file_token = file_tokens
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let file_account = file_tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    // Нет id в файле — берём из claim токена файла.
    let file_claim = file_token.and_then(jwt_account_id);
    let file_account = file_account.or(file_claim.as_deref());
    let stored_token = account.credentials.get("accessToken").map(|v| v.trim()).filter(|v| !v.is_empty());
    // Карточка без id — из claim её токена: иначе после обновления токена CLI сверка ломалась навсегда.
    let stored_claim = stored_token.and_then(jwt_account_id);
    let stored_account = account.credentials.get("accountId").map(|v| v.trim()).filter(|v| !v.is_empty()).or(stored_claim.as_deref());
    let same_account = same_account(stored_account, file_account, stored_token, file_token);
    // Карточка без токена, но привязанная к аккаунту: чужой вход в auth.json показал бы чужие лимиты.
    // Файл без id тоже не подтверждает привязку — это мог быть кто угодно.
    if stored_token.is_none() && file_token.is_some() && stored_account.is_some_and(|a| file_account != Some(a)) {
        let text = if file_account.is_none() {
            "Не удалось подтвердить, что в Codex CLI тот же аккаунт ChatGPT — войди в этот («codex login») или вставь токен в карточку"
        } else {
            "В Codex CLI вошли в другой аккаунт ChatGPT — войди в этот («codex login») или вставь токен в карточку"
        };
        return ProviderOutcome::unavailable(text.to_string());
    }

    let Some((mut token, mut from_file)) = pick_token(stored_token, file_token, same_account) else {
        let message = if codex_home.is_empty() {
            "Нет токена. Выполни «codex login» или добавь токен вручную (HOME/CODEX_HOME не задан)"
                .to_string()
        } else {
            format!("Нет токена. Выполни «codex login» или добавь токен вручную (искал в {codex_home}/auth.json)")
        };
        return ProviderOutcome::unavailable(message);
    };

    // Уходящий токен либо снимок из карточки, либо из auth.json (в том числе когда карточка
    // его вовсе не хранит) — заголовок с id обязан принадлежать именно ему.
    let mut token_from_card = !from_file && stored_token.is_some();
    let mut result = request_json(usage_request(
        &token,
        account_id_for(token_from_card, same_account, stored_account, file_account).as_ref(),
    ));
    // Сохранённый токен отвергнут, а в auth.json другой токен того же аккаунта — ещё одна попытка им.
    if let (Err(error), Some(file)) = (&result, file_token) {
        if error.is_unauthorized() && !from_file && same_account && file != token {
            token = file.to_string();
            from_file = true;
            token_from_card = false;
            result = request_json(usage_request(
                &token,
                account_id_for(token_from_card, same_account, stored_account, file_account).as_ref(),
            ));
        }
    }
    let mut patch = Vec::new();
    if result.is_ok() {
        // Токен из auth.json сработал — в карточку: она больше не держит просроченный снимок.
        if from_file && stored_token.is_some() {
            patch.push(("accessToken".to_string(), token.clone()));
        }
        // И его accountId: без него следующая сверка навсегда остаётся «неизвестно».
        // Токен карточки совпал с auth.json — это тот же вход, id тоже берём: иначе после ротации
        // токена в CLI сверка навсегда скажет «вошли в другой аккаунт».
        if (!token_from_card || same_account) && stored_account.is_none() {
            if let Some(id) = file_account {
                patch.push(("accountId".to_string(), id.to_string()));
            }
        }
    }

    let payload = match result {
        Ok(payload) => payload,
        Err(error) => {
            if error.is_unauthorized() {
                let message = if !same_account && file_token.is_some() && stored_token.is_some() && stored_account.is_none() {
                    "Токен ChatGPT просрочен, а вход Codex CLI не подтверждает этот аккаунт — войди в этот («codex login») или вставь токен в карточку"
                } else if !same_account && file_token.is_some() && stored_token.is_some() {
                    "Токен ChatGPT просрочен, а в Codex CLI вошли в другой аккаунт — войди в этот («codex login») или вставь токен в карточку"
                } else if file_token.is_some() {
                    "Вход Codex CLI просрочен — запусти «codex» (он обновит вход) или «codex login»"
                } else {
                    "Токен ChatGPT просрочен — запусти «codex login» и вставь новый токен в карточку"
                };
                return ProviderOutcome::fail(message);
            }
            if error.status == Some(403) {
                return ProviderOutcome::fail("ChatGPT запретил доступ к данным лимитов (403)");
            }
            if let Some(status) = error.status.filter(|s| (300..400).contains(s)) {
                // Редиректы не идём: токен в заголовке не должен уехать на чужой адрес.
                return ProviderOutcome::fail(format!("ChatGPT перенаправил запрос лимитов ({status}) — вход, похоже, устарел: «codex login»"));
            }
            if error.is_rate_limited() {
                return ProviderOutcome::fail("ChatGPT временно ограничил запросы (429)");
            }
            return ProviderOutcome::fail(error.message);
        }
    };

    let rate_limit = payload.get("rate_limit");
    let primary = window_from_backend(
        rate_limit.and_then(|r| r.get("primary_window")),
        "session",
        300,
    );
    let secondary = window_from_backend(
        rate_limit.and_then(|r| r.get("secondary_window")),
        "weekly",
        10080,
    );

    // Окно пришло, а процент не распознан — формат сменился. Молча выкинуть нельзя:
    // исчерпанный недельный лимит пропал бы, и ключ встал бы в пул с ложным запасом.
    let lost = |name: &str, got: &Option<RateLimitWindow>| got.is_none() && rate_limit.and_then(|r| r.get(name)).is_some_and(|w| !w.is_null());
    if lost("primary_window", &primary) || lost("secondary_window", &secondary) {
        return ProviderOutcome::fail("ChatGPT прислал окно без процента — формат ответа сменился").patch(patch);
    }
    let mut windows: Vec<RateLimitWindow> = [primary, secondary].into_iter().flatten().collect();
    append_additional_windows(&payload, &mut windows);
    let unlimited = payload
        .get("credits")
        .and_then(|c| c.get("unlimited"))
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s.trim().eq_ignore_ascii_case("true"))))
        .unwrap_or(false);
    let plan = payload
        .get("plan_type")
        .and_then(|v| v.as_str())
        // Тариф с чужого сервера рисуется в карточке: без управляющих символов и не длиннее имени лимита.
        .map(|s| s.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());
    let mut notes = Vec::new();
    if let Some(count) = payload
        .get("rate_limit_reset_credits")
        .and_then(|c| c.get("available_count"))
        .and_then(super::num)
        .filter(|v| v.fract() == 0.0 && (0.0..=10_000.0).contains(v))
        .map(|v| v as i64)
    {
        if count > 0 {
            notes.push(format!("в запасе: {} лимита", crate::util::plural(count as u64, "сброс", "сброса", "сбросов")));
        }
    }
    if windows.is_empty() && unlimited {
        // Безлимитный план лимитов не присылает — это не сбой; тариф при этом не терять.
        notes.push("Кредиты без лимита".to_string());
        return ProviderOutcome::ok(windows).with_plan(plan).with_notes(notes).patch(patch);
    }
    if windows.is_empty() {
        // Токен при этом отработал — сохраняем его, иначе следующий цикл снова начнёт со старого и 401.
        return ProviderOutcome::fail("ChatGPT API не вернул данные о лимитах").patch(patch);
    }

    if unlimited {
        notes.push("Кредиты без лимита".to_string());
    }

    ProviderOutcome::ok(windows)
        .with_plan(plan)
        .with_notes(notes)
        .patch(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(exp: i64) -> String {
        let payload = serde_json::json!({"exp": exp}).to_string();
        let b64 = |bytes: &[u8]| {
            const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
                for i in 0..=chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
                }
            }
            out
        };
        format!("{}.{}.sig", b64(br#"{"alg":"none"}"#), b64(payload.as_bytes()))
    }

    #[test]
    fn свежий_токен_того_же_аккаунта_из_auth_json_важнее_снимка_в_карточке() {
        let (old, new) = (jwt(1_000), jwt(2_000));
        assert_eq!(jwt_exp(&old), Some(1_000));
        assert_eq!(pick_token(Some(&old), Some(&new), true), Some((new.clone(), true)), "CLI обновил вход — берём новый");
        assert_eq!(pick_token(Some(&new), Some(&old), true), Some((new.clone(), false)), "в auth.json старее — свой");
        assert_eq!(pick_token(Some(&old), Some(&new), false), Some((old.clone(), false)), "чужой аккаунт — не подставляем");
        assert_eq!(pick_token(None, Some(&new), true), Some((new.clone(), false)), "карточка без токена читает auth.json");
        assert_eq!(pick_token(Some("manual"), Some("other"), true), Some(("manual".into(), false)), "не JWT: срок не сравнить, остаётся токен карточки");
        assert_eq!(pick_token(None, None, true), None);
    }

    #[test]
    fn backend_percent_and_window_length_are_safe() {
        let raw = serde_json::json!({"used_percent":250.0,
            "limit_window_seconds":21600, "reset_after_seconds":1e19});
        let parsed = window_from_backend(Some(&raw), "session", 300).unwrap();
        assert_eq!(parsed.used_percent, 100.0);
        assert_eq!(parsed.label, "6ч");
        assert!(parsed.resets_at.is_none());
    }

    #[test]
    fn окно_длиннее_года_не_становится_вечным_и_берёт_запасное() {
        let raw = serde_json::json!({"used_percent":10.0, "limit_window_seconds":1e19});
        let parsed = window_from_backend(Some(&raw), "session", 300).unwrap();
        assert_eq!(parsed.window_minutes, 300);
        assert_eq!(parsed.label, "5ч");
        let negative = serde_json::json!({"used_percent":10.0, "limit_window_seconds":-1.0});
        assert_eq!(window_from_backend(Some(&negative), "weekly", 10080).unwrap().window_minutes, 10080);
    }

    #[test]
    fn без_id_чужой_токен_не_считается_своим() {
        // Карточка без accountId, в auth.json другой аккаунт — сверять нечем, не подставляем.
        assert!(!same_account(None, Some("file-account"), Some("card-token"), Some("file-token")));
        // Карточка без accountId, но токен тот же самый: сверять не нужно, это тот же вход.
        assert!(same_account(None, Some("file-account"), Some("same"), Some("same")));
        // id есть только в файле — файл не подтверждает карточку.
        assert!(!same_account(Some("card-account"), None, Some("card-token"), Some("file-token")));
        // Нигде нет id: вход в этом CODEX_HOME один.
        assert!(same_account(None, None, Some("card-token"), Some("file-token")));
        assert!(same_account(Some("a"), Some("a"), Some("t1"), Some("t2")));
        assert!(!same_account(Some("a"), Some("b"), Some("t1"), Some("t2")));
    }

    #[test]
    fn id_в_заголовке_принадлежит_своему_токену() {
        // Снимок из карточки + id чужого входа: заголовок пустой, а не чужой.
        assert_eq!(account_id_for(true, false, None, Some("file-account")), None);
        // Тот же аккаунт: id один, можно смело.
        assert_eq!(account_id_for(true, true, None, Some("file-account")).as_deref(), Some("file-account"));
        // Токен из auth.json — и id оттуда же.
        assert_eq!(account_id_for(false, false, Some("card-account"), Some("file-account")).as_deref(), Some("file-account"));
    }
}
