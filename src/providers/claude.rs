use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::model::{Account, DetectedCredential, ProviderId, ProviderOutcome, RateLimitWindow};
use crate::providers::{request_json, HttpError, HttpRequest};
use crate::util::parse_reset_at;

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
// Публичный OAuth client id Claude Code (то же значение, что в самом Claude Code).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

struct ClaudeSource {
    kind: SourceKind,
    oauth: Value,
}

enum SourceKind {
    Keychain { account: Option<String> },
    File { path: String },
}

enum PersistOutcome {
    Saved,
    SourceChanged {
        access_token: Option<String>,
        refresh_token: Option<String>,
    },
    Failed,
}

fn wait_security(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Err(true) — записи нет (security выходит с кодом 44); Err(false) — сбой: связка заперта,
/// security завис или не запустился. Сбой нельзя путать с «нет записи»: иначе опрос уйдёт
/// в устаревший файл и сожжёт одноразовый refresh.
fn security_stdout(args: &[&str]) -> Result<Vec<u8>, bool> {
    let mut child = Command::new("/usr/bin/security")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| false)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(false);
    };
    let reader = std::thread::Builder::new()
        .name("claude-keychain-reader".to_string())
        .spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
    let Ok(reader) = reader else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(false);
    };
    let status = wait_security(&mut child);
    let bytes = reader.join().map_err(|_| false)?.map_err(|_| false)?;
    let status = status.ok_or(false)?;
    if !status.success() {
        return Err(status.code() == Some(44));
    }
    if bytes.len() > 64 * 1024 {
        return Err(false);
    }
    Ok(bytes)
}

fn keychain_account() -> Option<String> {
    let output = security_stdout(&["find-generic-password", "-s", KEYCHAIN_SERVICE]).ok()?;
    parse_acct(&String::from_utf8_lossy(&output))
}

/// Меньше — срок в секундах (≈1,7e9), больше — в миллисекундах. Общий порог для чтения и записи.
const SECONDS_LIMIT: i64 = 100_000_000_000;

/// Имя записи из вывода `security`: `"acct"<blob>="имя"`, а не-ASCII имя — `"acct"<blob>=0x…  "…"`.
/// Имя, прочитанное криво, дало бы при перезаписи вторую запись рядом с настоящей.
fn parse_acct(text: &str) -> Option<String> {
    let line = text.lines().find_map(|l| l.trim_start().strip_prefix("\"acct\"<blob>="))?;
    if let Some(hex) = line.strip_prefix("0x") {
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() % 2 != 0 {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
        return String::from_utf8(bytes?).ok().filter(|name| !name.is_empty());
    }
    // Кавычки внутри имени security не экранирует: имя — всё до последней кавычки строки.
    let quoted = line.strip_prefix('"')?;
    let end = quoted.rfind('"')?;
    // Пустое имя — не имя: иначе запись ушла бы с `-a ""` новой записью, а Claude Code читал бы старую.
    Some(quoted[..end].to_string()).filter(|name| !name.is_empty())
}

/// Err — связку ключей не прочитали (сбой, а не отсутствие записи).
fn read_keychain() -> Result<Option<ClaudeSource>, ()> {
    let output = match security_stdout(&["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"]) {
        Ok(output) => output,
        Err(true) => return Ok(None),
        Err(false) => return Err(()),
    };
    Ok(parse_keychain(&output))
}

fn parse_keychain(output: &[u8]) -> Option<ClaudeSource> {
    let raw = String::from_utf8_lossy(output).trim().to_string();
    let json: Value = serde_json::from_str(&raw).ok()?;
    let oauth = json.get("claudeAiOauth")?.clone();
    // Пустая запись (недописанный logout) не должна заслонять живой ~/.claude/.credentials.json.
    let has_token = |k: &str| oauth.get(k).and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty());
    if !has_token("accessToken") && !has_token("refreshToken") {
        return None;
    }
    Some(ClaudeSource {
        // Имя записи нужно только для перезаписи токена: второй вызов security — лишь тогда.
        kind: SourceKind::Keychain { account: None },
        oauth,
    })
}

