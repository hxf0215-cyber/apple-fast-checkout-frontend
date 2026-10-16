use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use base64::Engine;
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    ChaCha20Poly1305, Key as CipherKey, Nonce,
};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{process::Command, sync::Mutex, task::JoinHandle};

use crate::proxy_pool::{ProxyPool, ProxyPoolStats};
use crate::wechat::WeChatFlow;
use crate::wechat::run_wechat_flow;
use crate::{
    apple::{AppleClient, APPLE_CDN_IPS},
    models::{
        AccountConfig, AccountStatus, AppConfig, AppStatePayload, BatchProgress, ContactConfig,
        CookieMap, GlobalSettings, LogEntry, OrderRecord, ProxyHealth,
    },
};

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

#[derive(Debug, Deserialize)]
pub struct StockQuery {
    pub part: String,
    #[serde(default)]
    pub stores: Option<Vec<String>>,
    #[serde(default)]
    pub province: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub district: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchAccountImport {
    pub text: String,
    #[serde(default)]
    pub part: Option<String>,
    #[serde(default)]
    pub store_id: Option<String>,
    #[serde(default)]
    pub parts: Option<Vec<String>>,
    #[serde(default)]
    pub stores: Option<Vec<String>>,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub province: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub district: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchCookieImport {
    pub text: String,
    #[serde(default)]
    pub cookie_type: Option<String>,
    #[serde(default)]
    pub create_missing: bool,
    #[serde(default)]
    pub parts: Option<Vec<String>>,
    #[serde(default)]
    pub stores: Option<Vec<String>>,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub province: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub district: Option<String>,
    #[serde(default)]
    pub contact: Option<ContactConfig>,
}

/// 从 Cookie 串里取出指定名称的 `name=value` 整段（含名称）。
/// 例：`pick_cookie_value("as_dima=x; as_pvi=y", "as_dima")` → `"as_dima=x"`
/// 找不到返回空字符串；名称两侧空白容错。
fn pick_cookie_value(cookie_text: &str, name: &str) -> String {
    for item in cookie_text.split(';') {
        let item = item.trim();
        if let Some((k, _)) = item.split_once('=') {
            if k.trim() == name {
                return item.to_string();
            }
        }
    }
    String::new()
}

fn parse_compact_account_line(
    line: &str,
    default_parts: &[String],
    default_stores: &[String],
    proxy_url: &str,
    default_province: &str,
    default_city: &str,
    default_district: &str,
) -> Result<Option<AccountConfig>, String> {
    let fields: Vec<&str> = line.split("----").map(str::trim).collect();
    let Some(json_index) = fields.iter().position(|field| {
        field.starts_with('{')
            && field.ends_with('}')
            && serde_json::from_str::<Value>(field).is_ok()
    }) else {
        return Ok(None);
    };
    if json_index < 2 || json_index + 1 >= fields.len() {
        return Ok(None);
    }

    let username = fields[0].to_string();
    if username.is_empty() {
        return Err("账号邮箱不能为空".to_string());
    }
    let cookie_text = fields[1..json_index]
        .iter()
        .filter(|item| !item.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if cookie_text.is_empty() {
        return Err("小程序 Cookie 不能为空".to_string());
    }

    let data: Value =
        serde_json::from_str(fields[json_index]).map_err(|err| format!("配置 JSON 无效：{err}"))?;
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let string_list = |key: &str| -> Vec<String> {
        match data.get(key) {
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect(),
            Some(Value::String(value)) => {
                let value = value.trim();
                if value.is_empty() {
                    Vec::new()
                } else {
                    vec![value.to_string()]
                }
            }
            _ => Vec::new(),
        }
    };

    let parts = {
        let parsed = string_list("phoneId");
        if parsed.is_empty() {
            default_parts.to_vec()
        } else {
            parsed
        }
    };
    let stores = {
        let parsed = string_list("shopId");
        if parsed.is_empty() {
            default_stores.to_vec()
        } else {
            parsed
        }
    };
    let part = parts[0].clone();
    let store_id = stores[0].clone();
    let province = {
        let value = text("state");
        if value.is_empty() {
            default_province.to_string()
        } else {
            value
        }
    };
    let city = {
        let value = text("city");
        if value.is_empty() {
            default_city.to_string()
        } else {
            value
        }
    };
    let district = {
        let value = text("district");
        if value.is_empty() {
            default_district.to_string()
        } else {
            value
        }
    };
    let email = {
        let value = text("email");
        if value.is_empty() {
            username.clone()
        } else {
            value
        }
    };
    let remark = fields[json_index + 1..].join("----").trim().to_string();
    // 账号唯一标识必须用「邮箱」：备注只是给人看的标签，同一个人导入多行时
    // 备注完全相同（例如都写「残枫」），若拿备注当 id 会导致后导入的行互相覆盖，
    // 表现为「导入多行后只剩下同一个账号」。
    // 同邮箱需要多套配置时，由导入层（api_import_accounts）追加 #N 序号保证唯一。
    let _ = remark;
    let id = username.clone();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut account = AccountConfig {
        id,
        username,
        password: String::new(),
        proxy_url: proxy_url.to_string(),
        proxy_urls: Vec::new(),
        part,
        store_id,
        parts,
        stores,
        province,
        city,
        district,
        quantity: 1,
        contact: ContactConfig {
            last_name: text("lastName"),
            first_name: text("firstName"),
            email,
            phone: text("Phonenum"),
            national_id_suffix: text("Self"),
        },
        poll_delay_ms: None,
        login_cookies: HashMap::new(),
        wechat_cookie: cookie_text,
        cookie_saved_at: Some(now_ms),
        orders_placed: 0,
        last_order_number: None,
        last_payment_link: None,
        enabled: true,
    };
    account.normalize();
    Ok(Some(account))
}

#[derive(Debug, Deserialize)]
pub struct LoginStart {
    pub account: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginVerify {
    pub login_id: String,
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct BatchDeleteAccounts {
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct BulkAccountConfig {
    pub parts: Vec<String>,
    pub stores: Vec<String>,
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub proxy_urls: Vec<String>,
    pub province: String,
    pub city: String,
    #[serde(default)]
    pub district: String,
    #[serde(default = "default_bulk_quantity")]
    pub quantity: u32,
    #[serde(default)]
    pub poll_delay_ms: Option<u64>,
}

fn default_bulk_quantity() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
pub struct ManualOrderRequest {
    pub part: String,
    pub store_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WechatOrderRequest {
    pub part: String,
    pub store_id: String,
    pub place: bool,
    #[serde(default)]
    pub fast: bool,
    #[serde(default)]
    pub cookie: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    /// 要 watch 的账号；为空表示全部启用账号
    #[serde(default)]
    pub account_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct WechatPrepareBatchRequest {
    #[serde(default)]
    pub account_ids: Vec<String>,
    #[serde(default)]
    pub part: Option<String>,
    /// 批量预热指定型号；为空则用每个账号自身配置
    #[serde(default)]
    pub parts: Vec<String>,
    /// 批量预热指定门店；为空则用每个账号自身配置
    #[serde(default)]
    pub stores: Vec<String>,
}

struct Inner {
    path: String,
    session_path: String,
    orders_path: String,
    config: Mutex<AppConfig>,
    statuses: Mutex<HashMap<String, AccountStatus>>,
    handles: Mutex<HashMap<String, JoinHandle<()>>>,
    logs: Mutex<Vec<LogEntry>>,
    prepared_wechat_orders: Mutex<HashMap<String, HashMap<String, Value>>>,
    /// 会话写盘节流标记：true 表示已有待执行的合并写盘任务
    session_persist_pending: std::sync::atomic::AtomicBool,
    batch_progress: Mutex<BatchProgress>,
    central_ordering_accounts: Mutex<HashSet<String>>,
    central_completed_accounts: Mutex<HashSet<String>>,
    circuit_failure_count: Mutex<u32>,
    circuit_open_until_ms: Mutex<u64>,
    order_attempts: Mutex<u32>,
    order_successes: Mutex<u32>,
    order_failures: Mutex<u32>,
    account_attempts: Mutex<HashMap<String, u32>>,
    account_retry_after_ms: Mutex<HashMap<String, u64>>,
    proxy_health: Mutex<HashMap<String, ProxyHealth>>,
    cookie_health: Mutex<HashMap<String, String>>,
    account_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    monitor_last_latency_ms: Mutex<u128>,
    monitor_available_combos: Mutex<usize>,
    monitor_dispatched_count: Mutex<u64>,
    orders: Mutex<Vec<OrderRecord>>,
    state_broadcast: tokio::sync::broadcast::Sender<String>,
    /// 动态代理池
    proxy_pool: ProxyPool,
    /// 账号 → 当前分配的代理（静态或动态）
    account_proxy_assign: Mutex<HashMap<String, String>>,
    /// 静态 IP 的稳定映射：与动态池分配分开存储，停止监控时不清理，
    /// 保证「账号 → 静态 IP」在服务运行期间恒定，预热与下单始终同出口。
    static_proxy_assign: Mutex<HashMap<String, String>>,
}

const WECHAT_SESSION_MAX_AGE_MS: u64 = 30 * 60 * 1000;

/// 静态 IP 补全协议前缀
/// 规范化静态代理地址，支持多种常见录入格式：
///
/// | 输入 | 输出 |
/// |---|---|
/// | `1.2.3.4:8080` | `http://1.2.3.4:8080` |
/// | `1.2.3.4:8080:user:pass` | `http://user:pass@1.2.3.4:8080` |
/// | `1.2.3.4|8080|user|pass` | `http://user:pass@1.2.3.4:8080` |
/// | `user:pass@1.2.3.4:8080` | `http://user:pass@1.2.3.4:8080` |
/// | `socks5://user:pass@1.2.3.4:1080` | 原样返回 |
///
/// 说明：账号 / 密码中的特殊字符会做百分号编码，避免解析失败。
fn normalize_static_proxy(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }

    // 已带协议前缀：可能是 host:port / user:pass@host:port，直接补认证部分即可
    for scheme in ["http://", "https://", "socks5://", "socks4://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            if !rest.contains('|') {
                return s.to_string();
            }
            // 形如 scheme://host|port|user|pass —— 拆掉协议再按统一规则重组
            let parts: Vec<&str> = rest.split('|').map(str::trim).collect();
            return build_proxy_url(scheme, &parts);
        }
    }

    // 竖线分隔：host|port[|user|pass]
    if s.contains('|') {
        let parts: Vec<&str> = s.split('|').map(str::trim).collect();
        return build_proxy_url("http://", &parts);
    }

    // 冒号分隔且段数为 4：host:port:user:pass
    let colon_parts: Vec<&str> = s.split(':').map(str::trim).collect();
    if colon_parts.len() == 4 && !s.contains('@') {
        return build_proxy_url("http://", &colon_parts);
    }

    // 其余（host:port / user:pass@host:port）统一补 http:// 前缀
    format!("http://{s}")
}

/// 按「host / port [ / user / pass ]」拼装代理 URL（带认证时做百分号编码）。
fn build_proxy_url(scheme: &str, parts: &[&str]) -> String {
    let host = parts.first().copied().unwrap_or("");
    let port = parts.get(1).copied().unwrap_or("");
    let user = parts.get(2).copied().unwrap_or("");
    let pass = parts.get(3).copied().unwrap_or("");
    if host.is_empty() || port.is_empty() {
        return String::new();
    }
    if user.is_empty() {
        return format!("{scheme}{host}:{port}");
    }
    format!(
        "{scheme}{}:{}@{host}:{port}",
        urlencoding::encode(user),
        urlencoding::encode(pass)
    )
}

/// 代理地址脱敏，用于日志输出：隐藏账号密码，只保留出口地址。
/// 例：`http://uhe:elz@96.8.121.134:11515` → `96.8.121.134:11515（已认证）`
///     `http://1.2.3.4:8080`              → `1.2.3.4:8080`
///     `""`                               → `直连`
fn display_proxy(proxy_url: &str) -> String {
    let s = proxy_url.trim();
    if s.is_empty() {
        return "直连".to_string();
    }
    let rest = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .or_else(|| s.strip_prefix("socks5://"))
        .or_else(|| s.strip_prefix("socks4://"))
        .unwrap_or(s);
    match rest.split_once('@') {
        Some((_, host)) => format!("{host}（已认证）"),
        None => rest.to_string(),
    }
}
const GLOBAL_MONITOR_HANDLE_KEY: &str = "__global_monitor__";
const BATCH_PREPARE_HANDLE_KEY: &str = "__batch_prepare__";
/// watch（守着结账态等库存）任务句柄 key
const WATCH_HANDLE_KEY: &str = "__watch__";
const BATCH_WARMUP_HANDLE_KEY: &str = "__warmup_batch__";
const SESSION_KEEPER_HANDLE_KEY: &str = "__session_keeper__";
const PROXY_POOL_HANDLE_KEY: &str = "__proxy_pool__";
const SESSION_REFRESH_AGE_MS: u64 = 25 * 60 * 1000;

fn wechat_session_expired(session: &Value) -> bool {
    let Some(created_at_ms) = session.get("created_at_ms").and_then(Value::as_u64) else {
        return true;
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    now_ms.saturating_sub(created_at_ms) >= WECHAT_SESSION_MAX_AGE_MS
}

fn mark_wechat_session(session: &mut Value) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    session["created_at_ms"] = json!(now_ms);
}

fn wechat_cookie_from_result(result: &Value) -> Option<String> {
    let cookies = result.pointer("/result/cookies")?.as_object()?;
    let cookie = cookies
        .iter()
        .filter_map(|(name, value)| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("{}={}", name, value))
        })
        .collect::<Vec<_>>()
        .join("; ");
    (!cookie.trim().is_empty()).then_some(cookie)
}

fn classify_wechat_error(error: &str) -> &'static str {
    if error.contains("403") {
        "会话失效（403）"
    } else if error.contains("541") {
        "Apple 限流（541）"
    } else if error.contains("order not confirmed") {
        "订单未确认"
    } else if error.contains("cart cleanup") || error.contains("购物车") {
        "购物车状态异常"
    } else if error.contains("spoke") {
        "结账节点异常"
    } else if error.contains("time slots") {
        "无可用取货时间"
    } else {
        "未知错误"
    }
}

fn summarize_wechat_steps(steps: &[Value]) -> String {
    if steps.is_empty() {
        return "步骤 0/0 成功".to_string();
    }
    let ok_count = steps
        .iter()
        .filter(|step| step.get("status").and_then(Value::as_i64) == Some(200))
        .count();
    let total_ms = steps
        .iter()
        .map(|step| step.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0))
        .sum::<i64>();
    let deleted_count = steps
        .iter()
        .filter(|step| step.get("name").and_then(Value::as_str) == Some("delete_item"))
        .count();
    let added_count = steps
        .iter()
        .filter(|step| step.get("name").and_then(Value::as_str) == Some("add_cart"))
        .count();
    // 购物袋处理说明：
    //  - 没有 delete_item / add_cart → 购物袋本来就是「目标商品 1×1」，走了快路径；
    //  - 有 → 说明执行了清空 + 重新加购。
    let cart_note = if added_count == 0 && deleted_count == 0 {
        "购物袋已就绪（预加购命中，直接进入结账）".to_string()
    } else {
        format!("已强制清空购物车并删除旧商品 {deleted_count} 项 · 加购后校验为 1 个目标商品")
    };
    let detail = steps
        .iter()
        .map(|step| {
            format!(
                "{} {}/{}ms",
                step.get("name").and_then(Value::as_str).unwrap_or("-"),
                step.get("status").and_then(Value::as_i64).unwrap_or(0),
                step.get("elapsed_ms").and_then(Value::as_i64).unwrap_or(0)
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    format!(
        "步骤 {}/{} 成功 · 总耗时 {}ms · {} · {}",
        ok_count,
        steps.len(),
        total_ms,
        cart_note,
        detail
    )
}

fn model_display_name(part: &str) -> String {
    let model = [
        ("MJT74CH/A", "iPhone 18 Pro · 256GB · 黑色"),
        ("MJT84CH/A", "iPhone 18 Pro · 256GB · 银色"),
        ("MJT94CH/A", "iPhone 18 Pro · 256GB · 勃艮第酒红色"),
        ("MJTA4CH/A", "iPhone 18 Pro · 256GB · 冰川蓝色"),
        ("MJTC4CH/A", "iPhone 18 Pro · 512GB · 黑色"),
        ("MJTD4CH/A", "iPhone 18 Pro · 512GB · 银色"),
        ("MJTE4CH/A", "iPhone 18 Pro · 512GB · 勃艮第酒红色"),
        ("MJTF4CH/A", "iPhone 18 Pro · 512GB · 冰川蓝色"),
        ("MJTG4CH/A", "iPhone 18 Pro · 1TB · 黑色"),
        ("MJTH4CH/A", "iPhone 18 Pro · 1TB · 银色"),
        ("MJTJ4CH/A", "iPhone 18 Pro · 1TB · 勃艮第酒红色"),
        ("MJTK4CH/A", "iPhone 18 Pro · 1TB · 冰川蓝色"),
        ("MJTL4CH/A", "iPhone 18 Pro · 2TB · 黑色"),
        ("MJTM4CH/A", "iPhone 18 Pro · 2TB · 银色"),
        ("MJTN4CH/A", "iPhone 18 Pro · 2TB · 勃艮第酒红色"),
        ("MJTP4CH/A", "iPhone 18 Pro · 2TB · 冰川蓝色"),
        ("MJY64CH/A", "iPhone 18 Pro Max · 256GB · 黑色"),
        ("MJY74CH/A", "iPhone 18 Pro Max · 256GB · 银色"),
        ("MJY84CH/A", "iPhone 18 Pro Max · 256GB · 勃艮第酒红色"),
        ("MJY94CH/A", "iPhone 18 Pro Max · 256GB · 冰川蓝色"),
        ("MJYA4CH/A", "iPhone 18 Pro Max · 512GB · 黑色"),
        ("MJYC4CH/A", "iPhone 18 Pro Max · 512GB · 银色"),
        ("MJYD4CH/A", "iPhone 18 Pro Max · 512GB · 勃艮第酒红色"),
        ("MJYE4CH/A", "iPhone 18 Pro Max · 512GB · 冰川蓝色"),
        ("MK2M4CH/A", "iPhone Duo · 256GB · 星光白色"),
        ("MK2N4CH/A", "iPhone Duo · 256GB · 夜空色"),
        ("MK2P4CH/A", "iPhone Duo · 512GB · 星光白色"),
        ("MK2Q4CH/A", "iPhone Duo · 512GB · 夜空色"),
        ("MK2T4CH/A", "iPhone Duo · 1TB · 星光白色"),
        ("MK2U4CH/A", "iPhone Duo · 1TB · 夜空色"),
        ("MK2V4CH/A", "iPhone Duo · 2TB · 星光白色"),
        ("MK2W4CH/A", "iPhone Duo · 2TB · 夜空色"),
    ]
    .iter()
    .find(|(code, _)| *code == part);
    model
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| part.to_string())
}

fn store_display_name(store_id: &str) -> String {
    let store = fs::read_to_string("static/stores.json")
        .ok()
        .and_then(|content| serde_json::from_str::<Vec<Value>>(&content).ok())
        .and_then(|stores| {
            stores
                .into_iter()
                .find(|store| store.get("store_id").and_then(Value::as_str) == Some(store_id))
        });
    store
        .map(|store| {
            let province = store.get("province").and_then(Value::as_str).unwrap_or("");
            let city = store.get("city").and_then(Value::as_str).unwrap_or("");
            let name = store.get("name").and_then(Value::as_str).unwrap_or("");
            format!("Apple {name}（{province} {city}）")
        })
        .unwrap_or_else(|| format!("Apple 门店 {store_id}"))
}

fn wechat_order_success_text(result: &Value) -> String {
    let summary = result.get("summary").cloned().unwrap_or(json!({}));
    let field = |name: &str| {
        summary
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    format!(
        "微信小程序下单成功：型号 {}（{}） · 门店 {}（{}） · 取货时间 {} {} · 订单 {} · 支付链接 {}",
        field("part"),
        model_display_name(&field("part")),
        field("store"),
        store_display_name(&field("store")),
        field("pickup_date"),
        field("pickup_time"),
        field("order_number"),
        field("order_url")
    )
}

/// 判断下单失败是否属于「会话/购物袋失效」，从而值得重建会话重试一次。
///
/// 关键排除项：Apple 的业务拒绝（限购、库存不足、支付问题等）不是会话问题，
/// 重跑全流程只会浪费时间和触发风控，必须原样上报错误。
fn is_retryable_wechat_session_error(result: &Value) -> bool {
    let message = result
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();

    // 业务失败：直接判定不可重试
    const BUSINESS_REJECTIONS: [&str; 5] = [
        "order not confirmed",
        "quantity_limit_exceeded",
        "超出单件购买数量",
        "sorrykey",
        "无法完成处理订单",
    ];
    if BUSINESS_REJECTIONS
        .iter()
        .any(|kw| message.contains(kw))
    {
        return false;
    }

    ["session", "expired", "403", "lineitem", "购物袋", "会话", "过期"]
        .iter()
        .any(|keyword| message.contains(keyword))
}

/// 按「字符边界」安全截断，最多保留 `max_bytes` 字节（不会 panic）。
///
/// Rust 的 `&s[..n]` 要求 n 必须落在 UTF-8 字符边界上，否则 panic。
/// Apple 的响应/错误信息常含中文，`&s[..s.len().min(N)]` 这种写法
/// 在截断点正好落在多字节字符中间时会直接崩掉整个请求，必须用本函数。
pub(crate) fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 把账号 id 转成安全的文件名。
///
/// 不能简单地把所有非法字符都替换成 `_`：`a@b.com` 与 `a_b.com` 会撞成同一个
/// 文件名，导致两个账号互相覆盖 cookie 文件。因此这里对「被替换掉的字符」
/// 追加一段基于原值的短哈希后缀，保证不同 id 不会映射到同一路径。
fn safe_file_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 12);
    let mut replaced = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
            replaced = true;
        }
    }
    if replaced {
        // FNV-1a 32bit：碰撞概率极低，且对同一 id 稳定
        let mut hash: u32 = 0x811c9dc5;
        for byte in value.as_bytes() {
            hash ^= *byte as u32;
            hash = hash.wrapping_mul(0x01000193);
        }
        out.push_str(&format!("~{hash:08x}"));
    }
    out
}

fn clean_order_error(error: &str) -> String {
    if let Some(start) = error.find("url (") {
        if error[start..].find(')').is_some() {
            return format!("{}{}", &error[..start], "请求地址已隐藏");
        }
    }
    if error.len() > 260 {
        format!("{}...", truncate_utf8(error, 257))
    } else {
        error.to_string()
    }
}

fn cookie_file_path(account_id: &str) -> std::path::PathBuf {
    std::path::Path::new("cookies").join(format!("{}.json", safe_file_name(account_id)))
}

fn load_cookie_file(account_id: &str) -> Option<HashMap<String, String>> {
    let content = fs::read_to_string(cookie_file_path(account_id)).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    let cookies = value.get("cookies")?.as_object()?;
    Some(
        cookies
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())))
            .collect(),
    )
}

