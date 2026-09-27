//! Прокси для Claude Code: основная модель — на api.anthropic.com байт в байт (подписка),
//! субагенты по правилу — на модель OpenCode Go. Запуск: `subbar proxy [--config путь]`.
//!
//! Безопасность: заголовки авторизации Claude (OAuth подписки) уходят ТОЛЬКО на Anthropic;
//! в OpenCode идёт собственный ключ и ничего больше.
//!
//! Надёжность: кончился лимит ключа — запрос уходит на другой ключ; OpenCode молчит — `ping`,
//! как у Anthropic, а после долгой тишины — честная ошибка; SIGTERM — доделать начатое и выйти.

pub mod config;
pub mod control;
pub mod keys;
pub mod responses;
pub mod route;

use bytes::Bytes;
use config::ProxyConfig;
use futures_util::{Stream, StreamExt, TryStreamExt};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use route::Route;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

type Body = BoxBody<Bytes, std::io::Error>;

/// Заголовки одного соединения — не пересылаются.
const HOP: [&str; 9] = ["connection", "proxy-connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];
const RECENT: usize = 30;
const UA: &str = concat!("subbar/", env!("CARGO_PKG_VERSION"));
/// Так Anthropic держит поток живым, пока модель думает молча.
const PING: &[u8] = b"event: ping\ndata: {\"type\": \"ping\"}\n\n";
/// Запасные ключи перечитываются из карточек не чаще раза в полминуты.
const POOL_TTL: Duration = Duration::from_secs(30);
/// Предел тишины и ожидания заголовков без потока: дольше — полуоткрытое соединение, а не долгий ответ.
const ANTHROPIC_WAIT: Duration = Duration::from_secs(600);

/// Сроки. Переменные окружения — только для тестов (чтобы не ждать минутами).
#[derive(Clone, Copy, Debug)]
struct Timings {
    /// Сколько ждать ответа OpenCode на потоковый запрос; дольше — сбой (и откат).
    headers: Duration,
    /// Тишина в потоке дольше этого — шлём `ping`.
    ping: Duration,
    /// Тишина дольше этого — поток мёртв: честная ошибка вместо вечного ожидания.
    silence: Duration,
    /// SIGTERM: сколько максимум доделывать начатые запросы.
    drain: Duration,
    /// Первая пауза перед повтором при временном сбое OpenCode (вторая — втрое дольше).
    retry: Duration,
}

impl Timings {
    fn from_env() -> Self {
        let ms = |name: &str, default: u64| {
            Duration::from_millis(std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default))
        };
        Self {
            // Нули/опечатки не должны валить каждый запрос мгновенным таймаутом.
            headers: ms("SUBBAR_HEADERS_MS", 60_000).max(Duration::from_millis(100)),
            ping: ms("SUBBAR_PING_MS", 10_000).max(Duration::from_millis(20)),
            silence: ms("SUBBAR_SILENCE_MS", 300_000).max(Duration::from_millis(100)),
            // Укладываемся с запасом и в ExitTimeOut службы (150 с), и в 60 с, которые launchd даёт по умолчанию.
            drain: ms("SUBBAR_DRAIN_MS", 50_000).max(Duration::from_millis(500)),
            retry: ms("SUBBAR_RETRY_MS", 500).max(Duration::from_millis(1)),
        }
    }
}

#[derive(Serialize)]
struct Event {
    at: u64,
    route: &'static str,
    model: String,
    status: u16,
    ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Serialize, Default)]
struct Stats {
    pass: u64,
    sub: u64,
    fallback: u64,
    errors: u64,
    recent: VecDeque<Event>,
}

struct Shared {
    cfg_path: PathBuf,
    cfg: Mutex<(ProxyConfig, Option<SystemTime>)>,
    client: reqwest::Client,
    stats: Mutex<Stats>,
    started: Instant,
    /// Когда прокси запущен (мс) — для «с 23:05» у счётчиков в окне.
    started_ms: i64,
    t: Timings,
    /// Запросы в работе (вместе с отдачей тела) — для мягкого выхода.
    inflight: AtomicUsize,
    /// Идёт мягкий выход: новые запросы — «перегружен, повтори» (Claude Code повторит уже на новом процессе).
    draining: std::sync::atomic::AtomicBool,
    /// mtime битого файла конфига — чтобы не перечитывать и не писать в журнал на каждом запросе.
    cfg_bad: Mutex<Option<SystemTime>>,
    paused: Mutex<keys::Paused>,
    /// Долгие паузы прошлого процесса по хэшу ключа: сырые ключи на диск не пишем.
    saved_paused: Mutex<keys::Paused>,
    pool: Mutex<Option<(Instant, keys::Pool)>>,
    /// Время правки state.json, с которого собран пул: выключили карточку — не ждать POOL_TTL.
    pool_mtime: Mutex<Option<std::time::SystemTime>>,
    last_key: Mutex<String>,
    /// Счёт по сессиям Claude Code — для строки в терминале («deepseek-v4.1-flash · 12 отв»).
    sessions: Mutex<HashMap<String, SessionStat>>,
    /// Порт из конфига на старте: сокет занят на нём, смену порта на лету не подхватить.
    bound_port: u16,
    /// Порт в конфиге сменили — мягко выходим, launchd поднимет прокси уже на новом.
    port_moved: tokio::sync::Notify,
}

/// Что было в одной сессии Claude Code: сколько ответила OpenCode, сколько ушло в Claude, кто и когда отвечал.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SessionStat {
    /// Запусков субагентов (первый запрос субагента — диалог из одного сообщения).
    agents: u64,
    sub: u64,
    fallback: u64,
    errors: u64,
    pass: u64,
    last_at: i64,
    last_sub_at: i64,
    last_model: String,
    last_key: String,
    /// Скорость ответов Claude (насквозь) по замеру tok-speed из Pi: Σ output_tokens / Σ (конец − message_start).
    /// Ответы, закончившиеся не дальше `SPEED_TURN_MS` друг от друга, — один ход (цепочка вызовов инструментов).
    speed_tokens: u64,
    speed_ms: u64,
    speed_at: i64,
}

/// Пауза между ответами, после которой начинается новый ход (новый замер скорости).
const SPEED_TURN_MS: i64 = 30_000;

/// Сколько сессий помнить (самые давние забываются).
const SESSIONS: usize = 200;

/// Счёт сессий переживает перезапуск прокси (установку новой версии): файл рядом с конфигом.
fn sessions_path(cfg_path: &std::path::Path) -> PathBuf {
    cfg_path.with_file_name("proxy-sessions.json")
}

/// Прошлый счёт — только за последние сутки: старые сессии уже закрыты.
fn load_sessions(cfg_path: &std::path::Path) -> HashMap<String, SessionStat> {
    // Недописанные tmp от убитых процессов (kill -9 между записью и rename) иначе копятся вечно.
    // «proxy.json» без каталога даёт parent() == "" — read_dir("") падает, уборка молча не шла бы.
    let dir = cfg_path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    if let Ok(dir) = std::fs::read_dir(dir) {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stale = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|d| d > Duration::from_secs(600)));
            // Только свои снимки: tmp конфига (proxy.json.*.tmp) пишет окно — не нам его убирать.
            let ours = name.starts_with("proxy-sessions.json.") || name.starts_with("proxy-paused.json.");
            if ours && name.ends_with(".tmp") && stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let day_ago = now_ms() - 86_400_000;
    std::fs::read(sessions_path(cfg_path))
        .ok()
        // Поштучно, как load_paused: одна битая запись (обрезанный файл, поле новой версии) не обнуляет весь счёт.
        .and_then(|raw| serde_json::from_slice::<HashMap<String, Value>>(&raw).ok())
        .map(|m| {
            let m = m.into_iter().filter_map(|(k, v)| Some((k, serde_json::from_value::<SessionStat>(v).ok()?)));
            // Не больше SESSIONS самых свежих: вытеснение в note_session убирает лишь по одной.
            let mut v: Vec<_> = m.into_iter().filter(|(_, s)| s.last_at > day_ago).collect();
            v.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_at));
            v.truncate(SESSIONS);
            v.into_iter().collect()
        })
        .unwrap_or_default()
}

/// Недельный лимит ключа переживает перезапуск: иначе новый процесс снова бьётся в пустой ключ.
fn paused_path(cfg_path: &std::path::Path) -> PathBuf {
    cfg_path.with_file_name("proxy-paused.json")
}

/// FNV-1a: стабилен между версиями, в отличие от DefaultHasher.
fn key_hash(key: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn load_paused(cfg_path: &std::path::Path) -> HashMap<String, keys::Pause> {
    let now = now_ms();
    std::fs::read(paused_path(cfg_path))
        .ok()
        .and_then(|raw| serde_json::from_slice::<HashMap<String, Value>>(&raw).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(h, v)| {
            // Больше потолка паузы (31 день, с суткой запаса) не бывает: подправленный или битый файл выбил бы ключ навсегда.
            let until_ms = v["untilMs"].as_i64().filter(|u| *u > now && *u - now <= 32 * 86_400_000)?;
            let label = v["label"].as_str().unwrap_or("").to_string();
            let reason = v["reason"].as_str().unwrap_or("").to_string();
            let kind = keys::PauseKind::parse(v["kind"].as_str());
            Some((h, keys::Pause { label, until_ms, kind, reason }))
        })
        .collect()
}

struct SavePaused {
    cfg_path: std::path::PathBuf,
    saved: HashMap<String, keys::Pause>,
    paused: HashMap<String, keys::Pause>,
    gen: u64,
}

/// Номер снимка → записанный последним. Старый снимок, дошедший до диска позже нового, пропускается:
/// иначе он воскрешал снятую паузу или терял свежую.
static PAUSED_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SAVING_PAUSED: Mutex<u64> = Mutex::new(0);

impl SavePaused {
fn write(self) {
    let mut last = lock(&SAVING_PAUSED);
    if self.gen < *last {
        return;
    }
    let now = now_ms();
    let mut m: serde_json::Map<String, Value> = self.saved.iter()
        .filter(|(_, p)| p.until_ms > now)
        .map(|(h, p)| (h.clone(), json!({"label": p.label, "untilMs": p.until_ms, "kind": p.kind.as_str(), "reason": p.reason})))
        .collect();
    for (k, p) in self.paused.iter().filter(|(_, p)| p.until_ms > now && p.kind == keys::PauseKind::Limit) {
        m.insert(key_hash(k), json!({"label": p.label, "untilMs": p.until_ms, "kind": p.kind.as_str(), "reason": p.reason}));
    }
    let Ok(text) = serde_json::to_vec(&m) else { return };
    let path = paused_path(&self.cfg_path);
    let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), crate::store::new_id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        // Как config::save: без fsync каталога переименование может не пережить сбой питания.
        if let Some(dir) = path.parent() {
            let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    };
    match write() {
        // Поколение двигаем только после записи: иначе сорванный снимок не повторил бы никто.
        Ok(()) => *last = self.gen,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("[subbar] не сохранил паузы ключей: {e} — после перезапуска исчерпанный ключ снова пойдёт в ход");
        }
    }
}
}

/// fsync не на рабочем потоке tokio: серия 429 иначе держала бы потоки рантайма на диске.
fn save_paused_bg(sh: &Shared) {
    let snapshot = paused_snapshot(sh);
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn_blocking(move || snapshot.write());
        }
        Err(_) => snapshot.write(),
    }
}

fn paused_snapshot(sh: &Shared) -> SavePaused {
    // Номер и снимок — под замками обеих карт (в том же порядке, что restore_paused): порядок номеров = порядок состояний.
    let saved = lock(&sh.saved_paused);
    let paused = lock(&sh.paused);
    let snapshot = SavePaused {
        cfg_path: sh.cfg_path.clone(),
        saved: saved.clone(),
        paused: paused.clone(),
        gen: PAUSED_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
    };
    drop(paused);
    drop(saved);
    snapshot
}

