import assert from "node:assert/strict";
import { createHash, webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
const route = readFileSync(new URL("../src/admin.rs", import.meta.url), "utf8");
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const style = html.match(/<style>([\s\S]*?)<\/style>/)[1];

function loadPage(fetch) {
  const elements = new Map();
  const windowEvents = new Map();
  const timers = [];
  const fields = {
    signin: ["email", "password"], "grant-form": ["grant-password"],
    "create-form": ["create-email", "create-name"], "address-form": ["new-address"],
    "lidarr-form": ["lidarr-address", "lidarr-key"], "music-login-form": ["music-user", "music-password"],
  };
  const node = (tag, id) => ({
    tag, id, textContent: "", value: "", disabled: false, hidden: id === "admin", open: false,
    events: new Map(), children: [],
    append(...children) { this.children.push(...children); },
    replaceChildren() { this.children = []; },
    addEventListener(name, handler) { this.events.set(name, handler); },
    reset() {
      for (const key of fields[id] ?? []) {
        if (elements.has(key)) elements.get(key).value = "";
      }
    },
    focus() {},
    showModal() { this.open = true; },
    close() { this.open = false; },
    set innerHTML(_) { assert.fail("Untrusted text reached innerHTML"); },
    set outerHTML(_) { assert.fail("Untrusted text reached outerHTML"); },
    insertAdjacentHTML() { assert.fail("Untrusted text reached insertAdjacentHTML"); },
  });
  const from = id => {
    if (!elements.has(id)) elements.set(id, node(null, id));
    return elements.get(id);
  };
  let currentTime = Date.now();
  const context = {
    document: {
      getElementById: from,
      createElement(tag) {
        assert.ok(["li", "details", "summary", "div", "button"].includes(tag), tag);
        return node(tag);
      },
    },
    window: { addEventListener(name, handler) { windowEvents.set(name, handler); } },
    crypto: webcrypto,
    setTimeout(fn, delay) { timers.push({ fn, delay }); return timers.length; },
    clearTimeout: () => {},
    TextDecoder,
    TextEncoder,
    URL,
    Date: class extends Date { static now() { return currentTime; } },
    AbortController,
    fetch,
  };
  const instrumentation = "globalThis.forTest = { writeText, parseReply };";
  runInNewContext(script.replace(/\}\)\(\);\s*$/, instrumentation + "})();"), context);
  const findButton = (node, label) => {
    if (node.tag === "button" && node.textContent === label) return node;
    for (const child of node.children) {
      const found = findButton(child, label);
      if (found) return found;
    }
  };
  return {
    from,
    windowEvents,
    timers,
    advance(ms) { currentTime += ms; },
    submit(id, values = {}) {
      for (const [field, value] of Object.entries(values)) from(field).value = value;
      return from(id).events.get("submit")({ preventDefault() {} });
    },
    click(id) { return from(id).events.get("click")(); },
    press(container, label) {
      const button = findButton(from(container), label);
      assert.ok(button, label);
      return button.events.get("click")();
    },
    ...context.forTest,
    async signIn(email, password) {
      from("email").value = email;
      from("password").value = password;
      await from("signin").events.get("submit")({ preventDefault() {} });
    },
  };
}

test("CSP grants match exactly the inline script and stylesheet", () => {
  for (const [type, content] of [["script", script], ["style", style]]) {
    const digest = createHash("sha256").update(content).digest("base64");
    assert.ok(route.includes(type + "-src 'sha256-" + digest + "'"));
  }
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i);
  assert.doesNotMatch(html, /\/\//);
});

test("submit calls the sign-in route and shows the bare 501 as not built", async () => {
  const page = loadPage(async (path, options) => {
    assert.equal(path, "/nexus/api/signin");
    assert.equal(options.credentials, "omit");
    assert.equal(options.redirect, "error");
    assert.equal(options.cache, "no-store");
    assert.equal(options.headers["Content-Type"], "application/json");
    assert.equal(options.headers["X-Nexus-Admin"], "1");
    assert.equal(options.headers.Authorization, undefined);
    const body = JSON.parse(options.body);
    assert.match(body.device_secret, /^[0-9a-f]{64}$/);
    assert.deepEqual(body.credential, { password: {
      email: "sample@example.test", password: "example admin password",
    } });
    return new Response(null, { status: 501 });
  });
  await page.signIn("sample@example.test", "example admin password");
  assert.equal(page.from("status").textContent, "Sign-in has not been built yet.");
  assert.equal(page.from("password").value, "");
  assert.equal(page.from("email").value, "");
  assert.equal(page.from("admin").hidden, true);
});

