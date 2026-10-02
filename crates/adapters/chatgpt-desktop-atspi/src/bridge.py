import json
import re
import sys
import time
import hashlib

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
HISTORY_ACCESS_ACK_WORDS = (
    "got it", "understood", "i understand", "ok", "okay",
    "明白", "明白了", "知道了", "我知道了", "好的", "好", "确定", "确认", "收到",
)
RATE_LIMIT_TEXT = ("too many requests", "request too frequent", "请求过于频繁")
RATE_LIMIT_NOTICE_ROLES = ("alert", "notification", "status", "dialog", "alert dialog", "alertdialog")
HISTORY_ACCESS_CONTAINER_ROLES = RATE_LIMIT_NOTICE_ROLES + ("panel", "section")
HISTORY_ACCESS_WORDS = (
    "conversation history", "chat history", "previous conversation", "previous conversations",
    "对话记录", "聊天记录", "历史记录", "历史会话",
)
HISTORY_ACCESS_RESTRICTION_WORDS = (
    "access", "view", "load", "restrict", "restricted", "restriction", "limit", "limited", "limitation",
    "访问", "查看", "读取", "限制", "无法", "不能",
)
UNABLE_LOAD_TEXT = ("unable to load", "无法加载此 chatgpt 对话", "无法加载此对话")
INTERRUPTED_TEXT = ("connection interrupted", "连接中断")
LENGTH_LIMIT_TEXT = ("conversation is too long", "maximum length", "对话过长", "达到对话长度")
CACHE_EXPIRED_TEXT = ("stream cache expired", "流缓存已过期")
POLL_TIMEOUT_TEXT = ("stream polling timeout", "轮询超时")
SAFE_POPUP_DISMISS_LABELS = (
    "close", "dismiss", "later", "not now", "maybe later", "skip",
    "关闭", "稍后", "以后再说", "跳过",
)
SENSITIVE_POPUP_WORDS = (
    "login", "log in", "sign in", "authorization", "authorize", "permission", "consent",
    "account", "verify", "verification", "security", "payment", "billing", "purchase", "subscribe",
    "登录", "登陆", "授权", "权限", "同意", "账号", "账户", "验证", "安全验证", "支付", "付款", "订阅",
)
DIALOG_ROLES = ("dialog", "alert dialog", "alertdialog")

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
            "selected": state(node, pyatspi.STATE_SELECTED),
        })
    return out

def normalized(item):
    return (item["name"] + "\n" + item["text"]).strip().lower()

def node_attributes(node):
    try:
        return list(node.getAttributes())
    except Exception:
        return []

def node_uri(node):
    try:
        link = node.queryHyperlink()
        if getattr(link, "nAnchors", 0) == 1:
            value = link.getURI(0)
            if value:
                return value.strip()
    except Exception:
        pass
    attributes = node_attributes(node)
    if isinstance(attributes, dict):
        for key in ("url", "uri", "href"):
            value = attributes.get(key)
            if value:
                return value.strip()
    else:
        for attr in attributes:
            lower = str(attr).lower()
            for prefix in ("url:", "uri:", "href:"):
                if lower.startswith(prefix):
                    value = str(attr)[len(prefix):].strip()
                    if value:
                        return value
    return None

def opaque_conversation_ref(uri):
    return "atspi:" + hashlib.sha256(uri.encode("utf-8", "replace")).hexdigest()

def strong_current_conversation_links(items):
    matches = []
    for item in items:
        if item.get("role") not in ("link", "hyperlink") or not item.get("visible", True):
            continue
        uri = node_uri(item["node"])
        if not uri:
            continue
        attributes = node_attributes(item["node"])
        if isinstance(attributes, dict):
            attrs = "\n".join(f"{key}:{value}" for key, value in attributes.items()).lower()
        else:
            attrs = "\n".join(str(value) for value in attributes).lower()
        current = item.get("selected", False) or any(token in attrs for token in (
            "aria-current:true", "aria-current:page", "current:true", "selected:true"
        ))
        if current:
            matches.append((item, uri))
    return matches

def current_conversation_ref(items):
    matches = strong_current_conversation_links(items)
    if len(matches) != 1:
        return None
    return opaque_conversation_ref(matches[0][1])

def rebind_conversation(target_ref):
    if not re.fullmatch(r"atspi:[0-9a-f]{64}", target_ref or ""):
        return False
    app = find_app()
    items = flattened(app)
    auth_cards = authorization_cards(items)
    harmless, blocked = popup_semantics(items, auth_cards)
    if auth_cards or blocked:
        return False
    candidates = []
    for item in items:
        if item["role"] not in ("link", "hyperlink") or not item["visible"] or not item["enabled"]:
            continue
        uri = node_uri(item["node"])
        if uri and opaque_conversation_ref(uri) == target_ref:
            candidates.append(item)
    if len(candidates) != 1:
        return False
    if not action(candidates[0]["node"]):
        return False
    time.sleep(0.35)
    return current_conversation_ref(flattened(find_app())) == target_ref

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


def popup_dialogs(items):
    return [item for item in items if item["visible"] and item["role"] in DIALOG_ROLES]


def dialog_items(items, dialog):
    return [item for item in items if item["visible"] and is_descendant(item["node"], dialog["node"])]


