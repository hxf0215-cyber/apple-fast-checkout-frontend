export function modelLabel(model: {
  family: string;
  capacity: string;
  color: string;
}) {
  return `${model.family} · ${model.capacity} · ${model.color}`;
}

export function statusText(status?: string | null) {
  const map: Record<string, string> = {
    stopped: "已停止",
    "central-monitor": "中央监控",
    starting: "启动中",
    polling: "监控中",
    ordering: "下单中",
    error: "异常",
    "order placed": "已下单"
  };
  return map[status || ""] || status || "-";
}

export function stockStatusText(status?: string | null) {
  const map: Record<string, string> = {
    available: "可取货",
    unavailable: "不可取货",
    ineligible: "暂不支持"
  };
  return map[status || ""] || status || "-";
}

export function cookieStatusText(status?: string | null) {
  const map: Record<string, string> = {
    valid: "正常",
    stale: "可能过期",
    invalid: "已失效",
    rate_limited: "限流中"
  };
  return map[status || ""] || "缺失";
}

export function formatDuration(ms?: number | null) {
  const value = Number(ms || 0);
  if (value <= 0) return "0 分钟";
  const minutes = Math.round(value / 60000);
  if (minutes < 60) return `${minutes} 分钟`;
  const hours = Math.floor(minutes / 60);
  return `${hours} 小时 ${minutes % 60} 分钟`;
}

export function formatTime(value?: number | null) {
  return value ? new Date(value).toLocaleString() : "-";
}

export function parseCookieText(text: string) {
  return Object.fromEntries(
    String(text || "")
      .split(";")
      .map(cookie => cookie.trim())
      .filter(Boolean)
      .filter(cookie => cookie.includes("="))
      .map(cookie => {
        const index = cookie.indexOf("=");
        return [
          cookie.slice(0, index).trim(),
          cookie.slice(index + 1).trim()
        ] as const;
      })
  );
}
