# Apple Fast Checkout

Rust 后端 + Vue 3 前端的自动下单控制台。

## 功能

- 多账号并发轮询
- 每个账号独立代理（支持代理池）
- 全局延迟、启动延迟、随机抖动
- 每账号可单独覆盖轮询延迟
- 常驻 reqwest::Client，复用 TLS / DNS / Cookie / 连接池
- 支持导入 Apple ID 长期登录 Cookie
- 自动预热 dssid2、shld_bt_ck、as_atb
- 自动加购并缓存购物袋 item id 和商城域 stk
- 库存命中后自动进入结账链
- 结账链包含 authx、checkout/start、门店选择、取货时段、取货人、支付方式、下单和支付链接提取
- 541 时自动使用 Python 兜底通道查询库存
- 实时状态、延迟、错误、订单数、支付链接和日志面板
- 中央监控范围配置：型号 / 门店 / 并发 / 重试 / 熔断
- 手动下单、微信小程序演练 / 真实下单、批量准备会话
- 批量导入账号与 Cookie（支持新旧两种格式）
- Apple ID 验证码登录（/api/login/start + /api/login/verify）

## 前端

前端为 Vue 3 + Element Plus 单页应用（pure-admin 基础上定制），构建产物放在：

- `static/web/`（生产构建，`/static/web/` 直接访问）

打开 `http://127.0.0.1:8080` 会自动跳转到 `/static/web/`。

### 页面

- 系统总览：`/#/overview`
- 监控与下单：`/#/monitor`
- 账号管理：`/#/accounts`
- 账号配置：`/#/config`
- 批量导入：`/#/batch`
- 库存查询：`/#/query`
- 运行日志：`/#/logs`

导航栏齿轮按钮打开「全局设置」抽屉，包含：启动中央监控 / 停止监控 / 清空已完成状态，以及全局轮询延迟、启动延迟、随机抖动、HTTP 超时、成功后停止等设置。

## 启动

```powershell
cargo run --release
```

打开：<http://127.0.0.1:8080>

## 架构

- `src/apple.rs`：Apple 网络客户端、会话预热、库存轮询、结账链
- `src/app.rs`：多账号调度、API、日志和状态管理
- `src/models.rs`：配置和数据模型
- `src/main.rs`：路由（`/` 重定向到 `/static/web/`，`/api/*` 接口，`/static` 静态资源）
- `static/web/`：Vue 前端构建产物
- `apple_stock_cli.py`：541 时的 Python 库存兜底通道

前端源码维护在独立仓库（Vue 3 + TypeScript + Vite），构建后替换 `static/web/` 即可。

## 安全

- 服务只监听 127.0.0.1
- 密码保存在本地 config.bin（加密）
- 日志不输出密码
- 代理支持 http://、https:// 和 socks5://
