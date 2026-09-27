//! Прокси целиком: настоящий бинарь `subbar proxy` между фейковыми Anthropic и OpenCode.
//! Главное — куда что ушло и что OAuth подписки никогда не попадает в OpenCode.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Ответ фейкового сервера: статус, задержка до ответа, заголовки, куски тела с паузами (мс).
struct Reply {
    status: u16,
    delay_ms: u64,
    content_type: &'static str,
    chunks: Vec<(u64, String)>,
    retry_after: Option<&'static str>,
}

impl Reply {
    fn json(status: u16, body: &str) -> Reply {
        Reply { status, delay_ms: 0, content_type: "application/json", chunks: vec![(0, body.to_string())], retry_after: None }
    }
    fn sse(chunks: Vec<(u64, &str)>) -> Reply {
        let chunks = chunks.into_iter().map(|(p, c)| (p, c.to_string())).collect();
        Reply { status: 200, delay_ms: 0, content_type: "text/event-stream", chunks, retry_after: None }
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn serve_one(stream: TcpStream, log: &Log, reply: &(dyn Fn(&Seen) -> Reply + Send + Sync)) {
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if r.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut len = 0;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).is_err() {
            return;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        // Кривой заголовок — тихо бросаем соединение, а не роняем поток фейка с невнятной паникой.
        let Some((k, v)) = h.split_once(':') else { return };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
        if k == "content-length" {
            let Ok(n) = v.parse() else { return };
            len = n;
        }
        headers.push((k, v));
    }
    let mut body = vec![0; len];
    if r.read_exact(&mut body).is_err() {
        return;
    }
    let seen = Seen { path, headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null) };
    log.lock().unwrap().push(seen.clone());
    let out = reply(&seen);
    std::thread::sleep(Duration::from_millis(out.delay_ms));
    let mut s = stream;
    let retry = out.retry_after.map(|r| format!("retry-after: {r}\r\n")).unwrap_or_default();
    // Без content-length: тело до закрытия соединения — так можно отдавать поток кусками.
    let _ = write!(s, "HTTP/1.1 {} X\r\ncontent-type: {}\r\n{retry}connection: close\r\n\r\n", out.status, out.content_type);
    for (pause, chunk) in out.chunks {
        std::thread::sleep(Duration::from_millis(pause));
        if s.write_all(chunk.as_bytes()).and_then(|()| s.flush()).is_err() {
            return;
        }
    }
}

/// Фейковый сервер: пишет запросы, отвечает по правилу; каждое соединение — в своём потоке.
fn fake_with(reply: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Log = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let reply = Arc::new(reply);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (log, reply) = (log.clone(), reply.clone());
            std::thread::spawn(move || serve_one(stream, &log, &*reply));
        }
    });
    (port, seen)
}

fn fake(status: u16, reply: &'static str) -> (u16, Log) {
    fake_with(move |_| Reply::json(status, reply))
}

