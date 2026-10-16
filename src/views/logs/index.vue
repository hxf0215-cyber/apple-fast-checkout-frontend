<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { api, type ApiLog } from "@/api/client";
import { useRealtime } from "@/composables/useRealtime";

defineOptions({ name: "Logs" });

const logs = ref<ApiLog[]>([]);
const loading = ref(false);
const keyword = ref("");

// 当前后端版本：显示在标题旁，便于确认部署的是哪一版
const serverVersion = ref<string>("");
const serverBuild = ref<string>("");
const expanded = ref<Set<string>>(new Set());

// 单页最多渲染的日志条数：日志量大时 DOM 节点是主要瓶颈
const RENDER_LIMIT = 300;

/** 倒序 + 过滤 + 截断，用 computed 缓存，避免每次渲染都重建数组（原是模板内链式调用） */
const rendered = computed(() => {
  const kw = keyword.value.toLowerCase();
  const source = logs.value;
  const out: ApiLog[] = [];
  // 从最新往旧遍历，命中即收集，达到上限就停（避免处理全部 500 条）
  for (let i = source.length - 1; i >= 0 && out.length < RENDER_LIMIT; i--) {
    const item = source[i];
    if (!kw || item.message.toLowerCase().includes(kw)) out.push(item);
  }
  return out;
});

/** 裁剪 expanded 集合：日志轮转后旧 key 会无限累积（内存泄漏） */
watch(
  rendered,
  list => {
    if (expanded.value.size <= RENDER_LIMIT * 2) return;
    const alive = new Set(list.map(logKey));
    const next = new Set<string>();
    for (const key of expanded.value) if (alive.has(key)) next.add(key);
    expanded.value = next;
  },
  { flush: "post" }
);

async function load() {
  loading.value = true;
  try {
    logs.value = (await api.logs()) || [];
  } finally {
    loading.value = false;
  }
}

async function clearLogs() {
  await ElMessageBox.confirm("确认清空所有运行日志？", "确认操作", {
    type: "warning"
  });
  const result = await api.clearLogs();
  ElMessage.success(`运行日志已清空，处理 ${result.removed_files ?? 0} 个文件`);
  await load();
}

function logKey(log: ApiLog) {
  // 仅依赖日志自身内容，保证新日志插入 / 过滤 / 排序后展开状态不丢失
  return `${log.ts_ms}-${log.account_id ?? ""}-${log.message}`;
}

function isExpanded(log: ApiLog) {
  return expanded.value.has(logKey(log));
}

function toggleExpand(log: ApiLog) {
  const key = logKey(log);
  if (expanded.value.has(key)) expanded.value.delete(key);
  else expanded.value.add(key);
  // Set 是同一个引用，需手动触发响应式更新
  expanded.value = new Set(expanded.value);
}
function levelClass(log: ApiLog) {
  if (log.level === "error") return "log-error";
  if (log.level === "warn") return "log-warn";
  if (log.level === "success") return "log-success";
  if (
    log.message.includes("成功") ||
    log.message.includes("下单成功") ||
    log.message.includes("有货") ||
    log.message.includes("提交成功")
  )
    return "log-success";
  return "log-info";
}

