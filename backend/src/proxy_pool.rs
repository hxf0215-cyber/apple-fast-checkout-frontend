//! 代理池：动态提取、测活、按账号分配、用完即弃
//!
//! 设计要点：
//! - 池上限 = 启用账号数 × proxy_pool_per_account
//! - 每 proxy_check_interval_ms 对池内 IP 做一次并发测活
//! - 单个 IP 存活超过 proxy_max_age_ms 自动丢弃
//! - 池内可用数量低于 proxy_pool_low_watermark 时自动补充
//! - IP 被账号取用后立即从池中移除（不回池）
//! - 提取时最多使用 proxy_fetch_concurrency 个并发任务

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Mutex;

/// 池中一个可用代理
#[derive(Clone, Debug)]
pub struct PoolProxy {
    /// 完整代理地址，如 http://1.2.3.4:5678 或 socks5://1.2.3.4:5678
    pub url: String,
    /// 入池时间
    pub added_at: Instant,
    /// 最近一次测活是否成功
    pub alive: bool,
    /// 最近一次测活时间；None = 入池后从未测过（必须先验证才能被取用）
    pub last_checked_at: Option<Instant>,
}

/// 代理池状态快照（供 /api/state 输出）
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ProxyPoolStats {
    pub enabled: bool,
    pub total: usize,
    pub alive: usize,
    pub max_size: usize,
    pub fetched_total: u64,
    pub discarded_total: u64,
    pub check_failures: u64,
    pub last_fetch_error: Option<String>,
}

#[derive(Default)]
pub struct ProxyPoolInner {
    pub proxies: HashMap<String, PoolProxy>,
    pub fetched_total: u64,
    pub discarded_total: u64,
    pub check_failures: u64,
    pub last_fetch_error: Option<String>,
}

#[derive(Clone)]
pub struct ProxyPool {
    inner: Arc<Mutex<ProxyPoolInner>>,
}

