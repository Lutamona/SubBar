/// Форматирование, общее для рисования окна и CLI, и разбор base64 и дат ISO-8601 из ответов сервисов.
pub fn clamp_percent(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

/// Неизвестный расход (NaN/∞) — «осталось 0», а не «100%»: для пула ключей оптимизм тут опаснее всего.
pub fn remaining_percent(used: f64) -> f64 {
    if !used.is_finite() {
        return 0.0;
    }
    100.0 - clamp_percent(used)
}

/// Процент для показа целым числом: край не округляем в край. 99,6 — это ещё не «100%»
/// (окно не выбрано / не полно), 0,4 — уже не «0%»: иначе надпись спорит с полосой рядом.
pub fn display_percent(p: f64) -> i64 {
    // Контракт 0..=100 держим сами: NaN/∞ не должны печататься мусорным числом.
    let p = if p.is_finite() { p.clamp(0.0, 100.0) } else { 0.0 };
    match p {
        p if p > 0.0 && p < 1.0 => 1,
        p if p > 99.0 && p < 100.0 => 99,
        p => p.round() as i64,
    }
}

/// "4ч 12м", "12м", "3д 5ч", "сейчас", а по краям — "<1м" и ">999д"
pub fn format_duration(ms: i64) -> String {
    if ms <= 0 {
        return "сейчас".to_string();
    }
    let total_minutes = ms / 60_000;
    if total_minutes < 1 {
        return "<1м".to_string();
    }
    let days = total_minutes / 1440;
    // Потолок здесь, а не у каждого вызывающего: мусорная метка не печатается «106751991167д».
    if days > 999 {
        return ">999д".to_string();
    }
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;
    if days > 0 {
        return if hours > 0 {
            format!("{days}д {hours}ч")
        } else {
            format!("{days}д")
        };
    }
    if hours > 0 {
        return if minutes > 0 {
            format!("{hours}ч {minutes}м")
        } else {
            format!("{hours}ч")
        };
    }
    format!("{minutes}м")
}

pub fn format_reset_countdown(resets_at: Option<i64>, now: i64) -> Option<String> {
    let resets_at = resets_at?;
    let delta = resets_at.saturating_sub(now);
    // Срок прошёл давно — это старые данные (опрос не проходит), а не «вот-вот»:
    // иначе подпись висела бы сутками. Нет отсчёта — честнее.
    if delta <= -10 * 60_000 {
        return None;
    }
    // Дальше года — мусор провайдера или state.json (i64::MAX), а не срок: «сброс через 106751991167300д».
    if delta > 400 * 86_400_000 {
        return None;
    }
    if delta <= 0 {
        // Встаёт в «сброс через …»: «через вот-вот» не по-русски, а «1м» врёт — срок уже наступил.
        return Some("<1м".to_string());
    }
    Some(format_duration(delta))
}

/// Местное время отметки: «23:05» (сегодня) или «25.09 23:05».
/// Неразбираемая дата — «—». Раньше тут был ноль-инициализированный `tm` на NULL-ответе
/// (`00.01 00:00`), а на macOS `localtime_r` для мусора NULL не отдаёт, а пишет год
/// вроде 292277094 — это выглядело бы как правдоподобное «17.08 10:12».
pub fn local_clock(ts_ms: i64, now_ms: i64) -> String {
    fn parts(ms: i64) -> Option<(i64, i64, i64, i64, i64, i64)> {
        let secs = (ms / 1000) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
            return None;
        }
        // tm_year — годы с 1900; за пределами 1900..=9999 это мусор, а не отметка.
        if !(0..=8099).contains(&tm.tm_year) {
            return None;
        }
        Some((
            tm.tm_year as i64,
            tm.tm_yday as i64,
            tm.tm_mday as i64,
            tm.tm_mon as i64 + 1,
            tm.tm_hour as i64,
            tm.tm_min as i64,
        ))
    }
    // Ноль и отрицательное — «нет отметки», а не 1 января 1970-го.
    if ts_ms <= 0 {
        return "—".to_string();
    }
    let (Some((year, yday, day, month, hour, minute)), Some((now_year, now_yday, ..))) =
        (parts(ts_ms), parts(now_ms))
    else {
        return "—".to_string();
    };
    if (year, yday) == (now_year, now_yday) {
        format!("{hour:02}:{minute:02}")
    } else {
        format!("{day:02}.{month:02} {hour:02}:{minute:02}")
    }
}

