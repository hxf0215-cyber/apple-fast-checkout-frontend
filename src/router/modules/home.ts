const Layout = () => import("@/layout/index.vue");

export default {
  path: "/",
  name: "HomeRoot",
  component: Layout,
  redirect: "/overview",
  meta: {
    title: "首页",
    showLink: false,
    rank: 0
  }
} satisfies RouteConfigsTable;