def history_access_popup(items, auth_cards=None):
    """Find exactly one bounded history-only request-frequency popup.

    The classification intentionally requires three independent signals inside
    the same bounded accessibility container: a request-frequency headline,
    explicit history-access restriction language, and one exact
    acknowledgement action. Transcript text alone can never satisfy this shape.
    Authorization and other sensitive dialogs fail closed.
    """
    auth_cards = authorization_cards(items) if auth_cards is None else auth_cards
    candidates = []
    seen = []
    for item in items:
        if not item["visible"] or not any(token in normalized(item) for token in RATE_LIMIT_TEXT):
            continue
        current = item["node"]
        for _ in range(9):
            if current is None:
                break
            current_role = role(current)
            if current_role in BROAD_AUTH_CONTAINER_ROLES:
                break
            if current_role not in HISTORY_ACCESS_CONTAINER_ROLES:
                current = parent_of(current)
                continue
            scoped = [
                scoped_item for scoped_item in items
                if scoped_item["visible"] and is_descendant(scoped_item["node"], current)
            ]
            material = " ".join(normalized(scoped_item) for scoped_item in scoped).lower()
            has_headline = any(token in material for token in RATE_LIMIT_TEXT)
            has_history = any(token in material for token in HISTORY_ACCESS_WORDS)
            has_restriction = any(token in material for token in HISTORY_ACCESS_RESTRICTION_WORDS)
            nested_auth = any(
                is_descendant(card["container"], current)
                or is_descendant(current, card["container"])
                for card in auth_cards
            )
            sensitive = nested_auth or any(word in material for word in SENSITIVE_POPUP_WORDS)
            acknowledge = [
                scoped_item for scoped_item in scoped
                if scoped_item["role"] in ACTION_ROLES and scoped_item["enabled"]
                and exact_action_label(scoped_item, HISTORY_ACCESS_ACK_WORDS)
            ]
            if has_headline and has_history and has_restriction and not sensitive and len(acknowledge) == 1:
                if not any(same_node(existing, current) for existing in seen):
                    seen.append(current)
                    candidates.append((current, acknowledge[0]))
                break
            current = parent_of(current)
    return candidates[0] if len(candidates) == 1 else None


def popup_semantics(items, auth_cards=None):
    auth_cards = authorization_cards(items) if auth_cards is None else auth_cards
    harmless = []
    blocked = []
    for dialog in popup_dialogs(items):
        nested_auth = any(
            is_descendant(card["container"], dialog["node"])
            or is_descendant(dialog["node"], card["container"])
            for card in auth_cards
        )
        scoped = dialog_items(items, dialog)
        material = " ".join(normalized(item) for item in scoped).lower()
        sensitive = nested_auth or any(word in material for word in SENSITIVE_POPUP_WORDS)
        dismiss = [
            item for item in scoped
            if item["role"] in ACTION_ROLES and item["enabled"]
            and any(label in SAFE_POPUP_DISMISS_LABELS for label in item_labels(item))
        ]
        if not sensitive and len(dismiss) == 1:
            harmless.append((dialog, dismiss[0]))
        else:
            blocked.append(dialog)
    return harmless, blocked


def dismiss_harmless_popup():
    app = find_app()
    items = flattened(app)
    auth_cards = authorization_cards(items)
    history_popup = history_access_popup(items, auth_cards)
    if history_popup is not None:
        return bool(action(history_popup[1]["node"]))
    harmless, blocked = popup_semantics(items, auth_cards)
    if blocked or len(harmless) != 1:
        return False
    return bool(action(harmless[0][1]["node"]))


def rate_limit_notice_container(items):
    auth_cards = authorization_cards(items)
    history_popup = history_access_popup(items, auth_cards)
    history_container = history_popup[0] if history_popup is not None else None
    candidates = []
    for item in items:
        if not item["visible"] or not any(token in normalized(item) for token in RATE_LIMIT_TEXT):
            continue
        current = item["node"]
        notice = None
        for _ in range(9):
            if current is None:
                break
            if role(current) in RATE_LIMIT_NOTICE_ROLES:
                notice = current
                break
            current = parent_of(current)
        if notice is None:
            continue
        if history_container is not None and (
            is_descendant(notice, history_container)
            or is_descendant(history_container, notice)
        ):
            continue
        scoped = [
            scoped_item for scoped_item in items
            if scoped_item["visible"] and is_descendant(scoped_item["node"], notice)
        ]
        material = " ".join(normalized(scoped_item) for scoped_item in scoped).lower()
        nested_auth = any(
            is_descendant(card["container"], notice)
            or is_descendant(notice, card["container"])
            for card in auth_cards
        )
        if nested_auth or any(word in material for word in SENSITIVE_POPUP_WORDS):
            continue
        if not any(same_node(existing, notice) for existing in candidates):
            candidates.append(notice)
    return candidates[0] if len(candidates) == 1 else None