fn save_sessions(sh: &Shared) {
    // По одному: иначе фоновое сохранение со старым снимком может переименоваться после выходного и затереть его.
    static SAVING: Mutex<()> = Mutex::new(());
    let _one = lock(&SAVING);
    let Ok(text) = serde_json::to_vec(&*lock(&sh.sessions)) else { return };
    let path = sessions_path(&sh.cfg_path);
    // Имя с PID: при перезапуске старый и новый процесс пишут одновременно — общий tmp рвётся.
    // И уникальное на каждую запись: периодическое и выходное сохранения внутри одного процесса
    // иначе писали в один tmp и перемешивали байты.
    let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), crate::store::new_id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // 0600 как у state.json и proxy.json: здесь подписи карточек и имена моделей (ключей нет).
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        // Как config::save: без fsync каталога переименование может не пережить сбой питания.
        if let Some(dir) = path.parent() {
            let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("[subbar] не сохранил счёт сессий: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

use crate::store::now_ms;

impl Shared {
    /// Конфиг перечитывается, если файл менялся (окно сохранило новые настройки).
    fn config(&self) -> ProxyConfig {
        let mut warn = None;
        let cfg = {
            let mut g = lock(&self.cfg);
            // mtime — под замком: иначе второй запрос со старым mtime перечитывал файл повторно.
            let mtime = std::fs::metadata(&self.cfg_path).and_then(|m| m.modified()).ok();
            let known_bad = mtime.is_some() && *lock(&self.cfg_bad) == mtime;
            // Файл удалили на ходу — работаем на прежних настройках, а не на пустом ключе по умолчанию.
            if g.1 != mtime && mtime.is_none() && g.1.is_some() {
                return g.0.clone();
            }
            if g.1 != mtime && !known_bad {
                match config::try_load(&self.cfg_path) {
                    Ok(c) => {
                        *lock(&self.cfg_bad) = None;
                        g.1 = mtime; // запоминаем только удачное чтение: битый файл перечитаем, как только его поправят
                        // Выбрали другой ключ — если он был отложен как «негодный» (отвергнут, без оплаты), дать шанс снова.
                        // Тот же ключ выбрали заново (починили и кликнули его же) — тоже шанс: иначе час простоя.
                        // Правка других настроек (effort, тумблер) паузу не снимает — иначе мёртвый ключ долбится снова.
                        if c.port != self.bound_port {
                            self.port_moved.notify_one();
                        }
                        if c.api_key.trim() != g.0.api_key.trim() {
                            // Явный выбор ключа снимает и «кончился»: иначе пополненный ключ лежал бы до конца недели
                            // (другого способа снять долгую паузу нет). Всё ещё пуст — первый же 429 вернёт паузу.
                            let chosen = c.api_key.trim();
                            // Замки берутся по очереди и не вложены; порядок тот же, что в restore_paused (saved_paused → paused).
                            let from_saved = lock(&self.saved_paused).remove(&key_hash(chosen)).is_some();
                            let from_live = lock(&self.paused).remove(chosen).is_some_and(|p| p.kind == keys::PauseKind::Limit);
                            let had_long = from_saved | from_live;
                            if had_long {
                                save_paused_bg(self);
                            }
                            *lock(&self.pool) = None; // и запасные перечитать: могли добавить новый ключ
                        }
                        g.0 = c;
                    }
                    Err(e) => {
                        *lock(&self.cfg_bad) = mtime;
                        warn = Some(e.to_string());
                    }
                }
            }
            g.0.clone()
        };
        // Печать — уже без замка на конфиг (см. record).
        if let Some(e) = warn {
            eprintln!("[subbar] конфиг {} не прочитан ({e}) — работаю на прежнем", self.cfg_path.display());
        }
        cfg
    }

    fn record(&self, route: &'static str, model: &str, status: u16, t0: Instant, key: Option<&str>, note: Option<String>) {
        let ms = t0.elapsed().as_millis() as u64;
        // Строку журнала собираем до замка, печатаем после: stderr у процесса один, под мьютексом он встаёт в очередь.
        let known = matches!(route, "sub" | "fallback" | "error");
        let line = if route == "pass" || !known {
            None
        } else {
            let via = key.map(|k| format!(" · {k}")).unwrap_or_default();
            Some(format!("[subbar] {route} {model} {status} {ms}мс{via}{}", note.as_deref().map(|n| format!(" — {n}")).unwrap_or_default()))
        };
        let mut s = lock(&self.stats);
        match route {
            "sub" => s.sub += 1,
            "fallback" => s.fallback += 1,
            "error" => s.errors += 1,
            "pass" => s.pass += 1,
            // Неизвестный маршрут — сбой в коде: в ошибки, а не в «напрямую в Claude».
            other => {
                debug_assert!(false, "неизвестный маршрут {other}");
                s.errors += 1
            }
        }
        // Основная сессия (pass) шлёт запросы потоком: без своей квоты она за минуту вытесняла из журнала
        // все события субагентов, ради которых журнал и нужен. Прямых держим не больше четверти.
        if route == "pass" && s.recent.iter().filter(|e| e.route == "pass").count() >= RECENT / 4 {
            if let Some(i) = s.recent.iter().position(|e| e.route == "pass") {
                s.recent.remove(i);
            }
        }
        s.recent.push_back(Event { at: now_ms() as u64, route, model: model.to_string(), status, ms, key: key.map(str::to_string), note });
        while s.recent.len() > RECENT {
            s.recent.pop_front();
        }
        drop(s);
        if let Some(line) = line {
            eprintln!("{line}");
        }
    }

    /// Отметить запрос в счёте его сессии.
    fn note_session(&self, sid: Option<&str>, route: &'static str, model: &str, key: Option<&str>, first_turn: bool) {
        let Some(sid) = sid else { return };
        let mut sessions = lock(&self.sessions);
        if !sessions.contains_key(sid) && sessions.len() >= SESSIONS {
            if let Some(oldest) = sessions.iter().min_by_key(|(_, s)| s.last_at).map(|(k, _)| k.clone()) {
                sessions.remove(&oldest);
            }
        }
        let s = sessions.entry(sid.to_string()).or_default();
        let now = now_ms();
        s.last_at = now;
        match route {
            "sub" => {
                s.sub += 1;
                s.agents += u64::from(first_turn);
                s.last_sub_at = now;
                s.last_model = model.to_string();
                s.last_key = key.unwrap_or("").to_string();
            }
            "fallback" => s.fallback += 1,
            "error" => s.errors += 1,
            "pass" => s.pass += 1,
            other => {
                debug_assert!(false, "неизвестный маршрут {other}");
                s.errors += 1
            }
        }
    }

    /// Замер одного ответа в счёт скорости сессии.
    fn note_speed(&self, sid: &str, tokens: u64, decode_ms: u64) {
        let mut sessions = lock(&self.sessions);
        let Some(s) = sessions.get_mut(sid) else { return };
        let now = now_ms();
        if now - s.speed_at > SPEED_TURN_MS {
            (s.speed_tokens, s.speed_ms) = (0, 0);
        }
        s.speed_tokens += tokens;
        s.speed_ms += decode_ms;
        s.speed_at = now;
    }

    fn pause(&self, key: &keys::Key, secs: i64, kind: keys::PauseKind, reason: &str) {
        let until_ms = now_ms() + secs * 1000;
        let replaced_saved = {
            let mut paused = lock(&self.paused);
            // Короткая пауза не отменяет длинную: недельный лимит не «кончается» от случайного 429 на 30 с.
            if let Some(p) = paused.get_mut(&key.key).filter(|p| p.until_ms >= until_ms) {
                // Но приговор (ключ отвергнут, нет оплаты) виден сразу: иначе окно неделю показывало бы
                // «кончился лимит», а ключ давно мёртв. Срок — прежний, более долгий.
                let verdict = kind.verdict() && !p.kind.verdict();
                if verdict {
                    p.kind = kind;
                    p.reason = reason.to_string();
                }
                drop(paused);
                eprintln!("[subbar] ключ {}: {reason} — уже на более долгой паузе", key.label);
                if verdict {
                    save_paused_bg(self); // лимит в файле больше не «лимит»
                }
                return;
            }
            let old = paused.insert(key.key.clone(), keys::Pause { label: key.label.clone(), until_ms, kind, reason: reason.to_string() });
            old.is_some_and(|p| p.kind == keys::PauseKind::Limit)
        };
        // Заменили сохранённую паузу — файл тоже, иначе после перезапуска вернётся старая.
        if kind == keys::PauseKind::Limit || replaced_saved {
            save_paused_bg(self);
        }
        // Печать — после отпускания замка (см. record).
        eprintln!("[subbar] ключ {} на паузе на {}: {reason}", key.label, crate::util::format_duration(secs * 1000));
    }

    /// Паузы прошлого процесса — на ключи, чей хэш совпал (каждая переносится один раз).
    fn restore_paused<'a>(&self, keys: impl Iterator<Item = &'a keys::Key>) {
        let mut saved = lock(&self.saved_paused);
        if saved.is_empty() {
            return;
        }
        let mut paused = lock(&self.paused);
        for k in keys {
            if let Some(p) = saved.remove(&key_hash(&k.key)) {
                if paused.get(&k.key).is_none_or(|q| q.until_ms < p.until_ms) {
                    // Подпись — нынешняя: карточку могли переименовать, а окно ищет паузу по подписи.
                    paused.insert(k.key.clone(), keys::Pause { label: k.label.clone(), ..p });
                }
            }
        }
    }

    /// Запасные ключи из карточек (state.json окна). Читается в фоне и не дольше 2 с.
    async fn pool(&self) -> keys::Pool {
        let mtime = std::fs::metadata(crate::store::state_path()).and_then(|m| m.modified()).ok();
        let same_file = *lock(&self.pool_mtime) == mtime;
        if let Some((at, keys)) = lock(&self.pool).as_ref() {
            // После неудачного чтения повторяем не чаще раза в 2 с, но и не ждём POOL_TTL: карточку могли выключить.
            if (same_file && !keys.unread && at.elapsed() < POOL_TTL) || (keys.unread && at.elapsed() < Duration::from_secs(2)) {
                return keys.clone();
            }
        }
        let load = tokio::task::spawn_blocking(|| crate::store::try_load_state_within(Duration::from_millis(1800)).map(|s| keys::pool(&s.accounts)));
        // Внутри свой потолок 1,8 с; внешний — страховка на случай, если пул потоков забит. Сработает —
        // блокирующее чтение доработает само (spawn_blocking не отменить), его итог просто не ждём.
        let fresh = match tokio::time::timeout(Duration::from_secs(2), load).await {
            Ok(Ok(Ok(keys))) => keys,
            other => {
                let why = match other {
                    Ok(Ok(Err(e))) => e.to_string(),
                    Ok(Err(e)) => e.to_string(),
                    _ => "дольше 2 с".to_string(),
                };
                eprintln!("[subbar] не прочитал карточки OpenCode Go для запасных ключей: {why}");
                // Неудачу помним на 2 с, а не на POOL_TTL: не долбить state.json и журнал на каждом запросе.
                let mut stale = lock(&self.pool).as_ref().map(|(_, k)| k.clone()).unwrap_or_default();
                stale.unread = true;
                *lock(&self.pool) = Some((Instant::now(), stale.clone()));
                // pool_mtime не двигаем: иначе выключенная только что карточка (новый mtime) ещё 30 с
                // считалась бы включённой по старому пулу до POOL_TTL.
                return stale;
            }
        };
        *lock(&self.pool) = Some((Instant::now(), fresh.clone()));
        *lock(&self.pool_mtime) = mtime;
        fresh
    }

    /// Каким ключом слать сейчас: выбранный, а если он на паузе или его карточку выключили — запасной (при ротации).
    /// Второе — флаг «карточка выбранного ключа выключена», для текста ошибки.
    async fn pick_key(&self, cfg: &ProxyConfig, selected: &keys::Key) -> (Option<keys::Key>, bool, bool) {
        let pool = self.pool().await;
        let off = pool.off.contains(&selected.key);
        let usable = if off { keys::Key { label: selected.label.clone(), key: String::new() } } else { selected.clone() };
        self.restore_paused(pool.keys.iter().chain(std::iter::once(selected)));
        // Паузы и время — после чтения карточек: оно бывает до 2 с, пауза могла кончиться.
        let paused = {
            let mut p = lock(&self.paused);
            // Истёкшие — вон. Паузы ключей вне пула (карточку выключили) не трогаем: иначе недельный лимит
            // терялся при выключении и снова включении карточки. Карта ограничена числом ключей, а срок
            // паузы — 31 днём; Retry-After и выбор ключа смотрят только на пул (retry_after_secs, keys::pick).
            let now = now_ms();
            p.retain(|_, v| v.until_ms > now);
            p.clone()
        };
        // pick и так не вернёт выключенный ключ: запасной берётся только из pool.keys (включённые карточки).
        let pick = keys::pick(&usable, cfg.rotate, &pool.keys, &paused, now_ms());
        (pick, off, pool.unread)
    }

    /// Через сколько секунд освободится ближайший ключ на паузе — для Retry-After.
    /// Без ротации важен только выбранный ключ: чужая короткая пауза дала бы повтор по кругу в тот же 429.
    fn retry_after_secs(&self, cfg: &ProxyConfig) -> Option<i64> {
        let now = now_ms();
        let paused = lock(&self.paused);
        // Только паузы-лимиты: ключ, отвергнутый (401/403) или без оплаты (402), сам не оживёт —
        // 429 с Retry-After усыплял бы Claude Code по кругу вместо честного 503.
        let limit = |p: &&keys::Pause| p.kind.waits();
        let ms = if cfg.rotate {
            // Только ключи, которые могут освободиться для нас: пул и выбранный. Пауза выключенной карточки
            // дала бы клиенту повтор по кругу в тот же отказ.
            let in_pool: Option<Vec<String>> = lock(&self.pool).as_ref().filter(|(_, p)| !p.unread).map(|(_, p)| p.keys.iter().map(|k| k.key.clone()).collect());
            paused
                .iter()
                .filter(|(k, _)| k.as_str() == cfg.api_key.trim() || in_pool.as_ref().is_none_or(|pool| pool.contains(k)))
                .map(|(_, p)| p)
                .filter(limit)
                .map(|p| p.until_ms - now)
                .filter(|ms| *ms > 0)
                .min()
        } else {
            paused.get(cfg.api_key.trim()).filter(limit).map(|p| p.until_ms - now).filter(|ms| *ms > 0)
        };
        ms.map(|ms| (ms + 999) / 1000)
    }

    fn paused_now(&self) -> Vec<keys::Pause> {
        let now = now_ms();
        let mut v: Vec<keys::Pause> = lock(&self.paused).values().filter(|p| p.until_ms > now).cloned().collect();
        v.sort_by(|a, b| a.label.cmp(&b.label));
        v
    }
}

/// Запрос в работе, пока жив этот страж (он живёт в теле ответа — до последнего байта).
struct Busy(Arc<Shared>);

impl Busy {
    fn new(sh: &Arc<Shared>) -> Self {
        sh.inflight.fetch_add(1, Ordering::SeqCst);
        Busy(sh.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|never| match never {}).boxed()
}

fn json_resp(status: u16, v: Value) -> Response<Body> {
    let mut r = Response::new(full(v.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    r.headers_mut().insert("content-type", hyper::header::HeaderValue::from_static("application/json"));
    r
}

fn api_error(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": format!("SubBar: {message}")}})
}

fn short(text: &str) -> String {
    // Обрезанный текст ошибки не должен выглядеть законченным.
    if text.chars().count() > 300 {
        format!("{}…", text.chars().take(300).collect::<String>())
    } else {
        text.to_string()
    }
}

/// Ответ Anthropic клиенту — байт в байт, потоком.
fn stream_response(r: reqwest::Response) -> Response<Body> {
    let status = r.status();
    let headers = r.headers().clone();
    // Тишина дольше ANTHROPIC_WAIT — полуоткрытое соединение: без предела запрос висел бы вечно и держал выход.
    // После тишины поток кончается сам (состояние None), а не ждёт ещё столько же на каждом опросе.
    let stream = futures_util::stream::unfold(Some(Box::pin(r.bytes_stream())), |s| async move {
        let mut s = s?;
        match tokio::time::timeout(ANTHROPIC_WAIT, s.next()).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some(s))),
            Ok(None) => None,
            // Обрыв посреди ответа клиент видит сам, а журнал без этой строки молчал бы.
            Ok(Some(Err(e))) => {
                let e = e.without_url();
                eprintln!("[subbar] ответ Anthropic оборван: {e}");
                Some((Err(std::io::Error::other(e)), Some(s)))
            }
            Err(_) => {
                eprintln!("[subbar] Anthropic молчит дольше {} с — ответ оборван", ANTHROPIC_WAIT.as_secs());
                Some((Err(std::io::Error::other(format!("Anthropic молчит дольше {} с", ANTHROPIC_WAIT.as_secs()))), None))
            }
        }
    })
    .map_ok(Frame::data);
    let mut resp = Response::new(BodyExt::boxed(StreamBody::new(stream)));
    *resp.status_mut() = status;
    for (k, v) in headers.iter() {
        // Тело идёт потоком и может оборваться раньше — длину не обещаем, её проставит hyper.
        if !HOP.contains(&k.as_str()) && k != CONTENT_LENGTH {
            resp.headers_mut().append(k, v.clone());
        }
    }
    resp
}

/// Счётчик скорости на потоке ответа: время от message_start до конца и итоговые `output_tokens`
/// (их несёт message_delta). Отсчёт с message_start, а не с первого текста: скрытые мысли (display omitted)
/// в поток не идут, но в output_tokens входят — иначе их токены делились бы на миллисекунды (13963 t/s).
/// Оценок нет — нет итога или начала, нет и замера (как в Pi).
struct Meter {
    sh: Arc<Shared>,
    sid: String,
    first: Option<Instant>,
    out: Option<u64>,
    done: bool,
    /// Хвост прошлого куска: событие могло разрезаться посередине.
    carry: Vec<u8>,
}

impl Meter {
    fn feed(&mut self, chunk: &[u8]) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        if self.first.is_none() && has_event(&buf, b"message_start") {
            self.first = Some(Instant::now());
        }
        // Только строка события: слово «message_stop» может быть и в тексте ответа.
        if has_event(&buf, b"message_stop") {
            self.done = true;
        }
        let key = b"\"output_tokens\":";
        if let Some(i) = buf.windows(key.len()).rposition(|w| w == key) {
            let rest = &buf[i + key.len()..];
            let spaces = rest.iter().take_while(|c| **c == b' ').count();
            let digits: Vec<u8> = rest[spaces..].iter().take_while(|c| c.is_ascii_digit()).copied().collect();
            // Число дочитано, только если за ним что-то есть (иначе его конец — в следующем куске).
            if !digits.is_empty() && rest.len() > spaces + digits.len() {
                self.out = std::str::from_utf8(&digits).ok().and_then(|d| d.parse().ok());
            }
        }
        self.carry = carry_tail(&buf, 64);
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        if let (true, Some(first), Some(out)) = (self.done, self.first, self.out.filter(|n| *n > 0)) {
            let ms = first.elapsed().as_millis() as u64;
            // Слишком короткий ответ — не замер: одна-две пачки байтов, время почти ноль.
            if ms >= 200 {
                self.sh.note_speed(&self.sid, out, ms);
            }
        }
    }
}

