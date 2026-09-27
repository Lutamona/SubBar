//! Перевод Claude Messages ↔ OpenAI Responses (так говорит muse-spark на OpenCode Go).
//! Чистые функции и конечный автомат потока — без сети, проверяется на записанном потоке.
//!
//! Рассуждения muse приходят зашифрованными (`encrypted_content`). Их кладём в подпись блока
//! `thinking` (префикс SIG_PREFIX): Claude Code вернёт блок на следующем ходу — и muse получит
//! свои рассуждения обратно. Чужие (настоящие Claude) мысли в muse не отправляем.

use super::config::ProxyConfig;
use serde_json::{json, Map, Value};

const SIG_PREFIX: &str = "subbar-rs1:";

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = buf << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits & 0xff) as u8);
        }
    }
    Some(out)
}

fn encode_reasoning(item: &Value) -> String {
    let keep = json!({"id": item.get("id"), "encrypted_content": item.get("encrypted_content")});
    format!("{SIG_PREFIX}{}", b64(keep.to_string().as_bytes()))
}

fn decode_reasoning(sig: &str) -> Option<Value> {
    let raw = unb64(sig.strip_prefix(SIG_PREFIX)?)?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    // null/пусто — не рассуждения: такой элемент muse отвергнет (400 на каждом ходу).
    let id = v.get("id").filter(|x| x.as_str().is_some_and(|s| !s.is_empty()))?;
    let enc = v.get("encrypted_content").filter(|x| x.as_str().is_some_and(|s| !s.is_empty()))?;
    Some(json!({"type": "reasoning", "id": id, "encrypted_content": enc, "summary": []}))
}

/// Уровень размышления muse: max у него нет — берём xhigh; «как просит Claude» и незнакомое — high.
fn effort_for(cfg: &ProxyConfig) -> &'static str {
    match cfg.effort.as_str() {
        "low" => "low",
        "max" => "xhigh",
        _ => "high",
    }
}