fn read_credentials_file() -> Option<ClaudeSource> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())?;
    let path = format!("{home}/.claude/.credentials.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    let oauth = json.get("claudeAiOauth")?.clone();
    Some(ClaudeSource {
        kind: SourceKind::File { path },
        oauth,
    })
}

/// Err — связка ключей не ответила: в файл тогда не идём, он может быть устаревшей копией.
fn read_source() -> Result<Option<ClaudeSource>, ()> {
    Ok(read_keychain()?.or_else(read_credentials_file))
}

/// Свежий взгляд в тот же источник, откуда пришёл токен: при двух источниках `read_source()`
/// вернул бы Keychain даже для токена из файла.
fn reread_source(kind: &SourceKind) -> Option<ClaudeSource> {
    match kind {
        SourceKind::Keychain { .. } => read_keychain().ok().flatten(),
        SourceKind::File { .. } => read_credentials_file(),
    }
}

fn persist(
    source: &ClaudeSource,
    old_access: Option<&str>,
    old_refresh: &str,
    access_token: &str,
    refresh_token: &str,
    expires_at: i64,
) -> PersistOutcome {
    // Читаем свежий источник, а не снимок из начала опроса:
    // токен, уже обновлённый самим Claude Code, не заменяем никогда.
    let current = match &source.kind {
        SourceKind::File { path } => std::fs::read_to_string(path).ok(),
        SourceKind::Keychain { .. } => {
            security_stdout(&["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        }
    };
    let Some(mut json) = current.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) else {
        return PersistOutcome::Failed;
    };
    let Some(oauth) = json.get_mut("claudeAiOauth") else {
        return PersistOutcome::Failed;
    };
    let current_access = oauth.get("accessToken").and_then(|v| v.as_str()).map(str::trim);
    let current_refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).map(str::trim);
    // Без своего access-токена сверяем только refresh: иначе любой access в источнике (даже "")
    // выглядел бы «сменой», и только что полученная пара выбрасывалась при сожжённом refresh.
    let access_changed = old_access.is_some() && current_access != old_access;
    if access_changed || current_refresh != Some(old_refresh) {
        return PersistOutcome::SourceChanged {
            access_token: current_access.map(str::to_string),
            refresh_token: current_refresh.map(str::to_string),
        };
    }
    oauth["accessToken"] = Value::String(access_token.to_string());
    oauth["refreshToken"] = Value::String(refresh_token.to_string());
    // В той же единице, что лежала: срок в секундах, переписанный миллисекундами, Claude Code
    // прочёл бы как «через тысячи лет» и перестал бы обновлять токен сам.
    let in_seconds = oauth.get("expiresAt").and_then(crate::providers::num).is_some_and(|e| e > 0.0 && e < SECONDS_LIMIT as f64);
    oauth["expiresAt"] = Value::Number((if in_seconds { expires_at / 1000 } else { expires_at }).into());
    let raw = json.to_string();
    match &source.kind {
        SourceKind::File { path } => {
            use std::os::unix::fs::OpenOptionsExt;
            let tmp = format!("{path}.{}.tmp", crate::store::new_id());
            let result = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&tmp)?;
                file.write_all(raw.as_bytes())?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&tmp, path)?;
                if let Some(parent) = std::path::Path::new(path).parent() {
                    std::fs::File::open(parent)?.sync_all()?;
                }
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            if result.is_ok() {
                PersistOutcome::Saved
            } else {
                PersistOutcome::Failed
            }
        }
        SourceKind::Keychain { account } => {
            let Some(account) = account.clone().or_else(keychain_account) else {
                return PersistOutcome::Failed;
            };
            // Через stdin токены не видны в аргументах процесса. Полезная нагрузка
            // меньше буфера канала — зависшая связка ключей не заблокирует запись.
            if raw.len() > 8 * 1024 {
                return PersistOutcome::Failed;
            }
            let mut command = Command::new("/usr/bin/security");
            command.args(["add-generic-password", "-U", "-s", KEYCHAIN_SERVICE]);
            command.args(["-a", account.as_str()]);
            command
                .arg("-w")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let Ok(mut child) = command.spawn() else {
                return PersistOutcome::Failed;
            };
            let written = child
                .stdin
                .take()
                .map(|mut pipe| pipe.write_all(raw.as_bytes()).is_ok())
                .unwrap_or(false);
            let success = wait_security(&mut child)
                .map(|status| status.success())
                .unwrap_or(false);
            // Имя записи прочли криво — `-U` молча завёл бы вторую запись рядом с настоящей.
            // Проверяем, что в записи, которую читает Claude Code, теперь наш refresh.
            let landed = || {
                read_keychain().ok().flatten().is_some_and(|s| s.oauth.get("refreshToken").and_then(Value::as_str) == Some(refresh_token))
            };
            if written && success && landed() {
                PersistOutcome::Saved
            } else {
                PersistOutcome::Failed
            }
        }
    }
}

