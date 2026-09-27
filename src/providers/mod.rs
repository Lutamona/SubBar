pub mod claude;
pub mod codex;
pub mod commandcode;
pub mod custom;
pub mod devin;
pub mod opencode_go;

use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use crate::model::{Account, DetectedCredential, ProviderId, ProviderOutcome};
use crate::store::now_ms;

const DEFAULT_USER_AGENT: &str = concat!("SubBar/", env!("CARGO_PKG_VERSION"), " (+macos)");

pub struct HttpRequest {
    pub url: String,
    pub method: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub user_agent: String,
    /// Свои адреса закрепляют проверенные ответы DNS — перепривязка не обойдёт защиту по IP.
    pub resolved_addrs: Option<(String, Vec<std::net::SocketAddr>)>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        HttpRequest {
            url: url.into(),
            method: "GET",
            headers: Vec::new(),
            body: None,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            resolved_addrs: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        HttpRequest {
            method: "POST",
            body: Some(body.into()),
            ..HttpRequest::get(url)
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn user_agent(mut self, agent: impl Into<String>) -> Self {
        self.user_agent = agent.into();
        self
    }

    pub fn pinned_addresses(mut self, host: String, addresses: Vec<std::net::SocketAddr>) -> Self {
        self.resolved_addrs = Some((host, addresses));
        self
    }
}

/// Ошибка с HTTP-статусом, если ответ вообще был.
pub struct HttpError {
    pub status: Option<u16>,
    pub message: String,
}

impl HttpError {
    pub fn is_unauthorized(&self) -> bool {
        self.status == Some(401)
    }

    pub fn is_rate_limited(&self) -> bool {
        self.status == Some(429)
    }
}

/// Общий клиент на процесс. Редиректы не ходим никогда: same-host 302 унёс бы заголовок с токеном.
static HTTP_CLIENT_NO_REDIRECT: std::sync::LazyLock<Result<reqwest::blocking::Client, ()>> =
    std::sync::LazyLock::new(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ())
    });

/// Закреплённые клиенты по хосту и проверенным адресам: без кэша каждый тик поднимал бы новый клиент
/// с новым TLS-рукопожатием.
static PINNED: std::sync::LazyLock<std::sync::Mutex<HashMap<(String, Vec<std::net::SocketAddr>), reqwest::blocking::Client>>> =
    std::sync::LazyLock::new(Default::default);

fn pinned_client(host: &str, addresses: &[std::net::SocketAddr]) -> Result<reqwest::blocking::Client, HttpError> {
    let key = (host.to_string(), addresses.to_vec());
    let mut cache = PINNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(client) = cache.get(&key) {
        return Ok(client.clone());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        // Без системного прокси намеренно: прокси сам резолвит имя, и закрепление адреса (защита от rebind) потеряло бы смысл.
        .no_proxy()
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|_| HttpError { status: None, message: "Не удалось подготовить защищённый запрос".to_string() })?;
    // Адреса хоста меняются редко, но потолок нужен: иначе кэш рос бы с каждым новым DNS-ответом.
    // Снимаем одну произвольную запись (HashMap порядка не знает); идущий запрос держит свой клон — не ждём его.
    if cache.len() >= 8 {
        if let Some(old) = cache.keys().next().cloned() {
            cache.remove(&old);
        }
    }
    cache.insert(key, client.clone());
    Ok(client)
}

