// ---- runtime (identical in every generated client) -------------------------------------------

/** An exact decimal as plain digits, e.g. "2.5000". Never a JavaScript number. */
export type Decimal = string;
/** A calendar date, "2026-10-01". */
export type IsoDate = string;
/** A UTC timestamp, "2026-10-01T12:00:00Z". */
export type IsoDateTime = string;

/** An error answer from the server (RFC 9457 problem), with its stable machine code. */
export class CEngineError extends Error {
  readonly status: number;
  readonly code: string;
  readonly detail: string;
  readonly requestId: string;
  readonly retryable: boolean;
  readonly errors: Array<{ path?: string; code?: string; detail?: string }>;

  constructor(status: number, problem: Record<string, any>) {
    super(`${status} ${problem.code ?? "unknown"}: ${problem.detail ?? ""}`);
    this.name = "CEngineError";
    this.status = status;
    this.code = problem.code ?? "unknown";
    this.detail = problem.detail ?? "";
    this.requestId = problem.request_id ?? "";
    this.retryable = Boolean(problem.retryable);
    this.errors = problem.errors ?? [];
  }
}

type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

/** A filter expression. Build with `and`, `or`, `not` and the field handles. */
export class Filter {
  readonly node: Record<string, Json>;
  constructor(node: Record<string, Json>) {
    this.node = node;
  }
  toJSON(): string {
    return JSON.stringify(this.node);
  }
}

export const and = (...fs: Filter[]): Filter => new Filter({ and: fs.map((f) => f.node) });
export const or = (...fs: Filter[]): Filter => new Filter({ or: fs.map((f) => f.node) });
export const not = (f: Filter): Filter => new Filter({ not: f.node });

/** A sort key: `Item.f.name.desc()`. */
export class Sort {
  readonly field: string;
  readonly descending: boolean;
  constructor(field: string, descending: boolean = false) {
    this.field = field;
    this.descending = descending;
  }
  toString(): string {
    return (this.descending ? "-" : "") + this.field;
  }
}

function value(v: unknown): Json {
  if (typeof v === "number" && !Number.isInteger(v)) {
    throw new TypeError("use a Decimal string, not a fractional number, for decimal fields");
  }
  return v as Json;
}

/** A field of an entity, as a filter and sort builder: `Item.f.unit_cost.gte("2")`. */
export class Field<T> {
  readonly name: string;
  constructor(name: string) {
    this.name = name;
  }
  private op(op: string, v?: unknown): Filter {
    const node: Record<string, Json> = { field: this.name, op };
    if (v !== undefined) node.value = Array.isArray(v) ? v.map(value) : value(v);
    return new Filter(node);
  }
  eq = (v: T): Filter => this.op("eq", v);
  ne = (v: T): Filter => this.op("ne", v);
  gt = (v: T): Filter => this.op("gt", v);
  gte = (v: T): Filter => this.op("gte", v);
  lt = (v: T): Filter => this.op("lt", v);
  lte = (v: T): Filter => this.op("lte", v);
  in = (vs: T[]): Filter => this.op("in", vs);
  notIn = (vs: T[]): Filter => this.op("nin", vs);
  like = (pattern: string): Filter => this.op("like", pattern);
  ilike = (pattern: string): Filter => this.op("ilike", pattern);
  isNull = (want: boolean = true): Filter => this.op("null", want);
  asc = (): Sort => new Sort(this.name, false);
  desc = (): Sort => new Sort(this.name, true);
}

/** One page of a list. `next` is the cursor for the following page, or null at the end. */
export interface Page<T> {
  items: T[];
  next: string | null;
  requestId: string;
}

export interface ListOptions {
  where?: Filter;
  sort?: Sort | Sort[];
  limit?: number;
  after?: string;
  /** Text search over the entity's searchable fields. */
  q?: string;
  select?: string[];
}

export interface ClientOptions {
  baseUrl: string;
  installation: string;
  token: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
}

