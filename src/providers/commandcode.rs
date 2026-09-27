use serde_json::Value;

use crate::model::{Account, DetectedCredential, ProviderId, ProviderOutcome, RateLimitWindow};
use crate::providers::{request_json, HttpError, HttpRequest};
use crate::util::parse_reset_at;

/// Расход Command Code: CLI читает кредиты и окна 5ч/неделя из
/// своего API (`/alpha/billing/credits`). API-ключ лежит в
/// ~/.commandcode/auth.json.
const API_BASE: &str = "https://api.commandcode.ai";

fn auth_path() -> Option<String> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())?;
    Some(format!("{home}/.commandcode/auth.json"))
}

fn read_auth() -> Option<Value> {
    let raw = std::fs::read_to_string(auth_path()?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn window(raw: &Value, key: &str, label: &str, minutes: i64) -> Option<RateLimitWindow> {
    let used = raw.get("used").and_then(super::num)?;
    let cap = raw.get("cap").and_then(super::num).unwrap_or(0.0);
    // Нет лимита (cap пуст, ноль или не число) — окно не разобрать; отрицательный расход — мусор, а не «0%».
    if cap <= 0.0 || used < 0.0 {
        return None;
    }
    let percent = crate::providers::safe_percent((used / cap) * 100.0)?;
    Some(RateLimitWindow {
        key: key.to_string(),
        label: label.to_string(),
        used_percent: percent,
        window_minutes: minutes,
        resets_at: reset_of(raw),
        note: Some(format!("{} / {}", counter(used, used < cap), counter(cap, true))),
    })
}

fn reset_of(raw: &Value) -> Option<i64> {
    let now = crate::store::now_ms();
    // Срок дальше года — мусор (как у Custom и Devin), иначе «сброс через 2922777д».
    raw.get("resetAt").and_then(parse_reset_at).filter(|at| *at > now && *at - now <= 366 * 86_400_000)
}

/// Счётчик без обмана округлением: дробь, которую целое число исказило бы в «0» (при полосе 1%)
/// или в сам лимит у неисчерпанного окна, пишем с десятыми. Сравниваем с тем, что реально печатается.
fn counter(value: f64, below_cap: bool) -> String {
    let value = if value == 0.0 { 0.0 } else { value }; // «-0» → «0»
    let whole = format!("{value:.0}");
    let misleading = value.fract() != 0.0 && (whole == "0" || (below_cap && whole.parse::<f64>().is_ok_and(|w| w >= value.ceil())));
    if !misleading {
        return whole;
    }
    // 0,04 с десятыми — снова «0.0»: тогда сотые.
    let tenths = format!("{value:.1}");
    if tenths.parse::<f64>().is_ok_and(|t| t == 0.0) { format!("{value:.2}") } else { tenths }
}

fn credits_url(org_id: Option<&str>) -> String {
    let base = format!("{API_BASE}/alpha/billing/credits");
    let Ok(mut url) = reqwest::Url::parse(&base) else { return base };
    if let Some(org) = org_id {
        url.query_pairs_mut().append_pair("orgId", org);
    }
    url.to_string()
}

/// Отказ по ключу одинаков для обоих запросов: если ключ не приняли в whoami, его не примут
/// и в кредитах. Молчать об этом нельзя — иначе причина пропажи всех лимитов остаётся загадкой.
fn auth_failure(error: &HttpError) -> Option<&'static str> {
    if error.is_unauthorized() {
        Some("Command Code не принял ключ (401) — проверь API key")
    } else if error.status == Some(403) {
        Some("Command Code запретил доступ к данным аккаунта (403)")
    } else if error.is_rate_limited() {
        // Без этой ветки 429 уходил в «не удалось прочитать лимиты», и сразу летел второй запрос.
        Some("Command Code просит подождать (429) — обновлю позже")
    } else {
        None
    }
}

pub fn detect() -> Vec<DetectedCredential> {
    let Some(auth) = read_auth() else {
        return Vec::new();
    };
    // Как в fetch: пустой ключ дал бы карточку, которая вечно пишет «не задан API key».
    let Some(key) = auth.get("apiKey").and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let user = auth
        .get("userName")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default();
    vec![DetectedCredential {
        provider: ProviderId::CommandCode,
        label: if user.is_empty() {
            "Command Code".to_string()
        } else {
            format!("Command Code · {user}")
        },
        credentials: vec![("apiKey".to_string(), key.to_string())],
        options: Vec::new(),
        source: auth_path().unwrap_or_default(),
    }]
}

pub fn fetch(account: &Account) -> ProviderOutcome {
    let api_key = account
        .credentials
        .get("apiKey")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            read_auth().and_then(|auth| {
                auth.get("apiKey")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
        });

    let Some(api_key) = api_key else {
        return ProviderOutcome::unavailable("Не задан API key Command Code");
    };

    let auth = format!("Bearer {api_key}");
    // Непечатный символ в ключе — оба запроса упадут с невнятной ошибкой заголовка; скажем прямо.
    if reqwest::header::HeaderValue::from_str(&auth).is_err() {
        return ProviderOutcome::fail("Ключ Command Code повреждён (лишние символы) — вставь его заново");
    }
    let mut whoami_error: Option<String> = None;
    let whoami = match request_json(
        HttpRequest::get(format!("{API_BASE}/alpha/whoami?limits=1"))
            .header("Authorization", auth.clone()),
    ) {
        Ok(payload) => Some(payload),
        Err(error) => {
            if let Some(message) = auth_failure(&error) {
                return ProviderOutcome::fail(message);
            }
            // Сеть или 404: кредиты без org ещё читаются, идём дальше без лимитов организации.
            whoami_error = Some(match error.status {
                // 200 с не-JSON (прокси, captive portal): «HTTP 200» причиной не назвать.
                Some(status) if (200..300).contains(&status) => error.message.clone(),
                Some(status) => format!("HTTP {status}"),
                None => "нет связи".to_string(),
            });
            None
        }
    };

    let org_id = whoami
        .as_ref()
        .and_then(|payload| payload.get("org"))
        .and_then(|org| org.get("id"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());

    let credits_url = credits_url(org_id.as_deref());

    let payload = match request_json(
        HttpRequest::get(credits_url)
            .header("Authorization", auth.clone()),
    ) {
        Ok(payload) => payload,
        Err(error) => {
            if let Some(message) = auth_failure(&error) {
                return ProviderOutcome::fail(message);
            }
            if let Some(status) = error.status.filter(|s| (300..400).contains(s)) {
                return ProviderOutcome::fail(format!(
                    "Command Code ответил перенаправлением ({status}) — адрес API сменился, нужна новая версия SubBar"
                ));
            }
            return ProviderOutcome::fail(error.message);
        }
    };

    let mut windows = Vec::new();
    let mut notes_pre: Vec<String> = Vec::new();
    // Объект окна пришёл, а разобрать его не вышло — это смена формата, а не «лимиты не активны».
    let mut unparsed = false;
    if let Some(limits) = payload.get("windowLimits") {
        // Не объект (массив, строка) — формат сменился, а не «лимиты не активны».
        if !limits.is_object() && !limits.is_null() {
            unparsed = true;
        }
        // Непустой объект без единого знакомого поля — переименовали ключи, а не «лимиты не активны».
        if limits.as_object().is_some_and(|o| {
            !o.is_empty() && !o.keys().any(|k| matches!(k.as_str(), "fiveHour" | "weekly" | "exceeded" | "limited"))
        }) {
            unparsed = true;
        }
        if let Some(five_hour) = limits.get("fiveHour").filter(|value| !value.is_null()) {
            match window(five_hour, "session", "5ч", 300) {
                Some(window) => windows.push(window),
                None => unparsed = true,
            }
        }
        if let Some(weekly) = limits.get("weekly").filter(|value| !value.is_null()) {
            match window(weekly, "weekly", "7д", 10080) {
                Some(window) => windows.push(window),
                None => unparsed = true,
            }
        }
        // Сервер сам говорит, что лимит пробит, а объекта окна не прислал — не писать «пока не активны».
        // Смотрим именно то окно, что пробито: второе распарсенное окно не должно прятать отказ.
        let exceeded_flag = limits.get("exceeded").and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s.eq_ignore_ascii_case("true")))) == Some(true);
        let exceeded_name = limits.get("exceeded").and_then(Value::as_str).map(|s| s.replace(['_', '-'], "").to_ascii_lowercase());
        let exceeded = match exceeded_name.as_deref() {
            Some("fivehour") => Some(("session", "5ч", 300, "fiveHour")),
            Some("weekly") => Some(("weekly", "7д", 10080, "weekly")),
            // Незнакомое окно (или его не назвали) — не выдумывать «недельный»; отказ покажет примечание.
            _ => None,
        };
        // Строкой «true» тоже бывает; а названное пробитое окно само значит «лимит исчерпан».
        let limited = limits
            .get("limited")
            .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s.eq_ignore_ascii_case("true"))))
            == Some(true)
            || exceeded_flag;
        if limited && exceeded.is_none() {
            notes_pre.push("Command Code сообщает, что лимит исчерпан".to_string());
        }
        // Окно разобралось, а сервер говорит «пробито» — верим серверу: 96% при пробитом лимите врут.
        if let Some((key, ..)) = exceeded {
            if let Some(w) = windows.iter_mut().find(|w| w.key == key) {
                w.used_percent = 100.0;
                // Счётчик «120 / 500» уже в заметке — дописать, а не молча оставить цифры против 100%.
                w.note = Some(match w.note.take() {
                    Some(n) if !n.contains("исчерпан") => format!("{n} · лимит исчерпан"),
                    Some(n) => n,
                    None => "лимит исчерпан".to_string(),
                });
            }
        }
        if let Some((key, label, minutes, field)) = exceeded.filter(|(key, ..)| !windows.iter().any(|w| w.key == *key)) {
            windows.push(RateLimitWindow {
                key: key.to_string(),
                label: label.to_string(),
                used_percent: 100.0,
                window_minutes: minutes,
                // Срок сброса мог прийти и в неразобранном объекте окна — не терять «сброс через …».
                resets_at: limits.get(field).and_then(reset_of),
                note: Some("лимит исчерпан".to_string()),
            });
        }
    }

    // Лимиты организации (когда аккаунт в неё входит).
    match whoami.as_ref().and_then(|payload| payload.get("orgLimits")).filter(|v| !v.is_null()) {
        Some(Value::Array(org_limits)) => {
            for (index, limit) in org_limits.iter().enumerate() {
                // Длину квоты организации API не сообщает. Не называем её
                // пятичасовой только потому, что соседнее окно 5ч.
                // Ключ — по id организации, если он есть: порядок в массиве может смениться,
                // и уведомления «уже сообщали» переехали бы на чужое окно.
                let id = ["orgId", "id", "slug"].iter().find_map(|k| limit.get(*k).and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())))).filter(|s| !s.trim().is_empty());
                let name = ["orgName", "name"].iter().find_map(|k| limit.get(*k).and_then(|v| v.as_str())).map(str::trim).filter(|s| !s.is_empty());
                let key = id.map_or_else(|| format!("org:{index}"), |id| format!("org:{id}"));
                let label = name.map_or_else(|| format!("орг {}", index + 1), |n| format!("орг {n}"));
                match window(limit, &key, &label, 0) {
                    Some(window) => windows.push(window),
                    None => unparsed = true,
                }
            }
        }
        // Как и у личных окон: пришло, но не массив — смена формата, а не «лимитов нет».
        // Пустой объект — организации нет, а не смена формата (как у windowLimits).
        Some(serde_json::Value::Object(o)) if o.is_empty() => {}
        Some(_) => unparsed = true,
        None => {}
    }

    let mut notes: Vec<String> = notes_pre;
    if let Some(reason) = &whoami_error {
        notes.push(format!("лимиты организации не прочитаны ({reason})"));
    }
    // Лимиты окон — главное, что показывает карточка: пропал весь блок — это смена формата, а не «не активны».
    if payload.get("windowLimits").is_none() {
        unparsed = true;
    }
    if let Some(credits) = payload.get("credits") {
        let monthly = credits
            .get("monthlyCredits")
            .and_then(super::num)
            .unwrap_or(0.0);
        let purchased = credits
            .get("purchasedCredits")
            .and_then(super::num)
            .unwrap_or(0.0);
        let free = credits
            .get("freeCredits")
            .and_then(super::num)
            .unwrap_or(0.0);
        let total = monthly + purchased + free;
        if total.is_finite() && total > 0.0 {
            notes.push(format!("кредитов: {total:.2}"));
        }
    }

    let plan = payload
        .get("credits")
        .and_then(|credits| credits.get("planId"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    if windows.is_empty() && unparsed {
        // Баланс кредитов при этом верный — не выбрасываем его вместе с ошибкой.
        return ProviderOutcome::fail("Command Code прислал лимиты в незнакомом формате — нужна новая версия SubBar").with_plan(plan).with_notes(notes);
    }
    if unparsed {
        // Часть окон не разобралась — не делать вид, что их нет.
        notes.push("часть лимитов Command Code в незнакомом формате — нужна новая версия SubBar".to_string());
    }
    if windows.is_empty() {
        // Активных окон пока нет — ответ годный, просто рисовать нечего.
        return ProviderOutcome {
            status: crate::model::FetchStatus::Unavailable,
            windows,
            plan_type: plan,
            notes,
            error: Some("Лимиты Command Code пока не активны".to_string()),
            credentials_patch: Vec::new(),
        };
    }

    ProviderOutcome::ok(windows)
        .with_plan(plan)
        .with_notes(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_does_not_lie_by_rounding() {
        assert_eq!(counter(0.5, true), "0.5");
        assert_eq!(counter(99.6, true), "99.6");
        assert_eq!(counter(100.0, false), "100");
        assert_eq!(counter(1.5, true), "1.5");
        assert_eq!(counter(-0.0, true), "0");
        assert_eq!(counter(40.0, true), "40");
        assert_eq!(counter(0.04, true), "0.04");
    }

    #[test]
    fn org_window_does_not_claim_an_unknown_five_hour_period() {
        let raw = serde_json::json!({"used":10,"cap":100});
        let org = window(&raw, "org:0", "орг", 0).unwrap();
        assert_eq!(org.window_minutes, 0);
        assert_eq!(org.used_percent, 10.0);
        let overflow = serde_json::json!({"used":5,"cap":1e-320});
        assert!(window(&overflow, "org:1", "орг", 0).is_none());
    }

    #[test]
    fn org_id_is_one_encoded_query_value() {
        let url = reqwest::Url::parse(&credits_url(Some("synthetic&extra=wrong"))).unwrap();
        let pairs: Vec<_> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "orgId");
        assert_eq!(pairs[0].1, "synthetic&extra=wrong");
    }

    #[test]
    fn отказ_по_ключу_одинаков_для_обоих_запросов() {
        let denied = |status: Option<u16>| HttpError {
            status,
            message: "synthetic".to_string(),
        };
        assert_eq!(auth_failure(&denied(Some(401))), Some("Command Code не принял ключ (401) — проверь API key"));
        assert_eq!(
            auth_failure(&denied(Some(403))),
            Some("Command Code запретил доступ к данным аккаунта (403)")
        );
        // Сеть и прочие коды кредиты могут отдать и без whoami.
        assert!(auth_failure(&denied(None)).is_none());
        assert!(auth_failure(&denied(Some(500))).is_none());
    }
}