def strict_review_report_projection(prose, response_boundary):
    """Project exact current-response Review JSON without owning task policy."""
    if not prose or not response_boundary:
        return None
    source = prose.strip()
    fence = chr(96) * 3
    if source.startswith(fence):
        source = source[3:]
        if source[:4].lower() == "json":
            source = source[4:]
        source = source.lstrip()
        if not source.rstrip().endswith(fence):
            return None
        source = source.rstrip()[:-3].rstrip()
    try:
        value = json.loads(source)
    except Exception:
        return None
    if not isinstance(value, dict):
        return None
    task_id = value.get("taskId")
    round_value = value.get("round")
    status = value.get("status")
    summary = value.get("summary")
    next_value = value.get("next")
    if not isinstance(task_id, str) or not task_id.strip():
        return None
    if isinstance(round_value, bool) or not isinstance(round_value, int) or not (0 <= round_value <= 0xFFFFFFFF):
        return None
    if status not in ("complete", "next"):
        return None
    if not isinstance(summary, str) or not summary.strip():
        return None
    if next_value is not None and not isinstance(next_value, str):
        return None
    if status == "next" and (not isinstance(next_value, str) or not next_value.strip()):
        return None
    return {
        "task_id": task_id,
        "round": round_value,
        "status": status,
        "summary": summary,
        "next": next_value,
        "response_boundary": response_boundary,
    }


def is_stream_cache_expired_item(item):
    value = normalized(item).strip().rstrip(".!。！")
    return value in CACHE_EXPIRED_TEXT


def retry_stream_cache_expired():
    app = find_app()
    items = flattened(app)
    marker, marker_index = marker_info(items)
    if not marker or marker_index < 0:
        return False
    response_scope, response_text_items, _ = latest_owned_response(items, marker_index)
    if response_scope is None:
        return False
    cache_text = [item for item in response_text_items if is_stream_cache_expired_item(item)]
    if not cache_text:
        return False
    retry = [
        item for item in items
        if item["visible"] and item["enabled"]
        and item["role"] in ACTION_ROLES
        and is_descendant(item["node"], response_scope)
        and any(word in normalized(item) for word in RETRY_WORDS)
    ]
    if len(retry) != 1:
        return False
    return bool(action(retry[0]["node"]))


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

def owned_response_scope(node, marker_node, max_depth=16):
    current = node
    for _ in range(max_depth):
        parent = parent_of(current)
        if parent is None:
            return None
        # The response root is the child immediately below the first ancestor
        # that also contains the owned Fabushi user marker. This keeps a
        # virtualized older response or sibling response from donating prose or
        # Copy evidence to the latest response.
        if is_descendant(marker_node, parent, max_depth=24):
            if same_node(current, marker_node) or is_descendant(marker_node, current):
                return None
            return current
        if role(parent) in BROAD_RESPONSE_ROLES:
            return None if is_descendant(marker_node, current) else current
        current = parent
    return None

def latest_owned_response(items, marker_index):
    if marker_index < 0:
        return None, [], []
    marker_node = items[marker_index]["node"]
    groups = []
    for item in items[marker_index + 1:]:
        if item["role"] not in ("static", "paragraph", "text") or not item["visible"]:
            continue
        if assistant_activity_scope(item["node"]) is not None:
            continue
        value = (item["text"] or item["name"]).strip()
        if not value or len(value) > 12000 or "fabushi:" in value.lower():
            continue
        scope = owned_response_scope(item["node"], marker_node)
        if scope is None:
            continue
        group = next((entry for entry in groups if same_node(scope, entry["scope"])), None)
        if group is None:
            group = {"scope": scope, "items": [], "values": [], "last_index": -1}
            groups.append(group)
        group["items"].append(item)
        group["values"].append(value)
        group["last_index"] = items.index(item)
    if not groups:
        return None, [], []
    latest = max(groups, key=lambda entry: entry["last_index"])
    return latest["scope"], latest["items"], latest["values"]

def response_local_copy_evidence(items, marker_index, response_scope):
    if marker_index < 0 or response_scope is None:
        return False
    for item in reversed(items[marker_index + 1:]):
        if not item["visible"] or item["role"] not in ("push button", "button"):
            continue
        if not any(word in normalized(item) for word in COPY_WORDS):
            continue
        if is_descendant(item["node"], response_scope) or same_node(item["node"], response_scope):
            return True
    return False

def marker_info(items):
    marker_re = re.compile(r"\[Fabushi:([0-9a-fA-F-]{8,})\]")
    composer = unique_composer(items)
    composer_node = composer["node"] if composer is not None else None
    latest = None
    latest_index = -1
    for i, item in enumerate(items):
        if not item["visible"]:
            continue
        if composer_node is not None and (
            same_node(item["node"], composer_node)
            or is_descendant(item["node"], composer_node)
        ):
            continue
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

def composer_candidates(items):
    return [
        item for item in items
        if item["role"] == "entry" and item["enabled"] and item["visible"] and (
            "ask chatgpt" in normalized(item)
            or "message chatgpt" in normalized(item)
            or "询问 chatgpt" in normalized(item)
        )
    ]

def unique_composer(items):
    candidates = composer_candidates(items)
    return candidates[0] if len(candidates) == 1 else None

def composer_draft(node):
    try:
        children = list(node)
    except Exception:
        children = []
    if children:
        parts = []
        for child in children:
            value = text_of(child).replace("\ufffc", "")
            if value.startswith("\n"):
                value = value[1:]
            parts.append(value.rstrip("\n"))
        return "\n".join(parts)
    return text_of(node).replace("\ufffc", "").strip()

