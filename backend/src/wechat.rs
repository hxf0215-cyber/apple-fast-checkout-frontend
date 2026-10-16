use anyhow::{anyhow, bail, Result};
use rand::Rng;
use reqwest::header::CONTENT_LENGTH;
use reqwest::{Client, Method, Proxy};
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock as OnceLockAlias;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Apple CDN 边缘节点域名。
///
/// 实测：
///  - DNS：apne1b.mobileapp.prod.v.aaplimg.com → 17.8.132.10/113/132
///  - 证书：apple-shield.apple.com（**不含** web.mmap.apple.com），
///    直连该域名会 SNI 校验失败，因此必须同时：
///      ① 关闭 TLS 证书校验（build_wechat_client 的 danger_accept_invalid_certs）
///      ② 请求头 Host 覆写为 web.mmap.apple.com（服务端按 Host 路由）
const CDN_HOST: &str = "apne1b.mobileapp.prod.v.aaplimg.com";
/// 原始域名：URL 里的 web.mmap.apple.com 会替换成 CDN_HOST，
/// 同时 Host 头改回 ORIGIN_HOST 以通过服务端路由。
const ORIGIN_HOST: &str = "web.mmap.apple.com";

/// Apple 小程序初始化接口：领取会话指纹 Cookie `masts`（缺它时购物类接口会 302 到 covers.html/npip）。
const INIT: &str =
    "https://web.mmap.apple.com/miniprogram-merch/p/wechat/init";
const CART_ADD: &str =
    "https://web.mmap.apple.com/26.07.0-MAIN/miniprogram-merch/p/wechat/cart/add";
const BAG: &str = "https://web.mmap.apple.com/moe/p/wechat/chat/bag";
const CHECKOUT_START: &str = "https://web.mmap.apple.com/moe/p/wechat/v2/chat/checkout/start";
const FETCH_OPENID: &str =
    "https://web.mmap.apple.com/26.07.0-MAIN/miniprogram-bootstrap/fetchOpenId";
/// 下单状态轮询地址（checkout_status）。
/// place_order 正常时会给 302 信封里的 /head/data/url；走 CDN 时偶发直接返回
/// session_expired 且不带信封，这里用固定地址兜底轮询，确保仍能取到订单号。
const CHECKOUT_STATUS: &str =
    "https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx/statusx?dst=&_a=statusUrl&_m=spinner";
const UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_7_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 MicroMessenger/8.0.79(0x18004f26) NetType/WIFI Language/zh_CN";
const COOKIE_ORDER: &[&str] = &[
    "as_dima",
    "as_pvi",
    "as_disa",
    "as_pcts",
    "rdcStatus",
    "as_loc",
    "rtsid",
    "dssid2",
    "dssf",
    "as_dc",
    "as_ltn_wechat",
    "as_atb",
    "as_sfa",
    "dslang",
    "site",
    "geo",
];

pub struct WeChatFlow {
    client: Client,
    cookies: HashMap<String, String>,
    cookie_order: Vec<String>,
    part: String,
    store: String,
    steps: Vec<Value>,
    session_id: Option<String>,
    stk: Option<String>,
    model: String,
    line_item_id: Option<String>,
    openid: Option<String>,
    landing: Value,
    contact: Value,
    selected_time_slot: Value,
    fast: bool,
    /// 本轮流程是否已经调过 init 领 masts（避免每个步骤都重复请求）。
    /// Apple 只在它认为需要时才下发 masts，未下发不代表失败。
    masts_attempted: bool,
    /// 本流程实际使用的代理（空 = 直连）。预热/预准备会话与下单必须使用同一出口 IP，
    /// 否则 Apple 会判定会话失效（403）。
    proxy_url: String,
}

/// 构建微信小程序 HTTP 客户端（可选代理）
fn build_wechat_client(proxy: Option<Proxy>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(UA)
        .http1_only()
        .timeout(Duration::from_secs(35))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(64)
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        // 走 CDN 域名（apne1b.mobileapp.prod.v.aaplimg.com）时，其证书主体是
        // apple-shield.apple.com，不含 web.mmap.apple.com，无法通过标准校验。
        // 因此关闭证书校验；服务端按 Host 头路由，仍会正常处理请求。
        // 注意：这依赖 Host 头被正确覆写为 web.mmap.apple.com。
        .danger_accept_invalid_certs(true);
    // 注意：这里不禁用自动解压。Apple 的响应仍可能带 Content-Encoding: gzip，
    // 关掉解压会拿到压缩字节、JSON 解析直接失败。每个请求会显式声明
    // Accept-Encoding: zstd，reqwest 不会再用自己的默认值覆盖它。
    match proxy {
        Some(p) => builder = builder.proxy(p),
        None => builder = builder.no_proxy(),
    }
    builder.build().map_err(|e| anyhow!("{e}"))
}

impl WeChatFlow {
    pub fn new(cookie: &str, part: &str, store: &str, contact: Value, fast: bool) -> Result<Self> {
        Self::new_with_proxy(cookie, part, store, contact, fast, "")
    }

    /// 带代理的构造：每个代理 URL 缓存一个 Client，复用连接池。
    /// proxy_url 为空时走直连（使用全局共享的直连 Client）。
    pub fn new_with_proxy(
        cookie: &str,
        part: &str,
        store: &str,
        contact: Value,
        fast: bool,
        proxy_url: &str,
    ) -> Result<Self> {
        static CLIENT: OnceLockAlias<Client> = OnceLockAlias::new();
        static PROXY_CLIENTS: OnceLockAlias<StdMutex<StdHashMap<String, Client>>> =
            OnceLockAlias::new();

        let client = if proxy_url.trim().is_empty() {
            // 直连：全局共享单例
            CLIENT
                .get_or_init(|| {
                    build_wechat_client(None).expect("failed to build WeChat HTTP client")
                })
                .clone()
        } else {
            // 带代理：按 URL 缓存，避免每次下单都重建连接池
            let cache = PROXY_CLIENTS.get_or_init(|| StdMutex::new(StdHashMap::new()));
            let key = proxy_url.trim().to_string();
            {
                let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(existing) = guard.get(&key) {
                    return Self::build_from_client(
                        existing.clone(),
                        cookie,
                        part,
                        store,
                        contact,
                        fast,
                        proxy_url,
                    );
                }
            }
            let proxy = Proxy::all(&key).map_err(|e| anyhow!("代理地址无效（{key}）：{e}"))?;
            let built = build_wechat_client(Some(proxy))
                .map_err(|e| anyhow!("构建代理客户端失败（{key}）：{e}"))?;
            {
                let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
                // 容量控制：动态池会不断产生新 IP，若不淘汰 Client，
                // 100+ 账号长时间运行会堆积成百上千个连接池（内存与句柄泄漏）。
                const PROXY_CLIENT_CACHE_MAX: usize = 256;
                if guard.len() >= PROXY_CLIENT_CACHE_MAX && !guard.contains_key(&key) {
                    // 淘汰最早插入的一个（HashMap 无序，取任意一个即可满足容量约束）
                    if let Some(victim) = guard.keys().next().cloned() {
                        guard.remove(&victim);
                    }
                }
                guard.insert(key, built.clone());
            }
            built
        };

        Self::build_from_client(client, cookie, part, store, contact, fast, proxy_url)
    }

    /// 从已构建的 Client 组装 WeChatFlow
    fn build_from_client(
        client: Client,
        cookie: &str,
        part: &str,
        store: &str,
        contact: Value,
        fast: bool,
        proxy_url: &str,
    ) -> Result<Self> {
        let mut cookies = HashMap::new();
        let mut cookie_order = Vec::new();
        for item in cookie.split(';') {
            if let Some((name, value)) = item.split_once('=') {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                if !cookie_order.contains(&name) {
                    cookie_order.push(name.clone());
                }
                cookies.insert(name, value);
            }
        }
        Ok(Self {
            client,
            cookies,
            cookie_order,
            part: part.to_string(),
            store: store.to_string(),
            steps: Vec::new(),
            session_id: None,
            stk: None,
            model: "mobileWeChatCheckout".to_string(),
            line_item_id: None,
            openid: None,
            landing: json!({}),
            contact,
            selected_time_slot: json!({}),
            fast,
            masts_attempted: false,
            proxy_url: proxy_url.to_string(),
        })
    }