function timeShort(ms: number) {
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

let pollTimer: ReturnType<typeof setInterval> | null = null;

const { state: wsState, logs: wsLogs, connected: wsConnected } = useRealtime();

watch(wsLogs, newLogs => {
  if (newLogs && wsConnected.value) {
    logs.value = [...newLogs].reverse();
    loading.value = false;
  }
}, { immediate: true });

watch(wsState, () => {
  // State updates come via wsLogs above; no action needed here
}, { immediate: true });

onMounted(async () => {
  // 拉取版本信息（失败不影响日志功能）
  try {
    const v = await api.version();
    serverVersion.value = v?.version || "";
    serverBuild.value = v?.build || "";
  } catch (error) {
    console.warn("fetch version failed:", error);
  }
  await load();
  // 轮询仅在 WebSocket 断开时启用（见文件底部 watch(wsConnected)）
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
});

// 连接正常时停止轮询，仅在断线时启用（减少 CPU 与请求）
watch(
  wsConnected,
  connected => {
    if (connected) {
      if (pollTimer) {
        clearInterval(pollTimer);
        pollTimer = null;
      }
    } else if (!pollTimer) {
      pollTimer = setInterval(() => {
        if (document.visibilityState === "visible") load();
      }, 5000);
    }
  },
  { immediate: true }
);
</script>

<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card app-card-fill">
      <template #header>
        <div class="card-header">
          <div class="title-group">
            <span>运行日志</span>
            <el-tag
              v-if="serverVersion"
              type="info"
              size="small"
              effect="plain"
            >
              v{{ serverVersion }} · {{ serverBuild }}
            </el-tag>
          </div>
          <div class="header-tools">
            <el-input
              v-model="keyword"
              clearable
              placeholder="搜索日志内容"
              style="width: 240px"
            />
            <el-button type="danger" @click="clearLogs">清空运行日志</el-button>
          </div>
        </div>
      </template>
      <div v-loading="loading" class="log-container">
        <div
          v-for="log in rendered"
          :key="logKey(log)"
          :class="['log-entry', levelClass(log)]"
          @click.stop="toggleExpand(log)"
        >
          <div class="log-line">
            <span class="log-time">{{ timeShort(log.ts_ms) }}</span>
            <span class="log-account">{{
              log.username || log.account_id || "系统"
            }}</span>
            <span :class="['log-level', `level-${log.level}`]">{{
              log.level
            }}</span>
            <span
              :class="[
                'log-message',
                {
                  collapsed: !isExpanded(log) && log.message.length > 120
                }
              ]"
            >
              {{ log.message }}
            </span>
            <span v-if="log.message.length > 120" class="log-toggle">
              {{ isExpanded(log) ? "收起" : "展开" }}
            </span>
          </div>
        </div>
        <div v-if="!logs.length" class="log-empty">暂无日志</div>
      </div>
    </el-card>
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.card-header {
.title-group {
  display: flex;
  align-items: center;
  gap: 10px;
}
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.header-tools {
  display: flex;
  gap: 8px;
}
.log-container {
  height: 100%;
  overflow-y: auto;
  font-size: 12px;
  line-height: 1.5;
  font-family: Consolas, "Courier New", monospace;
}
.log-entry {
  padding: 4px 8px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  cursor: pointer;
  display: flex;
  align-items: flex-start;
  gap: 8px;
  transition: background 0.15s;
}
.log-entry:hover {
  background: rgba(0, 0, 0, 0.02);
}
.log-error {
  background: rgba(245, 108, 108, 0.06);
}
.log-error .log-message {
  color: #f56c6c;
}
.log-warn {
  background: rgba(230, 162, 60, 0.06);
}
.log-warn .log-message {
  color: #e6a23c;
}
.log-success {
  background: rgba(103, 194, 58, 0.06);
}
.log-success .log-message {
  color: #67c23a;
}
.log-info .log-message {
  color: var(--el-text-color-primary);
}
.log-line {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  width: 100%;
  min-height: 22px;
}
.log-time {
  color: var(--el-text-color-placeholder);
  white-space: nowrap;
  width: 78px;
  min-width: 78px;
  flex-shrink: 0;
}
.log-account {
  color: var(--el-text-color-secondary);
  white-space: nowrap;
  width: 160px;
  min-width: 160px;
  flex-shrink: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}
.log-level {
  white-space: nowrap;
  width: 64px;
  min-width: 64px;
  flex-shrink: 0;
  font-size: 11px;
  padding: 2px 0;
  border-radius: 3px;
  text-align: center;
  display: inline-flex;
  justify-content: center;
  align-items: center;
  height: 20px;
  line-height: 1;
}
.level-info {
  background: rgba(144, 147, 153, 0.12);
  color: #909399;
}
.level-success,
.level-success {
  background: rgba(103, 194, 58, 0.12);
  color: #67c23a;
}
.level-error {
  background: rgba(245, 108, 108, 0.12);
  color: #f56c6c;
}
.level-warn {
  background: rgba(230, 162, 60, 0.12);
  color: #e6a23c;
}
.log-message {
  flex: 1;
  word-break: break-all;
  word-wrap: break-word;
}
.log-message.collapsed {
  overflow: hidden;
  text-overflow: ellipsis;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
}
.log-toggle {
  color: var(--el-color-primary);
  white-space: nowrap;
  font-size: 11px;
  flex-shrink: 0;
}
.log-empty {
  padding: 40px;
  text-align: center;
  color: var(--el-text-color-secondary);
}
</style>