/// Поток ответа Claude как есть, но с замером скорости.
fn metered(resp: Response<Body>, sh: Arc<Shared>, sid: String) -> Response<Body> {
    let (parts, body) = resp.into_parts();
    let mut meter = Meter { sh, sid, first: None, out: None, done: false, carry: Vec::new() };
    let stream = body.into_data_stream().map_ok(move |chunk: Bytes| {
        meter.feed(&chunk);
        Frame::data(chunk)
    });
    Response::from_parts(parts, BodyExt::boxed(StreamBody::new(stream)))
}

/// Учёт ответа субагента — по концу потока, а не по заголовкам: OpenCode отдаёт их раньше тела,
/// а тело может умереть уже после них (keepalive вставит `event: error` на долгой тишине, redact_sse допишет
/// обрыв). Такой ответ отдан наполовину: в счёт он не идёт, а в ошибки — да, и время полное.
/// Непотоковый (JSON) ответ целиком пришёл вместе с заголовками — его по-прежнему считаем сразу.
struct SubMeter {
    sh: Arc<Shared>,
    sid: Option<String>,
    model: String,
    key: String,
    secret: String,
    status: u16,
    t0: Instant,
    first_turn: bool,
    /// Ответ дошёл до message_stop — только такой ответ зачтём как ответ.
    done: bool,
    /// Текст события error, если поток прервался.
    why: Option<String>,
    /// Хвост прошлого куска: событие могло разрезаться посередине.
    carry: Vec<u8>,
    /// Поток провайдера дошёл до конца (или упал). Нет — тело бросил клиент (Esc, обрыв, дренаж).
    ended: bool,
}

impl SubMeter {
    fn feed(&mut self, chunk: &[u8]) {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        if has_event(&buf, b"message_stop") {
            self.done = true;
        }
        if self.why.is_none() && has_event(&buf, b"error") {
            // Провайдер может вернуть ключ эхом — в журнал и подсказку только замаскированным.
            self.why = Some(event_message(&buf).map_or_else(|| "поток прерван ошибкой".to_string(), |m| keys::redact(&m, &self.secret)));
        }
        self.carry = carry_tail(&buf, 256);
    }
}

/// Хвост куска на следующий раз: не меньше `min` байт и с начала последней строки (вместе с `\n`
/// перед ней) — иначе разрезанная `event: message_stop` потеряет признак начала строки и не найдётся.
fn carry_tail(buf: &[u8], min: usize) -> Vec<u8> {
    let cut = buf.len().saturating_sub(min);
    let line = buf.iter().rposition(|c| *c == b'\n').filter(|i| buf.len() - i <= 64 << 10);
    buf[line.map_or(cut, |i| i.min(cut))..].to_vec()
}

/// Строка события `event: <имя>` — в начале строки и до конца строки. В тексте модели (он приходит
/// внутри data) та же последовательность встречается, и счёт ответа сломала бы.
fn has_event(buf: &[u8], name: &[u8]) -> bool {
    let mut needle = Vec::with_capacity(7 + name.len());
    needle.extend_from_slice(b"event: ");
    needle.extend_from_slice(name);
    buf.windows(needle.len()).enumerate().any(|(i, w)| {
        w == needle.as_slice() && (i == 0 || buf[i - 1] == b'\n') && buf.get(i + needle.len()).is_none_or(|c| *c == b'\n' || *c == b'\r')
    })
}

impl Drop for SubMeter {
    fn drop(&mut self) {
        // message_stop после события error — ответ всё равно сорван, в «отработал» его не пишем.
        if !self.done && !self.ended && self.why.is_none() {
            // Отмену клиентом провайдеру в ошибки не пишем: строка состояния винила бы OpenCode за Esc.
            eprintln!("[subbar] отменён клиентом {} · {}", self.model, self.key);
        } else if self.done && self.why.is_none() {
            self.sh.record("sub", &self.model, self.status, self.t0, Some(&self.key), None);
            self.sh.note_session(self.sid.as_deref(), "sub", &self.model, Some(&self.key), self.first_turn);
        } else {
            let why = self.why.clone().unwrap_or_else(|| "ответ не дочитан до message_stop — поток оборвался".to_string());
            self.sh.record("error", &self.model, self.status, self.t0, Some(&self.key), Some(why.clone()));
            self.sh.note_session(self.sid.as_deref(), "error", &self.model, None, false);
        }
    }
}

/// Текст из события ошибки — в журнале человек должен видеть причину, а не «ошибка».
/// Ищем после строки `event: error`, а не где попалось: в data того же куска бывает чужое «message».
fn event_message(buf: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"\"message\":\"";
    // Как в has_event: только строка события, не та же фраза в тексте модели.
    let at = buf.windows(12).enumerate().position(|(i, w)| {
        w == b"event: error" && (i == 0 || buf[i - 1] == b'\n') && buf.get(i + 12).is_none_or(|c| *c == b'\n' || *c == b'\r')
    })?;
    let tail = &buf[at..];
    let i = tail.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let rest = &tail[i..];
    let end = rest.iter().position(|c| *c == b'"')?;
    Some(String::from_utf8_lossy(&rest[..end]).to_string())
}

/// Ответ OpenCode с учётом по концу потока.
/// `key` — подпись для журнала, `secret` — сам ключ: маскировать в тексте ошибки надо его, а не подпись.
#[allow(clippy::too_many_arguments)]
fn sub_metered(resp: Response<Body>, sh: Arc<Shared>, sid: Option<String>, model: &str, key: &str, secret: &str, t0: Instant, first_turn: bool) -> Response<Body> {
    let status = resp.status().as_u16();
    let (parts, body) = resp.into_parts();
    let meter = SubMeter { sh, sid, model: model.to_string(), key: key.to_string(), secret: secret.to_string(), status, t0, first_turn, done: false, why: None, carry: Vec::new(), ended: false };
    // Счётчик делят поток и его хвост: хвост отмечает, что провайдер дослал всё сам.
    let meter = Arc::new(Mutex::new(meter));
    let tail = meter.clone();
    let stream = body
        .into_data_stream()
        .map(move |item| {
            let mut m = lock(&meter);
            match &item {
                Ok(chunk) => m.feed(chunk),
                Err(e) => {
                    m.ended = true;
                    // Обрыв после message_stop — клиент получил ответ целиком, это не ошибка.
                    if m.why.is_none() && !m.done {
                        m.why = Some(keys::redact(&format!("поток оборвался: {e}"), &m.secret));
                    }
                }
            }
            item.map(Frame::data)
        })
        .chain(
            futures_util::stream::once(async move {
                lock(&tail).ended = true;
            })
            .filter_map(|_| async { None }),
        );
    Response::from_parts(parts, BodyExt::boxed(StreamBody::new(stream)))
}

// ─────────── поток субагента: ping в тишине, честная ошибка вместо вечного ожидания ───────────

fn at_boundary(tail: &[u8]) -> bool {
    // «\n\r\n» — тоже пустая строка по спеке SSE (LF-строка и CRLF-пустая); «\r\n\r\n» оканчивается ею же.
    tail.is_empty() || tail.ends_with(b"\n\n") || tail.ends_with(b"\n\r\n")
}

fn remember(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(&chunk[chunk.len().saturating_sub(4)..]);
    let extra = tail.len().saturating_sub(4);
    tail.drain(..extra);
}

/// Событие ошибки в формате Claude: Claude Code повторит запрос. Посреди события — сначала закрыть его.
fn error_event(tail: &[u8], why: &str) -> Bytes {
    let close = if at_boundary(tail) { "" } else { "\n\n" };
    let data = json!({"type": "error", "error": {"type": "overloaded_error", "message": format!("SubBar: {why}")}});
    Bytes::from(format!("{close}event: error\ndata: {data}\n\n"))
}

struct Keep<S> {
    inner: Pin<Box<S>>,
    tail: Vec<u8>,
    last_out: Instant,
    /// Последний реальный признак жизни — любой кусок от OpenCode, даже пустой.
    last_alive: Instant,
    /// Кредит за простой клиента: пока он переваривал прошлый кусок, тишина OpenCode не копится.
    credit: Duration,
    /// Когда отдали клиенту последний кусок (или начали).
    yielded_at: Instant,
    done: bool,
}

impl<S> Keep<S> {
    /// Сколько OpenCode молчит на самом деле: простой клиента вычитается, но не больше накопленного кредита.
    fn quiet(&self) -> Duration {
        self.last_alive.elapsed().saturating_sub(self.credit)
    }
}

