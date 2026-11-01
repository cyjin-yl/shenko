// Shenko QuickJS cell driver v1 (long-lived session protocol).
//
// Runs on a QuickJS-for-WASI build exposing the qjs `std` module
// (std.in/out/err, std.open, std.evalScript). The process stays alive across
// cells; globals persist in interpreter memory for the process lifetime.
//
// Host -> driver framing on stdin (blocking, exact reads):
//     [u64be header_len][header JSON][u64be payload_len][cell source bytes]
// header JSON: {"seq": int, "cell_dir": str, "state_path": str, "env_dir": str}
//
// Per cell the driver writes <cell_dir>/<seq>.out, <seq>.err and renames
// <seq>.report.json into place last (host treats its appearance as cell
// completion). Report JSON: {"ok","error","dropped","state_bytes",
// "state_verified"}. The state file holds only JSON-serializable own globals;
// functions and non-serializable values are dropped and reported by name.
//
// Cells evaluate at global scope. The global `print` is re-bound per cell to
// the per-cell out file; code writing via std.out directly bypasses it (a
// documented per-kernel limitation).
//
// EOF on stdin exits cleanly (dispose).

import * as std from "qjs:std";
import * as os from "qjs:os";

function atomicRename(from, to) {
    const error = os.rename(from, to);
    if (error !== 0) throw new Error("atomic rename failed: " + error);
}

function decodeUtf8(buffer) {
    const bytes = new Uint8Array(buffer);
    const result = [];
    for (let i = 0; i < bytes.length;) {
        const first = bytes[i++];
        if (first < 128) { result.push(String.fromCharCode(first)); continue; }
        const width = first >= 0xc2 && first <= 0xdf ? 2 : first >= 0xe0 && first <= 0xef ? 3 : first >= 0xf0 && first <= 0xf4 ? 4 : 0;
        let value = first & (width === 2 ? 31 : width === 3 ? 15 : 7);
        let valid = width !== 0 && i + width - 1 <= bytes.length;
        for (let j = 1; valid && j < width; j++) {
            const next = bytes[i + j - 1];
            if ((next & 0xc0) !== 0x80) valid = false;
            else value = value * 64 + (next & 63);
        }
        if (!valid || value < (width === 2 ? 0x80 : width === 3 ? 0x800 : 0x10000)
            || value > 0x10ffff || (value >= 0xd800 && value <= 0xdfff)) {
            result.push("\ufffd"); continue;
        }
        i += width - 1;
        result.push(String.fromCodePoint(value));
    }
    return result.join("");
}

function readExact(file, count) {
    const buffer = new ArrayBuffer(count);

    let offset = 0;
    while (offset < count) {
        const n = file.read(buffer, offset, count - offset);
        if (n < 0) throw new Error("framed input read failed: " + n);
        if (!n) return null; // EOF
        offset += n;
    }
    return buffer;
}

const RESERVED = new Set([
    "std", "os", "scriptArgs", "print", "console", "globalThis",
    "Date", "Math", "JSON", "Object", "Array", "String", "Number", "Boolean",
    "RegExp", "Error", "TypeError", "RangeError", "SyntaxError", "ReferenceError",
    "Map", "Set", "WeakMap", "WeakSet", "Promise", "Symbol", "BigInt", "Function",
    "parseInt", "parseFloat", "isNaN", "isFinite", "NaN", "Infinity",
    "undefined", "null", "eval",
    "ArrayBuffer", "SharedArrayBuffer", "DataView", "Uint8Array", "Int8Array",
    "Uint16Array", "Int16Array", "Uint32Array", "Int32Array", "Float32Array",
    "Float64Array", "Uint8ClampedArray", "BigInt64Array", "BigUint64Array",
    "TextEncoder", "TextDecoder", "encodeURIComponent", "decodeURIComponent",
    "setTimeout", "clearTimeout", "setInterval", "clearInterval",
    "queueMicrotask", "structuredClone",
]);

let hydrated = false;

function loadState(statePath) {
        const status = {};
        const handle = std.open(statePath, "r", status);
        if (!handle) {
            if (status.errno === std.Error.ENOENT) return {};
            throw new Error("checkpoint open failed: " + status.errno);
        }
        const raw = handle.readAsString();
        handle.close();
        return JSON.parse(raw);
}

