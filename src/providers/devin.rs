use std::io::Read;
use std::path::PathBuf;

use crate::model::{Account, DetectedCredential, ProviderId, ProviderOutcome, RateLimitWindow};

/// Кэш статуса Devin старше получаса считаем протухшим.
const STALE_SECS: i64 = 1800;

/// Публичного адреса расхода для сессий CLI у Devin нет, но сам CLI
/// кэширует ответ `GetUserStatus` (protobuf) локально. Мы просим CLI
/// пересинхронизироваться и читаем из кэша дневную и недельную квоты.
struct DevinStatus {
    daily_remaining_percent: f64,
    weekly_remaining_percent: f64,
    daily_reset_at: Option<i64>,
    weekly_reset_at: Option<i64>,
    fetched_at: i64,
}

fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())?;
    let dir = PathBuf::from(format!("{home}/.cache/devin/cli"));
    dir.is_dir().then_some(dir)
}

/* ----------------------------- protobuf ----------------------------- */

enum PbValue {
    Varint(u64),
    Bytes(Vec<u8>),
}

fn pb_parse(buf: &[u8]) -> Option<Vec<(u64, PbValue)>> {
    let mut fields = Vec::new();
    let mut index = 0usize;
    while index < buf.len() {
        let (key, next) = pb_varint(buf, index)?;
        index = next;
        let field = key >> 3;
        let wire = key & 0x7;
        if field == 0 {
            return None;
        }
        match wire {
            0 => {
                let (value, next) = pb_varint(buf, index)?;
                index = next;
                fields.push((field, PbValue::Varint(value)));
            }
            1 => {
                let end = index.checked_add(8)?;
                if end > buf.len() {
                    return None;
                }
                index = end;
            }
            2 => {
                let (length, next) = pb_varint(buf, index)?;
                index = next;
                let length = usize::try_from(length).ok()?;
                let end = index.checked_add(length)?;
                if end > buf.len() {
                    return None;
                }
                fields.push((field, PbValue::Bytes(buf[index..end].to_vec())));
                index = end;
            }
            5 => {
                let end = index.checked_add(4)?;
                if end > buf.len() {
                    return None;
                }
                index = end;
            }
            _ => return None,
        }
    }
    Some(fields)
}

