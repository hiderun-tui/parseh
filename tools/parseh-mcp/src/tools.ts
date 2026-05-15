// tools.ts — Definition and dispatch for every MCP tool exposed by
// parseh-mcp. Kept separate from server.ts so the same dispatch table
// can be exercised directly from tests without going through stdio.
//
// Design notes:
//
// - Inputs are validated with hand-rolled type guards (no zod / no
//   ajv) — the schemas the MCP host receives are JSON Schema fragments
//   we hand-write below. This keeps the runtime dependency surface
//   small (only @modelcontextprotocol/sdk).
//
// - Every tool returns a serialisable object. The MCP framing layer in
//   server.ts wraps the object into the protocol's content envelope.
//
// - Tools that talk to the protocol stack go via runParseh / runParsehJson
//   (parseh-cli.ts). Tools that wrap shell scripts (parseh_speak,
//   parseh_listen) spawn the script directly via the same helper.
//
// - Tools that the in-flight parseh-cli does not yet support carry a
//   TODO marker AND fall back to a clearly-labeled "not yet wired"
//   error so the agent never receives stale or hallucinated data.

import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  ParsehCliError,
  runParseh,
  runParsehJson,
} from "./parseh-cli.js";

// ---------------------------------------------------------------------------
// Path resolution for the bundled TTS / STT shell scripts.
// ---------------------------------------------------------------------------
//
// At runtime, this file lives at tools/parseh-mcp/dist/src/tools.js.
// The TTS script lives at tools/parseh-tts/speak.sh and the STT script
// (when it lands) at tools/parseh-stt/listen.sh. We resolve those
// relative to this file's location so the MCP server works whether
// invoked from any cwd.

const HERE = path.dirname(fileURLToPath(import.meta.url));
const TOOLS_DIR = path.resolve(HERE, "..", "..", "..");
const TTS_SCRIPT = path.join(TOOLS_DIR, "parseh-tts", "speak.sh");
const STT_SCRIPT = path.join(TOOLS_DIR, "parseh-stt", "listen.sh");

// ---------------------------------------------------------------------------
// Tool definition shape.
// ---------------------------------------------------------------------------

/**
 * JSON Schema fragment for one tool's input. We hand-write these
 * rather than importing a heavy schema library — every fragment is
 * small enough to read in isolation.
 */
export interface JsonSchema {
  type: "object";
  properties: Record<string, unknown>;
  required?: string[];
  additionalProperties?: boolean;
}

/**
 * One MCP tool, defined declaratively. {@link handler} runs in the
 * server's event loop; it should throw on bad input or upstream
 * failure and return a serialisable object on success.
 */
export interface ToolDefinition {
  name: string;
  description: string;
  inputSchema: JsonSchema;
  /**
   * @param args parsed JSON input from the MCP client
   * @returns any JSON-serialisable value
   */
  handler: (args: Record<string, unknown>) => Promise<unknown>;
}

// ---------------------------------------------------------------------------
// Input-validation helpers. Cheap hand-rolled type guards.
// ---------------------------------------------------------------------------

function requireString(
  args: Record<string, unknown>,
  key: string,
): string {
  const v = args[key];
  if (typeof v !== "string" || v.length === 0) {
    throw new Error(`missing or invalid '${key}': expected non-empty string`);
  }
  return v;
}

function optionalString(
  args: Record<string, unknown>,
  key: string,
): string | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "string") {
    throw new Error(`invalid '${key}': expected string`);
  }
  return v;
}

function optionalBoolean(
  args: Record<string, unknown>,
  key: string,
): boolean | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "boolean") {
    throw new Error(`invalid '${key}': expected boolean`);
  }
  return v;
}

function optionalNumber(
  args: Record<string, unknown>,
  key: string,
): number | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "number" || !Number.isFinite(v)) {
    throw new Error(`invalid '${key}': expected finite number`);
  }
  return v;
}

function optionalEnum<T extends string>(
  args: Record<string, unknown>,
  key: string,
  variants: readonly T[],
): T | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "string" || !variants.includes(v as T)) {
    throw new Error(
      `invalid '${key}': expected one of ${variants.join(", ")}`,
    );
  }
  return v as T;
}

