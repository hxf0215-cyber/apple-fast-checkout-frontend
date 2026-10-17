# 代码优化记录（14 项）

本文件记录本轮代码审查后实施的优化项与后续建议。

## 已实施

| # | 项目 | 实施内容 |
|---|------|---------|
| 1 | WebSocket 重连 | 由「10 次后放弃」改为无限重连 + 指数退避（3s→6s→12s→24s→30s 封顶） |
| 2 | 文件原子写 | 新增 `atomic_write()`：先写 .tmp 再 rename，避免并发写坏 config.bin / orders.json / sessions.json |
| 3 | 前端轮询 | 由「5 秒空转」改为「WebSocket 连接时停止，断线才启用」（4 个页面） |
| 4 | 请求去重 | 批量预热 / 批量准备 / 启动监控 / 停止监控 加运行中标志，防连点 |
| 5 | stop_after_success | 确认可从设置读取（无需改代码） |
| 6 | 日志磁盘上限 | 单日日志文件上限 20MB，超出不再写入 |
| 7 | 账号删除清理 | 删除账号时同步清理 account_locks / attempts / retry / proxy_health / sessions |
| 8 | 选中状态同步 | accounts 页 load() 中已自动清理失效选中项 |
| 9 | 死代码清理 | 删除 views/home、views/welcome 空壳文件 |
| 12 | 单元测试 | wechat.rs 新增 6 个测试（Cookie 解析 / 参数编码 / 条款体） |
| 13 | 日志封装 | 后端已统一 self.log()；前端保留必要 console.error |
| 14 | 常量提取 | 新增 src/utils/constants.ts，集中管理轮询/重连/超时 |

## 后续建议（未实施）

### 10. 后端文件拆分（app.rs 5100 行）
建议拆分为：
- `server.rs` — HTTP 路由与 handler
- `monitor.rs` — 中央监控与派单
- `accounts.rs` — 账号 CRUD 与状态同步
- `orders.rs` — 订单记录与持久化
- `sessions.rs` — 微信会话管理与续期

### 11. 前端组件拆分（config/index.vue 488 行、monitor/index.vue 872 行）
建议拆分为：
- `ConfigForm.vue` — 账号表单
- `AppleLoginCard.vue` — Apple ID 验证码登录
- `MonitorSettings.vue` — 中央监控范围配置
- `WechatOrderPanel.vue` — 微信小程序下单面板
- `ManualOrderPanel.vue` — 手动下单面板

### 其他
- 补充集成测试（HTTP API 层）
- 考虑把内存中的 `prepared_wechat_orders` 改为带 TTL 的缓存结构
