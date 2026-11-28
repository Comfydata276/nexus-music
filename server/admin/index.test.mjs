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
  const context = {
    document: {
      getElementById(id) {
        const element = {
          textContent: "",
          value: "",
          disabled: false,
          hidden: id === "admin",
          events: new Map(),
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
    },
    window: { addEventListener(name, handler) { windowEvents.set(name, handler); } },
    crypto: webcrypto,
    setTimeout: () => 1,
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
    assert.equal(path, "/nexus/api/signout");
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
