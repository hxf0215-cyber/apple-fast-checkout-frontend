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
import { useRealtime } from "@/composables/useRealtime";
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
const proxyPoolStats = ref<any>(null);
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
  monitorStores: [] as string[],
  // 静态 IP
  staticProxyEnabled: false,
  staticProxiesText: "",
  // 动态代理池
  proxyPoolEnabled: false,
  proxyPoolApi: "",
  proxyPoolFetchProxy: "",
  proxyPoolLimitMode: "account",
  proxyPoolPerAccount: 3,
  proxyPoolFixedLimit: 20,
  proxyNumMode: "auto",
  proxyNumFixed: 50,
  proxyCheckIntervalMs: 10000,
  proxyMaxAgeMs: 30000,
  proxyPoolLowWatermark: 5,
  proxyFetchConcurrency: 5,
  proxyCheckUrl: "https://www.apple.com.cn",
  proxyPoolFormat: "line"
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

/** 批次类型标签：区分「批量预热」与「批量准备会话」（两者共用同一份进度） */
const progressKindLabel = computed(() => {
  switch (progress.value?.kind) {
    case "warmup":
      return "批量预热";
    case "prepare":
      return "批量准备会话";
    default:
      return "批量任务";
  }
});
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

// 把后端配置同步到表单。
// 不再使用「只填一次」的守卫 —— 那会导致首次填充失败后（前置请求抛异常）表单永久停留在
// 默认值，用户刷新后看到「配置被清空」。现在每次调用都会同步，避免这种假象。
function fillSettings() {
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
  settingsForm.staticProxyEnabled = settings.static_proxy_enabled ?? false;
  settingsForm.staticProxiesText = (settings.static_proxies || []).join("\n");
  settingsForm.proxyPoolEnabled = settings.proxy_pool_enabled ?? false;
  settingsForm.proxyPoolApi = settings.proxy_pool_api || "";
  settingsForm.proxyPoolFetchProxy = settings.proxy_pool_fetch_proxy || "";
  settingsForm.proxyPoolLimitMode = settings.proxy_pool_limit_mode || "account";
  settingsForm.proxyPoolPerAccount = settings.proxy_pool_per_account ?? 3;
  settingsForm.proxyPoolFixedLimit = settings.proxy_pool_fixed_limit ?? 20;
  settingsForm.proxyNumMode = settings.proxy_num_mode || "auto";
  settingsForm.proxyNumFixed = settings.proxy_num_fixed ?? 50;
  settingsForm.proxyCheckIntervalMs = settings.proxy_check_interval_ms ?? 10000;
  settingsForm.proxyMaxAgeMs = settings.proxy_max_age_ms ?? 30000;
  settingsForm.proxyPoolLowWatermark = settings.proxy_pool_low_watermark ?? 5;
  settingsForm.proxyFetchConcurrency = settings.proxy_fetch_concurrency ?? 5;
  settingsForm.proxyCheckUrl = settings.proxy_check_url || "https://www.apple.com.cn";
  settingsForm.proxyPoolFormat = settings.proxy_pool_format || "line";
}

async function loadProxyPoolStats() {
  try {
    proxyPoolStats.value = await api.proxyPoolStats();
  } catch {
    // 忽略
  }
}

async function clearProxyPool() {
  await ElMessageBox.confirm("确认清空代理池？", "确认操作", { type: "warning" });
  const res = await api.proxyPoolClear();
  ElMessage.success(`已清空 ${res.cleared} 个 IP`);
  await loadProxyPoolStats();
}

