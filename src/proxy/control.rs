//! Управление прокси из окна: статус, фоновая служба (LaunchAgent), ключи OpenCode Go.
//! Всё блокирующее — вызывать не с главного потока.

use super::config::{self, ProxyConfig};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use crate::util::plural;

/// Знак «связь есть, но подмена не сработает» в строке проверки: по нему её красят и считают дошедшей.
pub const WARN_MARK: char = '⚠';

pub const AGENT: &str = "ai.subbar.proxy";

/// Статус работающего прокси (`/_subbar/status`) или None, если не отвечает.
/// Клиент один на всё время: опрос идёт раз в 3 с, соединение переиспользуется.
pub fn fetch_status(port: u16) -> Option<Value> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    // Без системного прокси: запрос к 127.0.0.1 не должен уходить корпоративному прокси из http_proxy.
    // Неудачную сборку не кэшируем: иначе «Прокси не запущен» до конца жизни окна при живом прокси.
    let client = match CLIENT.get() {
        Some(c) => c,
        None => {
            let c = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_millis(600)).build().ok()?;
            CLIENT.get_or_init(|| c)
        }
    };
    client.get(format!("http://127.0.0.1:{port}/_subbar/status")).send().ok()?.json().ok()
}

/// Проверка связи через прокси: крошечный запрос субагента тем же путём (ключ, модель, перевод).
pub fn check(port: u16) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_secs(180)).build().map_err(|e| e.to_string())?;
    let r = client
        .post(format!("http://127.0.0.1:{port}/_subbar/check"))
        .send()
        .map_err(|e| {
            // Таймаут/обрыв при живой службе — не повод гнать человека включать службу.
            if e.is_connect() {
                format!("прокси не отвечает на :{port} — включи «Прокси как служба»")
            } else if e.is_timeout() {
                format!("прокси на :{port} не дождался ответа модели")
            } else {
                format!("прокси на :{port} оборвал проверку: {e}")
            }
        })?;
    // Старая версия прокси не знает /_subbar/check (404) или не принимает POST (405) — это не «ответил не то», а повод перезапустить.
    if matches!(r.status().as_u16(), 404 | 405) {
        return Err(format!("прокси ответил {} — перезапусти его (старая версия?)", r.status()));
    }
    if !r.status().is_success() {
        return Err(format!("прокси ответил {} — подробности в журнале ~/Library/Logs/SubBar", r.status()));
    }
    r.json().map_err(|e| format!("прокси ответил не то: {e}"))
}

fn agent_plist() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{AGENT}.plist")))
}

fn target() -> String {
    format!("gui/{}/{AGENT}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> bool {
    launchctl_output(args).is_some_and(|o| o.status.success())
}

/// launchctl с потолком времени: зависший launchd держал бы замок службы бесконечно, а тексты ошибок
/// обещают конкретные сроки. У bootout потолок выше ExitTimeOut службы (150 с) — он законно ждёт его
/// целиком; прочие команды (print, bootstrap, kill) — мгновенные запросы, им хватит 10 с,
/// иначе сотни опросов `print` в циклах ожидания растягивали бы обещанные сроки в разы.
fn launchctl_output(args: &[&str]) -> Option<std::process::Output> {
    let limit = if args.first() == Some(&"bootout") { 170 } else { 10 };
    use std::io::Read;
    let mut child = Command::new("/bin/launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Вывод читаем в потоке: `print` пишет много, и полная труба остановила бы launchctl навсегда.
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(limit);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    Some(std::process::Output { status, stdout: reader.join().unwrap_or_default(), stderr: Vec::new() })
}

/// Желаемое состояние службы, пока установка/снятие ещё идёт (они длятся до пары минут),
/// и номер решения. Номер — чтобы «вкл» и сразу «выкл» не переплелись: потоки стартуют в любом
/// порядке, и старый мог бы поднять службу уже после того, как её сняли.
static SERVICE_PENDING: std::sync::Mutex<Option<(u64, bool)>> = std::sync::Mutex::new(None);
static SERVICE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn pending() -> Option<(u64, bool)> {
    *SERVICE_PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Записать намерение, вернуть его номер: поток с другим номером — устаревший, он ничего не делает.
fn set_intent(on: bool) -> u64 {
    // Номер берём под тем же замком: иначе старший номер мог бы записаться раньше младшего и быть затёрт им.
    let mut pending = SERVICE_PENDING.lock().unwrap_or_else(|p| p.into_inner());
    let gen = SERVICE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    *pending = Some((gen, on));
    gen
}

/// Решение пользователя ещё то, за которое этот поток работает.
fn is_current(gen: u64) -> bool {
    pending().is_some_and(|(g, _)| g == gen)
}

/// Включена ли служба — с учётом ещё идущей операции: второй клик сравнивается с ней, а не с диском.
pub fn service_intent() -> bool {
    pending().map_or_else(agent_installed, |(_, on)| on)
}

/// Включить/выключить службу в фоне; итог — сообщением экрана.
pub fn set_service(on: bool) {
    let gen = set_intent(on);
    std::thread::spawn(move || {
        // Пока стояли в очереди, тумблер могли щёлкнуть ещё раз: устаревший поток не трогает службу.
        if !is_current(gen) {
            return;
        }
        // Решение перепроверяется и под замком службы: пока ждали его, могли щёлкнуть снова.
        let wanted = move || is_current(gen);
        let result = if on { install_agent_if(&wanted) } else { remove_agent_if(&wanted) };
        let (text, error) = match (on, result) {
            (true, Ok(())) => ("Служба прокси включена: работает и стартует при входе".to_string(), false),
            (false, Ok(())) => ("Служба прокси выключена".to_string(), false),
            (_, Err(e)) => (format!("Служба прокси: {e}"), true),
        };
        {
            let mut p = SERVICE_PENDING.lock().unwrap_or_else(|p| p.into_inner());
            // Не is_current(): он снова берёт этот же мьютекс — самоблокировка навсегда.
            if !p.is_some_and(|(g, _)| g == gen) {
                return; // пока шло, пользователь передумал — следующая операция уже в очереди
            }
            *p = None;
        }
        // Загружена ≠ работает: подождать ответа прокси (занятый порт — вечный перезапуск).
        // Отвечать должна именно служба: `subbar proxy` из терминала на том же порту — не она.
        let port = load_config().port;
        let alive = !on || error || (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(500));
            let pid = fetch_status(port).and_then(|v| v["pid"].as_u64());
            pid.is_some() && pid == agent_pid()
        });
        // Ждали ответа прокси — за это время могли щёлкнуть ещё раз: тогда итог не наш, молчим.
        // pending() мог уже опустеть, если следующая операция успела закончиться, — сверяем и поколение.
        if pending().is_some() || SERVICE_GEN.load(std::sync::atomic::Ordering::SeqCst) != gen {
            return;
        }
        if on && !error && !alive {
            post_notice("Служба включена, но прокси не отвечает — смотри ~/Library/Logs/SubBar/proxy.log".into(), true);
            return;
        }
        post_notice(text, error);
    });
}

/// pid процесса службы по launchd (нет процесса — None).
fn agent_pid() -> Option<u64> {
    let out = launchctl_output(&["print", &target()])?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.trim().strip_prefix("pid = ")?.trim().parse().ok())
}

