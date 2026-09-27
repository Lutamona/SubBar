#!/usr/bin/env python3
"""Живые тесты подмены: настоящие запросы в OpenCode Go через отдельный экземпляр прокси.

Твою службу и настройки не трогает: свой прокси на свободном порту, свой конфиг (копия proxy.json с другим портом
и выбранным для тестов ключом), свой каталог-песочница. Ключи в вывод не попадают.

    python3 tests/live/live.py api      # протокол: прямые запросы в прокси по каждой модели
    python3 tests/live/live.py route    # маршрутизация и надёжность: основная модель, sonnet, откат, ротация, нагрузка
    python3 tests/live/live.py agent    # настоящий Claude Code: задачи с проверяемым итогом, субагенты
    python3 tests/live/live.py all
Флаги: --models deepseek,bunny,muse,haiku (haiku — настоящая haiku для сравнения), --only имя, --lanes N, --opus.
Итог — таблица в конце и JSON в --out (по умолчанию /tmp/subbar-live-<время>.json).
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures as cf
import hashlib
import io
import json
import os
import queue
import random
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "release", "subbar")
DATA = os.environ.get("SUBBAR_DATA_DIR") or os.path.expanduser("~/Library/Application Support/SubBar")
MODELS = {
    "deepseek": "deepseek-v4.1-flash",
    "bunny": "space-bunny-free",
    "muse": "muse-spark-1.3-contributor",
}
HAIKU = "claude-haiku-4-5"
PRINT_LOCK = threading.Lock()


def log(*parts):
    with PRINT_LOCK:
        print(*parts, flush=True)


# ─────────── прокси для тестов ───────────


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def best_key() -> tuple[str, str]:
    """Ключ OpenCode с наибольшим запасом из карточек: (подпись, ключ). Сам ключ наружу не печатается."""
    state = json.load(open(os.path.join(DATA, "state.json")))
    now = time.time() * 1000
    best = None
    for a in state["accounts"]:
        key = (a.get("credentials") or {}).get("apiKey", "").strip()
        if a.get("provider") != "open-code-go" or not a.get("enabled") or not key:
            continue
        windows = [w for w in (a.get("lastUsage") or {}).get("windows", []) if not w.get("resetsAt") or w["resetsAt"] > now]
        headroom = min((100 - w["usedPercent"] for w in windows), default=50)
        if best is None or headroom > best[0]:
            best = (headroom, a["label"], key)
    if best is None:
        sys.exit("нет карточек OpenCode Go с ключом")
    return best[1], best[2]


class TestProxy:
    """Свой экземпляр `subbar proxy` со своим конфигом. Конфиг прокси перечитывает на лету."""

    def __init__(self, name: str, **overrides):
        self.dir = tempfile.mkdtemp(prefix=f"subbar-live-{name}-")
        self.port = free_port()
        self.cfg_path = os.path.join(self.dir, "proxy.json")
        base = json.load(open(os.path.join(DATA, "proxy.json")))
        label, key = best_key()
        base.update({"port": self.port, "apiKey": key, "accountLabel": label, "enabled": True, "rotate": True, "fallback": True, "matchModels": "haiku", "requireTools": True})
        self.cfg = base
        self.set(**overrides)
        self.log_path = os.path.join(self.dir, "proxy.log")
        env = dict(os.environ, SUBBAR_PROXY_CONFIG=self.cfg_path)
        with open(self.log_path, "w") as log_file:  # у дочернего своя копия дескриптора — нашу закрываем сразу
            self.proc = subprocess.Popen([BIN, "proxy", "--config", self.cfg_path], stdout=log_file, stderr=subprocess.STDOUT, env=env)
        for _ in range(100):
            if self.status():
                return
            time.sleep(0.1)
        tail = open(self.log_path).read()[-500:]
        self.stop()  # не оставлять живой прокси и каталог с настоящим ключом
        raise RuntimeError(f"прокси {name} не поднялся: {tail}")

    def set(self, **overrides):
        self.cfg.update(overrides)
        tmp = self.cfg_path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(self.cfg, f)
        os.chmod(tmp, 0o600)
        os.replace(tmp, self.cfg_path)
        # Прокси сверяет время изменения файла — дать ему смениться наверняка.
        time.sleep(0.05)

    def status(self) -> dict | None:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/_subbar/status", timeout=2) as r:
                return json.load(r)
        except Exception:
            return None

    def stats(self) -> dict:
        s = (self.status() or {}).get("stats", {})
        return {k: s.get(k, 0) for k in ("sub", "fallback", "errors", "pass")}

    def stop(self):
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        shutil.rmtree(self.dir, ignore_errors=True)


# ─────────── запросы и разбор потока ───────────

NOOP_TOOL = {"name": "noop", "description": "Ничего не делает. Не вызывай.", "input_schema": {"type": "object", "properties": {}}}
WEATHER_TOOL = {
    "name": "get_weather",
    "description": "Погода в городе сейчас.",
    "input_schema": {"type": "object", "properties": {"city": {"type": "string", "description": "Город"}}, "required": ["city"]},
}


def request(port: int, body: dict, headers: dict | None = None, timeout: float = 300) -> tuple[int, dict | list, float]:
    """POST /v1/messages. Поток — список событий SSE [(event, data)], иначе JSON. (статус, ответ, секунды)."""
    data = json.dumps(body).encode()
    h = {"content-type": "application/json", "anthropic-version": "2023-06-01"}
    h.update(headers or {})
    req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/messages", data=data, headers=h, method="POST")
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            ctype = r.headers.get("content-type", "")
            if "event-stream" in ctype:
                return r.status, parse_sse(r), time.time() - t0
            return r.status, json.load(r), time.time() - t0
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw), time.time() - t0
        except json.JSONDecodeError:
            return e.code, {"raw": raw[:300]}, time.time() - t0


def parse_sse(stream) -> list:
    events, event = [], None
    for raw in stream:
        line = raw.decode("utf-8").rstrip("\r\n")
        if line.startswith("event:"):
            event = line[6:].strip()
        elif line.startswith("data:"):
            try:
                events.append((event, json.loads(line[5:].strip())))
            except json.JSONDecodeError:
                events.append((event, {"_bad": line[:200]}))
    return events


def assemble(events: list) -> tuple[dict, list[str]]:
    """Собрать сообщение из потока и проверить порядок событий по протоколу Anthropic. (сообщение, нарушения)."""
    problems: list[str] = []
    # request() при ошибке отдаёт разобранное тело, а не события: «HTTP-ответ без потока», а не ValueError распаковки.
    if not isinstance(events, list):
        return {}, [f"ответ без потока: {str(events)[:120]}"]
    real = [(e, d) for e, d in events if e != "ping"]
    if not real:
        return {}, ["пустой поток"]
    if real[0][0] != "message_start":
        problems.append(f"первое событие {real[0][0]}, а не message_start")
    msg = dict(real[0][1].get("message", {}))
    msg["content"] = []
    open_blocks: dict[int, dict] = {}
    partial: dict[int, str] = {}
    stop_reason, usage = None, {}
    seen_stop = False
    for i, (e, d) in enumerate(real[1:], 1):
        if seen_stop:
            problems.append(f"событие {e} после message_stop")
        if e == "error":
            problems.append(f"ошибка в потоке: {d.get('error', {}).get('message', d)}")
        elif e == "content_block_start":
            idx = d.get("index")
            if idx in open_blocks:
                problems.append(f"блок {idx} открыт дважды")
            block = dict(d.get("content_block", {}))
            open_blocks[idx] = block
            partial[idx] = ""
        elif e == "content_block_delta":
            idx, delta = d.get("index"), d.get("delta", {})
            block = open_blocks.get(idx)
            if block is None:
                problems.append(f"дельта в незакрытый/неоткрытый блок {idx}")
                continue
            kind = delta.get("type")
            if kind == "text_delta":
                block["text"] = block.get("text", "") + delta.get("text", "")
            elif kind == "thinking_delta":
                block["thinking"] = block.get("thinking", "") + delta.get("thinking", "")
            elif kind == "signature_delta":
                block["signature"] = block.get("signature", "") + delta.get("signature", "")
            elif kind == "input_json_delta":
                partial[idx] += delta.get("partial_json", "")
            else:
                problems.append(f"неизвестная дельта {kind}")
        elif e == "content_block_stop":
            idx = d.get("index")
            block = open_blocks.pop(idx, None)
            if block is None:
                problems.append(f"закрыт неоткрытый блок {idx}")
                continue
            if block.get("type") == "tool_use":
                raw = partial.get(idx, "") or "{}"
                try:
                    block["input"] = json.loads(raw)
                except json.JSONDecodeError:
                    problems.append(f"битые аргументы инструмента: {raw[:80]}")
            msg["content"].append(block)
        elif e == "message_delta":
            stop_reason = d.get("delta", {}).get("stop_reason")
            usage = d.get("usage", {})
        elif e == "message_stop":
            seen_stop = True
    if open_blocks:
        problems.append(f"незакрытые блоки {list(open_blocks)}")
    if not seen_stop:
        problems.append("нет message_stop")
    msg["stop_reason"] = stop_reason
    msg["usage"] = {**msg.get("usage", {}), **usage}
    return msg, problems


def text_of(msg: dict) -> str:
    return "".join(b.get("text", "") for b in msg.get("content", []) if b.get("type") == "text")


def tool_uses(msg: dict) -> list[dict]:
    return [b for b in msg.get("content", []) if b.get("type") == "tool_use"]


def body(content, *, tools=None, stream=True, max_tokens=2048, session="live", **extra) -> dict:
    messages = content if isinstance(content, list) else [{"role": "user", "content": content}]
    b = {"model": HAIKU, "max_tokens": max_tokens, "stream": stream, "messages": messages, "tools": tools or [NOOP_TOOL],
         "metadata": {"user_id": json.dumps({"session_id": f"subbar-live-{session}"})}}
    b.update(extra)
    return b


# ─────────── результаты ───────────


@dataclass
class Result:
    suite: str
    name: str
    model: str
    ok: bool
    secs: float
    note: str = ""
    extra: dict = field(default_factory=dict)


RESULTS: list[Result] = []


def record(r: Result):
    RESULTS.append(r)
    mark = "✓" if r.ok else "✗"
    log(f"  {mark} {r.suite}/{r.name} [{r.model}] {r.secs:.1f} с  {r.note}")


def run_case(suite: str, name: str, model: str, fn):
    t0 = time.time()
    try:
        ok, note, extra = fn()
    except Exception as e:  # сам тест упал — это тоже итог
        ok, note, extra = False, f"исключение: {type(e).__name__}: {e}", {}
    record(Result(suite, name, model, ok, time.time() - t0, note, extra))


# ─────────── 1. протокол ───────────


def api_cases(port: int, model_key: str):
    """Прямые запросы в прокси: всё, что делает субагент Claude Code, и края протокола."""
    m = model_key

    def simple_stream():
        status, events, secs = request(port, body("Ответь одним словом по-русски: готово"))
        msg, problems = assemble(events) if status == 200 else ({}, [f"HTTP {status}: {events}"])
        text = text_of(msg)
        ok = not problems and bool(text.strip()) and msg.get("stop_reason") == "end_turn"
        return ok, f"«{text.strip()[:30]}» stop={msg.get('stop_reason')} {'; '.join(problems)}", {"secs": secs}

    def non_stream():
        status, resp, _ = request(port, body("Ответь одним словом по-русски: готово", stream=False))
        ok = status == 200 and isinstance(resp, dict) and resp.get("type") == "message" and bool(text_of(resp).strip())
        return ok, f"HTTP {status} type={resp.get('type') if isinstance(resp, dict) else '?'} «{text_of(resp)[:30] if isinstance(resp, dict) else ''}»", {}

    def tool_call():
        status, events, _ = request(port, body("Какая сейчас погода в Казани? Обязательно узнай через инструмент get_weather.", tools=[WEATHER_TOOL]))
        msg, problems = assemble(events)
        calls = tool_uses(msg)
        city = calls[0].get("input", {}).get("city", "") if calls else ""
        ok = not problems and msg.get("stop_reason") == "tool_use" and calls and calls[0].get("name") == "get_weather" and ("азан" in city or "azan" in city.lower())
        return ok, f"stop={msg.get('stop_reason')} вызовы={[c.get('name') for c in calls]} city={city!r} {'; '.join(problems)}", {}

    def tool_roundtrip():
        first = body("Какая сейчас погода в Казани? Обязательно узнай через инструмент get_weather, потом ответь одной фразой.", tools=[WEATHER_TOOL])
        status, events, _ = request(port, first)
        msg, problems = assemble(events)
        calls = tool_uses(msg)
        if problems or not calls:
            return False, f"нет вызова: {problems or msg.get('stop_reason')}", {}
        convo = first["messages"] + [
            {"role": "assistant", "content": msg["content"]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": calls[0]["id"], "content": "+12 °C, облачно, ветер 4 м/с"}]},
        ]
        status, events, _ = request(port, body(convo, tools=[WEATHER_TOOL]))
        final, problems = assemble(events)
        text = text_of(final)
        ok = not problems and "12" in text and final.get("stop_reason") == "end_turn"
        return ok, f"«{text.strip()[:60]}» {'; '.join(problems)}", {}

    def parallel_tools():
        status, events, _ = request(port, body("Узнай погоду сразу в Москве и в Казани — вызови get_weather для обоих городов в одном ответе.", tools=[WEATHER_TOOL]))
        msg, problems = assemble(events)
        cities = sorted(c.get("input", {}).get("city", "") for c in tool_uses(msg))
        ok = not problems and any("москв" in c.lower() for c in cities) and any("казан" in c.lower() for c in cities)
        return ok, f"вызовов {len(cities)}: {cities} {'; '.join(problems)}", {"parallel": len(cities)}

    def thinking_multiturn():
        # Как Claude Code: размышления включены, в следующий ход мысли уходят обратно вместе с tool_result.
        think = {"thinking": {"type": "enabled", "budget_tokens": 2048}}
        first = body("Какая сейчас погода в Казани? Узнай через get_weather.", tools=[WEATHER_TOOL], max_tokens=4096, **think)
        status, events, _ = request(port, first)
        msg, problems = assemble(events)
        calls = tool_uses(msg)
        thinking = [b for b in msg.get("content", []) if b.get("type") == "thinking"]
        if problems or not calls:
            return False, f"первый ход: {problems or msg.get('stop_reason')}", {}
        convo = first["messages"] + [
            {"role": "assistant", "content": msg["content"]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": calls[0]["id"], "content": "−3 °C, снег"}]},
        ]
        status, events, _ = request(port, body(convo, tools=[WEATHER_TOOL], max_tokens=4096, **think))
        final, problems = assemble(events) if status == 200 else ({}, [f"HTTP {status}: {str(events)[:200]}"])
        ok = not problems and bool(text_of(final).strip())
        return ok, f"мыслей в 1-м ходе {len(thinking)}, 2-й ход «{text_of(final).strip()[:40]}» {'; '.join(problems)}", {"thinking_blocks": len(thinking)}

    def max_tokens_cut():
        status, events, _ = request(port, body("Напиши подробный рассказ на 500 слов о море.", max_tokens=40))
        msg, problems = assemble(events)
        ok = not problems and msg.get("stop_reason") == "max_tokens"
        return ok, f"stop={msg.get('stop_reason')} {'; '.join(problems)}", {}

    def unicode_echo():
        phrase = "Ёжик 🦔 — ёлка «в лесу» №7"
        status, events, _ = request(port, body(f"Повтори дословно, без кавычек вокруг и без другого текста: {phrase}"))
        msg, problems = assemble(events)
        text = text_of(msg)
        ok = not problems and phrase in text
        return ok, f"«{text.strip()[:60]}» {'; '.join(problems)}", {}

    def long_output():
        status, events, _ = request(port, body("Выведи числа от 1 до 400 через пробел. Только числа, без другого текста.", max_tokens=8000))
        msg, problems = assemble(events)
        nums = [int(x) for x in re.findall(r"\d+", text_of(msg))]
        ok = not problems and nums == list(range(1, 401))
        return ok, f"чисел {len(nums)}, по порядку: {nums == list(range(1, len(nums) + 1))} {'; '.join(problems)}", {}

    def big_context():
        rng = random.Random(7)
        words = ["река", "город", "ветер", "поле", "книга", "огонь", "камень", "лист", "звезда", "дорога"]
        lines = [" ".join(rng.choice(words) for _ in range(12)) for _ in range(2500)]
        lines[2210] = "Кодовое слово для проверки: МАГНОЛИЯ-5521."
        doc = "\n".join(lines)
        status, events, secs = request(port, body(f"Вот документ:\n\n{doc}\n\nКакое кодовое слово для проверки есть в документе? Ответь только им.", max_tokens=4000))
        msg, problems = assemble(events)
        text = text_of(msg)
        ok = not problems and "МАГНОЛИЯ-5521" in text
        return ok, f"~{len(doc) // 4} ток. «{text.strip()[:30]}» {secs:.0f} с {'; '.join(problems)}", {"input_tokens": msg.get("usage", {}).get("input_tokens")}

    def prompt_cache():
        # Один и тот же длинный запрос с тем же id сессии: второй раз OpenCode должен взять из кэша.
        rules = "\n".join(f"Правило {i}: отвечай кратко и по делу, соблюдая регламент номер {i}." for i in range(900))
        b = body("Ответь одним словом: ок", session=f"cache-{m}-{int(time.time())}", system=[{"type": "text", "text": rules, "cache_control": {"type": "ephemeral"}}])
        request(port, b)
        time.sleep(1)
        status, events, _ = request(port, b)
        msg, problems = assemble(events)
        u = msg.get("usage", {})
        cached = u.get("cache_read_input_tokens", 0) or 0
        total = (u.get("input_tokens", 0) or 0) + cached
        ok = not problems and cached > 0
        return ok, f"из кэша {cached} из {total} {'; '.join(problems)}", {"cached": cached, "total": total}

    def image_input():
        try:
            from PIL import Image
        except ImportError:
            return True, "пропуск: нет Pillow (pip3 install pillow)", {}

        buf = io.BytesIO()
        Image.new("RGB", (64, 64), (220, 30, 30)).save(buf, format="PNG")
        png = base64.b64encode(buf.getvalue()).decode()
        content = [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}, {"type": "text", "text": "Какого цвета квадрат на картинке? Ответь одним словом по-русски."}]
        status, events, _ = request(port, body([{"role": "user", "content": content}]))
        if status != 200:
            return False, f"HTTP {status}: {str(events)[:120]}", {}
        msg, problems = assemble(events)
        text = text_of(msg).lower()
        ok = not problems and "красн" in text
        return ok, f"«{text.strip()[:30]}» {'; '.join(problems)}", {}

    def stop_sequence():
        # У Responses API (маршрут muse) стоп-последовательностей нет вовсе — конвертер их не переносит.
        if m == "muse":
            return True, "пропуск: Responses API не знает stop_sequences", {}
        status, events, _ = request(port, body("Напиши: раз два три СТОП четыре пять", stop_sequences=["СТОП"]))
        msg, problems = assemble(events)
        text = text_of(msg)
        ok = not problems and "четыре" not in text
        return ok, f"stop={msg.get('stop_reason')} «{text.strip()[:40]}» {'; '.join(problems)}", {}

    def cache_control_everywhere():
        # Как у Claude Code: system массивом, cache_control у инструментов и сообщений.
        tools = [dict(WEATHER_TOOL, cache_control={"type": "ephemeral"})]
        messages = [{"role": "user", "content": [{"type": "text", "text": "Ответь одним словом: ок", "cache_control": {"type": "ephemeral"}}]}]
        status, events, _ = request(port, body(messages, tools=tools, system=[{"type": "text", "text": "Ты помощник.", "cache_control": {"type": "ephemeral"}}]))
        msg, problems = assemble(events) if status == 200 else ({}, [f"HTTP {status}"])
        ok = not problems and bool(text_of(msg).strip())
        return ok, f"«{text_of(msg).strip()[:20]}» {'; '.join(problems)}", {}

    cases = [
        ("поток", simple_stream), ("без потока", non_stream), ("вызов инструмента", tool_call), ("круг с tool_result", tool_roundtrip),
        ("параллельные вызовы", parallel_tools), ("размышления и 2-й ход", thinking_multiturn), ("max_tokens", max_tokens_cut),
        ("юникод", unicode_echo), ("длинный вывод 1–400", long_output), ("большой контекст", big_context), ("кэш промптов", prompt_cache),
        ("картинка", image_input), ("stop_sequences", stop_sequence), ("cache_control везде", cache_control_everywhere),
    ]
    return cases


def suite_api(models: list[str], only: str | None):
    log("\n══ ПРОТОКОЛ: прямые запросы в прокси ══")

    def lane(mk):
        proxy = TestProxy(f"api-{mk}", model=MODELS[mk], effort="max")
        try:
            for name, fn in api_cases(proxy.port, mk):
                if only and only not in name:
                    continue
                run_case("api", name, mk, fn)
            s = proxy.stats()
            record(Result("api", "всё ушло в OpenCode", mk, s["pass"] == 0 and s["fallback"] == 0 and s["sub"] > 0, 0, f"sub={s['sub']} pass={s['pass']} fallback={s['fallback']} errors={s['errors']}"))
        finally:
            proxy.stop()

    with cf.ThreadPoolExecutor(len(models)) as pool:
        list(pool.map(lane, [m for m in models if m in MODELS]))


# ─────────── 2. маршрутизация и надёжность ───────────


def suite_route(only: str | None):
    log("\n══ МАРШРУТИЗАЦИЯ И НАДЁЖНОСТЬ ══")
    proxy = TestProxy("route", model=MODELS["deepseek"], effort="low")

    def delta(before, after, key):
        return after[key] - before[key]

    def case(name, fn):
        if not only or only in name:
            run_case("route", name, "deepseek", fn)

    def to_anthropic(before, after):
        # Без OAuth Anthropic отвечает 401, и прокси честно пишет «error», а не «pass»: важно лишь,
        # что запрос ушёл туда одной записью и ни одного — в OpenCode.
        return delta(before, after, "sub") == 0 and delta(before, after, "pass") + delta(before, after, "errors") == 1

    def main_model_passes():
        # Основная модель (Opus) с инструментами — на Anthropic, не в OpenCode. Без OAuth Anthropic ответит 401 —
        # это и доказывает, что запрос ушёл туда, а не подменился.
        before = proxy.stats()
        status, resp, _ = request(proxy.port, dict(body("привет"), model="claude-opus-5-5"))
        after = proxy.stats()
        ok = to_anthropic(before, after)
        return ok, f"HTTP {status} от Anthropic, pass+{delta(before, after, 'pass')} error+{delta(before, after, 'errors')} sub+{delta(before, after, 'sub')}", {}

    def sonnet_passes():
        before = proxy.stats()
        request(proxy.port, dict(body("привет"), model="claude-sonnet-5"))
        after = proxy.stats()
        return to_anthropic(before, after), f"pass+{delta(before, after, 'pass')} error+{delta(before, after, 'errors')} sub+{delta(before, after, 'sub')}", {}

    def sonnet_when_asked():
        proxy.set(matchModels="haiku|sonnet")
        try:
            before = proxy.stats()
            status, events, _ = request(proxy.port, dict(body("Ответь одним словом: ок"), model="claude-sonnet-5"))
            after = proxy.stats()
            return status == 200 and delta(before, after, "sub") == 1, f"HTTP {status}, sub+{delta(before, after, 'sub')}", {}
        finally:
            proxy.set(matchModels="haiku")

    def haiku_without_tools_passes():
        # Служебные вызовы Claude Code (заголовок сессии и т.п.) — haiku без инструментов: на Anthropic.
        before = proxy.stats()
        b = body("привет")
        del b["tools"]
        request(proxy.port, b)
        after = proxy.stats()
        return to_anthropic(before, after), f"pass+{delta(before, after, 'pass')} error+{delta(before, after, 'errors')} sub+{delta(before, after, 'sub')}", {}

    def no_oauth_to_opencode():
        # Заголовок подписки Claude в OpenCode не уходит: подменённый запрос с чужим OAuth всё равно отвечает (ключ — свой).
        status, events, _ = request(proxy.port, body("Ответь одним словом: ок"), headers={"authorization": "Bearer sk-ant-oat01-FAKE-SUBSCRIPTION"})
        msg, problems = assemble(events) if status == 200 else ({}, [f"HTTP {status}"])
        return status == 200 and not problems, f"HTTP {status} «{text_of(msg).strip()[:20]}»", {}

    def browser_blocked():
        status, resp, _ = request(proxy.port, body("привет"), headers={"origin": "https://evil.example"})
        return status == 403, f"HTTP {status}", {}

    def fallback_when_opencode_down():
        base = proxy.cfg["opencodeBase"]  # свой адрес пользователя, а не зашитый дефолт
        proxy.set(opencodeBase="http://127.0.0.1:9")
        try:
            before = proxy.stats()
            status, resp, _ = request(proxy.port, body("привет"))
            after = proxy.stats()
            # Без OAuth настоящая haiku ответит 401 — главное, что запрос ушёл в Claude (откат), а не упал.
            # Откат, на который Anthropic ответил 401, прокси пишет «error» — считаем обе записи.
            moved = delta(before, after, "fallback") + delta(before, after, "errors")
            return moved == 1 and delta(before, after, "sub") == 0, f"HTTP {status}, fallback+{delta(before, after, 'fallback')} error+{delta(before, after, 'errors')}", {}
        finally:
            proxy.set(opencodeBase=base)

    def rotation_on_bad_key():
        good, label = proxy.cfg["apiKey"], proxy.cfg.get("accountLabel", "тест")
        proxy.set(apiKey="sk-subbar-live-invalid-key-000000", accountLabel="Негодный ключ", rotate=True)
        try:
            status, events, _ = request(proxy.port, body("Ответь одним словом: ок"))
            msg, problems = assemble(events) if status == 200 else ({}, [f"HTTP {status}: {str(events)[:120]}"])
            st = proxy.status() or {}
            paused = [p["label"] + ": " + p["reason"] for p in st.get("keys", {}).get("paused", [])]
            used = st.get("keys", {}).get("lastUsed")
            ok = status == 200 and not problems and any("Негодный" in p for p in paused)
            return ok, f"ответил ключ {used}; на паузе: {paused}", {}
        finally:
            proxy.set(apiKey=good, accountLabel=label, rotate=True)

    def no_rotation_no_fallback_errors():
        good = proxy.cfg["apiKey"]
        proxy.set(apiKey="sk-subbar-live-invalid-key-111111", rotate=False, fallback=False)
        try:
            status, resp, _ = request(proxy.port, body("привет"))
            is_anthropic_error = isinstance(resp, dict) and resp.get("type") == "error"
            leaked = "invalid-key-111111" in json.dumps(resp, ensure_ascii=False)
            return status in (429, 502, 503) and is_anthropic_error and not leaked, f"HTTP {status} {resp.get('error', {}).get('message', '')[:70] if isinstance(resp, dict) else ''} ключ в ответе: {leaked}", {}
        finally:
            proxy.set(apiKey=good, rotate=True, fallback=True)

    def load_parallel():
        def one(i):
            status, events, secs = request(proxy.port, body(f"Сколько будет {i}+{i}? Ответь только числом.", session=f"load-{i}"))
            msg, problems = assemble(events) if status == 200 else ({}, [f"HTTP {status}"])
            return (not problems) and str(2 * i) in text_of(msg), secs

        t0 = time.time()
        with cf.ThreadPoolExecutor(8) as pool:
            got = list(pool.map(one, range(11, 19)))
        good = sum(1 for ok, _ in got if ok)
        return good == 8, f"{good}/8 верно, всего {time.time() - t0:.0f} с, самый долгий {max(s for _, s in got):.0f} с", {}

    for name, fn in [
        ("основная модель — не в OpenCode", main_model_passes), ("sonnet по умолчанию — не в OpenCode", sonnet_passes),
        ("sonnet, если попросили", sonnet_when_asked), ("haiku без инструментов — не в OpenCode", haiku_without_tools_passes),
        ("OAuth подписки не мешает и не уходит", no_oauth_to_opencode), ("запрос из браузера — 403", browser_blocked),
        ("OpenCode лежит — откат в Claude", fallback_when_opencode_down), ("негодный ключ — ротация", rotation_on_bad_key),
        ("без ротации и отката — честная ошибка", no_rotation_no_fallback_errors), ("8 потоков разом", load_parallel),
    ]:
        case(name, fn)
    proxy.stop()


# ─────────── 3. настоящий Claude Code ───────────


def make_workspace() -> str:
    ws = tempfile.mkdtemp(prefix="subbar-live-ws-")
    w = lambda p, t: (os.makedirs(os.path.dirname(os.path.join(ws, p)) or ws, exist_ok=True), open(os.path.join(ws, p), "w").write(t))
    w("memo.txt", "Служебная заметка.\nСлово дня: квазар-7319\n")
    w("notes/a.txt", "Альфа: утро началось с кофе.\nещё строка\n")
    w("notes/b.txt", "Бета: код ZEBRA_42 на месте.\nещё строка\n")
    w("notes/c.txt", "Гамма: вечером дождь.\nещё строка\n")
    for i in range(4):
        w(f"docs/doc{i}.md", f"# Документ {i}\n")
    w("docs/extra.txt", "не md\n")
    w("docs/readme.txt", "не md\n")
    rng = random.Random(42)
    open(os.path.join(ws, "data.bin"), "wb").write(bytes(rng.randrange(256) for _ in range(4096)))
    w("config.ini", "[net]\ntimeout = 30\nretries = 3\n\n[ui]\ntheme = light\n")
    w("src/net.py", "def recieve(sock):\n    # recieve bytes\n    return sock.recv(1024)\n")
    w("src/app.py", "from net import recieve\n\ndata = recieve(None)  # recieve once\n")
    w("src/util.py", "def helper():\n    return 'recieve'\n")
    w("заметки 📝.txt", "первая\nвторая\nТретья строка — ёж 🦔\nчетвёртая\n")
    lines = [f"2026-09-27 10:{i // 60 % 60:02d}:{i % 60:02d} INFO запрос {i} обработан" for i in range(4000)]
    lines[3570] = "2026-09-27 11:59:30 FATAL: disk full on /var/data"
    w("big.log", "\n".join(lines))  # без хвостового перевода строки: Read показал бы пустую 4001-ю
    rows = [(f"item{i}", rng.randrange(10, 999)) for i in range(20)]
    w("data.csv", "name,amount\n" + "".join(f"{n},{a}\n" for n, a in rows))
    w("lib/calc.py", "def foo(x):\n    return x * 2\n")
    w("lib/main.py", "from calc import foo\n\nprint(foo(21))\n")
    w("lib/util.py", "from calc import foo\n\ndef twice(x):\n    return foo(foo(x))\n")
    w("lib2/stats.py", "def median(xs):\n    s = sorted(xs)\n    n = len(s)\n    mid = n // 2\n    if n % 2:\n        return s[mid]\n    return (s[mid] + s[mid + 1]) / 2\n")
    w("lib2/test_stats.py", "import sys, os\nsys.path.insert(0, os.path.dirname(__file__))\nfrom stats import median\nassert median([3, 1, 2]) == 2\nassert median([4, 1, 3, 2]) == 2.5\nassert median([10, 20]) == 15\nprint('TESTS OK')\n")
    w("tricky.py", "def f(x, acc=[]):\n    acc.append(x)\n    return sum(acc)\n\nprint(f(1), f(2), f(3))\n")
    months = ["январь", "февраль", "март"]
    regions = ["Север", "Юг", "Запад", "Восток"]
    sales = [(regions[i % 4], months[(i // 4) % 3], rng.randrange(100, 999)) for i in range(60)]
    w("sales.csv", "region,month,amount\n" + "".join(f"{r},{m},{a}\n" for r, m, a in sales))
    w("lib/check.py", "import sys, os\nsys.path.insert(0, os.path.dirname(__file__))\nfrom calc import bar\nfrom util import twice\nassert bar(21) == 42 and twice(3) == 12\nprint('CHECK OK')\n")
    return ws


def march_leader(ws):
    totals = {}
    for line in read(ws, "sales.csv").splitlines()[1:]:
        region, month, amount = line.split(",")
        if month == "март":
            totals[region] = totals.get(region, 0) + int(amount)
    region = max(totals, key=totals.get)
    return region, totals[region]


def unittest_ok(ws, pkg):
    r = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", pkg, "-v"], cwd=ws, capture_output=True, text=True, timeout=60)
    return r.returncode == 0 and r.stderr.count(" ... ok") >= 5


def read(ws, p):
    try:
        return open(os.path.join(ws, p), encoding="utf-8").read()
    except OSError:
        return None


def expected_sha(ws):
    return hashlib.sha256(open(os.path.join(ws, "data.bin"), "rb").read()).hexdigest()[:12]


def expected_sum(ws):
    return sum(int(line.split(",")[1]) for line in read(ws, "data.csv").splitlines()[1:])


AGENT_TASKS = [
    ("арифметика", "Сколько будет 37*43? Ответь только числом.", lambda ws, out, tools: "1591" in out),
    ("прочитать файл", "Прочитай memo.txt и ответь только словом дня из этой заметки.", lambda ws, out, tools: "квазар-7319" in out),
    ("поиск по файлам", "В каком файле в папке notes есть строка ZEBRA_42? Ответь только именем файла.", lambda ws, out, tools: "b.txt" in out),
    ("подсчёт файлов", "Сколько файлов с расширением .md в папке docs? Ответь только числом.", lambda ws, out, tools: re.search(r"(?<!\d)4(?!\d)|четыре", out, re.I) is not None and not re.search(r"(?<!\d)(?!4\b)\d+(?!\d)", out)),
    ("bash и sha256", "Посчитай через bash sha256 файла data.bin и выведи только первые 12 hex-символов.", lambda ws, out, tools: expected_sha(ws) in out),
    ("создать JSON", 'Создай файл out/result.json с JSON {"ok": true, "n": 7}. Больше ничего не делай.', lambda ws, out, tools: (json.loads(read(ws, "out/result.json") or "null") == {"ok": True, "n": 7})),
    ("правка файла", "В config.ini поменяй значение timeout на 45. Остальное в файле не трогай.", lambda ws, out, tools: read(ws, "config.ini") == "[net]\ntimeout = 45\nretries = 3\n\n[ui]\ntheme = light\n"),
    ("опечатки во всех файлах", "Во всех файлах в папке src исправь опечатку «recieve» на «receive» (и в коде, и в комментариях, и в строках). Ответь, сколько замен сделал.",
     lambda ws, out, tools: all("recieve" not in (read(ws, f"src/{f}") or "recieve") for f in ("net.py", "app.py", "util.py")) and sum((read(ws, f"src/{f}") or "").count("receive") for f in ("net.py", "app.py", "util.py")) == 6),
    ("юникод в имени и тексте", "Прочитай файл «заметки 📝.txt» и выведи дословно его третью строку, без кавычек и другого текста.", lambda ws, out, tools: "Третья строка — ёж 🦔" in out),
    ("длинный вывод", "Выведи числа от 1 до 300 через пробел, без другого текста.", lambda ws, out, tools: [int(x) for x in re.findall(r"\d+", out)] == list(range(1, 301))),
    ("иголка в большом логе", "В файле big.log есть строка с FATAL. Найди номер этой строки (с 1) и её текст. Ответь в виде «N: текст».", lambda ws, out, tools: "3571" in out and "disk full" in out),
    ("сумма в CSV", "Посчитай сумму столбца amount в data.csv. Ответь только числом.", lambda ws, out, tools: str(expected_sum(ws)) in out.replace(" ", "")),
    ("рефакторинг и проверка", "Переименуй функцию foo в bar во всех .py файлах в папке lib (и определение, и все вызовы, и импорты). Потом запусти python3 lib/check.py и сообщи, что он вывел.",
     lambda ws, out, tools: subprocess.run([sys.executable, os.path.join(ws, "lib/check.py")], capture_output=True, text=True).stdout.strip() == "CHECK OK"),
    ("нет файла — создать", "Прочитай файл missing.txt. Если его нет — создай его с текстом пусто (одно слово, без кавычек) и ответь «создал».", lambda ws, out, tools: (read(ws, "missing.txt") or "").strip() == "пусто"),
    ("три файла", "Создай три файла a.txt, b.txt, c.txt с содержимым 1, 2 и 3 соответственно (только цифра в файле).", lambda ws, out, tools: [(read(ws, f) or "").strip() for f in ("a.txt", "b.txt", "c.txt")] == ["1", "2", "3"]),
    ("субагент", "Запусти одного субагента (инструмент Agent с model: haiku), который прочитает memo.txt и вернёт слово дня. Потом выведи это слово.",
     lambda ws, out, tools: "квазар-7319" in out and any(t in ("Agent", "Task") for t in tools)),
    ("три субагента параллельно", "Запусти трёх субагентов параллельно, в одном ответе (инструмент Agent, model: haiku): первый читает notes/a.txt, второй notes/b.txt, третий notes/c.txt; каждый возвращает первую строку своего файла. Потом выведи все три строки.",
     lambda ws, out, tools: all(k in out for k in ("Альфа", "Бета", "Гамма")) and sum(t in ("Agent", "Task") for t in tools) >= 3),
]


HARD_TASKS = [
    ("починить баг", "Запусти python3 lib2/test_stats.py — он падает. Найди и исправь баг в lib2/stats.py (тест не меняй), добейся, чтобы тест прошёл, и покажи его вывод.",
     lambda ws, out, tools: subprocess.run([sys.executable, os.path.join(ws, "lib2/test_stats.py")], capture_output=True, text=True).stdout.strip() == "TESTS OK" and "assert" in (read(ws, "lib2/test_stats.py") or "")),
    ("понять хитрый код", "Что выведет python3 tricky.py? Разберись в коде и ответь одной строкой — ровно тем, что будет напечатано.", lambda ws, out, tools: "1 3 6" in out),
    ("аналитика CSV", "В sales.csv продажи по регионам и месяцам. Какой регион дал наибольшую сумму продаж за март? Ответь в виде «регион: сумма».",
     lambda ws, out, tools: (lambda r: r[0] in out and str(r[1]) in out.replace(" ", ""))(march_leader(ws))),
    ("пакет с тестами", "Создай пакет calc/ (с __init__.py): модуль ops.py с функциями add, sub, mul, div (div поднимает ZeroDivisionError при делении на ноль) и тесты calc/test_ops.py на unittest — не меньше 5 тестов, включая деление на ноль. Запусти python3 -m unittest discover -s calc -v и добейся, чтобы всё прошло.",
     lambda ws, out, tools: unittest_ok(ws, "calc")),
    ("огромный вывод инструмента", "Прочитай ВЕСЬ файл big.log инструментом Read (без tail и head) и скажи: сколько в нём строк и какой текст у последней строки. Ответь в виде «N строк; последняя: …».",
     lambda ws, out, tools: "4000" in out and "запрос 3999 обработан" in out),
    ("рефакторинг руками субагента", "Поручи субагенту (инструмент Agent с model: haiku): переименовать функцию foo в bar во всех .py в папке lib (определение, вызовы, импорты) и запустить python3 lib/check.py. Потом выведи, что напечатал check.py.",
     lambda ws, out, tools: any(t in ("Agent", "Task") for t in tools) and subprocess.run([sys.executable, os.path.join(ws, "lib/check.py")], capture_output=True, text=True).stdout.strip() == "CHECK OK"),
    ("план через задачи", "Составь план из трёх шагов инструментами задач Claude Code (TaskCreate/TaskUpdate или TodoWrite — что есть) и выполни его: создай x.txt с текстом A, y.txt с текстом B, затем xy.txt с их объединением (AB). Отмечай шаги выполненными.",
     lambda ws, out, tools: (read(ws, "xy.txt") or "").strip() == "AB" and any(t in ("TaskCreate", "TodoWrite") for t in tools)),
]


def run_claude(port: int, ws: str, prompt: str, model: str = "haiku", timeout: int = 480) -> dict:
    cmd = ["claude", "-p", prompt, "--model", model, "--output-format", "stream-json", "--verbose",
           "--dangerously-skip-permissions", "--setting-sources", "project", "--strict-mcp-config", "--no-session-persistence", "--no-chrome"]
    env = {k: v for k, v in os.environ.items() if k not in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN")}
    env["ANTHROPIC_BASE_URL"] = f"http://127.0.0.1:{port}"
    t0 = time.time()
    try:
        proc = subprocess.run(cmd, cwd=ws, env=env, capture_output=True, text=True, timeout=timeout)
        raw = proc.stdout
    except FileNotFoundError:
        # Исключение из пула оставило бы все прокси живыми — отдаём проваленный кейс.
        return {"result": "", "error": "claude не найден в PATH", "tools": [], "secs": 0.0, "turns": 0}
    except subprocess.TimeoutExpired as e:
        raw = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
        return {"result": "", "error": f"таймаут {timeout} с", "tools": [], "secs": time.time() - t0, "turns": 0}
    tools, result, error, turns = [], "", "", 0
    for line in raw.splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "assistant":
            for block in ev.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    tools.append(block.get("name"))
        elif ev.get("type") == "result":
            result = ev.get("result") or ""
            turns = ev.get("num_turns", 0)
            if ev.get("is_error") or ev.get("subtype") != "success":
                error = f"{ev.get('subtype')}: {result[:120]}"
    if not result and not error:
        # claude не стартовал (нет бинаря, неизвестный флаг): без stderr причина провала не видна вовсе.
        error = f"нет итога, код {proc.returncode}: {proc.stderr.strip()[-200:]}"
    return {"result": result, "error": error, "tools": tools, "secs": time.time() - t0, "turns": turns}


def suite_agent(models: list[str], only: str | None, lanes: int, opus: bool, repeat: int = 1, tasks=None, suite_name: str = "agent"):
    log("\n══ НАСТОЯЩИЙ CLAUDE CODE: основная haiku → модель OpenCode, задачи с проверяемым итогом ══")
    jobs = []
    for mk in models:
        for name, prompt, check in (tasks or AGENT_TASKS):
            if only and only not in name:
                continue
            for i in range(repeat):
                jobs.append((mk, name if repeat == 1 else f"{name} #{i + 1}", prompt, check))

    proxies: dict[str, list[TestProxy]] = {}
    for mk in models:
        cfg = {"model": MODELS.get(mk, MODELS["deepseek"]), "effort": "max"}
        if mk == "haiku":
            cfg["enabled"] = False  # настоящая haiku: прокси пропускает всё в Anthropic
        proxies[mk] = []
        try:
            for i in range(max(1, lanes)):
                proxies[mk].append(TestProxy(f"agent-{mk}-{i}", **cfg))
        except Exception:
            for ps in proxies.values():  # уже поднятые не бросаем живыми
                for p in ps:
                    p.stop()
            raise
    # Свой прокси на каждый прогон: очередь на модель — задачи одной модели ждут свободный, а не хватают чужой.
    free = {mk: queue.Queue() for mk in proxies}
    for mk, ps in proxies.items():
        for p in ps:
            free[mk].put(p)

    def job(j):
        mk, name, prompt, check = j
        proxy = free[mk].get()
        try:
            ws = make_workspace()
            before = proxy.stats()
            t_start = time.time()
            r = run_claude(proxy.port, ws, prompt)
            after = proxy.stats()
            try:
                ok = not r["error"] and bool(check(ws, r["result"], r["tools"]))
            except Exception as e:
                ok, r["error"] = False, f"проверка: {e}"
            routed = {k: after[k] - before[k] for k in after}
            # Почему откатился или ошибся — из последних событий прокси (журнал удаляется вместе с песочницей).
            recent = (proxy.status() or {}).get("stats", {}).get("recent", [])
            why = [f"{e.get('route')}: {e.get('note')}" for e in recent if e.get("route") in ("fallback", "error") and e.get("at", 0) >= t_start * 1000]
            if why:
                r["error"] = (r["error"] + " причины: " + " | ".join(why[-3:])).strip()
            # Подмена честная: для моделей OpenCode рабочие запросы ушли в OpenCode, не откатились в Claude.
            if mk != "haiku" and (routed["sub"] == 0 or routed["fallback"] > 0):
                ok = False
                r["error"] = (r["error"] + f" маршрут: {routed}").strip()
            note = f"ходов {r['turns']}, инструменты {r['tools'][:8]}, sub+{routed['sub']} pass+{routed['pass']}"
            if not ok or r["error"]:
                note += f" | итог «{r['result'].strip()[:90]}» {r['error']}"
            record(Result(suite_name, name, mk, ok, r["secs"], note, {"tools": r["tools"], "routed": routed}))
            shutil.rmtree(ws, ignore_errors=True)
        finally:
            free[mk].put(proxy)

    workers = sum(len(ps) for ps in proxies.values())
    with cf.ThreadPoolExecutor(workers) as pool:
        list(pool.map(job, jobs))

    if opus and not only:
        log("\n══ НАСТОЯЩИЙ СЦЕНАРИЙ: основная Opus на подписке + субагенты haiku → OpenCode ══")
        proxy = proxies[models[0]][0] if models[0] != "haiku" else TestProxy("opus", model=MODELS["deepseek"], effort="max")
        if models[0] == "haiku":
            proxies.setdefault("opus", []).append(proxy)  # иначе ниже его не остановят: живой прокси и каталог с ключом
        for name, prompt, check, want_sub in [
            ("Opus без субагентов — не подменяется", "Сколько будет 2+2? Ответь только числом, не запускай субагентов.", lambda ws, out, t: "4" in out, False),
            ("Opus + субагент haiku", "Запусти одного субагента (инструмент Agent с model: haiku), который прочитает memo.txt и вернёт слово дня. Потом выведи это слово.",
             lambda ws, out, t: "квазар-7319" in out and any(x in ("Agent", "Task") for x in t), True),
        ]:
            ws = make_workspace()
            before = proxy.stats()
            r = run_claude(proxy.port, ws, prompt, model="opus")
            after = proxy.stats()
            routed = {k: after[k] - before[k] for k in after}
            ok = not r["error"] and check(ws, r["result"], r["tools"]) and routed["pass"] >= 1 and ((routed["sub"] >= 1) == want_sub)
            record(Result("opus", name, "opus+" + (proxy.cfg["model"].split("-")[0]), ok, r["secs"],
                          f"основная → Anthropic: pass+{routed['pass']}, субагенты → OpenCode: sub+{routed['sub']}, откатов {routed['fallback']} | «{r['result'].strip()[:50]}» {r['error']}"))
            shutil.rmtree(ws, ignore_errors=True)

    for ps in proxies.values():
        for p in ps:
            p.stop()


def suite_robust(only: str | None):
    log("\n══ НАДЁЖНОСТЬ НА ЖИВОМ CLAUDE CODE ══")
    read_task = AGENT_TASKS[1]

    def claude_case(name, proxy, task, want):
        if only and only not in name:
            return
        ws = make_workspace()
        before = proxy.stats()
        r = run_claude(proxy.port, ws, task[1])
        after = proxy.stats()
        routed = {k: after[k] - before[k] for k in after}
        st = proxy.status() or {}
        paused = [p["label"] for p in st.get("keys", {}).get("paused", [])]
        ok = not r["error"] and task[2](ws, r["result"], r["tools"]) and want(routed, st)
        record(Result("robust", name, proxy.cfg["model"].split("-")[0], ok, r["secs"], f"маршрут {routed}, на паузе {paused}, работал {st.get('keys', {}).get('lastUsed')} | «{r['result'].strip()[:40]}» {r['error']}"))
        shutil.rmtree(ws, ignore_errors=True)

    # 1. Негодный основной ключ: ротация уводит на рабочий, сессия не замечает.
    proxy = TestProxy("robust-rotate", model=MODELS["deepseek"], effort="max")
    good = proxy.cfg["apiKey"]
    proxy.set(apiKey="sk-subbar-live-invalid-key-222222", accountLabel="Негодный ключ")
    claude_case("негодный ключ — ротация посреди работы", proxy, read_task, lambda rt, st: rt["sub"] >= 1 and rt["fallback"] == 0 and any(p["label"] == "Негодный ключ" for p in st.get("keys", {}).get("paused", [])))
    proxy.set(apiKey=good, accountLabel="тест")
    proxy.stop()

    # 2. OpenCode лежит: запросы уходят в настоящую haiku, работа доделана.
    proxy = TestProxy("robust-down", model=MODELS["deepseek"], opencodeBase="http://127.0.0.1:9")
    claude_case("OpenCode лежит — откат, работа доделана", proxy, read_task, lambda rt, st: rt["fallback"] >= 1 and rt["sub"] == 0)
    proxy.stop()

    # 3. Шесть сессий Claude Code разом на одном прокси.
    if not only or only in "6 сессий разом":
        proxy = TestProxy("robust-load", model=MODELS["deepseek"], effort="max")
        before = proxy.stats()
        t0 = time.time()

        def one(i):
            ws = make_workspace()
            r = run_claude(proxy.port, ws, read_task[1])
            ok = not r["error"] and read_task[2](ws, r["result"], r["tools"])
            shutil.rmtree(ws, ignore_errors=True)
            return ok, r["secs"]

        with cf.ThreadPoolExecutor(6) as pool:
            got = list(pool.map(one, range(6)))
        after = proxy.stats()
        routed = {k: after[k] - before[k] for k in after}
        good_runs = sum(ok for ok, _ in got)
        record(Result("robust", "6 сессий Claude Code разом", "deepseek", good_runs == 6 and routed["fallback"] == 0 and routed["errors"] == 0, time.time() - t0,
                      f"{good_runs}/6 верно, маршрут {routed}, самая долгая {max(s for _, s in got):.0f} с"))
        proxy.stop()


# ─────────── итог ───────────


def summary(out_path: str):
    log("\n══ ИТОГ ══")
    by_suite: dict[str, list[Result]] = {}
    for r in RESULTS:
        by_suite.setdefault(r.suite, []).append(r)
    for suite, rs in by_suite.items():
        models = sorted({r.model for r in rs})
        names = list(dict.fromkeys(r.name for r in rs))
        log(f"\n{suite}: {sum(r.ok for r in rs)}/{len(rs)} прошло")
        width = max(len(n) for n in names) + 2
        log(" " * width + "".join(f"{m:>12}" for m in models))
        for n in names:
            row = ""
            for m in models:
                hit = [r for r in rs if r.name == n and r.model == m]
                row += f"{('✓' if hit[0].ok else '✗') + f' {hit[0].secs:.0f}с':>12}" if hit else f"{'·':>12}"
            log(f"{n:<{width}}{row}")
    fails = [r for r in RESULTS if not r.ok]
    if fails:
        log("\nНе прошло:")
        for r in fails:
            log(f"  ✗ {r.suite}/{r.name} [{r.model}]: {r.note}")
    json.dump([r.__dict__ for r in RESULTS], open(out_path, "w"), ensure_ascii=False, indent=1)
    log(f"\nВсего: {sum(r.ok for r in RESULTS)}/{len(RESULTS)} · подробно: {out_path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("suite", choices=["api", "route", "agent", "hard", "robust", "all"])
    ap.add_argument("--models", default="deepseek,bunny,muse")
    ap.add_argument("--only")
    ap.add_argument("--lanes", type=int, default=2, help="параллельных прогонов Claude Code на модель")
    ap.add_argument("--opus", action="store_true", help="плюс сценарий «основная Opus + субагенты» (тратит подписку)")
    ap.add_argument("--repeat", type=int, default=1, help="сколько раз гонять каждую задачу (стабильность)")
    ap.add_argument("--out", default=f"/tmp/subbar-live-{time.strftime('%H%M%S')}.json")
    a = ap.parse_args()
    if not os.path.exists(BIN):
        sys.exit(f"нет {BIN} — собери: cargo build --release")
    models = [m.strip() for m in a.models.split(",") if m.strip()]
    label, _ = best_key()
    log(f"Тестовый ключ: {label} (из карточек, с наибольшим запасом). Модели: {models}")
    if a.suite in ("api", "all"):
        suite_api(models, a.only)
    if a.suite in ("route", "all"):
        suite_route(a.only)
    if a.suite in ("agent", "all"):
        suite_agent(models, a.only, a.lanes, a.opus, a.repeat)
    if a.suite in ("hard", "all"):
        log("\n══ СЛОЖНЫЕ ЗАДАЧИ: качество модели в настоящем Claude Code ══")
        suite_agent(models, a.only, a.lanes, False, a.repeat, tasks=HARD_TASKS, suite_name="hard")
    if a.suite in ("robust", "all"):
        suite_robust(a.only)
    summary(a.out)


if __name__ == "__main__":
    main()
