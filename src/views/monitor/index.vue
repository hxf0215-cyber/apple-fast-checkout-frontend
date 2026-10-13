<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  api,
  type AccountConfig,
  type AppStatePayload,
  type PhoneModel,
  type StoreInfo,
  type StockResult
} from "@/api/client";
import {
  cookieStatusText,
  formatDuration,
  modelLabel,
  statusText
} from "@/utils/apple";

defineOptions({ name: "Monitor" });

// router not needed
const state = ref<AppStatePayload | null>(null);
const models = ref<PhoneModel[]>([]);
const stores = ref<StoreInfo[]>([]);
const manualStores = ref<StockResult[]>([]);
const selectedAccountId = ref("");
const keyword = ref("");
const statusFilter = ref("all");
const selectedIds = ref<string[]>([]);
const saving = ref(false);
const acting = ref(false);
let settingsLoaded = false;
const busy = ref(false);
const manualResult = ref("");
const wechatResult = ref("");
const centralResult = ref("");
// loginDetailId not used
const wechatDetail = ref<AccountConfig | null>(null);
const wechatForm = reactive({
  accountId: "",
  part: "",
  storeId: "",
  cookie: ""
});
const manualForm = reactive({ accountId: "", part: "", storeId: "" });
const settingsForm = reactive({
  batchPrepareConcurrency: 100,
  orderConcurrency: 100,
  directOrderConcurrency: 100,
  retryBaseDelayMs: 2000,
  retryMaxAttempts: 3,
  circuitBreakerThreshold: 20,
  circuitBreakerCooldownMs: 30000,
  autoRefreshSessions: true,
  retryEnabled: true,
  monitorParts: [] as string[],
  monitorStores: [] as string[]
});

const accounts = computed(() => state.value?.accounts || []);
const selectedAccount = computed(
  () =>
    accounts.value.find(account => account.id === selectedAccountId.value) ||
    accounts.value[0] ||
    null
);
const filteredAccounts = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  const list = key
    ? accounts.value.filter(account =>
        [
          account.username,
          account.id,
          ...account.parts,
          ...account.stores
        ].some(value =>
          String(value || "")
            .toLowerCase()
            .includes(key)
        )
      )
    : accounts.value;
  return list.filter(
    account =>
      statusFilter.value === "all" ||
      (statusFilter.value === "running" && account.running) ||
      (statusFilter.value === "stopped" && !account.running) ||
      (statusFilter.value === "error" && account.status === "error") ||
      (statusFilter.value === "ordered" && account.orders_placed > 0) ||
      (statusFilter.value === "no-cookie" && !account.cookie_count)
  );
});

const progress = computed(() => state.value?.batch_progress);
const progressPercent = computed(() =>
  progress.value?.total
    ? Math.round((progress.value.completed / progress.value.total) * 100)
    : 0
);

async function load() {
  const result = await api.state();
  state.value = result;
  if (!selectedAccountId.value && result.accounts.length)
    selectedAccountId.value = result.accounts[0].id;
}

function fillSettings(force = false) {
  if (settingsLoaded && !force) return;
  settingsLoaded = true;
  if (!state.value) return;
  const settings = state.value.settings;
  settingsForm.batchPrepareConcurrency =
    settings.batch_prepare_concurrency || 100;
  settingsForm.orderConcurrency = settings.order_concurrency || 100;
  settingsForm.directOrderConcurrency =
    settings.direct_order_concurrency || 100;
  settingsForm.retryBaseDelayMs = settings.retry_base_delay_ms || 2000;
  settingsForm.retryMaxAttempts = settings.retry_max_attempts || 3;
  settingsForm.circuitBreakerThreshold =
    settings.circuit_breaker_threshold || 20;
  settingsForm.circuitBreakerCooldownMs =
    settings.circuit_breaker_cooldown_ms || 30000;
  settingsForm.autoRefreshSessions = settings.auto_refresh_sessions;
  settingsForm.retryEnabled = settings.retry_enabled;
  settingsForm.monitorParts = settings.monitor_parts || [];
  settingsForm.monitorStores = settings.monitor_stores || [];
}

