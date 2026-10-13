<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { api, type ApiLog } from "@/api/client";

defineOptions({ name: "Logs" });

const logs = ref<ApiLog[]>([]);
const loading = ref(false);
const expanded = new Set<string>();
const keyword = ref("");

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

function logKey(log: ApiLog, index: number) {
  return `${log.ts_ms}-${index}`;
}

function isExpanded(log: ApiLog, index: number) {
  return expanded.has(logKey(log, index));
}

function toggleExpand(log: ApiLog, index: number) {
  const key = logKey(log, index);
  if (expanded.has(key)) expanded.delete(key);
  else expanded.add(key);
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

onMounted(async () => {
  await load();
  pollTimer = setInterval(load, 1500);
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
});
</script>

<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card app-card-fill">
      <template #header>
        <div class="card-header">
          <span>运行日志</span>
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
          v-for="(log, index) in logs
            .slice(-500)
            .reverse()
            .filter(
              item =>
                !keyword ||
                item.message.toLowerCase().includes(keyword.toLowerCase())
            )"
          :key="log.ts_ms + '-' + index"
          :class="['log-entry', levelClass(log)]"
          @click="toggleExpand(log, index)"
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
                  collapsed: !isExpanded(log, index) && log.message.length > 120
                }
              ]"
            >
              {{ log.message }}
            </span>
            <span v-if="log.message.length > 120" class="log-toggle">
              {{ isExpanded(log, index) ? "收起" : "展开" }}
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