/// Текстовый документ (`source.type == "text"`) передаём как есть — это просто текст; файлы (PDF, base64) — нет.
fn document_text(block: &Value) -> String {
    let src = &block["source"];
    match (src["type"].as_str(), src["data"].as_str()) {
        (Some("text"), Some(data)) if !data.is_empty() => match block["title"].as_str().filter(|t| !t.trim().is_empty()) {
            Some(title) => format!("Документ «{title}»:\n{data}"),
            None => data.to_string(),
        },
        _ => "[документ не передан — этот маршрут не принимает файлы]".to_string(),
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[изображение не передано]".to_string()),
                // Молча пустой вывод модель принимала за «файл пуст» и сочиняла содержание.
                Some("document") => Some(document_text(p)),
                // Ответ ToolSearch: ссылки на подключённые инструменты — иначе muse видел пустоту и искал по кругу.
                Some("tool_reference") => p.get("tool_name").and_then(Value::as_str).map(|n| format!("Инструмент {n} подключён — его можно вызывать.")),
                // Прочие блоки (search_result и будущие): текст, если он есть, иначе честная заглушка —
                // пропуск модель читала как «инструмент ничего не вернул» и дорисовывала вывод.
                Some(other) => Some(unknown_block_text(p, other)),
                None => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Текст неизвестного блока: его `text` или вложенный `content`, а без них — заглушка с типом.
fn unknown_block_text(block: &Value, kind: &str) -> String {
    let text = block
        .get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .or_else(|| block.get("content").map(text_of))
        .filter(|t| !t.trim().is_empty());
    text.unwrap_or_else(|| format!("[блок «{}» не передан]", kind.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(40).collect::<String>()))
}

fn user_part(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(json!({"type": "input_text", "text": block.get("text")?.as_str().filter(|t| !t.is_empty())?})),
        "image" => {
            // Замыкание: `?` внутри не должен выходить из всей user_part мимо заглушки.
            (|| {
                let src = block.get("source")?;
                match src.get("type").and_then(Value::as_str)? {
                    "base64" => Some(json!({"type": "input_image",
                        "image_url": format!("data:{};base64,{}", src.get("media_type")?.as_str()?, src.get("data")?.as_str()?)})),
                    "url" => Some(json!({"type": "input_image", "image_url": src.get("url").filter(|u| u.as_str().is_some_and(|u| !u.is_empty()))?})),
                    _ => None,
                }
            })()
            // Битая картинка — заглушкой, а не молчаливой дырой в ходе пользователя.
            .or(Some(json!({"type": "input_text", "text": "[картинка не передана — неполные данные]"})))
        }
        // Иначе сообщение из одного файла выпадало из input целиком, без следа.
        "document" => Some(json!({"type": "input_text", "text": document_text(block)})),
        // Мысли в реплике пользователя модели не нужны — заглушка читалась бы как часть задания.
        "thinking" | "redacted_thinking" => None,
        // Незнакомый блок — текстом или заглушкой, а не дырой в ходе пользователя.
        other => Some(json!({"type": "input_text", "text": unknown_block_text(block, other)})),
    }
}

/// Запрос Claude Messages → запрос OpenAI Responses.
pub fn to_responses(cfg: &ProxyConfig, body: &Value) -> Value {
    let instructions = match body.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    };
    let mut input: Vec<Value> = Vec::new();
    for msg in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let mut blocks: Vec<Value> = match msg.get("content") {
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            Some(Value::Array(a)) => a.clone(),
            _ => vec![],
        };
        // Как на родном пути: результаты инструментов вперёд, иначе текст встаёт между
        // function_call и его function_call_output.
        if role == "user" {
            blocks.sort_by_key(|b| b.get("type").and_then(Value::as_str) != Some("tool_result"));
        }
        // Соседние обычные части копим в одно сообщение; вызовы/результаты — отдельными элементами.
        let mut parts: Vec<Value> = Vec::new();
        // Результаты без своего вызова — текстом, но после всех function_call_output: иначе встали бы между ними.
        let mut orphans: Vec<Value> = Vec::new();
        let flush = |parts: &mut Vec<Value>, input: &mut Vec<Value>| {
            if !parts.is_empty() {
                input.push(json!({"role": role, "content": std::mem::take(parts)}));
            }
        };
        for b in &blocks {
            let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
            if role == "assistant" {
                match kind {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            parts.push(json!({"type": "output_text", "text": t}));
                        }
                    }
                    "tool_use" => {
                        flush(&mut parts, &mut input);
                        // Без id — свой, иначе "call_id": null роняет весь запрос 400.
                        let call_id = b.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("call_subbar_in_{}", input.len()));
                        // Не-объект не подменяем пустыми аргументами: модель поверила бы в вызов без параметров.
                        let arguments = match b.get("input") {
                            Some(v) if v.is_object() => v.to_string(),
                            None | Some(Value::Null) => "{}".into(),
                            Some(v) => json!({"input": v}).to_string(),
                        };
                        input.push(json!({"type": "function_call", "call_id": call_id, "name": b.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("unknown_tool"), "arguments": arguments}));
                    }
                    "thinking" => {
                        flush(&mut parts, &mut input);
                        if let Some(r) = b.get("signature").and_then(Value::as_str).and_then(decode_reasoning) {
                            input.push(r);
                        }
                    }
                    _ => {} // redacted_thinking и прочее Claude-специфичное — не для muse
                }
            } else if kind == "tool_result" {
                flush(&mut parts, &mut input);
                let mut out = text_of(b.get("content").unwrap_or(&Value::Null));
                if b.get("is_error").and_then(Value::as_bool) == Some(true) {
                    out = if out.trim().is_empty() { "Ошибка инструмента (без текста)".to_string() } else { format!("Ошибка:\n{out}") };
                } else if out.trim().is_empty() {
                    // Пустой вывод модель принимала за «ничего не сказал» и дорисовывала содержимое.
                    out = "(инструмент выполнен, вывода нет)".to_string();
                }
                match b.get("tool_use_id").and_then(Value::as_str) {
                    Some(id) if input.iter().any(|i| i["type"] == "function_call" && i["call_id"] == id) => {
                        input.push(json!({"type": "function_call_output", "call_id": id, "output": out}))
                    }
                    // Не к чему привязать (нет id или вызов срезан компактом) — текстом, а не висячим call_id (400 на весь запрос).
                    _ => orphans.push(json!({"type": "input_text", "text": format!("Результат инструмента:\n{out}")})),
                }
            } else if let Some(p) = user_part(b) {
                // Первый обычный блок после результатов: сироты — до него, в том же порядке, что пришли.
                parts.append(&mut orphans);
                parts.push(p);
            }
        }
        parts.append(&mut orphans);
        flush(&mut parts, &mut input);
    }
    // Отложенные инструменты (defer_loading) Claude Code подключает через ToolSearch: его ответ — ссылки
    // tool_reference. Для muse подключённые — это те, на которые сослались в разговоре; их и отдаём.
    let referenced: std::collections::HashSet<String> = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool_reference"))
        .filter_map(|p| p.get("tool_name").and_then(Value::as_str).map(str::to_string))
        .collect();
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| {
            let deferred = t.get("defer_loading").and_then(Value::as_bool) == Some(true);
            let loaded = t.get("name").and_then(Value::as_str).is_some_and(|n| referenced.contains(n));
            // Без имени — 400 на весь запрос («missing name»), а позвать такой инструмент всё равно нельзя.
            let named = t.get("name").and_then(Value::as_str).is_some_and(|n| !n.is_empty());
            named && t.get("input_schema").is_some() && (!deferred || loaded)
        })
        .map(|t| {
            // Как на родном пути того же шлюза: регулярка в схеме (поле pattern) даёт 400 — вырезаем все.
            let mut schema = t.get("input_schema").cloned().unwrap_or(Value::Null);
            super::route::strip_pattern(&mut schema);
            // Как на родном пути: у MCP бывает `{}` или одни properties без type — это 400 на весь ход.
            if !schema.is_object() {
                schema = json!({"type": "object"});
            } else if !schema.get("type").is_some_and(|t| t.is_string() || t.is_array()) {
                schema["type"] = json!("object");
            }
            json!({"type": "function", "name": t.get("name"), "description": t.get("description").and_then(Value::as_str).unwrap_or(""), "parameters": schema})
        })
        .collect();
    let mut out = Map::new();
    out.insert("model".into(), json!(cfg.model));
    out.insert("stream".into(), json!(true));
    out.insert("store".into(), json!(false));
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    out.insert("reasoning".into(), json!({"effort": effort_for(cfg), "summary": "auto"}));
    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions));
    }
    out.insert("input".into(), Value::Array(input));
    if !tools.is_empty() {
        let choice = match body.get("tool_choice").and_then(|c| c.get("type")).and_then(Value::as_str) {
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            // Инструмент выкинут фильтром (заглушка, без схемы) — не ссылаемся на него, иначе 400.
            Some("tool") if tools.iter().any(|t| t["name"] == body["tool_choice"]["name"]) => {
                json!({"type": "function", "name": body["tool_choice"].get("name")})
            }
            // Запрошенного инструмента нет — «любой» был бы подменой просьбы: пусть модель решает сама.
            Some("tool") => json!("auto"),
            _ => json!("auto"),
        };
        out.insert("tools".into(), Value::Array(tools));
        out.insert("tool_choice".into(), choice);
        if body["tool_choice"]["disable_parallel_tool_use"] == true {
            out.insert("parallel_tool_calls".into(), json!(false));
        }
    }
    // И у Anthropic, и у Responses потолок общий на мысли и текст: budget_tokens уже внутри max_tokens.
    if let Some(n) = body.get("max_tokens").and_then(Value::as_u64).filter(|&n| n > 0) {
        out.insert("max_output_tokens".into(), json!(n));
    }
    Value::Object(out)
}

/// Кусок SSE в формате Claude.
fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Thinking,
    Text,
    Tool,
}

