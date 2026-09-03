// Token Monitor 前端：ECharts 图表 + 整分钟统计 + 请求触发刷新 + 设置/主题
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const comboChart = echarts.init(document.getElementById("comboChart"));

// ---------- 主题管理 ----------
const mediaDark = window.matchMedia("(prefers-color-scheme: dark)");
let themeColors = {};

function currentTheme() {
  const pref = localStorage.getItem("tm_theme") || "dark";
  return pref === "system" ? (mediaDark.matches ? "dark" : "light") : pref;
}

function hexToRgba(hex, alpha) {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${alpha})`;
}

function applyTheme() {
  document.documentElement.dataset.theme = currentTheme();
  themeColors = currentTheme() === "light"
    ? { axis: "#d7dee9", label: "#66738c", split: "#e8edf4", blue: "#2f6fe4", green: "#149e78" }
    : { axis: "#263049", label: "#8b97b0", split: "#1c2436", blue: "#4f8cff", green: "#35d0a5" };
  if (typeof refresh === "function") refresh(); // 图表用新配色重画
}

mediaDark.addEventListener("change", () => {
  if ((localStorage.getItem("tm_theme") || "dark") === "system") applyTheme();
});

function axis() {
  return {
    axisLine: { lineStyle: { color: themeColors.axis } },
    axisLabel: { color: themeColors.label, fontSize: 11 },
    splitLine: { lineStyle: { color: themeColors.split } },
  };
}

// 悬浮提示框跟随深浅主题：ECharts 默认白底黑字，不随主题变，必须显式指定
function tooltipStyle() {
  const light = currentTheme() === "light";
  return {
    backgroundColor: light ? "rgba(255,255,255,.96)" : "rgba(28,36,54,.96)",
    borderColor: light ? "#d7dee9" : "#263049",
    textStyle: { color: light ? "#2b3550" : "#dbe3f0", fontSize: 12 },
    axisPointer: {
      type: "shadow",
      shadowStyle: { color: hexToRgba(themeColors.blue, .08) },
      lineStyle: { color: themeColors.axis },
    },
  };
}

// ---------- 自绘下拉组件（替代原生 select）----------
function closeAllDropdowns(except) {
  document.querySelectorAll(".dd.open").forEach((d) => {
    if (d !== except) d.classList.remove("open");
  });
}

function makeDropdown(el, { options, value, onChange }) {
  let current = value;
  const chevron = `<svg class="dd-chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>`;

  function render() {
    const sel = options.find((o) => o.v === current);
    el.innerHTML = `
      <button type="button" class="dd-btn">
        <span class="dd-label">${sel ? sel.label : ""}</span>${chevron}
      </button>
      <div class="dd-list">
        ${options.map((o) => `
          <div class="dd-item${o.v === current ? " selected" : ""}" data-v="${o.v}">
            <span>${o.label}</span>
            ${o.v === current ? `<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>` : ""}
          </div>`).join("")}
      </div>`;
  }
  render();

  el.addEventListener("click", (e) => {
    const item = e.target.closest(".dd-item");
    if (item) {
      current = item.dataset.v;
      render();
      el.classList.remove("open");
      onChange && onChange(current);
      return;
    }
    if (e.target.closest(".dd-btn")) {
      closeAllDropdowns(el);
      el.classList.toggle("open");
    }
  });

  return {
    get value() { return current; },
    set value(v) { current = v; render(); },
  };
}
document.addEventListener("click", (e) => {
  if (!e.target.closest(".dd")) closeAllDropdowns();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeAllDropdowns();
});

