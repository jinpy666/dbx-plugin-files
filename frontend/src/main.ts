import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";
import { installHostThemeBridge } from "../../shared/frontend/themeSync";
import { vTip } from "./lib/tooltip";

// 宿主令牌 → 插件变量桥：首绘即命中宿主主题，主题变化经 SDK 令牌更新自动跟随。
installHostThemeBridge();

// 开发验证口：?mock=1 且无宿主桥时注入 mock 宿主（P-FILES ④，见 lib/mockHost.ts）。
const wantsMock = new URLSearchParams(window.location.search).has("mock");
const boot = () => {
  const app = createApp(App);
  // 图标按钮悬浮提示：宿主 webview 不渲染原生 title，统一走 v-tip 绘制。
  app.directive("tip", vTip);
  app.mount("#app");
};
if (wantsMock) {
  import("./lib/mockHost").then(({ installMockHost }) => {
    installMockHost();
    boot();
  });
} else {
  boot();
}
