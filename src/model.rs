use serde::{Deserialize, Serialize};

/// Сервисы подписок, чей расход умеет читать SubBar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    Codex,
    // Слитное написание — как в CLI и README: руками поправленный state.json иначе не читался бы целиком.
    #[serde(alias = "opencode-go", alias = "opencode_go", alias = "open_code_go")]
    OpenCodeGo,
    #[serde(alias = "commandcode", alias = "command_code")]
    CommandCode,
    Devin,
    Claude,
    Custom,
}

impl ProviderId {
    pub const ALL: [ProviderId; 6] = [
        ProviderId::Codex,
        ProviderId::Claude,
        ProviderId::OpenCodeGo,
        ProviderId::CommandCode,
        ProviderId::Devin,
        ProviderId::Custom,
    ];

    /// Имя для окна и для выбора сервиса при добавлении.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderId::Codex => "ChatGPT / Codex",
            ProviderId::OpenCodeGo => "OpenCode Go",
            ProviderId::CommandCode => "Command Code",
            ProviderId::Devin => "Devin",
            ProviderId::Claude => "Claude (Pro/Max)",
            ProviderId::Custom => "Своя подписка (JSON API)",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            ProviderId::Codex => "ChatGPT",
            ProviderId::OpenCodeGo => "OpenCode",
            ProviderId::CommandCode => "Command Code",
            ProviderId::Devin => "Devin",
            ProviderId::Claude => "Claude",
            ProviderId::Custom => "Своя подписка",
        }
    }

    pub fn color(self) -> (f64, f64, f64) {
        match self {
            ProviderId::Codex => (0.063, 0.639, 0.498),
            ProviderId::OpenCodeGo => (0.659, 0.333, 0.969),
            ProviderId::CommandCode => (0.388, 0.400, 0.945),
            ProviderId::Devin => (0.055, 0.647, 0.914),
            ProviderId::Claude => (0.851, 0.467, 0.341),
            ProviderId::Custom => (0.392, 0.455, 0.545),
        }
    }
}

/// Поля ключей, которые нужны сервису, — из них строится форма добавления и правки.
pub struct CredentialField {
    pub key: &'static str,
    pub label: &'static str,
    pub placeholder: &'static str,
    pub hint: &'static str,
    pub secret: bool,
    pub optional: bool,
}