// 合并图表：左轴 RPM（柱状），右轴 TPM（折线）。
// grid.top 必须留足空间：yAxis.name 画在网格上方，top 太小会把“次/min”等文字裁掉
function renderCombo(labels, rpms, tpms) {
  document.getElementById("rpmNow").textContent = fmtTokens(rpms[rpms.length - 1] || 0);
  document.getElementById("tpmNow").textContent = fmtTokens(tpms[tpms.length - 1] || 0);
  comboChart.setOption({
    backgroundColor: "transparent",
    tooltip: { trigger: "axis", ...tooltipStyle() },
    legend: {
      data: ["RPM", "TPM"],
      top: 0,
      textStyle: { color: themeColors.label, fontSize: 12 },
    },
    grid: { left: 60, right: 70, top: 36, bottom: 30 },
    xAxis: { type: "category", data: labels, ...axis(), boundaryGap: true },
    yAxis: [
      {
        type: "value", name: "次/分钟",
        nameTextStyle: { color: themeColors.label, align: "left" },
        ...axis(),
        splitLine: { lineStyle: { color: themeColors.split } }, // 网格线只画一份，避免左右刻度线交叉
      },
      {
        type: "value", name: "词元/分钟",
        nameTextStyle: { color: themeColors.label, align: "right" },
        ...axis(),
        splitLine: { show: false },
      },
    ],
    series: [
      {
        name: "RPM",
        type: "bar",
        yAxisIndex: 0,
        data: rpms,
        itemStyle: { color: themeColors.blue, borderRadius: [3, 3, 0, 0] },
        barMaxWidth: 26,
      },
      {
        name: "TPM",
        type: "line",
        yAxisIndex: 1,
        data: tpms,
        smooth: true,
        symbol: "circle",
        symbolSize: 5,
        lineStyle: { color: themeColors.green, width: 2 },
        itemStyle: { color: themeColors.green },
        areaStyle: {
          color: {
            type: "linear", x: 0, y: 0, x2: 0, y2: 1,
            colorStops: [
              { offset: 0, color: hexToRgba(themeColors.green, .28) },
              { offset: 1, color: hexToRgba(themeColors.green, 0) },
            ],
          },
        },
      },
    ],
  });
}

// ---------- 词元单位转换（设置里可开关，默认关闭）----------
// 开启后：总词元超过 1000 显示为 x.yK，超过 100 万为 x.yM，超过 10 亿为 x.yB
const UNIT_KEY = "tm_unit_convert";
let convertUnits = localStorage.getItem(UNIT_KEY) === "1";

function fmtTokens(n) {
  if (!convertUnits) return n.toLocaleString();
  if (n >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}

function renderUnitToggle() {
  const btn = document.getElementById("btnUnitConvert");
  btn.textContent = convertUnits ? "已开启" : "已关闭";
  btn.classList.toggle("active", convertUnits);
}
document.getElementById("btnUnitConvert").addEventListener("click", () => {
  convertUnits = !convertUnits;
  localStorage.setItem(UNIT_KEY, convertUnits ? "1" : "0");
  renderUnitToggle();
  refresh();
});
renderUnitToggle();

async function refresh() {
  const minutes = parseInt(rangeDd.value, 10);
  try {
    const resp = await invoke("get_stats", { windowMinutes: minutes });
    const labels = resp.buckets.map((b) => b.minute); // 已对齐整分钟，空桶补零
    const rpms = resp.buckets.map((b) => b.rpm);
    const tpms = resp.buckets.map((b) => b.tpm);
    renderCombo(labels, rpms, tpms);
    document.getElementById("sumReq").textContent =
      fmtTokens(rpms.reduce((a, b) => a + b, 0));
    document.getElementById("sumTok").textContent =
      fmtTokens(tpms.reduce((a, b) => a + b, 0));
    const conc = resp.concurrency || 0;
    const concEl = document.getElementById("concurrencyTitle");
    concEl.textContent = conc > 0 ? `并发 ${conc}` : "";
  } catch (e) {
    console.error("get_stats failed:", e);
  }
}

async function initInfo() {
  try {
    const info = await invoke("get_server_info");
    setEndpointLabel(info);
  } catch (e) { /* ignore */ }
}

function setEndpointLabel(info) {
  const el = document.getElementById("endpoint");
  el.textContent =
    info.endpoint + "/*" + (info.model_override ? `（强制模型 ${info.model_override}）` : "");
  // 提示支持的三种 API 格式
  if (Array.isArray(info.endpoints)) {
    el.title = "支持的接口：" + info.endpoints.join("  ·  ");
  }
}

// 点击复制 API 地址（只复制纯 endpoint，不含模型后缀）
document.getElementById("btnCopyEndpoint").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  try {
    const info = await invoke("get_server_info");
    await navigator.clipboard.writeText(info.endpoint);
    btn.title = "已复制 ✓";
  } catch {
    // 剪贴板不可用时降级：选中文字让用户手动 Ctrl+C
    const range = document.createRange();
    range.selectNodeContents(document.getElementById("endpoint"));
    const sel = getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    btn.title = "已选中最地址，请手动 Ctrl+C";
  }
  setTimeout(() => (btn.title = "复制 API 地址"), 1500);
});

