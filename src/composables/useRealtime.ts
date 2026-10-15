/**
 * WebSocket 实时推送 — 状态 + 日志
 * 后端每 2 秒推送（只在数据变化时才发送）
 * 消息格式：{"type": "state"|"logs", "data": {...}}
 */
import { ref, readonly } from "vue";

export const wsState = ref<any>(null);
export const wsLogs = ref<any[]>([]);
export const wsConnected = ref(false);

let ws: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempts = 0;

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
      if (reconnectAttempts < 10) {
        reconnectAttempts++;
        reconnectTimer = setTimeout(connect, 3000);
      }
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
