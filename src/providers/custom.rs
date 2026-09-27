use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use crate::model::{Account, ProviderOutcome, RateLimitWindow};
use crate::providers::{find_number, request_json, HttpRequest};
use crate::util::parse_reset_at;

/// Свой JSON-адрес: любой API расхода сводится к одному окну по путям через точку.
pub fn fetch(account: &Account) -> ProviderOutcome {
    let Some(url) = account
        .credentials
        .get("url")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return ProviderOutcome::unavailable("Не задан URL");
    };

    let Some(window_minutes) =
        parse_window_minutes(account.credentials.get("windowMinutes").map(String::as_str))
    else {
        return ProviderOutcome::fail("Длина окна — целое число минут больше нуля");
    };
    let (url, host, addresses) = match validated_url(&url) {
        Ok(result) => result,
        Err(message) => return ProviderOutcome::fail(message),
    };
    let mut request = HttpRequest::get(url)
        .pinned_addresses(host, addresses);
    let filled = |key: &str| account.credentials.get(key).filter(|v| !v.trim().is_empty());
    match (filled("headerName"), filled("headerValue")) {
        // Host подменил бы виртуальный хост на закреплённом адресе — это уже не «заголовок авторизации».
        (Some(name), Some(_)) if name.trim().eq_ignore_ascii_case("host") => {
            return ProviderOutcome::fail("Заголовок Host задавать нельзя — укажи нужный хост в URL".to_string())
        }
        (Some(name), Some(value)) => request = request.header(name.trim(), value.trim()),
        (None, None) => {}
        // Половина заголовка — запрос ушёл бы без авторизации и тихо ловил 401.
        _ => return ProviderOutcome::fail("Заголовок заполнен наполовину: нужны и имя, и значение".to_string()),
    }

    let payload = match request_json(request) {
        Ok(payload) => payload,
        Err(error) => {
            let message = if error.is_unauthorized() && filled("headerName").is_none() {
                "Сервер требует авторизацию (401), а заголовок не задан".to_string()
            } else if error.is_unauthorized() {
                "Сервер не принял авторизацию (401) — проверь заголовок".to_string()
            } else if error.status == Some(403) {
                "Сервер отказал в доступе (403) — проверь ключ и его права в заголовке".to_string()
            } else if error.is_rate_limited() {
                "Сервер просит подождать (429) — обновлю позже".to_string()
            } else if let Some(status) = error.status.filter(|s| (300..400).contains(s)) {
                format!("Адрес перенаправляет ({status}) — укажи конечный URL")
            } else {
                error.message
            };
            return ProviderOutcome::fail(message);
        }
    };

    let used = match read_used_percent(&account.credentials, &payload) {
        Ok(used) => used,
        Err(message) => return ProviderOutcome::fail(message),
    };

    let resets_at = account
        .credentials
        .get("resetPath")
        .filter(|path| !path.trim().is_empty())
        .and_then(|path| {
            let current = super::find_value(&payload, path.trim())?;
            let now = crate::store::now_ms();
            // Срок дальше года — заглушка или секунды не той шкалы, а не настоящий сброс (как у Claude и Codex).
            parse_reset_at(current).filter(|at| *at > now && *at - now <= 366 * 86_400_000)
        });

    ProviderOutcome::ok(vec![RateLimitWindow {
        key: "main".to_string(),
        // Длина окна из формы — в подписи: иначе поле ни на что не влияло.
        label: crate::util::window_label(window_minutes),
        used_percent: used,
        window_minutes,
        resets_at,
        note: None,
    }])
}

fn parse_window_minutes(value: Option<&str>) -> Option<i64> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Some(300),
        Some(value) => value.parse::<i64>().ok().filter(|minutes| *minutes > 0),
    }
}

