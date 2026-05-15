// smoke.test.ts — smoke tests for parseh-mcp.
//
// We do NOT spin up a real MCP host. Instead we exercise:
//
//   1. Tool list construction (every tool has the required fields).
//   2. createServer() builds a Server without throwing.
//   3. Tool dispatch via invokeTool() with a mocked parseh-cli binary.
//
// The mock binary is a small shell script written into the OS temp
// directory. We point PARSEH_BIN at it for the duration of the test.
// This keeps the tests hermetic — no real `parseh` binary required,
// no network, no GitHub, no audio hardware.
//
// Node's built-in test runner is used (no external dependencies).

import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, writeFileSync, chmodSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { buildTools, invokeTool, type ToolDefinition } from "../src/tools.js";
import { createServer } from "../src/server.js";

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

/**
 * Write a small bash script to a temp dir that prints canned JSON on
 * stdout and returns the requested exit code. Returns the absolute
 * path to the script.
 */
function writeStubBin(opts: {
  stdout: string;
  stderr?: string;
  exit?: number;
}): { bin: string; dir: string } {
  const dir = mkdtempSync(join(tmpdir(), "parseh-mcp-stub-"));
  const bin = join(dir, "parseh-stub.sh");
  const script = `#!/usr/bin/env bash
cat <<'PARSEH_STUB_STDOUT'
${opts.stdout}
PARSEH_STUB_STDOUT
${opts.stderr ? `echo ${JSON.stringify(opts.stderr)} >&2` : ""}
exit ${opts.exit ?? 0}
`;
  writeFileSync(bin, script);
  chmodSync(bin, 0o755);
  return { bin, dir };
}

function findTool(tools: ToolDefinition[], name: string): ToolDefinition {
  const t = tools.find((x) => x.name === name);
  assert.ok(t, `tool not found: ${name}`);
  return t;
}

// --------------------------------------------------------------------------
// 1 · Tool registry is well-formed.
// --------------------------------------------------------------------------

test("buildTools exposes the expected nine tools, each well-formed", () => {
  const tools = buildTools();
  const names = tools.map((t) => t.name).sort();
  assert.deepEqual(names, [
    "parseh_detect_llm",
    "parseh_list_peers",
    "parseh_listen",
    "parseh_open_issue",
    "parseh_query_outcome",
    "parseh_run_tests",
    "parseh_speak",
    "parseh_status",
    "parseh_submit_job",
  ]);
  for (const t of tools) {
    assert.equal(typeof t.name, "string");
    assert.ok(t.name.length > 0, "tool name non-empty");
    assert.equal(typeof t.description, "string");
    assert.ok(t.description.length > 0, "tool description non-empty");
    assert.equal(t.inputSchema.type, "object");
    assert.equal(typeof t.handler, "function");
  }
});

// --------------------------------------------------------------------------
// 2 · createServer() builds a Server instance without throwing.
// --------------------------------------------------------------------------

test("createServer constructs a Server with tools capability", () => {
  const server = createServer();
  assert.ok(server, "server is constructed");
  // The Server class is opaque; we just confirm it has the expected
  // methods we rely on at runtime.
  assert.equal(
    typeof (server as unknown as { connect: unknown }).connect,
    "function",
  );
  assert.equal(
    typeof (server as unknown as { setRequestHandler: unknown })
      .setRequestHandler,
    "function",
  );
});

// --------------------------------------------------------------------------
// 3 · parseh_status calls parseh-cli and returns its JSON.
// --------------------------------------------------------------------------

test("parseh_status returns the parsed JSON from the stub binary", async () => {
  const stubStdout = JSON.stringify({
    peer_id: "12D3KooFakePeerId",
    listen_addrs: ["/ip4/127.0.0.1/tcp/4001"],
    peers_connected: 0,
    capabilities: ["Inference"],
    reputation: 0,
  });
  const { bin, dir } = writeStubBin({ stdout: stubStdout });
  process.env.PARSEH_BIN = bin;
  try {
    const tool = findTool(buildTools(), "parseh_status");
    const outcome = await invokeTool(tool, {});
    assert.ok(outcome.ok, `expected ok, got error: ${(outcome as { ok: false; error: string }).error ?? "unknown"}`);
    const value = (outcome as { ok: true; value: unknown }).value as {
      peer_id: string;
      capabilities: string[];
    };
    assert.equal(value.peer_id, "12D3KooFakePeerId");
    assert.deepEqual(value.capabilities, ["Inference"]);
  } finally {
    delete process.env.PARSEH_BIN;
    rmSync(dir, { recursive: true, force: true });
  }
});

// --------------------------------------------------------------------------
// 4 · parseh_submit_job validates input.
// --------------------------------------------------------------------------

test("parseh_submit_job rejects missing prompt", async () => {
  const tool = findTool(buildTools(), "parseh_submit_job");
  const outcome = await invokeTool(tool, {});
  assert.equal(outcome.ok, false);
  assert.match(
    (outcome as { ok: false; error: string }).error,
    /prompt/,
  );
});