pub fn format_time_ago(ts: Option<i64>, now: i64) -> String {
    let Some(ts) = ts else {
        return "никогда".to_string();
    };
    let diff = now.saturating_sub(ts);
    // Метка из будущего (часы переставили назад) — не «только что»: это неверные часы.
    if diff < -60_000 {
        // Встаёт в «данные …»/«обновлено …»: фраза должна читаться и там.
        return "в будущем — проверь часы".to_string();
    }
    if diff < 45_000 {
        return "только что".to_string();
    }
    if diff < 3_600_000 {
        let minutes = (diff / 60_000).max(1);
        return format!("{minutes} мин назад");
    }
    if diff < 86_400_000 {
        return format!("{} ч назад", diff / 3_600_000);
    }
    // Метка 0 или древняя (сентинел «нет данных») — не «20688 дней назад».
    if diff > 3650 * 86_400_000 {
        return "давно".to_string();
    }
    format!("{} назад", plural((diff / 86_400_000) as u64, "день", "дня", "дней"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Calm,
    Warn,
    Danger,
}

pub fn band(used_percent: f64) -> Band {
    // Неизвестный расход — не «спокойно»: как и в remaining_percent, оптимизм тут опасен.
    if !used_percent.is_finite() {
        return Band::Warn;
    }
    let used = clamp_percent(used_percent);
    if used < 60.0 {
        Band::Calm
    } else if used < 85.0 {
        Band::Warn
    } else {
        Band::Danger
    }
}

/// Window label from its length: "5ч", "7д", "30д".
pub fn window_label(minutes: i64) -> String {
    if minutes <= 0 {
        return "—".to_string();
    }
    if minutes < 60 {
        return format!("{minutes}м");
    }
    if minutes < 1440 {
        let hours = minutes / 60;
        let rest = minutes % 60;
        return if rest == 0 {
            format!("{hours}ч")
        } else {
            format!("{hours}ч {rest}м")
        };
    }
    let days = minutes / 1440;
    let rest = minutes % 1440;
    // Мусор из ответа не должен раздувать подпись до «30517205850д» — тот же потолок, что у format_duration.
    if days > 999 {
        return ">999д".to_string();
    }
    if rest >= 60 {
        format!("{days}д {}ч", rest / 60)
    } else if rest > 0 {
        format!("{days}д {rest}м")
    } else {
        format!("{days}д")
    }
}

/// Разбор base64url (паддинг не обязателен) — для JWT. Стандартный алфавит (+/) тоже принимается: значения те же.
pub fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for ch in input.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            // JWT из файлов/буфера бывает с переносом строки — это не повод терять весь токен.
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // Остаток в 6 бит — лишний одиночный символ: base64 такой длины не бывает.
    if bits == 6 {
        return None;
    }
    Some(out)
}

/// Разбор обычного base64 (паддинг не обязателен) — для кэша protobuf.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for ch in input.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break, // дополнение — конец данных, дальше не читаем
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // Остаток в 6 бит — лишний одиночный символ: base64 такой длины не бывает.
    if bits == 6 {
        return None;
    }
    Some(out)
}

/// Верхняя граница «разумной даты» — 9999-12-31T23:59:59Z в миллисекундах.
const MAX_RESET_MS: i64 = 253_402_300_799_000;

/// Разбор сброса в свободной форме: unix-секунды, unix-миллисекунды или строка ISO-8601.
pub fn parse_reset_at(value: &serde_json::Value) -> Option<i64> {
    fn normalize_epoch(value: f64) -> Option<i64> {
        if !value.is_finite() {
            return None;
        }
        // Current Unix seconds have 10 digits; milliseconds have 13. Порог 1e11: секунды до 5138 года
        // и миллисекунды после марта 1973 различаются однозначно.
        let millis = if value.abs() >= 100_000_000_000.0 {
            value
        } else {
            value * 1000.0
        };
        // 0 и отрицательное — «сброса нет» у JS-бэкендов; дальше 9999 года — мусор, а не дата.
        // Раньше 2001 года — не дата, а «секунд до сброса» (3600 → 1970 год): такой сброс «уже прошёл»
        // и выкидывал бы окно из расчёта запаса.
        (millis >= 978_307_200_000.0 && millis <= MAX_RESET_MS as f64).then_some(millis as i64)
    }

    match value {
        serde_json::Value::Number(number) => {
            let raw = number.as_f64()?;
            normalize_epoch(raw)
        }
        serde_json::Value::String(text) => {
            let text = text.trim();
            if let Ok(numeric) = text.parse::<f64>() {
                return normalize_epoch(numeric);
            }
            // Строку тоже через ту же границу: год «-001» разобрался бы в дату до эпохи.
            parse_iso8601(text).filter(|ms| *ms >= 978_307_200_000 && *ms <= MAX_RESET_MS)
        }
        _ => None,
    }
}