pub fn agent_installed() -> bool {
    agent_plist().is_some_and(|p| p.exists())
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Включение и выключение службы — по очереди (галку можно щёлкнуть, пока идёт прошлое).
static AGENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Сколько ждём чужую операцию со службой, прежде чем сдаться (install_agent сам тянется до ~5,5 мин).
// Установка службы держит замок до 315 с (165 с ожидания выгрузки + 150 с подъёма) — ждём с запасом.
const AGENT_LOCK_WAIT: Duration = Duration::from_secs(330);
// Чужую правку settings.json ждём недолго: человек ждёт ответа тумблера (правка идёт в фоновом потоке).
const SETTINGS_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Очередь и между процессами: install.sh зовёт `proxy-service on` отдельным процессом, пока окно
/// может щёлкнуть тумблер — два bootout/bootstrap вперемешку сносили бы plist друг другу.
#[allow(dead_code)] // держатся ради Drop: отпускают блокировку
struct AgentGuard(std::sync::MutexGuard<'static, ()>, Option<std::fs::File>);

fn agent_lock() -> Result<AgentGuard, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let deadline = std::time::Instant::now() + AGENT_LOCK_WAIT;
    let inner = loop {
        match AGENT_LOCK.try_lock() {
            Ok(g) => break g,
            Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err("служба занята другим окном SubBar дольше 5,5 мин — попробуй ещё раз".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // TMPDIR на macOS свой у каждого пользователя; без TMPDIR — общий /tmp, там спасает проверка uid ниже.
    let path = std::env::temp_dir().join(format!("{AGENT}.lock"));
    // Как settings_lock: подсунутая ссылка или FIFO на месте замка — не наша защита (FIFO без читателя с O_NONBLOCK даст ENXIO).
    let file = match std::fs::OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
        Ok(f) if f.metadata().is_ok_and(|m| m.is_file() && m.uid() == unsafe { libc::geteuid() }) => f,
        Ok(_) => {
            lock_warning(format!("{} — не обычный файл или чужой", path.display()));
            return Ok(AgentGuard(inner, None));
        }
        Err(e) => {
            // Межпроцессной защиты нет — внутри процесса она ещё держит, но сказать надо.
            lock_warning(format!("не открыть {} ({e})", path.display()));
            return Ok(AgentGuard(inner, None));
        }
    };
    // Живой держатель может висеть очень долго: ждём, но не насмерть.
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(AgentGuard(inner, Some(file)));
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::WouldBlock {
            lock_warning(format!("не взять блокировку {} ({e})", path.display()));
            return Ok(AgentGuard(inner, None));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("службой занят другой процесс SubBar ({} дольше 5,5 мин) — попробуй ещё раз", path.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Без межпроцессного замка работаем дальше (внутри процесса он держит), но сказать надо и в окне:
/// приложение из Finder stderr никому не показывает.
fn lock_warning(what: String) {
    let text = format!("Замок службы: {what} — два SubBar сразу могут помешать друг другу");
    eprintln!("SubBar: {text}");
    post_notice(text, true);
}

/// Срок ожидания выгрузки старой службы (прокси доделывает начатое до ExitTimeOut 150 с).
const AGENT_GONE_WAIT: Duration = Duration::from_secs(165);
/// Срок подъёма новой службы.
const AGENT_UP_WAIT: Duration = Duration::from_secs(150);

/// Ждать, пока служба выгрузится. Срок — по часам, а не по числу попыток: `launchctl print`
/// сам бывает небыстрым (потолок 10 с), и счётчик попыток растягивал ожидание в разы.
fn wait_agent_gone() -> bool {
    let deadline = std::time::Instant::now() + AGENT_GONE_WAIT;
    loop {
        if !launchctl(&["print", &target()]) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Прокси как служба: стартует при входе, перезапускается, если упал (KeepAlive).
pub fn install_agent() -> Result<(), String> {
    install_agent_if(&|| true)
}

/// `wanted` сверяется под замком: устаревший поток окна (тумблер щёлкнули снова) службу не трогает.
fn install_agent_if(wanted: &dyn Fn() -> bool) -> Result<(), String> {
    let _serial = agent_lock()?;
    if !wanted() {
        return Ok(());
    }
    let plist = agent_plist().ok_or("HOME не задан")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Сборка из target/ — временная: `cargo clean` или пересборка оставили бы службу без бинаря.
    // Как у restart_agent, только мягче: ~/.cargo/bin/subbar (README) — законный путь.
    let exe_text = exe.to_string_lossy();
    if exe_text.contains("/target/debug/") || exe_text.contains("/target/release/") {
        return Err("сборка из target/ службу не ставит — запусти установленный SubBar".into());
    }
    // HOME тем же способом, что и для plist: иначе при не-UTF-8 HOME журнал уехал бы по относительному пути.
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).ok_or("HOME не задан")?;
    let logs = PathBuf::from(&home).join("Library/Logs/SubBar");
    std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    // Журнал launchd пишет с 0644, а в нём подписи ключей и тексты ошибок — закрыть каталог.
    std::fs::set_permissions(&logs, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|e| format!("не закрыл каталог журнала {}: {e}", logs.display()))?;
    std::fs::create_dir_all(plist.parent().ok_or("HOME не задан")?).map_err(|e| e.to_string())?;
    // launchd не читает профиль оболочки: переменные, которыми выбраны конфиг и каталог данных,
    // передаём службе явно — иначе она поднимется на другом proxy.json, чем видят окно и claude-sub.
    let env: String = ["SUBBAR_PROXY_CONFIG", "LIMITBAR_DATA_DIR"]
        .iter()
        .filter_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()).map(|v| format!("<key>{k}</key><string>{}</string>", xml(&v.to_string_lossy()))))
        .collect();
    let env = if env.is_empty() { String::new() } else { format!("\n  <key>EnvironmentVariables</key><dict>{env}</dict>") };
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>proxy</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>ExitTimeOut</key><integer>150</integer>
  <key>ProcessType</key><string>Interactive</string>{}
  <key>StandardErrorPath</key><string>{}</string>
  <key>StandardOutPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(&exe.to_string_lossy()),
        env,
        xml(&logs.join("proxy.log").to_string_lossy()),
        xml(&logs.join("proxy.log").to_string_lossy()),
    );
    // Новый plist пишем во временный файл ДО снятия старой службы: не записался (диск полон, права) —
    // рабочая служба остаётся как была, а не пропадает совсем.
    let tmp = plist.with_extension(format!("plist.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all() // без этого после сбоя питания plist может остаться пустым
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("не записал {}: {e} — служба не тронута", tmp.display())
    })?;
    let _ = launchctl(&["bootout", &target()]); // старое определение (другой путь к бинарю)
    // bootout возвращается раньше, чем launchd отпустит службу: прокси ещё доделывает начатые
    // запросы (до ExitTimeOut). Пока служба числится — bootstrap не пройдёт, ждём.
    // Старая служба не ушла — bootstrap не пройдёт, а `print` ниже принял бы её за новую.
    if !wait_agent_gone() {
        let _ = std::fs::remove_file(&tmp);
        return Err("старая служба не остановилась за 2 мин 45 с — попробуй ещё раз".into());
    }
    // rename атомарен: оборванная запись не оставит битый plist.
    std::fs::rename(&tmp, &plist).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        // Старая служба уже снята — старый plist остался бы враньём «включено» у тумблера.
        let _ = std::fs::remove_file(&plist);
        format!("не записал {}: {e} — служба не установлена", plist.display())
    })?;
    // Как config::save: без fsync каталога переименование может не пережить сбой питания.
    if let Some(Err(e)) = plist.parent().map(|dir| std::fs::File::open(dir).and_then(|d| d.sync_all())) {
        eprintln!("SubBar: plist службы записан, но fsync каталога не удался: {e}");
    }
    let domain = target().rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    launchctl(&["enable", &target()]);
    // Старый процесс может дорабатывать до минуты — ждём с запасом, прежде чем сдаться.
    let deadline = std::time::Instant::now() + AGENT_UP_WAIT;
    loop {
        if launchctl(&["bootstrap", &domain, &plist.to_string_lossy()]) || launchctl(&["print", &target()]) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Служба не поднялась — снять и её остатки, и plist: иначе переключатель врёт «вкл», а службы нет.
    let _ = launchctl(&["bootout", &target()]);
    let _ = std::fs::remove_file(&plist);
    Err("launchctl bootstrap не прошёл — служба не установлена".into())
}

pub fn remove_agent() -> Result<(), String> {
    remove_agent_if(&|| true)
}

fn remove_agent_if(wanted: &dyn Fn() -> bool) -> Result<(), String> {
    let _serial = agent_lock()?;
    if !wanted() {
        return Ok(());
    }
    let _ = launchctl(&["bootout", &target()]);
    // «Выключена» — только когда прокси правда ушёл (он доделывает начатое до минуты).
    if !wait_agent_gone() {
        return Err("прокси не остановился за 2 мин 45 с — служба осталась включённой".into());
    }
    if let Some(p) = agent_plist() {
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Мягкий перезапуск службы: SIGTERM — прокси доделывает начатые запросы и выходит, launchd поднимает
/// заново (KeepAlive). Не запущена — просто запустить. Нужен при смене порта и после установки новой версии.
pub fn restart_agent() -> Result<(), String> {
    let serial = agent_lock()?; // не посреди установки/снятия
    if !agent_installed() {
        return Err("служба не установлена".into());
    }
    // plist указывает на другой бинарь (старая версия, приложение переставили) — перезапуск поднял бы
    // ту же старую сборку. Переписать plist на текущий бинарь — это и есть установка.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Только из установленного приложения: dev-сборка из target/ службу на себя не перетягивает.
    let stale = exe.to_string_lossy().contains(".app/Contents/MacOS/")
        && agent_plist()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|body| !body.contains(&format!("<string>{}</string>", xml(&exe.to_string_lossy()))));
    if stale {
        drop(serial);
        return install_agent();
    }
    if launchctl(&["kill", "SIGTERM", &target()]) || launchctl(&["kickstart", &target()]) {
        Ok(())
    } else {
        Err("launchctl не перезапустил службу".into())
    }
}

/// Строка списка ключей OpenCode Go на экране «Субагенты».
#[derive(Clone, PartialEq)]
pub struct KeyRow {
    pub label: String,
    /// Сам ключ — не рисуется: по нему выбор «основного».
    pub key: String,
    /// «основной · в работе», «запас», «пауза · ещё 1д 3ч», «сброс через 2д», «исчерпан»…
    pub status: String,
    pub tone: Tone,
    /// Процент самого тесного окна — как в карточке (остаток или расход) и расход для цвета.
    pub shown: Option<f64>,
    pub used: Option<f64>,
    /// Подсказка: все окна, сбросы, причина паузы.
    pub tip: String,
    pub selected: bool,
    /// Карточка выключена — окно приглушает строку (не по тексту статуса: его могут переписать).
    pub dim: bool,
    /// Запас ключа так, как его считает прокси (keys::headroom): по нему — «следующий».
    room: f64,
}

/// Сравнение подписей «по-человечески»: «OpenCode #2» < «OpenCode #10».
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let chunks = |s: &str| -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for ch in s.chars() {
            let digit = ch.is_ascii_digit();
            match out.last_mut() {
                Some((d, text)) if *d == digit => text.push(ch),
                _ => out.push((digit, ch.to_string())),
            }
        }
        out
    };
    let (ca, cb) = (chunks(a), chunks(b));
    for ((da, ta), (db, tb)) in ca.iter().zip(cb.iter()) {
        let order = if *da && *db {
            ta.trim_start_matches('0').len().cmp(&tb.trim_start_matches('0').len()).then_with(|| ta.trim_start_matches('0').cmp(tb.trim_start_matches('0')))
        } else {
            ta.to_lowercase().cmp(&tb.to_lowercase())
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    ca.len().cmp(&cb.len())
}

/// Статус строки ключа выключенной карточки: окно по нему приглушает строку.
pub const CARD_OFF: &str = "карточка выключена";

/// Все ключи OpenCode Go из карточек — со статусом для субагентов: основной, в работе, запас, пауза
/// (из статуса прокси), исчерпан (по лимитам), карточка выключена. Порядок — по названию: строки
/// не прыгают под мышью после выбора.
pub fn key_rows(accounts: &[crate::model::Account], cfg: &ProxyConfig, status: Option<&Value>, now_ms: i64, show_remaining: bool) -> Vec<KeyRow> {
    let selected_key = cfg.api_key.trim();
    let serving = serving_label(status);
    let paused: Vec<(&str, i64, &str)> = status
        .and_then(|s| s["keys"]["paused"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p["label"].as_str()?, p["untilMs"].as_i64()?, p["reason"].as_str().unwrap_or("на паузе"))))
                .filter(|(_, until, _)| *until > now_ms)
                .collect()
        })
        .unwrap_or_default();
    let shown_of = |used: f64| if show_remaining { crate::util::remaining_percent(used) } else { crate::util::clamp_percent(used) };
    let mut rows: Vec<KeyRow> = Vec::new();
    // Номера строк-запасов: «следующий» выбираем по ним, а не по тексту статуса (подпись можно переписать).
    let mut spares: Vec<usize> = Vec::new();
    for account in accounts.iter().filter(|a| a.provider == crate::model::ProviderId::OpenCodeGo) {
        let Some(key) = account.credentials.get("apiKey").map(|k| k.trim()).filter(|k| !k.is_empty()) else { continue };
        if rows.iter().any(|r| r.key == key) {
            continue; // две карточки с одним ключом — одна строка
        }
        // Выключенная карточка не затеняет включённую с тем же ключом: прокси возьмёт ключ по включённой.
        if !account.enabled
            && accounts.iter().any(|a| a.enabled && a.provider == account.provider && a.credentials.get("apiKey").map(|k| k.trim()) == Some(key))
        {
            continue;
        }
        let usage = account.last_usage.as_ref();
        let windows: Vec<&crate::model::RateLimitWindow> = usage.map(|u| u.windows.iter().filter(|w| w.used_percent.is_finite()).collect()).unwrap_or_default();
        let worst = windows.iter().copied().max_by(|a, b| a.used_percent.total_cmp(&b.used_percent));
        // «Исчерпан» — только по окну, чей сброс ещё впереди: прошедший сброс (опрос застрял) лимит уже обнулил,
        // и прокси такой ключ берёт — строка не должна вечно звать его исчерпанным.
        let live_worst = windows
            .iter()
            .copied()
            .filter(|w| w.resets_at.is_none_or(|r| r > now_ms))
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent));
        let selected = key == selected_key;
        let is_serving = serving.as_deref() == Some(account.label.as_str());
        let pause = paused.iter().find(|(label, ..)| *label == account.label);
        let (status_text, tone) = if !account.enabled {
            (CARD_OFF.to_string(), if selected { Tone::Warn } else { Tone::Off })
        } else if let Some((_, until, _)) = pause {
            (format!("пауза · ещё {}", crate::util::format_reset_countdown(Some(*until), now_ms).unwrap_or_else(|| "долго".to_string())), Tone::Warn)
        } else if live_worst.is_some_and(|w| crate::util::display_percent(crate::util::remaining_percent(w.used_percent)) == 0) {
            // Процент справа и так виден — главное, когда ключ вернётся.
            let reset = live_worst.and_then(|w| crate::util::format_reset_countdown(w.resets_at, now_ms)).map(|r| format!("сброс через {r}"));
            (reset.unwrap_or_else(|| "исчерпан".to_string()), Tone::Warn)
        } else if selected {
            (if is_serving { "основной · в работе" } else { "основной" }.to_string(), Tone::Ok)
        } else if is_serving {
            ("в работе".to_string(), Tone::Ok)
        } else if cfg.rotate {
            spares.push(rows.len());
            ("запас".to_string(), Tone::Off)
        } else {
            ("не используется".to_string(), Tone::Off)
        };
        let mut tip = windows
            .iter()
            .map(|w| {
                let reset = crate::util::format_reset_countdown(w.resets_at, now_ms).map(|r| format!(", сброс через {r}")).unwrap_or_default();
                format!("{} — {} {}%{reset}", w.label, if show_remaining { "осталось" } else { "потрачено" }, crate::util::display_percent(shown_of(w.used_percent)))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some((_, _, reason)) = pause {
            tip = if tip.is_empty() { format!("Пауза: {reason}") } else { format!("Пауза: {reason}\n{tip}") };
        }
        if windows.is_empty() {
            // Причина паузы — главное: не затирать её пустым списком окон.
            tip = if tip.is_empty() { "Лимиты ещё не загружены".to_string() } else { format!("{tip}\nЛимиты ещё не загружены") };
        }
        rows.push(KeyRow {
            label: account.label.clone(),
            key: key.to_string(),
            status: status_text,
            tone,
            // Худшее из всех окон, и сбросившихся тоже (как процент на карточке), а `room` —
            // только живые окна, как считает прокси: «0%» при room 50 — не ошибка.
            shown: worst.map(|w| shown_of(w.used_percent)),
            used: worst.map(|w| w.used_percent),
            tip,
            selected,
            dim: !account.enabled,
            room: super::keys::headroom(account, now_ms),
        });
    }
    // Куда уйдёт ротация, когда кончится основной: запасной с наибольшим запасом — как выбирает прокси
    // (keys::pool: тот же запас, при равенстве — первый по порядку карточек). Поэтому — до сортировки строк.
    if cfg.rotate {
        let next = spares
            .iter()
            .map(|&i| (i, &rows[i]))
            .fold(None::<(usize, f64)>, |best, (i, r)| match best {
                Some((_, room)) if room >= r.room => best,
                _ => Some((i, r.room)),
            })
            .map(|(i, _)| i);
        if let Some(i) = next {
            rows[i].status = "запас · следующий".to_string();
        }
    }
    rows.sort_by(|a, b| natural_cmp(&a.label, &b.label));
    // Ключ задан терминалом и его нет в карточках — строкой сверху, чтобы не потерять.
    if !selected_key.is_empty() && !rows.iter().any(|r| r.selected) {
        let name = if cfg.account_label.trim().is_empty() { "Свой ключ".to_string() } else { cfg.account_label.clone() };
        // Пауза и «в работе» — по подписи, как у карточек: прокси называет такой ключ account_label.
        let pause = paused.iter().find(|(label, ..)| *label == name);
        let mask = super::keys::mask(selected_key);
        let (status, tone) = match pause {
            Some((_, until, _)) => (
                format!("пауза · ещё {} · {mask}", crate::util::format_reset_countdown(Some(*until), now_ms).unwrap_or_else(|| "долго".to_string())),
                Tone::Warn,
            ),
            None if serving.as_deref() == Some(name.as_str()) => (format!("основной · в работе · {mask}"), Tone::Ok),
            None => (format!("основной · {mask}"), Tone::Ok),
        };
        let mut tip = "Ключ задан не из карточек (subbar proxy-config)".to_string();
        if let Some((_, _, reason)) = pause {
            tip = format!("Пауза: {reason}\n{tip}");
        }
        rows.insert(0, KeyRow {
            label: name,
            key: selected_key.to_string(),
            status,
            tone,
            shown: None,
            used: None,
            tip,
            selected: true,
            dim: false,
            room: 50.0,
        });
    }
    rows
}

/// Настройки для окна. Битый файл — последние удачно прочитанные (прокси тоже работает на них).
/// Битый уже при запуске окна — удачно прочитанных нет, тогда умолчания (порт 8479, пустой ключ).
pub fn load_config() -> config::ProxyConfig {
    static LAST_GOOD: std::sync::Mutex<Option<config::ProxyConfig>> = std::sync::Mutex::new(None);
    let mut last = LAST_GOOD.lock().unwrap_or_else(|p| p.into_inner());
    match config::try_load(&config::config_path()) {
        Ok(c) => {
            *last = Some(c.clone());
            c
        }
        Err(_) => last.clone().unwrap_or_default(),
    }
}

pub fn save_config(cfg: &config::ProxyConfig) -> Result<(), String> {
    config::save(&config::config_path(), cfg).map_err(|e| e.to_string())
}

// ─────────── фоновый опрос статуса (окно не ждёт сеть на главном потоке) ───────────

static STATUS: std::sync::Mutex<(u64, Option<Value>)> = std::sync::Mutex::new((0, None));

/// Раз в 3 с спрашивает прокси; поколение растёт, только если что-то поменялось.
pub fn start_poller() {
    // Паника одного опроса (битый конфиг, неожиданный ответ) не должна гасить опрос до перезапуска окна.
    std::thread::spawn(|| loop {
        let _ = std::panic::catch_unwind(poll_status_once);
        std::thread::sleep(Duration::from_secs(3));
    });
}

fn poll_status_once() {
    {
        let port = load_config().port;
        let mut fresh = fetch_status(port);
        if let Some(v) = fresh.as_mut() {
            // Не объект (чужой сервис на порту) — индексная вставка паниковала бы и убила опрос насовсем.
            match v.as_object_mut() {
                Some(o) => {
                    o.insert("uptimeSec".into(), Value::Null); // тикает всегда — не повод перерисовывать
                }
                None => fresh = None,
            }
        }
        {
            let mut g = STATUS.lock().unwrap_or_else(|p| p.into_inner());
            if g.1 != fresh {
                g.0 += 1;
                g.1 = fresh;
            }
        }
    }
}

/// (поколение, последний статус).
pub fn cached() -> (u64, Option<Value>) {
    STATUS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// На каком ключе сейчас работают субагенты (подпись карточки): выбранный, а если ротация увела
/// на другой, не стоящий на паузе, — тот. Подмена не работает — ни на каком ключе.
pub fn serving_label(status: Option<&Value>) -> Option<String> {
    let s = status?;
    if s["config"]["enabled"] != true || s["config"]["hasKey"] != true {
        return None;
    }
    let keys = &s["keys"];
    let selected = s["config"]["account"].as_str().unwrap_or("");
    let last = keys["lastUsed"].as_str().unwrap_or("");
    let paused = |label: &str| keys["paused"].as_array().is_some_and(|a| a.iter().any(|p| p["label"] == label));
    if keys["selected"].is_object() && s["config"]["rotate"] == true && !last.is_empty() && last != selected && !paused(last) {
        return Some(last.to_string());
    }
    (!selected.is_empty()).then(|| selected.to_string())
}

/// Подмена реально работает: прокси отвечает, включён и с ключом.
pub fn active() -> bool {
    with_cached(|s| s.is_some_and(|s| s["config"]["enabled"] == true && s["config"]["hasKey"] == true))
}

/// Взгляд на статус без копии всего JSON — для отрисовки, которая идёт на каждый кадр.
pub fn with_cached<R>(f: impl FnOnce(Option<&Value>) -> R) -> R {
    f(STATUS.lock().unwrap_or_else(|p| p.into_inner()).1.as_ref())
}

// ─────────── строки для окна (чистые функции — проверяются тестами) ───────────

/// Цвет точки состояния: работает / работает, но что-то не так / не запущен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Off,
}

/// Состояние прокси, строка состояния и строка счётчиков экрана «Субагенты».
pub fn status_lines(status: Option<&Value>, cfg: &ProxyConfig, now_ms: i64) -> (Tone, String, String) {
    let Some(s) = status else {
        return (Tone::Off, "Прокси не запущен — claude-sub запустит обычного Claude".into(), "включи «Прокси как служба» внизу".into());
    };
    let version = s["version"].as_str().unwrap_or("?");
    let stale = version != env!("CARGO_PKG_VERSION");
    let (tone, state) = if !cfg.enabled {
        (Tone::Warn, "Подмена выключена — всё идёт в Claude".to_string())
    } else if cfg.api_key.trim().is_empty() {
        (Tone::Warn, "Нет ключа OpenCode Go — всё идёт в Claude".to_string())
    } else {
        // Модель и уровень видны строками ниже — здесь итог последнего субагента: отработал в OpenCode,
        // ушёл в Claude (платно — это надо видеть сразу) или получил ошибку.
        let last = s["stats"]["recent"].as_array().and_then(|r| {
            r.iter()
                .rev()
                .filter(|e| e["at"].is_i64())
                .find(|e| e["route"] == "sub" || e["route"] == "fallback" || (e["route"] == "error" && e["model"] == cfg.model.as_str()))
        });
        let ago = |e: &Value| crate::util::format_time_ago(e["at"].as_i64(), now_ms);
        let why = |e: &Value| e["note"].as_str().map(|n| format!(": {n}")).unwrap_or_default();
        // Уход в Claude или ошибка часовой давности — уже не тревога: жёлтое висело бы сутками.
        let fresh = |e: &Value| e["at"].as_i64().is_some_and(|at| now_ms - at < 3_600_000);
        let tone_of = |e: &Value| if fresh(e) { Tone::Warn } else { Tone::Ok };
        match last {
            Some(e) if e["route"] == "fallback" => (tone_of(e), format!("Последний субагент ушёл в Claude {}{}", ago(e), why(e))),
            Some(e) if e["route"] == "error" => (tone_of(e), format!("Последний субагент получил ошибку {}{}", ago(e), why(e))),
            Some(e) => (Tone::Ok, format!("Работает · последний субагент {}", ago(e))),
            // Журнал — кольцо из 30 событий: ходы основной сессии могли вытеснить субагентов, а счётчик их помнит.
            None if s["stats"]["sub"].as_u64().unwrap_or(0) > 0 => (Tone::Ok, "Работает · последний субагент уже выпал из журнала".to_string()),
            None => (Tone::Ok, "Работает · субагентов пока не было".to_string()),
        }
    };
    // Подмена выключена или нет ключа — главное: всё идёт в Claude платно. Перезапуск и старая версия это не затирают.
    let off = !cfg.enabled || cfg.api_key.trim().is_empty();
    let (tone, state) = if off {
        (tone, if stale { format!("{state} · прокси старой версии {version}") } else { state })
    } else if s["draining"] == true {
        (Tone::Warn, "Прокси перезапускается — доделывает начатое, новые запросы подождут".to_string())
    } else {
        (tone, state)
    };
    // Перезапуск уже идёт — «перезапусти» поверх него советовал бы то, что и так делается.
    let (tone, state) = if stale && !off && s["draining"] != true { (Tone::Warn, format!("Прокси старой версии {version} — перезапусти его (↻ внизу)")) } else { (tone, state) };
    let st = &s["stats"];
    let n = |k: &str| st[k].as_u64().unwrap_or(0);
    let busy = s["inflight"].as_u64().filter(|n| *n > 0).map(|n| format!(" · в работе {n}")).unwrap_or_default();
    let failure = st["recent"]
        .as_array()
        // Откат уже посчитан выше как откат; сбой — только ошибка, и не старше часа (как tone_of).
        .and_then(|r| r.iter().rev().find(|e| is_failure(e)))
        .filter(|e| e["at"].as_i64().is_some_and(|at| now_ms - at < 3_600_000))
        .map(|e| format!(" · сбой {}", crate::util::format_time_ago(e["at"].as_i64(), now_ms)))
        .unwrap_or_default();
    (
        tone,
        state,
        format!("субагентов {} · откатов в Claude {} · ошибок {} · напрямую в Claude {}{busy}{failure}", n("sub"), n("fallback"), n("errors"), n("pass")),
    )
}

/// Счётчики рядом с заголовком экрана: с какого момента (прокси считает с запуска) и сколько;
/// ошибки — только если были.
pub fn status_meta(status: Option<&Value>, now_ms: i64) -> String {
    let Some(s) = status else { return String::new() };
    let st = &s["stats"];
    let n = |k: &str| st[k].as_u64().unwrap_or(0);
    // Нули — шум: откаты и ошибки видны, только когда они были, — тогда и бросаются в глаза.
    // «sub» — ответы, а не запуски: один субагент с 12 вызовами инструментов — 12 ответов.
    let mut parts = vec![plural(n("sub"), "ответ субагента", "ответа субагентов", "ответов субагентов")];
    if n("fallback") > 0 {
        parts.push(plural(n("fallback"), "откат", "отката", "откатов"));
    }
    if n("errors") > 0 {
        parts.push(plural(n("errors"), "ошибка", "ошибки", "ошибок"));
    }
    let since = s["startedAtMs"].as_i64().map(|at| format!("с {} · ", crate::util::local_clock(at, now_ms))).unwrap_or_default();
    format!("{since}{}", parts.join(" · "))
}

/// Сбой субагента: событие с причиной, не откат, и не ошибка основной сессии (её модель — claude-…):
/// чужой 502 от Anthropic не должен выглядеть как провал субагента.
fn is_failure(e: &Value) -> bool {
    e["note"].is_string() && e["route"] != "fallback" && !(e["route"] == "error" && e["model"].as_str().is_some_and(|m| m.starts_with("claude")))
}

/// Подробности последнего сбоя — для подсказки при наведении.
pub fn last_failure(status: Option<&Value>, now_ms: i64) -> Option<String> {
    // Как «сбой» в строке счётчиков: откат — не сбой, и не старше часа — иначе подсказка тянет суточную причину.
    status?["stats"]["recent"]
        .as_array()?
        .iter()
        .rev()
        .find(|e| is_failure(e))
        .filter(|e| e["at"].as_i64().is_some_and(|at| now_ms - at < 3_600_000))
        .and_then(|e| e["note"].as_str().map(str::to_string))
}

// ─────────── строка состояния Claude Code ───────────

/// Счёт сессии Claude Code у прокси (быстро: строка состояния не должна тормозить терминал).
pub fn fetch_session(port: u16, id: &str) -> Option<Value> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let client = match CLIENT.get() {
        Some(c) => c,
        None => {
            // Не жёстче статуса окна (600 мс): под нагрузкой 300 мс давали ложное красное «не отвечает».
            let c = reqwest::blocking::Client::builder().no_proxy().timeout(Duration::from_millis(800)).build().ok()?;
            CLIENT.get_or_init(|| c)
        }
    };
    let mut url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/_subbar/session")).ok()?;
    url.query_pairs_mut().append_pair("id", id);
    client.get(url).send().ok()?.json().ok()
}