function optionalStringArray(
  args: Record<string, unknown>,
  key: string,
): string[] | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (!Array.isArray(v) || !v.every((x) => typeof x === "string")) {
    throw new Error(`invalid '${key}': expected array of strings`);
  }
  return v as string[];
}

// ---------------------------------------------------------------------------
// Shell-script subprocess helper (for parseh_speak / parseh_listen).
// ---------------------------------------------------------------------------

interface ShellResult {
  stdout: string;
  stderr: string;
  code: number;
}

function runScript(
  script: string,
  args: string[],
  stdin?: string,
): Promise<ShellResult> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    let child;
    try {
      child = spawn(script, args, { stdio: ["pipe", "pipe", "pipe"] });
    } catch (err) {
      reject(new Error(`failed to spawn ${script}: ${err}`));
      return;
    }
    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");
    child.stdout?.on("data", (c: string) => (stdout += c));
    child.stderr?.on("data", (c: string) => (stderr += c));
    child.on("error", (err) => reject(err));
    child.on("close", (code) =>
      resolve({ stdout, stderr, code: code ?? -1 }),
    );
    if (stdin !== undefined) {
      child.stdin?.write(stdin);
      child.stdin?.end();
    } else {
      child.stdin?.end();
    }
  });
}

// ---------------------------------------------------------------------------
// Tool definitions.
// ---------------------------------------------------------------------------

const JOB_KINDS = ["Inference", "Relay", "Storage"] as const;
const SPEAK_LANGS = ["fa", "en"] as const;
const TEST_TARGETS = ["all", "workspace", "acceptance"] as const;