fn pb_varint(buf: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut index = start;
    while index < buf.len() {
        let byte = buf[index];
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        index += 1;
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn pb_bytes(buf: &[u8], field: u64) -> Option<Vec<u8>> {
    // Правило protobuf: у повторённого поля побеждает последнее вхождение.
    pb_parse(buf)?
        .into_iter()
        .rev()
        .find_map(|(number, value)| match value {
            PbValue::Bytes(data) if number == field => Some(data),
            _ => None,
        })
}

fn pb_number(buf: &[u8], field: u64) -> Option<f64> {
    pb_parse(buf)?
        .into_iter()
        .rev()
        .find_map(|(number, value)| match value {
            PbValue::Varint(raw) if number == field => Some(raw as f64),
            _ => None,
        })
}

fn reset_timestamp_ms(seconds: f64) -> Option<i64> {
    // Эпоха в секундах 2001–2100: иначе «сброс вот-вот» (1970) или «20684000д» (миллисекунды).
    if !seconds.is_finite() || !(978_307_200.0..=4_102_444_800.0).contains(&seconds) {
        return None;
    }
    Some((seconds as i64).saturating_mul(1000))
}

/* ------------------------------ cache ------------------------------- */

/// Файлы кэша `user_status.*.bin` — свежие первыми.
fn status_files() -> Vec<(std::time::SystemTime, PathBuf)> {
    let Some(entries) = cache_dir().and_then(|dir| std::fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("user_status.") || !name.ends_with(".bin") {
            continue;
        }
        // По ссылке, а не саму ссылку: симлинк на кэш — тоже кэш.
        let Ok(metadata) = entry.path().metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        // Без даты файла возраст данных не проверить — такой файл не берём.
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        candidates.push((modified, entry.path()));
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates
}

fn read_cached_status() -> Option<DevinStatus> {
    let candidates = status_files();
    for (modified, path) in candidates {
        if let Some(mut status) = read_cached_status_file(&path) {
            // Дата внутри файла не новее самого файла: иначе сдвиг часов или откат выдал бы старое за свежее.
            if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                let file_secs = since.as_secs() as i64;
                if status.fetched_at <= 0 || status.fetched_at > file_secs {
                    status.fetched_at = file_secs;
                }
            }
            return Some(status);
        }
    }
    None
}

fn read_cached_status_file(path: &std::path::Path) -> Option<DevinStatus> {
    const MAX_CACHE_FILE_BYTES: u64 = 4 * 1024 * 1024;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_CACHE_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CACHE_FILE_BYTES {
        return None;
    }
    let raw = String::from_utf8(bytes).ok()?;
    let envelope: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let fetched_at = envelope
        .get("fetched_at_secs")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)))
        .unwrap_or(0);
    let payload_b64 = envelope.get("payload").and_then(|v| v.as_str())?;
    let payload = crate::util::base64_decode(payload_b64)?;

    let plan_status = pb_bytes(&payload, 13)?;
    // proto3 не пишет нули: нет поля при живом plan_status — это 0% остатка (квота исчерпана),
    // а не «нет кэша». Отличить это от переименованного поля нельзя — формат сверен с живым кэшем CLI.
    // Нет самого plan_status — выше уже None.
    let daily = pb_number(&plan_status, 14).unwrap_or(0.0);
    let weekly = pb_number(&plan_status, 15).unwrap_or(0.0);
    if !(0.0..=100.0).contains(&daily) || !(0.0..=100.0).contains(&weekly) {
        return None;
    }
    // Сброс в прошлом или дальше года — мусор, как у соседних провайдеров.
    let now = crate::store::now_ms();
    let reset = |field| pb_number(&plan_status, field).and_then(reset_timestamp_ms).filter(|at| *at > now && *at - now <= 366 * 86_400_000);
    Some(DevinStatus {
        daily_remaining_percent: daily,
        weekly_remaining_percent: weekly,
        daily_reset_at: reset(17),
        weekly_reset_at: reset(18),
        fetched_at,
    })
}

/// Попросить CLI Devin обновить кэш статуса (вызывающий уже знает, что кэш устарел).
/// С потолком по времени: зависший CLI не держит поток вечно.
/// Обновить кэш через `devin list`; None — обновилось, Some — почему нет (в заметку карточки).
fn refresh_cache() -> Option<&'static str> {
    let Some(cli) = resolve_cli() else {
        return Some("CLI devin не найден");
    };
    let mut child = match std::process::Command::new(cli)
        .arg("list")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Some("devin list не запустился"),
    };
    // Ждём до 15 секунд, потом убиваем.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(code)) => return (!code.success()).then_some("devin list завершился с ошибкой"),
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some("devin list не ответил за 15 с");
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(_) => {
                // Иначе devin list остался бы сиротой и дописал бы кэш поверх следующего чтения.
                let _ = child.kill();
                let _ = child.wait();
                return Some("devin list не удалось дождаться");
            }
        }
    }
}

