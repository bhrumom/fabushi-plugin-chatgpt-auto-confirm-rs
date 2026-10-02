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
ATTACH_WORDS = ("attach", "attach files", "add files", "add photos & files", "添加文件", "附件")
UPLOAD_WORDS = ("upload files", "upload from computer", "from computer", "上传文件", "从电脑上传")
REMOVE_ATTACHMENT_WORDS = ("remove attachment", "remove file", "删除附件", "移除附件")
FILE_CHOOSER_WORDS = ("open", "choose", "select", "upload", "file", "打开", "选择", "上传", "文件")
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
            "focused": state(node, pyatspi.STATE_FOCUSED),
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

ACTION_ROLES = ("push button", "button", "toggle button")
ALLOW_LABELS = (
    "allow", "allow once", "allow one time", "approve", "approve once", "approve one time",
    "允许", "允许一次", "批准", "批准一次",
)
REJECT_LABELS = ("reject", "deny", "decline", "拒绝", "不允许")
OPTION_LABEL_HINTS = (
    "approval options", "authorization options", "options", "more", "menu", "expand",
    "审批选项", "授权选项", "选项", "更多", "展开", "箭头",
)
BROAD_AUTH_CONTAINER_ROLES = ("application", "frame", "document web", "document frame")


def item_labels(item):
    values = []
    for value in (item.get("name", ""), item.get("text", "")):
        value = " ".join((value or "").strip().lower().split())
        if value and value not in values:
            values.append(value)
    return values


def exact_action_label(item, labels):
    wanted = set(labels)
    return any(value in wanted for value in item_labels(item))


def parent_of(node):
    try:
        return node.parent
    except Exception:
        return None


def same_node(left, right):
    try:
        return left == right
    except Exception:
        return left is right


def is_descendant(node, ancestor, max_depth=12):
    current = node
    for _ in range(max_depth + 1):
        if current is None:
            return False
        if same_node(current, ancestor):
            return True
        current = parent_of(current)
    return False


def node_attributes(node):
    try:
        raw = node.getAttributes()
    except Exception:
        return {}
    result = {}
    for entry in raw or []:
        text = str(entry)
        if ":" in text:
            key, value = text.split(":", 1)
        elif "=" in text:
            key, value = text.split("=", 1)
        else:
            continue
        result[key.strip().lower()] = value.strip().lower()
    return result


def has_popup_semantics(item):
    attributes = node_attributes(item["node"])
    for key, value in attributes.items():
        if "haspopup" in key and value not in ("", "false", "0", "none"):
            return True
    return False


def is_allow_action(item):
    return item["visible"] and item["role"] in ACTION_ROLES and exact_action_label(item, ALLOW_LABELS)


def is_reject_action(item):
    return item["visible"] and item["role"] in ACTION_ROLES and exact_action_label(item, REJECT_LABELS)


def is_options_action(item, allow_item):
    if not item["visible"] or item["role"] not in ACTION_ROLES or same_node(item["node"], allow_item["node"]):
        return False
    if has_popup_semantics(item):
        return True
    labels = item_labels(item)
    return any(any(hint in value for hint in OPTION_LABEL_HINTS) for value in labels)


def authorization_cards(items):
    """Return only bounded Reject + Allow + split-menu authorization structures.

    Presence deliberately ignores enabled state. A disabled/remounting card must
    continue to block destructive recovery; actionability is reported separately.
    """
    cards = []
    seen_containers = []
    allow_candidates = [item for item in items if is_allow_action(item) and not has_popup_semantics(item)]
    for allow_item in allow_candidates:
        container = parent_of(allow_item["node"])
        for _ in range(9):
            if container is None or role(container) in BROAD_AUTH_CONTAINER_ROLES:
                break
            actions = [
                item for item in items
                if item["role"] in ACTION_ROLES and item["visible"]
                and is_descendant(item["node"], container)
            ]
            reject_item = next((item for item in actions if is_reject_action(item)), None)
            options_item = next((item for item in actions if is_options_action(item, allow_item)), None)
            if reject_item is not None and options_item is not None:
                if not any(same_node(container, seen) for seen in seen_containers):
                    seen_containers.append(container)
                    cards.append({
                        "container": container,
                        "allow": allow_item,
                        "reject": reject_item,
                        "options": options_item,
                        "actionable": bool(
                            allow_item["enabled"]
                            and reject_item["enabled"]
                            and options_item["enabled"]
                        ),
                    })
                break
            container = parent_of(container)
    return cards