/// Межпроцессный замок обновления токена Claude (окно ↔ CLI). Не вышло открыть — работаем без него.
fn refresh_lock() -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(crate::store::data_dir().join("claude-refresh.lock"))
        .ok()?;
    // Не дольше 30 с: зависший сосед не должен вешать опрос навсегда. Под замком всё равно
    // перечитываем источник, так что после срока идём дальше без него.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // SAFETY: fd живёт, пока жив file; замок снимается при его закрытии.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(file);
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => return None,
        }
    }
}

/// Передача пары между процессами: `old` — уже сгоревший refresh, по нему сосед узнаёт свою пару.
/// Живёт 10 минут — этого с запасом хватает, чтобы пара дошла до state.json.
const HANDOFF_TTL_MS: i64 = 10 * 60 * 1000;

fn put_handoff(file: &std::fs::File, old: &str, access: &str, refresh: &str) {
    use std::io::{Seek, Write};
    let body = serde_json::json!({"old": old, "access": access, "refresh": refresh, "at": crate::store::now_ms()}).to_string();
    let mut f = file;
    let _ = f.set_len(0);
    let _ = f.seek(std::io::SeekFrom::Start(0));
    let _ = f.write_all(body.as_bytes());
}

fn take_handoff(file: &std::fs::File, current: &str) -> Option<(String, String)> {
    use std::io::{Read, Seek};
    let mut f = file;
    let mut text = String::new();
    f.seek(std::io::SeekFrom::Start(0)).ok()?;
    f.take(64 * 1024).read_to_string(&mut text).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let at = v["at"].as_i64()?;
    if v["old"].as_str()? != current || crate::store::now_ms() - at > HANDOFF_TTL_MS {
        return None;
    }
    let access = v["access"].as_str().filter(|s| !s.trim().is_empty())?.to_string();
    let refresh = v["refresh"].as_str().filter(|s| !s.trim().is_empty())?.to_string();
    Some((access, refresh))
}

/// Err(true) — сбой связи или сервера (токен, скорее всего, жив); Err(false) — сервер отказал.
fn refresh_oauth(refresh_token: &str) -> Result<(String, String, i64), bool> {
    // 429 и 5xx — временные. 403/408 чаще от WAF и прокси по дороге, чем от сервера токенов: это «попробую позже», а не «войди заново».
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencode(refresh_token),
        CLIENT_ID
    );
    let payload = request_json(
        HttpRequest::post(TOKEN_URL, body)
            .header("Content-Type", "application/x-www-form-urlencoded"),
    )
    // 200 с не-JSON телом — портал или прокси посреди пути, а не отказ сервера: тоже повторим позже.
    .map_err(|e| e.status.is_none_or(|s| s == 200 || s == 403 || s == 408 || s == 429 || s >= 500))?;
    parse_refresh_payload(&payload, refresh_token, crate::store::now_ms()).ok_or(false)
}