pub fn credential_fields(provider: ProviderId) -> &'static [CredentialField] {
    macro_rules! field {
        ($key:expr, $label:expr, $placeholder:expr, $hint:expr, $secret:expr, $optional:expr) => {
            CredentialField {
                key: $key,
                label: $label,
                placeholder: $placeholder,
                hint: $hint,
                secret: $secret,
                optional: $optional,
            }
        };
    }
    match provider {
        ProviderId::Codex => &[
            field!(
                "accessToken",
                "Access token",
                "из ~/.codex/auth.json",
                "Можно не заполнять — возьмётся из ~/.codex/auth.json",
                true,
                true
            ),
            field!(
                "accountId",
                "ChatGPT Account ID",
                "определяется автоматически",
                "",
                false,
                true
            ),
            field!(
                "codexHome",
                "CODEX_HOME",
                "~/.codex",
                "Другой каталог, если аккаунтов Codex несколько",
                false,
                true
            ),
        ],
        ProviderId::OpenCodeGo => &[field!(
            "apiKey",
            "API key",
            "ключ opencode.ai",
            "Сколько ключей — столько карточек, добавляй свободно",
            true,
            false
        )],
        ProviderId::CommandCode => &[field!(
            "apiKey",
            "API key",
            "ключ Command Code",
            "Можно не заполнять — возьмётся из ~/.commandcode/auth.json",
            true,
            true
        )],
        ProviderId::Devin => &[],
        ProviderId::Claude => &[
            field!(
                "accessToken",
                "Access token",
                "из Keychain «Claude Code-credentials»",
                "Можно не заполнять — читается из Keychain",
                true,
                true
            ),
            field!(
                "refreshToken",
                "Refresh token",
                "из Keychain",
                "Позволяет приложению обновлять токен самому",
                true,
                true
            ),
        ],
        ProviderId::Custom => &[
            field!(
                "url",
                "URL",
                "https://example.com/api/usage",
                "",
                false,
                false
            ),
            field!(
                "headerName",
                "Заголовок авторизации",
                "Authorization",
                "Например, Authorization, x-api-key, Cookie",
                false,
                true
            ),
            field!(
                "headerValue",
                "Значение заголовка",
                "Bearer sk-...",
                "",
                true,
                true
            ),
            field!(
                "usedPath",
                "Путь к «использовано, %»",
                "data.usage.percent",
                "Точечный путь в JSON-ответе",
                false,
                true
            ),
            field!(
                "remainingPath",
                "Путь к «осталось, %»",
                "data.usage.remaining",
                "Точечный путь в JSON-ответе",
                false,
                true
            ),
            field!(
                "resetPath",
                "Путь к времени сброса",
                "data.usage.resets_at",
                "ISO-строка или unix-время",
                false,
                true
            ),
            field!("windowMinutes", "Длина окна (мин)", "300", "", false, true),
        ],
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    pub key: String,
    pub label: String,
    pub used_percent: f64,
    pub window_minutes: i64,
    #[serde(default)]
    pub resets_at: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchStatus {
    Ok,
    Error,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub status: FetchStatus,
    pub windows: Vec<RateLimitWindow>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    // Без срока кэш опроса всё равно годен — не терять из-за одного поля все проценты.
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub last_ok_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// Пустой или битый id store заменит новым — ручная правка не должна уводить весь state в corrupt.
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: String,
    pub provider: ProviderId,
    /// Пустое название store заполнит сам — без него файл не должен считаться битым.
    #[serde(default, deserialize_with = "lenient_string")]
    pub label: String,
    /// Ручная правка («"true"», 1) не должна уводить весь state.json в corrupt.
    #[serde(default = "default_true", deserialize_with = "lenient_bool_on")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "lenient_map")]
    pub credentials: std::collections::BTreeMap<String, String>,
    #[serde(default, deserialize_with = "lenient_map")]
    pub options: std::collections::BTreeMap<String, String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub created_at: i64,
    // Это кэш последнего опроса: битое поле в нём не должно ронять весь state с ключами.
    #[serde(default, deserialize_with = "lenient_usage")]
    pub last_usage: Option<AccountUsage>,
    /// Фильтр модели — «all» или id конкретной модели.
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub selected_model: Option<String>,
    /// Фильтр окна — «all» или ключ окна вроде «5h», «7d», «30d».
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub selected_window: Option<String>,
}

/// Настройки карточки, которые меняют только вид (закрепление и его порядок), а не то, что запрашивать у сервиса.
pub const PRESENTATION_OPTIONS: [&str; 2] = ["pinned", "pinOrder"];

impl Account {
    /// Закреплена: сверху списка, в своём порядке; верхняя из закреплённых — в строке меню.
    pub fn pinned(&self) -> bool {
        self.options.get("pinned").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"))
    }

    /// Место среди закреплённых (меньше — выше). Закреплённые до появления порядка — по времени создания.
    pub fn pin_rank(&self) -> i64 {
        self.options.get("pinOrder").and_then(|v| v.parse().ok()).unwrap_or(self.created_at)
    }

    /// Порядок закреплённых: по месту, при равенстве — по названию (стабильно).
    pub fn pin_cmp(&self, other: &Account) -> std::cmp::Ordering {
        self.pin_rank().cmp(&other.pin_rank()).then_with(|| crate::proxy::control::natural_cmp(&self.label, &other.label))
    }

    /// API-ключ, который меню карточки может скопировать.
    ///
    /// Только сервисы, где ключ — простой API-ключ; OAuth-токены
    /// (Codex/Claude) и значения своих заголовков в меню не попадают.
    pub fn copyable_api_key(&self) -> Option<&str> {
        match self.provider {
            ProviderId::OpenCodeGo | ProviderId::CommandCode => self
                .credentials
                .get("apiKey")
                .map(|key| key.trim())
                .filter(|key| !key.is_empty()),
            _ => None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Руками вписанный минус, дробь или строка не должны уводить весь state.json в «битый»:
/// любое число приводим к целому ≥ 0 (дальше store клампит), нечисло — к умолчанию.
fn lenient_seconds<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let n = match &v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    Ok(match n {
        // Дробь меньше секунды — не «выключить таймер» (0), а минимальный шаг.
        Some(x) if x.is_finite() && x > 0.0 && x < 1.0 => 1,
        // Больше суток — опечатка в экспоненте, а не частота: таймер на миллиарды лет молча выключал бы обновление.
        Some(x) if x.is_finite() => x.max(0.0).min(86_400.0) as u64,
        _ => Settings::default().refresh_seconds,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Период автообновления в секундах; 0 выключает таймер.
    #[serde(deserialize_with = "lenient_seconds")]
    pub refresh_seconds: u64,
    /// Показывать «% осталось» вместо «% потрачено».
    #[serde(deserialize_with = "lenient_bool_on")]
    pub show_remaining: bool,
    /// Уведомлять, когда окно переходит этот процент расхода; 0 — выключено.
    #[serde(deserialize_with = "lenient_percent")]
    pub notify_used_percent: f64,
    #[serde(deserialize_with = "lenient_bool")]
    pub launch_at_login: bool,
    #[serde(deserialize_with = "lenient_bool_on")]
    pub show_tray_percent: bool,
}

/// Как `lenient_seconds`: чужой тип у одного поля не должен уводить весь state.json в «битый».
fn lenient_percent<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let n = match &v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    // Вне 0..=100 поле настроек не проходило бы проверку, и экран «Настройки» не закрывался бы.
    Ok(n.filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 100.0)).unwrap_or(Settings::default().notify_used_percent))
}

/// Непонятное значение (null, «вкл») — `fallback`, то есть умолчание поля, а не всегда «выключено».
fn bool_or(v: serde_json::Value, fallback: bool) -> bool {
    match v {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => fallback,
        },
        _ => fallback,
    }
}

fn lenient_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(bool_or(serde_json::Value::deserialize(d)?, false))
}