impl ProxyPool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ProxyPoolInner::default())),
        }
    }

    pub async fn stats(&self, max_size: usize, enabled: bool) -> ProxyPoolStats {
        let g = self.inner.lock().await;
        ProxyPoolStats {
            enabled,
            total: g.proxies.len(),
            alive: g.proxies.values().filter(|p| p.alive).count(),
            max_size,
            fetched_total: g.fetched_total,
            discarded_total: g.discarded_total,
            check_failures: g.check_failures,
            last_fetch_error: g.last_fetch_error.clone(),
        }
    }

    /// 从提取接口拉取一批代理并加入池中（不测活，交给测活循环）
    pub async fn fetch_into_pool(
        api_url: &str,
        format: &str,
        max_size: usize,
    ) -> Result<Vec<String>, String> {
        Self::fetch_into_pool_with_num(api_url, format, max_size, None, "").await
    }

    /// 带 num 覆盖与自定义出口代理的提取
    /// - num_override：替换 URL 中的 num 参数
    /// - fetch_proxy：调用提取接口时使用的代理（为空则直连），用于绕过接口方白名单
    pub async fn fetch_into_pool_with_num(
        api_url: &str,
        format: &str,
        max_size: usize,
        num_override: Option<usize>,
        fetch_proxy: &str,
    ) -> Result<Vec<String>, String> {
        if api_url.trim().is_empty() {
            return Err("代理池提取接口为空".to_string());
        }
        // 动态改写 num 参数，避免一次拉取过量 IP 浪费配额
        let request_url = match num_override {
            Some(n) if n > 0 => replace_query_param(api_url, "num", &n.to_string()),
            _ => api_url.to_string(),
        };
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(15));
        // 若配置了出口代理，则通过它请求提取接口（绕过白名单限制）
        if !fetch_proxy.trim().is_empty() {
            match reqwest::Proxy::all(fetch_proxy.trim()) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(e) => return Err(format!("代理地址无效（{fetch_proxy}）：{e}")),
            }
        }
        let client = builder.build().map_err(|e| e.to_string())?;
        let text = client
            .get(&request_url)
            .send()
            .await
            .map_err(|e| format!("请求代理接口失败：{e}"))?
            .text()
            .await
            .map_err(|e| format!("读取代理接口响应失败：{e}"))?;

        let mut out: Vec<String> = Vec::new();
        if format.eq_ignore_ascii_case("json") {
            // 期望 {"data":["ip:port", ...]} 或 ["ip:port", ...]
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                let arr = v
                    .get("data")
                    .and_then(Value::as_array)
                    .or_else(|| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                for item in arr {
                    if let Some(s) = item.as_str() {
                        out.push(normalize_proxy(s));
                    }
                }
            }
        } else {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') {
                    continue;
                }
                // 支持一行多个，逗号或分号分隔
                for part in l.split([',', ';']) {
                    let p = part.trim();
                    if !p.is_empty() {
                        out.push(normalize_proxy(p));
                    }
                }
            }
        }

        // 过滤出合法代理地址（协议://IP:PORT 或 IP:PORT）
        out.retain(|p| is_valid_proxy(p));
        // 若一条都没解析出来，且响应里含中文错误提示，则作为错误返回
        if out.is_empty() {
            let trimmed = text.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('[') && !trimmed.starts_with('{') {
                let msg: String = trimmed.chars().take(160).collect();
                return Err(format!("提取接口未返回有效代理：{msg}"));
            }
            return Err("提取接口未返回有效代理（可能是白名单未配置或配额用尽）".to_string());
        }
        out.truncate(max_size.max(1));
        Ok(out)
    }

    /// 把代理加入池（已存在则跳过），受 max_size 限制
    pub async fn add(&self, urls: Vec<String>, max_size: usize) {
        // 统一规范化，避免调用方传入未带协议前缀的地址导致 key 不一致
        let urls: Vec<String> = urls
            .into_iter()
            .map(|u| normalize_proxy(&u))
            .filter(|u| !u.is_empty())
            .collect();
        let mut g = self.inner.lock().await;
        for u in urls {
            if g.proxies.len() >= max_size.max(1) {
                break;
            }
            // 新加入的 IP 未测活，标记 alive=false（take_one 不会取用）
            g.proxies.entry(u.clone()).or_insert(PoolProxy {
                url: u,
                added_at: Instant::now(),
                alive: false,
                last_checked_at: None,
            });
        }
    }

    /// 取一个可用代理给账号使用，取出后从池中移除
    /// 取出一个【已测活通过】的代理；没有可用 IP 时返回 None。
    /// 注意：绝不回退到未测活的 IP，避免把不可用代理交给下单流程。
    pub async fn take_one(&self) -> Option<String> {
        let mut g = self.inner.lock().await;
        // 取值策略：优先「入池最久」的可用 IP（FIFO）。
        // HashMap 的迭代顺序不确定，100+ 账号并发取用时会随机命中，
        // 导致部分 IP 被反复使用、部分闲置。按入池时间排序可让 IP 轮转均匀，
        // 同时优先消耗即将到期的 IP（更符合「用完即弃」的语义）。
        let key = g
            .proxies
            .iter()
            .filter(|(_, p)| p.alive)
            .min_by_key(|(_, p)| p.added_at)
            .map(|(k, _)| k.clone())?;
        g.proxies.remove(&key).map(|p| {
            g.discarded_total += 1;
            p.url
        })
    }

    /// 按需测活：只探测「必须验证」的代理，其余跳过以省流量与 CPU。
    ///
    /// 需要探测的条件（任一命中）：
    /// - 从未测过（新入池，不测不允许取用）
    /// - 上次探测失败（给一次复活机会）
    /// - 距过期不足 refresh_before_ms（确认剩余有效期内仍可用）
    /// 其余健康且离过期还早的 IP 直接复用上次结果。
    ///
    /// 无论是否探测，都会清理已过期的 IP。
    /// 返回 (当前存活数, 本次剔除数)。
    pub async fn health_check(
        &self,
        check_url: &str,
        timeout_ms: u64,
        max_age_ms: u64,
        concurrency: usize,
    ) -> (usize, usize, usize, usize) {
        // 未指定复测提前量时，默认用存活期的 1/3（例如 30s → 10s）
        let refresh_before_ms = (max_age_ms / 3).max(1);
        self.health_check_due(check_url, timeout_ms, max_age_ms, concurrency, refresh_before_ms)
            .await
    }

    /// health_check 的完整版本，可显式指定「距过期多久内需要复测」。
    pub async fn health_check_due(
        &self,
        check_url: &str,
        timeout_ms: u64,
        max_age_ms: u64,
        concurrency: usize,
        refresh_before_ms: u64,
    ) -> (usize, usize, usize, usize) {
        // 第一阶段：锁内完成「清理过期 + 挑选待测」，避免探测期间占着锁
        let (to_probe, expired_count, _alive_before, total_before) = {
            let mut g = self.inner.lock().await;
            let now = Instant::now();

            // 1) 清理超过 max_age_ms 的条目
            let expired: Vec<String> = g
                .proxies
                .iter()
                .filter(|(_, p)| now.duration_since(p.added_at).as_millis() as u64 > max_age_ms)
                .map(|(k, _)| k.clone())
                .collect();
            for url in &expired {
                g.proxies.remove(url);
                g.discarded_total += 1;
            }

            // 2) 挑选需要探测的条目
            let mut to_probe: Vec<String> = Vec::new();
            for (url, p) in g.proxies.iter() {
                let age_ms = now.duration_since(p.added_at).as_millis() as u64;
                let remaining_ms = max_age_ms.saturating_sub(age_ms);
                let never_checked = p.last_checked_at.is_none();
                let unhealthy = !p.alive;
                let near_expiry = remaining_ms <= refresh_before_ms;
                if never_checked || unhealthy || near_expiry {
                    to_probe.push(url.clone());
                }
            }
            let alive_before = g.proxies.values().filter(|p| p.alive).count();
            let total_before = g.proxies.len();
            (to_probe, expired.len(), alive_before, total_before)
        };

        let mut dead: Vec<String> = Vec::new();
        let probed = to_probe.len();
        // 全部命中「无需探测」：直接返回当前状态
        if !to_probe.is_empty() {
            let sem = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
            let mut tasks = tokio::task::JoinSet::new();
            let probe_total = to_probe.len();
            for url in to_probe {
                let sem = sem.clone();
                let check_url = check_url.to_string();
                tasks.spawn(async move {
                    let _permit = sem.acquire_owned().await.ok()?;
                    let ok = probe_proxy(&url, &check_url, timeout_ms).await;
                    Some((url, ok))
                });
            }

            let mut probed_alive: Vec<String> = Vec::new();
            while let Some(res) = tasks.join_next().await {
                if let Ok(Some((url, ok))) = res {
                    if ok {
                        probed_alive.push(url);
                    } else {
                        dead.push(url);
                    }
                }
            }

            {
                let mut g = self.inner.lock().await;
                let now = Instant::now();
                for url in &probed_alive {
                    if let Some(p) = g.proxies.get_mut(url) {
                        p.alive = true;
                        p.last_checked_at = Some(now);
                    }
                }
                for url in &dead {
                    if let Some(p) = g.proxies.get_mut(url) {
                        p.alive = false;
                        p.last_checked_at = Some(now);
                    }
                }
                g.check_failures += dead.len() as u64;
            }
            let _ = probe_total;
        }

        let alive_total = {
            let g = self.inner.lock().await;
            g.proxies.values().filter(|p| p.alive).count()
        };
        let skipped = total_before.saturating_sub(probed);
        (alive_total, expired_count, probed, skipped)
    }

    /// 池内健康数量
    pub async fn alive_count(&self) -> usize {
        let g = self.inner.lock().await;
        g.proxies.values().filter(|p| p.alive).count()
    }

    pub async fn total_count(&self) -> usize {
        let g = self.inner.lock().await;
        g.proxies.len()
    }

    /// 清空池
    pub async fn clear(&self) -> usize {
        let mut g = self.inner.lock().await;
        let n = g.proxies.len();
        g.proxies.clear();
        g.discarded_total += n as u64;
        n
    }

    pub async fn record_fetch_error(&self, err: String) {
        let mut g = self.inner.lock().await;
        g.last_fetch_error = Some(err);
    }

    pub async fn record_fetched(&self, n: usize) {
        let mut g = self.inner.lock().await;
        g.fetched_total += n as u64;
        g.last_fetch_error = None;
    }
}