// 时间范围下拉（自绘组件，记住上次选择）
const rangeDd = makeDropdown(document.getElementById("range"), {
  options: [
    { v: "5", label: "近 5 分钟" },
    { v: "10", label: "近 10 分钟" },
    { v: "30", label: "近 30 分钟" },
    { v: "60", label: "近 1 小时" },
    { v: "300", label: "近 5 小时" },
    { v: "0", label: "今日" },
    { v: "-1", label: "本周" },
  ],
  value: localStorage.getItem("tm_range") || "10",
  onChange: (v) => {
    localStorage.setItem("tm_range", v);
    refresh();
  },
});

// 测试请求：直接走本机代理，验证链路同时产生一条统计数据
document.getElementById("btnTest").addEventListener("click", async () => {
  const status = document.getElementById("testStatus");
  status.textContent = "请求中…";
  try {
    const info = await invoke("get_server_info");
    const r = await fetch(info.endpoint + "/chat/completions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        model: info.model_override || "x-preview-f",
        stream: false,
        max_tokens: 32,
        messages: [{ role: "user", content: "hi" }],
      }),
    });
    status.textContent = r.ok ? "✓ 成功" : `✗ HTTP ${r.status}`;
  } catch (e) {
    status.textContent = "✗ 失败";
  }
  refresh();
});

// ---------- 无边框标题栏窗口控制 ----------
const tauriWin = window.__TAURI__.window.getCurrentWindow();
let pinned = false;

// 拖动/双击最大化：显式调用窗口 API（capabilities 已授权 start-dragging）。
// 不用 data-tauri-drag-region：它和 CSS pointer-events hack 叠加时会把按钮
// 事件也吞成拖拽。这里用 closest() 精确排除所有交互控件。
const titlebarEl = document.getElementById("titlebar");
const toolbarEl = document.querySelector("header.toolbar");
const isInteractive = (t) => t.closest("button, select, input, a, .dd"); // .dd 包含下拉按钮与展开的选项列表，点击不能触发窗口拖动

function tryDrag(e) {
  if (e.button !== 0) return;      // 只响应左键
  if (isInteractive(e.target)) return; // 按钮/下拉框/输入框不触发拖动
  e.preventDefault();              // 防止选中文字干扰
  try { tauriWin.startDragging(); } catch (err) { console.error("startDragging failed:", err); }
}

for (const zone of [titlebarEl, toolbarEl]) {
  zone.addEventListener("mousedown", tryDrag);
  zone.addEventListener("dblclick", (e) => {
    if (isInteractive(e.target)) return;
    tauriWin.toggleMaximize();
    setTimeout(syncMaxIcon, 150);
  });
}

function syncMaxIcon() {
  tauriWin.isMaximized().then((m) => {
    document.getElementById("icoMax").style.display = m ? "none" : "";
    document.getElementById("icoRestore").style.display = m ? "" : "none";
  });
}
tauriWin.onResized(syncMaxIcon);
syncMaxIcon();

// 窗口控制失败时把原因显示在工具栏，便于排查（如权限缺失）
function reportWinErr(e) {
  const el = document.getElementById("testStatus");
  el.textContent = "⚠ " + (e && e.message ? e.message : String(e));
  setTimeout(() => { if (el.textContent.startsWith("⚠")) el.textContent = ""; }, 5000);
}
document.addEventListener("unhandledrejection", (e) => reportWinErr(e.reason));

document.getElementById("btnMin").addEventListener("click", () => tauriWin.minimize().catch(reportWinErr));
document.getElementById("btnMax").addEventListener("click", () => {
  tauriWin.toggleMaximize().catch(reportWinErr);
  setTimeout(syncMaxIcon, 120);
});
document.getElementById("btnClose").addEventListener("click", () => tauriWin.close().catch(reportWinErr)); // 隐藏到托盘
document.getElementById("btnPin").addEventListener("click", () => {
  pinned = !pinned;
  tauriWin.setAlwaysOnTop(pinned).catch(reportWinErr);
  document.getElementById("btnPin").classList.toggle("active", pinned);
});

