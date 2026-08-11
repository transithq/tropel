// Minimal postcard (v1.1.3) codec for the tropel-web C ABI wire types
// (TROPEL_WASM_BUILD.md Step 5A / Shape A).
//
// Postcard rules implemented here (verified against postcard 1.1.3 source):
//   - unsigned ints:   LEB128 varint (u8/u16/u32/u64/usize)
//   - f64:             8 bytes, little-endian IEEE bits (`to_bits().to_le_bytes()`)
//   - str:             varint byte-length + UTF-8 bytes
//   - bytes / Vec<u8>: varint length + raw bytes
//   - Option:          0x00 = None, 0x01 = Some + value
//   - unit enum:       varint variant index (SampleType: Point=0 Counter=1
//                      Trend=2 Rate=3; ResponseType: Text=0 Binary=1 None=2)
//   - struct:          fields in declaration order, no names
//   - map<str,str>:    varint entry count + (key, value) pairs
//   - Duration:        struct { secs: u64, nanos: u32 } (serde std impl)
//   - SystemTime:      struct { secs_since_epoch: u64, nanos_since_epoch: u32 }
//   - bool:            u8 0/1

import type {
  HttpRequest,
  HttpResponse,
  IterationResult,
  RunOutcome,
  RunRequest,
  Sample,
  Timings,
} from "./types.js";

// ── primitives ───────────────────────────────────────────────────────────

export class Writer {
  private out: number[] = [];

  varint(v: number): void {
    let x = Math.trunc(v);
    if (x < 0) throw new Error("postcard: negative varint");
    while (x >= 0x80) {
      this.out.push((x & 0x7f) | 0x80);
      x = Math.floor(x / 128);
    }
    this.out.push(x);
  }

  u8(v: number): void {
    this.out.push(v & 0xff);
  }

  f64(v: number): void {
    const buf = new ArrayBuffer(8);
    new DataView(buf).setFloat64(0, v, true);
    for (const b of new Uint8Array(buf)) this.out.push(b);
  }

  str(s: string): void {
    const bytes = new TextEncoder().encode(s);
    this.varint(bytes.length);
    for (const b of bytes) this.out.push(b);
  }

  bytes(b: Uint8Array): void {
    this.varint(b.length);
    for (const x of b) this.out.push(x);
  }

  optionSome(): void {
    this.u8(1);
  }

  optionNone(): void {
    this.u8(0);
  }

  strMap(m: Record<string, string>): void {
    const entries = Object.entries(m);
    this.varint(entries.length);
    for (const [k, v] of entries) {
      this.str(k);
      this.str(v);
    }
  }

  toUint8Array(): Uint8Array {
    return new Uint8Array(this.out);
  }
}

export class Reader {
  private pos = 0;

  constructor(private readonly buf: Uint8Array) {}

  private ensure(n: number): void {
    if (this.pos + n > this.buf.length) {
      throw new Error(
        `postcard: unexpected end of input (need ${n} byte(s) at offset ${this.pos}, have ${this.buf.length})`
      );
    }
  }

  varint(): number {
    // ARITHMETIC accumulation, NOT bitwise: `<<` and `|` coerce to 32-bit,
    // which silently corrupts any u64 > 2^32 and SystemTime secs past 2038
    // (postcard review catch). Real values (epoch secs ~1.8e9, counters)
    // stay far inside the 2^53 safe-integer range.
    let result = 0;
    let shift = 0;
    for (;;) {
      this.ensure(1);
      const b = this.buf[this.pos++];
      result += (b & 0x7f) * 2 ** shift;
      if ((b & 0x80) === 0) break;
      shift += 7;
      if (shift > 52) throw new Error("postcard: varint exceeds JS safe-integer range");
    }
    return result;
  }

  u8(): number {
    this.ensure(1);
    return this.buf[this.pos++];
  }

  f64(): number {
    this.ensure(8);
    const v = new DataView(this.buf.buffer, this.buf.byteOffset + this.pos, 8).getFloat64(0, true);
    this.pos += 8;
    return v;
  }

  str(): string {
    const len = this.varint();
    this.ensure(len);
    const s = new TextDecoder().decode(this.buf.subarray(this.pos, this.pos + len));
    this.pos += len;
    return s;
  }

