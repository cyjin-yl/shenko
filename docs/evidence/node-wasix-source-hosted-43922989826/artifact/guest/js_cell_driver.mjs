// Shenko JS cell driver v1 (long-lived session protocol) — native Node
// (>= 18, ESM) and Bun (>= 1). The process stays alive across cells;
// globals persist in interpreter memory for the process lifetime.
//
// Host -> driver framing on stdin (blocking, exact reads):
//     [u64be header_len][header JSON][u64be payload_len][cell source bytes]
// header JSON: {"seq": int, "cell_dir": str, "state_path": str, "env_dir": str}
// (host paths; this driver runs natively).
//
// Per cell the driver writes <cell_dir>/<seq>.out, <seq>.err and renames
// <seq>.report.json into place last (host treats its appearance as cell
// completion). Report JSON: {"ok","error","dropped","state_bytes",
// "state_verified"}. The state file holds only JSON-serializable own globals;
// functions and non-serializable values are dropped and reported by name.
//
// The actual interpreter REPL evaluator owns the persistent lexical context.
// require is resolved from the active environment's package.json. Native
// REPL syntax/async-import support is the bound interpreter's actual API,
// not a fresh ESM module pretending that its lexical bindings persist.
// console.log / process.stdout.write route to per-cell owned output files.
//
// EOF on stdin exits cleanly (dispose).

