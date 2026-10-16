<script setup lang="ts">
import { computed, ref } from "vue";
import { ElMessage } from "element-plus";
import { api, type ProxyTestResult } from "@/api/client";

defineOptions({ name: "ProxyTest" });

const input = ref("");
const loading = ref(false);
const concurrency = ref(8);
const timeoutMs = ref(10000);
const results = ref<ProxyTestResult[]>([]);
const okProxies = ref<string[]>([]);
const summary = ref({ total: 0, available: 0, unavailable: 0 });
const keyword = ref("");
const onlyFailed = ref(false);

const proxyList = computed(() =>
  input.value
    .split(/\r?\n/)
    .map(line => line.split("#")[0].trim())
    .filter(line => line.length > 0)
);

const visible = computed(() =>
  results.value.filter(r => {
    if (onlyFailed.value && r.ok) return false;
    if (!keyword.value) return true;
    return r.proxy.toLowerCase().includes(keyword.value.toLowerCase());
  })
);

async function run() {
  const list = proxyList.value;
  if (!list.length) {
    ElMessage.warning("请先粘贴代理列表（每行一个）");
    return;
  }
  loading.value = true;
  try {
    const res = await api.proxyTest(list, concurrency.value, timeoutMs.value);
    if (!res.ok) {
      ElMessage.error(res.error || "测试失败");
      return;
    }
    results.value = res.results || [];
    okProxies.value = res.ok_proxies || [];
    summary.value = {
      total: res.total || 0,
      available: res.available || 0,
      unavailable: res.unavailable || 0
    };
    ElMessage.success(`测试完成：可用 ${res.available} / ${res.total}`);
  } catch (e: any) {
    ElMessage.error(e?.message || "测试请求失败");
  } finally {
    loading.value = false;
  }
}

function fillOkList() {
  if (!okProxies.value.length) return ElMessage.warning("没有可用代理");
  input.value = okProxies.value.join("\n");
  ElMessage.success(`已填入 ${okProxies.value.length} 个可用代理`);
}

async function copyOkList() {
  if (!okProxies.value.length) return ElMessage.warning("没有可用代理");
  const text = okProxies.value.join("\n");
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success("可用代理列表已复制到剪贴板");
  } catch {
    ElMessage.error("复制失败，请手动选择");
  }
}

function clearAll() {
  input.value = "";
  results.value = [];
  okProxies.value = [];
  summary.value = { total: 0, available: 0, unavailable: 0 };
}

function fillExample() {
  input.value = [
    "192.3.248.101|11433|gfa|mnc",
    "# 支持格式：IP|端口|账号|密码"
  ].join("\n");
}

