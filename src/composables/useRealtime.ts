/**
 * WebSocket 实时状态推送 — 替代 HTTP 轮询
 */
import { ref, readonly } from "vue";

export const wsState = ref<any>(null);
export const wsConnected = ref(false);

let ws: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempts = 0;

function getWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return proto + "//" + location.host + "/ws";
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
        const data = JSON.parse(event.data);
        if (data && data.accounts !== undefined) {
          wsState.value = data;
        }
      } catch {}
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
      if (ws) ws.close();
    };
  } catch {
    wsConnected.value = false;
  }
}

function disconnect() {
  if (reconnectTimer) clearTimeout(reconnectTimer);
  if (ws) { ws.close(); ws = null; }
  wsConnected.value = false;
}

export function useRealtime() {
  return {
    connect,
    disconnect,
    state: readonly(wsState),
    connected: readonly(wsConnected),
  };
}

export function initRealtime() {
  connect();
}
