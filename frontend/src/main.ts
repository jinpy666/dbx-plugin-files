import { createApp } from "vue";
import App from "./App.vue";
import "./style.css";

// 开发验证口：?mock=1 且无宿主桥时注入 mock 宿主（P-FILES ④，见 lib/mockHost.ts）。
const wantsMock = new URLSearchParams(window.location.search).has("mock");
const boot = () => createApp(App).mount("#app");
if (wantsMock) {
  import("./lib/mockHost").then(({ installMockHost }) => {
    installMockHost();
    boot();
  });
} else {
  boot();
}