export class Http {
  private readonly opts: ClientOptions;
  private readonly base: string;
  private readonly doFetch: typeof fetch;
  constructor(opts: ClientOptions) {
    this.opts = opts;
    this.base = opts.baseUrl.replace(/\/+$/, "");
    this.doFetch = opts.fetch ?? ((...a) => fetch(...a));
  }

  async call(
    method: string,
    path: string,
    init: { params?: Record<string, string>; body?: unknown; headers?: Record<string, string> } = {},
  ): Promise<any> {
    let url = this.base + path;
    if (init.params && Object.keys(init.params).length > 0) {
      url += "?" + new URLSearchParams(init.params).toString();
    }
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.opts.token}`,
      Accept: "application/json",
      ...init.headers,
    };
    if (init.body !== undefined) headers["Content-Type"] = "application/json";
    const res = await this.doFetch(url, {
      method,
      headers,
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
    });
    const text = await res.text();
    let parsed: any = null;
    try {
      parsed = text ? JSON.parse(text) : null;
    } catch {
      parsed = null;
    }
    if (!res.ok) {
      throw new CEngineError(
        res.status,
        parsed ?? { code: "bad_response", detail: text.slice(0, 200) },
      );
    }
    return parsed;
  }
}

function uuid(): string {
  return globalThis.crypto.randomUUID();
}

/** The endpoints of one entity. `R` is the record, `I` what a create takes. */
export class Resource<R, I> {
  private readonly http: Http;
  private readonly path: string;
  constructor(http: Http, installation: string, entity: string) {
    this.http = http;
    this.path = `/v1/${installation}/${entity}`;
  }

  async list(o: ListOptions = {}): Promise<Page<R>> {
    const params: Record<string, string> = {};
    if (o.where) params.filter = o.where.toJSON();
    if (o.sort) params.sort = (Array.isArray(o.sort) ? o.sort : [o.sort]).map(String).join(",");
    if (o.limit !== undefined) params.limit = String(o.limit);
    if (o.after) params.after = o.after;
    if (o.q !== undefined) params.q = o.q;
    if (o.select) params.select = o.select.join(",");
    const body = await this.http.call("GET", this.path, { params });
    return {
      items: body.data as R[],
      next: body.meta?.next ?? null,
      requestId: body.meta?.request_id ?? "",
    };
  }

  /** Every matching record, fetching page after page. */
  async *iter(o: Omit<ListOptions, "after"> = {}): AsyncGenerator<R> {
    let after: string | undefined;
    for (;;) {
      const page = await this.list({ ...o, after });
      yield* page.items;
      if (!page.next) return;
      after = page.next;
    }
  }

  async get(id: string): Promise<R> {
    return (await this.http.call("GET", `${this.path}/${id}`)).data as R;
  }

  /** Creates a record. The idempotency key defaults to a fresh one; pass your own to make a
   * retry of this exact call safe. */
  async create(record: I, key?: string): Promise<R> {
    const out = await this.http.call("POST", this.path, {
      body: record,
      headers: { "Idempotency-Key": key ?? uuid() },
    });
    return out.data as R;
  }

  /** Patches a record at the version you last saw; a stale version is a 409 error. */
  async update(record: { id: string; version: number } | string, changes: Partial<I>, version?: number): Promise<R> {
    const [id, ver] = this.target(record, version);
    const out = await this.http.call("PATCH", `${this.path}/${id}`, {
      body: changes,
      headers: { "If-Match": `"${ver}"` },
    });
    return out.data as R;
  }

  async delete(record: { id: string; version: number } | string, version?: number): Promise<void> {
    const [id, ver] = this.target(record, version);
    await this.http.call("DELETE", `${this.path}/${id}`, { headers: { "If-Match": `"${ver}"` } });
  }

  private target(record: { id: string; version: number } | string, version?: number): [string, number] {
    if (typeof record === "string") {
      if (version === undefined) throw new Error("pass the record, or a version");
      return [record, version];
    }
    return [record.id, version ?? record.version];
  }
}
// ---- end of runtime ---------------------------------------------------------------------------
