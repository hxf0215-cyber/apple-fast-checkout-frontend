use anyhow::{anyhow, bail, Result};
use regex::Regex;
use reqwest::{cookie::Jar, header, Client, Method, Proxy, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

use crate::models::{AccountConfig, CookieMap, GlobalSettings};

const MALL: &str = "https://www.apple.com.cn";
const MMAP: &str = "https://web.mmap.apple.com/26.07.0-MAIN";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 Edg/154.0.0.0";
const SEC_UA: &str = r#""Chromium";v="154", "Microsoft Edge";v="154", "Not A(Brand";v="99""#;
pub const APPLE_CDN_IPS: &[&str] = &[
    "106.116.184.10",
    "106.116.184.167",
    "106.116.184.169",
    "124.238.121.57",
    "124.238.121.58",
    "124.238.121.60",
    "124.238.121.246",
    "124.238.121.247",
    "124.238.121.248",
    "124.238.121.249",
    "124.239.227.47",
    "124.239.227.48",
    "124.239.227.49",
    "124.239.227.50",
    "124.239.227.51",
    "124.239.227.52",
    "124.239.227.53",
    "124.239.227.54",
    "124.239.232.99",
];

#[derive(Clone)]
pub struct AppleClient {
    client: Client,
    jar: Arc<Jar>,
    cookies: CookieMap,
    last_status: Option<u16>,
    last_actk: Option<String>,
    last_add_response: String,
    bag_items: Vec<(String, String)>,
    item_id: Option<String>,
    mall_stk: Option<String>,
}

fn fetch_call_id() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    )
}

fn origin_of(url: &str) -> String {
    url.split('/').take(3).collect::<Vec<_>>().join("/")
}

impl AppleClient {
    pub fn new(proxy_url: &str, timeout_ms: u64) -> Result<Self> {
        Self::new_with_resolve_ip(proxy_url, timeout_ms, "")
    }

    pub fn new_with_resolve_ip(proxy_url: &str, timeout_ms: u64, resolve_ip: &str) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let mut builder = Client::builder()
            .user_agent(USER_AGENT)
            .http1_only()
            .timeout(Duration::from_millis(timeout_ms))
            .connect_timeout(Duration::from_millis(4000))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .cookie_provider(jar.clone());

        if !proxy_url.trim().is_empty() {
            let proxy = Proxy::all(proxy_url.trim())?;
            builder = builder.proxy(proxy);
        } else {
            builder = builder.no_proxy();
            if !resolve_ip.trim().is_empty() {
                let address: SocketAddr = format!("{}:443", resolve_ip.trim())
                    .parse()
                    .map_err(|e| anyhow!("invalid CDN IP {resolve_ip}: {e}"))?;
                builder = builder.resolve("www.apple.com.cn", address);
            }
        }