/// Для полей, включённых по умолчанию (карточка, процент в строке меню).
fn lenient_bool_on<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(bool_or(serde_json::Value::deserialize(d)?, true))
}

/// «"version": "1"» от ручной правки — не повод уводить весь файл в corrupt.
fn lenient_version<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let n = v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
    Ok(n.and_then(|n| u32::try_from(n).ok()).unwrap_or(State::default().version))
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            refresh_seconds: 300,
            show_remaining: true,
            notify_used_percent: 90.0,
            launch_at_login: false,
            show_tray_percent: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    #[serde(deserialize_with = "lenient_version")]
    pub version: u32,
    // Контейнерный default спасает только от отсутствия поля, а `null` уводил бы весь файл в corrupt.
    #[serde(deserialize_with = "null_default")]
    pub settings: Settings,
    #[serde(deserialize_with = "null_default")]
    pub accounts: Vec<Account>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: 1,
            settings: Settings::default(),
            accounts: Vec::new(),
        }
    }
}

/// Ключи, найденные на этой машине, — готовы к импорту карточкой.
#[derive(Debug, Clone)]
pub struct DetectedCredential {
    pub provider: ProviderId,
    pub label: String,
    pub credentials: Vec<(String, String)>,
    pub options: Vec<(String, String)>,
    pub source: String,
}

/// Итог одного опроса сервиса.
#[derive(Debug, Clone)]
pub struct ProviderOutcome {
    pub status: FetchStatus,
    pub windows: Vec<RateLimitWindow>,
    pub plan_type: Option<String>,
    pub notes: Vec<String>,
    pub error: Option<String>,
    pub credentials_patch: Vec<(String, String)>,
}

impl ProviderOutcome {
    pub fn ok(windows: Vec<RateLimitWindow>) -> Self {
        ProviderOutcome {
            status: FetchStatus::Ok,
            windows,
            plan_type: None,
            notes: Vec::new(),
            error: None,
            credentials_patch: Vec::new(),
        }
    }

    pub fn with_plan(mut self, plan: Option<String>) -> Self {
        self.plan_type = plan;
        self
    }