test("names and address examples reach only the text sink", async () => {
  const email = "<script>example()</script>@example.test";
  const device = "<script>exampleDevice()</script>";
  const address = "<script>exampleAddress()</script>";
  const token = "14".repeat(32);
  const response = '{"done":{"result":{"person":{"id":7,"email":' +
    JSON.stringify(email) + ',"admin":true},"session":"' + token +
    '","music":"none_set_yet","restore":18446744073709551615},' +
    '"receipt":{"stamp":{"account":7,"restore":18446744073709551615}}}}';
  const page = loadPage(async (path, options) => {
    if (path === "/nexus/api/signin") return new Response(response, { status: 200 });
    if (path !== "/nexus/api/signout") return new Response(null, { status: 503 });
    assert.equal(options.headers.Authorization, "Bearer " + token);
    assert.match(options.body, /"restore":18446744073709551615/);
    assert.equal(JSON.parse(options.body).revoke, null);
    return new Response('{"done":{"result":{}}}', { status: 200 });
  });
  await page.signIn(email, "example admin password");
  assert.equal(page.from("admin").hidden, false);
  assert.equal(page.from("identity").textContent, email);
  for (const value of [device, address]) {
    page.writeText(page.from("identity"), value);
    assert.equal(page.from("identity").textContent, value);
  }
  await page.from("signout").events.get("click")();
  assert.equal(page.from("identity").textContent, "");
  assert.equal(page.from("admin").hidden, true);
  assert.equal(page.from("status").textContent, "Signed out.");
});

test("whole JSON rejects duplicate keys, too much nesting and invalid Unicode", () => {
  const page = loadPage(() => assert.fail("No fetch expected"));
  assert.throws(() => page.parseReply('{"a": 1,"\\u0061":2}'));
  assert.throws(() => page.parseReply("[".repeat(17) + "0" + "]".repeat(17)));
  assert.throws(() => page.parseReply('{"ignored":"\\ud800"}'));
  assert.throws(() => page.parseReply('{"incomplete":'));
  assert.equal(page.parseReply('{"value":18446744073709551615}').value, 18446744073709551615n);
});

test("authorized reads render text, paginate losslessly, and rearm idle expiry", async () => {
  const token = "39".repeat(32);
  const injected = "<script>sample()</script>";
  const requests = [];
  const done = result => new Response(JSON.stringify({ done: result }));
  const page = loadPage(async (path, options) => {
    if (path === "/nexus/api/signin") {
      return done({
        result: { person: { id: 1, email: "owner@example.test", admin: true }, session: token,
          music: "none_set_yet", restore: 2 },
        receipt: { stamp: { account: 1, restore: 2 } },
      });
    }
    requests.push([path, options]);
    if (path === "/nexus/api/admin/accounts") {
      const after = JSON.parse(options.body).after;
      return after === null ?
        new Response('{"done":{"rows":[{"id":18446744073709551614,"email":"one@example.test",' +
          '"name":' + JSON.stringify(injected) + ',"admin":false,"enabled":true,"requests_enabled":true}],"more":true}}') :
        new Response('{"done":{"rows":[{"id":18446744073709551615,"email":"two@example.test",' +
          '"name":"Two","admin":false,"enabled":true,"requests_enabled":false}],"more":false}}');
    }
    if (path === "/nexus/api/admin/devices") {
      return done({ rows: [{ account: 1, device: 4, name: injected, kind: "phone",
        app_version: "1.2.3", last_seen_ms: 0, session: null }], more: false });
    }
    if (path === "/nexus/api/admin/music-server") return done({ internal: injected, public: null });
    if (path === "/nexus/api/admin/diagnostics-default") return done({ enabled: true });
    throw new Error("Unexpected request " + path);
  });

  await page.signIn("owner@example.test", "example admin password");
  await new Promise(setImmediate);
  assert.equal(requests.length, 4);
  for (const [, options] of requests) {
    assert.equal(options.headers.Authorization, "Bearer " + token);
    assert.deepEqual(JSON.parse(options.body).head, { speaks: [1, 1], restore: 2 });
  }
  assert.ok(page.from("accounts-list").children[0].textContent.includes(injected));
  assert.ok(page.from("devices-list").children[0].textContent.includes(injected));
  assert.equal(page.from("music-internal").textContent, injected);
  assert.equal(page.from("music-public").textContent, "Unset");
  assert.equal(page.from("diagnostics-status").textContent, "New devices send diagnostics by default.");
  assert.equal(page.from("requests-list").children[0].textContent, injected + " · On");
  assert.equal(page.from("accounts-more").hidden, false);
  await page.from("accounts-more").events.get("click")();
  assert.match(requests.at(-1)[1].body, /"after":18446744073709551614/);
  assert.equal(page.from("accounts-list").children.length, 2);
  assert.equal(page.from("accounts-more").hidden, true);
  assert.equal(page.timers.length, 6);
  assert.ok(page.timers.every(timer => timer.delay === 30 * 60 * 1000));

  page.windowEvents.get("pagehide")();
  assert.equal(page.from("admin").hidden, true);
  assert.equal(page.from("accounts-list").children.length, 0);
  assert.equal(page.from("devices-list").children.length, 0);
  assert.equal(page.from("music-internal").textContent, "");
});