/// Строка для Claude Code (внизу терминала): куда идут субагенты этой сессии. Цвета ANSI: зелёный — работают
/// на OpenCode, янтарный — что-то ушло в Claude или подмена выключена, серый — сессия не через claude-sub.
#[cfg(test)]
pub fn statusline_text(reply: Option<&Value>, now_ms: i64) -> String {
    statusline_text_via(reply, None, now_ms)
}

/// Та же строка, когда известно, через прокси ли сессия (адрес ANTHROPIC_BASE_URL): тогда «сессия не через
/// claude-sub» не соврёт в начале сессии или после перезапуска прокси, а пропавший прокси — это тревога.
pub fn statusline_text_via(reply: Option<&Value>, via_proxy: Option<bool>, now_ms: i64) -> String {
    const DIM: &str = "\x1b[2m";
    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";
    match (reply, via_proxy) {
        (None, Some(true)) => return format!("{RED}прокси SubBar не отвечает — запросы сессии не проходят{RESET}"),
        (Some(r), Some(true)) if r["found"] != true && r["enabled"] == true && r["hasKey"] == true => {
            let model = one_line(r["model"].as_str().unwrap_or("?"));
            return format!("{DIM}{model} · пока не запускались{RESET}");
        }
        (Some(r), Some(false)) if r["found"] != true => return format!("{DIM}настоящий Claude · не через claude-sub{RESET}"),
        _ => {}
    }
    statusline_core(reply, now_ms)
}