    fn cookie_header(&self) -> String {
        let mut names: Vec<String> = COOKIE_ORDER
            .iter()
            .filter(|name| self.cookies.contains_key(**name))
            .map(|name| name.to_string())
            .collect();
        let extra: Vec<String> = self
            .cookie_order
            .iter()
            .filter(|name| !COOKIE_ORDER.contains(&name.as_str()))
            .cloned()
            .collect();
        names.extend(extra);
        names
            .iter()
            .filter_map(|name| {
                self.cookies
                    .get(name)
                    .map(|value| format!("{}={}", name, value))
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn update_cookies(&mut self, response: &reqwest::Response) {
        for value in response.headers().get_all("set-cookie").iter() {
            if let Ok(text) = value.to_str() {
                for item in Self::split_set_cookie(text) {
                    let item = item.trim();
                    if let Some(pair) = item.split(';').next() {
                        if let Some((name, value)) = pair.split_once('=') {
                            let name = name.trim().to_string();
                            let value = value.trim().to_string();
                            if item.contains("Expires=Thu, 01 Jan 1970")
                                || item.contains("expires=Thu, 01 Jan 1970")
                                || value.is_empty()
                            {
                                self.cookies.remove(&name);
                                continue;
                            }
                            if !self.cookie_order.contains(&name) {
                                self.cookie_order.push(name.clone());
                            }
                            self.cookies.insert(name, value);
                        }
                    }
                }
            }
        }
    }

    fn split_set_cookie(text: &str) -> Vec<String> {
        let bytes = text.as_bytes();
        let mut parts = Vec::new();
        let mut start = 0usize;
        for index in 0..bytes.len() {
            if bytes[index] != b',' {
                continue;
            }
            let mut next = index + 1;
            while next < bytes.len() && (bytes[next] == b' ' || bytes[next] == b'\t') {
                next += 1;
            }
            let mut cursor = next;
            while cursor < bytes.len()
                && bytes[cursor] != b';'
                && bytes[cursor] != b','
                && bytes[cursor] != b'='
            {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'=' && cursor > next {
                parts.push(text[start..index].to_string());
                start = next;
            }
        }
        parts.push(text[start..].to_string());
        parts
    }

    async fn request_json(
        &mut self,
        name: &str,
        method: Method,
        url: &str,
        body: Option<&str>,
        checkout_headers: bool,
    ) -> Result<Value> {
        // 走 CDN：把 URL 里的 web.mmap.apple.com 换成 CDN 域名，
        // 同时请求头 Host 覆写回 web.mmap.apple.com（服务端按 Host 路由）。
        //
        // 【例外】下单终态（place_order / checkout_status）必须走原始域名：
        // 实测经 CDN 边缘节点时，Apple 会在**订单已创建**的情况下直接返回
        // session_expired，且不再返回订单号；换回原始域名后能正常拿到
        // orderNumber（实测 W1723339864）。
        // 注意：不要把 url 重新绑定成 CDN 域名——后面还要用它和 INIT 比较、
        // 以及记录步骤 URL。请求本身统一用 request_url。
        let cdn_url = if must_use_origin(name) {
            url.to_string()
        } else {
            rewrite_to_cdn(url)
        };
        let request_url: &str = &cdn_url;
        let started = Instant::now();
        // 541 是 Apple 的限流信号，退避要足够长才有机会恢复
        let retry_delays_ms = [3_000, 8_000, 15_000];
        // 命中 npip 拦截页时只允许「重新领取 masts + 重放」一次，避免死循环
        let mut masts_refresh_used = false;
        for attempt in 0..=retry_delays_ms.len() {
            // masts 是 Apple 签发的会话指纹，缺它时购物类接口可能被判为异常客户端。
            // 每个流程只尝试领取一次；Apple 未下发属正常情况，继续走原请求。
            self.ensure_masts_once().await;
            let mut request = self.client.request(method.clone(), request_url);
            // 只声明 zstd —— 实测声明 gzip 会被 Apple 判定为异常客户端，
            // 直接返回 302 到 covers.html（x-ma-covers: npip）拦截页。
            // 注意：不能改成"不发"，因为 reqwest 在 build 时会自动补 gzip。
            request = request.header("Accept-Encoding", "zstd");
            request = request.header("Accept", "application/json, text/plain, */*");
            request = request.header("Connection", "keep-alive");
            // Host 必须保持原始域名（CDN 证书不匹配，靠 Host 路由）
            request = request.header(reqwest::header::HOST, ORIGIN_HOST);
            request = request.header("Accept-Language", "zh-CN,zh-Hans;q=0.9");
            request = request.header(
                "Referer",
                "https://servicewechat.com/wxbc25a7df24ae0e6a/164/page-frame.html",
            );
            request = request.header("xweb_xhr", "1");
            if url == INIT {
                // 小程序 init 请求固定带的版本头
                request = request.header("x-ma-version", "26.07.0-MAIN");
            }
            request = request.header("cookie", self.cookie_header());
            if method == Method::POST {
                request = request.header("Content-Type", "application/x-www-form-urlencoded");
                if let Some(body) = body {
                    if body.is_empty() {
                        request = request.header(CONTENT_LENGTH, 0).body(String::new());
                    } else {
                        request = request.body(body.to_string());
                    }
                } else {
                    request = request.header(CONTENT_LENGTH, 0).body(String::new());
                }
            } else {
                request = request.header("Content-Type", "application/json");
            }
            if checkout_headers {
                request = request.header("x-aos-model-page", self.model.clone());
                if let Some(stk) = self.stk.clone() {
                    request = request.header("x-aos-stk", stk);
                }
            }
            let request = request.build()?;
            let response = self.client.execute(request).await?;
            let status = response.status().as_u16();
            let response_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // Apple 的网络限制标记：命中 npip 说明当前出口不被购物类接口接受
            let covers_tag = response
                .headers()
                .get("x-ma-covers")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            self.update_cookies(&response);
            let text = response.text().await?;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // 解析响应：这里必须严格，不能把解析失败静默变成空对象。
            // 之前用 unwrap_or(json!({})) 会导致「Apple 返回 HTML 等待页」被误判成
            // 「购物袋是真的空的」，进而抛出误导性的 target item not found。
            let value: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(parse_err) => {
                    // 识别 Apple 的网络限制页（302 重定向到 covers.html，带 x-ma-covers: npip）
                    let npip = covers_tag == "npip" || is_apple_cover_page(&text);
                    // 先记录这一步（便于日志里看到真实状态码与响应类型）
                    self.steps.push(json!({
                        "name": name,
                        "method": method.as_str(),
                        "url": url,
                        "status": status,
                        "body": body.unwrap_or(""),
                        "response_type": response_type,
                        "note": if npip {
                            "npip：出口被购物类接口拦截".to_string()
                        } else {
                            format!("non-json response: {parse_err}")
                        },
                        "elapsed_ms": elapsed_ms,
                        "response": crate::app::truncate_utf8(&text, 200_000).to_string(),
                    }));
                    if npip {
                        // 被 npip 拦截：强制重新领取一次 masts 后原样重放
                        if !masts_refresh_used {
                            masts_refresh_used = true;
                            match self.refresh_masts().await {
                                Ok(true) => {
                                    self.steps.push(json!({
                                        "name": "session_init",
                                        "method": "GET",
                                        "url": INIT,
                                        "status": 200,
                                        "response_type": "application/json",
                                        "note": "npip 拦截后重新领取 masts",
                                        "elapsed_ms": 0,
                                    }));
                                    continue;
                                }
                                Ok(false) => {}
                                Err(err) => {
                                    self.steps.push(json!({
                                        "name": "session_init",
                                        "method": "GET",
                                        "url": INIT,
                                        "status": 0,
                                        "response_type": "",
                                        "note": format!("npip 后重新领取 masts 失败：{err}"),
                                        "elapsed_ms": 0,
                                    }));
                                }
                            }
                        }
                        bail!(
                            "Apple 限制了当前网络环境访问 {}（返回 npip 拦截页）。已尝试重新领取 masts 仍被拦截，通常表示该出口 IP 不被购物类接口接受，请更换代理或直连测试。",
                            name
                        );
                    }
                    bail!(
                        "{} 返回非 JSON（HTTP {}，Content-Type: {}）：{}",
                        name,
                        status,
                        if response_type.is_empty() { "-" } else { &response_type },
                        crate::app::truncate_utf8(&text, 200)
                    );
                }
            };
            let note = if (status == 541 || status == 503) && attempt < retry_delays_ms.len() {
                format!("{} retry in {}ms", status, retry_delays_ms[attempt])
            } else {
                String::new()
            };
            self.steps.push(json!({
                "name": name,
                "method": method.as_str(),
                "url": url,
                "status": status,
                "body": body.unwrap_or(""),
                "response_type": response_type,
                "note": note,
                "elapsed_ms": elapsed_ms,
                // 保存响应体（供下单流程留档），限制长度避免超大
                "response": crate::app::truncate_utf8(&text, 200_000).to_string(),
            }));
            if status == 200 {
                self.refresh(&value);
                return Ok(value);
            }
            // 541 = 限流、503 = 服务暂不可用，两者都值得重试
            let retryable = status == 541 || status == 503;
            if retryable && attempt < retry_delays_ms.len() {
                tokio::time::sleep(Duration::from_millis(retry_delays_ms[attempt])).await;
                continue;
            }
            bail!(
                "{} failed with {}: {}",
                name,
                status,
                crate::app::truncate_utf8(&text, 300)
            );
        }
        unreachable!("WeChat request retry loop must return");
    }

    async fn request_json_stk_only(
        &mut self,
        name: &str,
        method: Method,
        url: &str,
        body: Option<&str>,
    ) -> Result<Value> {
        self.request_json(name, method, url, body, true).await
    }

    fn refresh(&mut self, data: &Value) {
        let mh = data
            .pointer("/meta/mandatoryAdditionalHeaders")
            .or_else(|| data.pointer("/review/meta/mandatoryAdditionalHeaders"))
            .or_else(|| data.get("mandatoryAdditionalHeaders"));
        if let Some(mh) = mh {
            if let Some(stk) = mh.get("x-aos-stk").and_then(Value::as_str) {
                self.stk = Some(stk.to_string());
            }
            if let Some(model) = mh.get("x-aos-model-page").and_then(Value::as_str) {
                self.model = model.to_string();
            }
        }
        let params = data
            .pointer("/review/transactions/placeOrderParams")
            .or_else(|| data.pointer("/transactions/placeOrderParams"));
        if let Some(params) = params {
            if let Some(session_id) = params.get("sessionID").and_then(Value::as_str) {
                self.session_id = Some(session_id.to_string());
            }
        }
        let has_spokes = data
            .pointer("/review/spokes")
            .and_then(Value::as_array)
            .map(|spokes| !spokes.is_empty())
            .unwrap_or(false);
        if has_spokes {
            self.landing = data.clone();
        }
    }

    fn spokes(&self) -> Vec<Value> {
        self.landing
            .pointer("/review/spokes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn spoke_select_url(&self, id: &str) -> Result<String> {
        for spoke in self.spokes() {
            if spoke.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(url) = spoke.get("selectUrl").and_then(Value::as_str) {
                    return Ok(url.to_string());
                }
            }
        }
        let spokes = self
            .spokes()
            .iter()
            .map(|spoke| {
                spoke
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing-id>")
            })
            .collect::<Vec<_>>()
            .join(",");
        bail!("spoke {} not found; spokes: {}", id, spokes)
    }

    /// 是否已持有 Apple 签发的会话指纹 Cookie。
    fn has_masts(&self) -> bool {
        self.cookies
            .get("masts")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    }

    /// 每个流程最多尝试领取一次 masts（Apple 未下发时不算失败）。
    async fn ensure_masts_once(&mut self) {
        if self.has_masts() || self.masts_attempted {
            return;
        }
        self.masts_attempted = true;
        match self.refresh_masts().await {
            Ok(true) => {
                self.steps.push(json!({
                    "name": "session_init",
                    "method": "GET",
                    "url": INIT,
                    "status": 200,
                    "response_type": "application/json",
                    "note": "已从 init 领取 masts",
                    "elapsed_ms": 0,
                }));
            }
            Ok(false) => {}
            Err(err) => {
                self.steps.push(json!({
                    "name": "session_init",
                    "method": "GET",
                    "url": INIT,
                    "status": 0,
                    "response_type": "",
                    "note": format!("masts 领取失败：{err}"),
                    "elapsed_ms": 0,
                }));
            }
        }
    }

    /// 调 init 领取 masts。这里直接发请求（不经过 request_json），
    /// 避免 request_json → refresh_masts → request_json 的递归。
    /// 返回 true 表示本次拿到了 masts。
    async fn refresh_masts(&mut self) -> Result<bool> {
        // 同样走 CDN + Host 覆写
        let cdn_init = rewrite_to_cdn(INIT);
        let request = self
            .client
            .request(Method::GET, &cdn_init)
            .header("Accept-Encoding", "zstd")
            .header("Accept", "application/json, text/plain, */*")
            .header("Connection", "keep-alive")
            .header(reqwest::header::HOST, ORIGIN_HOST)
            .header("Accept-Language", "zh-CN,zh-Hans;q=0.9")
            .header(
                "Referer",
                "https://servicewechat.com/wxbc25a7df24ae0e6a/164/page-frame.html",
            )
            .header("content-type", "application/json")
            .header("x-ma-version", "26.07.0-MAIN")
            .header("cookie", self.cookie_header())
            .build()?;
        let response = self.client.execute(request).await?;
        let status = response.status().as_u16();
        self.update_cookies(&response);
        let _ = response.text().await?;
        if status != 200 {
            bail!("init 返回 HTTP {status}");
        }
        // Apple 只在它认为需要时下发 masts；未下发时同样返回 200，属正常情况。
        Ok(self.has_masts())
    }

    async fn bag(&mut self) -> Result<Value> {
        self.request_json("bag", Method::GET, BAG, None, false)
            .await
    }

    async fn add_cart(&mut self) -> Result<Value> {
        let encoded_part = urlencoding::encode(&self.part).replace("%2F", "/");
        let body = format!("p={}", encoded_part);
        self.request_json("add_cart", Method::POST, CART_ADD, Some(&body), false)
            .await
    }

    /// 读取购物袋里「目标商品」的总件数（同一 lineItem 的 quantity 求和）。
    /// Apple 的限购是按件数判定：1 条记录 × quantity=2 同样会触发
    /// quantity_limit_exceeded，所以只看 lineItems 条数是不够的。
    async fn target_cart_quantity(&mut self) -> Result<u64> {
        let data = self.bag().await?;
        let items = data
            .get("lineItems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = items
            .iter()
            .filter(|item| item.get("partNumber").and_then(Value::as_str) == Some(self.part.as_str()))
            .map(|item| item.get("quantity").and_then(Value::as_u64).unwrap_or(1))
            .sum();
        Ok(total)
    }

    async fn prepare_cart(&mut self) -> Result<()> {
        // 先看当前购物袋，判断是否已经「干净」（1 条目标商品 × 1 件）。
        // 干净则完全跳过清理与加购，省掉 clear + add + 复检请求；
        // 不干净才清空重加，保证最终状态精确为 1×1。
        let data = self.bag().await?;
        let items = data
            .get("lineItems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let target_qty: u64 = items
            .iter()
            .filter(|item| item.get("partNumber").and_then(Value::as_str) == Some(self.part.as_str()))
            .map(|item| item.get("quantity").and_then(Value::as_u64).unwrap_or(1))
            .sum();
        if items.len() == 1 && target_qty == 1 {
            self.line_item_id = items[0]
                .get("lineItemId")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Ok(());
        }

        // 不干净：清空 → 加购 1 件 → 校验（校验只再发 1 次 bag）
        // 复用上面已拿到的 data，避免额外请求
        self.clear_cart_with(Some(data.clone())).await?;
        self.add_cart().await?;
        // 加购后必须重新拉一次，确认 Apple 侧真实结果
        self.verify_single_cart_item(None).await
    }

    /// 校验购物袋已精确为「1 条目标商品 × 1 件」。
    /// 只发一次 bag 请求，多件时清空重加并复检一次，避免重复拉取。
    async fn verify_single_cart_item(&mut self, cached: Option<Value>) -> Result<()> {
        let data = match cached {
            Some(d) => d,
            None => self.bag().await?,
        };
        let items = data
            .get("lineItems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let target_qty: u64 = items
            .iter()
            .filter(|item| item.get("partNumber").and_then(Value::as_str) == Some(self.part.as_str()))
            .map(|item| item.get("quantity").and_then(Value::as_u64).unwrap_or(1))
            .sum();
        let extra_count = items.len().saturating_sub(
            items
                .iter()
                .filter(|item| {
                    item.get("partNumber").and_then(Value::as_str) == Some(self.part.as_str())
                })
                .count(),
        );

        // 快路径：已经是「1 条 × 1 件」，无需任何额外请求
        if items.len() == 1 && target_qty == 1 {
            self.line_item_id = items[0]
                .get("lineItemId")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Ok(());
        }

        if target_qty == 0 {
            let names = items
                .iter()
                .map(|item| {
                    item.get("partNumber")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(",");
            bail!("target item not found; items={}", names);
        }

        // 异常路径：有额外商品或数量不为 1，清空后重新加购并复检一次
        let _ = extra_count;
        self.clear_cart_with(Some(data.clone())).await?;
        self.add_cart().await?;
        let data = self.bag().await?;
        let items = data
            .get("lineItems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let qty: u64 = items
            .iter()
            .map(|item| item.get("quantity").and_then(Value::as_u64).unwrap_or(1))
            .sum();
        if items.len() != 1 || qty != 1 {
            bail!("cart not normalized: {} items / {qty} pcs (expected 1x1)", items.len());
        }
        self.line_item_id = items[0]
            .get("lineItemId")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(())
    }

    async fn checkout_start(&mut self) -> Result<Value> {
        let data = self
            .request_json(
                "checkout_start",
                Method::POST,
                CHECKOUT_START,
                Some("dv="),
                false,
            )
            .await?;
        if data.pointer("/review/spokes").is_none() {
            let keys = data
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .join(",");
            let _ = keys;
            let view = data.get("view").and_then(Value::as_str).unwrap_or("");
            bail!("checkout/start 未进入结账页，可能需要更新小程序 Cookie（view={view}）");
        }
        if let Some(item) = data.pointer("/review/fulfillment/items/0") {
            self.line_item_id = item
                .get("lineItemId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Ok(data)
    }

    /// 把商品从「送货」切到「到店取货」。
    ///
    /// Apple 会按商品/门店把 fulfillment 默认成 HOME 或 RETAIL：
    /// - HOME：spokes = shipping / billing / invoice，没有 pickup；
    /// - RETAIL：spokes = pickup / 3pp / billing / invoice。
    /// 目标是到店取货，因此发现是 HOME 时提交一次 deliveryLocationType=RETAIL。
    async fn ensure_pickup_fulfillment(&mut self) -> Result<()> {
        let already_pickup = self.spokes().iter().any(|spoke| {
            matches!(
                spoke.get("id").and_then(Value::as_str),
                Some("pickup") | Some("timeSlot")
            )
        });
        if already_pickup {
            return Ok(());
        }
        let item = self
            .landing
            .pointer("/review/fulfillment/items/0")
            .cloned()
            .unwrap_or(json!({}));
        let is_home = item
            .get("isHomeDelivery")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_home {
            return Ok(());
        }
        let url = item
            .get("selectDeliveryMethod")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if url.is_empty() {
            return Ok(());
        }
        let prefix = item
            .get("keyPath")
            .and_then(Value::as_str)
            .unwrap_or("checkout.review.fulfillment.items.0");
        let body = format!("{}.deliveryLocationType=RETAIL", prefix);
        let data = self
            .request_json(
                "select_delivery_method",
                Method::POST,
                &url,
                Some(&body),
                true,
            )
            .await?;
        // 切换成功后返回体带 review/spokes，refresh 会用它更新 landing
        self.refresh(&data);
        if self.spokes().is_empty() {
            bail!("切换到店取货失败：Apple 未返回可用的结账步骤");
        }
        Ok(())
    }

    async fn change_store(&mut self) -> Result<Value> {
        // Apple 可能把商品默认为「送货」，此时 spokes 里没有 pickup/timeSlot，
        // 必须先切到「到店取货」，后面的选店 / 时间段 / 到店联系人链路才存在。
        self.ensure_pickup_fulfillment().await?;
        let url = self
            .landing
            .pointer("/review/fulfillment/items/0/changeStoreAction")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=changeStoreAction&_m=checkout.review.fulfillment.items.0")
            .to_string();
        self.request_json("change_store", Method::GET, &url, None, true)
            .await
    }

    async fn select_store(&mut self) -> Result<Value> {
        let check_url = self
            .landing
            .pointer("/pickup/checkAvailabilityUrl")
            .and_then(Value::as_str)
            .unwrap_or(
                "https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=checkAvailabilityUrl&_m=checkout.pickup",
            );
        if !self.fast && !check_url.is_empty() {
            let line_item_id = self.line_item_id.clone().unwrap_or_default();
            let query = format!(
                "checkout.pickup.retailAvailabilityData.currentRetailCountries=CN&checkout.pickup.retailAvailabilityData.currentRetailIds={}&checkout.pickup.retailAvailabilityData.cartLineItemId={}",
                urlencoding::encode(&self.store),
                urlencoding::encode(&line_item_id),
            );
            let separator = if check_url.contains('?') { '&' } else { '?' };
            self.request_json(
                "check_availability",
                Method::GET,
                &format!("{check_url}{separator}{query}"),
                None,
                true,
            )
            .await?;
        }

        let url = self
            .landing
            .pointer("/pickup/selectStoreUrl")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=selectStoreUrl&_m=checkout.pickup")
            .to_string();
        let line_item_id = self.line_item_id.clone().unwrap_or_default();
        let body = format!(
            "checkout.pickup.cartLineItemId={}&checkout.pickup.currentRetailId={}",
            urlencoding::encode(&line_item_id),
            urlencoding::encode(&self.store)
        );
        self.request_json("select_store", Method::POST, &url, Some(&body), true)
            .await
    }

    async fn save_time_slot(&mut self) -> Result<()> {
        if !self
            .spokes()
            .iter()
            .any(|s| s.get("id").and_then(Value::as_str) == Some("timeSlot"))
        {
            return self.select_pickup_method().await;
        }
        let url = self.spoke_select_url("timeSlot")?;
        let data = self
            .request_json("open_time_slot", Method::GET, &url, None, true)
            .await?;
        let ts = data.get("timeSlot").cloned().unwrap_or(json!({}));
        let day_slots = ts
            .get("daySlots")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let day = day_slots
            .iter()
            .find(|day| {
                day.get("timeSlots")
                    .and_then(Value::as_array)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
            .cloned()
            .ok_or_else(|| anyhow!("no time slots"))?;

        let parse_hour = |value: &str| -> u32 {
            let value = value.trim().to_uppercase();
            let hour: u32 = value
                .split(':')
                .next()
                .and_then(|part| part.parse().ok())
                .unwrap_or(0);
            let is_pm = value.contains("PM");
            if is_pm && hour != 12 {
                hour + 12
            } else {
                hour
            }
        };

        let all_slots = day
            .get("timeSlots")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let afternoon_slots: Vec<_> = all_slots
            .iter()
            .filter(|slot| {
                slot.get("startTime")
                    .and_then(Value::as_str)
                    .map(|value| parse_hour(value) >= 12)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let pool = if !afternoon_slots.is_empty() {
            afternoon_slots
        } else {
            all_slots
        };
        if pool.is_empty() {
            bail!("no slot");
        }
        let slot = pool[rand::thread_rng().gen_range(0..pool.len())].clone();
        let save_url = ts.get("saveUrl").and_then(Value::as_str).unwrap_or("");
        if save_url.is_empty() {
            bail!("timeSlot missing saveUrl");
        }
        let prefix = ts
            .get("keyPath")
            .and_then(Value::as_str)
            .unwrap_or("checkout.timeSlot");
        let mut body = String::new();
        let selected_method = ts
            .get("selectedMethod")
            .and_then(Value::as_str)
            .unwrap_or("INSTORE");
        body.push_str(&format!(
            "{}.selectedMethod={}",
            prefix,
            urlencoding::encode(selected_method)
        ));
        if let Some(date) = day.get("date").and_then(Value::as_str) {
            body.push_str(&format!("&{}.date={}", prefix, urlencoding::encode(date)));
        }
        if let Some(time_zone) = day
            .get("timeZone")
            .and_then(Value::as_str)
            .or_else(|| ts.get("timeZone").and_then(Value::as_str))
        {
            body.push_str(&format!(
                "&{}.timeZone={}",
                prefix,
                urlencoding::encode(time_zone)
            ));
        }
        for key in ["startTime", "endTime", "reservationId", "timeSlotId"] {
            if let Some(value) = slot.get(key).and_then(Value::as_str) {
                body.push_str(&format!(
                    "&{}.{}={}",
                    prefix,
                    key,
                    urlencoding::encode(value)
                ));
            }
        }
        if let Some(selected) = slot.get("id").and_then(Value::as_str) {
            body.push_str(&format!(
                "&{}.selectedTimeSlot={}",
                prefix,
                urlencoding::encode(selected)
            ));
        }
        body.push_str(&format!("&{}.recommendedTimeSlot=false", prefix));
        let result = self
            .request_json("save_time_slot", Method::POST, save_url, Some(&body), true)
            .await?;
        self.selected_time_slot = json!({
            "selectedDate": day.get("date").cloned().unwrap_or(json!("")),
            "selectedTimeSlot": slot
                .get("time")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "{} – {}",
                        slot.get("startTime").and_then(Value::as_str).unwrap_or(""),
                        slot.get("endTime").and_then(Value::as_str).unwrap_or("")
                    )
                }),
            "retailStoreSelected": self.store,
        });
        if result.get("timeSlotData").is_some() {
            self.selected_time_slot = result.get("timeSlotData").cloned().unwrap_or(json!({}));
        }
        Ok(())
    }

    /// 选择取货方式（INSTORE=到店取货）。
    /// Apple 通过 checkoutStates.pickupMethodReady 判定；未设置会导致提交订单被拒。
    async fn select_pickup_method(&mut self) -> Result<()> {
        let url = self.spoke_select_url("pickup")?;
        let data = self
            .request_json("open_pickup", Method::GET, &url, None, true)
            .await?;
        let select_url = find_url(&data, "selectPickupMethodUrl").0;
        if select_url.is_empty() {
            let view = data.get("view").and_then(Value::as_str).unwrap_or("-");
            let compact = serde_json::to_string(&data).unwrap_or_default();
            bail!(
                "pickup missing selectPickupMethodUrl (view={view}; body={})",
                crate::app::truncate_utf8(&compact, 800)
            );
        }
        let key_path = find_url(&data, "selectPickupMethodUrl").1;
        let body = format!(
            "{}.selectedMethod=INSTORE",
            if key_path.is_empty() {
                "checkout.pickup".to_string()
            } else {
                key_path
            }
        );
        let result = self
            .request_json(
                "select_pickup_method",
                Method::POST,
                &select_url,
                Some(&body),
                false,
            )
            .await?;
        let has_spokes = result
            .pointer("/review/spokes")
            .and_then(Value::as_array)
            .map(|spokes| !spokes.is_empty())
            .unwrap_or(false);
        if has_spokes {
            self.landing = result;
        }
        let ready = self
            .landing
            .pointer("/review/checkoutStates/pickupMethodReady")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !ready {
            bail!("pickupMethodReady is false");
        }
        Ok(())
    }

    async fn save_pickup_contact(&mut self) -> Result<()> {
        let url = self.spoke_select_url("3pp")?;
        let data = self
            .request_json("open_pickup_contact", Method::GET, &url, None, true)
            .await?;
        let pc = data.get("pickupContact").cloned().unwrap_or(json!({}));
        let save_url = pc
            .get("saveThirdPartyPickupUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if save_url.is_empty() {
            bail!("pickupContact missing saveThirdPartyPickupUrl");
        }
        let addr_prefix = pc
            .pointer("/mobileAddress/addressData/keyPath")
            .and_then(Value::as_str)
            .unwrap_or("checkout.pickupContact.mobileAddress.addressData");
        let tax_prefix = pc
            .pointer("/taxFields/keyPath")
            .and_then(Value::as_str)
            .unwrap_or("checkout.pickupContact.taxFields");
        let terms_prefix = pc
            .pointer("/pickupContactTerms/keyPath")
            .and_then(Value::as_str)
            .unwrap_or("checkout.pickupContact.pickupContactTerms");
        let last_name = self
            .contact
            .get("lastName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let first_name = self
            .contact
            .get("firstName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let email = self
            .contact
            .get("emailAddress")
            .and_then(Value::as_str)
            .unwrap_or("");
        let phone = self
            .contact
            .get("fullDaytimePhone")
            .and_then(Value::as_str)
            .unwrap_or("");
        let national_id = self
            .contact
            .get("nationalId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let body = format!(
            "{}.lastName={}&{}.firstName={}&{}.emailAddress={}&{}.fullDaytimePhone={}&{}.nationalId={}&{}.terms-accept=true",
            addr_prefix, urlencoding::encode(last_name),
            addr_prefix, urlencoding::encode(first_name),
            addr_prefix, urlencoding::encode(email),
            addr_prefix, urlencoding::encode(phone),
            tax_prefix, urlencoding::encode(national_id),
            terms_prefix
        );
        self.request_json(
            "save_pickup_contact",
            Method::POST,
            &save_url,
            Some(&body),
            true,
        )
        .await?;
        Ok(())
    }

    async fn select_wechat(&mut self) -> Result<()> {
        let url = self.spoke_select_url("billing")?;
        let data = self
            .request_json("open_billing", Method::GET, &url, None, true)
            .await?;
        let billing = data.get("billing").cloned().unwrap_or(json!({}));
        let select_url = billing
            .pointer("/transactions/selectBillingOptionUrl")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/checkoutx/billing?_a=selectBillingOptionUrl&_m=checkout.billing")
            .to_string();
        let prefix = billing
            .get("keyPath")
            .and_then(Value::as_str)
            .unwrap_or("checkout.billing")
            .to_string();
        let body = format!(
            "{}.paymentOptionsAvailableForm=true&{}.paymentTypeCategory=WECHAT",
            prefix, prefix
        );
        self.request_json(
            "select_wechat",
            Method::POST,
            &select_url,
            Some(&body),
            true,
        )
        .await?;
        Ok(())
    }

    async fn fetch_openid(&mut self) -> Result<()> {
        let data = self
            .request_json("fetch_openid", Method::GET, FETCH_OPENID, None, false)
            .await?;
        self.openid = data
            .get("openID")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(())
    }

    fn terms_body(&self) -> String {
        let terms = self
            .landing
            .pointer("/review/terms")
            .cloned()
            .unwrap_or(json!({}));
        if let Some(key) = terms.get("keyPath").and_then(Value::as_str) {
            if terms
                .get("mergeTerms")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return format!("&{}.termsAccepted=1", key);
            }
        }
        let mut out = String::new();
        if let Some(sections) = terms.get("sections").and_then(Value::as_array) {
            for section in sections {
                if let Some(key) = section.get("keyPath").and_then(Value::as_str) {
                    out.push_str(&format!("&{}.termsAccepted=1", key));
                }
            }
        }
        if out.is_empty() {
            out.push_str("&checkout.review.terms.sections.0.termsAccepted=1");
        }
        out
    }

    async fn place_order(&mut self) -> Result<Value> {
        let url = self
            .landing
            .pointer("/review/transactions/placeOrderUrl")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=placeOrderUrl&_m=checkout.review.transactions")
            .to_string();
        let params = self
            .landing
            .pointer("/review/transactions/placeOrderParams")
            .cloned()
            .unwrap_or(json!({}));
        let m_sign_cookies = params
            .get("mSignCookies")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mas_config_key = params
            .get("masConfigKey")
            .and_then(Value::as_str)
            .unwrap_or("placeOrder");
        let session_id = self.session_id.clone().unwrap_or_default();
        let openid = self.openid.clone().unwrap_or_default();
        let body = format!(
            "mSignCookies={}&masConfigKey={}&sessionID={}&mSignParameters=sessionID{}&checkout.review.transactions.openid={}&checkout.review.transactions.tzos=-7&checkout.review.transactions.udid=",
            urlencoding::encode(m_sign_cookies),
            urlencoding::encode(mas_config_key),
            urlencoding::encode(&session_id),
            self.terms_body(),
            urlencoding::encode(&openid)
        );
        let data = self
            .request_json("place_order", Method::POST, &url, Some(&body), true)
            .await?;
        let result = self.follow_status(data).await?;
        let page_type = result
            .get("pageType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let order_number = result
            .pointer("/model/orderNumber")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if page_type != "thankyou" || order_number.is_empty() {
            let keys = result
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .join(",");
            let view = result.get("view").and_then(Value::as_str).unwrap_or("-");
            let sorry_key = result
                .get("sorryKey")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let checkout_states = result
                .pointer("/review/checkoutStates")
                .cloned()
                .unwrap_or(json!({}));
            let alerts = result.get("alerts").cloned().unwrap_or(json!([]));
            let head_data = result.pointer("/head/data").cloned().unwrap_or(json!({}));
            let pickup_contact = result.get("pickupContact").cloned().unwrap_or(json!({}));
            let debug = serde_json::to_string(&json!({
                "keys": keys,
                "view": view,
                "sorryKey": sorry_key,
                "checkoutStates": checkout_states,
                "alerts": alerts,
                "headData": head_data,
                "pickupContact": pickup_contact,
            }))
            .unwrap_or_default();
            bail!(
                "order not confirmed: pageType={}, orderNumber={}, detail={}",
                if page_type.is_empty() {
                    "-"
                } else {
                    &page_type
                },
                if order_number.is_empty() {
                    "-"
                } else {
                    &order_number
                },
                debug
            );
        }
        Ok(result)
    }

    async fn follow_status(&mut self, mut data: Value) -> Result<Value> {
        // session_expired 兜底轮询：Apple 偶发在订单已创建的情况下先回一次
        // session_expired，随后状态页仍会变成 thank you。这里多给几次机会，
        // 避免把已成功的订单判成失败（真正的过期会在后续轮询中稳定复现）。
        let mut session_expired_retries = 0u32;
        const SESSION_EXPIRED_RETRIES: u32 = 5;
        for _ in 0..30 {
            let session_expired = data
                .get("sorryKey")
                .and_then(Value::as_str)
                .map(|key| key.eq_ignore_ascii_case("session_expired"))
                .unwrap_or(false);
            if data.get("sorryKey").is_some() && !session_expired {
                return Ok(data);
            }
            if session_expired {
                // 优先用响应自带的信封地址；走 CDN 时可能既没有信封也没有订单号，
                // 这时用固定状态地址继续轮询——下单本身多半已成功，
                // 只是这一步的响应被替换成了过期文案。
                let envelope_url = data
                    .pointer("/head/data/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let status_url = if envelope_url.is_empty() {
                    CHECKOUT_STATUS
                } else {
                    envelope_url
                };
                if session_expired_retries >= SESSION_EXPIRED_RETRIES {
                    return Ok(data);
                }
                session_expired_retries += 1;
                tokio::time::sleep(Duration::from_millis(500)).await;
                match self
                    .request_json("checkout_status", Method::GET, status_url, None, true)
                    .await
                {
                    Ok(next) => data = next,
                    // 轮询失败（如 403）时保留上一次结果，交给外层做购物袋兜底判定。
                    Err(_) => return Ok(data),
                }
                continue;
            }
            if data.get("pageType").is_some() {
                return Ok(data);
            }
            if data.get("review").is_some() {
                self.refresh(&data);
                return Ok(data);
            }
            let status_url = data
                .pointer("/head/data/url")
                .and_then(Value::as_str)
                .unwrap_or("");
            if status_url.is_empty() {
                return Ok(data);
            }
            let delay = data
                .pointer("/head/data/delay")
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
                .min(0.3);
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            data = self
                .request_json("checkout_status", Method::GET, status_url, None, true)
                .await?;
        }
        Ok(data)
    }

    pub async fn run(&mut self, place: bool) -> Result<Value> {
        self.prepare_cart().await?;
        if self.line_item_id.is_none() {
            bail!("wechat cart line item missing");
        }
        self.complete_checkout(place).await
    }

    /// 一次性建立「到店取货」结账态，停在 select_store 之后。
    ///
    /// 流程：prepare_cart → checkout_start → 切 RETAIL → change_store
    ///      → check_availability → select_store
    ///
    /// 返回 select_store 的响应（含 spokes），供 watch 模式反复刷新。
    /// 无货时也能执行成功（Apple 不报错），只是 spokes 里没有 timeSlot。
    pub async fn build_pickup_checkout(&mut self) -> Result<Value> {
        self.prepare_cart().await?;
        if self.line_item_id.is_none() {
            bail!("wechat cart line item missing");
        }
        let data = self.checkout_start().await?;
        self.refresh(&data);
        self.ensure_pickup_fulfillment().await?;
        self.change_store().await?;
        // 注意：这里显式走 check_availability（忽略 fast 标志），
        // 因为 watch 模式需要把库存判定写进结账态。
        let saved_fast = self.fast;
        self.fast = false;
        let selected = self.select_store().await?;
        self.fast = saved_fast;
        self.refresh(&selected);
        // 预取 openID：微信支付的 openID 与具体支付方式无关（都是微信），
        // 因此在建态时取一次缓存起来，命中下单时可省掉这一步（约 300~600ms）。
        // 失败不影响建态（命中时会再取一次兜底）。
        let _ = self.fetch_openid().await;
        Ok(selected)
    }

    /// 刷新一次结账态（重放 selectStoreUrl），返回是否已出现 timeSlot。
    ///
    /// Apple 在生成 spokes 时会依据库存判定：
    ///  - 有货 → spokes 里带 timeSlot
    ///  - 无货 → 不带
    /// 返回 (spokes, has_time_slot, 原始响应)
    pub async fn refresh_pickup_checkout(&mut self) -> Result<(Vec<Value>, bool, Value, String)> {
        let url = self
            .landing
            .pointer("/pickup/selectStoreUrl")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=selectStoreUrl&_m=checkout.pickup")
            .to_string();
        let line_item_id = self.line_item_id.clone().unwrap_or_default();
        if line_item_id.is_empty() {
            bail!("line_item_id missing while refreshing pickup checkout");
        }
        let body = format!(
            "checkout.pickup.cartLineItemId={}&checkout.pickup.currentRetailId={}",
            urlencoding::encode(&line_item_id),
            urlencoding::encode(&self.store)
        );
        let data = self
            .request_json("refresh_pickup", Method::POST, &url, Some(&body), true)
            .await?;
        self.refresh(&data);
        let spokes = self.spokes();
        let has_time_slot = spokes.iter().any(|spoke| {
            spoke.get("id").and_then(Value::as_str) == Some("timeSlot")
        });
        // 诊断摘要：状态、取货可用性、告警，用于 watch 每轮日志
        let status = data.get("view").and_then(Value::as_str).unwrap_or("-").to_string();
        let alerts = data
            .pointer("/review/alerts")
            .or_else(|| data.get("alerts"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|a| {
                        a.get("message")
                            .and_then(Value::as_str)
                            .or_else(|| a.get("title").and_then(Value::as_str))
                    })
                    .collect::<Vec<_>>()
                    .join(" / ")
            })
            .unwrap_or_default();
        let digest = if alerts.is_empty() {
            format!("view={status}")
        } else {
            format!("view={status} alerts={alerts}")
        };
        Ok((spokes, has_time_slot, data, digest))
    }

    /// 探测 timeSlot 是否**真的**有可预约时段。
    ///
    /// 背景：spokes 里出现 `timeSlot` 节点 ≠ 有可预约时段。
    /// 实测会遇到 spokes 有 timeSlot，但 open_time_slot 返回的 daySlots 全空
    /// （库存刚放、时段未生成、或时段已被抢完），此时下单会报 no time slots。
    ///
    /// 返回 (是否有可预约时段, 概要说明)。
    /// 注意：若 spokes 里没有 timeSlot，直接返回 (false, "no-timeSlot-spoke")。
    pub async fn probe_time_slot_available(&mut self) -> Result<(bool, String)> {
        let has_spoke = self
            .spokes()
            .iter()
            .any(|s| s.get("id").and_then(Value::as_str) == Some("timeSlot"));
        if !has_spoke {
            return Ok((false, "无 timeSlot 节点".to_string()));
        }
        let url = self.spoke_select_url("timeSlot")?;
        let data = self
            .request_json("probe_time_slot", Method::GET, &url, None, true)
            .await?;
        let ts = data.get("timeSlot").cloned().unwrap_or(json!({}));
        let day_slots = ts
            .get("daySlots")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total_days = day_slots.len();
        let days_with_slots = day_slots
            .iter()
            .filter(|day| {
                day.get("timeSlots")
                    .and_then(Value::as_array)
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
            .count();
        let summary = format!("{days_with_slots}/{total_days} 天有时段");
        Ok((days_with_slots > 0, summary))
    }

    /// watch 命中后：从 timeSlot 开始把剩余结账步骤走完并提交。
    ///
    /// 前置条件：spokes 里已有 timeSlot（即库存已到货）。
    pub async fn finish_checkout_after_slot(&mut self) -> Result<Value> {
        self.save_time_slot().await?;
        self.save_pickup_contact().await?;
        self.select_wechat().await?;
        // openID 若已在建态阶段缓存则直接复用（省一步请求）。
        // 缓存为空时才真正去取。
        match self.openid.clone() {
            Some(id) if !id.trim().is_empty() => {}
            _ => {
                self.fetch_openid().await?;
            }
        }
        let mut result = self.place_order().await?;
        if let Some(object) = result.as_object_mut() {
            object.insert("cookies".to_string(), json!(self.cookies));
            object.insert("timeSlotData".to_string(), self.selected_time_slot.clone());
            object.insert("line_item_id".to_string(), json!(self.line_item_id));
            object.insert("session_id".to_string(), json!(self.session_id));
            object.insert("stk".to_string(), json!(self.stk));
            object.insert("openid".to_string(), json!(self.openid));
        }
        Ok(result)
    }

    /// 购物袋已就绪时推进结账流程直到下单。
    /// 被 run()（全流程）与「预热会话快速下单」共用：
    /// - 已有结账页数据（预热时已进入结账）则跳过 checkout_start，省一步；
    /// - 否则从 checkout_start 开始走完整结账链路。
    async fn complete_checkout(&mut self, place: bool) -> Result<Value> {
        if self.line_item_id.is_none() {
            bail!("wechat cart line item missing");
        }
        if self.spokes().is_empty() {
            self.checkout_start().await?;
        }
        self.change_store().await?;
        self.select_store().await?;
        self.save_time_slot().await?;
        self.save_pickup_contact().await?;
        self.select_wechat().await?;
        self.fetch_openid().await?;
        let mut result = if place {
            self.place_order().await?
        } else {
            json!({"ok": true, "dry_run": true})
        };
        if let Some(object) = result.as_object_mut() {
            object.insert("cookies".to_string(), json!(self.cookies));
            object.insert("timeSlotData".to_string(), self.selected_time_slot.clone());
            object.insert("line_item_id".to_string(), json!(self.line_item_id));
            object.insert("session_id".to_string(), json!(self.session_id));
            object.insert("stk".to_string(), json!(self.stk));
            object.insert("openid".to_string(), json!(self.openid));
        }
        Ok(result)
    }

    /// Account warmup: pre-prepare bag, contact, payment, openid (no stock needed)
    /// Skips checkout_start, store, time_slot (these require available stock)
    /// Account warmup: add to bag + get OpenID (no stock/checkout needed)
    /// Contact/payment steps require active checkout, so they are skipped here
    /// 预热：加购 → 取 OpenID → 尽力进入结账页（拿到 landing 与 sessionID）。
    /// 结账页数据是否拿到取决于该型号/门店当前是否有货：
    /// - 拿到 landing：下单时可直接 place，省掉加购 + checkout_start；
    /// - 没拿到（无货）：仍完成加购与 OpenID 预热，下单时走完整结账流程，
    ///   由于购物袋已就绪，仍能省掉清空购物车 + 加购步骤。
    /// 注意：库存监控不使用代理；但预热与下单必须使用同一出口 IP（由调用方保证）。
    /// 只读探活：仅请求 fetchOpenId 判断 Cookie / 会话是否仍然有效。
    /// 注意：绝对不能在这里重跑 prepare（add_cart / checkout_start / 选店 / 支付），
    /// 那会改动 Apple 侧的结账状态，把已经预热好的会话弄坏（实测会导致后续 place 403）。
    pub async fn verify(&mut self) -> Result<Value> {
        self.fetch_openid().await?;
        Ok(json!({
            "ok": true,
            "verified": true,
            "openid": self.openid,
            "part": self.part,
            "store": self.store,
        }))
    }

    pub async fn warmup(&mut self) -> Result<Value> {
        // 预热分两级：
        //  - 有货：走完整结账准备（加购→结账→选店→时间段→联系人→支付），
        //          产出可直接 place 的会话，下单时只剩 3 步；
        //  - 无货：Apple 不会把商品放进购物袋，只能降级为「清空购物袋 + 缓存 OpenID」，
        //          有货时下单仍需完整结账流程，但省掉清空购物袋一步。
        //
        // 注意：只做 checkout_start 是不够的——Apple 要求选店 / 时间段 / 联系人 /
        // 支付都完成后才接受 placeOrder，否则提交时返回 403。
        if self.prepare_cart().await.is_ok() && self.line_item_id.is_some() {
            // 加购成功（商品已进袋）。接下来尝试走到「下单前一步」。
            //  - 有货：能一路走到支付，产出可直接 place 的会话；
            //  - 无货：会在选店/取货方式处被 Apple 拒绝，此时**不回滚购物车**，
            //          降级为「预加购完成」，等监控到货后直接命中快路径。
            match self.prepare().await {
                Ok(prepared) => {
                    return Ok(json!({
                        "ok": true,
                        "warmup": true,
                        "cart_ready": true,
                        "checkout_ready": !self.spokes().is_empty(),
                        "openid_ready": self.openid.is_some(),
                        "proxy_url": self.proxy_url,
                        "part": self.part,
                        "store": self.store,
                        "line_item_id": self.line_item_id,
                        "cookies": self.cookies,
                        "stk": self.stk,
                        "openid": self.openid,
                        "landing": prepared.get("landing").cloned().unwrap_or(json!({})),
                        "session_id": self.session_id,
                        "model": self.model
                    }));
                }
                Err(err) => {
                    // 无货（或 Apple 拒绝继续结账）：购物车已保留目标商品 1×1，
                    // 标记为「预加购就绪」，不计为失败。
                    let openid_ready = self.fetch_openid().await.is_ok();
                    return Ok(json!({
                        "ok": true,
                        "warmup": true,
                        "cart_ready": true,
                        "checkout_ready": false,
                        "pre_added": true,
                        "openid_ready": openid_ready,
                        "pending_reason": err.to_string(),
                        "proxy_url": self.proxy_url,
                        "part": self.part,
                        "store": self.store,
                        "line_item_id": self.line_item_id,
                        "cookies": self.cookies,
                        "stk": self.stk,
                        "openid": self.openid,
                        "landing": self.landing,
                        "session_id": self.session_id,
                        "model": self.model
                    }));
                }
            }
        }

        // 连加购都没成功（清空/加购本身失败）：只缓存 OpenID
        let openid_ready = self.fetch_openid().await.is_ok();
        Ok(json!({
            "ok": true,
            "warmup": true,
            "cart_ready": false,
            "checkout_ready": false,
            "openid_ready": openid_ready,
            "proxy_url": self.proxy_url,
            "part": self.part,
            "store": self.store,
            "line_item_id": self.line_item_id,
            "cookies": self.cookies,
            "stk": self.stk,
            "openid": self.openid,
            "landing": self.landing,
            "session_id": self.session_id,
            "model": self.model
        }))
    }

    /// Public: only clear the cart (delete stale items), never add anything.
    pub async fn clear_cart_only(&mut self) -> Result<Value> {
        self.clear_cart_with(None).await
    }

    /// 清空购物袋；传入已获取的 bag 数据可省一次请求。
    async fn clear_cart_with(&mut self, cached: Option<Value>) -> Result<Value> {
        let data = match cached {
            Some(d) => d,
            None => self.bag().await?,
        };
        let items = data
            .get("lineItems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            return Ok(json!({"ok": true, "cleared": 0}));
        }
        let stk = data
            .pointer("/mandatoryAdditionalHeaders/x-aos-stk")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if stk.is_empty() {
            bail!("missing stk while clearing cart");
        }
        let mut cleared = 0usize;
        for item in items.iter() {
            let url = item.get("deleteItem").and_then(Value::as_str).unwrap_or("");
            if url.is_empty() { continue; }
            if self
                .request_json_stk_only("delete_item", Method::POST, url, Some(""))
                .await
                .is_ok()
            {
                cleared += 1;
            }
        }
        Ok(json!({"ok": true, "cleared": cleared}))
    }

    pub async fn prepare(&mut self) -> Result<Value> {
        self.prepare_cart().await?;
        if self.line_item_id.is_none() {
            bail!("wechat cart line item missing");
        }
        self.checkout_start().await?;
        self.change_store().await?;
        self.select_store().await?;
        self.save_time_slot().await?;
        self.save_pickup_contact().await?;
        self.select_wechat().await?;
        self.fetch_openid().await?;
        Ok(json!({
            "ok": true,
            "proxy_url": self.proxy_url,
            "cookies": self.cookies,
            "session_id": self.session_id,
            "stk": self.stk,
            "model": self.model,
            "line_item_id": self.line_item_id,
            "openid": self.openid,
            "landing": self.landing,
            "part": self.part,
            "store": self.store,
        }))
    }

    /// 从预热/结账态的 JSON 恢复 flow 内联状态。
    /// 供 place_prepared / watch-refresh / watch-finish 共用。
    fn restore_from_state(&mut self, state: &Value) {
        if let Some(cookies) = state.get("cookies").and_then(Value::as_object) {
            self.cookies = cookies
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect();
        }
        self.session_id = state
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.stk = state.get("stk").and_then(Value::as_str).map(str::to_string);
        self.model = state
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("mobileWeChatCheckout")
            .to_string();
        self.line_item_id = state
            .get("line_item_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.openid = state
            .get("openid")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(landing) = state.get("landing") {
            if !landing.is_null() {
                self.landing = landing.clone();
            }
        }
        if let Some(proxy) = state.get("proxy_url").and_then(Value::as_str) {
            self.proxy_url = proxy.to_string();
        }
        if let Some(part) = state.get("part").and_then(Value::as_str) {
            self.part = part.to_string();
        }
        if let Some(store) = state.get("store").and_then(Value::as_str) {
            self.store = store.to_string();
        }
    }

    pub async fn place_prepared(&mut self, state: Value) -> Result<Value> {
        self.restore_from_state(&state);
        // 预热会话已包含结账页数据（landing + sessionID + 购物袋）时直接下单，
        // 跳过清空购物车与加购，这是预热提速的主要来源；
        // 数据不完整（无货预热）时回退到完整结账流程，避免直接失败。
        if !self.landing.is_null() && !self.spokes().is_empty() {
            self.place_order().await
        } else {
            self.complete_checkout(true).await
        }
    }
}

/// 判断响应体是否是 Apple 的「网络限制/等待」页。
///
/// Apple 会对某些网络环境或时间段把接口 302 到 `/wow-assets/covers.html`，
/// 页面里带有 `x-ma-covers: npip`（no proxy/受限 IP）标记，返回的是 HTML 而非 JSON。
/// 识别出来可以给出准确提示，避免被误判成「购物袋为空」。
/// 把 Apple 原始域名替换成 CDN 边缘节点域名。
///
/// 只替换 host 部分，path/query 原样保留；非 Apple 域名的 URL 原样返回。
fn rewrite_to_cdn(url: &str) -> String {
    if mmap_edge_disabled() {
        return url.to_string();
    }
    if url.contains(ORIGIN_HOST) {
        url.replace(ORIGIN_HOST, CDN_HOST)
    } else {
        url.to_string()
    }
}

/// 必须走原始域名（web.mmap.apple.com）的步骤。
///
/// 只把 checkout_status 拉回原始域名：
/// - place_order 继续走 CDN（提交本身没问题，订单确实创建成功）；
/// - 但「查订单号」这步经 CDN 时响应被替换成 session_expired 文案，
///   轮询拿不到 pageType=thankyou / orderNumber；换回原始域名后同一步
///   能正常返回订单号（实测 W1723339864）。
/// 也就是说：换回原域名只是为了「读到确切的订单结果」，不改变下单行为。
fn must_use_origin(name: &str) -> bool {
    matches!(name, "checkout_status")
}

/// 逃生开关：设置环境变量 AFC_MMAP_DOMAIN=origin 可临时切回原始域名
/// （用于排查「CDN 边缘节点是否影响下单/会话」这类问题，无需重新编译）。
fn mmap_edge_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("AFC_MMAP_DOMAIN")
            .map(|v| v.eq_ignore_ascii_case("origin"))
            .unwrap_or(false)
    })
}

fn is_apple_cover_page(text: &str) -> bool {
    let lower = text.to_lowercase();
    // covers.html 是 Apple 的拦截/等待页特征
    lower.contains("covers.html")
        || lower.contains("<!doctype html>") && lower.contains("wow-assets")
        || text.contains("<!-- covers -->")
}

fn find_url(data: &Value, key: &str) -> (String, String) {
    fn walk(node: &Value, key: &str) -> Option<(String, String)> {
        if let Value::Object(map) = node {
            if let Some(value) = map.get(key).and_then(Value::as_str) {
                let path = map.get("keyPath").and_then(Value::as_str).unwrap_or("");
                return Some((value.to_string(), path.to_string()));
            }
            for value in map.values() {
                if let Some(found) = walk(value, key) {
                    return Some(found);
                }
            }
        } else if let Value::Array(items) = node {
            for item in items {
                if let Some(found) = walk(item, key) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(data, key).unwrap_or_default()
}

pub async fn run_wechat_flow(payload: Value) -> Value {
    let cookie = payload.get("cookie").and_then(Value::as_str).unwrap_or("");
    let part = payload.get("part").and_then(Value::as_str).unwrap_or("");
    let store = payload.get("store").and_then(Value::as_str).unwrap_or("");
    let contact = payload.get("contact").cloned().unwrap_or(json!({}));
    let mode = payload
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("full");
    let place = payload
        .get("place")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fast = payload
        .get("fast")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let proxy = payload.get("proxy").and_then(Value::as_str).unwrap_or("");
    let mut flow = match WeChatFlow::new_with_proxy(
        cookie,
        part,
        store,
        contact.clone(),
        fast,
        proxy,
    ) {
        Ok(flow) => flow,
        Err(err) => return json!({"ok": false, "error": err.to_string()}),
    };
    let outcome = match mode {
        "clear-cart" => flow.clear_cart_only().await,
        "prepare" => flow.prepare().await,
        "warmup" => flow.warmup().await,
        "verify" => flow.verify().await,
        "place" => {
            let state = payload.get("state").cloned().unwrap_or(json!({}));
            flow.place_prepared(state).await
        }
        // watch 模式三段式：
        //   watch-setup   建一次结账态（清空购物车 + 加购 + 走到 select_store）
        //   watch-refresh 反复刷新 selectStoreUrl，检查 spokes 是否出现 timeSlot
        //   watch-finish  命中后从 timeSlot 开始走完并提交
        "watch-setup" => match flow.build_pickup_checkout().await {
            Ok(_v) => Ok(json!({
                "ok": true,
                "cookies": flow.cookies,
                "landing": flow.landing,
                "line_item_id": flow.line_item_id,
                "part": flow.part,
                "store": flow.store,
                "stk": flow.stk,
                "model": flow.model,
                "session_id": flow.session_id,
                "openid": flow.openid,
                "spokes": flow.spokes().iter().filter_map(|s| s.get("id").cloned()).collect::<Vec<_>>(),
            })),
            Err(err) => Err(err),
        },
        "watch-refresh" => {
            let state = payload.get("state").cloned().unwrap_or(json!({}));
            flow.restore_from_state(&state);
            match flow.refresh_pickup_checkout().await {
            Ok((spokes, has_time_slot, _data, digest)) => {
                // 方案 C 第 1 步：spokes 有 timeSlot 时，进一步确认「真的有可预约时段」，
                // 避免 spokes 有节点但 daySlots 为空导致下单报 no time slots。
                let (has_real_slot, slot_note) = if has_time_slot {
                    match flow.probe_time_slot_available().await {
                        Ok((ok, note)) => (ok, note),
                        Err(e) => (false, format!("探测失败：{e}")),
                    }
                } else {
                    (false, String::new())
                };
                Ok(json!({
                    "ok": true,
                    "has_time_slot": has_time_slot,
                    "has_real_slot": has_real_slot,
                    "slot_note": slot_note,
                    "digest": digest,
                    "spokes": spokes.iter().filter_map(|s| s.get("id").cloned()).collect::<Vec<_>>(),
                    "part": flow.part,
                    "store": flow.store,
                    "line_item_id": flow.line_item_id,
                    "cookies": flow.cookies,
                    "landing": flow.landing,
                    "stk": flow.stk,
                    "model": flow.model,
                    "session_id": flow.session_id,
                }))
            }
            Err(err) => Err(err),
            }
        },
        "watch-finish" => {
            let state = payload.get("state").cloned().unwrap_or(json!({}));
            flow.restore_from_state(&state);
            match flow.finish_checkout_after_slot().await {
            Ok(v) => Ok(v),
            Err(err) => Err(err),
            }
        },
        _ => flow.run(place).await,
    };
    match outcome {
        Ok(result) => {
            let model = result.get("model").cloned().unwrap_or(json!({}));
            let time_slot = result.get("timeSlotData").cloned().unwrap_or(json!({}));
            let order_number = model
                .get("orderNumber")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let email = contact
                .get("emailAddress")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let payment_link = time_slot
                .get("olssSmartLink")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let payment_link =
                if payment_link.is_empty() && !order_number.is_empty() && !email.is_empty() {
                    format!(
                        "https://www.apple.com.cn/xc/wechat/vieworder/{}/{}",
                        order_number, email
                    )
                } else {
                    payment_link
                };
            json!({
                "ok": true,
                "result": result,
                "error": "",
                "steps": flow.steps,
                "summary": {
                    "part": part,
                    "store": store,
                    "order_number": order_number,
                    "pickup_store": time_slot.get("retailStoreSelected").and_then(Value::as_str).unwrap_or(""),
                    "pickup_date": time_slot.get("selectedDate").and_then(Value::as_str).unwrap_or(""),
                    "pickup_time": time_slot.get("selectedTimeSlot").and_then(Value::as_str).unwrap_or(""),
                    "order_url": payment_link,
                }
            })
        }
        Err(err) => json!({
            "ok": false,
            "result": {},
            "error": err.to_string(),
            "steps": flow.steps,
        }),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 空 Cookie：仍可构造 flow，但 cookie 表为空
    #[test]
    fn empty_cookie_yields_empty_map() {
        let flow = WeChatFlow::new("", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        assert!(flow.cookies.is_empty());
    }

    /// Cookie 解析：正常键值对应
    #[test]
    fn cookie_parsing_basic() {
        let flow = WeChatFlow::new("a=1; b=2; c=3", "MJTL4CH/A", "R480", json!({}), false);
        assert!(flow.is_ok());
        let flow = flow.unwrap();
        assert_eq!(flow.cookies.get("a"), Some(&"1".to_string()));
        assert_eq!(flow.cookies.get("b"), Some(&"2".to_string()));
    }

    /// Cookie 解析：重复键取最后一个
    #[test]
    fn cookie_parsing_duplicate_key() {
        let flow = WeChatFlow::new("a=1; a=2", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        assert_eq!(flow.cookies.get("a"), Some(&"2".to_string()));
    }

    /// Cookie 解析：忽略非法片段
    #[test]
    fn cookie_parsing_invalid_segments() {
        let flow = WeChatFlow::new("valid=1; invalid; another=2", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        assert_eq!(flow.cookies.get("valid"), Some(&"1".to_string()));
        assert_eq!(flow.cookies.get("another"), Some(&"2".to_string()));
        assert_eq!(flow.cookies.len(), 2);
    }

    /// 参数编码：斜杠不被转义（Apple 料号格式要求）
    #[test]
    fn add_cart_part_encoding() {
        let encoded = urlencoding::encode("MJTL4CH/A").replace("%2F", "/");
        assert_eq!(encoded, "MJTL4CH/A");
    }

    /// terms body：无 terms 时输出默认条款键
    #[test]
    fn terms_body_default() {
        let mut flow = WeChatFlow::new("a=1", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        flow.landing = json!({});
        let body = flow.terms_body();
        assert!(body.contains("termsAccepted=1"));
    }
}