import { appendFileSync, createWriteStream, mkdirSync, readFileSync, readSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { start as startRepl } from "node:repl";
import { PassThrough } from "node:stream";
import { createRequire } from "node:module";

// The interpreter's real REPL context retains lexical bindings and functions
// across cells. Importing a fresh ESM file per cell does not do that.
const controlInput = new PassThrough();
const controlOutput = new PassThrough();
controlOutput.resume(); // REPL prompts/control, not authored cell output.
const replServer = startRepl({ input: controlInput, output: controlOutput,
    terminal: false, useGlobal: false, ignoreUndefined: true });
let hydrated = false;

function completionNames() {
    return new Promise((resolve, reject) => replServer.completer("", (error, result) =>
        error ? reject(error) : resolve(result[0])));
}
function evaluate(source, filename) {
    return new Promise((resolve, reject) => replServer.eval(source,
        replServer.context, filename, (error, value) => error ? reject(error) : resolve(value)));
}
const initialBindings = new Set(await completionNames());

function readExact(fd, count) {
    const buffer = Buffer.alloc(count);
    let offset = 0;
    while (offset < count) {
        const n = readSync(fd, buffer, offset, count - offset, null);
        if (!n) return null; // EOF
        offset += n;
    }
    return buffer;
}

const RESERVED = new Set([
    "globalThis", "process", "console", "Buffer", "fetch", "URL", "URLSearchParams",
    "TextEncoder", "TextDecoder", "atob", "btoa",
    "setTimeout", "clearTimeout", "setInterval", "clearInterval",
    "setImmediate", "clearImmediate", "queueMicrotask",
    "performance", "crypto", "navigator", "structuredClone",
    "AbortController", "AbortSignal", "Event", "EventTarget", "MessageEvent",
    "CloseEvent", "WebSocket", "XMLHttpRequest",
    "localStorage", "sessionStorage", "indexedDB", "location",
    "document", "window", "self", "global", "require", "module", "exports",
    "__filename", "__dirname", "argv", "Bun", "Deno", "std", "os", "scriptArgs",
    "Object", "Array", "String", "Number", "Boolean", "Symbol", "BigInt",
    "Math", "JSON", "Date", "RegExp", "Error", "TypeError", "RangeError",
    "SyntaxError", "ReferenceError", "EvalError", "URIError", "AggregateError",
    "Map", "Set", "WeakMap", "WeakSet", "Promise", "Function", "Reflect", "Proxy",
    "parseInt", "parseFloat", "isNaN", "isFinite", "NaN", "Infinity",
    "undefined", "null", "eval", "ArrayBuffer", "SharedArrayBuffer", "DataView",
    "Uint8Array", "Int8Array", "Uint16Array", "Int16Array", "Uint32Array",
    "Int32Array", "Float32Array", "Float64Array", "Uint8ClampedArray",
    "BigInt64Array", "BigUint64Array", "Intl", "Temporal",
]);

function loadState(statePath) {
    try { return JSON.parse(readFileSync(statePath, "utf8")); }
    catch (error) { if (error.code === "ENOENT") return {}; throw error; }
}

async function persistState(statePath, report) {
    const persisted = {};
    const dropped = [];
    // Use the actual interpreter's completion/lexical scope view, not source
    // keyword rewriting or an ESM module's unrelated global object.
    const names = new Set([...await completionNames(), ...Object.getOwnPropertyNames(replServer.context)]);
    for (const key of names) {
        if (initialBindings.has(key) || key.startsWith("__") || RESERVED.has(key)
            || !/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(key)) continue;
        let value;
        try { value = await evaluate(key, "<shenko-checkpoint>"); }
        catch { dropped.push(key); continue; }
        if (typeof value === "undefined" || typeof value === "function" || typeof value === "symbol") {
            dropped.push(key);
            continue;
        }
        try {
            const round = JSON.parse(JSON.stringify(value));
            if (typeof round === "undefined") { dropped.push(key); continue; }
            persisted[key] = round;
        } catch {
            dropped.push(key);
        }
    }
    const body = JSON.stringify(persisted);
    writeFileSync(statePath + ".tmp", body);
    renameSync(statePath + ".tmp", statePath);
    report.dropped = dropped.sort();
    report.state_bytes = Buffer.byteLength(body);
    try {
        const check = JSON.parse(readFileSync(statePath, "utf8"));
        report.state_verified = typeof check === "object" && check !== null;
    } catch {
        report.state_verified = false;
    }
}

async function runCell(header, source) {
    const seq = header.seq;
    const cellDir = header.cell_dir;
    const statePath = header.state_path;
    const envDir = header.env_dir;
    mkdirSync(cellDir, { recursive: true });
    mkdirSync(envDir, { recursive: true });


    const outPath = join(cellDir, seq + ".out");
    const errPath = join(cellDir, seq + ".err");
    const outStream = createWriteStream(outPath);
    const errStream = createWriteStream(errPath);
    const outWrite = (chunk) => outStream.write(String(chunk));
    const errWrite = (chunk) => errStream.write(String(chunk));

    const oldLog = console.log;
    const oldError = console.error;
    const oldStdoutWrite = process.stdout.write.bind(process.stdout);
    const oldStderrWrite = process.stderr.write.bind(process.stderr);
    console.log = (...args) => outWrite(args.map(String).join(" ") + "\n");
    console.error = (...args) => errWrite(args.map(String).join(" ") + "\n");
    process.stdout.write = (chunk, ...rest) => { outWrite(chunk); return true; };
    process.stderr.write = (chunk, ...rest) => { errWrite(chunk); return true; };

    const report = { ok: false, error: null, dropped: [],
                     state_bytes: 0, state_verified: false };
    try {
        if (!hydrated) {
            for (const [key, value] of Object.entries(loadState(statePath))) {
                replServer.context[key] = value;
            }
            hydrated = true;
        }
        replServer.context.require = createRequire(join(envDir, "package.json"));
        replServer.context.console = console;
        const cellFile = join(envDir, ".shenko-cell-" + seq + ".js");
        await evaluate(source, cellFile);
        report.ok = true;
    } catch (e) {
        errWrite((e && e.stack ? e.stack : String(e)) + "\n");
        report.error = "cell raised; see " + seq + ".err";
    } finally {
        console.log = oldLog;
        console.error = oldError;
        process.stdout.write = oldStdoutWrite;
        process.stderr.write = oldStderrWrite;
        await new Promise((resolve) => outStream.end(resolve));
        await new Promise((resolve) => errStream.end(resolve));
    }

    try {
        await persistState(statePath, report);
    } catch (e) {
        report.ok = false;
        report.error = (report.error || "") + "\nstate-persist-failed: " + String(e);
    }

    const finalPath = join(cellDir, seq + ".report.json");
    writeFileSync(finalPath + ".tmp", JSON.stringify(report));
    renameSync(finalPath + ".tmp", finalPath);
}

async function main() {
    for (;;) {
        const headerLenRaw = readExact(0, 8);
        if (!headerLenRaw) return 0;
        const headerLen = Number(headerLenRaw.readBigUInt64BE(0));
        const headerRaw = readExact(0, headerLen);
        if (!headerRaw) return 0;
        const payloadLenRaw = readExact(0, 8);
        if (!payloadLenRaw) return 0;
        const payloadLen = Number(payloadLenRaw.readBigUInt64BE(0));
        const sourceRaw = readExact(0, payloadLen);
        if (!sourceRaw) return 0;
        try {
            const header = JSON.parse(headerRaw.toString("utf8"));
            await runCell(header, sourceRaw.toString("utf8"));
        } catch (e) {
            try {
                appendFileSync(2, (e && e.stack ? e.stack : String(e)) + "\n");
            } catch {}
        }
    }
}

process.exit(await main());