struct Proxy {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Карточки OpenCode Go для запасных ключей: (подпись, ключ, % потрачено).
fn state(keys: &[(&str, &str, f64)]) -> Value {
    // Свежий опрос: старше суток прокси считает окно без срока «неизвестным».
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let accounts: Vec<Value> = keys
        .iter()
        .map(|(label, key, used)| {
            json!({"id": label, "provider": "open-code-go", "label": label, "credentials": {"apiKey": key},
                   "lastUsage": {"status": "ok", "updatedAt": now, "windows": [
                       {"key": "7d", "label": "7д", "usedPercent": used, "windowMinutes": 10080}]}})
        })
        .collect();
    json!({"version": 1, "accounts": accounts, "settings": {"refreshSeconds": 300, "showRemaining": true,
           "notifyUsedPercent": 90.0, "launchAtLogin": false, "showTrayPercent": true}})
}

fn proxy_with(cfg: Value, env: &[(&str, &str)], accounts: Option<Value>) -> Proxy {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!("subbar-e2e-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Каталог данных окна (карточки) — отдельный: там не должно быть чужих файлов.
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    if let Some(st) = accounts {
        let p = data.join("state.json");
        std::fs::write(&p, st.to_string()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // Порт 0: свободный порт берёт сам прокси и называет его в первой строке журнала —
    // заранее выбранный порт при параллельных тестах может занять сосед.
    spawn_in(&dir, cfg, env)
}

/// Прокси в готовом каталоге (второй запуск — после мягкого выхода первого).
fn spawn_in(dir: &std::path::Path, cfg: Value, env: &[(&str, &str)]) -> Proxy {
    let dir = dir.to_path_buf();
    let data = dir.join("data");
    let mut cfg = cfg;
    cfg["port"] = json!(0);
    // Как в жизни: proxy.json лежит в каталоге данных рядом с state.json.
    let path = data.join("proxy.json");
    std::fs::write(&path, cfg.to_string()).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_subbar"));
    cmd.args(["proxy", "--config", path.to_str().unwrap()]).env("SUBBAR_DATA_DIR", &data).stderr(Stdio::piped());
    // Повторы при временном сбое — без настоящих полсекунды ожидания.
    cmd.env("SUBBAR_RETRY_MS", "20").env("SUBBAR_ALLOW_PORT0", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    // Предупреждения могут идти до строки с портом; не нашли порт — гасим прокси и чистим каталог, а не оставляем сироту.
    // В сообщение об ошибке — первая строка: обычно в ней и причина.
    let mut first: Option<String> = None;
    let mut last = String::new();
    let mut port = None;
    for line in lines.by_ref().take(20).map_while(Result::ok) {
        port = line.split("127.0.0.1:").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|p| p.parse::<u16>().ok());
        first.get_or_insert_with(|| line.clone());
        last = line;
        if port.is_some() {
            break;
        }
    }
    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        panic!("нет порта в «{}» (последняя строка: «{last}»)", first.unwrap_or_default());
    };
    // SUBBAR_E2E_LOG=1 — видеть журнал прокси при разборе падений; иначе просто вычитываем.
    let show = std::env::var_os("SUBBAR_E2E_LOG").is_some();
    if show {
        eprintln!("{last}");
    }
    std::thread::spawn(move || {
        for line in lines.map_while(Result::ok) {
            if show {
                eprintln!("{line}");
            }
        }
    });
    Proxy { child, port, dir }
}

fn proxy(cfg: Value) -> Proxy {
    proxy_with(cfg, &[], None)
}

fn post(port: u16, body: Value) -> (u16, String) {
    let r = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/messages?beta=true"))
        .header("authorization", "Bearer OAUTH-SECRET")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&body)
        .send()
        .unwrap();
    (r.status().as_u16(), r.text().unwrap())
}

fn proxy_status(port: u16) -> Value {
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
    client.get(format!("http://127.0.0.1:{port}/_subbar/status")).send().unwrap().json().unwrap()
}

/// Ждать, пока прокси досчитает поток: учёт идёт в Drop тела, то есть сразу после последнего байта клиенту.
fn wait_stats(port: u16, want: impl Fn(&Value) -> bool) -> Value {
    let t0 = Instant::now();
    loop {
        let st = proxy_status(port);
        if want(&st) || t0.elapsed() > Duration::from_secs(3) {
            return st;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn base(anthropic: u16, opencode: u16) -> Value {
    json!({
        "apiKey": "OC-KEY", "model": "deepseek-v4.1-flash", "effort": "high",
        "anthropicBase": format!("http://127.0.0.1:{anthropic}"),
        "opencodeBase": format!("http://127.0.0.1:{opencode}/zen/go/v1"),
    })
}

fn subagent() -> Value {
    json!({"model": "claude-haiku-4-5-20251001", "tools": [{"name": "Bash"}], "messages": [],
           "metadata": {"user_id": "{\"session_id\":\"S-42\"}"}})
}

const WEEKLY: &str = r#"{"type":"error","error":{"type":"GoUsageLimitError","message":"Go usage limit exceeded"},"metadata":{"limitName":"weekly"}}"#;

/// OpenCode, у которого ключ OC-A исчерпан (неделя), а остальные работают.
fn opencode_with_exhausted_a() -> (u16, Log) {
    fake_with(|s| {
        if s.header("x-api-key") == Some("OC-A") {
            Reply { retry_after: Some("3600"), ..Reply::json(429, WEEKLY) }
        } else {
            Reply::json(200, r#"{"from":"opencode"}"#)
        }
    })
}

fn keys_of(log: &Log) -> Vec<String> {
    log.lock().unwrap().iter().map(|s| s.header("x-api-key").unwrap_or("").to_string()).collect()
}

#[test]
fn основная_модель_насквозь_с_oauth() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, seen_o) = fake(200, r#"{"from":"opencode"}"#);
    let p = proxy(base(a, o));
    let (status, text) = post(p.port, json!({"model": "claude-opus-5-5", "tools": [{"name": "Bash"}]}));
    assert_eq!(status, 200);
    assert!(text.contains("anthropic"));
    let req = seen_a.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/v1/messages?beta=true", "путь и query — как есть");
    assert_eq!(req.header("authorization"), Some("Bearer OAUTH-SECRET"), "подписка доходит до Anthropic");
    assert_eq!(req.body["model"], "claude-opus-5-5", "тело не тронуто");
    assert!(seen_o.lock().unwrap().is_empty());
}

#[test]
fn субагент_уходит_в_opencode_без_oauth() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, seen_o) = fake(200, r#"{"from":"opencode"}"#);
    let p = proxy(base(a, o));
    let (status, text) = post(p.port, subagent());
    assert_eq!(status, 200);
    assert!(text.contains("opencode"));
    let req = seen_o.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/zen/go/v1/messages");
    assert_eq!(req.header("authorization"), None, "OAuth подписки НЕ уходит в OpenCode");
    assert_eq!(req.header("x-api-key"), Some("OC-KEY"));
    assert_eq!(req.header("x-opencode-session"), Some("S-42"));
    assert_eq!(req.header("anthropic-beta"), Some("oauth-2025-04-20"));
    assert_eq!(req.header("accept-encoding"), Some("identity"), "поток читается и режется по событиям — без сжатия");
    assert_eq!(req.body["model"], "deepseek-v4.1-flash");
    assert_eq!(req.body["output_config"]["effort"], "high");
    assert!(seen_a.lock().unwrap().is_empty());
}

#[test]
fn сбой_opencode_откат_на_настоящую_haiku() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, _) = fake(500, r#"{"error":"boom"}"#);
    let p = proxy(base(a, o));
    let (status, text) = post(p.port, subagent());
    assert_eq!(status, 200);
    assert!(text.contains("anthropic"), "откат");
    let req = seen_a.lock().unwrap()[0].clone();
    assert_eq!(req.body["model"], "claude-haiku-4-5-20251001", "на откате — исходный запрос");
    assert_eq!(req.header("authorization"), Some("Bearer OAUTH-SECRET"));
    let st = proxy_status(p.port);
    assert_eq!(st["stats"]["fallback"], 1);
    let note = st["stats"]["recent"][0]["note"].as_str().unwrap();
    assert!(note.contains("OpenCode 500") && note.contains("повторов: 2"), "{note}");
}

#[test]
fn временный_сбой_opencode_повтор_а_не_откат() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let n = Arc::new(AtomicUsize::new(0));
    let (o, seen_o) = fake_with(move |_| match n.fetch_add(1, Ordering::SeqCst) {
        0 => Reply::json(503, r#"{"error":"upstream overloaded"}"#),
        1 => Reply::json(502, "bad gateway"),
        _ => Reply::json(200, r#"{"from":"opencode"}"#),
    });
    let p = proxy(base(a, o));
    let (status, text) = post(p.port, subagent());
    assert_eq!(status, 200);
    assert!(text.contains("opencode"), "два сбоя подряд — повтор, подписку не тронули: {text}");
    assert_eq!(seen_o.lock().unwrap().len(), 3);
    assert!(seen_a.lock().unwrap().is_empty());
    assert_eq!(proxy_status(p.port)["stats"]["fallback"], 0);
}

#[test]
fn ошибка_самого_запроса_не_повторяется() {
    let (a, _) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, seen_o) = fake(400, r#"{"model":"deepseek-v4.1-flash"}"#);
    let p = proxy(base(a, o));
    let (status, text) = post(p.port, subagent());
    assert_eq!(status, 200);
    assert!(text.contains("anthropic"));
    assert_eq!(seen_o.lock().unwrap().len(), 1, "400 со второго раза не пройдёт — сразу откат");
}

#[test]
fn opencode_недоступен_повтор_и_откат() {
    // Порт, на котором никто не слушает: соединение отвергается.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let p = proxy(base(a, dead));
    let (status, _) = post(p.port, subagent());
    assert_eq!(status, 200);
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    let note = proxy_status(p.port)["stats"]["recent"][0]["note"].as_str().unwrap().to_string();
    assert!(note.contains("недоступен") && note.contains("повторов: 2"), "{note}");
}

#[test]
fn после_смены_модели_чужие_мысли_не_уходят_в_anthropic() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, seen_o) = fake(200, "{}");
    let p = proxy(base(a, o));
    // Сессия шла на haiku через OpenCode, теперь основная модель — opus: история та же.
    let body = json!({"model": "claude-opus-5-5", "tools": [{"name": "Bash"}], "messages": [
        {"role": "user", "content": "x"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "hm", "signature": "229b1eb2-536e-4568-9839-4f44875e0eaa"},
            {"type": "text", "text": "ok"}]},
        {"role": "user", "content": "y"}
    ]});
    let (code, _) = post(p.port, body);
    assert_eq!(code, 200);
    let req = seen_a.lock().unwrap()[0].clone();
    assert_eq!(req.body["messages"][1]["content"], json!([{"type": "text", "text": "ok"}]));
    assert_eq!(req.header("authorization"), Some("Bearer OAUTH-SECRET"));
    assert!(seen_o.lock().unwrap().is_empty());
    // Своя подпись Claude (конверт CAQS) — тело уходит нетронутым.
    let caqs = "CAQSQAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=";
    let own = json!({"model": "claude-opus-5-5", "messages": [
        {"role": "user", "content": "x"},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm", "signature": caqs}, {"type": "text", "text": "ok"}]},
        {"role": "user", "content": "y"}
    ]});
    post(p.port, own.clone());
    assert_eq!(seen_a.lock().unwrap()[1].body, own);
}