/// Конечный автомат: события Responses → события Claude Messages.
pub struct StreamConv {
    model: String,
    started: bool,
    finished: bool,
    next_index: usize,
    open: Vec<Block>,
    saw_tool: bool,
    /// Был ли в ответе хоть какой-то смысл: непустой текст, мысли или вызов инструмента.
    said: bool,
    /// Модель ответила отказом (refusal-часть) — ход закончится stop_reason refusal.
    refusal: bool,
    /// Уже закрытые элементы: повтор их с начала (реконнект после output_item.done) — не новый блок.
    closed: Vec<u64>,
}

/// Открытый блок: элемент Responses → блок Claude.
struct Block {
    out_idx: u64,
    idx: usize,
    kind: Kind,
    /// Текст/мысли — что уже отдали клиенту; инструмент — накопленные аргументы (отдаём целиком в конце).
    got: String,
    /// Провайдер начал элемент заново (реконнект): сколько байт повтора уже сверили с `got`.
    replay: Option<usize>,
    /// Повтор разошёлся с отданным: дальше этот элемент молчит, чтобы не клеить второй вариант к первому.
    dead: bool,
}

impl StreamConv {
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn new(model: &str) -> Self {
        Self { model: model.to_string(), started: false, finished: false, next_index: 0, open: vec![], saw_tool: false, said: false, refusal: false, closed: vec![] }
    }