export function buildTools(): ToolDefinition[] {
  return [
    {
      name: "parseh_status",
      description:
        "Get the local PARSEH node's network status (peer id, listen " +
        "addrs, peers connected, capabilities, reputation). No inputs.",
      inputSchema: {
        type: "object",
        properties: {},
        additionalProperties: false,
      },
      handler: async () => {
        // `parseh status --text` writes a structured JSON line on stdout.
        // The brief says --text but the parseh-cli is in flight and we
        // assume a JSON shape. The shell-out is best-effort and surfaces
        // raw stdout as a fallback when parsing fails.
        try {
          return await runParsehJson(["status", "--json"]);
        } catch (err) {
          if (err instanceof ParsehCliError) {
            return {
              warning: "parseh-cli status not yet wired or returned non-JSON",
              stdout: err.result.stdout,
              stderr: err.result.stderr,
              code: err.result.code,
            };
          }
          throw err;
        }
      },
    },

    {
      name: "parseh_submit_job",
      description:
        "Submit a signed JobSpec to the local PARSEH miner. Returns the " +
        "spec content-hash that identifies the job for downstream lookup " +
        "via parseh_query_outcome.",
      inputSchema: {
        type: "object",
        properties: {
          prompt: { type: "string", description: "The job prompt text." },
          kind: {
            type: "string",
            enum: [...JOB_KINDS],
            description: "JobKind variant. Defaults to 'Inference'.",
          },
          sensitive: {
            type: "boolean",
            description:
              "If true, mark the job as sensitive so it is routed only " +
              "to peers that have the privacy capability.",
          },
        },
        required: ["prompt"],
        additionalProperties: false,
      },
      handler: async (args) => {
        const prompt = requireString(args, "prompt");
        const kind = optionalEnum(args, "kind", JOB_KINDS) ?? "Inference";
        const sensitive = optionalBoolean(args, "sensitive") ?? false;

        const cliArgs = [
          "submit",
          "--kind",
          kind,
          ...(sensitive ? ["--sensitive"] : []),
          "--prompt",
          prompt,
          "--json",
        ];
        // Expected shape: { spec_hash: "...", submitted_at: <unix>, ... }
        return await runParsehJson(cliArgs);
      },
    },

    {
      name: "parseh_query_outcome",
      description:
        "Query the local SharedState for a JobOutcome by its spec hash. " +
        "Returns the outcome JSON if found, or null if no outcome has " +
        "been recorded yet for that hash.",
      inputSchema: {
        type: "object",
        properties: {
          spec_hash: {
            type: "string",
            description: "Hex-encoded ContentHash of the JobSpec.",
          },
        },
        required: ["spec_hash"],
        additionalProperties: false,
      },
      handler: async (args) => {
        const specHash = requireString(args, "spec_hash");
        // TODO: parseh-cli `outcome <hash>` subcommand is in flight in
        // the parallel agent's branch. Until that lands we shell out
        // and tolerate non-zero exit (meaning "no outcome yet") by
        // returning null. The shape is intentionally permissive.
        const result = await runParseh(["outcome", specHash, "--json"]);
        if (result.code === 0) {
          try {
            return JSON.parse(result.stdout);
          } catch {
            return { raw: result.stdout, warning: "non-JSON stdout" };
          }
        }
        // Exit code 2 in the planned subcommand will mean "not found".
        if (result.code === 2) return null;
        throw new ParsehCliError(
          `parseh outcome ${specHash} exited with code ${result.code}: ${result.stderr.trim()}`,
          result,
        );
      },
    },

    {
      name: "parseh_list_peers",
      description:
        "List known peers with their capability set + reputation. " +
        "Optionally filter by minimum reputation and/or required service.",
      inputSchema: {
        type: "object",
        properties: {
          min_reputation: {
            type: "number",
            description: "Filter out peers below this reputation score.",
          },
          service: {
            type: "string",
            description:
              "Filter to peers advertising this service (Inference, " +
              "Relay, Storage).",
          },
        },
        additionalProperties: false,
      },
      handler: async (args) => {
        const minRep = optionalNumber(args, "min_reputation");
        const service = optionalString(args, "service");
        const cliArgs = ["peers", "--json"];
        if (minRep !== undefined) cliArgs.push("--min-reputation", String(minRep));
        if (service !== undefined) cliArgs.push("--service", service);
        return await runParsehJson(cliArgs);
      },
    },

    {
      name: "parseh_detect_llm",
      description:
        "Probe the local machine for installed LLM runtimes (Ollama, " +
        "llama.cpp, GGUF model files, GPU). Wraps parseh-llm-detect.",
      inputSchema: {
        type: "object",
        properties: {},
        additionalProperties: false,
      },
      handler: async () => {
        return await runParsehJson(["detect-llm", "--json"]);
      },
    },

    {
      name: "parseh_speak",
      description:
        "Text-to-speech in Persian (or English). Wraps tools/parseh-tts " +
        "and runs entirely locally — zero external network egress.",
      inputSchema: {
        type: "object",
        properties: {
          text: { type: "string", description: "The text to speak." },
          lang: {
            type: "string",
            enum: [...SPEAK_LANGS],
            description: "Language: 'fa' (default) or 'en'.",
          },
          save_path: {
            type: "string",
            description:
              "If set, write the WAV to this path instead of playing it " +
              "back through speakers.",
          },
        },
        required: ["text"],
        additionalProperties: false,
      },
      handler: async (args) => {
        const text = requireString(args, "text");
        const lang = optionalEnum(args, "lang", SPEAK_LANGS) ?? "fa";
        const savePath = optionalString(args, "save_path");
        const scriptArgs: string[] = ["--lang", lang];
        if (savePath !== undefined) scriptArgs.push("--save", savePath);
        // Pass text via stdin to avoid argv-length / quoting headaches
        // with long Persian prompts that contain shell metacharacters.
        const result = await runScript(TTS_SCRIPT, scriptArgs, text);
        if (result.code !== 0) {
          throw new Error(
            `parseh-tts speak.sh exited ${result.code}: ${result.stderr.trim()}`,
          );
        }
        return {
          ok: true,
          lang,
          saved_to: savePath ?? null,
          message: result.stdout.trim() || "spoke (no stdout)",
        };
      },
    },

    {
      name: "parseh_listen",
      description:
        "Speech-to-text for Persian. Wraps tools/parseh-stt/listen.sh " +
        "when installed; runs entirely locally. Either point at an " +
        "existing audio file or record from the microphone for N seconds.",
      inputSchema: {
        type: "object",
        properties: {
          audio_path: {
            type: "string",
            description: "Path to a WAV/FLAC/MP3 file to transcribe.",
          },
          seconds: {
            type: "number",
            description:
              "Seconds to record from the default microphone. Ignored " +
              "when audio_path is set.",
          },
        },
        additionalProperties: false,
      },
      handler: async (args) => {
        const audioPath = optionalString(args, "audio_path");
        const seconds = optionalNumber(args, "seconds");
        const scriptArgs: string[] = [];
        if (audioPath !== undefined) scriptArgs.push("--file", audioPath);
        if (seconds !== undefined) scriptArgs.push("--seconds", String(seconds));
        const result = await runScript(STT_SCRIPT, scriptArgs);
        if (result.code !== 0) {
          // STT is not yet in the repo — surface a clear stub message.
          throw new Error(
            `parseh-stt listen.sh not available or failed (exit ${result.code}): ${result.stderr.trim()}`,
          );
        }
        return { transcription: result.stdout.trim() };
      },
    },

    {
      name: "parseh_run_tests",
      description:
        "Run V0.2 acceptance + unit tests. Returns a structured test " +
        "report. Use target='acceptance' for the 3-node testnet harness.",
      inputSchema: {
        type: "object",
        properties: {
          target: {
            type: "string",
            enum: [...TEST_TARGETS],
            description: "'all' | 'workspace' | 'acceptance'. Default: 'all'.",
          },
        },
        additionalProperties: false,
      },
      handler: async (args) => {
        const target = optionalEnum(args, "target", TEST_TARGETS) ?? "all";
        return await runParsehJson(["test", "--target", target, "--json"]);
      },
    },

    {
      name: "parseh_open_issue",
      description:
        "Open a GitHub issue against the parseh repo with the given " +
        "title/body and optional labels. Uses local `gh` CLI; no API " +
        "tokens leave the machine. Returns the issue URL.",
      inputSchema: {
        type: "object",
        properties: {
          title: { type: "string" },
          body: { type: "string" },
          labels: {
            type: "array",
            items: { type: "string" },
            description: "Optional list of label names to apply.",
          },
        },
        required: ["title", "body"],
        additionalProperties: false,
      },
      handler: async (args) => {
        const title = requireString(args, "title");
        const body = requireString(args, "body");
        const labels = optionalStringArray(args, "labels") ?? [];
        // Shell out to `gh issue create` rather than hitting the GitHub
        // API directly — `gh` uses the local user's auth and respects
        // their proxy / mirror configuration. The MCP server itself
        // never makes an HTTP call.
        const cliArgs = ["issue", "create", "--title", title, "--body", body];
        for (const label of labels) cliArgs.push("--label", label);
        return await new Promise<unknown>((resolve, reject) => {
          let stdout = "";
          let stderr = "";
          const child = spawn("gh", cliArgs, { stdio: ["ignore", "pipe", "pipe"] });
          child.stdout?.setEncoding("utf8");
          child.stderr?.setEncoding("utf8");
          child.stdout?.on("data", (c: string) => (stdout += c));
          child.stderr?.on("data", (c: string) => (stderr += c));
          child.on("error", (err) => reject(err));
          child.on("close", (code) => {
            if (code !== 0) {
              reject(
                new Error(
                  `gh issue create exited ${code}: ${stderr.trim() || "no stderr"}`,
                ),
              );
              return;
            }
            // gh prints the issue URL on stdout when successful.
            resolve({ url: stdout.trim() });
          });
        });
      },
    },
  ];
}

/**
 * Wrap a handler call so unknown errors come out as a structured MCP
 * error payload rather than propagating a raw exception across the
 * stdio boundary.
 */
export async function invokeTool(
  tool: ToolDefinition,
  args: Record<string, unknown>,
): Promise<{ ok: true; value: unknown } | { ok: false; error: string }> {
  try {
    const value = await tool.handler(args);
    return { ok: true, value };
  } catch (err) {
    const message =
      err instanceof Error ? err.message : String(err);
    return { ok: false, error: message };
  }
}
