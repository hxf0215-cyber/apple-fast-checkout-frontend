# Apple Fast Checkout Web 前端源码（纯净版）

Vue 3 + Vite + TypeScript + Element Plus + auth-admin 基座。  
本目录是可直接交给他人继续开发、构建的纯源码副本，不包含 `node_modules/`、`dist/`、日志、Cookie、账号数据或密钥。

## 一、目录说明

```
.
├─ src/                  # 业务源码
│  ├─ api/client.ts      # 全部后端 API 封装
│  ├─ router/modules/    # 业务路由
│  ├─ utils/apple.ts     # 状态 / Cookie / 时间等工具函数
│  └─ views/             # 业务页面
├─ public/               # 静态配置与图标
├─ build/                # Vite 构建配置
├─ types/                # TS 类型声明
├─ package.json
├─ pnpm-lock.yaml
└─ vite.config.ts
```

## 二、安装依赖

必须使用 `pnpm`，并且必须带 `--frozen-lockfile`：

```powershell
pnpm.cmd install --frozen-lockfile
```

Windows 若提示脚本执行策略问题，请使用 `pnpm.cmd`。

## 三、开发模式

```powershell
pnpm.cmd dev
```

默认地址：`http://127.0.0.1:8848`

开发环境已配置代理：

- `/api` → `http://127.0.0.1:8080`
- `/static` → `http://127.0.0.1:8080`

因此开发模式下需要同时启动 Rust 后端。

## 四、生产构建

```powershell
pnpm.cmd build
```

构建产物输出到 `dist/`。

生产 `VITE_PUBLIC_PATH` 为 `/static/web/`，默认匹配 Rust 后端的静态目录结构：

```
/static/web/index.html
/static/web/static/js/...
/static/web/static/css/...
```

如果部署到其他路径，请修改 `.env.production` 中的 `VITE_PUBLIC_PATH`。

## 五、页面

| 路径 | 页面 |
|---|---|
| `/#/overview` | 系统总览 |
| `/#/monitor` | 监控与下单 |
| `/#/accounts` | 账号管理 |
| `/#/config` | 账号配置 |
| `/#/batch` | 批量导入 |
| `/#/query` | 库存查询 |
| `/#/logs` | 运行日志 |

路由使用 hash 模式，刷新后页面状态不会丢。

## 六、修改建议

1. 修改页面：
   - 优先改 `src/views/<page>/index.vue`
2. 新增接口：
   - 先在 `src/api/client.ts` 增加类型和方法
   - 页面内只调用 `api.xxx()`，不要散落 `fetch`
3. 修改菜单：
   - `src/router/modules/main.ts`
   - `src/layout/components/lay-sidebar/NavVertical.vue`
4. 修改代理：
   - `vite.config.ts`
5. 修改部署路径：
   - `.env.production`

## 七、注意

- `src/views/home/welcome/permission/error` 是 auth-admin 基座模板页面，虽然当前菜单未使用，但路由自动导入依赖它们，请勿直接删除。
- 登录页为本地控制台入口，不调用后端鉴权接口。
- 本项目只包含前端源码，不含后端。
