// Browser test: background refreshes never disturb a text field Owner is
// typing in (on iOS Safari a replaced or blurred field closes the keyboard).
//   node --test web/dev/keyboard.mjs
// Drives a headless Chromium over the DevTools protocol (no npm packages):
// CM_BOARD_CHROME, else Playwright's cached chrome-headless-shell. Without a
// browser the tests skip, or fail with CM_BOARD_BROWSER_REQUIRED=1. It emulates
// an iPhone (Safari user agent, 390x844, touch); it is still Blink, not WebKit.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const REFRESH_MS = 300;
const CYCLES = 5;
const IPHONE_UA = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 "
  + "(KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";

function findBrowser() {
  if (process.env.CM_BOARD_CHROME) return process.env.CM_BOARD_CHROME;
  const cache = path.join(homedir(), ".cache", "ms-playwright");
  if (!existsSync(cache)) return null;
  for (const dir of readdirSync(cache).filter((d) => d.startsWith("chromium_headless_shell")).sort().reverse()) {
    for (const sub of readdirSync(path.join(cache, dir))) {
      const bin = path.join(cache, dir, sub, "chrome-headless-shell");
      if (existsSync(bin)) return bin;
    }
  }
  return null;
}

const browserBin = findBrowser();
const skip = !browserBin && !process.env.CM_BOARD_BROWSER_REQUIRED ? "no headless Chromium found" : false;

async function withServer(fn) {
  const port = 20000 + Math.floor(Math.random() * 2000);
  const proc = spawn("python3", [path.join(here, "fake_server.py"), "--port", String(port)], { stdio: ["ignore", "pipe", "inherit"] });
  await new Promise((resolve, reject) => {
    proc.stdout.once("data", resolve);
    proc.once("exit", (code) => reject(new Error(`fake server exited ${code}`)));
  });
  try {
    await fn(`http://127.0.0.1:${port}`);
  } finally {
    proc.kill();
  }
}

/** A page over CDP: `send(method, params)` and `eval(expression)`. */
async function withPage(fn) {
  assert.ok(browserBin, "no headless Chromium (set CM_BOARD_CHROME)");
  const profile = mkdtempSync(path.join(tmpdir(), "cm-board-kb-"));
  const proc = spawn(browserBin, ["--headless", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
    "--no-first-run", "--no-sandbox", "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
  try {
    const wsUrl = await new Promise((resolve, reject) => {
      let err = "";
      proc.stderr.on("data", (d) => {
        err += d;
        const m = err.match(/DevTools listening on (ws:\/\/\S+)/);
        if (m) resolve(m[1]);
      });
      proc.once("exit", (code) => reject(new Error(`browser exited ${code}: ${err}`)));
    });
    const list = await fetch(`http://${new URL(wsUrl).host}/json/list`).then((r) => r.json());
    const ws = new WebSocket(list.find((t) => t.type === "page").webSocketDebuggerUrl);
    await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
    let id = 0;
    const pending = new Map();
    ws.onmessage = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.id && pending.has(msg.id)) {
        const { resolve, reject } = pending.get(msg.id);
        pending.delete(msg.id);
        if (msg.error) reject(new Error(msg.error.message));
        else resolve(msg.result);
      }
    };
    const send = (method, params = {}) => new Promise((resolve, reject) => {
      id += 1;
      pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params }));
    });
    const evaluate = async (expression) => {
      const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
      if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
      return r.result.value;
    };
    await send("Emulation.setUserAgentOverride", { userAgent: IPHONE_UA, platform: "iPhone" });
    await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 3, mobile: true });
    await send("Emulation.setTouchEmulationEnabled", { enabled: true });
    try {
      await fn({ send, eval: evaluate });
    } finally {
      ws.close();
    }
  } finally {
    proc.kill();
    rmSync(profile, { recursive: true, force: true });
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(page, expression, what, ms = 8000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await page.eval(expression)) return;
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${what}`);
}

/** Focus `selector`, type, put the caret mid-text, then let several refresh
 *  cycles pass (with new agent messages, so the conversation really
 *  re-renders) and check the field never changed under Owner's fingers. */
async function typeThroughRefreshes(page, base, { selector, conversation }) {
  await waitFor(page, `!!document.querySelector(${JSON.stringify(selector)})`, selector);
  await page.eval(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    el.focus();
    window.__field = el;
    const s = document.querySelector(".conv .scroll");
    window.__scroll = s ? s.scrollTop : null;
    window.__pageY = window.scrollY;
    window.__refreshes = 0;
    const orig = window.fetch;
    window.fetch = (...a) => { if (String(a[0]).includes("/api/counts")) window.__refreshes += 1; return orig(...a); };
  })()`);
  await page.send("Input.insertText", { text: "hello world" });
  await page.eval(`window.__field.setSelectionRange(3, 5)`);
  const snapshot = () => page.eval(`(() => {
    const f = window.__field;
    const s = document.querySelector(".conv .scroll");
    return { same: document.activeElement === f, connected: f.isConnected, value: f.value,
             caret: [f.selectionStart, f.selectionEnd], scroll: s ? s.scrollTop : null,
             pageY: window.scrollY, refreshes: window.__refreshes };
  })()`);
  const startScroll = await page.eval("window.__scroll");
  for (let i = 0; i < CYCLES; i += 1) {
    const before = await page.eval("window.__refreshes");
    if (conversation) {
      await fetch(`${base}/api/_dev/post`, {
        method: "POST", headers: { "Content-Type": "application/json", "X-CM-Board": "1" },
        body: JSON.stringify({ conversation, body: `incoming ${i} while typing` }),
      });
    }
    await waitFor(page, `window.__refreshes > ${before}`, `refresh cycle ${i}`);
    if (conversation) {
      // The new message is on screen: the conversation re-rendered under the field.
      await waitFor(page, `document.querySelector(".conv .messages").textContent.includes("incoming ${i} while typing")`,
        `message ${i} rendered`);
    } else {
      await sleep(REFRESH_MS / 2);
    }
    const now = await snapshot();
    assert.ok(now.connected, `cycle ${i}: the field's node was replaced`);
    assert.ok(now.same, `cycle ${i}: the field lost focus`);
    assert.equal(now.value, "hello world", `cycle ${i}: value changed`);
    assert.deepEqual(now.caret, [3, 5], `cycle ${i}: caret moved`);
    if (startScroll !== null) assert.equal(now.scroll, startScroll, `cycle ${i}: the conversation scrolled`);
    assert.equal(now.pageY, 0, `cycle ${i}: the page scrolled`);
  }
  // Typing replaces the selection that survived ("lo").
  await page.send("Input.insertText", { text: "X" });
  assert.equal(await page.eval("window.__field.value"), "helX world");
}

