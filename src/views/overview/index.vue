<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import {
  api,
  type AccountStatus,
  type OrderRecord,
  type PhoneModel,
  type StoreInfo
} from "@/api/client";
import {
  cookieStatusText,
  formatDuration,
  formatTime,
  modelLabel,
  statusText
} from "@/utils/apple";

defineOptions({ name: "Overview" });

const accounts = ref<AccountStatus[]>([]);
const orders = ref<OrderRecord[]>([]);
const models = ref<PhoneModel[]>([]);
const stores = ref<StoreInfo[]>([]);
const keyword = ref("");
const statusFilter = ref("all");
const orderKeyword = ref("");
const openGroups = ref<string[]>([]);
const groupsInitialized = ref(false);
const loading = ref(false);

const quickLinks = [
  { path: "/config", title: "账号配置", desc: "多型号、多门店、代理、Cookie" },
  { path: "/monitor", title: "监控与手动下单", desc: "自动监控，手动指定门店" },
  { path: "/query", title: "库存查询", desc: "按账号代理查询门店" },
  { path: "/logs", title: "运行日志", desc: "中文日志，按账号显示" }
];

const metrics = computed(() => ({
  running: accounts.value.filter(account => account.running).length,
  total: accounts.value.length,
  orders: accounts.value.reduce(
    (sum, account) => sum + account.orders_placed,
    0
  ),
  errors: accounts.value.filter(account => account.status === "error").length
}));

const filteredAccounts = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  return accounts.value.filter(account => {
    const matched =
      !key ||
      [account.username, account.id, ...account.parts, ...account.stores].some(
        value =>
          String(value || "")
            .toLowerCase()
            .includes(key)
      );
    const matchedStatus =
      statusFilter.value === "all" ||
      (statusFilter.value === "running" && account.running) ||
      (statusFilter.value === "stopped" && !account.running) ||
      (statusFilter.value === "error" && account.status === "error") ||
      (statusFilter.value === "ordered" && account.orders_placed > 0) ||
      (statusFilter.value === "no-cookie" && !account.cookie_count);
    return matched && matchedStatus;
  });
});

const filteredOrders = computed(() => {
  const key = orderKeyword.value.trim().toLowerCase();
  if (!key) return [...orders.value].reverse();
  return orders.value
    .filter(order =>
      [
        order.username,
        order.order_number,
        order.part,
        order.store_id,
        order.pickup_date,
        order.pickup_time
      ].some(value =>
        String(value || "")
          .toLowerCase()
          .includes(key)
      )
    )
    .reverse();
});

const orderGroups = computed(() => {
  const groups = [
    ...new Set(filteredOrders.value.map(order => order.username))
  ];
  return groups.map(username => ({
    username,
    orders: filteredOrders.value.filter(order => order.username === username)
  }));
});

watch(
  filteredOrders,
  () => {
    if (!groupsInitialized.value && orderGroups.value.length) {
      openGroups.value = [orderGroups.value[0].username];
      groupsInitialized.value = true;
    }
  },
  { immediate: true }
);

function storeName(storeId: string) {
  const store = stores.value.find(item => item.store_id === storeId);
  return store
    ? `${store.province} ${store.city} · ${store.name}`
    : storeId || "-";
}

function partText(parts?: string[], part?: string) {
  const list = parts && parts.length ? parts : part ? [part] : [];
  return (
    list
      .map(item => {
        const model = models.value.find(model => model.part === item);
        return model ? modelLabel(model) : item;
      })
      .join(" / ") || "-"
  );
}

function storeIds(account: AccountStatus) {
  const list =
    account.stores && account.stores.length
      ? account.stores
      : [account.store_id];
  return list
    .map(id => {
      const store = stores.value.find(item => item.store_id === id);
      return store ? store.province + " " + store.city + " " + store.name : id;
    })
    .join(" / ");
}