ACTIVITY_TEXT_ROLES = ("static", "paragraph", "text", "status", "notification", "alert")
INTERACTIVE_ACTIVITY_ROLES = (
    "push button", "button", "toggle button", "entry", "menu item", "link", "check box", "radio button"
)


def attribute_map(node):
    try:
        raw = node.getAttributes()
    except Exception:
        return {}
    result = {}
    for entry in raw or []:
        value = str(entry)
        if ":" in value:
            key, item = value.split(":", 1)
        elif "=" in value:
            key, item = value.split("=", 1)
        else:
            continue
        result[key.strip().lower()] = item.strip().lower()
    return result


def assistant_activity_scope(node, max_depth=7):
    current = node
    for _ in range(max_depth + 1):
        if current is None:
            return None
        attrs = attribute_map(current)
        blob = " ".join(f"{key}:{value}" for key, value in attrs.items())
        style = attrs.get("data-markdown-text-style", "")
        tone = attrs.get("data-markdown-text-tone", "")
        css_class = attrs.get("class", "")
        explicit = style == "assistant-message" and tone == "tertiary"
        preserved = (
            "assistant-message" in blob
            and "tertiary" in blob
        )
        class_fallback = (
            "assistant" in css_class
            and "tertiary" in css_class
            and "message" in css_class
        )
        if explicit or preserved or class_fallback:
            return current
        current = parent_of(current)
    return None


def assistant_activity_trace(items):
    entries = []
    seen_scopes = []
    seen_text = set()
    remaining = 12000
    for item in items:
        if remaining <= 0:
            break
        if not item["visible"] or item["role"] in INTERACTIVE_ACTIVITY_ROLES:
            continue
        if item["role"] not in ACTIVITY_TEXT_ROLES:
            continue
        scope = assistant_activity_scope(item["node"])
        if scope is None:
            continue
        value = (item["text"] or item["name"]).strip()
        if not value:
            continue
        normalized_value = " ".join(value.split())
        if not normalized_value or normalized_value in seen_text:
            continue
        # A tertiary scope can expose both a paragraph and its static child.
        # Keep the first bounded semantic text per scope to avoid duplicates.
        if any(same_node(scope, existing) for existing in seen_scopes):
            continue
        seen_scopes.append(scope)
        seen_text.add(normalized_value)
        bounded = value[-min(len(value), remaining, 4000):]
        entries.append(bounded)
        remaining -= len(bounded)
    return entries


BROAD_RESPONSE_ROLES = ("application", "frame", "document", "document web", "desktop frame", "root pane")

def bounded_common_ancestor(left, right, max_depth=8):
    left_ancestors = []
    current = left
    for _ in range(max_depth + 1):
        if current is None: break
        left_ancestors.append(current)
        current = parent_of(current)
    current = right
    for _ in range(max_depth + 1):
        if current is None: break
        for candidate in left_ancestors:
            if same_node(current, candidate):
                return None if role(candidate) in BROAD_RESPONSE_ROLES else candidate
        current = parent_of(current)
    return None

def response_local_copy_evidence(items, marker_index, response_text_items):
    if marker_index < 0 or not response_text_items: return False
    marker_node = items[marker_index]["node"]
    latest_text_item = response_text_items[-1]
    copies = [item for item in items[marker_index + 1:] if item["visible"] and item["role"] in ("push button", "button") and any(word in normalized(item) for word in COPY_WORDS)]
    # Fail closed when more than one assistant response is still mounted after
    # the owned Fabushi user turn. A Copy from an older response must never
    # complete the latest response just because both live under a broad turn.
    for copy_item in reversed(copies):
        scope = bounded_common_ancestor(latest_text_item["node"], copy_item["node"])
        if scope is not None and not is_descendant(marker_node, scope):
            return True
    return False

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
    work_trace = assistant_activity_trace(after) if marker_index >= 0 else []
    after_text = []
    response_text_items = []
    for item in after:
        if item["role"] in ("static", "paragraph", "text") and item["visible"]:
            if assistant_activity_scope(item["node"]) is not None:
                continue
            value = (item["text"] or item["name"]).strip()
            if value and len(value) <= 12000 and "fabushi:" not in value.lower():
                after_text.append(value)
                response_text_items.append(item)
    prose = "\n".join(after_text[-80:])[-16000:]
    copy_after = response_local_copy_evidence(items, marker_index, response_text_items)
    stop = any(
        item["enabled"] and item["role"] in ("push button", "button")
        and any(w in normalized(item) for w in STOP_WORDS)
        for item in items
    )

    auth_cards = authorization_cards(items)
    auth_present = bool(auth_cards)
    auth_actionable = any(card["actionable"] for card in auth_cards)

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
    progress_material = prose + "|" + "\n".join(work_trace) + "|" + str(stop) + "|" + str(auth_present)
    progress = hash_text(progress_material) if marker else None

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
        "assistant_visible_work_trace": work_trace,
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
        "attachment_ready": any_attachment_ready(items),
        "blocker_or_modal": False,
        "progress_fingerprint": progress,
    }