fn parse_refresh_payload(
    payload: &Value,
    old_refresh: &str,
    now: i64,
) -> Option<(String, String, i64)> {
    let access = payload.get("access_token")?.as_str()?.trim();
    if access.is_empty() {
        return None;
    }
    let refresh = payload
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(old_refresh);
    if refresh.is_empty() {
        return None;
    }
    // Сервер уже обменял одноразовый refresh: выкинуть пару из-за кривого срока — потерять вход.
    // Нечитаемый срок = короткий (60 с): токен сохраним и скоро обновим честно. Нет поля — час, как у Anthropic.
    let expires = match payload.get("expires_in") {
        None => 3600.0,
        Some(Value::String(value)) => value.parse::<f64>().unwrap_or(0.0),
        Some(value) => value.as_f64().unwrap_or(0.0),
    };
    let expires = if expires.is_finite() && expires > 0.0 { expires.clamp(60.0, 604_800.0) } else { 60.0 } as i64;
    Some((
        access.to_string(),
        refresh.to_string(),
        now.saturating_add(expires.saturating_mul(1000)),
    ))
}

fn select_token(
    stored: Option<&String>,
    source: Option<&str>,
    prefer_source: bool,
) -> Option<String> {
    let stored = stored.map(|s| s.trim()).filter(|s| !s.is_empty());
    let source = source.map(str::trim).filter(|s| !s.is_empty());
    (if prefer_source {
        source.or(stored)
    } else {
        stored.or(source)
    })
    .map(str::to_string)
}

fn matches_source(
    access: Option<&str>,
    refresh: Option<&str>,
    source_access: Option<&str>,
    source_refresh: Option<&str>,
) -> bool {
    refresh.is_some() && source_refresh == refresh && (access.is_none() || source_access == access)
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn window(
    payload: Option<&Value>,
    key: &str,
    label: &str,
    minutes: i64,
) -> Option<RateLimitWindow> {
    let payload = payload?;
    // Мусор в основном поле не должен прятать годное запасное.
    let used = payload
        .get("utilization")
        .and_then(crate::providers::num)
        .and_then(crate::providers::safe_percent)
        .or_else(|| payload.get("used_percentage").and_then(crate::providers::num).and_then(crate::providers::safe_percent))?;
    let now = crate::store::now_ms();
    Some(RateLimitWindow {
        key: key.to_string(),
        label: label.to_string(),
        used_percent: used,
        window_minutes: minutes,
        // Сброс дальше длины окна (с запасом 5 мин) — мусор в ответе (как у Codex), а не срок.
        resets_at: payload.get("resets_at").and_then(parse_reset_at).filter(|at| *at > now && *at - now <= (minutes + 5) * 60_000),
        note: None,
    })
}

pub fn detect() -> Vec<DetectedCredential> {
    let Ok(Some(source)) = read_source() else {
        return Vec::new();
    };
    let access = source
        .oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let refresh = source
        .oauth
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if access.trim().is_empty() && refresh.trim().is_empty() {
        return Vec::new();
    }
    let plan = source
        .oauth
        .get("subscriptionType")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());
    vec![DetectedCredential {
        provider: ProviderId::Claude,
        label: match plan {
            // «max» из источника — как тариф в карточке: с большой буквы.
            Some(plan) => {
                let mut chars = plan.chars();
                let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
                format!("Claude · {first}{}", chars.as_str())
            }
            None => "Claude (Claude Code)".to_string(),
        },
        credentials: vec![
            ("accessToken".to_string(), access.to_string()),
            ("refreshToken".to_string(), refresh.to_string()),
        ],
        options: vec![("claudeCodeSource".to_string(), "true".to_string())],
        source: match source.kind {
            SourceKind::Keychain { .. } => "Keychain · Claude Code-credentials".to_string(),
            SourceKind::File { path } => path,
        },
    }]
}

