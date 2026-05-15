#!/usr/bin/env node
// server.ts — parseh-mcp entry point.
//
// A Model Context Protocol server that exposes PARSEH V0.2 protocol
// primitives as tools that LLM agents can call. Implements the
// stdio transport: it reads MCP requests on stdin and writes responses
// on stdout. The MCP host (Claude Desktop / Claude Code / etc) launches
// this process and pipes JSON-RPC frames through it.
//
// This server makes ZERO outbound network calls. Every tool either:
//   1. Spawns the local `parseh` Rust binary (via src/parseh-cli.ts), or
//   2. Spawns a local shell script in tools/parseh-tts or tools/parseh-stt.
//
// Cloud APIs are never invoked. Tool inputs and outputs never leave the
// machine. This matches the PARSEH no-telemetry / no-egress rule.

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

import { buildTools, invokeTool, type ToolDefinition } from "./tools.js";

const SERVER_NAME = "parseh-mcp";
const SERVER_VERSION = "0.1.0";

/**
 * Print a short --help message when invoked with --help/-h and exit.
 * MCP hosts spawn parseh-mcp without args, so this is purely a human
 * convenience when a user runs `parseh-mcp --help` from a shell.
 */
function maybePrintHelpAndExit(tools: ToolDefinition[]): void {
  const argv = process.argv.slice(2);
  if (argv.includes("--help") || argv.includes("-h")) {
    const lines: string[] = [];
    lines.push(`${SERVER_NAME} v${SERVER_VERSION}`);
    lines.push("");
    lines.push(
      "Local Model Context Protocol server exposing PARSEH V0.2 primitives.",
    );
    lines.push("");
    lines.push("Usage:");
    lines.push("  parseh-mcp           run the MCP server on stdio");
    lines.push("  parseh-mcp --help    print this message");
    lines.push("  parseh-mcp --tools   list tools as JSON and exit");
    lines.push("");
    lines.push("Capabilities:");
    lines.push("  tools     (read/write — every tool runs locally)");
    lines.push("");
    lines.push("Exposed tools:");
    for (const t of tools) {
      lines.push(`  - ${t.name}`);
    }
    lines.push("");
    lines.push("Env vars:");
    lines.push(
      "  PARSEH_BIN   path to the parseh-cli binary (default: 'parseh' on PATH)",
    );
    lines.push("");
    lines.push(
      "Zero external network egress — every tool calls a local subprocess.",
    );
    process.stdout.write(lines.join("\n") + "\n");
    process.exit(0);
  }
  if (argv.includes("--tools")) {
    const payload = tools.map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: t.inputSchema,
    }));
    process.stdout.write(JSON.stringify(payload, null, 2) + "\n");
    process.exit(0);
  }
}

/**
 * Construct an MCP Server, register tool listing + call handlers, and
 * return it. Exported for test reuse (tests instantiate the server
 * against an in-memory transport).
 */
export function createServer(tools: ToolDefinition[] = buildTools()): Server {
  const server = new Server(
    {
      name: SERVER_NAME,
      version: SERVER_VERSION,
    },
    {
      capabilities: {
        tools: {},
      },
      instructions:
        "PARSEH MCP server. Every tool invocation calls a local " +
        "subprocess and never reaches the network. Use parseh_detect_llm " +
        "first if unsure what LLM runtimes are available.",
    },
  );

  // tools/list
  server.setRequestHandler(ListToolsRequestSchema, async () => {
    return {
      tools: tools.map((t) => ({
        name: t.name,
        description: t.description,
        inputSchema: t.inputSchema,
      })),
    };
  });

  // tools/call
  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const { name, arguments: rawArgs } = request.params;
    const tool = tools.find((t) => t.name === name);
    if (!tool) {
      return {
        isError: true,
        content: [
          {
            type: "text",
            text: `unknown tool: ${name}`,
          },
        ],
      };
    }
    const args = (rawArgs ?? {}) as Record<string, unknown>;
    const outcome = await invokeTool(tool, args);
    if (!outcome.ok) {
      return {
        isError: true,
        content: [
          {
            type: "text",
            text: outcome.error,
          },
        ],
      };
    }
    // MCP content envelope is an array of typed content items. We
    // serialise the structured result as a single JSON text item; the
    // agent (Claude) parses the JSON itself. This is the same pattern
    // used by reference MCP servers like @modelcontextprotocol/server-filesystem.
    return {
      content: [
        {
          type: "text",
          text:
            typeof outcome.value === "string"
              ? outcome.value
              : JSON.stringify(outcome.value, null, 2),
        },
      ],
    };
  });

  return server;
}

/**
 * Boot the server on stdio. Never returns; the process exits when
 * stdin closes (the MCP host terminates the connection) or on an
 * unhandled error.
 */
async function main(): Promise<void> {
  const tools = buildTools();
  maybePrintHelpAndExit(tools);

  const server = createServer(tools);
  const transport = new StdioServerTransport();
  await server.connect(transport);

  // Keep the process alive — server.connect() returns once the
  // transport is wired up; the actual lifetime is owned by stdin.
  // Log nothing to stdout (would corrupt the MCP wire); stderr is OK.
  process.stderr.write(
    `[${SERVER_NAME}] listening on stdio (v${SERVER_VERSION}, ${tools.length} tools)\n`,
  );
}

// Run main only when invoked directly (not when imported by tests).
// We detect "directly invoked" by comparing argv[1] against import.meta.url.
const invokedDirectly = (() => {
  const arg1 = process.argv[1];
  if (!arg1) return false;
  try {
    const url = new URL(`file://${arg1}`).href;
    return import.meta.url === url;
  } catch {
    return false;
  }
})();

if (invokedDirectly) {
  main().catch((err) => {
    process.stderr.write(`[${SERVER_NAME}] fatal: ${err}\n`);
    process.exit(1);
  });
}
