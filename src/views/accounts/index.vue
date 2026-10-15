<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import { api, type AccountStatus, type PhoneModel } from "@/api/client";
import { useRealtime } from "@/composables/useRealtime";
import { cookieStatusText, modelLabel, statusText } from "@/utils/apple";

defineOptions({ name: "Accounts" });

const router = useRouter();
const accounts = ref<AccountStatus[]>([]);
const models = ref<PhoneModel[]>([]);
const selectedIds = ref<string[]>([]);
const keyword = ref("");
const loading = ref(false);
const result = ref("");

const filteredAccounts = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  if (!key) return accounts.value;
  return accounts.value.filter(account =>
    [account.username, account.id, ...account.parts, ...account.stores].some(
      value =>
        String(value || "")
          .toLowerCase()
          .includes(key)
    )
  );
});

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

async function load() {
  loading.value = true;
  try {
    const [state, modelList] = await Promise.all([api.state(), api.models()]);
    accounts.value = state.accounts || [];
    models.value = modelList || [];
    selectedIds.value = selectedIds.value.filter(id =>
      accounts.value.some(account => account.id === id)
    );
  } finally {
    loading.value = false;
  }
}

function editAccount(id: string) {
  router.push({ path: "/config", query: { id } });
}

async function prepareSelected() {
  const ids = selectedIds.value;
  await ElMessageBox.confirm(
    ids.length
      ? `将按选中的 ${ids.length} 个账号准备会话。`
      : "将按全部启用且已配置小程序 Cookie 的账号准备会话。",
    "确认操作"
  );
  const response = await api.wechatPrepareBatch({ account_ids: ids });
  result.value = `批量准备已开始：${response.accounts || 0} 个账号 / ${response.sessions || 0} 个组合。`;
  ElMessage.success(result.value);
}

async function deleteSelected() {
  if (!selectedIds.value.length)
    return ElMessage.warning("请先勾选要删除的账号");
  await ElMessageBox.confirm(
    `确认删除 ${selectedIds.value.length} 个账号？此操作不可恢复。`,
    "确认操作",
    { type: "warning" }
  );
  await api.deleteAccounts(selectedIds.value);
  ElMessage.success("账号已删除");
  selectedIds.value = [];
  await load();
}

async function deleteOne(id: string) {
  await ElMessageBox.confirm("确认删除这个账号？", "确认操作", {
    type: "warning"
  });
  await api.deleteAccounts([id]);
  selectedIds.value = selectedIds.value.filter(item => item !== id);
  await load();
}

let pollTimer: ReturnType<typeof setInterval> | null = null;

const { state: wsState, connected: wsConnected } = useRealtime();
watch(wsState, (newState: any) => {
  if (newState && newState.accounts) {
    accounts.value = newState.accounts;
    loading.value = false;
  }
}, { immediate: true });

onMounted(async () => {
  await load();
  pollTimer = setInterval(() => {
    if (document.visibilityState === 'visible') load();
  }, 5000);
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
          <span>账号管理</span>
          <div class="header-tools">
            <el-input
              v-model="keyword"
              clearable
              placeholder="搜索账号 / ID / 型号 / 门店"
            />
            <el-button
              @click="selectedIds = filteredAccounts.map(item => item.id)"
              >全选</el-button
            >
            <el-button
              @click="
                selectedIds = filteredAccounts
                  .filter(item => !selectedIds.includes(item.id))
                  .map(item => item.id)
              "
              >反选</el-button
            >
            <el-button type="primary" @click="prepareSelected"
              >批量准备会话</el-button
            >
            <el-button type="danger" @click="deleteSelected"
              >删除勾选</el-button
            >
          </div>
        </div>
      </template>
      <el-skeleton v-if="loading && !accounts.length" :rows="5" animated style="padding: 20px" />
      <el-table v-else
        v-loading="loading"
        :data="filteredAccounts"
        height="100%"
        @selection-change="rows => (selectedIds = rows.map(row => row.id))"
      >
        <el-table-column type="selection" width="48" />
        <el-table-column
          prop="username"
          label="账号"
          min-width="180"
          show-overflow-tooltip
        />
        <el-table-column
          prop="id"
          label="ID"
          width="120"
          show-overflow-tooltip
        />
        <el-table-column label="型号" min-width="170" show-overflow-tooltip>
          <template #default="{ row }">{{
            partText(row.parts, row.part)
          }}</template>
        </el-table-column>
        <el-table-column label="门店" min-width="150" show-overflow-tooltip>
          <template #default="{ row }">{{
            (row.stores && row.stores.length
              ? row.stores
              : [row.store_id]
            ).join(" / ") || "-"
          }}</template>
        </el-table-column>
        <el-table-column label="手机号" min-width="130" show-overflow-tooltip>
          <template #default="{ row }">{{ row.phone || "-" }}</template>
        </el-table-column>
        <el-table-column label="登录" width="95">
          <template #default="{ row }">{{
            row.cookie_count ? `Cookie ${row.cookie_count}` : "未登录"
          }}</template>
        </el-table-column>
        <el-table-column label="小程序 Cookie" width="105">
          <template #default="{ row }">{{
            cookieStatusText(row.wechat_cookie_status)
          }}</template>
        </el-table-column>
        <el-table-column label="状态" width="90">
          <template #default="{ row }"
            ><el-tag
              :type="
                row.running
                  ? 'success'
                  : row.status === 'error'
                    ? 'danger'
                    : 'info'
              "
              >{{ statusText(row.status) }}</el-tag
            ></template
          >
        </el-table-column>
        <el-table-column label="操作" width="170" fixed="right">
          <template #default="{ row }">
            <el-button
              size="small"
              type="primary"
              plain
              @click="editAccount(row.id)"
              >配置</el-button
            >
            <el-button
              size="small"
              type="danger"
              plain
              @click="deleteOne(row.id)"
              >删除</el-button
            >
          </template>
        </el-table-column>
        <template #empty>暂无账号</template>
      </el-table>
    </el-card>
    <el-alert
      v-if="result"
      :title="result"
      type="success"
      show-icon
      :closable="false"
      class="result-alert"
    />
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}
.header-tools {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.result-alert {
  margin-top: 12px;
}
</style>



