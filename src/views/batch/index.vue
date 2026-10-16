<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { api, type PhoneModel, type StoreInfo } from "@/api/client";
import { modelLabel } from "@/utils/apple";

defineOptions({ name: "Batch" });

const router = useRouter();
const models = ref<PhoneModel[]>([]);
const stores = ref<StoreInfo[]>([]);
const activeTab = ref("accounts");
const importing = ref(false);
const result = ref("");
const form = reactive({
  parts: [] as string[],
  stores: [] as string[],
  proxy: "",
  province: "重庆",
  city: "重庆",
  district: "",
  accounts: "",
  cookies: "",
  lastName: "",
  firstName: "",
  email: "",
  phone: "",
  nationalId: "",
  createMissing: true
});

const provinces = computed(() => [
  ...new Set(stores.value.map(store => store.province))
]);

function settings() {
  if (!form.parts.length) throw new Error("请至少选择一个默认型号");
  if (!form.stores.length) throw new Error("请至少选择一个默认门店");
  return {
    parts: form.parts,
    stores: form.stores,
    proxy_url: form.proxy.trim(),
    province: form.province.trim() || "重庆",
    city: form.city.trim() || "重庆",
    district: form.district.trim()
  };
}

async function importAccounts() {
  try {
    importing.value = true;
    const response: any = await api.importAccounts({
      ...settings(),
      text: form.accounts
    });
    result.value = `导入完成：新增/更新 ${response?.imported ?? 0} 个账号`;
    ElMessage.success("导入完成，未自动启动监控");
    router.push("/accounts");
  } catch (error: any) {
    ElMessage.error(error?.message || "导入失败");
  } finally {
    importing.value = false;
  }
}

async function importCookies() {
  try {
    importing.value = true;
    const response = await api.importCookies({
      ...settings(),
      text: form.cookies,
      create_missing: form.createMissing,
      contact: {
        last_name: form.lastName.trim(),
        first_name: form.firstName.trim(),
        email: form.email.trim(),
        phone: form.phone.trim(),
        national_id_suffix: form.nationalId.trim()
      }
    });
    const success = response.updated + response.created;
    result.value = response.results
      .map((item: any) =>
        item.ok
          ? `${item.created ? "创建成功" : "更新成功"} · ${item.account}`
          : `失败 · ${item.error}`
      )
      .join("；");
    ElMessage.success(
      `导入完成：成功 ${success} 条，失败 ${response.failed} 条（未自动启动监控）`
    );
    if (success > 0) router.push("/accounts");
  } catch (error: any) {
    ElMessage.error(error?.message || "导入失败");
  } finally {
    importing.value = false;
  }
}

onMounted(async () => {
  try {
    models.value = (await api.models()) || [];
    stores.value = (await api.stores()) || [];
    const chongqing = stores.value.find(store => store.store_id === "R480");
    if (chongqing) form.stores = [chongqing.store_id];
    if (models.value.length) form.parts = [models.value[0].part];
  } catch (error: any) {
    console.error("batch init failed:", error);
  }
});
</script>

<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card app-card-fill">
      <template #header><span>批量导入</span></template>
      <el-form label-width="150px">
        <el-row :gutter="12">
          <el-col :span="12"
            ><el-form-item label="默认监控型号"
              ><el-select v-model="form.parts" multiple filterable
                ><el-option
                  v-for="model in models"
                  :key="model.part"
                  :value="model.part"
                  :label="modelLabel(model)" /></el-select></el-form-item
          ></el-col>
          <el-col :span="12"
            ><el-form-item label="全国门店"
              ><el-select v-model="form.stores" multiple filterable
                ><el-option-group
                  v-for="province in provinces"
                  :key="province"
                  :label="province"
                  ><el-option
                    v-for="store in stores.filter(
                      item => item.province === province
                    )"
                    :key="store.store_id"
                    :value="store.store_id"
                    :label="`${store.store_id} · ${store.name} · ${store.city}`" /></el-option-group></el-select></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="代理"
              ><el-input v-model="form.proxy" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="省"
              ><el-input v-model="form.province" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="市"
              ><el-input v-model="form.city" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="区 / 县"
              ><el-input v-model="form.district" /></el-form-item
          ></el-col>
        </el-row>
      </el-form>
      <el-tabs v-model="activeTab">
        <el-tab-pane label="批量导入账号" name="accounts">
          <el-input
            v-model="form.accounts"
            type="textarea"
            :rows="10"
            placeholder="user@example.com----小程序Cookie----配置JSON----备注"
          />
          <div class="toolbar">
            <el-button
              type="primary"
              :loading="importing"
              @click="importAccounts()"
              >导入账号</el-button
            >
          </div>
        </el-tab-pane>
        <el-tab-pane label="批量导入 Cookie" name="cookies">
          <el-input
            v-model="form.cookies"
            type="textarea"
            :rows="10"
            placeholder="user@example.com----小程序Cookie----配置JSON----备注"
          />
          <el-row :gutter="12">
            <el-col :span="8"
              ><el-input v-model="form.lastName" placeholder="姓"
            /></el-col>
            <el-col :span="8"
              ><el-input v-model="form.firstName" placeholder="名"
            /></el-col>
            <el-col :span="8"
              ><el-input v-model="form.email" placeholder="邮箱"
            /></el-col>
            <el-col :span="8"
              ><el-input v-model="form.phone" placeholder="手机"
            /></el-col>
            <el-col :span="8"
              ><el-input v-model="form.nationalId" placeholder="身份证后四位"
            /></el-col>
            <el-col :span="8"
              ><el-checkbox v-model="form.createMissing"
                >自动创建未存在的账号</el-checkbox
              ></el-col
            >
          </el-row>
          <div class="toolbar">
            <el-button
              type="primary"
              :loading="importing"
              @click="importCookies()"
              >导入 Cookie</el-button
            >
          </div>
          <el-alert
            v-if="result"
            :title="result"
            type="info"
            show-icon
            :closable="false"
          />
        </el-tab-pane>
      </el-tabs>
    </el-card>
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  gap: 8px;
  margin-top: 12px;
}
:deep(.el-textarea__inner) {
  font-family: Consolas, monospace;
}
</style>
