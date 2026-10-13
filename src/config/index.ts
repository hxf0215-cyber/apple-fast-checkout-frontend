import type { App } from "vue";

let config: object = {};
const { VITE_PUBLIC_PATH } = import.meta.env;

const setConfig = (cfg?: unknown) => {
  config = Object.assign(config, cfg);
};

const getConfig = (key?: string): PlatformConfigs => {
  if (typeof key === "string") {
    const arr = key.split(".");
    if (arr && arr.length) {
      let data = config;
      arr.forEach(v => {
        if (data && typeof data[v] !== "undefined") {
          data = data[v];
        } else {
          data = null;
        }
      });
      return data;
    }
  }
  return config;
};

/** 获取项目动态全局配置（fetch 加载，失败时使用默认配置继续启动） */
export const getPlatformConfig = async (app: App): Promise<any> => {
  app.config.globalProperties.$config = getConfig();
  let result: any = getConfig();
  try {
    const response = await fetch(`${VITE_PUBLIC_PATH}platform-config.json`);
    const configData = await response.json();
    let $config = app.config.globalProperties.$config;
    if (app && $config && typeof configData === "object") {
      $config = Object.assign($config, configData);
      app.config.globalProperties.$config = $config;
      setConfig($config);
      result = $config;
    }
  } catch {
    console.warn("platform-config.json 加载失败，使用默认配置");
  }
  return result;
};

/** 本地响应式存储的命名空间 */
const responsiveStorageNameSpace = () => getConfig().ResponsiveStorageNameSpace;

export { getConfig, setConfig, responsiveStorageNameSpace };