function persistState(statePath, report) {
    const persisted = {};
    const dropped = [];
    for (const key of Object.getOwnPropertyNames(globalThis)) {
        if (key.startsWith("__") || RESERVED.has(key)) continue;
        const value = globalThis[key];
        if (typeof value === "undefined" || typeof value === "function" || typeof value === "symbol") {
            dropped.push(key);
            continue;
        }
        try {
            const round = JSON.parse(JSON.stringify(value));
            if (typeof round === "undefined") { dropped.push(key); continue; }
            persisted[key] = round;
        } catch (e) {
            dropped.push(key);
        }
    }
    const body = JSON.stringify(persisted);
    const handle = std.open(statePath + ".tmp", "w");
    if (!handle) throw new Error("cannot open state file");
    handle.puts(body);
    handle.close();
    atomicRename(statePath + ".tmp", statePath);
    report.dropped = dropped.sort();
    const [metadata, statError] = os.stat(statePath);
    if (statError !== 0) throw new Error("checkpoint stat failed: " + statError);
    report.state_bytes = metadata.size;
    try {
        const check = loadState(statePath);
        report.state_verified = typeof check === "object" && check !== null;
    } catch (e) {
        report.state_verified = false;
    }
}

function runCell(header, source) {
    const seq = header.seq;
    const cellDir = header.cell_dir;
    const statePath = header.state_path;

    // Environment switching is per cell: the header carries the active
    // generation's env dir. For QuickJS there is no sys.path equivalent;
    // env content is advisory unless the cell imports it by explicit path.


    const outPath = cellDir + "/" + seq + ".out";
    const errPath = cellDir + "/" + seq + ".err";
    let outHandle = null, errHandle = null;
    const report = { ok: false, error: null, dropped: [],
                     state_bytes: 0, state_verified: false };

    try {
        outHandle = std.open(outPath, "w");
        errHandle = std.open(errPath, "w");
        globalThis.print = (...args) => {
            outHandle.puts(args.map(String).join(" ") + "\n");
        };
        try {
            if (!hydrated) {
                for (const [key, value] of Object.entries(loadState(statePath))) {
                    globalThis[key] = value;
                }
                hydrated = true;
            }
            std.evalScript(source, { backtraceBarrier: false });
            report.ok = true;
        } catch (e) {
            errHandle.puts(String((e && e.stack) ? e.stack : e) + "\n");
            report.error = "cell raised; see " + seq + ".err";
        }
    } finally {
        if (outHandle) outHandle.close();
        if (errHandle) errHandle.close();
    }

    try {
        persistState(statePath, report);
    } catch (e) {
        report.ok = false;
        report.error = (report.error || "") + "\nstate-persist-failed: " + String(e);
    }

    const finalPath = cellDir + "/" + seq + ".report.json";
    const tmpHandle = std.open(finalPath + ".tmp", "w");
    tmpHandle.puts(JSON.stringify(report));
    tmpHandle.close();
    atomicRename(finalPath + ".tmp", finalPath);
}

globalThis.std = std;

function main() {
    for (;;) {
        const headerLenRaw = readExact(std.in, 8);
        if (!headerLenRaw) return 0;
        const headerLength = new DataView(headerLenRaw);
        if (headerLength.getUint32(0) !== 0) throw new Error("header exceeds wasm32 address space");
        const headerLen = headerLength.getUint32(4);
        const headerRaw = readExact(std.in, headerLen);
        if (!headerRaw) return 0;
        const payloadLenRaw = readExact(std.in, 8);
        if (!payloadLenRaw) return 0;
        const payloadLength = new DataView(payloadLenRaw);
        if (payloadLength.getUint32(0) !== 0) throw new Error("source exceeds wasm32 address space");
        const payloadLen = payloadLength.getUint32(4);
        const sourceRaw = readExact(std.in, payloadLen);
        if (!sourceRaw) return 0;
        let header;
        try {
            header = JSON.parse(decodeUtf8(headerRaw));
            const source = decodeUtf8(sourceRaw);
            runCell(header, source);
        } catch (e) {
            std.err.puts(String((e && e.stack) ? e.stack : e) + "\n");
            std.err.flush();
        }
    }
}

main();
