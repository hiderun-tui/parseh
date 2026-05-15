// parseh-cli — thin Node.js wrapper around the local `parseh` Rust binary.
//
// This module is the SOLE point of contact between the MCP layer and the
// PARSEH protocol stack. Every MCP tool implementation funnels through
// `runParseh`. Reasons:
//
// 1. Single seam for the future Rust-native MCP server — when that lands,
//    only this file (and the dispatch in server.ts) gets replaced.
// 2. Single place to enforce the no-cloud-egress rule: this module spawns
//    a local subprocess and nothing else. There are no fetch/http imports
//    anywhere in tools/parseh-mcp/.
// 3. Testability — tests inject a fake binary path via env (PARSEH_BIN)
//    that points at a shell-script stub returning canned JSON.
//
// The current parseh-cli surface is in flight (see report). Subcommands
// the brief references that do not yet exist are documented as TODO at
// the call site; this module does not assume they exist — it just spawns
// whatever args it is given and surfaces stdout/stderr/exit-code.

import { spawn } from "node:child_process";

/**
 * Options accepted by {@link runParseh}.
 */
export interface ParsehCliOptions {
  /**
   * Override the binary that gets spawned. Defaults, in order:
   *  1. opts.bin (explicit caller override)
   *  2. process.env.PARSEH_BIN
   *  3. "parseh" (resolved against PATH)
   *
   * Tests set PARSEH_BIN to a shell-script stub.
   */
  bin?: string;

  /**
   * Hard timeout in milliseconds. The child is sent SIGTERM, then
   * SIGKILL 250ms later if it does not exit cleanly. Default: 30_000.
   */
  timeoutMs?: number;

  /**
   * Optional stdin payload. When set, the string is written and the
   * stream closed before we read stdout. Used for stdio-of-text tools
   * like parseh_speak which take Persian text on stdin.
   */
  stdin?: string;

  /**
   * Optional working directory for the spawned process. Defaults to
   * the MCP server's cwd.
   */
  cwd?: string;
}

/**
 * Structured result of a parseh-cli invocation. Every MCP tool inspects
 * `code` first — non-zero is a hard error and surfaces as an MCP-level
 * error response.
 */
export interface ParsehCliResult {
  stdout: string;
  stderr: string;
  code: number;
  /** True when the timeout fired before the child exited. */
  timedOut: boolean;
}

/**
 * Error thrown when a parseh-cli invocation fails in a way the caller
 * should treat as a hard error (non-zero exit, timeout, spawn failure).
 *
 * The {@link result} field carries the full stdout/stderr/code triple
 * so MCP tools can surface a helpful diagnostic to the agent.
 */
export class ParsehCliError extends Error {
  public readonly result: ParsehCliResult;

  constructor(message: string, result: ParsehCliResult) {
    super(message);
    this.name = "ParsehCliError";
    this.result = result;
  }
}

const DEFAULT_TIMEOUT_MS = 30_000;

/**
 * Resolve the parseh binary path from caller options / env / default.
 */
export function resolveParsehBin(opts: ParsehCliOptions = {}): string {
  if (opts.bin && opts.bin.length > 0) return opts.bin;
  const fromEnv = process.env.PARSEH_BIN;
  if (fromEnv && fromEnv.length > 0) return fromEnv;
  return "parseh";
}

/**
 * Spawn the parseh-cli binary with the given args, capture stdout/stderr,
 * enforce a hard timeout, and return a structured result.
 *
 * Never throws on non-zero exit code — caller decides whether non-zero
 * is an error or expected (e.g. `parseh_query_outcome` returning null
 * when an outcome does not exist may use a non-zero exit).
 *
 * Throws {@link ParsehCliError} only on spawn failure (binary not found,
 * permission denied) or on timeout.
 */
export function runParseh(
  args: string[],
  opts: ParsehCliOptions = {},
): Promise<ParsehCliResult> {
  const bin = resolveParsehBin(opts);
  const timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;

  return new Promise<ParsehCliResult>((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let child;

    try {
      child = spawn(bin, args, {
        cwd: opts.cwd,
        stdio: ["pipe", "pipe", "pipe"],
        // Inherit env so PARSEH_DATA_DIR and friends propagate.
        env: process.env,
      });
    } catch (err) {
      const result: ParsehCliResult = {
        stdout: "",
        stderr: String(err),
        code: -1,
        timedOut: false,
      };
      reject(new ParsehCliError(`failed to spawn ${bin}: ${err}`, result));
      return;
    }

    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
      setTimeout(() => {
        if (!child.killed) child.kill("SIGKILL");
      }, 250);
    }, timeoutMs);

    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr?.on("data", (chunk: string) => {
      stderr += chunk;
    });

    child.on("error", (err) => {
      clearTimeout(timer);
      const result: ParsehCliResult = {
        stdout,
        stderr: stderr || String(err),
        code: -1,
        timedOut,
      };
      reject(new ParsehCliError(`parseh-cli spawn error: ${err}`, result));
    });

    child.on("close", (code) => {
      clearTimeout(timer);
      const result: ParsehCliResult = {
        stdout,
        stderr,
        code: code ?? -1,
        timedOut,
      };
      if (timedOut) {
        reject(
          new ParsehCliError(
            `parseh-cli timed out after ${timeoutMs}ms (args: ${args.join(" ")})`,
            result,
          ),
        );
        return;
      }
      resolve(result);
    });

    if (opts.stdin !== undefined) {
      child.stdin?.write(opts.stdin);
      child.stdin?.end();
    } else {
      child.stdin?.end();
    }
  });
}

/**
 * Convenience: run parseh-cli and parse stdout as JSON.
 *
 * Treats non-zero exit as an error (most JSON-producing subcommands
 * should never exit non-zero unless something is genuinely wrong).
 */
export async function runParsehJson<T = unknown>(
  args: string[],
  opts: ParsehCliOptions = {},
): Promise<T> {
  const result = await runParseh(args, opts);
  if (result.code !== 0) {
    throw new ParsehCliError(
      `parseh ${args.join(" ")} exited with code ${result.code}: ${result.stderr.trim()}`,
      result,
    );
  }
  try {
    return JSON.parse(result.stdout) as T;
  } catch (err) {
    throw new ParsehCliError(
      `parseh ${args.join(" ")} produced non-JSON stdout: ${err}`,
      result,
    );
  }
}
