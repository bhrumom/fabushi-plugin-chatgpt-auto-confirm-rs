import json
import re
import sys
import time

import pyatspi

APP_NAMES = ("Codex", "ChatGPT")
SAFE_CONVERSATION_WORDS = (
    "conversation",
    "current chat",
    "this chat",
    "session",
    "本次会话",
    "当前会话",
    "本次聊天",
)
PERSISTENT_WORDS = ("always", "permanent", "all chats", "永久", "总是", "始终")
ALLOW_WORDS = ("allow", "approve", "允许", "批准")
REJECT_WORDS = ("reject", "deny", "拒绝")
OPTIONS_WORDS = ("approval options", "options", "审批选项", "授权选项")
COPY_WORDS = ("copy", "复制")
STOP_WORDS = ("stop generating", "stop", "停止生成", "停止回答")
SEND_WORDS = ("send", "send message", "发送", "发送消息")
RETRY_WORDS = ("retry", "重试")
GOT_IT_WORDS = ("got it", "明白了", "知道了")
RATE_LIMIT_TEXT = ("too many requests", "request too frequent", "请求过于频繁")
UNABLE_LOAD_TEXT = ("unable to load", "无法加载此 chatgpt 对话", "无法加载此对话")
INTERRUPTED_TEXT = ("connection interrupted", "连接中断")
LENGTH_LIMIT_TEXT = ("conversation is too long", "maximum length", "对话过长", "达到对话长度")
CACHE_EXPIRED_TEXT = ("stream cache expired", "流缓存已过期")
POLL_TIMEOUT_TEXT = ("stream polling timeout", "轮询超时")

def walk(root, limit=14000):
    stack = [root]
    seen = 0
    while stack and seen < limit:
        node = stack.pop()
        seen += 1
        yield node
        try:
            children = list(node)
        except Exception:
            children = []
        for child in reversed(children):
            stack.append(child)

def text_of(node):
    try:
        if node.getRoleName() in ("entry", "text", "paragraph", "static"):
            txt = node.queryText()
            return txt.getText(0, -1) or ""
    except Exception:
        pass
    try:
        return node.name or ""
    except Exception:
        return ""

def state(node, which):
    try:
        return node.getState().contains(which)
    except Exception:
        return False

def enabled(node):
    return state(node, pyatspi.STATE_ENABLED) and not state(node, pyatspi.STATE_DEFUNCT)

def visible(node):
    return state(node, pyatspi.STATE_SHOWING) or state(node, pyatspi.STATE_VISIBLE)

def node_name(node):
    try:
        return (node.name or "").strip()
    except Exception:
        return ""

def role(node):
    try:
        return node.getRoleName()
    except Exception:
        return ""

def find_app():
    desktop = pyatspi.Registry.getDesktop(0)
    candidates = []
    for app in desktop:
        try:
            name = app.name or ""
        except Exception:
            continue
        if name in APP_NAMES:
            candidates.append(app)
    if not candidates:
        raise RuntimeError("ChatGPT desktop AT-SPI application not found")
    # Prefer the application that exposes a ChatGPT frame/document.
    for app in candidates:
        for node in walk(app, 1500):
            if role(node) in ("frame", "document web") and node_name(node) == "ChatGPT":
                return app
    return candidates[0]

def flattened(app):
    out = []
    for node in walk(app):
        name = node_name(node)
        txt = text_of(node)
        out.append({
            "node": node,
            "role": role(node),
            "name": name,
            "text": txt,
            "enabled": enabled(node),
            "visible": visible(node),
        })
    return out

def normalized(item):
    return (item["name"] + "\n" + item["text"]).strip().lower()

def action(node):
    act = node.queryAction()
    preferred = ("press", "click", "open", "select", "doDefault")
    names = [act.getName(i) for i in range(act.nActions)]
    for wanted in preferred:
        for i, name in enumerate(names):
            if name == wanted:
                return act.doAction(i)
    if act.nActions:
        return act.doAction(0)
    return False

def find_named(items, names, roles=None, actionable=False):
    names = tuple(n.lower() for n in names)
    for item in items:
        if roles and item["role"] not in roles:
            continue
        if actionable and not item["enabled"]:
            continue
        value = normalized(item)
        if any(value == n or n in value for n in names):
            return item
    return None

def marker_info(items):
    marker_re = re.compile(r"\[Fabushi:([0-9a-fA-F-]{8,})\]")
    latest = None
    latest_index = -1
    for i, item in enumerate(items):
        hay = item["text"] or item["name"]
        match = marker_re.search(hay)
        if match:
            latest = match.group(1)
            latest_index = i
    return latest, latest_index

def hash_text(value):
    import hashlib
    return hashlib.sha256(value.encode("utf-8", "replace")).hexdigest()

def current_reasoning(items):
    mapping = {
        "instant": "instant",
        "medium": "medium",
        "high": "high",
        "extra high": "extra_high",
        "max": "extra_high",
        "pro": "pro",
    }
    rx = re.compile(r"^(Instant|Medium|High|Extra High|Max|Pro),\s*[1-5]\s+of\s+5\.?$", re.I)
    for item in items:
        value = (item["name"] or item["text"]).strip()
        match = rx.match(value)
        if match:
            return mapping.get(match.group(1).lower())
    return None

