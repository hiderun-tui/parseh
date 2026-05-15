# parseh-mcp — local Model Context Protocol server for PARSEH

A small Node.js daemon that exposes the PARSEH V0.2 protocol primitives as MCP **tools** that LLM agents (Claude Code, Claude Desktop, ChatGPT desktop, Continue, Zed, etc.) can call. The MCP server is a thin protocol shim: every tool ultimately runs a **local subprocess** — either the `parseh` Rust binary or one of the bundled shell wrappers in `tools/parseh-tts` / `tools/parseh-stt`. **No cloud APIs are invoked. No telemetry. No external network egress.**

## What MCP is and why this exists

The [Model Context Protocol](https://modelcontextprotocol.io/specification) is an open JSON-RPC protocol that lets LLM agents discover and call tools at inference time. An MCP server advertises a list of tools (each with a name, description, and JSON-Schema for its input), and the agent picks tools to call based on the user's request.

For PARSEH this gives us something we did not have before: a Persian-speaking contributor can sit at their terminal with Claude Code running, ask "ببین شبکه پارسه چه وضعی داره" ("show me the state of the PARSEH network"), and the agent picks `parseh_status` automatically. They can ask "این درخواست رو به شبکه بفرست" ("submit this request to the network") and the agent picks `parseh_submit_job`. The whole flow stays in Persian and stays on the contributor's machine.

## Installation

You need Node.js ≥ 20.

```bash
cd tools/parseh-mcp
npm install
npm run build
npm install -g .         # installs the `parseh-mcp` binary on your $PATH
```

Or run without a global install:

```bash
cd tools/parseh-mcp
npm install && npm run build
node dist/src/server.js  # the MCP host normally invokes this for you
```

## Wiring into Claude Code

Claude Code reads `~/.claude/mcp_settings.json` (or a project-local `.claude/mcp_settings.json`). Add:

```json
{
  "mcpServers": {
    "parseh": {
      "command": "parseh-mcp",
      "env": {
        "PARSEH_BIN": "/usr/local/bin/parseh"
      }
    }
  }
}
```

If you did not `npm install -g`, point `command` at the local `node dist/src/server.js` path instead.

## Wiring into Claude Desktop

Edit `~/.config/Claude/claude_desktop_config.json` (Linux) or the macOS equivalent:

```json
{
  "mcpServers": {
    "parseh": {
      "command": "/absolute/path/to/parseh/tools/parseh-mcp/dist/src/server.js",
      "args": [],
      "env": {
        "PARSEH_BIN": "/usr/local/bin/parseh"
      }
    }
  }
}
```

Restart Claude Desktop. The PARSEH tools then appear in the tool drawer.

## Available tools

| Tool | What it does | Inputs |
|---|---|---|
| `parseh_status` | Local node's network status (peer ID, peers, capabilities, reputation) | none |
| `parseh_submit_job` | Submit a signed JobSpec to the network | `prompt`, optional `kind`, optional `sensitive` |
| `parseh_query_outcome` | Look up a JobOutcome by spec hash | `spec_hash` |
| `parseh_list_peers` | List known peers, filterable by reputation / service | optional `min_reputation`, `service` |
| `parseh_detect_llm` | Probe local machine for Ollama / llama.cpp / GGUF / GPU | none |
| `parseh_speak` | Persian text-to-speech via `parseh-tts` (piper / espeak-ng) | `text`, optional `lang`, `save_path` |
| `parseh_listen` | Persian speech-to-text via `parseh-stt` (when installed) | optional `audio_path`, `seconds` |
| `parseh_run_tests` | Run V0.2 unit + acceptance tests | optional `target` |
| `parseh_open_issue` | Open a GitHub issue via local `gh` CLI | `title`, `body`, optional `labels` |

### Example LLM invocations

You don't call these directly. You speak (or type) what you want and the agent picks the tool. For instance:

> *"What's the state of my PARSEH node? List peers with reputation above 1.0."*
>
> Agent calls `parseh_status` then `parseh_list_peers({ min_reputation: 1.0 })` and summarises both.

> *"Submit this prompt: 'Translate the following Persian into English …'. Track the outcome and read the result back in Persian when it arrives."*
>
> Agent calls `parseh_submit_job({ prompt: ... })`, polls `parseh_query_outcome({ spec_hash: ... })`, then `parseh_speak({ text: <result>, lang: "fa" })`.

> *"Run the V0.2 acceptance harness and if anything failed, open a GitHub issue with the test log."*
>
> Agent calls `parseh_run_tests({ target: "acceptance" })`, then `parseh_open_issue({ title: ..., body: ... })` on failure.

## Persian-only contributor flow

The end-to-end Persian-only experience the MCP layer makes possible:

```
Voice in (Persian)              →  parseh_listen          (local piper / vosk)
Idea → JobSpec                  →  parseh_submit_job       (local Rust binary)
Watch for outcome               →  parseh_query_outcome    (local SharedState SQLite)
Hear the result back            →  parseh_speak            (local piper / espeak-ng)
```

No part of that loop touches the cloud. No audio leaves the machine. No prompt text leaves the machine. The MCP server is the glue.

## Privacy guarantees

- The MCP server itself contains **zero** HTTP / fetch / cloud-SDK code. The dependency tree is exactly `@modelcontextprotocol/sdk` + Node standard library.
- `parseh_speak` and `parseh_listen` invoke local shell scripts (piper-tts, espeak-ng, whisper.cpp / vosk). None of those reach the internet at runtime.
- `parseh_status`, `parseh_submit_job`, etc. spawn the local `parseh` Rust binary. That binary is what handles P2P traffic and stays inside PARSEH's normal egress policy (gossipsub to peers, never to a cloud LLM provider).
- `parseh_open_issue` invokes the local `gh` CLI, which uses the user's existing GitHub credentials and respects their proxy configuration. The MCP server never sees the GitHub token.

If you grep this directory for `http`, `fetch`, `axios`, `request`, or any cloud-SDK package name, you get zero hits.

## Why TypeScript (not Rust)

We picked TypeScript for this layer because:

1. The official MCP SDK is TS-first and mature (v1.x as of 2026-05). The Rust SDK is younger and rough around the edges.
2. The MCP layer is a thin protocol shim — it is not the hot path. The hot path stays in `server/` Rust crates.
3. Most MCP examples in the ecosystem are TypeScript; contributors will find prior art.
4. We tolerate adding Node as a developer dependency in `tools/` (the developer-ergonomics layer) but not in `server/` (the production stack).

A Rust-native MCP server lands when the Rust SDK matures. This TypeScript implementation is **V0.2-scaffold-quality** and intentionally so — it is one `parseh-cli.ts` shim away from being swapped out.

## Development

```bash
npm install                # one-time
npm run build              # tsc
npm test                   # node --test on the compiled dist/
npm run typecheck          # tsc --noEmit (no output, just verify)
node dist/src/server.js --help
node dist/src/server.js --tools     # dump tool list as JSON (useful for debugging)
```

## Environment variables

- `PARSEH_BIN` — path to the `parseh` Rust binary. Defaults to `parseh` on `$PATH`.
- The MCP server otherwise reads nothing from the environment. All state lives in the local `parseh` binary's data dir.

## Status

V0.1. Most tools work; some shell out to `parseh-cli` subcommands that are still landing (e.g. `parseh outcome <hash>` is in flight in a parallel agent's branch). The tool surface and JSON schemas are stable; the binary it shells out to is the moving target. The TypeScript layer absorbs that flux.

## Future work

- Replace this TypeScript server with a Rust-native one once the Rust MCP SDK is production-ready.
- Add streaming tool responses for `parseh_run_tests` (currently buffers until completion).
- Add a `parseh_subscribe_state_deltas` tool that streams gossipsub events as MCP notifications.
- Wire `parseh_listen` against the `tools/parseh-stt/` script once that lands.
