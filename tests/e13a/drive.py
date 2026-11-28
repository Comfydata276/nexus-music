"""Drives the headless app through Slint's inspection port (see the README, "Running it on
Linux for development")."""
import base64, json, time, urllib.request

class App:
    def __init__(self, port=9315):
        self.url = f"http://127.0.0.1:{port}/mcp"
        self.n = 0
        self.window = None

    def rpc(self, method, params=None):
        self.n += 1
        body = json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params or {}}).encode()
        req = urllib.request.Request(self.url, data=body, headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
        with urllib.request.urlopen(req, timeout=60) as r:
            text = r.read().decode()
        if text.startswith("event:") or text.startswith("data:"):
            text = "\n".join(l[5:].strip() for l in text.splitlines() if l.startswith("data:"))
        out = json.loads(text)
        if "error" in out:
            raise RuntimeError(out["error"])
        return out["result"]

    def tool(self, name, **args):
        result = self.rpc("tools/call", {"name": name, "arguments": args})
        if result.get("isError"):
            raise RuntimeError(result)
        return result["content"]

    def data(self, name, **args):
        content = self.tool(name, **args)
        for block in content:
            if block.get("type") == "text":
                return json.loads(block["text"])
        return {}

    def wait_up(self, seconds=60):
        end = time.time() + seconds
        while time.time() < end:
            try:
                self.rpc("initialize")
                windows = self.data("list_windows").get("windowHandles", [])
                if windows:
                    self.window = windows[0]
                    self.root = self.data("get_window_properties", windowHandle=self.window)["rootElementHandle"]
                    return self
            except Exception:
                pass
            time.sleep(0.5)
        raise RuntimeError("the app's inspection port didn't come up")

    def tree(self, limit=20000):
        return self.data("get_element_tree", elementHandle=self.root, maxElements=limit).get("elements", [])

    def shot(self, path):
        for block in self.tool("take_screenshot", windowHandle=self.window):
            if block.get("type") == "image":
                open(path, "wb").write(base64.b64decode(block["data"]))
                return path
        raise RuntimeError("no picture came back")

    def click(self, handle, **kw):
        self.tool("click_element", elementHandle=handle, **kw)

    def set(self, handle, text):
        self.tool("set_element_value", elementHandle=handle, value=text)

    def key(self, text):
        self.tool("dispatch_key_event", windowHandle=self.window, text=text)

    # ---- finding things ----------------------------------------------------------------------
    def find(self, label=None, role=None, type=None, id=None, contains=None, tree=None):
        out = []
        for e in (tree if tree is not None else self.tree()):
            names = e.get("typeNamesAndIds", [])
            if label is not None and e.get("accessibleLabel") != label: continue
            if contains is not None and contains not in (e.get("accessibleLabel") or ""): continue
            if role is not None and e.get("accessibleRole") != role: continue
            if type is not None and not any(n.get("typeName") == type for n in names): continue
            if id is not None and not any(n.get("id") == id for n in names): continue
            out.append(e)
        return out

    def one(self, wait=15, **how):
        end = time.time() + wait
        while True:
            found = self.find(**how)
            if found:
                return found[0]
            if time.time() > end:
                raise RuntimeError(f"nothing on screen like {how}")
            time.sleep(0.3)

    def wait_for(self, seconds=20, **how):
        return self.one(wait=seconds, **how)

    def labels(self):
        """Every piece of text on screen, in the window's own order."""
        return [e["accessibleLabel"] for e in self.tree() if e.get("accessibleLabel") and e.get("accessibleRole") == "Text"]

    def type_into(self, label, text):
        self.set(self.one(label=label, role="TextInput")["handle"], text)

    def press(self, label, role="Button", **kw):
        self.click(self.one(label=label, role=role)["handle"], **kw)