/// Обёртка потока OpenCode. `sse` — в поток можно вставлять события (ping, ошибка);
/// пустые куски от внутреннего потока — признак жизни без вывода (muse думает).
fn keepalive<S>(inner: S, sse: bool, t: Timings) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let now = Instant::now();
    let st = Keep { inner: Box::pin(inner), tail: Vec::new(), last_out: now, last_alive: now, credit: Duration::ZERO, yielded_at: now, done: false };
    futures_util::stream::unfold(st, move |mut st| async move {
        if st.done {
            return None;
        }
        // Пока клиент не опрашивал нас (переваривал прошлый кусок), тишина OpenCode не копится:
        // сдвигаем её часы ровно на время этого простоя.
        let polled = Instant::now();
        // Кредит суммарный и не больше половины предела с последнего настоящего куска: сдвиг «на опрос»
        // копился бы без предела, и клиент, читающий раз в полторы минуты, отодвинул бы ошибку на сутки.
        st.credit = (st.credit + polled.saturating_duration_since(st.yielded_at)).min(t.silence / 2);
        // И ping отсчитывать от опроса: клиент только что пришёл за данными, торопить его нечем.
        st.last_out = st.last_out.max(polled);
        loop {
            // ping можно и до первых байтов: SDK Claude пропускает его где угодно.
            let can_ping = sse && at_boundary(&st.tail);
            let wait = if can_ping { t.ping.saturating_sub(st.last_out.elapsed()) } else { t.ping.min(Duration::from_secs(1)) };
            // Предел тишины — не позже срока, даже если ping реже.
            let wait = wait.min(t.silence.saturating_sub(st.quiet()));
            match tokio::time::timeout(wait.max(Duration::from_millis(5)), st.inner.next()).await {
                Ok(Some(Ok(chunk))) => {
                    // Настоящий кусок: простой клиента с этого места считаем заново.
                    st.last_alive = Instant::now();
                    if !chunk.is_empty() {
                        // Кредит тишины снимает только отданный клиенту кусок: байты по одному
                        // без конца события иначе отменяли бы предел навсегда.
                        st.credit = Duration::ZERO;
                        remember(&mut st.tail, &chunk);
                        st.last_out = Instant::now();
                        st.yielded_at = Instant::now();
                        return Some((Ok(chunk), st));
                    }
                }
                Ok(Some(Err(e))) => {
                    st.done = true;
                    let why = format!("OpenCode оборвал поток: {e}");
                    eprintln!("[subbar] {why}");
                    return Some(if sse { (Ok(error_event(&st.tail, &why)), st) } else { (Err(e), st) });
                }
                Ok(None) => return None,
                Err(_) => {}
            }
            if st.quiet() >= t.silence {
                st.done = true;
                let why = format!("OpenCode молчит {} с — обрываю, Claude Code повторит", t.silence.as_secs());
                eprintln!("[subbar] {why}");
                return Some(if sse { (Ok(error_event(&st.tail, &why)), st) } else { (Err(std::io::Error::other(why)), st) });
            }
            if can_ping && st.last_out.elapsed() >= t.ping {
                st.last_out = Instant::now();
                remember(&mut st.tail, PING);
                st.yielded_at = Instant::now();
                return Some((Ok(Bytes::from_static(PING)), st));
            }
        }
    })
}

/// Ответ OpenCode клиенту: заголовки как есть, тело — через keepalive.
/// `secret` — сам ключ: им маскируются тексты ошибок в потоке (не подпись, как `key` у соседей).
fn sub_response(r: reqwest::Response, t: Timings, secret: &str) -> Response<Body> {
    let status = r.status();
    let headers = r.headers().clone();
    let sse = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
    let raw = r.bytes_stream().map_err(std::io::Error::other);
    use futures_util::future::Either;
    // Не поток (JSON целиком) — тоже с маской: шлюз мог вернуть ключ эхом в тексте ошибки. Ответ без
    // потока приходит одним куском после генерации, так что собрать его целиком ничего не задерживает.
    let body = if sse {
        Either::Left(keepalive(redact_sse(raw, secret.to_string()), sse, t))
    } else {
        let secret = secret.to_string();
        let whole = async move {
            let mut all = Vec::new();
            let mut raw = std::pin::pin!(raw);
            while let Some(chunk) = raw.next().await {
                all.extend_from_slice(&chunk?);
                if all.len() > 64 << 20 {
                    return Err(std::io::Error::other("ответ провайдера больше 64 МБ"));
                }
            }
            Ok(Bytes::from(keys::redact(&String::from_utf8_lossy(&all), &secret)))
        };
        Either::Right(keepalive(futures_util::stream::once(whole), sse, t))
    };
    let mut resp = Response::new(BodyExt::boxed(StreamBody::new(body.map_ok(Frame::data))));
    *resp.status_mut() = status;
    for (k, v) in headers.iter() {
        // В поток могут добавиться ping — длина заранее неизвестна.
        // Тело прошло через redact — длина могла измениться; её проставит hyper.
        if !HOP.contains(&k.as_str()) && k != CONTENT_LENGTH && k != hyper::header::CONTENT_ENCODING {
            resp.headers_mut().append(k, v.clone());
        }
    }
    resp
}

// ─────────── маршруты ───────────

/// На Anthropic как есть: те же метод, путь, заголовки (включая OAuth), тело.
async fn forward_anthropic(sh: &Shared, cfg: &ProxyConfig, parts: &Parts, bytes: Bytes) -> Result<Response<Body>, String> {
    let pq = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    // База с «/v1» (как у OpenCode) дала бы «/v1/v1/messages» и 404 на каждый запрос.
    let base = cfg.anthropic_base.trim_end_matches('/');
    let base = if pq.starts_with("/v1/") { base.strip_suffix("/v1").unwrap_or(base) } else { base };
    let url = format!("{base}{pq}");
    let mut rb = sh.client.request(parts.method.clone(), url);
    // Ответы на сообщения — без сжатия: поток читает счётчик скорости (Anthropic жмёт brotli), а до
    // своего компьютера сжатие ничего не даёт. Остальное — как прислал клиент.
    let plain = parts.uri.path().starts_with("/v1/messages");
    // Заголовки, названные в Connection, — тоже для одного звена (RFC 9110 §7.6.1).
    let named: Vec<String> = parts.headers.get_all("connection").iter().chain(parts.headers.get_all("proxy-connection").iter())
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| !n.is_empty())
        .collect();
    for (k, v) in parts.headers.iter() {
        if k == HOST || k == CONTENT_LENGTH || HOP.contains(&k.as_str()) || named.iter().any(|n| n == k.as_str()) || (plain && k == "accept-encoding") {
            continue;
        }
        rb = rb.header(k, v);
    }
    if plain {
        rb = rb.header("accept-encoding", "identity");
    }
    // Не удалось даже соединиться — до Anthropic не ушло ни байта, повтор безопасен (как у OpenCode: 1× и 3× паузы).
    // Предел ожидания заголовков — как у OpenCode без потока: иначе молчащий сервер держит сокет и inflight вечно.
    const HEADERS: Duration = ANTHROPIC_WAIT;
    let timed_out = || format!("Anthropic не ответил за {} с", HEADERS.as_secs());
    for n in 0.. {
        let Some(req) = rb.try_clone() else { break };
        match tokio::time::timeout(HEADERS, req.body(bytes.clone()).send()).await {
            Err(_) => return Err(timed_out()),
            Ok(Err(e)) if e.is_connect() && n < 2 => tokio::time::sleep(sh.t.retry * (2 * n + 1)).await,
            Ok(r) => return r.map(stream_response).map_err(|e| format!("Anthropic недоступен: {}", e.without_url())),
        }
    }
    match tokio::time::timeout(HEADERS, rb.body(bytes).send()).await {
        Err(_) => Err(timed_out()),
        Ok(r) => r.map(stream_response).map_err(|e| format!("Anthropic недоступен: {}", e.without_url())),
    }
}

enum SubError {
    /// Ключ не годится сейчас (лимит, отвергнут): (секунд паузы, причина).
    Pause(i64, keys::PauseKind, String),
    /// Временный сбой по дороге (5xx, 408, обрыв соединения): тот же запрос стоит повторить через миг.
    Retry(String),
    Fail(String),
    /// OpenCode счёл кривым сам запрос (400/413/422): повтор и другой ключ дадут то же.
    Rejected(String),
}

/// Ответ, который скорее всего пройдёт со второго раза: сервер или шлюз споткнулся, запрос ни при чём.
/// 400/413/422 — ошибка самого запроса, повтор даст то же (как `IsRequestFault` в CLIProxyAPI).
fn transient_status(status: u16) -> bool {
    matches!(status, 408 | 500 | 502 | 503 | 504 | 520..=529)
}

/// Не вышло на OpenCode. `no_keys` — все ключи на паузе (а не сбой сети/сервера).
struct SubFail {
    why: String,
    no_keys: bool,
    /// Ключей нет именно из-за пауз-лимитов (а не выключенной карточки или пустого ключа) — тогда 429 со сроком.
    limited: bool,
    /// Ошибка самого запроса — клиенту 400, а не 502 «сбой по дороге» (Claude Code повторял бы его зря).
    bad_request: bool,
}

/// Маска ключей — только в событиях ошибки: в обычном потоке «sk-» бывает частью пути или слова
/// (`task-management.xlsx`), и маскировать его — портить ответ модели.
/// Поток SSE провайдера → те же события, но ошибки (event: error посреди ответа 200) — с ключом маской.
/// Режем по границам событий: ключ, разорванный между кусками, тоже найдётся.
fn redact_sse<S>(raw: S, key: String) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static
where
    S: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
{
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    // Дошёл ли ответ до конца (message_stop или ошибка): чистый обрыв от провайдера — не успех.
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tail_buf, tail_key, tail_ended) = (buf.clone(), key.clone(), ended.clone());
    let body = raw.map_ok(move |chunk| {
        let mut b = lock(&buf);
        b.extend_from_slice(&chunk);
        // Граница события: пустая строка, в том числе с CRLF («\r\n\r\n» не содержит двух \n подряд,
        // но оканчивается на «\n\r\n» — как и смешанная «data: x\n\r\n» от иных шлюзов).
        let lf = b.windows(2).rposition(|w| w == b"\n\n").map(|i| i + 2);
        let crlf = b.windows(3).rposition(|w| w == b"\n\r\n").map(|i| i + 3);
        let Some(end) = lf.max(crlf) else {
            // Границы событий нет слишком долго — это уже не SSE; копить без предела нельзя, отдаём как есть.
            if b.len() > 4 << 20 {
                let all = std::mem::take(&mut *b);
                let text = String::from_utf8_lossy(&all).into_owned();
                if has_event(&all, b"message_stop") || has_event(&all, b"error") || has_error_data(&text) {
                    ended.store(true, Ordering::SeqCst); // иначе хвост допишет «оборвался» после полного ответа
                }
                // Отдали посреди строки — закрыть событие, иначе следующее (или наша ошибка) склеится с обрывком.
                let mut out = redact_errors(text, &key);
                if !at_boundary(&all) {
                    out += "\n\n";
                }
                return Bytes::from(out);
            }
            return Bytes::new();
        };
        let ready: Vec<u8> = b.drain(..end).collect();
        let text = String::from_utf8_lossy(&ready);
        if has_event(&ready, b"message_stop") || has_event(&ready, b"error") || has_error_data(&text) {
            ended.store(true, Ordering::SeqCst);
        }
        Bytes::from(redact_errors(text.into_owned(), &key))
    });
    let tail = futures_util::stream::once(async move {
        let rest = std::mem::take(&mut *lock(&tail_buf));
        // Последнее событие без пустой строки в конце — тоже конец ответа.
        let ended = tail_ended.load(Ordering::SeqCst)
            || has_event(&rest, b"message_stop")
            || has_event(&rest, b"error")
            || has_error_data(&String::from_utf8_lossy(&rest));
        // Та же политика, что у всего потока: маска только в событиях ошибок, текст модели не трогаем.
        let mut out = if rest.is_empty() { String::new() } else { redact_errors(String::from_utf8_lossy(&rest).into_owned(), &tail_key) };
        if !ended {
            // Недописанное событие закрыть, иначе наша ошибка склеится с ним в одно.
            if !at_boundary(&rest) {
                out += "\n\n";
            }
            // Claude Code увидит ошибку и повторит запрос, а не примет обрывок за целый ответ.
            out += "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"SubBar: поток OpenCode оборвался\"}}\n\n";
        }
        Ok::<Bytes, std::io::Error>(Bytes::from(out))
    });
    // Пустые куски тела не глушим: keepalive считает их признаком жизни (событие ещё копится).
    body.chain(tail.try_filter(|b| std::future::ready(!b.is_empty())))
}

/// Маска ключа только в событиях ошибок: текст модели (например, «task-sk-management.xlsx») не трогаем.
/// Кусок может нести несколько событий — смотрим каждое по его собственной строке `event:`.
fn redact_errors(text: String, key: &str) -> String {
    // Дёшево отсеять куски без ошибок; «"api_error"» тоже должен пройти — поэтому без кавычек.
    if !text.contains("error") {
        return text;
    }
    // Построчно, а не по "\n\n": в CRLF-потоке границы событий — "\r\n\r\n", и ошибка после
    // другого события в том же куске уходила бы без маски.
    let mut in_error = false;
    text.split_inclusive('\n')
        .map(|line| {
            let t = line.trim_end_matches(['\r', '\n']);
            if t.is_empty() {
                in_error = false;
            } else if let Some(name) = t.strip_prefix("event:") {
                in_error = name.trim() == "error";
            } else if t.starts_with("data:") && is_error_data(t) {
                // Ошибка без строки `event:` — только `data: {"type":"error",…}`.
                in_error = true;
            }
            if in_error { keys::redact(line, key) } else { line.to_string() }
        })
        .collect()
}

/// Ошибка строкой `data:` без `event: error` — у чужих шлюзов бывает; тоже конец ответа.
fn has_error_data(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("data:") && is_error_data(l))
}

fn is_error_data(line: &str) -> bool {
    // `{"type":"error"}`, `{"type":"api_error"}`, `{"error":{…}}` и то же внутри массива — у чужих шлюзов бывает всякое.
    fn looks(v: &Value) -> bool {
        match v {
            Value::Array(items) => items.iter().any(looks),
            Value::Object(o) => o.get("type").and_then(Value::as_str).is_some_and(|t| t == "error" || t.ends_with("_error")) || o.get("error").is_some_and(Value::is_object),
            _ => false,
        }
    }
    serde_json::from_str::<Value>(line["data:".len()..].trim()).is_ok_and(|v| looks(&v))
}

fn session(parts: &Parts, body: &Value) -> String {
    route::session_id(body, parts.headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()))
}

