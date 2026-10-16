use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContactConfig {
    pub last_name: String,
    pub first_name: String,
    pub email: String,
    pub phone: String,
    pub national_id_suffix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub proxy_urls: Vec<String>,
    pub part: String,
    pub store_id: String,
    #[serde(default)]
    pub parts: Vec<String>,
    #[serde(default)]
    pub stores: Vec<String>,
    pub city: String,
    pub province: String,
    pub district: String,
    #[serde(default = "default_quantity")]
    pub quantity: u32,
    pub contact: ContactConfig,
    #[serde(default)]
    pub poll_delay_ms: Option<u64>,
    #[serde(default)]
    pub login_cookies: HashMap<String, String>,
    #[serde(default)]
    pub wechat_cookie: String,
    #[serde(default)]
    pub cookie_saved_at: Option<u64>,
    #[serde(default)]
    pub orders_placed: u32,
    #[serde(default)]
    pub last_order_number: Option<String>,
    #[serde(default)]
    pub last_payment_link: Option<String>,
    #[serde(default)]
    pub enabled: bool,
}

impl AccountConfig {
    pub fn normalize(&mut self) {
        self.parts.retain(|part| !part.trim().is_empty());
        self.stores.retain(|store| !store.trim().is_empty());
        if self.parts.is_empty() {
            self.parts = vec![self.part.trim().to_string()];
        }
        if self.stores.is_empty() {
            self.stores = vec![self.store_id.trim().to_string()];
        }
        self.part = self.parts[0].clone();
        self.store_id = self.stores[0].clone();
    }
}