pub fn fetch(account: &Account) -> ProviderOutcome {
    let linked = account.options.get("claudeCodeSource").map(String::as_str) == Some("true");
    // Связка ключей не ответила: не знаем, чей токен и не обновил ли его Claude Code —
    // обновлять вслепую нельзя, одноразовый refresh сгорел бы.
    let (source, keychain_down) = match read_source() {
        Ok(source) => (source, false),
        Err(()) => (None, true),
    };
    if keychain_down && linked {
        return ProviderOutcome::unavailable("Связка ключей не ответила — попробую позже");
    }
    let source_access = source
        .as_ref()
        .and_then(|s| s.oauth.get("accessToken"))
        .and_then(|v| v.as_str())
        .map(str::trim);
    // Обрезаем как select_token: иначе « abc » никогда не совпадёт и новый токен не запишется.
    let source_refresh = source
        .as_ref()
        .and_then(|s| s.oauth.get("refreshToken"))
        .and_then(|v| v.as_str())
        .map(str::trim);
    // Ручная карточка со своим refresh, но без access не берёт access Claude Code — иначе покажет чужой аккаунт.
    let own_refresh = account.credentials.get("refreshToken").is_some_and(|v| !v.trim().is_empty());
    let mut access_token = select_token(
        account.credentials.get("accessToken"),
        source_access.filter(|_| linked || !own_refresh),
        linked,
    );
    // Ручная карточка со своим access, но без refresh не берёт refresh Claude Code: он одноразовый,
    // потратив его, карточка получила бы чужой аккаунт, а Claude Code — сгоревший токен.
    let own_access = account.credentials.get("accessToken").is_some_and(|v| !v.trim().is_empty());
    let mut refresh_token = select_token(
        account.credentials.get("refreshToken"),
        source_refresh.filter(|_| linked || !own_access),
        linked,
    );

    let mut patch = Vec::new();
    // Ручные ключи второго аккаунта Claude не должны затирать чужую запись
    // Claude Code в связке ключей или наследовать её срок годности.
    let same_source = matches_source(
        access_token.as_deref(),
        refresh_token.as_deref(),
        source_access,
        source_refresh,
    );
    let persist_failed = std::cell::Cell::new(false);
    let refresh_failed = std::cell::Cell::new(false);
    // Провал обновления из-за связи/сервера: советовать «войди заново» тогда нечестно.
    let refresh_transient = std::cell::Cell::new(false);
    let expires_at = same_source
        .then(|| {
            source
                .as_ref()
                .and_then(|s| s.oauth.get("expiresAt"))
                // Дробный срок (1.7e12) тоже срок; ноль и меньше — «неизвестно», а не «давно истёк»:
                // иначе каждый опрос жёг бы одноразовый refresh.
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
                })
                .filter(|&e| e > 0)
        })
        .flatten();

    let try_refresh = |refresh: &mut Option<String>,
                       patch: &mut Vec<(String, String)>,
                       access: &mut Option<String>|
     -> bool {
        let Some(current) = refresh.as_deref() else {
            return false;
        };
        // Обновление в этом fetch уже провалилось — второй раз этот одноразовый
        // refresh_token слать нельзя, сервер мог его сжечь, а ответ не дошёл.
        if refresh_failed.get() {
            return false;
        }
        if keychain_down {
            refresh_failed.set(true);
            refresh_transient.set(true);
            return false;
        }
        // Окно и `subbar usage` могли прийти за одним токеном одновременно: одноразовый refresh
        // у второго сгорел бы. Замок на время обновления, а под ним — свежий взгляд в Keychain:
        // сосед уже обновил — берём его пару и не шлём второй запрос.
        let _guard = refresh_lock();
        if same_source {
            let now = source.as_ref().and_then(|s| reread_source(&s.kind));
            let pick = |k: &str| now.as_ref().and_then(|s| s.oauth.get(k)).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
            if let (Some(new_access), Some(new_refresh)) = (pick("accessToken"), pick("refreshToken")) {
                if new_refresh != current.trim() {
                    patch.clear();
                    patch.push(("accessToken".to_string(), new_access.clone()));
                    patch.push(("refreshToken".to_string(), new_refresh.clone()));
                    *access = Some(new_access);
                    *refresh = Some(new_refresh);
                    return true;
                }
            }
        }
        // Ручная пара живёт в state.json, а туда её запишут уже после замка: сосед-процесс
        // мог только что обменять этот же refresh. Берём его пару из передачи под замком.
        if !same_source {
            if let Some((new_access, new_refresh)) = _guard.as_ref().and_then(|f| take_handoff(f, current.trim())) {
                patch.clear();
                patch.push(("accessToken".to_string(), new_access.clone()));
                patch.push(("refreshToken".to_string(), new_refresh.clone()));
                *access = Some(new_access);
                *refresh = Some(new_refresh);
                return true;
            }
        }
        let fresh = refresh_oauth(current);
        if let (false, Some(file), Ok((new_access, new_refresh, _))) = (same_source, _guard.as_ref(), &fresh) {
            put_handoff(file, current.trim(), new_access, new_refresh);
        }
        if let Err(transient) = fresh {
            refresh_transient.set(transient);
        }
        if let Ok((new_access, new_refresh, expires)) = fresh {
            if same_source {
                if let Some(source) = &source {
                    match persist(
                        source,
                        access.as_deref(),
                        current,
                        &new_access,
                        &new_refresh,
                        expires,
                    ) {
                        PersistOutcome::Saved => {}
                        PersistOutcome::Failed => persist_failed.set(true),
                        PersistOutcome::SourceChanged {
                            access_token,
                            refresh_token,
                        } => {
                            // Другой процесс Claude обновил эту пару, пока шёл наш запрос.
                            // Его новая пара уже лежит в источнике — это успех, а не
                            // сбой сохранения, поэтому без предупреждения. Оставляем её,
                            // а не записываем наш
                            // теперь уже устаревший ответ.
                            patch.clear();
                            *access = access_token.filter(|value| !value.trim().is_empty());
                            *refresh = refresh_token.filter(|value| !value.trim().is_empty());
                            if let Some(value) = access {
                                patch.push(("accessToken".to_string(), value.clone()));
                            }
                            if let Some(value) = refresh {
                                patch.push(("refreshToken".to_string(), value.clone()));
                            }
                            // Сосед оставил только refresh: он живой — «попробую позже», а не «войди заново».
                            if access.is_none() && refresh.is_some() {
                                refresh_failed.set(true);
                                refresh_transient.set(true);
                            }
                            return access.is_some();
                        }
                    }
                }
            }
            *access = Some(new_access.clone());
            *refresh = Some(new_refresh.clone());
            patch.push(("accessToken".to_string(), new_access));
            patch.push(("refreshToken".to_string(), new_refresh));
            true
        } else {
            // Запоминаем провал: refresh_token одноразовый, повтор в этом же fetch
            // только сожжёт его впустую.
            refresh_failed.set(true);
            false
        }
    };

    let had_access = access_token.is_some();
    if !had_access {
        if refresh_token.is_none() {
            return ProviderOutcome::unavailable(
                "Нет токена Claude: войди в Claude Code или вставь токен вручную",
            );
        }
        if !try_refresh(&mut refresh_token, &mut patch, &mut access_token) {
            // Сосед мог успеть сменить пару (SourceChanged без access): её refresh не теряем.
            return ProviderOutcome::unavailable(if refresh_transient.get() {
                if keychain_down { "Связка ключей не ответила, токен не обновлял — попробую позже" } else { "Токен Claude не обновился: сервер не ответил или отказал — попробую позже" }
            } else {
                if linked || same_source { "Токен Claude не обновился — войди в Claude Code заново" } else { "Токен Claude не обновился — вставь в карточку новый токен" }
            })
            .patch(patch);
        }
    }
    // Токен только что обновлён: 401 на свежем — не повод жечь одноразовый refresh второй раз.
    let mut refreshed_now = !had_access;
    // Срок в секундах (≈1,7e9) читался бы как 1970 год: refresh на каждом опросе жёг бы одноразовый токен.
    if let Some(expires) = expires_at.filter(|_| had_access).map(|e| if e < SECONDS_LIMIT { e.saturating_mul(1000) } else { e }) {
        if expires.saturating_sub(300_000) < crate::store::now_ms() {
            refreshed_now = try_refresh(&mut refresh_token, &mut patch, &mut access_token);
        }
    }

    let perform = |token: &str| -> Result<Value, HttpError> {
        request_json(
            HttpRequest::get(USAGE_URL)
                .header("Authorization", format!("Bearer {token}"))
                .header("anthropic-beta", "oauth-2025-04-20")
                .user_agent("claude-code/2.1.0"),
        )
    };

    // Без токена сюда доходим, когда сосед оставил в источнике только refresh: он живой, опрос — позже.
    let Some(token) = access_token.clone() else {
        return ProviderOutcome::unavailable("Claude Code как раз обновляет вход — попробую позже").patch(patch);
    };
    let mut result = perform(&token);
    let mut refresh_exhausted = false;
    if let Err(error) = &result {
        if error.is_unauthorized() && !refreshed_now {
            let retried = try_refresh(&mut refresh_token, &mut patch, &mut access_token);
            if retried {
                refreshed_now = true;
                if let Some(new_token) = &access_token {
                    result = perform(new_token);
                }
            } else if (refresh_failed.get() && !refresh_transient.get()) || refresh_token.is_none() {
                // Обновить нечем (ручной токен без refresh) — тоже «войди заново», а не сырой HTTP 401.
                refresh_exhausted = true;
            }
        }
    }

    let relogin = if linked || same_source { "войди в Claude Code заново" } else { "вставь в карточку новый токен" };
    let payload = match result {
        Ok(payload) => payload,
        Err(error) => {
            let message = if error.is_unauthorized() && refresh_failed.get() && refresh_transient.get() {
                if keychain_down { "Токен Claude истёк, а связка ключей не ответила — попробую позже" } else { "Токен Claude истёк, а обновить его не вышло: сервер не ответил или отказал — попробую позже" }.to_string()
            } else if error.is_unauthorized() && refreshed_now && !refresh_failed.get() {
                format!("Anthropic отверг даже только что обновлённый токен — {relogin}")
            } else if refresh_exhausted {
                format!("Токен Claude истёк, а обновить его не удалось — попробую позже, или {relogin}")
            } else if error.is_rate_limited() {
                "Anthropic притормозил запросы (429) — попробую позже".to_string()
            } else {
                error.message
            };
            let warning = if persist_failed.get() {
                " Токен Claude Code не сохранился."
            } else {
                ""
            };
            return ProviderOutcome::fail(format!("{message}{warning}")).patch(patch);
        }
    };

    let mut windows: Vec<RateLimitWindow> = [
        window(payload.get("five_hour"), "session", &crate::util::window_label(300), 300),
        window(payload.get("seven_day"), "weekly", &crate::util::window_label(10080), 10080),
    ]
    .into_iter()
    .flatten()
    .collect();

    if let Some(limits) = payload.get("limits").and_then(|v| v.as_array()) {
        for limit in limits {
            let is_fable = limit
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|kind| kind == "weekly_scoped")
                .unwrap_or(false)
                && limit
                    .get("scope")
                    .and_then(|scope| scope.get("model"))
                    .and_then(|model| model.get("display_name"))
                    .and_then(|v| v.as_str())
                    .map(|name| name.eq_ignore_ascii_case("fable"))
                    .unwrap_or(false);
            // Ключ окна должен быть один: два «fable» склеили бы уведомления и перепутали кольца.
            if is_fable && !windows.iter().any(|w| w.key == "fable") {
                if let Some(fable) = window(Some(limit), "fable", &format!("Fable {}", crate::util::window_label(10080)), 10080) {
                    windows.push(fable);
                }
            }
        }
    }

    if windows.is_empty() {
        return ProviderOutcome::fail("Anthropic API не вернул окна лимитов").patch(patch);
    }

    // Тариф из Claude Code — только если карточка и есть Claude Code, иначе это чужой тариф.
    let plan = source
        .as_ref()
        .filter(|_| same_source || linked)
        .and_then(|s| s.oauth.get("subscriptionType"))
        .and_then(|v| v.as_str())
        .map(|value| value.to_string())
        // Связку ключей не прочитали или в ней нет тарифа — тариф прежний, а не пустой:
        // успешный опрос иначе стирал бы «Max» до следующего удачного чтения.
        .or_else(|| account.last_usage.as_ref().and_then(|u| u.plan_type.clone()));

    let notes = if persist_failed.get() {
        vec!["Токен Claude Code не сохранился — может потребоваться новый вход (или отвяжи карточку от Claude Code)".to_string()]
    } else {
        Vec::new()
    };
    ProviderOutcome::ok(windows)
        .with_plan(plan)
        .with_notes(notes)
        .patch(patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keychain_account_name_is_read_whole() {
        assert_eq!(parse_acct("    \"acct\"<blob>=\"apple\"\n"), Some("apple".to_string()));
        assert_eq!(parse_acct("    \"acct\"<blob>=\"a\"b\"\n"), Some("a\"b".to_string()));
        assert_eq!(parse_acct("    \"acct\"<blob>=0xD0B0  \"\\320\\260\"\n"), Some("а".to_string()));
        assert_eq!(parse_acct("    \"acct\"<blob>=<NULL>\n"), None);
    }

    #[test]
    fn usage_percentage_is_clamped() {
        let raw = serde_json::json!({"utilization":250.0});
        let parsed = window(Some(&raw), "weekly", "7д", 10080).unwrap();
        assert_eq!(parsed.used_percent, 100.0);
    }

    #[test]
    fn rejects_empty_and_invalid_rotated_tokens() {
        let empty = serde_json::json!({"access_token":"", "refresh_token":"synthetic-new"});
        assert!(parse_refresh_payload(&empty, "synthetic-old", 1_000).is_none());
        for odd in [serde_json::json!(-30), serde_json::Value::Null, serde_json::json!("мусор")] {
            let payload = serde_json::json!({"access_token":"synthetic-new", "expires_in": odd});
            let (_, _, at) = parse_refresh_payload(&payload, "synthetic-old", 1_000).expect("пара не теряется");
            assert_eq!(at, 61_000, "кривой срок — короткий, но токен сохранён");
        }
        let rotated = serde_json::json!({"access_token":"synthetic-new",
            "refresh_token":"synthetic-rotated", "expires_in":"3600"});
        assert_eq!(
            parse_refresh_payload(&rotated, "synthetic-old", 1_000),
            Some((
                "synthetic-new".into(),
                "synthetic-rotated".into(),
                3_601_000
            ))
        );
    }

    #[test]
    fn linked_account_follows_rotated_source_but_manual_account_does_not() {
        let old = "synthetic-stored".to_string();
        assert_eq!(
            select_token(Some(&old), Some("synthetic-new-source"), true).as_deref(),
            Some("synthetic-new-source")
        );
        assert_eq!(
            select_token(Some(&old), Some("synthetic-new-source"), false).as_deref(),
            Some("synthetic-stored")
        );
    }

    #[test]
    fn manually_entered_account_cannot_overwrite_claude_code_keychain() {
        assert!(matches_source(
            Some("synthetic-source-a"),
            Some("synthetic-source-r"),
            Some("synthetic-source-a"),
            Some("synthetic-source-r")
        ));
        assert!(!matches_source(
            Some("synthetic-manual-a"),
            Some("synthetic-manual-r"),
            Some("synthetic-source-a"),
            Some("synthetic-source-r")
        ));
        assert!(!matches_source(
            Some("synthetic-manual-a"),
            Some("synthetic-source-r"),
            Some("synthetic-source-a"),
            Some("synthetic-source-r")
        ));
    }
    #[test]
    fn передача_пары_соседу_по_сгоревшему_refresh() {
        let path = std::env::temp_dir().join(format!("subbar-handoff-{}", std::process::id()));
        let file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&path).unwrap();
        assert!(take_handoff(&file, "R1").is_none());
        put_handoff(&file, "R1", "A2", "R2");
        assert_eq!(take_handoff(&file, "R1"), Some(("A2".to_string(), "R2".to_string())));
        assert!(take_handoff(&file, "R9").is_none());
        let _ = std::fs::remove_file(&path);
    }

}