/// Разбор меток API вида `2026-09-22T14:14:12.981Z` — битые даты и смещения
/// не должны сойти за правдоподобное время сброса.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    // Разделители: 2026-09-22T14:14:12
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    // parse() принял бы «+2» и «-1» — поля даты только из цифр.
    let digits_ok = [0..4, 5..7, 8..10, 11..13, 14..16, 17..19].into_iter().all(|r| bytes[r].iter().all(u8::is_ascii_digit));
    if !digits_ok {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    // День месяца с учётом високосных лет.
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        _ => 28,
    };
    if day > days_in_month {
        return None;
    }

    let mut suffix = text.get(19..)?;
    if let Some(fraction) = suffix.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        suffix = &fraction[digits..];
    }
    let tz_offset_seconds = parse_timezone_offset(suffix)?;

    // Дни от эпохи unix (алгоритм civil-from-days)
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let utc_seconds = days * 86_400 + hour * 3600 + minute * 60 + second - tz_offset_seconds;
    Some(utc_seconds.saturating_mul(1000))
}

fn parse_timezone_offset(value: &str) -> Option<i64> {
    if value.is_empty() || value.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let bytes = value.as_bytes();
    if bytes.len() != 6
        || !matches!(bytes[0], b'+' | b'-')
        || bytes[3] != b':'
        || !bytes[1..3].iter().all(u8::is_ascii_digit)
        || !bytes[4..6].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let hours: i64 = value.get(1..3)?.parse().ok()?;
    let minutes: i64 = value.get(4..6)?.parse().ok()?;
    // Смещение ISO-8601 не больше 14:00.
    if hours > 14 || minutes > 59 || (hours == 14 && minutes != 0) {
        return None;
    }
    let offset = hours * 3600 + minutes * 60;
    Some(if bytes[0] == b'-' { -offset } else { offset })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parsing_matches_known_timestamps() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601("2026-09-22T14:14:12.981Z"),
            Some(1790086452000)
        );
    }

    #[test]
    fn window_labels_reflect_actual_length() {
        assert_eq!(window_label(300), "5ч");
        assert_eq!(window_label(360), "6ч");
        assert_eq!(window_label(10080), "7д");
        assert_eq!(window_label(11520), "8д");
        assert_eq!(window_label(43200), "30д");
    }

    #[test]
    fn duration_formatting_is_russian() {
        assert_eq!(format_duration(4 * 3_600_000 + 12 * 60_000), "4ч 12м");
        assert_eq!(format_duration(3 * 86_400_000 + 5 * 3_600_000), "3д 5ч");
        assert_eq!(format_duration(30_000), "<1м");
    }
}

#[cfg(test)]
mod base64_tests {
    use super::*;

    #[test]
    fn decodes_standard_base64() {
        assert_eq!(
            base64_decode("CAEaGw==").unwrap(),
            vec![0x08, 0x01, 0x1a, 0x1b]
        );
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello".to_vec());
    }
}

#[cfg(test)]
mod iso_tests {
    use super::*;

    #[test]
    fn rejects_invalid_dates() {
        assert!(parse_iso8601("2026-02-30T00:00:00Z").is_none()); // Feb 30
        assert!(parse_iso8601("2023-02-29T00:00:00Z").is_none()); // not leap
        assert!(parse_iso8601("2024-02-29T00:00:00Z").is_some()); // leap ok
        assert!(parse_iso8601("2026x09x22T14:14:12Z").is_none()); // bad separators
        assert!(parse_iso8601("2026-09-22 14:14:12Z").is_none()); // space not T
        assert!(parse_iso8601("2026-09-22T14:00:00+99:99").is_none()); // bad tz
        assert!(parse_iso8601("2026-09-22T14:00:00+15:00").is_none()); // >14h
        assert!(parse_iso8601("2026-09-22T14:00:00+05:30").is_some()); // ok
    }

