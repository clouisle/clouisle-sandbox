// TypeScript SDK client tests: exercise request construction, response
// parsing, error mapping and SSE streaming against a fake HTTP server.
// Run: npm test  (tsc && node --test test/)

const { test } = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");

const { Client, SandboxError } = require("../dist/index.js");

function startServer(handler) {
  const server = http.createServer(handler);
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      resolve({
        server,
        baseUrl: `http://127.0.0.1:${server.address().port}`,
        close: () => new Promise((r) => server.close(r)),
      });
    });
  });
}

function jsonResponse(res, status, body) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(body));
}

test("createSandbox sends POST with bearer auth and parses sandbox", async () => {
  const seen = {};
  const { server, baseUrl, close } = await startServer((req, res) => {
    seen.method = req.method;
    seen.url = req.url;
    seen.auth = req.headers.authorization;
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      seen.body = JSON.parse(body);
      jsonResponse(res, 201, {
        id: "sbx-1",
        status: "running",
        spec: { image: { reference: "alpine:latest" } },
      });
    });
  });
  try {
    const client = new Client(baseUrl, "secret-key");
    const sandbox = await client.createSandbox({
      image: { reference: "alpine:latest" },
      resources: { vcpu: 1, memory_mb: 256, disk_mb: 512 },
    });
    assert.equal(seen.method, "POST");
    assert.equal(seen.url, "/api/v1/sandboxes");
    assert.equal(seen.auth, "Bearer secret-key");
    assert.equal(seen.body.image.reference, "alpine:latest");
    assert.equal(sandbox.id, "sbx-1");
    assert.equal(sandbox.status, "running");
  } finally {
    await close();
  }
});

test("getSandbox and exec target the sandbox-scoped paths", async () => {
  const seen = [];
  const { server, baseUrl, close } = await startServer((req, res) => {
    seen.push({ method: req.method, url: req.url });
    if (req.url.startsWith("/api/v1/sandboxes/sbx-1/exec")) {
      jsonResponse(res, 200, {
        exit_code: 0,
        stdout: "hello\n",
        stderr: "",
        duration_ms: 1,
      });
    } else {
      jsonResponse(res, 200, { id: "sbx-1", status: "running" });
    }
  });
  try {
    const client = new Client(baseUrl, "k");
    const sandbox = await client.getSandbox("sbx-1");
    assert.equal(sandbox.id, "sbx-1");
    const result = await client.exec("sbx-1", {
      argv: ["echo", "hello"],
      timeout_ms: 5000,
    });
    assert.equal(result.exit_code, 0);
    assert.equal(result.stdout, "hello\n");
    assert.deepEqual(seen, [
      { method: "GET", url: "/api/v1/sandboxes/sbx-1" },
      { method: "POST", url: "/api/v1/sandboxes/sbx-1/exec" },
    ]);
  } finally {
    await close();
  }
});

test("trailing slash on baseUrl is normalized", async () => {
  const { server, baseUrl, close } = await startServer((req, res) => {
    jsonResponse(res, 200, { id: "sbx-1", status: "running" });
  });
  try {
    const client = new Client(`${baseUrl}/`, "k");
    await client.getSandbox("sbx-1");
    const reqs = [];
    server.on("request", (req) => reqs.push(req.url));
    assert.equal(reqs.some((u) => u.includes("//")), false);
  } finally {
    await close();
  }
});

test("API error maps to SandboxError with code and message", async () => {
  const { server, baseUrl, close } = await startServer((req, res) => {
    jsonResponse(res, 404, {
      error: { code: "NOT_FOUND", message: "sandbox sbx-x not found", details: null },
    });
  });
  try {
    const client = new Client(baseUrl, "k");
    await assert.rejects(
      client.getSandbox("sbx-x"),
      (err) => {
        assert.ok(err instanceof SandboxError);
        assert.equal(err.statusCode, 404);
        assert.equal(err.code, "NOT_FOUND");
        assert.match(err.message, /not found/);
        return true;
      },
    );
  } finally {
    await close();
  }
});

test("streamExec yields SSE stdout and exit events in order", async () => {
  const { server, baseUrl, close } = await startServer((req, res) => {
    res.writeHead(200, { "Content-Type": "text/event-stream" });
    res.write("event: stdout\ndata: hello\n\n");
    res.write("event: stderr\ndata: warn\n\n");
    res.write("event: exit\ndata: 0\n\n");
    res.end();
  });
  try {
    const client = new Client(baseUrl, "k");
    const events = [];
    for await (const event of client.streamExec("sbx-1", {
      argv: ["echo", "hello"],
      timeout_ms: 5000,
    })) {
      events.push(event);
    }
    assert.deepEqual(events, [
      { event: "stdout", data: "hello" },
      { event: "stderr", data: "warn" },
      { event: "exit", data: "0" },
    ]);
  } finally {
    await close();
  }
});

test("uploadFile sends raw body with path query", async () => {
  const seen = {};
  const { server, baseUrl, close } = await startServer((req, res) => {
    seen.url = req.url;
    seen.ct = req.headers["content-type"];
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      seen.body = body;
      jsonResponse(res, 200, { ok: true });
    });
  });
  try {
    const client = new Client(baseUrl, "k");
    await client.uploadFile("sbx-1", "/work/a.txt", Buffer.from("payload"));
    assert.equal(seen.url, "/api/v1/sandboxes/sbx-1/files/upload?path=%2Fwork%2Fa.txt");
    assert.equal(seen.ct, "application/octet-stream");
    assert.equal(seen.body, "payload");
  } finally {
    await close();
  }
});