/// Отправка в OpenCode с пределом ожидания ответа; ответ-отказ разбирается: пауза ключа или сбой.
/// Текст ошибки идёт в журнал, статус и уведомления — ключи в нём маской.
async fn send(sh: &Shared, rb: reqwest::RequestBuilder, stream: bool, key: &str) -> Result<reqwest::Response, SubError> {
    let wait = if stream { sh.t.headers } else { ANTHROPIC_WAIT };
    let r = match tokio::time::timeout(wait, rb.send()).await {
        Err(_) => return Err(SubError::Fail(format!("OpenCode не прислал заголовки за {:.1} с", wait.as_secs_f64()).replace('.', ","))),
        // Не дошли (соединение, DNS, TLS) — запрос не принят, повтор безопасен.
        Ok(Err(e)) => return Err(SubError::Retry(keys::redact(&format!("OpenCode недоступен: {}", if e.is_connect() { "нет соединения (сеть/VPN)".to_string() } else if e.is_timeout() { "таймаут".to_string() } else { e.without_url().to_string() }), key))),
        Ok(Ok(r)) => r,
    };
    if r.status().is_success() {
        return Ok(r);
    }
    let status = r.status().as_u16();
    let retry = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    // Тело отказа не дошло — это сбой дороги, а не приговор ключу: пустое тело иначе читается как «ключ отвергнут».
    // Не больше 256 КиБ: ошибке нужны первые строки, а не гигабайт в памяти.
    let capped = async {
        let mut s = r.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = s.next().await {
            buf.extend_from_slice(&chunk?);
            if buf.len() >= 256 << 10 {
                break;
            }
        }
        Ok::<_, reqwest::Error>(String::from_utf8_lossy(&buf).into_owned())
    };
    let Some(text) = tokio::time::timeout(Duration::from_secs(10), capped).await.ok().and_then(Result::ok) else {
        // 429/402 понятны и без тела: без паузы каждый запрос снова шёл бы в тот же лимит и ждал по 10 с.
        if let Some((secs, kind, reason)) = keys::pause_for(status, retry.as_deref(), "") {
            return Err(SubError::Pause(secs, kind, reason));
        }
        return Err(SubError::Retry(format!("OpenCode {status}: ответ оборвался")));
    };
    Err(match keys::pause_for(status, retry.as_deref(), &text) {
        Some((secs, kind, reason)) => SubError::Pause(secs, kind, reason),
        None if transient_status(status) => SubError::Retry(format!("OpenCode {status}: {}", short(&keys::redact(&text, key)))),
        None if matches!(status, 400 | 413 | 422) => SubError::Rejected(format!("OpenCode {status}: {}", short(&keys::redact(&text, key)))),
        None => SubError::Fail(format!("OpenCode {status}: {}", short(&keys::redact(&text, key)))),
    })
}

/// deepseek, space-bunny: формат Claude напрямую — другая модель, свой ключ, уровень размышления.
async fn send_native(sh: &Shared, cfg: &ProxyConfig, parts: &Parts, body: &Value, key: &keys::Key) -> Result<Response<Body>, SubError> {
    let out = route::rewrite_native(cfg, body);
    let mut rb = sh
        .client
        .post(format!("{}/messages", cfg.opencode_base.trim_end_matches('/')))
        .header("content-type", "application/json")
        .header("x-api-key", &key.key)
        .header("x-opencode-session", session(parts, body))
        // Поток режется по \n\n (события) и читается на лету: сжатый ответ разобрать нельзя.
        .header("accept-encoding", "identity")
        .header("user-agent", UA);
    for name in ["anthropic-version", "anthropic-beta", "accept"] {
        // anthropic-beta Claude Code может прислать несколькими строками — передаём все.
        for v in parts.headers.get_all(HeaderName::from_static(name)) {
            rb = rb.header(name, v);
        }
    }
    if !parts.headers.contains_key("anthropic-version") {
        rb = rb.header("anthropic-version", "2023-06-01");
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let r = send(sh, rb.body(out.to_string()), stream, &key.key).await?;
    Ok(sub_response(r, sh.t, &key.key))
}

/// muse и прочие модели Responses API: перевод запроса и потока на лету.
async fn send_responses(sh: &Shared, cfg: &ProxyConfig, parts: &Parts, body: &Value, key: &keys::Key) -> Result<Response<Body>, SubError> {
    let out = responses::to_responses(cfg, body);
    let rb = sh
        .client
        .post(format!("{}/responses", cfg.opencode_base.trim_end_matches('/')))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("accept-encoding", "identity") // поток разбираем на лету — сжатый не прочесть
        .header("authorization", format!("Bearer {}", key.key))
        .header("x-opencode-session", session(parts, body))
        .header("user-agent", UA)
        .body(out.to_string());
    let r = send(sh, rb, true, &key.key).await?;
    // В message_start — имя модели, которое просил Claude Code: ему так спокойнее.
    let claude_model = body.get("model").and_then(Value::as_str).unwrap_or("claude-haiku").to_string();
    let wants_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let up = Box::pin(r.bytes_stream());
    // Одна копия ключа на поток: в цикле клонируется только счётчик ссылок.
    let secret: std::sync::Arc<str> = key.key.as_str().into();
    let state = (up, responses::SseLines::default(), responses::StreamConv::new(&claude_model), false);
    let converted = futures_util::stream::unfold(state, move |(mut up, mut lines, mut conv, done)| {
        let secret = secret.clone();
        async move {
        if done {
            return None;
        }
        // Пульс шлюза («: heartbeat» без событий) — не признак жизни модели: читаем дальше, иначе он навсегда
        // отключил бы предел тишины при умершей генерации. Решают собранные строки, а не сырой кусок:
        // кусок — произвольная нарезка байтов и может начинаться с «:» посреди JSON.
        let next = loop {
            match up.next().await {
                Some(Ok(chunk)) => {
                    let events = lines.push(&chunk);
                    if events.is_empty() && lines.pulse_only() {
                        continue;
                    }
                    break Some(Ok(events));
                }
                Some(Err(e)) => break Some(Err(e)),
                None => break None,
            }
        };
        let (out, done) = match next {
            // Ключ, если провайдер вернул его эхом в тексте ошибки, — маской.
            // Маска — по каждому событию отдельно: соседний текст модели в том же куске не трогаем.
            // Ответ завершён (completed/failed) — поток закрываем сами, не ждём, пока провайдер закроет соединение.
            Some(Ok(events)) => (events.iter().map(|ev| redact_errors(conv.feed(ev), &secret)).collect::<String>(), conv.is_finished()),
            // Оборвалось или кончилось без response.completed — ошибка: Claude Code повторит,
            // а не примет обрезанный ответ за целый.
            Some(Err(e)) => (redact_errors(conv.fail(&format!("поток muse оборвался: {}", e.without_url())), &secret), true),
            None => {
                let tail = redact_errors(lines.finish().iter().map(|ev| conv.feed(ev)).collect(), &secret);
                (tail + &conv.fail("поток muse кончился без завершения"), true)
            }
        };
        Some((Ok::<_, std::io::Error>(Bytes::from(out)), (up, lines, conv, done)))
        }
    });
    if wants_stream {
        let body = keepalive(converted, true, sh.t);
        let mut resp = Response::new(BodyExt::boxed(StreamBody::new(body.map_ok(Frame::data))));
        resp.headers_mut().insert("content-type", "text/event-stream".parse().unwrap());
        resp.headers_mut().insert("cache-control", "no-cache".parse().unwrap());
        return Ok(resp);
    }
    // Клиенту нужен целый ответ: ещё ничего не отдано — при сбое можно откатиться.
    // С потолком: шлюз, не закрывающий поток, иначе копил бы ответ в памяти до падения прокси.
    const WHOLE_MAX: usize = 64 << 20;
    let all: Vec<Bytes> = keepalive(converted, false, sh.t)
        .map_err(|e| SubError::Fail(format!("поток muse: {e}")))
        .try_fold((Vec::new(), 0usize), |(mut all, size), b| async move {
            let size = size + b.len();
            if size > WHOLE_MAX {
                return Err(SubError::Fail("ответ muse больше 64 МБ — оборвал".to_string()));
            }
            all.push(b);
            Ok((all, size))
        })
        .await?
        .0;
    let sse: String = all.iter().map(|b| String::from_utf8_lossy(b)).collect();
    if let Some(err) = responses::stream_error(&sse) {
        return Err(SubError::Fail(keys::redact(&err, &key.key)));
    }
    Ok(json_resp(200, responses::aggregate(&sse)))
}

/// Запрос субагента или проверка связи из окна. Проверка — не субагент: она не должна менять
/// «последний ключ» в строке состояния (и не считаться заработавшим ответом).
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Live,
    Check,
}

/// На OpenCode Go. Только свой ключ: заголовки авторизации Claude сюда не попадают.
/// Лимит ключа кончился — ключ на паузу, тот же запрос — следующим ключом.
async fn forward_sub(sh: &Shared, cfg: &ProxyConfig, parts: &Parts, body: &Value, mode: Mode) -> Result<(Response<Body>, keys::Key), SubFail> {
    let label = if cfg.account_label.trim().is_empty() { "выбранный".to_string() } else { cfg.account_label.clone() };
    let selected = keys::Key { label, key: cfg.api_key.trim().to_string() };
    // Временный сбой — повтор через паузу (0,5 с, потом 1,5 с), а не сразу откат на подписку.
    // Ждём, только пока всё укладывается в разумное время: на тайм-аут заголовков повторов нет вовсе.
    let t0 = Instant::now();
    let mut retries = 0u32;
    // Каждый круг ставит на паузу один ключ — цикл кончится, когда свободных не останется.
    // Ключ, уже поставленный на паузу в этом запросе: короткий Retry-After (1 с) иначе давал до 64 одинаковых ударов.
    let mut paused_here: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..64 {
        let (key, off, unread) = sh.pick_key(cfg, &selected).await;
        let Some(key) = key else {
            let mut why = keys::why_none(&selected, &lock(&sh.paused), now_ms(), cfg.rotate, off);
            if unread && cfg.rotate {
                why += " (карточки SubBar сейчас не читаются — запасные ключи могли не подхватиться)";
            }
            let limited = !off && !selected.key.trim().is_empty();
            return Err(SubFail { why, no_keys: true, limited, bad_request: false });
        };
        let res = if cfg.is_native() { send_native(sh, cfg, parts, body, &key).await } else { send_responses(sh, cfg, parts, body, &key).await };
        match res {
            Ok(resp) => {
                // «Последний ключ» — про субагентов; проверка связи сюда не считается.
                if mode == Mode::Live {
                    *lock(&sh.last_key) = key.label.clone();
                }
                return Ok((resp, key));
            }
            Err(SubError::Pause(secs, kind, reason)) => {
                sh.pause(&key, secs, kind, &reason);
                if !paused_here.insert(key.key.clone()) {
                    return Err(SubFail { why: format!("ключ {} снова на паузе: {reason}", key.label), no_keys: true, limited: true, bad_request: false });
                }
            }
            Err(SubError::Retry(why)) => {
                let wait = sh.t.retry * (2 * retries + 1);
                if retries >= 2 || t0.elapsed() + wait > Duration::from_secs(30) {
                    let why = if retries > 0 { format!("{why} (повторов: {retries})") } else { why };
                    return Err(SubFail { why, no_keys: false, limited: false, bad_request: false });
                }
                retries += 1;
                eprintln!("[subbar] {why} — повтор {retries} через {} мс", wait.as_millis());
                tokio::time::sleep(wait).await;
            }
            Err(SubError::Fail(why)) => return Err(SubFail { why, no_keys: false, limited: false, bad_request: false }),
            Err(SubError::Rejected(why)) => return Err(SubFail { why, no_keys: false, limited: false, bad_request: true }),
        }
    }
    Err(SubFail { why: "ни один ключ OpenCode Go не подошёл".into(), no_keys: true, limited: false, bad_request: false })
}

fn status_json(sh: &Shared) -> Value {
    let cfg = sh.config();
    let paused: Vec<Value> = sh.paused_now().iter().map(|p| json!({"label": p.label, "untilMs": p.until_ms, "reason": p.reason})).collect();
    // Выбранный в окне ключ на паузе? Сами ключи в статус не попадают — только подписи.
    let selected = lock(&sh.paused)
        .get(cfg.api_key.trim())
        .filter(|p| p.until_ms > now_ms())
        .map(|p| json!({"label": p.label, "untilMs": p.until_ms, "reason": p.reason}));
    let last_key = lock(&sh.last_key).clone();
    let stats = lock(&sh.stats);
    json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "uptimeSec": sh.started.elapsed().as_secs(),
        "startedAtMs": sh.started_ms,
        "inflight": sh.inflight.load(Ordering::SeqCst),
        "draining": sh.draining.load(Ordering::SeqCst),
        "config": {
            "enabled": cfg.enabled, "port": cfg.port, "model": cfg.model, "effort": cfg.effort,
            "matchModels": cfg.match_models, "requireTools": cfg.require_tools, "fallback": cfg.fallback,
            "rotate": cfg.rotate, "account": cfg.account_label, "hasKey": !cfg.api_key.trim().is_empty(),
        },
        "keys": {"lastUsed": last_key, "paused": paused, "selected": selected},
        "stats": &*stats,
    })
}

/// Счёт одной сессии Claude Code и что сейчас в настройках — для строки состояния в терминале.
fn session_json(sh: &Shared, id: &str) -> Value {
    let cfg = sh.config();
    let found = lock(&sh.sessions).get(id).cloned();
    json!({
        "found": found.is_some(),
        "session": found,
        "enabled": cfg.enabled,
        "hasKey": !cfg.api_key.trim().is_empty(),
        "model": cfg.model,
        "matchModels": cfg.match_models,
        "selectedKey": cfg.account_label,
    })
}

