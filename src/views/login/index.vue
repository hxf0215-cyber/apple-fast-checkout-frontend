<script setup lang="ts">
import { reactive, ref } from "vue";
import { useRouter } from "vue-router";
import { ElMessage } from "element-plus";
import { useUserStoreHook } from "@/store/modules/user";
import { initRouter } from "@/router/utils";

defineOptions({ name: "Login" });

const router = useRouter();
const loading = ref(false);
const form = reactive({ username: "admin", password: "admin123" });

const onLogin = async () => {
  if (!form.username || !form.password) {
    ElMessage.warning("请输入用户名和密码");
    return;
  }
  loading.value = true;
  try {
    const res = await useUserStoreHook().loginByUsername({
      username: form.username,
      password: form.password
    });
    if (res?.success) {
      ElMessage.success("登录成功，正在进入控制台");
      await initRouter();
      router.push("/");
    } else {
      ElMessage.error("登录失败，请稍后重试");
    }
  } catch {
    ElMessage.error("登录失败，请稍后重试");
  } finally {
    loading.value = false;
  }
};
</script>

<template>
  <div class="login-wrapper">
    <div class="login-box">
      <div class="login-title">Apple Fast Checkout</div>
      <div class="login-sub">本地管理控制台</div>
      <el-form @submit.prevent="onLogin">
        <el-form-item
          ><el-input v-model="form.username" placeholder="用户名"
        /></el-form-item>
        <el-form-item
          ><el-input
            v-model="form.password"
            type="password"
            placeholder="密码"
            show-password
        /></el-form-item>
        <el-button
          class="login-button"
          type="primary"
          native-type="submit"
          :loading="loading"
          >进入控制台</el-button
        >
      </el-form>
    </div>
  </div>
</template>

<style scoped>
.login-wrapper {
  display: grid;
  place-items: center;
  min-height: 100vh;
  background: #f5f7fb;
}
.login-box {
  width: 340px;
  padding: 28px;
  background: #fff;
  border-radius: 12px;
  box-shadow: 0 12px 40px rgba(0, 21, 41, 0.08);
}
.login-title {
  font-size: 20px;
  font-weight: 650;
}
.login-sub {
  margin: 4px 0 20px;
  color: #6b7280;
  font-size: 13px;
}
.login-button {
  width: 100%;
}
</style>