fn resolve_cli() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())?;
    let candidates = [
        PathBuf::from(format!("{home}/.local/bin/devin")),
        // Apple Silicon раньше Intel: протухшая сборка в /usr/local не должна побеждать свежую.
        PathBuf::from("/opt/homebrew/bin/devin"),
        PathBuf::from("/usr/local/bin/devin"),
    ];
    // Без бита исполнения запуск молча упал бы, а карточка звала бы «запусти devin list».
    candidates.into_iter().find(|path| {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/* ----------------------------- provider ----------------------------- */

pub fn detect() -> Vec<DetectedCredential> {
    // Расход Devin — только из кэша: поиск никогда не читает и не импортирует
    // файл с ключами CLI. Хватает установленного CLI или его локального кэша.
    // Путь, как у соседей: пользователь видит, откуда взялась карточка.
    let Some(source) = resolve_cli()
        .or_else(|| cache_dir().filter(|_| !status_files().is_empty()))
        .map(|path| path.display().to_string())
    else {
        return Vec::new();
    };
    vec![DetectedCredential {
        provider: ProviderId::Devin,
        label: "Devin".to_string(),
        credentials: Vec::new(),
        options: Vec::new(),
        source,
    }]
}

pub fn fetch(_account: &Account) -> ProviderOutcome {
    let mut status = read_cached_status();
    let stale = status
        .as_ref()
        .map(|s| {
            let age = (crate::store::now_ms() / 1000).saturating_sub(s.fetched_at);
            !(0..STALE_SECS).contains(&age)
        })
        .unwrap_or(true);
    let mut refresh_error = None;
    if stale {
        refresh_error = refresh_cache();
        // Второе чтение не удалось — берём первое (CLI мог писать файл в этот момент).
        status = read_cached_status().or(status);
    }

    let Some(status) = status else {
        // Файлы есть, но ни один не разобрался — «devin list» тут не поможет, скорее сменился формат.
        // Сменившийся формат — сбой (красная карточка), а не штатное «данных нет».
        if !status_files().is_empty() {
            return ProviderOutcome::fail(if resolve_cli().is_some() {
                "Кэш Devin есть, но не читается — запусти «devin list» заново; не помогло — CLI сменил формат, нужна новая версия SubBar"
            } else {
                "Кэш Devin есть, но не читается, а CLI devin не найден — установи его, чтобы обновить кэш"
            });
        }
        return ProviderOutcome::unavailable(if resolve_cli().is_some() {
            "Нет данных Devin: CLI не оставил кэш — запусти «devin list» в терминале"
        } else {
            "Нет данных Devin: CLI devin не найден — установи его, иначе обновить кэш нечем"
        });
    };

    // Кэш мог устареть: показываем, но пишем, насколько он старый.
    // «Сейчас» — после обновления кэша: иначе свежий файл от devin list выглядел бы «из будущего».
    let age = (crate::store::now_ms() / 1000).saturating_sub(status.fetched_at);
    let mut notes = Vec::new();
    if age < 0 {
        notes.push("время кэша CLI в будущем — проверь часы на Mac".to_string());
    } else if age >= STALE_SECS {
        // Тот же порог, что у «устарело»: CLI не обновил кэш — показать, насколько данные старые.
        notes.push(format!(
            "данные из кэша CLI: {} назад",
            crate::util::format_duration(age.saturating_mul(1000))
        ));
        if let Some(why) = refresh_error {
            notes.push(format!("обновить не вышло: {why}"));
        }
    }
    // Кэш ведётся по профилю, а карточка Devin одна — показываем текущий вход (самый свежий файл).
    if status_files().len() > 1 {
        notes.push("в кэше Devin несколько профилей — показан тот, в который вошли последним".to_string());
    }

    let windows = vec![
        RateLimitWindow {
            key: "daily".to_string(),
            label: crate::util::window_label(1440),
            used_percent: crate::util::clamp_percent(100.0 - status.daily_remaining_percent),
            window_minutes: 1440,
            resets_at: status.daily_reset_at,
            note: None,
        },
        RateLimitWindow {
            key: "weekly".to_string(),
            label: crate::util::window_label(10080),
            used_percent: crate::util::clamp_percent(100.0 - status.weekly_remaining_percent),
            window_minutes: 10080,
            resets_at: status.weekly_reset_at,
            note: None,
        },
    ];

    ProviderOutcome::ok(windows).with_notes(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_status_fields() {
        // root.13 { 14: 42, 15: 77, 17: 1790150400, 18: 1790496000 }
        let plan = vec![
            0x70, 42, // field 14 varint
            0x78, 77, // field 15 varint
            0x88, 0x01, 0x80, 0x8e, 0xce, 0xd5, 0x06, // field 17 varint = 1790150400
            0x90, 0x01, 0x80, 0x9a, 0xe3, 0xd5, 0x06, // field 18 varint = 1790496000
        ];
        assert_eq!(pb_number(&plan, 14), Some(42.0));
        assert_eq!(pb_number(&plan, 15), Some(77.0));
        assert_eq!(pb_number(&plan, 17).map(|v| v as i64), Some(1790150400));
    }
}
