import { http } from "@/utils/http";

export type PhoneModel = {
  family: string;
  capacity: string;
  color: string;
  part: string;
};

export type ContactConfig = {
  last_name: string;
  first_name: string;
  email: string;
  phone: string;
  national_id_suffix: string;
};

export type AccountConfig = {
  id: string;
  username: string;
  password?: string;
  proxy_url?: string;
  proxy_urls?: string[];
  part: string;
  store_id: string;
  parts: string[];
  stores: string[];
  city: string;
  province: string;
  district: string;
  quantity?: number;
  contact: ContactConfig;
  poll_delay_ms?: number | null;
  login_cookies?: Record<string, string>;
  wechat_cookie?: string;
  enabled?: boolean;
};

export type AccountStatus = {
  phone?: string;
  id: string;
  username: string;
  part: string;
  store_id: string;
  parts: string[];
  stores: string[];
  cookie_count: number;
  proxy: string;
  enabled: boolean;
  running: boolean;
  status: string;
  last_latency_ms: number;
  last_poll?: number | null;
  orders_placed: number;
  last_error?: string | null;
  last_order_number?: string | null;
  last_payment_link?: string | null;
  wechat_cookie_status: string;
  wechat_cookie_age_ms?: number | null;
  order_attempts: number;
  retry_after_ms?: number | null;
  proxy_health: string;
  priority_score: number;
  wechat_session_count: number;
  wechat_session_fresh_count: number;
  wechat_session_expired_count: number;
  wechat_session_min_remaining_ms?: number | null;
  wechat_session_part?: string | null;
  wechat_session_store?: string | null;
  /** 预热会话是否已进结账页，下单可走 place 快速通道 */
  wechat_session_checkout_ready?: boolean;
  /** 预热会话是否只是降级预热（当前无货，仅清空购物袋 + 缓存 OpenID） */
  wechat_session_fallback_only?: boolean;
};

export type OrderRecord = {
  account_id: string;
  username: string;
  order_number: string;
  part: string;
  store_id: string;
  pickup_date: string;
  pickup_time: string;
  payment_link: string;
  total_ms: number;
  created_at_ms: number;
};

export type BatchProgress = {
  kind?: "prepare" | "warmup" | "idle" | string;
  running: boolean;
  total: number;
  completed: number;
  succeeded: number;
  failed: number;
  last_error?: string | null;
};

export type GlobalSettings = {
  poll_delay_ms: number;
  start_delay_ms: number;
  jitter_ms: number;
  stop_after_success: boolean;
  http_timeout_ms: number;
  batch_prepare_concurrency: number;
  monitor_parts: string[];
  monitor_stores: string[];
  auto_refresh_sessions: boolean;
  order_concurrency: number;
  direct_order_concurrency: number;
  retry_enabled: boolean;
  retry_base_delay_ms: number;
  retry_max_attempts: number;
  circuit_breaker_threshold: number;
  circuit_breaker_cooldown_ms: number;
  // 静态 IP 代理
  static_proxies: string[];
  static_proxy_enabled: boolean;
  // 动态代理池
  proxy_pool_api: string;
  proxy_pool_enabled: boolean;
  // 池上限模式：account（账号数×倍率）/ fixed（固定值）
  proxy_pool_limit_mode: string;
  proxy_pool_per_account: number;
  proxy_pool_fixed_limit: number;
  // num 参数模式：auto（按需）/ fixed（固定值）
  proxy_num_mode: string;
  proxy_num_fixed: number;
  proxy_check_interval_ms: number;
  proxy_max_age_ms: number;
  proxy_pool_low_watermark: number;
  proxy_fetch_concurrency: number;
  proxy_pool_fetch_proxy: string;
  proxy_check_url: string;
  proxy_pool_format: string;
};

export type ProxyPoolStats = {
  enabled: boolean;
  total: number;
  alive: number;
  max_size: number;
  fetched_total: number;
  discarded_total: number;
  check_failures: number;
  last_fetch_error?: string | null;
};

export type ProxyTestDetail = {
  kind: string;
  name: string;
  status: number;
  elapsed_ms: number;
  note: string;
  ok: boolean;
};

export type ProxyTestResult = {
  proxy: string;
  normalized?: string;
  exit_ip?: string;
  ok: boolean;
  max_ms?: number;
  error?: string;
  details: ProxyTestDetail[];
};

export type ProxyTestResponse = {
  ok: boolean;
  total?: number;
  available?: number;
  unavailable?: number;
  ok_proxies?: string[];
  results?: ProxyTestResult[];
  error?: string;
};

export type AppStatePayload = {
  settings: GlobalSettings;
  accounts: AccountStatus[];
  batch_progress: BatchProgress;
  recommended_batch_concurrency: number;
  cpu_count: number;
  circuit_open: boolean;
  circuit_open_until_ms?: number | null;
  circuit_failure_count: number;
  order_attempts: number;
  order_successes: number;
  order_failures: number;
  active_orders: number;
  monitor_last_latency_ms: number;
  monitor_available_combos: number;
  monitor_dispatched_count: number;
  orders: OrderRecord[];
  orders_total?: number;
};

export type StoreInfo = {
  store_id: string;
  name: string;
  city: string;
  province: string;
};

export type StockResult = {
  store_id?: string;
  name?: string;
  city?: string;
  province?: string;
  status: string;
  quote?: string;
};

export type WechatStep = {
  name?: string;
  status?: string;
  elapsed_ms?: number;
};