async function saveCentralMonitor() {
  if (!settingsForm.monitorParts.length)
    return ElMessage.warning("请至少选择一个中央监控型号");
  if (!settingsForm.monitorStores.length)
    return ElMessage.warning("请至少选择一个中央监控门店");
  if (!state.value) return;
  saving.value = true;
  try {
    const payload = {
      ...state.value.settings,
      monitor_parts: settingsForm.monitorParts,
      monitor_stores: settingsForm.monitorStores,
      auto_refresh_sessions: settingsForm.autoRefreshSessions,
      retry_enabled: settingsForm.retryEnabled,
      batch_prepare_concurrency: settingsForm.batchPrepareConcurrency,
      order_concurrency: settingsForm.orderConcurrency,
      direct_order_concurrency: settingsForm.directOrderConcurrency,
      retry_base_delay_ms: settingsForm.retryBaseDelayMs,
      retry_max_attempts: settingsForm.retryMaxAttempts,
      circuit_breaker_threshold: settingsForm.circuitBreakerThreshold,
      circuit_breaker_cooldown_ms: settingsForm.circuitBreakerCooldownMs
    };
    await api.saveSettings(payload);
    state.value.settings = payload;
    centralResult.value = `中央监控范围已保存：型号 ${settingsForm.monitorParts.length} 个 / 门店 ${settingsForm.monitorStores.length} 家。`;
    ElMessage.success("监控范围已保存");
  } catch (error: any) {
    ElMessage.error(error?.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

async function startAccount(id: string) {
  await api.startAccount(id);
  await load();
}
async function stopAccount(id: string) {
  await api.stopAccount(id);
  await load();
}

async function startAll() {
  acting.value = true;
  try {
    await api.start();
    ElMessage.success("中央监控已启动");
    await load();
  } catch (error: any) {
    ElMessage.error(error?.message || "启动失败");
  } finally {
    acting.value = false;
  }
}

async function stopAll() {
  acting.value = true;
  try {
    await api.stop();
    ElMessage.success("监控已停止");
    await load();
  } catch (error: any) {
    ElMessage.error(error?.message || "停止失败");
  } finally {
    acting.value = false;
  }
}

async function clearCompleted() {
  try {
    await ElMessageBox.confirm(
      "确认清空所有账号的“已完成”状态？清空后这些账号会继续参与下单。",
      "确认操作",
      { type: "warning" }
    );
    const result = await api.clearCompleted();
    ElMessage.success(`已清空 ${result.cleared ?? 0} 个账号状态`);
    await load();
  } catch (error: any) {
    if (!["cancel", "close"].includes(error)) {
      ElMessage.error(error?.message || "操作失败");
    }
  }
}

async function queryManualStores() {
  if (!manualForm.accountId || !manualForm.part)
    return ElMessage.warning("请先选择账号和型号");
  busy.value = true;
  try {
    const result = await api.queryStock(manualForm.accountId, {
      part: manualForm.part
    });
    manualStores.value = Array.isArray(result) ? result : [];
    manualResult.value = `已获取 ${manualStores.value.length} 家门店。`;
  } finally {
    busy.value = false;
  }
}

async function manualOrder() {
  if (!manualForm.accountId || !manualForm.part || !manualForm.storeId)
    return ElMessage.warning("请选择账号、型号和门店");
  await ElMessageBox.confirm(
    `确认使用当前账号手动下单？型号：${manualForm.part}，门店：${manualForm.storeId}`,
    "确认操作",
    { type: "warning" }
  );
  busy.value = true;
  try {
    const result = await api.manualOrder(manualForm.accountId, {
      part: manualForm.part,
      store_id: manualForm.storeId
    });
    if (!result.ok) throw new Error(result.error || "下单失败");
    manualResult.value = "下单成功";
    await load();
  } catch (error: any) {
    manualResult.value = error?.message || "下单失败";
  } finally {
    busy.value = false;
  }
}

async function loadWechatDetail(force = false) {
  if (!wechatForm.accountId) return;
  if (!force && wechatDetail.value?.id === wechatForm.accountId) return;
  wechatDetail.value = await api.getAccount(wechatForm.accountId);
  wechatForm.cookie = wechatDetail.value.wechat_cookie || "";
}

async function wechatOrder(place: boolean) {
  if (
    !wechatForm.accountId ||
    !wechatForm.part ||
    !wechatForm.storeId ||
    !wechatForm.cookie
  )
    return ElMessage.warning("请选择账号、型号、门店并填写 Cookie");
  if (place)
    await ElMessageBox.confirm(
      `确认使用微信小程序真实下单？型号：${wechatForm.part}，门店：${wechatForm.storeId}`,
      "确认操作",
      { type: "warning" }
    );
  busy.value = true;
  try {
    const result = await api.wechatOrder(wechatForm.accountId, {
      part: wechatForm.part,
      store_id: wechatForm.storeId,
      place,
      cookie: wechatForm.cookie
    });
    if (!result.ok) throw new Error(result.error || "小程序下单失败");
    wechatResult.value = `${place ? "小程序下单流程完成" : "小程序下单演练完成"}${result.summary?.order_number ? `，订单号：${result.summary.order_number}` : ""}`;
  } catch (error: any) {
    wechatResult.value = error?.message || "小程序下单失败";
  } finally {
    busy.value = false;
  }
}

async function wechatPrepare() {
  if (
    !wechatForm.accountId ||
    !wechatForm.part ||
    !wechatForm.storeId ||
    !wechatForm.cookie
  )
    return ElMessage.warning("请选择账号、型号、门店并填写 Cookie");
  busy.value = true;
  try {
    const result = await api.wechatPrepare(wechatForm.accountId, {
      part: wechatForm.part,
      store_id: wechatForm.storeId,
      place: false,
      cookie: wechatForm.cookie
    });
    if (!result.ok) throw new Error(result.error || "准备下单会话失败");
    wechatResult.value = "微信小程序下单会话已准备";
  } catch (error: any) {
    wechatResult.value = error?.message || "准备失败";
  } finally {
    busy.value = false;
  }
}

async function wechatPrepareBatch() {
  busy.value = true;
  try {
    const result = await api.wechatPrepareBatch({
      account_ids: selectedIds.value,
      part: wechatForm.part
    });
    wechatResult.value = `批量准备会话已启动：${result.accounts || 0} 个账号 / ${result.sessions || 0} 个组合`;
  } catch (error: any) {
    wechatResult.value = error?.message || "批量准备失败";
  } finally {
    busy.value = false;
  }
}

function exportReport() {
  if (!state.value) return;
  const rows = [
    [
      "类型",
      "ID",
      "账号",
      "状态",
      "优先级",
      "会话",
      "Cookie",
      "代理",
      "尝试",
      "订单"
    ],
    ...state.value.accounts.map(account => [
      "账号",
      account.id,
      account.username,
      account.status,
      account.priority_score,
      `${account.wechat_session_fresh_count}/${account.wechat_session_count}`,
      account.wechat_cookie_status,
      account.proxy_health,
      account.order_attempts,
      account.orders_placed
    ]),
    [
      "汇总",
      "-",
      "-",
      state.value.circuit_open ? "熔断中" : "正常",
      "-",
      "-",
      "-",
      "-",
      state.value.order_attempts,
      state.value.order_successes
    ]
  ];
  const csv = rows
    .map(row =>
      row
        .map(value => `"${String(value ?? "").replaceAll('"', '""')}"`)
        .join(",")
    )
    .join("\n");
  const link = document.createElement("a");
  link.href = URL.createObjectURL(
    new Blob(["\uFEFF" + csv], { type: "text/csv;charset=utf-8" })
  );
  link.download = `apple-runtime-report-${Date.now()}.csv`;
  link.click();
  URL.revokeObjectURL(link.href);
}

let pollTimer: ReturnType<typeof setInterval> | null = null;

onMounted(async () => {
  try {
    const [stateResult, modelList, storeList] = await Promise.all([
      api.state(),
      api.models(),
      api.stores()
    ]);
    state.value = stateResult;
    models.value = modelList || [];
    stores.value = storeList || [];
    selectedAccountId.value = stateResult.accounts[0]?.id || "";
    manualForm.accountId = selectedAccountId.value;
    wechatForm.accountId = selectedAccountId.value;
    manualForm.part =
      stateResult.accounts[0]?.parts?.[0] || modelList?.[0]?.part || "";
    wechatForm.part = manualForm.part;
    await loadWechatDetail(true);
    fillSettings();
    pollTimer = setInterval(async () => {
      await load();
      fillSettings();
    }, 2000);
  } catch (error: any) {
    console.error("monitor init failed:", error);
  }
});

watch(selectedAccountId, id => {
  manualForm.accountId = id;
  wechatForm.accountId = id;
  const account = selectedAccount.value;
  manualForm.part = account?.parts?.[0] || manualForm.part;
  wechatForm.part = manualForm.part;
  loadWechatDetail(true);
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
});
</script>

<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card">
      <template #header><span>中央监控范围</span></template>
      <el-form label-width="160px">
        <el-row :gutter="12">
          <el-col :span="12"
            ><el-form-item label="监控型号"
              ><el-select
                v-model="settingsForm.monitorParts"
                multiple
                filterable
                ><el-option
                  v-for="model in models"
                  :key="model.part"
                  :value="model.part"
                  :label="modelLabel(model)" /></el-select></el-form-item
          ></el-col>
          <el-col :span="12"
            ><el-form-item label="监控门店"
              ><el-select
                v-model="settingsForm.monitorStores"
                multiple
                filterable
                ><el-option
                  v-for="store in stores"
                  :key="store.store_id"
                  :value="store.store_id"
                  :label="`${store.store_id} · ${store.name} · ${store.city}`" /></el-select></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="批量准备并发"
              ><el-input-number
                v-model="settingsForm.batchPrepareConcurrency"
                :min="1"
                :max="200" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="下单并发"
              ><el-input-number
                v-model="settingsForm.orderConcurrency"
                :min="1"
                :max="200" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="直连下单并发"
              ><el-input-number
                v-model="settingsForm.directOrderConcurrency"
                :min="1"
                :max="200" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="重试基础延迟 ms"
              ><el-input-number
                v-model="settingsForm.retryBaseDelayMs"
                :min="100" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="最大尝试次数"
              ><el-input-number
                v-model="settingsForm.retryMaxAttempts"
                :min="1"
                :max="10" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="熔断阈值"
              ><el-input-number
                v-model="settingsForm.circuitBreakerThreshold"
                :min="1" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="熔断冷却 ms"
              ><el-input-number
                v-model="settingsForm.circuitBreakerCooldownMs"
                :min="1000" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="自动续期会话"
              ><el-switch
                v-model="settingsForm.autoRefreshSessions" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="失败重试"
              ><el-switch v-model="settingsForm.retryEnabled" /></el-form-item
          ></el-col>
        </el-row>
      </el-form>
      <div class="toolbar">
        <el-button
          @click="
            settingsForm.monitorStores = stores.map(store => store.store_id)
          "
          >全选全国门店</el-button
        >
        <el-button @click="settingsForm.monitorStores = []">清空门店</el-button>
        <el-button
          @click="
            settingsForm.batchPrepareConcurrency =
              state?.recommended_batch_concurrency || 32
          "
          >推荐并发</el-button
        >
        <el-button @click="exportReport">导出运行报告</el-button>
        <el-button type="primary" :loading="saving" @click="saveCentralMonitor"
          >保存监控范围</el-button
        >
      </div>
      <el-divider content-position="left">全局操作</el-divider>
      <div class="toolbar">
        <el-button
          type="success"
          size="large"
          :loading="acting"
          @click="startAll"
          >启动中央监控</el-button
        >
        <el-button type="danger" size="large" :loading="acting" @click="stopAll"
          >停止监控</el-button
        >
        <el-button size="large" :loading="acting" @click="clearCompleted"
          >清空已完成状态</el-button
        >
      </div>
      <div class="batch-progress-wrap">
        <el-progress
          :percentage="progressPercent"
          :status="
            progress?.running
              ? undefined
              : progress?.failed
                ? 'warning'
                : progress?.completed
                  ? 'success'
                  : undefined
          "
        />
        <div class="batch-progress-text">
          <template v-if="progress?.running">
            批量准备中：{{ progress.completed }}/{{ progress.total }} · 成功
            {{ progress.succeeded }} · 失败 {{ progress.failed }} ·
            {{ progressPercent }}%
          </template>
          <template v-else-if="progress?.completed > 0">
            上次批量准备：{{ progress.completed }}/{{ progress.total }} · 成功
            {{ progress.succeeded }} · 失败 {{ progress.failed }} ·
            {{ progressPercent }}%
          </template>
          <template v-else
            >暂未开始批量准备（点击「批量准备全部账号会话」后会显示进度）</template
          >
        </div>
      </div>
      <el-alert
        v-if="centralResult"
        :title="centralResult"
        type="success"
        show-icon
        :closable="false"
      />
    </el-card>

    <el-row :gutter="12" class="fill-row">
      <el-col :xs="24" :md="8">
        <el-card shadow="never" class="app-card fill-card">
          <template #header>
            <div class="card-header">
              <span>账号列表</span>
              <el-select
                v-model="statusFilter"
                size="small"
                style="width: 120px"
              >
                <el-option label="全部状态" value="all" />
                <el-option label="运行中" value="running" />
                <el-option label="已停止" value="stopped" />
                <el-option label="异常" value="error" />
                <el-option label="已下单" value="ordered" />
                <el-option label="未登录" value="no-cookie" />
              </el-select>
            </div>
          </template>
          <el-input
            v-model="keyword"
            clearable
            placeholder="搜索账号"
            class="search-input"
          />
          <el-table
            :data="filteredAccounts"
            height="100%"
            @selection-change="rows => (selectedIds = rows.map(row => row.id))"
          >
            <el-table-column type="selection" width="42" />
            <el-table-column label="账号" min-width="120">
              <template #default="{ row }">
                <el-link @click="selectedAccountId = row.id">{{
                  row.username
                }}</el-link>
              </template>
            </el-table-column>
            <el-table-column label="状态" width="80">
              <template #default="{ row }">{{
                statusText(row.status)
              }}</template>
            </el-table-column>
            <el-table-column label="操作" width="90">
              <template #default="{ row }">
                <el-button
                  size="small"
                  :disabled="row.running"
                  @click="startAccount(row.id)"
                  >启动</el-button
                >
                <el-button
                  size="small"
                  :disabled="!row.running"
                  @click="stopAccount(row.id)"
                  >停止</el-button
                >
              </template>
            </el-table-column>
          </el-table>
        </el-card>
      </el-col>
      <el-col :xs="24" :md="16">
        <el-card shadow="never" class="app-card fill-card">
          <template #header><span>账号详情与下单</span></template>
          <el-descriptions
            v-if="selectedAccount"
            :column="3"
            border
            size="small"
            class="detail-descriptions"
          >
            <el-descriptions-item label="账号">{{
              selectedAccount.username
            }}</el-descriptions-item>
            <el-descriptions-item label="状态">{{
              statusText(selectedAccount.status)
            }}</el-descriptions-item>
            <el-descriptions-item label="登录">{{
              selectedAccount.cookie_count
                ? `Cookie ${selectedAccount.cookie_count}`
                : "未登录"
            }}</el-descriptions-item>
            <el-descriptions-item label="监控型号">{{
              selectedAccount.parts.join(" / ")
            }}</el-descriptions-item>
            <el-descriptions-item label="监控门店">{{
              selectedAccount.stores.join(" / ")
            }}</el-descriptions-item>
            <el-descriptions-item label="最近耗时"
              >{{ selectedAccount.last_latency_ms }}ms</el-descriptions-item
            >
            <el-descriptions-item label="会话"
              >{{ selectedAccount.wechat_session_fresh_count }}/{{
                selectedAccount.wechat_session_count
              }}{{
                selectedAccount.wechat_session_min_remaining_ms
                  ? ` · ${formatDuration(selectedAccount.wechat_session_min_remaining_ms)}`
                  : ""
              }}</el-descriptions-item
            >
            <el-descriptions-item label="小程序 Cookie">{{
              cookieStatusText(selectedAccount.wechat_cookie_status)
            }}</el-descriptions-item>
            <el-descriptions-item label="订单">{{
              selectedAccount.orders_placed
            }}</el-descriptions-item>
            <el-descriptions-item label="最近错误" :span="3">{{
              selectedAccount.last_error || "-"
            }}</el-descriptions-item>
            <el-descriptions-item label="支付链接" :span="3">
              <el-link
                v-if="selectedAccount.last_payment_link"
                :href="selectedAccount.last_payment_link"
                target="_blank"
                >打开支付</el-link
              >
              <span v-else>-</span>
            </el-descriptions-item>
          </el-descriptions>
          <el-empty v-else description="暂无账号" />

          <el-divider>手动指定门店下单</el-divider>
          <el-form label-width="90px">
            <el-row :gutter="12">
              <el-col :span="8"
                ><el-form-item label="账号"
                  ><el-select v-model="manualForm.accountId" filterable
                    ><el-option
                      v-for="account in accounts"
                      :key="account.id"
                      :value="account.id"
                      :label="account.username" /></el-select></el-form-item
              ></el-col>
              <el-col :span="8"
                ><el-form-item label="型号"
                  ><el-select v-model="manualForm.part" filterable
                    ><el-option
                      v-for="model in models"
                      :key="model.part"
                      :value="model.part"
                      :label="modelLabel(model)" /></el-select></el-form-item
              ></el-col>
              <el-col :span="8"
                ><el-form-item label="门店"
                  ><el-select v-model="manualForm.storeId" filterable
                    ><el-option
                      v-for="store in manualStores"
                      :key="store.store_id"
                      :value="store.store_id"
                      :label="`${store.store_id} · ${store.name || '-'}`" /></el-select></el-form-item
              ></el-col>
            </el-row>
          </el-form>
          <div class="toolbar">
            <el-button :loading="busy" @click="queryManualStores"
              >查询可选门店</el-button
            >
            <el-button type="danger" :loading="busy" @click="manualOrder"
              >确认手动下单</el-button
            >
          </div>
          <el-alert
            v-if="manualResult"
            :title="manualResult"
            :closable="false"
            show-icon
          />

          <el-divider>微信小程序下单</el-divider>
          <el-form label-width="90px">
            <el-row :gutter="12">
              <el-col :span="8"
                ><el-form-item label="账号"
                  ><el-select
                    v-model="wechatForm.accountId"
                    filterable
                    @change="loadWechatDetail(true)"
                    ><el-option
                      v-for="account in accounts"
                      :key="account.id"
                      :value="account.id"
                      :label="account.username" /></el-select></el-form-item
              ></el-col>
              <el-col :span="8"
                ><el-form-item label="型号"
                  ><el-select v-model="wechatForm.part" filterable
                    ><el-option
                      v-for="model in models"
                      :key="model.part"
                      :value="model.part"
                      :label="modelLabel(model)" /></el-select></el-form-item
              ></el-col>
              <el-col :span="8"
                ><el-form-item label="门店"
                  ><el-select v-model="wechatForm.storeId" filterable
                    ><el-option
                      v-for="store in stores"
                      :key="store.store_id"
                      :value="store.store_id"
                      :label="`${store.store_id} · ${store.name}`" /></el-select></el-form-item
              ></el-col>
              <el-col :span="24"
                ><el-form-item label="Cookie"
                  ><el-input
                    v-model="wechatForm.cookie"
                    type="textarea"
                    :rows="3"
                    placeholder="留空使用账号保存的 Cookie" /></el-form-item
              ></el-col>
            </el-row>
          </el-form>
          <div class="toolbar">
            <el-button :loading="busy" @click="wechatPrepare"
              >准备下单会话</el-button
            >
            <el-button :loading="busy" @click="wechatPrepareBatch"
              >批量准备全部账号会话</el-button
            >
            <el-button :loading="busy" @click="wechatOrder(false)"
              >演练</el-button
            >
            <el-button type="danger" :loading="busy" @click="wechatOrder(true)"
              >真实下单</el-button
            >
          </div>
          <el-alert
            v-if="wechatResult"
            :title="wechatResult"
            :closable="false"
            show-icon
          />
        </el-card>
      </el-col>
    </el-row>
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.app-card {
  margin-bottom: 12px;
}
.fill-row {
  height: calc(100vh - 320px);
}
.fill-card {
  height: 100%;
  display: flex;
  flex-direction: column;
}
.toolbar {
  display: flex;
  gap: 8px;
  margin-bottom: 12px;
  flex-wrap: wrap;
}
.search-input {
  margin-bottom: 8px;
}
.detail-descriptions {
  margin-bottom: 12px;
}
.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}
.batch-progress-wrap {
  margin-top: 12px;
}
.batch-progress-text {
  margin-top: 6px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
  line-height: 1.6;
}
</style>