    #[test]
    fn reset_values_outside_9999_are_rejected() {
        // Год 10000 и дальше — мусор, а не «сброс через 8000 лет».
        assert_eq!(parse_reset_at(&serde_json::json!(253_402_300_800_000i64)), None);
        assert_eq!(parse_reset_at(&serde_json::json!(1e300)), None);
        assert_eq!(parse_reset_at(&serde_json::json!("9999-12-31T23:59:59Z")), Some(MAX_RESET_MS));
        // До эпохи и ровно 0 — «сброса нет», и строками тоже.
        assert_eq!(parse_reset_at(&serde_json::json!(0)), None);
        assert_eq!(parse_reset_at(&serde_json::json!("1969-01-01T00:00:00Z")), None);
        assert!(parse_reset_at(&serde_json::json!(1_780_000_000_000i64)).is_some());
    }

    #[test]
    fn local_clock_rejects_impossible_dates() {
        // Мусорная отметка: на macOS localtime_r вернёт не NULL, а год за пределами
        // разумного — такой рисовался бы как правдоподобное «17.08 10:12».
        assert_eq!(local_clock(i64::MAX, 0), "—");
        assert_eq!(local_clock(i64::MIN, 0), "—");
        // Мусор в «сейчас» — тоже прочерк, иначе сравнение дней ломается.
        assert_eq!(local_clock(0, i64::MAX), "—");
        // Нормальная отметка в ту же секунду рисуется часами, а не прочерком.
        let now = 1_780_000_000_000i64;
        // Тот же день — только «ЧЧ:ММ», без даты.
        let same = local_clock(now, now);
        assert!(same.len() == 5 && same.as_bytes()[2] == b':', "{same}");
        // Год 9999 — последний допустимый, рисуется как дата.
        assert_ne!(local_clock(253_402_214_400_000, 1_780_000_000_000), "—");
    }
}

/// Русское число с существительным: 1 откат, 2 отката, 5 откатов.
pub fn plural(n: u64, one: &str, few: &str, many: &str) -> String {
    let word = match (n % 10, n % 100) {
        (_, 11..=14) => many,
        (1, _) => one,
        (2..=4, _) => few,
        _ => many,
    };
    format!("{n} {word}")
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn plural_covers_russian_forms() {
        for (n, want) in [(1, "1 день"), (2, "2 дня"), (5, "5 дней"), (11, "11 дней"), (21, "21 день"), (22, "22 дня"), (25, "25 дней"), (111, "111 дней")] {
            assert_eq!(plural(n, "день", "дня", "дней"), want);
        }
    }

    #[test]
    fn base64url_decodes_and_skips_whitespace() {
        assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello".to_vec());
        assert_eq!(base64url_decode("aGVs\nbG8").unwrap(), b"hello".to_vec());
        assert_eq!(base64url_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert!(base64url_decode("a*b").is_none());
    }

    #[test]
    fn timezone_sign_and_century_leap_years() {
        let plus = parse_iso8601("2026-09-22T14:00:00+05:30").unwrap();
        let minus = parse_iso8601("2026-09-22T14:00:00-05:30").unwrap();
        assert_eq!(minus - plus, 11 * 3_600_000);
        assert!(parse_iso8601("2000-02-29T00:00:00Z").is_some());
        assert!(parse_iso8601("1900-02-29T00:00:00Z").is_none());
        assert!(parse_iso8601("2100-02-29T00:00:00Z").is_none());
    }

    #[test]
    fn reset_accepts_epoch_seconds_string() {
        assert_eq!(parse_reset_at(&serde_json::json!("1780000000")), Some(1_780_000_000_000));
    }

    #[test]
    fn window_label_branches() {
        assert_eq!(window_label(90), "1ч 30м");
        assert_eq!(window_label(1500), "1д 1ч");
        assert_eq!(window_label(0), "—");
    }

    #[test]
    fn overdue_reset_reads_as_under_a_minute() {
        assert_eq!(format_reset_countdown(Some(1000), 1000).as_deref(), Some("<1м"));
    }

    #[test]
    fn countdown_drops_long_overdue_and_absurd_resets() {
        let now = 1_800_000_000_000;
        assert_eq!(format_reset_countdown(Some(now - 11 * 60_000), now), None);
        assert_eq!(format_reset_countdown(Some(now + 401 * 86_400_000), now), None);
    }

    #[test]
    fn display_percent_never_rounds_to_the_edges() {
        assert_eq!(display_percent(99.6), 99);
        assert_eq!(display_percent(0.4), 1);
        assert_eq!(display_percent(0.0), 0);
        assert_eq!(display_percent(100.0), 100);
    }
}
