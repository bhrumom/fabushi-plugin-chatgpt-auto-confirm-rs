use fabushi_chatgpt_cdp::ChatGptCdp;
use std::fs;

#[tokio::test]
#[ignore = "requires CHROMIUM_CDP_ENDPOINT"]
async fn chromium_fixture_covers_dispatch_approval_terminal_notice_and_reload() {
    let endpoint = std::env::var("CHROMIUM_CDP_ENDPOINT")
        .expect("CHROMIUM_CDP_ENDPOINT is required");
    let path = std::env::temp_dir().join("fabushi-chatgpt-fixture.html");
    fs::write(
        &path,
        r#"<!doctype html>
<html><body>
<button aria-label="GPT-5.6">GPT-5.6</button>
<button aria-label="Extra High">Extra High</button>
<div id="rate">Too many requests <button id="gotit">Got it</button></div>
<section data-testid="approval-card" data-message-id="approval-1">
<span>Use tool?</span><button id="allow">Allow once</button><button id="always">Always allow</button>
</section>
<div id="messages"></div>
<textarea id="prompt-textarea"></textarea>
<button data-testid="send-button" aria-label="Send">Send</button>
<script>
document.getElementById('allow').onclick = () => document.querySelector('[data-testid="approval-card"]').remove();
document.getElementById('gotit').onclick = () => document.getElementById('rate').remove();
document.querySelector('[data-testid="send-button"]').onclick = () => {
  const messages = document.getElementById('messages');
  const user = document.createElement('div');
  user.setAttribute('data-message-author-role', 'user');
  user.textContent = document.getElementById('prompt-textarea').value;
  messages.appendChild(user);
  const assistant = document.createElement('div');
  assistant.setAttribute('data-message-author-role', 'assistant');
  assistant.innerHTML = '<p>fixture done</p><button aria-label="Copy">Copy</button>';
  messages.appendChild(assistant);
};
</script>
</body></html>"#,
    ).unwrap();

    let url = format!("file://{}", path.display());
    let browser = ChatGptCdp::create_target(&endpoint, &url).await.unwrap();
    let initial = browser.snapshot().await.unwrap();
    assert!(initial.waiting_for_approval);
    assert!(initial.rate_limit_notice);
    assert!(initial.approval_card_key.is_some());
    assert!(browser.click_allow_once().await.unwrap());
    assert!(!browser.snapshot().await.unwrap().waiting_for_approval);
    assert!(browser.dismiss_rate_limit_notice().await.unwrap());
    assert!(!browser.snapshot().await.unwrap().rate_limit_notice);
    browser.send_prompt("fixture prompt").await.unwrap();
    let terminal = browser.snapshot().await.unwrap();
    assert_eq!(terminal.user_turns, 1);
    assert_eq!(terminal.assistant_turns, 1);
    assert!(terminal.is_terminal());
    assert_eq!(terminal.assistant_text, "fixture done Copy");
    browser.reload().await.unwrap();
    browser.close_owned_target().await.unwrap();
}
