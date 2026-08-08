// ============================================================
// Clouisle Sandbox TypeScript SDK — types
// ============================================================

/** Image reference for a sandbox. */
export interface ImageRef {
  /** Image reference, e.g. "alpine:latest" */
  reference: string;
  /** Optional digest (sha256:...) */
  digest?: string;
}

/** Resources allocated to a sandbox. */
export interface Resources {
  /** vCPU count (1-4) */
  vcpu: number;
  /** Memory in MiB (>=64) */
  memory_mb: number;
  /** Disk scratch in MiB (>=64) */
  disk_mb: number;
  /** Egress bandwidth cap in Mbps */
  bandwidth_mbps?: number;
  /** Disk IOPS cap */
  iops?: number;
}

/** Network configuration. */
export interface NetworkConfig {
  /** Whether networking is enabled. */
  enabled: boolean;
  /** Egress domain allowlist. Empty = deny all egress. */
  allow_egress: string[];
}

/** Spec for creating a sandbox. */
export interface SandboxSpec {
  /** Image reference */
  image: ImageRef;
  /** Resource allocation */
  resources?: Resources;
  /** Network config */
  network?: NetworkConfig;
  /** Environment variables */
  env?: Record<string, string>;
  /** Sandbox TTL in seconds (force destroy on expiry) */
  ttl_secs?: number;
  /** Start timeout in seconds */
  start_timeout_secs?: number;
  /** Restart policy: "never" | "on_failure" | "always" */
  restart_policy?: string;
}

/** VMM runtime metadata. */
export interface VmmMeta {
  /** VMM backend type: "firecracker" | "test" | ... */
  backend: string;
  /** Process PID */
  pid?: number;
  /** API socket path */
  api_socket?: string;
  /** vsock socket path */
  vsock_socket?: string;
  /** VMM-assigned ID */
  vmm_id?: string;
  /** Extra metadata */
  extra?: Record<string, string>;
}

/** Sandbox status. */
export type SandboxStatus =
  | "pending"
  | "starting"
  | "running"
  | "stopping"
  | "stopped"
  | "error";

/** A sandbox instance. */
export interface Sandbox {
  /** Sandbox UUID */
  id: string;
  /** Creation spec */
  spec: SandboxSpec;
  /** Current status */
  status: SandboxStatus;
  /** ISO8601 creation timestamp */
  created_at: string;
  /** ISO8601 last-update timestamp */
  updated_at: string;
  /** ISO8601 time sandbox became running */
  ready_at?: string;
  /** VMM runtime metadata */
  vmm_meta?: VmmMeta;
  /** Owning node ID (multi-node) */
  node_id?: string;
}

/** Command execution request. */
export interface ExecRequest {
  /** Command and args, e.g. ["echo", "hello"] */
  argv: string[];
  /** Extra environment variables */
  env?: Record<string, string>;
  /** Working directory */
  cwd?: string;
  /** Exec timeout in ms */
  timeout_ms?: number;
  /** SSE streaming output */
  stream?: boolean;
}

/** Result of a command execution. */
export interface ExecResult {
  /** Execution record ID */
  exec_id: string;
  /** Process exit code (-1 = timeout) */
  exit_code: number;
  /** Standard output */
  stdout: string;
  /** Standard error */
  stderr: string;
  /** Execution duration in ms */
  duration_ms: number;
  /** Whether timed out */
  timed_out: boolean;
  /** Whether stdout was truncated */
  stdout_truncated: boolean;
  /** Whether stderr was truncated */
  stderr_truncated: boolean;
}

/** Persisted execution record. */
export interface ExecutionRecord {
  /** Execution record ID */
  id: string;
  /** Owning sandbox ID */
  sandbox_id: string;
  /** Process exit code */
  exit_code: number;
  /** Standard output */
  stdout: string;
  /** Standard error */
  stderr: string;
  /** ISO8601 start timestamp */
  started_at: string;
  /** ISO8601 finish timestamp */
  finished_at: string;
  /** Whether timed out */
  timed_out: boolean;
  /** Whether stdout was truncated */
  stdout_truncated: boolean;
  /** Whether stderr was truncated */
  stderr_truncated: boolean;
  /** Executing node ID */
  node_id?: string;
}

/** Directory entry. */
export interface DirEntry {
  /** Entry name */
  name: string;
  /** Size in bytes */
  size: number;
  /** Unix file mode */
  mode: number;
  /** Modification time (unix seconds) */
  mtime: number;
  /** Whether it's a directory */
  is_dir: boolean;
}

/** Directory listing response. */
export interface ListFilesResponse {
  items: DirEntry[];
}

/** Sandbox list response. */
export interface SandboxListResponse {
  items: Sandbox[];
  total: number;
}

/** Health check response. */
export interface HealthResponse {
  status: string;
  store: string;
  version: string;
}

/** API error payload. */
export interface ApiErrorBody {
  code: string;
  message: string;
  details?: unknown;
}

/** API error response wrapper. */
export interface ApiErrorResponse {
  error: ApiErrorBody;
}

/** SDK error. */
export class SandboxError extends Error {
  /** HTTP status code */
  statusCode: number;
  /** API error code, e.g. "VALIDATION" */
  code: string;
  /** Error details */
  details?: unknown;

  constructor(statusCode: number, code: string, message: string, details?: unknown) {
    super(message);
    this.name = "SandboxError";
    this.statusCode = statusCode;
    this.code = code;
    this.details = details;
  }
}