test("parseh_submit_job rejects wrong-type kind", async () => {
  const tool = findTool(buildTools(), "parseh_submit_job");
  const outcome = await invokeTool(tool, { prompt: "hi", kind: 42 });
  assert.equal(outcome.ok, false);
  assert.match(
    (outcome as { ok: false; error: string }).error,
    /kind/,
  );
});

test("parseh_submit_job rejects bogus kind variant", async () => {
  const tool = findTool(buildTools(), "parseh_submit_job");
  const outcome = await invokeTool(tool, {
    prompt: "hi",
    kind: "MiningProofOfStake",
  });
  assert.equal(outcome.ok, false);
});

test("parseh_submit_job succeeds with stubbed binary", async () => {
  const stubStdout = JSON.stringify({
    spec_hash:
      "0x" + "0123456789abcdef".repeat(4),
    submitted_at: 1715745600,
  });
  const { bin, dir } = writeStubBin({ stdout: stubStdout });
  process.env.PARSEH_BIN = bin;
  try {
    const tool = findTool(buildTools(), "parseh_submit_job");
    const outcome = await invokeTool(tool, {
      prompt: "what is the airspeed velocity?",
      kind: "Inference",
    });
    assert.ok(outcome.ok);
    const value = (outcome as { ok: true; value: unknown }).value as {
      spec_hash: string;
    };
    assert.ok(value.spec_hash.startsWith("0x"));
  } finally {
    delete process.env.PARSEH_BIN;
    rmSync(dir, { recursive: true, force: true });
  }
});

// --------------------------------------------------------------------------
// 5 · parseh_query_outcome handles "not found" cleanly.
// --------------------------------------------------------------------------

test("parseh_query_outcome returns null on exit code 2 (not found)", async () => {
  const { bin, dir } = writeStubBin({ stdout: "", exit: 2 });
  process.env.PARSEH_BIN = bin;
  try {
    const tool = findTool(buildTools(), "parseh_query_outcome");
    const outcome = await invokeTool(tool, {
      spec_hash: "0xdeadbeef",
    });
    assert.ok(outcome.ok);
    assert.equal((outcome as { ok: true; value: unknown }).value, null);
  } finally {
    delete process.env.PARSEH_BIN;
    rmSync(dir, { recursive: true, force: true });
  }
});

test("parseh_query_outcome rejects missing spec_hash", async () => {
  const tool = findTool(buildTools(), "parseh_query_outcome");
  const outcome = await invokeTool(tool, {});
  assert.equal(outcome.ok, false);
});

// --------------------------------------------------------------------------
// 6 · parseh_detect_llm parses JSON.
// --------------------------------------------------------------------------

test("parseh_detect_llm returns the DetectionResult JSON", async () => {
  const stubStdout = JSON.stringify({
    ollama: null,
    llama_cpp: null,
    gguf_files: [],
    gpu: null,
  });
  const { bin, dir } = writeStubBin({ stdout: stubStdout });
  process.env.PARSEH_BIN = bin;
  try {
    const tool = findTool(buildTools(), "parseh_detect_llm");
    const outcome = await invokeTool(tool, {});
    assert.ok(outcome.ok);
    const value = (outcome as { ok: true; value: unknown }).value as {
      gguf_files: unknown[];
    };
    assert.deepEqual(value.gguf_files, []);
  } finally {
    delete process.env.PARSEH_BIN;
    rmSync(dir, { recursive: true, force: true });
  }
});

// --------------------------------------------------------------------------
// 7 · parseh_list_peers passes filter args through.
// --------------------------------------------------------------------------

test("parseh_list_peers parses an array result", async () => {
  const stubStdout = JSON.stringify([
    {
      peer_id: "12D3KooA",
      capabilities: ["Inference"],
      reputation: 1.5,
    },
    {
      peer_id: "12D3KooB",
      capabilities: ["Relay"],
      reputation: 0.1,
    },
  ]);
  const { bin, dir } = writeStubBin({ stdout: stubStdout });
  process.env.PARSEH_BIN = bin;
  try {
    const tool = findTool(buildTools(), "parseh_list_peers");
    const outcome = await invokeTool(tool, { min_reputation: 1.0 });
    assert.ok(outcome.ok);
    const value = (outcome as { ok: true; value: unknown }).value as unknown[];
    assert.equal(value.length, 2);
  } finally {
    delete process.env.PARSEH_BIN;
    rmSync(dir, { recursive: true, force: true });
  }
});

test("parseh_list_peers rejects non-number min_reputation", async () => {
  const tool = findTool(buildTools(), "parseh_list_peers");
  const outcome = await invokeTool(tool, { min_reputation: "high" });
  assert.equal(outcome.ok, false);
});

// --------------------------------------------------------------------------
// 8 · parseh_open_issue validates required fields.
// --------------------------------------------------------------------------

test("parseh_open_issue requires title and body", async () => {
  const tool = findTool(buildTools(), "parseh_open_issue");
  const a = await invokeTool(tool, {});
  assert.equal(a.ok, false);
  const b = await invokeTool(tool, { title: "x" });
  assert.equal(b.ok, false);
});
