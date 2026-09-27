//! Настройки прокси: `<data_dir>/proxy.json` (0600 — там ключ OpenCode Go).
//! Окно пишет, прокси перечитывает на лету по mtime.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MODELS: [&str; 3] = ["deepseek-v4.1-flash", "space-bunny-free", "muse-spark-1.3-contributor"];
/// Модели, которые OpenCode Go отдаёт в формате Claude напрямую (замер 26.09); остальные — через перевод в OpenAI.
pub const ANTHROPIC_NATIVE: [&str; 2] = ["deepseek-v4.1-flash", "space-bunny-free"];
const _: () = {
    let mut i = 0;
    while i < ANTHROPIC_NATIVE.len() {
        let (a, mut j, mut found) = (ANTHROPIC_NATIVE[i].as_bytes(), 0, false);
        while j < MODELS.len() {
            let b = MODELS[j].as_bytes();
            if a.len() == b.len() {
                let mut k = 0;
                while k < a.len() && a[k] == b[k] {
                    k += 1;
                }
                found |= k == a.len();
            }
            j += 1;
        }
        assert!(found, "ANTHROPIC_NATIVE должен быть подмножеством MODELS");
        i += 1;
    }
};
pub const EFFORTS: [&str; 4] = ["", "low", "high", "max"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyConfig {
    /// false — чистый проход на Anthropic, ничего не подменяем.
    pub enabled: bool,
    // 70000 или строка от ручной правки не должны превращать весь файл в «битый» — берём порт по умолчанию.
    #[serde(deserialize_with = "lenient_port")]
    pub port: u16,
    /// Ключ OpenCode Go (из карточки аккаунта в окне).
    pub api_key: String,
    /// Подпись аккаунта для окна (сам ключ не показываем).
    pub account_label: String,
    pub model: String,
    /// "" — как просит Claude Code; иначе low/high/max в `output_config.effort`.
    pub effort: String,
    /// Какие модели Claude подменять: части имени через «|» (по умолчанию haiku).
    pub match_models: String,
    /// Подменять только запросы с инструментами (субагенты), не служебные вызовы haiku.
    pub require_tools: bool,
    /// Сбой OpenCode до начала ответа → тот же запрос на настоящую модель Claude.
    pub fallback: bool,
    /// Кончился лимит выбранного ключа (или ключ отвергнут) → другой ключ OpenCode Go из карточек.
    pub rotate: bool,
    pub anthropic_base: String,
    pub opencode_base: String,
    /// Ключи, которых эта версия не знает (новая версия, опечатка): сохранение их не стирает.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8479,
            api_key: String::new(),
            account_label: String::new(),
            model: MODELS[0].to_string(),
            effort: "max".to_string(),
            match_models: "haiku".to_string(),
            require_tools: true,
            fallback: true,
            rotate: true,
            anthropic_base: "https://api.anthropic.com".to_string(),
            opencode_base: "https://opencode.ai/zen/go/v1".to_string(),
            extra: serde_json::Map::new(),
        }
    }
}

/// Предупреждение о правке файла — один раз на текст: окно перечитывает конфиг каждые 3 с,
/// и опечатка в proxy.json иначе сыпала бы в журнал тысячи одинаковых строк в час.
fn warn_once(message: String) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    // Значение поля — из файла: ключ, вставленный не в то поле, не должен уйти в журнал открытым.
    let message = crate::proxy::keys::redact(&message, "");
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    let seen = seen.get_or_insert_with(Default::default);
    // Служба живёт неделями: каждое новое битое значение — новая строка в наборе. Потолок, а не рост без края;
    // после сброса повторится разве что одно предупреждение.
    if seen.len() >= 64 {
        seen.clear();
    }
    if seen.insert(message.clone()) {
        eprintln!("{message}");
    }
}

/// Значение поля для предупреждения: коротко — длинная строка (скорее всего вставленный не туда ключ)
/// показывается началом и длиной.
fn shown(v: &str) -> String {
    if v.chars().count() <= 24 {
        format!("{v:?}")
    } else {
        format!("«{}…» ({} знаков)", v.chars().take(6).collect::<String>(), v.chars().count())
    }
}

fn lenient_port<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let n = v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
    let port = n.and_then(|n| u16::try_from(n).ok());
    if port.is_none() {
        warn_once(format!("[subbar] port {} в proxy.json не годится — беру {}", v.as_str().map(shown).unwrap_or_else(|| v.to_string()), ProxyConfig::default().port));
    }
    Ok(port.unwrap_or_else(|| ProxyConfig::default().port))
}