def snapshot():
    app = find_app()
    items = flattened(app)
    composer = None
    for item in items:
        if item["role"] == "entry" and item["enabled"] and (
            "ask chatgpt" in normalized(item)
            or "message chatgpt" in normalized(item)
            or "询问 chatgpt" in normalized(item)
        ):
            composer = item
            break

    marker, marker_index = marker_info(items)
    after = items[marker_index + 1 :] if marker_index >= 0 else []
    after_text = []
    for item in after:
        if item["role"] in ("static", "paragraph", "text") and item["visible"]:
            value = (item["text"] or item["name"]).strip()
            if value and len(value) <= 12000 and "fabushi:" not in value.lower():
                after_text.append(value)
    prose = "\n".join(after_text[-80:])[-16000:]
    copy_after = any(
        item["role"] in ("push button", "button") and any(w in normalized(item) for w in COPY_WORDS)
        for item in after
    )
    stop = any(
        item["enabled"] and item["role"] in ("push button", "button")
        and any(w in normalized(item) for w in STOP_WORDS)
        for item in items
    )

    reject = [item for item in items if any(w in normalized(item) for w in REJECT_WORDS)]
    allow = [item for item in items if any(w in normalized(item) for w in ALLOW_WORDS)]
    options = [item for item in items if any(w in normalized(item) for w in OPTIONS_WORDS)]
    auth_present = bool(reject and allow and options)
    auth_actionable = auth_present and any(item["enabled"] for item in allow) and any(item["enabled"] for item in options)

    all_text = "\n".join((item["text"] or item["name"]) for item in items if item["visible"]).lower()
    rate_limit = any(t in all_text for t in RATE_LIMIT_TEXT)
    unable_load = any(t in all_text for t in UNABLE_LOAD_TEXT)
    interrupted = any(t in all_text for t in INTERRUPTED_TEXT)
    length_limit = any(t in all_text for t in LENGTH_LIMIT_TEXT)
    cache_expired = any(t in all_text for t in CACHE_EXPIRED_TEXT)
    polling_timeout = any(t in all_text for t in POLL_TIMEOUT_TEXT)
    retryable = any(
        item["enabled"] and item["role"] in ("push button", "button")
        and any(w in normalized(item) for w in RETRY_WORDS)
        for item in after if marker_index >= 0
    )

    picker = any(
        item["enabled"] and (
            "select chatgpt model" in normalized(item)
            or "model" == item["name"].strip().lower()
        )
        for item in items
    )
    selected = current_reasoning(items)

    draft = ""
    if composer:
        draft = composer["text"] or ""
    response_boundary = hash_text(prose) if marker and prose else None
    conversation_fingerprint = hash_text((marker or "") + "|" + (response_boundary or "")) if marker else None
    progress = hash_text(prose + "|" + str(stop) + "|" + str(auth_present)) if marker else None

    return {
        "app_healthy": True,
        "composer_ready": composer is not None,
        "draft_fingerprint": hash_text(draft) if draft else None,
        "user_turn_boundary": hash_text(marker) if marker else None,
        "current_dispatch_id": marker,
        "user_turn_ownership": "strong" if marker else "none",
        "assistant_response_boundary": response_boundary,
        "assistant_response_ownership": "strong" if marker and response_boundary else "none",
        "conversation_ref": None,
        "conversation_fingerprint": conversation_fingerprint,
        "assistant_visible_prose": prose,
        "assistant_visible_work_trace": [],
        "streaming_or_busy": stop,
        "stop_available": stop,
        "authorization_surface_present": auth_present,
        "authorization_actionable": auth_actionable,
        "authorization_settlement": "inactive",
        "response_local_copy": copy_after,
        "strict_review_report": None,
        "rate_limit": rate_limit,
        "retryable_error": retryable,
        "unable_to_load_conversation": unable_load,
        "connection_interrupted": interrupted,
        "conversation_length_limit": length_limit,
        "stream_polling_timeout": polling_timeout,
        "stream_cache_expired": cache_expired,
        "hydration": "ready" if composer else "loading",
        "reasoning_picker_available": picker,
        "selected_reasoning_preset": selected,
        "attachment_ready": True,
        "blocker_or_modal": False,
        "progress_fingerprint": progress,
    }

def send_prompt(prompt):
    app = find_app()
    items = flattened(app)
    entry = None
    for item in items:
        if item["role"] == "entry" and item["enabled"] and (
            "ask chatgpt" in normalized(item) or "message chatgpt" in normalized(item)
            or "询问 chatgpt" in normalized(item)
        ):
            entry = item["node"]
            break
    if entry is None:
        raise RuntimeError("ChatGPT composer is not ready")
    editable = entry.queryEditableText()
    editable.setTextContents(prompt)
    time.sleep(0.15)
    items = flattened(app)
    button = find_named(items, SEND_WORDS, roles=("push button", "button"), actionable=True)
    if button is None:
        raise RuntimeError("ChatGPT Send control is not actionable after composing")
    if not action(button["node"]):
        raise RuntimeError("ChatGPT Send action failed")
    return True

