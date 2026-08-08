// ============================================================
// Clouisle Sandbox TypeScript SDK — API client
// ============================================================

import axios, { AxiosInstance, AxiosRequestConfig } from "axios";
import {
  ApiErrorResponse,
  ExecRequest,
  ExecResult,
  HealthResponse,
  ListFilesResponse,
  Sandbox,
  SandboxError,
  SandboxListResponse,
  SandboxSpec,
} from "./types";

/**
 * Clouisle Sandbox API client.
 *
 * @example
 * ```ts
 * import { Client, SandboxSpec } from "@clouisle/sdk";
 *
 * const client = new Client("http://localhost:8080", "my-api-key");
 * const sandbox = await client.createSandbox({
 *   image: { reference: "alpine:latest" },
 *   resources: { vcpu: 1, memory_mb: 256, disk_mb: 512 },
 * });
 * ```
 */
export class Client {
  private http: AxiosInstance;
  private baseUrl: string;
  private apiKey: string;

  /**
   * @param baseUrl - API base URL, e.g. "http://localhost:8080"
   * @param apiKey - API key for authentication (empty = no auth)
   */
  constructor(baseUrl: string, apiKey: string = "") {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.apiKey = apiKey;
    this.http = axios.create({
      baseURL: this.baseUrl,
      timeout: 30000,
      headers: this.buildHeaders(),
    });
  }

  // ──────────────────────────────────────────
  //  Sandbox Lifecycle
  // ──────────────────────────────────────────

  /**
   * Create a sandbox.
   * @param spec - Sandbox creation spec
   * @returns The created sandbox
   * @throws {SandboxError} On API error
   */
  async createSandbox(spec: SandboxSpec): Promise<Sandbox> {
    return this.post("/api/v1/sandboxes", spec);
  }

  /**
   * Get a sandbox by ID.
   * @param sandboxId - Sandbox UUID
   * @returns The sandbox
   * @throws {SandboxError} On 404 or other errors
   */
  async getSandbox(sandboxId: string): Promise<Sandbox> {
    return this.get(`/api/v1/sandboxes/${sandboxId}`);
  }

  /**
   * List sandboxes with optional filters.
   * @param params - Optional filters: status, limit, offset
   * @returns Paginated list of sandboxes
   */
  async listSandboxes(params?: {
    status?: string;
    limit?: number;
    offset?: number;
  }): Promise<SandboxListResponse> {
    return this.get("/api/v1/sandboxes", { params });
  }

  /**
   * Delete a sandbox.
   * @param sandboxId - Sandbox UUID
   * @throws {SandboxError} On 404 or other errors
   */
  async deleteSandbox(sandboxId: string): Promise<void> {
    await this.delete(`/api/v1/sandboxes/${sandboxId}`);
  }

  // ──────────────────────────────────────────
  //  Command Execution
  // ──────────────────────────────────────────

  /**
   * Execute a command synchronously in a sandbox.
   * @param sandboxId - Sandbox UUID
   * @param req - Execution request
   * @returns Execution result
   */
  async exec(sandboxId: string, req: ExecRequest): Promise<ExecResult> {
    return this.post(`/api/v1/sandboxes/${sandboxId}/exec`, req);
  }

  /**
   * Convenience: execute a command with just argv.
   * @param sandboxId - Sandbox UUID
   * @param argv - Command and args, e.g. ["echo", "hello"]
   * @param timeoutMs - Timeout in ms (default 30000)
   * @returns Execution result
   */
  async execCmd(
    sandboxId: string,
    argv: string[],
    timeoutMs: number = 30000,
  ): Promise<ExecResult> {
    return this.exec(sandboxId, { argv, timeout_ms: timeoutMs });
  }

  // ──────────────────────────────────────────
  //  File Transfer
  // ──────────────────────────────────────────