impl ProxyConfig {
    /// Незнакомая модель (записала версия новее) идёт напрямую, как семейство deepseek:
    /// перевод в OpenAI нужен только тем, про кого точно известно, что формат Claude они не понимают.
    pub fn is_native(&self) -> bool {
        ANTHROPIC_NATIVE.contains(&self.model.as_str()) || !MODELS.contains(&self.model.as_str())
    }
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SUBBAR_PROXY_CONFIG") {
        if !p.trim().is_empty() {
            // Пробел от квотинга в plist/.env давал «файла нет» и молча дефолты.
            return PathBuf::from(p.trim());
        }
    }
    crate::store::data_dir().join("proxy.json")
}

/// Правила подмены: через «|», каждое про haiku или sonnet. Запятая или пробел внутри —
/// не разделитель: «haiku,sonnet» ни с одной моделью не совпадёт, и подмена молча выключится.
pub fn valid_rules(rules: &str) -> bool {
    // Пустые части матчер пропускает («haiku|sonnet|» работает) — и валидатор тоже, лишь бы была хоть одна.
    let parts: Vec<String> = rules.to_ascii_lowercase().split('|').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    !parts.is_empty() && parts.iter().all(|p| valid_rule(p))
}

/// Правило ищется подстрокой в имени модели (claude-haiku-4-5-…), поэтому слово haiku/sonnet должно стоять
/// целиком, между дефисами или краями: «haikux» прошло бы проверку и не совпало бы ни с одной моделью.
/// Точки в именах моделей Claude нет («claude-3.5-haiku» не совпал бы ни с чем), а два семейства в одном
/// правиле («haiku-sonnet» — опечатка вместо «|») тоже не встречаются ни в одном имени.
fn valid_rule(p: &str) -> bool {
    if !p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return false;
    }
    // Дефис по краю («haiku-») не совпадёт ни с одним именем: слово ищется целиком, до конца имени.
    if p.contains("haiku") && p.contains("sonnet") || p.starts_with('-') || p.ends_with('-') {
        return false;
    }
    ["haiku", "sonnet"].iter().any(|w| {
        p.match_indices(w).any(|(i, _)| {
            let before = p[..i].bytes().last();
            let after = p[i + w.len()..].bytes().next();
            before.is_none_or(|b| b == b'-') && after.is_none_or(|b| b == b'-')
        })
    })
}

/// Адрес http(s) с хостом. Схема без хоста («https://») — та же поломка на каждом запросе, что и пустой.
/// Общий для CLI и чтения файла: иначе CLI печатал «сохранено», а прокси молча брал умолчание.
/// Схема без учёта регистра, хост обязателен, пробелов внутри нет — иначе каждый запрос падал бы «builder error».
pub fn valid_url(u: &str) -> bool {
    if u.chars().any(char::is_whitespace) {
        return false;
    }
    // Query или фрагмент в базе съели бы путь запроса: «…com?x=1» + «/v1/messages» ушло бы целиком в query.
    reqwest::Url::parse(u).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|h| !h.is_empty()) && url.query().is_none() && url.fragment().is_none()
            // Логин:пароль в адресе ушёл бы на хост и в текст ошибок reqwest.
            && url.username().is_empty() && url.password().is_none()
    })
}

/// Похоже на имя модели: незнакомое такое оставляем — его могла записать версия новее, где список моделей шире.
pub fn plausible_model(model: &str) -> bool {
    model.len() <= 100
        && model.bytes().any(|b| b.is_ascii_alphanumeric())
        && model.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._".contains(&b))
}

