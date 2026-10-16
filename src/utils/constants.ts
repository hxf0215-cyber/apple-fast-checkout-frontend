/**
 * 全局常量：集中管理轮询间隔、超时等硬编码值
 */

/** WebSocket 断开时的 HTTP 轮询间隔（毫秒） */
export const FALLBACK_POLL_INTERVAL_MS = 5000;

/** WebSocket 重连基础延迟（毫秒），指数退避 */
export const WS_RECONNECT_BASE_MS = 3000;

/** WebSocket 重连最大延迟（毫秒） */
export const WS_RECONNECT_MAX_MS = 30000;

/** Axios/fetch 请求默认超时（毫秒） */
export const REQUEST_TIMEOUT_MS = 15000;

/** 登录验证码接口超时（毫秒） */
export const LOGIN_CODE_TIMEOUT_MS = 95000;