    fn start(&mut self, id: &str) -> String {
        if self.started {
            return String::new();
        }
        self.started = true;
        sse("message_start", json!({"type": "message_start", "message": {
            "id": format!("msg_{}", id.trim_start_matches("resp_")), "type": "message", "role": "assistant",
            "model": self.model, "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}}}))
    }

    fn block(&mut self, out_idx: u64) -> Option<&mut Block> {
        self.open.iter_mut().find(|b| b.out_idx == out_idx)
    }

    /// Одно событие Responses → ноль или больше событий Claude.
    pub fn feed(&mut self, ev: &Value) -> String {
        // После message_stop / ошибки — ничего: иначе клиент получит события после конца сообщения.
        if self.finished {
            return String::new();
        }
        let t = ev.get("type").and_then(Value::as_str).unwrap_or("");
        // Без номера — отдельная «ничейная» позиция: иначе чужие дельты сольются в первый элемент хода.
        let out_idx = ev.get("output_index").and_then(Value::as_u64).unwrap_or(u64::MAX);
        let mut out = String::new();
        match t {
            "response.created" | "response.in_progress" => {
                let id = ev["response"]["id"].as_str().unwrap_or("subbar").to_string();
                out += &self.start(&id);
            }
            "response.output_item.added" => {
                // Повтор того же элемента (реконнект у провайдера): второй блок не открываем,
                // а повторные дельты сверяем с уже отданным — клиент не получит текст дважды.
                if let Some(b) = self.block(out_idx) {
                    if b.kind == Kind::Tool {
                        b.got.clear();
                    } else {
                        b.replay = Some(0);
                    }
                    return out;
                }
                if self.closed.contains(&out_idx) {
                    return out;
                }
                let item = &ev["item"];
                let (kind, block) = match item["type"].as_str().unwrap_or("") {
                    "reasoning" => (Kind::Thinking, json!({"type": "thinking", "thinking": "", "signature": ""})),
                    "function_call" => {
                        // Без call_id клиент отвергнет блок (id должен быть строкой) — даём свой.
                        // Номер блока, а не out_idx: у двух вызовов без output_index он один (u64::MAX), и id совпали бы.
                        let id = item["call_id"].as_str().filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| format!("call_subbar_{}", self.next_index));
                        // Без имени клиент позвал бы инструмент «null» — честная ошибка лучше.
                        let Some(name) = item["name"].as_str().filter(|s| !s.is_empty()) else {
                            out.push_str(&self.fail("muse прислал вызов инструмента без имени"));
                            return out;
                        };
                        (Kind::Tool, json!({"type": "tool_use", "id": id, "name": name, "input": {}}))
                    }
                    "message" => (Kind::Text, json!({"type": "text", "text": ""})),
                    // Поиск, код и прочие элементы, которых у Claude нет, — пропускаем, а не открываем пустой текст.
                    _ => return out,
                };
                out += &self.start("subbar");
                if kind == Kind::Tool {
                    self.saw_tool = true;
                    self.said = true;
                }
                let idx = self.next_index;
                self.next_index += 1;
                self.open.push(Block { out_idx, idx, kind, got: String::new(), replay: None, dead: false });
                out += &sse("content_block_start", json!({"type": "content_block_start", "index": idx, "content_block": block}));
            }
            "response.output_text.delta" | "response.refusal.delta" | "response.reasoning_summary_text.delta" | "response.function_call_arguments.delta" => {
                let delta = ev["delta"].as_str().unwrap_or("");
                if delta.is_empty() {
                    return out;
                }
                // Отказ — только по дельте живого текстового блока: чужой индекс не должен перекрашивать весь ход.
                if t == "response.refusal.delta" && self.open.iter().any(|b| b.out_idx == out_idx && !b.dead && b.kind == Kind::Text) {
                    self.refusal = true;
                }
                let Some(b) = self.block(out_idx) else { return out };
                if b.dead {
                    return out;
                }
                // Дельта не своего вида (чужой/пропавший output_index) — не смешиваем JSON с текстом.
                let want = match t {
                    "response.function_call_arguments.delta" => Kind::Tool,
                    "response.reasoning_summary_text.delta" => Kind::Thinking,
                    _ => Kind::Text,
                };
                if b.kind != want {
                    return out;
                }
                if b.kind == Kind::Tool {
                    // Аргументы копим: отдадим целиком и проверенными на output_item.done.
                    b.got.push_str(delta);
                    return out;
                }
                let mut fresh = delta;
                if let Some(pos) = b.replay {
                    let seen = &b.got[pos..];
                    let same = seen.bytes().zip(fresh.bytes()).take_while(|(a, c)| a == c).count();
                    let same = (0..=same).rev().find(|&n| fresh.is_char_boundary(n)).unwrap_or_default(); // 0 — всегда граница
                    if same < seen.len() && same < fresh.len() {
                        // Повтор разошёлся с отданным: второй вариант к первому не клеим, элемент молчит до конца.
                        b.dead = true;
                        return out;
                    } else {
                        b.replay = if same == seen.len() { None } else { Some(pos + same) };
                    }
                    fresh = &fresh[same..];
                    if fresh.is_empty() {
                        return out;
                    }
                }
                b.got.push_str(fresh);
                let (idx, kind) = (b.idx, b.kind);
                let d = match kind {
                    Kind::Thinking => json!({"type": "thinking_delta", "thinking": fresh}),
                    _ => json!({"type": "text_delta", "text": fresh}),
                };
                if kind == Kind::Text && !fresh.trim().is_empty() {
                    self.said = true;
                }
                out += &sse("content_block_delta", json!({"type": "content_block_delta", "index": idx, "delta": d}));
            }
            "response.output_item.done" => {
                let Some(pos) = self.open.iter().position(|b| b.out_idx == out_idx) else { return out };
                let b = self.open.remove(pos);
                self.closed.push(out_idx);
                let item = &ev["item"];
                match b.kind {
                    Kind::Thinking => {
                        // Дельты рассуждений потерялись (или шлюз шлёт только итог) — берём текст из summary.
                        let summary: String = item["summary"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n\n");
                        if b.got.is_empty() && !summary.is_empty() {
                            out += &sse("content_block_delta", json!({"type": "content_block_delta", "index": b.idx, "delta": {"type": "thinking_delta", "thinking": summary}}));
                        }
                        // Подпись шлём всегда: мысль с пустой подписью клиент вернёт, и Anthropic ответит 400.
                        // Без encrypted_content наша подпись на приёме просто отбросится (decode_reasoning).
                        let sig = encode_reasoning(item);
                        out += &sse("content_block_delta", json!({"type": "content_block_delta", "index": b.idx, "delta": {"type": "signature_delta", "signature": sig}}));
                        // Ход из одних мыслей — тоже ответ, а не «пустой»; но мысль без единого слова — не ответ.
                        self.said |= !b.dead && (!b.got.trim().is_empty() || !summary.trim().is_empty());
                    }
                    Kind::Tool => {
                        // Итоговые аргументы надёжнее дельт; объект тоже бывает — не терять его в «{}».
                        let args = match &item["arguments"] {
                            Value::String(s) if !s.trim().is_empty() => s.clone(),
                            Value::String(_) | Value::Null if !b.got.trim().is_empty() => b.got.clone(),
                            Value::String(_) | Value::Null => "{}".to_string(),
                            other => other.to_string(),
                        };
                        if !serde_json::from_str::<Value>(&args).is_ok_and(|v| v.is_object()) {
                            out += &sse("content_block_stop", json!({"type": "content_block_stop", "index": b.idx}));
                            return out + &self.fail("muse прислал битые аргументы инструмента");
                        }
                        out += &sse("content_block_delta", json!({"type": "content_block_delta", "index": b.idx, "delta": {"type": "input_json_delta", "partial_json": args}}));
                    }
                    Kind::Text => {
                        // Итог элемента полнее дельт (часть потерялась) — дописываем недостающий хвост.
                        // Отказ модели — тоже ответ (иначе «пустой ход»).
                        self.refusal |= item["content"].as_array().into_iter().flatten().any(|c| c["type"] == "refusal");
                        let full: String = item["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str().or(c["refusal"].as_str())).collect();
                        if !b.dead {
                            let text = full.strip_prefix(b.got.as_str()).unwrap_or("");
                            if !text.is_empty() {
                                self.said |= !text.trim().is_empty();
                                out += &sse("content_block_delta", json!({"type": "content_block_delta", "index": b.idx, "delta": {"type": "text_delta", "text": text}}));
                            }
                        }
                    }
                }
                out += &sse("content_block_stop", json!({"type": "content_block_stop", "index": b.idx}));
            }
            "response.completed" | "response.incomplete" => {
                out += &self.finish_with(&ev["response"]);
            }
            "response.function_call_arguments.done" => {
                // Шлюз может отдать аргументы только здесь — без них инструмент ушёл бы с «{}».
                // Это полные аргументы: они надёжнее накопленных дельт (клиенту те ещё не отданы).
                let args = ev["arguments"].as_str().unwrap_or("");
                if let Some(b) = self.block(out_idx) {
                    if b.kind == Kind::Tool && !b.dead && !args.trim().is_empty() {
                        b.got = args.to_string();
                    }
                }
            }
            "response.failed" | "error" => {
                let msg = ev["response"]["error"]["message"].as_str().or(ev["message"].as_str()).or(ev["error"]["message"].as_str()).unwrap_or("ошибка muse");
                // Сбой шлюза посреди потока — повторяемый, как и наши собственные обрывы (fail).
                out += &sse("error", json!({"type": "error", "error": {"type": "overloaded_error", "message": format!("SubBar: muse: {msg}")}}));
                self.finished = true;
            }
            _ => {}
        }
        out
    }

    fn finish_with(&mut self, resp: &Value) -> String {
        if self.finished {
            return String::new();
        }
        // События элементов не дошли (битый кадр, шлюз шлёт только итог), а готовый ответ лежит
        // в response.output — проигрываем его как добавление и завершение элементов.
        let mut out = String::new();
        // Вызов инструмента начат, а done потерян: аргументы клиенту ещё не отданы — дописать из итога,
        // иначе вместо целого вызова вышла бы ошибка и повтор всего хода.
        // Обрезанный ответ (лимит, фильтр) — аргументы в итоге тоже обрезаны: не дописывать, пусть finish_checked откажет.
        let whole = resp["status"] == "completed";
        let open_tools: Vec<u64> = if !whole { vec![] } else { self.open.iter().filter(|b| b.kind == Kind::Tool && !b.dead).map(|b| b.out_idx).collect() };
        for at in open_tools {
            let item = resp["output"].as_array().and_then(|o| o.get(at as usize)).filter(|i| i["type"] == "function_call");
            if let Some(item) = item {
                out += &self.feed(&json!({"type": "response.output_item.done", "output_index": at, "item": item}));
            }
        }
        if self.next_index == 0 && self.open.is_empty() {
            for (i, item) in resp["output"].as_array().into_iter().flatten().enumerate() {
                let at = RECOVERED_BASE + i as u64;
                out += &self.feed(&json!({"type": "response.output_item.added", "output_index": at, "item": item}));
                out += &self.feed(&json!({"type": "response.output_item.done", "output_index": at, "item": item}));
                if self.finished {
                    return out;
                }
            }
        }
        out + &self.finish_checked(resp)
    }

    fn finish_checked(&mut self, resp: &Value) -> String {
        // Уже отдали event: error (например, битые аргументы при восстановлении) — message_stop после него нельзя.
        if self.finished {
            return String::new();
        }
        // Обрыв по фильтру и т.п. (не лимит токенов) или вызов инструмента без завершения —
        // ответ неполный: честная ошибка, а не «целый» ход или инструмент с пустыми аргументами.
        // Фильтр содержимого — это отказ (stop_reason refusal, как у Anthropic), а не сбой: повтор даст то же.
        let refused = self.refusal || (resp["status"] == "incomplete" && resp["incomplete_details"]["reason"] == "content_filter");
        let incomplete_other = !refused && resp["status"] == "incomplete" && resp["incomplete_details"]["reason"] != "max_output_tokens";
        // Незакрытый вызов инструмента — в любом случае (и при лимите токенов и отказе) аргументы обрезаны:
        // tool_use с пустым input клиент исполнил бы.
        let open_tool = self.open.iter().any(|b| b.kind == Kind::Tool);
        // Ни текста, ни вызова инструмента — «модель промолчала» не выдаём за ответ: это потерянные события.
        // Но если ответ обрезан лимитом токенов (все ушли на размышления) — это честный max_tokens, как у Claude.
        let cut_by_limit = resp["status"] == "incomplete" && resp["incomplete_details"]["reason"] == "max_output_tokens";
        // Диагностика шлюза — раньше «пустого ответа»: при failed пустой ход и есть следствие, а причина — в ней.
        // «completed» с чужим статусом (failed/cancelled) — сбой шлюза, а не законченный ход.
        if let Some(st) = resp["status"].as_str().filter(|s| !matches!(*s, "completed" | "incomplete")) {
            let msg = resp["error"]["message"].as_str().unwrap_or("");
            return self.fail(&format!("ответ muse не завершён ({st}){}", if msg.is_empty() { String::new() } else { format!(": {msg}") }));
        }
        if incomplete_other || open_tool {
            let why = if open_tool { "muse не дописал вызов инструмента".to_string() } else { format!("ответ muse оборван ({})", resp["incomplete_details"]["reason"].as_str().unwrap_or("причина не названа")) };
            return self.fail(&why);
        }
        // Мысль, уже отданная клиенту без output_item.done, — тоже ответ: иначе токены сожжены, а ход «пустой».
        let thought = self.open.iter().any(|b| b.kind == Kind::Thinking && !b.got.trim().is_empty());
        if !self.said && !thought && !cut_by_limit && !refused {
            return self.fail("muse вернул пустой ответ");
        }
        self.finished = true;
        let mut out = self.start("subbar");
        for b in std::mem::take(&mut self.open) {
            // Мысль без подписи: следующий ход её потеряет; пустая подпись отбросится на приёме (decode_reasoning).
            if b.kind == Kind::Thinking {
                out += &sse("content_block_delta", json!({"type": "content_block_delta", "index": b.idx, "delta": {"type": "signature_delta", "signature": encode_reasoning(&Value::Null)}}));
            }
            out += &sse("content_block_stop", json!({"type": "content_block_stop", "index": b.idx}));
        }
        // Упёрлись в лимит токенов — max_tokens, даже если начали вызов инструмента: его аргументы обрезаны.
        let stop = if resp["status"] == "incomplete" && resp["incomplete_details"]["reason"] == "max_output_tokens" {
            "max_tokens"
        } else if refused {
            "refusal"
        } else if self.saw_tool {
            "tool_use"
        } else {
            "end_turn"
        };
        out += &sse("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": usage_of(&resp["usage"])}));
        out += &sse("message_stop", json!({"type": "message_stop"}));
        out
    }

    /// Поток не дал целого хода (оборвался, пустой или негодный ответ) — ошибка в формате Claude
    /// (Claude Code повторит запрос), а не обрезанный ответ под видом целого.
    pub fn fail(&mut self, why: &str) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        sse("error", json!({"type": "error", "error": {"type": "overloaded_error", "message": format!("SubBar: {why}")}}))
    }
}

/// Ошибка внутри потока Claude (для не-потокового ответа: её лучше отдать откату, чем клиенту).
pub fn stream_error(anthropic_sse: &str) -> Option<String> {
    anthropic_sse
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(|ev| ev["type"] == "error")
        // Без нашего префикса: api_error добавит свой, иначе выходило «SubBar: SubBar: …».
        .map(|ev| ev["error"]["message"].as_str().unwrap_or("ошибка muse").trim_start_matches("SubBar: ").to_string())
}

/// Токены Responses → Claude: у OpenAI `input_tokens` включает кэш, у Claude кэш считается отдельно.
fn usage_of(u: &Value) -> Value {
    let total = u["input_tokens"].as_u64().unwrap_or(0);
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(total);
    json!({"input_tokens": total - cached, "cache_read_input_tokens": cached, "cache_creation_input_tokens": 0,
           "output_tokens": u["output_tokens"].as_u64().unwrap_or(0)})
}

/// Номера элементов, восстановленных из response.output: вдали от настоящих output_index.
const RECOVERED_BASE: u64 = 1 << 40;

/// Разбор SSE-строк Responses: копит хвост между кусками, отдаёт JSON событий.
/// Копит байты, а не текст: кусок может оборваться посреди буквы (кириллица — 2 байта).
#[derive(Default)]
pub struct SseLines {
    buf: Vec<u8>,
    /// Последний кусок нёс только пульс: пустые строки и комментарии «: …», без данных (и без начала данных).
    pulse: bool,
}

impl SseLines {
    /// Был ли последний кусок одним пульсом шлюза — не признаком жизни модели.
    pub fn pulse_only(&self) -> bool {
        self.pulse
    }

    /// Остаток без завершающего перевода строки (поток кончился сразу после `data: …`).
    pub fn finish(&mut self) -> Vec<Value> {
        // Остаток может быть и из нескольких строк (`event: …` + `data: …`) — дописать перевод строки и разобрать.
        if self.buf.is_empty() {
            return vec![];
        }
        self.push(b"\n")
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.buf.extend_from_slice(chunk);
        let mut out = vec![];
        let mut start = 0;
        let mut meaningful = false;
        while let Some(n) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[start..start + n]);
            let line = line.trim_end_matches('\r');
            if line.starts_with("data:") {
                meaningful = true;
            }
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    out.push(v);
                }
            }
            start += n + 1;
        }
        self.buf.drain(..start);
        // Строка без конца длиннее 16 МБ — не событие, а сломанный поток: не копить его до конца запроса.
        // Молча не глотаем: событие потеряно — честная ошибка, Claude Code повторит ход.
        if self.buf.len() > 16 << 20 {
            self.buf.clear();
            out.push(json!({"type": "error", "message": "событие больше 16 МБ — поток сломан"}));
            meaningful = true;
        }
        // Недособранная строка — если это данные, а не комментарий, это тоже жизнь (идёт длинное событие).
        if !self.buf.is_empty() && !self.buf.starts_with(b":") {
            meaningful = true;
        }
        self.pulse = !meaningful;
        out
    }
}

