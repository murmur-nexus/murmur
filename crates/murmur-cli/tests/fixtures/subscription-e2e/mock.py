"""Scripted mock inference endpoint for probing harness CLIs without a subscription.

Speaks: Anthropic Messages (/v1/messages), OpenAI Responses (/v1/responses),
OpenAI Chat Completions (/v1/chat/completions), Gemini (/v1beta/models/*:*GenerateContent).
Logs every request body to LOG dir. Behaviour is chosen by the SCENARIO file, read per request:
  text   -> reply with a probe string (memory check: does the request contain PINEAPPLE?)
  tool   -> if an offered tool name contains 'echo_tool' and no tool result is present yet, call it; else text
  429    -> provider-shaped rate limit error
  401    -> provider-shaped auth error
  slow   -> stream text slowly (for interrupt tests)
  think  -> (anthropic) a thinking block, then text
  call:<json>  -> call the offered tool named json["name"] (bridge prefixes matched) with json["args"], once
  patch:<path> -> (openai responses) an apply_patch call adding <path>
Per-harness override: a file <SCEN>.gemini, if present, is read for Gemini requests.
Usage: MOCK_LOG=<dir> MOCK_SCEN=<file> python3 mock.py <port>
"""
import json, os, sys, time, re, itertools
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler

LOG = os.environ.get("MOCK_LOG", "/tmp/mocklog")
SCEN = os.environ.get("MOCK_SCEN", "/tmp/mockscen")
os.makedirs(LOG, exist_ok=True)
counter = itertools.count(1 + max([int(f[:4]) for f in os.listdir(LOG) if f[:4].isdigit()] or [0]))


def scenario(path=""):
    kind = "gemini" if "GenerateContent" in path else ""
    if kind and os.path.exists(SCEN + "." + kind):
        return open(SCEN + "." + kind).read().strip() or "text"
    try:
        return open(SCEN).read().strip() or "text"
    except FileNotFoundError:
        return "text"