        Ok(Self {
            client: builder.build()?,
            jar,
            cookies: CookieMap::new(),
            last_status: None,
            last_actk: None,
            last_add_response: String::new(),
            bag_items: Vec::new(),
            item_id: None,
            mall_stk: None,
        })
    }

    pub fn set_login_cookies(&mut self, cookies: &CookieMap) {
        let url = "https://www.apple.com.cn".parse().unwrap();
        for (name, value) in cookies {
            // dssid2 is account/session stable; shld_bt_ck is refreshed on expiry.
            if !matches!(
                name.as_str(),
                "myacinfo" | "aasp" | "acn01" | "dssid2" | "shld_bt_ck"
            ) {
                continue;
            }
            self.jar.add_cookie_str(
                &format!("{}={}; Domain=.apple.com.cn; Path=/", name, value),
                &url,
            );
            self.cookies.insert(name.clone(), value.clone());
        }
    }

    pub async fn mmap_store_numbers(
        &self,
        province: &str,
        city: &str,
        district: &str,
    ) -> Result<Vec<String>> {
        let url = format!("{MMAP}/miniprogram/retailapp/p/wechat/stores/storesFederatedSearch");
        let response = self
            .client
            .get(url)
            .query(&[
                ("state", province),
                ("city", city),
                ("district", district),
                ("sba", "true"),
                ("type", "json"),
            ])
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        if status != 200 {
            bail!("mmap store search failed with {}", status);
        }
        let json: Value = serde_json::from_str(&body)?;
        if json.get("hasError") == Some(&json!(true)) {
            bail!("mmap store search returned an error");
        }
        let stores = json
            .get("storeNumbers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(stores
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect())
    }

    pub async fn mmap_stock_details(&self, part: &str, stores: &[String]) -> Result<Vec<Value>> {
        if stores.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for chunk in stores.chunks(12) {
            let details = self.mmap_stock_details_chunk(part, chunk).await?;
            result.extend(details);
        }
        Ok(result)
    }

    async fn mmap_stock_details_chunk(&self, part: &str, stores: &[String]) -> Result<Vec<Value>> {
        let url = format!("{MMAP}/miniprogram-merch/p/wechat/product-locator/quotes");
        let store_list = stores.join(",");
        let response = self
            .client
            .get(url)
            .query(&[("pn", part), ("stores", store_list.as_str())])
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        if status != 200 {
            bail!("mmap stock query failed with {}", status);
        }
        let json: Value = serde_json::from_str(&body)?;
        if json.get("error") == Some(&json!(true)) {
            bail!("mmap stock query returned an error");
        }

        let mut chunk_result = Vec::new();
        for (section, status) in [
            ("availableNow", "available"),
            ("availableLater", "available_later"),
            ("unavailable", "unavailable"),
        ] {
            let Some(items) = json.get(section).and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                let store_id = item
                    .get("storeNumber")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = item
                    .pointer("/dude20Data/storeDisplayName")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let quote = item
                    .get("pickupQuote")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                chunk_result.push(json!({
                    "store_id": store_id,
                    "name": name,
                    "city": "",
                    "state": "",
                    "status": status,
                    "quote": quote,
                    "raw_status": item.get("availabilityStatus"),
                }));
            }
        }
        Ok(chunk_result)
    }

    pub fn session_cookies(&self) -> CookieMap {
        let mut cookies = CookieMap::new();
        for name in ["dssid2", "shld_bt_ck", "as_atb"] {
            if let Some(value) = self.parse_cookie(name) {
                cookies.insert(name.to_string(), value);
            }
        }
        cookies
    }

    fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn save_cookies(&mut self, resp: &reqwest::Response) {
        for value in resp.headers().get_all(header::SET_COOKIE) {
            if let Ok(raw) = value.to_str() {
                if let Some((pair, _)) = raw.split_once(';') {
                    if let Some((name, value)) = pair.split_once('=') {
                        self.cookies
                            .insert(name.trim().to_string(), value.trim().to_string());
                    }
                }
            }
        }
    }

    pub async fn request(
        &mut self,
        method: &str,
        url: &str,
        body: Option<&str>,
        headers: Option<Vec<(&str, &str)>>,
    ) -> Result<(u16, String)> {
        let mut req = self
            .client
            .request(Method::from_bytes(method.to_uppercase().as_bytes())?, url);
        req = req.header(header::COOKIE, self.cookie_header());
        req = req
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Upgrade-Insecure-Requests", "1")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .header("sec-ch-ua", SEC_UA)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", r#""Windows""#);
        if let Some(hs) = headers {
            for (name, value) in hs {
                req = req.header(name, value);
            }
        }
        if let Some(content) = body {
            req = req.body(content.to_string());
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        self.last_status = Some(status);
        if let Some(actk) = resp
            .headers()
            .get("x-as-actk")
            .and_then(|v| v.to_str().ok())
        {
            self.last_actk = Some(actk.to_string());
        }
        self.save_cookies(&resp);
        let text = resp.text().await?;
        Ok((status, text))
    }

    pub async fn get(&mut self, url: &str) -> Result<(u16, String)> {
        self.request("GET", url, None, None).await
    }

    pub async fn post_form(
        &mut self,
        url: &str,
        body: &str,
        headers: Vec<(&str, &str)>,
    ) -> Result<(u16, String)> {
        self.request("POST", url, Some(body), Some(headers)).await
    }

    pub fn current_item_id(&self) -> Option<String> {
        self.item_id.clone()
    }

    pub fn last_add_response(&self) -> String {
        self.last_add_response.clone()
    }

    pub fn current_bag_items(&self) -> Vec<(String, String)> {
        self.bag_items.clone()
    }

    fn parse_cookie(&self, name: &str) -> Option<String> {
        self.cookies.get(name).cloned()
    }

    pub async fn bootstrap(&mut self, config: &AccountConfig) -> Result<()> {
        self.set_login_cookies(&config.login_cookies);
        self.ensure_mall_session().await?;
        self.set_location(config).await?;
        self.add_to_bag(config).await?;
        self.bag_snapshot(&config.part).await?;
        if self.item_id.is_none() {
            bail!("configured part not found in bag");
        }
        Ok(())
    }

    pub async fn ensure_mall_session(&mut self) -> Result<()> {
        let entries = [
            "/",
            "/shop/bag",
            "/shop/buy-iphone/iphone-17",
            "/shop/buy-iphone/iphone-18-pro",
            "/iphone-18-pro/",
        ];
        let mut got_dssid = false;
        for _ in 0..3 {
            for entry in entries.iter() {
                let (status, _) = self.get(&format!("{}{}", MALL, entry)).await?;
                if status >= 400 {
                    continue;
                }
                if self.parse_cookie("dssid2").is_some() {
                    got_dssid = true;
                    break;
                }
            }
            if got_dssid {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !got_dssid {
            bail!("failed to acquire dssid2");
        }
        if self.shld_expiring(1800) {
            self.solve_shld().await?;
        }
        let atb_headers = vec![
            (
                "Accept",
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            ),
            ("Sec-Fetch-Site", "same-origin"),
            ("Sec-Fetch-Mode", "no-cors"),
            ("Sec-Fetch-Dest", "image"),
            (
                "Referer",
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro",
            ),
        ];
        let (status, _) = self
            .request(
                "GET",
                &format!("{}/shop/beacon/atb", MALL),
                None,
                Some(atb_headers),
            )
            .await?;
        if status >= 400 || self.parse_cookie("as_atb").is_none() {
            bail!("failed to acquire as_atb");
        }
        Ok(())
    }

    pub async fn set_location(&mut self, config: &AccountConfig) -> Result<()> {
        let state = urlencoding::encode(&config.province).to_string();
        let city = urlencoding::encode(&config.city).to_string();
        let district = urlencoding::encode(&config.district).to_string();
        let body = format!(
            "state={state}&city={city}&district={district}&countryCode=CN&locationConsent=false"
        );
        let headers = vec![
            ("Accept", "application/json"),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Origin", MALL),
            (
                "Referer",
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro",
            ),
        ];
        let (status, _) = self
            .request(
                "POST",
                &format!("{}/shop/address/location/update", MALL),
                Some(&body),
                Some(headers),
            )
            .await?;
        if status != 200 {
            bail!("location update failed with {}", status);
        }
        Ok(())
    }

    pub fn shld_expiring(&self, margin_secs: u64) -> bool {
        self.parse_cookie("shld_bt_ck").is_none()
            || self.cookie_age_secs() > 7200u64.saturating_sub(margin_secs)
    }

    fn cookie_age_secs(&self) -> u64 {
        let issued = self
            .parse_cookie("shld_bt_ck")
            .and_then(|v| v.split('|').nth(1).map(str::to_string))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        (now - issued).max(0) as u64
    }

    async fn solve_shld(&mut self) -> Result<()> {
        let url = format!("{}/shop/shld/work/v2_1/q?jr=255", MALL);
        for _ in 0..4 {
            let (status, body) = self.get(&url).await?;
            if status != 200 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            let challenge: Value = serde_json::from_str(&body)?;
            let version = challenge
                .get("challenge_version")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let solve_started = std::time::Instant::now();
            let number: Value = if version == 1 {
                let result = challenge
                    .get("result")
                    .and_then(Value::as_str)
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| anyhow!("missing result"))?;
                let parts = challenge.get("parts").and_then(Value::as_i64).unwrap_or(0) as u32;
                let low = challenge.get("low").and_then(Value::as_i64).unwrap_or(1) as u64;
                let high = challenge.get("high").and_then(Value::as_i64).unwrap_or(0) as u64;
                let values =
                    solve_v1(result, parts, low, high).ok_or_else(|| anyhow!("v1 solve failed"))?;
                json!(values)
            } else {
                let salt = challenge.get("salt").and_then(Value::as_str).unwrap_or("");
                let target = challenge
                    .get("challenge")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let nonce = solve_sha256(salt, target).ok_or_else(|| anyhow!("v2 solve failed"))?;
                json!(nonce)
            };

            let mut payload = challenge.clone();
            payload["number"] = number;
            let took_ms = if version == 1 {
                solve_started.elapsed().as_millis().max(1)
            } else {
                1
            };
            payload["took"] = json!(took_ms);
            payload["flagskv"] = json!({"patSkip": true});
            payload["jsa"] = json!({"s": 255, "f": []});
            let (post_status, _) = self
                .request(
                    "POST",
                    &url,
                    Some(&payload.to_string()),
                    Some(vec![("Content-Type", "text/plain;charset=UTF-8")]),
                )
                .await?;
            if post_status == 200 && self.parse_cookie("shld_bt_ck").is_some() {
                return Ok(());
            }
        }
        bail!("shld_bt_ck was not issued")
    }

    pub async fn bag_snapshot(&mut self, part: &str) -> Result<(Vec<String>, Option<String>)> {
        let cache_bust = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let (status, body) = self
            .get(&format!("{}/shop/bag?_={}", MALL, cache_bust))
            .await?;
        if status != 200 {
            bail!("bag request failed with {}", status);
        }
        let cart_re = Regex::new(r#""c":\[(.*?)\]"#)?;
        let sku_re = Regex::new(r#""sku"\s*:\s*"([^"]+)""#)?;
        let stk_re = Regex::new(r#""x-aos-stk"\s*:\s*"([^"]+)""#)?;
        let mut items: Vec<String> = Vec::new();
        for capture in cart_re.captures_iter(&body) {
            let candidates: Vec<String> = capture[1]
                .split(',')
                .map(|value| value.trim_matches('"').to_string())
                .filter(|value| value.starts_with("item-"))
                .collect();
            if !candidates.is_empty() {
                items = candidates;
                break;
            }
        }
        let stk = stk_re.captures(&body).map(|m| m[1].to_string());
        let item_skus: Vec<(String, String)> = items
            .iter()
            .map(|item| {
                let needle = format!("\"{}\"", item);
                let start = body.find(&needle).unwrap_or(0);
                let end = body
                    .char_indices()
                    .map(|(offset, _)| offset)
                    .filter(|offset| offset >= &start)
                    .find(|offset| offset >= &start.saturating_add(5000))
                    .unwrap_or(body.len());
                let sku = sku_re
                    .captures(&body[start..end])
                    .and_then(|capture| capture.get(1))
                    .map(|value| value.as_str().to_string())
                    .unwrap_or_default();
                (item.clone(), sku)
            })
            .collect();
        self.bag_items = item_skus.clone();
        self.item_id = item_skus
            .iter()
            .find(|(_, sku)| sku.eq_ignore_ascii_case(part))
            .map(|(item, _)| item.clone());
        self.mall_stk = stk.clone();
        Ok((items, stk))
    }

    pub async fn add_to_bag(&mut self, config: &AccountConfig) -> Result<()> {
        let part = urlencoding::encode(&config.part).to_string();
        let atb = self
            .parse_cookie("as_atb")
            .and_then(|v| v.split('|').nth(2).map(str::to_string))
            .ok_or_else(|| anyhow!("missing atbtoken"))?;
        let url = format!(
            "{}/cn/shop/bag/add?product={}&storefront=cn&atbtoken={}",
            MALL, part, atb
        );
        let headers = vec![
            ("Accept", "application/json"),
            ("x-skip-redirect", "true"),
            (
                "Referer",
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro",
            ),
        ];
        let (status, body) = self.request("GET", &url, None, Some(headers)).await?;
        if status != 200 {
            bail!("add to bag failed with {}", status);
        }
        let result: Value = serde_json::from_str(&body)?;
        self.last_add_response = body.clone();
        if result.get("addedToBag") != Some(&json!(true)) {
            bail!("add to bag rejected: {}", &body[..body.len().min(200)]);
        }
        Ok(())
    }

    pub async fn stock_details(&self, part: &str, stores: &[String]) -> Result<Vec<Value>> {
        self.mmap_stock_details(part, stores).await
    }

    pub async fn stock_details_multi(
        &self,
        parts: &[String],
        stores: &[String],
    ) -> Result<HashMap<String, Vec<Value>>> {
        if parts.is_empty() {
            return Ok(HashMap::new());
        }
        let mut tasks = JoinSet::new();
        for part in parts {
            let client = self.clone();
            let part = part.clone();
            let stores = stores.to_vec();
            tasks.spawn(async move {
                // 每个型号独立重试：Apple 库存接口偶发返回非 JSON（上游抖动），
                // 单次失败不应让整个多型号查询失败（100+ 型号时尤其明显）。
                let mut last_err = String::new();
                for attempt in 0..3u32 {
                    match client.mmap_stock_details(&part, &stores).await {
                        Ok(details) => return Ok::<_, String>((part.clone(), details)),
                        Err(err) => {
                            last_err = err.to_string();
                            if attempt < 2 {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    250 * (attempt as u64 + 1),
                                ))
                                .await;
                            }
                        }
                    }
                }
                Err(format!("{part}: {last_err}"))
            });
        }
        let mut result = HashMap::new();
        let mut failures: Vec<String> = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok((part, details))) => {
                    result.insert(part, details);
                }
                Ok(Err(msg)) => failures.push(msg),
                Err(join_err) => failures.push(join_err.to_string()),
            }
        }
        // 部分型号失败：返回已成功的部分，调用方仍可用（避免 100+ 型号一损俱损）；
        // 全部失败才上报错误。
        if result.is_empty() && !failures.is_empty() {
            bail!("库存查询全部失败：{}", failures.join("; "));
        }
        Ok(result)
    }

    async fn post_checkout(
        &self,
        secure: &str,
        path: &str,
        data: &str,
        page: &str,
        ref_url: &str,
        stk_val: &str,
        actk: Option<&str>,
    ) -> Result<(u16, String)> {
        let fc = fetch_call_id();
        let mut req = self.client.post(format!("{}{}", secure, path));
        req = req
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("x-aos-model-page", page)
            .header("x-aos-stk", stk_val)
            .header("x-aos-ui-fetch-call-1", fc)
            .header("X-Requested-With", "Fetch")
            .header("syntax", "graviton")
            .header("modelVersion", "v2")
            .header("Accept", "*/*")
            .header("Origin", secure)
            .header("Referer", ref_url)
            .header("Sec-Fetch-Site", "same-origin")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty");
        if let Some(actk) = actk {
            req = req.header("x-as-actk", actk);
        }
        let resp = req.body(data.to_string()).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        Ok((status, text))
    }

    pub async fn checkout(
        &mut self,
        config: &AccountConfig,
        _settings: &GlobalSettings,
    ) -> Result<String> {
        if self.item_id.is_none() || self.mall_stk.is_none() {
            self.bag_snapshot(&config.part).await?;
        }
        let item_id = self.item_id.clone().ok_or_else(|| anyhow!("empty bag"))?;
        let mall_stk = self
            .mall_stk
            .clone()
            .ok_or_else(|| anyhow!("missing mall stk"))?;

        let item_field = format!("shoppingCart.items.{}", item_id);
        let city = urlencoding::encode(&config.city).to_string();
        let province = urlencoding::encode(&config.province).to_string();
        let district = urlencoding::encode(&config.district).to_string();
        let district_full =
            urlencoding::encode(&format!("{} {}", config.city, config.district)).to_string();
        let checkout_fetch_id = fetch_call_id();

        let checkout_body = format!(
            "{0}.itemQuantity.quantity={1}&{0}.delivery.lineDeliveryOptions.address.provinceCityDistrictTabs.city={2}&{0}.delivery.lineDeliveryOptions.address.provinceCityDistrictTabs.state={3}&{0}.delivery.lineDeliveryOptions.address.provinceCityDistrictTabs.provinceCityDistrict={4}&{0}.delivery.lineDeliveryOptions.address.provinceCityDistrictTabs.district={5}&{0}.delivery.lineDeliveryOptions.address.provinceCityDistrictTabs.countryCode=CN&shoppingCart.locationConsent.locationConsent=false&shoppingCart.summary.promoCode.promoCode=&shoppingCart.actions.fcscounter=&shoppingCart.actions.fcsdata=",
            item_field, config.quantity, city, province, district_full, district
        );
        let checkout_headers = vec![
            ("Accept", "*/*"),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("x-aos-model-page", "cart"),
            ("x-aos-stk", &mall_stk),
            ("x-aos-ui-fetch-call-1", &checkout_fetch_id),
            ("X-Requested-With", "Fetch"),
            ("syntax", "graviton"),
            ("modelVersion", "v2"),
            ("Origin", MALL),
            ("Sec-Fetch-Site", "same-origin"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Dest", "empty"),
            ("Referer", "https://www.apple.com.cn/shop/bag"),
        ];
        let (status, body) = self
            .post_form(
                &format!(
                    "{}/shop/bagx/checkout_now?_a=checkout&_m=shoppingCart.actions",
                    MALL
                ),
                &checkout_body,
                checkout_headers,
            )
            .await?;
        if status != StatusCode::OK {
            bail!(
                "checkout_now failed with {} {}",
                status,
                &body[..body.len().min(200)]
            );
        }
        let secure_url = serde_json::from_str::<Value>(&body)?
            .pointer("/head/data/url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("checkout_now did not return a secure host"))?;
        let secure = origin_of(&secure_url);

        // signIn + authx + checkout/start
        let (sign_status, _) = self.get(&secure_url).await?;
        if sign_status >= 400 {
            bail!("signIn failed with {}", sign_status);
        }
        let ssi = secure_url
            .split("ssi=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap_or("")
            .to_string();
        let pltn = decode_pltn(&ssi).ok_or_else(|| anyhow!("failed to decode pltn"))?;

        let sign_stk = self.mall_stk.clone().unwrap_or_default();
        let authx_url = format!("{}/shop/signIn/idms/authx?ssi={}&up=true", secure, ssi);
        let authx_fc = fetch_call_id();
        let authx_headers = vec![
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("x-aos-model-page", "signInPage"),
            ("x-aos-stk", &sign_stk),
            ("x-aos-ui-fetch-call-1", &authx_fc),
            ("X-Requested-With", "Fetch"),
            ("syntax", "graviton"),
            ("modelVersion", "v2"),
            ("Accept", "*/*"),
            ("Origin", &secure),
            ("Referer", &secure_url),
            ("Sec-Fetch-Site", "same-origin"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Dest", "empty"),
        ];
        let (authx_status, _) = self
            .post_form(&authx_url, "grantCode=", authx_headers)
            .await?;
        if authx_status >= 400 {
            bail!("authx failed with {}", authx_status);
        }

        let start_headers = vec![
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Origin", &secure),
            ("Referer", &secure_url),
            ("Upgrade-Insecure-Requests", "1"),
        ];
        let (start_status, _) = self
            .post_form(
                &format!("{}/shop/checkout/start", secure),
                &format!("pltn={}", pltn),
                start_headers,
            )
            .await?;
        if start_status >= 400 {
            bail!("checkout/start failed with {}", start_status);
        }

        let (checkout_page_status, checkout_body) =
            self.get(&format!("{}/shop/checkout", secure)).await?;
        if checkout_page_status != 200 {
            bail!("checkout page failed with {}", checkout_page_status);
        }
        let stk_re = Regex::new(r#""x-aos-stk"\s*:\s*"([^"]+)""#)?;
        let mut stk = stk_re
            .captures(&checkout_body)
            .map(|m| m[1].to_string())
            .unwrap_or_default();

        // A: selectFulfillmentLocation
        let (a_status, a_body) = self.post_checkout(
            &secure,
            "/shop/checkoutx/fulfillment?_a=selectFulfillmentLocationAction&_m=checkout.fulfillment.fulfillmentOptions",
            "checkout.fulfillment.fulfillmentOptions.selectFulfillmentLocation=RETAIL",
            "checkoutPage",
            &format!("{}/shop/checkout?_s=Fulfillment-init", secure),
            &stk,
            None,
        )
        .await?;
        if a_status >= 400 {
            bail!("selectFulfillmentLocation failed with {}", a_status);
        }
        if let Some(new_stk) = extract_stk(&a_body) {
            stk = new_stk;
        }

        // S: select store
        let sl = "checkout.fulfillment.pickupTab.pickup.storeLocator";
        let store_body = format!(
            "{sl}.showAllStores=false&{sl}.selectStore={store}&{sl}.searchInput={search}&{sl}.address.stateCitySelectorForCheckout.city={city}&{sl}.address.stateCitySelectorForCheckout.state={province}&{sl}.address.stateCitySelectorForCheckout.provinceCityDistrict={district_full}&{sl}.address.stateCitySelectorForCheckout.district={district}&{sl}.address.stateCitySelectorForCheckout.countryCode=CN",
            sl = sl,
            store = config.store_id,
            search = district_full,
            city = city,
            province = province,
            district_full = district_full,
            district = district
        );
        let (s_status, s_body) = self
            .post_checkout(
                &secure,
                &format!("/shop/checkoutx/fulfillment?_a=select&_m={}", sl),
                &store_body,
                "checkoutPage",
                &format!("{}/shop/checkout?_s=Fulfillment-init", secure),
                &stk,
                None,
            )
            .await?;
        if s_status >= 400 {
            bail!("select store failed with {}", s_status);
        }
        if let Some(new_stk) = extract_stk(&s_body) {
            stk = new_stk;
        }

        // Parse timeSlot
        let s_json: Value = serde_json::from_str(&s_body)?;
        let time_slot = find_key(&s_json, "timeSlot");
        let (day, slot, dts) = extract_time_slot(time_slot);

        // D: continueFromFulfillmentToPickupContact
        let ts_prefix = "checkout.fulfillment.pickupTab.pickup.timeSlot.dateTimeSlots";
        let mut d_body = format!(
            "{}&checkout.fulfillment.fulfillmentOptions.selectFulfillmentLocation=RETAIL",
            store_body
        );
        if let (Some(day), Some(slot), Some(dts)) = (day, slot, dts) {
            d_body.push_str(&format!(
                "&{0}.startTime={1}&{0}.endTime={2}&{0}.timeSlotType=&{0}.isRecommended=false&{0}.timeSlotId={3}&{0}.signKey={4}&{0}.timeZone={5}&{0}.date={6}&{0}.dayRadio={7}&{0}.timeSlotValue={8}&{0}.isRestricted=&{0}.displayStartTime=",
                ts_prefix,
                slot.get("checkInStart").and_then(Value::as_str).unwrap_or(""),
                slot.get("checkInEnd").and_then(Value::as_str).unwrap_or(""),
                slot.get("SlotId").and_then(Value::as_str).unwrap_or(""),
                slot.get("signKey").and_then(Value::as_str).unwrap_or(""),
                slot.get("timeZone").and_then(Value::as_str).unwrap_or("Asia/Shanghai"),
                dts.get("date").and_then(Value::as_str).unwrap_or(""),
                day,
                slot.get("timeSlotValue").and_then(Value::as_str).unwrap_or("")
            ));
        }
        let (d_status, d_body_resp) = self.post_checkout(
            &secure,
            "/shop/checkoutx/fulfillment?_a=continueFromFulfillmentToPickupContact&_m=checkout.fulfillment",
            &d_body,
            "checkoutPage",
            &format!("{}/shop/checkout?_s=Fulfillment", secure),
            &stk,
            None,
        )
        .await?;
        if d_status >= 400 {
            bail!(
                "continueFromFulfillmentToPickupContact failed with {}",
                d_status
            );
        }
        if let Some(new_stk) = extract_stk(&d_body_resp) {
            stk = new_stk;
        }

        // E: contact
        let c = "checkout.pickupContact.selfPickupContact.selfContact.address";
        let contact_body = format!(
            "{c}.lastName={last}&{c}.firstName={first}&{c}.emailAddress={email}&{c}.fullDaytimePhone={phone}&checkout.pickupContact.selfPickupContact.verificationModule.verificationToken=&checkout.pickupContact.selfPickupContact.nationalIdSelf.nationalIdSelf={id4}&checkout.pickupContact.eFapiaoSelector.selectFapiao=e_personal_fdf&checkout.pickupContact.eFapiaoSelector.ePersonalFapiao.invoiceHeader=",
            c = c,
            last = urlencoding::encode(&config.contact.last_name),
            first = urlencoding::encode(&config.contact.first_name),
            email = urlencoding::encode(&config.contact.email),
            phone = urlencoding::encode(&config.contact.phone),
            id4 = urlencoding::encode(&config.contact.national_id_suffix)
        );
        let (e_status, e_body) = self
            .post_checkout(
                &secure,
                "/shop/checkoutx?_a=continueFromPickupContactToBilling&_m=checkout.pickupContact",
                &contact_body,
                "checkoutPage",
                &format!("{}/shop/checkout?_s=PickupContact", secure),
                &stk,
                None,
            )
            .await?;
        if e_status >= 400 {
            bail!(
                "continueFromPickupContactToBilling failed with {}",
                e_status
            );
        }
        if let Some(new_stk) = extract_stk(&e_body) {
            stk = new_stk;
        }

        // F: billing
        let b = "checkout.billing.billingOptions";
        let billing_body = format!(
            "{b}.selectBillingOption=WECHAT&{b}.bankLookUp.selectBank=&checkout.locationConsent.locationConsent=false",
            b = b
        );
        let (f_status, f_body) = self
            .post_checkout(
                &secure,
                &format!(
                    "/shop/checkoutx/billing?_a=selectBillingOptionAction&_m={}",
                    b
                ),
                &billing_body,
                "checkoutPage",
                &format!("{}/shop/checkout?_s=Billing-init", secure),
                &stk,
                None,
            )
            .await?;
        if f_status >= 400 {
            bail!("selectBillingOptionAction failed with {}", f_status);
        }
        if let Some(new_stk) = extract_stk(&f_body) {
            stk = new_stk;
        }
        if let Some(actk) = extract_actk(&f_body) {
            self.last_actk = Some(actk);
        }

        // G: review
        let g_body_str = format!(
            "{b}.selectBillingOption=WECHAT&{b}.bankLookUp.selectBank=",
            b = b
        );
        let (g_status, g_body) = self
            .post_checkout(
                &secure,
                "/shop/checkoutx/billing?_a=continueFromBillingToReview&_m=checkout.billing",
                &g_body_str,
                "checkoutPage",
                &format!("{}/shop/checkout?_s=Billing", secure),
                &stk,
                self.last_actk.as_deref(),
            )
            .await?;
        if g_status >= 400 {
            bail!("continueFromBillingToReview failed with {}", g_status);
        }
        if let Some(new_stk) = extract_stk(&g_body) {
            stk = new_stk;
        }

        // H: place order
        let (h_status, h_body) = self.post_checkout(
            &secure,
            "/shop/checkoutx/review?_a=continueFromReviewToProcess&_m=checkout.review.placeOrder",
            "",
            "checkoutPage",
            &format!("{}/shop/checkout?_s=Review", secure),
            &stk,
            self.last_actk.as_deref(),
        )
        .await?;
        if h_status >= 400 {
            bail!("place order failed with {}", h_status);
        }
        if let Some(new_stk) = extract_stk(&h_body) {
            stk = new_stk;
        }

        // Poll status
        for _ in 0..15 {
            let _ = self.get(&format!("{}/shop/checkout/status", secure)).await;
            let _ = self
                .post_checkout(
                    &secure,
                    "/shop/checkoutx/statusX?_a=checkStatus&_m=spinner",
                    "",
                    "checkoutPage",
                    &format!("{}/shop/checkout", secure),
                    &stk,
                    self.last_actk.as_deref(),
                )
                .await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Payment QR
        for _ in 0..12 {
            let qr_result = self
                .post_checkout(
                    &secure,
                    "/shop/checkoutx/paymentQRCode?_a=checkstatus&_m=thankYou.paymentQRCodePoll",
                    "",
                    "checkoutThankYouPage",
                    &format!("{}/shop/checkout/thankyou", secure),
                    &stk,
                    self.last_actk.as_deref(),
                )
                .await;
            let (qr_status, qr_body) = match qr_result {
                Ok(v) => v,
                Err(_) => continue,
            };
            if qr_status == 200 {
                if let Ok(qr_json) = serde_json::from_str::<Value>(&qr_body) {
                    if let Some(link) = qr_json
                        .pointer("/body/thankYou/paymentQRCode/d/imageCodeLink")
                        .and_then(Value::as_str)
                    {
                        return Ok(link.to_string());
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bail!("payment QR code was not issued")
    }
}

fn extract_stk(body: &str) -> Option<String> {
    Regex::new(r#""x-aos-stk"\s*:\s*"([^"]+)""#)
        .ok()?
        .captures(body)
        .map(|m| m[1].to_string())
}

fn extract_actk(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.pointer("/body/meta/h/x-as-actk")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn find_key<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Object(map) = node {
        if let Some(v) = map.get(key) {
            return Some(v);
        }
        for v in map.values() {
            if let Some(found) = find_key(v, key) {
                return Some(found);
            }
        }
    } else if let Value::Array(arr) = node {
        for v in arr {
            if let Some(found) = find_key(v, key) {
                return Some(found);
            }
        }
    }
    None
}

fn extract_time_slot(ts: Option<&Value>) -> (Option<String>, Option<Value>, Option<Value>) {
    let ts = match ts {
        Some(ts) => ts,
        None => return (None, None, None),
    };
    let d = match ts.get("dateTimeSlots").and_then(|v| v.get("d")) {
        Some(d) => d,
        None => return (None, None, None),
    };
    let windows = match d.get("timeSlotWindows").and_then(Value::as_array) {
        Some(w) => w,
        None => return (None, None, None),
    };
    for window in windows {
        if let Value::Object(map) = window {
            for (day, slots) in map {
                if let Some(slot) = slots.as_array().and_then(|a| a.first()) {
                    return (Some(day.clone()), Some(slot.clone()), Some(d.clone()));
                }
            }
        }
    }
    (None, None, None)
}

fn decode_pltn(ssi: &str) -> Option<String> {
    use base64::Engine;
    let candidates = [
        ssi,
        ssi.split("AAAA").last().unwrap_or(ssi),
        ssi.split("BGAAAA").last().unwrap_or(ssi),
    ];
    for candidate in candidates {
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(candidate.as_bytes())
            .ok();
        let Some(decoded) = decoded else {
            continue;
        };
        let text = String::from_utf8_lossy(&decoded).to_string();
        if let Some(start) = text.find("pltn=") {
            let start = start + 5;
            let end = text[start..]
                .find("||;")
                .map(|offset| start + offset)
                .unwrap_or(text.len());
            return Some(text[start..end].to_string());
        }
    }
    None
}

fn solve_v1(result: u64, parts: u32, low: u64, high: u64) -> Option<Vec<u64>> {
    let divs: Vec<u64> = (low..=high).filter(|d| result % d == 0).collect();
    fn rec(
        result: u64,
        divs: &[u64],
        idx: usize,
        depth: u32,
        prod: u64,
        out: &mut Vec<u64>,
    ) -> bool {
        if depth == 0 {
            return prod == result;
        }
        let mut i = idx;
        while i < divs.len() {
            let d = divs[i];
            if prod > result / d {
                break;
            }
            if result % (prod * d) == 0 || depth == 1 {
                out.push(d);
                if rec(result, divs, i, depth - 1, prod * d, out) {
                    return true;
                }
                out.pop();
            }
            i += 1;
        }
        false
    }
    let mut out = Vec::new();
    rec(result, &divs, 0, parts, 1, &mut out);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn solve_sha256(salt: &str, target_hex: &str) -> Option<u64> {
    for i in 0..1_000_000u64 {
        let mut hasher = Sha256::new();
        hasher.update(salt.as_bytes());
        hasher.update(i.to_string().as_bytes());
        let digest = hasher.finalize();
        if hex::encode(digest) == target_hex {
            return Some(i);
        }
    }
    None
}
