# Apple Fast Checkout

Rust 后端 + Vue 3 前端的自动下单控制台。**前后端源码都在本仓库，可独立编译。**

```
.
├─ src/                # 前端源码（Vue 3 + TypeScript + Vite）
├─ backend/            # 后端源码（Rust + Axum）
│  ├─ src/             # Rust 源码
│  ├─ static/web/      # 前端预构建产物（已随仓库提供，可直接运行）
│  └─ *.py             # Apple ID 登录桥（可选功能）
├─ package.json
└─ pnpm-lock.yaml
```

---

## 一、快速开始（只跑，不编译）

后端 `backend/static/web/` 里已包含构建好的前端，**开箱即用**：

```powershell
cd backend
cargo build --release      # 首次编译约 3-5 分钟
.\start.bat                 # 启动并自动打开浏览器
```

打开 <http://127.0.0.1:8080>，会自动跳转到 `/static/web/`。

停止服务：`backend\stop.bat`

---

## 二、完整编译（前后端都自己构建）

### 环境要求

| 依赖 | 版本 | 说明 |
|---|---|---|
| Rust | 1.75+ | <https://rustup.rs> |
| Node.js | 18+ | <https://nodejs.org> |
| pnpm | 8+ | `npm i -g pnpm` |
| Python | 3.9+ | **可选**，仅「Apple ID 验证码登录」功能需要 |

### 1. 编译前端

在仓库根目录：

```powershell
pnpm install --frozen-lockfile
pnpm build
```

产物在 `dist/`，**需要复制到后端静态目录**：

```powershell
Remove-Item backend/static/web/* -Recurse -Force
Copy-Item dist/* backend/static/web/ -Recurse -Force
```

> 后端通过 `ServeDir::new("static")` 提供静态资源，前端固定挂在 `/static/web/`。
> 如需改路径，改 `.env.production` 里的 `VITE_PUBLIC_PATH`，同时改 `src/main.rs` 的路由。

### 2. 编译后端

```powershell
cd backend
cargo build --release
```

产物：`backend/target/release/apple-fast-checkout.exe`

### 3. （可选）Python 登录桥

仅当需要用「Apple ID + 验证码」登录时才需要：

```powershell
pip install -r backend/requirements.txt
```

不需要这个功能的话可以跳过，其他功能完全正常。

---

## 三、前后端联调（开发模式）

**终端 1** — 启动后端：

```powershell
cd backend
cargo run --release
```

**终端 2** — 启动前端开发服务器：

```powershell
pnpm dev
```

打开 <http://127.0.0.1:8848>

Vite 已配置代理，开发模式下 `/api` 和 `/static` 都会转发到 `http://127.0.0.1:8080`（见 `vite.config.ts`）。

---

## 四、页面

| 路径 | 页面 |
|---|---|
| `/#/overview` | 系统总览 |
| `/#/monitor` | 监控与下单 |
| `/#/accounts` | 账号管理 |
| `/#/config` | 账号配置 |
| `/#/batch` | 批量导入 |
| `/#/query` | 库存查询 |
| `/#/logs` | 运行日志 |

路由为 hash 模式，刷新不丢状态。

---

## 五、功能

- 多账号并发轮询，每账号可独立配置型号 / 门店 / 延迟
- 中央监控：跨账号共享库存查询，命中后按优先级派发下单
- **批量预热**：有货时提前进入结账页，下单时只需 3-5 秒（直连或代理）
- **批量准备会话**：预先完成加购、联系人、支付方式
- 动态代理池：自动提取、按需测活、用完即弃、541 时自动换 IP
- 静态代理：每个账号固定出口 IP，支持 `IP|端口|账号|密码` 格式
- 实时推送：WebSocket 推送状态与日志（只在数据变化时发送）
- 完整下单链：加购 → 结账 → 选店 → 取货时段 → 联系人 → 支付 → 下单
- 订单历史、日志高亮与检索、批量导入账号 / Cookie

---

## 六、架构

### 后端（Rust + Axum）

| 文件 | 职责 |
|---|---|
| `backend/src/main.rs` | 路由注册、WebSocket、静态资源服务 |
| `backend/src/app.rs` | 多账号调度、API 处理、状态管理、日志 |
| `backend/src/apple.rs` | Apple 网络客户端、会话预热、库存轮询 |
| `backend/src/wechat.rs` | 微信小程序下单流程（加购 / 结账 / 下单） |
| `backend/src/proxy_pool.rs` | 代理池：提取、测活、分配、淘汰 |
| `backend/src/models.rs` | 配置与数据模型 |

监听地址：`0.0.0.0:8080`

### 前端（Vue 3 + Vite + TypeScript + Element Plus）

| 目录 | 说明 |
|---|---|
| `src/api/client.ts` | 所有后端 API 封装 |
| `src/views/` | 业务页面 |
| `src/composables/useRealtime.ts` | WebSocket 实时推送（含重连退避 + 心跳） |
| `src/router/modules/main.ts` | 路由与菜单 |
| `build/` | Vite 构建配置 |

---

## 七、配置

首次启动会自动生成加密配置 `backend/config.bin`，并在前端页面里配置账号。

参考格式见 `backend/config.example.json`。

### 代理格式

静态代理支持多种写法：

```
1.2.3.4:8080                      # 无认证
1.2.3.4|8080|账号|密码            # 竖线分隔（推荐）
1.2.3.4:8080:账号:密码            # 冒号分隔
账号:密码@1.2.3.4:8080            # 带认证
socks5://1.2.3.4:1080
```

账号密码中的特殊字符会自动做百分号编码。

---

## 八、注意事项

### 数据安全

以下文件**含真实凭据**，已在 `.gitignore` 中排除，请勿手动提交：

```
backend/config.bin        # 加密后的账号配置
backend/config.key        # 加密密钥
backend/sessions.json     # 预准备会话
backend/orders.json       # 订单历史
backend/cookies/          # Cookie 文件
```

### 关于 Apple 接口

Apple 的库存查询接口（`product-locator/quotes`）在**预购开始前**会返回一个 HTML 等待页而不是 JSON（页面上写着"精彩这就到""请不要走开"等文案）。这属于 Apple 的正常行为，等接口切回 JSON 后自动恢复，不需要改代码。

### 仅本机访问

默认监听 `0.0.0.0:8080`（局域网可访问）。如需限制为仅本机，把 `backend/src/main.rs` 里的绑定地址改成 `127.0.0.1:8080`。

---

## 九、许可

仅供学习研究使用。
