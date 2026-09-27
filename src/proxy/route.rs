//! Куда отправить запрос и как его переписать. Чистые функции — без сети.

use super::config::ProxyConfig;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// На api.anthropic.com (подписка); тело трогаем, только чтобы убрать чужие подписи мыслей.
    Pass,
    /// На OpenCode Go.
    Sub,
}

/// Подменяем только POST /v1/messages (не count_tokens) нужной модели Claude с инструментами.
pub fn decide(cfg: &ProxyConfig, method: &str, path: &str, body: Option<&Value>) -> Route {
    if !cfg.enabled || cfg.api_key.trim().is_empty() || method != "POST" {
        return Route::Pass;
    }
    let path = path.split_once('?').map_or(path, |(p, _)| p);
    if path != "/v1/messages" {
        return Route::Pass;
    }
    let Some(body) = body else { return Route::Pass };
    let model = body.get("model").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let matched = cfg
        .match_models
        .split('|')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .any(|p| word_match(&model, &p));
    if !matched {
        return Route::Pass;
    }
    if cfg.require_tools {
        let has_tools = body.get("tools").and_then(Value::as_array).is_some_and(|t| !t.is_empty());
        if !has_tools {
            return Route::Pass;
        }
    }
    Route::Sub
}

/// Правило совпадает целым словом (мягче valid_rule: разделителем считается любой не буквенно-цифровой символ): «haiku» — в «claude-haiku-4-5»,
/// но не в «haikuish»; иначе подменялась бы любая модель с таким куском в имени.
fn word_match(model: &str, rule: &str) -> bool {
    let edge = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
    model.match_indices(rule).any(|(i, _)| edge(model[..i].chars().next_back()) && edge(model[i + rule.len()..].chars().next()))
}

/// Сессия Claude Code для `x-opencode-session`: из metadata.user_id (JSON с session_id),
/// иначе из заголовка `x-claude-code-session-id`. Замер 26.09: кэш промптов OpenCode держится только
/// при постоянном id (липкая маршрутизация) — у всех субагентов сессии он общий, и кэш у них общий.
/// В заголовок — только видимый ASCII и не длиннее 128: иначе reqwest откажет и субагент уйдёт в откат.
fn clean_sid(s: &str) -> Option<String> {
    // Пробел или управляющий символ — id целиком мимо: склейка «S 1» → «S1» слила бы две сессии в одну.
    if !s.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    // Длиннее 128 — мимо, а не обрезка: две сессии с общим началом иначе делили бы кэш.
    (!s.is_empty() && s.len() <= 128).then(|| s.to_string())
}

pub fn session_id(body: &Value, header: Option<&str>) -> String {
    body.get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("session_id").and_then(Value::as_str).and_then(clean_sid))
        // Негодный id из metadata (пустой после чистки) не должен глушить заголовок.
        .or_else(|| header.and_then(clean_sid))
        .unwrap_or_else(|| "subbar".to_string())
}

/// Подпись мыслей настоящего Claude — base64 с protobuf внутри. Три вида (замер 27.09 по журналам
/// Claude Code: 3782 подписи «CAQS…», единицы «E…»; разбор — CLIProxyAPI claude_validation.go):
/// - «E…» — сразу поле 2 (байт 0x12);
/// - «CAQS…»/«CAIS…» — конверт: поле 1 varint (0x08, версия), затем поле 2 (0x12);
/// - «R…» — двойной слой: base64 от строки «E…».
/// У deepseek подпись — UUID, у space-bunny — 64 hex-знака, у muse — наш префикс: такие Anthropic
/// отвергает (400, замер 26.09). Свою же мысль выкинуть тоже плохо: пропадают рассуждения, а посреди
/// работы с инструментом Anthropic может потребовать их обратно — поэтому узнаём по устройству, не по букве.
pub fn anthropic_signature(sig: &str) -> bool {
    let Some(raw) = b64(sig) else { return false };
    if raw.first() == Some(&b'E') {
        return std::str::from_utf8(&raw).is_ok_and(|inner| inner.len() < sig.len() && anthropic_signature(inner));
    }
    // Обёртки нет — это тот же protobuf без конверта: 0x12, длина, данные.
    let body = match raw.as_slice() {
        [0x08, rest @ ..] => {
            let n = rest.iter().take(10).position(|b| b & 0x80 == 0).map_or(0, |i| i + 1);
            if n == 0 {
                return false;
            }
            &rest[n..]
        }
        other => other,
    };
    body.first() == Some(&0x12)
}