/// Проверка связи: крошечный запрос субагента тем же путём (ключ, ротация, модель, перевод).
async fn check(sh: &Shared) -> Value {
    let cfg = sh.config();
    // Ровно тот запрос, который шлёт субагент на haiku: на нём и смотрим, пропустит ли его route::decide.
    // При правиле «только sonnet» предупреждение честное: haiku-субагенты и правда идут на подписку.
    let model = "claude-haiku-4-5";
    let body = json!({
        "model": model, "max_tokens": 1024, "stream": false,
        "messages": [{"role": "user", "content": "Ответь одним словом: ок"}],
        "tools": [{"name": "noop", "description": "Не вызывай.", "input_schema": {"type": "object", "properties": {}}}],
        "metadata": {"user_id": "{\"session_id\":\"subbar-check\"}"},
    });
    if cfg.api_key.trim().is_empty() {
        return json!({"ok": false, "model": cfg.model, "error": "не выбран ключ OpenCode Go", "warn": Value::Null});
    }
    if !cfg.enabled {
        return json!({"ok": false, "model": cfg.model, "error": "подмена выключена — включи «Подмена», чтобы проверить", "warn": Value::Null});
    }
    // Проверка идёт в OpenCode напрямую, мимо route::decide, — а прокси так не делает. Если правила такой
    // запрос не пропустят, связь есть, а подмена в бою не сработает: говорим об этом отдельным «warn».
    let warn = if route::decide(&cfg, "POST", "/v1/messages", Some(&body)) == Route::Sub {
        Value::Null
    } else {
        json!(format!("подмена не сработает: matchModels «{}» не ловит {} — такие запросы идут на подписку", cfg.match_models, model))
    };
    let (parts, ()) = Request::post("/v1/messages").header("anthropic-version", "2023-06-01").body(()).expect("запрос проверки собран из констант").into_parts();
    let t0 = Instant::now();
    let limit = Duration::from_secs(90);
    let run = async {
        let (resp, key) = forward_sub(sh, &cfg, &parts, &body, Mode::Check).await.map_err(|f| f.why)?;
        let status = resp.status().as_u16();
        // С потолком, как у muse: шлюз, льющий тело без конца, не раздует процесс за 90 с проверки.
        let bytes = http_body_util::Limited::new(resp.into_body(), 4 << 20).collect().await.map(|c| c.to_bytes()).map_err(|e| format!("ответ оборвался: {e}"))?;
        Ok::<_, String>((status, bytes, key))
    };
    let result = tokio::time::timeout(limit, run).await;
    match result {
        Err(_) => json!({"ok": false, "model": cfg.model, "error": format!("OpenCode не ответил за {} с", limit.as_secs()), "ms": t0.elapsed().as_millis() as u64, "warn": warn}),
        Ok(Err(why)) => json!({"ok": false, "model": cfg.model, "error": why, "ms": t0.elapsed().as_millis() as u64, "warn": warn}),
        Ok(Ok((status, bytes, key))) => {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let reply: String = v["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
            // Одни мысли без текста — модель не ответила (часто: всё ушло в размышления).
            let said = !reply.trim().is_empty() || v["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use"));
            let ok = v["type"] == "message" && said;
            let error = match () {
                _ if ok => Value::Null,
                _ if v["type"] == "message" => json!("модель ответила без текста (только размышления) — попробуй уровень пониже"),
                _ => json!(format!("ответ {status}: {}", short(&keys::redact(&String::from_utf8_lossy(&bytes), &key.key)))),
            };
            json!({"ok": ok, "model": cfg.model, "key": key.label, "status": status, "error": error,
                   "ms": t0.elapsed().as_millis() as u64, "reply": reply.trim(), "warn": warn})
        }
    }
}

/// Запрос из браузера (Origin, Sec-Fetch-*) или с чужим именем хоста (DNS rebinding): любая открытая
/// страница могла бы слать сюда запросы — а прокси сам подставляет ключ OpenCode. Claude Code так не шлёт.
fn foreign(req: &Request<Incoming>) -> bool {
    let h = req.headers();
    if h.contains_key("origin") || h.contains_key("sec-fetch-site") || h.contains_key("sec-fetch-mode") {
        return true;
    }
    // Без Host (или с нечитаемым) — не свой: браузер его всегда ставит, а ключ отдаём только своим.
    let Some(host) = h.get(HOST).and_then(|v| v.to_str().ok()) else { return true };
    let name = if host.starts_with('[') { host.split(']').next().map(|n| format!("{n}]")).unwrap_or_default() } else { host.split(':').next().unwrap_or("").to_string() };
    !matches!(name.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

async fn route_request(req: Request<Incoming>, sh: &Arc<Shared>) -> Response<Body> {
    if foreign(&req) {
        return json_resp(403, api_error("permission_error", "запросы из браузера и на чужие имена хоста не принимаются"));
    }
    match (req.method().as_str(), req.uri().path()) {
        (_, "/_subbar/status") => return json_resp(200, status_json(sh)),
        (_, "/_subbar/session") => {
            // id приходит экранированным (в нём бывают & # ?) — разобрать как настоящий запрос.
            let query = req.uri().query().unwrap_or("");
            let id = reqwest::Url::parse(&format!("http://h/?{query}"))
                .ok()
                .and_then(|u| u.query_pairs().find(|(k, _)| k == "id").map(|(_, v)| v.into_owned()))
                .unwrap_or_default();
            return json_resp(200, session_json(sh, &id));
        }
        ("POST", "/_subbar/check") => return json_resp(200, check(sh).await),
        (_, "/_subbar/check") => return json_resp(405, api_error("invalid_request_error", "проверка — только POST")),
        _ => {}
    }
    let t0 = Instant::now();
    let cfg = sh.config();
    let (parts, body) = req.into_parts();
    // Маршрут решается по сырому пути, а наверх уходит нормализованный URL: «/v1/x/../messages»
    // прошёл бы мимо подмены на подписку. Точечные сегменты клиенту не нужны — отказ — до чтения тела,
    // чтобы заведомо отвергнутый запрос не держал соединение и память.
    // «%2e» URL-парсер тоже считает точкой.
    if parts.uri.path().split('/').any(|seg| matches!(seg.to_ascii_lowercase().replace("%2e", ".").as_str(), "." | "..")) {
        return json_resp(400, api_error("invalid_request_error", "путь с «.» или «..» не принимаю"));
    }
    // Предел тела: у Anthropic запрос до 32 МБ, больше — либо ошибка, либо чужой процесс ест память.
    // Тело по байту без конца держало бы задачу вечно: на всё тело — 2 минуты.
    let collected = tokio::time::timeout(Duration::from_secs(120), http_body_util::Limited::new(body, 64 << 20).collect()).await;
    let Ok(collected) = collected else {
        return json_resp(408, api_error("invalid_request_error", "запрос не дочитался за 2 минуты"));
    };
    let bytes = match collected {
        Ok(c) => c.to_bytes(),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            return json_resp(413, api_error("request_too_large", "запрос больше 64 МБ"));
        }
        Err(e) => return json_resp(400, api_error("invalid_request_error", &format!("не прочитал запрос: {e}"))),
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let parsed: Option<Value> = if parts.method == "POST" && parts.uri.path().starts_with("/v1/messages") {
        serde_json::from_slice(&bytes).ok()
    } else {
        None
    };
    let claude_model = parsed.as_ref().and_then(|b| b.get("model")).and_then(Value::as_str).unwrap_or("").to_string();
    // Сессия Claude Code (тот же id уходит в OpenCode для кэша) — для счёта по сессиям.
    let sid = parsed.as_ref().map(|b| session(&parts, b));
    let sid = sid.as_deref();
    // Новый субагент начинает с одного сообщения; дальше каждый его ход несёт всю историю.
    let first_turn = parsed.as_ref().and_then(|b| b["messages"].as_array()).is_some_and(|m| m.len() == 1);
    let mut fell_back = None;
    if let (Route::Sub, Some(body)) = (route::decide(&cfg, parts.method.as_str(), &path, parsed.as_ref()), parsed.as_ref()) {
        match forward_sub(sh, &cfg, &parts, body, Mode::Live).await {
            Ok((resp, key)) => {
                let status = resp.status().as_u16();
                let sse = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
                // Поток считаем в его конце (SubMeter): заголовки пришли, а тело может ещё умереть.
                if sse {
                    return sub_metered(resp, sh.clone(), sid.map(str::to_string), &cfg.model, &key.label, &key.key, t0, first_turn);
                }
                sh.record("sub", &cfg.model, status, t0, Some(&key.label), None);
                sh.note_session(sid, "sub", &cfg.model, Some(&key.label), first_turn);
                return resp;
            }
            Err(f) if cfg.fallback => fell_back = Some(f.why),
            Err(f) => {
                // Все ключи на паузе — это лимит (429 со сроком), остальное — сбой по дороге (502, Claude Code повторит).
                // Кривой запрос — 400 (повтор бесполезен). Ключей нет не из-за пауз (карточка выключена, ключ не задан) —
                // это настройка, а не лимит: 429 заставлял Claude Code ждать и повторять то, что само не пройдёт.
                // Срок считаем один раз: статус и заголовок не должны разойтись, если паузу поставили между вызовами.
                let retry_after = sh.retry_after_secs(&cfg).filter(|_| f.no_keys && f.limited);
                let (status, kind) = if f.bad_request {
                    (400, "invalid_request_error")
                } else if retry_after.is_some() {
                    (429, "rate_limit_error")
                } else if f.no_keys {
                    (503, "overloaded_error")
                } else {
                    (502, "api_error")
                };
                sh.record("error", &cfg.model, status, t0, None, Some(f.why.clone()));
                sh.note_session(sid, "error", &cfg.model, None, false);
                let mut resp = json_resp(status, api_error(kind, &f.why));
                if let Some(secs) = retry_after {
                    resp.headers_mut().insert("retry-after", secs.to_string().parse().unwrap());
                }
                return resp;
            }
        }
    }
    // Мысли с подписью deepseek/space-bunny/muse Anthropic не примет — убираем заранее: на откате всегда,
    // а насквозь — когда в истории есть чужая мысль (сессия шла через OpenCode и сменила модель).
    // Обычная сессия Claude уходит байт в байт.
    let bytes = match (&fell_back, &parsed) {
        (Some(_), Some(body)) => route::strip_foreign_thinking(body).map(|b| Bytes::from(b.to_string())).unwrap_or(bytes),
        (None, Some(body)) if route::has_foreign_thinking(body) => {
            route::strip_foreign_thinking(body).map(|b| Bytes::from(b.to_string())).unwrap_or(bytes)
        }
        _ => bytes,
    };
    match forward_anthropic(sh, &cfg, &parts, bytes).await {
        Ok(resp) => {
            // Откат, который Anthropic тоже отверг, — это ошибка, а не «ушёл в подписку» штатно.
            let how = match (&fell_back, resp.status().is_client_error() || resp.status().is_server_error()) {
                // Anthropic отверг и откат, и ход основной сессии — это ошибка, а не тихий «pass» без строки в журнале.
                (_, true) => "error",
                (Some(_), false) => "fallback",
                _ => "pass",
            };
            sh.note_session(sid, how, &claude_model, None, first_turn);
            sh.record(how, &claude_model, resp.status().as_u16(), t0, None, fell_back);
            let sse = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
            match sid.filter(|_| how == "pass" && sse && resp.status().is_success()) {
                Some(sid) => metered(resp, sh.clone(), sid.to_string()),
                None => resp,
            }
        }
        Err(why) => {
            sh.record("error", &claude_model, 502, t0, None, Some(why.clone()));
            sh.note_session(sid, "error", &claude_model, None, false);
            json_resp(502, api_error("api_error", &why))
        }
    }
}

async fn handle(req: Request<Incoming>, sh: Arc<Shared>) -> Result<Response<Body>, Infallible> {
    // Опрос статуса (окно — раз в 3 с) — не работа: не держит мягкий выход и не портит «в работе».
    // Строка состояния (/_subbar/session) — тоже: иначе на весь выход она писала бы «не через claude-sub».
    if matches!(req.uri().path(), "/_subbar/status" | "/_subbar/session") {
        return Ok(route_request(req, &sh).await);
    }
    // Мягкий выход: начатое доделываем, а новое не берём — иначе при живой сессии выход не сойдётся никогда.
    if sh.draining.load(Ordering::SeqCst) {
        return Ok(json_resp(529, api_error("overloaded_error", "прокси перезапускается — повтори через пару секунд")));
    }
    let busy = Busy::new(&sh);
    let resp = route_request(req, &sh).await;
    // Страж уходит в тело: запрос «в работе», пока клиент не получил последний байт.
    Ok(resp.map(|b| {
        BodyExt::boxed(b.map_frame(move |f| {
            let _busy = &busy;
            f
        }))
    }))
}

/// `--config <путь>` из аргументов; остаток — прочие аргументы. Err — ключ без значения.
fn split_config_arg(args: &[String]) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut path = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            match it.next().filter(|v| !v.trim().is_empty() && !v.starts_with("--")) {
                Some(v) => path = Some(PathBuf::from(v)),
                None => return Err("после --config нужен путь к файлу".to_string()),
            }
        } else if let Some(v) = a.strip_prefix("--config=") {
            if v.trim().is_empty() {
                return Err("после --config нужен путь к файлу".to_string());
            }
            path = Some(PathBuf::from(v));
        } else {
            rest.push(a.clone());
        }
    }
    Ok((path, rest))
}

