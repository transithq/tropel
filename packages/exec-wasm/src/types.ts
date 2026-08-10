// Wire type shapes for the tropel-web C ABI (mirror of the Rust types in
// crates/tropel-web/src/wire.rs + crates/tropel-sdk/src/types.rs).

/** Host → wasm run request (crates/tropel-web/src/wire.rs RunRequest). */
export interface RunRequest {
  /** The scenario to walk, JSON-encoded. */
  scenarioJson: string;
  /** VU id surfaced to exec.vu.idInInstance(). */
  vuId: number;
  /** Scenario name surfaced to exec.scenario.name. */
  scenarioName: string;
  /** Iterations to run. */
  iterations: number;
  /** CLI/env variables merged into the variable scope. */
  envVars: Record<string, string>;
  /** Expected status spec strings: "200", "200-399", "2xx". */
  expectedStatuses: string[];
}

/** Wasm → host run outcome (wire.rs RunOutcome). */
export interface RunOutcome {
  iterations: IterationResult[];
  /** Fatal error string when the run could not complete. */
  error: string | null;
}

export interface IterationResult {
  samples: Sample[];
  iterationIndex: number;
  scriptFailures: number;
}

export interface Sample {
  metric: string;
  value: number;
  tags: Record<string, string>;
  /** SystemTime: seconds + sub-second nanos since the Unix epoch. */
  timestampSecs: number;
  timestampNanos: number;
  sampleType: "Point" | "Counter" | "Trend" | "Rate" | string;
}

/** Timings of a response, in milliseconds (types.rs Timings, μs → ms). */
export interface Timings {
  blockedMs: number;
  dnsMs: number;
  connectingMs: number;
  tlsHandshakingMs: number;
  sendingMs: number;
  waitingMs: number;
  receivingMs: number;
}

/**
 * A request the JS host must answer, decoded from the wasm's linear memory
 * across the `env.tropel_host_http` bridge (types.rs Request — v1 decodes
 * the fields a fetch needs; see postcard.ts decodeHttpRequest).
 */
export interface HttpRequest {
  url: string;
  method: string;
  headers: Record<string, string>;
  queryParams: Record<string, string>;
  /** Raw string body (Body::Raw) when present and decodable; else null. */
  body: string | null;
  /** Whether the body was present on the wire (even if undecodable). */
  bodyDecoded: boolean;
}

/** Response the JS host returns across the bridge (types.rs Response). */
export interface HttpResponse {
  url: string;
  statusCode: number;
  statusText?: string;
  headers: Record<string, string>;
  body: Uint8Array;
  responseTimeMs: number;
  timings?: Timings;
  cookies?: ResponseCookie[];
  size?: number;
  requestBodySize?: number;
}

/** types.rs Cookie — every field is Option on the wire. */
export interface ResponseCookie {
  name: string;
  value: string;
  domain?: string | null;
  path?: string | null;
  httpOnly?: boolean | null;
  secure?: boolean | null;
  sameSite?: string | null;
  expires?: string | null;
}