// ---------- 设置弹窗 ----------
const modal = document.getElementById("settingsModal");

document.getElementById("btnSettings").addEventListener("click", async () => {
  try {
    const s = await invoke("get_settings");
    document.getElementById("portInput").value = s.port;
    document.getElementById("modelInput").value = s.model_override || "";
    document.getElementById("upstreamInput").value = s.upstream_url || "";
    document.getElementById("apiKeyInput").value = ""; // 安全起见不回显
    document.getElementById("settingsInfo").textContent =
      `API Key：${s.has_api_key ? "已配置" : "未配置"}` +
      (s.model_override ? ` · 强制模型 ${s.model_override}` : " · 未强制模型");
  } catch (e) { /* ignore */ }
  modal.classList.remove("hidden");
});

document.getElementById("btnCloseSettings").addEventListener("click", () =>
  modal.classList.add("hidden"));
modal.addEventListener("click", (e) => {
  if (e.target === modal) modal.classList.add("hidden");
});

// 保存端口并重启服务（重新监听）
document.getElementById("btnSavePort").addEventListener("click", async () => {
  const msg = document.getElementById("portMsg");
  const port = parseInt(document.getElementById("portInput").value, 10);
  msg.className = "hint";
  msg.textContent = "重启中…";
  try {
    const r = await invoke("set_port", { port });
    msg.className = "hint ok";
    msg.textContent = `✓ 已在端口 ${r.port} 重新监听`;
    initInfo();
  } catch (e) {
    msg.className = "hint err";
    msg.textContent = String(e);
  }
});

// 保存模型名 / API Key（立即热更新，无需重启服务）
document.getElementById("btnSaveModel").addEventListener("click", async () => {
  const msg = document.getElementById("modelMsg");
  msg.className = "hint";
  msg.textContent = "保存中…";
  try {
    const r = await invoke("set_model_config", {
      apiKey: document.getElementById("apiKeyInput").value,
      modelOverride: document.getElementById("modelInput").value,
    });
    msg.className = "hint ok";
    msg.textContent = `✓ 已保存${r.has_api_key ? "（Key 已更新）" : ""}`;
    if (document.getElementById("apiKeyInput").value) {
      document.getElementById("apiKeyInput").value = "";
    }
    initInfo();
  } catch (e) {
    msg.className = "hint err";
    msg.textContent = String(e);
  }
});

// 保存转发目标地址（立即热更新，无需重启服务）
document.getElementById("btnSaveUpstream").addEventListener("click", async () => {
  const msg = document.getElementById("upstreamMsg");
  msg.className = "hint";
  msg.textContent = "保存中…";
  try {
    const r = await invoke("set_upstream", { url: document.getElementById("upstreamInput").value });
    msg.className = "hint ok";
    msg.textContent = `✓ 已生效：${r.upstream_url}`;
  } catch (e) {
    msg.className = "hint err";
    msg.textContent = String(e);
  }
});

// 主题下拉（自绘组件）
makeDropdown(document.getElementById("themeSelect"), {
  options: [
    { v: "dark", label: "深色" },
    { v: "light", label: "浅色" },
    { v: "system", label: "跟随系统" },
  ],
  value: localStorage.getItem("tm_theme") || "dark",
  onChange: (v) => {
    localStorage.setItem("tm_theme", v);
    applyTheme();
  },
});

// 每次代理收到请求（后端 emit）立即刷新
listen("stats-updated", () => refresh());

window.addEventListener("resize", () => comboChart.resize());
// flex 布局下容器尺寸在首帧/字体加载后才稳定，用 ResizeObserver 跟随容器实际尺寸重绘
new ResizeObserver(() => comboChart.resize()).observe(document.getElementById("comboChart"));

applyTheme();
initInfo();
refresh();
// 兜底轮询：即使错过事件也保持最新
setInterval(refresh, 5000);