def attachment_ready_for(items, file_name):
    wanted = file_name.strip().lower()
    if not wanted:
        return False
    for item in items:
        if not item["visible"]:
            continue
        value = normalized(item)
        if wanted not in value:
            continue
        if item["role"] in ("push button", "button", "list item", "label"):
            if value.strip() == wanted or any(word in value for word in REMOVE_ATTACHMENT_WORDS):
                return True
        if "attachment" in value or "uploaded" in value or "附件" in value:
            return True
    return False

def any_attachment_ready(items):
    return any(
        item["visible"]
        and item["role"] in ("push button", "button")
        and any(word in normalized(item) for word in REMOVE_ATTACHMENT_WORDS)
        for item in items
    )

def file_chooser_scope(node):
    current = node
    for _ in range(10):
        current = parent_of(current)
        if current is None:
            return None
        current_role = role(current)
        current_label = (node_name(current) + " " + text_of(current)).strip().lower()
        if current_role == "file chooser":
            return current
        if current_role in ("dialog", "window") and any(word in current_label for word in FILE_CHOOSER_WORDS):
            return current
    return None

def desktop_flattened():
    desktop = pyatspi.Registry.getDesktop(0)
    out = []
    for app in desktop:
        try:
            app_name = app.name or ""
        except Exception:
            app_name = ""
        for item in flattened(app):
            item["app_name"] = app_name
            out.append(item)
    return out

def attach_file(path):
    file_name = path.rsplit("/", 1)[-1]
    app = find_app()
    items = flattened(app)
    if attachment_ready_for(items, file_name):
        return True
    trigger = find_named(items, ATTACH_WORDS, roles=("push button", "button", "toggle button"), actionable=True)
    if trigger is None:
        raise RuntimeError("ChatGPT attachment control not found")
    if not action(trigger["node"]):
        raise RuntimeError("ChatGPT attachment control action failed")
    time.sleep(0.25)
    items = flattened(app)
    upload = find_named(items, UPLOAD_WORDS, roles=("menu item", "push button", "button"), actionable=True)
    if upload is not None:
        if not action(upload["node"]):
            raise RuntimeError("ChatGPT upload-files action failed")
        time.sleep(0.35)
    # Native Electron file selection is outside the renderer tree. Open the
    # location entry through the desktop keyboard path, then set the exact path
    # through Accessibility rather than screen coordinates.
    pyatspi.Registry.generateKeyboardEvent(0, "l", pyatspi.KEY_PRESSRELEASE | pyatspi.KEY_CONTROL)
    time.sleep(0.15)
    candidates = [
        item for item in desktop_flattened()
        if item["enabled"] and item["visible"] and item["role"] in ("entry", "text")
        and item.get("app_name") not in APP_NAMES
        and file_chooser_scope(item["node"]) is not None
    ]
    focused = [item for item in candidates if item["focused"]]
    if len(focused) == 1:
        entry = focused[0]["node"]
    else:
        location = [
            item for item in candidates
            if any(word in normalized(item) for word in ("location", "file name", "filename", "位置", "文件名"))
        ]
        if len(location) != 1:
            raise RuntimeError(
                "native attachment file chooser did not expose exactly one focused/location entry"
            )
        entry = location[0]["node"]
    try:
        entry.queryEditableText().setTextContents(path)
    except Exception as exc:
        raise RuntimeError("native attachment file chooser location is not editable") from exc
    pyatspi.Registry.generateKeyboardEvent(65293, None, pyatspi.KEY_SYM)
    return True

def attachment_ready(file_name):
    return attachment_ready_for(flattened(find_app()), file_name)

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
    cards = [card for card in authorization_cards(items) if card["actionable"]]
    if len(cards) != 1:
        return False
    option = cards[0]["options"]
    if not action(option["node"]):
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
    elif op == "attach":
        result = attach_file(sys.argv[2])
    elif op == "attachment-ready":
        result = attachment_ready(sys.argv[2])
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
