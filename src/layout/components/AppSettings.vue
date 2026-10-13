<script setup lang="ts">
import { reactive, ref, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { api, type GlobalSettings } from "@/api/client";

defineOptions({ name: "AppSettings" });

const visible = defineModel<boolean>("visible", { default: false });
const saving = ref(false);
const acting = ref(false);
const form = reactive({
  pollDelayMs: 500,
  startDelayMs: 0,
  jitterMs: 150,
  httpTimeoutMs: 8000,
  stopAfterSuccess: false
});
let currentSettings: GlobalSettings | null = null;

async function load() {
  const state = await api.state();
  currentSettings = state.settings;
  form.pollDelayMs = state.settings.poll_delay_ms ?? 500;
  form.startDelayMs = state.settings.start_delay_ms ?? 0;
  form.jitterMs = state.settings.jitter_ms ?? 0;
  form.httpTimeoutMs = state.settings.http_timeout_ms ?? 8000;
  form.stopAfterSuccess = state.settings.stop_after_success ?? false;
}

let syncTimer: ReturnType<typeof setInterval> | null = null;
watch(visible, async v => {
  if (v) {
    try {
      await load();
    } catch (error: any) {
      ElMessage.error(error?.message || "加载设置失败");
    }
  }
});
syncTimer = setInterval(async () => {
  if (!visible.value) return;
  try {
    await load();
  } catch {}
}, 3000);

async function saveSettings() {
  if (!currentSettings) return;
  saving.value = true;
  try {
    await api.saveSettings({
      ...currentSettings,
      poll_delay_ms: form.pollDelayMs,
      start_delay_ms: form.startDelayMs,
      jitter_ms: form.jitterMs,
      http_timeout_ms: form.httpTimeoutMs,
      stop_after_success: form.stopAfterSuccess
    });
    ElMessage.success("设置已保存");
  } catch (error: any) {
    ElMessage.error(error?.message || "保存失败");
  } finally {
    saving.value = false;
  }
}

async function startAll() {
  acting.value = true;
  try {
    await api.start();
    ElMessage.success("中央监控已启动");
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
  } catch (error: any) {
    if (!["cancel", "close"].includes(error)) {
      ElMessage.error(error?.message || "操作失败");
    }
  }
}
</script>

<template>
  <el-drawer v-model="visible" title="全局设置" size="380">
    <div class="section">
      <div class="section-title">全局操作</div>
      <div class="global-actions">
        <el-button type="primary" :loading="acting" @click="startAll"
          >启动中央监控</el-button
        >
        <el-button type="danger" :loading="acting" @click="stopAll"
          >停止监控</el-button
        >
      </div>
      <div class="global-actions">
        <el-button :loading="acting" @click="clearCompleted"
          >清空已完成状态</el-button
        >
      </div>
      <div class="section-hint">
        监控与下单需要在「监控与下单」页配置好型号、门店范围后启动。
      </div>
    </div>

    <el-divider />

    <div class="section">
      <div class="section-title">全局延迟设置</div>
      <el-form label-width="130px">
        <el-form-item label="轮询延迟 ms">
          <el-input-number v-model="form.pollDelayMs" :min="0" :max="60000" />
        </el-form-item>
        <el-form-item label="启动延迟 ms">
          <el-input-number v-model="form.startDelayMs" :min="0" :max="60000" />
        </el-form-item>
        <el-form-item label="随机抖动 ms">
          <el-input-number v-model="form.jitterMs" :min="0" :max="60000" />
        </el-form-item>
        <el-form-item label="HTTP 超时 ms">
          <el-input-number
            v-model="form.httpTimeoutMs"
            :min="500"
            :max="120000"
          />
        </el-form-item>
        <el-form-item label="成功后停止账号">
          <el-switch v-model="form.stopAfterSuccess" />
        </el-form-item>
      </el-form>
    </div>

    <template #footer>
      <el-button @click="visible = false">取消</el-button>
      <el-button type="primary" :loading="saving" @click="saveSettings"
        >保存设置</el-button
      >
    </template>
  </el-drawer>
</template>

<style scoped>
.section {
  margin-bottom: 8px;
}
.section-title {
  font-weight: 600;
  font-size: 14px;
  margin-bottom: 12px;
}
.section-hint {
  margin-top: 10px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.global-actions {
  display: flex;
  gap: 8px;
  margin-bottom: 8px;
}
</style>