/// 替换 URL 查询参数中的指定键值（不存在则追加）
fn replace_query_param(url: &str, key: &str, value: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, q),
        None => return format!("{url}?{key}={value}"),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut replaced = false;
    for seg in query.split('&') {
        if seg.is_empty() {
            continue;
        }
        let k = seg.split('=').next().unwrap_or("");
        if k.eq_ignore_ascii_case(key) {
            parts.push(format!("{key}={value}"));
            replaced = true;
        } else {
            parts.push(seg.to_string());
        }
    }
    if !replaced {
        parts.push(format!("{key}={value}"));
    }
    format!("{base}?{}", parts.join("&"))
}

/// 校验是否为合法代理地址：协议://host:port 或 host:port
fn is_valid_proxy(s: &str) -> bool {
    let addr = s
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("socks5://")
        .trim_start_matches("socks4://");
    // 必须形如 host:port，且 port 为纯数字
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // host 只能是 IP 或域名（不允许中文/空格）
    host.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// 补全代理协议前缀
fn normalize_proxy(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("socks5://")
        || s.starts_with("socks4://")
    {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

/// 用给定代理访问 check_url，判断是否可用
async fn probe_proxy(proxy_url: &str, check_url: &str, timeout_ms: u64) -> bool {
    let proxy = match reqwest::Proxy::all(proxy_url) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_millis(timeout_ms.max(1000)))
        .connect_timeout(Duration::from_millis(timeout_ms.max(1000)))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(client.get(check_url).send().await, Ok(r) if r.status().is_success() || r.status().is_redirection())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_query_param_rewrites_num() {
        let url = "http://api.test/get?uid=1&num=200&type=7";
        let out = replace_query_param(url, "num", "5");
        assert!(out.contains("num=5"));
        assert!(!out.contains("num=200"));
        assert!(out.contains("uid=1"));
    }

    #[test]
    fn replace_query_param_appends_when_absent() {
        let out = replace_query_param("http://api.test/get?uid=1", "num", "8");
        assert!(out.ends_with("num=8"));
    }

    #[test]
    fn is_valid_proxy_accepts_common_forms() {
        assert!(is_valid_proxy("1.2.3.4:8080"));
        assert!(is_valid_proxy("http://1.2.3.4:8080"));
        assert!(is_valid_proxy("socks5://1.2.3.4:1080"));
        assert!(!is_valid_proxy("1.2.3.4"));
        assert!(!is_valid_proxy("1.2.3.4:abc"));
        assert!(!is_valid_proxy(""));
    }

    /// 按需测活的核心不变量：
    /// 新入池的 IP 必须被标记为「未测活」，因此不会被 take_one 取走。
    #[tokio::test]
    async fn new_proxy_starts_unchecked_and_unavailable() {
        let pool = ProxyPool::new();
        pool.add(vec!["1.2.3.4:8080".to_string()], 10).await;

        let g = pool.inner.lock().await;
        let p = g.proxies.get("http://1.2.3.4:8080").expect("已入池");
        assert!(!p.alive, "新入池不能直接标记为可用");
        assert!(p.last_checked_at.is_none(), "新入池必须是未测活状态");
        drop(g);

        // 未测活 → 不允许取用（避免把不可用代理交给下单流程）
        assert!(pool.take_one().await.is_none());
    }

    /// 过期项必须被剔除，且不会因「跳过探测」而残留。
    #[tokio::test]
    async fn expired_proxies_are_removed() {
        let pool = ProxyPool::new();
        pool.add(vec!["1.2.3.4:8080".to_string()], 10).await;
        {
            let mut g = pool.inner.lock().await;
            if let Some(p) = g.proxies.get_mut("http://1.2.3.4:8080") {
                // 伪造一个很早的入池时间
                p.added_at = Instant::now() - Duration::from_secs(3600);
                p.alive = true;
                p.last_checked_at = Some(Instant::now());
            }
        }
        // max_age 很小 → 应被判定过期并剔除（注意 check_url 不可达也不影响剔除逻辑）
        let (alive, removed, _probed, _skipped) = pool.health_check("http://127.0.0.1:1/", 300, 1000, 4).await;
        assert_eq!(removed, 1, "过期项应被剔除");
        assert_eq!(alive, 0);
        assert_eq!(pool.total_count().await, 0);
    }

    /// 已测活且离过期还远的 IP：按需测活不应重复探测。
    /// 用一个不可达的 check_url 验证「若真去探测则必然失败」，从而反证它被跳过了。
    #[tokio::test]
    async fn healthy_far_from_expiry_is_not_rechecked() {
        let pool = ProxyPool::new();
        pool.add(vec!["1.2.3.4:8080".to_string()], 10).await;
        {
            let mut g = pool.inner.lock().await;
            if let Some(p) = g.proxies.get_mut("http://1.2.3.4:8080") {
                p.alive = true;
                p.last_checked_at = Some(Instant::now());
            }
        }
        // max_age 很大（1 小时），refresh_before 很小（1s）→ 不该探测
        let (alive, removed, _probed, _skipped) = pool
            .health_check_due("http://127.0.0.1:1/", 200, 3_600_000, 4, 1_000)
            .await;
        assert_eq!(removed, 0);
        assert_eq!(alive, 1, "跳过探测后应保留原有可用状态");
    }

    /// 距过期很近的健康 IP：必须复测（用不可达地址验证它确实被探测且判失败）。
    #[tokio::test]
    async fn healthy_near_expiry_is_rechecked() {
        let pool = ProxyPool::new();
        pool.add(vec!["1.2.3.4:8080".to_string()], 10).await;
        {
            let mut g = pool.inner.lock().await;
            if let Some(p) = g.proxies.get_mut("http://1.2.3.4:8080") {
                p.alive = true;
                p.last_checked_at = Some(Instant::now());
                // 已存活 9.5s，max_age 10s → 仅剩 0.5s，refresh_before=1s 应触发复测
                p.added_at = Instant::now() - Duration::from_millis(9_500);
            }
        }
        let (alive, _removed, _probed, _skipped) = pool
            .health_check_due("http://127.0.0.1:1/", 200, 10_000, 4, 1_000)
            .await;
        assert_eq!(alive, 0, "复测失败后应标记为不可用");
    }
}