fn save_cookie_file(
    account_id: &str,
    username: &str,
    cookies: &HashMap<String, String>,
) -> Result<(), String> {
    fs::create_dir_all("cookies").map_err(|e| e.to_string())?;
    let value = json!({
        "account_id": account_id,
        "username": username,
        "saved_at": AppState::now_ms(),
        "cookies": cookies,
    });
    fs::write(
        cookie_file_path(account_id),
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn save_wechat_cookie_file(account_id: &str, username: &str, cookie: &str) -> Result<(), String> {
    fs::create_dir_all("cookies").map_err(|e| e.to_string())?;
    let value = json!({
        "account_id": account_id,
        "username": username,
        "cookie": cookie,
        "saved_at": AppState::now_ms(),
    });
    fs::write(
        cookie_file_path(account_id),
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn load_all_store_ids() -> Vec<String> {
    fs::read_to_string("static/stores.json")
        .ok()
        .and_then(|content| serde_json::from_str::<Vec<Value>>(&content).ok())
        .map(|stores| {
            stores
                .iter()
                .filter_map(|store| {
                    store
                        .get("store_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn load_prepared_sessions(path: &str) -> HashMap<String, HashMap<String, Value>> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_prepared_sessions(
    path: &str,
    sessions: &HashMap<String, HashMap<String, Value>>,
) -> Result<(), String> {
    let content = serde_json::to_string_pretty(sessions).map_err(|e| e.to_string())?;
    atomic_write(path, content.as_bytes())
}

fn load_order_records(path: &str) -> Vec<OrderRecord> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<Vec<OrderRecord>>(&content).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|order| !order.order_number.trim().is_empty())
        .collect()
}

/// 原子写入：先写临时文件再 rename，避免并发写导致文件损坏
fn atomic_write(path: &str, content: &[u8]) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let tmp = format!("{}.tmp{}", path, std::process::id());
    fs::write(&tmp, content).map_err(|e| e.to_string())?;
    match fs::rename(&tmp, path) {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.to_string())
        }
    }
}

fn save_order_records(path: &str, orders: &[OrderRecord]) -> Result<(), String> {
    let content = serde_json::to_string_pretty(orders).map_err(|e| e.to_string())?;
    atomic_write(path, content.as_bytes())
}

fn backup_file(source: &str, target: &str) -> Result<(), String> {
    if !std::path::Path::new(source).exists() {
        return Ok(());
    }
    if let Some(parent) = std::path::Path::new(target).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::copy(source, target)
        .map_err(|e| e.to_string())
        .map(|_| ())
}

fn append_log_file(entry: &LogEntry) -> Result<(), String> {
    const MAX_LOG_FILE_BYTES: u64 = 20 * 1024 * 1024; // 单日日志上限 20MB
    fs::create_dir_all("logs").map_err(|e| e.to_string())?;
    let path = format!(
        "logs/{}.jsonl",
        entry.ts_ms.checked_div(86_400_000).unwrap_or_default()
    );
    // 超过上限则不再写入，避免磁盘被写满
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() >= MAX_LOG_FILE_BYTES {
            return Ok(());
        }
    }
    let line = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| e.to_string())
}

/// Auto clean logs older than 7 days
fn clean_old_logs() {
    let _ = fs::create_dir_all("logs");
    let current_day = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        / 86_400_000;
    if let Ok(entries) = fs::read_dir("logs") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".jsonl") {
                if let Ok(day) = stem.parse::<u64>() {
                    if current_day.saturating_sub(day) > 7 {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
}

fn config_key_path(path: &str) -> std::path::PathBuf {
    std::path::Path::new(path).with_file_name("config.key")
}

fn load_or_create_config_key(path: &str) -> Result<[u8; 32], String> {
    let path = config_key_path(path);
    if path.exists() {
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(content.trim())
            .map_err(|e| e.to_string())?;
        if decoded.len() != 32 {
            return Err("invalid config key".to_string());
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&decoded);
        return Ok(key);
    }
    let mut key = [0u8; 32];
    use rand::RngCore;
    OsRng.fill_bytes(&mut key);
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    fs::write(&path, encoded).map_err(|e| e.to_string())?;
    Ok(key)
}

fn encrypt_config_value(path: &str, value: &AppConfig) -> Result<Vec<u8>, String> {
    let key = load_or_create_config_key(path)?;
    let cipher = ChaCha20Poly1305::new(CipherKey::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    use rand::RngCore;
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|e| e.to_string())?;
    let mut output = nonce_bytes.to_vec();
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

fn decrypt_config_value(path: &str, encrypted: &[u8]) -> Result<AppConfig, String> {
    if encrypted.len() < 13 {
        return Err("invalid encrypted config".to_string());
    }
    let key = load_or_create_config_key(path)?;
    let cipher = ChaCha20Poly1305::new(CipherKey::from_slice(&key));
    let nonce = Nonce::from_slice(&encrypted[..12]);
    let plaintext = cipher
        .decrypt(nonce, &encrypted[12..])
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&plaintext).map_err(|e| e.to_string())
}

impl AppState {
    pub async fn load_or_create(path: &str) -> Self {
        clean_old_logs();
        let legacy_path = std::path::Path::new(path)
            .with_file_name("config.json")
            .to_string_lossy()
            .to_string();
        let session_path = std::path::Path::new(path)
            .with_file_name("sessions.json")
            .to_string_lossy()
            .to_string();
        let orders_path = std::path::Path::new(path)
            .with_file_name("orders.json")
            .to_string_lossy()
            .to_string();
        let mut config = if std::path::Path::new(path).exists() {
            fs::read(path)
                .ok()
                .and_then(|encrypted| decrypt_config_value(path, &encrypted).ok())
                .unwrap_or_default()
        } else {
            fs::read_to_string(&legacy_path)
                .ok()
                .and_then(|content| serde_json::from_str(&content).ok())
                .unwrap_or_default()
        };
        for account in &mut config.accounts {
            account.normalize();
            if account.login_cookies.is_empty() {
                if let Some(cookies) = load_cookie_file(&account.id) {
                    account.login_cookies = cookies;
                }
            }
            if !account.login_cookies.is_empty() {
                let saved_at = AppState::now_ms();
                account.cookie_saved_at = Some(saved_at);
                let account_id = account.id.clone();
                let username = account.username.clone();
                let cookies = account.login_cookies.clone();
                let _ = save_cookie_file(&account_id, &username, &cookies);
            }
        }
        let mut prepared_sessions = load_prepared_sessions(&session_path);
        {
            let valid_account_ids = config
                .accounts
                .iter()
                .map(|account| account.id.clone())
                .collect::<Vec<_>>();
            prepared_sessions.retain(|account_id, _| valid_account_ids.contains(account_id));
            for account in &config.accounts {
                if let Some(sessions) = prepared_sessions.get_mut(&account.id) {
                    let newest = sessions
                        .iter()
                        .max_by_key(|(_, session)| {
                            session
                                .get("created_at_ms")
                                .and_then(Value::as_u64)
                                .unwrap_or_default()
                        })
                        .map(|(key, _)| key.clone());
                    if let Some(newest) = newest {
                        sessions.retain(|key, _| key == &newest);
                    }
                    sessions.retain(|session_key, _| {
                        session_key
                            .split_once('|')
                            .map(|(part, store)| {
                                account.parts.iter().any(|item| item == part)
                                    && account.stores.iter().any(|item| item == store)
                            })
                            .unwrap_or(false)
                    });
                }
            }
        }
        let state = Self {
            inner: Arc::new(Inner {
                path: path.to_string(),
                session_path: session_path.clone(),
                orders_path: orders_path.clone(),
                config: Mutex::new(config),
                statuses: Mutex::new(HashMap::new()),
                handles: Mutex::new(HashMap::new()),
                logs: Mutex::new(Vec::new()),
                prepared_wechat_orders: Mutex::new(prepared_sessions),
                session_persist_pending: std::sync::atomic::AtomicBool::new(false),
                batch_progress: Mutex::new(BatchProgress::default()),
                central_ordering_accounts: Mutex::new(HashSet::new()),
                central_completed_accounts: Mutex::new(HashSet::new()),
                circuit_failure_count: Mutex::new(0),
                circuit_open_until_ms: Mutex::new(0),
                order_attempts: Mutex::new(0),
                order_successes: Mutex::new(0),
                order_failures: Mutex::new(0),
                account_attempts: Mutex::new(HashMap::new()),
                account_retry_after_ms: Mutex::new(HashMap::new()),
                proxy_health: Mutex::new(HashMap::new()),
                cookie_health: Mutex::new(HashMap::new()),
                account_locks: Mutex::new(HashMap::new()),
                monitor_last_latency_ms: Mutex::new(0),
                monitor_available_combos: Mutex::new(0),
                monitor_dispatched_count: Mutex::new(0),
                orders: Mutex::new(load_order_records(&orders_path)),
                state_broadcast: {
                    let (tx, _) = tokio::sync::broadcast::channel(16);
                    tx
                },
                proxy_pool: ProxyPool::new(),
                account_proxy_assign: Mutex::new(HashMap::new()),
                static_proxy_assign: Mutex::new(HashMap::new()),
            }),
        };
        let _ = state.persist_config().await;
        if std::path::Path::new(&legacy_path).exists() {
            let _ = fs::remove_file(&legacy_path);
        }
        let _ = backup_file(path, "backups/config.bin");
        let _ = backup_file(&session_path, "backups/sessions.json");
        let initial_orders = state.inner.orders.lock().await.clone();
        if let Err(err) = save_order_records(&orders_path, &initial_orders) {
            eprintln!("failed to save order records: {err}");
        }
        let initial_sessions = state.inner.prepared_wechat_orders.lock().await.clone();
        if let Err(err) = save_prepared_sessions(&session_path, &initial_sessions) {
            eprintln!("failed to save WeChat sessions: {err}");
        }
        state.sync_statuses().await;
        state
    }


    pub async fn subscribe_state(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.inner.state_broadcast.subscribe()
    }


    pub async fn serialize_logs_for_ws(&self) -> String {
        let logs = self.inner.logs.lock().await;
        let recent: Vec<&LogEntry> = logs.iter().rev().take(500).collect();
        serde_json::to_string(&recent).unwrap_or_default()
    }

    pub async fn serialize_state_for_ws(&self) -> String {
        let payload = build_state_payload(self).await;
        serde_json::to_string(&payload).unwrap_or_default()
    }


    /// 当前毫秒时间戳。系统时钟早于 UNIX_EPOCH（被回拨到 1970 前）时返回 0，
    /// 绝不 panic —— 该方法被大量调用，panic 会直接打死整个进程。
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    /// 启动一个「受保护」的后台任务：panic 时写日志 + 标记状态，而不是静默死亡。
    ///
    /// 实现要点：必须是**单层 spawn**。
    /// 早期版本用「外层 spawn + 内层 spawn 干活」来捕获 panic，但 api_stop 只能 abort
    /// 外层 handle，内层任务继续跑 —— 表现为「日志说已停止，实际还在监控」。
    /// 这里用 catch_unwind 在同一层捕获 panic，abort 才真正生效。
    fn spawn_guarded<F>(&self, name: &'static str, fut: F) -> JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        use futures_util::FutureExt;

        let state = self.clone();
        tokio::spawn(async move {
            // AssertUnwindSafe：任务内部持有 Arc<Mutex>，panic 后我们不再依赖其一致性
            // （panic 即视为任务终止，仅记录日志），因此跳过 UnwindSafe 约束。
            let guarded = std::panic::AssertUnwindSafe(fut).catch_unwind();
            if guarded.await.is_err() {
                state
                    .log(
                        "batch",
                        "error",
                        format!("后台任务 [{name}] 异常崩溃（已停止）"),
                    )
                    .await;
                // 让前端能看到异常：把运行中的账号标记为 error
                let mut statuses = state.inner.statuses.lock().await;
                for status in statuses.values_mut() {
                    if status.running {
                        status.status = "error".to_string();
                        status.last_error = Some(format!("后台任务 [{name}] 异常崩溃"));
                    }
                }
            }
        })
    }

    async fn account_lock(self: &Self, account_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.inner.account_locks.lock().await;
        locks
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn set_cookie_health(&self, account_id: &str, status: &str) {
        self.inner
            .cookie_health
            .lock()
            .await
            .insert(account_id.to_string(), status.to_string());
        self.sync_statuses().await;
    }

    /// 记录成功订单：去重 + 只保留最近 ORDER_HISTORY_MAX 条。
    async fn record_order(
        &self,
        account_id: &str,
        username: &str,
        order_number: &str,
        part: &str,
        store_id: &str,
        pickup_date: &str,
        pickup_time: &str,
        payment_link: &str,
        total_ms: u64,
    ) {
        /// 内存中保留的订单上限（超出后丢弃最旧的）
        const ORDER_HISTORY_MAX: usize = 2000;

        let mut orders = self.inner.orders.lock().await;

        // 去重：同一订单号只记一次（重试路径可能重复上报）
        if !order_number.is_empty() && orders.iter().any(|o| o.order_number == order_number) {
            return;
        }

        orders.push(OrderRecord {
            account_id: account_id.to_string(),
            username: username.to_string(),
            order_number: order_number.to_string(),
            part: part.to_string(),
            store_id: store_id.to_string(),
            pickup_date: pickup_date.to_string(),
            pickup_time: pickup_time.to_string(),
            payment_link: payment_link.to_string(),
            total_ms,
            created_at_ms: Self::now_ms(),
        });

        // 裁剪：只保留最近 N 条，避免内存与 WebSocket payload 无限膨胀
        if orders.len() > ORDER_HISTORY_MAX {
            let start = orders.len() - ORDER_HISTORY_MAX;
            orders.drain(0..start);
        }
        let orders_snapshot = orders.clone();
        drop(orders);
        if let Err(err) = save_order_records(&self.inner.orders_path, &orders_snapshot) {
            self.log("batch", "error", format!("订单历史保存失败：{err}"))
                .await;
        }

        // 去重已通过 → 推送订单到外部接口。
        // 放在 drop(orders) 之后，避免与 upload_order 内取 config 锁形成嵌套。
        if !order_number.is_empty() {
            self.upload_order(account_id, username, order_number, payment_link, part)
                .await;
        }
    }

    /// 把成交订单推送到外部接口（fire-and-forget，绝不影响下单主流程）。
    ///
    /// 触发点在 record_order：那是订单落库的唯一入口，6 处下单路径全部汇聚于此。
    async fn upload_order(
        &self,
        account_id: &str,
        username: &str,
        order_number: &str,
        url: &str,
        part: &str,
    ) {
        let (upload_url, enabled, cookie_text) = {
            let config = self.inner.config.lock().await;
            let enabled = config.settings.order_upload_enabled
                && !config.settings.order_upload_url.trim().is_empty();
            // 取 wechat_cookie 必须按 account_id 精确匹配：
            // 同邮箱可以有多套配置（id = email / email#2 / email#3），
            // 它们的 username 完全相同但 cookie 不同，按 username 找会串号。
            let cookie = config
                .accounts
                .iter()
                .find(|a| a.id == account_id)
                .or_else(|| config.accounts.iter().find(|a| a.username == username))
                .map(|a| a.wechat_cookie.clone())
                .unwrap_or_default();
            (
                config.settings.order_upload_url.clone(),
                enabled,
                cookie,
            )
        };
        if !enabled {
            return;
        }
        let payload = json!({
            // 契约：account 传 username（邮箱），与接收端文档一致
            "account": username,
            "as_dima": pick_cookie_value(&cookie_text, "as_dima"),
            "as_pvi": pick_cookie_value(&cookie_text, "as_pvi"),
            "orderNumber": order_number,
            "url": url,
            "phoneId": part,
        });
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                self.log(
                    "batch",
                    "error",
                    format!("订单上传失败（构建客户端）：{err}"),
                )
                .await;
                return;
            }
        };
        match client.post(&upload_url).json(&payload).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                if status == 200 {
                    self.log(
                        "batch",
                        "info",
                        format!("订单已上传：{order_number} → {upload_url}"),
                    )
                    .await;
                } else {
                    self.log(
                        "batch",
                        "error",
                        format!(
                            "订单上传失败（HTTP {status}）：{}",
                            truncate_utf8(&body, 200)
                        ),
                    )
                    .await;
                }
            }
            Err(err) => {
                self.log(
                    "batch",
                    "error",
                    format!("订单上传失败（网络错误）：{err}"),
                )
                .await;
            }
        }
    }

    async fn log(&self, account_id: &str, level: &str, message: impl AsRef<str>) {
        let entry = LogEntry {
            ts_ms: Self::now_ms(),
            account_id: account_id.to_string(),
            level: level.to_string(),
            message: message.as_ref().to_string(),
        };
        let _ = append_log_file(&entry);
        let mut logs = self.inner.logs.lock().await;
        logs.push(entry);
        if logs.len() > 500 {
            let start = logs.len() - 500;
            logs.drain(0..start);
        }
    }

    async fn sync_statuses(&self) {
        let session_summaries = {
            let prepared = self.inner.prepared_wechat_orders.lock().await;
            prepared
                .iter()
                .map(|(account_id, sessions)| {
                    let now_ms = Self::now_ms();
                    let mut fresh_count = 0usize;
                    let mut min_remaining_ms = None;
                    let mut newest_key = None;
                    let mut newest_created_ms = 0u64;
                    for (session_key, session) in sessions {
                        let created_at_ms = session
                            .get("created_at_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        if created_at_ms >= newest_created_ms {
                            newest_created_ms = created_at_ms;
                            newest_key = Some(session_key.clone());
                        }
                    }
                    for session in sessions.values() {
                        let created_at_ms = session
                            .get("created_at_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        let remaining_ms = WECHAT_SESSION_MAX_AGE_MS
                            .saturating_sub(now_ms.saturating_sub(created_at_ms));
                        if remaining_ms > 0 {
                            fresh_count += 1;
                            min_remaining_ms = Some(
                                min_remaining_ms
                                    .map(|current: u64| current.min(remaining_ms))
                                    .unwrap_or(remaining_ms),
                            );
                        }
                    }
                    let newest_key = newest_key.unwrap_or_default();
                    let session_part = newest_key.split_once('|').map(|(part, _)| part.to_string());
                    let session_store = newest_key
                        .split_once('|')
                        .map(|(_, store)| store.to_string());
                    (
                        account_id.clone(),
                        (
                            sessions.len(),
                            fresh_count,
                            sessions.len().saturating_sub(fresh_count),
                            min_remaining_ms,
                            session_part,
                            session_store,
                            sessions.values().any(|s| {
                                s.pointer("/result/checkout_ready")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                            }),
                            sessions.values().any(|s| {
                                s.pointer("/result/warmup")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                    && !s
                                        .pointer("/result/checkout_ready")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false)
                            }),
                        ),
                    )
                })
                .collect::<HashMap<_, _>>()
        };
        let account_attempts = self.inner.account_attempts.lock().await.clone();
        let account_retry_after_ms = self.inner.account_retry_after_ms.lock().await.clone();
        let proxy_health = self.inner.proxy_health.lock().await.clone();
        let cookie_health = self.inner.cookie_health.lock().await.clone();
        let config = self.inner.config.lock().await;
        let mut statuses = self.inner.statuses.lock().await;
        for account in &config.accounts {
            let mut default = AccountStatus {
                id: account.id.clone(),
                phone: account.contact.phone.clone(),
                username: account.username.clone(),
                part: account.part.clone(),
                store_id: account.store_id.clone(),
                parts: account.parts.clone(),
                stores: account.stores.clone(),
                cookie_count: account.login_cookies.len(),
                cookie_saved_at: account.cookie_saved_at,
                proxy: account.proxy_url.clone(),
                enabled: account.enabled,
                running: false,
                status: "stopped".to_string(),
                last_latency_ms: 0,
                last_poll: None,
                orders_placed: account.orders_placed as u64,
                last_error: None,
                last_order_number: account.last_order_number.clone(),
                last_payment_link: account.last_payment_link.clone(),
                wechat_session_count: 0,
                wechat_session_fresh_count: 0,
                wechat_session_expired_count: 0,
                wechat_session_min_remaining_ms: None,
                wechat_session_part: None,
                wechat_session_store: None,
        wechat_session_checkout_ready: false,
        wechat_session_fallback_only: false,
                wechat_cookie_status: if account.wechat_cookie.trim().is_empty() {
                    "missing".to_string()
                } else if cookie_health
                    .get(&account.id)
                    .map(String::as_str)
                    .is_some_and(|status| status == "invalid" || status == "rate_limited")
                {
                    cookie_health
                        .get(&account.id)
                        .cloned()
                        .unwrap_or_else(|| "invalid".to_string())
                } else if account
                    .cookie_saved_at
                    .map(|saved_at| Self::now_ms().saturating_sub(saved_at) > 2 * 60 * 60 * 1000)
                    .unwrap_or(true)
                {
                    "stale".to_string()
                } else {
                    "valid".to_string()
                },
                wechat_cookie_age_ms: account
                    .cookie_saved_at
                    .map(|saved_at| Self::now_ms().saturating_sub(saved_at)),
                order_attempts: account_attempts.get(&account.id).cloned().unwrap_or(0),
                retry_after_ms: account_retry_after_ms.get(&account.id).cloned(),
                proxy_health: proxy_health
                    .get(if account.proxy_url.trim().is_empty() {
                        "direct"
                    } else {
                        &account.proxy_url
                    })
                    .map(|health| {
                        if health.healthy {
                            "healthy".to_string()
                        } else {
                            "unhealthy".to_string()
                        }
                    })
                    .unwrap_or_else(|| "unknown".to_string()),
                priority_score: 0,
            };
            default.priority_score += default.wechat_session_fresh_count as u32 * 10;
            if default.wechat_cookie_status == "valid" {
                default.priority_score += 20;
            }
            if default.proxy_health == "healthy" {
                default.priority_score += 20;
            }
            if default.retry_after_ms.unwrap_or_default() <= Self::now_ms() {
                default.priority_score += 10;
            }
            if let Some(summary) = session_summaries.get(&account.id) {
                default.wechat_session_count = summary.0;
                default.wechat_session_fresh_count = summary.1;
                default.wechat_session_expired_count = summary.2;
                default.wechat_session_checkout_ready = summary.6;
                default.wechat_session_fallback_only = summary.7;
                default.wechat_session_min_remaining_ms = summary.3;
                default.wechat_session_part = summary.4.clone();
                default.wechat_session_store = summary.5.clone();
            }
            match statuses.get_mut(&account.id) {
                Some(status) => {
                    status.username = default.username;
                    status.part = default.part;
                    status.store_id = default.store_id;
                    status.parts = default.parts;
                    status.stores = default.stores;
                    status.cookie_count = default.cookie_count;
                    status.cookie_saved_at = default.cookie_saved_at;
                    status.orders_placed = default.orders_placed;
                    status.last_order_number = default.last_order_number;
                    status.last_payment_link = default.last_payment_link;
                    status.proxy = default.proxy;
                    status.enabled = default.enabled;
                    status.wechat_session_count = default.wechat_session_count;
                    status.wechat_session_fresh_count = default.wechat_session_fresh_count;
                    status.wechat_session_expired_count = default.wechat_session_expired_count;
            status.wechat_session_checkout_ready = default.wechat_session_checkout_ready;
            status.wechat_session_fallback_only = default.wechat_session_fallback_only;
                    status.wechat_session_min_remaining_ms =
                        default.wechat_session_min_remaining_ms;
                    status.wechat_session_part = default.wechat_session_part.clone();
                    status.wechat_session_store = default.wechat_session_store.clone();
                    status.order_attempts = default.order_attempts;
                    status.retry_after_ms = default.retry_after_ms;
                    status.proxy_health = default.proxy_health;
                    status.priority_score = default.priority_score;
                }
                None => {
                    statuses.insert(account.id.clone(), default);
                }
            }
        }
    }

    async fn persist_config(&self) -> Result<(), String> {
        let (config, path) = {
            let config = self.inner.config.lock().await;
            (config.clone(), self.inner.path.clone())
        };
        let encrypted = encrypt_config_value(&path, &config)?;
        // 原子写：避免并发写坏配置
        atomic_write(&path, &encrypted)
    }

    async fn store_wechat_session(&self, account_id: &str, session_key: String, mut result: Value) {
        mark_wechat_session(&mut result);
        {
            let mut prepared = self.inner.prepared_wechat_orders.lock().await;
            let sessions = prepared.entry(account_id.to_string()).or_default();
            sessions.clear();
            sessions.insert(session_key, result);
        }
        // 写盘改为「节流合并」：批量预热时 100 个账号会在短时间内各写一次，
        // 每次都做全量序列化 + 加密 + 原子写，磁盘与 CPU 开销很大。
        // 这里延迟 800ms 合并为一次写入；进程若在此期间退出最多丢失最近一次会话，
        // 而会话本身可从 Apple 侧重新准备，代价可接受。
        self.schedule_session_persist().await;
        self.sync_statuses().await;
    }

    /// 节流合并写盘：短时间内的多次调用只触发一次真实写入。
    async fn schedule_session_persist(&self) {
        // 已有待执行任务则不重复排程
        if self.inner.session_persist_pending.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            // 先落盘再释放标记：期间的写入会看到 pending=true 而跳过排程，
            // 但由于我们在写盘前才 take 快照，仍能保证不丢数据。
            let sessions = state.inner.prepared_wechat_orders.lock().await.clone();
            if let Err(err) = save_prepared_sessions(&state.inner.session_path, &sessions) {
                state
                    .log("batch", "error", format!("保存微信小程序会话失败：{err}"))
                    .await;
            }
            state
                .inner
                .session_persist_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    async fn remove_wechat_session(&self, account_id: &str, session_key: String) {
        let mut changed = false;
        {
            let mut prepared = self.inner.prepared_wechat_orders.lock().await;
            if let Some(sessions) = prepared.get_mut(account_id) {
                changed = sessions.remove(&session_key).is_some();
                if sessions.is_empty() {
                    prepared.remove(account_id);
                }
            }
        }
        if !changed {
            return;
        }
        // 与 store 一致：改为节流合并写盘
        self.schedule_session_persist().await;
        self.sync_statuses().await;
    }

    async fn record_proxy_result(
        &self,
        account_id: &str,
        proxy_url: &str,
        success: bool,
        error: Option<String>,
    ) {
        /// proxy_health 容量上限：动态池每个 IP 都会产生一条记录，
        /// 长期运行会累积上千条且不再有引用，必须定期裁剪。
        const PROXY_HEALTH_MAX: usize = 500;

        let key = if proxy_url.trim().is_empty() {
            "direct".to_string()
        } else {
            proxy_url.to_string()
        };
        // 直连模式下不存在"代理故障"概念：Apple 侧的 541/503 只反映 Apple 限流，
        // 不代表本地网络异常。只在配置了真实代理时才跟踪健康度。
        if key == "direct" {
            let is_apple_side = error
                .as_deref()
                .map(|e| e.contains("541") || e.contains("503") || e.contains("Service Unavailable"))
                .unwrap_or(false);
            if !success && is_apple_side {
                return;
            }
        }
        {
            let mut health_map = self.inner.proxy_health.lock().await;
            let now_ms = Self::now_ms();
            let health = health_map.entry(key.clone()).or_insert_with(|| ProxyHealth {
                proxy_url: key.clone(),
                successes: 0,
                failures: 0,
                consecutive_failures: 0,
                last_error: None,
                last_used_ms: None,
                healthy: true,
            });
            health.last_error = error;
            health.last_used_ms = Some(now_ms);
            if success {
                health.successes += 1;
                health.consecutive_failures = 0;
                health.healthy = true;
            } else {
                health.failures += 1;
                health.consecutive_failures += 1;
                if health.consecutive_failures >= 3 {
                    health.healthy = false;
                }
            }

            // 容量控制：超出上限时淘汰「最久未使用」的记录（direct 永久保留）
            if health_map.len() > PROXY_HEALTH_MAX {
                let victim = health_map
                    .iter()
                    .filter(|(k, _)| k.as_str() != "direct")
                    .min_by_key(|(_, h)| h.last_used_ms.unwrap_or(0))
                    .map(|(k, _)| k.clone());
                if let Some(victim) = victim {
                    health_map.remove(&victim);
                }
            }
        }
        // 脱敏：不能把代理账号密码写进日志
        self.log(
            account_id,
            "info",
            format!(
                "代理状态更新：{} · {}",
                display_proxy(proxy_url),
                if success { "正常" } else { "异常" }
            ),
        )
        .await;
    }

    async fn record_order_success(&self, account_id: &str, proxy_url: &str) {
        *self.inner.order_attempts.lock().await += 1;
        *self.inner.order_successes.lock().await += 1;
        self.inner.account_attempts.lock().await.remove(account_id);
        self.inner
            .account_retry_after_ms
            .lock()
            .await
            .remove(account_id);
        self.record_proxy_result(account_id, proxy_url, true, None)
            .await;
    }

    async fn record_order_failure(
        &self,
        account_id: &str,
        proxy_url: &str,
        error: String,
        base_delay_ms: u64,
        max_attempts: u32,
    ) {
        *self.inner.order_attempts.lock().await += 1;
        *self.inner.order_failures.lock().await += 1;
        let attempts = {
            let mut attempts = self.inner.account_attempts.lock().await;
            let next = attempts.get(account_id).cloned().unwrap_or(0) + 1;
            attempts.insert(account_id.to_string(), next);
            next
        };
        if attempts >= max_attempts {
            self.inner.account_retry_after_ms.lock().await.insert(
                account_id.to_string(),
                Self::now_ms().saturating_add(
                    base_delay_ms
                        .saturating_mul(1u64.checked_shl(attempts).unwrap_or(1))
                        .max(30_000),
                ),
            );
        } else {
            let delay_ms =
                base_delay_ms.saturating_mul(1u64.checked_shl(attempts - 1).unwrap_or(1));
            self.inner.account_retry_after_ms.lock().await.insert(
                account_id.to_string(),
                Self::now_ms().saturating_add(delay_ms),
            );
        }
        if is_network_error(&error) {
            self.record_proxy_result(account_id, proxy_url, false, Some(error))
                .await;
        }
    }

    async fn persist_session_cookies(&self, account_id: &str, cookies: CookieMap) {
        {
            let mut config = self.inner.config.lock().await;
            if let Some(account) = config
                .accounts
                .iter_mut()
                .find(|account| account.id == account_id)
            {
                for (name, value) in cookies {
                    account.login_cookies.insert(name, value);
                }
            }
        }
        let _ = self.persist_config().await;
        self.sync_statuses().await;
    }

    async fn persist_wechat_cookie_from_result(&self, account_id: &str, result: &Value) {
        let Some(cookie) = wechat_cookie_from_result(result) else {
            return;
        };
        let saved_at_ms = Self::now_ms();
        {
            let mut config = self.inner.config.lock().await;
            if let Some(account) = config
                .accounts
                .iter_mut()
                .find(|account| account.id == account_id)
            {
                if account.wechat_cookie == cookie {
                    return;
                }
                account.wechat_cookie = cookie;
                account.cookie_saved_at = Some(saved_at_ms);
            }
        }
        let _ = self.persist_config().await;
        self.sync_statuses().await;
    }

    async fn persist_placed_order(
        &self,
        account_id: &str,
        order_number: String,
        payment_link: String,
    ) {
        {
            let mut config = self.inner.config.lock().await;
            if let Some(account) = config
                .accounts
                .iter_mut()
                .find(|account| account.id == account_id)
            {
                account.orders_placed = account.orders_placed.saturating_add(1);
                if !order_number.is_empty() {
                    account.last_order_number = Some(order_number);
                }
                if !payment_link.is_empty() {
                    account.last_payment_link = Some(payment_link);
                }
            }
        }
        let _ = self.persist_config().await;
        self.sync_statuses().await;
    }

    async fn start_account_by_config(&self, account: AccountConfig) {
        {
            let mut handles = self.inner.handles.lock().await;
            if let Some(handle) = handles.remove(&account.id) {
                handle.abort();
            }
        }
        if !account.enabled {
            return;
        }
        let account_id = account.id.clone();
        let state = self.clone();
        let handle = self.spawn_guarded("account_monitor", async move {
            state.run_account_multi(account).await;
        });
        self.inner
            .handles
            .lock()
            .await
            .insert(account_id.clone(), handle);
        self.update_status(&account_id, |status| {
            status.running = true;
            status.status = "starting".to_string();
            status.last_error = None;
        })
        .await;
    }

    async fn start_global_monitor(&self) {
        self.inner.central_completed_accounts.lock().await.clear();
        self.inner.account_attempts.lock().await.clear();
        self.inner.account_retry_after_ms.lock().await.clear();
        {
            let mut handles = self.inner.handles.lock().await;
            // 仅终止「按账号的独立监控任务」，保留后台常驻任务：
            //   - 会话续期（否则定时刷新会失效）
            //   - 代理池维护（否则 541 换 IP 无池可用）
            //   - 批量预热 / 批量准备（否则正在跑的预热会被打断，进度条永远卡住）
            const KEEP: [&str; 4] = [
                SESSION_KEEPER_HANDLE_KEY,
                PROXY_POOL_HANDLE_KEY,
                BATCH_PREPARE_HANDLE_KEY,
                BATCH_WARMUP_HANDLE_KEY,
            ];
            let stale: Vec<String> = handles
                .keys()
                .filter(|key| !KEEP.contains(&key.as_str()))
                .cloned()
                .collect();
            for key in stale {
                if let Some(handle) = handles.remove(&key) {
                    handle.abort();
                }
            }
        }
        let state = self.clone();
        let handle = self.spawn_guarded("global_monitor", async move {
            state.run_global_monitor().await;
        });
        self.inner
            .handles
            .lock()
            .await
            .insert(GLOBAL_MONITOR_HANDLE_KEY.to_string(), handle);

        let account_ids = {
            let config = self.inner.config.lock().await;
            config
                .accounts
                .iter()
                .filter(|account| account.enabled)
                .map(|account| account.id.clone())
                .collect::<Vec<_>>()
        };
        for account_id in account_ids {
            self.update_status(&account_id, |status| {
                status.running = true;
                status.status = "central-monitor".to_string();
                status.last_error = None;
            })
            .await;
        }
    }

    /// 为账号解析实际使用的代理：
    /// 1) 账号自身配置了 proxy_url / proxy_urls → 优先使用
    /// 2) 启用静态 IP 分配 → 按账号序号取一个静态 IP（固定）
    /// 3) 启用动态代理池 → 从池中取一个（取出即消耗，用完丢弃）
    pub async fn resolve_account_proxy(self: &Self, account: &AccountConfig) -> Option<String> {
        // 1. 账号自带代理优先
        if !account.proxy_url.trim().is_empty() {
            return Some(account.proxy_url.clone());
        }
        if let Some(first) = account.proxy_urls.iter().find(|p| !p.trim().is_empty()) {
            return Some(first.clone());
        }

        let settings = self.inner.config.lock().await.settings.clone();

        // 2. 静态 IP 分配（每个账号固定一个）
        if settings.static_proxy_enabled && !settings.static_proxies.is_empty() {
            // 分配策略：按「账号顺序」取第 N 个 IP（N = 该账号在启用账号中的序号），
            // 保证 #IP >= #账号 时是严格的一对一；并用持久化映射表锁定结果，
            // 这样账号增删 / 列表重排 / 服务重启都不会把账号换到别的 IP 上。
            let fallback_idx = {
                let config = self.inner.config.lock().await;
                config
                    .accounts
                    .iter()
                    .filter(|a| a.enabled)
                    .position(|a| a.id == account.id)
                    .unwrap_or(0)
            };
            let proxies = settings.static_proxies.clone();
            let assigned = self
                .static_proxy_for(account, &proxies, fallback_idx)
                .await;
            return Some(assigned);
        }

        // 3. 动态代理池
        if settings.proxy_pool_enabled {
            // 已有分配则复用（同一轮监控中不重复消耗 IP）
            {
                let assign = self.inner.account_proxy_assign.lock().await;
                if let Some(existing) = assign.get(&account.id) {
                    if !existing.is_empty() {
                        return Some(existing.clone());
                    }
                }
            }
            match self.inner.proxy_pool.take_one().await {
                Some(proxy) => {
                    let mut assign = self.inner.account_proxy_assign.lock().await;
                    assign.insert(account.id.clone(), proxy.clone());
                    return Some(proxy);
                }
                None => {
                    // 池内暂无已测活的可用 IP：记录一次提示，本次直连（不阻塞下单）
                    self.log(
                        &account.id,
                        "warn",
                        "代理池暂无可用 IP（已测活为 0），本次下单直连",
                    )
                    .await;
                }
            }
        }

        None
    }

    /// 读取某个已保存会话记录的出口代理（可能为空 = 直连）。
    /// 预热 / 续期 / 下单 / 校验都必须复用同一个出口 IP，否则 Apple 判定会话失效（403）。
    async fn resolve_session_proxy(&self, account_id: &str, session_key: &str) -> Option<String> {
        let prepared = self.inner.prepared_wechat_orders.lock().await;
        prepared
            .get(account_id)
            .and_then(|sessions| sessions.get(session_key))
            .and_then(|session| {
                session
                    .pointer("/result/proxy_url")
                    .and_then(Value::as_str)
                    .or_else(|| session.get("proxy_url").and_then(Value::as_str))
                    .map(str::to_string)
            })
    }

    /// 账号完成下单后释放动态代理分配（IP 用完即弃）
    /// 为账号分配静态代理，并保证分配稳定：
    /// - 首次（或账号新增）时按 fallback_idx 取第 N 个 IP；
    /// - 之后始终复用同一条映射，账号列表重排、增删账号、服务重启都不变；
    /// - 写入时二次检查，避免并发首次分配时同一 IP 被分给多个账号。
    async fn static_proxy_for(
        &self,
        account: &AccountConfig,
        proxies: &[String],
        fallback_idx: usize,
    ) -> String {
        if proxies.is_empty() {
            return String::new();
        }
        // 1) 已有映射直接复用
        {
            let assign = self.inner.static_proxy_assign.lock().await;
            if let Some(existing) = assign.get(&account.id) {
                if !existing.is_empty() {
                    return existing.clone();
                }
            }
        }
        // 2) 首次分配：从 fallback_idx 开始，取第一个「当前仍在用的账号没占用」的 IP。
        //    关键：taken 只统计「仍然存在（未被删除）的账号」的占用，
        //    否则已删除账号的 IP 会永久占位，新账号只能撞车共用。
        let chosen = {
            let assign = self.inner.static_proxy_assign.lock().await;
            let live_ids: std::collections::HashSet<String> = {
                let config = self.inner.config.lock().await;
                config.accounts.iter().map(|a| a.id.clone()).collect()
            };
            let taken: std::collections::HashSet<&String> = assign
                .iter()
                .filter(|(id, _)| live_ids.contains(*id))
                .map(|(_, proxy)| proxy)
                .collect();
            let mut picked: Option<String> = None;
            for offset in 0..proxies.len() {
                let idx = (fallback_idx + offset) % proxies.len();
                let candidate = normalize_static_proxy(&proxies[idx]);
                if candidate.is_empty() || taken.contains(&candidate) {
                    continue;
                }
                picked = Some(candidate);
                break;
            }
            // IP 数量少于账号数量：允许复用，按序号取模
            picked.unwrap_or_else(|| normalize_static_proxy(&proxies[fallback_idx % proxies.len()]))
        };
        // 3) 写入映射（并发安全：再次检查，避免被抢先写入）
        let mut assign = self.inner.static_proxy_assign.lock().await;
        if let Some(existing) = assign.get(&account.id) {
            if !existing.is_empty() {
                return existing.clone();
            }
        }
        assign.insert(account.id.clone(), chosen.clone());
        chosen
    }

    pub async fn release_account_proxy(self: &Self, account_id: &str) {
        let mut assign = self.inner.account_proxy_assign.lock().await;
        // 只释放动态池的绑定；静态映射保存在 static_proxy_assign，不在此处清理。
        assign.remove(account_id);
    }

    /// 计算代理池上限（支持账号倍率 / 固定值两种模式）
    fn calc_pool_max_size(settings: &GlobalSettings, account_count: usize) -> usize {
        if settings.proxy_pool_limit_mode == "fixed" {
            settings.proxy_pool_fixed_limit.max(1)
        } else {
            (account_count * settings.proxy_pool_per_account).max(1)
        }
    }

    /// 当前代理池统计
    pub async fn proxy_pool_stats(self: &Self) -> ProxyPoolStats {
        let (enabled, account_count, max_size) = {
            let config = self.inner.config.lock().await;
            let account_count = config.accounts.iter().filter(|a| a.enabled).count();
            (
                config.settings.proxy_pool_enabled,
                account_count,
                Self::calc_pool_max_size(&config.settings, account_count),
            )
        };
        let _ = account_count;
        self.inner.proxy_pool.stats(max_size, enabled).await
    }

    /// 启动代理池维护循环：定期测活 + 补充
    pub async fn start_proxy_pool_keeper(&self) {
        {
            let mut handles = self.inner.handles.lock().await;
            if let Some(h) = handles.remove(PROXY_POOL_HANDLE_KEY) {
                h.abort();
            }
        }
        let state = self.clone();
        let handle = self.spawn_guarded("proxy_pool_keeper", async move {
            // 首次立即补充
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            loop {
                let s = {
                    let config = state.inner.config.lock().await;
                    config.settings.clone()
                };
                if !s.proxy_pool_enabled {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }

                let account_count = {
                    let config = state.inner.config.lock().await;
                    config.accounts.iter().filter(|a| a.enabled).count()
                };
                let max_size = Self::calc_pool_max_size(&s, account_count);

                // 测活
                let (alive, dead, probed, skipped) = state
                    .inner
                    .proxy_pool
                    .health_check_due(
                        &s.proxy_check_url,
                        s.http_timeout_ms,
                        s.proxy_max_age_ms,
                        s.proxy_fetch_concurrency,
                        // 0 = 自动取 max_age 的 1/3
                        s.proxy_refresh_before_ms.max(s.proxy_max_age_ms / 3),
                    )
                    .await;
                if dead > 0 || probed > 0 {
                    state
                        .log(
                            "batch",
                            "info",
                            format!(
                                "代理池按需测活：探测 {probed} 个 / 跳过 {skipped} 个 / 存活 {alive} 个 / 剔除 {dead} 个"
                            ),
                        )
                        .await;
                }

                let total = state.inner.proxy_pool.total_count().await;
                let alive = state.inner.proxy_pool.alive_count().await;
                // 已分配给账号、正在使用中的 IP 数量（这些 IP 已从池中移除）
                let in_use = state.inner.account_proxy_assign.lock().await.len();
                // 补充触发条件：把「使用中」也计入需求。
                // 100+ 账号并发时，若只看池内 alive，会出现「池里够用但账号拿不到 IP」
                // 的情况（因为 IP 已被取走），导致部分账号降级直连。
                let effective_available = alive + in_use;
                // 只有"可用数量"不足时才补充（无效 IP 占位不触发补充）
                if effective_available < max_size && total < max_size {
                    let need = max_size.saturating_sub(total).max(1);
                    let mut fetched: Vec<String> = Vec::new();
                    let mut last_err: Option<String> = None;
                    // num 参数：auto 模式按需计算，fixed 模式用配置值
                    let num_per_call = if s.proxy_num_mode == "fixed" {
                        s.proxy_num_fixed.max(1)
                    } else {
                        need.div_ceil(s.proxy_fetch_concurrency.max(1)).max(1)
                    };
                    // 并发提取：满足"短时间获取到上限"的需求
                    let mut tasks = tokio::task::JoinSet::new();
                    let fetch_proxy = s.proxy_pool_fetch_proxy.clone();
                    for _ in 0..s.proxy_fetch_concurrency.max(1) {
                        let api = s.proxy_pool_api.clone();
                        let fmt = s.proxy_pool_format.clone();
                        let num = num_per_call;
                        let fp = fetch_proxy.clone();
                        tasks.spawn(async move {
                            ProxyPool::fetch_into_pool_with_num(&api, &fmt, num, Some(num), &fp).await
                        });
                    }
                    while let Some(res) = tasks.join_next().await {
                        match res {
                            Ok(Ok(list)) => fetched.extend(list),
                            Ok(Err(e)) => last_err = Some(e),
                            Err(e) => last_err = Some(e.to_string()),
                        }
                    }
                    if !fetched.is_empty() {
                        let n = fetched.len();
                        state.inner.proxy_pool.add(fetched, max_size).await;
                        state.inner.proxy_pool.record_fetched(n).await;
                        state
                            .log(
                                "batch",
                                "info",
                                format!("代理池补充：新增 {n} 个 IP（池 {} / 上限 {max_size}）", state.inner.proxy_pool.total_count().await),
                            )
                            .await;
                    }
                    if let Some(e) = last_err {
                        state.inner.proxy_pool.record_fetch_error(e.clone()).await;
                        state.log("batch", "error", format!("代理池补充失败：{e}")).await;
                    }
                }

                tokio::time::sleep(std::time::Duration::from_millis(
                    s.proxy_check_interval_ms.max(1000),
                ))
                .await;
            }
        });
        self.inner
            .handles
            .lock()
            .await
            .insert(PROXY_POOL_HANDLE_KEY.to_string(), handle);
    }

    pub async fn start_session_keeper(&self) {
        {
            let mut handles = self.inner.handles.lock().await;
            if let Some(handle) = handles.remove(SESSION_KEEPER_HANDLE_KEY) {
                handle.abort();
            }
        }
        let state = self.clone();
        let handle = self.spawn_guarded("session_keeper", async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let (auto_refresh, accounts) = {
                    let config = state.inner.config.lock().await;
                    (
                        config.settings.auto_refresh_sessions,
                        config.accounts.clone(),
                    )
                };
                if !auto_refresh {
                    continue;
                }
                let batch_running = state.inner.batch_progress.lock().await.running;
                if batch_running {
                    continue;
                }

                let mut refresh_tasks = Vec::new();
                {
                    let prepared = state.inner.prepared_wechat_orders.lock().await;
                    let now_ms = Self::now_ms();
                    for account in &accounts {
                        if account.wechat_cookie.trim().is_empty() {
                            continue;
                        }
                        let Some(sessions) = prepared.get(&account.id) else {
                            continue;
                        };
                        if let Some((session_key, session)) =
                            sessions.iter().max_by_key(|(_, session)| {
                                session
                                    .get("created_at_ms")
                                    .and_then(Value::as_u64)
                                    .unwrap_or_default()
                            })
                        {
                            let created_at_ms = session
                                .get("created_at_ms")
                                .and_then(Value::as_u64)
                                .unwrap_or_default();
                            if now_ms.saturating_sub(created_at_ms) < SESSION_REFRESH_AGE_MS {
                                continue;
                            }
                            let Some((part, store_id)) = session_key.split_once('|') else {
                                continue;
                            };
                            if !account.parts.iter().any(|item| item == part)
                                || !account.stores.iter().any(|item| item == store_id)
                            {
                                continue;
                            }
                            refresh_tasks.push((
                                account.clone(),
                                session_key.clone(),
                                part.to_string(),
                                store_id.to_string(),
                            ));
                        }
                    }
                }

                if refresh_tasks.is_empty() {
                    continue;
                }

                state
                    .log(
                        "batch",
                        "info",
                        format!("会话自动续期开始：{} 个会话", refresh_tasks.len()),
                    )
                    .await;
                let concurrency = state
                    .inner
                    .config
                    .lock()
                    .await
                    .settings
                    .batch_prepare_concurrency
                    .clamp(1, 32);
                let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
                let mut tasks = tokio::task::JoinSet::new();
                for (account, session_key, part, store_id) in refresh_tasks {
                    let state = state.clone();
                    let semaphore = semaphore.clone();
                    let account_lock = state.account_lock(&account.id).await;
                    tasks.spawn(async move {
                        let _account_guard = account_lock.lock().await;
                        let _permit = semaphore
                            .acquire_owned()
                            .await
                            .expect("session keeper semaphore closed");
                        // 续期必须复用会话建立时的出口 IP（否则 403，反而把好会话清掉）
                        let renew_proxy = state
                            .resolve_session_proxy(&account.id, &session_key)
                            .await
                            .unwrap_or_else(|| account.proxy_url.clone());
                        let payload = json!({
                            "mode": "verify",
                            "cookie": account.wechat_cookie,
                            "part": part,
                            "store": store_id,
                            "debug": false,
                            "proxy": renew_proxy,
                        });
                        let result = run_wechat_bridge(payload).await;
                        if result.get("ok") == Some(&json!(true)) {
                            state
                                .persist_wechat_cookie_from_result(&account.id, &result)
                                .await;
                            state
                                .log(&account.id, "info", "会话续期探活通过（Cookie 已刷新）")
                                .await;
                        } else {
                            state
                                .log(
                                    &account.id,
                                    "error",
                                    format!(
                                        "会话自动续期失败：{}",
                                        result
                                            .get("error")
                                            .and_then(Value::as_str)
                                            .unwrap_or("未知错误")
                                    ),
                                )
                                .await;
                            state
                                .remove_wechat_session(&account.id, session_key.clone())
                                .await;
                        }
                    });
                }
                while tasks.join_next().await.is_some() {}
                state.log("batch", "info", "会话自动续期完成").await;
            }
        });
        self.inner
            .handles
            .lock()
            .await
            .insert(SESSION_KEEPER_HANDLE_KEY.to_string(), handle);
    }

    async fn run_global_monitor(self: &Self) {
        let mut settings = self.inner.config.lock().await.settings.clone();
        let order_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
            settings.order_concurrency.max(1),
        ));
        let direct_order_concurrency = settings.direct_order_concurrency.max(1);
        let direct_order_semaphore =
            std::sync::Arc::new(tokio::sync::Semaphore::new(direct_order_concurrency));
        let mut error_delay_ms: u64 = 0;
        let mut last_heartbeat_ms: u64 = 0;
        let mut last_found_log_ms: u64 = 0;
        let mut last_no_cookie_log_ms: HashMap<String, u64> = HashMap::new();

        self.log(
            "batch",
            "info",
            "中央库存监控启动：将查询独立配置的型号和门店组合，发现库存后匹配账号下单",
        )
        .await;

        // 启动时：先做纯内存统计，再后台校验会话在服务端的真实性
        self.sync_statuses().await;
        let (ready_sessions, total_accounts) = {
            // 锁顺序必须与 sync_statuses 一致（config → statuses），
            // 否则两条路径并发时会互相等待造成死锁。
            let config = self.inner.config.lock().await;
            let statuses = self.inner.statuses.lock().await;
            let total = config
                .accounts
                .iter()
                .filter(|a| a.enabled && !a.wechat_cookie.trim().is_empty())
                .count();
            let ready = config
                .accounts
                .iter()
                .filter(|a| a.enabled && !a.wechat_cookie.trim().is_empty())
                .filter(|a| {
                    statuses
                        .get(&a.id)
                        .map(|s| s.wechat_session_fresh_count > 0)
                        .unwrap_or(false)
                })
                .count();
            (ready, total)
        };
        self.log(
            "batch",
            "info",
            format!(
                "Session check: {}/{} accounts ready (in-memory). Verifying with Apple...",
                ready_sessions, total_accounts
            ),
        )
        .await;

        // 后台校验内存中的会话在 Apple 端是否仍然有效。
        // 失效的立即清除并重新准备，避免发现库存时才发现会话过期而浪费时间。
        {
            let verify_targets = {
                let prepared = self.inner.prepared_wechat_orders.lock().await;
                let config = self.inner.config.lock().await;
                config
                    .accounts
                    .iter()
                    .filter(|a| a.enabled && !a.wechat_cookie.trim().is_empty())
                    .filter(|a| {
                        prepared
                            .get(&a.id)
                            .map(|sessions| sessions.values().any(|s| !wechat_session_expired(s)))
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if !verify_targets.is_empty() {
                self.log(
                    "batch",
                    "info",
                    format!("会话真实性校验开始：{} 个账号", verify_targets.len()),
                )
                .await;
                let verify_state = self.clone();
                tokio::spawn(async move {
                    let concurrency = verify_state
                        .inner
                        .config
                        .lock()
                        .await
                        .settings
                        .batch_prepare_concurrency
                        .clamp(1, 32);
                    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
                    let mut tasks = tokio::task::JoinSet::new();
                    for account in verify_targets {
                        let semaphore = semaphore.clone();
                        let verify_state = verify_state.clone();
                        tasks.spawn(async move {
                            let _permit = semaphore
                                .acquire_owned()
                                .await
                                .expect("verify semaphore closed");
                            let (part, store_id) = {
                                let prepared = verify_state.inner.prepared_wechat_orders.lock().await;
                                match prepared
                                    .get(&account.id)
                                    .and_then(|sessions| sessions.keys().next())
                                {
                                    Some(key) => match key.split_once('|') {
                                        Some((p, st)) => (p.to_string(), st.to_string()),
                                        None => (
                                            account.parts.first().cloned().unwrap_or_default(),
                                            account.stores.first().cloned().unwrap_or_default(),
                                        ),
                                    },
                                    None => (
                                        account.parts.first().cloned().unwrap_or_default(),
                                        account.stores.first().cloned().unwrap_or_default(),
                                    ),
                                }
                            };
                            if part.is_empty() || store_id.is_empty() {
                                return;
                            }
                            // 启动校验只做「只读探活」：仅请求 fetchOpenId 判断 Cookie 是否有效。
                            // 绝不能在这里重跑 prepare！那会 add_cart / checkout_start / 选店 / 支付，
                            // 等于把用户刚预热好的会话状态破坏掉（实测会导致随后 place 403）。
                            let verify_proxy = {
                                let prepared = verify_state.inner.prepared_wechat_orders.lock().await;
                                prepared
                                    .get(&account.id)
                                    .and_then(|sessions| sessions.get(&format!("{}|{}", part, store_id)))
                                    .and_then(|session| {
                                        session
                                            .pointer("/result/proxy_url")
                                            .and_then(Value::as_str)
                                            .map(str::to_string)
                                    })
                                    .unwrap_or_else(|| account.proxy_url.clone())
                            };
                            let payload = json!({
                                "mode": "verify",
                                "cookie": account.wechat_cookie,
                                "part": part,
                                "store": store_id,
                                "debug": false,
                                "proxy": verify_proxy,
                            });
                            let result = run_wechat_bridge(payload).await;
                            let session_key = format!("{}|{}", part, store_id);
                            if result.get("ok") == Some(&json!(true)) {
                                verify_state
                                    .persist_wechat_cookie_from_result(&account.id, &result)
                                    .await;
                                verify_state
                                    .log(&account.id, "info", "会话探活通过（Cookie 有效，预热状态保持不变）")
                                    .await;
                            } else {
                                let msg = result
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("会话已失效")
                                    .to_string();
                                verify_state
                                    .remove_wechat_session(&account.id, session_key)
                                    .await;
                                verify_state
                                    .log(&account.id, "warn", format!("会话校验失败，已清除：{msg}"))
                                    .await;
                            }
                        });
                    }
                    while tasks.join_next().await.is_some() {}
                    verify_state.sync_statuses().await;
                    verify_state
                        .log("batch", "info", "会话真实性校验完成")
                        .await;
                });
            }
        }

        // 启动时输出监控范围的组合状态（/0 无库存，/1 有库存）
        {
            let mut scope_parts = settings.monitor_parts.clone();
            let mut scope_stores = settings.monitor_stores.clone();
            if scope_parts.is_empty() || scope_stores.is_empty() {
                let config = self.inner.config.lock().await;
                for account in &config.accounts {
                    if !account.enabled { continue; }
                    if scope_parts.is_empty() { scope_parts = account.parts.clone(); }
                    if scope_stores.is_empty() { scope_stores = account.stores.clone(); }
                }
            }
            if !scope_parts.is_empty() && !scope_stores.is_empty() {
                let combos: Vec<String> = scope_parts.iter()
                    .flat_map(|p| scope_stores.iter().map(move |s| format!("{}@{}/0", p, s)))
                    .collect();
                self.log("batch", "info", format!("监控范围（{} 个组合）：{}", combos.len(), combos.join("，"))).await;
            }
        }

        if settings.start_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(settings.start_delay_ms)).await;
        }

        loop {
            let circuit_open_until_ms = *self.inner.circuit_open_until_ms.lock().await;
            if Self::now_ms() < circuit_open_until_ms {
                let remaining_ms = circuit_open_until_ms - Self::now_ms();
                tokio::time::sleep(std::time::Duration::from_millis(remaining_ms.min(1000))).await;
                continue;
            }

            let started = std::time::Instant::now();
            let (accounts, current_settings) = {
                let config = self.inner.config.lock().await;
                (config.accounts.clone(), config.settings.clone())
            };
            settings = current_settings;
            let timeout_ms = settings.http_timeout_ms;
            let mut enabled_accounts = accounts
                .into_iter()
                .filter(|account| account.enabled)
                .collect::<Vec<_>>();
            let priority_scores = {
                let statuses = self.inner.statuses.lock().await;
                statuses
                    .values()
                    .map(|status| (status.id.clone(), status.priority_score))
                    .collect::<HashMap<_, _>>()
            };
            enabled_accounts.sort_by(|left, right| {
                let left_score = priority_scores.get(&left.id).cloned().unwrap_or(0);
                let right_score = priority_scores.get(&right.id).cloned().unwrap_or(0);
                right_score.cmp(&left_score)
            });
            if enabled_accounts.is_empty() {
                self.log("batch", "info", "中央库存监控停止：没有启用账号")
                    .await;
                let mut statuses = self.inner.statuses.lock().await;
                for status in statuses.values_mut() {
                    status.running = false;
                    status.status = "stopped".to_string();
                }
                drop(statuses);
                self.inner
                    .handles
                    .lock()
                    .await
                    .remove(GLOBAL_MONITOR_HANDLE_KEY);
                return;
            }

            let mut parts_set = settings
                .monitor_parts
                .iter()
                .cloned()
                .collect::<HashSet<_>>();
            let mut stores_set = settings
                .monitor_stores
                .iter()
                .cloned()
                .collect::<HashSet<_>>();
            if parts_set.is_empty() || stores_set.is_empty() {
                for account in &enabled_accounts {
                    if parts_set.is_empty() {
                        parts_set.extend(account.parts.iter().cloned());
                    }
                    if stores_set.is_empty() {
                        stores_set.extend(account.stores.iter().cloned());
                    }
                }
            }
            let parts = parts_set.into_iter().collect::<Vec<_>>();
            let stores = stores_set.into_iter().collect::<Vec<_>>();
            let client = match AppleClient::new_with_resolve_ip("", timeout_ms, APPLE_CDN_IPS[0]) {
                Ok(client) => client,
                Err(err) => {
                    self.log(
                        "batch",
                        "error",
                        format!("创建中央库存监控客户端失败：{err}"),
                    )
                    .await;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        error_delay_ms.saturating_mul(2).max(5_000).min(30_000),
                    ))
                    .await;
                    continue;
                }
            };

            match client.stock_details_multi(&parts, &stores).await {
                Ok(stock_by_part) => {
                    error_delay_ms = 0;
                    *self.inner.circuit_failure_count.lock().await = 0;
                    *self.inner.circuit_open_until_ms.lock().await = 0;
                    let latency = started.elapsed().as_millis();
                    *self.inner.monitor_last_latency_ms.lock().await = latency;
                    for (account_id, status) in self.inner.statuses.lock().await.iter_mut() {
                        let _ = account_id;
                        status.last_latency_ms = latency;
                        status.last_poll = Some(Self::now_ms());
                        status.last_error = None;
                    }

                    let available_combos = parts
                        .iter()
                        .filter_map(|part| {
                            stock_by_part.get(part).map(|stores| {
                                stores
                                    .iter()
                                    .filter(|store| {
                                        store.get("status").and_then(Value::as_str)
                                            == Some("available")
                                    })
                                    .filter_map(|store| {
                                        store
                                            .get("store_id")
                                            .and_then(Value::as_str)
                                            .map(|store_id| (part.clone(), store_id.to_string()))
                                    })
                                    .collect::<Vec<_>>()
                            })
                        })
                        .flatten()
                        .collect::<Vec<_>>();
                    *self.inner.monitor_available_combos.lock().await = available_combos.len();
                    let found_any = !available_combos.is_empty();
                    if found_any {
                        if last_found_log_ms == 0
                            || Self::now_ms().saturating_sub(last_found_log_ms) >= 30_000
                        {
                            last_found_log_ms = Self::now_ms();
                            let matched_account_count = enabled_accounts
                                .iter()
                                .filter(|account| {
                                    available_combos.iter().any(|(part, store)| {
                                        account.parts.iter().any(|item| item == part)
                                            && account.stores.iter().any(|item| item == store)
                                    })
                                })
                                .count();
                            let combo_text = available_combos
                                .iter()
                                .map(|(part, store)| format!("{part}@{store}/1"))
                                .collect::<Vec<_>>()
                                .join("、");
                            self.log(
                                "batch",
                                if matched_account_count == 0 {
                                    "error"
                                } else {
                                    "info"
                                },
                                format!(
                                    "中央监控发现 {} 个有货组合，匹配账号 {} 个；组合：{}",
                                    available_combos.len(),
                                    matched_account_count,
                                    combo_text
                                ),
                            )
                            .await;
                        }
                    }

                    for account in &enabled_accounts {
                        if settings.retry_enabled {
                            let retry_after_ms = self
                                .inner
                                .account_retry_after_ms
                                .lock()
                                .await
                                .get(&account.id)
                                .cloned();
                            if let Some(retry_after_ms) = retry_after_ms {
                                if Self::now_ms() < retry_after_ms {
                                    continue;
                                }
                            }
                        }
                        let should_skip = {
                            let active = self.inner.central_ordering_accounts.lock().await;
                            let completed = self.inner.central_completed_accounts.lock().await;
                            active.contains(&account.id) || completed.contains(&account.id)
                        };
                        if should_skip {
                            continue;
                        }
                        let batch_running = self.inner.batch_progress.lock().await.running;
                        if batch_running {
                            continue;
                        }
                        let prepared_keys = {
                            let prepared = self.inner.prepared_wechat_orders.lock().await;
                            prepared
                                .get(&account.id)
                                .map(|sessions| {
                                    sessions
                                        .iter()
                                        .filter(|(_, session)| !wechat_session_expired(session))
                                        .map(|(key, _)| key.clone())
                                        .collect::<HashSet<_>>()
                                })
                                .unwrap_or_default()
                        };
                        let selected = account.parts.iter().find_map(|part| {
                            account.stores.iter().find_map(|store| {
                                let key = format!("{}|{}", part, store);
                                if !prepared_keys.contains(&key) {
                                    return None;
                                }
                                available_combos
                                    .iter()
                                    .find(|combo| combo.0 == *part && combo.1 == *store)
                                    .map(|combo| combo.clone())
                            })
                        });
                        let selected = selected.or_else(|| {
                            account.parts.iter().find_map(|part| {
                                account.stores.iter().find_map(|store| {
                                    available_combos
                                        .iter()
                                        .find(|combo| combo.0 == *part && combo.1 == *store)
                                        .map(|combo| combo.clone())
                                })
                            })
                        });
                        let Some((dispatch_part, dispatch_store)) = selected else {
                            continue;
                        };
                        let prepared_state = {
                            let prepared = self.inner.prepared_wechat_orders.lock().await;
                            prepared
                                .get(&account.id)
                                .and_then(|sessions| {
                                    sessions.get(&format!("{}|{}", dispatch_part, dispatch_store))
                                })
                                .cloned()
                        };
                        // 只有「完整预热」的会话才能走 place 快速通道：
                        // 它必须已经进入结账页（checkout_ready）并带 line_item_id，
                        // 否则是「无货时的降级预热」（仅清空购物袋 + 缓存 OpenID），
                        // 直接 place 必然失败，必须回退全流程。
                        let use_prepared_session = prepared_state.as_ref().is_some_and(|session| {
                            !wechat_session_expired(session)
                                && session
                                    .pointer("/result/checkout_ready")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                        });
                        let dispatch_account_id = account.id.clone();
                        let has_wechat_cookie = !account.wechat_cookie.trim().is_empty();
                        if !has_wechat_cookie {
                            let now_ms = Self::now_ms();
                            if now_ms.saturating_sub(
                                *last_no_cookie_log_ms.get(&account.id).unwrap_or(&0),
                            ) >= 30_000
                            {
                                let message =
                                    "中央监控匹配到库存，但账号缺少微信小程序 Cookie 或网页登录 Cookie，已跳过下单"
                                        .to_string();
                                self.log(&account.id, "error", message.clone()).await;
                                self.update_status(&account.id, |status| {
                                    status.status = "error".to_string();
                                    status.last_error = Some(message);
                                })
                                .await;
                                last_no_cookie_log_ms.insert(account.id.clone(), now_ms);
                            }
                            continue;
                        }
                        self.inner
                            .central_ordering_accounts
                            .lock()
                            .await
                            .insert(account.id.clone());
                        self.update_status(&dispatch_account_id, |status| {
                            status.status = "ordering".to_string();
                        })
                        .await;
                        self.log(
                            &account.id,
                            "info",
                            format!(
                                "中央监控为账号选择下单组合：{} · {} · {}",
                                dispatch_part,
                                dispatch_store,
                                if use_prepared_session {
                                    "使用准备会话"
                                } else {
                                    "使用全流程下单"
                                }
                            ),
                        )
                        .await;
                        // 解析该账号使用的代理：
                        //   1) 若命中「已预准备会话」，必须复用该会话建立时的出口 IP，
                        //      否则 Apple 会判定会话失效（403），预热就白做了；
                        //   2) 否则按 账号自身 > 静态 IP > 动态池 解析（动态池取出即消耗）。
                        // 会话记录里的 proxy_url 有三种语义：
                        //   - 非空字符串：会话是在该出口 IP 下建立的，下单必须复用；
                        //   - 空字符串  ：会话是直连建立的，下单必须直连，不能换成代理 IP；
                        //   - 字段缺失  ：老数据，无法判断，回退账号代理配置。
                        let session_proxy = prepared_state.as_ref().and_then(|session| {
                            session
                                .pointer("/result/proxy_url")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| {
                                    session.get("proxy_url").and_then(Value::as_str).map(str::to_string)
                                })
                        });
                        let resolved_proxy = if use_prepared_session {
                            match session_proxy {
                                // 会话记录了出口 IP：必须复用（含空字符串 = 直连）
                                Some(proxy) => proxy,
                                // 老数据无记录：只能回退账号代理配置
                                None => {
                                    self.resolve_account_proxy(account)
                                        .await
                                        .unwrap_or_default()
                                }
                            }
                        } else {
                            self.resolve_account_proxy(account)
                                .await
                                .unwrap_or_default()
                        };
                        if has_wechat_cookie {
                            let cleanup_state = self.clone();
                            let cleanup_account_id = dispatch_account_id.clone();
                            let dispatch_username = account.username.clone();
                            let dispatch_cookie = account.wechat_cookie.clone();
                            // 可变：541 时从代理池换一个 IP 重试
                            let mut dispatch_proxy = resolved_proxy.clone();
                            // 是否使用动态池（池中的 IP 用完即弃，可换新）
                            let proxy_from_pool = {
                                // 注意：此处仍在 spawn 外，可用 self
                                let s = self.inner.config.lock().await.settings.clone();
                                s.proxy_pool_enabled
                                    && account.proxy_url.trim().is_empty()
                                    && account.proxy_urls.is_empty()
                            };
                            let dispatch_contact = account.contact.clone();
                            let order_semaphore = order_semaphore.clone();
                            let direct_order_semaphore = direct_order_semaphore.clone();
                            let retry_enabled = settings.retry_enabled;
                            let retry_base_delay_ms = settings.retry_base_delay_ms;
                            let retry_max_attempts = settings.retry_max_attempts;
                            let prepared_result = if use_prepared_session {
                                prepared_state
                                    .as_ref()
                                    .and_then(|session| session.get("result").cloned())
                                    .unwrap_or(json!({}))
                            } else {
                                json!({})
                            };
                            let account_lock = self.account_lock(&account.id).await;
                            *self.inner.monitor_dispatched_count.lock().await += 1;
                            tokio::spawn(async move {
                                let _account_guard = account_lock.lock().await;
                                cleanup_state
                                    .log(
                                        &cleanup_account_id,
                                        "info",
                                        format!(
                                            "中央监控下单开始：{} · {} · {}",
                                            dispatch_part,
                                            dispatch_store,
                                            if use_prepared_session {
                                                "准备会话"
                                            } else {
                                                "全流程"
                                            }
                                        ),
                                    )
                                    .await;
                                // 明确输出本次下单使用的出口（脱敏，隐藏账号密码）
                                cleanup_state
                                    .log(
                                        &cleanup_account_id,
                                        "info",
                                        format!(
                                            "本次下单出口代理：{}",
                                            display_proxy(&dispatch_proxy)
                                        ),
                                    )
                                    .await;
                                let _order_permit = order_semaphore
                                    .acquire_owned()
                                    .await
                                    .expect("order semaphore closed");
                                let _direct_order_permit = if dispatch_proxy.trim().is_empty() {
                                    Some(
                                        direct_order_semaphore
                                            .acquire_owned()
                                            .await
                                            .expect("direct order semaphore closed"),
                                    )
                                } else {
                                    None
                                };
                                let order_started_at_ms = std::time::Instant::now();
                                let bridge_payload = if use_prepared_session {
                                    json!({
                                        "mode": "place",
                                        "state": prepared_result,
                                        "cookie": dispatch_cookie,
                                        "part": dispatch_part,
                                        "store": dispatch_store,
                                        "debug": false,
                                        "proxy": dispatch_proxy.clone(),
                                    })
                                } else {
                                    json!({
                                        "mode": "full",
                                        "cookie": dispatch_cookie,
                                        "part": dispatch_part,
                                        "store": dispatch_store,
                                        "place": true,
                                        "clear": true,
                                        "proxy": dispatch_proxy.clone(),
                                        "fast": true,
                                        "contact": {
                                                "lastName": dispatch_contact.last_name,
                                                "firstName": dispatch_contact.first_name,
                                                "emailAddress": dispatch_contact.email,
                                                "fullDaytimePhone": dispatch_contact.phone,
                                                "nationalId": dispatch_contact.national_id_suffix,
                                            },
                                        "debug": false,
                                    })
                                };
                                let mut result = run_wechat_bridge(bridge_payload.clone()).await;
                                // 代理池模式：下单失败（限流/网络错误）时，每次失败都重新从池中取一个新 IP 重试，最多 2 次
                                let mut proxy_retries = 0u32;
                                while result.get("ok") != Some(&json!(true))
                                    && proxy_from_pool
                                    && proxy_retries < 2
                                    && result
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .map(is_proxy_retryable_order_error)
                                        .unwrap_or(false)
                                {
                                    let Some(next) =
                                        cleanup_state.inner.proxy_pool.take_one().await
                                    else {
                                        cleanup_state
                                            .log(
                                                &cleanup_account_id,
                                                "warn",
                                                "下单失败需换 IP，但代理池暂无已测活 IP，停止重试",
                                            )
                                            .await;
                                        break;
                                    };
                                    let failure_hint = result
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .chars()
                                        .take(80)
                                        .collect::<String>();
                                    cleanup_state
                                        .log(
                                            &cleanup_account_id,
                                            "info",
                                            format!(
                                                "下单失败（{failure_hint}），第 {} 次切换代理重试：{}",
                                                proxy_retries + 1,
                                                display_proxy(&next),
                                            ),
                                        )
                                        .await;
                                    dispatch_proxy = next.clone();
                                    let mut retry_payload = bridge_payload.clone();
                                    if let Some(obj) = retry_payload.as_object_mut() {
                                        obj.insert("proxy".to_string(), json!(next));
                                    }
                                    result = run_wechat_bridge(retry_payload).await;
                                    proxy_retries += 1;
                                }
                                if result.get("ok") != Some(&json!(true))
                                    && use_prepared_session
                                    && is_retryable_wechat_session_error(&result)
                                {
                                    cleanup_state
                                        .set_cookie_health(&cleanup_account_id, "invalid")
                                        .await;
                                    cleanup_state
                                        .remove_wechat_session(
                                            &cleanup_account_id,
                                            format!("{}|{}", dispatch_part, dispatch_store),
                                        )
                                        .await;
                                    cleanup_state
                                        .log(
                                            &cleanup_account_id,
                                            "info",
                                            "预准备会话失效（403/过期），自动重建购物车并回退全流程",
                                        )
                                        .await;
                                    result = run_wechat_bridge(json!({
                                        "mode": "full",
                                        "cookie": dispatch_cookie,
                                        "part": dispatch_part,
                                        "store": dispatch_store,
                                        "place": true,
                                        "clear": true,
                                        "proxy": dispatch_proxy.clone(),
                                        "fast": true,
                                        "contact": {
                                                "lastName": dispatch_contact.last_name,
                                                "firstName": dispatch_contact.first_name,
                                                "emailAddress": dispatch_contact.email,
                                                "fullDaytimePhone": dispatch_contact.phone,
                                                "nationalId": dispatch_contact.national_id_suffix,
                                            },
                                        "debug": false,
                                    }))
                                    .await;
                                }
                                let account_id = cleanup_account_id;
                                if result.get("ok") == Some(&json!(true)) {
                                    cleanup_state
                                        .persist_wechat_cookie_from_result(&account_id, &result)
                                        .await;
                                    let order_number = result
                                        .get("summary")
                                        .and_then(|summary| summary.get("order_number"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    let payment_link = result
                                        .get("summary")
                                        .and_then(|summary| summary.get("order_url"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    cleanup_state
                                        .update_status(&account_id, |status| {
                                            status.status = "order placed".to_string();
                                            status.orders_placed += 1;
                                            if !order_number.is_empty() {
                                                status.last_order_number =
                                                    Some(order_number.clone());
                                            }
                                            if !payment_link.is_empty() {
                                                status.last_payment_link =
                                                    Some(payment_link.clone());
                                            }
                                            status.last_error = None;
                                        })
                                        .await;
                                    cleanup_state
                                        .persist_placed_order(
                                            &account_id,
                                            order_number.clone(),
                                            payment_link.clone(),
                                        )
                                        .await;
                                    let used_session_key =
                                        format!("{}|{}", dispatch_part, dispatch_store);
                                    cleanup_state
                                        .remove_wechat_session(&account_id, used_session_key)
                                        .await;
                                    cleanup_state.set_cookie_health(&account_id, "valid").await;
                                    let elapsed_ms = order_started_at_ms.elapsed().as_millis();
                                    cleanup_state
                                        .record_order(
                                            &account_id,
                                            &dispatch_username,
                                            &order_number,
                                            &dispatch_part,
                                            &dispatch_store,
                                            result
                                                .pointer("/summary/pickup_date")
                                                .and_then(Value::as_str)
                                                .unwrap_or(""),
                                            result
                                                .pointer("/summary/pickup_time")
                                                .and_then(Value::as_str)
                                                .unwrap_or(""),
                                            &payment_link,
                                            elapsed_ms as u64,
                                        )
                                        .await;
                                    let success_text = wechat_order_success_text(&result);
                                    let success_text = format!(
                                        "{success_text} · 总下单耗时 {:.3}s",
                                        elapsed_ms as f64 / 1000.0
                                    );
                                    cleanup_state.log(&account_id, "info", success_text).await;
                                    if let Some(steps) =
                                        result.get("steps").and_then(Value::as_array)
                                    {
                                        let summary = summarize_wechat_steps(steps);
                                        cleanup_state
                                            .log(
                                                &account_id,
                                                "info",
                                                format!("下单步骤汇总：{summary}"),
                                            )
                                            .await;
                                    }
                                    cleanup_state
                                        .inner
                                        .central_completed_accounts
                                        .lock()
                                        .await
                                        .insert(account_id.clone());
                                    cleanup_state
                                        .record_order_success(&account_id, &dispatch_proxy)
                                        .await;
                                        cleanup_state.release_account_proxy(&account_id).await;
                                } else {
                                    let error = result
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("微信小程序下单失败")
                                        .to_string();
                                    let error_class = classify_wechat_error(&error);
                                    cleanup_state
                                        .set_cookie_health(
                                            &account_id,
                                            if error.contains("403") {
                                                "invalid"
                                            } else if error.contains("541") {
                                                "rate_limited"
                                            } else {
                                                "valid"
                                            },
                                        )
                                        .await;
                                    cleanup_state
                                        .log(
                                            &account_id,
                                            "error",
                                            format!(
                                                "中央监控下单失败：{error} · 分类：{error_class}"
                                            ),
                                        )
                                        .await;
                                    if let Some(steps) =
                                        result.get("steps").and_then(Value::as_array)
                                    {
                                        let summary = summarize_wechat_steps(steps);
                                        cleanup_state
                                            .log(
                                                &account_id,
                                                "info",
                                                format!("中央监控步骤汇总：{summary}"),
                                            )
                                            .await;
                                    }
                                    cleanup_state
                                        .update_status(&account_id, |status| {
                                            status.status = "error".to_string();
                                            status.last_error = Some(error.clone());
                                        })
                                        .await;
                                    if retry_enabled {
                                        cleanup_state
                                            .record_order_failure(
                                                &account_id,
                                                &dispatch_proxy,
                                                error,
                                                retry_base_delay_ms,
                                                retry_max_attempts,
                                            )
                                            .await;
                                    }
                                    if proxy_from_pool {
                                        // 池内 IP 用完即弃：下单失败后释放分配，下次立即取新 IP
                                        cleanup_state
                                            .release_account_proxy(&account_id)
                                            .await;
                                    }
                                }
                                cleanup_state
                                    .inner
                                    .central_ordering_accounts
                                    .lock()
                                    .await
                                    .remove(&account_id);
                            });
                        } else {
                            self.inner
                                .central_ordering_accounts
                                .lock()
                                .await
                                .remove(&dispatch_account_id);
                        }
                    }

                    if last_heartbeat_ms == 0
                        || Self::now_ms().saturating_sub(last_heartbeat_ms) >= 30_000
                    {
                        last_heartbeat_ms = Self::now_ms();
                        self.log(
                            "batch",
                            "info",
                            format!(
                            "中央监控心跳：{}（型号 {} 个 / 门店 {} 家 / 账号 {} 个；轮询 {}ms）",
                            if found_any { "存在已派发的库存" } else { "暂无新库存" },
                                parts.len(),
                                stores.len(),
                                enabled_accounts.len(),
                                settings.poll_delay_ms
                            ),
                        )
                        .await;
                    }
                }
                Err(err) => {
                    let message = format!("中央库存监控查询失败：{err}");
                    self.log("batch", "error", message.clone()).await;
                    for account in &enabled_accounts {
                        self.update_status(&account.id, |status| {
                            status.last_error = Some(message.clone());
                        })
                        .await;
                    }
                    error_delay_ms = error_delay_ms.saturating_mul(2).max(5_000).min(30_000);
                    let mut failures = self.inner.circuit_failure_count.lock().await;
                    *failures += 1;
                    if *failures >= settings.circuit_breaker_threshold {
                        let cooldown_ms = settings.circuit_breaker_cooldown_ms;
                        *self.inner.circuit_open_until_ms.lock().await =
                            Self::now_ms().saturating_add(cooldown_ms);
                        self.log("batch", "error", format!("全局熔断开启：{cooldown_ms}ms"))
                            .await;
                        *failures = 0;
                    }
                }
            }

            let jitter = rand::thread_rng().gen_range(0..=settings.jitter_ms.max(1));
            tokio::time::sleep(std::time::Duration::from_millis(
                settings.poll_delay_ms + jitter,
            ))
            .await;
        }
    }

    async fn update_status<F>(&self, account_id: &str, update: F)
    where
        F: FnOnce(&mut AccountStatus),
    {
        let mut statuses = self.inner.statuses.lock().await;
        if let Some(status) = statuses.get_mut(account_id) {
            update(status);
        }
    }

    async fn run_account_multi(self: &Self, config: AccountConfig) {
        let account_id = config.id.clone();
        let poll_delay = config.poll_delay_ms;
        let settings = self.inner.config.lock().await.settings.clone();
        let mut error_delay_ms: u64 = 0;
        let mut consecutive_541_count = 0u32;
        let mut last_heartbeat_ms: u64 = 0;
        let cdn_ip_index = 0usize;
        let has_duo_parts = config.parts.iter().any(|part| part.starts_with("MK2"));
        let batch_size = if has_duo_parts { 2 } else { 3 };
        let part_batches: Vec<Vec<String>> = config
            .parts
            .chunks(batch_size)
            .map(|chunk| chunk.to_vec())
            .collect();
        let mut part_batch_index = 0usize;
        // 库存监控（本函数）始终直连，不使用任何代理；代理仅用于下单链路。

        // 库存监控使用直连（不受代理影响，避免代理抖动导致误判）
        let mut stock_client = match AppleClient::new_with_resolve_ip(
            "",
            settings.http_timeout_ms,
            APPLE_CDN_IPS[cdn_ip_index],
        ) {
            Ok(client) => client,
            Err(err) => {
                let message = format!("创建网络客户端失败：{err}");
                self.update_status(&account_id, |status| {
                    status.status = "error".to_string();
                    status.last_error = Some(message.clone());
                })
                .await;
                self.log(&account_id, "error", message.clone()).await;
                return;
            }
        };
        self.update_status(&account_id, |status| status.status = "polling".to_string())
            .await;
        self.log(&account_id, "info", "会话预热完成，开始监控库存")
            .await;
        if settings.start_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(settings.start_delay_ms)).await;
        }

        loop {
            let started = std::time::Instant::now();
            let mut available: Option<(String, String, String)> = None;
            let mut last_error: Option<String> = None;
            let current_parts = part_batches[part_batch_index].clone();

            match stock_client
                .stock_details_multi(&current_parts, &config.stores)
                .await
            {
                Ok(stock_by_part) => {
                    for part in config.parts.clone() {
                        let Some(stores) = stock_by_part.get(&part) else {
                            continue;
                        };
                        let found = stores.iter().find(|store| {
                            store.get("status").and_then(Value::as_str) == Some("available")
                                && store
                                    .get("store_id")
                                    .and_then(Value::as_str)
                                    .map(|store_id| {
                                        config.stores.iter().any(|selected| selected == store_id)
                                    })
                                    .unwrap_or(false)
                        });
                        if let Some(store) = found {
                            available = Some((
                                part.clone(),
                                store
                                    .get("store_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                store
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("未知门店")
                                    .to_string(),
                            ));
                            break;
                        }
                    }
                }
                Err(err) => {
                    last_error = Some(format!("库存查询失败：{err}"));
                }
            }

            if let Some((part, store_id, store_name)) = available {
                last_heartbeat_ms = 0;
                let latency = started.elapsed().as_millis();
                self.update_status(&account_id, |status| {
                    status.status = "ordering".to_string();
                    status.last_latency_ms = latency;
                    status.last_poll = Some(Self::now_ms());
                    status.last_error = None;
                })
                .await;
                self.log(
                    &account_id,
                    "info",
                    format!("发现库存：{store_name}（{store_id}）· 料号 {part}，开始下单"),
                )
                .await;

                let mut prepared_state = {
                    let prepared = self.inner.prepared_wechat_orders.lock().await;
                    prepared
                        .get(&account_id)
                        .and_then(|sessions| sessions.get(&format!("{}|{}", part, store_id)))
                        .cloned()
                };
                if prepared_state
                    .as_ref()
                    .is_some_and(|session| wechat_session_expired(session))
                {
                    self.log(
                        &account_id,
                        "info",
                        format!(
                            "微信小程序预下单会话已超过 30 分钟，自动重新准备：{}|{}",
                            part, store_id
                        ),
                    )
                    .await;
                    prepared_state = None;
                }
                if let Some(prepared_state) = prepared_state {
                    let bridge_payload = json!({
                        "mode": "place",
                        "state": prepared_state.get("result").cloned().unwrap_or(json!({})),
                        "cookie": config.wechat_cookie,
                        "part": part,
                        "store": store_id,
                        "debug": false,
                    });
                    let mut result = run_wechat_bridge(bridge_payload).await;
                    if result.get("ok") != Some(&json!(true))
                        && is_retryable_wechat_session_error(&result)
                    {
                        self.log(
                            &account_id,
                            "info",
                            "预下单会话疑似失效，自动重新准备并重试一次",
                        )
                        .await;
                        let contact = json!({
                            "lastName": config.contact.last_name,
                            "firstName": config.contact.first_name,
                            "emailAddress": config.contact.email,
                            "fullDaytimePhone": config.contact.phone,
                            "nationalId": config.contact.national_id_suffix,
                        });
                        let prepare_payload = json!({
                            "mode": "prepare",
                            "cookie": config.wechat_cookie,
                            "part": part,
                            "store": store_id,
                            "contact": contact,
                            "proxy": config.proxy_url.clone(),
                            "clear": true,
                            "debug": false,
                        });
                        let retry_session = run_wechat_bridge(prepare_payload).await;
                        if retry_session.get("ok") == Some(&json!(true)) {
                            let session_key = format!("{}|{}", part, store_id);
                            self.store_wechat_session(
                                &account_id,
                                session_key,
                                retry_session.clone(),
                            )
                            .await;
                            let retry_payload = json!({
                                "mode": "place",
                                "state": retry_session.get("result").cloned().unwrap_or(json!({})),
                                "cookie": config.wechat_cookie,
                                "part": part,
                                "store": store_id,
                                "debug": false,
                            });
                            result = run_wechat_bridge(retry_payload).await;
                        }
                    }
                    let ok = result.get("ok") == Some(&json!(true));
                    if ok {
                        self.log(&account_id, "info", "微信小程序预下单会话提交成功")
                            .await;
                        self.update_status(&account_id, |status| {
                            status.status = "order placed".to_string();
                            status.orders_placed += 1;
                            status.last_payment_link = result
                                .get("summary")
                                .and_then(|s| s.get("order_url"))
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            status.last_error = None;
                        })
                        .await;
                        let order_number = result
                            .get("summary")
                            .and_then(|s| s.get("order_number"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let payment_link = result
                            .get("summary")
                            .and_then(|s| s.get("order_url"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let pickup_date = result
                            .pointer("/summary/pickup_date")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let pickup_time = result
                            .pointer("/summary/pickup_time")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.persist_placed_order(&account_id, order_number.clone(), payment_link.clone())
                            .await;
                        self.record_order(
                            &account_id,
                            &config.username,
                            &order_number,
                            &part,
                            &store_id,
                            pickup_date,
                            pickup_time,
                            &payment_link,
                            latency as u64,
                        )
                        .await;
                        if settings.stop_after_success {
                            break;
                        }
                    } else {
                        let message = result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("微信小程序预下单会话提交失败");
                        self.log(&account_id, "error", message.to_string()).await;
                        self.update_status(&account_id, |status| {
                            status.status = "error".to_string();
                            status.last_error = Some(message.to_string());
                        })
                        .await;
                    }
                    continue;
                }

                if !config.wechat_cookie.trim().is_empty() {
                    self.log(
                        &account_id,
                        "info",
                        format!(
                            "未找到匹配的预下单会话（{}|{}），现场准备小程序会话",
                            part, store_id
                        ),
                    )
                    .await;
                    let contact = json!({
                        "lastName": config.contact.last_name,
                        "firstName": config.contact.first_name,
                        "emailAddress": config.contact.email,
                        "fullDaytimePhone": config.contact.phone,
                        "nationalId": config.contact.national_id_suffix,
                    });
                    let prepare_payload = json!({
                        "mode": "prepare",
                        "cookie": config.wechat_cookie,
                        "part": part,
                        "store": store_id,
                        "contact": contact,
                        "proxy": config.proxy_url.clone(),
                        "clear": true,
                        "debug": false,
                    });
                    let prepare_result = run_wechat_bridge(prepare_payload).await;
                    if prepare_result.get("ok") == Some(&json!(true)) {
                        let session_key = format!("{}|{}", part, store_id);
                        self.store_wechat_session(&account_id, session_key, prepare_result.clone())
                            .await;
                        let bridge_payload = json!({
                            "mode": "place",
                            "state": prepare_result.get("result").cloned().unwrap_or(json!({})),
                            "cookie": config.wechat_cookie,
                            "part": part,
                            "store": store_id,
                            "debug": false,
                        });
                        let result = run_wechat_bridge(bridge_payload).await;
                        let ok = result.get("ok") == Some(&json!(true));
                        if ok {
                            self.log(&account_id, "info", "微信小程序现场准备并提交成功")
                                .await;
                            self.update_status(&account_id, |status| {
                                status.status = "order placed".to_string();
                                status.orders_placed += 1;
                                status.last_payment_link = result
                                    .get("summary")
                                    .and_then(|s| s.get("order_url"))
                                    .and_then(Value::as_str)
                                    .map(str::to_string);
                                status.last_error = None;
                            })
                            .await;
                            let order_number = result
                                .get("summary")
                                .and_then(|s| s.get("order_number"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let payment_link = result
                                .get("summary")
                                .and_then(|s| s.get("order_url"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let pickup_date = result
                                .pointer("/summary/pickup_date")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let pickup_time = result
                                .pointer("/summary/pickup_time")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            self.persist_placed_order(&account_id, order_number.clone(), payment_link.clone())
                                .await;
                            self.record_order(
                                &account_id,
                                &config.username,
                                &order_number,
                                &part,
                                &store_id,
                                pickup_date,
                                pickup_time,
                                &payment_link,
                                latency as u64,
                            )
                            .await;
                            if settings.stop_after_success {
                                break;
                            }
                        } else {
                            let message = result
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("微信小程序现场准备后提交失败");
                            self.log(&account_id, "error", message.to_string()).await;
                            self.update_status(&account_id, |status| {
                                status.status = "error".to_string();
                                status.last_error = Some(message.to_string());
                            })
                            .await;
                        }
                        continue;
                    } else {
                        let message = prepare_result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("微信小程序现场准备会话失败");
                        self.log(&account_id, "error", message.to_string()).await;
                        self.update_status(&account_id, |status| {
                            status.status = "error".to_string();
                            status.last_error = Some(message.to_string());
                        })
                        .await;
                        continue;
                    }
                }

                self.log(
                    &account_id,
                    "info",
                    format!(
                        "未找到匹配的预下单会话（{}|{}），回退网页下单链路",
                        part, store_id
                    ),
                )
                .await;

                let mut order_config = config.clone();
                order_config.part = part;
                order_config.store_id = store_id;
                let mut order_client =
                    match AppleClient::new(&order_config.proxy_url, settings.http_timeout_ms) {
                        Ok(client) => client,
                        Err(err) => {
                            let message = format!("创建下单客户端失败：{err}");
                            self.update_status(&account_id, |status| {
                                status.status = "error".to_string();
                                status.last_error = Some(message.clone());
                            })
                            .await;
                            self.log(&account_id, "error", message).await;
                            error_delay_ms = error_delay_ms.saturating_mul(2).max(2000).min(10_000);
                            continue;
                        }
                    };
                order_client.set_login_cookies(&order_config.login_cookies);
                if let Err(err) = order_client.bootstrap(&order_config).await {
                    let message = format!("下单前加购失败：{err}");
                    self.update_status(&account_id, |status| {
                        status.status = "error".to_string();
                        status.last_error = Some(message.clone());
                    })
                    .await;
                    self.log(&account_id, "error", message).await;
                    error_delay_ms = error_delay_ms.saturating_mul(2).max(2000).min(10_000);
                    continue;
                }

                match order_client.checkout(&order_config, &settings).await {
                    Ok(payment_link) => {
                        self.log(
                            &account_id,
                            "info",
                            format!("下单成功，支付链接：{payment_link}"),
                        )
                        .await;
                        self.update_status(&account_id, |status| {
                            status.status = "order placed".to_string();
                            status.orders_placed += 1;
                            status.last_payment_link = Some(payment_link);
                            status.last_error = None;
                        })
                        .await;
                        error_delay_ms = 0;
                        consecutive_541_count = 0;
                        if settings.stop_after_success {
                            break;
                        }
                    }
                    Err(err) => {
                        let message =
                            format!("网页下单失败：{}", clean_order_error(&err.to_string()));
                        self.update_status(&account_id, |status| {
                            status.status = "error".to_string();
                            status.last_error = Some(message.clone());
                        })
                        .await;
                        self.log(&account_id, "error", message).await;
                        error_delay_ms = error_delay_ms.saturating_mul(2).max(2000).min(10_000);
                    }
                }
            } else if let Some(message) = last_error {
                last_heartbeat_ms = 0;
                let is_541 = message.contains("541");
                if is_541 {
                    consecutive_541_count += 1;
                } else {
                    consecutive_541_count = 0;
                }

                let next_delay_ms = if is_541 {
                    match consecutive_541_count {
                        1 => 10_000,
                        2 => 30_000,
                        3 => 60_000,
                        4 => 120_000,
                        _ => 300_000,
                    }
                } else {
                    error_delay_ms.saturating_mul(2).max(5_000).min(30_000)
                };
                let status_message = if is_541 {
                    format!("Apple 541 限流，{} 秒后自动重试", next_delay_ms / 1000)
                } else {
                    message.clone()
                };

                self.update_status(&account_id, |status| {
                    status.status = "polling".to_string();
                    status.last_error = Some(status_message.clone());
                })
                .await;

                if !is_541 {
                    self.log(&account_id, "error", message).await;
                } else if consecutive_541_count == 1 || next_delay_ms != error_delay_ms {
                    self.log(&account_id, "error", status_message).await;
                }
                if is_541 {
                    self.persist_session_cookies(&account_id, stock_client.session_cookies())
                        .await;
                    self.log(
                        &account_id,
                        "info",
                        "541 后已保存新的库存会话 Cookie（库存监控为直连，不占用代理池）",
                    )
                    .await;
                }
                error_delay_ms = next_delay_ms;
            } else {
                let latency = started.elapsed().as_millis();
                self.update_status(&account_id, |status| {
                    status.status = "polling".to_string();
                    status.last_latency_ms = latency;
                    status.last_poll = Some(Self::now_ms());
                    status.last_error = None;
                })
                .await;
                error_delay_ms = 0;
                consecutive_541_count = 0;
                let now = Self::now_ms();
                if last_heartbeat_ms == 0 || now.saturating_sub(last_heartbeat_ms) >= 30_000 {
                    let configured_poll_delay_ms = poll_delay.unwrap_or(settings.poll_delay_ms);
                    self.log(
                        &account_id,
                        "info",
                        format!(
                            "监控心跳：暂无库存（型号 {} 个 / 门店 {} 家；本轮查询 {} 个型号，实际轮询 {}ms，日志每 30 秒记录一次）",
                            config.parts.len(),
                            config.stores.len(),
                            current_parts.len(),
                            configured_poll_delay_ms
                        ),
                    )
                    .await;
                    last_heartbeat_ms = now;
                }
            }

            part_batch_index = (part_batch_index + 1) % part_batches.len();

            let base = poll_delay.unwrap_or(settings.poll_delay_ms);
            let jitter = if settings.jitter_ms > 0 {
                rand::thread_rng().gen_range(0..settings.jitter_ms)
            } else {
                0
            };
            tokio::time::sleep(std::time::Duration::from_millis(
                base + jitter + error_delay_ms,
            ))
            .await;
        }
    }
}

fn is_network_error(error: &str) -> bool {
    let message = error.to_lowercase();
    [
        "timeout",
        "timed out",
        "connect",
        "connection",
        "dns",
        "proxy",
        "error sending request",
        "request failed",
        "541",
    ]
    .iter()
    .any(|keyword| message.contains(keyword))
}

/// 下单链路「可换代理重试」的错误判定：仅限 Apple 限流/服务不可用与网络层失败。
/// 业务失败（如 order not confirmed、重复下单限制）不在此列，避免误重复下单。
/// 判断下单失败是否值得「换一个代理 IP 重试」。
/// 仅限网络层失败与 Apple 限流；业务拒绝（限购、库存等）换 IP 也没用，
/// 反而会重复下单。
fn is_proxy_retryable_order_error(error: &str) -> bool {
    let message = error.to_lowercase();

    // 业务失败：不换 IP
    const BUSINESS_REJECTIONS: [&str; 5] = [
        "order not confirmed",
        "quantity_limit_exceeded",
        "超出单件购买数量",
        "sorrykey",
        "无法完成处理订单",
    ];
    if BUSINESS_REJECTIONS.iter().any(|kw| message.contains(kw)) {
        return false;
    }

    [
        "541",
        "503",
        "service unavailable",
        "too many requests",
        "timeout",
        "timed out",
        "connect",
        "connection",
        "dns",
        "proxy",
        "error sending request",
        "request failed",
    ]
    .iter()
    .any(|keyword| message.contains(keyword))
}

pub async fn api_state(State(state): State<AppState>) -> Json<AppStatePayload> {
    Json(build_state_payload(&state).await)
}

pub async fn build_state_payload(state: &AppState) -> AppStatePayload {
    let config = state.inner.config.lock().await;
    let statuses = state.inner.statuses.lock().await;
    let batch_progress = state.inner.batch_progress.lock().await.clone();
    let cpu_count = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4);
    let recommended_batch_concurrency = (cpu_count * 6).clamp(16, 64);
    let accounts = config
        .accounts
        .iter()
        .map(|account| {
            let default = AccountStatus {
                id: account.id.clone(),
                phone: account.contact.phone.clone(),
                username: account.username.clone(),
                part: account.part.clone(),
                store_id: account.store_id.clone(),
                parts: account.parts.clone(),
                stores: account.stores.clone(),
                cookie_count: account.login_cookies.len(),
                cookie_saved_at: account.cookie_saved_at,
                proxy: account.proxy_url.clone(),
                enabled: account.enabled,
                running: false,
                status: "stopped".to_string(),
                last_latency_ms: 0,
                last_poll: None,
                orders_placed: account.orders_placed as u64,
                last_error: None,
                last_order_number: account.last_order_number.clone(),
                last_payment_link: account.last_payment_link.clone(),
                wechat_session_count: 0,
                wechat_session_fresh_count: 0,
                wechat_session_expired_count: 0,
                wechat_session_min_remaining_ms: None,
                wechat_session_part: None,
                wechat_session_store: None,
        wechat_session_checkout_ready: false,
        wechat_session_fallback_only: false,
                wechat_cookie_status: "missing".to_string(),
                wechat_cookie_age_ms: None,
                order_attempts: 0,
                retry_after_ms: None,
                proxy_health: "unknown".to_string(),
                priority_score: 0,
            };
            statuses.get(&account.id).cloned().unwrap_or(default)
        })
        .collect::<Vec<_>>();
    let circuit_open_until_ms = *state.inner.circuit_open_until_ms.lock().await;
    let circuit_open = circuit_open_until_ms
        > SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
    let circuit_failure_count = *state.inner.circuit_failure_count.lock().await;
    let order_attempts = *state.inner.order_attempts.lock().await;
    let order_successes = *state.inner.order_successes.lock().await;
    let order_failures = *state.inner.order_failures.lock().await;
    let active_orders = state.inner.central_ordering_accounts.lock().await.len();
    let monitor_last_latency_ms = *state.inner.monitor_last_latency_ms.lock().await;
    let monitor_available_combos = *state.inner.monitor_available_combos.lock().await;
    let monitor_dispatched_count =
        usize::try_from(*state.inner.monitor_dispatched_count.lock().await).unwrap_or_default();
    // 只推送最近 N 条订单：订单历史可能长期累积，全量推送会让 WebSocket payload 无限膨胀
    const ORDERS_PUSH_MAX: usize = 100;
    let (orders, orders_total) = {
        let all = state.inner.orders.lock().await;
        let total = all.len();
        let slice = if total > ORDERS_PUSH_MAX {
            all[total - ORDERS_PUSH_MAX..].to_vec()
        } else {
            all.clone()
        };
        (slice, total)
    };
    AppStatePayload {
        settings: config.settings.clone(),
        accounts,
        batch_progress,
        recommended_batch_concurrency,
        cpu_count,
        circuit_open,
        circuit_open_until_ms: Some(circuit_open_until_ms),
        circuit_failure_count,
        order_attempts,
        order_successes,
        order_failures,
        active_orders,
        monitor_last_latency_ms,
        monitor_available_combos,
        monitor_dispatched_count,
        orders,
        orders_total,
    }
}

pub async fn api_logs(State(state): State<AppState>) -> Json<Value> {
    let username_map = {
        let config = state.inner.config.lock().await;
        config
            .accounts
            .iter()
            .map(|account| (account.id.clone(), account.username.clone()))
            .collect::<HashMap<_, _>>()
    };
    let logs = state.inner.logs.lock().await;
    Json(json!(logs
        .iter()
        .map(|log| {
            let username = username_map
                .get(&log.account_id)
                .cloned()
                .unwrap_or_else(|| "系统".to_string());
            json!({
                "ts_ms": log.ts_ms,
                "account_id": log.account_id,
                "username": username,
                "level": log.level,
                "message": log.message,
            })
        })
        .collect::<Vec<_>>()))
}

pub async fn api_clear_logs(State(state): State<AppState>) -> impl IntoResponse {
    let mut logs = state.inner.logs.lock().await;
    logs.clear();
    drop(logs);

    let mut removed_files = 0usize;
    if let Ok(entries) = fs::read_dir("logs") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                if fs::remove_file(&path).is_ok() {
                    removed_files += 1;
                }
            }
        }
    }

    state.log("batch", "info", "运行日志已清空").await;

    Json(json!({
        "ok": true,
        "removed_files": removed_files
    }))
    .into_response()
}

pub async fn api_models() -> Json<serde_json::Value> {
    Json(json!([
        { "family": "iPhone 18 Pro", "capacity": "256GB", "color": "黑色", "part": "MJT74CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "256GB", "color": "银色", "part": "MJT84CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "256GB", "color": "勃艮第酒红色", "part": "MJT94CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "256GB", "color": "冰川蓝色", "part": "MJTA4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "512GB", "color": "黑色", "part": "MJTC4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "512GB", "color": "银色", "part": "MJTD4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "512GB", "color": "勃艮第酒红色", "part": "MJTE4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "512GB", "color": "冰川蓝色", "part": "MJTF4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "1TB", "color": "黑色", "part": "MJTG4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "1TB", "color": "银色", "part": "MJTH4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "1TB", "color": "勃艮第酒红色", "part": "MJTJ4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "1TB", "color": "冰川蓝色", "part": "MJTK4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "2TB", "color": "黑色", "part": "MJTL4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "2TB", "color": "银色", "part": "MJTM4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "2TB", "color": "勃艮第酒红色", "part": "MJTN4CH/A" },
        { "family": "iPhone 18 Pro", "capacity": "2TB", "color": "冰川蓝色", "part": "MJTP4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "256GB", "color": "黑色", "part": "MJY64CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "256GB", "color": "银色", "part": "MJY74CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "256GB", "color": "勃艮第酒红色", "part": "MJY84CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "256GB", "color": "冰川蓝色", "part": "MJY94CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "512GB", "color": "黑色", "part": "MJYA4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "512GB", "color": "银色", "part": "MJYC4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "512GB", "color": "勃艮第酒红色", "part": "MJYD4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "512GB", "color": "冰川蓝色", "part": "MJYE4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "1TB", "color": "黑色", "part": "MJYF4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "1TB", "color": "银色", "part": "MJYG4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "1TB", "color": "勃艮第酒红色", "part": "MJYH4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "1TB", "color": "冰川蓝色", "part": "MJYJ4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "2TB", "color": "黑色", "part": "MJYK4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "2TB", "color": "银色", "part": "MJYL4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "2TB", "color": "勃艮第酒红色", "part": "MJYM4CH/A" },
        { "family": "iPhone 18 Pro Max", "capacity": "2TB", "color": "冰川蓝色", "part": "MJYN4CH/A" },
        { "family": "iPhone Duo", "capacity": "256GB", "color": "星光白色", "part": "MK2M4CH/A" },
        { "family": "iPhone Duo", "capacity": "256GB", "color": "夜空色", "part": "MK2N4CH/A" },
        { "family": "iPhone Duo", "capacity": "512GB", "color": "星光白色", "part": "MK2P4CH/A" },
        { "family": "iPhone Duo", "capacity": "512GB", "color": "夜空色", "part": "MK2Q4CH/A" },
        { "family": "iPhone Duo", "capacity": "1TB", "color": "星光白色", "part": "MK2T4CH/A" },
        { "family": "iPhone Duo", "capacity": "1TB", "color": "夜空色", "part": "MK2U4CH/A" },
        { "family": "iPhone Duo", "capacity": "2TB", "color": "星光白色", "part": "MK2V4CH/A" },
        { "family": "iPhone Duo", "capacity": "2TB", "color": "夜空色", "part": "MK2W4CH/A" }
    ]))
}

pub async fn api_get_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let config = state.inner.config.lock().await;
    match config.accounts.iter().find(|account| account.id == id) {
        Some(account) => Json(account.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn api_manual_order(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(payload): Json<ManualOrderRequest>,
) -> impl IntoResponse {
    let (mut account, settings) = {
        let config = state.inner.config.lock().await;
        match config
            .accounts
            .iter()
            .find(|account| account.id == account_id)
        {
            Some(account) => (account.clone(), config.settings.clone()),
            None => {
                return Json(json!({
                    "ok": false,
                    "error": "账号不存在"
                }))
                .into_response()
            }
        }
    };

    if payload.part.trim().is_empty() || payload.store_id.trim().is_empty() {
        return Json(json!({
            "ok": false,
            "error": "请选择型号和门店"
        }))
        .into_response();
    }
    if account.login_cookies.is_empty() {
        return Json(json!({
            "ok": false,
            "error": "该账号还没有登录 Cookie，请先完成 Apple ID 登录"
        }))
        .into_response();
    }

    account.part = payload.part.trim().to_string();
    account.store_id = payload.store_id.trim().to_string();
    account.normalize();
    state
        .log(
            &account.id,
            "info",
            format!("手动下单开始：{} · {}", account.part, account.store_id),
        )
        .await;

    let mut client = match AppleClient::new(&account.proxy_url, settings.http_timeout_ms) {
        Ok(client) => client,
        Err(err) => {
            let message = format!("创建下单客户端失败：{err}");
            state.log(&account.id, "error", message.clone()).await;
            return Json(json!({"ok": false, "error": message})).into_response();
        }
    };
    client.set_login_cookies(&account.login_cookies);

    let stores = match client.stock_details(&account.part, &account.stores).await {
        Ok(stores) => stores,
        Err(err) => {
            let message = format!("手动下单库存预检失败：{err}");
            state.log(&account.id, "error", message.clone()).await;
            return Json(json!({"ok": false, "error": message})).into_response();
        }
    };
    let store_status = stores
        .iter()
        .find(|store| {
            store.get("store_id").and_then(Value::as_str) == Some(account.store_id.as_str())
        })
        .and_then(|store| store.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("not_returned");
    if store_status != "available" {
        let status_label = match store_status {
            "ineligible" => "不符合购买条件",
            "unavailable" => "不可取货",
            "not_returned" => "门店未返回该型号",
            other => other,
        };
        let message = format!(
            "手动下单已阻止：{} 在门店 {} 当前状态为“{}”，不能加购下单",
            account.part, account.store_id, status_label
        );
        state.log(&account.id, "error", message.clone()).await;
        state
            .update_status(&account.id, |status| {
                status.status = "error".to_string();
                status.last_error = Some(message.clone());
            })
            .await;
        return Json(json!({"ok": false, "error": message, "stock_status": store_status}))
            .into_response();
    }

    state
        .log(
            &account.id,
            "info",
            format!("手动下单预检通过：{} · {}", account.part, account.store_id),
        )
        .await;

    if let Err(err) = client.bootstrap(&account).await {
        let message = format!("手动下单加购失败：{err}");
        state.log(&account.id, "error", message.clone()).await;
        state
            .update_status(&account.id, |status| {
                status.status = "error".to_string();
                status.last_error = Some(message.clone());
            })
            .await;
        return Json(json!({"ok": false, "error": message})).into_response();
    }
    state
        .persist_session_cookies(&account.id, client.session_cookies())
        .await;

    match client.checkout(&account, &settings).await {
        Ok(payment_link) => {
            state
                .log(
                    &account.id,
                    "info",
                    format!("手动下单成功，支付链接：{payment_link}"),
                )
                .await;
            state
                .update_status(&account.id, |status| {
                    status.status = "order placed".to_string();
                    status.orders_placed += 1;
                    status.last_payment_link = Some(payment_link.clone());
                    status.last_error = None;
                })
                .await;
            Json(json!({
                "ok": true,
                "payment_link": payment_link
            }))
            .into_response()
        }
        Err(err) => {
            let message = format!("手动下单失败：{err}");
            state.log(&account.id, "error", message.clone()).await;
            state
                .update_status(&account.id, |status| {
                    status.status = "error".to_string();
                    status.last_error = Some(message.clone());
                })
                .await;
            Json(json!({"ok": false, "error": message})).into_response()
        }
    }
}

pub async fn api_wechat_order(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(payload): Json<WechatOrderRequest>,
) -> impl IntoResponse {
    let account = {
        let config = state.inner.config.lock().await;
        match config
            .accounts
            .iter()
            .find(|account| account.id == account_id)
        {
            Some(account) => account.clone(),
            None => {
                return Json(json!({
                    "ok": false,
                    "error": "账号不存在"
                }))
                .into_response()
            }
        }
    };
    let account_lock = state.account_lock(&account.id).await;
    let _account_guard = account_lock.lock().await;

    let cookie = payload
        .cookie
        .clone()
        .filter(|cookie| !cookie.trim().is_empty())
        .unwrap_or_else(|| account.wechat_cookie.clone());
    if cookie.trim().is_empty() {
        return Json(json!({
            "ok": false,
            "error": "请先填写微信小程序 Cookie"
        }))
        .into_response();
    }

    let contact = json!({
        "lastName": account.contact.last_name,
        "firstName": account.contact.first_name,
        "emailAddress": account.contact.email,
        "fullDaytimePhone": account.contact.phone,
        "nationalId": account.contact.national_id_suffix,
    });
    // 手动下单也走代理：账号自带 > 静态 IP > 动态池（与监控派发规则一致）
    let order_proxy = state
        .resolve_account_proxy(&account)
        .await
        .unwrap_or_default();

    let bridge_payload = json!({
        "cookie": cookie,
        "part": payload.part,
        "store": payload.store_id,
        "place": payload.place,
        "fast": payload.fast,
        "clear": true,
        "debug": false,
        "proxy": order_proxy.clone(),
        "contact": contact,
    });

    state
        .log(
            &account.id,
            "info",
            format!(
                "微信小程序下单开始：{} · {} · {} · 出口代理 {}",
                payload.part,
                payload.store_id,
                if payload.place {
                    "真实下单"
                } else {
                    "演练"
                },
                display_proxy(&order_proxy),
            ),
        )
        .await;

    let order_started_at_ms = std::time::Instant::now();
    let result = run_wechat_bridge(bridge_payload).await;
    let elapsed_ms = order_started_at_ms.elapsed().as_millis();

    if result.get("ok") == Some(&json!(true)) {
        state
            .persist_wechat_cookie_from_result(&account.id, &result)
            .await;
        let order_number = result
            .get("summary")
            .and_then(|s| s.get("order_number"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payment_link = result
            .get("summary")
            .and_then(|s| s.get("order_url"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        state
            .update_status(&account.id, |status| {
                status.status = if payload.place {
                    "order placed".to_string()
                } else {
                    "dry-run ok".to_string()
                };
                if payload.place {
                    status.orders_placed += 1;
                    status.last_order_number = if order_number.is_empty() {
                        status.last_order_number.clone()
                    } else {
                        Some(order_number.clone())
                    };
                    status.last_payment_link = if payment_link.is_empty() {
                        status.last_payment_link.clone()
                    } else {
                        Some(payment_link.clone())
                    };
                }
                status.last_error = None;
            })
            .await;
        if payload.place {
            state
                .persist_placed_order(&account.id, order_number.clone(), payment_link.clone())
                .await;
            state
                .record_order(
                    &account.id,
                    &account.username,
                    &order_number,
                    &payload.part,
                    &payload.store_id,
                    result
                        .pointer("/summary/pickup_date")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    result
                        .pointer("/summary/pickup_time")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    &payment_link,
                    elapsed_ms as u64,
                )
                .await;
        }
        let summary = result.get("summary").cloned().unwrap_or(json!({}));
        let success_text = if payload.place {
            wechat_order_success_text(&result)
        } else {
            format!(
                "微信小程序演练完成：型号 {} · 门店 {} · 取货时间 {} {}",
                summary.get("part").and_then(Value::as_str).unwrap_or(""),
                summary
                    .get("pickup_store")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                summary
                    .get("pickup_date")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                summary
                    .get("pickup_time")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            )
        };
        let success_text = format!(
            "{success_text} · 总下单耗时 {:.3}s",
            elapsed_ms as f64 / 1000.0
        );
        state.log(&account.id, "info", success_text).await;
        if let Some(steps) = result.get("steps").and_then(Value::as_array) {
            let summary = summarize_wechat_steps(steps);
            state
                .log(&account.id, "info", format!("小程序步骤汇总：{summary}"))
                .await;
        }
    } else {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
            .to_string();
        state
            .update_status(&account.id, |status| {
                status.status = "error".to_string();
                status.last_error = Some(error.clone());
            })
            .await;
        state
            .log(
                &account.id,
                "error",
                format!(
                    "微信小程序下单失败：{}",
                    result.get("error").cloned().unwrap_or(json!("未知错误"))
                ),
            )
            .await;
        // 失败时也输出步骤汇总：无货/拦截这类情况同样需要看到走了哪些步骤、
        // 以及购物袋是否命中了「预加购快路径」。
        if let Some(steps) = result.get("steps").and_then(Value::as_array) {
            if !steps.is_empty() {
                let summary = summarize_wechat_steps(steps);
                state
                    .log(&account.id, "info", format!("小程序步骤汇总：{summary}"))
                    .await;
            }
        }
    }

    Json(result).into_response()
}

async fn run_wechat_bridge(payload: Value) -> Value {
    run_wechat_flow(payload).await
}

pub async fn api_stock(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(query): Json<StockQuery>,
) -> impl IntoResponse {
    // 库存查询与中央监控一致：始终直连，不使用任何代理。
    let timeout_ms = {
        let config = state.inner.config.lock().await;
        if !config.accounts.iter().any(|a| a.id == account_id) {
            return Json(json!({"error": "account not found"})).into_response();
        }
        config.settings.http_timeout_ms
    };

    let part = query.part.clone();
    let mut last_error = String::new();
    // Apple 库存接口偶发返回非 JSON（上游抖动），重试 3 次并递增退避
    for attempt in 0..3u32 {
        let cdn_ip_index = 0usize;
        let client = match AppleClient::new_with_resolve_ip(
            "",
            timeout_ms,
            APPLE_CDN_IPS[cdn_ip_index],
        ) {
            Ok(c) => c,
            Err(e) => {
                last_error = format!("client init: {e}");
                continue;
            }
        };
        let mut selected = query.stores.clone().unwrap_or_default();
        if selected.is_empty() {
            if let Some(province) = query.province.as_deref() {
                if !province.trim().is_empty() {
                    let city = query.city.as_deref().unwrap_or(province);
                    let district = query.district.as_deref().unwrap_or("");
                    match client.mmap_store_numbers(province, city, district).await {
                        Ok(stores) => selected = stores,
                        Err(e) => {
                            last_error = format!("mmap store search: {e}");
                            continue;
                        }
                    }
                }
            }
            if selected.is_empty() {
                selected = load_all_store_ids();
            }
        }
        match client.stock_details(&part, &selected).await {
            Ok(stores) => {
                let filtered: Vec<Value> = if selected.is_empty() {
                    stores
                } else {
                    stores
                        .into_iter()
                        .filter(|store| {
                            store
                                .get("store_id")
                                .and_then(Value::as_str)
                                .map(|id| selected.iter().any(|s| s == id))
                                .unwrap_or(false)
                        })
                        .collect()
                };
                return Json(json!(filtered)).into_response();
            }
            Err(e) => {
                last_error = format!("stock: {e}");
                // 前两次失败后短暂退避再重试，第 3 次才真正返回错误
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        250 * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
    }
    Json(json!({"error": last_error})).into_response()
}

pub async fn api_wechat_prepare(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(payload): Json<WechatOrderRequest>,
) -> impl IntoResponse {
    let account = {
        let config = state.inner.config.lock().await;
        config.accounts.iter().find(|a| a.id == account_id).cloned()
    };
    let Some(account) = account else {
        return Json(json!({"ok": false, "error": "账号不存在"})).into_response();
    };
    let account_lock = state.account_lock(&account.id).await;
    let _account_guard = account_lock.lock().await;
    let contact = json!({
        "lastName": account.contact.last_name,
        "firstName": account.contact.first_name,
        "emailAddress": account.contact.email,
        "fullDaytimePhone": account.contact.phone,
        "nationalId": account.contact.national_id_suffix,
    });
    let bridge_payload = json!({
        "mode": "prepare",
        "cookie": account.wechat_cookie,
        "part": payload.part,
        "store": payload.store_id,
        "contact": contact,
        "proxy": account.proxy_url.clone(),
        "clear": true,
        "debug": false,
    });
    let result = run_wechat_bridge(bridge_payload.clone()).await;
    if result.get("ok") == Some(&json!(true)) {
        state
            .persist_wechat_cookie_from_result(&account.id, &result)
            .await;
        state
            .update_status(&account.id, |status| {
                status.status = "prepared".to_string();
                status.last_error = None;
            })
            .await;
        state
            .log(
                &account.id,
                "info",
                format!(
                    "微信小程序下单会话准备完成：{} · {}",
                    payload.part, payload.store_id
                ),
            )
            .await;
        if let Some(steps) = result.get("steps").and_then(Value::as_array) {
            let summary = summarize_wechat_steps(steps);
            state
                .log(&account.id, "info", format!("准备步骤汇总：{summary}"))
                .await;
        }
        let session_key = format!("{}|{}", payload.part, payload.store_id);
        state
            .store_wechat_session(&account.id, session_key, result.clone())
            .await;
    } else {
        state
            .log(
                &account.id,
                "error",
                format!(
                    "微信小程序下单会话准备失败：{}",
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误")
                ),
            )
            .await;
    }
    Json(result).into_response()
}

pub async fn api_wechat_place(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let account = {
        let config = state.inner.config.lock().await;
        config.accounts.iter().find(|a| a.id == account_id).cloned()
    };
    let Some(account) = account else {
        return Json(json!({"ok": false, "error": "账号不存在"})).into_response();
    };
    let account_lock = state.account_lock(&account.id).await;
    let _account_guard = account_lock.lock().await;
    let prepared = state.inner.prepared_wechat_orders.lock().await;
    let mut prepared_state = prepared
        .get(&account.id)
        .and_then(|sessions| {
            sessions
                .get(&format!("{}|{}", account.part, account.store_id))
                .or_else(|| sessions.values().next())
        })
        .cloned();
    drop(prepared);
    if prepared_state
        .as_ref()
        .map(|session| wechat_session_expired(session))
        .unwrap_or(true)
    {
        if account.wechat_cookie.trim().is_empty() {
            return Json(
                json!({"ok": false, "error": "缺少微信小程序 Cookie，无法自动准备下单会话"}),
            )
            .into_response();
        }
        state
            .log(
                &account.id,
                "info",
                format!(
                    "微信小程序下单会话不存在或已超过 30 分钟，自动重新准备：{}|{}",
                    account.part, account.store_id
                ),
            )
            .await;
        // 现场自动准备会话也必须走代理池（与后续下单同 IP），否则会话一旦建立就与下单出口不一致
        let prepare_proxy = state
            .resolve_account_proxy(&account)
            .await
            .unwrap_or_default();
        let contact = json!({
            "lastName": account.contact.last_name,
            "firstName": account.contact.first_name,
            "emailAddress": account.contact.email,
            "fullDaytimePhone": account.contact.phone,
            "nationalId": account.contact.national_id_suffix,
        });
        let prepare_payload = json!({
            "mode": "prepare",
            "cookie": account.wechat_cookie,
            "part": account.part,
            "store": account.store_id,
            "contact": contact,
            "proxy": prepare_proxy.clone(),
            "clear": true,
            "debug": false,
        });
        let prepare_result = run_wechat_bridge(prepare_payload).await;
        if prepare_result.get("ok") != Some(&json!(true)) {
            return Json(prepare_result).into_response();
        }
        state
            .persist_wechat_cookie_from_result(&account.id, &prepare_result)
            .await;
        let session_key = format!("{}|{}", account.part, account.store_id);
        state
            .store_wechat_session(&account.id, session_key, prepare_result.clone())
            .await;
        prepared_state = Some(prepare_result);
    }
    let Some(prepared_state) = prepared_state else {
        return Json(json!({"ok": false, "error": "请先准备微信小程序下单会话"})).into_response();
    };
    // 下单必须使用会话建立时的出口 IP；会话没记录（老数据）时回退账号代理
    // 会话 proxy_url 语义：非空 = 该出口 IP 建的会话；空串 = 直连建的会话；缺失 = 老数据。
    // 前两种都必须原样复用，否则 Apple 会判定会话失效（403）。
    let order_proxy = prepared_state
        .pointer("/result/proxy_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            prepared_state
                .get("proxy_url")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| account.proxy_url.clone());
    let order_started_at_ms = std::time::Instant::now();
    let mut bridge_payload = json!({
        "mode": "place",
        "state": prepared_state.get("result").cloned().unwrap_or(json!({})),
        "cookie": account.wechat_cookie,
        "part": prepared_state.get("result").and_then(|r| r.get("part")).and_then(Value::as_str).unwrap_or(&account.part),
        "store": prepared_state.get("result").and_then(|r| r.get("store")).and_then(Value::as_str).unwrap_or(&account.store_id),
        "debug": false,
        "proxy": order_proxy.clone(),
    });
    let mut result = run_wechat_bridge(bridge_payload.clone()).await;
    if result.get("ok") != Some(&json!(true)) && is_retryable_wechat_session_error(&result) {
        state
            .log(
                &account.id,
                "info",
                "微信小程序下单会话提交失败且疑似会话失效，自动重新准备并重试一次",
            )
            .await;
        let contact = json!({
            "lastName": account.contact.last_name,
            "firstName": account.contact.first_name,
            "emailAddress": account.contact.email,
            "fullDaytimePhone": account.contact.phone,
            "nationalId": account.contact.national_id_suffix,
        });
        let prepare_payload = json!({
            "mode": "prepare",
            "cookie": account.wechat_cookie,
            "part": account.part,
            "store": account.store_id,
            "contact": contact,
            "proxy": order_proxy.clone(),
            "clear": true,
            "debug": false,
        });
        let prepare_result = run_wechat_bridge(prepare_payload).await;
        if prepare_result.get("ok") != Some(&json!(true)) {
            return Json(prepare_result).into_response();
        }
        let session_key = format!("{}|{}", account.part, account.store_id);
        state
            .store_wechat_session(&account.id, session_key, prepare_result.clone())
            .await;
        bridge_payload = json!({
            "mode": "place",
            "state": prepare_result.get("result").cloned().unwrap_or(json!({})),
            "cookie": account.wechat_cookie,
            "part": account.part,
            "store": account.store_id,
            "debug": false,
            "proxy": order_proxy.clone(),
        });
        result = run_wechat_bridge(bridge_payload.clone()).await;
    }
    if result.get("ok") == Some(&json!(true)) {
        let order_number = result
            .pointer("/summary/order_number")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payment_link = result
            .pointer("/summary/order_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        state
            .update_status(&account.id, |status| {
                status.status = "order placed".to_string();
                status.orders_placed += 1;
                if !order_number.is_empty() {
                    status.last_order_number = Some(order_number.clone());
                }
                if !payment_link.is_empty() {
                    status.last_payment_link = Some(payment_link.clone());
                }
                status.last_error = None;
            })
            .await;
        let used_part = bridge_payload
            .get("part")
            .and_then(Value::as_str)
            .unwrap_or(&account.part);
        let used_store = bridge_payload
            .get("store")
            .and_then(Value::as_str)
            .unwrap_or(&account.store_id);
        state
            .remove_wechat_session(&account.id, format!("{}|{}", used_part, used_store))
            .await;
        state
            .persist_placed_order(&account.id, order_number.clone(), payment_link.clone())
            .await;
        state
            .record_order_success(&account.id, &account.proxy_url)
            .await;
        state
            .record_order(
                &account.id,
                &account.username,
                &order_number,
                used_part,
                used_store,
                result
                    .pointer("/summary/pickup_date")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                result
                    .pointer("/summary/pickup_time")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                &payment_link,
                order_started_at_ms.elapsed().as_millis() as u64,
            )
            .await;
        let elapsed_ms = order_started_at_ms.elapsed().as_millis();
        let success_text = format!(
            "{} · 总下单耗时 {:.3}s",
            wechat_order_success_text(&result),
            elapsed_ms as f64 / 1000.0
        );
        state.log(&account.id, "info", success_text).await;
        if let Some(steps) = result.get("steps").and_then(Value::as_array) {
            let summary = summarize_wechat_steps(steps);
            state
                .log(&account.id, "info", format!("下单步骤汇总：{summary}"))
                .await;
        }
    } else {
        state
            .log(
                &account.id,
                "error",
                format!(
                    "微信小程序最后一步下单失败：{}",
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误")
                ),
            )
            .await;
    }
    Json(result).into_response()
}

pub async fn api_wechat_prepare_batch(
    State(state): State<AppState>,
    Json(payload): Json<WechatPrepareBatchRequest>,
) -> impl IntoResponse {
    let batch_part = payload.part.clone().unwrap_or_default();
    let accounts = {
        let config = state.inner.config.lock().await;
        config
            .accounts
            .iter()
            .filter(|account| {
                account.enabled
                    && !account.wechat_cookie.trim().is_empty()
                    && (payload.account_ids.is_empty() || payload.account_ids.contains(&account.id))
            })
            .cloned()
            .map(|mut account| {
                if !batch_part.trim().is_empty() {
                    account.part = batch_part.trim().to_string();
                    account.parts = vec![batch_part.trim().to_string()];
                }
                account
            })
            .collect::<Vec<_>>()
    };

    if accounts.is_empty() {
        return Json(json!({
            "ok": false,
            "error": "没有可准备的账号，请先启用账号并填写小程序 Cookie"
        }))
        .into_response();
    }

    let mut unique_parts: HashSet<String> = HashSet::new();
    let mut unique_stores: HashSet<String> = HashSet::new();
    for account in &accounts {
        unique_parts.extend(account.parts.iter().cloned());
        unique_stores.extend(account.stores.iter().cloned());
    }
    let unique_parts = unique_parts.into_iter().collect::<Vec<_>>();
    let unique_stores = unique_stores.into_iter().collect::<Vec<_>>();
    let timeout_ms = state.inner.config.lock().await.settings.http_timeout_ms;
    let stock_client = match AppleClient::new_with_resolve_ip("", timeout_ms, APPLE_CDN_IPS[0]) {
        Ok(client) => client,
        Err(err) => {
            return Json(json!({"ok": false, "error": format!("创建库存客户端失败：{err}")}))
                .into_response();
        }
    };
    let stock_by_part = match stock_client
        .stock_details_multi(&unique_parts, &unique_stores)
        .await
    {
        Ok(stock_by_part) => stock_by_part,
        Err(err) => {
            return Json(json!({"ok": false, "error": format!("查询可用库存失败：{err}")}))
                .into_response();
        }
    };
    let available_combinations = stock_by_part
        .iter()
        .flat_map(|(part, stores)| {
            stores
                .iter()
                .filter(|store| store.get("status").and_then(Value::as_str) == Some("available"))
                .filter_map(|store| {
                    store
                        .get("store_id")
                        .and_then(Value::as_str)
                        .map(|store_id| (part.clone(), store_id.to_string()))
                })
        })
        .collect::<HashSet<_>>();
    for (part, stores) in &stock_by_part {
        let available_count = stores
            .iter()
            .filter(|store| store.get("status").and_then(Value::as_str) == Some("available"))
            .count();
        state
            .log(
                "batch",
                "info",
                format!(
                    "批量准备库存检查：{part} · 返回 {} 家 · 有货 {} 家",
                    stores.len(),
                    available_count
                ),
            )
            .await;
    }
    let total_sessions = accounts.len();
    let account_count = accounts.len();
        let started_ms = AppState::now_ms();
    state
        .log(
            "batch",
            "info",
            format!(
                "批量准备微信小程序会话开始：{} 个账号 / 每个账号只准备 1 个有效会话",
                accounts.len()
            ),
        )
        .await;
    state
        .log(
            "batch",
            "info",
            format!("批量准备可用库存组合：{}", available_combinations.len()),
        )
        .await;

    let batch_state = state.clone();
    let batch_prepare_concurrency = state
        .inner
        .config
        .lock()
        .await
        .settings
        .batch_prepare_concurrency;
    {
        let mut handles = state.inner.handles.lock().await;
        if let Some(handle) = handles.remove(BATCH_PREPARE_HANDLE_KEY) {
            handle.abort();
        }
    }
    let handle = tokio::spawn(async move {
        {
            let mut progress = batch_state.inner.batch_progress.lock().await;
            *progress = BatchProgress {
                kind: "prepare".to_string(),
                running: true,
                total: total_sessions,
                completed: 0,
                succeeded: 0,
                failed: 0,
                started_at_ms: Some(started_ms as u64),
                updated_at_ms: Some(AppState::now_ms()),
                finished_at_ms: None,
                last_error: None,
            };
        }
        let mut tasks = tokio::task::JoinSet::new();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
            batch_prepare_concurrency.max(1),
        ));
        for account in accounts {
            let batch_state = batch_state.clone();
            let available_combinations = available_combinations.clone();
            let semaphore = semaphore.clone();
            let account_lock = batch_state.account_lock(&account.id).await;
            tasks.spawn(async move {
                let _account_guard = account_lock.lock().await;
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("batch semaphore closed");
                let parts = if account.parts.is_empty() {
                    vec![account.part.clone()]
                } else {
                    account.parts.clone()
                };
                let stores = if account.stores.is_empty() {
                    vec![account.store_id.clone()]
                } else {
                    account.stores.clone()
                };
                let mut has_prepared_session = false;
                for part in parts {
                    if has_prepared_session {
                        break;
                    }
                    for store_id in stores.clone() {
                        if !available_combinations.contains(&(part.clone(), store_id.clone())) {
                            continue;
                        }
                        let contact = json!({
                            "lastName": account.contact.last_name,
                            "firstName": account.contact.first_name,
                            "emailAddress": account.contact.email,
                            "fullDaytimePhone": account.contact.phone,
                            "nationalId": account.contact.national_id_suffix,
                        });
                        // 预准备会话与后续下单必须同出口 IP，否则下单时 Apple 判定会话失效（403）
                        let prepare_proxy = batch_state
                            .resolve_account_proxy(&account)
                            .await
                            .unwrap_or_default();
                        let bridge_payload = json!({
                            "mode": "prepare",
                            "cookie": account.wechat_cookie,
                            "part": part,
                            "store": store_id,
                            "contact": contact,
                            "clear": true,
                            "debug": false,
                            "proxy": prepare_proxy.clone(),
                        });
                        let result = run_wechat_bridge(bridge_payload).await;
                        let session_key = format!("{}|{}", part, store_id);
                        if result.get("ok") == Some(&json!(true)) {
                            batch_state
                                .persist_wechat_cookie_from_result(&account.id, &result)
                                .await;
                            batch_state
                                .store_wechat_session(&account.id, session_key, result.clone())
                                .await;
                            let mut progress = batch_state.inner.batch_progress.lock().await;
                            progress.completed += 1;
                            progress.succeeded += 1;
                            progress.updated_at_ms = Some(AppState::now_ms());
                            drop(progress);
                            batch_state
                                .log(
                                    &account.id,
                                    "info",
                                    format!(
                                        "批量准备会话完成：{} · {} · {}",
                                        part,
                                        store_id,
                                        summarize_wechat_steps(
                                            result
                                                .get("steps")
                                                .and_then(Value::as_array)
                                                .map(|steps| steps.as_slice())
                                                .unwrap_or(&[])
                                        )
                                    ),
                                )
                                .await;
                        } else {
                            batch_state
                                .log(
                                    &account.id,
                                    "error",
                                    format!(
                                        "批量准备会话失败：{} · {} · {}",
                                        part,
                                        store_id,
                                        result
                                            .get("error")
                                            .and_then(Value::as_str)
                                            .unwrap_or("未知错误")
                                    ),
                                )
                                .await;
                            let mut progress = batch_state.inner.batch_progress.lock().await;
                            progress.completed += 1;
                            progress.failed += 1;
                            progress.updated_at_ms = Some(AppState::now_ms());
                            progress.last_error = result
                                .get("error")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            drop(progress);
                        }
                        has_prepared_session = true;
                        break;
                    }
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        {
            let mut progress = batch_state.inner.batch_progress.lock().await;
            progress.running = false;
            progress.kind = "idle".to_string();
            progress.finished_at_ms = Some(AppState::now_ms());
            progress.updated_at_ms = Some(AppState::now_ms());
        }
        let elapsed_ms = AppState::now_ms().saturating_sub(started_ms);
        batch_state
            .log(
                "batch",
                "info",
                format!("批量准备微信小程序会话完成：耗时 {}ms", elapsed_ms),
            )
            .await;
        batch_state
            .inner
            .handles
            .lock()
            .await
            .remove(BATCH_PREPARE_HANDLE_KEY);
    });
    state
        .inner
        .handles
        .lock()
        .await
        .insert(BATCH_PREPARE_HANDLE_KEY.to_string(), handle);

    Json(json!({
        "ok": true,
        "started": true,
                "accounts": account_count,
        "sessions": total_sessions
    }))
    .into_response()
}


pub async fn api_wechat_warmup_batch(
    State(state): State<AppState>,
    Json(payload): Json<WechatPrepareBatchRequest>,
) -> impl IntoResponse {
    // 上一次预热若被中断（例如启动监控时被 abort），进度会停留在 running=true，
    // 导致监控一直跳过派发、进度条卡住。这里先清理旧任务并复位进度。
    {
        let mut handles = state.inner.handles.lock().await;
        if let Some(old) = handles.remove(BATCH_WARMUP_HANDLE_KEY) {
            old.abort();
            let mut progress = state.inner.batch_progress.lock().await;
            *progress = BatchProgress::default();
            drop(progress);
            state
                .log("batch", "warn", "检测到上一次预热未正常结束，已重置预热进度")
                .await;
        }
    }

    // 允许显式指定型号 / 门店；为空时回退到每个账号自身配置
    let batch_parts: Vec<String> = payload
        .parts
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let batch_stores: Vec<String> = payload
        .stores
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let accounts = {
        let config = state.inner.config.lock().await;
        config
            .accounts
            .iter()
            .filter(|a| {
                a.enabled
                    && !a.wechat_cookie.trim().is_empty()
                    && (payload.account_ids.is_empty() || payload.account_ids.contains(&a.id))
            })
            .cloned()
            .collect::<Vec<_>>()
    };

    // 预热前先查一次库存：优先预热「有货组合」，能一口气拿到结账页数据；
    // 无货组合仍会做降级预热（清空购物袋 + 取 OpenID），有货下单时省一步。
    let mut warmup_available: HashSet<(String, String)> = HashSet::new();
    let warmup_combos: Vec<(String, String)> = {
        let mut parts: Vec<String> = if !batch_parts.is_empty() {
            batch_parts.clone()
        } else {
            accounts.iter().flat_map(|a| a.parts.iter().cloned()).collect()
        };
        let mut stores: Vec<String> = if !batch_stores.is_empty() {
            batch_stores.clone()
        } else {
            accounts.iter().flat_map(|a| a.stores.iter().cloned()).collect()
        };
        parts.sort();
        parts.dedup();
        stores.sort();
        stores.dedup();

        if !parts.is_empty() && !stores.is_empty() {
            let timeout_ms = state.inner.config.lock().await.settings.http_timeout_ms;
            if let Ok(client) = AppleClient::new_with_resolve_ip("", timeout_ms, APPLE_CDN_IPS[0]) {
                match client.stock_details_multi(&parts, &stores).await {
                    Ok(stock_by_part) => {
                        for (part, store_list) in &stock_by_part {
                            for store in store_list {
                                if store.get("status").and_then(Value::as_str) == Some("available") {
                                    if let Some(store_id) = store.get("store_id").and_then(Value::as_str) {
                                        warmup_available.insert((part.clone(), store_id.to_string()));
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        state
                            .log("batch", "warn", format!("预热前库存查询失败：{err}（将按配置组合预热）"))
                            .await;
                    }
                }
            }
        }

        // 有货组合排前面，无货的放后面兜底
        let mut combos: Vec<(String, String)> = warmup_available.iter().cloned().collect();
        combos.sort();
        for part in &parts {
            for store in &stores {
                let key = (part.clone(), store.clone());
                if !combos.contains(&key) {
                    combos.push(key);
                }
            }
        }
        state
            .log(
                "batch",
                "info",
                format!(
                    "预热库存检查：{} 个组合，其中有货 {} 个（每个账号只保留 1 个预热结果，优先有货组合）",
                    combos.len(),
                    warmup_available.len()
                ),
            )
            .await;
        combos
    };

    if accounts.is_empty() {
        return Json(json!({"ok": false, "error": "No eligible accounts"})).into_response();
    }

    state
        .log(
            "batch",
            "info",
            format!("Batch warmup started: {} accounts (bag + contact + payment)", accounts.len()),
        )
        .await;

    let warmup_state = state.clone();
    let concurrency = {
        warmup_state
            .inner
            .config
            .lock()
            .await
            .settings
            .batch_prepare_concurrency
            .max(1)
    };
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));

    let total = accounts.len();
    {
        let mut progress = warmup_state.inner.batch_progress.lock().await;
        progress.kind = "warmup".to_string();
        progress.running = true;
        progress.total = total;
        progress.completed = 0;
        progress.succeeded = 0;
        progress.failed = 0;
        progress.last_error = None;
    }
    let account_count = accounts.len();

    let combos_total = warmup_combos.len();
    let combos_available = warmup_available.len();
    let handle = tokio::spawn(async move {
        let warmup_tasks: Vec<_> = accounts
            .iter()
            .map(|account| {
                let batch_parts = batch_parts.clone();
                let batch_stores = batch_stores.clone();
                let warmup_available = warmup_available.clone();
                let semaphore = semaphore.clone();
                let warmup_state = warmup_state.clone();
                let account = account.clone();
                async move {
                    let _permit = semaphore.acquire().await;
                    // 预热与后续下单必须使用同一个出口 IP，否则 Apple 判定会话失效（403），预热就白做了。
                    let warmup_proxy = warmup_state
                        .resolve_account_proxy(&account)
                        .await
                        .unwrap_or_default();
                    // 组合顺序：有货的排前面（能直接拿到结账页数据），无货的放后面做降级预热。
                    // 每个账号只保留一个会话，因此命中真正成功（有货）的结果即停止。
                    let account_parts: Vec<String> = if !batch_parts.is_empty() {
                        batch_parts.clone()
                    } else if !account.parts.is_empty() {
                        account.parts.clone()
                    } else {
                        vec![account.part.clone()]
                    };
                    let account_stores: Vec<String> = if !batch_stores.is_empty() {
                        batch_stores.clone()
                    } else if !account.stores.is_empty() {
                        account.stores.clone()
                    } else {
                        vec![account.store_id.clone()]
                    };
                    // 候选组合：有货的排前面（能一口气拿到结账页数据）。
                    let mut in_stock: Vec<(String, String)> = Vec::new();
                    for part in &account_parts {
                        for store in &account_stores {
                            if warmup_available.contains(&(part.clone(), store.clone())) {
                                in_stock.push((part.clone(), store.clone()));
                            }
                        }
                    }
                    let mut candidates: Vec<(String, String)> = in_stock.clone();
                    // 无货兜底：全都没货时随机挑一个组合做「预加购」；
                    // 有货组合也放在末尾，作为「有货但预热失败」时的后备。
                    // 目的：等库存到货时，若命中的正是预加购的型号，
                    // 下单可直接走「bag 校验通过 → checkout_start」快路径。
                    {
                        let mut all: Vec<(String, String)> = Vec::new();
                        for part in &account_parts {
                            for store in &account_stores {
                                let key = (part.clone(), store.clone());
                                if !in_stock.contains(&key) {
                                    all.push(key);
                                }
                            }
                        }
                        if !all.is_empty() {
                            let pick = rand::thread_rng().gen_range(0..all.len());
                            candidates.push(all[pick].clone());
                        }
                    }

                    let mut done = false;
                    let mut last_error = String::new();
                    for (part, store) in candidates {
                        if done {
                            break;
                        }
                        let payload = json!({
                            "mode": "warmup",
                            "cookie": account.wechat_cookie,
                            "part": part,
                            "store": store,
                            "contact": json!({
                                "lastName": account.contact.last_name,
                                "firstName": account.contact.first_name,
                                "emailAddress": account.contact.email,
                                "fullDaytimePhone": account.contact.phone,
                                "nationalId": account.contact.national_id_suffix,
                            }),
                            "debug": false,
                            "proxy": warmup_proxy.clone(),
                        });
                        let result = run_wechat_bridge(payload).await;
                        if result.get("ok") == Some(&json!(true)) {
                            let cart_ready = result
                                .pointer("/result/cart_ready")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let checkout_ready = result
                                .pointer("/result/checkout_ready")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let pre_added = result
                                .pointer("/result/pre_added")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            let session_key = format!("{}|{}", part, store);
                            warmup_state
                                .persist_wechat_cookie_from_result(&account.id, &result)
                                .await;
                            warmup_state
                                .store_wechat_session(&account.id, session_key, result)
                                .await;
                            warmup_state
                                .log(
                                    &account.id,
                                    "info",
                                    format!(
                                        "预热完成：{} · {} · {}（出口 IP {}）",
                                        part,
                                        store,
                                        if checkout_ready {
                                            "已进入结账页，下单可直接提交"
                                        } else if pre_added {
                                            "当前无货，目标商品已预加购（有货后直接结账，省去清空+加购+复检）"
                                        } else if cart_ready {
                                            "购物袋已就绪（未进入结账页，下单时补结账步骤）"
                                        } else {
                                            "仅完成 OpenID 缓存"
                                        },
                                        display_proxy(&warmup_proxy)
                                    ),
                                )
                                .await;
                            // 预加购也算成功：命中库存后能直接走快路径，
                            // 因此不再继续尝试其它组合（每个账号只保留一个会话）。
                            done = cart_ready;
                        } else {
                            last_error = result
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("预热失败")
                                .to_string();
                        }
                    }
                    let mut progress = warmup_state.inner.batch_progress.lock().await;
                    progress.completed += 1;
                    if done {
                        progress.succeeded += 1;
                    } else {
                        progress.failed += 1;
                        if !last_error.is_empty() {
                            progress.last_error =
                                Some(format!("{}：{}", account.username, last_error));
                        }
                    }
                    drop(progress);
                }
            })
            .collect();

        let mut tasks = tokio::task::JoinSet::new();
        for task in warmup_tasks {
            tasks.spawn(task);
        }
        while tasks.join_next().await.is_some() {}

        {
            let mut progress = warmup_state.inner.batch_progress.lock().await;
            progress.running = false;
            progress.kind = "idle".to_string();
        }
        warmup_state.sync_statuses().await;
        warmup_state
            .log("batch", "info", "Batch warmup completed")
            .await;
    });

    {
        let mut handles = state.inner.handles.lock().await;
        handles.insert(BATCH_WARMUP_HANDLE_KEY.to_string(), handle);
    }

    Json(json!({
        "ok": true,
        "accounts": account_count,
        "mode": "warmup",
        "combos": combos_total,
        "available_combos": combos_available,
    }))
    .into_response()
}
pub async fn api_bootstrap_account(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let account = {
        let config = state.inner.config.lock().await;
        match config.accounts.iter().find(|a| a.id == account_id) {
            Some(a) => a.clone(),
            None => {
                return Json(json!({"ok": false, "error": "account not found"})).into_response()
            }
        }
    };
    let mut client = match AppleClient::new(&account.proxy_url, 8000) {
        Ok(client) => client,
        Err(e) => {
            return Json(json!({"ok": false, "error": e.to_string()})).into_response();
        }
    };
    client.set_login_cookies(&account.login_cookies);
    match client.bootstrap(&account).await {
        Ok(()) => {
            state
                .persist_session_cookies(&account.id, client.session_cookies())
                .await;
            Json(json!({
                "ok": true,
                "item_id": client.current_item_id(),
                "bag_items": client.current_bag_items(),
                "add_response": client.last_add_response(),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "ok": false,
            "error": e.to_string(),
            "bag_items": client.current_bag_items(),
            "add_response": client.last_add_response(),
        }))
        .into_response(),
    }
}

pub async fn api_login_start(
    State(state): State<AppState>,
    Json(payload): Json<LoginStart>,
) -> impl IntoResponse {
    let login_id = format!(
        "login-{}",
        AppState::now_ms()
    );
    let output = tokio::time::timeout(
        tokio::time::Duration::from_secs(90),
        Command::new("python")
            .arg("apple_login_bridge.py")
            .arg("start")
            .arg(&payload.account)
            .arg(&payload.password)
            .arg(&login_id)
            .output(),
    )
    .await;
    state
        .log(
            "login",
            "info",
            format!("开始发送验证码：{}", payload.account),
        )
        .await;
    match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => {
                    state
                        .log(
                            "login",
                            "info",
                            format!(
                                "发送验证码完成：{}",
                                match v.get("status").and_then(Value::as_str) {
                                    Some("need_2fa") => "等待输入验证码",
                                    Some("success") => "无需验证码，登录成功",
                                    _ => "状态未知",
                                }
                            ),
                        )
                        .await;
                    Json(v).into_response()
                }
                Err(_) => {
                    state
                        .log(
                            "login",
                            "error",
                            format!("发送验证码脚本输出异常：{}", stderr.trim()),
                        )
                        .await;
                    Json(json!({"ok": false, "error": stderr.trim()})).into_response()
                }
            }
        }
        Ok(Err(e)) => Json(json!({"ok": false, "error": format!("spawn: {e}")})).into_response(),
        Err(_) => Json(json!({"ok": false, "error": "登录超时，请稍后重试"})).into_response(),
    }
}

pub async fn api_login_verify(
    State(state): State<AppState>,
    Json(payload): Json<LoginVerify>,
) -> impl IntoResponse {
    let output = tokio::time::timeout(
        tokio::time::Duration::from_secs(90),
        Command::new("python")
            .arg("apple_login_bridge.py")
            .arg("verify")
            .arg(&payload.login_id)
            .arg(&payload.code)
            .output(),
    )
    .await;
    state.log("login", "info", "收到验证码，开始验证").await;
    let mut result = match output {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            serde_json::from_str::<Value>(stdout.trim())
                .unwrap_or(json!({"ok": false, "error": stderr.trim()}))
        }
        Ok(Err(e)) => json!({"ok": false, "error": format!("spawn: {e}")}),
        Err(_) => json!({"ok": false, "error": "验证超时，请稍后重试"}),
    };
    if result.get("ok") == Some(&json!(true)) {
        let cookies = result.get("cookies").cloned().unwrap_or(json!({}));
        if let Some(map) = cookies.as_object() {
            let mut config = state.inner.config.lock().await;
            let mut cookie_file_id = payload.login_id.clone();
            let login_account = result
                .get("account")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(account) = config
                .accounts
                .iter_mut()
                .find(|a| a.username == login_account)
            {
                for (k, v) in map {
                    if let Some(val) = v.as_str() {
                        account.login_cookies.insert(k.clone(), val.to_string());
                    }
                }
                account.cookie_saved_at = Some(
                    AppState::now_ms() as u64
                );
                cookie_file_id = account.id.clone();
            }
            drop(config);
            let _ = state.persist_config().await;
            let saved_cookies: HashMap<String, String> = map
                .iter()
                .filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())))
                .collect();
            let _ = save_cookie_file(&cookie_file_id, &login_account, &saved_cookies);
            result["cookie_count"] = json!(saved_cookies.len());
            result["cookie_file"] = json!(cookie_file_path(&cookie_file_id)
                .to_string_lossy()
                .to_string());
            state
                .log(
                    "login",
                    "info",
                    format!("登录成功，已保存 {} 个 Cookie", saved_cookies.len()),
                )
                .await;
        }
    } else {
        state
            .log(
                "login",
                "error",
                format!(
                    "验证码验证失败：{}",
                    result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
            )
            .await;
    }
    Json(result).into_response()
}

pub async fn api_import_accounts(
    State(state): State<AppState>,
    Json(payload): Json<BatchAccountImport>,
) -> StatusCode {
    let mut default_parts = payload.parts.clone().unwrap_or_default();
    if let Some(part) = payload.part.clone() {
        default_parts.push(part);
    }
    if default_parts.is_empty() {
        default_parts.push("MJT74CH/A".to_string());
    }
    let mut default_stores = payload.stores.clone().unwrap_or_default();
    if let Some(store) = payload.store_id.clone() {
        default_stores.push(store);
    }
    if default_stores.is_empty() {
        default_stores.push("R480".to_string());
    }
    let default_province = payload
        .province
        .clone()
        .unwrap_or_else(|| "重庆".to_string());
    let default_city = payload.city.clone().unwrap_or_else(|| "重庆".to_string());
    let default_district = payload.district.clone().unwrap_or_default();

    let mut imported = Vec::new();
    for line in payload.text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_compact_account_line(
            line,
            &default_parts,
            &default_stores,
            payload.proxy_url.as_deref().unwrap_or(""),
            &default_province,
            &default_city,
            &default_district,
        ) {
            Ok(Some(account)) => {
                imported.push(account);
                continue;
            }
            Ok(None) => {}
            Err(_) => return StatusCode::BAD_REQUEST,
        }
        let fields: Vec<&str> = line.split("----").map(str::trim).collect();
        if fields.len() != 8 {
            continue;
        }
        let mut account = AccountConfig {
            id: fields[0].to_string(),
            username: fields[1].to_string(),
            password: fields[2].to_string(),
            proxy_url: payload.proxy_url.clone().unwrap_or_default(),
            proxy_urls: Vec::new(),
            part: default_parts[0].clone(),
            store_id: default_stores[0].clone(),
            parts: default_parts.clone(),
            stores: default_stores.clone(),
            province: default_province.clone(),
            city: default_city.clone(),
            district: default_district.clone(),
            quantity: 1,
            contact: crate::models::ContactConfig {
                last_name: fields[3].to_string(),
                first_name: fields[4].to_string(),
                email: fields[5].to_string(),
                phone: fields[6].to_string(),
                national_id_suffix: fields[7].to_string(),
            },
            poll_delay_ms: None,
            login_cookies: HashMap::new(),
            wechat_cookie: String::new(),
            cookie_saved_at: None,
            orders_placed: 0,
            last_order_number: None,
            last_payment_link: None,
            enabled: true,
        };
        account.normalize();
        imported.push(account);
    }
    if imported.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    // 同一邮箱出现多行时（id 相同会互相覆盖），给第 2 行起追加序号，
    // 保证每行都能独立保留；「同邮箱重新导入」仍按基名覆盖旧配置。
    {
        let mut seen: HashMap<String, usize> = HashMap::new();
        for account in &mut imported {
            let base = account.id.clone();
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count > 1 {
                account.id = format!("{base}#{count}");
            }
        }
    }

    // 按「邮箱基名」计算本次导入影响到的账号（含 email 与 email#N 全部变体）
    let touched_bases = imported
        .iter()
        .map(|a| a.id.split('#').next().unwrap_or("").to_string())
        .collect::<Vec<_>>();

    {
        let mut config = state.inner.config.lock().await;
        config.accounts.retain(|e| {
            !touched_bases
                .iter()
                .any(|b| b == e.id.split('#').next().unwrap_or(""))
        });
        config.accounts.extend(imported.clone());
    }

    // 被覆盖的账号（同邮箱重新导入）必须清掉旧附属状态：
    // 旧会话内容可能与新配置（型号/门店）不匹配，代理绑定也已失效。
    {
        let hit = |id: &str| {
            touched_bases
                .iter()
                .any(|b| b == id.split('#').next().unwrap_or(""))
        };
        let mut prepared = state.inner.prepared_wechat_orders.lock().await;
        let mut completed = state.inner.central_completed_accounts.lock().await;
        let mut assign = state.inner.account_proxy_assign.lock().await;
        let mut static_assign = state.inner.static_proxy_assign.lock().await;
        prepared.retain(|k, _| !hit(k));
        completed.retain(|k| !hit(k));
        assign.retain(|k, _| !hit(k));
        static_assign.retain(|k, _| !hit(k));
    }
    let _ = state.persist_config().await;
    state.sync_statuses().await;
    // 导入后不自动开始监控：由用户在「监控与下单」页显式点击「启动中央监控」
    StatusCode::OK
}

pub async fn api_import_cookies(
    State(state): State<AppState>,
    Json(payload): Json<BatchCookieImport>,
) -> impl IntoResponse {
    let mut updated = 0;
    let mut created = 0;
    let mut failed = 0;
    let mut results: Vec<Value> = Vec::new();
    let default_parts = payload
        .parts
        .clone()
        .filter(|parts| !parts.is_empty())
        .unwrap_or_else(|| vec!["MJT74CH/A".to_string()]);
    let default_stores = payload
        .stores
        .clone()
        .filter(|stores| !stores.is_empty())
        .unwrap_or_else(|| vec!["R480".to_string()]);
    let default_province = payload
        .province
        .clone()
        .unwrap_or_else(|| "重庆".to_string());
    let default_city = payload.city.clone().unwrap_or_else(|| "重庆".to_string());
    let default_district = payload.district.clone().unwrap_or_default();
    let lines: Vec<String> = payload
        .text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut line = line.to_string();
            if let Some(stripped) = line.strip_prefix("Cookie:") {
                line = stripped.trim().to_string();
            }
            line
        })
        .collect();

    for line in lines.iter() {
        let fields: Vec<&str> = line.split("----").map(str::trim).collect();
        let mut compact_account = None;
        let (selector, username, cookie_text, inline_contact) = if let Ok(Some(parsed)) =
            parse_compact_account_line(
                line,
                &default_parts,
                &default_stores,
                payload.proxy_url.as_deref().unwrap_or(""),
                &default_province,
                &default_city,
                &default_district,
            ) {
            let contact = parsed.contact.clone();
            compact_account = Some(parsed);
            (
                compact_account
                    .as_ref()
                    .map(|account| account.username.clone())
                    .unwrap_or_default(),
                compact_account
                    .as_ref()
                    .map(|account| account.username.clone())
                    .unwrap_or_default(),
                compact_account
                    .as_ref()
                    .map(|account| account.wechat_cookie.clone())
                    .unwrap_or_default(),
                Some(contact),
            )
        } else if fields.len() == 2 {
            (
                fields[0].to_string(),
                fields[0].to_string(),
                fields[1].to_string(),
                None,
            )
        } else if fields.len() == 3 {
            (
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].to_string(),
                None,
            )
        } else if fields.len() == 8 {
            (
                fields[0].to_string(),
                fields[1].to_string(),
                fields[7].to_string(),
                Some(ContactConfig {
                    last_name: fields[2].to_string(),
                    first_name: fields[3].to_string(),
                    email: fields[4].to_string(),
                    phone: fields[5].to_string(),
                    national_id_suffix: fields[6].to_string(),
                }),
            )
        } else if lines.len() == 1 {
            let account_count = state.inner.config.lock().await.accounts.len();
            if account_count == 1 {
                let account_id = state.inner.config.lock().await.accounts[0].id.clone();
                (
                    account_id,
                    state.inner.config.lock().await.accounts[0].username.clone(),
                    line.clone(),
                    None,
                )
            } else {
                failed += 1;
                results.push(json!({
                    "ok": false,
                    "line": line,
                    "error": "格式应为：账号ID----Cookie；如果只有一个账号，也可以直接粘贴 Cookie"
                }));
                continue;
            }
        } else {
            failed += 1;
            results.push(json!({
                "ok": false,
                "line": line,
                    "error": "缺少账号ID，格式应为：账号ID----Cookie"
            }));
            continue;
        };

        let cookie_text = cookie_text.trim_start_matches("Cookie:").trim().to_string();
        let cookies = cookie_text
            .split(';')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .filter_map(|c| c.split_once('='))
            .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
            .collect::<HashMap<String, String>>();

        let mut config = state.inner.config.lock().await;
        let account_index = config
            .accounts
            .iter()
            .position(|a| a.id == selector || a.username == selector)
            .or_else(|| {
                config
                    .accounts
                    .iter()
                    .position(|a| a.username.to_lowercase() == selector.to_lowercase())
            });
        if let Some(index) = account_index {
            let account = &mut config.accounts[index];
            if let Some(inline_account) = compact_account.as_ref() {
                account.part = inline_account.part.clone();
                account.store_id = inline_account.store_id.clone();
                account.parts = inline_account.parts.clone();
                account.stores = inline_account.stores.clone();
                account.province = inline_account.province.clone();
                account.city = inline_account.city.clone();
                account.district = inline_account.district.clone();
                account.contact = inline_account.contact.clone();
            }
            let is_wechat_cookie = payload.cookie_type.as_deref() == Some("wechat")
                || (payload.cookie_type.is_none()
                    && cookies.contains_key("as_dima")
                    && cookies.contains_key("as_pvi"));
            if is_wechat_cookie {
                account.wechat_cookie = cookie_text.clone();
            } else {
                account.login_cookies.extend(cookies);
            }
            account.cookie_saved_at = Some(
                AppState::now_ms() as u64
            );
            let account_id = account.id.clone();
            let username = account.username.clone();
            let saved_cookies = account.login_cookies.clone();
            let wechat_cookie = account.wechat_cookie.clone();
            drop(config);
            if !wechat_cookie.trim().is_empty() {
                let _ = save_wechat_cookie_file(&account_id, &username, &wechat_cookie);
            } else {
                let _ = save_cookie_file(&account_id, &username, &saved_cookies);
            }
            state
                .log(
                    &account_id,
                    "info",
                    format!(
                        "批量导入 Cookie 成功：{}",
                        if is_wechat_cookie {
                            "微信小程序 Cookie"
                        } else {
                            "网页登录 Cookie"
                        }
                    ),
                )
                .await;
            results.push(json!({
                "ok": true,
                "account": username,
                "type": if is_wechat_cookie { "wechat" } else { "login" }
            }));
            updated += 1;
        } else {
            if !payload.create_missing || cookie_text.is_empty() {
                failed += 1;
                results.push(json!({
                    "ok": false,
                    "line": line,
                    "error": format!("没有找到账号：{selector}")
                }));
                continue;
            }

            let contact = inline_contact
                .clone()
                .or_else(|| payload.contact.clone())
                .unwrap_or_else(|| ContactConfig {
                    last_name: String::new(),
                    first_name: String::new(),
                    email: username.clone(),
                    phone: String::new(),
                    national_id_suffix: String::new(),
                });
            let mut account = if let Some(inline_account) = compact_account.clone() {
                inline_account
            } else {
                AccountConfig {
                    id: selector.clone(),
                    username: username.clone(),
                    password: String::new(),
                    proxy_url: payload.proxy_url.clone().unwrap_or_default(),
                    proxy_urls: Vec::new(),
                    part: default_parts[0].clone(),
                    store_id: default_stores[0].clone(),
                    parts: default_parts.clone(),
                    stores: default_stores.clone(),
                    province: default_province.clone(),
                    city: default_city.clone(),
                    district: default_district.clone(),
                    quantity: 1,
                    contact,
                    poll_delay_ms: None,
                    login_cookies: HashMap::new(),
                    wechat_cookie: String::new(),
                    cookie_saved_at: None,
                    orders_placed: 0,
                    last_order_number: None,
                    last_payment_link: None,
                    enabled: true,
                }
            };
            let is_wechat_cookie = payload.cookie_type.as_deref() == Some("wechat")
                || (payload.cookie_type.is_none()
                    && cookies.contains_key("as_dima")
                    && cookies.contains_key("as_pvi"));
            if is_wechat_cookie {
                account.wechat_cookie = cookie_text.clone();
            } else {
                account.login_cookies = cookies.clone();
            }
            account.cookie_saved_at = Some(
                AppState::now_ms() as u64
            );
            account.normalize();
            let account_id = account.id.clone();
            let account_username = account.username.clone();
            let login_cookies = account.login_cookies.clone();
            let wechat_cookie = account.wechat_cookie.clone();
            config.accounts.push(account);
            drop(config);
            if !wechat_cookie.trim().is_empty() {
                let _ = save_wechat_cookie_file(&account_id, &account_username, &wechat_cookie);
            } else {
                let _ = save_cookie_file(&account_id, &account_username, &login_cookies);
            }
            state
                .log(
                    &account_id,
                    "info",
                    format!(
                        "Cookie 导入并创建账号成功：{}",
                        if is_wechat_cookie {
                            "微信小程序 Cookie"
                        } else {
                            "网页 Cookie"
                        }
                    ),
                )
                .await;
            results.push(json!({
                "ok": true,
                "account": account_username,
                "type": if is_wechat_cookie { "wechat" } else { "login" },
                "created": true
            }));
            created += 1;
        }
    }
    let _ = state.persist_config().await;
    Json(json!({
        "ok": updated > 0 || created > 0,
        "updated": updated,
        "created": created,
        "failed": failed,
        "results": results
    }))
    .into_response()
}

pub async fn api_delete_accounts(
    State(state): State<AppState>,
    Json(payload): Json<BatchDeleteAccounts>,
) -> StatusCode {
    if payload.ids.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    {
        let mut handles = state.inner.handles.lock().await;
        for id in &payload.ids {
            if let Some(handle) = handles.remove(id) {
                handle.abort();
            }
        }
    }
    {
        let mut config = state.inner.config.lock().await;
        config.accounts.retain(|a| !payload.ids.contains(&a.id));
    }
    {
        let mut statuses = state.inner.statuses.lock().await;
        for id in &payload.ids {
            statuses.remove(id);
        }
    }
    // 清理账号相关的内存状态，避免长时间运行内存累积
    {
        let mut locks = state.inner.account_locks.lock().await;
        for id in &payload.ids {
            locks.remove(id);
        }
    }
    {
        let mut attempts = state.inner.account_attempts.lock().await;
        for id in &payload.ids {
            attempts.remove(id);
        }
    }
    {
        let mut retry = state.inner.account_retry_after_ms.lock().await;
        for id in &payload.ids {
            retry.remove(id);
        }
    }
    {
        let mut health = state.inner.proxy_health.lock().await;
        for id in &payload.ids {
            health.remove(id);
        }
    }
    {
        let mut prepared = state.inner.prepared_wechat_orders.lock().await;
        for id in &payload.ids {
            prepared.remove(id);
        }
    }
    // 清理代理绑定：否则已删账号占用的 IP 会永久占位，新账号只能撞车共用同一 IP
    {
        let mut assign = state.inner.account_proxy_assign.lock().await;
        let mut static_assign = state.inner.static_proxy_assign.lock().await;
        for id in &payload.ids {
            assign.remove(id);
            static_assign.remove(id);
        }
    }
    let _ = state.persist_config().await;
    StatusCode::OK
}

pub async fn api_apply_account_bulk(
    State(state): State<AppState>,
    Json(payload): Json<BulkAccountConfig>,
) -> impl IntoResponse {
    let parts: Vec<String> = payload
        .parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    let stores: Vec<String> = payload
        .stores
        .into_iter()
        .map(|store| store.trim().to_string())
        .filter(|store| !store.is_empty())
        .collect();
    if parts.is_empty() {
        return Json(json!({"ok": false, "error": "请至少选择一个手机型号"})).into_response();
    }
    if stores.is_empty() {
        return Json(json!({"ok": false, "error": "请至少选择一个门店"})).into_response();
    }

    let updated = {
        let mut config = state.inner.config.lock().await;
        for account in config.accounts.iter_mut() {
            account.part = parts[0].clone();
            account.parts = parts.clone();
            account.store_id = stores[0].clone();
            account.stores = stores.clone();
            account.proxy_url = payload.proxy_url.clone();
            account.proxy_urls = payload.proxy_urls.clone();
            account.province = payload.province.clone();
            account.city = payload.city.clone();
            account.district = payload.district.clone();
            account.quantity = payload.quantity;
            account.poll_delay_ms = payload.poll_delay_ms;
        }
        config.accounts.len()
    };
    let _ = state.persist_config().await;
    state.sync_statuses().await;
    state
        .log(
            "batch",
            "info",
            format!(
                "统一账号配置完成：{} 个账号 · {} 个型号 · {} 个门店",
                updated,
                parts.len(),
                stores.len()
            ),
        )
        .await;
    Json(json!({"ok": true, "updated": updated})).into_response()
}

pub async fn api_start(State(state): State<AppState>) -> StatusCode {
    state.start_global_monitor().await;
    StatusCode::OK
}

/// 启动 watch 模式：每个账号建立一次「到店取货」结账态，然后持续刷新 select_store，
/// 一旦 spokes 出现 timeSlot（= 库存到货）就立即走完剩余步骤并提交。
pub async fn api_watch_start(
    State(state): State<AppState>,
    Json(payload): Json<WatchRequest>,
) -> impl IntoResponse {
    // 先停掉上一轮 watch（若在跑）
    if let Some(handle) = state.inner.handles.lock().await.remove(WATCH_HANDLE_KEY) {
        handle.abort();
    }

    let accounts = {
        let config = state.inner.config.lock().await;
        config
            .accounts
            .iter()
            .filter(|a| {
                a.enabled
                    && !a.wechat_cookie.trim().is_empty()
                    && (payload.account_ids.is_empty() || payload.account_ids.contains(&a.id))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    if accounts.is_empty() {
        return Json(json!({"ok": false, "error": "没有可用账号（需启用且有 Cookie）"})).into_response();
    }

    let settings = state.inner.config.lock().await.settings.clone();
    let interval_ms = settings.watch_interval_ms.max(200);
    let max_refresh = settings.watch_max_refresh;
    let watch_state = state.clone();
    let account_count = accounts.len();

    let handle = tokio::spawn(async move {
        // 每个账号：先建结账态（清空购物车 + 加购 1 个型号 + 走到 select_store）
        let mut ready: Vec<(String, String, String, Value, String, Value)> = Vec::new();
        for account in &accounts {
            let proxy = watch_state
                .resolve_account_proxy(account)
                .await
                .unwrap_or_default();
            // 多型号时随机挑一个（用户要求：多型号随便选一个加入购物车）
            let parts = if account.parts.is_empty() {
                vec![account.part.clone()]
            } else {
                account.parts.clone()
            };
            let pick = if parts.len() == 1 {
                0
            } else {
                use rand::Rng;
                rand::thread_rng().gen_range(0..parts.len())
            };
            let part = parts[pick].clone();
            // 联系人随快照一起传递：watch-finish 的 save_pickup_contact 需要
            let contact = json!({
                "lastName": account.contact.last_name,
                "firstName": account.contact.first_name,
                "emailAddress": account.contact.email,
                "fullDaytimePhone": account.contact.phone,
                "nationalId": account.contact.national_id_suffix,
            });
            let store = if account.store_id.trim().is_empty() {
                account.stores.first().cloned().unwrap_or_default()
            } else {
                account.store_id.clone()
            };
            watch_state
                .log(
                    &account.id,
                    "info",
                    format!("watch 建立结账态：{part} · {store}（出口 {proxy_display}）", proxy_display = display_proxy(&proxy)),
                )
                .await;
            let result = run_wechat_bridge(json!({
                "mode": "watch-setup",
                "cookie": account.wechat_cookie,
                "part": part,
                "store": store,
                "contact": json!({
                    "lastName": account.contact.last_name,
                    "firstName": account.contact.first_name,
                    "emailAddress": account.contact.email,
                    "fullDaytimePhone": account.contact.phone,
                    "nationalId": account.contact.national_id_suffix,
                }),
                "proxy": proxy,
                "debug": false,
            }))
            .await;
            if result.get("ok") == Some(&json!(true)) {
                watch_state
                    .persist_wechat_cookie_from_result(&account.id, &result)
                    .await;
                let state_snapshot = json!({
                    "cookies": result.pointer("/result/cookies").cloned().unwrap_or(json!({})),
                    "landing": result.pointer("/result/landing").cloned().unwrap_or(json!({})),
                    "line_item_id": result.pointer("/result/line_item_id").cloned().unwrap_or(json!(null)),
                    "stk": result.pointer("/result/stk").cloned().unwrap_or(json!(null)),
                    "model": result.pointer("/result/model").cloned().unwrap_or(json!("mobileWeChatCheckout")),
                    "session_id": result.pointer("/result/session_id").cloned().unwrap_or(json!(null)),
                    "part": part,
                    "store": store,
                    "proxy_url": proxy,
                });
                ready.push((account.id.clone(), part.clone(), store.clone(), state_snapshot, proxy, contact));
                watch_state
                    .log(&account.id, "info", format!("watch 结账态就绪：{part} · {store}，开始守候 timeSlot"))
                    .await;
            } else {
                let err = result.get("error").and_then(Value::as_str).unwrap_or("未知错误");
                watch_state
                    .log(&account.id, "error", format!("watch 建立结账态失败：{err}"))
                    .await;
            }
        }

        if ready.is_empty() {
            watch_state.log("batch", "error", "watch 无可守候账号，任务退出").await;
            return;
        }
        watch_state
            .log(
                "batch",
                "info",
                format!("watch 已启动：{}/{} 个账号就绪，刷新间隔 {interval_ms}ms", ready.len(), account_count),
            )
            .await;

        // 持续刷新
        let mut rounds: u64 = 0;
        loop {
            rounds += 1;
            if max_refresh > 0 && rounds > max_refresh {
                watch_state
                    .log("batch", "info", format!("watch 达到最大刷新次数 {max_refresh}，自动停止"))
                    .await;
                break;
            }
            let mut still: Vec<(String, String, String, Value, String, Value)> = Vec::new();
            for (account_id, part, store, state_snapshot, proxy, contact) in ready.into_iter() {

                let result = run_wechat_bridge(json!({
                    "mode": "watch-refresh",
                    "cookie": "",
                    "part": part,
                    "store": store,
                    "contact": contact,
                    "state": state_snapshot,
                    "proxy": proxy,
                    "debug": false,
                }))
                .await;
                if result.get("ok") != Some(&json!(true)) {
                    let err = result.get("error").and_then(Value::as_str).unwrap_or("未知错误");
                    watch_state
                        .log(&account_id, "warn", format!("watch 刷新失败（跳过本轮）：{err}"))
                        .await;
                    still.push((account_id, part, store, state_snapshot, proxy, contact.clone()));
                    continue;
                }
                let has_slot = result
                    .pointer("/result/has_time_slot")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !has_slot {
                    still.push((account_id, part, store, state_snapshot, proxy, contact.clone()));
                    continue;
                }
                // 命中：立即结账
                watch_state
                    .log(&account_id, "info", format!("watch 命中库存：{part} · {store}，立即提交订单"))
                    .await;
                let finish = run_wechat_bridge(json!({
                    "mode": "watch-finish",
                    "cookie": "",
                    "part": part,
                    "store": store,
                    "contact": contact,
                    "state": state_snapshot,
                    "proxy": proxy,
                    "debug": false,
                }))
                .await;
                if finish.get("ok") == Some(&json!(true)) {
                    watch_state
                        .persist_wechat_cookie_from_result(&account_id, &finish)
                        .await;
                    let order_number = finish
                        .pointer("/summary/order_number")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let payment_link = finish
                        .pointer("/summary/order_url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    watch_state
                        .log(
                            &account_id,
                            "info",
                            format!("watch 下单成功：{part} · {store} · 订单 {order_number}"),
                        )
                        .await;
                    watch_state
                        .update_status(&account_id, |status| {
                            status.status = "order placed".to_string();
                            status.orders_placed += 1;
                            if !order_number.is_empty() {
                                status.last_order_number = Some(order_number.clone());
                            }
                            if !payment_link.is_empty() {
                                status.last_payment_link = Some(payment_link.clone());
                            }
                            status.last_error = None;
                        })
                        .await;
                } else {
                    let err = finish.get("error").and_then(Value::as_str).unwrap_or("未知错误");
                    watch_state
                        .log(&account_id, "error", format!("watch 下单失败：{err}"))
                        .await;
                    // 失败则继续守候（结账态可能已被消费，下一轮会重建）
                    still.push((account_id, part, store, state_snapshot, proxy, contact.clone()));
                }
            }
            if still.is_empty() {
                watch_state.log("batch", "info", "watch 所有账号已完成，任务结束").await;
                break;
            }
            ready = still;
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    });

    state
        .inner
        .handles
        .lock()
        .await
        .insert(WATCH_HANDLE_KEY.to_string(), handle);
    Json(json!({"ok": true, "accounts": account_count, "interval_ms": interval_ms})).into_response()
}

/// 停止 watch 模式
pub async fn api_watch_stop(State(state): State<AppState>) -> impl IntoResponse {
    let stopped = if let Some(handle) = state.inner.handles.lock().await.remove(WATCH_HANDLE_KEY) {
        handle.abort();
        true
    } else {
        false
    };
    state
        .log(
            "batch",
            "info",
            if stopped { "watch 已停止" } else { "watch 未在运行" },
        )
        .await;
    Json(json!({"ok": true, "stopped": stopped})).into_response()
}

/// 查询 watch 运行状态
pub async fn api_watch_status(State(state): State<AppState>) -> impl IntoResponse {
    // 句柄可能已结束但还没被 CleanupTask 回收，用 is_finished() 判定真实运行状态。
    let mut handles = state.inner.handles.lock().await;
    let running = match handles.get(WATCH_HANDLE_KEY) {
        Some(handle) if !handle.is_finished() => true,
        Some(_) => {
            // 已结束：顺手回收句柄
            handles.remove(WATCH_HANDLE_KEY);
            false
        }
        None => false,
    };
    drop(handles);
    Json(json!({"ok": true, "running": running})).into_response()
}

pub async fn api_stop(State(state): State<AppState>) -> StatusCode {
    {
        let mut handles = state.inner.handles.lock().await;
        for (key, handle) in handles.iter() {
            if key != SESSION_KEEPER_HANDLE_KEY {
                handle.abort();
            }
        }
        let session_keeper = handles.remove(SESSION_KEEPER_HANDLE_KEY);
        handles.clear();
        if let Some(handle) = session_keeper {
            handles.insert(SESSION_KEEPER_HANDLE_KEY.to_string(), handle);
        }
    }
    drop_monitor_state(state).await;
    StatusCode::OK
}

async fn drop_monitor_state(state: AppState) {
    // 释放本轮的动态代理分配：这些 IP 已从池中取出，若不清理会永久丢失，
    // 100+ 账号反复启停后池容量会被"幽灵占用"吃光，导致后续账号拿不到 IP。
    // 这里会同时清掉静态 IP 的缓存映射：静态分配本身是确定性的（账号顺序 +
    // 占用检查），下次 resolve 仍会得到同一个 IP；而动态池的映射清掉后，
    // 那些被取走的 IP 才算真正释放，池能重新回到上限容量。
    {
        let mut assign = state.inner.account_proxy_assign.lock().await;
        let released = assign.len();
        assign.clear();
        if released > 0 {
            state
                .log(
                    "batch",
                    "info",
                    format!("已释放 {released} 个账号的代理绑定"),
                )
                .await;
        }
    }
    state.inner.central_ordering_accounts.lock().await.clear();
    state.inner.central_completed_accounts.lock().await.clear();
    state.inner.account_attempts.lock().await.clear();
    state.inner.account_retry_after_ms.lock().await.clear();
    let mut statuses = state.inner.statuses.lock().await;
    for status in statuses.values_mut() {
        status.running = false;
        status.status = "stopped".to_string();
    }
    drop(statuses);
    state
        .log("batch", "info", "中央监控已停止：所有监控和下单任务已停止")
        .await;
}

pub async fn api_clear_completed_orders(State(state): State<AppState>) -> impl IntoResponse {
    let completed = {
        let mut completed = state.inner.central_completed_accounts.lock().await;
        let count = completed.len();
        completed.clear();
        count
    };
    state.inner.account_attempts.lock().await.clear();
    state.inner.account_retry_after_ms.lock().await.clear();
    state
        .log(
            "batch",
            "info",
            format!("已清空 {completed} 个账号的“已完成”状态，账号将继续参与本轮监控下单"),
        )
        .await;
    Json(json!({"ok": true, "cleared": completed})).into_response()
}

pub async fn api_add_account(
    State(state): State<AppState>,
    Json(mut account): Json<AccountConfig>,
) -> StatusCode {
    account.normalize();
    if account.id.trim().is_empty() {
        account.id = format!(
            "acc-{}",
            AppState::now_ms()
        );
    }
    {
        let mut config = state.inner.config.lock().await;
        config.accounts.retain(|e| e.id != account.id);
        config.accounts.push(account.clone());
    }
    let _ = state.persist_config().await;
    state.sync_statuses().await;
    StatusCode::OK
}

pub async fn api_update_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut account): Json<AccountConfig>,
) -> StatusCode {
    account.id = id.clone();
    account.normalize();
    {
        let mut config = state.inner.config.lock().await;
        let index = config.accounts.iter().position(|e| e.id == id);
        match index {
            Some(i) => config.accounts[i] = account.clone(),
            None => config.accounts.push(account.clone()),
        }
    }
    // 账号配置（型号/门店）可能被改动：清除"已完成"与旧会话，
    // 否则该账号会一直被视为"已下过单"而不再参与监控。
    {
        state.inner.central_completed_accounts.lock().await.remove(&id);
        let mut prepared = state.inner.prepared_wechat_orders.lock().await;
        prepared.remove(&id);
    }
    let _ = state.persist_config().await;
    state.sync_statuses().await;
    StatusCode::OK
}

pub async fn api_start_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let accounts = state.inner.config.lock().await.accounts.clone();
    match accounts.into_iter().find(|a| a.id == id) {
        Some(account) => {
            state.start_account_by_config(account).await;
            StatusCode::OK
        }
        None => StatusCode::NOT_FOUND,
    }
}

pub async fn api_stop_account(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    let mut handles = state.inner.handles.lock().await;
    if let Some(handle) = handles.remove(&id) {
        handle.abort();
    }
    drop(handles);
    state
        .update_status(&id, |s| {
            s.running = false;
            s.status = "stopped".to_string();
        })
        .await;
    StatusCode::OK
}

pub async fn api_proxy_pool_stats(State(state): State<AppState>) -> Json<ProxyPoolStats> {
    Json(state.proxy_pool_stats().await)
}

pub async fn api_proxy_pool_clear(State(state): State<AppState>) -> Json<Value> {
    let n = state.inner.proxy_pool.clear().await;
    {
        let mut assign = state.inner.account_proxy_assign.lock().await;
        assign.clear();
    }
    state
        .log("batch", "info", format!("代理池已清空：移除 {n} 个 IP"))
        .await;
    Json(json!({"ok": true, "cleared": n}))
}

pub async fn api_proxy_pool_fetch(State(state): State<AppState>) -> Json<Value> {
    let s = state.inner.config.lock().await.settings.clone();
    if s.proxy_pool_api.trim().is_empty() {
        return Json(json!({"ok": false, "error": "代理池提取接口未配置"}));
    }
    let account_count = {
        let config = state.inner.config.lock().await;
        config.accounts.iter().filter(|a| a.enabled).count()
    };
    let max_size = AppState::calc_pool_max_size(&s, account_count);
    match ProxyPool::fetch_into_pool_with_num(
        &s.proxy_pool_api,
        &s.proxy_pool_format,
        max_size,
        None,
        &s.proxy_pool_fetch_proxy,
    )
    .await
    {
        Ok(list) => {
            let n = list.len();
            state.inner.proxy_pool.add(list, max_size).await;
            state.inner.proxy_pool.record_fetched(n).await;
            state
                .log("batch", "info", format!("手动补充代理池：新增 {n} 个 IP"))
                .await;
            Json(json!({"ok": true, "fetched": n}))
        }
        Err(e) => {
            state.inner.proxy_pool.record_fetch_error(e.clone()).await;
            Json(json!({"ok": false, "error": e}))
        }
    }
}

pub async fn api_update_settings(
    State(state): State<AppState>,
    Json(settings): Json<GlobalSettings>,
) -> StatusCode {
    {
        let mut config = state.inner.config.lock().await;
        config.settings = settings;
    }
    let _ = state.persist_config().await;
    StatusCode::OK
}


/// 代理测试请求体
#[derive(Debug, Deserialize)]
pub struct ProxyTestRequest {
    /// 待测代理列表（每行一个，支持 IP|端口|账号|密码 等格式）
    pub proxies: Vec<String>,
    /// 并发数（默认 8）
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// 单请求超时毫秒（默认 10000）
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// 单个测试目标
fn proxy_test_targets() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    // (类别, 名称, URL, 期望特征)
    vec![
        (
            "后端",
            "Apple官网",
            "https://www.apple.com.cn/",
            "apple",
        ),
        (
            "前端",
            "小程序库存",
            "https://web.mmap.apple.com/26.07.0-MAIN/miniprogram-merch/p/wechat/product-locator/quotes?pn=MJT74CH%2FA&stores=R480",
            "mmap",
        ),
        (
            "前端",
            "小程序袋子",
            "https://web.mmap.apple.com/moe/p/wechat/chat/bag",
            "mmap",
        ),
    ]
}

/// 测试单个代理的所有目标，返回明细与是否全部通过
async fn test_one_proxy(
    proxy_raw: &str,
    timeout_ms: u64,
) -> Value {
    let normalized = normalize_static_proxy(proxy_raw);
    if normalized.is_empty() {
        return json!({
            "proxy": proxy_raw,
            "ok": false,
            "error": "无法解析代理格式",
            "details": [],
        });
    }

    let proxy = match reqwest::Proxy::all(&normalized) {
        Ok(p) => p,
        Err(e) => {
            return json!({
                "proxy": proxy_raw,
                "ok": false,
                "error": format!("代理地址无效：{e}"),
                "details": [],
            })
        }
    };

    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .connect_timeout(std::time::Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Mozilla/5.0 (iPhone; CPU iPhone OS 16_7_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 MicroMessenger/8.0.79")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "proxy": proxy_raw,
                "ok": false,
                "error": format!("创建客户端失败：{e}"),
                "details": [],
            })
        }
    };

    let mut details: Vec<Value> = Vec::new();
    let mut all_ok = true;
    let mut max_ms: u64 = 0;

    for (kind, name, url, expect) in proxy_test_targets() {
        let started = std::time::Instant::now();
        let result = client.get(url).send().await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms > max_ms {
            max_ms = elapsed_ms;
        }

        let (status, note, ok) = match result {
            Ok(resp) => {
                let code = resp.status().as_u16();
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = resp.text().await.unwrap_or_default();

                if code == 407 {
                    (code, "代理认证失败".to_string(), false)
                } else if code == 403 {
                    (code, "被拒绝(403)".to_string(), false)
                } else if expect == "mmap" {
                    // 小程序接口：302 -> covers.html 是 Apple 预购等待页，代理本身没问题
                    if (code == 301 || code == 302) && location.contains("covers.html") {
                        (code, "预购等待期(Apple未开放)".to_string(), true)
                    } else if code == 301 || code == 302 {
                        (code, "OK(跳转)".to_string(), true)
                    } else if code == 200 {
                        (code, "OK".to_string(), true)
                    } else {
                        (code, format!("OK(需Cookie:{code})"), true)
                    }
                } else {
                    let low = body.to_lowercase();
                    if low.contains("apple") {
                        (code, "OK".to_string(), true)
                    } else if !body.is_empty() {
                        (code, "内容非Apple".to_string(), false)
                    } else {
                        (code, "空响应".to_string(), false)
                    }
                }
            }
            Err(e) => {
                let msg = if e.is_timeout() {
                    "连接超时".to_string()
                } else if e.is_connect() {
                    "连接失败".to_string()
                } else {
                    format!("请求失败：{}", e.to_string().chars().take(60).collect::<String>())
                };
                (0u16, msg, false)
            }
        };

        if !ok {
            all_ok = false;
        }
        details.push(json!({
            "kind": kind,
            "name": name,
            "status": status,
            "elapsed_ms": elapsed_ms,
            "note": note,
            "ok": ok,
        }));
    }

    json!({
        "proxy": proxy_raw,
        "normalized": normalized,
        "exit_ip": normalized
            .rsplit('@')
            .next()
            .and_then(|hp| hp.split(':').next())
            .unwrap_or(""),
        "ok": all_ok,
        "max_ms": max_ms,
        "details": details,
    })
}

/// POST /api/proxy/test —— 批量测试代理（前端 + 后端目标）
pub async fn api_proxy_test(
    State(state): State<AppState>,
    Json(payload): Json<ProxyTestRequest>,
) -> Json<Value> {
    let proxies: Vec<String> = payload
        .proxies
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();

    if proxies.is_empty() {
        return Json(json!({"ok": false, "error": "没有提供代理"}));
    }

    let concurrency = payload.concurrency.unwrap_or(8).clamp(1, 64);
    let timeout_ms = payload.timeout_ms.unwrap_or(10_000).clamp(2000, 60_000);

    state
        .log(
            "batch",
            "info",
            format!("开始测试 {} 个代理（并发 {concurrency}）", proxies.len()),
        )
        .await;

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    for raw in proxies {
        let sem = sem.clone();
        tasks.spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            test_one_proxy(&raw, timeout_ms).await
        });
    }

    let mut results: Vec<Value> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok(v) = joined {
            results.push(v);
        }
    }

    // 汇总
    let total = results.len();
    let ok_count = results
        .iter()
        .filter(|r| r.get("ok") == Some(&json!(true)))
        .count();
    let mut ok_proxies: Vec<&str> = results
        .iter()
        .filter(|r| r.get("ok") == Some(&json!(true)))
        .filter_map(|r| r.get("proxy").and_then(Value::as_str))
        .collect();
    // 按耗时排序
    ok_proxies.sort_by_key(|p| {
        results
            .iter()
            .find(|r| r.get("proxy").and_then(Value::as_str) == Some(*p))
            .and_then(|r| r.get("max_ms"))
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
    });

    state
        .log(
            "batch",
            "info",
            format!("代理测试完成：可用 {ok_count}/{total}"),
        )
        .await;

    Json(json!({
        "ok": true,
        "total": total,
        "available": ok_count,
        "unavailable": total.saturating_sub(ok_count),
        "ok_proxies": ok_proxies,
        "results": results,
    }))
}

#[cfg(test)]
mod app_tests {
    use super::*;

    // --- 下单 proxy 池换 IP 重试的触发条件 ---

    #[test]
    // --- 静态代理格式适配 ---

    #[test]
    fn static_proxy_pipe_format() {
        // 用户实际格式：IP|端口|账号|密码
        assert_eq!(
            normalize_static_proxy("96.8.121.134|11515|uhe|elz"),
            "http://uhe:elz@96.8.121.134:11515"
        );
    }

    #[test]
    fn static_proxy_pipe_without_auth() {
        assert_eq!(
            normalize_static_proxy("96.8.121.134|11515"),
            "http://96.8.121.134:11515"
        );
    }

    #[test]
    fn static_proxy_plain_and_colon_formats() {
        assert_eq!(normalize_static_proxy("1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(
            normalize_static_proxy("1.2.3.4:8080:uhe:elz"),
            "http://uhe:elz@1.2.3.4:8080"
        );
        assert_eq!(
            normalize_static_proxy("socks5://1.2.3.4:1080"),
            "socks5://1.2.3.4:1080"
        );
        assert_eq!(normalize_static_proxy(""), "");
    }

    #[test]
    fn static_proxy_credentials_are_encoded() {
        // 密码含特殊字符时需百分号编码，否则 URL 解析失败
        let out = normalize_static_proxy("1.2.3.4|8080|user@x|p@ss:w");
        assert!(out.starts_with("http://user%40x:p%40ss%3Aw@1.2.3.4:8080"), "got {out}");
    }

    #[test]
    // --- 时钟安全（系统时钟回拨不应导致 panic） ---

    #[test]
    fn now_ms_never_panics() {
        // 只要不 panic 即通过；返回值语义由调用方用 saturating_sub 兜底
        let _ = AppState::now_ms();
    }

    // --- WebSocket 变更检测哈希 ---

    #[test]
    fn fnv1a_hash_detects_change() {
        let a = crate::fnv1a_hash(b"{\"state\":1}");
        let b = crate::fnv1a_hash(b"{\"state\":2}");
        assert_ne!(a, b, "内容变化必须导致 hash 变化");
        assert_eq!(a, crate::fnv1a_hash(b"{\"state\":1}"), "相同内容 hash 必须一致");
    }

    #[test]
    fn fnv1a_hash_handles_empty() {
        // 空输入返回 offset basis，不与首次标记(first=true)逻辑冲突
        assert_eq!(crate::fnv1a_hash(b""), 0xcbf2_9ce4_8422_2325);
    }

    fn display_proxy_hides_credentials() {
        assert_eq!(display_proxy(""), "直连");
        assert_eq!(display_proxy("http://1.2.3.4:8080"), "1.2.3.4:8080");
        assert_eq!(
            display_proxy("http://uhe:elz@96.8.121.134:11515"),
            "96.8.121.134:11515（已认证）"
        );
        // 日志里绝不能出现账号或密码
        let shown = display_proxy("http://uhe:elz@96.8.121.134:11515");
        assert!(!shown.contains("uhe") && !shown.contains("elz"));
    }

    // --- 日志脱敏：任何路径都不能泄漏代理账号密码 ---

    #[test]
    fn display_proxy_covers_all_scheme_prefixes() {
        let cases = [
            ("http://u:p@1.2.3.4:8080", "1.2.3.4:8080（已认证）"),
            ("https://u:p@1.2.3.4:8080", "1.2.3.4:8080（已认证）"),
            ("socks5://u:p@1.2.3.4:1080", "1.2.3.4:1080（已认证）"),
            ("socks4://u:p@1.2.3.4:1080", "1.2.3.4:1080（已认证）"),
        ];
        for (input, expected) in cases {
            let shown = display_proxy(input);
            assert_eq!(shown, expected, "input={input}");
            assert!(!shown.contains("u:p"), "凭据泄漏: {shown}");
        }
    }

    #[test]
    fn normalized_static_proxy_never_logs_raw_credentials() {
        // 端到端：用户录入格式 → 规范化 → 日志显示，全程不出现账号密码
        let raw = "96.8.121.134|11515|uhe|elz";
        let normalized = normalize_static_proxy(raw);
        assert_eq!(normalized, "http://uhe:elz@96.8.121.134:11515");
        let shown = display_proxy(&normalized);
        assert_eq!(shown, "96.8.121.134:11515（已认证）");
        assert!(!shown.contains("uhe"));
        assert!(!shown.contains("elz"));
    }

    fn retry_on_apple_rate_limit_541() {
        assert!(is_proxy_retryable_order_error(
            "bag failed with 541: Apple 限流"
        ));
    }

    #[test]
    fn retry_on_service_unavailable_503() {
        assert!(is_proxy_retryable_order_error(
            "checkout_start failed with 503: <h1>Oops!</h1><pre>Service Unavailable</pre>"
        ));
    }

    #[test]
    fn retry_on_transport_failure() {
        assert!(is_proxy_retryable_order_error(
            "error sending request for url (https://web.mmap.apple.com/moe/p/wechat/v2/checkout)"
        ));
        assert!(is_proxy_retryable_order_error("operation timed out"));
        assert!(is_proxy_retryable_order_error(
            "connection reset by peer (proxy http://1.2.3.4:8080)"
        ));
    }

    // --- 会话重建 / 换代理 都必须排除 Apple 业务拒绝 ---

    #[test]
    fn business_rejection_is_not_session_error() {
        let result = json!({
            "error": "order not confirmed: pageType=-, orderNumber=-, detail={\"sorryKey\":\"quantity_limit_exceeded\"}"
        });
        assert!(!is_retryable_wechat_session_error(&result));
    }

    #[test]
    fn business_rejection_does_not_switch_proxy() {
        let msg = "order not confirmed: detail={\"alerts\":[{\"title\":\"无法完成处理订单\"}]}";
        assert!(!is_proxy_retryable_order_error(msg));
    }

    #[test]
    fn real_session_expiry_still_retries() {
        let result = json!({ "error": "save_time_slot failed with 403: " });
        assert!(is_retryable_wechat_session_error(&result));
    }

    #[test]
    fn rate_limit_still_switches_proxy() {
        assert!(is_proxy_retryable_order_error("bag failed with 541: "));
    }

    #[test]
    fn do_not_retry_on_business_failure() {
        // 订单未确认属于业务失败：换 IP 重试会重复下单，必须排除
        assert!(!is_proxy_retryable_order_error(
            "order not confirmed: sorryKey=quantity_limit_exceeded"
        ));
        assert!(!is_proxy_retryable_order_error(
            "pickup 缺少 selectPickupMethodUrl"
        ));
        assert!(!is_proxy_retryable_order_error(""));
    }

    #[test]
    fn do_not_retry_on_session_expired_403() {
        // 403 会话失效由「预会话回退全流程」分支处理，不应触发换 IP
        assert!(!is_proxy_retryable_order_error("save_time_slot failed with 403: "));
    }

    // --- 账号导入：id 必须是邮箱，备注重复不能互相覆盖 ---

    #[test]
    fn import_id_uses_email_not_remark() {
        // 四行备注完全相同（残枫），但邮箱不同
        let line = "a@x.com----as_dima=1; as_pvi=2----{\"phoneId\":[\"MK2N4CH/A\"],\"shopId\":[\"R476\"],\"email\":\"a@x.com\"}----残枫";
        let parts = vec!["MK2N4CH/A".to_string()];
        let stores = vec!["R476".to_string()];
        let acc = parse_compact_account_line(line, &parts, &stores, "", "重庆", "重庆", "")
            .expect("parse should not error")
            .expect("should produce an account");
        assert_eq!(acc.id, "a@x.com");
        assert_eq!(acc.username, "a@x.com");
    }

    // --- UTF-8 安全截断（防止多字节字符中间截断 panic）---

    #[test]
    fn truncate_utf8_never_panics_on_multibyte() {
        // 全中文：任意字节位置切都可能落在字符中间
        let s = "错误：".repeat(40);
        assert_eq!(s.len(), 360);
        // 257 落在多字节字符中间（旧实现会 panic）
        let cut = truncate_utf8(&s, 257);
        assert!(cut.len() <= 257);
        assert!(s.starts_with(cut));
        assert_eq!(truncate_utf8(&s, 300).len(), 300);
        // 覆盖所有截断长度都不 panic
        for n in 0..=s.len() + 5 {
            let _ = truncate_utf8(&s, n);
        }
    }

    #[test]
    fn truncate_utf8_passthrough_when_short() {
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("", 5), "");
        assert_eq!(truncate_utf8("错误", 0), "");
    }

    // --- 文件名安全化：不同 id 不得撞同一路径 ---

    #[test]
    fn safe_file_name_does_not_collide() {
        let a = safe_file_name("171997008@qq.com");
        let b = safe_file_name("171997008_qq_com");
        let c = safe_file_name("171997008@qq.com#2");
        assert_ne!(a, b, "邮箱与下划线形式不能撞名");
        assert_ne!(a, c, "同邮箱的不同序号不能撞名");
        assert_ne!(b, c);
        assert_eq!(a, safe_file_name("171997008@qq.com"));
        assert_eq!(safe_file_name("1"), "1");
        assert_eq!(safe_file_name("abc-123_x"), "abc-123_x");
    }

    // --- 订单上传：Cookie 提取 ---

    #[test]
    fn pick_cookie_extracts_named_pair() {
        let c = "as_dima=ee11b12; as_pvi=cc9e1; site=CHN";
        assert_eq!(pick_cookie_value(c, "as_dima"), "as_dima=ee11b12");
        assert_eq!(pick_cookie_value(c, "as_pvi"), "as_pvi=cc9e1");
        assert_eq!(pick_cookie_value(c, "site"), "site=CHN");
    }

    #[test]
    fn pick_cookie_handles_missing_and_spacing() {
        let c = "as_dima = x1 ;  as_pvi=y2";
        assert_eq!(pick_cookie_value(c, "as_dima"), "as_dima = x1");
        assert_eq!(pick_cookie_value(c, "as_pvi"), "as_pvi=y2");
        assert_eq!(pick_cookie_value(c, "nope"), "");
        assert_eq!(pick_cookie_value("", "as_dima"), "");
    }
}