#[test]
fn без_отката_ошибка_в_формате_anthropic() {
    let (a, seen_a) = fake(200, "{}");
    let (o, _) = fake(500, r#"{"error":"boom"}"#);
    let mut cfg = base(a, o);
    cfg["fallback"] = json!(false);
    let p = proxy(cfg);
    let (status, text) = post(p.port, subagent());
    assert_eq!(status, 502);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["type"], "error");
    assert!(seen_a.lock().unwrap().is_empty());
}

#[test]
fn конфиг_перечитывается_на_лету() {
    let (a, seen_a) = fake(200, "{}");
    let (o, seen_o) = fake(200, "{}");
    let p = proxy(base(a, o));
    post(p.port, subagent());
    assert_eq!(seen_o.lock().unwrap().len(), 1);
    let path = p.dir.join("data").join("proxy.json");
    let mut cfg: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    cfg["enabled"] = json!(false);
    std::thread::sleep(Duration::from_millis(20)); // другой mtime
    std::fs::write(&path, cfg.to_string()).unwrap();
    post(p.port, subagent());
    assert_eq!(seen_a.lock().unwrap().len(), 1, "выключено — насквозь");
    assert_eq!(seen_o.lock().unwrap().len(), 1);
}

#[test]
fn лимит_ключа_кончился_запрос_уходит_на_ключ_с_запасом() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, seen_o) = opencode_with_exhausted_a();
    let mut cfg = base(a, o);
    cfg["apiKey"] = json!("OC-A");
    cfg["accountLabel"] = json!("#A");
    let accounts = state(&[("#A", "OC-A", 100.0), ("#C", "OC-C", 50.0), ("#B", "OC-B", 10.0)]);
    let p = proxy_with(cfg, &[], Some(accounts));
    let (status_code, text) = post(p.port, subagent());
    assert_eq!(status_code, 200);
    assert!(text.contains("opencode"), "тот же запрос — другим ключом, не на подписку");
    assert_eq!(keys_of(&seen_o), ["OC-A", "OC-B"], "следующий — с наибольшим запасом");
    let st = proxy_status(p.port);
    assert_eq!(st["keys"]["lastUsed"], "#B");
    assert_eq!(st["keys"]["paused"][0]["label"], "#A");
    assert_eq!(st["keys"]["paused"][0]["reason"], "кончился недельный лимит");
    assert_eq!(st["stats"]["sub"], 1);
    assert_eq!(st["stats"]["fallback"], 0);
    assert_eq!(st["stats"]["recent"][0]["key"], "#B");
    post(p.port, subagent());
    assert_eq!(keys_of(&seen_o), ["OC-A", "OC-B", "OC-B"], "исчерпанный ключ больше не дёргаем");
    assert!(seen_a.lock().unwrap().is_empty());
}