    pub fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = notes;
        self
    }

    pub fn patch(mut self, patch: Vec<(String, String)>) -> Self {
        self.credentials_patch = patch;
        self
    }

    pub fn fail(message: impl Into<String>) -> Self {
        ProviderOutcome {
            status: FetchStatus::Error,
            windows: Vec::new(),
            plan_type: None,
            notes: Vec::new(),
            error: Some(message.into()),
            credentials_patch: Vec::new(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        ProviderOutcome {
            status: FetchStatus::Unavailable,
            windows: Vec::new(),
            plan_type: None,
            notes: Vec::new(),
            error: Some(message.into()),
            credentials_patch: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn account(provider: ProviderId, credentials: &[(&str, &str)]) -> Account {
        Account {
            id: "test".to_string(),
            provider,
            label: "Test".to_string(),
            enabled: true,
            credentials: credentials
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            options: BTreeMap::new(),
            created_at: 0,
            last_usage: None,
            selected_model: None,
            selected_window: None,
        }
    }

    #[test]
    fn only_api_key_providers_expose_a_copyable_secret() {
        assert_eq!(
            account(ProviderId::OpenCodeGo, &[("apiKey", "sk-opencode")]).copyable_api_key(),
            Some("sk-opencode")
        );
        assert_eq!(
            account(ProviderId::CommandCode, &[("apiKey", "cc-key")]).copyable_api_key(),
            Some("cc-key")
        );

        // OAuth-токены и значения своих заголовков — не API-ключи.
        assert_eq!(
            account(ProviderId::Codex, &[("accessToken", "tok")]).copyable_api_key(),
            None
        );
        assert_eq!(
            account(ProviderId::Claude, &[("accessToken", "tok")]).copyable_api_key(),
            None
        );
        assert_eq!(
            account(ProviderId::Custom, &[("headerValue", "Bearer x")]).copyable_api_key(),
            None
        );

        // Пустой или отсутствующий ключ не должен класть в буфер пустую строку.
        assert_eq!(
            account(ProviderId::OpenCodeGo, &[("apiKey", "   ")]).copyable_api_key(),
            None
        );
        assert_eq!(account(ProviderId::OpenCodeGo, &[]).copyable_api_key(), None);
    }
}

/// Ручная правка state.json: `null`, число или `true` вместо строки не должны ронять весь файл с ключами.
fn scalar_string(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn lenient_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(scalar_string(serde_json::Value::deserialize(d)?).unwrap_or_default())
}

fn lenient_opt_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(scalar_string(serde_json::Value::deserialize(d)?))
}

fn null_default<'de, D: serde::Deserializer<'de>, T: Default + serde::Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn lenient_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    // Как lenient_seconds: число строкой («"1700000000"») — тоже число.
    let v = match v {
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        v => v.as_f64(),
    };
    Ok(v.filter(|v| v.is_finite()).map(|v| v as i64).unwrap_or_default())
}

fn lenient_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<std::collections::BTreeMap<String, String>, D::Error> {
    let serde_json::Value::Object(map) = serde_json::Value::deserialize(d)? else {
        return Ok(Default::default());
    };
    Ok(map.into_iter().filter_map(|(k, v)| scalar_string(v).map(|v| (k, v))).collect())
}

fn lenient_usage<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<AccountUsage>, D::Error> {
    let mut v = <serde_json::Value as serde::Deserialize>::deserialize(d)?;
    if let Ok(usage) = serde_json::from_value(v.clone()) {
        return Ok(Some(usage));
    }
    // Одно битое окно не должно стирать весь кэш: выкидываем только его.
    if let Some(windows) = v.get_mut("windows").and_then(|w| w.as_array_mut()) {
        windows.retain(|w| serde_json::from_value::<RateLimitWindow>(w.clone()).is_ok());
    }
    Ok(serde_json::from_value(v).ok())
}

#[cfg(test)]
mod lenient_tests {
    use super::*;

    #[test]
    fn broken_window_drops_only_itself() {
        let raw = r#"{"id":"a","provider":"claude","label":"x","lastUsage":{"status":"ok","updatedAt":1,"windows":[{"key":"w","label":"5ч","usedPercent":"много"},{"key":"d","label":"7д","usedPercent":40,"windowMinutes":10080}]}}"#;
        let account: Account = serde_json::from_str(raw).unwrap();
        let usage = account.last_usage.expect("кэш уцелел");
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].key, "d");
    }

    #[test]
    fn hand_edited_account_fields_do_not_break_state() {
        let raw = r#"{"id":"a","provider":"claude","label":null,"credentials":{"apiKey":123,"x":null},"options":{"pinned":true},"createdAt":null}"#;
        let account: Account = serde_json::from_str(raw).unwrap();
        assert_eq!(account.label, "");
        assert_eq!(account.credentials.get("apiKey").map(String::as_str), Some("123"));
        assert!(!account.credentials.contains_key("x"));
        assert!(account.pinned());
    }

    #[test]
    fn null_containers_and_typed_filters_do_not_break_state() {
        let state: State = serde_json::from_str(r#"{"settings":null,"accounts":null}"#).unwrap();
        assert!(state.accounts.is_empty());
        let raw = r#"{"id":"a","provider":"claude","selectedModel":5,"createdAt":"1700000000"}"#;
        let account: Account = serde_json::from_str(raw).unwrap();
        assert_eq!(account.selected_model.as_deref(), Some("5"));
        assert_eq!(account.created_at, 1_700_000_000);
    }

    #[test]
    fn кривой_интервал_не_ломает_настройки() {
        for (raw, want) in [("-1", 0), ("300.5", 300), ("\"120\"", 120), ("null", 300)] {
            let s: super::Settings = serde_json::from_str(&format!("{{\"refreshSeconds\":{raw}}}")).unwrap();
            assert_eq!(s.refresh_seconds, want, "{raw}");
        }
    }
}