  bytes(): Uint8Array {
    const len = this.varint();
    this.ensure(len);
    const out = this.buf.slice(this.pos, this.pos + len);
    this.pos += len;
    return out;
  }

  optionTag(): boolean {
    return this.u8() === 1;
  }

  strMap(): Record<string, string> {
    const n = this.varint();
    const out: Record<string, string> = {};
    for (let i = 0; i < n; i++) {
      const k = this.str();
      const v = this.str();
      out[k] = v;
    }
    return out;
  }

  /** Seconds + nanos for a postcard Duration/SystemTime struct. */
  durationSecsNanos(): { secs: number; nanos: number } {
    const secs = this.varint();
    const nanos = this.varint();
    return { secs, nanos };
  }
}

// ── RunRequest (host → wasm) ─────────────────────────────────────────────

export function encodeRunRequest(req: RunRequest): Uint8Array {
  const w = new Writer();
  w.str(req.scenarioJson);
  w.varint(req.vuId);
  w.str(req.scenarioName);
  w.varint(req.iterations);
  w.strMap(req.envVars);
  w.varint(req.expectedStatuses.length);
  for (const s of req.expectedStatuses) w.str(s);
  return w.toUint8Array();
}

// ── RunOutcome (wasm → host) ─────────────────────────────────────────────

const SAMPLE_TYPE: readonly ["Point", "Counter", "Trend", "Rate"] = [
  "Point",
  "Counter",
  "Trend",
  "Rate",
];

function readSample(r: Reader): Sample {
  const metric = r.str();
  const value = r.f64();
  const tags = r.strMap();
  const { secs, nanos } = r.durationSecsNanos(); // SystemTime
  const typeIdx = r.varint();
  return {
    metric,
    value,
    tags,
    timestampSecs: secs,
    timestampNanos: nanos,
    sampleType: SAMPLE_TYPE[typeIdx] ?? `Unknown(${typeIdx})`,
  };
}

function readIteration(r: Reader): IterationResult {
  const samples: Sample[] = [];
  const n = r.varint();
  for (let i = 0; i < n; i++) samples.push(readSample(r));
  const iterationIndex = r.varint();
  const scriptFailures = r.varint();
  return { samples, iterationIndex, scriptFailures };
}

export function decodeRunOutcome(bytes: Uint8Array): RunOutcome {
  const r = new Reader(bytes);
  const iterations: IterationResult[] = [];
  const n = r.varint();
  for (let i = 0; i < n; i++) iterations.push(readIteration(r));
  let error: string | null = null;
  if (r.optionTag()) error = r.str();
  return { iterations, error };
}

// ── Request (wasm → host, across the tropel_host_http bridge) ────────────

// Decode only the fields the JS host needs to perform an HTTP request. The
// wire Request (http.rs WireRequest) is: url, method (plain str), headers,
// query_params, body (Option<str> — a JSON envelope, see body_to_wire).
// Nothing follows body on the wire: auth/certificate/follow_redirects/
// timeout/response_type are not part of the v1 wire, so the position stops
// here (line-44 P0 — the native Body form put ambiguous map/JSON-value
// shapes here and the decoder produced a 1-char garbage body for JSON POSTs).
export function decodeHttpRequest(bytes: Uint8Array): HttpRequest {
  const r = new Reader(bytes);
  const url = r.str();
  const method = r.str();
  const headers = r.strMap();
  const queryParams = r.strMap();
  let body: string | null = null;
  let bodyMode: HttpRequest["bodyMode"] = null;
  let bodyBytes: Uint8Array | undefined;
  let formFields: Record<string, string> | undefined;
  const bodyDecoded = r.optionTag();
  if (bodyDecoded) {
    // The wire body is ALWAYS a JSON envelope string (http.rs body_to_wire)
    // — no postcard shape-sniffing. mode tags the variant unambiguously.
    // NOTE: r.str() is deliberately OUTSIDE the try — a truncated/mismatched
    // envelope is a wire version-mismatch and must fail LOUDLY (it traps the
    // wasm import) rather than degrade to a garbage body like the old
    // shape-sniffing decoder did (backlog line 44).
    const envelope = r.str();
    try {
      const e = JSON.parse(envelope) as Record<string, unknown>;
      const mode =
        typeof e === "object" && e !== null && typeof e.mode === "string"
          ? e.mode
          : "raw";
      switch (mode) {
        case "raw":
          body = typeof e.raw === "string" ? e.raw : String(e.raw ?? "");
          break;
        case "json":
          body = JSON.stringify(e.json);
          break;
        case "url_encoded":
          body = new URLSearchParams((e.fields as Record<string, string>) ?? {}).toString();
          break;
        case "form_data":
          formFields = (e.fields as Record<string, string>) ?? {};
          // v1: URL-encoded default; transports needing multipart build it
          // from bodyMode + formFields.
          body = new URLSearchParams(formFields).toString();
          break;
        case "graphql":
          body = JSON.stringify({
            query: e.query,
            variables: e.variables ?? {},
          });
          break;
        case "binary":
          if (Array.isArray(e.data)) bodyBytes = new Uint8Array(e.data as number[]);
          body = null;
          break;
        default:
          body = envelope;
      }
      bodyMode = mode as HttpRequest["bodyMode"];
    } catch {
      // Not a JSON envelope (shouldn't happen) — keep the raw text.
      body = envelope;
    }
  }
  return { url, method, headers, queryParams, body, bodyMode, bodyDecoded, bodyBytes, formFields };
}

