/** 所有路由都是静态路由，无需请求后端，直接返回空动态路由列表 */
type Result = {
  success: boolean;
  data: Array<any>;
};

export const getAsyncRoutes = () => {
  return Promise.resolve({
    success: true,
    data: []
  } satisfies Result);
};