function partLabel(part: string) {
  const model = models.value.find(item => item.part === part);
  return model ? modelLabel(model) : part;
}

async function load() {
  loading.value = true;
  try {
    const state = await api.state();
    accounts.value = state.accounts || [];
    orders.value = state.orders || [];
  } finally {
    loading.value = false;
  }
}

let pollTimer: ReturnType<typeof setInterval> | null = null;

onMounted(async () => {
  try {
    const [state, modelList, storeList] = await Promise.all([
      api.state(),
      api.models(),
      api.stores()
    ]);
    accounts.value = state.accounts || [];
    orders.value = state.orders || [];
    models.value = modelList || [];
    stores.value = storeList || [];
    pollTimer = setInterval(load, 1500);
  } catch (error: any) {
    console.error("overview init failed:", error);
  }
});

watch(orderKeyword, () => {
  groupsInitialized.value = false;
});

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer);
});
</script>
<template>
  <div class="app-page">
    <el-row :gutter="12" class="metric-row">
      <el-col
        v-for="item in [
          { label: '运行中', value: metrics.running, type: 'primary' },
          { label: '账号', value: metrics.total, type: 'info' },
          { label: '下单数', value: metrics.orders, type: 'success' },
          { label: '异常', value: metrics.errors, type: 'danger' }
        ]"
        :key="item.label"
        :xs="12"
        :sm="6"
      >
        <el-card shadow="never" class="metric-card">
          <span :class="['metric-value', `is-${item.type}`]">{{
            item.value
          }}</span>
          <small>{{ item.label }}</small>
        </el-card>
      </el-col>
    </el-row>

    <el-row :gutter="12" class="metric-row">
      <el-col v-for="item in quickLinks" :key="item.path" :xs="12" :sm="6">
        <el-card
          shadow="hover"
          class="metric-card quick-card"
          @click="$router.push(item.path)"
        >
          <strong>{{ item.title }}</strong>
          <small>{{ item.desc }}</small>
        </el-card>
      </el-col>
    </el-row>

    <el-card shadow="never" class="app-card app-card-fill">
      <template #header>
        <div class="card-header">
          <span>账号状态</span>
          <div class="header-tools">
            <el-input
              v-model="keyword"
              clearable
              placeholder="搜索账号 / ID / 型号 / 门店"
            />
            <el-select v-model="statusFilter" style="width: 140px">
              <el-option label="全部状态" value="all" />
              <el-option label="运行中" value="running" />
              <el-option label="已停止" value="stopped" />
              <el-option label="异常" value="error" />
              <el-option label="已下单" value="ordered" />
              <el-option label="未登录" value="no-cookie" />
            </el-select>
          </div>
        </div>
      </template>
      <el-table v-loading="loading" :data="filteredAccounts" height="100%">
        <el-table-column
          prop="username"
          label="账号"
          min-width="180"
          show-overflow-tooltip
        />
        <el-table-column label="状态" width="110">
          <template #default="{ row }">
            <el-tag
              :type="
                row.running
                  ? 'success'
                  : row.status === 'error'
                    ? 'danger'
                    : 'info'
              "
              >{{ statusText(row.status) }}</el-tag
            >
          </template>
        </el-table-column>
        <el-table-column
          prop="id"
          label="ID"
          width="130"
          show-overflow-tooltip
        />
        <el-table-column label="型号" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">{{
            partText(row.parts, row.part)
          }}</template>
        </el-table-column>
        <el-table-column label="门店" min-width="200" show-overflow-tooltip>
          <template #default="{ row }">{{ storeIds(row) }}</template>
        </el-table-column>
        <el-table-column label="手机号" min-width="130" show-overflow-tooltip>
          <template #default="{ row }">{{ row.phone || "-" }}</template>
        </el-table-column>
        <el-table-column label="会话" min-width="130">
          <template #default="{ row }"
            >{{ row.wechat_session_fresh_count || 0 }}/{{
              row.wechat_session_count || 0
            }}{{
              row.wechat_session_min_remaining_ms
                ? ` · ${formatDuration(row.wechat_session_min_remaining_ms)}`
                : ""
            }}</template
          >
        </el-table-column>
        <el-table-column label="Cookie" width="100">
          <template #default="{ row }">{{
            cookieStatusText(row.wechat_cookie_status)
          }}</template>
        </el-table-column>
        <el-table-column prop="last_latency_ms" label="延迟" width="90" />
        <el-table-column prop="orders_placed" label="订单" width="80" />
        <el-table-column label="代理" width="80">
          <template #default="{ row }">{{
            row.proxy ? "代理" : "直连"
          }}</template>
        </el-table-column>
        <template #empty>暂无账号数据</template>
      </el-table>
    </el-card>

    <el-card shadow="never" class="app-card">
      <template #header>
        <div class="card-header">
          <span>下单成功账号</span>
          <el-input
            v-model="orderKeyword"
            clearable
            placeholder="搜索账号 / 订单号 / 型号 / 门店"
            style="width: 260px"
          />
        </div>
      </template>
      <el-collapse v-model="openGroups">
        <el-collapse-item
          v-for="group in orderGroups"
          :key="group.username"
          :name="group.username"
        >
          <template #title>
            <div class="collapse-title">
              <strong>{{ group.username }}</strong>
              <span>{{ group.orders.length }} 单</span>
              <small
                >最新订单：{{ group.orders[0]?.order_number || "-" }}</small
              >
            </div>
          </template>
          <el-table :data="group.orders" size="small">
            <el-table-column label="订单号" min-width="150">
              <template #default="{ row }">{{
                row.order_number || "-"
              }}</template>
            </el-table-column>
            <el-table-column label="型号" min-width="180">
              <template #default="{ row }">{{ partLabel(row.part) }}</template>
            </el-table-column>
            <el-table-column label="门店" min-width="180">
              <template #default="{ row }">{{
                storeName(row.store_id)
              }}</template>
            </el-table-column>
            <el-table-column label="下单时间" width="170">
              <template #default="{ row }">{{
                formatTime(row.created_at_ms)
              }}</template>
            </el-table-column>
            <el-table-column label="取货" width="160">
              <template #default="{ row }"
                >{{ row.pickup_date || "-" }}
                {{ row.pickup_time || "-" }}</template
              >
            </el-table-column>
            <el-table-column label="耗时" width="90">
              <template #default="{ row }"
                >{{ (row.total_ms / 1000).toFixed(3) }}s</template
              >
            </el-table-column>
            <el-table-column label="支付" width="120">
              <template #default="{ row }">
                <el-link
                  v-if="row.payment_link"
                  :href="row.payment_link"
                  target="_blank"
                  type="primary"
                  >打开支付</el-link
                >
                <span v-else>暂无</span>
              </template>
            </el-table-column>
          </el-table>
        </el-collapse-item>
      </el-collapse>
      <el-empty v-if="!orderGroups.length" description="暂无成功订单" />
    </el-card>
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.metric-row {
  margin-bottom: 12px;
}
.metric-card {
  text-align: left;
  cursor: default;
}
.quick-card {
  cursor: pointer;
}
.quick-card strong {
  display: block;
  font-size: 15px;
  margin-bottom: 4px;
}
.quick-card small {
  color: var(--el-text-color-secondary);
}
.metric-value {
  display: block;
  font-size: 26px;
  font-weight: 650;
}
.metric-value.is-primary {
  color: #409eff;
}
.metric-value.is-success {
  color: #34c759;
}
.metric-value.is-danger {
  color: #f56c6c;
}
.metric-value.is-info {
  color: #909399;
}
.app-card {
  margin-bottom: 12px;
}
.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.header-tools {
  display: flex;
  gap: 8px;
}
.collapse-title {
  display: flex;
  gap: 12px;
  align-items: center;
}
</style>