#[test]
fn без_ротации_исчерпанный_ключ_уходит_в_откат_и_не_дёргается() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, seen_o) = opencode_with_exhausted_a();
    let mut cfg = base(a, o);
    cfg["apiKey"] = json!("OC-A");
    cfg["accountLabel"] = json!("#A");
    cfg["rotate"] = json!(false);
    let p = proxy_with(cfg, &[], Some(state(&[("#A", "OC-A", 100.0), ("#B", "OC-B", 10.0)])));
    assert!(post(p.port, subagent()).1.contains("anthropic"));
    assert!(post(p.port, subagent()).1.contains("anthropic"));
    assert_eq!(keys_of(&seen_o), ["OC-A"], "на паузе — сразу в откат, без лишнего круга");
    assert_eq!(seen_a.lock().unwrap().len(), 2);
    let st = proxy_status(p.port);
    assert!(st["stats"]["recent"][1]["note"].as_str().unwrap().contains("#A на паузе: кончился недельный лимит"));
}

#[test]
fn все_ключи_исчерпаны_без_отката_429() {
    let (a, seen_a) = fake(200, "{}");
    let (o, seen_o) = fake_with(|_| Reply { retry_after: Some("3600"), ..Reply::json(429, WEEKLY) });
    let mut cfg = base(a, o);
    cfg["apiKey"] = json!("OC-A");
    cfg["fallback"] = json!(false);
    let p = proxy_with(cfg, &[], Some(state(&[("#A", "OC-A", 0.0), ("#B", "OC-B", 0.0)])));
    let r = reqwest::blocking::Client::new()
        .post(format!("http://127.0.0.1:{}/v1/messages", p.port))
        .header("authorization", "Bearer OAUTH-SECRET")
        .json(&subagent())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 429);
    let retry: i64 = r.headers().get("retry-after").expect("нет retry-after в 429").to_str().unwrap().parse().unwrap();
    assert!((3500..=3600).contains(&retry), "срок — до конца паузы ключа: {retry}");
    let v: Value = r.json().unwrap();
    assert_eq!(v["error"]["type"], "rate_limit_error");
    assert_eq!(keys_of(&seen_o), ["OC-A", "OC-B"]);
    assert!(seen_a.lock().unwrap().is_empty());
}

#[test]
fn откат_без_чужих_мыслей() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, _) = fake(500, r#"{"error":"boom"}"#);
    let p = proxy(base(a, o));
    let mut body = subagent();
    body["messages"] = json!([
        {"role": "user", "content": "x"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": "229b1eb2-536e-4568-9839-4f44875e0eaa"},
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "7"}]}
    ]);
    let (code, _) = post(p.port, body);
    assert_eq!(code, 200);
    let req = seen_a.lock().unwrap()[0].clone();
    assert_eq!(req.body["messages"][1]["content"], json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]),
        "мысль с подписью deepseek Anthropic не примет — её нет");
    assert_eq!(req.body["model"], "claude-haiku-4-5-20251001");
    assert_eq!(req.header("authorization"), Some("Bearer OAUTH-SECRET"));
}