const testAddress = "https:" + "/" + "/music.example.test";
const internalAddress = "http:" + "/" + "/music-inside.test:4533";
const lidarrAddress = "http:" + "/" + "/requests-inside.test:8686";
const encodedToken = "39".repeat(32);
const adminPassword = "only an example administrator password";
const typedMusicPassword = "not a real music password";
const lidarrKey = "sample-lidarr-key";
const code = "01AB234CDE56";
const settle = () => new Promise(setImmediate);

function adminStandIn() {
  const calls = [];
  let pendingGrant = null;
  let grantCount = 0;
  let nextRefusal;
  let nextCode = code;
  let enabled = true;
  let canRequest = true;
  let isAdmin = false;
  let hasAccount = true;
  let publicAddress = testAddress;
  let diagnostics = true;
  let hasSession = true;
  const id = "18446744073709551615";
  const answer = (result, status = 200) => new Response(JSON.stringify(result), { status });
  const done = result => answer({ done: result });
  const parse = raw => JSON.parse(raw.replace(/:\s*(\d{16,})(?=[,}])/g, ':"$1"'));
  const fetch = async (path, options) => {
    if (path === "/nexus/api/signin") {
      return done({
        result: { person: { id: 1, email: "owner@example.test", admin: true }, session: encodedToken,
          music: "none_set_yet", restore: 2 },
        receipt: { stamp: { account: 1, restore: 2 } },
      });
    }
    if (path === "/nexus/api/signout") return done({ result: {} });
    assert.equal(options.headers.Authorization, "Bearer " + encodedToken);
    assert.equal(options.credentials, "omit");
    assert.equal(options.cache, "no-store");
    const data = parse(options.body);
    assert.deepEqual(data.head, { speaks: [1, 1], restore: 2 });
    if (path === "/nexus/api/admin/accounts") {
      const row = JSON.stringify({
        name: "Pat <script>sample()</script>", email: "pat@example.test", enabled,
        admin: isAdmin, requests_enabled: canRequest,
      });
      return new Response('{"done":{"rows":' + (hasAccount ? '[{"id":' + id + ',' + row.slice(1) + ']' : "[]") + ',"more":false}}');
    }
    if (path === "/nexus/api/admin/devices") {
      return new Response('{"done":{"rows":[{"account":' + id +
        ',"device":19,"name":"Pat’s phone","kind":"phone","app_version":"1.2.3","last_seen_ms":0,"session":' +
        (hasSession ? "23" : "null") + '}],"more":false}}');
    }
    if (path === "/nexus/api/admin/music-server") return done({ internal: internalAddress, public: publicAddress });
    if (path === "/nexus/api/admin/diagnostics-default") return done({ enabled: diagnostics });
    if (path === "/nexus/api/admin/grant") {
      calls.push({ phase: "grant", request_id: data.change.request_id, action: data.change.action });
      if (nextRefusal) {
        const refusal = nextRefusal;
        nextRefusal = undefined;
        return answer({ refused: { not_allowed: refusal } }, 403);
      }
      if (data.password !== adminPassword) return answer({ refused: { not_allowed: "wrong_password" } }, 403);
      const secret = (++grantCount).toString(16).padStart(64, "0");
      pendingGrant = { secret, ...data.change };
      return done({ grant: secret });
    }
    const action = path.split("/").at(-1).replaceAll("-", "_");
    const target = {
      create_account: () => data.change.email,
      delete_account: () => data.change.account,
      reset_password: () => data.change.account,
      set_music_login: () => data.change.account,
      set_music_server: () => data.change.public,
      set_lidarr: () => data.change.address,
    }[action]?.() ?? data.change;
    assert.ok(pendingGrant, "Each mutation must have its own grant");
    assert.equal(data.grant, pendingGrant.secret);
    assert.equal(data.request_id, pendingGrant.request_id);
    assert.deepEqual(pendingGrant.action, { [action]: target });
    pendingGrant = null;
    calls.push({ phase: "change", action, payload: data.change });
    if (action === "set_account_enabled") enabled = data.change.enabled;
    if (action === "set_requests_enabled") canRequest = data.change.enabled;
    if (action === "set_admin") isAdmin = data.change.admin;
    if (action === "set_music_server") publicAddress = data.change.public;
    if (action === "set_diagnostics_default") diagnostics = data.change.enabled;
    if (action === "force_out") hasSession = false;
    if (action === "delete_account") hasAccount = false;
    if (action === "set_music_login" && data.change.password !== typedMusicPassword) {
      return answer({ refused: { not_allowed: "music_login_refused" } }, 403);
    }
    if (action === "create_account" || action === "reset_password") {
      const result = { code: nextCode === null ? "already_shown" : { issued: nextCode } };
      if (action === "create_account") result.account = 4;
      return done(result);
    }
    return done({});
  };
  return { fetch, calls, id, refuseNext(reason) { nextRefusal = reason; }, code(value) { nextCode = value; } };
}