  /**
   * Upload a file to a sandbox.
   * @param sandboxId - Sandbox UUID
   * @param path - Target path in the sandbox, e.g. "/work/file.txt"
   * @param data - File content as bytes
   * @returns Response JSON
   */
  async uploadFile(
    sandboxId: string,
    path: string,
    data: Buffer | Uint8Array,
  ): Promise<Record<string, unknown>> {
    return this.postRaw(
      `/api/v1/sandboxes/${sandboxId}/files/upload`,
      path,
      data,
    );
  }

  /**
   * Download a file from a sandbox.
   * @param sandboxId - Sandbox UUID
   * @param path - File path in the sandbox
   * @returns File content as ArrayBuffer
   */
  async downloadFile(
    sandboxId: string,
    path: string,
  ): Promise<ArrayBuffer> {
    const url = `/api/v1/sandboxes/${sandboxId}/files/download`;
    const resp = await this.http.get(url, {
      params: { path },
      responseType: "arraybuffer",
    });
    return resp.data;
  }

  /**
   * List files in a directory inside a sandbox.
   * @param sandboxId - Sandbox UUID
   * @param path - Directory path, e.g. "/work"
   * @returns Directory listing
   */
  async listFiles(
    sandboxId: string,
    path: string,
  ): Promise<ListFilesResponse> {
    return this.get(`/api/v1/sandboxes/${sandboxId}/files/ls`, {
      params: { path },
    });
  }

  // ──────────────────────────────────────────
  //  Observability
  // ──────────────────────────────────────────

  /**
   * Health check endpoint.
   * @returns Health status
   */
  async health(): Promise<HealthResponse> {
    return this.get("/health");
  }

  /**
   * Liveness probe.
   * @returns { status: "alive" }
   */
  async liveness(): Promise<Record<string, string>> {
    return this.get("/health/live");
  }

  /**
   * Readiness probe.
   * @returns { status: "ready" | "not_ready" }
   */
  async readiness(): Promise<Record<string, string>> {
    return this.get("/health/ready");
  }

  /**
   * Prometheus metrics (raw text).
   * @returns Metrics text
   */
  async metrics(): Promise<string> {
    const resp = await this.http.get("/metrics");
    return String(resp.data);
  }

  // ──────────────────────────────────────────
  //  Internal HTTP
  // ──────────────────────────────────────────

  private buildHeaders(): Record<string, string> {
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
    };
    if (this.apiKey) {
      headers["Authorization"] = `Bearer ${this.apiKey}`;
    }
    return headers;
  }

  private async get<T>(url: string, config?: AxiosRequestConfig): Promise<T> {
    try {
      const resp = await this.http.get(url, config);
      return resp.data as T;
    } catch (err: unknown) {
      throw this.wrapError(err);
    }
  }

  private async post<T>(url: string, body: unknown): Promise<T> {
    try {
      const resp = await this.http.post(url, body);
      return resp.data as T;
    } catch (err: unknown) {
      throw this.wrapError(err);
    }
  }

  private async postRaw(
    url: string,
    path: string,
    data: Buffer | Uint8Array,
  ): Promise<Record<string, unknown>> {
    try {
      const resp = await this.http.post(
        `${url}?path=${encodeURIComponent(path)}`,
        data,
        { headers: { "Content-Type": "application/octet-stream" } },
      );
      return resp.data as Record<string, unknown>;
    } catch (err: unknown) {
      throw this.wrapError(err);
    }
  }

  private async delete(url: string): Promise<void> {
    try {
      await this.http.delete(url);
    } catch (err: unknown) {
      throw this.wrapError(err);
    }
  }

  private wrapError(err: unknown): SandboxError {
    if (axios.isAxiosError(err) && err.response) {
      const status = err.response.status;
      const data = err.response.data as ApiErrorResponse | string;
      const body = typeof data === "object" ? data : { error: { code: "HTTP", message: String(data) } };
      const error = body.error ?? { code: "HTTP", message: String(data) };
      return new SandboxError(status, error.code, error.message, error.details);
    }
    if (err instanceof SandboxError) return err;
    return new SandboxError(0, "UNKNOWN", String(err));
  }
}