def wait_for_composer_draft(expected, timeout_seconds=2.0):
    deadline = time.monotonic() + timeout_seconds
    while True:
        composer = unique_composer(flattened(find_app()))
        if composer is not None and composer_draft(composer["node"]) == expected:
            return composer
        if time.monotonic() >= deadline:
            return None
        time.sleep(0.05)

def explicit_renderer_error(items):
    return any(
        item["visible"]
        and item["role"] in ("heading", "alert")
        and "something went wrong" in normalized(item)
        for item in items
    )

def snapshot():
    app = find_app()
    items = flattened(app)
    composer = unique_composer(items)
    renderer_error = composer is None and explicit_renderer_error(items)

    marker, marker_index = marker_info(items)
    after = items[marker_index + 1 :] if marker_index >= 0 else []
    work_trace = assistant_activity_trace(after) if marker_index >= 0 else []
    response_scope, response_text_items, response_values = latest_owned_response(items, marker_index)
    prose = "\n".join(response_values[-80:])[-16000:]
    copy_after = response_local_copy_evidence(items, marker_index, response_scope)
    stop = any(
        item["enabled"] and item["role"] in ("push button", "button")
        and any(w in normalized(item) for w in STOP_WORDS)
        for item in items
    )

    auth_cards = authorization_cards(items)
    auth_present = bool(auth_cards)
    auth_actionable = any(card["actionable"] for card in auth_cards)
    history_popup = history_access_popup(items, auth_cards)
    harmless_popups, blocked_popups = popup_semantics(items, auth_cards)

    all_text = "\n".join((item["text"] or item["name"]) for item in items if item["visible"]).lower()
    rate_limit = rate_limit_notice_container(items) is not None
    unable_load = any(t in all_text for t in UNABLE_LOAD_TEXT)
    interrupted = any(t in all_text for t in INTERRUPTED_TEXT)
    length_limit = any(t in all_text for t in LENGTH_LIMIT_TEXT)
    cache_expired = bool(response_scope) and any(
        is_stream_cache_expired_item(item) for item in response_text_items
    )
    polling_timeout = any(t in all_text for t in POLL_TIMEOUT_TEXT)
    retryable = renderer_error or any(
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

    draft = composer_draft(composer["node"]) if composer else ""
    response_boundary = hash_text(prose) if marker and prose else None
    strict_review_report = strict_review_report_projection(prose, response_boundary)
    conversation_fingerprint = hash_text((marker or "") + "|" + (response_boundary or "")) if marker else None
    conversation_ref = current_conversation_ref(items)
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
        "conversation_ref": conversation_ref,
        "conversation_fingerprint": conversation_fingerprint,
        "assistant_visible_prose": prose,
        "assistant_visible_work_trace": work_trace,
        "streaming_or_busy": stop,
        "stop_available": stop,
        "authorization_surface_present": auth_present,
        "authorization_actionable": auth_actionable,
        "authorization_settlement": "inactive",
        "response_local_copy": copy_after,
        "strict_review_report": strict_review_report,
        "rate_limit": rate_limit,
        "retryable_error": retryable,
        "unable_to_load_conversation": unable_load,
        "connection_interrupted": interrupted,
        "conversation_length_limit": length_limit,
        "stream_polling_timeout": polling_timeout,
        "stream_cache_expired": cache_expired,
        "hydration": "failed" if renderer_error else ("ready" if composer else "loading"),
        "reasoning_picker_available": picker,
        "selected_reasoning_preset": selected,
        "attachment_ready": any_attachment_ready(items),
        "harmless_popup_present": bool(history_popup or harmless_popups),
        "sensitive_or_unknown_popup_present": bool(blocked_popups),
        "blocker_or_modal": bool(renderer_error or history_popup or harmless_popups or blocked_popups),
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

def fabushi_owned_draft(value):
    return bool(re.search(r"\s\[Fabushi:[0-9a-fA-F]{8,32}(?:\])?\s*$", (value or "").strip()))

def set_composer_text(entry, prompt):
    before = composer_draft(entry)
    replacing_fabushi_draft = (
        bool(before)
        and before != prompt
        and fabushi_owned_draft(before)
        and fabushi_owned_draft(prompt)
    )
    if before and before != prompt and not replacing_fabushi_draft:
        raise RuntimeError("ChatGPT composer contains an unrelated draft; refusing to overwrite it")
    if before == prompt:
        return True

    try:
        editable = entry.queryEditableText()
    except Exception:
        editable = None
    if editable is not None:
        try:
            editable.setTextContents(prompt)
        except Exception as exc:
            raise RuntimeError("ChatGPT composer EditableText update failed") from exc
    else:
        if not state(entry, pyatspi.STATE_FOCUSABLE) or not state(entry, pyatspi.STATE_EDITABLE):
            raise RuntimeError("ChatGPT composer lacks a safe editable keyboard surface")
        focused = False
        try:
            focused = bool(entry.queryComponent().grabFocus())
        except Exception:
            focused = False
        activated = action(entry)
        if not activated and not focused:
            raise RuntimeError("ChatGPT composer could not activate its keyboard surface")
        # Chromium may retain AT-SPI focus on document web while ProseMirror
        # owns the internal caret. We therefore do not treat STATE_FOCUSED as
        # the safety proof. The exact semantic draft readback below is.
        if "\n" in prompt:
            raise RuntimeError("ChatGPT desktop prepared prompt must be single-line for safe AT-SPI input")
        if replacing_fabushi_draft:
            pyatspi.Registry.generateKeyboardEvent(
                0, "a", pyatspi.KEY_PRESSRELEASE | pyatspi.KEY_CONTROL
            )
            pyatspi.Registry.generateKeyboardEvent(65288, None, pyatspi.KEY_SYM)
            if wait_for_composer_draft("") is None:
                raise RuntimeError("ChatGPT composer did not clear the stale Fabushi draft")
        pyatspi.Registry.generateKeyboardEvent(0, prompt, pyatspi.KEY_STRING)

    if wait_for_composer_draft(prompt) is None:
        raise RuntimeError("ChatGPT composer did not expose the exact prepared prompt")
    return True

def send_prompt(prompt):
    app = find_app()
    items = flattened(app)
    composer = unique_composer(items)
    if composer is None:
        raise RuntimeError("ChatGPT composer is not uniquely ready")
    set_composer_text(composer["node"], prompt)
    time.sleep(0.15)
    items = flattened(app)
    button = find_named(items, SEND_WORDS, roles=("push button", "button"), actionable=True)
    if button is None:
        raise RuntimeError("ChatGPT Send control is not actionable after composing")
    if not action(button["node"]):
        raise RuntimeError("ChatGPT Send action failed")
    return True

def new_chat_candidates(items):
    labels = {"new chat", "新聊天", "新对话"}
    return [
        item for item in items
        if item["role"] in ("push button", "button")
        and item["visible"] and item["enabled"]
        and state(item["node"], pyatspi.STATE_SHOWING)
        and attribute_map(item["node"]).get("tag") == "button"
        and (item["name"] or item["text"]).strip().lower() in labels
    ]

def start_fresh():
    app = find_app()
    items = flattened(app)
    candidates = new_chat_candidates(items)
    if len(candidates) != 1:
        raise RuntimeError(f"New chat control is not uniquely actionable: {len(candidates)} candidates")
    if not action(candidates[0]["node"]):
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
    notice = rate_limit_notice_container(items)
    if notice is None:
        return False
    buttons = [
        item for item in items
        if item["visible"] and item["enabled"]
        and item["role"] in ("push button", "button")
        and is_descendant(item["node"], notice)
        and any(label in GOT_IT_WORDS for label in item_labels(item))
    ]
    if len(buttons) != 1:
        return False
    return bool(action(buttons[0]["node"]))

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

def rate_limit_contract_self_test():
    class FakeNode:
        def __init__(self, node_role, name, parent=None, attributes=None):
            self._role = node_role
            self.name = name
            self.parent = parent
            self._attributes = attributes or []
        def getRoleName(self):
            return self._role
        def getAttributes(self):
            return self._attributes

    def item(node):
        return {
            "node": node,
            "role": node.getRoleName(),
            "name": node.name,
            "text": node.name,
            "visible": True,
            "enabled": True,
        }

    document = FakeNode("document web", "ChatGPT")

    user = FakeNode("section", "user", document)
    quoted = FakeNode("paragraph", "请求过于频繁", user)
    if rate_limit_notice_container([item(document), item(user), item(quoted)]) is not None:
        return False
    if history_access_popup([item(document), item(user), item(quoted)]) is not None:
        return False

    assistant = FakeNode("section", "assistant", document)
    quoted_assistant = FakeNode("paragraph", "too many requests", assistant)
    if rate_limit_notice_container(
        [item(document), item(assistant), item(quoted_assistant)]
    ) is not None:
        return False

    history_panel = FakeNode("panel", "请求过于频繁", document)
    history_text = FakeNode(
        "paragraph", "暂时限制访问聊天记录，请稍后再查看历史会话", history_panel
    )
    history_ack = FakeNode("push button", "明白了", history_panel)
    history_items = [
        item(document), item(history_panel), item(history_text), item(history_ack)
    ]
    history = history_access_popup(history_items)
    if history is None or not same_node(history[0], history_panel):
        return False
    if not same_node(history[1]["node"], history_ack):
        return False
    if rate_limit_notice_container(history_items) is not None:
        return False

    notice = FakeNode("alert", "请求过于频繁", document)
    notice_text = FakeNode("paragraph", "请稍等几分钟后再重试", notice)
    notice_ack = FakeNode("push button", "明白了", notice)
    current_request_items = [item(document), item(notice), item(notice_text), item(notice_ack)]
    if not same_node(rate_limit_notice_container(current_request_items), notice):
        return False
    if history_access_popup(current_request_items) is not None:
        return False

    auth_dialog = FakeNode("dialog", "Authorization", document)
    auth_text = FakeNode(
        "paragraph", "请求过于频繁，暂时限制访问聊天记录；authorization required", auth_dialog
    )
    auth_allow = FakeNode("push button", "Allow", auth_dialog)
    auth_reject = FakeNode("push button", "Reject", auth_dialog)
    auth_options = FakeNode("push button", "Options", auth_dialog, ["haspopup:true"])
    auth_ack = FakeNode("push button", "Got it", auth_dialog)
    auth_items = [
        item(document), item(auth_dialog), item(auth_text), item(auth_allow),
        item(auth_reject), item(auth_options), item(auth_ack),
    ]
    if history_access_popup(auth_items) is not None:
        return False
    if rate_limit_notice_container(auth_items) is not None:
        return False

    return True


def strict_review_report_contract_self_test():
    boundary = "response-boundary-1"
    valid = strict_review_report_projection(
        '{"taskId":"task-1","round":7,"status":"next","summary":"continue","next":"do more"}',
        boundary,
    )
    if valid != {
        "task_id": "task-1",
        "round": 7,
        "status": "next",
        "summary": "continue",
        "next": "do more",
        "response_boundary": boundary,
    }:
        return False
    fence = chr(96) * 3
    fenced = strict_review_report_projection(
        fence + 'json\n{"taskId":"task-1","round":7,"status":"complete","summary":"done","next":""}\n' + fence,
        boundary,
    )
    if fenced is None or fenced["status"] != "complete":
        return False
    if strict_review_report_projection(
        'prefix {"taskId":"task-1","round":7,"status":"complete","summary":"done","next":""}',
        boundary,
    ) is not None:
        return False
    if strict_review_report_projection(
        '{"taskId":"task-1","round":7,"status":"next","summary":"continue","next":""}',
        boundary,
    ) is not None:
        return False
    return strict_review_report_projection(
        '{"taskId":"task-1","round":7,"status":"complete","summary":"done","next":""}',
        None,
    ) is None


def popup_contract_self_test():
    class FakeNode:
        def __init__(self, node_role, name, parent=None, attributes=None):
            self._role = node_role
            self.name = name
            self.parent = parent
            self._attributes = attributes or []
        def getRoleName(self):
            return self._role
        def getAttributes(self):
            return self._attributes

    def item(node, enabled=True):
        return {
            "node": node,
            "role": node.getRoleName(),
            "name": node.name,
            "text": node.name,
            "visible": True,
            "enabled": enabled,
        }

    safe_dialog = FakeNode("dialog", "Product tip")
    safe_text = FakeNode("paragraph", "Try this new feature", safe_dialog)
    safe_close = FakeNode("push button", "Close", safe_dialog)
    harmless, blocked = popup_semantics([item(safe_dialog), item(safe_text), item(safe_close)], [])
    if len(harmless) != 1 or blocked or not same_node(harmless[0][1]["node"], safe_close):
        return False

    sensitive_dialog = FakeNode("dialog", "Account verification")
    sensitive_text = FakeNode("paragraph", "Verify your account", sensitive_dialog)
    sensitive_close = FakeNode("push button", "Close", sensitive_dialog)
    harmless, blocked = popup_semantics(
        [item(sensitive_dialog), item(sensitive_text), item(sensitive_close)], []
    )
    if harmless or len(blocked) != 1:
        return False

    auth_dialog = FakeNode("dialog", "Connector access")
    auth_panel = FakeNode("panel", "Approval", auth_dialog)
    auth_allow = FakeNode("push button", "Allow", auth_panel)
    auth_reject = FakeNode("push button", "Reject", auth_panel)
    auth_options = FakeNode("push button", "Options", auth_panel, ["haspopup:true"])
    auth_close = FakeNode("push button", "Close", auth_dialog)
    auth_items = [
        item(auth_dialog), item(auth_panel), item(auth_allow), item(auth_reject),
        item(auth_options), item(auth_close),
    ]
    cards = authorization_cards(auth_items)
    if len(cards) != 1:
        return False
    harmless, blocked = popup_semantics(auth_items, cards)
    return not harmless and len(blocked) == 1


def response_boundary_contract_self_test():
    class FakeNode:
        def __init__(self, node_role, name, parent=None):
            self._role = node_role
            self.name = name
            self.parent = parent
        def getRoleName(self):
            return self._role

    document = FakeNode("document web", "ChatGPT")
    conversation = FakeNode("section", "conversation", document)
    user = FakeNode("section", "user", conversation)
    marker = FakeNode("static", "[Fabushi:12345678]", user)
    old_response = FakeNode("section", "old response", conversation)
    old_text = FakeNode("paragraph", "old answer", old_response)
    old_copy = FakeNode("push button", "Copy", old_response)
    latest_response = FakeNode("section", "latest response", conversation)
    latest_text = FakeNode("paragraph", "latest answer", latest_response)
    latest_copy = FakeNode("push button", "Copy", latest_response)
    items = [
        {"node": marker, "role": "static", "name": marker.name, "text": marker.name, "visible": True, "enabled": True},
        {"node": old_text, "role": "paragraph", "name": old_text.name, "text": old_text.name, "visible": True, "enabled": True},
        {"node": old_copy, "role": "push button", "name": "Copy", "text": "", "visible": True, "enabled": True},
        {"node": latest_text, "role": "paragraph", "name": latest_text.name, "text": latest_text.name, "visible": True, "enabled": True},
        {"node": latest_copy, "role": "push button", "name": "Copy", "text": "", "visible": True, "enabled": True},
    ]
    scope, _, values = latest_owned_response(items, 0)
    if not same_node(scope, latest_response) or values != ["latest answer"]:
        return False
    if not response_local_copy_evidence(items, 0, scope):
        return False
    items[-1]["visible"] = False
    if response_local_copy_evidence(items, 0, scope):
        return False
    return True

def conversation_ref_contract_self_test():
    class FakeHyperlink:
        def __init__(self, uri):
            self.uri = uri
            self.nAnchors = 1
        def getURI(self, index):
            return self.uri if index == 0 else None
    class FakeNode:
        def __init__(self, node_role, name, uri=None, attributes=None):
            self._role = node_role
            self.name = name
            self._uri = uri
            self._attributes = attributes or []
        def getRoleName(self):
            return self._role
        def getAttributes(self):
            return self._attributes
        def queryHyperlink(self):
            if not self._uri:
                raise RuntimeError("no hyperlink")
            return FakeHyperlink(self._uri)
    def item(node, selected=False):
        return {"node":node,"role":node.getRoleName(),"name":node.name,"text":node.name,"visible":True,"enabled":True,"selected":selected}
    uri = "chatgpt://conversation/native-opaque-123"
    current = item(FakeNode("link", "Current title", uri), True)
    transcript = item(FakeNode("link", "chatgpt://conversation/native-opaque-123", None), False)
    ref = current_conversation_ref([transcript, current])
    if ref != opaque_conversation_ref(uri):
        return False
    ambiguous = item(FakeNode("link", "Duplicate", "chatgpt://conversation/other"), True)
    if current_conversation_ref([current, ambiguous]) is not None:
        return False
    attr_current = item(FakeNode("link", "Current", None, ["href:" + uri, "aria-current:page"]), False)
    return current_conversation_ref([attr_current]) == opaque_conversation_ref(uri)


def new_chat_locator_contract_self_test():
    class FakeNode:
        def __init__(self, showing, tag):
            self.showing = showing
            self.tag = tag
    nav = FakeNode(True, "button")
    thread_named_new_chat = FakeNode(True, "div")
    hidden_nav = FakeNode(False, "button")
    items = [
        {"node": thread_named_new_chat, "role": "button", "name": "New chat", "text": "", "visible": True, "enabled": True},
        {"node": hidden_nav, "role": "button", "name": "New chat", "text": "", "visible": True, "enabled": True},
        {"node": nav, "role": "button", "name": "New chat", "text": "", "visible": True, "enabled": True},
    ]
    old_state = globals().get("state")
    old_attrs = globals().get("attribute_map")
    old_showing = getattr(pyatspi, "STATE_SHOWING", None)
    pyatspi.STATE_SHOWING = 1004
    globals()["state"] = lambda node, which: node.showing if which == pyatspi.STATE_SHOWING else False
    globals()["attribute_map"] = lambda node: {"tag": node.tag}
    try:
        candidates = new_chat_candidates(items)
        return len(candidates) == 1 and candidates[0]["node"] is nav
    finally:
        globals()["state"] = old_state
        globals()["attribute_map"] = old_attrs
        if old_showing is None:
            try: delattr(pyatspi, "STATE_SHOWING")
            except Exception: pass
        else:
            pyatspi.STATE_SHOWING = old_showing

def dispatch_marker_contract_self_test():
    class FakeNode:
        def __init__(self, parent=None): self.parent = parent
    composer = FakeNode()
    draft = FakeNode(composer)
    transcript = FakeNode()
    old_unique = globals().get("unique_composer")
    old_same = globals().get("same_node")
    old_desc = globals().get("is_descendant")
    globals()["unique_composer"] = lambda items: {"node": composer}
    globals()["same_node"] = lambda left, right: left is right
    globals()["is_descendant"] = lambda node, root: getattr(node, "parent", None) is root
    try:
        hidden_history = {"node": transcript, "text": "old [Fabushi:baadf00d-0002]", "name": "", "visible": False}
        draft_item = {"node": draft, "text": "draft [Fabushi:deadbeef-0000]", "name": "", "visible": True}
        committed_item = {"node": transcript, "text": "sent [Fabushi:feedface-0001]", "name": "", "visible": True}
        if marker_info([hidden_history]) != (None, -1): return False
        if marker_info([draft_item]) != (None, -1): return False
        marker, index = marker_info([hidden_history, draft_item, committed_item])
        return marker == "feedface-0001" and index == 2
    finally:
        globals()["unique_composer"] = old_unique
        globals()["same_node"] = old_same
        globals()["is_descendant"] = old_desc

def renderer_error_contract_self_test():
    class FakeNode:
        def __init__(self, node_role, name, visible=True):
            self._role = node_role
            self.name = name
            self._visible = visible
        def getRoleName(self): return self._role
        def getState(self):
            visible = self._visible
            class State:
                def contains(self, which):
                    return visible
            return State()
        def queryText(self):
            value = self.name
            class Text:
                def getText(self, start, end): return value
            return Text()
    def item(role_name, value, visible=True):
        node = FakeNode(role_name, value, visible)
        return {
            "node": node, "role": role_name, "name": value, "text": value,
            "visible": visible, "enabled": True, "focused": False,
        }
    if not explicit_renderer_error([item("heading", "Something went wrong…")]):
        return False
    if explicit_renderer_error([item("paragraph", "Something went wrong…")]):
        return False
    if explicit_renderer_error([item("heading", "Something went wrong…", False)]):
        return False
    if explicit_renderer_error([item("heading", "Normal heading")]):
        return False
    return True

def composer_write_contract_self_test():
    values = {"draft": "", "typed": []}
    pyatspi.STATE_FOCUSABLE, pyatspi.STATE_EDITABLE, pyatspi.STATE_FOCUSED = 1001, 1002, 1003
    pyatspi.KEY_STRING = 8
    class FakeState:
        def contains(self, which):
            return which in (pyatspi.STATE_FOCUSABLE, pyatspi.STATE_EDITABLE)
    class FakeComponent:
        def grabFocus(self):
            return True
    class FakeAction:
        nActions = 1
        def getName(self, index):
            return "activate"
        def doAction(self, index):
            return True
    class FakeText:
        def __init__(self, getter): self.getter = getter
        def getText(self, start, end): return self.getter()
    class FakeParagraph:
        name = ""
        def getRoleName(self): return "paragraph"
        def queryText(self): return FakeText(lambda: values["draft"] if values["draft"] else "\n\ufffc")
    class FakeEntry:
        name = "Ask ChatGPT"
        def __iter__(self): return iter([FakeParagraph()])
        def getRoleName(self): return "entry"
        def getState(self): return FakeState()
        def queryEditableText(self): raise NotImplementedError()
        def queryComponent(self): return FakeComponent()
        def queryAction(self): return FakeAction()
        def queryText(self): return FakeText(lambda: "\ufffc")
    pyatspi.KEY_PRESSRELEASE, pyatspi.KEY_CONTROL, pyatspi.KEY_SYM = 16, 32, 64
    class FakeRegistry:
        @staticmethod
        def generateKeyboardEvent(keyval, text, synth):
            if synth == pyatspi.KEY_STRING:
                values["typed"].append(text)
                values["draft"] += text
                return
            if synth == (pyatspi.KEY_PRESSRELEASE | pyatspi.KEY_CONTROL) and text == "a":
                return
            if synth == pyatspi.KEY_SYM and keyval == 65288:
                values["draft"] = ""
                return
            raise RuntimeError("unexpected keyboard input")
    pyatspi.Registry = FakeRegistry
    old_find_app = globals().get("find_app")
    old_flattened = globals().get("flattened")
    fake = FakeEntry()
    globals()["find_app"] = lambda: object()
    globals()["flattened"] = lambda app: [{
        "node": fake, "role": "entry", "name": "Ask ChatGPT", "text": "\ufffc",
        "enabled": True, "visible": True, "focused": False,
    }]
    try:
        prepared = "alpha beta [Fabushi:deadbeef]"
        if composer_draft(fake) != "": return False
        if not set_composer_text(fake, prepared): return False
        if values["draft"] != prepared or values["typed"] != [prepared]: return False
        values["draft"] = "unrelated draft"
        try:
            set_composer_text(fake, "different [Fabushi:deadbeef]")
        except RuntimeError as exc:
            if "unrelated draft" not in str(exc): return False
        else:
            return False
        values["draft"] = "stale task [Fabushi:feedface"
        values["typed"] = []
        if not set_composer_text(fake, "replacement [Fabushi:deadbeef]"): return False
        if values["draft"] != "replacement [Fabushi:deadbeef]": return False
        if values["typed"] != ["replacement [Fabushi:deadbeef]"]: return False
        values["draft"] = ""
        try:
            set_composer_text(fake, "alpha\\nbeta")
        except RuntimeError as exc:
            return "single-line" in str(exc)
        return False
    finally:
        if old_find_app is not None: globals()["find_app"] = old_find_app
        if old_flattened is not None: globals()["flattened"] = old_flattened


def main():
    op = sys.argv[1]
    if op == "contract-new-chat-locator":
        result = new_chat_locator_contract_self_test()
    elif op == "contract-dispatch-marker":
        result = dispatch_marker_contract_self_test()
    elif op == "contract-renderer-error":
        result = renderer_error_contract_self_test()
    elif op == "contract-composer-write":
        result = composer_write_contract_self_test()
    elif op == "contract-conversation-ref":
        result = conversation_ref_contract_self_test()
    elif op == "contract-response-boundary":
        result = response_boundary_contract_self_test()
    elif op == "contract-rate-limit":
        result = rate_limit_contract_self_test()
    elif op == "contract-review-report":
        result = strict_review_report_contract_self_test()
    elif op == "contract-popup":
        result = popup_contract_self_test()
    elif op == "snapshot":
        result = snapshot()
    elif op == "send":
        result = send_prompt(sys.argv[2])
    elif op == "fresh":
        result = start_fresh()
    elif op == "rebind":
        result = rebind_conversation(sys.argv[2])
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
    elif op == "dismiss-harmless-popup":
        result = dismiss_harmless_popup()
    elif op == "retry-stream-cache-expired":
        result = retry_stream_cache_expired()
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