test("all changes show a confirmation and spend one grant on the exact wire target", async () => {
  const mock = adminStandIn();
  const page = loadPage(mock.fetch);
  await page.signIn("owner@example.test", "example sign-in password");
  await settle();
  const confirm = async (password) => {
    assert.equal(page.from("grant").open, true);
    const saved = page.submit("grant-form", password === undefined ? {} : { "grant-password": password });
    assert.equal(page.from("grant-password").value, "");
    await saved;
    await settle();
  };

  page.submit("create-form", { "create-name": "<script>sample()</script>", "create-email": "NEW@EXAMPLE.TEST " });
  assert.match(page.from("grant-summary").textContent, /new@example\.test/);
  await confirm(adminPassword);
  assert.equal(page.from("code-value").textContent, code);
  assert.equal(page.from("new-code").open, true);
  page.click("code-done");
  assert.equal(page.from("code-value").textContent, "");
  assert.equal(page.from("new-code").open, false);
  assert.equal(page.from("create-name").value, "");

  page.press("accounts-list", "Switch account off");
  assert.equal(page.from("grant-password").disabled, true);
  await confirm();
  page.press("accounts-list", "Switch account on");
  await confirm();
  page.press("requests-list", "Turn requests off");
  await confirm();
  page.press("requests-list", "Allow requests");
  await confirm();
  page.press("accounts-list", "Make administrator");
  await confirm();
  page.press("accounts-list", "Remove administrator access");
  await confirm();
  mock.code("01AB234CDE57");
  page.press("accounts-list", "Reset password");
  await confirm();
  assert.equal(page.from("code-value").textContent, "01AB234CDE57");
  page.click("code-done");

  page.press("devices-list", "Sign this device out");
  assert.match(page.from("grant-note").textContent, /Downloaded music stays/);
  await confirm();
  page.press("accounts-list", "Set music sign-in");
  page.submit("music-login-form", { "music-user": "Pat", "music-password": typedMusicPassword });
  assert.equal(page.from("music-password").value, "");
  assert.equal(page.from("music-user").value, "");
  assert.equal(page.from("grant-summary").textContent.includes(typedMusicPassword), false);
  await confirm();
  assert.equal(page.from("change-status").textContent, "The music sign-in works.");

  const address = testAddress + "/library";
  page.submit("address-form", { "new-address": address });
  assert.ok(page.from("grant-summary").textContent.includes(internalAddress));
  assert.ok(page.from("grant-summary").textContent.includes(address));
  await confirm();
  assert.equal(page.from("music-public").textContent, address);

  page.submit("lidarr-form", { "lidarr-address": lidarrAddress, "lidarr-key": lidarrKey });
  assert.equal(page.from("lidarr-key").value, "");
  assert.equal(page.from("lidarr-address").value, "");
  assert.equal(page.from("grant-summary").textContent.includes(lidarrKey), false);
  await confirm();
  page.click("diagnostics-toggle");
  await confirm();

  page.press("accounts-list", "Remove account");
  assert.match(page.from("grant-summary").textContent, /listening history/);
  await confirm();
  assert.equal(page.from("accounts-list").children.length, 0);

  const grants = mock.calls.filter(call => call.phase === "grant");
  const changes = mock.calls.filter(call => call.phase === "change");
  assert.equal(grants.length, changes.length);
  assert.equal(new Set(grants.map(grant => grant.request_id)).size, grants.length);
  assert.deepEqual([...new Set(changes.map(change => change.action))].sort(), [
    "create_account", "delete_account", "force_out", "reset_password", "set_account_enabled", "set_admin",
    "set_diagnostics_default", "set_lidarr", "set_music_login", "set_music_server", "set_requests_enabled",
  ]);
  assert.deepEqual(changes.find(call => call.action === "force_out").payload, { account: mock.id, device: 19, session: 23 });
  assert.deepEqual(changes.find(call => call.action === "set_music_server").payload, { public: address });
  assert.deepEqual(changes.find(call => call.action === "set_lidarr").payload, { address: lidarrAddress, key: lidarrKey });
  assert.ok(!JSON.stringify(grants).includes(typedMusicPassword));
  assert.ok(!JSON.stringify(grants).includes(lidarrKey));
});