/// Скорость ответов Claude последнего хода, как у tok-speed в Pi: от десяти — целыми, ниже — с десятой.
pub fn speed_text(reply: &Value) -> Option<String> {
    let s = &reply["session"];
    let (tokens, ms) = (s["speedTokens"].as_u64()?, s["speedMs"].as_u64()?);
    if tokens == 0 || ms == 0 {
        return None;
    }
    let tps = tokens as f64 / (ms as f64 / 1000.0);
    Some(if tps >= 10.0 { format!("{}", tps.round()) } else { format!("{}", (tps * 10.0).round() / 10.0) })
}

/// Ширина символа в колонках терминала: восточноазиатские и эмодзи занимают две.
/// Грубая эвристика по диапазонам — точность тут не нужна, нужна лишь правдоподобная длина.
fn char_cols(c: char) -> usize {
    match c as u32 {
        // Склейка и нулевые знаки — не занимают места.
        0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xA960..=0xA97F | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Ширина строки в колонках, без учёта ANSI-последовательностей.
pub fn statusline_cols(s: &str) -> usize {
    let mut n = 0;
    // 0 — обычный текст, 1 — сразу после ESC, 2 — внутри CSI до финального байта @..~.
    let mut esc = 0u8;
    for c in s.chars() {
        if c == '\x1b' {
            esc = 1;
        } else if esc == 1 {
            esc = if c == '[' { 2 } else { 0 };
        } else if esc == 2 {
            if ('@'..='~').contains(&c) {
                esc = 0;
            }
        } else {
            n += char_cols(c);
        }
    }
    n
}

/// Под ширину терминала: цветные куски («· ключ», «· N в Claude», «· N ош») уходят с конца —
/// по одному за проход, пока строка не влезет.
pub fn statusline_fit(line: &str, width: Option<usize>) -> String {
    const CUTS: [&str; 3] = ["\x1b[2m · ", "\x1b[33m · ", "\x1b[31m · "];
    let Some(width) = width.filter(|w| *w > 20) else { return line.to_string() };
    let mut out = line.to_string();
    while statusline_cols(&out) > width {
        // Порядок важен: сперва серые куски, потом жёлтые и красные — за проход по одному.
        let Some(i) = CUTS.iter().find_map(|cut| out.rfind(cut)) else { break };
        // Кусок до конца своего цвета — вместе с закрывающим переключением, иначе остаётся висячий.
        let end = out[i..].find("\x1b[0m").map_or(out.len(), |j| i + j + "\x1b[0m".len());
        out.replace_range(i..end, "");
    }
    out
}

fn statusline_core(reply: Option<&Value>, now_ms: i64) -> String {
    const GREEN: &str = "\x1b[32m";
    const YELLOW: &str = "\x1b[33m";
    const RED: &str = "\x1b[31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";
    let Some(r) = reply else {
        return format!("{DIM}настоящий Claude · прокси SubBar не запущен{RESET}");
    };
    // Подмена выключена — сессия и не появится в счёте прокси: это важнее, чем «не через claude-sub».
    if r["enabled"] == false {
        return format!("{YELLOW}подмена выключена · настоящий Claude{RESET}");
    }
    if r["found"] != true {
        return format!("{DIM}настоящий Claude · не через claude-sub{RESET}");
    }
    if r["hasKey"] != true {
        return format!("{YELLOW}нет ключа OpenCode Go · настоящий Claude{RESET}");
    }
    let s = &r["session"];
    let n = |k: &str| s[k].as_u64().unwrap_or(0);
    // Модель — тоже текст из конфига пользователя: без переводов строки и ESC.
    let model = one_line(s["lastModel"].as_str().filter(|m| !m.is_empty()).or(r["model"].as_str()).unwrap_or("?"));
    let (sub, fallback, errors) = (n("sub"), n("fallback"), n("errors"));
    if sub + fallback + errors == 0 {
        return format!("{DIM}{model} · пока не запускались{RESET}");
    }
    let agents = n("agents");
    // Коротко: строка делит ширину терминала с остальным статусом Claude Code.
    let count = if agents > 0 { format!("{agents} аг · {sub} отв") } else { format!("{sub} отв") };
    let mut line = format!("{GREEN}{model} · {count}{RESET}");
    // Ротация увела с основного ключа — видно, чей лимит сейчас тратится.
    // Подпись — текст пользователя: перевод строки в ней разорвал бы строку статуса.
    let key = one_line(s["lastKey"].as_str().unwrap_or(""));
    let key = key.as_str();
    if !key.is_empty() && r["selectedKey"].as_str().is_some_and(|sel| !sel.is_empty() && sel != key) {
        line += &format!("{DIM} · ключ {key}{RESET}");
    }
    let _ = now_ms; // время последнего ответа не показываем: строке не хватает ширины
    if fallback > 0 {
        line += &format!("{YELLOW} · {fallback} в Claude{RESET}");
    }
    if errors > 0 {
        line += &format!("{RED} · {errors} ош{RESET}");
    }
    line
}

/// Файл настроек Claude Code (с учётом CLAUDE_CONFIG_DIR).
fn claude_settings_path() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".claude")))?;
    Some(dir.join("settings.json"))
}

