<!DOCTYPE html>
<template>
  <el-config-provider :locale="currentLocale">
    <div v-if="!isOnline" class="offline-banner">
      ⚠ 后端服务连接中断，正在自动重试...
    </div>
    <router-view />
  </el-config-provider>
</template>

<script lang="ts">
import { defineComponent, onMounted, onUnmounted, ref } from "vue";
import { ElConfigProvider } from "element-plus";
import { initRealtime } from "@/composables/useRealtime";
import zhCn from "element-plus/es/locale/lang/zh-cn";

export default defineComponent({
  name: "App",
  components: { ElConfigProvider },
  setup() {
    const currentLocale = zhCn;
    const isOnline = ref(true);
    let checkTimer: ReturnType<typeof setInterval> | null = null;

    onMounted(() => {
    initRealtime();
      checkTimer = setInterval(async () => {
        try {
          const response = await fetch("/api/state", { signal: AbortSignal.timeout(3000) });
          isOnline.value = response.ok;
        } catch {
          isOnline.value = false;
        }
      }, 10000);
    });

    onUnmounted(() => {
      if (checkTimer) clearInterval(checkTimer);
    });

    return { currentLocale, isOnline };
  }
});
</script>

<style>
.offline-banner {
  position: fixed;
  top: 0;
  left: 0;
  right: 0;
  z-index: 9999;
  padding: 6px 16px;
  background: #f56c6c;
  color: #fff;
  font-size: 13px;
  text-align: center;
}
</style>

