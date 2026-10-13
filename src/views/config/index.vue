<script setup lang="ts">
import { computed, onMounted, reactive, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { ElMessage, ElMessageBox } from "element-plus";
import {
  api,
  type AccountConfig,
  type PhoneModel,
  type StoreInfo,
  type StockResult
} from "@/api/client";
import { modelLabel, parseCookieText } from "@/utils/apple";

defineOptions({ name: "Config" });

const route = useRoute();
const router = useRouter();
const models = ref<PhoneModel[]>([]);
const stores = ref<StoreInfo[]>([]);
const storeCache = ref<StockResult[]>([]);
const selectedStores = ref<string[]>([]);
const editingId = ref<string | null>(null);
const saving = ref(false);
const loginSending = ref(false);
const loginVerifying = ref(false);
const loginStatus = ref("");
let loginId: string | null = sessionStorage.getItem("apple_login_id") || null;

const form = reactive({
  id: "",
  username: "",
  password: "",
  proxyUrl: "",
  proxyUrls: "",
  wechatCookie: "",
  province: "重庆",
  city: "重庆",
  district: "",
  quantity: 1,
  accountDelay: "",
  lastName: "",
  firstName: "",
  email: "",
  phone: "",
  idSuffix: "",
  loginCookies: "",
  parts: [] as string[],
  manualStoreIds: ""
});

const loginForm = reactive({ account: "", password: "", code: "" });

const selectedParts = computed(() => form.parts.filter(Boolean));
const selectedStoreIds = computed(() => {
  const manual = form.manualStoreIds
    .split(/[,，、\s]+/)
    .map(value => value.trim().toUpperCase())
    .filter(Boolean);
  return [...new Set([...selectedStores.value, ...manual])];
});

function buildAccount(): AccountConfig {
  const parts = selectedParts.value;
  const resultStores = selectedStoreIds.value;
  if (!parts.length) throw new Error("请至少选择一个手机型号");
  if (!resultStores.length) throw new Error("请至少选择或填写一个门店 ID");
  return {
    id: form.id.trim() || `acc-${Date.now()}`,
    username: form.username.trim(),
    password: form.password,
    proxy_url: form.proxyUrl.trim(),
    proxy_urls: form.proxyUrls
      .split(/\r?\n/)
      .map(value => value.trim())
      .filter(Boolean),
    wechat_cookie: form.wechatCookie.trim(),
    part: parts[0],
    store_id: resultStores[0],
    parts,
    stores: resultStores,
    province: form.province.trim() || "重庆",
    city: form.city.trim() || "重庆",
    district: form.district.trim(),
    quantity: Number(form.quantity || 1),
    contact: {
      last_name: form.lastName.trim(),
      first_name: form.firstName.trim(),
      email: form.email.trim(),
      phone: form.phone.trim(),
      national_id_suffix: form.idSuffix.trim()
    },
    poll_delay_ms: form.accountDelay ? Number(form.accountDelay) : null,
    login_cookies: parseCookieText(form.loginCookies),
    enabled: true
  };
}

function fillForm(account: AccountConfig) {
  editingId.value = account.id;
  form.id = account.id;
  form.username = account.username;
  form.password = account.password || "";
  form.proxyUrl = account.proxy_url || "";
  form.proxyUrls = (account.proxy_urls || []).join("\n");
  form.wechatCookie = account.wechat_cookie || "";
  form.province = account.province || "重庆";
  form.city = account.city || "重庆";
  form.district = account.district || "";
  form.quantity = account.quantity || 1;
  form.accountDelay = account.poll_delay_ms?.toString() || "";
  form.lastName = account.contact?.last_name || "";
  form.firstName = account.contact?.first_name || "";
  form.email = account.contact?.email || "";
  form.phone = account.contact?.phone || "";
  form.idSuffix = account.contact?.national_id_suffix || "";
  form.loginCookies = Object.entries(account.login_cookies || {})
    .map(([key, value]) => `${key}=${value}`)
    .join("; ");
  form.parts = account.parts?.length ? account.parts : [account.part];
  selectedStores.value = account.stores?.length
    ? account.stores
    : [account.store_id];
  storeCache.value = selectedStores.value.map(store => ({
    store_id: store,
    status: "-",
    name: "已保存门店",
    city: "-"
  }));
}

async function saveAccount(startAfterSave = false) {
  try {
    const account = buildAccount();
    if (!account.username) throw new Error("请填写 Apple ID");
    saving.value = true;
    if (editingId.value) await api.updateAccount(editingId.value, account);
    else await api.saveAccount(account);
    if (startAfterSave) await api.startAccount(account.id);
    ElMessage.success("账号已保存");
    editingId.value = account.id;
  } catch (error: any) {
    ElMessage.error(error?.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

async function applyToAll() {
  try {
    const account = buildAccount();
    await ElMessageBox.confirm(
      `将把当前型号、门店、代理、地区、数量和轮询延迟应用到全部账号。不会修改账号 ID、Apple ID、密码、Cookie 和联系人。`,
      "确认操作"
    );
    const result = await api.applyAccountBulk({
      parts: account.parts,
      stores: account.stores,
      proxy_url: account.proxy_url,
      proxy_urls: account.proxy_urls,
      province: account.province,
      city: account.city,
      district: account.district,
      quantity: account.quantity,
      poll_delay_ms: account.poll_delay_ms
    });
    ElMessage.success(`统一配置完成：成功 ${result.updated || 0} 个账号`);
  } catch (error: any) {
    if (error !== "cancel") ElMessage.error(error?.message || "统一配置失败");
  }
}

async function loadStores() {
  if (!editingId.value) return ElMessage.warning("请先保存账号，再拉取门店");
  if (!selectedParts.value.length) return ElMessage.warning("请先选择手机型号");
  const result = await api.queryStock(editingId.value, {
    part: selectedParts.value[0]
  });
  storeCache.value = Array.isArray(result) ? result : [];
  ElMessage.success(`已获取 ${storeCache.value.length} 家门店`);
}

async function sendCode() {
  if (!loginForm.account || !loginForm.password)
    return ElMessage.warning("请填写 Apple ID 和密码");
  loginSending.value = true;
  loginStatus.value = "正在发送验证码…";
  try {
    const timeout = new Promise((_, reject) =>
      setTimeout(() => reject(new Error("发送验证码超时")), 95000)
    );
    const result = (await Promise.race([
      api.loginStart({
        account: loginForm.account,
        password: loginForm.password
      }),
      timeout
    ])) as any;
    if (result.ok && result.status === "need_2fa") {
      loginId = result.login_id;
      sessionStorage.setItem("apple_login_id", loginId || "");
      loginStatus.value = `验证码已发送，会话：${loginId}`;
    } else if (result.ok && result.status === "success") {
      loginStatus.value = "登录成功，Cookie 已保存。";
    } else {
      loginStatus.value = result.error || "发送验证码失败。";
    }
  } catch (error: any) {
    loginStatus.value = error?.message || "发送验证码失败";
  } finally {
    loginSending.value = false;
  }
}

async function verifyCode() {
  if (!loginId) return ElMessage.warning("请先点击“发送验证码”");
  if (!/^\d{6}$/.test(loginForm.code))
    return ElMessage.warning("请输入 6 位数字验证码");
  loginVerifying.value = true;
  loginStatus.value = "正在验证…";
  try {
    const timeout = new Promise((_, reject) =>
      setTimeout(() => reject(new Error("验证超时")), 95000)
    );
    const result = (await Promise.race([
      api.loginVerify({ login_id: loginId, code: loginForm.code }),
      timeout
    ])) as any;
    if (result.ok) {
      loginId = null;
      sessionStorage.removeItem("apple_login_id");
      loginStatus.value = `登录成功，已保存 ${result.cookie_count ?? "多"} 个 Cookie`;
      if (editingId.value) fillForm(await api.getAccount(editingId.value));
    } else if (String(result.error || "").includes("login session not found")) {
      loginId = null;
      sessionStorage.removeItem("apple_login_id");
      loginStatus.value = "登录会话已失效，请重新发送验证码。";
    } else {
      loginStatus.value = result.error || "验证码错误。";
    }
  } catch (error: any) {
    loginStatus.value = error?.message || "验证失败";
  } finally {
    loginVerifying.value = false;
  }
}

function resetForm() {
  editingId.value = null;
  Object.assign(form, {
    id: "",
    username: "",
    password: "",
    proxyUrl: "",
    proxyUrls: "",
    wechatCookie: "",
    province: "重庆",
    city: "重庆",
    district: "",
    quantity: 1,
    accountDelay: "",
    lastName: "",
    firstName: "",
    email: "",
    phone: "",
    idSuffix: "",
    loginCookies: "",
    parts: [],
    manualStoreIds: ""
  });
  selectedStores.value = [];
  storeCache.value = [];
}

onMounted(async () => {
  [models.value, stores.value] = [
    (await api.models()) || [],
    (await api.stores()) || []
  ];
  const part = route.query.part as string | undefined;
  if (part) form.parts = [part];
  const id = route.query.id as string | undefined;
  if (id) {
    try {
      fillForm(await api.getAccount(id));
    } catch {
      resetForm();
    }
  }
});
</script>

<template>
  <div class="app-page">
    <el-card shadow="never" class="app-card">
      <template #header><span>Apple ID 登录</span></template>
      <el-form label-width="110px">
        <el-row :gutter="12">
          <el-col :span="12"
            ><el-form-item label="Apple ID"
              ><el-input
                v-model="loginForm.account"
                placeholder="user@example.com" /></el-form-item
          ></el-col>
          <el-col :span="12"
            ><el-form-item label="密码"
              ><el-input
                v-model="loginForm.password"
                type="password"
                show-password /></el-form-item
          ></el-col>
        </el-row>
        <el-form-item label="验证码">
          <div class="inline-form">
            <el-button :loading="loginSending" @click="sendCode"
              >发送验证码</el-button
            >
            <el-input
              v-model="loginForm.code"
              maxlength="6"
              placeholder="123456"
              style="width: 140px"
            />
            <el-button :loading="loginVerifying" @click="verifyCode"
              >登录</el-button
            >
          </div>
        </el-form-item>
      </el-form>
      <el-alert
        v-if="loginStatus"
        :title="loginStatus"
        :closable="false"
        show-icon
      />
    </el-card>

    <el-card shadow="never" class="app-card app-card-fill">
      <template #header
        ><span>{{
          editingId ? `编辑账号：${form.username}` : "账号配置"
        }}</span></template
      >
      <el-form label-width="150px">
        <el-row :gutter="12">
          <el-col :span="8"
            ><el-form-item label="账号 ID"
              ><el-input v-model="form.id" placeholder="acc-01" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="Apple ID"
              ><el-input v-model="form.username" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="密码"
              ><el-input
                v-model="form.password"
                type="password"
                show-password /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="独立代理"
              ><el-input v-model="form.proxyUrl" /></el-form-item
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
          <el-col :span="8"
            ><el-form-item label="购买数量"
              ><el-input-number
                v-model="form.quantity"
                :min="1" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="账号轮询延迟 ms"
              ><el-input
                v-model="form.accountDelay"
                placeholder="留空使用全局" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="姓"
              ><el-input v-model="form.lastName" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="名"
              ><el-input v-model="form.firstName" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="邮箱"
              ><el-input v-model="form.email" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="手机"
              ><el-input v-model="form.phone" /></el-form-item
          ></el-col>
          <el-col :span="8"
            ><el-form-item label="身份证后四位"
              ><el-input v-model="form.idSuffix" maxlength="4" /></el-form-item
          ></el-col>
          <el-col :span="24"
            ><el-form-item label="代理池"
              ><el-input
                v-model="form.proxyUrls"
                type="textarea"
                :rows="3" /></el-form-item
          ></el-col>
          <el-col :span="24"
            ><el-form-item label="微信小程序 Cookie"
              ><el-input
                v-model="form.wechatCookie"
                type="textarea"
                :rows="3" /></el-form-item
          ></el-col>
          <el-col :span="24"
            ><el-form-item label="登录 Cookie"
              ><el-input
                v-model="form.loginCookies"
                type="textarea"
                :rows="3" /></el-form-item
          ></el-col>
        </el-row>
        <el-form-item label="监控型号">
          <el-select
            v-model="form.parts"
            multiple
            filterable
            placeholder="请选择型号"
            class="full-width"
          >
            <el-option
              v-for="model in models"
              :key="model.part"
              :value="model.part"
              :label="`${modelLabel(model)} · ${model.part}`"
            />
          </el-select>
        </el-form-item>
        <el-form-item label="监控门店">
          <div class="store-options">
            <el-checkbox-group v-model="selectedStores">
              <el-checkbox
                v-for="store in storeCache"
                :key="store.store_id"
                :value="store.store_id"
              >
                {{ store.store_id }} · {{ store.name || "-" }} ·
                {{ store.city || "-" }}
              </el-checkbox>
            </el-checkbox-group>
            <el-empty
              v-if="!storeCache.length"
              description="暂无门店数据，保存账号后点击拉取门店"
              :image-size="60"
            />
          </div>
        </el-form-item>
        <el-form-item label="手动添加门店"
          ><el-input v-model="form.manualStoreIds" placeholder="R480, R476"
        /></el-form-item>
        <el-form-item>
          <el-button
            type="primary"
            :loading="saving"
            @click="saveAccount(false)"
            >保存配置</el-button
          >
          <el-button :loading="saving" @click="saveAccount(true)"
            >保存并启动监控</el-button
          >
          <el-button @click="applyToAll">统一应用到全部账号</el-button>
          <el-button @click="loadStores">拉取门店</el-button>
          <el-button @click="resetForm">清空</el-button>
          <el-button @click="router.push('/accounts')">返回账号管理</el-button>
        </el-form-item>
      </el-form>
    </el-card>
    <div class="app-spacer" />
  </div>
</template>

<style scoped>
.app-card {
  margin-bottom: 12px;
}
.inline-form {
  display: flex;
  gap: 8px;
}
.full-width {
  width: 100%;
}
.store-options {
  max-height: 220px;
  overflow: auto;
  width: 100%;
}
</style>
