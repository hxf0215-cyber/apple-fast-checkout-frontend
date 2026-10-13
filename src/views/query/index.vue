<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { ElMessage } from "element-plus";
import {
  api,
  type AccountStatus,
  type PhoneModel,
  type StockResult,
  type StoreInfo
} from "@/api/client";
import { stockStatusText } from "@/utils/apple";

defineOptions({ name: "Query" });

const accounts = ref<AccountStatus[]>([]);
const models = ref<PhoneModel[]>([]);
const stores = ref<StoreInfo[]>([]);
const stockResults = ref<StockResult[]>([]);
const selectedStores = ref<string[]>([]);
const accountId = ref("");
const part = ref("");
const province = ref("");
const loading = ref(false);

const provinces = computed(() => [
  ...new Set(stores.value.map(store => store.province))
]);
const visibleStores = computed(() =>
  province.value
    ? stores.value.filter(store => store.province === province.value)
    : stores.value
);

async function queryStock() {
  if (!accountId.value) return ElMessage.warning("请选择账号");
  if (!part.value) return ElMessage.warning("请选择手机型号");
  loading.value = true;
  try {
    const result = await api.queryStock(accountId.value, {
      part: part.value,
      stores: selectedStores.value,
      province: province.value,
      city: province.value,
      district: ""
    });
    stockResults.value = Array.isArray(result) ? result : [];
  } catch (error: any) {
    ElMessage.error(error?.message || "库存查询失败");
  } finally {
    loading.value = false;
  }
}

onMounted(async () => {
  const [state, modelList, storeList] = await Promise.all([
    api.state(),
    api.models(),
    api.stores()
  ]);
  accounts.value = state.accounts || [];
  models.value = modelList || [];
  stores.value = storeList || [];
  accountId.value = accounts.value[0]?.id || "";
  part.value = models.value[0]?.part || "";
});
</script>

<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card">
      <template #header><span>库存查询</span></template>
      <el-form label-width="110px">
        <el-row :gutter="12">
          <el-col :span="6"
            ><el-form-item label="账号 / 代理"
              ><el-select v-model="accountId" filterable
                ><el-option
                  v-for="account in accounts"
                  :key="account.id"
                  :value="account.id"
                  :label="account.username" /></el-select></el-form-item
          ></el-col>
          <el-col :span="6"
            ><el-form-item label="省份"
              ><el-select v-model="province" clearable
                ><el-option label="全国" value="" /><el-option
                  v-for="item in provinces"
                  :key="item"
                  :value="item"
                  :label="item" /></el-select></el-form-item
          ></el-col>
          <el-col :span="6"
            ><el-form-item label="手机型号"
              ><el-select v-model="part" filterable
                ><el-option
                  v-for="model in models"
                  :key="model.part"
                  :value="model.part"
                  :label="`${model.family} · ${model.capacity} · ${model.color} · ${model.part}`" /></el-select></el-form-item
          ></el-col>
          <el-col :span="6"
            ><el-form-item label="操作"
              ><el-button type="primary" :loading="loading" @click="queryStock"
                >查询门店库存</el-button
              ></el-form-item
            ></el-col
          >
        </el-row>
      </el-form>
    </el-card>

    <el-card shadow="never" class="app-card">
      <template #header>
        <div class="card-header">
          <span>门店筛选</span>
          <div>
            <el-button
              size="small"
              @click="
                selectedStores = visibleStores.map(store => store.store_id)
              "
              >全选</el-button
            >
            <el-button size="small" @click="selectedStores = []"
              >清空</el-button
            >
          </div>
        </div>
      </template>
      <el-select
        v-model="selectedStores"
        multiple
        filterable
        clearable
        class="full-width"
      >
        <el-option
          v-for="store in visibleStores"
          :key="store.store_id"
          :value="store.store_id"
          :label="`${store.store_id} · ${store.name} · ${store.city}`"
        />
      </el-select>
    </el-card>

    <el-card shadow="never" class="app-card app-card-fill">
      <template #header><span>库存结果</span></template>
      <el-table v-loading="loading" :data="stockResults" height="100%">
        <el-table-column label="状态" width="110">
          <template #default="{ row }"
            ><el-tag :type="row.status === 'available' ? 'success' : 'info'">{{
              stockStatusText(row.status)
            }}</el-tag></template
          >
        </el-table-column>
        <el-table-column label="门店" min-width="180">
          <template #default="{ row }"
            >{{ row.store_id || "-" }} · {{ row.name || "-" }}</template
          >
        </el-table-column>
        <el-table-column prop="city" label="城市" width="120" />
        <el-table-column prop="province" label="省份" width="120" />
        <el-table-column prop="quote" label="说明" min-width="180" />
        <template #empty>选择账号与型号后，点击查询</template>
      </el-table>
    </el-card>
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.app-card {
  margin-bottom: 12px;
}
.card-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.full-width {
  width: 100%;
}
</style>