/// То, что CLI отверг бы, из файла (ручная правка, старая версия) тоже не берём — берём умолчание:
/// пустой/не-http адрес ронял бы 502 на КАЖДЫЙ запрос, `matchModels: claude` уводил бы основную сессию.
fn sanitized(mut cfg: ProxyConfig) -> ProxyConfig {
    let def = ProxyConfig::default();
    // Пробел от ручной правки ронял бы каждый запрос «builder error» без адреса в тексте.
    cfg.anthropic_base = cfg.anthropic_base.trim().to_string();
    cfg.opencode_base = cfg.opencode_base.trim().to_string();
    cfg.model = cfg.model.trim().to_string();
    cfg.effort = cfg.effort.trim().to_string();
    cfg.api_key = cfg.api_key.trim().to_string();
    cfg.account_label = cfg.account_label.trim().to_string();
    // Порт 0 — это «любой свободный»: всё вокруг (строка Claude Code, окно, proxy-check) искало бы прокси на :0.
    // Тестам он нужен — им разрешено через SUBBAR_ALLOW_PORT0.
    if cfg.port == 0 && std::env::var_os("SUBBAR_ALLOW_PORT0").is_none() {
        cfg.port = def.port;
    }
    if !valid_url(&cfg.anthropic_base) {
        cfg.anthropic_base = def.anthropic_base.clone();
    }
    if !valid_url(&cfg.opencode_base) {
        cfg.opencode_base = def.opencode_base.clone();
    }
    if !valid_rules(&cfg.match_models) {
        // Оставляем годные части: «haiku|fable» от новой версии не должно молча превращаться в голое «haiku» с потерей
        // остального — но и мусор в матчер не пускаем.
        let good: Vec<String> = cfg.match_models.split('|').map(|p| p.trim().to_ascii_lowercase()).filter(|p| valid_rule(p)).collect();
        let fixed = if good.is_empty() { def.match_models.clone() } else { good.join("|") };
        warn_once(format!("[subbar] matchModels {} в proxy.json не годится — беру «{fixed}»", shown(&cfg.match_models)));
        cfg.match_models = fixed;
    }
    // Битое имя модели уходило бы в OpenCode, получало 4xx — и каждый субагент молча ехал на подписку (fallback).
    // Незнакомое, но похожее на имя, оставляем: его могла записать версия новее, где список моделей шире.
    if !MODELS.contains(&cfg.model.as_str()) && !plausible_model(&cfg.model) {
        warn_once(format!("[subbar] модель {} в proxy.json неизвестна — беру «{}»", shown(&cfg.model), def.model));
        cfg.model = def.model.clone();
    }
    if !EFFORTS.contains(&cfg.effort.as_str()) {
        // Опечатка не должна поднимать расход до max — «как просит Claude Code».
        warn_once(format!("[subbar] effort {} в proxy.json неизвестен — беру «как просит Claude Code»", shown(&cfg.effort)));
        cfg.effort = String::new();
    }
    cfg
}

/// Строгое чтение: битый файл — ошибка (прокси останется на прежних настройках, CLI откажется писать).
/// Нет файла — настройки по умолчанию.
pub fn try_load(path: &std::path::Path) -> Result<ProxyConfig, String> {
    // FIFO или каталог на месте файла: чтение FIFO повесило бы прокси, который перечитывает конфиг на каждом запросе.
    // Открываем без блокировки и проверяем уже открытый дескриптор: подмена на FIFO между проверкой и чтением не повесит.
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let read = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("это не обычный файл"));
            }
            // Конфиг — пара сотен байт: подсунутый гигантский файл не тащим в память целиком.
            let mut raw = String::new();
            file.take(1 << 20).read_to_string(&mut raw).map(|_| raw)
        });
    match read {
        Ok(raw) => serde_json::from_str::<ProxyConfig>(&raw).map(sanitized).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProxyConfig::default()),
        Err(e) => Err(e.to_string()),
    }
}

/// Атомарная запись с правами 0600.
pub fn save(path: &std::path::Path, cfg: &ProxyConfig) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    // «proxy.json» без каталога: parent() — пустой путь, mkdir("") падал ENOENT.
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    if !dir.exists() {
        // В файле ключ — новый каталог только для себя (как каталог данных окна).
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    let write = || -> std::io::Result<()> {
        // Хвост от убитого процесса с тем же PID (права могли быть любыми) убираем; create_new +
        // O_NOFOLLOW — чтобы ключ не ушёл по подложенной ссылке и права 0600 точно применились.
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        // Без fsync каталога переименование может не пережить сбой питания — прокси встал бы без ключа.
        // Файл уже заменён: сбой fsync — не «не сохранил», иначе окно пропустило бы перезапуск службы.
        if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            eprintln!("[subbar] proxy.json записан, но fsync каталога не удался: {e}");
        }
        Ok(())
    };
    // Сорвалось (нет места на диске) — не оставлять рядом копию с ключом открытым текстом.
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn правила_которые_ничего_не_поймают_отвергаются() {
        assert!(valid_rules("haiku"));
        assert!(valid_rules("haiku|sonnet"));
        assert!(valid_rules("claude-3-5-haiku"));
        assert!(!valid_rules("claude-3.5-haiku"));
        assert!(!valid_rules("haiku-sonnet"));
        assert!(!valid_rules("haikux"));
        assert!(!valid_rules("claude"));
    }

    #[test]
    fn из_файла_берутся_годные_части_правила_и_модель() {
        let mut cfg = ProxyConfig { match_models: "sonnet|claude".into(), model: "GPT 5".into(), ..ProxyConfig::default() };
        cfg = sanitized(cfg);
        assert_eq!(cfg.match_models, "sonnet");
        assert_eq!(cfg.model, ProxyConfig::default().model);
        let cfg = sanitized(ProxyConfig { model: "future-model-2".into(), ..ProxyConfig::default() });
        assert_eq!(cfg.model, "future-model-2");
    }
}