/// Стоит ли строка SubBar в строке состояния Claude Code.
pub fn statusline_installed() -> bool {
    claude_settings_path().is_some_and(|p| statusline_installed_at(&p))
}

/// Правка строки в Claude Code: желаемое и «рабочий уже крутится». Один рабочий применяет
/// последнее решение: быстрые щелчки вкл→выкл→вкл не дают двух потоков, финиш которых решает планировщик.
static STATUSLINE_WANT: std::sync::Mutex<(Option<bool>, bool)> = std::sync::Mutex::new((None, false));

/// Что будет в Claude Code, когда правка допишется (пока её нет — что стоит сейчас).
pub fn statusline_intent() -> bool {
    let pending = STATUSLINE_WANT.lock().unwrap_or_else(|p| p.into_inner()).0;
    pending.unwrap_or_else(statusline_installed)
}

/// Поставить или снять строку в фоне; итог — через post_notice (только по последнему решению).
pub fn set_statusline(install: bool) {
    {
        let mut g = STATUSLINE_WANT.lock().unwrap_or_else(|p| p.into_inner());
        g.0 = Some(install);
        if g.1 {
            return; // рабочий уже есть — подхватит новое решение
        }
        g.1 = true;
    }
    std::thread::spawn(|| loop {
        let Some(want) = STATUSLINE_WANT.lock().unwrap_or_else(|p| p.into_inner()).0 else {
            return;
        };
        // Паника не должна оставить «рабочий занят» навсегда — тогда тумблер строки больше не сработал бы.
        let result = std::panic::catch_unwind(|| statusline_setup(want)).unwrap_or_else(|_| Err("внутренняя ошибка SubBar".into()));
        let mut g = STATUSLINE_WANT.lock().unwrap_or_else(|p| p.into_inner());
        if g.0 != Some(want) {
            continue; // пока правили, передумали — применяем новое, этот итог уже не новость
        }
        g.0 = None;
        g.1 = false;
        drop(g);
        match result {
            Ok(msg) => post_notice(msg, false),
            Err(e) => post_notice(format!("Строка в Claude Code не изменена: {e}"), true),
        }
        return;
    });
}

fn statusline_installed_at(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v["statusLine"]["command"].as_str().and_then(wrapper_len))
        .is_some()
}

/// Длина префикса-обёртки SubBar в команде: `<любой путь>/(SubBar|subbar) statusline`.
/// Имя файла без учёта регистра: `target/release/subbar` (сборка cargo) — та же наша строка, что и
/// `SubBar.app`, иначе каждое сохранение оборачивало бы обёртку ещё раз. Кавычки вокруг пути
/// с пробелами опознанию не мешают.
fn wrapper_len(cmd: &str) -> Option<usize> {
    let (head, _) = cmd.split_once(" statusline")?;
    let name = head.trim_end_matches('\'').rsplit('/').next()?;
    let len = head.len() + " statusline".len();
    // `subbar statusline-helper` — чужая команда, не наша обёртка.
    let bounded = cmd[len..].chars().next().is_none_or(char::is_whitespace);
    (bounded && name.eq_ignore_ascii_case("subbar")).then_some(len)
}

/// Снять все слои обёртки (они могли наложиться друг на друга) — вернуть, что осталось от команды
/// человека. None — обёртки не было; пустая строка — была чистая, своей команды не осталось.
fn strip_wrapper(cmd: &str) -> Option<String> {
    let mut rest = cmd;
    let mut seen = false;
    loop {
        let Some(len) = wrapper_len(rest) else { return seen.then(|| rest.to_string()) };
        seen = true;
        match rest[len..].trim_start().strip_prefix("--then ") {
            Some(tail) => rest = tail,
            None => return Some(String::new()), // чистая обёртка
        }
        // Команду человека кладём одним словом в кавычках (см. sh_quote) — снимаем их обратно.
        if let Some(inner) = sh_unquote(rest) {
            return Some(inner);
        }
    }
}

/// Команда человека — одним словом для оболочки: иначе `a | b` уходил бы в конвейер после нашей
/// строки, а кавычки в `--label "моя сессия"` съедала бы оболочка до нас. Простые команды — как есть.
fn sh_quote(cmd: &str) -> String {
    let plain = cmd.chars().all(|c| c.is_alphanumeric() || "/._-~+=:@,".contains(c));
    if plain { cmd.to_string() } else { format!("'{}'", cmd.replace('\'', "'\\''")) }
}

/// Обратное к sh_quote: вся строка — одно слово в одинарных кавычках. Иначе None.
fn sh_unquote(s: &str) -> Option<String> {
    let body = s.strip_prefix('\'')?.strip_suffix('\'')?;
    let parts: Vec<&str> = body.split("'\\''").collect();
    parts.iter().all(|p| !p.contains('\'')).then(|| parts.join("'"))
}

/// Что записать в `settings.statusLine.command`.
enum StatuslineEdit {
    /// Уже как надо — файл не трогаем.
    Same,
    /// Нашей строки нет — файл не трогаем.
    Absent,
    /// Записать; пусто — убрать statusLine.
    Write(String),
}

/// Чистая логика правки строки состояния: `current` — что стоит сейчас, `ours` — наша обёртка на
/// текущем exe. Приложение переехало (`current` наш, но другой путь) — переписываем на `ours`,
/// сохраняя команду человека; обёртка, уже вложенная в обёртку, — разворачивается, а не множится.
fn statusline_command(current: Option<&str>, ours: &str, install: bool) -> StatuslineEdit {
    let wrap = |old: &str| if old.trim().is_empty() { ours.to_string() } else { format!("{ours} --then {}", sh_quote(old)) };
    if install {
        match current {
            None => StatuslineEdit::Write(ours.to_string()),
            Some(cmd) if wrapper_len(cmd).is_some() => {
                // Своя строка уже стоит, но путь мог смениться: молчать тут нельзя — тумблер врал бы,
                // а Claude Code звал бы уже несуществующий бинарь.
                let want = wrap(&strip_wrapper(cmd).unwrap_or_default());
                if want == cmd { StatuslineEdit::Same } else { StatuslineEdit::Write(want) }
            }
            Some(cmd) => StatuslineEdit::Write(wrap(cmd)),
        }
    } else {
        match current.and_then(strip_wrapper) {
            Some(old) => StatuslineEdit::Write(old),
            None => StatuslineEdit::Absent,
        }
    }
}

/// Встроить строку SubBar в строку состояния Claude Code (своя строка человека остаётся — выводится первой,
/// через `--then`) или убрать, вернув как было. Копия настроек перед правкой — settings.json.subbar-backup.
pub fn statusline_setup(install: bool) -> Result<String, String> {
    let path = claude_settings_path().ok_or("HOME не задан")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().to_string();
    let exe = if exe.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) { exe } else { sh_quote(&exe) };
    let message = statusline_setup_at(&path, &format!("{exe} statusline"), install)?;
    let backup = path.with_extension("json.subbar-backup");
    // Копия делается только при записи; «уже стоит»/«нет» файл не трогали — и копию не обещаем.
    let wrote = !message.starts_with("строка SubBar уже стоит") && !message.starts_with("строки SubBar в Claude Code нет");
    Ok(if wrote && backup.exists() { format!("{message} · копия: {}", backup.display()) } else { message })
}

/// Всё то же, но с явным файлом и нашей командой: тестам нужен свой каталог, а не ~/.claude.
fn statusline_setup_at(path: &std::path::Path, ours: &str, install: bool) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let _lock = settings_lock(path)?; // до чтения: иначе два процесса перепишут друг друга
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut settings: Value = serde_json::from_str(&raw).map_err(|e| format!("{} — не JSON ({e}), не трогаю", path.display()))?;
    // Корень не объект (массив/строка/null): вставка в ключ на такой разваливается с паникой,
    // а в сборке panic=abort — просто убила бы приложение. Чужой файл не трогаем.
    if !settings.is_object() {
        return Err(format!("{} — корень не объект, не трогаю", path.display()));
    }
    let current = settings["statusLine"]["command"].as_str().map(str::to_string);
    let next = match statusline_command(current.as_deref(), ours, install) {
        StatuslineEdit::Same => return Ok("строка SubBar уже стоит в Claude Code".into()),
        StatuslineEdit::Absent => return Ok("строки SubBar в Claude Code нет".into()),
        StatuslineEdit::Write(next) => next,
    };
    // Права исходного файла — иначе tmp создастся по umask (обычно 0644) и закрытость потеряется.
    let mode = std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    // Свежая копия прямо перед правкой — старая копия была бы не о том состоянии.
    let backup = path.with_extension("json.subbar-backup");
    if path.exists() {
        std::fs::copy(path, &backup).map_err(|e| format!("копия настроек: {e}"))?;
        // copy наследует 0644 оригинала, а в env настроек бывают ключи.
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("копия настроек Claude Code не закрылась (права 0600): {e}"))?;
    }
    if next.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("statusLine");
        }
    } else {
        // Прочие поля строки (padding и что добавит Claude Code) — пользователя, не выбрасываем.
        let mut line = settings["statusLine"].as_object().cloned().unwrap_or_default();
        line.insert("type".into(), serde_json::json!("command"));
        line.insert("command".into(), serde_json::json!(next));
        settings["statusLine"] = Value::Object(line);
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())? + "\n";
    write_settings(path, &text, mode)?;
    Ok(if install {
        "строка SubBar добавлена в Claude Code (видна в новых и открытых сессиях)".into()
    } else {
        "строка SubBar убрана из Claude Code, своя строка — как была".into()
    })
}

