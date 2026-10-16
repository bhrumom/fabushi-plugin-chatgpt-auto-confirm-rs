const { chromium } = require('playwright');
const { readFile } = require('node:fs/promises');
const { resolve, sep, extname } = require('node:path');
const assert = require('node:assert/strict');
const fs = require('node:fs');

const resources = resolve(process.argv[2]);
const origin = 'https://chatgpt-auto-confirm.fixture.invalid';
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm' };

async function runBrowserSuite() {
  const instance = await chromium.launch();
  let count = 0;
  const context = await instance.newContext({ acceptDownloads: true });
  const external = [];
  await context.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin !== origin) {
      external.push(url.origin);
      return route.abort();
    }
    const path = resolve(resources, '.' + decodeURIComponent(url.pathname));
    if (!path.startsWith(resources + sep)) return route.abort();
    try {
      await route.fulfill({ body: await readFile(path), contentType: mime[extname(path)] || 'application/octet-stream' });
    } catch {
      await route.fulfill({ status: 404, body: '' });
    }
  });
  await context.setOffline(true);
  const page = await context.newPage();
  try {
    await page.goto(origin + '/index.html');
    await page.waitForFunction(() => document.getElementById('status').textContent.includes('已就绪'));
    assert.equal(await page.locator('#add-task').isDisabled(), false);
    assert.equal(await page.locator('#verify-review').isDisabled(), true);
    count++;

    await page.locator('#task-title').fill('<img onerror=alert(1)> 上架任务');
    await page.locator('#task-goal').fill('实现 OMA-001 离线小程序并完成独立验收');
    await page.locator('#add-task').click();
    assert.equal(await page.locator('#selection img').count(), 0);
    assert.equal(await page.locator('#task-count').textContent(), '1');
    assert.match(await page.locator('#prompt-preview').textContent(), /\[Fabushi:task-1-r1\]/);
    count++;

    await page.locator('#review-input').fill('{"taskId":"task-1","round":1,"status":"next","summary":"已生成离线包","next":"继续完成签名与扫描"}');
    await page.locator('#verify-review').click();
    assert.equal(await page.locator('#review-count').textContent(), '1');
    count++;

    await page.locator('#verify-snapshot').click();
    assert.equal(await page.locator('#snapshot-count').textContent(), '1');
    count++;

    const downloadPromise = page.waitForEvent('download');
    await page.locator('#export').click();
    const download = await downloadPromise;
    const report = JSON.parse(fs.readFileSync(await download.path(), 'utf8'));
    assert.equal(report.networkSent, false);
    assert.equal(report.liveCdpAvailable, false);
    assert.equal(report.tasks.length, 1);
    assert.equal(report.reviews.length, 1);
    assert.equal(report.snapshots.length, 1);
    count++;

    const negative = await page.evaluate(async () => {
      const adapter = await import('./pkg/chatgpt_auto_confirm_web_adapter.js');
      let rejects = 0;
      for (const bad of [
        '{"taskId":"wrong","round":1,"status":"complete","summary":"ok","next":""}',
        '{"taskId":"task-1","round":2,"status":"complete","summary":"ok","next":""}',
        '{"taskId":"task-1","round":1,"status":"complete","summary":"","next":""}',
        '{"taskId":"task-1","round":1,"status":"complete","summary":"ok","next":"non-empty"}',
      ]) {
        try { adapter.verify_review_report(bad, 'task-1', 1, 'goal'); } catch { rejects++; }
      }
      const nonTerminal = JSON.parse(adapter.evaluate_snapshot(JSON.stringify({
        url: 'https://chatgpt.com/',
        title: 'Chat',
        user_turns: 1,
        assistant_turns: 1,
        stop_available: false,
        waiting_for_approval: false,
        rate_limit_notice: false,
        copy_available_on_last_assistant: false,
        response_actions_complete: false,
        response_action_turn_bound_to_last: false,
        awaiting_assistant: false,
        assistant_text: 'partial',
        composer_text: '',
      })));
      if (!nonTerminal.isTerminal && nonTerminal.canonicalUrl === null) rejects++;
      return rejects;
    });
    assert.equal(negative, 5);
    count++;

    await page.locator('#task-goal').fill('x'.repeat(17 * 1024));
    await page.locator('#add-task').click();
    assert.match(await page.locator('#selection-summary').textContent(), /超出限制/);
    count++;

    await page.locator('summary').click();
    await page.locator('#remember').check();
    await page.locator('#clear').click();
    assert.equal(await page.locator('#remember').isChecked(), false);
    count++;

    await page.reload();
    await page.waitForFunction(() => document.getElementById('status').textContent.includes('已就绪'));
    assert.equal(await page.locator('#selection li').count(), 0);
    assert.deepEqual(external, []);
    count++;

    await page.screenshot({ path: 'miniapp-ui.png', fullPage: true });
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.screenshot({ path: 'miniapp-ui-dark.png', fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    count++;
    await page.screenshot({ path: 'miniapp-ui-narrow.png', fullPage: true });
    console.log(JSON.stringify({ tests: count, source: process.env.TESTED_COMMIT, result: 'pass', scope: 'offline-browser-fixture-not-Fabushi-installed-acceptance' }));
  } finally {
    await context.close();
    await instance.close();
  }
}

runBrowserSuite().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