async function fetchProxyPool() {
  const res = await api.proxyPoolFetch();
  if (res.ok) ElMessage.success(`已补充 ${res.fetched || 0} 个 IP`);
  else ElMessage.error(res.error || "补充失败");
  await loadProxyPoolStats();
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
      circuit_breaker_cooldown_ms: settingsForm.circuitBreakerCooldownMs,
      static_proxy_enabled: settingsForm.staticProxyEnabled,
      static_proxies: settingsForm.staticProxiesText
        .split(/\r?\n/)
        .map(s => s.trim())
        .filter(Boolean),
      proxy_pool_enabled: settingsForm.proxyPoolEnabled,
      proxy_pool_api: settingsForm.proxyPoolApi.trim(),
      proxy_pool_fetch_proxy: settingsForm.proxyPoolFetchProxy.trim(),
      proxy_pool_limit_mode: settingsForm.proxyPoolLimitMode,
      proxy_pool_per_account: settingsForm.proxyPoolPerAccount,
      proxy_pool_fixed_limit: settingsForm.proxyPoolFixedLimit,
      proxy_num_mode: settingsForm.proxyNumMode,
      proxy_num_fixed: settingsForm.proxyNumFixed,
      proxy_check_interval_ms: settingsForm.proxyCheckIntervalMs,
      proxy_max_age_ms: settingsForm.proxyMaxAgeMs,
      proxy_pool_low_watermark: settingsForm.proxyPoolLowWatermark,
      proxy_fetch_concurrency: settingsForm.proxyFetchConcurrency,
      proxy_check_url: settingsForm.proxyCheckUrl.trim(),
      proxy_pool_format: settingsForm.proxyPoolFormat
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
  if (acting.value) return;
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
  if (acting.value) return;
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

/** 会话状态文案：区分「可直接下单」「仅半预热」「无」 */
function sessionLabel(account: any) {
  if (!account?.wechat_session_count) return "无";
  if (account.wechat_session_checkout_ready) return "✅ 可快速下单";
  if (account.wechat_session_fallback_only) return "⚠️ 半预热(无货)";
  return `${account.wechat_session_fresh_count}/${account.wechat_session_count}`;
}

let warmupRunning = false;
async function warmupBatch() {
  if (warmupRunning) {
    ElMessage.warning("批量预热正在进行中，请稍候");
    return;
  }
  warmupRunning = true;
  busy.value = true;
  try {
    const result = await api.warmupBatch({
      account_ids: selectedIds.value,
      // 把当前监控范围一起传过去：后端会先查库存，优先预热有货组合
      parts: settingsForm.monitorParts,
      stores: settingsForm.monitorStores
    });
    if (result.ok) {
      ElMessage.success(
        `批量预热已启动：${result.accounts || 0} 个账号 · ${result.available_combos ?? 0}/${result.combos ?? 0} 个组合有货（优先预热有货组合）`
      );
    } else {
      ElMessage.error(result.error || "批量预热失败");
    }
  } catch (error: any) {
    ElMessage.error(error?.message || "批量预热失败");
  } finally {
    busy.value = false;
    warmupRunning = false;
  }
}

let prepareBatchRunning = false;
async function wechatPrepareBatch() {
  if (prepareBatchRunning) {
    ElMessage.warning("批量准备正在进行中，请稍候");
    return;
  }
  prepareBatchRunning = true;
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
    prepareBatchRunning = false;
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
      sessionLabel(account),
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

const { state: wsState, connected: wsConnected } = useRealtime();
// 是否已完成首次设置同步（避免 WebSocket 推送覆盖用户正在编辑的内容）
let settingsSyncedOnce = false;

watch(
  wsState,
  (newState: any) => {
    if (!newState || !newState.accounts) return;
    state.value = newState;
    // 双保险：首次拿到带 settings 的状态时同步一次表单。
    // 这样即使 onMounted 的 step 1 失败了，WebSocket 推来数据后表单也能正确回填，
    // 不会再出现「刷新后静态 IP 列表变空」的假象。
    if (!settingsSyncedOnce && newState.settings) {
      settingsSyncedOnce = true;
      try {
        fillSettings();
      } catch (error) {
        console.error("monitor: fillSettings via ws failed:", error);
      }
    }
  },
  { immediate: true }
);

onMounted(async () => {
  // 初始化分三步独立容错：任一步失败都不影响其它步骤。
  // 尤其是设置同步（step 2）必须与账号详情加载解耦 —— 否则账号详情请求失败会导致
  // 表单停留在默认值，用户刷新后误以为「配置被清空」。

  // step 1：拉取基础数据（状态 / 型号 / 门店）
  let stateResult: AppStatePayload | null = null;
  let modelList: PhoneModel[] = [];
  let storeList: StoreInfo[] = [];
  try {
    const [s, m, st] = await Promise.all([api.state(), api.models(), api.stores()]);
    stateResult = s;
    modelList = m || [];
    storeList = st || [];
    state.value = s;
    models.value = modelList;
    stores.value = storeList;
    selectedAccountId.value = s.accounts[0]?.id || "";
    manualForm.accountId = selectedAccountId.value;
    wechatForm.accountId = selectedAccountId.value;
    manualForm.part =
      s.accounts[0]?.parts?.[0] || modelList?.[0]?.part || "";
    wechatForm.part = manualForm.part;
  } catch (error: any) {
    console.error("monitor: load state failed:", error);
    ElMessage.error("页面数据加载失败，请刷新重试");
  }

  // step 2：同步设置到表单（关键！不依赖 step 3，失败也不影响）
  // 这一步决定了「静态 IP 列表 / 开关」是否显示为已保存的值。
  try {
    fillSettings();
  } catch (error: any) {
    console.error("monitor: fillSettings failed:", error);
    ElMessage.error("配置回填失败，请刷新页面");
  }

  // step 3：加载账号详情 + 代理池统计（失败只提示，不阻断）
  try {
    await loadWechatDetail(true);
  } catch (error: any) {
    console.error("monitor: loadWechatDetail failed:", error);
  }
  try {
    await loadProxyPoolStats();
  } catch (error: any) {
    console.error("monitor: loadProxyPoolStats failed:", error);
  }

  // 代理池状态独立定时刷新（与主轮询解耦）
  setInterval(() => {
    if (settingsForm.proxyPoolEnabled) loadProxyPoolStats();
  }, 10000);
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
      <el-divider content-position="left">代理配置</el-divider>
      <el-form label-width="160px">
        <el-row :gutter="12">
          <el-col :span="12">
            <el-form-item label="静态 IP 分配">
              <el-switch v-model="settingsForm.staticProxyEnabled" />
              <span class="form-hint">为每个账号固定分配一个独立静态 IP</span>
            </el-form-item>
          </el-col>
          <el-col :span="12">
            <el-form-item label="动态代理池">
              <el-switch v-model="settingsForm.proxyPoolEnabled" />
              <span class="form-hint">从接口动态提取，取用后丢弃</span>
            </el-form-item>
          </el-col>
        </el-row>
        <el-form-item v-if="settingsForm.staticProxyEnabled" label="静态 IP 列表">
          <el-input
            v-model="settingsForm.staticProxiesText"
            type="textarea"
            :rows="4"
            placeholder="每行一个，支持以下格式：&#10;1.2.3.4:8080（无认证）&#10;1.2.3.4|8080|账号|密码（竖线分隔，推荐）&#10;1.2.3.4:8080:账号:密码（冒号分隔）&#10;账号:密码@1.2.3.4:8080（带认证）&#10;socks5://1.2.3.4:1080&#10;（不带协议前缀默认按 http:// 处理）"
          />
          <div class="form-hint">
            竖线格式示例：96.8.121.134|11515|uhe|elz（IP|端口|账号|密码）
          </div>
        </el-form-item>
        <template v-if="settingsForm.proxyPoolEnabled">
          <el-form-item label="提取接口">
            <el-input
              v-model="settingsForm.proxyPoolApi"
              placeholder="任意供应商接口，如 http://api.xiequ.cn/VAD/GetIp.aspx?...&num=200"
            />
            <div class="form-hint">可填入任何代理服务商的提取链接（支持 / 分隔、&amp; 参数、API Key）</div>
          </el-form-item>
          <el-form-item label="接口请求代理">
            <el-input
              v-model="settingsForm.proxyPoolFetchProxy"
              placeholder="留空直连；如需绕过接口方白名单，填 socks5://user:pass@host:port"
            />
            <div class="form-hint">
              调用提取接口时使用的出口代理。填了这个，接口方看到的是该代理的 IP，可绕过 IP 白名单限制。
            </div>
          </el-form-item>
          <el-row :gutter="12">
            <el-col :span="6">
              <el-form-item label="池上限模式">
                <el-select v-model="settingsForm.proxyPoolLimitMode">
                  <el-option label="按账号倍数" value="account" />
                  <el-option label="固定数量" value="fixed" />
                </el-select>
              </el-form-item>
            </el-col>
            <el-col v-if="settingsForm.proxyPoolLimitMode === 'account'" :span="6">
              <el-form-item label="每账号 IP 倍率">
                <el-input-number v-model="settingsForm.proxyPoolPerAccount" :min="1" :max="50" />
              </el-form-item>
            </el-col>
            <el-col v-else :span="6">
              <el-form-item label="固定池上限">
                <el-input-number v-model="settingsForm.proxyPoolFixedLimit" :min="1" :max="5000" />
              </el-form-item>
            </el-col>
            <el-col :span="6">
              <el-form-item label="num 参数模式">
                <el-select v-model="settingsForm.proxyNumMode">
                  <el-option label="按需自动计算" value="auto" />
                  <el-option label="固定值" value="fixed" />
                </el-select>
              </el-form-item>
            </el-col>
            <el-col v-if="settingsForm.proxyNumMode === 'fixed'" :span="6">
              <el-form-item label="num 固定值">
                <el-input-number v-model="settingsForm.proxyNumFixed" :min="1" :max="1000" />
              </el-form-item>
            </el-col>
            <el-col :span="6"><el-form-item label="测活间隔 ms"><el-input-number v-model="settingsForm.proxyCheckIntervalMs" :min="1000" :max="600000" /></el-form-item></el-col>
            <el-col :span="6"><el-form-item label="IP 最大存活 ms"><el-input-number v-model="settingsForm.proxyMaxAgeMs" :min="5000" :max="600000" /></el-form-item></el-col>
            <el-col :span="6"><el-form-item label="低水位阈值"><el-input-number v-model="settingsForm.proxyPoolLowWatermark" :min="1" :max="500" /></el-form-item></el-col>
            <el-col :span="6"><el-form-item label="并发提取线程"><el-input-number v-model="settingsForm.proxyFetchConcurrency" :min="1" :max="50" /></el-form-item></el-col>
            <el-col :span="6"><el-form-item label="返回格式"><el-select v-model="settingsForm.proxyPoolFormat"><el-option label="每行一个" value="line" /><el-option label="JSON 数组" value="json" /></el-select></el-form-item></el-col>
            <el-col :span="12"><el-form-item label="测活目标 URL"><el-input v-model="settingsForm.proxyCheckUrl" /></el-form-item></el-col>
          </el-row>
          <el-form-item label="代理池状态">
            <div class="pool-stats">
              <el-tag :type="proxyPoolStats?.alive > 0 ? 'success' : 'info'">
                存活 {{ proxyPoolStats?.alive || 0 }}
              </el-tag>
              <el-tag type="info">池内 {{ proxyPoolStats?.total || 0 }} / 上限 {{ proxyPoolStats?.max_size || 0 }}</el-tag>
              <el-tag type="info">累计提取 {{ proxyPoolStats?.fetched_total || 0 }}</el-tag>
              <el-tag type="warning">已消耗 {{ proxyPoolStats?.discarded_total || 0 }}</el-tag>
              <el-button size="small" @click="loadProxyPoolStats">刷新状态</el-button>
              <el-button size="small" @click="fetchProxyPool">立即补充</el-button>
              <el-button size="small" type="danger" plain @click="clearProxyPool">清空池</el-button>
            </div>
            <div v-if="proxyPoolStats?.last_fetch_error" class="pool-error">
              最近提取错误：{{ proxyPoolStats.last_fetch_error }}
            </div>
          </el-form-item>
        </template>
      </el-form>

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
            {{ progressKindLabel }}中：{{ progress.completed }}/{{ progress.total }} · 成功
            {{ progress.succeeded }} · 失败 {{ progress.failed }} ·
            {{ progressPercent }}%
          </template>
          <template v-else-if="progress?.completed > 0">
            上次{{ progressKindLabel }}：{{ progress.completed }}/{{ progress.total }} · 成功
            {{ progress.succeeded }} · 失败 {{ progress.failed }} ·
            {{ progressPercent }}%
          </template>
          <template v-else
            >暂未开始批量任务（批量预热 / 批量准备会话 都会在此显示进度）</template
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
            <el-button :loading="busy" type="warning" @click="warmupBatch">
              批量预热（有货更快，无货可降级）
            </el-button>
            <el-button :loading="busy" @click="wechatPrepareBatch"
            >
              批量准备全部账号会话
            </el-button>
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
.form-hint {
  margin-left: 10px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.pool-stats {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
}
.pool-error {
  margin-top: 6px;
  font-size: 12px;
  color: #f56c6c;
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