pub fn run(args: &[String]) -> i32 {
    let (cfg_path, rest) = match split_config_arg(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("subbar proxy: {e}");
            return 1;
        }
    };
    // Любой другой аргумент (--help, опечатка) молча запустил бы сервер и занял порт.
    if let Some(extra) = rest.first() {
        eprintln!("subbar proxy: неизвестный аргумент «{extra}». Как вызывать: subbar proxy [--config <путь>]");
        return 1;
    }
    let cfg_path = cfg_path.unwrap_or_else(config::config_path);
    let (cfg, cfg_ok) = match config::try_load(&cfg_path) {
        Ok(c) => (c, true),
        Err(e) => {
            eprintln!("[subbar] конфиг {} не прочитан ({e}) — стартую на настройках по умолчанию, перечитаю, как только поправят", cfg_path.display());
            (ProxyConfig::default(), false)
        }
    };
    let client = match reqwest::Client::builder()
        .no_gzip() // тело — как есть: сжатое Anthropic уходит клиенту сжатым
        // HTTPS_PROXY из терминала не должен получать OAuth Claude и ключи OpenCode (под launchd его и так нет).
        .no_proxy()
        .connect_timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subbar] http-клиент: {e}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[subbar] tokio: {e}");
            return 1;
        }
    };
    runtime.block_on(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let cfg_port = cfg.port;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[subbar] не занять {addr}: {e}");
                return 1;
            }
        };
        let (Ok(mut term), Ok(mut intr)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
            eprintln!("[subbar] не поставил обработчик сигналов");
            return 1;
        };
        // Порт 0 (тесты) — система даёт свободный; настоящий адрес — в первой строке журнала.
        let addr = listener.local_addr().unwrap_or(addr);
        eprintln!("[subbar] прокси {} на http://{addr} · модель {} · конфиг {}", env!("CARGO_PKG_VERSION"), cfg.model, cfg_path.display());
        let sh = Arc::new(Shared {
            cfg_path: cfg_path.clone(),
            cfg: Mutex::new((cfg, if cfg_ok { std::fs::metadata(&cfg_path).and_then(|m| m.modified()).ok() } else { None })),
            client,
            stats: Mutex::new(Stats::default()),
            started: Instant::now(),
            started_ms: now_ms(),
            t: Timings::from_env(),
            inflight: AtomicUsize::new(0),
            draining: std::sync::atomic::AtomicBool::new(false),
            cfg_bad: Mutex::new(None),
            paused: Mutex::new(keys::Paused::new()),
            saved_paused: Mutex::new(load_paused(&cfg_path)),
            pool: Mutex::new(None),
            pool_mtime: Mutex::new(None),
            sessions: Mutex::new(load_sessions(&cfg_path)),
            last_key: Mutex::new(String::new()),
            bound_port: cfg_port,
            port_moved: tokio::sync::Notify::new(),
        });
        // SIGTERM (перезапуск службы, установка новой версии): не рвать начатое. Продолжаем принимать
        // и отвечать, а выходим в первый момент, когда в работе ни одного запроса (не дольше drain).
        let mut drain_until: Option<Instant> = None;
        let mut drain_started: Option<Instant> = None;
        let mut idle_ticks = 0u32;
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        // Тик опрашивается только в дренаже: без Delay он «догонял» бы все пропущенные с запуска разом.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Счёт сессий — на диск раз в минуту, а не только на выходе: SIGKILL или паника иначе съедают всё.
        {
            let sh = sh.clone();
            tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_secs(60));
                every.tick().await;
                loop {
                    every.tick().await;
                    let sh = sh.clone();
                    let _ = tokio::task::spawn_blocking(move || save_sessions(&sh)).await;
                }
            });
        }
        const MAX_CONNS: usize = 256;
        let conns = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        // Потолок соединений: каждое может держать тело до 64 МБ. Сверх него — закрываем сразу.
                        let Ok(permit) = conns.clone().try_acquire_owned() else {
                            eprintln!("[subbar] соединений больше {MAX_CONNS} — новому ответ 503");
                            // Честный 503 вместо обрыва: клиент поймёт «занято», а не «сеть упала».
                            tokio::spawn(async move {
                                use tokio::io::AsyncWriteExt;
                                let mut stream = stream;
                                let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"прокси SubBar перегружен соединениями"}}"#;
                                let head = format!("HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nretry-after: 1\r\nconnection: close\r\n\r\n", body.len());
                                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                                    let _ = stream.write_all(head.as_bytes()).await;
                                    let _ = stream.write_all(body.as_bytes()).await;
                                    let _ = stream.shutdown().await;
                                })
                                .await;
                            });
                            continue;
                        };
                        let sh = sh.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let svc = service_fn(move |req| handle(req, sh.clone()));
                            let _ = http1::Builder::new()
                                .timer(hyper_util::rt::TokioTimer::new())
                                .header_read_timeout(Duration::from_secs(30))
                                .serve_connection(TokioIo::new(stream), svc)
                                .await;
                        });
                    }
                    Err(e) => {
                        eprintln!("[subbar] accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await
                    }
                },
                _ = term.recv() => {
                    // Повторный SIGTERM (второй клик ↻) не рвёт начатое: launchd сам добьёт по ExitTimeOut.
                    if drain_until.is_some() { continue; }
                    eprintln!("[subbar] SIGTERM: доделываю начатые запросы ({}), потом выхожу", sh.inflight.load(Ordering::SeqCst));
                    drain_until = Some(Instant::now() + sh.t.drain);
                    sh.draining.store(true, Ordering::SeqCst);
                    drain_started = Some(Instant::now());
                }
                _ = sh.port_moved.notified(), if drain_until.is_none() => {
                    eprintln!("[subbar] порт в конфиге сменён — доделываю начатое и выхожу; под службой прокси поднимется на новом, вручную — перезапусти");
                    drain_until = Some(Instant::now() + sh.t.drain);
                    sh.draining.store(true, Ordering::SeqCst);
                    drain_started = Some(Instant::now());
                }
                _ = intr.recv() => {
                    if drain_until.is_some() { break; }
                    eprintln!("[subbar] Ctrl+C: доделываю начатые запросы ({}), ещё раз — выйти сразу", sh.inflight.load(Ordering::SeqCst));
                    drain_until = Some(Instant::now() + sh.t.drain);
                    sh.draining.store(true, Ordering::SeqCst);
                    drain_started = Some(Instant::now());
                }
                _ = tick.tick(), if drain_until.is_some() => {
                    let busy = sh.inflight.load(Ordering::SeqCst);
                    // Запрос мог уже прийти в соединение, но ещё не разобран: ноль должен продержаться.
                    idle_ticks = if busy == 0 { idle_ticks + 1 } else { 0 };
                    if idle_ticks >= 3 && drain_started.is_some_and(|d| d.elapsed() >= Duration::from_millis(300)) {
                        eprintln!("[subbar] всё доделано — выхожу");
                        break;
                    }
                    if drain_until.is_some_and(|d| Instant::now() >= d) {
                        eprintln!("[subbar] не дождался {busy} запрос(ов) за {} с — выхожу", sh.t.drain.as_secs());
                        break;
                    }
                }
            }
        }
        save_sessions(&sh);
        // Фоновая запись паузы, поставленная в последние секунды дренажа, с рантаймом не доживёт:
        // пауза, ради которой файл и существует, пропала бы как раз на перезапуске.
        paused_snapshot(&sh).write();
        0
    })
}

/// `subbar statusline [--then <команда>]` — строка состояния Claude Code: сначала своя строка человека
/// (`--then`, тот же JSON на вход), потом — куда идут субагенты этой сессии.
/// `subbar statusline install|remove` — встроить в настройки Claude Code или убрать.
pub fn run_statusline(args: &[String]) -> i32 {
    use std::io::Read;
    match args.first().map(String::as_str) {
        Some("install") | Some("remove") => {
            return match control::statusline_setup(args[0] == "install") {
                Ok(msg) => {
                    println!("{msg}");
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            };
        }
        _ => {}
    }
    let mut input = String::new();
    // Запуск руками из терминала: stdin — tty, чтение висело бы до Ctrl-D. Claude Code всегда подаёт трубу.
    // Читаем в сторонке и ждём не дольше секунды: незакрытая труба (чужой лаунчер) держала бы строку вечно.
    if unsafe { libc::isatty(0) } == 0 {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::stdin().read_to_string(&mut s);
            let _ = tx.send(s);
        });
        input = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    }
    let id = serde_json::from_str::<Value>(&input).ok().and_then(|v| v["session_id"].as_str().map(str::to_string)).unwrap_or_default();
    // Claude Code запускает строку со своим окружением: адрес сессии точно говорит, через прокси ли она.
    // Порт берём из самого адреса: отставший proxy.json не должен превращать тревогу в «не запущен».
    let base_port = std::env::var("ANTHROPIC_BASE_URL").ok().and_then(|u| local_base_port(&u));
    let port = base_port.unwrap_or_else(|| config::try_load(&config::config_path()).map(|c| c.port).unwrap_or(8479));
    let reply = control::fetch_session(port, &id);
    if let Some(cmd) = args.iter().position(|a| a == "--then").map(|i| args[i + 1..].join(" ")).filter(|c| !c.trim().is_empty()) {
        let text = run_then(&cmd, &input, reply.as_ref().and_then(control::speed_text).unwrap_or_default().as_str());
        if !text.is_empty() {
            println!("{text}");
        }
    }
    let via = std::env::var("ANTHROPIC_BASE_URL").ok().map(|_| base_port.is_some());
    let width = std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<usize>().ok());
    println!("{}", control::statusline_fit(&control::statusline_text_via(reply.as_ref(), via, now_ms()), width));
    0
}

/// Порт локального адреса вида `http://127.0.0.1:8479/...`; для чужих хостов — None.
/// Сравнение подстрокой путало порт 847 с 8479.
fn local_base_port(url: &str) -> Option<u16> {
    let url = url.trim();
    // Схема без учёта регистра: «HTTP://127.0.0.1:8479» — тот же прокси, и тревога «не отвечает» не должна гаснуть.
    let rest = url.get(..7).filter(|p| p.eq_ignore_ascii_case("http://")).map(|_| &url[7..])?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = authority.rsplit_once(':')?;
    // Тот же список, что у foreign(): на «0.0.0.0» прокси ответит 403 — это не «через прокси».
    (matches!(host, "127.0.0.1" | "[::1]") || host.eq_ignore_ascii_case("localhost")).then_some(())?;
    port.parse().ok()
}

#[cfg(test)]
mod base_port_tests {
    #[test]
    fn exact_local_port_only() {
        assert_eq!(super::local_base_port("http://127.0.0.1:8479"), Some(8479));
        assert_eq!(super::local_base_port("http://localhost:8479/v1"), Some(8479));
        assert_eq!(super::local_base_port("http://[::1]:18901"), Some(18901));
        assert_eq!(super::local_base_port("https://api.anthropic.com"), None);
        assert_eq!(super::local_base_port("http://127.0.0.1.evil:8479"), None);
    }
}

/// Сколько ждём команду `--then`. Claude Code рисует строку по нашему ответу: зависшая команда вешает окно.
const THEN_WAIT: Duration = Duration::from_secs(2);

/// Команда `--then`: свой вход на stdin, предел ожидания. Пустой вывод — печатать нечего,
/// о неудаче говорим в stderr (свою строку Claude Code покажет в любом случае).
fn run_then(cmd: &str, input: &str, tps: &str) -> String {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    let child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Своя группа: `a | b` или `x &` — внуки тоже гибнут по таймауту, а не держат пайп вечно.
        .process_group(0)
        // Скорость ответов Claude этой сессии (замер прокси) — строке человека: `SUBBAR_TPS=42`.
        .env("SUBBAR_TPS", tps)
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("[subbar] не запустил «{}»", then_prog(&cmd));
        return String::new();
    };
    let t0 = Instant::now();
    // Запись — в потоке: команда, не читающая stdin, не должна вешать нас до старта таймера.
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let out = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = out {
            let _ = out.read_to_end(&mut buf);
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    let pgid = child.id() as i32;
    let kill_group = || unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    };
    let mut hung = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if t0.elapsed() < THEN_WAIT => std::thread::sleep(Duration::from_millis(20)),
            // Не ответила — убиваем всю группу: иначе строка состояния ждёт её вечно.
            Ok(None) => {
                hung = true;
                kill_group();
                let _ = child.wait();
                break None;
            }
            Err(e) => {
                eprintln!("[subbar] «{}» — {e}", then_prog(&cmd));
                kill_group();
                break None;
            }
        }
    };
    // Вывод дочитываем с тем же пределом: фоновый внук мог унести конец пайпа с собой.
    let left = THEN_WAIT.saturating_sub(t0.elapsed()).max(Duration::from_millis(100));
    let text = rx.recv_timeout(left).unwrap_or_else(|_| {
        // Внук держит пайп: гасим группу — пайп закроется, и уже напечатанное дочитается.
        kill_group();
        rx.recv_timeout(Duration::from_millis(300)).unwrap_or_default()
    });
    let text = text.trim_end_matches(['\n', '\r']).to_string();
    if text.is_empty() {
        if hung {
            eprintln!("[subbar] «{}» не ответил за {} с — печатаю свою строку", then_prog(&cmd), THEN_WAIT.as_secs());
        } else if let Some(s) = status.filter(|s| !s.success()) {
            eprintln!("[subbar] «{}» — {s} без вывода, печатаю свою строку", then_prog(&cmd));
        }
    }
    text
}

