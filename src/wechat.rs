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

/// 时段「打开」结果（GET open_time_slot）——保存时段时真正需要的字段。
///
/// 为什么要单独存：watch 每轮刷新已经用 probe_time_slot_available 打开过一次时段，
/// 命中后 finish 的第一步 save_time_slot 又会再打开一次（实测 1.7~2.5s）。
/// 把打开结果顺着 state 带过去，finish 就能少一次往返（只留保存那一次 POST）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SlotPrefetch {
    /// timeSlot 面板的保存地址（saveUrl）
    pub save_url: String,
    /// 表单字段前缀（keyPath）
    pub key_path: String,
    /// 面板原始响应 JSON
    pub data: Value,
    /// 已选好的时段对象（startTime/endTime/id...）
    pub slot: Value,
    /// 已选好的日期对象（含 date / timeZone）
    pub day: Value,
}

/// 守候阶段提前打开好的某个结账面板，命中后直接 POST，省掉一次 GET。
///
/// 只存「复用 POST 所必需」的三样东西：保存地址、字段前缀、请求体。
/// 面板本身会过期，POST 失败时调用方回退到完整流程（GET 一次再 POST），
/// 所以这里不需要额外判定有效期。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct PanelPrefetch {
    /// 面板的保存地址（saveUrl / selectBillingOptionUrl）
    pub save_url: String,
    /// 表单字段前缀（keyPath）
    pub key_path: String,
    /// 已经拼好的 x-www-form-urlencoded 请求体
    pub body: String,
    /// 面板原始响应 JSON（留档排查用）
    pub data: Value,
}

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
    /// 探测阶段已经打开过的时段面板（命中下单时复用，省掉一次 open_time_slot GET）。
    slot_prefetch: Option<SlotPrefetch>,
    /// 守候阶段提前打开过的「联系人」与「微信支付」面板。
    ///
    /// 这两块面板的字段和库存/时段无关，所以可以在守候期间先 GET 一次把
    /// `saveUrl` / `keyPath` / 表单体算好；命中后直接 POST，
    /// 每个面板省掉 250~690ms 的 GET 往返。
    contact_prefetch: Option<PanelPrefetch>,
    billing_prefetch: Option<PanelPrefetch>,
    /// 已经命中库存、正在提交订单的阶段。
    ///
    /// 这个阶段值得为 541 多等一会儿（限流一般几秒到一分钟就恢复），
    /// 守候阶段的短重试在这里会把到手的库存放掉。
    hit_mode: bool,
    /// 冷态是否已经把「联系人 + 微信支付」真正提交过了。
    ///
    /// 官方流程把这两步放在建态阶段（提交前就绪），热路径只剩
    /// 「存时段 → 提交 → 轮询」。但只有 POST 成功且**时段面板仍可打开**
    /// 才算真的就绪，否则热路径照旧重做一遍（见 `presave_contact_and_payment`）。
    presaved: bool,
    /// 状态页（checkout_status）要求的 `x-aos-model-page` 值。
    ///
    /// Apple 在 place_order 的响应里用 `meta.mandatoryAdditionalHeaders` 明确告诉你
    /// 状态页要用哪个 model-page（实测是 `mobileCheckoutStatusPage`），
    /// 而默认发的是结账页的 `mobileWeChatCheckout`。发错了这一轮只会拿到
    /// 「还在处理中」的 spinner，要再等一轮才读到真正的订单页 —— 白多一次往返（约 1s）。
    status_model_page: Option<String>,
    /// 本流程实际使用的代理（空 = 直连）。预热/预准备会话与下单优先用同一出口 IP，
    /// 让重放的 landing / stk / cookie 与会话上下文自洽（注意：403 与出口 IP 无关）。
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
            // 带代理：按 URL 缓存，避免每次下单都重建连接池。
            //
            // 这里必须先规范化再建 Proxy：`Proxy::all` 只认带协议的 URL，
            // 直接喂 `1.2.3.4:8080` 或 `1.2.3.4|8080|user|pass` 会 builder error。
            // 规范化后的地址同时用作缓存 key，避免同一个出口被拆成两个连接池。
            let key = normalize_proxy_url(proxy_url);
            let cache = PROXY_CLIENTS.get_or_init(|| StdMutex::new(StdHashMap::new()));
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
            slot_prefetch: None,
            contact_prefetch: None,
            billing_prefetch: None,
            hit_mode: false,
            presaved: false,
            status_model_page: None,
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
        // 541 是 Apple 的限流信号，退避要足够长才有机会恢复。
        // 命中之后（hit_mode）订单已经到手边，值得多等：限流通常几秒~1 分钟就恢复，
        // 守候阶段的 3 次重试太短，会把到手的库存白白放掉。
        let retry_delays_ms: &[u64] = if self.hit_mode {
            &[2_000, 4_000, 8_000, 15_000, 25_000, 40_000]
        } else {
            &[3_000, 8_000, 15_000]
        };
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
                // 状态页要用 Apple 指定的 model-page：发错这一轮只会拿到 spinner，
                // 要再等一轮才读到订单页，白多一次往返（实测约 1s）。
                let model_page = self.page_model_for(name);
                request = request.header("x-aos-model-page", model_page);
                if let Some(stk) = self.stk.clone() {
                    request = request.header("x-aos-stk", stk);
                }
            }
            let request = request.build()?;
            // 用 {:#} 打出完整 error source 链：代理连不上和业务错误都长得像
            // "error sending request"，只有展开原因才能分清是代理握手失败还是别的。
            let response = self.client.execute(request).await.map_err(|err| {
                anyhow!(
                    "{name} 请求发送失败（出口 {}）：{err:#}",
                    proxy_host_only(&self.proxy_url)
                )
            })?;
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

        // 不干净：清空 → 加购 1 件 → 复核一次（清空+加购后 Apple 侧结果必须重拉确认）。
        // 干净的情况上面已经直接返回了，所以这里只多付「加购后 1 次 bag」的代价。
        self.clear_cart_with(Some(data)).await?;
        self.add_cart().await?;
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
            "checkout.pickup.cartLineItemId={}&checkout.pickup.currentRetailId={}&timeSlotRequest=true",
            urlencoding::encode(&line_item_id),
            urlencoding::encode(&self.store)
        );
        self.request_json("select_store", Method::POST, &url, Some(&body), true)
            .await
    }

    async fn save_time_slot(&mut self) -> Result<()> {
        // 有 slot_prefetch 就直接用，不再要求 spokes 里有 timeSlot 节点
        // （冷存付款后苹果不再下发该节点，但面板数据已经拿到了）。
        if self.slot_prefetch.is_none()
            && !self
                .spokes()
                .iter()
                .any(|s| s.get("id").and_then(Value::as_str) == Some("timeSlot"))
        {
            // 以前这里会「重选取货方式 INSTORE」，指望苹果因此重新下发时段面板。
            // 实测走不通：会话一旦越过取货选择页，那个视图里就没有
            // `selectPickupMethodUrl`，只会抛一条误导性的
            // `pickup missing selectPickupMethodUrl`，把真正的原因
            //（时段表是空的）盖掉，排障时全被带偏。
            // 缺的是时段，不是取货方式，这里就该直说。
            let spoke_ids = self
                .spokes()
                .iter()
                .filter_map(|s| s.get("id").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(",");
            bail!(
                "no time slots（时段面板未就绪：slot_prefetch 空 · spokes 无 timeSlot · 当前 spokes=[{spoke_ids}]）"
            );
        }
        // 快路径：watch 探测（probe_time_slot_available）已经打开过一次时段面板，
        // 直接复用它的 saveUrl + 已选时段，省掉一次 open_time_slot 往返（实测 1.7~2.5s）。
        // 复用失败（时段被抢/会话变化）就丢掉缓存，退回「现场打开」的老路径。
        if let Some(prefetch) = self.slot_prefetch.clone() {
            match self.save_time_slot_with(prefetch).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    self.slot_prefetch = None;
                    self.steps.push(json!({
                        "name": "save_time_slot_prefetch_stale",
                        "method": "POST",
                        "url": "",
                        "status": 0,
                        "response_type": "",
                        "note": format!("复用探测时段失败，改用现场打开：{err}"),
                        "elapsed_ms": 0,
                    }));
                }
            }
        }
        let url = self.spoke_select_url("timeSlot")?;
        let data = self
            .request_json("open_time_slot", Method::GET, &url, None, true)
            .await?;
        let (day, slot) =
            pick_slot_candidate(&data).ok_or_else(|| anyhow!("no time slots"))?;
        let prefetch = SlotPrefetch {
            save_url: data
                .pointer("/timeSlot/saveUrl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            key_path: data
                .pointer("/timeSlot/keyPath")
                .and_then(Value::as_str)
                .unwrap_or("checkout.timeSlot")
                .to_string(),
            data,
            slot,
            day,
        };
        self.save_time_slot_with(prefetch).await
    }

    /// 保存时段：url / 前缀 / 选中时段全部来自已经打开好的时段面板。
    /// 「打开」由调用方负责（探测阶段复用，或这里现场拉一次）。
    async fn save_time_slot_with(&mut self, prefetch: SlotPrefetch) -> Result<()> {
        let (save_url, body, selected_label) = build_slot_save(&prefetch)?;
        let result = self
            .request_json("save_time_slot", Method::POST, &save_url, Some(&body), true)
            .await?;
        self.selected_time_slot = json!({
            "selectedDate": prefetch.day.get("date").cloned().unwrap_or(json!("")),
            "selectedTimeSlot": selected_label,
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

    /// 打开 3pp（联系人）面板并把「保存联系人所需要的一切」算好。
    ///
    /// 拆出来的理由：守候阶段（watch 预热）需要先 GET 一次面板把 saveUrl 缓存下来，
    /// 命中之后才 POST；这样命中路径省掉一次 GET 往返。
    async fn open_pickup_contact_panel(&mut self) -> Result<PanelPrefetch> {
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
        Ok(PanelPrefetch {
            save_url,
            key_path: addr_prefix.to_string(),
            body,
            data,
        })
    }

    /// 保存联系人（POST）。命中路径调用；`prefetch` 为空时才自己去开面板。
    ///
    /// 预取的面板可能已经过期（守候期每 8 轮预热一次，最老可能已过去 20 多秒，
    /// Apple 的结账面板会失效）。所以 POST 失败时必须**回退到完整流程**：
    /// 重新 GET 一次面板再 POST 一次。没有这条兜底，一个过期面板就会让整单失败。
    async fn save_pickup_contact_with(&mut self, prefetch: Option<PanelPrefetch>) -> Result<()> {
        match prefetch {
            Some(panel) => {
                match self
                    .request_json(
                        "save_pickup_contact",
                        Method::POST,
                        &panel.save_url,
                        Some(&panel.body),
                        true,
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(err) => {
                        // 预取过期：记一笔，走完整流程兜底
                        self.steps.push(json!({
                            "name": "save_pickup_contact_prefetch_stale",
                            "method": "POST",
                            "url": panel.save_url,
                            "status": 0,
                            "note": format!("预取面板已失效，回退完整流程：{err}"),
                            "elapsed_ms": 0,
                        }));
                    }
                }
                let fresh = self.open_pickup_contact_panel().await?;
                self.request_json(
                    "save_pickup_contact",
                    Method::POST,
                    &fresh.save_url,
                    Some(&fresh.body),
                    true,
                )
                .await?;
                Ok(())
            }
            None => {
                let fresh = self.open_pickup_contact_panel().await?;
                self.request_json(
                    "save_pickup_contact",
                    Method::POST,
                    &fresh.save_url,
                    Some(&fresh.body),
                    true,
                )
                .await?;
                Ok(())
            }
        }
    }

    async fn save_pickup_contact(&mut self) -> Result<()> {
        let prefetch = self.contact_prefetch.take();
        self.save_pickup_contact_with(prefetch).await
    }

    /// 打开 billing（付款方式）面板并把选中微信支付所需要的一切算好。
    async fn open_billing_panel(&mut self) -> Result<PanelPrefetch> {
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
        Ok(PanelPrefetch {
            save_url: select_url,
            key_path: prefix,
            body,
            data,
        })
    }

    /// 选中微信支付（POST）。命中路径调用；`prefetch` 为空时才自己去开面板。
    ///
    /// 与联系人面板同理：预取过期时回退到「重新 GET 面板 + 再 POST」，
    /// 不让一个过期面板把整单带死。
    async fn select_wechat_with(&mut self, prefetch: Option<PanelPrefetch>) -> Result<()> {
        match prefetch {
            Some(panel) => {
                match self
                    .request_json(
                        "select_wechat",
                        Method::POST,
                        &panel.save_url,
                        Some(&panel.body),
                        true,
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(err) => {
                        self.steps.push(json!({
                            "name": "select_wechat_prefetch_stale",
                            "method": "POST",
                            "url": panel.save_url,
                            "status": 0,
                            "note": format!("付款面板预取已失效，回退完整流程：{err}"),
                            "elapsed_ms": 0,
                        }));
                    }
                }
                let fresh = self.open_billing_panel().await?;
                self.request_json(
                    "select_wechat",
                    Method::POST,
                    &fresh.save_url,
                    Some(&fresh.body),
                    true,
                )
                .await?;
                Ok(())
            }
            None => {
                let fresh = self.open_billing_panel().await?;
                self.request_json(
                    "select_wechat",
                    Method::POST,
                    &fresh.save_url,
                    Some(&fresh.body),
                    true,
                )
                .await?;
                Ok(())
            }
        }
    }

    async fn select_wechat(&mut self) -> Result<()> {
        let prefetch = self.billing_prefetch.take();
        self.select_wechat_with(prefetch).await
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

    /// 请求该带哪个 `x-aos-model-page`。
    ///
    /// 除了状态页，其它请求一律用结账页的 model。状态页必须用
    /// `mobileCheckoutStatusPage`（Apple 在 `meta.mandatoryAdditionalHeaders` 里指定），
    /// 发成结账页的 model 只会拿到「还在处理中」的 spinner，得多等一轮（约 1s）。
    fn page_model_for(&self, name: &str) -> String {
        if name == "checkout_status" {
            return self
                .status_model_page
                .clone()
                .unwrap_or_else(|| "mobileCheckoutStatusPage".to_string());
        }
        self.model.clone()
    }

    async fn follow_status(&mut self, mut data: Value) -> Result<Value> {
        // Apple 会在响应里告诉你状态页要用的 model-page（实测 mobileCheckoutStatusPage）。
        // 先把它读出来，后面每次轮询都带上正确的那一个，避免第一轮拿到 spinner。
        self.status_model_page = data
            .pointer("/meta/mandatoryAdditionalHeaders/x-aos-model-page")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| self.status_model_page.clone());
        // session_expired 兜底轮询：Apple 偶发在订单已创建的情况下先回一次
        // session_expired，随后状态页仍会变成 thank you。这里多给几次机会，
        // 避免把已成功的订单判成失败（真正的过期会在后续轮询中稳定复现）。
        let mut session_expired_retries = 0u32;
        const SESSION_EXPIRED_RETRIES: u32 = 5;
        for _ in 0..30 {
            // 每一轮都从最新响应里同步一次状态页要求，Apple 可能在任意一轮才给出来。
            if let Some(page) = data
                .pointer("/meta/mandatoryAdditionalHeaders/x-aos-model-page")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                self.status_model_page = Some(page.to_string());
            }
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
        // 冷态收尾（官方第 6/8 步）：真提交联系人 + 微信支付，热路径只剩存时段和提交。
        let presave = self.presave_contact_and_payment().await;
        let mut out = selected;
        if let Some(object) = out.as_object_mut() {
            object.insert("presave".to_string(), presave);
        }
        Ok(out)
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
            "checkout.pickup.cartLineItemId={}&checkout.pickup.currentRetailId={}&timeSlotRequest=true",
            urlencoding::encode(&line_item_id),
            urlencoding::encode(&self.store)
        );
        // 冷存过联系人/支付之后，会话已经越过「取货」这一步 ——
        // 此时只重放 selectStoreUrl 拿回来的 spokes 里没有 timeSlot，
        // 必须先重开门店选择器（官方 9.1）把它拉回来。
        if self.presaved {
            let change_url = self
                .landing
                .pointer("/review/fulfillment/items/0/changeStoreAction")
                .and_then(Value::as_str)
                .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=changeStoreAction&_m=checkout.review.fulfillment.items.0")
                .to_string();
            let change_body = format!(
                "checkout.pickup.cartLineItemId={}&timeSlotRequest=true",
                urlencoding::encode(&line_item_id)
            );
            if let Ok(first) = self
                .request_json(
                    "change_store_hot",
                    Method::POST,
                    &change_url,
                    Some(&change_body),
                    true,
                )
                .await
            {
                self.refresh(&first);
            }
        }
        let data = self
            .request_json("refresh_pickup", Method::POST, &url, Some(&body), true)
            .await?;
        self.refresh(&data);
        let spokes = self.spokes();
        // 冷存付款后苹果不再下发 timeSlot 节点，直接把时段面板塞在响应体顶层
        // （顶层 keys 就是 timeSlot），所以这里不能只看 spokes。
        let has_time_slot = data.get("timeSlot").is_some()
            || spokes
                .iter()
                .any(|spoke| spoke.get("id").and_then(Value::as_str) == Some("timeSlot"));
        // 面板就在响应体里时，顺手把 saveUrl / 已选时段缓存下来，
        // 命中路径的 save_time_slot 就能省掉一次 open_time_slot。
        if data.get("timeSlot").is_some() {
            self.apply_slot_panel(&data);
        }
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
            // 苹果没下发 timeSlot 节点，但把时段面板塞在 selectStore 响应体顶层时，
            // refresh_pickup_checkout 已经用 apply_slot_panel 把材料缓存好了，直接算命中。
            if self.slot_prefetch.is_some() {
                return Ok((
                    true,
                    "面板随 selectStore 下发(已缓存 saveUrl/时段)".to_string(),
                ));
            }
            // 冷存付款之后苹果可能不再下发 timeSlot 节点，但时段面板本身还能打开。
            // 这时退回官方 9.1→9.2 连打路径（重开门店选择器再 selectStoreUrl）。
            if self.presaved {
                return self.probe_slot_hot_once().await;
            }
            return Ok((false, "无 timeSlot 节点".to_string()));
        }
        let url = self.spoke_select_url("timeSlot")?;
        // 先清掉上一轮的预取，避免「这轮没时段」却还留着上轮的材料
        self.slot_prefetch = None;
        let data = self
            .request_json("probe_time_slot", Method::GET, &url, None, true)
            .await?;
        Ok(self.apply_slot_panel(&data))
    }

    /// 解析时段面板并缓存保存材料。返回 (是否真有时段, 说明)。
    ///
    /// `0/0 天有时段` 这种写法分不清两件事，所以这里明确区分：
    ///  - 响应里压根没有 `timeSlot` 对象 → 说明探针被重定向到了别的面板（状态问题）
    ///  - 有 `timeSlot` 但 `daySlots` 是空数组 → 苹果确实还没放号
    fn apply_slot_panel(&mut self, data: &Value) -> (bool, String) {
        self.slot_prefetch = None;
        let Some(ts) = data.get("timeSlot") else {
            let keys = data
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            return (false, format!("响应无 timeSlot 对象(顶层keys={keys})"));
        };
        let Some(day_slots) = ts.get("daySlots").and_then(Value::as_array) else {
            let keys = ts
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            return (false, format!("timeSlot 无 daySlots(keys={keys})"));
        };
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
        let summary = if total_days == 0 {
            format!(
                "0 天(daySlots空数组,date={})",
                ts.get("date").and_then(Value::as_str).unwrap_or("-")
            )
        } else {
            format!("{days_with_slots}/{total_days} 天有时段")
        };
        self.slot_prefetch = if days_with_slots > 0 {
            pick_slot_candidate(data).map(|(day, slot)| SlotPrefetch {
                save_url: data
                    .pointer("/timeSlot/saveUrl")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                key_path: data
                    .pointer("/timeSlot/keyPath")
                    .and_then(Value::as_str)
                    .unwrap_or("checkout.timeSlot")
                    .to_string(),
                data: data.clone(),
                slot,
                day,
            })
        } else {
            None
        };
        (days_with_slots > 0, summary)
    }

    /// 命中态探针：按官方流程 9.1 → 9.2 打，带 `timeSlotRequest=true`。
    ///
    /// 和 `probe_time_slot_available` 的区别是**先重开门店选择器**
    /// （`changeStoreAction`）。苹果放时段比放库存晚 1~6 秒，命中那一刻面板常常是空的，
    /// 只有重走这一步才会把时段生成出来。
    pub async fn probe_slot_hot_once(&mut self) -> Result<(bool, String)> {
        let line_item_id = self.line_item_id.clone().unwrap_or_default();
        if line_item_id.is_empty() {
            bail!("line_item_id missing while probing slot");
        }
        // 两个 URL 都要在第一次请求前取好：refresh() 会覆盖 landing
        let change_url = self
            .landing
            .pointer("/review/fulfillment/items/0/changeStoreAction")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=changeStoreAction&_m=checkout.review.fulfillment.items.0")
            .to_string();
        let select_url = self
            .landing
            .pointer("/pickup/selectStoreUrl")
            .and_then(Value::as_str)
            .unwrap_or("https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=selectStoreUrl&_m=checkout.pickup")
            .to_string();
        // 9.1 重开门店选择器 —— 缺了 timeSlotRequest=true 苹果只回 200 不给时段
        let change_body = format!(
            "checkout.pickup.cartLineItemId={}&timeSlotRequest=true",
            urlencoding::encode(&line_item_id)
        );
        match self
            .request_json(
                "change_store_hot",
                Method::POST,
                &change_url,
                Some(&change_body),
                true,
            )
            .await
        {
            Ok(first) => self.refresh(&first),
            // 这一步失败不致命，9.2 照样可能拿到时段
            Err(err) => self.steps.push(json!({
                "name": "change_store_hot_skip",
                "method": "POST",
                "url": "",
                "status": 0,
                "response_type": "",
                "note": format!("重开选择器失败，直接探时段：{err}"),
                "elapsed_ms": 0,
            })),
        }
        // 9.2 带 timeSlotRequest=true 重放 selectStoreUrl
        let body = format!(
            "checkout.pickup.cartLineItemId={}&checkout.pickup.currentRetailId={}&timeSlotRequest=true",
            urlencoding::encode(&line_item_id),
            urlencoding::encode(&self.store)
        );
        let data = self
            .request_json("probe_slot_hot", Method::POST, &select_url, Some(&body), true)
            .await?;
        self.refresh(&data);
        Ok(self.apply_slot_panel(&data))
    }

    /// 命中后循环探时段：官方口径是空表就重打，最多 8 次、每次约 400ms（≈3.6s）。
    pub async fn ensure_slot_hot(&mut self, attempts: usize, gap_ms: u64) -> (bool, String) {
        let mut last = String::from("未探测");
        for i in 0..attempts.max(1) {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(gap_ms)).await;
            }
            match self.probe_slot_hot_once().await {
                Ok((true, note)) => return (true, format!("{note}（第 {} 发命中）", i + 1)),
                Ok((false, note)) => last = note,
                Err(err) => last = format!("探测失败：{err}"),
            }
        }
        (false, last)
    }

    /// watch 命中后：从 timeSlot 开始把剩余结账步骤走完并提交。
    ///
    /// 前置条件：spokes 里已有 timeSlot（即库存已到货）。
    /// 命中后的收尾流程：补时间段 → 补联系人 → 选微信支付 → 提交订单。
    ///
    /// 只有「时间段」和「提交」必须等命中之后才能做：
    ///   - save_time_slot 需要真实可预约时段（盯梢阶段拿到的 slot_prefetch 已经把它压到一步）；
    ///   - place_order 是唯一的不可逆动作。
    /// 联系人 / 付款方式与库存、时段都无关，所以可以在守候期间提前做完
    /// （见 `prewarm_pickup_contact_and_payment`），把命中后的关键路径从 4.8s 压到 2s 级别。
    pub async fn finish_checkout_after_slot(&mut self) -> Result<Value> {
        // 没有现成时段材料时，按官方口径连打：空表就重打，最多 8 次 × 300ms。
        // 苹果放时段比放库存晚 1~6 秒，只探一次就是拿运气赌。
        if self.slot_prefetch.is_none() {
            let (ok, note) = self.ensure_slot_hot(8, 300).await;
            self.steps.push(json!({
                "name": "ensure_slot_hot",
                "method": "POST",
                "url": "",
                "status": if ok { 200 } else { 0 },
                "response_type": "",
                "note": format!("命中连打探时段：{note}"),
                "elapsed_ms": 0,
            }));
            if !ok {
                bail!("no time slots（连打 8 次×300ms 都没有）：{note}");
            }
        }
        self.save_time_slot().await?;
        if self.presaved {
            // 冷存付款会把 `pickupMethodReady` 打回未就绪，place_order 就会回
            // `order not confirmed: pageType=-`。存完时段后重选一次 INSTORE 把它顶回来。
            self.select_pickup_method().await?;
        }
        // 取货人必须等时段存完才有面板，没法冷存
        self.save_pickup_contact().await?;
        // 微信支付在建态时就选中了（presaved），热路径省掉 open_billing + select_wechat
        if !self.presaved {
            self.select_wechat().await?;
        }
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

    /// 守卫期间的「提前预热」：把与库存 / 时间段无关的两段结账步骤先做完。
    ///
    /// 做的是 finish 路径里的前两步：
    ///   1) save_pickup_contact —— 内部先 GET 3pp 面板（open_pickup_contact 250~690ms）
    ///      再 POST 保存（save_pickup_contact 380~1068ms）
    ///   2) select_wechat —— 内部先 GET billing 面板（open_billing 250~735ms）
    ///      再 POST 选中微信支付（select_wechat 1.2~2.3s，全流程最贵的一步）
    ///
    /// 为什么可以提前：这些字段与「有没有库存 / 有没有时段」完全无关，
    /// Apple 侧的结账态允许先填联系人和付款方式，最后再补时间段。
    /// 失败不致命——finish 路径仍会照常重做一遍（幂等），只是没省下时间。
    pub async fn prewarm_pickup_contact_and_payment(&mut self) -> Value {
        // 只「打开面板」缓存 saveUrl / keyPath / 请求体，**不提前 POST**。
        //
        // 血的教训（实测）：如果在守候期间就把 save_pickup_contact / select_wechat
        // POST 掉，Apple 会把结账态推进到「付款就绪」但把取货方式标成未就绪
        // （pickupMethodReady=false、isShippingReady=true），命中后的 place_order
        // 直接返回 order not confirmed（pageType=-），订单根本提交不上去。
        // 所以预热只做无副作用的 GET，POST 仍然留在命中后的收尾路径里。
        let mut contact_ok = false;
        let mut contact_err = String::new();
        match self.open_pickup_contact_panel().await {
            Ok(panel) => {
                self.contact_prefetch = Some(panel);
                contact_ok = true;
            }
            Err(err) => contact_err = err.to_string(),
        }
        let mut payment_ok = false;
        let mut payment_err = String::new();
        match self.open_billing_panel().await {
            Ok(panel) => {
                self.billing_prefetch = Some(panel);
                payment_ok = true;
            }
            Err(err) => payment_err = err.to_string(),
        }
        // openID 与付款方式无关，顺手缓存，命中时省一次请求
        let openid_ready = self.fetch_openid().await.is_ok();
        json!({
            "ok": true,
            "prewarm": true,
            "contact_ready": contact_ok,
            "contact_error": contact_err,
            "payment_ready": payment_ok,
            "payment_error": payment_err,
            "contact_prefetch": self.contact_prefetch,
            "billing_prefetch": self.billing_prefetch,
            "openid_ready": openid_ready,
            "spokes": self.spokes().iter().filter_map(|s| s.get("id").cloned()).collect::<Vec<_>>(),
            "landing": self.landing,
            "line_item_id": self.line_item_id,
            "part": self.part,
            "store": self.store,
            "stk": self.stk,
            "model": self.model,
            "session_id": self.session_id,
            "cookies": self.cookies,
        })
    }

    /// 冷态把「微信支付」**真正提交**（不是只开面板）。
    ///
    /// 官方流程第 6 步设付款方式、第 8 步填取货人，都在命中之前完成。
    /// 实测只有付款方式能提前做：它和时段无关，冷存后时段面板照旧 `3/3 天有时段`。
    /// **取货人不行** —— 取货人面板要挂在已选时段上，没时段时苹果回的面板里连
    /// `saveThirdPartyPickupUrl` 都没有（`pickupContact missing saveThirdPartyPickupUrl`），
    /// 所以那两步只能在命中后做。
    ///
    /// 冷存后会话会越过「取货」这一步，`refresh_pickup_checkout` 里因此会在
    /// `presaved` 时先重开门店选择器把 timeSlot 拉回来。
    pub async fn presave_contact_and_payment(&mut self) -> Value {
        // 结论（实测两次，均失败，别再来第三次）：
        //   冷存微信支付 → 会话推进到「付款就绪」，`pickupMethodReady` 被打回 false，
        //   place_order 回 `order not confirmed: pageType=-`；
        //   想用重选 INSTORE 修，又因为该视图下已没有 `selectPickupMethodUrl`
        //   直接报 `pickup missing selectPickupMethodUrl`；
        //   冷存取货人则连面板都拿不到（`pickupContact missing saveThirdPartyPickupUrl`，
        //   取货人表单挂在已选时段上）。
        // 官方文档把这两步放在命中前，是因为人家的会话一直停在提交前没动过；
        // 我们这边一提前写，苹果就把取货方式判成未就绪。所以保持不冷存。
        self.presaved = false;
        json!({
            "presaved": false,
            "skipped": "冷存会打掉 pickupMethodReady，实测无法提交，已禁用",
        })
    }

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
        // 中央监控走全流程时最容易死在这里：命中那一刻时段往往还没生成。
        // 按官方口径空表就重打（最多 8 次 × 400ms），别一枪打空就整单作废。
        if self.slot_prefetch.is_none() {
            let (ok, note) = self.ensure_slot_hot(8, 300).await;
            if !ok {
                bail!("no time slots（全流程连打 8 次×300ms 都没有）：{note}");
            }
        }
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
                    // 关键：如果已经走到结账页（spokes 齐全 + landing 有值），
                    // 那这份会话其实是「冷态」——只差时段没存。
                    // 以前这里一律写 checkout_ready=false，监控会把它当废纸丢掉、
                    // 回退 18 步全流程；现在标成 ready 但 slot_saved=false，
                    // 到货时只需收尾（探时段→存时段→联系人→支付→提交）约 6 步。
                    let cold_ready = !self.spokes().is_empty() && !self.landing.is_null();
                    return Ok(json!({
                        "ok": true,
                        "warmup": true,
                        "cart_ready": true,
                        "checkout_ready": cold_ready,
                        "slot_saved": false,
                        "cold_state": cold_ready,
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
            // prepare() 的定义就是「一路走到提交前一步」：选店、存时段、联系人、
            // 支付、openid 全部完成。没这个字段监控就不认这份会话，只能回退全流程，
            // 快通道永远打不中 —— 这里必须显式标出来。
            "checkout_ready": !self.spokes().is_empty(),
            "cookies": self.cookies,
            "session_id": self.session_id,
            "stk": self.stk,
            "model": self.model,
            "line_item_id": self.line_item_id,
            "openid": self.openid,
            "landing": self.landing,
            "part": self.part,
            "store": self.store,
            // 会话里带上建态用的取货人：命中收尾时要重存联系人，
            // 缺了它就会按空值提交（苹果回「请填写此必填字段」，place 变成订单未确认）。
            "contact": self.contact,
            // 标记：这个会话的时段已经存过了，命中后可以直接 place_order。
            // 没这个标记（例如 watch 那种只建态没存时段的会话）必须先走收尾。
            "slot_saved": self.selected_time_slot.as_object().map(|o| !o.is_empty()).unwrap_or(false),
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
        // 复用 watch 探测阶段缓存的时段面板（可能为 null）
        self.slot_prefetch = state
            .get("slot_prefetch")
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value::<SlotPrefetch>(v.clone()).ok());
        // 复用守候阶段预热好的「联系人 / 付款方式」面板（命中时代替一次 GET）
        let prewarm = state.get("prewarm").filter(|v| !v.is_null());
        self.contact_prefetch = prewarm
            .and_then(|p| p.get("contact_prefetch"))
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value::<PanelPrefetch>(v.clone()).ok());
        self.billing_prefetch = prewarm
            .and_then(|p| p.get("billing_prefetch"))
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value::<PanelPrefetch>(v.clone()).ok());
        self.presaved = state
            .get("presaved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 会话里存了取货人就恢复；只有非空对象才覆盖，避免把 payload 传进来的好联系人清空。
        if let Some(contact) = state.get("contact") {
            if contact
                .as_object()
                .map(|o| !o.is_empty())
                .unwrap_or(false)
            {
                self.contact = contact.clone();
            }
        }
    }

    pub async fn place_prepared(&mut self, state: Value) -> Result<Value> {
        self.restore_from_state(&state);
        // 预热会话已包含结账页数据（landing + sessionID + 购物袋）时直接下单，
        // 跳过清空购物车与加购，这是预热提速的主要来源；
        // 数据不完整（无货预热）时回退到完整结账流程，避免直接失败。
        if !self.landing.is_null() && !self.spokes().is_empty() {
            // 只有「时段已经存过」的会话才能直接提交。
            // 否则（只建态没存时段）先走收尾：连打探时段 → 存时段 → 联系人 → 支付 → 提交。
            // 这一步是把冷态会话直接变成可下单状态的关键，省掉 bag/加购/checkout_start/选店。
            if state.get("slot_saved").and_then(Value::as_bool).unwrap_or(false) {
                self.place_order().await
            } else {
                self.finish_checkout_after_slot().await
            }
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

/// 把各种录入格式的代理地址规范成 `Proxy::all` 能吃的 URL。
///
/// 复用 app 模块那一套规则（`1.2.3.4:8080` / `1.2.3.4|8080|user|pass` /
/// `user:pass@1.2.3.4:8080` 都能收），避免同一个出口因为写法不同
/// 被当成两个不同的连接池，或者直接 builder error。
fn normalize_proxy_url(raw: &str) -> String {
    crate::app::normalize_static_proxy_public(raw)
}

/// 只取代理的 host:port 用于日志/报错，绝不带出账号密码。
fn proxy_host_only(proxy_url: &str) -> String {
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() {
        return "直连".to_string();
    }
    match trimmed.rsplit('@').next() {
        Some(tail) if !tail.is_empty() => tail.to_string(),
        _ => "(proxy)".to_string(),
    }
}

/// 链路探针专用客户端：与真实流程同源的构建参数，但**不缓存**，
/// 避免复用已经被污染/失效的连接池。空代理 = 直连。
pub fn build_probe_client(proxy_url: &str) -> Result<Client> {
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() {
        return build_wechat_client(None);
    }
    let normalized = normalize_proxy_url(trimmed);
    let proxy = Proxy::all(&normalized).map_err(|e| anyhow!("代理地址无效（{trimmed}）：{e}"))?;
    build_wechat_client(Some(proxy))
}

/// 探针目标 URL：与真实流程使用同一套 CDN 改写规则。
pub fn probe_url(target: &str) -> String {
    let origin = match target {
        "init" => "https://web.mmap.apple.com/miniprogram-merch/p/wechat/init",
        "bag" => BAG,
        _ => FETCH_OPENID,
    };
    rewrite_to_cdn(origin)
}

/// 探针要覆写的 Host 头（服务端按 Host 路由）。
pub fn probe_origin_host() -> &'static str {
    ORIGIN_HOST
}

fn is_apple_cover_page(text: &str) -> bool {
    let lower = text.to_lowercase();
    // covers.html 是 Apple 的拦截/等待页特征
    lower.contains("covers.html")
        || lower.contains("<!doctype html>") && lower.contains("wow-assets")
        || text.contains("<!-- covers -->")
}

/// 打开时段面板的响应 → (选中的日期, 选中的时段)。
///
/// 挑法与原实现一致：取第一个「有时段」的日期，其中优先下午（>=12:00）。
/// 探测阶段和保存阶段共用，保证复用探测结果时选出的时段与现场打开时同源。
fn pick_slot_candidate(data: &Value) -> Option<(Value, Value)> {
    let day = first_day_with_slots(data)?;
    let slot = pick_slot_in_day(&day)?;
    Some((day, slot))
}

fn first_day_with_slots(data: &Value) -> Option<Value> {
    data.pointer("/timeSlot/daySlots")
        .and_then(Value::as_array)?
        .iter()
        .find(|day| {
            day.get("timeSlots")
                .and_then(Value::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        })
        .cloned()
}

fn pick_slot_in_day(day: &Value) -> Option<Value> {
    let parse_hour = |value: &str| -> u32 {
        let value = value.trim().to_uppercase();
        let hour: u32 = value
            .split(':')
            .next()
            .and_then(|part| part.parse().ok())
            .unwrap_or(0);
        if value.contains("PM") && hour != 12 {
            hour + 12
        } else {
            hour
        }
    };
    let all = day
        .get("timeSlots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let afternoon: Vec<Value> = all
        .iter()
        .filter(|slot| {
            slot.get("startTime")
                .and_then(Value::as_str)
                .map(|v| parse_hour(v) >= 12)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let pool = if afternoon.is_empty() { all } else { afternoon };
    pool.into_iter().next()
}

/// 由「已打开好的时段面板」拼出保存请求：(saveUrl, 表单体, 时段展示文案)。
///
/// 抽成纯函数：一是让复用探测结果与现场打开走同一段拼装逻辑（不会悄悄走偏），
/// 二是可以脱网单测。选时段优先下午，与原实现一致。
fn build_slot_save(prefetch: &SlotPrefetch) -> Result<(String, String, String)> {
    let ts = prefetch.data.get("timeSlot").cloned().unwrap_or(json!({}));
    let day = &prefetch.day;
    let slot = if prefetch.slot.is_null() {
        pick_slot_in_day(day).ok_or_else(|| anyhow!("no slot"))?
    } else {
        prefetch.slot.clone()
    };
    let save_url = ts.get("saveUrl").and_then(Value::as_str).unwrap_or("");
    if save_url.is_empty() {
        bail!("timeSlot missing saveUrl");
    }
    let prefix = ts
        .get("keyPath")
        .and_then(Value::as_str)
        .unwrap_or("checkout.timeSlot");
    let mut body = String::new();
    // 探测阶段打开的面板可能不带 selectedMethod，取不到就按到店取货兜底
    // （空值 Apple 目前也接受，但不值得赌它一直宽松）。
    let selected_method = ts
        .get("selectedMethod")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
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
    let label = slot
        .get("time")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "{} – {}",
                slot.get("startTime").and_then(Value::as_str).unwrap_or(""),
                slot.get("endTime").and_then(Value::as_str).unwrap_or("")
            )
        });
    Ok((save_url.to_string(), body, label))
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
            flow.hit_mode = true;
            flow.place_prepared(state).await
        }
        // 守候期间提前把「联系人 + 微信支付」两段做完。
        // 命中之后 finish 只需要 save_time_slot → place_order，关键路径大幅缩短。
        "watch-prewarm" => {
            let state = payload.get("state").cloned().unwrap_or(json!({}));
            flow.restore_from_state(&state);
            Ok(flow.prewarm_pickup_contact_and_payment().await)
        }
        // 命中路径的「面板复用」验证器（只做面板 POST，不下单）。
        //
        // 用途：命中那一刻到底有没有真的省掉两次 GET，光看代码不算数。
        // 这个模式拿同一份预取、按命中路径的真实顺序把两步 POST 打出去，
        // 然后把 steps 原样返回 —— 里面只会出现 save_pickup_contact /
        // select_wechat 两条 POST，不出现 open_pickup_contact / open_billing，
        // 就证明复用成立。带 `stale=true` 时故意塞一个坏地址，
        // 验证「预取过期 → 回退完整流程」的兜底确实生效。
        "panel-reuse" => {
            let state = payload.get("state").cloned().unwrap_or(json!({}));
            let stale = payload
                .get("stale")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // state 里没有可用的结账态（没有 landing / spokes）时现场建一次，
            // 这样整条链路能在同一次调用里跑完：
            //   建态 → 预热 → 序列化进快照 → restore_from_state → 命中路径的两步 POST
            let usable = state
                .get("landing")
                .map(|v| !v.is_null() && v.as_object().map(|o| !o.is_empty()).unwrap_or(false))
                .unwrap_or(false);
            let mut built = false;
            let mut build_error = String::new();
            if !usable {
                match flow.build_pickup_checkout().await {
                    Ok(_) => built = true,
                    Err(err) => build_error = err.to_string(),
                }
            } else {
                flow.restore_from_state(&state);
            }
            if !build_error.is_empty() {
                Ok(json!({
                    "ok": false,
                    "built_state": false,
                    "error": format!("建态失败，无法做面板复用验证：{build_error}"),
                    "steps": flow.steps,
                }))
            } else {
                let prewarmed_here = if built {
                    Some(flow.prewarm_pickup_contact_and_payment().await)
                } else {
                    None
                };
                // 走一遍 app.rs 里「预热结果 → watch 快照」的序列化再 restore 回来，
                // 保证复用不是靠同一次进程内的对象残留，而是真的过了一次快照。
                let snapshot = json!({
                    "cookies": flow.cookies.clone(),
                    "landing": flow.landing.clone(),
                    "stk": flow.stk.clone(),
                    "session_id": flow.session_id.clone(),
                    "line_item_id": flow.line_item_id.clone(),
                    "openid": flow.openid.clone(),
                    "model": flow.model.clone(),
                    "prewarm": json!({
                        "contact_prefetch": flow.contact_prefetch.clone(),
                        "billing_prefetch": flow.billing_prefetch.clone(),
                    }),
                });
                flow.contact_prefetch = None;
                flow.billing_prefetch = None;
                flow.restore_from_state(&snapshot);
                if stale {
                    // 故意指向一个必然失效的地址，触发回退分支
                    let bogus = PanelPrefetch {
                        save_url: "https://web.mmap.apple.com/moe/p/wechat/checkoutx/__stale__"
                            .to_string(),
                        key_path: "checkout.pickupContact".to_string(),
                        body: "__stale__=1".to_string(),
                        data: json!({}),
                    };
                    flow.contact_prefetch = Some(bogus.clone());
                    flow.billing_prefetch = Some(bogus);
                }
                let had_contact = flow.contact_prefetch.is_some();
                let had_billing = flow.billing_prefetch.is_some();
                let started = std::time::Instant::now();
                let contact = flow.save_pickup_contact().await;
                let payment = flow.select_wechat().await;
                let _ = &mut built;
                Ok(json!({
                    "ok": contact.is_ok() && payment.is_ok(),
                    "built_state": built,
                    "stale": stale,
                    "prefetch_restored": {"contact": had_contact, "billing": had_billing},
                    "prewarm": prewarmed_here,
                    "elapsed_ms": started.elapsed().as_millis(),
                    "contact_ok": contact.is_ok(),
                    "contact_error": contact.err().map(|e| e.to_string()).unwrap_or_default(),
                    "payment_ok": payment.is_ok(),
                    "payment_error": payment.err().map(|e| e.to_string()).unwrap_or_default(),
                    "steps": flow.steps,
                }))
            }
        }
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
                "presaved": flow.presaved,
                "presave": _v.get("presave").cloned().unwrap_or(json!(null)),
                "spokes": flow.spokes().iter().filter_map(|s| s.get("id").cloned()).collect::<Vec<_>>(),
            })),
            Err(err) => Err(err),
        },
        "watch-refresh" => {            let state = payload.get("state").cloned().unwrap_or(json!({}));
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
                    "slot_prefetch": flow.slot_prefetch,
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
            // 命中提交阶段：541 多等几轮，别把到手的库存放掉
            flow.hit_mode = true;
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

    /// 时段面板 → 保存请求：复用探测结果时拼出的表单必须与现场打开一致
    /// （少字段就会被 Apple 判为「未选时段」，整单提交被拒）。
    #[test]
    fn slot_save_body_from_prefetch() {
        let panel = json!({
            "timeSlot": {
                "saveUrl": "https://web.mmap.apple.com/moe/p/wechat/v2/checkoutx?_a=saveUrl&_m=checkout.timeSlot",
                "keyPath": "checkout.timeSlot",
                "selectedMethod": "INSTORE",
                "daySlots": [
                    { "date": "2026-10-17", "timeSlots": [] },
                    { "date": "2026-10-18", "timeZone": "Asia/Shanghai", "timeSlots": [
                        { "startTime": "10:00 AM", "endTime": "10:15 AM", "id": "MORNING" },
                        { "startTime": "2:00 PM", "endTime": "2:15 PM", "id": "AFTERNOON" }
                    ]}
                ]
            }
        });
        let (day, slot) = pick_slot_candidate(&panel).expect("应挑出有时段的日期");
        assert_eq!(day["date"], json!("2026-10-18"), "要跳过没有时段的日期");
        assert_eq!(slot["id"], json!("AFTERNOON"), "优先下午时段");

        let (url, body, label) = build_slot_save(&SlotPrefetch {
            save_url: "".into(),
            key_path: "".into(),
            data: panel,
            slot,
            day,
        })
        .unwrap();
        assert!(url.contains("_a=saveUrl"));
        assert!(body.contains("checkout.timeSlot.selectedMethod=INSTORE"));
        assert!(body.contains("&checkout.timeSlot.date=2026-10-18"));
        assert!(body.contains("&checkout.timeSlot.timeZone=Asia%2FShanghai"));
        assert!(body.contains("&checkout.timeSlot.startTime=2%3A00%20PM"));
        assert!(body.contains("&checkout.timeSlot.endTime=2%3A15%20PM"));
        assert!(body.contains("&checkout.timeSlot.selectedTimeSlot=AFTERNOON"));
        assert!(body.ends_with("&checkout.timeSlot.recommendedTimeSlot=false"));
        assert_eq!(label, "2:00 PM – 2:15 PM");
    }

    /// terms body：无 terms 时输出默认条款键
    #[test]
    fn terms_body_default() {
        let mut flow = WeChatFlow::new("a=1", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        flow.landing = json!({});
        let body = flow.terms_body();
        assert!(body.contains("termsAccepted=1"));
    }

    /// 状态页要用 Apple 指定的 model-page，其它请求用结账页的 model。
    ///
    /// 实测依赖：place_order 响应里 `meta.mandatoryAdditionalHeaders["x-aos-model-page"]`
    /// 给的是 `mobileCheckoutStatusPage`；发成 `mobileWeChatCheckout` 时第一轮状态查询
    /// 只会返回 spinner（delay 0.5），要再等一轮才读到订单页 —— 白多一次往返约 1s。
    #[test]
    fn status_page_uses_its_own_model_header() {
        let mut flow = WeChatFlow::new("a=1", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        // 默认：状态页有专用值，其它步骤用结账 model
        assert_eq!(flow.page_model_for("checkout_status"), "mobileCheckoutStatusPage");
        assert_eq!(flow.page_model_for("place_order"), flow.model.clone());
        assert_eq!(flow.page_model_for("save_time_slot"), flow.model.clone());

        // Apple 指定了别的值时以 Apple 为准
        flow.status_model_page = Some("someOtherStatusPage".to_string());
        assert_eq!(flow.page_model_for("checkout_status"), "someOtherStatusPage");
        // 且不能污染普通步骤
        assert_eq!(flow.page_model_for("place_order"), flow.model.clone());
    }

    /// 预热出来的联系人 / 付款面板预取，必须能顺着 state 一路带到命中下单那一步。
    ///
    /// 这条链路出过一次真实事故：watch 建态时预热成功，但后续重建快照时把
    /// `prewarm` 字段丢了，命中那一刻又得重新 open 两个面板，白丢 500~1100ms。
    /// 这里做一次「预热 → 序列化进快照 → restore_from_state」的往返，
    /// 保证面板预取不会在中途被吃掉。
    #[test]
    fn prewarm_panels_survive_snapshot_roundtrip() {
        let mut flow = WeChatFlow::new("a=1", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        flow.contact_prefetch = Some(PanelPrefetch {
            save_url: "https://example.invalid/3pp/save".to_string(),
            key_path: "checkout.pickupContact".to_string(),
            body: "checkout.pickupContact.firstName=%E4%B8%89".to_string(),
            data: json!({"ok": true}),
        });
        flow.billing_prefetch = Some(PanelPrefetch {
            save_url: "https://example.invalid/billing/save".to_string(),
            key_path: "checkout.billingOptions".to_string(),
            body: "checkout.billingOptions.selectedBillingOption=wechat".to_string(),
            data: json!({"ok": true}),
        });

        // 模拟 app.rs 把预热结果塞进 watch 快照的那一段
        let prewarm = json!({
            "ok": true,
            "contact_prefetch": flow.contact_prefetch.clone(),
            "billing_prefetch": flow.billing_prefetch.clone(),
            "openid_ready": true,
        });
        let snapshot = json!({
            "cookies": {"masts": "x", "dssid2": "y"},
            "landing": {"k": "v"},
            "stk": "stk-1",
            "session_id": "sid-1",
            "prewarm": prewarm,
            "part": "MJTL4CH/A",
            "store": "R480",
            "proxy_url": "http://user:pass@1.2.3.4:8080",
        });

        let mut restored = WeChatFlow::new("a=1", "MJTL4CH/A", "R480", json!({}), false).unwrap();
        restored.restore_from_state(&snapshot);

        let contact = restored.contact_prefetch.expect("联系人面板预取丢了");
        assert_eq!(contact.key_path, "checkout.pickupContact");
        assert_eq!(contact.save_url, "https://example.invalid/3pp/save");
        assert!(contact.body.contains("firstName"));
        let billing = restored.billing_prefetch.expect("付款面板预取丢了");
        assert_eq!(billing.key_path, "checkout.billingOptions");
        assert!(billing.body.contains("wechat"));
        // 顺带确认会话关键字段也带过来了
        assert_eq!(restored.stk.as_deref(), Some("stk-1"));
        assert_eq!(restored.session_id.as_deref(), Some("sid-1"));
    }
}