/// Строгий base64 (с корректным паддингом 0…2): иначе None. Anthropic подписи всегда паддит — обрезанная подпись не своя. Своя функция — ради одной проверки тащить крейт незачем.
fn b64(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if s.len() < 64 || s.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let pad = s.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in &s[..s.len() - pad] {
        acc = (acc << 6) | u32::from(val(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// В истории есть мысль с чужой подписью: сессию вели на OpenCode, а теперь запрос идёт на Anthropic
/// (сменили модель, выключили подмену, кончились ключи). Без чистки — 400 от Anthropic на каждом ходу.
pub fn has_foreign_thinking(body: &Value) -> bool {
    body.get("messages").and_then(Value::as_array).is_some_and(|msgs| {
        msgs.iter().filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant")).any(|m| {
            m.get("content").and_then(Value::as_array).is_some_and(|blocks| {
                blocks.iter().any(|b| {
                    // Один redacted_thinking — не повод: Claude отдаёт его штатно, и тело главной сессии
                    // ушло бы переписанным без его рассуждения. Чистим только при явно чужой подписи.
                    b.get("type").and_then(Value::as_str) == Some("thinking")
                        && !b.get("signature").and_then(Value::as_str).is_some_and(anthropic_signature)
                })
            })
        })
    })
}

/// Тело для отката на Anthropic: без мыслей с чужой подписью — так же после 400 чистит и сам Claude Code,
/// только мы делаем это сразу, без лишнего круга. None — чистить нечего, уходит исходное тело байт в байт.
pub fn strip_foreign_thinking(body: &Value) -> Option<Value> {
    let mut out = body.clone();
    let mut changed = false;
    for msg in out.get_mut("messages")?.as_array_mut()? {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = msg.get_mut("content").and_then(Value::as_array_mut) else { continue };
        let before = blocks.len();
        blocks.retain(|b| match b.get("type").and_then(Value::as_str) {
            Some("thinking") => b.get("signature").and_then(Value::as_str).is_some_and(anthropic_signature),
            // redacted_thinking бывает только у настоящего Claude (OpenCode его не выдаёт) — Anthropic его примет,
            // а выкинутый терял бы зашифрованное рассуждение главной сессии на каждом откате.
            _ => true,
        });
        // Опустевший ход не заполняем выдуманным «…»: Claude Code пересылает историю каждый ход,
        // и настоящий Claude жил бы с репликой, которой не говорил. Пустое сообщение убирается ниже.
        if blocks.len() != before {
            changed = true;
        }
    }
    // Сообщение с пустым массивом content — 400 и у Anthropic, как и два хода одной роли подряд после его удаления.
    if let Some(msgs) = out.get_mut("messages").and_then(Value::as_array_mut) {
        let before = msgs.len();
        drop_empty_turns(msgs);
        changed |= msgs.len() != before;
    }
    changed.then_some(out)
}

/// Запрос в формате Claude для OpenCode Go: другая модель и (по желанию) уровень размышления.
/// Остальное — как есть: замер показал, что OpenCode принимает запрос субагента целиком.
pub fn rewrite_native(cfg: &ProxyConfig, body: &Value) -> Value {
    // Не объект: IndexMut ниже паниковал бы и ронял прокси. Сейчас сюда такое не доходит (decide требует model).
    if !body.is_object() {
        return body.clone();
    }
    let mut out = body.clone();
    out["model"] = Value::String(cfg.model.clone());
    if !cfg.effort.is_empty() {
        if !out.get("output_config").is_some_and(Value::is_object) {
            out["output_config"] = serde_json::json!({});
        }
        out["output_config"]["effort"] = Value::String(cfg.effort.clone());
    }
    // Вызов конкретного инструмента при включённых мыслях OpenCode отвергает немым 400 `{"model":…}`
    // (замер 27.09: tool/enabled, tool/adaptive — 400; tool/disabled — 200; any и auto с мыслями — 200).
    // Anthropic такое сочетание тоже запрещает — CLIProxyAPI здесь так же снимает мысли.
    if out.pointer("/tool_choice/type").and_then(Value::as_str) == Some("tool") {
        out["thinking"] = serde_json::json!({"type": "disabled"});
    }
    tidy_messages(&mut out);
    if out.pointer("/thinking/type").and_then(Value::as_str) != Some("disabled") {
        pad_tool_turns(&mut out);
    }
    if let Some(tools) = out.get_mut("tools").and_then(Value::as_array_mut) {
        for t in tools {
            if let Some(s) = t.get_mut("input_schema") {
                strip_pattern(s);
                // Схема без `type` (так бывает у MCP-серверов: `{}` или одни properties) — 400.
                match s {
                    Value::Object(m) if !m.get("type").is_some_and(|t| t.is_string() || t.is_array()) => {
                        m.insert("type".into(), Value::String("object".into()));
                    }
                    Value::Object(_) => {}
                    other => *other = serde_json::json!({"type": "object"}),
                }
            }
        }
    }
    out
}

/// История под правила OpenCode (замер 27.09, иначе 400/422 `{"model":…}`):
/// - `redacted_thinking` — зашифрованная мысль Claude, DeepSeek её не прочтёт, а рядом с обычной мыслью — 422;
/// - текст в сообщении пользователя раньше `tool_result` — 400: результаты инструментов вперёд, порядок внутри сохраняем;
/// - сообщение с пустым массивом `content` (или опустевшее после чистки) — 400: убираем целиком,
///   два сообщения пользователя подряд OpenCode принимает.
fn tidy_messages(out: &mut Value) {
    let Some(msgs) = out.get_mut("messages").and_then(Value::as_array_mut) else { return };
    let kind = |b: &Value, t: &str| b.get("type").and_then(Value::as_str) == Some(t);
    for m in msgs.iter_mut() {
        let user = m.get("role").and_then(Value::as_str) == Some("user");
        let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) else { continue };
        blocks.retain(|b| !kind(b, "redacted_thinking"));
        if user && blocks.iter().any(|b| kind(b, "tool_result")) {
            blocks.sort_by_key(|b| !kind(b, "tool_result"));
        }
    }
    drop_empty_turns(msgs);
}

/// Убрать ходы с пустым `content` и склеить соседей одной роли, которые из-за этого сошлись.
/// Общая для OpenCode и отката в Anthropic: там два user подряд или пустой ход — тоже 400.
fn drop_empty_turns(msgs: &mut Vec<Value>) {
    let kind = |b: &Value, t: &str| b.get("type").and_then(Value::as_str) == Some(t);
    // Без пустого соседи одной роли встают рядом (два хода модели подряд) — их склеиваем в один.
    // Только те, что сошлись из-за удаления: остальную историю отдаём как прислал Claude Code.
    let empty = |m: &Value| m.get("content").and_then(Value::as_array).is_some_and(Vec::is_empty);
    // Пусты все — отдаём как есть: пустой messages был бы нашим 400, а не клиентским.
    if msgs.iter().all(empty) {
        return;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(msgs.len());
    let mut dropped = false;
    for m in msgs.drain(..) {
        if empty(&m) {
            dropped = true;
            continue;
        }
        // Не объект (кривой запрос) не склеиваем: `prev["content"]` на строке паникует и роняет прокси.
        // Склеиваем только ходы с настоящим содержимым (строка или массив): null-соседи склеились бы
        // в пустой массив и выпали оба — вплоть до пустого messages.
        let has_content = |v: &Value| matches!(v.get("content"), Some(Value::String(_) | Value::Array(_)));
        // Флаг гасим только при обычной вставке: иначе после одной склейки третий такой же ход вставал бы вторым подряд.
        let join = dropped
            && m.is_object()
            && has_content(&m)
            && merged.last().is_some_and(|p| p.is_object() && has_content(p) && p.get("role").is_some() && p.get("role") == m.get("role"));
        match merged.last_mut().filter(|_| join) {
            Some(prev) => {
                let blocks = |v: &Value| match v.get("content") {
                    Some(Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
                let mut all = blocks(prev);
                all.extend(blocks(&m));
                // После склейки текст первого снова встал бы перед tool_result второго — тот же 400.
                if prev.get("role").and_then(Value::as_str) == Some("user") {
                    all.sort_by_key(|b| !kind(b, "tool_result"));
                }
                prev["content"] = Value::Array(all);
            }
            None => {
                dropped = false;
                merged.push(m);
            }
        }
    }
    *msgs = merged;
}

/// Ход модели с вызовом инструмента, но без мысли, нативные модели OpenCode Go (DeepSeek и др.) отвергают немым 400 `{"model":…}` — им
/// нужно вернуть рассуждения этого хода (CLIProxyAPI #4890). Так бывает, когда ход сделала настоящая
/// Claude на откате без мыслей или мысль вычистили. Пустая мысль без подписи его устраивает
/// (замер 27.09); `redacted_thinking` — нет.
fn pad_tool_turns(out: &mut Value) {
    let Some(msgs) = out.get_mut("messages").and_then(Value::as_array_mut) else { return };
    for m in msgs.iter_mut().filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant")) {
        let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) else { continue };
        let kind = |b: &Value, t: &str| b.get("type").and_then(Value::as_str) == Some(t);
        if blocks.iter().any(|b| kind(b, "tool_use")) && !blocks.iter().any(|b| kind(b, "thinking")) {
            blocks.insert(0, serde_json::json!({"type": "thinking", "thinking": "", "signature": ""}));
        }
    }
}

/// OpenCode отвечает 400 `{"model":…}` на регулярку с `\0` (у Artifact: `^[^\0]*$`, замер 27.09),
/// поэтому ключевое слово `pattern` выкидываем целиком — как CLIProxyAPI для чужих бэкендов.
/// Имена полей в `properties` не трогаем: у Grep есть поле `pattern`.
pub(crate) fn strip_pattern(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if m.get("pattern").is_some_and(Value::is_string) {
                m.remove("pattern");
            }
            for (k, child) in m.iter_mut() {
                if k == "properties" {
                    match child.as_object_mut() {
                        Some(props) => props.values_mut().for_each(strip_pattern),
                        // Кривая схема (properties массивом): имён полей тут нет — чистим как обычную схему.
                        None => strip_pattern(child),
                    }
                } else {
                    strip_pattern(child);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_pattern),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> ProxyConfig {
        ProxyConfig { api_key: "k".into(), ..Default::default() }
    }
    fn sub_body() -> Value {
        json!({"model":"claude-haiku-4-5-20251001","tools":[{"name":"Bash"}],"messages":[]})
    }

    #[test]
    fn регулярки_в_схемах_инструментов_вычищаются_а_поле_pattern_остаётся() {
        let body = json!({"tools":[
            {"name":"Artifact","input_schema":{"type":"object","properties":{"file_paths":{"type":"array","items":{"type":"string","pattern":"^[^\\0]*$"}}}}},
            {"name":"Grep","input_schema":{"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}}
        ]});
        let out = rewrite_native(&cfg(), &body);
        assert_eq!(out["tools"][0]["input_schema"]["properties"]["file_paths"]["items"], json!({"type":"string"}));
        assert_eq!(out["tools"][1]["input_schema"], body["tools"][1]["input_schema"]);
    }

    #[test]
    fn вызов_конкретного_инструмента_без_мыслей() {
        let body = json!({"thinking":{"type":"adaptive"},"tool_choice":{"type":"tool","name":"Write"},"tools":[]});
        assert_eq!(rewrite_native(&cfg(), &body)["thinking"], json!({"type":"disabled"}));
        let any = json!({"thinking":{"type":"adaptive"},"tool_choice":{"type":"any"},"tools":[]});
        assert_eq!(rewrite_native(&cfg(), &any)["thinking"], json!({"type":"adaptive"}), "any с мыслями OpenCode принимает");
    }

    #[test]
    fn ходу_с_инструментом_без_мысли_подкладывается_пустая() {
        let body = json!({"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":[{"type":"text","text":"a"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"7"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"h","signature":"s"},{"type":"tool_use","id":"t2","name":"Bash","input":{}}]},
            {"role":"assistant","content":[{"type":"redacted_thinking","data":"d"},{"type":"tool_use","id":"t3","name":"Bash","input":{}}]},
            {"role":"assistant","content":[{"type":"text","text":"без инструмента"}]},
            {"role":"assistant","content":"строкой"}
        ]});
        let out = rewrite_native(&cfg(), &body);
        let empty = json!({"type":"thinking","thinking":"","signature":""});
        assert_eq!(out["messages"][1]["content"][0], empty);
        assert_eq!(out["messages"][1]["content"][1]["type"], "text", "остальное на месте");
        assert_eq!(out["messages"][3], body["messages"][3], "своя мысль есть — не трогаем");
        assert_eq!(out["messages"][4]["content"], json!([empty, body["messages"][4]["content"][1]]), "redacted_thinking вон, вместо него пустая");
        assert_eq!(out["messages"][5], body["messages"][5]);
        assert_eq!(out["messages"][6], body["messages"][6]);
        let forced = json!({"tool_choice":{"type":"tool","name":"Bash"},"messages":body["messages"].clone()});
        let mut plain = body["messages"].clone();
        plain[4]["content"].as_array_mut().unwrap().remove(0);
        assert_eq!(rewrite_native(&cfg(), &forced)["messages"], plain, "мысли выключены — подкладывать незачем");
    }

    #[test]
    fn история_под_правила_opencode() {
        let body = json!({"tools":[
            {"name":"Mcp1","input_schema":{}},
            {"name":"Mcp2","input_schema":{"properties":{"a":{"type":"string"}}}},
            {"name":"Mcp3"},
            {"name":"Bash","input_schema":{"type":"object","properties":{}}}
        ],"messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":[{"type":"redacted_thinking","data":"d"}]},
            {"role":"user","content":[]},
            {"role":"assistant","content":[{"type":"redacted_thinking","data":"d"},{"type":"thinking","thinking":"h","signature":"s"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{}},{"type":"tool_use","id":"t2","name":"Bash","input":{}}]},
            {"role":"user","content":[{"type":"text","text":"до"},{"type":"tool_result","tool_use_id":"t1","content":"1"},
                {"type":"text","text":"после"},{"type":"tool_result","tool_use_id":"t2","content":"2"}]}
        ]});
        let out = rewrite_native(&cfg(), &body);
        let tools = &out["tools"];
        assert_eq!(tools[0]["input_schema"], json!({"type":"object"}));
        assert_eq!(tools[1]["input_schema"], json!({"properties":{"a":{"type":"string"}},"type":"object"}));
        assert_eq!(tools[2].get("input_schema"), None, "схемы нет вовсе — не выдумываем");
        assert_eq!(tools[3], body["tools"][3]);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "пустые и опустевшие сообщения убраны: {msgs:?}");
        let kinds: Vec<&str> = msgs[1]["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["thinking", "tool_use", "tool_use"], "redacted_thinking вон");
        let order: Vec<String> = msgs[2]["content"].as_array().unwrap().iter()
            .map(|b| b["tool_use_id"].as_str().or(b["text"].as_str()).unwrap().to_string()).collect();
        assert_eq!(order, ["t1", "t2", "до", "после"], "результаты вперёд, порядок внутри тот же");
    }

    #[test]
    fn пустой_ход_не_ставит_две_роли_подряд() {
        let body = json!({"messages":[
            {"role":"user","content":"a"},
            {"role":"assistant","content":[{"type":"text","text":"готово"}]},
            {"role":"user","content":[]},
            {"role":"assistant","content":[{"type":"text","text":"всё"}]}
        ]});
        let msgs = rewrite_native(&cfg(), &body)["messages"].as_array().unwrap().clone();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant"], "{msgs:?}");
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn склейка_держит_результаты_впереди_и_не_падает_на_мусоре() {
        let tr = |id: &str| json!({"type":"tool_result","tool_use_id":id,"content":"ok"});
        let body = json!({"messages":[
            {"role":"user","content":[tr("t1"), {"type":"text","text":"A"}]},
            {"role":"assistant","content":[]},
            {"role":"user","content":[{"type":"text","text":"B"}, tr("t2")]}
        ]});
        let msgs = rewrite_native(&cfg(), &body)["messages"].as_array().unwrap().clone();
        let order: Vec<&str> = msgs[0]["content"].as_array().unwrap().iter()
            .map(|b| b["tool_use_id"].as_str().or(b["text"].as_str()).unwrap()).collect();
        assert_eq!(order, ["t1", "t2", "A", "B"]);
        let junk = json!({"messages":["x", {"content":[]}, "y"]});
        let out = rewrite_native(&cfg(), &junk);
        // Мусор не валит и не склеивается: не-объекты остаются, пустой ход уходит.
        assert_eq!(out["messages"], json!(["x", "y"]));
        let redacted = json!({"messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"z"}]}]});
        assert!(!has_foreign_thinking(&redacted), "redacted_thinking сам по себе — не повод переписывать");
    }

    #[test]
    fn субагент_haiku_с_инструментами_уходит_в_opencode() {
        assert_eq!(decide(&cfg(), "POST", "/v1/messages?beta=true", Some(&sub_body())), Route::Sub);
    }

    #[test]
    fn основная_модель_и_служебная_haiku_идут_насквозь() {
        let opus = json!({"model":"claude-opus-5-5","tools":[{"name":"Bash"}]});
        assert_eq!(decide(&cfg(), "POST", "/v1/messages", Some(&opus)), Route::Pass);
        let service = json!({"model":"claude-haiku-4-5","messages":[]});
        assert_eq!(decide(&cfg(), "POST", "/v1/messages", Some(&service)), Route::Pass, "без инструментов");
        let empty_tools = json!({"model":"claude-haiku-4-5","tools":[]});
        assert_eq!(decide(&cfg(), "POST", "/v1/messages", Some(&empty_tools)), Route::Pass);
    }

    #[test]
    fn выключено_без_ключа_count_tokens_и_get_насквозь() {
        let off = ProxyConfig { enabled: false, ..cfg() };
        assert_eq!(decide(&off, "POST", "/v1/messages", Some(&sub_body())), Route::Pass);
        let nokey = ProxyConfig { api_key: " ".into(), ..cfg() };
        assert_eq!(decide(&nokey, "POST", "/v1/messages", Some(&sub_body())), Route::Pass);
        assert_eq!(decide(&cfg(), "POST", "/v1/messages/count_tokens", Some(&sub_body())), Route::Pass);
        assert_eq!(decide(&cfg(), "GET", "/v1/messages", Some(&sub_body())), Route::Pass);
    }

    #[test]
    fn правило_haiku_или_sonnet() {
        let c = ProxyConfig { match_models: "haiku|sonnet".into(), ..cfg() };
        let sonnet = json!({"model":"claude-sonnet-4-6","tools":[{"name":"Read"}]});
        assert_eq!(decide(&c, "POST", "/v1/messages", Some(&sonnet)), Route::Sub);
        assert_eq!(decide(&cfg(), "POST", "/v1/messages", Some(&sonnet)), Route::Pass);
    }

    #[test]
    fn переписывание_модели_и_уровня() {
        let body = json!({"model":"claude-haiku-4-5","output_config":{"format":"x"},"tools":[1]});
        let out = rewrite_native(&ProxyConfig { effort: "low".into(), ..cfg() }, &body);
        assert_eq!(out["model"], "deepseek-v4.1-flash");
        assert_eq!(out["output_config"], json!({"format":"x","effort":"low"}));
        let keep = rewrite_native(&ProxyConfig { effort: "".into(), ..cfg() }, &body);
        assert_eq!(keep["output_config"], json!({"format":"x"}), "пусто — как просит Claude");
    }

    #[test]
    fn сессия_из_metadata_или_заголовка() {
        let body = json!({"metadata":{"user_id":"{\"device_id\":\"d\",\"session_id\":\"s-1\"}"}});
        assert_eq!(session_id(&body, Some("h-1")), "s-1");
        assert_eq!(session_id(&json!({}), Some("h-1")), "h-1");
        assert_eq!(session_id(&json!({}), None), "subbar");
        let empty = json!({"metadata":{"user_id":"{\"session_id\":\"\"}"}});
        assert_eq!(session_id(&empty, Some("h-1")), "h-1", "пустой id в теле не глушит заголовок");
    }

    /// Подписи того же устройства, что у Claude (байты — выдуманные): конверт CAQS, голое поле «E…», двойной слой «R…».
    const CAQS: &str = "CAQSQAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=";
    const E_SIG: &str = "EkAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4/";
    const R_SIG: &str = "RWtBQUFRSURCQVVHQndnSkNnc01EUTRQRUJFU0V4UVZGaGNZR1JvYkhCMGVIeUFoSWlNa0pTWW5LQ2txS3l3dExpOHdNVEl6TkRVMk56ZzVPanM4UFQ0Lw==";
    const FOREIGN_B64: &str = "MAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj9A";

    #[test]
    fn правило_модели_совпадает_целым_словом() {
        assert!(word_match("claude-haiku-4-5", "haiku"));
        assert!(word_match("claude-sonnet-4-6[1m]", "sonnet"));
        assert!(!word_match("claude-haikuish-1", "haiku"));
        assert!(!word_match("xsonnet", "sonnet"));
    }

    #[test]
    fn подписи_claude_всех_трёх_видов_свои() {
        for sig in [CAQS, E_SIG, R_SIG] {
            assert!(anthropic_signature(sig), "{sig}");
        }
        for sig in ["229b1eb2-536e-4568-9839-4f44875e0eaa", "7756f0072f7fd90d6c686328bb4fdf8566782df0972523e1a53e7323368dfbe3",
            "subbar-rs1:eyJpZCI6InJzXzEifQ==", "", FOREIGN_B64, &CAQS[..CAQS.len() - 1]] {
            assert!(!anthropic_signature(sig), "{sig}");
        }
    }

    #[test]
    fn на_откате_чужие_мысли_вон_свои_остаются() {
        let claude_sig = CAQS;
        let body = json!({"model":"claude-haiku-4-5","messages":[
            {"role":"user","content":"x"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"","signature":"229b1eb2-536e-4568-9839-4f44875e0eaa"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"7"}]},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"","signature":claude_sig},
                {"type":"redacted_thinking","data":"abc"},
                {"type":"thinking","thinking":"","signature":"7756f0072f7fd90d6c686328bb4fdf8566782df0972523e1a53e7323368dfbe3"},
                {"type":"thinking","thinking":"","signature":"subbar-rs1:eyJpZCI6InJzXzEifQ=="},
                {"type":"text","text":"ok"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"uuid-only"}]}
        ]});
        let out = strip_foreign_thinking(&body).expect("есть что чистить");
        assert_eq!(out["messages"][1]["content"], json!([{"type":"tool_use","id":"t1","name":"Bash","input":{}}]));
        let kinds: Vec<&str> = out["messages"][3]["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["thinking", "redacted_thinking", "text"], "подпись Claude и redacted остаются — OpenCode redacted не выдаёт");
        assert_eq!(out["messages"].as_array().unwrap().len(), 4, "опустевший ход убран, а не заполнен выдуманным «…»");
        assert_eq!(out["model"], "claude-haiku-4-5", "остальное не тронуто");
        let long_foreign = "aB9+".repeat(40);
        assert!(!anthropic_signature(&long_foreign), "длинная base64 без protobuf-заголовка — чужая");
        assert!(!anthropic_signature(FOREIGN_B64), "base64, но первый байт не protobuf Claude");
        let clean = json!({"messages":[{"role":"assistant","content":[{"type":"thinking","signature":claude_sig},{"type":"text","text":"a"}]}]});
        assert_eq!(strip_foreign_thinking(&clean), None, "нечего чистить — тело уходит байт в байт");
    }
}
