//! Ключи OpenCode Go: выбранный в окне и запасные из карточек.
//! Кончился лимит (429 `GoUsageLimitError`) или ключ отвергнут — ключ уходит на паузу до `Retry-After`,
//! запрос повторяется на ключе с наибольшим запасом. Выбранный ключ возвращается сам, когда пауза кончится.

use crate::model::{Account, ProviderId};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, PartialEq, Eq)]
pub struct Key {
    pub label: String,
    pub key: String,
}

// Сам ключ — секрет: `{:?}` не должен выводить его в журнал.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("label", &self.label).field("key", &mask(&self.key)).finish()
    }
}

/// Почему ключ на паузе. Решения (хранить ли через перезапуск, звать ли клиента подождать) — по виду,
/// а не по тексту причины: текст только для показа, его можно переписать, ничего не сломав.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseKind {
    /// Кончился лимит подписки (GoUsageLimitError): долго, переживает перезапуск.
    Limit,
    /// Просто 429 — подождать.
    Throttled,
    /// Ключ отвергнут (401/403 AuthError): сам не оживёт.
    Rejected,
    /// Нет оплаченной подписки (402): сам не оживёт.
    Unpaid,
}

impl PauseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::Throttled => "throttled",
            Self::Rejected => "rejected",
            Self::Unpaid => "unpaid",
        }
    }

    /// Из файла пауз. Старый файл без поля — там хранились только лимиты.
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("throttled") => Self::Throttled,
            Some("rejected") => Self::Rejected,
            Some("unpaid") => Self::Unpaid,
            _ => Self::Limit,
        }
    }

    /// Пауза сама кончится — клиенту стоит подождать (429 с Retry-After), а не получить 503.
    pub fn waits(self) -> bool {
        matches!(self, Self::Limit | Self::Throttled)
    }

    /// Приговор ключу: сам не оживёт, поэтому причина важнее, чем более длинная пауза лимита.
    pub fn verdict(self) -> bool {
        matches!(self, Self::Rejected | Self::Unpaid)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Pause {
    pub label: String,
    pub until_ms: i64,
    pub kind: PauseKind,
    pub reason: String,
}

// Ключ паузы (в `Paused`) — сам секрет, но Pause его не хранит; Debug без подписи ключа не нужен.
impl std::fmt::Debug for Pause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pause").field("label", &self.label).field("until_ms", &self.until_ms).field("kind", &self.kind).field("reason", &self.reason).finish()
    }
}

/// Ключ на паузе: ключ → до какого момента и почему.
pub type Paused = HashMap<String, Pause>;

/// Ключ для показа: только последние 4 знака (по символам — ключ может быть любым текстом).
pub fn mask(key: &str) -> String {
    // Короткий «ключ» хвостом выдал бы себя целиком.
    if key.trim().chars().count() <= 8 {
        return "…".into();
    }
    let tail: Vec<char> = key.trim().chars().rev().take(4).collect();
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

/// Текст ошибки провайдера — в журнал, статус и уведомления: наш ключ и любые `sk-…` маской
/// (провайдер может вернуть ключ эхом в «Invalid API key …»).
pub fn redact(text: &str, key: &str) -> String {
    let key = key.trim();
    // Ключ короче 4 знаков не ищем: замена «x» или «ab» во всём тексте калечила бы сообщение, а такой ключ не секрет.
    let text = if key.chars().count() >= 4 { text.replace(key, &mask(key)) } else { text.to_string() };
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(i) = rest.find("sk-") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 3..];
        let n = tail.bytes().take_while(|b| b.is_ascii_alphanumeric() || b"-_.~/+=".contains(b)).count();
        // «sk-» внутри слова («task-management.xlsx», «task-sk-x») — не ключ, если хвост короче ключа.
        // После «/», «.», «_», «=», «:» — маскируем: ключ в URL и пути — самое частое эхо,
        // а лишняя маска в имени файла безобиднее утечки.
        let inside_word = out.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '-');
        out.push_str(if n >= 8 && (!inside_word || n >= 20) { "sk-…" } else { &rest[i..i + 3 + n] });
        rest = &tail[n..];
    }
    out.push_str(rest);
    out
}