export const api = {
  state: () => http.get<null, AppStatePayload>("/api/state"),
  // 版本信息：用于界面显示当前构建版本，便于确认是否加载了最新前端
  version: () =>
    http.get<null, { version: string; build: string; git: string }>(
      "/api/version"
    ),
  logs: () => http.get<null, ApiLog[]>("/api/logs"),
  clearLogs: () =>
    http.post<null, { ok: boolean; removed_files: number }>("/api/logs/clear", {
      data: {}
    }),
  models: () => http.get<null, PhoneModel[]>("/api/models"),
  stores: () => http.get<null, StoreInfo[]>("/static/stores.json"),
  proxyTest: (proxies: string[], concurrency = 8, timeout_ms = 10000) =>
    http.post<null, ProxyTestResponse>("/api/proxy/test", {
      data: { proxies, concurrency, timeout_ms }
    }),
  proxyPoolStats: () => http.get<null, ProxyPoolStats>("/api/proxy-pool/stats"),
  proxyPoolClear: () => http.post<null, { ok: boolean; cleared: number }>("/api/proxy-pool/clear", { data: {} }),
  proxyPoolFetch: () => http.post<null, { ok: boolean; fetched?: number; error?: string }>("/api/proxy-pool/fetch", { data: {} }),
  saveSettings: (settings: GlobalSettings) =>
    http.post<null, unknown>("/api/settings", { data: settings }),
  start: () => http.post<null, unknown>("/api/start", { data: {} }),
  stop: () => http.post<null, unknown>("/api/stop", { data: {} }),
  clearCompleted: () =>
    http.post<null, { ok: boolean; cleared: number }>(
      "/api/central/clear-completed",
      { data: {} }
    ),
  getAccount: (id: string) =>
    http.get<null, AccountConfig>(`/api/accounts/${encodeURIComponent(id)}`),
  saveAccount: (account: AccountConfig) =>
    http.post<null, unknown>("/api/accounts", { data: account }),
  updateAccount: (id: string, account: AccountConfig) =>
    http.post<null, unknown>(`/api/accounts/${encodeURIComponent(id)}`, {
      data: account
    }),
  startAccount: (id: string) =>
    http.post<null, unknown>(`/api/accounts/${encodeURIComponent(id)}/start`, {
      data: {}
    }),
  stopAccount: (id: string) =>
    http.post<null, unknown>(`/api/accounts/${encodeURIComponent(id)}/stop`, {
      data: {}
    }),
  manualOrder: (id: string, data: { part: string; store_id: string }) =>
    http.post<null, { ok: boolean; payment_link?: string; error?: string }>(
      `/api/accounts/${encodeURIComponent(id)}/order`,
      { data }
    ),
  wechatOrder: (
    id: string,
    data: { part: string; store_id: string; place: boolean; cookie: string }
  ) =>
    http.post<
      null,
      {
        ok: boolean;
        steps?: WechatStep[];
        summary?: Record<string, any>;
        error?: string;
      }
    >(`/api/accounts/${encodeURIComponent(id)}/wechat-order`, { data }),
  wechatPrepare: (
    id: string,
    data: { part: string; store_id: string; place: boolean; cookie: string }
  ) =>
    http.post<null, { ok: boolean; steps?: WechatStep[]; error?: string }>(
      `/api/accounts/${encodeURIComponent(id)}/wechat-prepare`,
      { data }
    ),
  warmupBatch: (data: { account_ids?: string[]; parts?: string[]; stores?: string[] }) =>
    http.post<
      null,
      {
        ok: boolean;
        accounts?: number;
        combos?: number;
        available_combos?: number;
        error?: string;
      }
    >(
      "/api/wechat/warmup-batch",
      { data }
    ),
  wechatPrepareBatch: (data: { account_ids: string[]; part?: string }) =>
    http.post<
      null,
      { ok: boolean; accounts?: number; sessions?: number; error?: string }
    >("/api/wechat/prepare-batch", { data }),
  // ===== watch 模式：守着结账态等库存（建一次结账态后持续刷新 select_store） =====
  watchStart: (data: { account_ids?: string[] }) =>
    http.post<
      null,
      { ok: boolean; accounts?: number; interval_ms?: number; error?: string }
    >("/api/watch/start", { data }),
  watchStop: () =>
    http.post<null, { ok: boolean; stopped?: boolean }>("/api/watch/stop"),
  watchStatus: () =>
    http.get<null, { ok: boolean; running: boolean }>("/api/watch/status"),

  queryStock: (
    id: string,
    data: {
      part: string;
      stores?: string[];
      province?: string;
      city?: string;
      district?: string;
    }
  ) =>
    http.post<null, StockResult[]>(`/api/stock/${encodeURIComponent(id)}`, {
      data
    }),
  importAccounts: (data: any) =>
    http.post<null, unknown>("/api/accounts/import", { data }),
  importCookies: (data: any) =>
    http.post<
      null,
      { updated: number; created: number; failed: number; results: any[] }
    >("/api/accounts/cookies", { data }),
  deleteAccounts: (ids: string[]) =>
    http.post<null, unknown>("/api/accounts/delete", { data: { ids } }),
  applyAccountBulk: (data: any) =>
    http.post<null, { ok: boolean; updated: number }>(
      "/api/accounts/apply-bulk",
      { data }
    ),
  loginStart: (data: { account: string; password: string }) =>
    http.post<null, any>("/api/login/start", { data }),
  loginVerify: (data: { login_id: string; code: string }) =>
    http.post<null, any>("/api/login/verify", { data })
};

export type ApiLog = {
  ts_ms: number;
  account_id: string;
  username: string;
  level: string;
  message: string;
};
