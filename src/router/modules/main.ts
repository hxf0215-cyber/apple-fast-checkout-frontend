export default {
  path: "",
  name: "Main",
  component: () => import("@/layout/index.vue"),
  redirect: "/overview",
  meta: {
    icon: "ep/home-filled",
    title: "Apple Fast Checkout",
    rank: 1,
    showParent: false
  },
  children: [
    {
      path: "/overview",
      name: "Overview",
      component: () => import("@/views/overview/index.vue"),
      meta: { title: "系统总览", showLink: true, showParent: false }
    },
    {
      path: "/monitor",
      name: "Monitor",
      component: () => import("@/views/monitor/index.vue"),
      meta: { title: "监控与下单", showLink: true, showParent: false }
    },
    {
      path: "/accounts",
      name: "Accounts",
      component: () => import("@/views/accounts/index.vue"),
      meta: { title: "账号管理", showLink: true, showParent: false }
    },
    {
      path: "/config",
      name: "Config",
      component: () => import("@/views/config/index.vue"),
      meta: { title: "账号配置", showLink: true, showParent: false }
    },
    {
      path: "/batch",
      name: "Batch",
      component: () => import("@/views/batch/index.vue"),
      meta: { title: "批量导入", showLink: true, showParent: false }
    },
    {
      path: "/query",
      name: "Query",
      component: () => import("@/views/query/index.vue"),
      meta: { title: "库存查询", showLink: true, showParent: false }
    },
    {
      path: "/logs",
      name: "Logs",
      component: () => import("@/views/logs/index.vue"),
      meta: { title: "运行日志", showLink: true, showParent: false }
    }
  ]
} satisfies RouteConfigsTable;
