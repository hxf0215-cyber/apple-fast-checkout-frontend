import type { AxiosRequestConfig } from "axios";
import { stringify } from "qs";
import { getToken, formatToken } from "@/utils/auth";

export interface PureHttpRequestConfig extends AxiosRequestConfig {
  beforeRequestCallback?: (config: PureHttpRequestConfig) => void;
  beforeResponseCallback?: (response: any) => void;
}

export type RequestMethods =
  | "get"
  | "post"
  | "put"
  | "delete"
  | "head"
  | "options"
  | "patch";

/**
 * 基于 fetch 的轻量 HTTP 客户端（与旧 HTML 前端一致的请求方式）
 * 保持与之前 PureHttp 相同的公开 API：request / get / post
 */
class PureHttp {
  private static whiteList = ["/refresh-token", "/login"];

  private static buildHeaders(
    config: PureHttpRequestConfig
  ): Record<string, string> {
    const headers: Record<string, string> = {
      Accept: "application/json, text/plain, */*",
      "Content-Type": "application/json",
      "X-Requested-With": "XMLHttpRequest",
      ...((config.headers as Record<string, string>) || {})
    };

    const url = config.url || "";
    if (!PureHttp.whiteList.some(item => url.endsWith(item))) {
      const data = getToken();
      if (data) {
        const now = Date.now();
        const expired = Number(data.expires) - now <= 0;
        if (!expired && data.accessToken) {
          headers["Authorization"] = formatToken(data.accessToken);
        }
      }
    }
    return headers;
  }

  public async request<T>(
    method: RequestMethods,
    url: string,
    param?: AxiosRequestConfig,
    axiosConfig?: PureHttpRequestConfig
  ): Promise<T> {
    const config: PureHttpRequestConfig = {
      method,
      url,
      ...param,
      ...axiosConfig
    };

    if (typeof config.beforeRequestCallback === "function") {
      config.beforeRequestCallback(config);
    }

    const headers = PureHttp.buildHeaders(config);
    const methodUpper = method.toUpperCase();
    let fetchUrl = url;

    // GET 请求把 params 拼到 URL
    if (config.params) {
      const qs = stringify(config.params as Record<string, any>);
      if (qs) fetchUrl += (fetchUrl.includes("?") ? "&" : "?") + qs;
    }

    const fetchOptions: RequestInit = {
      method: methodUpper,
      headers
    };

    if (
      methodUpper !== "GET" &&
      methodUpper !== "HEAD" &&
      config.data !== undefined &&
      config.data !== null
    ) {
      fetchOptions.body =
        typeof config.data === "string"
          ? config.data
          : JSON.stringify(config.data);
    }

    const response = await fetch(fetchUrl, fetchOptions);

    if (!response.ok && response.status >= 400) {
      const error: any = new Error(
        `HTTP ${response.status}: ${response.statusText}`
      );
      error.isCancelRequest = false;
      error.response = {
        status: response.status,
        statusText: response.statusText
      };
      try {
        error.response.data = await response.json();
      } catch {
        error.response.data = null;
      }
      throw error;
    }

    const contentType = response.headers.get("content-type") || "";
    let result: any;
    if (contentType.includes("application/json")) {
      result = await response.json();
    } else {
      result = await response.text();
    }

    if (typeof config.beforeResponseCallback === "function") {
      config.beforeResponseCallback({ data: result, status: response.status });
    }

    return result as T;
  }

  public post<_P = any, T = any>(
    url: string,
    params?: AxiosRequestConfig<any>,
    config?: PureHttpRequestConfig
  ): Promise<T> {
    return this.request<T>("post", url, params, config);
  }

  public get<_P = any, T = any>(
    url: string,
    params?: AxiosRequestConfig<any>,
    config?: PureHttpRequestConfig
  ): Promise<T> {
    return this.request<T>("get", url, params, config);
  }
}

export const http = new PureHttp();