function maxSec(r: ProxyTestResult) {
  return r.max_ms ? (r.max_ms / 1000).toFixed(2) : "-";
}
</script>
<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card">
      <template #header>
        <div class="card-header">
          <span>代理检测</span>
          <div class="header-tools">
            <el-button text @click="fillExample">填入示例</el-button>
            <el-button text @click="clearAll">清空</el-button>
          </div>
        </div>
      </template>

      <el-alert type="info" :closable="false" show-icon class="tip">
        <template #title>
          每个代理会测试 3 个目标：<b>后端</b> Apple 官网、<b>前端</b>
          小程序库存接口 + 购物袋接口。
        </template>
        <div class="tip-sub">
          <div>
            小程序接口返回 302 跳到 covers.html 属于 Apple 预购等待期，
            <b>代理本身是好的</b>，会自动判定为可用。
          </div>
          <div>支持格式：IP|端口|账号|密码 ／ 账号:密码@IP:端口 ／ IP:端口</div>
        </div>
      </el-alert>

      <el-row :gutter="12" class="opts">
        <el-col :span="8">
          <div class="opt-label">并发数</div>
          <el-input-number v-model="concurrency" :min="1" :max="64" style="width: 100%" />
        </el-col>
        <el-col :span="8">
          <div class="opt-label">单请求超时（毫秒）</div>
          <el-input-number
            v-model="timeoutMs"
            :min="2000"
            :max="60000"
            :step="1000"
            style="width: 100%"
          />
        </el-col>
        <el-col :span="8">
          <div class="opt-label">已输入</div>
          <div class="count-box">{{ proxyList.length }} 个代理</div>
        </el-col>
      </el-row>

      <el-input
        v-model="input"
        type="textarea"
        :rows="8"
        spellcheck="false"
        placeholder="每行一个代理，例如：&#10;192.3.248.101|11433|账号|密码&#10;# 以 # 开头是注释"
      />

      <div class="actions">
        <el-button type="primary" :loading="loading" @click="run">
          开始测试
        </el-button>
        <el-button :disabled="!okProxies.length" @click="fillOkList">
          把可用代理填回输入框
        </el-button>
        <el-button :disabled="!okProxies.length" @click="copyOkList">
          复制可用列表
        </el-button>
      </div>
    </el-card>

    <el-card v-if="summary.total" shadow="never" class="app-card">
      <template #header>
        <div class="card-header">
          <span>测试结果</span>
          <div class="header-tools">
            <el-input
              v-model="keyword"
              clearable
              placeholder="筛选代理"
              style="width: 200px"
            />
            <el-checkbox v-model="onlyFailed" label="只看失败" />
          </div>
        </div>
      </template>

      <div class="stat-row">
        <div class="stat">
          <div class="stat-num">{{ summary.total }}</div>
          <div class="stat-label">总计</div>
        </div>
        <div class="stat ok">
          <div class="stat-num">{{ summary.available }}</div>
          <div class="stat-label">可用</div>
        </div>
        <div class="stat bad">
          <div class="stat-num">{{ summary.unavailable }}</div>
          <div class="stat-label">不可用</div>
        </div>
      </div>

      <div class="result-list">
        <div
          v-for="(r, idx) in visible"
          :key="r.proxy + idx"
          :class="['result-item', r.ok ? 'is-ok' : 'is-bad']"
        >
          <div class="result-head">
            <el-tag :type="r.ok ? 'success' : 'danger'" size="small" effect="dark">
              {{ r.ok ? "可用" : "不可用" }}
            </el-tag>
            <span class="proxy-text">{{ r.proxy }}</span>
            <span v-if="r.ok" class="max-time">{{ maxSec(r) }}s</span>
            <span v-if="r.error" class="err-text">{{ r.error }}</span>
          </div>
          <div v-if="r.details?.length" class="detail-row">
            <div
              v-for="(d, i) in r.details"
              :key="i"
              :class="['detail-chip', d.ok ? 'chip-ok' : 'chip-bad']"
            >
              <span class="chip-kind">{{ d.kind }}</span>
              <span class="chip-name">{{ d.name }}</span>
              <span class="chip-status">{{ d.status || "-" }}</span>
              <span class="chip-time">{{ (d.elapsed_ms / 1000).toFixed(2) }}s</span>
              <span class="chip-note">{{ d.note }}</span>
            </div>
          </div>
        </div>
        <el-empty v-if="!visible.length" description="没有匹配的结果" :image-size="80" />
      </div>

      <div v-if="okProxies.length" class="ok-list">
        <div class="ok-list-title">可用代理列表（可直接粘贴到「监控与下单 → 静态 IP 列表」）</div>
        <pre class="ok-list-body">{{ okProxies.join("\n") }}</pre>
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
  align-items: center;
  gap: 8px;
}
.tip {
  margin-bottom: 14px;
}
.tip-sub {
  margin-top: 6px;
  font-size: 12px;
  line-height: 1.7;
  opacity: 0.85;
}
.opts {
  margin-bottom: 14px;
}
.opt-label {
  font-size: 12px;
  color: var(--el-text-color-secondary);
  margin-bottom: 6px;
}
.count-box {
  height: 32px;
  line-height: 32px;
  padding: 0 12px;
  border: 1px solid var(--el-border-color);
  border-radius: 4px;
  font-size: 13px;
}
.actions {
  margin-top: 12px;
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.stat-row {
  display: flex;
  gap: 12px;
  margin-bottom: 16px;
}
.stat {
  flex: 1;
  padding: 12px;
  border-radius: 6px;
  text-align: center;
  background: var(--el-fill-color-light);
}
.stat.ok {
  background: rgba(103, 194, 58, 0.12);
}
.stat.bad {
  background: rgba(245, 108, 108, 0.12);
}
.stat-num {
  font-size: 22px;
  font-weight: 600;
}
.stat-label {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.result-list {
  max-height: 520px;
  overflow: auto;
}
.result-item {
  padding: 10px 12px;
  border-radius: 6px;
  margin-bottom: 8px;
  border-left: 3px solid var(--el-border-color);
  background: var(--el-fill-color-lighter);
}
.result-item.is-ok {
  border-left-color: var(--el-color-success);
}
.result-item.is-bad {
  border-left-color: var(--el-color-danger);
}
.result-head {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.proxy-text {
  font-family: ui-monospace, Consolas, monospace;
  font-size: 13px;
  word-break: break-all;
}
.max-time {
  font-size: 12px;
  color: var(--el-color-success);
  font-weight: 600;
}
.err-text {
  font-size: 12px;
  color: var(--el-color-danger);
}
.detail-row {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 8px;
}
.detail-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 8px;
  border-radius: 4px;
  font-size: 12px;
  background: var(--el-fill-color);
}
.detail-chip.chip-ok {
  background: rgba(103, 194, 58, 0.12);
}
.detail-chip.chip-bad {
  background: rgba(245, 108, 108, 0.12);
}
.chip-kind {
  font-weight: 600;
}
.chip-status {
  font-family: ui-monospace, monospace;
}
.chip-time {
  opacity: 0.7;
}
.chip-note {
  opacity: 0.85;
}
.ok-list {
  margin-top: 16px;
  padding: 12px;
  border-radius: 6px;
  background: var(--el-fill-color-light);
}
.ok-list-title {
  font-size: 13px;
  margin-bottom: 8px;
  color: var(--el-text-color-secondary);
}
.ok-list-body {
  margin: 0;
  font-family: ui-monospace, Consolas, monospace;
  font-size: 13px;
  white-space: pre-wrap;
  word-break: break-all;
}
</style>
