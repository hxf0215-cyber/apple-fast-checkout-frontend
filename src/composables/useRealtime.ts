/**
 * WebSocket 实时推送 — 状态 + 日志
 * 后端每 2 秒推送（只在数据变化时才发送）
 * 消息格式：{"type": "state"|"logs", "data": {...}}
 */
import { ref, readonly } from "vue";
import { WS_RECONNECT_BASE_MS, WS_RECONNECT_MAX_MS } from "@/utils/constants";

export const wsState = ref<any>(null);
export const wsLogs = ref<any[]>([]);
export const wsConnected = ref(false);

let ws: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempts = 0;
const MAX_BACKOFF_MS = WS_RECONNECT_MAX_MS;

// 心跳间隔
const HEARTBEAT_MS = 30_000;
let heartbeatTimer: ReturnType<typeof setInterval> | null = null;

/** 指数退避 + ±25% 抖动：避免服务端重启时所有客户端同时重连（惊群） */
function backoffDelay(attempt: number): number {
  const base = Math.min(WS_RECONNECT_BASE_MS * Math.pow(2, attempt - 1), MAX_BACKOFF_MS);
  const jitter = base * 0.25 * (Math.random() * 2 - 1);
  return Math.max(500, Math.round(base + jitter));
}

function getWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/ws`;
}

function connect() {
  if (ws && ws.readyState === WebSocket.OPEN) return;

  try {
    ws = new WebSocket(getWsUrl());

    ws.onopen = () => {
      wsConnected.value = true;
      reconnectAttempts = 0;
      // 心跳保活：定期 ping，防止空闲连接被中间设备回收
      if (heartbeatTimer) clearInterval(heartbeatTimer);
      heartbeatTimer = setInterval(() => {
        if (ws && ws.readyState === WebSocket.OPEN) {
          try {
            ws.send("ping");
          } catch {
            /* 已断开，交给 onclose 处理 */
          }
        }
      }, HEARTBEAT_MS);
    };

    ws.onmessage = (event: MessageEvent) => {
      try {
        const msg = JSON.parse(event.data);
        if (msg && msg.type === "state" && msg.data) {
          wsState.value = msg.data;
        } else if (msg && msg.type === "logs" && Array.isArray(msg.data)) {
          wsLogs.value = msg.data;
        }
      } catch {
        // 忽略非 JSON 消息
      }
    };

    ws.onclose = () => {
      wsConnected.value = false;
      ws = null;
      if (heartbeatTimer) { clearInterval(heartbeatTimer); heartbeatTimer = null; }
      // 无限重连 + 指数退避（3s → 6s → 12s → 24s → 30s 封顶）
      reconnectAttempts++;
      const delay = backoffDelay(reconnectAttempts);
      reconnectTimer = setTimeout(connect, delay);
    };

    ws.onerror = () => {
      ws?.close();
    };
  } catch {
    wsConnected.value = false;
  }
}

function disconnect() {
  if (reconnectTimer) clearTimeout(reconnectTimer);
  if (heartbeatTimer) { clearInterval(heartbeatTimer); heartbeatTimer = null; }
  if (ws) {
    ws.close();
    ws = null;
  }
  wsConnected.value = false;
}

export function useRealtime() {
  return {
    connect,
    disconnect,
    state: readonly(wsState),
    logs: readonly(wsLogs),
    connected: readonly(wsConnected),
  };
}

export function initRealtime() {
  connect();
}