def probe_text(body_s, ntools):
    saw = "PINEAPPLE" in body_s
    return f"MOCKREPLY saw_secret={saw} tools_offered={ntools}"


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _body(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        if self.headers.get("content-encoding") == "gzip":
            import gzip; raw = gzip.decompress(raw)
        elif self.headers.get("content-encoding") == "zstd":
            try:
                import zstandard; raw = zstandard.ZstdDecompressor().decompress(raw, max_output_size=50_000_000)
            except Exception:
                pass
        return raw

    def _log(self, raw):
        i = next(counter)
        with open(f"{LOG}/{i:04d}.json", "w") as f:
            json.dump({"path": self.path, "headers": dict(self.headers), "scenario": scenario(self.path),
                       "body": raw.decode("utf-8", "replace")}, f)
        return i

    def _json(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def _sse_start(self):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

    def _sse(self, data, event=None):
        s = ""
        if event:
            s += f"event: {event}\n"
        s += f"data: {json.dumps(data)}\n\n"
        self.wfile.write(s.encode())
        self.wfile.flush()

    def do_GET(self):
        self._log(b"")
        if "models" in self.path:
            return self._json(200, {"data": [{"id": "mock-model", "object": "model"}], "models": []})
        self._json(200, {})

    def do_POST(self):
        raw = self._body()
        self._log(raw)
        try:
            body = json.loads(raw or b"{}")
        except Exception:
            body = {}
        p = self.path
        sc = scenario(self.path)
        if "/v1/messages" in p:
            return self.anthropic(body, raw.decode("utf-8", "replace"), sc)
        if p.rstrip("/").endswith("/responses"):
            return self.oai_responses(body, raw.decode("utf-8", "replace"), sc)
        if "chat/completions" in p:
            return self.chat(body, raw.decode("utf-8", "replace"), sc)
        if "GenerateContent" in p or "generateContent" in p or "countTokens" in p:
            return self.gemini(body, raw.decode("utf-8", "replace"), sc, p)
        self._json(404, {"error": "unknown path " + p})

    # ---------------- Anthropic ----------------
    def anthropic(self, body, s, sc):
        if "count_tokens" in self.path:
            return self._json(200, {"input_tokens": 10})
        if sc == "429":
            return self._json(429, {"type": "error", "error": {"type": "rate_limit_error", "message": "MOCK: rate limit window exhausted"}})
        if sc == "401":
            return self._json(401, {"type": "error", "error": {"type": "authentication_error", "message": "MOCK: invalid x-api-key"}})
        tools = [t.get("name") for t in body.get("tools", [])]
        has_result = '"tool_result"' in s
        echo = next((t for t in tools if t and "echo_tool" in t), None)
        if not body.get("stream"):
            return self._json(200, {"id": "msg_x", "type": "message", "role": "assistant", "model": "mock",
                                    "content": [{"type": "text", "text": probe_text(s, len(tools))}],
                                    "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}})
        self._sse_start()
        self._sse({"type": "message_start", "message": {"id": "msg_x", "type": "message", "role": "assistant", "model": "mock",
                   "content": [], "stop_reason": None, "usage": {"input_tokens": 1, "output_tokens": 0}}}, "message_start")
        call = None
        if sc.startswith("call:") and not has_result:
            spec = json.loads(sc.split(":", 1)[1])
            want = spec["name"]
            call = (next((t for t in tools if t and (t == want or t.endswith("__" + want))), want), spec.get("args", {}))
        elif sc == "tool" and echo and not has_result:
            call = (echo, {"text": "hi"})
        if call:
            self._sse({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": call[0], "input": {}}}, "content_block_start")
            self._sse({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json.dumps(call[1])}}, "content_block_delta")
            self._sse({"type": "content_block_stop", "index": 0}, "content_block_stop")
            stop = "tool_use"
        elif sc == "think":
            self._sse({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}, "content_block_start")
            self._sse({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me consider the request."}}, "content_block_delta")
            self._sse({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "c2lnbmF0dXJl"}}, "content_block_delta")
            self._sse({"type": "content_block_stop", "index": 0}, "content_block_stop")
            self._sse({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}, "content_block_start")
            self._sse({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Answer after thinking. "}}, "content_block_delta")
            self._sse({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": probe_text(s, len(tools))}}, "content_block_delta")
            self._sse({"type": "content_block_stop", "index": 1}, "content_block_stop")
            stop = "end_turn"
        else:
            self._sse({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}, "content_block_start")
            parts = [probe_text(s, len(tools))]
            if sc == "slow":
                parts = [f"tick{i} " for i in range(30)]
            for part in parts:
                self._sse({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": part}}, "content_block_delta")
                if sc == "slow":
                    time.sleep(1)
            self._sse({"type": "content_block_stop", "index": 0}, "content_block_stop")
            stop = "end_turn"
        self._sse({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 5}}, "message_delta")
        self._sse({"type": "message_stop"}, "message_stop")

    # ---------------- OpenAI Responses (codex) ----------------
    def oai_responses(self, body, s, sc):
        if sc == "429":
            return self._json(429, {"error": {"type": "usage_limit_reached", "code": "rate_limit_exceeded", "message": "MOCK: usage limit window exhausted"}})
        if sc == "401":
            return self._json(401, {"error": {"type": "invalid_request_error", "code": "invalid_api_key", "message": "MOCK: invalid api key"}})
        tools = []
        for t in body.get("tools", []):
            tools.append(t.get("name") or t.get("type"))
        has_result = "function_call_output" in s
        echo = next((t for t in tools if t and "echo_tool" in t), None)
        self._sse_start()
        rid = "resp_x"
        self._sse({"type": "response.created", "response": {"id": rid}}, "response.created")
        has_result = has_result or "custom_tool_call_output" in s
        if sc.startswith("patch") and not has_result:
            item = {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_p", "name": "apply_patch", "input": "*** Begin Patch\n*** Add File: " + sc.split(":",1)[1] + "\n+pwned\n*** End Patch\n"}
            self._sse({"type": "response.output_item.added", "output_index": 0, "item": item}, "response.output_item.added")
            self._sse({"type": "response.output_item.done", "output_index": 0, "item": item}, "response.output_item.done")
        elif sc.startswith("call:") and not has_result:
            spec = json.loads(sc.split(":",1)[1])
            item = {"type": "function_call", "id": "fc_1", "call_id": "call_1", "arguments": json.dumps(spec.pop("args", {"text": "hi"})), **spec}
            self._sse({"type": "response.output_item.added", "output_index": 0, "item": item}, "response.output_item.added")
            self._sse({"type": "response.output_item.done", "output_index": 0, "item": item}, "response.output_item.done")
        elif sc == "tool" and echo and not has_result:
            item = {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": echo, "arguments": json.dumps({"text": "hi"})}
            self._sse({"type": "response.output_item.added", "output_index": 0, "item": item}, "response.output_item.added")
            self._sse({"type": "response.output_item.done", "output_index": 0, "item": item}, "response.output_item.done")
        else:
            txt = probe_text(s, len(tools))
            parts = [txt] if sc != "slow" else [f"tick{i} " for i in range(30)]
            item = {"type": "message", "id": "msg_1", "role": "assistant", "content": []}
            self._sse({"type": "response.output_item.added", "output_index": 0, "item": item}, "response.output_item.added")
            for part in parts:
                self._sse({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": part}, "response.output_text.delta")
                if sc == "slow":
                    time.sleep(1)
            item = {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "".join(parts), "annotations": []}]}
            self._sse({"type": "response.output_item.done", "output_index": 0, "item": item}, "response.output_item.done")
        self._sse({"type": "response.completed", "response": {"id": rid, "usage": {"input_tokens": 1, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 2}}}, "response.completed")

    # ---------------- OpenAI Chat Completions (kimi) ----------------
    def chat(self, body, s, sc):
        if sc == "429":
            return self._json(429, {"error": {"type": "rate_limit_reached_error", "message": "MOCK: request window exhausted"}})
        if sc == "401":
            return self._json(401, {"error": {"type": "invalid_authentication_error", "message": "MOCK: invalid api key"}})
        tools = [t.get("function", {}).get("name") for t in body.get("tools", [])]
        has_result = '"role": "tool"' in s or '"role":"tool"' in s
        echo = next((t for t in tools if t and "echo_tool" in t), None)
        stream = body.get("stream")
        if sc == "tool" and echo and not has_result:
            msg = {"role": "assistant", "content": None, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": echo, "arguments": json.dumps({"text": "hi"})}}]}
            fin = "tool_calls"
        else:
            msg = {"role": "assistant", "content": probe_text(s, len(tools))}
            fin = "stop"
        if not stream:
            return self._json(200, {"id": "c1", "object": "chat.completion", "model": "mock", "choices": [{"index": 0, "message": msg, "finish_reason": fin}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
        self._sse_start()
        if fin == "tool_calls":
            tc = msg["tool_calls"][0]
            self._sse({"id": "c1", "object": "chat.completion.chunk", "model": "mock", "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, **tc}]}, "finish_reason": None}]})
        else:
            self._sse({"id": "c1", "object": "chat.completion.chunk", "model": "mock", "choices": [{"index": 0, "delta": {"role": "assistant", "content": msg["content"]}, "finish_reason": None}]})
        self._sse({"id": "c1", "object": "chat.completion.chunk", "model": "mock", "choices": [{"index": 0, "delta": {}, "finish_reason": fin}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
        self.wfile.write(b"data: [DONE]\n\n"); self.wfile.flush()

    # ---------------- Gemini ----------------
    def gemini(self, body, s, sc, p):
        if "countTokens" in p:
            return self._json(200, {"totalTokens": 10})
        if sc == "429":
            return self._json(429, {"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "MOCK: quota exhausted"}})
        if sc == "401":
            return self._json(401, {"error": {"code": 401, "status": "UNAUTHENTICATED", "message": "MOCK: invalid api key"}})
        tools = []
        for t in body.get("tools", []):
            for fd in t.get("functionDeclarations", []) or []:
                tools.append(fd.get("name"))
        has_result = "functionResponse" in s
        echo = next((t for t in tools if t and "echo_tool" in t), None)
        if sc == "tool" and echo and not has_result:
            parts = [{"functionCall": {"name": echo, "args": {"text": "hi"}}}]
        else:
            parts = [{"text": probe_text(s, len(tools))}]
        cand = {"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP", "index": 0}],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}, "modelVersion": "mock"}
        if "stream" in p:
            self._sse_start()
            self._sse(cand)
        else:
            self._json(200, cand)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8787
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