fn default_quantity() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobalSettings {
    #[serde(default = "default_poll_delay_ms")]
    pub poll_delay_ms: u64,
    #[serde(default = "default_start_delay_ms")]
    pub start_delay_ms: u64,
    #[serde(default = "default_jitter_ms")]
    pub jitter_ms: u64,
    #[serde(default)]
    pub stop_after_success: bool,
    pub http_timeout_ms: u64,
    #[serde(default = "default_batch_prepare_concurrency")]
    pub batch_prepare_concurrency: usize,
    #[serde(default)]
    pub monitor_parts: Vec<String>,
    #[serde(default)]
    pub monitor_stores: Vec<String>,
    #[serde(default = "default_auto_refresh_sessions")]
    pub auto_refresh_sessions: bool,
    #[serde(default = "default_order_concurrency")]
    pub order_concurrency: usize,
    #[serde(default = "default_order_concurrency")]
    pub direct_order_concurrency: usize,
    #[serde(default = "default_retry_enabled")]
    pub retry_enabled: bool,
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,
    #[serde(default = "default_retry_max_attempts")]
    pub retry_max_attempts: u32,
    #[serde(default = "default_circuit_breaker_threshold")]
    pub circuit_breaker_threshold: u32,
    #[serde(default = "default_circuit_breaker_cooldown_ms")]
    pub circuit_breaker_cooldown_ms: u64,

    // ===== 静态 IP 代理 =====
    /// 静态 IP 列表（每行一个，支持 http://、socks5:// 前缀）
    #[serde(default)]
    pub static_proxies: Vec<String>,
    /// 是否为每个账号自动分配独立静态 IP
    #[serde(default)]
    pub static_proxy_enabled: bool,

    // ===== 动态代理池 =====
    /// 代理池提取接口地址（含全部参数）
    #[serde(default)]
    pub proxy_pool_api: String,
    /// 是否启用动态代理池
    #[serde(default)]
    pub proxy_pool_enabled: bool,
    /// 池上限模式：account（账号数 × 倍率）或 fixed（固定值）
    #[serde(default = "default_proxy_pool_limit_mode")]
    pub proxy_pool_limit_mode: String,
    /// 每个账号预留的 IP 倍数（limit_mode=account 时生效）
    #[serde(default = "default_proxy_pool_per_account")]
    pub proxy_pool_per_account: usize,
    /// 固定池上限（limit_mode=fixed 时生效）
    #[serde(default = "default_proxy_pool_fixed_limit")]
    pub proxy_pool_fixed_limit: usize,
    /// 提取时附加的 num 参数模式：auto（按需计算）或 fixed（用 num 值）
    #[serde(default = "default_proxy_num_mode")]
    pub proxy_num_mode: String,
    /// num 固定值（proxy_num_mode=fixed 时生效）
    #[serde(default = "default_proxy_num_fixed")]
    pub proxy_num_fixed: usize,
    /// 测活间隔（毫秒）
    #[serde(default = "default_proxy_check_interval_ms")]
    pub proxy_check_interval_ms: u64,
    /// 单个 IP 最大存活时间（毫秒），超时自动丢弃
    #[serde(default = "default_proxy_max_age_ms")]
    pub proxy_max_age_ms: u64,
    /// 距过期多久内需要「复测」。按需测活只探测新入池 / 失败的 / 快过期的 IP；
    /// 默认取 max_age 的 1/3（例如 30s → 10s）。
    #[serde(default)]
    pub proxy_refresh_before_ms: u64,
    /// 池存活数量低于该阈值时自动补充
    #[serde(default = "default_proxy_pool_low_watermark")]
    pub proxy_pool_low_watermark: usize,
    /// 并发提取代理的线程数
    #[serde(default = "default_proxy_fetch_concurrency")]
    pub proxy_fetch_concurrency: usize,
    /// 调用提取接口时使用的代理（为空则直连）
    /// 用途：绕过接口方的 IP 白名单限制
    #[serde(default)]
    pub proxy_pool_fetch_proxy: String,
    /// 测活使用的目标 URL
    #[serde(default = "default_proxy_check_url")]
    pub proxy_check_url: String,
    /// 提取接口返回格式：line（每行一个）/ json（带 data 数组）
    #[serde(default = "default_proxy_pool_format")]
    pub proxy_pool_format: String,

    // ===== 订单上传 =====
    /// 订单成交后推送的外部接口地址（为空表示不推送）
    #[serde(default = "default_order_upload_url")]
    pub order_upload_url: String,
    /// 订单上传总开关
    #[serde(default = "default_order_upload_enabled")]
    pub order_upload_enabled: bool,

    // ===== watch 模式（守着结账态等库存）=====
    /// watch 刷新间隔（毫秒）
    #[serde(default = "default_watch_interval_ms")]
    pub watch_interval_ms: u64,
    /// watch 单账号刷新的最大次数（0 = 不限）
    #[serde(default = "default_watch_max_refresh")]
    pub watch_max_refresh: u64,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            poll_delay_ms: default_poll_delay_ms(),
            start_delay_ms: default_start_delay_ms(),
            jitter_ms: default_jitter_ms(),
            stop_after_success: true,
            http_timeout_ms: 8000,
            batch_prepare_concurrency: 100,
            monitor_parts: Vec::new(),
            monitor_stores: Vec::new(),
            auto_refresh_sessions: true,
            order_concurrency: default_order_concurrency(),
            direct_order_concurrency: default_order_concurrency(),
            retry_enabled: true,
            retry_base_delay_ms: default_retry_base_delay_ms(),
            retry_max_attempts: default_retry_max_attempts(),
            circuit_breaker_threshold: default_circuit_breaker_threshold(),
            circuit_breaker_cooldown_ms: default_circuit_breaker_cooldown_ms(),
            static_proxies: Vec::new(),
            static_proxy_enabled: false,
            proxy_pool_api: String::new(),
            proxy_pool_enabled: false,
            proxy_pool_limit_mode: default_proxy_pool_limit_mode(),
            proxy_pool_per_account: default_proxy_pool_per_account(),
            proxy_pool_fixed_limit: default_proxy_pool_fixed_limit(),
            proxy_num_mode: default_proxy_num_mode(),
            proxy_num_fixed: default_proxy_num_fixed(),
            proxy_check_interval_ms: default_proxy_check_interval_ms(),
            proxy_max_age_ms: default_proxy_max_age_ms(),
            proxy_refresh_before_ms: 0,
            proxy_pool_low_watermark: default_proxy_pool_low_watermark(),
            proxy_fetch_concurrency: default_proxy_fetch_concurrency(),
            proxy_pool_fetch_proxy: String::new(),
            proxy_check_url: default_proxy_check_url(),
            proxy_pool_format: default_proxy_pool_format(),
            order_upload_url: default_order_upload_url(),
            order_upload_enabled: default_order_upload_enabled(),
            watch_interval_ms: default_watch_interval_ms(),
            watch_max_refresh: default_watch_max_refresh(),
        }
    }
}

fn default_poll_delay_ms() -> u64 {
    1000
}
fn default_start_delay_ms() -> u64 {
    0
}
fn default_jitter_ms() -> u64 {
    150
}

fn default_batch_prepare_concurrency() -> usize {
    100
}

fn default_auto_refresh_sessions() -> bool {
    true
}

fn default_order_concurrency() -> usize {
    100
}

fn default_retry_enabled() -> bool {
    true
}

fn default_retry_base_delay_ms() -> u64 {
    2000
}

fn default_retry_max_attempts() -> u32 {
    3
}

fn default_circuit_breaker_threshold() -> u32 {
    20
}