pub fn request_json(request: HttpRequest) -> Result<serde_json::Value, HttpError> {
    let pinned = if let Some((host, addresses)) = &request.resolved_addrs {
        // Адреса закреплены за одним хостом: запрос на другой шёл бы мимо проверки rebind.
        let url_host = reqwest::Url::parse(&request.url).ok().and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_ascii_lowercase()));
        if url_host.as_deref() != Some(host.trim_matches(['[', ']']).to_ascii_lowercase().as_str()) {
            return Err(HttpError { status: None, message: "Адрес запроса не совпал с проверенным хостом".to_string() });
        }
        Some(pinned_client(host, addresses)?)
    } else {
        None
    };
    let client = match pinned.as_ref() {
        Some(client) => Ok(client),
        None => HTTP_CLIENT_NO_REDIRECT.as_ref(),
    }
    .map_err(|_| HttpError {
        status: None,
        message: "Не удалось подготовить HTTP-клиент".to_string(),
    })?;

    let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| HttpError {
        status: None,
        message: "Неверный HTTP-метод".to_string(),
    })?;
    let mut builder = client
        .request(method, &request.url)
        .timeout(Duration::from_secs(15));
    // Свой Accept/User-Agent из карточки заменяет встроенный, а не уходит вторым заголовком.
    let has = |h: &str| request.headers.iter().any(|(n, _)| n.trim().eq_ignore_ascii_case(h));
    if !has("accept") {
        builder = builder.header(reqwest::header::ACCEPT, "application/json");
    }
    if !has("user-agent") {
        builder = builder.header(reqwest::header::USER_AGENT, &request.user_agent);
    }
    if request.body.is_some() && !has("content-type") {
        builder = builder.header(reqwest::header::CONTENT_TYPE, "application/json");
    }

    for (name, value) in &request.headers {
        match (
            reqwest::header::HeaderName::try_from(name.trim()),
            reqwest::header::HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(name), Ok(value)) => {
                builder = builder.header(name, value);
            }
            _ => {
                // Молча не выбрасываем — запрос ушёл бы без авторизации.
                // Имя называем, значение (там ключ) — нет. Но в «имя» по ошибке вставляют всю строку
                // «X-Api-Key: sk-…» — такое имя уже с ключом, его не показываем.
                let name = name.trim();
                let message = if name.len() <= 40 && !name.contains([':', ' ']) {
                    let shown: String = name.chars().filter(|c| c.is_ascii_graphic()).collect();
                    format!("Неверный заголовок «{shown}» — проверь его имя и значение")
                } else {
                    "Неверное имя заголовка — там должно быть только имя, без двоеточия и значения".to_string()
                };
                return Err(HttpError { status: None, message });
            }
        }
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    let response = builder.send().map_err(|error| HttpError {
        status: None,
        // Текст ошибки reqwest может содержать URL (в том числе API-ключи
        // в параметрах). Ни хранить, ни показывать его дословно нельзя.
        message: if error.is_timeout() {
            "Таймаут запроса".to_string()
        } else if error.is_connect() {
            "Не удалось подключиться к серверу".to_string()
        } else {
            "Ошибка сетевого запроса".to_string()
        },
    })?;

    let status = response.status().as_u16();
    // Редиректы не выполняем (ушёл бы заголовок с ключом) — так и сказать, иначе адрес крутят по кругу.
    // До чтения тела: огромное тело 302 иначе дало бы «слишком большой» вместо настоящей причины.
    if (300..400).contains(&status) {
        return Err(HttpError {
            status: Some(status),
            message: format!("HTTP {status}: сервер перенаправляет — укажи адрес, куда он ведёт"),
        });
    }
    // Потоковое чтение с жёстким потолком — враждебный адрес не съест память.
    let mut body = Vec::new();
    let mut limited = response.take(4 * 1024 * 1024 + 1);
    limited.read_to_end(&mut body).map_err(|_| HttpError {
        status: Some(status),
        message: "Не удалось прочитать ответ сервера".to_string(),
    })?;
    if body.len() > 4 * 1024 * 1024 {
        return Err(HttpError {
            status: Some(status),
            message: "Ответ слишком большой (>4MB)".to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(HttpError {
            status: Some(status),
            // Сервер может вернуть эхом Authorization, токены из запроса или весь
            // запрос целиком. Тело в state и журнале ошибок утекло бы секретами.
            message: format!("HTTP {status}: сервер вернул ошибку"),
        });
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(HttpError { status: Some(status), message: "Сервер вернул пустой ответ".to_string() });
    }
    serde_json::from_slice::<serde_json::Value>(&body).map_err(|_| HttpError {
        status: Some(status),
        message: "Сервер ответил не JSON — адрес или провайдер не тот".to_string(),
    })
}

/// Значения сервиса бывают вне диапазона или переполняются в арифметике.
/// Не храним нечисловой процент под видом обманчивых 0%.
pub fn safe_percent(value: f64) -> Option<f64> {
    // Отрицательное — метка («безлимит», «нет данных»), а не 0%: уверенное «осталось 100%» врало бы.
    // Шум округления в пределах −0,01…0 — ноль.
    (value.is_finite() && value > -0.01).then(|| value.clamp(0.0, 100.0))
}

/// Значение по пути вида `a.b.0.c` (номер — индекс массива).
pub fn find_value<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    if path.trim().is_empty() {
        return None;
    }
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            serde_json::Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            serde_json::Value::Object(map) => map.get(segment)?,
            _ => return None,
        };
    }
    Some(current)
}