def start_fresh():
    app = find_app()
    items = flattened(app)
    button = find_named(items, ("New chat", "新聊天", "新对话"), roles=("push button", "button"), actionable=True)
    if button is None:
        raise RuntimeError("New chat control not found")
    if not action(button["node"]):
        raise RuntimeError("New chat action failed")
    return True

def recover():
    pyatspi.Registry.generateKeyboardEvent(0, "r", pyatspi.KEY_PRESSRELEASE | pyatspi.KEY_CONTROL)
    return True

def approve():
    app = find_app()
    items = flattened(app)
    reject = [i for i in items if any(w in normalized(i) for w in REJECT_WORDS)]
    options = [i for i in items if any(w in normalized(i) for w in OPTIONS_WORDS)]
    if not reject or not options:
        return False
    option = next((i for i in options if i["enabled"]), None)
    if option is None or not action(option["node"]):
        return False
    time.sleep(0.25)
    items = flattened(app)
    candidates = []
    for item in items:
        value = normalized(item)
        if item["role"] not in ("menu item", "push button", "button") or not item["enabled"]:
            continue
        if any(bad in value for bad in PERSISTENT_WORDS):
            continue
        if any(word in value for word in SAFE_CONVERSATION_WORDS) and any(word in value for word in ALLOW_WORDS):
            candidates.append(item)
    if len(candidates) != 1:
        return False
    return bool(action(candidates[0]["node"]))

def dismiss_rate_limit():
    app = find_app()
    items = flattened(app)
    all_text = "\n".join((i["text"] or i["name"]) for i in items if i["visible"]).lower()
    if not any(t in all_text for t in RATE_LIMIT_TEXT):
        return False
    button = find_named(items, GOT_IT_WORDS, roles=("push button", "button"), actionable=True)
    return bool(button and action(button["node"]))

def reasoning_position(items):
    rx = re.compile(r"^(Instant|Medium|High|Extra High|Max|Pro),\s*([1-5])\s+of\s+5\.?$", re.I)
    for item in items:
        value = (item["name"] or item["text"]).strip()
        match = rx.match(value)
        if match:
            return int(match.group(2)) - 1
    return None

def ensure_reasoning_menu(app):
    items = flattened(app)
    if any(item["name"] == "Power" and item["enabled"] for item in items):
        return items
    picker = find_named(
        items, ("Select ChatGPT model", "选择 ChatGPT 模型"),
        roles=("push button", "button"), actionable=True
    )
    if picker is None:
        return None
    action(picker["node"])
    time.sleep(0.25)
    return flattened(app)

def close_reasoning_menu(app):
    items = flattened(app)
    if not any(item["name"] == "Power" for item in items):
        return
    picker = find_named(
        items, ("Select ChatGPT model", "选择 ChatGPT 模型"),
        roles=("push button", "button"), actionable=True
    )
    if picker is not None:
        action(picker["node"])
        time.sleep(0.08)

def reasoning():
    app = find_app()
    items = ensure_reasoning_menu(app)
    if items is None:
        return None
    observed = current_reasoning(items)
    close_reasoning_menu(app)
    return observed

def set_reasoning(target):
    if target < 0 or target > 4:
        raise RuntimeError("reasoning preset must be between 0 and 4")
    app = find_app()
    items = ensure_reasoning_menu(app)
    if items is None:
        return False
    current = reasoning_position(items)
    if current is None:
        close_reasoning_menu(app)
        return False
    for _ in range(8):
        if current == target:
            close_reasoning_menu(app)
            return True
        items = flattened(app)
        power = next((item for item in items if item["name"] == "Power" and item["enabled"]), None)
        if power is None:
            close_reasoning_menu(app)
            return False
        try:
            power["node"].queryComponent().grabFocus()
        except Exception:
            close_reasoning_menu(app)
            return False
        time.sleep(0.06)
        keyval = 65363 if target > current else 65361
        pyatspi.Registry.generateKeyboardEvent(keyval, None, pyatspi.KEY_SYM)
        time.sleep(0.18)
        next_position = reasoning_position(flattened(app))
        if next_position is None or next_position == current:
            close_reasoning_menu(app)
            return False
        current = next_position
    verified = current == target
    close_reasoning_menu(app)
    return verified

def main():
    op = sys.argv[1]
    if op == "snapshot":
        result = snapshot()
    elif op == "send":
        result = send_prompt(sys.argv[2])
    elif op == "fresh":
        result = start_fresh()
    elif op == "recover":
        result = recover()
    elif op == "approve":
        result = approve()
    elif op == "dismiss-rate-limit":
        result = dismiss_rate_limit()
    elif op == "reasoning":
        result = reasoning()
    elif op == "set-reasoning":
        result = set_reasoning(int(sys.argv[2]))
    else:
        raise RuntimeError("unknown operation: " + op)
    print(json.dumps(result, ensure_ascii=False))

try:
    main()
except Exception as exc:
    print(json.dumps({"error": str(exc)}, ensure_ascii=False))
    sys.exit(2)