/// Процент израсходованного. Заданный путь — это обещание, что поле в ответе есть:
/// молча брать второй путь вместо отсутствующего поля значит показать не ту метрику.
/// Значение принимаем с запасом в процент (доля может прийти 100.0000001), а всё остальное —
/// честная ошибка: «осталось 150» это не 0% израсходовано.
fn read_used_percent(
    credentials: &std::collections::BTreeMap<String, String>,
    payload: &serde_json::Value,
) -> Result<f64, String> {
    let path = |key: &str| credentials.get(key).map(String::as_str).unwrap_or("").trim();
    let (used_path, remaining_path) = (path("usedPath"), path("remainingPath"));
    let used = if !used_path.is_empty() {
        find_number(payload, used_path).ok_or_else(|| {
            format!("По пути «{used_path}» нет числа — проверь путь или ответ сервера")
        })?
    } else if !remaining_path.is_empty() {
        let remaining = find_number(payload, remaining_path).ok_or_else(|| {
            format!("По пути «{remaining_path}» нет числа — проверь путь или ответ сервера")
        })?;
        // «-1» у многих API — «безлимит», а не «потрачено 101%»: без этой проверки карточка краснела бы.
        if remaining < -0.01 {
            return Err(format!("Отрицательный остаток в ответе: {:.1} — похоже на «безлимит», проверь путь", remaining.max(-999.0)));
        }
        // В ошибке — число, которое прислал сервер, а не пересчитанное «использовано».
        // Граница та же, что у safe_percent ниже: иначе «осталось 100,5» дало бы «процент −0,5».
        if remaining > 100.01 {
            return Err(format!("Неожиданный остаток в ответе: {:.1} — проверь путь и формат", remaining.min(99_999.0)));
        }
        100.0 - remaining
    } else {
        return Err(
            "Не нашёл процент в ответе: проверь пути «использовано, %» / «осталось, %»"
                .to_string(),
        );
    };
    // Снизу запас только на погрешность дробей: «-0,5» или «осталось 101» — мусор/заглушка, а не «0% потрачено».
    // Граница та же, что у safe_percent (строго больше −0,01): иначе ровно −0,01 давал ошибку без подсказки.
    match crate::providers::safe_percent(used).filter(|_| used <= 101.0) {
        Some(used) => Ok(used),
        None => Err(format!("Неожиданный процент в ответе: {:.1} — проверь путь и формат", used.clamp(-99_999.0, 99_999.0))),
    }
}

/// Резолвим один раз, отвергаем любой частный адрес в ответе и закрепляем принятые
/// адреса для самого запроса. Разбор одной строки пропускает числовые IP,
/// IPv4-mapped IPv6 и DNS-имена, указывающие на localhost.
fn validated_url(input: &str) -> Result<(String, String, Vec<SocketAddr>), &'static str> {
    let mut url = reqwest::Url::parse(input).map_err(|_| "Неверный URL")?;
    // Локальные адреса запрещены ниже, а по голому http ключ ушёл бы в сеть открытым текстом.
    if url.scheme() != "https" {
        return Err("URL должен начинаться с https://");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Логин и пароль в URL не поддерживаются");
    }
    let raw_host = url
        .host_str()
        .ok_or("В URL нет имени сервера")?
        .trim_matches(['[', ']'])
        .to_string();
    let (host, ip) = match raw_host.parse::<IpAddr>() {
        Ok(ip) => (ip.to_string(), Some(ip)),
        Err(_) => {
            // Закрепляем по тому же каноническому имени, что возьмёт reqwest: так
            // и запись с точкой в конце не обойдёт закрепление DNS.
            let normalized = raw_host.to_ascii_lowercase();
            let normalized = normalized.trim_end_matches('.');
            if normalized.is_empty() {
                return Err("В URL нет имени сервера");
            }
            if normalized != raw_host {
                url.set_host(Some(normalized))
                    .map_err(|_| "Неверное имя сервера в URL")?;
            }
            (normalized.to_string(), None)
        }
    };
    let name = host.as_str();
    if name == "localhost"
        || name.ends_with(".localhost")
        || name.ends_with(".local")
        || name.ends_with(".internal")
    {
        return Err("URL указывает на локальный адрес — не безопасно");
    }
    // Схема выше — только https: порт по умолчанию есть всегда, 443 — лишь страховка.
    let port = url.port_or_known_default().unwrap_or(443);
    if port == 0 {
        return Err("В URL задан недопустимый порт");
    }
    let addresses: Vec<SocketAddr> = match ip {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => {
            // Системный резолвер не знает таймаутов: зависший DNS держал бы поток обновления
            // вечно, а вместе с ним флаг «обновляется» и авто-волну всех карточек.
            // Зависший getaddrinfo не прервать — поток остаётся висеть. Потолок на все резолвы разом (и здоровые тоже),
            // иначе каждая авто-волна при мёртвом DNS добавляла бы по потоку навсегда.
            static RESOLVING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            use std::sync::atomic::Ordering::SeqCst;
            if RESOLVING.fetch_add(1, SeqCst) >= 64 {
                RESOLVING.fetch_sub(1, SeqCst);
                return Err("Слишком много одновременных DNS-запросов (прошлые ещё висят) — проверь сеть или адрес");
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let name = host.clone();
            // Счётчик снимает Drop: и после паники резолвера, и если поток не создался.
            struct Slot;
            impl Drop for Slot {
                fn drop(&mut self) {
                    RESOLVING.fetch_sub(1, SeqCst);
                }
            }
            let slot = Slot;
            if std::thread::Builder::new()
                .spawn(move || {
                    let _slot = slot;
                    let _ = tx.send((name.as_str(), port).to_socket_addrs().map(|a| a.collect::<Vec<_>>()));
                })
                .is_err()
            {
                return Err("Не удалось проверить адрес сервера");
            }
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .map_err(|_| "DNS не ответил за 10 с — проверь сеть или адрес")?
                .map_err(|_| "Не удалось проверить адрес сервера")?
        }
    };
    if addresses.is_empty() {
        return Err("Не удалось проверить адрес сервера");
    }
    if addresses.iter().any(|address| forbidden_ip(address.ip())) {
        return Err("URL указывает на локальный адрес — не безопасно");
    }
    Ok((url.to_string(), host, addresses))
}

fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            ip.is_private() || ip.is_loopback() || ip.is_link_local()
                || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast()
                || ip.is_documentation() || a == 0 || a >= 224
                || (a == 100 && (64..=127).contains(&b)) // carrier-grade NAT
                || (a == 198 && (18..=19).contains(&b)) // benchmark networks
                || (a == 192 && b == 0 && c == 0) // IETF protocol assignments 192.0.0.0/24
                || (a == 192 && b == 88 && c == 99) // deprecated 6to4 relay anycast
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4() {
                return forbidden_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            let globally_routable_unicast = segments[0] & 0xe000 == 0x2000;
            !globally_routable_unicast
                || ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
                || (segments[0] == 0x2001 && segments[1] & 0xfe00 == 0x0000) // IETF special-purpose block
                || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
                || segments[0] == 0x2002 // deprecated 6to4 (embeds IPv4)
                || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0x0000) // documentation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn typo_in_window_length_is_not_silently_a_five_hour_window() {
        assert_eq!(parse_window_minutes(None), Some(300));
        assert_eq!(parse_window_minutes(Some("360")), Some(360));
        assert_eq!(parse_window_minutes(Some("abc")), None);
        assert_eq!(parse_window_minutes(Some("0")), None);
    }

    #[test]
    fn проценты_вне_диапазона_не_становятся_правдоподобным_нулём() {
        let payload = serde_json::json!({"usage":{"remaining":150.0}});
        let err = read_used_percent(&creds(&[("remainingPath", "usage.remaining")]), &payload)
            .unwrap_err();
        assert!(err.contains("150"), "подмена 150 «осталось» в 0% не заметна: {err}");
        let payload = serde_json::json!({"usage":{"used":-40.0}});
        assert!(read_used_percent(&creds(&[("usedPath", "usage.used")]), &payload).is_err());
        let payload = serde_json::json!({"usage":{"used":250.0}});
        assert!(read_used_percent(&creds(&[("usedPath", "usage.used")]), &payload).is_err());
        // Доля: 100.0000001 — это ещё 100%, а не ошибка формата.
        let payload = serde_json::json!({"usage":{"used":100.0000001}});
        assert_eq!(read_used_percent(&creds(&[("usedPath", "usage.used")]), &payload), Ok(100.0));
    }

    #[test]
    fn заданный_путь_не_подменяется_вторым_молча() {
        let payload = serde_json::json!({"usage":{"remaining":40.0}});
        let err = read_used_percent(
            &creds(&[("usedPath", "usage.used"), ("remainingPath", "usage.remaining")]),
            &payload,
        )
        .unwrap_err();
        assert!(err.contains("usage.used"), "тихо взяли «осталось» вместо «использовано»: {err}");
        // Без usedPath второй путь работает как и прежде.
        assert_eq!(
            read_used_percent(&creds(&[("remainingPath", "usage.remaining")]), &payload),
            Ok(60.0)
        );
        // Путей нет — тоже ошибка, а не ноль процентов.
        assert!(read_used_percent(&creds(&[]), &payload).is_err());
    }

    #[test]
    fn custom_url_rejects_local_and_obfuscated_addresses() {
        for url in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://2130706433/",
            "https://0x7f.0.0.1/",
            "https://10.0.0.5/",
            "https://192.168.1.10:8080/",
            "https://169.254.169.254/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://user:password@8.8.8.8/",
            "https://host.local/",
        ] {
            assert!(validated_url(url).is_err(), "accepted local URL: {url}");
        }
        assert!(validated_url("https://8.8.8.8/usage").is_ok());
    }
}