// ── Response (host → wasm, across the tropel_host_http bridge) ───────────

function writeOptionStr(w: Writer, v: string | null | undefined): void {
  if (v == null) w.optionNone();
  else {
    w.optionSome();
    w.str(v);
  }
}

function writeOptionBool(w: Writer, v: boolean | null | undefined): void {
  if (v == null) w.optionNone();
  else {
    w.optionSome();
    w.u8(v ? 1 : 0);
  }
}

function writeDurationMs(w: Writer, ms: number): void {
  const totalNs = Math.max(0, Math.round(ms * 1_000_000));
  const secs = Math.floor(totalNs / 1_000_000_000);
  const nanos = totalNs % 1_000_000_000;
  w.varint(secs);
  w.varint(nanos);
}

function writeTimings(w: Writer, t: Timings): void {
  writeDurationMs(w, t.blockedMs);
  writeDurationMs(w, t.dnsMs);
  writeDurationMs(w, t.connectingMs);
  writeDurationMs(w, t.tlsHandshakingMs);
  writeDurationMs(w, t.sendingMs);
  writeDurationMs(w, t.waitingMs);
  writeDurationMs(w, t.receivingMs);
  // Backlog line 43 (P0): `total` is the 8th Timings field on the Rust side
  // (types.rs:481-521). Postcard is positional — omitting it made the wasm
  // decode fail with DeserializeUnexpectedEnd on EVERY response that carried
  // timings (the happy path), and the hand-written TS encoder was the only
  // host that got it wrong.
  writeDurationMs(w, t.totalMs);
}

// Response wire (types.rs): url, status_code(u16), status_text, headers,
// body(Vec<u8>), [text_cache/json_cache: serde(skip) — absent from wire],
// response_time(Duration), timings(Option<Timings>), cookies(Vec<Cookie>),
// size(u64), request_body_size(u64), redirects(Vec<Response>).
export function encodeResponse(resp: HttpResponse): Uint8Array {
  const w = new Writer();
  w.str(resp.url);
  w.varint(resp.statusCode);
  w.str(resp.statusText ?? "");
  w.strMap(resp.headers);
  w.bytes(resp.body);
  writeDurationMs(w, resp.responseTimeMs);
  if (resp.timings) {
    w.optionSome();
    writeTimings(w, resp.timings);
  } else {
    w.optionNone();
  }
  w.varint(resp.cookies?.length ?? 0);
  for (const c of resp.cookies ?? []) {
    // Rust Cookie.name / Cookie.value are REQUIRED String fields (types.rs) —
    // writing them through writeOptionStr would emit a 0x01 tag where the
    // wasm expects a bare string and corrupt every cookie (review catch). The
    // other six fields are Option on the Rust side.
    w.str(c.name);
    w.str(c.value);
    writeOptionStr(w, c.domain);
    writeOptionStr(w, c.path);
    writeOptionBool(w, c.httpOnly);
    writeOptionBool(w, c.secure);
    writeOptionStr(w, c.sameSite);
    writeOptionStr(w, c.expires);
  }
  w.varint(resp.size ?? resp.body.length);
  w.varint(resp.requestBodySize ?? 0);
  w.varint(0); // redirects — empty for the browser bridge (v1)
  return w.toUint8Array();
}