test("refusals give a next step, cancel does not spend, and expired password asks again", async () => {
  const mock = adminStandIn();
  const page = loadPage(mock.fetch);
  await page.signIn("owner@example.test", "example sign-in password");
  await settle();
  page.click("diagnostics-toggle");
  page.click("grant-cancel");
  assert.equal(mock.calls.length, 0);

  page.click("diagnostics-toggle");
  await page.submit("grant-form", { "grant-password": "wrong" });
  assert.match(page.from("change-status").textContent, /^That password is not right\./);
  assert.equal(mock.calls.filter(call => call.phase === "change").length, 0);

  page.click("diagnostics-toggle");
  await page.submit("grant-form", { "grant-password": adminPassword });
  await settle();
  page.press("accounts-list", "Reset password");
  assert.equal(page.from("grant-password").disabled, true);
  page.advance(10 * 60 * 1000);
  await page.submit("grant-form");
  assert.equal(page.from("grant-password").disabled, false);
  assert.equal(page.from("grant").open, true);
  mock.code(null);
  await page.submit("grant-form", { "grant-password": adminPassword });
  assert.equal(page.from("code-value").textContent, "");
  assert.match(page.from("change-status").textContent, /already shown/);
  await settle();
  page.press("accounts-list", "Set music sign-in");
  page.submit("music-login-form", { "music-user": "Pat", "music-password": "wrong music password" });
  await page.submit("grant-form");
  assert.match(page.from("change-status").textContent, /did not accept that sign-in/);
  page.windowEvents.get("pagehide")();
  assert.equal(page.from("music-password").value, "");
  assert.equal(page.from("grant-password").value, "");
  assert.equal(page.from("change-status").textContent, "");
});