const opts = { skip, timeout: 120_000 };

test("channel composer keeps node, focus, draft and caret through refreshes", opts, () => withServer((base) => withPage(async (page) => {
  await page.send("Page.navigate", { url: `${base}/?refresh_ms=${REFRESH_MS}#conversation=c-swarm` });
  await waitFor(page, `document.querySelectorAll(".conv .msg").length > 0`, "messages");
  await typeThroughRefreshes(page, base, { selector: ".composer textarea", conversation: "c-swarm" });
  // The draft survives a reload too (localStorage).
  await page.send("Page.reload");
  await waitFor(page, `document.querySelector(".composer textarea")?.value === "helX world"`, "draft after reload");
})));

test("DM composer, and a reply in progress, survive refreshes", opts, () => withServer((base) => withPage(async (page) => {
  await page.send("Page.navigate", { url: `${base}/?refresh_ms=${REFRESH_MS}#conversation=d-scout` });
  await waitFor(page, `document.querySelectorAll(".conv .msg").length > 0`, "messages");
  await typeThroughRefreshes(page, base, { selector: ".composer textarea", conversation: "d-scout" });
  // Reply (the thread sheet's "Reply in thread" lands here too).
  await page.eval(`[...document.querySelectorAll(".msg .actions button")].find((b) => b.textContent === "Reply").click()`);
  await waitFor(page, `!!document.querySelector("#composer .replying")`, "reply banner");
  await page.eval(`document.querySelector(".composer textarea").value = ""`);
  await typeThroughRefreshes(page, base, { selector: ".composer textarea", conversation: "d-scout" });
  assert.ok(await page.eval(`!!document.querySelector("#composer .replying")`), "the reply survives");
})));

test("a new DM's composer survives refreshes", opts, () => withServer((base) => withPage(async (page) => {
  await page.send("Page.navigate", { url: `${base}/?refresh_ms=${REFRESH_MS}#dm=agent%3Aswarm-coord` });
  await typeThroughRefreshes(page, base, { selector: ".composer textarea", conversation: null });
})));

test("conversation filter and New DM search survive refreshes", opts, () => withServer((base) => withPage(async (page) => {
  await page.send("Page.navigate", { url: `${base}/?refresh_ms=${REFRESH_MS}#` });
  await waitFor(page, `document.querySelectorAll("#list-rows .row").length > 0`, "list rows");
  await typeThroughRefreshes(page, base, { selector: ".list-pane .search input", conversation: null });
  await page.eval(`document.querySelector(".list-pane .search input").value = ""; document.querySelector(".list-pane .search input").dispatchEvent(new Event("input"))`);
  await page.eval(`document.querySelector(".new-dm").click()`);
  await typeThroughRefreshes(page, base, { selector: ".sheet .search input", conversation: null });
})));