/// Блокировка read-modify-write настроек между процессами: окно и `subbar statusline install`
/// могут открыть один файл одновременно. Файл — рядом с settings.json, закрытый и только наш.
fn settings_lock(path: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let lock_path = path.with_extension("json.subbar.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&lock_path)
        .map_err(|e| format!("блокировка настроек: {e}"))?;
    // Подсунутый файл блокировки — не наша защита, а чужая дырка.
    let meta = file.metadata().map_err(|e| format!("блокировка настроек: {e}"))?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Err("файл блокировки настроек SubBar имеет небезопасный тип или владельца".into());
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("блокировка настроек: {e}"))?;
    // Живой держатель может зависнуть — ждём, но не насмерть.
    let deadline = std::time::Instant::now() + SETTINGS_LOCK_WAIT;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::WouldBlock {
            return Err(format!("блокировка настроек: {e}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err("другая SubBar правит настройки Claude Code прямо сейчас — подожди секунд пять и повтори".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Запись настроек: tmp + rename (оборванная запись не оставит битый файл) с правами исходного файла.
fn write_settings(path: &std::path::Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // settings.json — ссылка (dotfiles): пишем в цель, иначе rename заменил бы ссылку обычным файлом.
    let real = std::fs::canonicalize(path).ok().filter(|p| p != path);
    let path = real.as_deref().unwrap_or(path);
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        // Хвост прошлой попытки убираем, открываем только новый файл и не по ссылке: в settings.json лежат ключи.
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(mode).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?; // umask мог урезать режим
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| format!("{}: {e}", path.display()))
}

/// Итог проверки связи одной строкой: (текст, успех).
/// Текст из ответа — в одну строку: перевод строки или управляющий символ разорвал бы строку статуса.
fn one_line(raw: &str) -> String {
    // ANSI-последовательность вырезаем целиком: иначе от «ESC[31m» в строке остался бы «[31m».
    let mut clean = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            clean.push(c);
        }
    }
    let flat: String = clean.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn check_line(v: &Value) -> (String, bool) {
    if v["ok"] == true {
        let secs = format!("{:.1}", v["ms"].as_u64().unwrap_or(0) as f64 / 1000.0).replace('.', ",");
        // Модель в ответе может вернуть перевод строки или управляющий символ — в строке статуса
        // это разорвало бы её на куски. Всё служебное — пробелом, длинный ответ — многоточием.
        let flat = one_line(v["reply"].as_str().unwrap_or(""));
        let reply = match flat.char_indices().nth(30) {
            Some((at, _)) => format!("{}…", &flat[..at]),
            None => flat,
        };
        let reply = if reply.is_empty() { String::new() } else { format!(" · «{reply}»") };
        let line = format!("Отвечает за {secs} с · {}{reply}", one_line(v["key"].as_str().unwrap_or("")));
        // Связь есть, но правила подмены такие запросы не пропустят — зелёная галочка тут врала бы.
        match v["warn"].as_str().filter(|w| !w.is_empty()) {
            Some(warn) => (format!("{WARN_MARK} {line} — но {warn}"), false),
            None => (format!("✓ {line}"), true),
        }
    } else {
        let error = one_line(v["error"].as_str().or(v["error"]["message"].as_str()).unwrap_or("нет ответа"));
        // Предупреждение о правилах подмены важно и при сбое: иначе после починки связи — сюрприз.
        match v["warn"].as_str().filter(|w| !w.is_empty()) {
            Some(warn) => (format!("✗ {error}; к тому же {warn}"), false),
            None => (format!("✗ {error}"), false),
        }
    }
}

// ─────────── сообщения экрана «Субагенты» (из фоновых потоков; забирает таймер окна) ───────────

static NOTICE: std::sync::Mutex<Option<(String, bool, std::time::Instant)>> = std::sync::Mutex::new(None);

/// Сообщение для экрана «Субагенты» (служба включена, перезапуск…). Своё — не общая строка статуса окна.
pub fn post_notice(text: String, error: bool) {
    *NOTICE.lock().unwrap_or_else(|p| p.into_inner()) = Some((text, error, std::time::Instant::now()));
}

/// Сообщение, если оно свежее (до 10 минут): итог установки не теряется, даже если окно открыли не сразу.
pub fn take_notice() -> Option<(String, bool)> {
    let n = NOTICE.lock().unwrap_or_else(|p| p.into_inner()).take()?;
    (n.2.elapsed() < Duration::from_secs(600)).then_some((n.0, n.1))
}

// ─────────── проверка связи из окна (в фоне; итог забирает таймер окна) ───────────

/// (поколение, итог: текст, успех, когда).
type CheckState = (u64, Option<(String, bool, i64)>);
static CHECK: std::sync::Mutex<CheckState> = std::sync::Mutex::new((0, None));
/// Какая проверка идёт сейчас: (ключ+модель, когда начата) — повторный клик её не дублирует.
static CHECK_RUNNING: std::sync::Mutex<Option<(String, std::time::Instant)>> = std::sync::Mutex::new(None);
/// Последняя удачная проверка: (подпись настроек, когда) — повторно открытый экран не жжёт квоту.
static CHECK_OK: std::sync::Mutex<Option<(String, std::time::Instant)>> = std::sync::Mutex::new(None);

/// Была ли удачная проверка этих же настроек за последние `secs` секунд.
pub fn checked_recently(signature: &str, secs: u64) -> bool {
    CHECK_OK.lock().unwrap_or_else(|p| p.into_inner()).as_ref().is_some_and(|(sig, at)| sig == signature && at.elapsed() < Duration::from_secs(secs))
}

/// Запустить проверку для этих ключа и модели. Та же уже идёт (моложе 3 мин) — false, запрос не шлём:
/// каждая проверка — живой запрос к модели. Старые проверки свой итог уже не покажут.
pub fn start_check(signature: &str) -> bool {
    let started = std::time::Instant::now();
    {
        let mut running = CHECK_RUNNING.lock().unwrap_or_else(|p| p.into_inner());
        if running.as_ref().is_some_and(|(sig, at)| sig == signature && at.elapsed() < Duration::from_secs(200)) { // с запасом над таймаутом запроса (180 с)
            return false;
        }
        *running = Some((signature.to_string(), started));
    }
    let generation = {
        let mut g = CHECK.lock().unwrap_or_else(|p| p.into_inner());
        g.0 += 1;
        g.1 = None;
        g.0
    };
    let signature = signature.to_string();
    std::thread::spawn(move || {
        let (text, ok) = match check(load_config().port) {
            Ok(v) => check_line(&v),
            Err(e) => (format!("✗ {e}"), false),
        };
        // Предупреждение — связь есть: повторное открытие экрана не должно снова жечь квоту.
        let reached = ok || text.starts_with(WARN_MARK);
        // Неудача не стирает метку прошлой удачной проверки другого ключа — иначе лишний платный запрос.
        // Итог, отброшенный более новой проверкой, метку не трогает: иначе затёр бы её свежую удачу.
        let current = CHECK.lock().unwrap_or_else(|p| p.into_inner()).0 == generation;
        if current {
            let mut last = CHECK_OK.lock().unwrap_or_else(|p| p.into_inner());
            if reached {
                *last = Some((signature.clone(), std::time::Instant::now()));
            } else if last.as_ref().is_some_and(|(sig, _)| *sig == signature) {
                *last = None; // этот же ключ теперь не отвечает — старой удаче не верим
            }
        }
        {
            let mut g = CHECK.lock().unwrap_or_else(|p| p.into_inner());
            if g.0 == generation {
                g.1 = Some((text, ok, crate::store::now_ms()));
            }
        }
        // Слот — последним, когда итог уже записан: иначе в щели второй клик запустил бы ту же
        // платную проверку заново. Замки выше уже отпущены — порядок с start_check не пересекается.
        {
            let mut running = CHECK_RUNNING.lock().unwrap_or_else(|p| p.into_inner());
            // Свой слот, а не любой с той же подписью: проверка на грани таймаута стёрла бы слот следующей.
            if running.as_ref().is_some_and(|(sig, at)| *sig == signature && *at == started) {
                *running = None;
            }
        }
    });
    true
}

/// (поколение, итог); итог None — проверка идёт или не запускалась.
pub fn check_result() -> CheckState {
    CHECK.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Итог для показа: старый — с пометкой, когда он был.
pub fn check_text(text: &str, at_ms: i64, now_ms: i64) -> String {
    if now_ms - at_ms < 60_000 {
        text.to_string()
    } else {
        format!("{text} ({})", crate::util::format_time_ago(Some(at_ms), now_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> ProxyConfig {
        ProxyConfig { api_key: "K2".into(), account_label: "OpenCode #2".into(), ..Default::default() }
    }

    fn opencode(label: &str, key: &str, used: f64, enabled: bool) -> crate::model::Account {
        crate::model::Account {
            id: label.into(),
            provider: crate::model::ProviderId::OpenCodeGo,
            label: label.into(),
            enabled,
            credentials: [("apiKey".to_string(), key.to_string())].into(),
            options: Default::default(),
            created_at: 0,
            last_usage: Some(crate::model::AccountUsage {
                status: crate::model::FetchStatus::Ok,
                windows: vec![crate::model::RateLimitWindow { key: "30d".into(), label: "30д".into(), used_percent: used, window_minutes: 43200, resets_at: Some(3 * 86_400_000), note: None }],
                plan_type: None,
                notes: vec![],
                error: None,
                updated_at: 0,
                last_ok_at: Some(0),
            }),
            selected_model: None,
            selected_window: None,
        }
    }

    #[test]
    fn ключи_со_статусами_для_субагентов() {
        let accounts = vec![
            opencode("OpenCode #10", "K10", 10.0, true),
            opencode("OpenCode #2", "K2", 30.0, true),
            opencode("OpenCode #3", "K3", 51.0, true),
            opencode("OpenCode #4", "K4", 100.0, true),
            opencode("OpenCode #5", "K5", 20.0, false),
            opencode("OpenCode #6", "K2", 0.0, true), // тот же ключ, что у #2 — одна строка
        ];
        // Выбранный #2 на паузе, ротация увела на #3.
        let st = json!({"config": {"enabled": true, "hasKey": true, "rotate": true, "account": "OpenCode #2"},
            "keys": {"lastUsed": "OpenCode #3", "selected": {"label": "OpenCode #2"},
                     "paused": [{"label": "OpenCode #2", "untilMs": 1_000 + 26 * 3_600_000, "reason": "кончился недельный лимит"}]}});
        let rows = key_rows(&accounts, &cfg(), Some(&st), 1_000, true);
        let view: Vec<(&str, &str, bool)> = rows.iter().map(|r| (r.label.as_str(), r.status.as_str(), r.selected)).collect();
        assert_eq!(view, [
            ("OpenCode #2", "пауза · ещё 1д 2ч", true),
            ("OpenCode #3", "в работе", false),
            ("OpenCode #4", "сброс через 2д 23ч", false),
            ("OpenCode #5", "карточка выключена", false),
            ("OpenCode #10", "запас · следующий", false),
        ]);
        assert!(rows[0].tip.starts_with("Пауза: кончился недельный лимит"));
        assert_eq!(rows[1].shown, Some(49.0), "остаток самого тесного окна");

        // Всё спокойно: основной и работает.
        let calm = json!({"config": {"enabled": true, "hasKey": true, "rotate": true, "account": "OpenCode #2"},
            "keys": {"lastUsed": "OpenCode #2", "selected": null, "paused": []}});
        let rows = key_rows(&accounts, &cfg(), Some(&calm), 1_000, false);
        assert_eq!((rows[0].status.as_str(), rows[0].tone), ("основной · в работе", Tone::Ok));
        assert_eq!(rows[0].shown, Some(30.0), "в режиме «потрачено» — расход");
        let no_rotate = ProxyConfig { rotate: false, ..cfg() };
        let rows = key_rows(&accounts, &no_rotate, None, 1_000, true);
        assert_eq!(rows.iter().find(|r| r.label == "OpenCode #10").map(|r| r.status.as_str()), Some("не используется"));

        // Ключ из терминала, которого нет в карточках, — строкой сверху, не теряется.
        let own = ProxyConfig { api_key: "sk-own-1234".into(), account_label: String::new(), ..cfg() };
        let rows = key_rows(&accounts, &own, None, 1_000, true);
        assert_eq!((rows[0].label.as_str(), rows[0].selected), ("Свой ключ", true));
        assert!(rows[0].status.ends_with("…1234"));
    }

    #[test]
    fn подписи_по_порядку_как_у_людей() {
        let mut labels = vec!["OpenCode #10", "OpenCode #2", "opencode #1", "OpenCode"];
        labels.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(labels, ["OpenCode", "opencode #1", "OpenCode #2", "OpenCode #10"]);
    }

    #[test]
    fn строка_состояния() {
        let st = json!({"version": env!("CARGO_PKG_VERSION"), "inflight": 2,
            "stats": {"sub": 5, "fallback": 1, "errors": 0, "pass": 40, "recent": [{"at": 1_000, "note": "OpenCode 500"}, {"route": "sub"}]}});
        let (tone, line, stats) = status_lines(Some(&st), &cfg(), 1_000 + 5 * 60_000);
        assert_eq!(tone, Tone::Ok);
        assert_eq!(line, "Работает · последний субагент уже выпал из журнала", "в recent нет времени у события субагента, а счётчик их помнит");
        let worked = json!({"version": env!("CARGO_PKG_VERSION"), "stats": {"recent": [{"route": "sub", "at": 1_000}, {"route": "pass", "at": 2_000}]}});
        assert_eq!(status_lines(Some(&worked), &cfg(), 1_000 + 3 * 60_000).1, "Работает · последний субагент 3 мин назад");
        // Последний субагент ушёл в Claude — не «работает», а тревога с причиной.
        let fell = json!({"version": env!("CARGO_PKG_VERSION"), "stats": {"recent": [{"route": "sub", "at": 1_000},
            {"route": "fallback", "at": 2_000, "note": "OpenCode не ответил за 60 с"}, {"route": "error", "at": 3_000, "model": "claude-opus-5-5"}]}});
        assert_eq!(status_lines(Some(&fell), &cfg(), 2_000 + 2 * 60_000), (Tone::Warn, "Последний субагент ушёл в Claude 2 мин назад: OpenCode не ответил за 60 с".to_string(), status_lines(Some(&fell), &cfg(), 2_000 + 2 * 60_000).2), "ошибка основной модели — не про субагентов");
        let failed = json!({"version": env!("CARGO_PKG_VERSION"), "stats": {"recent": [{"route": "error", "at": 1_000, "model": "deepseek-v4.1-flash", "note": "все ключи на паузе"}]}});
        assert_eq!(status_lines(Some(&failed), &cfg(), 1_000).1, "Последний субагент получил ошибку только что: все ключи на паузе");
        assert_eq!(status_meta(Some(&st), 0), "5 ответов субагентов · 1 откат");
        let clean = json!({"stats": {"sub": 7, "fallback": 0, "errors": 0}});
        assert_eq!(status_meta(Some(&clean), 0), "7 ответов субагентов", "нули не показываем");
        let many = json!({"stats": {"sub": 21, "fallback": 12, "errors": 3}, "startedAtMs": 1_000});
        assert!(status_meta(Some(&many), 1_000).starts_with("с "), "с какого момента считает прокси");
        assert!(status_meta(Some(&many), 1_000).ends_with("21 ответ субагента · 12 откатов · 3 ошибки"));
        assert_eq!(status_meta(None, 0), "");
        assert_eq!(stats, "субагентов 5 · откатов в Claude 1 · ошибок 0 · напрямую в Claude 40 · в работе 2 · сбой 5 мин назад");
        assert_eq!(last_failure(Some(&st), 1_000 + 5 * 60_000).as_deref(), Some("OpenCode 500"));
        let old = json!({"version": "0.2.0", "stats": {}});
        let (tone, line, _) = status_lines(Some(&old), &cfg(), 0);
        assert_eq!(tone, Tone::Warn);
        assert!(line.starts_with("Прокси старой версии 0.2.0"));
        assert_eq!(status_lines(Some(&st), &ProxyConfig { enabled: false, ..cfg() }, 0).0, Tone::Warn);
        assert_eq!(status_lines(None, &cfg(), 0).0, Tone::Off);
    }

    #[test]
    fn строка_в_терминале_говорит_куда_идут_субагенты() {
        let plain = |s: String| s.replace("\x1b[32m", "").replace("\x1b[33m", "").replace("\x1b[31m", "").replace("\x1b[2m", "").replace("\x1b[0m", "");
        assert!(plain(statusline_text(None, 0)).contains("прокси SubBar не запущен"));
        assert!(plain(statusline_text(Some(&json!({"found": false})), 0)).contains("не через claude-sub"));
        assert!(plain(statusline_text(Some(&json!({"found": true, "enabled": false})), 0)).contains("подмена выключена"));
        let ready = json!({"found": true, "enabled": true, "hasKey": true, "model": "deepseek-v4.1-flash", "session": {"sub": 0, "pass": 3}});
        assert_eq!(plain(statusline_text(Some(&ready), 0)), "deepseek-v4.1-flash · пока не запускались");
        let working = json!({"found": true, "enabled": true, "hasKey": true, "model": "deepseek-v4.1-flash",
            "session": {"sub": 12, "fallback": 0, "errors": 0, "lastSubAt": 1_000, "lastModel": "deepseek-v4.1-flash"}});
        let line = statusline_text(Some(&working), 1_000 + 3 * 60_000);
        assert!(line.starts_with("\x1b[32m"), "работает — зелёным");
        assert_eq!(plain(line), "deepseek-v4.1-flash · 12 отв");
        let agents = json!({"found": true, "enabled": true, "hasKey": true, "model": "m", "selectedKey": "OpenCode #4",
            "session": {"agents": 3, "sub": 12, "lastSubAt": 1_000, "lastModel": "deepseek-v4.1-flash", "lastKey": "OpenCode #3"}});
        assert_eq!(plain(statusline_text(Some(&agents), 1_000)), "deepseek-v4.1-flash · 3 аг · 12 отв · ключ OpenCode #3");
        let fell = json!({"found": true, "enabled": true, "hasKey": true, "model": "deepseek-v4.1-flash",
            "session": {"sub": 5, "fallback": 2, "errors": 1, "lastSubAt": 1_000, "lastModel": "deepseek-v4.1-flash"}});
        let line = plain(statusline_text(Some(&fell), 1_000));
        assert!(line.contains("5 отв") && line.contains("2 в Claude") && line.contains("1 ош"), "{line}");
    }

    #[test]
    fn строка_знает_адрес_сессии_и_ширину() {
        let plain = |s: String| s.replace("\x1b[31m", "").replace("\x1b[2m", "").replace("\x1b[0m", "").replace("\x1b[32m", "").replace("\x1b[33m", "");
        let fresh = json!({"found": false, "enabled": true, "hasKey": true, "model": "deepseek-v4.1-flash"});
        assert_eq!(plain(statusline_text_via(Some(&fresh), Some(true), 0)), "deepseek-v4.1-flash · пока не запускались", "начало сессии через прокси");
        assert!(plain(statusline_text_via(Some(&fresh), Some(false), 0)).contains("не через claude-sub"));
        assert!(plain(statusline_text_via(None, Some(true), 0)).contains("не отвечает"));
        let fell = json!({"found": true, "enabled": true, "hasKey": true, "model": "m",
            "session": {"sub": 5, "fallback": 2, "lastSubAt": 1_000, "lastModel": "deepseek-v4.1-flash"}});
        let full = statusline_text(Some(&fell), 1_000);
        let narrow = plain(statusline_fit(&full, Some(60)));
        assert!(narrow.contains("2 в Claude"), "{narrow}");
        assert_eq!(statusline_fit(&full, None), full);
    }

    #[test]
    fn скорость_как_в_pi() {
        assert_eq!(speed_text(&json!({"session": {"speedTokens": 840, "speedMs": 20_000}})), Some("42".into()));
        assert_eq!(speed_text(&json!({"session": {"speedTokens": 17, "speedMs": 2_000}})), Some("8.5".into()));
        assert_eq!(speed_text(&json!({"session": {"speedTokens": 0, "speedMs": 0}})), None);
        assert_eq!(speed_text(&json!({"found": false})), None);
    }

    #[test]
    fn итог_проверки() {
        let ok = json!({"ok": true, "model": "deepseek-v4.1-flash", "key": "OpenCode #3", "ms": 1432, "reply": "ок"});
        assert_eq!(check_line(&ok), ("✓ Отвечает за 1,4 с · OpenCode #3 · «ок»".to_string(), true));
        let bad = json!({"ok": false, "error": "ключ OpenCode #2 на паузе: кончился недельный лимит"});
        assert_eq!(check_line(&bad).1, false);
        assert!(check_line(&bad).0.starts_with("✗ ключ OpenCode #2"));
    }

    #[test]
    fn ответ_модели_не_ломает_строку_итога() {
        // Перевод строки и управляющие символы в ответе — в строке статуса разорвали бы её.
        let messy = json!({"ok": true, "key": "OpenCode #3", "ms": 100, "reply": "ок\nвторая\r\nтретья\tстрока"});
        let (line, ok) = check_line(&messy);
        assert!(ok && !line.contains('\n') && !line.contains('\r') && !line.contains('\t'), "{line:?}");
        assert_eq!(line, "✓ Отвечает за 0,1 с · OpenCode #3 · «ок вторая третья строка»");
        let esc = json!({"ok": true, "key": "K", "ms": 10, "reply": "\u{1b}[31mкрасным\u{1b}[0m"});
        assert!(!check_line(&esc).0.contains('\u{1b}'), "управляющие символы в итог не попадают");
        // Длинный ответ обрезается многоточием, а не молча.
        let long = json!({"ok": true, "key": "K", "ms": 10, "reply": "я".repeat(80)});
        let (line, _) = check_line(&long);
        assert!(line.ends_with("…»"), "{line}");
        assert!(line.contains(&"я".repeat(30)) && !line.contains(&"я".repeat(31)), "обрезано на 30 символов с многоточием: {line}");
    }

    /// Каталог теста: свой, ~/.claude тесты не трогают.
    fn temp_dir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("subbar-test-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn command_of(path: &std::path::Path) -> Option<String> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        v["statusLine"]["command"].as_str().map(str::to_string)
    }

    #[test]
    fn своя_строка_опознаётся_по_любому_пути() {
        // Бинарь cargo — `subbar` с маленькой буквы, не `SubBar`.
        assert!(wrapper_len("/x/target/release/subbar statusline").is_some());
        assert!(wrapper_len("'/Applications/Sub Bar/SubBar' statusline --then myline").is_some());
        assert!(wrapper_len("/Applications/SubBar.app/Contents/MacOS/SubBar statusline").is_some());
        assert!(wrapper_len("/usr/local/bin/subbar statusline").is_some());
        assert!(wrapper_len("/usr/local/bin/SubBar statuss").is_none(), "чужой бинарь или другой подкоманд");
        assert!(wrapper_len("/usr/local/bin/subbarx statusline").is_none(), "не наше имя");
        assert!(wrapper_len("~/.claude/statusline.sh").is_none(), "своя строка человека — не наша");

        let ours = "/new/SubBar statusline";
        let write = |e: StatuslineEdit| match e {
            StatuslineEdit::Write(s) => s,
            StatuslineEdit::Same => "<не трогаем: уже так>".to_string(),
            StatuslineEdit::Absent => "<не трогаем: нет нашей>".to_string(),
        };
        // Приложение переехало: путь переписывается, команда человека сохраняется.
        assert_eq!(
            write(statusline_command(Some("/old/Applications/SubBar.app/Contents/MacOS/SubBar statusline --then myline.sh"), ours, true)),
            "/new/SubBar statusline --then myline.sh"
        );
        // Тот же путь — файл не трогаем.
        assert_eq!(write(statusline_command(Some("/new/SubBar statusline --then myline.sh"), ours, true)), "<не трогаем: уже так>");
        // Обёртка в обёртке (бинарь переименовали при смене пути) — разворачивается, а не множится.
        assert_eq!(
            write(statusline_command(Some("/old/subbar statusline --then /old/subbar statusline --then myline.sh"), ours, true)),
            "/new/SubBar statusline --then myline.sh"
        );
        // Чужая строка — оборачивается один раз, как раньше.
        assert_eq!(write(statusline_command(Some("myline.sh"), ours, true)), "/new/SubBar statusline --then myline.sh");
        // Составная команда — одним словом, и обратно снимается как была.
        let wrapped = write(statusline_command(Some("a --l \"x y\" | b 'q'"), ours, true));
        assert_eq!(wrapped, "/new/SubBar statusline --then 'a --l \"x y\" | b '\\''q'\\'''");
        assert_eq!(write(statusline_command(Some(&wrapped), ours, false)), "a --l \"x y\" | b 'q'");
        assert_eq!(write(statusline_command(None, ours, true)), "/new/SubBar statusline");
        // Снятие: достаём команду человека, обёртку не оставляем.
        assert_eq!(write(statusline_command(Some("/old/subbar statusline --then myline.sh"), ours, false)), "myline.sh");
        assert_eq!(write(statusline_command(Some("/old/subbar statusline --then /old/subbar statusline --then myline.sh"), ours, false)), "myline.sh");
        assert_eq!(write(statusline_command(Some("/old/subbar statusline"), ours, false)), "", "чистая наша строка — statusLine убираем");
        assert_eq!(write(statusline_command(Some("myline.sh"), ours, false)), "<не трогаем: нет нашей>");
        assert_eq!(write(statusline_command(None, ours, false)), "<не трогаем: нет нашей>");
    }

    #[test]
    fn правка_настроек_на_временном_файле() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("statusline");
        let path = dir.join("settings.json");
        let ours = "/Applications/SubBar.app/Contents/MacOS/SubBar statusline";

        // Пустых настроек нет — ставим с нуля, ключи чужие не теряем.
        assert!(!statusline_installed_at(&path));
        std::fs::write(&path, "{\"model\": \"opus\"}").unwrap();
        statusline_setup_at(&path, ours, true).unwrap();
        assert_eq!(command_of(&path).as_deref(), Some(ours));
        assert!(statusline_installed_at(&path), "своя строка опознаётся");

        // Чужая строка: наша встаёт первой, человеческая — через --then.
        std::fs::write(&path, r#"{"statusLine": {"type": "command", "command": "myline.sh", "padding": 3}}"#).unwrap();
        statusline_setup_at(&path, ours, true).unwrap();
        assert_eq!(command_of(&path).as_deref(), Some("/Applications/SubBar.app/Contents/MacOS/SubBar statusline --then myline.sh"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["statusLine"]["padding"], 3, "настройка человека сохранена");

        // Снятие возвращает его строку как была.
        statusline_setup_at(&path, ours, false).unwrap();
        assert_eq!(command_of(&path).as_deref(), Some("myline.sh"));
        assert!(!statusline_installed_at(&path));

        // Приложение переехало: тот же файл переписывается на новый путь.
        let moved = "/Applications/SubBar2.app/Contents/MacOS/SubBar statusline";
        statusline_setup_at(&path, moved, true).unwrap();
        assert_eq!(command_of(&path).as_deref(), Some("/Applications/SubBar2.app/Contents/MacOS/SubBar statusline --then myline.sh"));
        assert!(path.with_extension("json.subbar-backup").exists(), "копия настроек перед правкой");
        assert!(dir.join("settings.json.subbar.lock").exists(), "межпроцессная блокировка рядом с файлом");

        // Права 0600 не должны разжаться при перезаписи через tmp.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Третий путь — иначе правка «та же» и файл вовсе не переписывается.
        statusline_setup_at(&path, "/Applications/SubBar3.app/Contents/MacOS/SubBar statusline", true).unwrap();
        assert!(command_of(&path).is_some_and(|c| c.starts_with("/Applications/SubBar3.app/")), "файл реально переписан");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "права исходного файла сохранены");

        // Корень не объект — раньше был бы panic (в release — abort), а не понятная ошибка.
        for broken in ["[]", "\"settings\"", "null", "42"] {
            std::fs::write(&path, broken).unwrap();
            let err = statusline_setup_at(&path, ours, true).unwrap_err();
            assert!(err.contains("не трогаю"), "{broken}: {err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "файл не тронут");
            let err = statusline_setup_at(&path, ours, false).unwrap_err();
            assert!(err.contains("не трогаю"), "{broken}: {err}");
        }
        // Битый JSON — тоже не трогаем, и это не паника.
        std::fs::write(&path, "{не json").unwrap();
        assert!(statusline_setup_at(&path, ours, true).unwrap_err().contains("не трогаю"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn обрезка_строки_по_ширине() {
        const DIM: &str = "\x1b[2m";
        const RESET: &str = "\x1b[0m";
        let line = format!("{DIM}model · 3 аг · 12 отв{DIM} · ключ K{RESET}{DIM} · 2 в Claude{RESET}{DIM} · 5 ош{RESET}");
        let cols = |s: &str| statusline_cols(&s.replace(DIM, "").replace(RESET, ""));
        assert!(cols(&line) > 40);
        // Обрезается по нескольким серым кускам, а не по одному (в списке маркеров был дубль).
        let narrow = statusline_fit(&line, Some(24));
        assert!(statusline_cols(&narrow) <= 24, "{:?} = {}", narrow, statusline_cols(&narrow));
        assert!(!narrow.contains("5 ош") && !narrow.contains("2 в Claude") && !narrow.contains("ключ K"), "{narrow:?}");
        assert!(narrow.contains("model · 3 аг · 12 отв"), "главное остаётся: {narrow:?}");
        let two = format!("\x1b[32mmodel · 5 отв{RESET}{DIM} · 2 в Claude{RESET}");
        assert_eq!(statusline_fit(&two, Some(22)), "\x1b[32mmodel · 5 отв\x1b[0m", "серый кусок уходит по ширине 22");
        assert_eq!(statusline_fit(&two, Some(30)), two, "в 30 влезает всё");
        // Уже влезло — не трогаем; узкая ширина вроде 10 игнорируется (иначе пустая строка).
        assert_eq!(statusline_fit(&line, Some(1000)), line);
        assert_eq!(statusline_fit(&line, None), line);
        // Широкие символы — две колонки: CJK-имя модели обрезается раньше, чем латиница той же длины.
        let cjk = format!("\x1b[32mмодель 中文字体测试版{RESET}\x1b[2m · ключ K{RESET}");
        assert_eq!(statusline_cols(&cjk), "модель 中文字体测试版".chars().map(char_cols).sum::<usize>() + " · ключ K".chars().count());
        assert!(statusline_cols(&cjk) > "модель 中文字体测试版".chars().count());
        assert!(!statusline_fit(&cjk, Some(25)).contains("ключ"), "широкие символы считаются за две");
        // Нет бесконечного кружения, когда резать больше нечего.
        let flat = "\x1b[32mочень длинная строка без единого серого куска и скобки\x1b[0m";
        assert_eq!(statusline_fit(flat, Some(21)), flat);
    }

    #[test]
    fn причина_паузы_не_теряется_без_лимитов() {
        let mut a = opencode("OpenCode #2", "K2", 0.0, true);
        a.last_usage = Some(crate::model::AccountUsage {
            status: crate::model::FetchStatus::Ok,
            windows: vec![], // лимиты ещё не приехали
            plan_type: None,
            notes: vec![],
            error: None,
            updated_at: 0,
            last_ok_at: Some(0),
        });
        let st = json!({"keys": {"paused": [{"label": "OpenCode #2", "untilMs": 1_000 + 3_600_000, "reason": "кончился недельный лимит"}]}});
        let rows = key_rows(&[a.clone()], &cfg(), Some(&st), 1_000, true);
        assert_eq!(rows[0].tip, "Пауза: кончился недельный лимит\nЛимиты ещё не загружены", "причина паузы главнее пустого списка окон");
        // Без паузы остаётся только напоминание.
        assert_eq!(key_rows(&[a], &cfg(), None, 1_000, true)[0].tip, "Лимиты ещё не загружены");
    }

    #[test]
    fn устаревший_поток_службы_ничего_не_делает() {
        // Быстрые «вкл» → «выкл»: второе решение должно победить, первое — забыть.
        // Намерение общее на процесс — сбросить и при упавшем assert.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                *SERVICE_PENDING.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
        let _reset = Reset;
        let first = set_intent(true);
        let second = set_intent(false);
        assert!(!is_current(first), "поток от первого клика устарел");
        assert!(is_current(second) && !service_intent(), "тумблер показывает второе намерение");
    }
}