/// Сколько секунд до HTTP-даты IMF-fixdate («Wed, 21 Oct 2026 07:28:00 GMT»).
fn http_date_in(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, zone] = parts.as_slice() else { return None };
    if !zone.eq_ignore_ascii_case("GMT") && !zone.eq_ignore_ascii_case("UTC") {
        return None;
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? + 1;
    let iso = format!("{year}-{month:02}-{:02}T{time}Z", day.parse::<u32>().ok()?);
    let at = crate::util::parse_iso8601(&iso)?;
    // Вверх: дата через 800 мс давала 0 и выкидывалась — ставили час вместо секунды.
    let ms = at - crate::store::now_ms();
    Some(if ms > 0 { (ms + 999) / 1000 } else { ms / 1000 })
}

/// Ответ OpenCode, после которого ключ надо отложить: (на сколько секунд, причина по-русски).
pub fn pause_for(status: u16, retry_after: Option<&str>, body: &str) -> Option<(i64, PauseKind, String)> {
    // Retry-After бывает секундами или HTTP-датой («Wed, 21 Oct 2026 07:28:00 GMT»).
    let retry = retry_after
        .and_then(|s| s.trim().parse::<i64>().ok().or_else(|| http_date_in(s.trim())))
        .filter(|s| *s > 0);
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match status {
        429 if v["error"]["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("GoUsageLimitError")) => {
            // Без учёта регистра: «Rolling» от провайдера иначе давал паузу на месяц вместо 6 часов.
            let limit = v["metadata"]["limitName"].as_str().unwrap_or("").to_ascii_lowercase();
            // Вариации имени («5h_rolling», «5-hourly», «Weekly-Limit») — по смыслу, а не точной строкой:
            // незнакомое имя 5-часового окна иначе уходило под месячный потолок.
            let limit = if limit.contains("month") {
                "monthly".to_string()
            } else if ["week", "7d"].iter().any(|k| limit.contains(k)) {
                "weekly".to_string()
            } else if ["roll", "hour", "5h", "session"].iter().any(|k| limit.contains(k)) {
                "rolling".to_string()
            } else {
                limit
            };
            let what = match limit.as_str() {
                "weekly" => "недельный лимит".to_string(),
                "monthly" => "месячный лимит".to_string(),
                "rolling" => "5-часовой лимит".to_string(),
                "" => "лимит".to_string(),
                // Текст провайдера идёт в журнал, статус и клиенту: только буквы, цифры, дефис и пробел, коротко.
                other => {
                    let safe: String = other.chars().filter(|c| c.is_alphanumeric() || "-_ ".contains(*c)).take(24).collect();
                    // Одни спецзнаки — не «лимит «»», а просто лимит.
                    if safe.trim().is_empty() { "лимит".to_string() } else { format!("лимит «{}»", safe.trim()) }
                }
            };
            // Потолок по типу окна: Retry-After в миллисекундах или мусорная дата не должны класть
            // 5-часовой лимит на месяц (пауза переживает перезапуск).
            let cap = match limit.as_str() {
                "rolling" => 6 * 3600,
                "weekly" => 8 * 86_400,
                "monthly" => 31 * 86_400,
                // Окно не назвали — мусорный Retry-After (2e9 с) не должен класть ключ на месяц.
                _ => 8 * 86_400,
            };
            // Без Retry-After — час: когда именно сбросится неделя, мы не знаем, а один 429 в час дешевле простоя ключа.
            Some((retry.unwrap_or(3600).clamp(1, cap), PauseKind::Limit, format!("кончился {what}")))
        }
        // Тело могло не дойти — тогда верим Retry-After (он бывает и на неделю), без него — 30 с.
        // Не больше суток: Retry-After от шлюза перед OpenCode бывает мусорным (2e9 с), а долгий лимит
        // OpenCode сообщает как GoUsageLimitError — с ним выше.
        429 => Some((retry.unwrap_or(30).clamp(1, 86_400), PauseKind::Throttled, "OpenCode просит подождать (429)".to_string())),
        // Как 403: 401 от прокси/WAF или смены схемы — не вина ключа, весь пул за запрос не гасим.
        401 if v["error"]["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("AuthError")) => Some((retry.unwrap_or(600).clamp(1, 86_400), PauseKind::Rejected, "ключ отвергнут (401)".to_string())),
        // 403 бывает и от политики/WAF/недоступной модели — это не вина ключа, весь пул гасить нельзя.
        403 if v["error"]["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("AuthError")) => Some((retry.unwrap_or(600).clamp(1, 86_400), PauseKind::Rejected, "ключ отвергнут (403)".to_string())),
        // 402 — только ответ самого OpenCode (JSON или тело не дошло): HTML-страница шлюза/CDN
        // с 402 не повод класть годный ключ на месяц.
        402 if body.trim().is_empty() || v.is_object() => Some((retry.unwrap_or(3600).clamp(1, 31 * 86_400), PauseKind::Unpaid, "у ключа нет оплаченной подписки Go (402)".to_string())),
        _ => None,
    }
}

/// Запас ключа по последним лимитам: самое тесное окно (0–100). Лимиты не загружены — середина.
/// Окно, чей сброс уже прошёл, не в счёт: опрос мог застрять, а лимит давно обнулился.
pub fn headroom(account: &Account, now_ms: i64) -> f64 {
    let Some(usage) = account.last_usage.as_ref() else { return 50.0 };
    let finite: Vec<_> = usage.windows.iter().filter(|w| w.used_percent.is_finite()).collect();
    // Окон нет или все битые — «не знаем», а не «запас 100%» (иначе битый файл делает ключ первым).
    if finite.is_empty() {
        return 50.0;
    }
    // Неудачный опрос хранит окна последнего успеха: окно без срока сброса там могло давно
    // прокрутиться, и ключ навсегда застревал бы последним. Такое окно — «не знаем».
    // Удачный, но старый (больше суток) опрос — то же «окно без срока могло прокрутиться».
    // Отметка из будущего (правленый файл, скачок часов) — не «свежий опрос».
    let ok = matches!(usage.status, crate::model::FetchStatus::Ok) && (0..=86_400_000).contains(&now_ms.saturating_sub(usage.updated_at));
    if !ok && finite.iter().all(|w| w.resets_at.is_none()) {
        return 50.0;
    }
    finite
        .iter()
        .filter(|w| ok || w.resets_at.is_some())
        .filter(|w| w.resets_at.is_none_or(|r| r > now_ms))
        .map(|w| crate::util::remaining_percent(w.used_percent))
        // Выброшенное «не знаем» окно считаем за 50: иначе исчерпанный недельный без срока
        // терялся, и ключ с 5% на 5-часовом вставал первым в пуле.
        .chain((!ok && finite.iter().any(|w| w.resets_at.is_none())).then_some(50.0))
        .reduce(f64::min)
        // Все окна отфильтровались (сроки давно прошли) — «не знаем», а не полный запас.
        .unwrap_or(50.0)
}

/// Ключи из карточек: запасные и те, чьи карточки выключены.
#[derive(Clone, Default)]
pub struct Pool {
    pub keys: Vec<Key>,
    /// Ключи только выключенных карточек: выбранный такой ключ тоже не используется.
    pub off: Vec<String>,
    /// Карточки прочитать не вышло — пул старый или пустой (для честного текста ошибки).
    pub unread: bool,
}

/// Все ключи OpenCode Go из включённых карточек, больше всего запаса — первыми.
pub fn pool(accounts: &[Account]) -> Pool {
    let now_ms = crate::store::now_ms();
    // Дубли ключа убираем до сортировки — по порядку карточек, как окно: иначе подписи расходились бы.
    let mut seen = std::collections::HashSet::new();
    let mut keys: Vec<(f64, Key)> = accounts
        .iter()
        .filter(|a| a.enabled && a.provider == ProviderId::OpenCodeGo)
        .filter_map(|a| {
            let key = a.credentials.get("apiKey")?.trim().to_string();
            (!key.is_empty() && seen.insert(key.clone())).then(|| (headroom(a, now_ms), Key { label: a.label.clone(), key }))
        })
        .collect();
    keys.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let keys: Vec<Key> = keys.into_iter().map(|(_, k)| k).collect();
    let off = accounts
        .iter()
        .filter(|a| !a.enabled && a.provider == ProviderId::OpenCodeGo)
        .filter_map(|a| a.credentials.get("apiKey").map(|k| k.trim().to_string()))
        .filter(|k| !k.is_empty() && seen.insert(k.clone()))
        .collect();
    Pool { keys, off, unread: false }
}

fn free(paused: &Paused, key: &str, now_ms: i64) -> bool {
    paused.get(key).is_none_or(|p| p.until_ms <= now_ms)
}

/// Каким ключом слать: выбранный, если он не на паузе; иначе (при ротации) — первый свободный из запаса.
pub fn pick(selected: &Key, rotate: bool, pool: &[Key], paused: &Paused, now_ms: i64) -> Option<Key> {
    if !selected.key.is_empty() && free(paused, &selected.key, now_ms) {
        return Some(selected.clone());
    }
    if !rotate {
        return None;
    }
    pool.iter().find(|k| k.key != selected.key && free(paused, &k.key, now_ms)).cloned()
}

/// Почему не нашлось ключа — для ошибки и статуса.
pub fn why_none(selected: &Key, paused: &Paused, now_ms: i64, rotate: bool, off: bool) -> String {
    if off {
        let rest = if rotate { "свободных запасных ключей нет" } else { "ротация ключей выключена" };
        return format!("карточка ключа {} выключена, {rest}", selected.label);
    }
    match paused.get(&selected.key).filter(|p| p.until_ms > now_ms) {
        Some(p) => {
            // Тем же счётом, что и строка ключа в окне («пауза · ещё 31д»), а не «ещё 744 ч».
            let left = format!("ещё {}", crate::util::format_reset_countdown(Some(p.until_ms), now_ms).unwrap_or_else(|| "долго".to_string()));
            let rest = if rotate { "свободных запасных ключей нет" } else { "ротация ключей выключена" };
            format!("ключ {} на паузе: {} ({left}), {rest}", p.label, p.reason)
        }
        // Пустой выбранный ключ — главное, что чинить, даже если запасные на паузе.
        None if selected.key.is_empty() => "не выбран ключ OpenCode Go".to_string(),
        // Выбранный свободен, а pick никого не дал: его пауза истекла между выбором и этим текстом.
        None => "свободных ключей OpenCode Go не нашлось — повтори запрос".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountUsage, FetchStatus, RateLimitWindow};

    fn account(label: &str, key: &str, used: &[f64]) -> Account {
        Account {
            id: label.into(),
            provider: ProviderId::OpenCodeGo,
            label: label.into(),
            enabled: true,
            credentials: [("apiKey".to_string(), key.to_string())].into_iter().collect(),
            options: Default::default(),
            created_at: 0,
            last_usage: Some(AccountUsage {
                status: FetchStatus::Ok,
                windows: used
                    .iter()
                    .map(|u| RateLimitWindow { key: "7d".into(), label: "7д".into(), used_percent: *u, window_minutes: 0, resets_at: None, note: None })
                    .collect(),
                plan_type: None,
                notes: vec![],
                error: None,
                updated_at: crate::store::now_ms(),
                last_ok_at: None,
            }),
            selected_model: None,
            selected_window: None,
        }
    }

    fn k(label: &str, key: &str) -> Key {
        Key { label: label.into(), key: key.into() }
    }

    #[test]
    fn исчерпанный_ключ_на_паузу_до_retry_after() {
        let body = r#"{"type":"error","error":{"type":"GoUsageLimitError","message":"Go usage limit exceeded"},"metadata":{"limitName":"weekly"}}"#;
        assert_eq!(pause_for(429, Some("146553"), body), Some((146_553, PauseKind::Limit, "кончился недельный лимит".to_string())));
        assert_eq!(pause_for(429, None, body).unwrap().0, 3600, "без Retry-After — час");
        let auth = r#"{"type":"error","error":{"type":"AuthError","message":"Invalid API key."}}"#;
        assert_eq!(pause_for(401, None, auth), Some((600, PauseKind::Rejected, "ключ отвергнут (401)".to_string())));
        assert_eq!(pause_for(429, Some("5"), "{}").unwrap().0, 5, "просто 429 — сколько просят");
        let secs = pause_for(429, Some("Wed, 21 Oct 2099 07:28:00 GMT"), "{}").unwrap().0;
        assert_eq!(secs, 86_400, "HTTP-дата понимается (далёкая — потолок сутки)");
        assert_eq!(pause_for(429, Some("Wed, 21 Oct 2015 07:28:00 GMT"), "{}").unwrap().0, 30, "прошедшая дата — как без неё");
        assert_eq!(pause_for(402, None, "{}").unwrap().0, 3600, "без оплаты — другой ключ, а не подписка Claude");
        assert_eq!(pause_for(402, None, "").unwrap().1, PauseKind::Unpaid, "тело не дошло — верим статусу");
        assert_eq!(pause_for(402, None, "<html>Payment Required</html>"), None, "402 от шлюза — не приговор ключу");
        assert_eq!(pause_for(403, None, r#"{"error":{"type":"Forbidden"}}"#), None, "403 политики не гасит ключи");
        assert!(pause_for(403, None, auth).is_some(), "403 AuthError — ключ");
        assert_eq!(pause_for(500, Some("5"), body), None, "сбой сервера — не вина ключа");
        let named = |n: &str| pause_for(429, None, &format!(r#"{{"error":{{"type":"GoUsageLimitError"}},"metadata":{{"limitName":"{n}"}}}}"#)).unwrap().2;
        for (name, want) in [("5h_rolling", "5-часовой"), ("5-hourly", "5-часовой"), ("Weekly-Limit", "недельный"), ("monthly", "месячный")] {
            assert!(named(name).contains(want), "{name} → {}", named(name));
        }
    }

    #[test]
    fn ключи_в_тексте_ошибки_маской() {
        let key = "sk-opencode-SECRET1234abcd";
        let text = format!("Invalid API key: {key}; other sk-zzzzzzzzzzzz; short sk-ab");
        let safe = redact(&text, key);
        assert!(!safe.contains("SECRET") && !safe.contains("zzzzzzzz"), "{safe}");
        assert!(safe.contains("sk-ab"), "короткое — не ключ");
        assert_eq!(redact("файл task-sk-management.xlsx и disk-image12345", "x"), "файл task-sk-management.xlsx и disk-image12345", "sk- внутри слова — не ключ");
        for leak in ["https://x.ai/v1/sk-opencode-LIVEKEY000012345678", "file.sk-opencode-LIVEKEY000012345678.tmp", "env_sk-opencode-LIVEKEY000012345678"] {
            assert!(!redact(leak, "x").contains("LIVEKEY"), "чужой ключ после разделителя утёк: {leak}");
        }
        assert_eq!(mask("🙂x🙂x🙂x🙂x🙂"), "…x🙂x🙂", "не байты, а символы — без паники");
        assert_eq!(mask("abcdefghi"), "…fghi");
        assert_eq!(mask("abcd"), "…", "короткий ключ хвостом не выдаём");
    }

    #[test]
    fn запас_по_самому_тесному_окну_и_без_дублей() {
        let mut off = account("выкл", "K0", &[0.0]);
        off.enabled = false;
        let accounts = vec![
            account("#2", "K2", &[0.0, 100.0]),
            account("#3", "K3", &[2.0, 49.0]),
            off,
            account("#4", "K4", &[0.0, 41.0]),
            account("#4 копия", "K4", &[0.0, 41.0]),
        ];
        let labels: Vec<String> = pool(&accounts).keys.into_iter().map(|k| k.label).collect();
        assert_eq!(pool(&accounts).off, vec!["K0".to_string()], "выключенная карточка — в off");
        assert_eq!(labels, ["#4", "#3", "#2"]);
    }

    #[test]
    fn выбор_ключа_с_паузами() {
        let pool = vec![k("#4", "K4"), k("#3", "K3"), k("#2", "K2")];
        let sel = k("#2", "K2");
        let mut paused = Paused::new();
        assert_eq!(pick(&sel, true, &pool, &paused, 1000), Some(sel.clone()), "свободен — он");
        paused.insert("K2".into(), Pause { label: "#2".into(), until_ms: 5000, kind: PauseKind::Limit, reason: "кончился недельный лимит".into() });
        assert_eq!(pick(&sel, true, &pool, &paused, 1000), Some(k("#4", "K4")), "на паузе — с наибольшим запасом");
        assert_eq!(pick(&sel, false, &pool, &paused, 1000), None, "без ротации — никого");
        assert_eq!(pick(&sel, true, &pool, &paused, 5000), Some(sel.clone()), "пауза кончилась — вернулся");
        paused.insert("K4".into(), Pause { label: "#4".into(), until_ms: 9000, kind: PauseKind::Throttled, reason: "x".into() });
        paused.insert("K3".into(), Pause { label: "#3".into(), until_ms: 9000, kind: PauseKind::Throttled, reason: "x".into() });
        assert_eq!(pick(&sel, true, &pool, &paused, 1000), None);
        assert_eq!(why_none(&sel, &paused, 1000, true, false), "ключ #2 на паузе: кончился недельный лимит (ещё <1м), свободных запасных ключей нет");
        assert!(why_none(&sel, &paused, 1000, false, false).ends_with("ротация ключей выключена"));
    }

    #[test]
    fn запас_ключа_по_окнам_и_сбросам() {
        let now = crate::store::now_ms();
        let with = |status: FetchStatus, updated_at: i64, windows: &[(f64, Option<i64>)]| {
            let mut a = account("#1", "K1", &[]);
            let u = a.last_usage.as_mut().unwrap();
            u.status = status;
            u.updated_at = updated_at;
            u.windows = windows
                .iter()
                .map(|(used, resets_at)| RateLimitWindow { key: "w".into(), label: "w".into(), used_percent: *used, window_minutes: 0, resets_at: *resets_at, note: None })
                .collect();
            a
        };
        assert_eq!(headroom(&with(FetchStatus::Ok, now, &[]), now), 50.0, "окон нет — не знаем");
        assert_eq!(headroom(&with(FetchStatus::Error, now, &[(10.0, None)]), now), 50.0, "сбой, сроков нет — не знаем");
        assert_eq!(headroom(&with(FetchStatus::Error, now, &[(90.0, Some(now - 1))]), now), 50.0, "сбой, сброс прошёл — не знаем");
        assert_eq!(headroom(&with(FetchStatus::Error, now, &[(70.0, Some(now + 60_000))]), now), 30.0, "сбой, но сброс впереди — окно живо");
        assert_eq!(headroom(&with(FetchStatus::Ok, now, &[(20.0, None), (70.0, Some(now + 60_000))]), now), 30.0, "свежий опрос — самое тесное окно");
        assert_eq!(headroom(&with(FetchStatus::Ok, now, &[(90.0, Some(now - 1))]), now), 50.0, "все сроки прошли — не знаем");
        assert_eq!(headroom(&with(FetchStatus::Ok, now - 2 * 86_400_000, &[(20.0, None)]), now), 50.0, "опрос старше суток — не знаем");
    }
}