/// `subbar proxy-check` — проверка связи через работающий прокси.
pub fn run_check() -> i32 {
    let cfg = match config::try_load(&config::config_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ {} повреждён ({e}) — поправь файл", config::config_path().display());
            return 1;
        }
    };
    match control::check(cfg.port) {
        Ok(v) if v["ok"] == true => {
            println!(
                "✓ {} отвечает · ключ {} · {} мс · «{}»",
                v["model"].as_str().unwrap_or("прокси"),
                v["key"].as_str().unwrap_or(""),
                v["ms"],
                v["reply"].as_str().unwrap_or("").chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>()
            );
            // Связь есть, а правила такой запрос мимо — об этом надо сказать, а не молча вернуть «✓».
            if let Some(w) = v["warn"].as_str() {
                eprintln!("⚠ {w}");
            }
            0
        }
        Ok(v) => {
            let why = v["error"].as_str().unwrap_or("нет ответа");
            eprintln!("✗ {}: {why}", v["model"].as_str().unwrap_or(""));
            if let Some(w) = v["warn"].as_str() {
                eprintln!("⚠ {w}");
            }
            1
        }
        Err(e) => {
            eprintln!("✗ {e}");
            1
        }
    }
}

/// `subbar proxy-config [ключ=значение …]` — показать или поменять настройки из терминала.
pub fn run_config(args: &[String]) -> i32 {
    // Как у `subbar proxy`: иначе правился бы не тот файл, с которым запущен прокси.
    let (path, args) = match split_config_arg(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("subbar proxy-config: {e}");
            return 1;
        }
    };
    let args = args.as_slice();
    let path = path.unwrap_or_else(config::config_path);
    let mut cfg = match config::try_load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} повреждён ({e}) — не трогаю, чтобы не потерять ключ; поправь или удали файл", path.display());
            return 1;
        }
    };
    if args.is_empty() {
        let mut shown = serde_json::to_value(&cfg).unwrap_or_default();
        if !cfg.api_key.trim().is_empty() {
            shown["apiKey"] = json!(keys::mask(cfg.api_key.trim()));
        }
        println!("{}\n{}", path.display(), serde_json::to_string_pretty(&shown).unwrap_or_default());
        return 0;
    }
    let old_port = cfg.port;
    let was_on = cfg.enabled && !cfg.api_key.trim().is_empty();
    let mut value = serde_json::to_value(&cfg).unwrap_or_default();
    for pair in args {
        let Some((k, v)) = pair.split_once('=') else {
            // Сам аргумент не печатать: опечатка «apiKey sk-…» вывела бы ключ в терминал.
            eprintln!("ожидаю ключ=значение");
            return 1;
        };
        // Тип — по текущему значению поля: `accountLabel=2024` — строка, а не число.
        let parsed = match value.get(k) {
            None => {
                // Имя не печатаем: `sk-…=1` вывело бы вставленный ключ в терминал.
                let known: Vec<&str> = value.as_object().map(|m| m.keys().map(String::as_str).collect()).unwrap_or_default();
                eprintln!("нет такой настройки; есть: {}", known.join(", "));
                return 1;
            }
            Some(Value::Bool(_)) => match v {
                "true" => json!(true),
                "false" => json!(false),
                _ => {
                    eprintln!("{k} — true или false");
                    return 1;
                }
            },
            Some(Value::Number(_)) => match v.parse::<u64>() {
                // Порт вне диапазона прокси молча заменил бы на 8479, а CLI отрапортовал бы «сохранено».
                Ok(n) if k == "port" && !(1..=65535).contains(&n) => {
                    eprintln!("port — от 1 до 65535");
                    return 1;
                }
                Ok(n) => json!(n),
                Err(_) if k == "port" => {
                    eprintln!("port — от 1 до 65535");
                    return 1;
                }
                Err(_) => {
                    eprintln!("{k} — число");
                    return 1;
                }
            },
            Some(_) => json!(v),
        };
        value[k] = parsed;
    }
    cfg = match serde_json::from_value(value) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("плохое значение: {e}");
            return 1;
        }
    };
    // Незнакомую, но похожую на имя модель (записала версия новее) не отвергаем: иначе из-за неё
    // нельзя было бы сменить ни effort, ни порт.
    if !config::MODELS.contains(&cfg.model.as_str()) && !config::plausible_model(&cfg.model) {
        eprintln!("модель — одна из: {}", config::MODELS.join(", "));
        return 1;
    }
    if !config::EFFORTS.contains(&cfg.effort.as_str()) {
        eprintln!("effort — пусто, low, high или max");
        return 1;
    }
    // Подменять можно только субагентов: каждое правило — про haiku или sonnet (своё, вроде «3-5-haiku», тоже можно).
    // «claude» или «opus» увели бы в OpenCode основную сессию — проверяем всегда, а не только когда правило меняют.
    // Регистр как у прокси (он сравнивает без регистра): «HAIKU» — то же правило.
    cfg.match_models = cfg.match_models.to_ascii_lowercase();
    if !config::valid_rules(&cfg.match_models) {
        eprintln!("matchModels — правила через |: haiku, sonnet или свои вроде 3-5-haiku (сейчас: «{}»)", cfg.match_models);
        return 1;
    }
    cfg.opencode_base = cfg.opencode_base.trim().to_string();
    cfg.anthropic_base = cfg.anthropic_base.trim().to_string();
    if !config::valid_url(&cfg.opencode_base) || !config::valid_url(&cfg.anthropic_base) {
        eprintln!("opencodeBase/anthropicBase — адреса http(s) с хостом, без ?запроса и #якоря");
        return 1;
    }
    let port_changed = old_port != cfg.port;
    let now_on = cfg.enabled && !cfg.api_key.trim().is_empty();
    match config::save(&path, &cfg) {
        Ok(()) => {
            println!("сохранено: {}", path.display());
            if port_changed {
                println!("служба сама перезапустится на новом порту (ручной `subbar proxy` просто выйдет — запусти заново)");
            }
            if was_on && !now_on {
                println!("подмена выключена: субагенты теперь идут в подписку Claude");
            }
            0
        }
        Err(e) => {
            eprintln!("не сохранил: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn маска_ошибки_в_crlf_потоке() {
        let k = "OC-LIVE-abcdef123456";
        let s = format!("event: message_start\r\ndata: {{}}\r\n\r\nevent: error\r\ndata: {{\"m\":\"bad {k}\"}}\r\n\r\n");
        let out = super::redact_errors(s, k);
        assert!(!out.contains(k), "{out}");
        assert!(out.starts_with("event: message_start\r\n"));
    }

    fn t(ping: u64, silence: u64) -> Timings {
        Timings {
            headers: Duration::from_secs(1),
            ping: Duration::from_millis(ping),
            silence: Duration::from_millis(silence),
            drain: Duration::from_secs(1),
            retry: Duration::from_millis(10),
        }
    }

    /// Поток из заранее заданных кусков с паузами (мс) перед каждым.
    fn scripted(items: Vec<(u64, &'static str)>) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
        futures_util::stream::iter(items).then(|(pause, text)| async move {
            tokio::time::sleep(Duration::from_millis(pause)).await;
            Ok(Bytes::from_static(text.as_bytes()))
        })
    }

    async fn collect(s: impl Stream<Item = Result<Bytes, std::io::Error>>) -> (String, bool) {
        let items: Vec<Result<Bytes, std::io::Error>> = s.collect().await;
        let failed = items.iter().any(Result::is_err);
        (items.into_iter().filter_map(Result::ok).map(|b| String::from_utf8_lossy(&b).to_string()).collect(), failed)
    }

    #[tokio::test]
    async fn ping_в_тишине_только_между_событиями() {
        let s = scripted(vec![(0, "event: a\ndata: {}\n\n"), (120, "event: b\nda"), (120, "ta: {}\n\n"), (0, "event: c\ndata: {}\n\n")]);
        let (out, failed) = collect(keepalive(s, true, t(50, 5_000))).await;
        assert!(!failed);
        let (before_b, after_b) = out.split_once("event: b").unwrap();
        assert!(before_b.contains("event: ping"), "тишина после целого события — ping: {out:?}");
        let (inside_b, _) = after_b.split_once("event: c").unwrap();
        assert!(!inside_b.contains("ping"), "посреди события ping нельзя: {out:?}");
    }

    #[tokio::test]
    async fn долгая_тишина_честная_ошибка() {
        let s = scripted(vec![(0, "event: a\ndata: {}\n\n"), (60_000, "never")]);
        let (out, _) = collect(keepalive(s, true, t(20, 150))).await;
        assert!(out.ends_with("\n\n") && out.contains("event: error") && out.contains("overloaded_error"), "{out:?}");
        let s = scripted(vec![(0, "{\"par"), (60_000, "tial\"}")]);
        let (out, failed) = collect(keepalive(s, false, t(20, 150))).await;
        assert!(failed && out == "{\"par", "не SSE — вставлять нечего, только ошибка: {out:?}");
    }

    #[tokio::test]
    async fn пустые_куски_признак_жизни_а_не_тишины() {
        let items: Vec<(u64, &'static str)> = std::iter::once((0, "event: a\ndata: {}\n\n"))
            .chain((0..8).map(|_| (40, "")))
            .chain(std::iter::once((0, "event: z\ndata: {}\n\n")))
            .collect();
        let (out, failed) = collect(keepalive(scripted(items), true, t(60, 100))).await;
        assert!(!failed && out.contains("event: z") && !out.contains("event: error"), "muse думает молча, но жив: {out:?}");
        assert!(out.contains("event: ping"), "клиенту всё равно нужен ping: {out:?}");
    }

    #[tokio::test]
    async fn ping_и_до_первых_байтов() {
        let s = scripted(vec![(130, "event: message_start\ndata: {}\n\n")]);
        let (out, _) = collect(keepalive(s, true, t(50, 5_000))).await;
        assert!(out.starts_with("event: ping"), "OpenCode ответил заголовками и молчит — ping сразу: {out:?}");
    }

    #[tokio::test]
    async fn медленный_клиент_не_отключает_предел_тишины() {
        // OpenCode умер после первого события, клиент читает реже ping: ошибка всё равно обязана прийти.
        let s = scripted(vec![(0, "event: a\ndata: {}\n\n"), (60_000, "never")]);
        let mut ka = Box::pin(keepalive(s, true, t(50, 300)));
        let t0 = Instant::now();
        let mut out = String::new();
        while let Some(item) = ka.next().await {
            out.push_str(&String::from_utf8_lossy(&item.unwrap()));
            if out.contains("event: error") || t0.elapsed() > Duration::from_secs(3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
        assert!(out.contains("event: error"), "мёртвый поток обязан кончиться ошибкой: {out:?}");
    }

    #[tokio::test]
    async fn кредит_за_простой_клиента_не_копится_без_предела() {
        // Клиент опрашивает редко (дольше половины предела): сдвиг «на опрос» копиться не должен,
        // иначе ошибка тишины отодвигалась бы на каждом опросе и не наступала никогда.
        let s = scripted(vec![(0, "event: a\ndata: {}\n\n"), (60_000, "never")]);
        let mut ka = Box::pin(keepalive(s, true, t(200, 300)));
        let t0 = Instant::now();
        let mut out = String::new();
        while let Some(item) = ka.next().await {
            out.push_str(&String::from_utf8_lossy(&item.unwrap()));
            if out.contains("event: error") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(out.contains("event: error"), "предел тишины не наступил: {out:?}");
        assert!(t0.elapsed() < Duration::from_millis(300 + 150 + 500), "и не позже предела с кредитом: {:?}", t0.elapsed());
    }

    #[test]
    fn команда_строки_состояния_не_вешает_окно() {
        let t0 = Instant::now();
        assert_eq!(run_then("sleep 30", "", ""), "", "зависшая команда — пустой вывод");
        assert!(t0.elapsed() < Duration::from_secs(5), "ждали {:?}, а предел 2 с", t0.elapsed());
        assert_eq!(run_then("echo ок", "{}", "42"), "ок");
        assert_eq!(run_then("cat", "на вход", ""), "на вход", "свой вход уходит в команду");
        assert_eq!(run_then("echo $SUBBAR_TPS", "", "7"), "7", "скорость Claude уходит в переменной");
        assert_eq!(run_then("exit 3", "", ""), "", "упала без вывода — печатать нечего");
        let t0 = Instant::now();
        assert_eq!(run_then("echo ок; sleep 30 &", "", ""), "ок", "фоновый внук не держит строку");
        assert!(t0.elapsed() < Duration::from_secs(4), "ждали {:?}", t0.elapsed());
    }

    #[test]
    fn ошибка_посреди_события_сначала_закрывает_его() {
        assert!(error_event(b"da", "x").starts_with(b"\n\nevent: error"));
        assert!(error_event(b"}\n\n", "x").starts_with(b"event: error"));
    }

    #[test]
    fn событие_опознаётся_только_строкой() {
        let echo = b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"event: message_stop\"}}\n\n";
        assert!(!has_event(echo, b"message_stop"), "слово из текста модели — не событие");
        let real = b"event: message_start\ndata: {}\n\nevent: message_stop\r\ndata: {}\n\n";
        assert!(has_event(real, b"message_stop"));
        let err = "event: error\ndata: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"SubBar: поток молчит\"}}\n\n".as_bytes();
        assert!(has_event(err, b"error"));
        assert_eq!(event_message(err).as_deref(), Some("SubBar: поток молчит"), "в счёт идёт причина, а не «ошибка»");
        assert_eq!(event_message(b"event: error\ndata: {}\n\n"), None, "без текста — придумаем своё");
    }
}

/// В журнал — только программа из `--then`: остальное часто curl с заголовком авторизации.
fn then_prog(cmd: &str) -> String {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let rest = if cmd.trim() != first { " …" } else { "" };
    format!("{first}{rest}")
}