/// Не-потоковый ответ Claude из событий (если клиент попросил stream:false).
pub fn aggregate(anthropic_sse: &str) -> Value {
    let mut msg = json!({});
    let mut blocks: Vec<Value> = vec![];
    let mut json_bufs: Vec<String> = vec![];
    for line in anthropic_sse.lines() {
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => msg = ev["message"].clone(),
            "content_block_start" => {
                blocks.push(ev["content_block"].clone());
                json_bufs.push(String::new());
            }
            "content_block_delta" => {
                let i = ev["index"].as_u64().unwrap_or(0) as usize;
                let (Some(b), Some(jb)) = (blocks.get_mut(i), json_bufs.get_mut(i)) else { continue };
                let d = &ev["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => b["text"] = json!(format!("{}{}", b["text"].as_str().unwrap_or(""), d["text"].as_str().unwrap_or(""))),
                    "thinking_delta" => b["thinking"] = json!(format!("{}{}", b["thinking"].as_str().unwrap_or(""), d["thinking"].as_str().unwrap_or(""))),
                    "signature_delta" => b["signature"] = d["signature"].clone(),
                    "input_json_delta" => jb.push_str(d["partial_json"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            "message_delta" => {
                msg["stop_reason"] = ev["delta"]["stop_reason"].clone();
                msg["usage"] = ev["usage"].clone();
            }
            _ => {}
        }
    }
    for (b, jb) in blocks.iter_mut().zip(json_bufs) {
        if b["type"] == "tool_use" {
            b["input"] = serde_json::from_str(&jb).unwrap_or(json!({}));
        }
    }
    msg["content"] = Value::Array(blocks);
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ProxyConfig {
        ProxyConfig { model: "muse-spark-1.3-contributor".into(), api_key: "k".into(), ..Default::default() }
    }

    #[test]
    fn текстовый_документ_доходит_текстом() {
        let doc = json!({"type": "document", "title": "notes", "source": {"type": "text", "media_type": "text/plain", "data": "строка"}});
        assert_eq!(document_text(&doc), "Документ «notes»:\nстрока");
        let pdf = json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBE"}});
        assert!(document_text(&pdf).starts_with("[документ не передан"));
    }

    #[test]
    fn base64_туда_и_обратно() {
        for s in ["", "a", "ab", "abc", "Привет, muse!", "{\"id\":\"rs_1\"}"] {
            assert_eq!(unb64(&b64(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }

    #[test]
    fn запрос_claude_в_responses() {
        let body = json!({
            "model": "claude-haiku-4-5", "max_tokens": 1000,
            "system": [{"type": "text", "text": "A", "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": "B"}],
            "tools": [{"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
                      {"name": "Placeholder", "defer_loading": true, "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "посчитай"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "чужое", "signature": "anthropic-sig"},
                    {"type": "thinking", "thinking": "", "signature": encode_reasoning(&json!({"id": "rs_1", "encrypted_content": "ENC"}))},
                    {"type": "text", "text": "сейчас"},
                    {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "ls"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": [{"type": "text", "text": "7"}]},
                    {"type": "text", "text": "дальше"}]}
            ]
        });
        let r = to_responses(&ProxyConfig { effort: "max".into(), ..cfg() }, &body);
        assert_eq!(r["model"], "muse-spark-1.3-contributor");
        assert_eq!(r["instructions"], "A\n\nB");
        assert_eq!(r["reasoning"]["effort"], "xhigh", "max у muse нет");
        assert_eq!(r["max_output_tokens"], 1000);
        assert_eq!(r["tools"].as_array().unwrap().len(), 1, "заглушка с defer_loading — прочь");
        let input = r["input"].as_array().unwrap();
        let kinds: Vec<String> = input.iter().map(|i| i["type"].as_str().or(i["role"].as_str()).unwrap().to_string()).collect();
        assert_eq!(kinds, ["user", "reasoning", "assistant", "function_call", "function_call_output", "user"]);
        assert_eq!(input[1]["encrypted_content"], "ENC", "свои рассуждения muse вернулись; чужие — нет");
        assert_eq!(input[3]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(input[4]["output"], "7");
    }

    #[test]
    fn настоящий_поток_muse_в_поток_claude() {
        let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/muse-tool-call.sse")).unwrap();
        let mut lines = SseLines::default();
        let mut conv = StreamConv::new("claude-haiku-4-5");
        let mut out = String::new();
        for chunk in raw.chunks(97) {
            for ev in lines.push(chunk) {
                out += &conv.feed(&ev);
            }
        }
        assert_eq!(conv.fail("не должно понадобиться"), "", "поток завершён — ошибки нет");
        let msg = aggregate(&out);
        assert_eq!(msg["stop_reason"], "tool_use");
        let content = msg["content"].as_array().unwrap();
        let tool = content.iter().find(|b| b["type"] == "tool_use").expect("вызов инструмента");
        assert_eq!(tool["name"], "count");
        assert_eq!(tool["input"], json!({"dir": "/tmp"}));
        assert!(tool["id"].as_str().unwrap().starts_with("call_"));
        let thinking: Vec<_> = content.iter().filter(|b| b["type"] == "thinking").collect();
        assert_eq!(thinking.len(), 2);
        assert!(thinking[0]["signature"].as_str().unwrap().starts_with(SIG_PREFIX));
        assert!(decode_reasoning(thinking[0]["signature"].as_str().unwrap()).unwrap()["encrypted_content"].as_str().unwrap().len() > 20);
        assert_eq!(out.matches("event: message_stop").count(), 1);
    }

    #[test]
    fn буква_разрезанная_между_кусками_цела() {
        let ev = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Привет\"}\n\n".as_bytes();
        let cut = ev.iter().position(|&b| b == 0xD0).unwrap() + 1; // посреди «П»
        let mut lines = SseLines::default();
        let mut got = lines.push(&ev[..cut]);
        got.extend(lines.push(&ev[cut..]));
        assert_eq!(got[0]["delta"], "Привет");
    }

    #[test]
    fn хвост_без_перевода_строки_разбирается_на_финише() {
        let mut lines = SseLines::default();
        assert!(lines.push(b"data: {\"type\":\"response.completed\"}").is_empty());
        assert_eq!(lines.finish()[0]["type"], "response.completed");
        let mut cut = SseLines::default();
        cut.push(b"data: {\"type\":\"resp");
        assert!(cut.finish().is_empty(), "оборванный JSON — не событие");
    }

    #[test]
    fn потерянный_done_инструмента_дописан_из_итога() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {},
            "output": [{"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{\"command\":\"ls\"}"}]}}));
        assert!(!out.contains("event: error"), "{out}");
        assert_eq!(aggregate(&out)["content"][0]["input"], json!({"command": "ls"}));
    }

    #[test]
    fn сбой_шлюза_посреди_потока_одна_ошибка() {
        for ev in [json!({"type": "response.failed", "response": {"error": {"message": "упал"}}}), json!({"type": "error", "message": "упал"})] {
            let mut conv = StreamConv::new("m");
            let out = conv.feed(&ev);
            assert_eq!(out.matches("event: error").count(), 1, "{out}");
            assert!(out.contains("SubBar: muse: упал"), "{out}");
            assert!(conv.finished);
        }
    }

    #[test]
    fn пустая_дельта_не_съедает_аргументы_инструмента() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}}));
        out += &conv.feed(&json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": ""}));
        out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0,
            "item": {"type": "function_call", "arguments": "{\"command\":\"ls\"}"}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        let msg = aggregate(&out);
        assert_eq!(msg["content"][0]["input"], json!({"command": "ls"}));
        assert!(!out.contains("\"partial_json\":\"\""), "пустых дельт клиенту не шлём");
    }

    #[test]
    fn ответ_без_событий_элементов_берётся_из_итога() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.created", "response": {"id": "resp_1"}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}, "output": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "думаю"}]},
            {"type": "message", "content": [{"type": "output_text", "text": "Готово"}]},
            {"type": "function_call", "call_id": "call_1", "name": "Bash", "arguments": "{\"command\":\"ls\"}"}]}}));
        let msg = aggregate(&out);
        assert!(stream_error(&out).is_none(), "{out}");
        assert_eq!(msg["content"][0]["thinking"], "думаю");
        assert_eq!(msg["content"][1]["text"], "Готово");
        assert_eq!(msg["content"][2]["input"], json!({"command": "ls"}));
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    #[test]
    fn реконнект_после_конца_элемента_не_дублирует_инструмент() {
        let mut conv = StreamConv::new("m");
        let mut out = String::new();
        for _ in 0..2 {
            out += &conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "call_a", "name": "Bash"}}));
            out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "arguments": "{}"}}));
        }
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        assert_eq!(aggregate(&out)["content"].as_array().unwrap().len(), 1, "{out}");
    }

    #[test]
    fn итог_текста_дописывает_потерянное_а_разошедшийся_повтор_молчит() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}));
        out += &conv.feed(&json!({"type": "response.output_text.delta", "output_index": 0, "delta": "При"}));
        out += &conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}));
        out += &conv.feed(&json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Пока"}));
        out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "content": [{"text": "Привет"}]}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        assert_eq!(aggregate(&out)["content"][0]["text"], "При", "каши нет: {out}");
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}));
        out += &conv.feed(&json!({"type": "response.output_text.delta", "output_index": 0, "delta": "При"}));
        out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "content": [{"text": "Привет"}]}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        assert_eq!(aggregate(&out)["content"][0]["text"], "Привет");
    }

    #[test]
    fn фильтр_это_отказ_а_без_call_id_свой_id() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "name": "Bash"}}));
        out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "arguments": "{}"}}));
        out += &conv.feed(&json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}, "usage": {}}}));
        let msg = aggregate(&out);
        assert_eq!(msg["stop_reason"], "refusal", "{out}");
        assert_eq!(msg["content"][0]["id"], "call_subbar_0");
    }

    #[test]
    fn кэш_muse_считается_как_у_claude() {
        let mut conv = StreamConv::new("m");
        conv.feed(&json!({"type": "response.created", "response": {"id": "resp_1"}}));
        conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}));
        conv.feed(&json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ok"}));
        let out = conv.feed(&json!({"type": "response.completed", "response": {"status": "completed",
            "usage": {"input_tokens": 8410, "input_tokens_details": {"cached_tokens": 8305}, "output_tokens": 48}}}));
        let msg = aggregate(&out);
        assert_eq!(msg["usage"], json!({"input_tokens": 105, "cache_read_input_tokens": 8305, "cache_creation_input_tokens": 0, "output_tokens": 48}));
    }

    #[test]
    fn пустой_ответ_и_оборванный_вызов_это_ошибка() {
        let mut conv = StreamConv::new("m");
        let out = conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        assert!(out.contains("event: error"), "ни одного блока — не ответ: {out}");
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "Bash"}}));
        out += &conv.feed(&json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"comm"}));
        out += &conv.feed(&json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}));
        assert!(out.contains("event: error") && !out.contains("message_stop"), "обрезанные аргументы не исполняем: {out}");
    }

    #[test]
    fn обрыв_без_completed_это_ошибка_а_не_обрезанный_ответ() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}));
        out += &conv.feed(&json!({"type": "response.output_text.delta", "output_index": 0, "delta": "при"}));
        out += &conv.fail("поток muse оборвался");
        assert!(!out.contains("message_stop"), "целым ответ не выдаём");
        assert!(out.ends_with("\n\n") && out.contains("event: error") && out.contains("overloaded_error"));
        assert_eq!(stream_error(&out).as_deref(), Some("поток muse оборвался"));
        assert_eq!(conv.fail("ещё раз"), "", "ошибка — одна");
    }

    #[test]
    fn инструменты_подключённые_через_toolsearch_доходят_до_muse() {
        let cfg = ProxyConfig { model: "muse-spark-1.3-contributor".into(), ..Default::default() };
        let body = json!({"model": "claude-haiku-4-5", "max_tokens": 100,
            "tools": [{"name": "ToolSearch", "input_schema": {"type": "object"}},
                      {"name": "TaskCreate", "defer_loading": true, "input_schema": {"type": "object"}},
                      {"name": "WebFetch", "defer_loading": true, "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "составь план"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "ToolSearch", "input": {"query": "task"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "tool_reference", "tool_name": "TaskCreate"}]}]}]});
        let r = to_responses(&cfg, &body);
        let names: Vec<&str> = r["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["ToolSearch", "TaskCreate"], "подключённый — есть, не подключённый — нет");
        let output = r["input"].as_array().unwrap().iter().find(|i| i["type"] == "function_call_output").unwrap();
        assert!(output["output"].as_str().unwrap().contains("TaskCreate подключён"), "ответ ToolSearch не пустой: {output}");
    }

    #[test]
    fn все_токены_на_размышления_это_max_tokens_а_не_ошибка() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning"}}));
        out += &conv.feed(&json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "думаю…"}));
        out += &conv.feed(&json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "usage": {}}}));
        assert!(!out.contains("event: error"), "{out}");
        assert_eq!(aggregate(&out)["stop_reason"], "max_tokens");
    }

    #[test]
    fn пульс_шлюза_по_строкам_а_не_по_кускам() {
        let mut lines = SseLines::default();
        assert!(lines.push(b": heartbeat\n\n").is_empty() && lines.pulse_only(), "одни комментарии — пульс");
        // Длинное событие, разрезанное так, что кусок начинается с «:» посреди JSON, — это данные, а не пульс.
        assert!(lines.push(b"data: {\"type\":\"response.completed\",\"text\"").is_empty());
        assert!(!lines.pulse_only(), "начало события — жизнь");
        assert!(lines.push(b":\"1 2 3\"").is_empty());
        assert!(!lines.pulse_only(), "кусок с «:» посреди строки — не комментарий");
        let events = lines.push(b"}\n\n");
        assert_eq!(events.len(), 1, "событие собралось целым");
        assert_eq!(events[0]["text"], "1 2 3");
    }

    #[test]
    fn пустой_текст_и_чужие_элементы_это_не_ответ() {
        let mut conv = StreamConv::new("m");
        let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "web_search_call"}}));
        out += &conv.feed(&json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message"}}));
        out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "content": [{"text": ""}]}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        assert!(out.contains("event: error") && !out.contains("message_stop"), "{out}");
        assert_eq!(out.matches("event: content_block_start").count(), 1, "поиск блока не открывает: {out}");
    }

    #[test]
    fn аргументы_из_done_важнее_дельт_а_битые_это_ошибка() {
        let call = |delta: &str, done: Value| {
            let mut conv = StreamConv::new("m");
            let mut out = conv.feed(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "Bash"}}));
            out += &conv.feed(&json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": delta}));
            out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "arguments": done}}));
            out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
            out
        };
        assert_eq!(aggregate(&call("{\"comm", json!("{\"command\":\"ls\"}")))["content"][0]["input"], json!({"command": "ls"}));
        assert_eq!(aggregate(&call("{\"a\":1}", Value::Null))["content"][0]["input"], json!({"a": 1}));
        let bad = call("{\"comm", Value::Null);
        assert!(bad.contains("event: error") && !bad.contains("message_stop"), "{bad}");
    }

    #[test]
    fn повтор_элемента_после_реконнекта_не_дублирует_текст() {
        let mut conv = StreamConv::new("m");
        let add = json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}});
        let d = |t: &str| json!({"type": "response.output_text.delta", "output_index": 0, "delta": t});
        let mut out = conv.feed(&add) + &conv.feed(&d("При")) + &conv.feed(&d("вет"));
        out += &conv.feed(&add);
        out += &conv.feed(&d("Привет, "));
        out += &conv.feed(&d("мир"));
        out += &conv.feed(&json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message"}}));
        out += &conv.feed(&json!({"type": "response.completed", "response": {"status": "completed", "usage": {}}}));
        assert_eq!(aggregate(&out)["content"][0]["text"], "Привет, мир");
    }
}