#[test]
fn opencode_не_отвечает_откат_по_таймауту() {
    let (a, seen_a) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, _) = fake_with(|_| Reply { delay_ms: 5_000, ..Reply::json(200, "{}") });
    let p = proxy_with(base(a, o), &[("SUBBAR_HEADERS_MS", "300")], None);
    let mut body = subagent();
    body["stream"] = json!(true);
    let t0 = Instant::now();
    let (code, text) = post(p.port, body);
    assert_eq!(code, 200);
    assert!(text.contains("anthropic"));
    assert!(t0.elapsed() < Duration::from_secs(3), "не ждём OpenCode вечно: {:?}", t0.elapsed());
    assert_eq!(seen_a.lock().unwrap().len(), 1);
    assert!(proxy_status(p.port)["stats"]["recent"][0]["note"].as_str().unwrap().contains("не прислал заголовки"));
}

#[test]
fn ping_пока_opencode_думает() {
    let (a, _) = fake(200, "{}");
    let (o, _) = fake_with(|_| {
        Reply::sse(vec![
            (0, "event: message_start\ndata: {\"type\":\"message_start\"}\n\n"),
            (700, "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        ])
    });
    let p = proxy_with(base(a, o), &[("SUBBAR_PING_MS", "150")], None);
    let mut body = subagent();
    body["stream"] = json!(true);
    let (code, text) = post(p.port, body);
    assert_eq!(code, 200);
    let start = text.find("message_start").unwrap();
    let ping = text.find("event: ping").expect("в тишине — ping");
    let stop = text.find("message_stop").unwrap();
    assert!(start < ping && ping < stop, "{text:?}");
    // Поток дошёл до message_stop — только теперь он и зачтён ответом, с полным временем.
    let st = wait_stats(p.port, |v| v["stats"]["sub"].as_u64() == Some(1));
    assert_eq!(st["stats"]["sub"], 1, "{st}");
    assert_eq!(st["stats"]["errors"], 0, "{st}");
    let s: Value = reqwest::blocking::get(format!("http://127.0.0.1:{}/_subbar/session?id=S-42", p.port)).unwrap().json().unwrap();
    assert_eq!(s["session"]["sub"], 1, "счёт сессии — по факту конца потока: {s}");
}

#[test]
fn sigterm_доделывает_начатый_поток_и_выходит() {
    let (a, _) = fake(200, "{}");
    let chunks: Vec<(u64, &str)> = (0..5).map(|_| (200, "event: content_block_delta\ndata: {}\n\n")).collect();
    let (o, _) = fake_with(move |_| Reply::sse(chunks.clone()));
    let mut p = proxy(base(a, o));
    let port = p.port;
    let worker = std::thread::spawn(move || {
        let mut body = subagent();
        body["stream"] = json!(true);
        post(port, body)
    });
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(p.child.id() as i32, libc::SIGTERM) };
    let (code, text) = worker.join().unwrap();
    assert_eq!(code, 200);
    assert_eq!(text.matches("content_block_delta").count(), 5, "поток дожил до конца: {text:?}");
    let t0 = Instant::now();
    let exit = loop {
        if let Some(s) = p.child.try_wait().unwrap() {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(3), "после работы прокси должен выйти сам");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success());
}

#[test]
fn проверка_связи() {
    let (a, seen_a) = fake(200, "{}");
    let (o, seen_o) = fake(200, r#"{"type":"message","content":[{"type":"thinking","thinking":""},{"type":"text","text":"ок"}]}"#);
    let mut cfg = base(a, o);
    cfg["accountLabel"] = json!("#4");
    let p = proxy(cfg);
    let v: Value = reqwest::blocking::Client::new()
        .post(format!("http://127.0.0.1:{}/_subbar/check", p.port))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["reply"], "ок");
    assert_eq!(v["key"], "#4");
    assert_eq!(v["model"], "deepseek-v4.1-flash");
    assert_eq!(v["warn"], Value::Null, "правила ловят субагента — предупреждать не о чем");
    assert_eq!(seen_o.lock().unwrap()[0].header("x-api-key"), Some("OC-KEY"));
    assert!(seen_a.lock().unwrap().is_empty(), "проверка идёт только в OpenCode");
    assert_eq!(proxy_status(p.port)["stats"]["sub"], 0, "проверка не портит статистику");
}

#[test]
fn проверка_не_трогает_последний_ключ() {
    let (a, _) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, _) = fake(200, r#"{"type":"message","content":[{"type":"text","text":"ок"}]}"#);
    let mut cfg = base(a, o);
    cfg["accountLabel"] = json!("#4");
    let p = proxy(cfg);
    // Субагент записал свой ключ — проверка связи не должна его затирать.
    assert_eq!(post(p.port, subagent()).0, 200);
    assert_eq!(proxy_status(p.port)["keys"]["lastUsed"], "#4");
    reqwest::blocking::Client::new().post(format!("http://127.0.0.1:{}/_subbar/check", p.port)).send().unwrap();
    assert_eq!(proxy_status(p.port)["keys"]["lastUsed"], "#4", "проверка — не субагент");
}

#[test]
fn проверка_предупреждает_о_несработающей_подмене() {
    let (a, _) = fake(200, "{}");
    let (o, _) = fake(200, r#"{"type":"message","content":[{"type":"text","text":"ок"}]}"#);
    let mut cfg = base(a, o);
    // Правило только про sonnet: связь есть, а субагент на haiku в бою мимо правил пойдёт.
    cfg["matchModels"] = json!("sonnet");
    let p = proxy(cfg);
    let v: Value = reqwest::blocking::Client::new()
        .post(format!("http://127.0.0.1:{}/_subbar/check", p.port))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    let warn = v["warn"].as_str().unwrap_or("");
    assert!(warn.contains("подмена не сработает") && warn.contains("sonnet"), "{v}");
}

fn muse_fixture() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/muse-tool-call.sse")).unwrap()
}

#[test]
fn muse_поток_переводится_и_ключ_только_свой() {
    let (a, seen_a) = fake(200, "{}");
    let fixture = muse_fixture();
    let (o, seen_o) = fake_with(move |_| Reply { chunks: vec![(0, fixture.clone())], ..Reply::sse(vec![]) });
    let mut cfg = base(a, o);
    cfg["model"] = json!("muse-spark-1.3-contributor");
    let p = proxy(cfg);
    let mut body = subagent();
    body["stream"] = json!(true);
    let (code, text) = post(p.port, body);
    assert_eq!(code, 200);
    assert!(text.starts_with("event: message_start"), "{text:.200}");
    assert!(text.contains("\"type\":\"tool_use\"") && text.contains("event: message_stop"));
    let req = seen_o.lock().unwrap()[0].clone();
    assert_eq!(req.path, "/zen/go/v1/responses");
    assert_eq!(req.header("authorization"), Some("Bearer OC-KEY"), "в OpenCode — его ключ, не OAuth подписки");
    assert_eq!(req.header("x-opencode-session"), Some("S-42"));
    assert!(seen_a.lock().unwrap().is_empty());
    let st = wait_stats(p.port, |v| v["stats"]["sub"].as_u64() == Some(1));
    assert_eq!(st["stats"]["sub"], 1, "переведённый поток дошёл до message_stop — он ответ: {st}");
}

#[test]
fn muse_обрыв_потока_это_ошибка_для_повтора() {
    let (a, _) = fake(200, "{}");
    let fixture = muse_fixture();
    let cut = fixture[..fixture.find("response.completed").expect("в фикстуре есть response.completed")].rsplit_once("\n\n").unwrap().0.to_string() + "\n\n";
    let (o, _) = fake_with(move |_| Reply { chunks: vec![(0, cut.clone())], ..Reply::sse(vec![]) });
    let mut cfg = base(a, o);
    cfg["model"] = json!("muse-spark-1.3-contributor");
    let p = proxy(cfg);
    let mut body = subagent();
    body["stream"] = json!(true);
    let (_, text) = post(p.port, body);
    assert!(!text.contains("message_stop"), "обрезанный ответ не выдаём за целый");
    assert!(text.trim_end().ends_with('}') && text.contains("event: error") && text.contains("overloaded_error"), "{text}");
    let st = wait_stats(p.port, |v| v["stats"]["errors"].as_u64() == Some(1));
    assert_eq!(st["stats"]["sub"], 0, "обрезанный поток — не ответ: {st}");
    assert_eq!(st["stats"]["errors"], 1, "обрыв считается ошибкой: {st}");
}

#[test]
fn запросы_из_браузера_и_на_чужой_host_отвергаются() {
    let (a, seen_a) = fake(200, "{}");
    let (o, seen_o) = fake(200, "{}");
    let p = proxy(base(a, o));
    let client = reqwest::blocking::Client::new();
    let url = format!("http://127.0.0.1:{}/v1/messages", p.port);
    // Любая открытая страница могла бы тратить квоту OpenCode: прокси сам подставляет ключ.
    let r = client.post(&url).header("origin", "https://evil.example").json(&subagent()).send().unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let r = client.post(&url).header("sec-fetch-site", "cross-site").json(&subagent()).send().unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // DNS rebinding: имя хоста атакующего, адрес — наш.
    let r = client.post(&url).header("host", "evil.example:8479").json(&subagent()).send().unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let r = client.post(format!("http://127.0.0.1:{}/_subbar/check", p.port)).header("origin", "https://evil.example").send().unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert!(seen_o.lock().unwrap().is_empty() && seen_a.lock().unwrap().is_empty(), "ни одного запроса наружу");
    let r = client.post(format!("http://127.0.0.1:{}/v1/messages", p.port)).header("host", format!("localhost:{}", p.port)).json(&subagent()).send().unwrap();
    assert_eq!(r.status().as_u16(), 200, "localhost — свой");
}

#[test]
fn ошибка_opencode_с_ключом_в_тексте_маской() {
    let (a, _) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, _) = fake(500, r#"{"error":"upstream said: bad key OC-KEY-SECRET-123456 and sk-leakleakleak99"}"#);
    let mut cfg = base(a, o);
    cfg["apiKey"] = json!("OC-KEY-SECRET-123456");
    let p = proxy(cfg);
    let (code, text) = post(p.port, subagent());
    assert_eq!(code, 200);
    assert!(text.contains("anthropic"), "откат сработал");
    assert!(!text.contains("OC-KEY-SECRET") && !text.contains("leakleak"), "ключи в ответе клиенту");
    let st = proxy_status(p.port).to_string();
    assert!(!st.contains("OC-KEY-SECRET") && !st.contains("leakleak"), "ключи в статусе");
    assert!(st.contains("…3456"), "маска с хвостом есть: {st}");
}

#[test]
fn ротация_ключей_и_на_пути_muse() {
    let (a, _) = fake(200, "{}");
    let fixture = muse_fixture();
    let (o, seen_o) = fake_with(move |s| {
        if s.header("authorization") == Some("Bearer OC-A") {
            Reply { retry_after: Some("3600"), ..Reply::json(429, WEEKLY) }
        } else {
            Reply { chunks: vec![(0, fixture.clone())], ..Reply::sse(vec![]) }
        }
    });
    let mut cfg = base(a, o);
    cfg["model"] = json!("muse-spark-1.3-contributor");
    cfg["apiKey"] = json!("OC-A");
    cfg["accountLabel"] = json!("#A");
    let p = proxy_with(cfg, &[], Some(state(&[("#A", "OC-A", 100.0), ("#B", "OC-B", 10.0)])));
    let mut body = subagent();
    body["stream"] = json!(true);
    let (code, text) = post(p.port, body);
    assert_eq!(code, 200);
    assert!(text.contains("message_stop"), "{text:.200}");
    let auths: Vec<String> = seen_o.lock().unwrap().iter().map(|s| s.header("authorization").unwrap_or("").to_string()).collect();
    assert_eq!(auths, ["Bearer OC-A", "Bearer OC-B"]);
}

#[test]
fn во_время_выхода_новые_запросы_529_а_начатый_доживает() {
    let (a, _) = fake(200, "{}");
    let chunks: Vec<(u64, &str)> = (0..5).map(|_| (200, "event: content_block_delta\ndata: {}\n\n")).collect();
    let (o, _) = fake_with(move |_| Reply::sse(chunks.clone()));
    let mut p = proxy(base(a, o));
    let port = p.port;
    let worker = std::thread::spawn(move || {
        let mut body = subagent();
        body["stream"] = json!(true);
        post(port, body)
    });
    std::thread::sleep(Duration::from_millis(300));
    unsafe { libc::kill(p.child.id() as i32, libc::SIGTERM) };
    std::thread::sleep(Duration::from_millis(100));
    let (code, text) = post(port, subagent());
    assert_eq!(code, 529, "новый — «повтори»: {text}");
    assert!(text.contains("overloaded_error"));
    let (code, text) = worker.join().unwrap();
    assert_eq!(code, 200);
    assert_eq!(text.matches("content_block_delta").count(), 5);
    let t0 = Instant::now();
    while p.child.try_wait().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(3), "после работы прокси должен выйти сам");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn ошибка_внутри_потока_200_с_ключом_маской_а_текст_цел() {
    let (a, _) = fake(200, "{}");
    let (o, _) = fake_with(|_| {
        Reply::sse(vec![
            (0, "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"файл task-sk-management.xlsx\"}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"bad key OC-KEY-SEC"),
            (50, "RET-123456\"}}\n\n"),
        ])
    });
    let mut cfg = base(a, o);
    cfg["apiKey"] = json!("OC-KEY-SECRET-123456");
    let p = proxy(cfg);
    let mut body = subagent();
    body["stream"] = json!(true);
    let (_, text) = post(p.port, body);
    assert!(!text.contains("OC-KEY-SEC"), "ключ, разорванный между кусками, не утёк: {text}");
    assert!(text.contains("…3456"), "маска есть: {text}");
    assert!(text.contains("task-sk-management.xlsx"), "текст модели не тронут: {text}");
}

#[test]
fn обрыв_нативного_потока_без_message_stop_это_ошибка() {
    let (a, _) = fake(200, "{}");
    let (o, _) = fake_with(|_| Reply::sse(vec![(0, "event: message_start\ndata: {\"type\":\"message_start\"}\n\n")]));
    let p = proxy(base(a, o));
    let mut body = subagent();
    body["stream"] = json!(true);
    let (_, text) = post(p.port, body);
    assert!(text.contains("message_start") && text.contains("поток OpenCode оборвался"), "{text}");
    // Ответ не дочитан — он не ответ: счётчик ответа не растёт, а в ошибки идёт с причиной.
    let st = wait_stats(p.port, |v| v["stats"]["errors"].as_u64() == Some(1));
    assert_eq!(st["stats"]["sub"], 0, "оборванный поток — не ответ: {st}");
    assert_eq!(st["stats"]["errors"], 1, "{st}");
    let note = st["stats"]["recent"][0]["note"].as_str().unwrap();
    assert!(note.contains("поток OpenCode оборвался"), "в счёте видна причина: {note}");
}

#[test]
fn счёт_по_сессии_для_строки_в_терминале() {
    let (a, _) = fake(200, r#"{"from":"anthropic"}"#);
    let (o, _) = fake(200, r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#);
    let p = proxy(base(a, o));
    // Субагент сессии S-42 — в OpenCode, основная модель той же сессии — в Anthropic.
    assert_eq!(post(p.port, subagent()).0, 200);
    let mut main = subagent();
    main["model"] = json!("claude-opus-5-5");
    assert_eq!(post(p.port, main).0, 200);
    let s: Value = reqwest::blocking::get(format!("http://127.0.0.1:{}/_subbar/session?id=S-42", p.port)).unwrap().json().unwrap();
    assert_eq!(s["found"], true, "{s}");
    assert_eq!((s["session"]["sub"].as_u64(), s["session"]["pass"].as_u64()), (Some(1), Some(1)), "{s}");
    assert_eq!(s["session"]["lastModel"], "deepseek-v4.1-flash");
    let other: Value = reqwest::blocking::get(format!("http://127.0.0.1:{}/_subbar/session?id=nope", p.port)).unwrap().json().unwrap();
    assert_eq!(other["found"], false, "чужая сессия — не найдена");
}

#[test]
fn счёт_сессий_переживает_перезапуск_прокси() {
    let (a, _) = fake(200, "{}");
    let (o, _) = fake(200, r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#);
    let cfg = base(a, o);
    let mut p = proxy(cfg.clone());
    assert_eq!(post(p.port, subagent()).0, 200);
    // SIGTERM: мягкий выход сохраняет счёт.
    unsafe { libc::kill(p.child.id() as i32, libc::SIGTERM) };
    let t0 = Instant::now();
    while p.child.try_wait().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(3), "после SIGTERM прокси должен выйти");
        std::thread::sleep(Duration::from_millis(20));
    }
    let p2 = spawn_in(&p.dir.clone(), cfg, &[]);
    let s: Value = reqwest::blocking::get(format!("http://127.0.0.1:{}/_subbar/session?id=S-42", p2.port)).unwrap().json().unwrap();
    assert_eq!(s["session"]["sub"].as_u64(), Some(1), "{s}");
}

#[test]
fn anthropic_не_соединился_повтор_спасает() {
    // Anthropic поднимается через миг после первой попытки: повтор соединения, а не 502 основной сессии.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 65536];
        let _ = std::io::Read::read(&mut s, &mut buf);
        let body = r#"{"from":"anthropic"}"#;
        let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    });
    let (o, _) = fake(200, "{}");
    let p = proxy(base(port, o));
    let (status, text) = post(p.port, json!({"model": "claude-opus-5-5", "messages": [{"role": "user", "content": "x"}]}));
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("anthropic"));
}

#[test]
fn anthropic_мёртв_502_после_повторов() {
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (o, _) = fake(200, "{}");
    let p = proxy(base(dead, o));
    let (status, text) = post(p.port, json!({"model": "claude-opus-5-5", "messages": [{"role": "user", "content": "x"}]}));
    assert_eq!(status, 502);
    assert!(text.contains("Anthropic недоступен"), "{text}");
}

#[test]
fn скорость_ответов_claude_в_сессии() {
    // Поток Claude: первый токен, 300 мс генерации, итог 60 токенов в message_delta — событие разрезано посередине.
    let (a, seen_a) = fake_with(|_| Reply::sse(vec![
        (0, "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":1}}}\n\n"),
        (50, "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n"),
        (300, "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tok"),
        (0, "ens\":60}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
    ]));
    let (o, _) = fake(200, "{}");
    let p = proxy(base(a, o));
    let body = json!({"model": "claude-opus-5-5", "stream": true, "messages": [{"role": "user", "content": "x"}],
        "metadata": {"user_id": "{\"session_id\":\"S-42\"}"}});
    let (status, text) = post(p.port, body);
    assert_eq!(status, 200);
    assert!(text.contains("message_stop"), "поток отдан как есть");
    assert_eq!(seen_a.lock().unwrap()[0].header("accept-encoding"), Some("identity"), "без сжатия — иначе поток не прочесть");
    // Скорость пишется, когда сервер отпустит тело, — это может случиться чуть позже последнего байта у клиента.
    let t0 = Instant::now();
    let s: Value = loop {
        let s: Value = reqwest::blocking::get(format!("http://127.0.0.1:{}/_subbar/session?id=S-42", p.port)).unwrap().json().unwrap();
        if !s["session"]["speedTokens"].is_null() || t0.elapsed() > Duration::from_secs(3) {
            break s;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let (tokens, ms) = (s["session"]["speedTokens"].as_u64().unwrap(), s["session"]["speedMs"].as_u64().unwrap());
    assert_eq!(tokens, 60, "{s}");
    assert!((330..1500).contains(&ms), "время от message_start до конца (скрытые мысли входят): {ms}");
}