fn default_proxy_pool_limit_mode() -> String {
    "account".to_string()
}
fn default_proxy_pool_fixed_limit() -> usize {
    20
}
fn default_proxy_num_mode() -> String {
    "auto".to_string()
}
fn default_proxy_num_fixed() -> usize {
    50
}
fn default_proxy_pool_per_account() -> usize {
    3
}
fn default_proxy_check_interval_ms() -> u64 {
    10_000
}
fn default_proxy_max_age_ms() -> u64 {
    30_000
}
fn default_proxy_pool_low_watermark() -> usize {
    5
}
fn default_proxy_fetch_concurrency() -> usize {
    5
}
fn default_proxy_check_url() -> String {
    "https://www.apple.com.cn".to_string()
}
fn default_proxy_pool_format() -> String {
    "line".to_string()
}
fn default_circuit_breaker_cooldown_ms() -> u64 {
    30000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub settings: GlobalSettings,
    pub accounts: Vec<AccountConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            settings: GlobalSettings::default(),
            accounts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountStatus {
    pub id: String,
    pub phone: String,
    pub username: String,
    pub part: String,
    pub store_id: String,
    pub parts: Vec<String>,
    pub stores: Vec<String>,
    pub cookie_count: usize,
    pub cookie_saved_at: Option<u64>,
    pub proxy: String,
    pub enabled: bool,
    pub running: bool,
    pub status: String,
    pub last_latency_ms: u128,
    pub last_poll: Option<u64>,
    pub orders_placed: u64,
    pub last_error: Option<String>,
    pub last_order_number: Option<String>,
    pub last_payment_link: Option<String>,
    pub wechat_cookie_status: String,
    pub wechat_cookie_age_ms: Option<u64>,
    pub order_attempts: u32,
    pub retry_after_ms: Option<u64>,
    pub proxy_health: String,
    pub priority_score: u32,
    pub wechat_session_count: usize,
    pub wechat_session_fresh_count: usize,
    pub wechat_session_expired_count: usize,
    pub wechat_session_min_remaining_ms: Option<u64>,
    pub wechat_session_part: Option<String>,
    pub wechat_session_store: Option<String>,
    /// 预热会话是否可直接下单（已进入结账页，下单只需 place）
    pub wechat_session_checkout_ready: bool,
    /// 预热会话是否只是降级预热（当前无货，仅清空购物袋 + 缓存 OpenID）
    pub wechat_session_fallback_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderRecord {
    pub account_id: String,
    pub username: String,
    pub order_number: String,
    pub part: String,
    pub store_id: String,
    pub pickup_date: String,
    pub pickup_time: String,
    pub payment_link: String,
    pub total_ms: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchProgress {
    /// 批次类型："prepare" / "warmup" / "idle"。两个批次互相覆盖会造成进度错乱。
    pub kind: String,
    pub running: bool,
    pub total: usize,
    pub completed: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub started_at_ms: Option<u64>,
    pub updated_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

impl Default for BatchProgress {
    fn default() -> Self {
        Self {
            running: false,
            kind: "idle".to_string(),
            total: 0,
            completed: 0,
            succeeded: 0,
            failed: 0,
            started_at_ms: None,
            updated_at_ms: None,
            finished_at_ms: None,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AppStatePayload {
    pub settings: GlobalSettings,
    pub accounts: Vec<AccountStatus>,
    pub batch_progress: BatchProgress,
    pub recommended_batch_concurrency: usize,
    pub cpu_count: usize,
    pub circuit_open: bool,
    pub circuit_open_until_ms: Option<u64>,
    pub circuit_failure_count: u32,
    pub order_attempts: u32,
    pub order_successes: u32,
    pub order_failures: u32,
    pub active_orders: usize,
    pub monitor_last_latency_ms: u128,
    pub monitor_available_combos: usize,
    pub monitor_dispatched_count: usize,
    /// 最近订单（已裁剪到上限，避免 WebSocket 每次推送全量历史导致 payload 膨胀）
    pub orders: Vec<OrderRecord>,
    /// 订单总数（含已裁剪掉的历史）
    pub orders_total: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProxyHealth {
    pub proxy_url: String,
    pub successes: u32,
    pub failures: u32,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_used_ms: Option<u64>,
    pub healthy: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub ts_ms: u64,
    pub account_id: String,
    pub level: String,
    pub message: String,
}

pub type CookieMap = HashMap<String, String>;

fn default_order_upload_url() -> String {
    "http://44.197.230.30:3434/api/order/upload".to_string()
}

fn default_order_upload_enabled() -> bool {
    true
}

fn default_watch_interval_ms() -> u64 {
    800
}

fn default_watch_max_refresh() -> u64 {
    0
}
