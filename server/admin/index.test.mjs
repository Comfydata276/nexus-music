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
  const context = {
    document: {
      getElementById(id) {
        const element = {
          textContent: "",
          value: "",
          disabled: false,
          hidden: id === "admin",
          events: new Map(),
          children: [],
          append(child) { this.children.push(child); },
          replaceChildren() { this.children = []; },
          addEventListener(name, handler) { this.events.set(name, handler); },
          reset() {
            elements.get("email").value = "";
            elements.get("password").value = "";
          },
          focus() {},
          set innerHTML(_) { assert.fail("Untrusted text reached innerHTML"); },
          set outerHTML(_) { assert.fail("Untrusted text reached outerHTML"); },
          insertAdjacentHTML() { assert.fail("Untrusted text reached insertAdjacentHTML"); },
        };
        elements.set(id, element);
        return element;
      },
      createElement(tag) {
        assert.equal(tag, "li");
        return { textContent: "" };
      },
    },
    window: { addEventListener(name, handler) { windowEvents.set(name, handler); } },
    crypto: webcrypto,
    setTimeout(fn, delay) { timers.push({ fn, delay }); return timers.length; },
    clearTimeout: () => {},
    TextDecoder,
    AbortController,
    fetch,
  };
  const instrumentation = "globalThis.forTest = { writeText, parseReply };";
  runInNewContext(script.replace(/\}\)\(\);\s*$/, instrumentation + "})();"), context);
  const from = id => elements.get(id);
  return {
    from,
    windowEvents,
    timers,
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
  assert.equal(page.from("diagnostics-status").textContent, "Default is on.");
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