pub fn find_number(value: &serde_json::Value, path: &str) -> Option<f64> {
    find_value(value, path).and_then(num)
}

/// Завернуть сырой итог сервиса в форму сохраняемого снимка.
pub fn finalize(account: &Account, outcome: ProviderOutcome) -> crate::model::AccountUsage {
    let now = now_ms();
    let ok = matches!(outcome.status, crate::model::FetchStatus::Ok);
    // Последний рубеж для любого провайдера: не-конечный процент в state.json не пишем,
    // а вылет за 0..100 прижимаем — иначе цвет полосы и цифра расходятся.
    let windows = outcome
        .windows
        .into_iter()
        .filter(|w| w.used_percent.is_finite())
        .map(|mut w| {
            w.used_percent = w.used_percent.clamp(0.0, 100.0);
            w
        })
        .collect();
    crate::model::AccountUsage {
        status: outcome.status,
        windows,
        plan_type: outcome.plan_type,
        notes: outcome.notes,
        error: outcome.error,
        updated_at: now,
        last_ok_at: if ok {
            Some(now)
        } else {
            account
                .last_usage
                .as_ref()
                .and_then(|usage| usage.last_ok_at)
        },
    }
}

pub fn fetch_account(account: &Account) -> (crate::model::AccountUsage, Vec<(String, String)>) {
    let outcome = match account.provider {
        ProviderId::Codex => codex::fetch(account),
        ProviderId::OpenCodeGo => opencode_go::fetch(account),
        ProviderId::CommandCode => commandcode::fetch(account),
        ProviderId::Devin => devin::fetch(account),
        ProviderId::Claude => claude::fetch(account),
        ProviderId::Custom => custom::fetch(account),
    };
    let patch = outcome.credentials_patch.clone();
    (finalize(account, outcome), patch)
}

pub fn detect_all() -> Vec<DetectedCredential> {
    let mut found = Vec::new();
    found.extend(codex::detect());
    found.extend(opencode_go::detect());
    found.extend(commandcode::detect());
    found.extend(devin::detect());
    found.extend(claude::detect());
    found
}

/// Число из JSON, в том числе присланное строкой: иначе окно лимита молча пропадало.
pub fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).filter(|n: &f64| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn percentages_reject_overflow_instead_of_becoming_zero() {
        assert_eq!(safe_percent(f64::INFINITY), None);
        assert_eq!(safe_percent(f64::NAN), None);
        assert_eq!(safe_percent(250.0), Some(100.0));
        assert_eq!(safe_percent(-5.0), None);
        assert_eq!(safe_percent(-0.001), Some(0.0));
    }

    #[test]
    fn http_errors_never_echo_response_or_secret_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let body = "synthetic-secret-in-error-body";
            write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let url = format!(
            "http://synthetic.invalid:{}/?token=synthetic-secret-in-url",
            address.port()
        );
        let request =
            HttpRequest::get(url).pinned_addresses("synthetic.invalid".into(), vec![address]);
        let error = request_json(request)
            .err()
            .expect("server returned HTTP 401");
        server.join().unwrap();
        assert_eq!(error.status, Some(401));
        assert!(!error.message.contains("synthetic-secret"));
        assert_eq!(error.message, "HTTP 401: сервер вернул ошибку");
    }
}

