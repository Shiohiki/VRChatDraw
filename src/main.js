// ============================================================
// VRChat 绘画脚本 · 前端逻辑 v2（绘图工作台布局）
// 对接 Tauri 2 Rust commands · 基于 Iconoir 图标
// ============================================================
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { openUrl } = window.__TAURI__.opener;
const { getCurrentWindow } = window.__TAURI__.window;

// ===================== 全局状态 =====================
const state = {
  config: null,        // AppConfig
  aiConfig: null,      // AiConfig
  image: null,         // ImageInfo { path, file_name, data_url, width, height }
  outcome: null,       // ProcessOutcome { strokes, stroke_count, point_count, revision }
  clientRevision: 0,   // 前端参数/图片版本，避免异步处理结果回写到旧状态
  lineKey: null,
  lineKeyBase: null,
  zoom: 1, tx: 0, ty: 0,
  tool: "pan",
  mode: "idle",        // idle | processing | ready | drawing
  drawing: false,      // Rust 侧实际绘制状态
  rehearsing: false,    // Shift+F9 边界预演状态
  drawingGeneration: 0, // 忽略旧绘制线程晚到的结束事件
  processing: false,
  testingAi: false,
  toastTimer: null,
  progressTimer: null,
  dragging: false,
  startX: 0, startY: 0, startTx: 0, startTy: 0,
  modalTimer: null,
  tutorialTimer: null,
  tutorialPrevFocus: null,
  prevFocus: null,
  settingsNavFrame: 0,
  tutorialNavFrame: 0,
  tutorialNavSync: null,
  // 笔画相册
  galleryTimer: null,
  galleryFocusTimer: null,
  galleryPrevFocus: null,
  galleryPromptTimer: null,
  galleryPromptFocusTimer: null,
  galleryPromptPrevFocus: null,
  tutorialFocusTimer: null,
};

const root = document.documentElement;
const $ = (id) => document.getElementById(id);

// ===================== 工具函数 =====================
function toast(title, copy = "", warning = false, ms = 2400, kind = warning ? "error" : "success") {
  $("toastTitle").textContent = title;
  $("toastCopy").textContent = copy;
  $("toastIcon").className = kind === "info"
    ? "toast-info-icon"
    : (warning ? "iconoir-warning-circle-solid" : "iconoir-check-circle-solid");
  $("toast").classList.add("show");
  clearTimeout(state.toastTimer);
  state.toastTimer = setTimeout(() => $("toast").classList.remove("show"), ms);
}

function fmt(n, digits = 1) {
  const v = parseFloat(n);
  return Number.isInteger(v) ? String(v) : v.toFixed(digits);
}

// 秒数格式化：先整体取整再拆分，避免 Math.round(secs % 60) 得到 60（如 59.6 秒 → "0 分 60 秒"）
function formatSeconds(secs) {
  if (!(secs > 0)) return null;
  const total = Math.round(secs);
  const m = Math.floor(total / 60);
  const r = total - m * 60;
  return m > 0 ? `${m} 分 ${r} 秒` : `${r} 秒`;
}

// 旧结果/异常回退时的按点数估算（后端未提供 estimate_seconds 时使用）
function estimateSecondsForCounts(strokeCount, pointCount) {
  const c = state.config?.drawing;
  if (!c || !strokeCount || !pointCount) return null;
  const delay = Math.max(c.draw_speed, 0.016);
  const lift = Math.max(c.lift_pen_delay, 0.04);
  // 每笔固定开销：到达同步 60ms + 硬同步 30ms + 稳定停顿 20ms + 抬笔延迟（与后端 0.110 构成一致）
  let secs = strokeCount * (0.11 + lift) + Math.max(0, pointCount - strokeCount) * delay;
  return formatSeconds(secs);
}

function estimateSeconds() {
  const o = state.outcome;
  if (!o || !o.strokes?.length) return null;
  // 优先使用后端按"步数模型"（段距离/步长）计算的估算，避免长直线段被低估
  if (typeof o.estimate_seconds === "number") return formatSeconds(o.estimate_seconds);
  return estimateSecondsForCounts(o.stroke_count, o.point_count);
}

let configSaveQueue = Promise.resolve();
let aiSaveQueue = Promise.resolve();
let lastPersistedConfig = null;
let lastPersistedAiConfig = null;
let pendingAiApiKey = "";
let presetControlApi = null;

function cloneData(value) {
  return JSON.parse(JSON.stringify(value));
}

function sameData(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

// 持久化界面配置：串行化完整快照，避免快速拖动滑块时旧快照覆盖新快照。
// 后端返回规范化后的配置；只有当前状态仍等于本次快照时才回写规范化结果。
async function persistConfig() {
  const snapshot = cloneData(state.config);
  const task = configSaveQueue.catch(() => {}).then(() => invoke("save_config", { cfg: snapshot }));
  configSaveQueue = task.catch(() => {});
  try {
    const persisted = cloneData((await task) || snapshot);
    lastPersistedConfig = persisted;
    if (sameData(state.config, snapshot) && !sameData(state.config, persisted)) {
      state.config = cloneData(persisted);
      renderAll();
    }
    return persisted;
  } catch (e) {
    console.warn("配置保存失败", e);
    // 仅当用户没有继续修改参数时回滚；后续更晚的快照由队列继续处理。
    if (lastPersistedConfig
        && sameData(state.config, snapshot)) {
      state.config = cloneData(lastPersistedConfig);
      renderAll();
      // 补偿同步：save_config 失败时 Rust 内存可能仍停在 sync_config 的新值，
      // 把回滚后的旧值重新同步进内存（sync 只改内存、零 IO 风险），
      // 避免"界面显示旧值、F9 却用新值"的不一致窗口。
      configSaveQueue = configSaveQueue
        .catch(() => {})
        .then(() => invoke("sync_config", { cfg: cloneData(state.config) }))
        .catch(() => {});
    }
    toast("参数保存失败", String(e), true);
    return null;
  } finally {
  }
}

// 等待并保存当前完整配置。若用户在保存期间继续修改，最多追赶三次，
// 防止“点击生成”的请求拿到一个已经过期的配置快照。
async function flushConfig() {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const before = cloneData(state.config);
    const persisted = await persistConfig();
    if (!persisted) return false;
    if (sameData(state.config, persisted) || sameData(state.config, before)) return true;
  }
  toast("参数仍在变化", "请停止调整参数后再生成笔画。", true);
  return false;
}

// ===================== 参数实时同步（不写盘） =====================
// 拖动滑块/键入数值时把内存配置实时同步给 Rust（节流 ~150ms），
// 使 F9/Shift+F9 等全局热键立即使用最新参数做绘制与边界预演；
// 磁盘持久化仍由 change/blur 触发的 persistConfig 负责。
let liveSyncTimer = null;

async function syncConfigLive() {
  const snapshot = cloneData(state.config);
  const task = configSaveQueue
    .catch(() => {})
    .then(() => invoke("sync_config", { cfg: snapshot }));
  configSaveQueue = task.catch(() => {});
  try {
    const synced = await task;
    // 与 persistConfig 一致：仅当用户未继续修改时回写规范化结果
    if (sameData(state.config, snapshot) && synced && !sameData(state.config, synced)) {
      state.config = cloneData(synced);
      renderAll();
    }
  } catch (e) {
    // 静默失败：change/blur 的 persistConfig 会兜底落盘
    console.warn("参数实时同步失败", e);
  }
}

function scheduleConfigSync() {
  if (liveSyncTimer) return;
  liveSyncTimer = setTimeout(() => {
    liveSyncTimer = null;
    syncConfigLive();
  }, 150);
}

async function resetConfigPersisted() {
  const task = configSaveQueue
    .catch(() => {})
    .then(() => invoke("reset_config"));
  configSaveQueue = task.catch(() => {});
  try {
    const cfg = await task;
    state.config = cloneData(cfg);
    lastPersistedConfig = cloneData(cfg);
    return cfg;
  } catch (e) {
    toast("重置失败", String(e), true);
    return null;
  } finally {
  }
}

async function persistAiConfig() {
  const snapshot = cloneData(state.aiConfig);
  const keyForSave = pendingAiApiKey;
  const payload = cloneData(snapshot);
  delete payload.api_key_set;
  if (snapshot.clear_api_key) {
    payload.api_key = "";
  } else if (keyForSave) {
    payload.api_key = keyForSave;
  }
  const task = aiSaveQueue.catch(() => {}).then(() => invoke("save_ai_config", { cfg: payload }));
  aiSaveQueue = task.catch(() => {});
  try {
    await task;
    const persisted = cloneData(snapshot);
    persisted.clear_api_key = false;
    if (snapshot.clear_api_key) {
      persisted.api_key_set = false;
    } else if (keyForSave) {
      persisted.api_key_set = true;
    }
    const stateStillMatchesSnapshot = JSON.stringify(state.aiConfig) === JSON.stringify(snapshot);
    if (stateStillMatchesSnapshot) {
      state.aiConfig = cloneData(persisted);
    }
    if (pendingAiApiKey === keyForSave) {
      pendingAiApiKey = "";
      $("aiKey").value = "";
    }
    if (stateStillMatchesSnapshot) {
      $("aiKeyClear").disabled = !state.aiConfig.api_key_set;
      $("aiKey").placeholder = state.aiConfig.api_key_set
        ? "已配置 API Key（留空保持）"
        : "sk-...";
    }
    // Record the exact request that was persisted; a newer edit may already be
    // present in state.aiConfig when this queued request completes.
    lastPersistedAiConfig = persisted;
    return true;
  } catch (e) {
    if (lastPersistedAiConfig
        && JSON.stringify(state.aiConfig) === JSON.stringify(snapshot)) {
      state.aiConfig = cloneData(lastPersistedAiConfig);
      renderAll();
      if (pendingAiApiKey) $("aiKey").value = pendingAiApiKey;
    }
    console.warn("AI 配置保存失败", e);
    toast("AI 配置保存失败", String(e), true);
    return false;
  }
}

// ===================== 自定义下拉选择器（Select） =====================
// 参照 Radix Select 交互模式：Trigger 按钮 + 弹出面板 + 键盘导航（↑↓/Enter/Esc）
// + 点击外部关闭 + 位置避让（空间不足向上展开）+ transform-origin 入场动画
const SELECT_PANEL_CLOSE_MS = 210; // CSS 退场 200ms，额外留一帧后再 hidden

// 下拉面板公共定位（select/combobox 共用）：锚点下方展开，空间不足向上；
// 宽高与位置都在视口内 clamp。measureContentWidth 时按最长选项自适应宽度。
function positionFloatingPanel(anchorRect, panel, { minWidth = 0, measureContentWidth = false } = {}) {
  let width = Math.max(anchorRect.width, minWidth);
  if (measureContentWidth) {
    [...panel.children].forEach((el) => { width = Math.max(width, el.scrollWidth); });
    width = Math.min(Math.round(width + 4), window.innerWidth - 16);
  }
  const left = Math.max(8, Math.min(Math.round(anchorRect.left), window.innerWidth - width - 8));
  panel.style.left = left + "px";
  panel.style.width = width + "px";
  panel.classList.remove("open-up");
  const childCount = panel.children.length;
  let panelHeight = panel.offsetHeight || (childCount ? childCount * 32 + 10 : 240);
  panelHeight = Math.min(panelHeight, window.innerHeight - 16); // 不超过视口高度
  const spaceBelow = window.innerHeight - anchorRect.bottom;
  if (spaceBelow < panelHeight + 8 && anchorRect.top > panelHeight + 8) {
    panel.style.top = Math.round(anchorRect.top - panelHeight - 6) + "px";
    panel.classList.add("open-up");
  } else {
    panel.style.top = Math.round(anchorRect.bottom + 6) + "px";
  }
}

// 外部点击关闭（面板已 Portal 到 body：须同时排除面板本身，
// 否则点面板项会先触发关闭导致后续 click 丢失）
function bindPanelDismiss(wrap, panel, isOpen, close) {
  document.addEventListener("pointerdown", (e) => {
    if (isOpen() && !wrap.contains(e.target) && !panel.contains(e.target)) close();
  });
}

// 键盘高亮循环索引（select/combobox 共用；count=0 返回 -1）
function nextHighlightIndex(dir, highlighted, count) {
  if (!count) return -1;
  let i = highlighted;
  if (i < 0) i = dir > 0 ? -1 : 0;
  do { i = (i + dir + count) % count; } while (i === highlighted && count > 1);
  return i;
}

function initSelect(wrapId, { onChange } = {}) {
  const wrap = $(wrapId);
  const trigger = wrap.querySelector(".ds-select-trigger");
  const panel = wrap.querySelector(".ds-select-panel");
  const valueEl = wrap.querySelector(".ds-select-value");
  const items = [...wrap.querySelectorAll(".ds-select-item")];
  let open = false;
  let highlighted = -1;

  function setValue(val, silent = false) {
    valueEl.textContent = val;
    items.forEach((it) => it.setAttribute("aria-selected", String(it.dataset.value === val)));
    if (!silent && onChange) onChange(val);
  }

  function positionPanel() {
    positionFloatingPanel(trigger.getBoundingClientRect(), panel, { minWidth: 170 });
  }

  function openPanel() {
    if (open) return;
    open = true;
    // Portal：面板挂到 body——modal 的 transform 会使 position:fixed 退化为 absolute，
    // 导致面板相对 modal 定位错乱（Radix Select 同样用 Portal 解决此问题）
    if (panel.parentElement !== document.body) document.body.appendChild(panel);
    trigger.setAttribute("aria-expanded", "true");
    panel.hidden = false;
    positionPanel();
    highlighted = items.findIndex((it) => it.getAttribute("aria-selected") === "true");
    if (highlighted < 0) highlighted = 0;
    items.forEach((it, i) => it.classList.toggle("highlighted", i === highlighted));
    // 双 rAF 确保 display 切换后 transition 生效
    requestAnimationFrame(() => requestAnimationFrame(() => panel.classList.add("open")));
  }

  function closePanel() {
    if (!open) return;
    open = false;
    trigger.setAttribute("aria-expanded", "false");
    panel.classList.remove("open");
    setTimeout(() => { if (!open) panel.hidden = true; }, SELECT_PANEL_CLOSE_MS);
  }

  function moveHighlight(dir) {
    highlighted = nextHighlightIndex(dir, highlighted, items.length);
    items.forEach((it, idx) => it.classList.toggle("highlighted", idx === highlighted));
  }

  trigger.addEventListener("click", (e) => {
    e.stopPropagation();
    open ? closePanel() : openPanel();
  });

  items.forEach((it, idx) => {
    it.addEventListener("click", (e) => {
      e.stopPropagation();
      setValue(it.dataset.value);
      closePanel();
    });
    it.addEventListener("mousemove", () => {
      highlighted = idx;
      items.forEach((o, j) => o.classList.toggle("highlighted", j === idx));
    });
  });

  trigger.addEventListener("keydown", (e) => {
    const k = e.key;
    if (!open && (k === "ArrowDown" || k === "ArrowUp" || k === "Enter" || k === " ")) {
      e.preventDefault();
      openPanel();
      return;
    }
    if (!open) return;
    if (k === "ArrowDown") { e.preventDefault(); moveHighlight(1); }
    else if (k === "ArrowUp") { e.preventDefault(); moveHighlight(-1); }
    else if (k === "Enter" || k === " ") {
      e.preventDefault();
      const el = items[highlighted];
      if (el) setValue(el.dataset.value);
      closePanel();
    } else if (k === "Escape") {
      closePanel();
      trigger.focus();
      e.stopPropagation(); // 与 combobox 一致：仅收起下拉，不连带关闭设置弹窗
    }
  });

  bindPanelDismiss(wrap, panel, () => open, closePanel);

  // 初始值（不触发 onChange）
  const initial = wrap.dataset.value || (items[0] ? items[0].dataset.value : "");
  if (initial) setValue(initial, true);

  return { setValue, close: closePanel };
}

// 生图接口后缀选择器（images/edits | chat/completions）
const aiEndpointSelect = initSelect("aiEndpointSelect", {
  onChange: (val) => {
    if (!state.aiConfig) return;
    state.aiConfig.api_endpoint = val;
    persistAiConfig();
  },
});

// ===================== 可输入下拉组合框（Combobox，模型选择） =====================
// 输入框 + 箭头按钮 + 可滚动面板（模型较多时滚动查看）；获取模型后动态填充
function initCombobox(wrapId, { onSelect } = {}) {
  const wrap = $(wrapId);
  const input = wrap.querySelector("input");
  const arrowBtn = wrap.querySelector(".ds-combobox-arrow");
  const panel = wrap.querySelector(".ds-select-panel");
  let open = false;
  let highlighted = -1;

  function renderHighlight() {
    [...panel.children].forEach((el, i) => el.classList.toggle("highlighted", i === highlighted));
  }

  function setInputValue(value) {
    input.value = value;
    [...panel.children].forEach((el) => {
      el.setAttribute("aria-selected", String(el.dataset.value === value));
    });
  }

  // 填充模型列表（获取模型后调用）
  function setItems(list) {
    const models = [...new Set((Array.isArray(list) ? list : [])
      .map((value) => String(value).trim())
      .filter(Boolean))].slice(0, 200);
    panel.innerHTML = "";
    models.forEach((m, idx) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "ds-select-item";
      b.setAttribute("role", "option");
      b.setAttribute("aria-selected", String(m === input.value));
      b.dataset.value = m;
      b.style.setProperty("--item-index", String(Math.min(idx, 10)));
      const span = document.createElement("span");
      span.className = "ds-select-item-label";
      span.textContent = m;
      b.appendChild(span);
      const check = document.createElement("i");
      check.className = "iconoir-check ds-select-check";
      check.setAttribute("aria-hidden", "true");
      b.appendChild(check);
      b.addEventListener("click", (e) => {
        e.stopPropagation();
        setInputValue(m);
        // 光标置尾：模型名较长时让输入框滚动显示末尾，避免"显示不全"
        try { input.setSelectionRange(m.length, m.length); } catch (_) { /* 忽略 */ }
        if (onSelect) onSelect(m);
        closePanel();
      });
      b.addEventListener("mousemove", () => {
        highlighted = idx;
        renderHighlight();
      });
      panel.appendChild(b);
    });
    highlighted = models.findIndex((m) => m === input.value);
    renderHighlight();
  }

  function positionPanel() {
    // 宽度自适应：取"输入框宽"与"最长选项文本宽"的较大者（含勾选标记余量）
    positionFloatingPanel(wrap.getBoundingClientRect(), panel, { measureContentWidth: true });
  }

  function openPanel() {
    if (open) return;
    if (!panel.children.length) {
      // 尚未获取模型：提示而不是静默无响应
      toast("请先点击获取模型", "获取接口可用模型后再进行选择。", true);
      return;
    }
    open = true;
    if (panel.parentElement !== document.body) document.body.appendChild(panel);
    arrowBtn.classList.add("open");
    panel.hidden = false;
    positionPanel();
    // 输入值在列表中时高亮对应项
    highlighted = [...panel.children].findIndex((el) => el.dataset.value === input.value);
    renderHighlight();
    requestAnimationFrame(() => requestAnimationFrame(() => panel.classList.add("open")));
  }

  function closePanel() {
    if (!open) return;
    open = false;
    arrowBtn.classList.remove("open");
    panel.classList.remove("open");
    setTimeout(() => { if (!open) panel.hidden = true; }, SELECT_PANEL_CLOSE_MS);
  }

  function moveHighlight(dir) {
    highlighted = nextHighlightIndex(dir, highlighted, panel.children.length);
    renderHighlight();
    const el = panel.children[highlighted];
    if (el) el.scrollIntoView({ block: "nearest" });
  }

  arrowBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    open ? closePanel() : openPanel();
  });
  // 手动输入时不弹出面板：仅点击箭头/获取模型后才显示列表，避免干扰输入
  input.addEventListener("keydown", (e) => {
    const k = e.key;
    if (!open && (k === "ArrowDown" || k === "ArrowUp")) {
      e.preventDefault();
      openPanel();
      return;
    }
    if (!open) return;
    if (k === "ArrowDown") { e.preventDefault(); moveHighlight(1); }
    else if (k === "ArrowUp") { e.preventDefault(); moveHighlight(-1); }
    else if (k === "Enter") {
      e.preventDefault();
      const el = panel.children[highlighted];
      if (el) {
        setInputValue(el.dataset.value);
        // 光标置尾，长模型名滚动显示末尾
        try { input.setSelectionRange(input.value.length, input.value.length); } catch (_) { /* 忽略 */ }
        if (onSelect) onSelect(el.dataset.value);
      } else if (input.value.trim()) {
        // 手动输入的模型名（列表外）按 Enter 也要保存，避免静默丢弃
        if (onSelect) onSelect(input.value.trim());
      }
      closePanel();
    } else if (k === "Escape") {
      closePanel();
      e.stopPropagation(); // 面板已关时不再拦截，允许关闭设置弹窗
    }
  });
  bindPanelDismiss(wrap, panel, () => open, closePanel);

  return { setItems, open: openPanel, close: closePanel };
}

// 模型下拉组合框（模型列表由"获取模型"按钮填充）
const aiModelCombobox = initCombobox("aiModelCombobox", {
  onSelect: () => saveAiInput("aiModel"),
});

// ===================== 初始化 =====================
function normalizeDrawingEvent(payload, finished = false) {
  if (payload && typeof payload === "object" && !Array.isArray(payload)) {
    return {
      generation: Number.isFinite(Number(payload.generation))
        ? Number(payload.generation)
        : null,
      active: !!payload.active,
      finished: !!payload.finished,
      rehearsal: !!payload.rehearsal,
    };
  }
  return {
    generation: null,
    active: !finished && !!payload,
    finished: finished && !!payload,
    rehearsal: false,
  };
}

function acceptDrawingGeneration(event) {
  if (event.generation == null) {
    // 新协议始终带 generation；已有 generation 后不再接受旧格式事件，
    // 防止旧线程的布尔结束事件覆盖新绘制状态。
    return state.drawingGeneration === 0;
  }
  if (event.generation < state.drawingGeneration) return false;
  state.drawingGeneration = event.generation;
  return true;
}

async function registerRuntimeListeners() {
  try {
    await Promise.all([
      listen("drawing-state", (e) => {
        const event = normalizeDrawingEvent(e.payload);
        if (!acceptDrawingGeneration(event)) return;
        state.drawing = event.active;
        state.rehearsing = event.rehearsal;
        updateCanvasControls();
        // 处理图像进行中收到绘制信号时只同步 drawing 标志，不打断 processing 状态机
        if (state.processing) return;
        if (state.drawing) {
          state.mode = "drawing";
          startIndeterminate();
          renderState(state.rehearsing ? "正在预演...按 F10 停止" : "正在绘制...按 F10 停止");
        } else if (state.mode === "drawing") {
          state.mode = state.outcome ? "ready" : "idle";
          stopProgress();
          renderState();
        }
      }),
      listen("drawing-finished", (e) => {
        const event = normalizeDrawingEvent(e.payload, true);
        if (!acceptDrawingGeneration(event)) return;
        state.drawing = false;
        state.rehearsing = false;
        updateCanvasControls();
        if (state.mode === "drawing") {
          state.mode = state.outcome ? "ready" : "idle";
          stopProgress();
          renderState();
        }
        // 预演完成不提示“绘制完成”；只有真实绘制自然结束才提示。
        if (event.finished && !event.rehearsal) toast("绘制完成", "全部笔画已发送到 VRChat。");
      }),
      listen("toast", (e) => {
        // 防御：非数组 payload（异常来源）不再被解构出首字符
        const payload = Array.isArray(e.payload) ? e.payload : [e.payload, "info"];
        const [msg, kind] = payload;
        if (msg) {
          // 错误/警告（失焦诊断、参数失效等）可读性优先：延长停留时间
          const serious = kind === "error" || kind === "warning";
          toast(String(msg), "", serious, serious ? 6000 : 2400, kind);
        }
      }),
      // 拖拽导入：把图片文件拖入窗口即可加载（与"选择"共用同一导入路径）
      getCurrentWindow().onDragDropEvent((e) => {
        const payload = e.payload;
        if (!payload || payload.type !== "drop") return;
        const paths = payload.paths || [];
        if (!paths.length) return;
        if (paths.length > 1) {
          toast("仅支持单张图片", "一次拖入一张图片。", true);
          return;
        }
        importImage(String(paths[0]));
      }),
    ]);
  } catch (e) {
    // 事件注册失败不阻断主流程，但给出提示
    toast("事件监听注册失败", String(e), true);
  }
}

async function init() {
  // 先完成事件注册，再读取后端状态，避免启动瞬间错过绘制结束事件。
  await registerRuntimeListeners();
  try {
    const [config, aiConfig, drawing] = await Promise.all([
      invoke("get_config"),
      invoke("get_ai_config"),
      invoke("drawing_active"),
    ]);
    state.config = config || null;
    state.aiConfig = aiConfig || {};
    delete state.aiConfig.api_key;
    delete state.aiConfig.clear_api_key;
    lastPersistedConfig = cloneData(config);
    lastPersistedAiConfig = cloneData(state.aiConfig);
    state.drawing = drawing;
    // 启动时若 Rust 侧已在绘制（热键触发/异常恢复），同步状态机，避免界面显示"就绪"
    if (drawing) {
      state.mode = "drawing";
      startIndeterminate(); // 与 drawing-state 事件路径一致：显示进度条
    }
  } catch (e) {
    toast("初始化失败", String(e), true);
    return;
  }
  renderAll();
  bindEvents();
  bindWindowControls();
  // 拉取 setup 阶段累积的错误（如热键注册失败，避免事件丢失）
  try {
    const startupErrors = await invoke("get_startup_errors");
    startupErrors.forEach((msg) => toast(String(msg), "", true));
  } catch { /* 忽略拉取失败 */ }
  // 首次启动展示新手教程（已勾选"以后不再显示"则跳过）
  maybeOpenTutorial();
  // 窗口显示前把原生背景同步为当前主题色（tauri.conf 的 backgroundColor 是静态深色）：
  // WebView2 在隐藏窗口下尚未上屏，show() 瞬间会先露一帧原生背景，
  // 浅色主题下若不提前同步会先看到一帧深色背景（"深→浅"闪变）
  try {
    await invoke("apply_window_background", { dark: !!state.config?.theme_dark });
  } catch (e) {
    console.warn("窗口背景同步失败", e);
  }
  // 首帧渲染完成后显式显示窗口（配合 tauri.conf 的 visible:false）：
  // WebView2 内容尚未就绪时窗口若已可见，会露出 background 底色的等待段
  // （启动黑屏等待）；延迟到 UI 全部就绪后再展示，保证首个可见帧即完整界面
  try {
    await getCurrentWindow().show();
  } catch (e) {
    console.warn("窗口显示失败", e);
  }
}

// 图片元信息渲染/清除（侧栏缩略图 + 画布预览 + 尺寸徽章）
function applyImageMeta(info) {
  $("sourceThumb").src = info.data_url;
  $("fileName").textContent = info.file_name;
  $("baseImage").src = info.data_url;
  $("imgSizeStatus").querySelector("span").textContent = `${info.width} × ${info.height}`;
  $("stageBadge").hidden = false;
  $("canvasSize").textContent = `${info.width} × ${info.height}`;
}

function clearImageMeta() {
  $("sourceThumb").removeAttribute("src");
  $("baseImage").removeAttribute("src");
  $("lineImage").removeAttribute("src");
  $("fileName").textContent = "未选择图片";
  $("imgSizeStatus").querySelector("span").textContent = "—";
  $("stageBadge").hidden = true;
  $("canvasSize").textContent = "0 × 0";
}

// ===================== 渲染 =====================
function renderAll() {
  const c = state.config;
  setRange("eps", c.contour.epsilon_ratio);
  setRange("blur", c.image.blur_size);
  setRange("sens", c.drawing.sensitivity);
  setRange("speed", Math.round(c.drawing.draw_speed * 1000));
  setRange("step", c.drawing.max_step_px);
  setRange("lift", c.drawing.lift_pen_speed);
  setRange("stretch", c.drawing.vertical_stretch);
  $("aiSwitch").setAttribute("aria-checked", String(!!c.use_ai));

  const a = state.aiConfig;
  // 与 aiKey/aiModel 一致：编辑中不重置（任何 renderAll 回写路径都不会抹掉未提交输入）
  if (document.activeElement !== $("aiBase")) $("aiBase").value = a.api_base_url || "";
  // Rust 侧不回传 Key 本体，只显示“已配置”占位；用户输入新值时才写入。
  // 用户正在编辑 Key/模型名时不清空（避免其他参数保存回写触发 renderAll 抹掉未提交输入）
  if (document.activeElement !== $("aiKey")) $("aiKey").value = "";
  $("aiKey").placeholder = a.api_key_set ? "已配置 API Key（留空保持）" : "sk-...";
  $("aiKeyClear").disabled = !a.api_key_set;
  $("aiKey").type = "password";
  $("aiKeyToggle").querySelector("i").className = "iconoir-eye";
  if (document.activeElement !== $("aiModel")) $("aiModel").value = a.model || "";
  aiEndpointSelect.setValue(a.api_endpoint || "images/edits", true);
  setSeg(["themeDark", "themeLight"], c.theme_dark ? "themeDark" : "themeLight");
  setSeg(["canvasLight", "canvasDark"], c.canvas_dark ? "canvasDark" : "canvasLight");
  $("gridSwitch").setAttribute("aria-checked", String(c.show_grid ?? true));

  applyTheme(state.config.theme_dark);
  applyCanvasBg();

  if (state.image) applyImageMeta(state.image);
  if (state.outcome) {
    $("strokeCount").textContent = state.outcome.stroke_count;
    $("pointCount").textContent = state.outcome.point_count.toLocaleString();
    drawLineImage();
  }
  updateCanvasControls();
  renderState();
}

function setRange(key, value) {
  const el = $(key);
  if (!el) return;
  el.value = value;
  updateRange(el);
}

function updateRange(input) {
  const min = Number(input.min || 0), max = Number(input.max || 100), value = Number(input.value);
  input.style.setProperty("--pct", ((value - min) / (max - min)) * 100 + "%");
  const badge = input.closest(".control-row")?.querySelector(".value-badge input");
  if (badge) {
    const key = input.dataset.key;
    badge.value = ["blur", "speed", "step", "lift"].includes(key)
      ? String(Math.round(value)) : fmt(value, 2);
  }
}

function setSeg(ids, activeId) {
  ids.forEach((id) => {
    const el = $(id);
    el.classList.toggle("active", id === activeId);
    el.setAttribute("aria-pressed", String(id === activeId));
  });
}

function applyTheme(dark) {
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}

// 画布底色：true=深色舞台，false=白色舞台（棋盘格由 show_grid 开关控制）
function applyCanvasBg() {
  const dark = state.config.canvas_dark;
  root.style.setProperty("--stage-bg", dark ? "#0e0e10" : "#ffffff");
  root.style.setProperty("--stage-grid", dark ? "#1a1a1e" : "#e5e5e8");
  // 预览网格开关：关闭时隐藏棋盘格（纯色舞台）
  $("stage").classList.toggle("no-grid", !state.config.show_grid);
}

// 把 strokes 画到离屏 canvas → 生成线稿图（对照视图用）
// 画布 = 原图全尺寸比例（长边 ≤2000），笔画坐标已是原图尺寸 → 与 baseImage 完全对齐
// 缓存：仅当 path/stroke 数量变化时重建（避免渲染/裁剪后无谓的重复 4M 像素重绘）
function drawLineImage() {
  const strokes = state.outcome?.strokes;
  if (!strokes || !strokes.length) return;
  const iw = state.image?.width, ih = state.image?.height;
  if (!(iw > 0 && ih > 0)) return;
  // 缓存前缀（不含坐标内容）：图片/结果版本、尺寸、数量任一变化才需要重建。
  // 坐标只随 outcome 整体替换而变（revision 门禁保证），前缀相同则坐标必相同，
  // 可直接短路，避免每次渲染都对数万采样点做全量坐标哈希。
  const base = `${state.image.path}|${state.image.revision || 0}|${state.outcome.revision || 0}|${iw}x${ih}|${state.outcome.stroke_count}|${state.outcome.point_count}`;
  if (state.lineKeyBase === base && $("lineImage").src) return;
  let checksum = 2166136261;
  // 用可复用缓冲直接哈希 f64 的 IEEE754 字节，避免逐点拼字符串的分配开销。
  // 不同坐标必产生不同字节序列，缓存失效语义与原来的字符串哈希完全一致。
  const coordBuf = new ArrayBuffer(16);
  const coordView = new DataView(coordBuf);
  const coordBytes = new Uint8Array(coordBuf);
  for (const stroke of strokes) {
    if (!stroke || !Array.isArray(stroke.points)) continue;
    for (const point of stroke.points) {
      if (point) {
        // Hash the exact serialized coordinates instead of only rounded values;
        // otherwise a small coordinate change could reuse a stale canvas image.
        coordView.setFloat64(0, Number(point.x), true);
        coordView.setFloat64(8, Number(point.y), true);
        for (let i = 0; i < 16; i++) {
          checksum ^= coordBytes[i];
          checksum = Math.imul(checksum, 16777619);
        }
      }
    }
  }
  const key = `${base}|${checksum >>> 0}`;
  if (state.lineKey === key && $("lineImage").src) return;
  const maxSide = 2000;
  const scale = Math.min(maxSide / iw, maxSide / ih, 1);
  const W = Math.max(1, Math.round(iw * scale));
  const H = Math.max(1, Math.round(ih * scale));
  const cv = document.createElement("canvas");
  cv.width = W; cv.height = H;
  const ctx = cv.getContext("2d");
  ctx.fillStyle = "#fff";
  ctx.fillRect(0, 0, W, H);
  ctx.strokeStyle = "#000";
  ctx.lineWidth = 1.5;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  ctx.beginPath();
  for (const s of strokes) {
    if (!s || !Array.isArray(s.points) || s.points.length < 2) continue;
    ctx.moveTo(s.points[0].x * scale, s.points[0].y * scale);
    for (let i = 1; i < s.points.length; i++) {
      ctx.lineTo(s.points[i].x * scale, s.points[i].y * scale);
    }
  }
  ctx.stroke();
  $("lineImage").src = cv.toDataURL("image/png");
  state.lineKey = key;
  state.lineKeyBase = base;
}

// ===================== 状态机渲染 =====================
function renderState(customFooter) {
  const action = $("primaryAction"), label = $("primaryLabel"),
    icon = $("primaryIcon"), footer = $("footerStateText"), work = $("workStatus");
  const busy = state.mode === "processing";
  action.classList.toggle("running", busy);
  // 主按钮固定为"生成笔画"（处理中显示进行中；原图被裁剪过则提示重新生成）
  const processing = state.mode === "processing";
  const needsRegen = !processing && !!state.image?.needsRegen;
  label.textContent = processing ? "处理中..." : (needsRegen ? "重新生成笔画" : "生成笔画");
  icon.className = processing ? "iconoir-magic-wand" : (needsRegen ? "iconoir-refresh" : "iconoir-spark");
  const stepSource = $("stepSource"), stepProcess = $("stepProcess"), stepDraw = $("stepDraw");
  [stepSource, stepProcess, stepDraw].forEach((step) => step.classList.remove("done", "active"));
  if (state.mode === "idle") {
    footer.textContent = customFooter || "参数已同步，可生成笔画";
    work.innerHTML = '<i class="iconoir-check-circle-solid"></i><span>就绪</span>';
    stepSource.classList.add("done");
  } else if (state.mode === "processing") {
    footer.textContent = customFooter || "正在分析轮廓与路径";
    work.innerHTML = '<i class="iconoir-scanning"></i><span>正在处理图像</span>';
    stepProcess.classList.add("active");
  } else if (state.mode === "ready") {
    footer.textContent = customFooter || "笔画已生成，可开始绘制";
    work.innerHTML = '<i class="iconoir-check-circle-solid"></i><span>笔画已就绪</span>';
    stepProcess.classList.add("done");
    stepDraw.classList.add("active");
  } else if (state.mode === "drawing") {
    // customFooter 在 drawing 分支也必须生效（如"正在预演...按 F10 停止"）
    footer.textContent = customFooter || "正在发送绘制轨迹";
    work.innerHTML = state.rehearsing
      ? '<i class="iconoir-view-grid"></i><span>正在预演边界</span>'
      : '<i class="iconoir-edit-pencil"></i><span>正在绘制</span>';
    stepDraw.classList.add("active");
  }
  $("drawStatus").innerHTML = state.drawing
    ? (state.rehearsing
      ? '<i class="iconoir-view-grid"></i><span>正在预演</span>'
      : '<i class="iconoir-edit-pencil"></i><span>正在绘制</span>')
    : '<i class="iconoir-wifi-off"></i><span>未在绘制</span>';
}

function startIndeterminate() {
  stopProgress();
  const bar = $("progressBar");
  $("progress").classList.add("show");
  let p = 0;
  state.progressTimer = setInterval(() => {
    p = (p + 2.2) % 100;
    bar.style.width = p + "%";
  }, 80);
}

function stopProgress() {
  clearInterval(state.progressTimer);
  state.progressTimer = null;
  $("progress").classList.remove("show");
  $("progressBar").style.width = "0%";
}

// ===================== 视图 / 画布 =====================
function applyTransform() {
  root.style.setProperty("--zoom", state.zoom);
  root.style.setProperty("--tx", state.tx + "px");
  root.style.setProperty("--ty", state.ty + "px");
  const t = Math.round(state.zoom * 100) + "%";
  $("zoomValue").textContent = t;
  $("statusZoom").textContent = t;
  // 裁剪/区域框跟随画布缩放/平移（内容坐标锚定）
  if (state.tool === "crop" && !$("cropBox").hidden) renderCropBox();
  if (state.tool === "region" && !$("regionBox").hidden) renderRegionBox();
}

function fit() {
  if (!state.image) return; // 未选择图片时画布固定
  // 与其他清理点一致：取消未落定的滚轮防抖记录，避免贴合后旧快照被塞进撤销栈
  if (wheelTimer) { clearTimeout(wheelTimer); wheelTimer = null; }
  wheelStart = null;
  state.zoom = 1; state.tx = 0; state.ty = 0;
  applyTransform();
}

// 画布控件可用性：未选择图片时全部禁用（画布固定）
function updateCanvasControls() {
  const hasImg = !!state.image;
  const locked = state.drawing || state.processing;
  $("chooseFile").disabled = locked;
  $("zoomIn").disabled = !hasImg || locked;
  $("zoomOut").disabled = !hasImg || locked;
  $("fitButton").disabled = !hasImg || locked;
  $("clearButton").disabled = !hasImg || state.drawing || state.processing;
  // 无图时隐藏白底 image-frame（避免无 src 的 img 占位显示成白色长条卡片）
  $("imageFrame").style.display = hasImg ? "block" : "none";
  // 缩略图容器：仅导入图片后显示（无图时文字左移）
  $("thumbWrap").classList.toggle("has-img", hasImg);
  document.querySelectorAll(".stage-tools .tool-button").forEach((b) => {
    b.disabled = !hasImg || locked;
  });
}

function setProcessingControlsDisabled(disabled) {
  document.querySelectorAll(
    "input[type=range], .value-badge input, #aiSwitch, #resetButton, #presetButton, #chooseFile, "
      + "#presetSaveButton, #settingsModal input, #settingsModal button:not(#closeSettings), "
      + "#settingsModal .ds-select-trigger, #cropConfirm, #cropCancel, #regionConfirm, #regionCancel, "
      + "#regionBadgeClear, #presetMenu button, #profileSaveButton, .strategy-btn, "
      + "#presetNameModal button, #presetConfirmModal button",
  ).forEach((control) => {
    control.disabled = disabled;
  });
  if (disabled) window.__vrcClosePresetMenu?.();
}

// 工具状态：pan（可拖拽）/ fixed（画布固定）/ crop（裁剪）/ region（区域补画）
function setTool(next) {
  if (!state.image) return; // 未选择图片时画布固定
  if (next === "pan") {
    // 平移按钮 = 切换 可拖拽/固定
    state.tool = state.tool === "pan" ? "fixed" : "pan";
  } else {
    state.tool = next;
  }
  syncToolUI();
  if (state.tool === "crop") enterCrop();
  else exitCrop();
  if (state.tool === "region") enterRegion();
  else exitRegion();
}

function syncToolUI() {
  document.querySelectorAll(".tool-button[data-tool]").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.tool === state.tool);
  });
  $("stage").classList.toggle("pan-mode", state.tool === "pan");
}

// 步进缩放（按钮/键盘共用）：以画布中心为锚，delta=+1 放大 / -1 缩小
function zoomStep(delta) {
  // 已到上下限时不入撤销栈：clamp 后无变化，入栈只产生一条"无效果"死条目
  const next = Math.max(0.45, Math.min(2.5, Math.round((state.zoom + 0.1 * delta) * 20) / 20));
  if (Math.abs(state.zoom - next) < 1e-9) return;
  pushUndo();
  const r = $("stage").getBoundingClientRect();
  zoomAt(r.left + r.width / 2, r.top + r.height / 2, next);
}

// 以屏幕光标点为中心缩放/平移（内容坐标锚定）
function zoomAt(clientX, clientY, nextZoom) {
  if (!state.image) return;
  const stage = $("stage");
  const r = stage.getBoundingClientRect();
  const mx = clientX - r.left - r.width / 2;
  const my = clientY - r.top - r.height / 2;
  const ct = $("canvasTransform");
  const cw = ct.offsetWidth || 1;
  const ch = ct.offsetHeight || 1;
  // 光标处的内容坐标（逆变换）
  const px = (mx - state.tx) / state.zoom + cw / 2;
  const py = (my - state.ty) / state.zoom + ch / 2;
  state.zoom = Math.max(0.45, Math.min(2.5, Math.round(nextZoom * 20) / 20));
  // 锚定：缩放后光标处内容点仍位于光标位置
  state.tx = mx - (px - cw / 2) * state.zoom;
  state.ty = my - (py - ch / 2) * state.zoom;
  applyTransform();
}

// ===================== 视图撤销（undo） =====================
// 只记录视图操作（缩放/平移），工具栏按钮触发的操作（适应/重置/工具切换）不入栈
state.undoStack = [];
// 滚轮防抖：连续滚动起点（合并为一条撤销），供 undoView 取消残留定时器
let wheelStart = null;
let wheelTimer = null;

function pushUndo(s) {
  // 允许传入快照（拖拽前状态）；默认记录当前状态；与栈顶相同则跳过（去重）
  const snap = s || { zoom: state.zoom, tx: state.tx, ty: state.ty };
  const top = state.undoStack[state.undoStack.length - 1];
  if (top && top.zoom === snap.zoom && top.tx === snap.tx && top.ty === snap.ty) return;
  state.undoStack.push(snap);
  if (state.undoStack.length > 50) state.undoStack.shift();
}

function undoView() {
  const s = state.undoStack.pop();
  if (!s) {
    toast("没有可撤销的操作", "暂无历史视图状态。", true);
    return;
  }
  // 取消可能仍在进行的滚轮防抖记录，避免残留 wheel 事件干扰撤销
  if (wheelTimer) { clearTimeout(wheelTimer); wheelTimer = null; }
  wheelStart = null;
  state.zoom = s.zoom;
  state.tx = s.tx;
  state.ty = s.ty;
  // 瞬时恢复（禁用过渡动画，避免撤销瞬间的跳变感）
  $("canvasTransform").classList.add("crop-live");
  applyTransform();
  requestAnimationFrame(() => requestAnimationFrame(() => {
    $("canvasTransform").classList.remove("crop-live");
  }));
}

// ===================== 裁剪 =====================
// 裁剪框使用**内容坐标**（原图像素坐标系）存储，渲染时换算屏幕坐标：
// 画布缩放/平移时裁剪框自动跟随（自适应），且天然限制在原图范围内
const cropState = { cx0: 0, cy0: 0, cx1: 0, cy1: 0, mode: null, startX: 0, startY: 0, init: { cx0: 0, cy0: 0, cx1: 0, cy1: 0 } };

// 内容坐标（原图像素）↔ 屏幕坐标（相对 stage）：直接映射 image-frame 实际显示区域
// （getBoundingClientRect 已包含 zoom/tx/ty 变换，无需手动换算）
function contentToScreen(px, py) {
  const fr = $("imageFrame").getBoundingClientRect();
  const sr = $("stage").getBoundingClientRect();
  // 防御：图片尚未完成布局时 frame 尺寸为 0，避免除零产生 NaN 坐标
  if (fr.width <= 0 || fr.height <= 0) return { x: 0, y: 0 };
  return {
    x: fr.left - sr.left + (px / state.image.width) * fr.width,
    y: fr.top - sr.top + (py / state.image.height) * fr.height,
  };
}

function enterCrop() {
  const box = $("cropBox");
  box.hidden = false;
  // 裁剪模式下禁用画布过渡动画，保证裁剪框实时跟随缩放/平移
  $("canvasTransform").classList.add("crop-live");
  // 暂时隐藏底部悬浮工具栏（渐出）
  document.querySelector(".stage-tools").classList.add("tools-hidden");
  // 初始框 = 原图内容中央 60%（内容坐标）
  const iw = state.image.width;
  const ih = state.image.height;
  cropState.cx0 = Math.round(iw * 0.2);
  cropState.cy0 = Math.round(ih * 0.2);
  cropState.cx1 = Math.round(iw * 0.8);
  cropState.cy1 = Math.round(ih * 0.8);
  renderCropBox();
}

function exitCrop() {
  $("cropBox").hidden = true;
  cropState.mode = null;
  $("canvasTransform").classList.remove("crop-live");
  // 恢复底部悬浮工具栏（渐入）
  document.querySelector(".stage-tools").classList.remove("tools-hidden");
}

// 内容坐标（原图像素）→ 屏幕坐标定位盒子（裁剪/区域共用）
function renderContentBox(box, x0, y0, x1, y1) {
  const x = Math.min(x0, x1);
  const y = Math.min(y0, y1);
  const w = Math.abs(x1 - x0);
  const h = Math.abs(y1 - y0);
  const p = contentToScreen(x, y);
  const p2 = contentToScreen(x + w, y + h);
  box.style.left = Math.min(p.x, p2.x) + "px";
  box.style.top = Math.min(p.y, p2.y) + "px";
  box.style.width = Math.abs(p2.x - p.x) + "px";
  box.style.height = Math.abs(p2.y - p.y) + "px";
}

function renderCropBox() {
  renderContentBox($("cropBox"), cropState.cx0, cropState.cy0, cropState.cx1, cropState.cy1);
}

// ===================== 区域补画 =====================
// 与裁剪框同用内容坐标（原图像素坐标系），选区只筛选笔画、不修改原图
const regionState = { x0: 0, y0: 0, x1: 0, y1: 0, mode: null, startX: 0, startY: 0, init: { x0: 0, y0: 0, x1: 0, y1: 0 } };

// 屏幕坐标（相对 stage）→ 内容坐标（原图像素）
function screenToContent(sx, sy) {
  const fr = $("imageFrame").getBoundingClientRect();
  const sr = $("stage").getBoundingClientRect();
  // 防御：图片尚未完成布局时 frame 尺寸为 0，避免除零产生 NaN 坐标
  if (fr.width <= 0 || fr.height <= 0) return { x: 0, y: 0 };
  return {
    x: (sx - (fr.left - sr.left)) / fr.width * state.image.width,
    y: (sy - (fr.top - sr.top)) / fr.height * state.image.height,
  };
}

function enterRegion() {
  const box = $("regionBox");
  box.hidden = false;
  $("canvasTransform").classList.add("crop-live");
  document.querySelector(".stage-tools").classList.add("tools-hidden");
  // 已有生效选区时显示该选区，否则默认内容中央 60%
  const iw = state.image.width;
  const ih = state.image.height;
  if (state.region) {
    regionState.x0 = state.region.x;
    regionState.y0 = state.region.y;
    regionState.x1 = state.region.x + state.region.w;
    regionState.y1 = state.region.y + state.region.h;
  } else {
    regionState.x0 = iw * 0.2;
    regionState.y0 = ih * 0.2;
    regionState.x1 = iw * 0.8;
    regionState.y1 = ih * 0.8;
  }
  renderRegionBox();
}

function exitRegion() {
  $("regionBox").hidden = true;
  regionState.mode = null;
  $("canvasTransform").classList.remove("crop-live");
  document.querySelector(".stage-tools").classList.remove("tools-hidden");
}

function renderRegionBox() {
  renderContentBox($("regionBox"), regionState.x0, regionState.y0, regionState.x1, regionState.y1);
}

// 工具栏徽章：显示区域补画命中数；× 退出
function updateRegionBadge() {
  const badge = $("regionBadge");
  if (state.regionInfo) {
    $("regionInfo").textContent = `${state.regionInfo.stroke_count}/${state.regionInfo.total_count} 笔`;
    badge.hidden = false;
  } else {
    badge.hidden = true;
  }
}

function resetRegionUi() {
  state.region = null;
  state.regionInfo = null;
  updateRegionBadge();
}

// 应用选区：Rust 侧筛选笔画，后续 F9 只绘制命中笔画
async function applyRegion() {
  if (!state.image) {
    toast("请先选择图片", "导入图片后才能框选区域。", true);
    return;
  }
  if (state.processing) {
    toast("正在处理图像", "请等待处理完成后再框选区域。", true);
    return;
  }
  if (state.drawing) {
    toast("正在绘制中", "请按 F10 停止后再框选区域。", true);
    return;
  }
  const x = Math.min(regionState.x0, regionState.x1);
  const y = Math.min(regionState.y0, regionState.y1);
  const w = Math.abs(regionState.x1 - regionState.x0);
  const h = Math.abs(regionState.y1 - regionState.y0);
  if (w < 6 || h < 6) {
    toast("选区过小", "请拖动扩大区域范围。", true);
    return;
  }
  const requestClientRevision = state.clientRevision;
  let info;
  try {
    info = await invoke("filter_strokes", {
      x: Math.round(x), y: Math.round(y),
      w: Math.round(w), h: Math.round(h),
    });
  } catch (e) {
    toast("区域筛选失败", String(e), true);
    return;
  }
  // await 期间换图/裁剪：Rust 侧选区已被清空或拒绝（strokes_revision 门禁），
  // 前端静默丢弃过期结果，避免陈旧徽章（换图/裁剪的 toast 已提示工作区变化）
  if (requestClientRevision !== state.clientRevision || !state.image) return;
  // 防御：后端异常返回时避免解引用崩溃
  if (!info || typeof info.stroke_count !== "number" || !info.total_count) {
    toast("区域筛选失败", "接口返回异常，请重试。", true);
    return;
  }
  state.region = { x, y, w, h };
  state.regionInfo = info;
  updateRegionBadge();
  const est = typeof info.estimate_seconds === "number"
    ? formatSeconds(info.estimate_seconds)
    : estimateSecondsForCounts(info.stroke_count, info.point_count);
  toast("区域补画已生效", `选区命中 ${info.stroke_count}/${info.total_count} 笔、${info.point_count} 点${est ? `，预计约 ${est}` : ""}。仅绘制这些笔画，框选其他区域可更换。`);
  setTool(state.tool === "region" ? "fixed" : state.tool);
}

// 退出区域补画（恢复绘制全部笔画）
async function clearRegion() {
  if (state.drawing || state.processing) return;
  try {
    await invoke("clear_strokes_filter");
  } catch (e) {
    toast("退出区域补画失败", String(e), true);
    return;
  }
  resetRegionUi();
  if (state.tool === "region") setTool("fixed");
  toast("已退出区域补画", "恢复绘制全部笔画。");
}

// 应用裁剪：有笔画 → 裁剪笔画；无笔画 → 裁剪原图（内容坐标即原图像素坐标）
async function applyCrop() {
  if (!state.image) {
    toast("请先选择图片", "导入图片后才能裁剪。", true);
    return;
  }
  // 处理中不允许裁剪：Rust 侧 ProcessingGuard 会拒绝，但先拦截避免误导性"裁剪失败"文案
  if (state.processing) {
    toast("正在处理图像", "请等待处理完成后再裁剪。", true);
    return;
  }
  if (state.drawing) {
    toast("正在绘制中", "请按 F10 停止后再裁剪。", true);
    return;
  }
  const cx0 = Math.max(0, Math.round(Math.min(cropState.cx0, cropState.cx1)));
  const cy0 = Math.max(0, Math.round(Math.min(cropState.cy0, cropState.cy1)));
  const cx1 = Math.min(state.image.width, Math.round(Math.max(cropState.cx0, cropState.cx1)));
  const cy1 = Math.min(state.image.height, Math.round(Math.max(cropState.cy0, cropState.cy1)));
  const cw = cx1 - cx0;
  const ch = cy1 - cy0;
  if (cw < 6 || ch < 6) {
    toast("裁剪区域过小", "请拖动四角扩大裁剪范围。", true);
    return;
  }
  // 统一裁剪：总是裁剪原图（画布随之变小）；若有笔画则同步保留框内笔画并平移坐标
  try {
    const info = await invoke("crop_image", { path: state.image.path, x: cx0, y: cy0, w: cw, h: ch });
    applyCropStateUpdate(info);
    toast("裁剪完成", `画布已裁剪为 ${info.width} × ${info.height}，可重新生成笔画。`);
  } catch (e) {
    toast("裁剪失败", String(e), true);
    return;
  }
  setTool(state.tool === "crop" ? "fixed" : state.tool);
  // 裁剪后相册检查：内容一致 → 接上"预览笔画"弹窗；仅来源命中 → 轻提示其他区域记录
  checkGalleryAfterCrop();
}

// 裁剪成功后的统一状态应用（applyCrop 与相册"自动裁剪并载入"共用）
function applyCropStateUpdate(info) {
  // 裁剪已提交：提示弹窗（若有）展示的是裁剪前图层的命中，一并失效
  closeGalleryPrompt();
  state.image = info;
  state.clientRevision += 1;
  state.image.needsRegen = true; // 原图已变，Rust 侧已清空旧笔画
  state.outcome = null;
  state.lineKey = null;
  state.lineKeyBase = null;
  state.mode = "idle"; // 与 applyImportedImage/clearCanvas 一致：裁剪后回到初始状态
  resetRegionUi(); // 裁剪改变原图：区域筛选失效
  $("strokeCount").textContent = "0";
  $("pointCount").textContent = "0";
  // 清理滚轮防抖残留，避免旧记录 300ms 后误入撤销栈
  if (wheelTimer) { clearTimeout(wheelTimer); wheelTimer = null; }
  wheelStart = null;
  // 裁剪改变了画布坐标系：旧视图历史不再适用（与 clearCanvas 一致）
  state.undoStack.length = 0;
  fit();
  applyImageMeta(info);
  $("lineImage").removeAttribute("src");
  updateCanvasControls();
  renderState();
}

// 裁剪后相册检查：内容一致 → 接上"预览笔画"弹窗；仅来源命中 → 轻提示其他区域记录
async function checkGalleryAfterCrop() {
  const image = state.image;
  if (!image || !image.source_hash || !image.content_hash) return;
  const requestClientRevision = state.clientRevision;
  let match;
  try {
    match = await invoke("gallery_check", { sourceHash: image.source_hash, contentHash: image.content_hash });
  } catch {
    return; // 查询失败静默：不影响裁剪主流程
  }
  if (!match) return;
  // await 期间工作区可能已切换：丢弃过期结果（与 galleryCheckAndPrompt 同守卫）
  if (requestClientRevision !== state.clientRevision
      || state.image?.content_hash !== image.content_hash) return;
  if (match.exact) {
    renderGalleryPrompt(match);
    openGalleryPrompt(match.exact.image_hash);
    return;
  }
  const labels = (match.variants || []).slice(0, 2).map((v) => cropLabel(v) || "全图").join("、");
  toast("相册有此图其他区域的笔画",
    `还有 ${match.variants.length} 组记录（${labels}${match.variants.length > 2 ? " 等" : ""}），可在相册查看或一键复现。`);
}

// ===================== 主操作 =====================
// 主按钮只负责"生成笔画"；开始/停止绘制统一走 F9/F10（全局热键或快捷键卡片）
async function primaryAction() {
  if (state.mode === "processing") return;
  if (state.drawing || state.mode === "drawing") {
    toast("正在绘制中", "请按 F10 停止后再生成笔画。", true);
    return;
  }
  await processImage();
}

async function processImage() {
  if (!state.image) {
    toast("请先选择图片", "点击左侧「选择」导入需要转换的图像。", true);
    return;
  }
  if (state.processing) return;
  state.processing = true;
  state.mode = "processing";
  setProcessingControlsDisabled(true);
  startIndeterminate(); // 内部统一负责 progress 显示
  updateCanvasControls();
  renderState();
  try {
    // 生成前强制提交当前配置，避免滑块 change 事件排队时后端仍使用旧值。
    if (!await flushConfig()) {
      // 配置保存失败时没有进入 Rust 处理流程，需撤销本次 processing UI。
      state.mode = state.outcome ? "ready" : "idle";
      stopProgress();
      renderState();
      return;
    }
    const requestRevision = state.clientRevision;
    const outcome = await invoke("process_image");
    const outcomeRevision = Number(outcome.revision);
    if (requestRevision !== state.clientRevision
        || !state.image
        || outcomeRevision !== Number(state.image.revision)) {
      throw new Error("处理结果已过期，请重新生成");
    }
    state.outcome = outcome;
    // 同 revision 替换 outcome（重生成不递增 revision）：显式失效线稿预览缓存，
    // 避免"笔画/点数相同而坐标不同"的重生成沿用旧画布（与换图/裁剪/清空同模式）
    state.lineKey = null;
    state.lineKeyBase = null;
    if (state.image) state.image.needsRegen = false; // 已基于当前画布重新生成
    resetRegionUi(); // 笔画已重新生成：区域筛选失效（选区基于旧笔画）
    $("strokeCount").textContent = outcome.stroke_count;
    $("pointCount").textContent = outcome.point_count.toLocaleString();
    drawLineImage();
    state.mode = "ready";
    stopProgress();
    renderState();
    const estimate = estimateSeconds();
    toast("笔画已生成", `共 ${outcome.stroke_count} 笔、${outcome.point_count.toLocaleString()} 个采样点${estimate ? `，预计耗时约 ${estimate}` : ""}。`);
    // AI 开启但 AI 线稿化失败已回退普通管线：明确告知用户，避免误判质量成因
    if (outcome.ai_fallback) {
      toast("AI 处理失败已回退", "AI 线稿化失败（网络/Key/额度），本次使用普通管线生成。", true, 4000);
    }
  } catch (e) {
    // 处理失败时保留上一份有效结果，避免界面与 Rust 侧仍可绘制的旧笔画分裂。
    state.mode = state.outcome ? "ready" : "idle";
    stopProgress();
    renderState();
    toast("重新生成失败", state.outcome
      ? `仍保留上一次的 ${state.outcome.stroke_count} 笔结果。${String(e)}`
      : String(e), true);
  } finally {
    state.processing = false;
    setProcessingControlsDisabled(false);
    // 恢复时按 Key 配置重算清除按钮可用性（全量恢复会把"无 Key 时的禁用"冲掉）
    $("aiKeyClear").disabled = !(state.aiConfig && state.aiConfig.api_key_set);
    updateCanvasControls();
  }
}

// ===================== 事件绑定 =====================
function bindEvents() {
  // 折叠分区
  document.querySelectorAll(".section-header").forEach((header) => {
    header.addEventListener("click", () => {
      const section = header.closest(".section");
      const closed = section.classList.toggle("closed");
      header.setAttribute("aria-expanded", String(!closed));
    });
  });
  // 工作流步骤跳转
  document.querySelectorAll(".workflow-step").forEach((btn) => {
    btn.addEventListener("click", () => {
      const section = $(btn.dataset.scroll);
      if (section) {
        section.classList.remove("closed");
        section.querySelector(".section-header").setAttribute("aria-expanded", "true");
        section.scrollIntoView({ behavior: "smooth", block: "start" });
      }
    });
  });

  // 滑块
  document.querySelectorAll("input[type=range]").forEach((input) => {
    updateRange(input);
    input.addEventListener("input", () => {
      updateRange(input);
      applySliderToConfig(input.dataset.key, Number(input.value));
      presetControlApi?.markDirty();
      scheduleConfigSync(); // 实时同步内存配置，Shift+F9 预演立即生效
    });
    input.addEventListener("change", () => {
      persistConfig();
    });
  });

  // 参数显示框：直接输入数字调整（实时改滑块与配置，失焦保存）
  document.querySelectorAll(".value-badge input").forEach((bi) => {
    bi.addEventListener("input", () => {
      const slider = bi.closest(".control-row")?.querySelector("input[type=range]");
      if (!slider) return;
      const v = parseFloat(bi.value);
      if (Number.isNaN(v)) return;
      const min = parseFloat(slider.min), max = parseFloat(slider.max);
      const clamped = Math.min(max, Math.max(min, v));
      slider.value = clamped;
      updateRange(slider);
      applySliderToConfig(slider.dataset.key, clamped);
      presetControlApi?.markDirty();
      scheduleConfigSync(); // 键入数值同样实时生效（无需先失焦）
    });
    bi.addEventListener("blur", () => {
      const slider = bi.closest(".control-row")?.querySelector("input[type=range]");
      if (slider) updateRange(slider); // 恢复为滑块当前格式化值
      persistConfig();
    });
    bi.addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); bi.blur(); }
    });
  });

  // AI 开关
  $("aiSwitch").addEventListener("click", async (e) => {
    if (state.processing) return;
    const next = e.currentTarget.getAttribute("aria-checked") !== "true";
    e.currentTarget.setAttribute("aria-checked", String(next));
    state.config.use_ai = next;
    if (!await persistConfig()) return;
    toast(next ? "AI 预处理已开启" : "AI 预处理已关闭",
      next ? "将优先识别主体轮廓和连续路径。" : "恢复基础线稿处理流程。");
  });

  // 选图
  $("chooseFile").addEventListener("click", pickImage);

  // 视图切换
  $("viewTabs").addEventListener("click", (e) => {
    const btn = e.target.closest("[data-view]");
    if (!btn) return;
    document.querySelectorAll(".view-button").forEach((x) => x.classList.toggle("active", x === btn));
    const frame = $("imageFrame");
    frame.classList.remove("view-original", "view-strokes", "view-compare");
    frame.classList.add("view-" + btn.dataset.view);
  });

  // 对照分割线拖动
  $("imageFrame").addEventListener("pointermove", (e) => {
    if (!$("imageFrame").classList.contains("view-compare")) return;
    const r = $("imageFrame").getBoundingClientRect();
    root.style.setProperty("--split", Math.max(4, Math.min(96, ((e.clientX - r.left) / r.width) * 100)) + "%");
  });

  // 清空画布
  $("clearButton").addEventListener("click", clearCanvas);

  // 缩放（以光标为中心；delta=+1 放大 / -1 缩小）
  $("zoomIn").addEventListener("click", () => zoomStep(1));
  $("zoomOut").addEventListener("click", () => zoomStep(-1));
  $("fitButton").addEventListener("click", fit);
  // 滚轮缩放：防抖合并入栈（一次连续滚动只记录一条撤销，避免惯性/多事件产生碎栈）
  $("stage").addEventListener("wheel", (e) => {
    e.preventDefault();
    if (!state.image) return; // 无图时滚动不记录撤销，避免空操作污染撤销栈
    if (!wheelStart) {
      wheelStart = { zoom: state.zoom, tx: state.tx, ty: state.ty }; // 记录滚动起点
    }
    clearTimeout(wheelTimer);
    wheelTimer = setTimeout(() => {
      if (wheelStart) {
        // 与当前状态无实质差异（缩放边界钳制/净零滚动）时不入栈：
        // 入栈只产生一条"撤销无效果"的死条目（与 zoomStep 的边界守卫同一容差）
        const changed = Math.abs(wheelStart.zoom - state.zoom) > 1e-9
          || Math.abs(wheelStart.tx - state.tx) > 1e-9
          || Math.abs(wheelStart.ty - state.ty) > 1e-9;
        if (changed) {
          state.undoStack.push(wheelStart);
          if (state.undoStack.length > 50) state.undoStack.shift();
        }
        wheelStart = null;
      }
    }, 300);
    zoomAt(e.clientX, e.clientY, state.zoom + (e.deltaY < 0 ? 0.08 : -0.08));
  }, { passive: false });
  $("stage").addEventListener("dblclick", (e) => {
    // crop 模式下双击 = 应用裁剪；region 模式下双击 = 应用区域；其他 = 适应窗口
    if (state.tool === "crop" && cropHit(e)) applyCrop();
    else if (state.tool === "region") applyRegion();
    else fit();
  });

  // 画布工具
  document.querySelector(".stage-tools").addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    if (btn.dataset.tool) setTool(btn.dataset.tool);
    if (btn.dataset.action === "undo") undoView();
  });

  // 画布指针交互：平移拖拽 / 裁剪框 / 聚焦悬停
  const stage = $("stage");
  const cropHit = (e) => {
    const box = $("cropBox");
    const r = box.getBoundingClientRect();
    const T = 12;
    const x = e.clientX;
    const y = e.clientY;
    if (Math.abs(x - r.left) <= T && Math.abs(y - r.top) <= T) return "nw";
    if (Math.abs(x - r.right) <= T && Math.abs(y - r.top) <= T) return "ne";
    if (Math.abs(x - r.right) <= T && Math.abs(y - r.bottom) <= T) return "se";
    if (Math.abs(x - r.left) <= T && Math.abs(y - r.bottom) <= T) return "sw";
    if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return "move";
    return null;
  };
  stage.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    // 按钮区域点击不进入画布交互——否则 pointer capture 会吞掉按钮的 click 事件
    if (e.target.closest("button")) return;
    if (state.tool === "pan") {
      // 暂存拖拽前视图状态，实际发生位移时才入撤销栈（避免点一下没拖也污染栈）
      state.dragUndo = { zoom: state.zoom, tx: state.tx, ty: state.ty };
      state.dragging = true;
      state.startX = e.clientX; state.startY = e.clientY;
      state.startTx = state.tx; state.startTy = state.ty;
      stage.classList.add("dragging");
      stage.setPointerCapture(e.pointerId);
    } else if (state.tool === "crop") {
      const hit = cropHit(e);
      if (!hit) return;
      cropState.mode = hit;
      cropState.startX = e.clientX;
      cropState.startY = e.clientY;
      cropState.init = {
        cx0: cropState.cx0, cy0: cropState.cy0,
        cx1: cropState.cx1, cy1: cropState.cy1,
      };
      stage.setPointerCapture(e.pointerId);
    } else if (state.tool === "region") {
      // 在现有选区内按下 = 移动；框外按下 = 重新框选
      const r = $("regionBox").getBoundingClientRect();
      const inside = e.clientX >= r.left && e.clientX <= r.right
        && e.clientY >= r.top && e.clientY <= r.bottom;
      regionState.mode = inside ? "move" : "new";
      regionState.startX = e.clientX;
      regionState.startY = e.clientY;
      if (inside) {
        regionState.init = {
          x0: regionState.x0, y0: regionState.y0,
          x1: regionState.x1, y1: regionState.y1,
        };
      } else {
        const p = screenToContent(e.clientX, e.clientY);
        regionState.x0 = Math.max(0, Math.min(state.image.width, p.x));
        regionState.y0 = Math.max(0, Math.min(state.image.height, p.y));
        regionState.x1 = regionState.x0;
        regionState.y1 = regionState.y0;
      }
      stage.setPointerCapture(e.pointerId);
    }
  });
  stage.addEventListener("pointermove", (e) => {
    if (state.dragging) {
      state.tx = state.startTx + (e.clientX - state.startX);
      state.ty = state.startTy + (e.clientY - state.startY);
      applyTransform();
      return;
    }
    if (state.tool === "crop" && cropState.mode) {
      // 屏幕增量 → 内容增量：按显示比例映射（frame rect 已含 zoom），固定 1:1 视觉
      const fr = $("imageFrame").getBoundingClientRect();
      const scaleX = state.image.width / fr.width;
      const scaleY = state.image.height / fr.height;
      const dx = (e.clientX - cropState.startX) * scaleX;
      const dy = (e.clientY - cropState.startY) * scaleY;
      const m = cropState.mode;
      let { cx0, cy0, cx1, cy1 } = cropState.init;
      if (m === "move") {
        cx0 += dx; cy0 += dy; cx1 += dx; cy1 += dy;
      } else {
        if (m.includes("e")) cx1 += dx;
        if (m.includes("s")) cy1 += dy;
        if (m.includes("w")) cx0 += dx;
        if (m.includes("n")) cy0 += dy;
      }
      // 限制在原图范围内：尺寸与位置都 clamp（先限制尺寸，再限制位置）
      const iw = state.image.width;
      const ih = state.image.height;
      const left = Math.min(cx0, cx1), right = Math.max(cx0, cx1);
      const top = Math.min(cy0, cy1), bottom = Math.max(cy0, cy1);
      let w = Math.min(Math.max(right - left, 24), iw);
      let h = Math.min(Math.max(bottom - top, 24), ih);
      const nl = Math.max(0, Math.min(iw - w, left));
      const nt = Math.max(0, Math.min(ih - h, top));
      cropState.cx0 = nl;
      cropState.cy0 = nt;
      cropState.cx1 = nl + w;
      cropState.cy1 = nt + h;
      renderCropBox();
      return;
    }
    if (state.tool === "region" && regionState.mode) {
      if (regionState.mode === "move") {
        // 屏幕增量 → 内容增量：按显示比例映射（frame rect 已含 zoom）
        const fr = $("imageFrame").getBoundingClientRect();
        const scaleX = state.image.width / fr.width;
        const scaleY = state.image.height / fr.height;
        const dx = (e.clientX - regionState.startX) * scaleX;
        const dy = (e.clientY - regionState.startY) * scaleY;
        let { x0, y0, x1, y1 } = regionState.init;
        x0 += dx; y0 += dy; x1 += dx; y1 += dy;
        const iw = state.image.width, ih = state.image.height;
        // 先归一化（反向拖拽时 x1 < x0），再统一钳制：
        // 否则 w 为负时 iw - w > iw，上界钳制失效，选区可被拖出图片范围
        const left = Math.min(x0, x1), right = Math.max(x0, x1);
        const top = Math.min(y0, y1), bottom = Math.max(y0, y1);
        const w = right - left, h = bottom - top;
        const nl = Math.max(0, Math.min(iw - w, left));
        const nt = Math.max(0, Math.min(ih - h, top));
        regionState.x0 = nl;
        regionState.y0 = nt;
        regionState.x1 = nl + w;
        regionState.y1 = nt + h;
      } else {
        const p = screenToContent(e.clientX, e.clientY);
        regionState.x1 = Math.max(0, Math.min(state.image.width, p.x));
        regionState.y1 = Math.max(0, Math.min(state.image.height, p.y));
      }
      renderRegionBox();
      return;
    }
  });
  stage.addEventListener("pointerup", (e) => {
    if (state.dragging) {
      const moved = Math.abs(e.clientX - state.startX) + Math.abs(e.clientY - state.startY) > 3;
      if (moved && state.dragUndo) pushUndo(state.dragUndo); // 实际拖动了才记录
      state.dragUndo = null;
      state.dragging = false;
      stage.classList.remove("dragging");
      // 防御：捕获已被 lostpointercapture 释放时不再抛异常
      if (stage.hasPointerCapture(e.pointerId)) stage.releasePointerCapture(e.pointerId);
      return;
    }
    if (state.tool === "crop" && cropState.mode) {
      cropState.mode = null;
      if (stage.hasPointerCapture(e.pointerId)) stage.releasePointerCapture(e.pointerId);
    }
    if (state.tool === "region" && regionState.mode) {
      regionState.mode = null;
      if (stage.hasPointerCapture(e.pointerId)) stage.releasePointerCapture(e.pointerId);
    }
  });
  const cancelPointerInteraction = () => {
    state.dragging = false;
    state.dragUndo = null;
    stage.classList.remove("dragging");
    cropState.mode = null;
    regionState.mode = null;
  };
  stage.addEventListener("pointercancel", cancelPointerInteraction);
  stage.addEventListener("lostpointercapture", cancelPointerInteraction);
  // 裁剪确认/取消按钮（按钮点击已被 pointerdown 排除，不触发画布交互）
  $("cropConfirm").addEventListener("click", () => {
    if (state.tool === "crop") applyCrop();
  });
  $("cropCancel").addEventListener("click", () => {
    if (state.tool === "crop") setTool("fixed");
  });
  // 区域补画确认/取消/退出
  $("regionConfirm").addEventListener("click", () => {
    if (state.tool === "region") applyRegion();
  });
  $("regionCancel").addEventListener("click", () => {
    if (state.tool === "region") setTool("fixed");
  });
  $("regionBadgeClear").addEventListener("click", clearRegion);

  // 侧栏折叠
  $("sidebarToggle").addEventListener("click", () => {
    const hidden = $("shell").classList.toggle("sidebar-hidden");
    const icon = $("sidebarToggleIcon");
    icon.className = hidden ? "iconoir-sidebar-expand" : "iconoir-sidebar-collapse";
    $("sidebarToggle").dataset.tooltip = hidden ? "展开参数面板" : "收起参数面板";
  });

  // 主操作（F9/F10 由全局热键处理，按钮卡片不响应点击）
  $("primaryAction").addEventListener("click", primaryAction);

  // 重置
  $("resetButton").addEventListener("click", async () => {
    if (state.processing || state.drawing) {
      // 绘制/处理中给出明确反馈，而不是静默忽略
      toast("当前无法重置", state.drawing ? "正在绘制中，请先按 F10 停止。" : "正在处理图像，请稍候。", true);
      return;
    }
    const cfg = await resetConfigPersisted();
    if (!cfg) return;
    renderAll();
    presetControlApi?.markDirty();
    toast("参数已重置", "处理参数已恢复，主题、网格和画布底色保持不变。");
  });

  // 参数预设：支持命名保存、切换、删除，并兼容旧版单个预设
  presetControlApi = initPresetControl();
  initWorldProfiles();

  // 设置弹窗
  $("btnSettings").addEventListener("click", openSettings);
  $("closeSettings").addEventListener("click", closeSettings);
  $("modalMask").addEventListener("click", closeSettings);

  // 笔画相册
  $("btnGallery").addEventListener("click", openGallery);
  $("galleryClose").addEventListener("click", closeGallery);
  $("galleryMask").addEventListener("click", closeGallery);
  $("galleryPromptClose").addEventListener("click", closeGalleryPrompt);
  $("galleryPromptMask").addEventListener("click", closeGalleryPrompt);
  $("galleryPromptPreview").addEventListener("click", async () => {
    const hash = state.galleryPromptHash;
    closeGalleryPrompt();
    if (hash) await galleryRestore(hash);
  });
  $("galleryPromptRegenerate").addEventListener("click", () => {
    closeGalleryPrompt();
    processImage();
  });
  const settingsContent = $("settingsContent");
  document.querySelectorAll(".settings-nav-item").forEach((item) => {
    item.addEventListener("click", (e) => {
      e.preventDefault();
      const targetId = item.getAttribute("href")?.slice(1);
      if (!targetId) return;
      setSettingsNavActive(targetId);
      $(targetId)?.scrollIntoView({ behavior: "smooth", block: "start" });
    });
  });
  settingsContent.addEventListener("scroll", scheduleSettingsNavSync, { passive: true });
  if ("ResizeObserver" in window) {
    new ResizeObserver(scheduleSettingsNavSync).observe(settingsContent);
  }
  scheduleSettingsNavSync();
  bindHelpTips();

  // 主题 / 画布 / 网格
  $("themeDark").addEventListener("click", () => saveTheme(true));
  $("themeLight").addEventListener("click", () => saveTheme(false));
  $("canvasLight").addEventListener("click", () => saveCanvasMode("light"));
  $("canvasDark").addEventListener("click", () => saveCanvasMode("dark"));
  $("gridSwitch").addEventListener("click", (e) => {
    if (state.processing) return;
    const previous = state.config.show_grid ?? true;
    const next = e.currentTarget.getAttribute("aria-checked") !== "true";
    e.currentTarget.setAttribute("aria-checked", String(next));
    state.config.show_grid = next;
    applyCanvasBg(); // 立即切换画布棋盘格显隐
    persistConfig().then((saved) => {
      if (!saved && sameData(state.config.show_grid, next)) {
        state.config.show_grid = previous;
        e.currentTarget.setAttribute("aria-checked", String(previous));
        applyCanvasBg();
      }
    });
  });

  // AI 设置输入
  AI_FIELD_IDS.forEach((id) => {
    $(id).addEventListener("change", () => saveAiInput(id));
  });
  // 显示/隐藏 API Key（密码框 ↔ 明文）
  $("aiKeyToggle").addEventListener("click", toggleKeyVisibility);
  $("aiKeyClear").addEventListener("click", clearApiKey);
  // 获取可用模型列表 → 填充输入框下拉建议
  $("fetchModelsBtn").addEventListener("click", fetchAiModels);
  $("testAiBtn").addEventListener("click", testAi);

  // GitHub 链接
  $("linkGithub").addEventListener("click", (e) => {
    e.preventDefault();
    openUrl("https://github.com/cocokoishi/vrchat-drawing-script")
      .catch((err) => toast("打开链接失败", String(err), true));
  });

  // 新手教程：打开/关闭/勾选"以后不再显示"/设置页重新查看
  $("tutorialStart").addEventListener("click", () => closeTutorial());
  $("tutorialClose").addEventListener("click", () => closeTutorial());
  $("tutorialMask").addEventListener("click", () => closeTutorial());
  $("tutorialNoShow").addEventListener("change", (e) => {
    try {
      if (e.target.checked) {
        localStorage.setItem(TUTORIAL_STORAGE_KEY, "1");
        toast("以后不再显示", "如果有需要可以在设置中再次打开哦。");
      } else {
        localStorage.removeItem(TUTORIAL_STORAGE_KEY);
      }
    } catch (_) { /* 忽略存储异常 */ }
  });
  // 教程章节导航：滚动到对应卡片，并同步当前章节状态
  const tutorialNavItems = [...document.querySelectorAll(".tutorial-nav-item")];
  const tutorialBody = document.querySelector(".tutorial-body");
  const tutorialModal = $("tutorialModal");
  const tutorialNavTargets = tutorialNavItems.map((item) => {
    const targetId = item.getAttribute("href")?.slice(1);
    const target = targetId ? document.getElementById(targetId) : null;
    return target?.closest(".tutorial-section") || null;
  });
  const setTutorialNavActive = (activeIndex) => {
    tutorialNavItems.forEach((navItem, index) => {
      navItem.classList.toggle("is-active", index === activeIndex);
    });
  };
  const syncTutorialNav = () => {
    state.tutorialNavFrame = 0;
    if (!tutorialBody || tutorialModal?.hidden || !tutorialBody.clientHeight || !tutorialNavItems.length) return;
    const bodyRect = tutorialBody.getBoundingClientRect();
    // 以正文顶部略下方作为“当前章节”判定线，避免下一张卡片刚露出就抢先高亮。
    const marker = bodyRect.top + 30;
    let activeIndex = 0;
    tutorialNavTargets.forEach((section, index) => {
      if (section && section.getBoundingClientRect().top <= marker) activeIndex = index;
    });
    // 滚到底部时，最后一个章节卡片可能不足以滚到判定线，仍应高亮最后一项。
    if (tutorialBody.scrollTop + tutorialBody.clientHeight >= tutorialBody.scrollHeight - 2) {
      activeIndex = tutorialNavItems.length - 1;
    }
    setTutorialNavActive(activeIndex);
  };
  const scheduleTutorialNavSync = () => {
    if (state.tutorialNavFrame) return;
    state.tutorialNavFrame = requestAnimationFrame(syncTutorialNav);
  };
  tutorialNavItems.forEach((item) => {
    item.addEventListener("click", (e) => {
      const targetId = item.getAttribute("href")?.slice(1);
      const target = targetId ? document.getElementById(targetId) : null;
      const body = target?.closest(".tutorial-body");
      const section = target?.closest(".tutorial-section");
      if (!target || !body || !section) return;
      e.preventDefault();
      const bodyRect = body.getBoundingClientRect();
      const sectionRect = section.getBoundingClientRect();
      const scrollTop = body.scrollTop + sectionRect.top - bodyRect.top - 14;
      body.scrollTo({ top: Math.max(0, scrollTop), behavior: "smooth" });
      setTutorialNavActive(tutorialNavItems.indexOf(item));
    });
  });
  tutorialBody?.addEventListener("scroll", scheduleTutorialNavSync, { passive: true });
  if (tutorialBody && "ResizeObserver" in window) {
    new ResizeObserver(scheduleTutorialNavSync).observe(tutorialBody);
  }
  window.addEventListener("resize", scheduleTutorialNavSync, { passive: true });
  state.tutorialNavSync = scheduleTutorialNavSync;
  scheduleTutorialNavSync();
  $("tutorialReopen").addEventListener("click", () => openTutorial());
  // 弹窗内键盘：Esc 关闭、Tab 焦点循环（stopPropagation 避免与设置弹窗处理冲突）
  $("tutorialModal").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      closeTutorial();
      return;
    }
    if (e.key === "Tab") {
      const modal = $("tutorialModal");
      const focusable = [...modal.querySelectorAll(
        'button, [href], input, [tabindex]:not([tabindex="-1"])')];
      if (!focusable.length) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;
      if (e.shiftKey) {
        if (active === first || !modal.contains(active)) {
          e.preventDefault();
          e.stopPropagation();
          last.focus();
        }
      } else if (active === last || !modal.contains(active)) {
        e.preventDefault();
        e.stopPropagation();
        first.focus();
      }
    }
  });

  // 快捷键
  document.addEventListener("keydown", (e) => {
    // 教程/预设命名/删除确认/相册提示弹窗打开时：拦截其余快捷键（Ctrl+V、R 等），
    // 避免在弹窗背后误操作主界面；Esc 视打开者关闭对应弹窗
    if (!$("tutorialModal").hidden || !$("presetNameModal").hidden || !$("presetConfirmModal").hidden || !$("galleryPromptModal").hidden) {
      if (e.key === "Escape") {
        e.preventDefault();
        if (!$("tutorialModal").hidden) closeTutorial();
        else if (!$("galleryPromptModal").hidden) closeGalleryPrompt();
        else if (!$("presetNameModal").hidden) getSharedDialogs().nameDialog.close();
        else getSharedDialogs().confirmDialog.close();
        return;
      }
      // 预设命名/删除确认/相册提示弹窗：与设置/相册一致的 Tab 焦点循环（类同 pageModal 分支）。
      // 相册提示弹窗的变体按钮可能整行禁用，选择器排除 disabled 防止圈边 last.focus() 落空
      const trappedModal = !$("presetNameModal").hidden
        ? $("presetNameModal")
        : (!$("presetConfirmModal").hidden
          ? $("presetConfirmModal")
          : (!$("galleryPromptModal").hidden ? $("galleryPromptModal") : null));
      if (trappedModal && e.key === "Tab") {
        const focusable = [...trappedModal.querySelectorAll(
          'button:not([disabled]), [href], input, select, textarea, [tabindex]:not([tabindex="-1"])')]
          .filter(el => el.offsetParent !== null); // 排除 display:none（如 exact 缺席时隐藏的预览按钮）
        if (focusable.length) {
          const first = focusable[0];
          const last = focusable[focusable.length - 1];
          const active = document.activeElement;
          if (e.shiftKey) {
            if (active === first || active === trappedModal || !trappedModal.contains(active)) {
              e.preventDefault();
              last.focus();
            }
          } else if (active === last || active === trappedModal || !trappedModal.contains(active)) {
            e.preventDefault();
            first.focus();
          }
        }
        return;
      }
      return;
    }
    // 设置/相册页面弹窗打开时：Tab 在弹窗内循环（焦点陷阱）；Esc 优先关闭弹窗
    const pageModal = !$("settingsModal").hidden ? "settings" : (!$("galleryModal").hidden ? "gallery" : null);
    if (pageModal) {
      if (e.key === "Escape") {
        e.preventDefault();
        if (pageModal === "settings") closeSettings();
        else closeGallery();
        return;
      }
      if (e.key === "Tab") {
        // 焦点移出下拉面板前先收起（面板 Portal 在 body，Tab 循环不会自动关闭它）
        aiEndpointSelect.close();
        aiModelCombobox.close();
        const modal = pageModal === "settings" ? $("settingsModal") : $("galleryModal");
        const focusable = modal.querySelectorAll(
          'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])');
        if (!focusable.length) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        const active = document.activeElement;
        if (e.shiftKey) {
          if (active === first || !modal.contains(active)) {
            e.preventDefault();
            last.focus();
          }
        } else if (active === last || !modal.contains(active)) {
          e.preventDefault();
          first.focus();
        }
        return;
      }
      return;
    }
    if (e.target instanceof Element && e.target.matches("input")) {
      // 输入框内按 Esc：先失焦，便于快速返回画布交互
      if (e.key === "Escape") { e.target.blur(); return; }
      return;
    }
    if (e.repeat) return; // 忽略按住键的重复触发，避免 pushUndo/缩放栈被连续刷入
    const k = e.key.toLowerCase();
    // Ctrl+V：从剪贴板粘贴图片（输入框内已被上方 input 分支拦截，不冲突）
    if ((e.ctrlKey || e.metaKey) && k === "v") {
      e.preventDefault();
      pasteClipboardImage();
      return;
    }
    // R：进入区域补画工具（与画布"区域"按钮一致；处理/绘制中忽略）
    // 排除 Ctrl/Win/Alt 修饰键：Ctrl+R 是常见条件反射（且可能触发 WebView2 刷新），不得误入工具
    if (k === "r" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      if (!state.drawing && !state.processing) setTool("region");
      return;
    }
    if (!state.image) return; // 未选择图片时画布固定（禁用快捷键）
    // 画布工具（平移/查看/裁剪/区域/适应/缩放）均有对应按钮，不绑定快捷键
    if (e.key === "Escape") {
      if (state.tool === "crop" || state.tool === "region") { setTool("fixed"); return; }
      closeSettings();
    }
  });

  applyTransform();
  renderState();
  syncToolUI();
}

// ===================== 窗口控制（自绘标题栏） =====================
function bindWindowControls() {
  const runWindowAction = async (action, title) => {
    try {
      await action();
    } catch (e) {
      toast(title, String(e), true);
    }
  };
  $("winMin").addEventListener("click", () => runWindowAction(
    () => getCurrentWindow().minimize(),
    "窗口最小化失败",
  ));
  $("winMax").addEventListener("click", () => runWindowAction(async () => {
    const w = getCurrentWindow();
    if (await w.isMaximized()) await w.unmaximize();
    else await w.maximize();
  }, "窗口状态切换失败"));
  $("winClose").addEventListener("click", () => runWindowAction(
    () => getCurrentWindow().close(),
    "窗口关闭失败",
  ));
}

// ===================== 图片 =====================
// 导入图片后的统一状态应用（对话框/拖拽/剪贴板共用）
async function applyImportedImage(info, footerText, title, copy) {
  // 复位画布工具与相册提示弹窗：crop/region 模式下换图会残留旧框（cropBox/regionBox
  // 可见但坐标系已失效）；提示弹窗展示的是旧图的命中（变体按钮按旧图坐标系计算，
  // 拖拽导入可在弹窗打开期间触发换图），换图必须一并失效
  closeGalleryPrompt();
  if (state.tool === "crop") exitCrop();
  if (state.tool === "region") exitRegion();
  state.tool = "pan";
  syncToolUI();
  state.image = info;
  state.clientRevision += 1;
  state.image.needsRegen = false;
  state.outcome = null;
  state.lineKey = null;
  state.lineKeyBase = null;
  state.mode = "idle";
  resetRegionUi(); // 新图片：区域筛选失效
  // 清理滚轮防抖残留，避免旧记录 300ms 后误入撤销栈
  if (wheelTimer) { clearTimeout(wheelTimer); wheelTimer = null; }
  wheelStart = null;
  // 视图历史属于上一张图，换图后清空（与 clearCanvas 一致）
  state.undoStack.length = 0;
  fit();
  applyImageMeta(info);
  $("strokeCount").textContent = "0";
  $("pointCount").textContent = "0";
  $("lineImage").removeAttribute("src");
  updateCanvasControls();
  renderState(footerText);
  toast(title, copy);
  // 相册匹配：这张图片已有保存的笔画时弹提示（预览 / 重新生成）
  await galleryCheckAndPrompt(info);
}

// 导入图片统一入口（对话框/拖拽/剪贴板共用）：处理/绘制守卫 + invoke + 应用
async function loadImageIntoWorkspace(fetchPromise, opts) {
  if (state.processing) {
    toast("正在处理图像", `请等待处理完成后再${opts.action}。`, true);
    return;
  }
  if (state.drawing) {
    toast("正在绘制中", `请先按 F10 停止绘制再${opts.action}。`, true);
    return;
  }
  let info;
  try {
    info = await fetchPromise();
  } catch (e) {
    toast(opts.errorTitle, String(e), true);
    return;
  }
  if (!info) {
    if (opts.noneText) toast(opts.noneText.title, opts.noneText.copy);
    return; // 用户取消 / 剪贴板无图片
  }
  await applyImportedImage(info, opts.footer, opts.title, opts.copy);
}

async function pickImage() {
  await loadImageIntoWorkspace(() => invoke("pick_image"), {
    action: "选择新图片",
    errorTitle: "加载图片失败",
    footer: "新图片已载入，可生成笔画",
    title: "图片已替换",
    copy: "缩略图与画布预览已同步更新。",
  });
}

// 拖拽导入（拖入窗口的文件路径）
async function importImage(path) {
  await loadImageIntoWorkspace(() => invoke("import_image", { path }), {
    action: "导入图片",
    errorTitle: "图片导入失败",
    footer: "新图片已载入，可生成笔画",
    title: "图片已导入",
    copy: "拖拽导入成功。",
  });
}

// 剪贴板粘贴导入（Ctrl+V）
async function pasteClipboardImage() {
  await loadImageIntoWorkspace(() => invoke("import_clipboard_image"), {
    action: "粘贴图片",
    errorTitle: "剪贴板导入失败",
    noneText: { title: "剪贴板中没有图片", copy: "请先复制一张图片再按 Ctrl+V。" },
    footer: "剪贴板图片已载入，可生成笔画",
    title: "图片已导入",
    copy: "已从剪贴板载入图片。",
  });
}

// ===================== 清空画布 =====================
async function clearCanvas() {
  if (state.drawing || state.processing) return; // 绘制/处理中不允许清空
  try {
    await invoke("clear_workspace");
  } catch (e) {
    toast("清空失败", String(e), true);
    return;
  }
  // 复位画布工具、裁剪框与相册提示弹窗：crop/region 模式清空画布后框状态残留
  // 会导致下次进入死锁；提示弹窗的命中基于已清空的图片，一并失效
  closeGalleryPrompt();
  if (state.tool === "crop") exitCrop();
  if (state.tool === "region") exitRegion();
  state.tool = "pan";
  syncToolUI();
  // 清理滚轮防抖残留
  if (wheelTimer) { clearTimeout(wheelTimer); wheelTimer = null; }
  wheelStart = null;
  // 重置前端状态
  state.image = null;
  state.clientRevision += 1;
  state.outcome = null;
  state.lineKey = null;
  state.lineKeyBase = null;
  resetRegionUi(); // 清空画布：区域筛选失效
  state.zoom = 1;
  state.tx = 0;
  state.ty = 0;
  state.undoStack.length = 0;
  applyTransform();
  clearImageMeta();
  $("strokeCount").textContent = "0";
  $("pointCount").textContent = "0";
  state.mode = "idle";
  updateCanvasControls();
  renderState("画布已清空，可导入新图片");
  toast("画布已清空", "图片与笔画已移除。");
}

// ===================== 笔画相册 =====================
// 后端在 F9 有效发起时按图片内容哈希自动保存笔画；这里负责导入时提示、相册页管理。
function formatGalleryTime(savedAt) {
  const time = new Date(Number(savedAt) / 1e6);
  const pad = (n) => String(n).padStart(2, "0");
  return `${time.getFullYear()}-${pad(time.getMonth() + 1)}-${pad(time.getDate())} ${pad(time.getHours())}:${pad(time.getMinutes())}`;
}

// 裁剪区域显示标签（"裁剪 x,y · w×h"；全图返回 null）
function cropLabel(entry) {
  return entry.crop
    ? `裁剪 ${entry.crop.x},${entry.crop.y} · ${entry.crop.w}×${entry.crop.h}`
    : null;
}

// 目标条目的裁剪区域能否从当前工作区通过（跨）裁剪到达：
// 未裁剪视为全图界；目标矩形完全落在当前区域内即像素一致 ⇒ 内容哈希一致 ⇒ 可载入
function canReachByCrop(entry, image) {
  if (!entry.crop || !image) return false;
  const cur = image.crop_rect ?? { x: 0, y: 0, w: image.width, h: image.height };
  return entry.crop.x >= cur.x && entry.crop.y >= cur.y
      && entry.crop.x + entry.crop.w <= cur.x + cur.w
      && entry.crop.y + entry.crop.h <= cur.y + cur.h;
}

// 渲染相册提示弹窗：exact = 当前内容哈希的精确条目（两键流程）；
// variants = 同一来源图的其他变体（每行一键复现，不可达时置灰）
function renderGalleryPrompt(match) {
  const exact = match.exact || null;
  const variants = match.variants || [];
  state.galleryPromptHash = exact ? exact.image_hash : null;
  if (exact) {
    const aiNote = exact.use_ai
      ? `，AI 预处理${exact.ai_fallback ? "（AI 已回退）" : ""}`
      : "";
    $("galleryPromptSubtitle").textContent = `${exact.image_name} · ${exact.image_size}`;
    $("galleryPromptCopy").textContent =
      `相册中已保存这张图片的笔画（${formatGalleryTime(exact.saved_at)} 生成，` +
      `${exact.stroke_count} 笔 / ${exact.point_count.toLocaleString()} 点${aiNote}）。` +
      `可以预览并直接绘制，或按当前参数重新生成。`;
    $("galleryPromptPreview").hidden = false;
  } else {
    const first = variants[0];
    $("galleryPromptSubtitle").textContent = first ? `${first.image_name} · ${first.image_size}` : "";
    $("galleryPromptCopy").textContent =
      "相册中有这张来源图片的笔画，但没有与当前工作区内容一致的版本。" +
      "可选择下方变体一键复现（自动裁剪并载入），或按当前参数重新生成。";
    $("galleryPromptPreview").hidden = true;
  }
  const list = $("galleryPromptVariants");
  list.replaceChildren();
  list.hidden = variants.length === 0;
  if (variants.length) {
    const note = document.createElement("div");
    note.className = "gallery-prompt-note";
    note.textContent = exact ? `另有 ${variants.length} 个其他区域的版本：` : "可一键复现的版本：";
    list.appendChild(note);
    for (const variant of variants) {
      list.appendChild(buildGalleryPromptVariantRow(variant));
    }
  }
}

function buildGalleryPromptVariantRow(variant) {
  const row = document.createElement("div");
  row.className = "gallery-item";
  const thumb = document.createElement("img");
  thumb.className = "gallery-thumb";
  thumb.alt = variant.image_name;
  if (variant.thumbnail) thumb.src = variant.thumbnail;
  const meta = document.createElement("div");
  meta.className = "gallery-meta";
  const name = document.createElement("div");
  name.className = "gallery-name";
  name.textContent = cropLabel(variant) || "全图";
  const sub = document.createElement("div");
  sub.className = "gallery-sub";
  sub.textContent = `${variant.stroke_count} 笔 / ${variant.point_count.toLocaleString()} 点 · ${formatGalleryTime(variant.saved_at)}`;
  meta.append(name, sub);
  const actions = document.createElement("div");
  actions.className = "gallery-actions";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "gallery-load-btn";
  if (canReachByCrop(variant, state.image)) {
    btn.innerHTML = '<i class="iconoir-crop"></i><span>自动裁剪并载入</span>';
    btn.addEventListener("click", () => {
      closeGalleryPrompt();
      autoCropAndRestore(variant);
    });
  } else {
    btn.disabled = true;
    btn.title = "目标区域不在当前工作区内，请先导入原图后再一键复现";
    btn.innerHTML = '<i class="iconoir-crop"></i><span>需先导入原图</span>';
  }
  actions.append(btn);
  row.append(thumb, meta, actions);
  return row;
}

// 导入后检查相册：按来源图归组命中（无需重裁到与上次一致即可提醒）
async function galleryCheckAndPrompt(info) {
  if (!info || !info.source_hash || !info.content_hash) return;
  const requestClientRevision = state.clientRevision;
  let match;
  try {
    match = await invoke("gallery_check", { sourceHash: info.source_hash, contentHash: info.content_hash });
  } catch {
    return; // 查询失败静默：不影响导入主流程
  }
  if (!match) return;
  // await 期间工作区可能已切换（两次导入的查询乱序完成）：丢弃过期结果，
  // 避免为新图弹出旧图的命中提示、或按旧图坐标系渲染变体按钮
  if (requestClientRevision !== state.clientRevision
      || state.image?.content_hash !== info.content_hash) return;
  renderGalleryPrompt(match);
  openGalleryPrompt(match.exact ? match.exact.image_hash : null);
}

function openGalleryPrompt(hash) {
  const mask = $("galleryPromptMask");
  const modal = $("galleryPromptModal");
  if (state.galleryPromptTimer) { clearTimeout(state.galleryPromptTimer); state.galleryPromptTimer = null; }
  if (state.galleryPromptFocusTimer) { clearTimeout(state.galleryPromptFocusTimer); state.galleryPromptFocusTimer = null; }
  state.galleryPromptPrevFocus = document.activeElement;
  state.galleryPromptHash = hash;
  mask.hidden = false;
  modal.hidden = false;
  requestAnimationFrame(() => {
    mask.classList.add("show");
    modal.classList.add("show");
    // exact 存在 → 聚焦"预览笔画"；仅变体时聚焦第一个可达变体按钮，
    // 全部不可达则落弹窗容器（tabindex="-1"）——避免对 display:none 的
    // 隐藏按钮 focus() 静默无效导致焦点滞留弹窗外、Tab 逃逸到背景
    const preview = $("galleryPromptPreview");
    if (!preview.hidden) {
      preview.focus();
    } else {
      const reachable = $("galleryPromptVariants").querySelector(".gallery-load-btn:not(:disabled)");
      (reachable || modal).focus();
    }
  });
}

function closeGalleryPrompt() {
  const mask = $("galleryPromptMask");
  const modal = $("galleryPromptModal");
  mask.classList.remove("show");
  modal.classList.remove("show");
  if (state.galleryPromptTimer) clearTimeout(state.galleryPromptTimer);
  state.galleryPromptTimer = setTimeout(() => {
    mask.hidden = true;
    modal.hidden = true;
    state.galleryPromptTimer = null;
  }, 200);
  const focusTarget = state.galleryPromptPrevFocus;
  state.galleryPromptPrevFocus = null;
  if (focusTarget && focusTarget.focus) {
    // 焦点恢复定时器同样登记：快速重开时必须取消旧回调，否则焦点被拽回已关闭弹窗背后的元素
    if (state.galleryPromptFocusTimer) clearTimeout(state.galleryPromptFocusTimer);
    state.galleryPromptFocusTimer = setTimeout(() => {
      state.galleryPromptFocusTimer = null;
      focusTarget.focus();
    }, 210);
  }
}

// 从相册恢复笔画到当前工作区（后端校验图片哈希一致）
async function galleryRestore(hash) {
  if (state.processing) {
    toast("正在处理图像", "请等待处理完成后再载入笔画。", true);
    return false;
  }
  if (state.drawing) {
    toast("正在绘制中", "请按 F10 停止后再载入笔画。", true);
    return false;
  }
  const requestClientRevision = state.clientRevision;
  const requestImageHash = state.image?.content_hash ?? null;
  let outcome;
  try {
    outcome = await invoke("gallery_restore", { hash });
  } catch (e) {
    toast("载入笔画失败", String(e), true);
    return false;
  }
  // await 期间工作区可能已切换（如拖拽导入另一张图）：Rust 侧状态始终自洽
  // （换图会清空笔画与选区），前端仅丢弃过期结果，避免旧图笔画回填到新图工作区
  if (requestClientRevision !== state.clientRevision || !state.image
      || state.image.content_hash !== requestImageHash) {
    toast("已取消载入", "工作区图片已切换，相册笔画未载入。");
    return false;
  }
  state.outcome = outcome;
  // 恢复同样属于"同 revision 替换 outcome"：失效预览缓存保证画布重绘
  state.lineKey = null;
  state.lineKeyBase = null;
  if (state.image) state.image.needsRegen = false;
  resetRegionUi(); // 恢复笔画：区域筛选失效（与重新生成一致）
  $("strokeCount").textContent = outcome.stroke_count;
  $("pointCount").textContent = outcome.point_count.toLocaleString();
  drawLineImage();
  state.mode = "ready";
  renderState();
  const estimate = estimateSeconds();
  toast("笔画已从相册载入",
    `共 ${outcome.stroke_count} 笔、${outcome.point_count.toLocaleString()} 个采样点` +
    `${estimate ? `，预计耗时约 ${estimate}` : ""}。`);
  return true;
}

// 一键复现：按条目记录的裁剪区域（换算为当前工作区的子矩形）自动裁剪并载入笔画。
// 正确性支点：目标区域像素与条目生成时一致 ⇒ 重编码字节相同 ⇒ 内容哈希一致 ⇒
// gallery_restore 的既有校验自然通过，坐标系安全不依赖新字段。
async function autoCropAndRestore(variant) {
  if (!variant || !variant.crop || !state.image) return false;
  if (state.processing) {
    toast("正在处理图像", "请等待处理完成后再载入笔画。", true);
    return false;
  }
  if (state.drawing) {
    toast("正在绘制中", "请按 F10 停止后再载入笔画。", true);
    return false;
  }
  // 弹窗展示期间工作区可能已换图（拖拽导入/乱序提示）：点击时实时重算同源与可达性，
  // 把错误裁剪挡在发起之前——crop_image 在后端提交后即无法撤销，事后哈希校验只能放弃载入
  if (variant.source_hash && variant.source_hash !== state.image.source_hash) {
    toast("载入已跳过", "工作区图片已切换，与相册记录不再同源。");
    return false;
  }
  if (!canReachByCrop(variant, state.image)) {
    toast("区域超出当前裁剪", "该记录的目标区域不在当前工作区内，请先导入原图后再一键复现。", true);
    return false;
  }
  const cur = state.image.crop_rect ?? { x: 0, y: 0 };
  const sub = {
    x: variant.crop.x - cur.x,
    y: variant.crop.y - cur.y,
    w: variant.crop.w,
    h: variant.crop.h,
  };
  let info;
  try {
    info = await invoke("crop_image", {
      path: state.image.path, x: sub.x, y: sub.y, w: sub.w, h: sub.h,
    });
  } catch (e) {
    toast("自动裁剪失败", String(e), true);
    return false;
  }
  applyCropStateUpdate(info);
  if (info.content_hash !== variant.image_hash) {
    // 防御：同源同区域的重编码应逐字节一致；不一致说明来源内容已漂移，放弃载入
    toast("载入已跳过", "自动裁剪结果与相册记录不一致。", true);
    return false;
  }
  return galleryRestore(variant.image_hash);
}

function openGallery() {
  if (state.galleryTimer) { clearTimeout(state.galleryTimer); state.galleryTimer = null; }
  if (state.galleryFocusTimer) { clearTimeout(state.galleryFocusTimer); state.galleryFocusTimer = null; }
  state.galleryPrevFocus = document.activeElement;
  window.__vrcClosePresetMenu?.();
  $("galleryMask").hidden = false;
  $("galleryModal").hidden = false;
  requestAnimationFrame(() => {
    $("galleryMask").classList.add("show");
    $("galleryModal").classList.add("show");
    $("galleryClose").focus();
  });
  refreshGalleryList();
}

function closeGallery() {
  $("galleryMask").classList.remove("show");
  $("galleryModal").classList.remove("show");
  if (state.galleryTimer) clearTimeout(state.galleryTimer);
  state.galleryTimer = setTimeout(() => {
    $("galleryMask").hidden = true;
    $("galleryModal").hidden = true;
    state.galleryTimer = null;
  }, 200);
  const focusTarget = state.galleryPrevFocus;
  state.galleryPrevFocus = null;
  if (focusTarget && focusTarget.focus) {
    if (state.galleryFocusTimer) clearTimeout(state.galleryFocusTimer);
    state.galleryFocusTimer = setTimeout(() => {
      state.galleryFocusTimer = null;
      focusTarget.focus();
    }, 210);
  }
}

async function refreshGalleryList() {
  let list = [];
  try {
    list = await invoke("gallery_list");
  } catch (e) {
    toast("读取相册失败", String(e), true);
  }
  const el = $("galleryList");
  el.replaceChildren();
  $("galleryEmpty").hidden = list.length > 0;
  for (const entry of list) {
    const card = document.createElement("div");
    card.className = "gallery-item";

    const thumb = document.createElement("img");
    thumb.className = "gallery-thumb";
    thumb.alt = entry.image_name;
    if (entry.thumbnail) thumb.src = entry.thumbnail;

    const meta = document.createElement("div");
    meta.className = "gallery-meta";
    const nameRow = document.createElement("div");
    nameRow.className = "gallery-name";
    const nameSpan = document.createElement("span");
    nameSpan.textContent = entry.image_name;
    nameRow.appendChild(nameSpan);
    if (entry.use_ai) {
      const badge = document.createElement("span");
      badge.className = "gallery-ai-badge";
      badge.textContent = entry.ai_fallback ? "AI 回退" : "AI";
      nameRow.appendChild(badge);
    }
    const sub = document.createElement("div");
    sub.className = "gallery-sub";
    const subParts = [
      ["", `${entry.stroke_count}`],
      [" 笔 / ", `${entry.point_count.toLocaleString()}`],
      [" 点 · ", formatGalleryTime(entry.saved_at)],
      [" · ", entry.image_size],
      ...(entry.crop
        ? [[" · 裁剪 ", `${entry.crop.x},${entry.crop.y} ${entry.crop.w}×${entry.crop.h}`]]
        : []),
    ];
    for (const [text, bold] of subParts) {
      sub.appendChild(document.createTextNode(text));
      const b = document.createElement("b");
      b.textContent = bold;
      sub.appendChild(b);
    }
    meta.append(nameRow, sub);

    const actions = document.createElement("div");
    actions.className = "gallery-actions";
    const loadBtn = document.createElement("button");
    loadBtn.type = "button";
    loadBtn.className = "gallery-load-btn";
    loadBtn.innerHTML = '<i class="iconoir-eye"></i><span>载入笔画</span>';
    loadBtn.title = "载入笔画到当前工作区（需先导入同一张图片）";
    loadBtn.addEventListener("click", () => onGalleryLoad(entry));
    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.className = "gallery-delete-btn";
    delBtn.innerHTML = '<i class="iconoir-trash"></i><span>删除</span>';
    delBtn.title = `删除${entry.image_name}的笔画`;
    delBtn.addEventListener("click", () => onGalleryDelete(entry));
    actions.append(loadBtn, delBtn);

    card.append(thumb, meta, actions);
    el.appendChild(card);
  }
}

async function onGalleryLoad(entry) {
  if (!state.image) {
    toast("请先导入这张图片", "相册笔画与当前工作区图片不匹配，请先导入对应图片。", true);
    return;
  }
  if (state.image.content_hash !== entry.image_hash) {
    // 来源相同 → 尽量提供自动裁剪复现；否则按可达性给出明确提示
    if (entry.source_hash && entry.source_hash === state.image.source_hash) {
      if (canReachByCrop(entry, state.image)) {
        const ok = await autoCropAndRestore(entry);
        if (ok) closeGallery();
      } else {
        toast("区域超出当前裁剪", "该记录的目标区域不在当前工作区内，请先导入原图后一键复现。", true);
      }
    } else {
      toast("请先导入这张图片", "相册笔画与当前工作区图片不匹配，请先导入对应图片。", true);
    }
    return;
  }
  const ok = await galleryRestore(entry.image_hash);
  if (ok) closeGallery();
}

function onGalleryDelete(entry) {
  const { confirmDialog } = getSharedDialogs();
  confirmDialog.open(entry.image_name, async () => {
    try {
      await invoke("gallery_delete", { hash: entry.image_hash });
    } catch (e) {
      toast("删除失败", String(e), true);
      return;
    }
    toast("已删除", "该相册条目已移除。");
    refreshGalleryList();
  }, { title: "删除相册条目" });
}

// ===================== 新手教程 =====================
// v2：更换存储键，清除早期测试在本机残留的 "1"（曾导致"未勾选也不弹出"）
const TUTORIAL_STORAGE_KEY = "vrc_tutorial_seen_v2";

// 打开教程弹窗（设置页"查看教程"入口强制打开，不检查存储标记）
function openTutorial() {
  const mask = $("tutorialMask");
  const modal = $("tutorialModal");
  if (!mask || !modal) return;
  // 取消尚未执行的关闭定时器，防止快速重开时被旧定时器把 hidden 改回 true
  if (state.tutorialTimer) { clearTimeout(state.tutorialTimer); state.tutorialTimer = null; }
  if (state.tutorialFocusTimer) { clearTimeout(state.tutorialFocusTimer); state.tutorialFocusTimer = null; }
  state.tutorialPrevFocus = document.activeElement;
  // 勾选框反映真实存储状态（勾选过则显示勾选，取消勾选即恢复每次弹出）
  try {
    $("tutorialNoShow").checked = localStorage.getItem(TUTORIAL_STORAGE_KEY) === "1";
  } catch (_) {
    $("tutorialNoShow").checked = false;
  }
  mask.hidden = false;
  modal.hidden = false;
  requestAnimationFrame(() => {
    mask.classList.add("show");
    modal.classList.add("show");
    state.tutorialNavSync?.();
    $("tutorialStart").focus();
  });
}

function closeTutorial() {
  const mask = $("tutorialMask");
  const modal = $("tutorialModal");
  mask.classList.remove("show");
  modal.classList.remove("show");
  if (state.tutorialTimer) clearTimeout(state.tutorialTimer);
  state.tutorialTimer = setTimeout(() => {
    mask.hidden = true;
    modal.hidden = true;
    state.tutorialTimer = null;
  }, 200);
  // 恢复关闭前的焦点（从设置打开时回到设置弹窗内）
  const focusTarget = state.tutorialPrevFocus;
  state.tutorialPrevFocus = null;
  if (focusTarget && focusTarget.focus) {
    if (state.tutorialFocusTimer) clearTimeout(state.tutorialFocusTimer);
    state.tutorialFocusTimer = setTimeout(() => {
      state.tutorialFocusTimer = null;
      focusTarget.focus();
    }, 210);
  }
}

// 首次启动（未勾选"以后不再显示"）时弹出教程
function maybeOpenTutorial() {
  try {
    if (localStorage.getItem(TUTORIAL_STORAGE_KEY) === "1") return;
  } catch (_) { /* 存储异常时照常展示 */ }
  openTutorial();
}

// ===================== 设置 =====================
function openSettings() {
  // 取消尚未执行的关闭定时器，防止快速重开时被旧定时器把 hidden 改回 true
  if (state.modalTimer) { clearTimeout(state.modalTimer); state.modalTimer = null; }
  $("modalMask").hidden = false;
  $("settingsModal").hidden = false;
  state.prevFocus = document.activeElement;
  window.__vrcClosePresetMenu?.();
  const tooltip = $("helpTooltip");
  if (tooltip) {
    tooltip.classList.remove("show");
    tooltip.hidden = true;
  }
  requestAnimationFrame(() => {
    $("modalMask").classList.add("show");
    $("settingsModal").classList.add("show");
    scheduleSettingsNavSync();
    $("settingsModal").querySelector("button, input, [href]")?.focus();
  });
}

function closeSettings() {
  // 关闭设置前先收起已展开的下拉面板（面板已 Portal 到 body，不关会悬浮在主界面）
  aiEndpointSelect.close();
  aiModelCombobox.close();
  $("modalMask").classList.remove("show");
  $("settingsModal").classList.remove("show");
  if (state.modalTimer) clearTimeout(state.modalTimer);
  state.modalTimer = setTimeout(() => {
    $("modalMask").hidden = true;
    $("settingsModal").hidden = true;
    state.modalTimer = null;
  }, 200);
  // 恢复关闭前的焦点
  if (state.prevFocus && state.prevFocus.focus) state.prevFocus.focus();
  state.prevFocus = null;
}

// 设置页滚动联动：当前滚动区域的卡片标题进入顶部阅读带后，对应左侧分类高亮。
function setSettingsNavActive(sectionId) {
  document.querySelectorAll(".settings-nav-item").forEach((item) => {
    const active = item.getAttribute("href") === `#${sectionId}`;
    item.classList.toggle("active", active);
    if (active) item.setAttribute("aria-current", "true");
    else item.removeAttribute("aria-current");
  });
}

function syncSettingsNav() {
  const content = $("settingsContent");
  if (!content) return;
  const sections = [...content.querySelectorAll(".settings-card")];
  if (!sections.length) return;
  const marker = content.getBoundingClientRect().top + 42;
  let current = sections[0];
  for (const section of sections) {
    if (section.getBoundingClientRect().top <= marker) current = section;
    else break;
  }
  // 最后一张卡片通常短于可视区，滚到底部时其顶部可能永远到不了 marker。
  if (content.scrollTop + content.clientHeight >= content.scrollHeight - 2) {
    current = sections[sections.length - 1];
  }
  setSettingsNavActive(current.id);
}

function scheduleSettingsNavSync() {
  if (state.settingsNavFrame) return;
  state.settingsNavFrame = requestAnimationFrame(() => {
    state.settingsNavFrame = 0;
    syncSettingsNav();
  });
}

// 参数说明提示：使用 body 下的固定定位层，避免被侧栏滚动容器裁切
const PRESET_STORAGE_KEY = "vrc_presets";
const LEGACY_PRESET_STORAGE_KEY = "vrc_preset";

function clonePresetData(config) {
  return {
    image: { ...(config?.image || {}) },
    contour: { ...(config?.contour || {}) },
    drawing: { ...(config?.drawing || {}) },
  };
}

function normalizePreset(raw, index = 0) {
  if (!raw || typeof raw !== "object") return null;
  if (!raw.image && !raw.contour && !raw.drawing) return null;
  return {
    id: String(raw.id || `preset-${Date.now()}-${index}`),
    name: String(raw.name || `预设 ${index + 1}`).trim() || `预设 ${index + 1}`,
    updatedAt: Number(raw.updatedAt) || Date.now(),
    ...clonePresetData(raw),
  };
}

function readPresetList() {
  let stored = null;
  try { stored = JSON.parse(localStorage.getItem(PRESET_STORAGE_KEY) || "null"); } catch (_) { /* 使用迁移/空列表 */ }
  if (Array.isArray(stored)) {
    return stored.map((item, index) => normalizePreset(item, index)).filter(Boolean).slice(0, 12);
  }

  let legacy = null;
  try { legacy = JSON.parse(localStorage.getItem(LEGACY_PRESET_STORAGE_KEY) || "null"); } catch (_) { /* 忽略损坏的旧预设 */ }
  const migrated = normalizePreset(legacy, 0);
  if (!migrated) return [];
  migrated.name = "我的预设";
  try { localStorage.setItem(PRESET_STORAGE_KEY, JSON.stringify([migrated])); } catch (_) { /* 稍后保存时再提示 */ }
  return [migrated];
}

function writePresetList(list) {
  try {
    localStorage.setItem(PRESET_STORAGE_KEY, JSON.stringify(list));
    return true;
  } catch (e) {
    toast("预设保存失败", String(e), true);
    return false;
  }
}

function initPresetNameDialog() {
  const mask = $("presetNameMask");
  const modal = $("presetNameModal");
  const input = $("presetNameInput");
  const confirmButton = $("presetNameConfirm");
  const cancelButton = $("presetNameCancel");
  const closeButton = $("presetNameClose");
  if (!mask || !modal || !input || !confirmButton || !cancelButton || !closeButton) {
    return { open: () => {}, close: () => {} };
  }

  let open = false;
  let closeTimer = null;
  let previousFocus = null;
  let onConfirm = null;

  function close({ restoreFocus = true } = {}) {
    if (!open) return;
    open = false;
    modal.classList.remove("show");
    mask.classList.remove("show");
    clearTimeout(closeTimer);
    closeTimer = setTimeout(() => {
      if (!open) {
        modal.hidden = true;
        mask.hidden = true;
      }
    }, 200);
    const focusTarget = previousFocus;
    previousFocus = null;
    if (restoreFocus && focusTarget?.focus) setTimeout(() => focusTarget.focus(), 210);
  }

  function openDialog(defaultName, callback, opts = {}) {
    clearTimeout(closeTimer);
    open = true;
    onConfirm = callback;
    previousFocus = document.activeElement;
    // 弹窗标题可被"世界配置档"复用（预设调用时不传，保持默认文案）
    const titleEl = $("presetNameTitle");
    const subtitleEl = $("presetNameSubtitle");
    if (titleEl) titleEl.textContent = opts.title || "保存预设";
    if (subtitleEl) subtitleEl.textContent = opts.subtitle || "为当前参数组合命名";
    input.value = defaultName;
    input.classList.remove("invalid");
    mask.hidden = false;
    modal.hidden = false;
    requestAnimationFrame(() => {
      mask.classList.add("show");
      modal.classList.add("show");
    });
    setTimeout(() => { input.focus(); input.select(); }, 50);
  }

  function submit() {
    const value = input.value.trim();
    if (!value) {
      input.classList.add("invalid");
      input.focus();
      return;
    }
    if (onConfirm) onConfirm(value);
    close();
  }

  confirmButton.addEventListener("click", submit);
  cancelButton.addEventListener("click", () => close());
  closeButton.addEventListener("click", () => close());
  mask.addEventListener("click", () => close());
  input.addEventListener("input", () => input.classList.remove("invalid"));
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submit(); }
    if (e.key === "Escape") { e.preventDefault(); close(); }
  });
  return { open: openDialog, close };
}

function initPresetConfirmDialog() {
  const mask = $("presetConfirmMask");
  const modal = $("presetConfirmModal");
  const copy = $("presetConfirmCopy");
  const okButton = $("presetConfirmOk");
  const cancelButton = $("presetConfirmCancel");
  const closeButton = $("presetConfirmClose");
  if (!mask || !modal || !copy || !okButton || !cancelButton || !closeButton) {
    return { open: () => {}, close: () => {} };
  }

  let visible = false;
  let closeTimer = null;
  let previousFocus = null;
  let onConfirm = null;

  function close({ restoreFocus = true } = {}) {
    if (!visible) return;
    visible = false;
    modal.classList.remove("show");
    mask.classList.remove("show");
    clearTimeout(closeTimer);
    closeTimer = setTimeout(() => {
      if (!visible) {
        modal.hidden = true;
        mask.hidden = true;
      }
    }, 200);
    const focusTarget = previousFocus;
    previousFocus = null;
    if (restoreFocus && focusTarget?.focus) setTimeout(() => focusTarget.focus(), 210);
  }

  function openDialog(name, callback, opts = {}) {
    clearTimeout(closeTimer);
    visible = true;
    onConfirm = callback;
    previousFocus = document.activeElement;
    // 标题/副标题/文案可被其他调用方（如相册删除）覆盖，未传时恢复默认
    const titleEl = $("presetConfirmTitle");
    const subtitleEl = modal.querySelector(".modal-subtitle");
    if (titleEl) titleEl.textContent = opts.title || "删除预设";
    if (subtitleEl) subtitleEl.textContent = opts.subtitle || "此操作无法撤销";
    copy.textContent = opts.copy || `确定删除“${name}”吗？删除后无法恢复。`;
    mask.hidden = false;
    modal.hidden = false;
    requestAnimationFrame(() => {
      mask.classList.add("show");
      modal.classList.add("show");
      okButton.focus();
    });
  }

  function confirm() {
    const callback = onConfirm;
    close();
    if (callback) callback();
  }

  okButton.addEventListener("click", confirm);
  cancelButton.addEventListener("click", () => close());
  closeButton.addEventListener("click", () => close());
  mask.addEventListener("click", () => close());
  modal.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); close(); }
    if (e.key === "Enter" && document.activeElement === okButton) { e.preventDefault(); confirm(); }
  });
  return { open: openDialog, close };
}

function initPresetControl() {
  const button = $("presetButton");
  const menu = $("presetMenu");
  const list = $("presetList");
  const empty = $("presetEmpty");
  const count = $("presetCount");
  const saveButton = $("presetSaveButton");
  const wrap = $("presetControl");
  if (!button || !menu || !list || !empty || !count || !saveButton || !wrap) return null;

  // 名称/确认弹窗与"世界配置档"共用（同一组 DOM 元素，避免重复绑定）
  const { nameDialog, confirmDialog } = getSharedDialogs();
  let presets = readPresetList();
  let activePresetId = null;
  let open = false;

  function positionMenu() {
    const rect = button.getBoundingClientRect();
    const width = menu.offsetWidth || 280;
    const left = Math.max(12, Math.min(rect.left, window.innerWidth - width - 12));
    let top = rect.top - menu.offsetHeight - 8;
    menu.classList.remove("open-down");
    if (top < 12) {
      top = rect.bottom + 8;
      menu.classList.add("open-down");
    }
    menu.style.left = `${Math.round(left)}px`;
    menu.style.top = `${Math.round(Math.min(top, window.innerHeight - menu.offsetHeight - 12))}px`;
  }

  function closeMenu({ restoreFocus = false } = {}) {
    if (!open) return;
    open = false;
    wrap.classList.remove("is-open");
    button.setAttribute("aria-expanded", "false");
    menu.classList.remove("show");
    setTimeout(() => { if (!open) menu.hidden = true; }, 150);
    if (restoreFocus) button.focus();
  }

  async function applyPreset(preset) {
    Object.assign(state.config.image, preset.image || {});
    Object.assign(state.config.contour, preset.contour || {});
    Object.assign(state.config.drawing, preset.drawing || {});
    closeMenu();
    if (!await persistConfig()) {
      renderAll();
      return;
    }
    activePresetId = preset.id;
    renderAll();
    renderList();
    toast("预设已应用", `已切换到“${preset.name}”。`);
  }

  function renderList() {
    list.replaceChildren();
    count.textContent = `${presets.length} 个预设`;
    empty.hidden = presets.length > 0;
    presets.forEach((preset) => {
      const row = document.createElement("div");
      row.className = "preset-item";
      row.setAttribute("role", "presentation");

      const select = document.createElement("button");
      select.type = "button";
      select.className = "preset-item-select";
      select.setAttribute("role", "option");
      select.setAttribute("aria-selected", String(preset.id === activePresetId));
      select.title = `应用${preset.name}`;
      const name = document.createElement("span");
      name.className = "preset-item-name";
      name.textContent = preset.name;
      const check = document.createElement("i");
      check.className = "iconoir-check preset-check";
      check.hidden = preset.id !== activePresetId;
      select.append(name, check);
      select.addEventListener("click", () => applyPreset(preset));

      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "preset-delete";
      remove.setAttribute("aria-label", `删除${preset.name}`);
      remove.title = `删除${preset.name}`;
      remove.innerHTML = '<span class="preset-delete-glyph" aria-hidden="true">×</span>';
      remove.addEventListener("click", () => {
        confirmDialog.open(preset.name, () => {
          const previous = presets;
          const previousActive = activePresetId;
          presets = presets.filter((item) => item.id !== preset.id);
          if (activePresetId === preset.id) activePresetId = null;
          if (!writePresetList(presets)) {
            presets = previous;
            activePresetId = previousActive;
            return;
          }
          renderList();
          if (open) positionMenu();
        });
      });

      row.append(select, remove);
      list.appendChild(row);
    });
    if (open) positionMenu();
  }

  function openMenu() {
    if (open) return;
    open = true;
    if (menu.parentElement !== document.body) document.body.appendChild(menu);
    wrap.classList.add("is-open");
    button.setAttribute("aria-expanded", "true");
    menu.hidden = false;
    renderList();
    positionMenu();
    requestAnimationFrame(() => { if (open) menu.classList.add("show"); });
  }

  saveButton.addEventListener("click", () => {
    const defaultName = `预设 ${presets.length + 1}`;
    closeMenu({ restoreFocus: true });
    nameDialog.open(defaultName, (name) => {
      const existing = presets.find((preset) => preset.name === name);
      const previousPresets = presets;
      const previousActive = activePresetId;
      const next = {
        ...(existing || { id: `preset-${Date.now()}-${Math.random().toString(36).slice(2, 8)}` }),
        name,
        updatedAt: Date.now(),
        ...clonePresetData(state.config),
      };
      let nextPresets;
      if (existing) nextPresets = presets.map((preset) => preset.id === existing.id ? next : preset);
      else {
        if (presets.length >= 12) {
          toast("预设数量已达上限", "最多保存 12 个预设，请先删除不用的预设。", true);
          return;
        }
        nextPresets = [next, ...presets];
      }
      if (!writePresetList(nextPresets)) {
        presets = previousPresets;
        activePresetId = previousActive;
        return;
      }
      presets = nextPresets;
      activePresetId = next.id;
      renderList();
      toast(existing ? "预设已更新" : "预设已保存", `“${name}”已保存当前参数。`);
    });
  });

  button.addEventListener("click", (e) => {
    e.stopPropagation();
    open ? closeMenu() : openMenu();
  });
  document.addEventListener("pointerdown", (e) => {
    if (open && !button.contains(e.target) && !menu.contains(e.target)) closeMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (open && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      closeMenu({ restoreFocus: true });
    }
  });
  window.addEventListener("resize", () => closeMenu());
  document.addEventListener("scroll", (e) => {
    if (open && !menu.contains(e.target)) closeMenu();
  }, true);
  window.__vrcClosePresetMenu = () => {
    closeMenu();
    nameDialog.close({ restoreFocus: false });
    confirmDialog.close({ restoreFocus: false });
  };
  function markDirty() {
    if (activePresetId === null) return;
    activePresetId = null;
    if (open) renderList();
  }
  renderList();
  return { markDirty, closeMenu };
}

// ===================== 绘制策略 / 世界配置档 =====================
// 共享的名称输入/删除确认弹窗实例（预设与配置档共用同一组 DOM）
let sharedNameDialog = null;
let sharedConfirmDialog = null;
function getSharedDialogs() {
  if (!sharedNameDialog) sharedNameDialog = initPresetNameDialog();
  if (!sharedConfirmDialog) sharedConfirmDialog = initPresetConfirmDialog();
  return { nameDialog: sharedNameDialog, confirmDialog: sharedConfirmDialog };
}

const WORLD_PROFILE_STORAGE_KEY = "vrc_world_profiles";
// 绘制策略：只改节奏参数（点间隔/步长/抬笔），不改线稿几何与灵敏度
const DRAW_STRATEGIES = {
  stable: { draw_speed: 0.024, max_step_px: 3, lift_pen_delay: 0.08, start_delay: 1.5, lift_pen_speed: 50 },
  standard: { draw_speed: 0.016, max_step_px: 4, lift_pen_delay: 0.05, start_delay: 1.5, lift_pen_speed: 100 },
  fast: { draw_speed: 0.016, max_step_px: 6, lift_pen_delay: 0.04, start_delay: 1.0, lift_pen_speed: 100 },
};

function normalizeWorldProfile(raw, index = 0) {
  if (!raw || typeof raw !== "object" || !raw.drawing || typeof raw.drawing !== "object") return null;
  return {
    id: String(raw.id || `profile-${Date.now()}-${index}`),
    name: String(raw.name || `配置档 ${index + 1}`).trim() || `配置档 ${index + 1}`,
    updatedAt: Number(raw.updatedAt) || Date.now(),
    drawing: { ...raw.drawing },
  };
}

function readWorldProfiles() {
  let stored = null;
  try { stored = JSON.parse(localStorage.getItem(WORLD_PROFILE_STORAGE_KEY) || "null"); } catch (_) { /* 忽略损坏数据 */ }
  if (Array.isArray(stored)) {
    return stored.map((item, index) => normalizeWorldProfile(item, index)).filter(Boolean).slice(0, 12);
  }
  return [];
}

function writeWorldProfiles(list) {
  try {
    localStorage.setItem(WORLD_PROFILE_STORAGE_KEY, JSON.stringify(list));
    return true;
  } catch (e) {
    toast("配置档保存失败", String(e), true);
    return false;
  }
}

function initWorldProfiles() {
  const strategyRow = $("strategyRow");
  const list = $("profileList");
  const empty = $("profileEmpty");
  const count = $("profileCount");
  const saveButton = $("profileSaveButton");
  if (!strategyRow || !list || !empty || !count || !saveButton) return;
  const { nameDialog, confirmDialog } = getSharedDialogs();
  let profiles = readWorldProfiles();

  function closeMenu() { presetControlApi?.closeMenu?.(); }

  async function applyDrawing(params) {
    Object.assign(state.config.drawing, params);
    closeMenu();
    if (!await persistConfig()) {
      renderAll();
      return false;
    }
    presetControlApi?.markDirty(); // 绘制参数已变：取消参数预设的选中高亮
    renderAll();
    return true;
  }

  // 内置绘制策略（稳定/标准/快速）
  strategyRow.querySelectorAll(".strategy-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const strategy = DRAW_STRATEGIES[btn.dataset.strategy];
      if (!strategy) return;
      applyDrawing(strategy).then((ok) => {
        if (ok) toast("策略已应用", `已切换到「${btn.textContent.trim()}」节奏。`);
      });
    });
  });

  function renderList() {
    list.replaceChildren();
    count.textContent = `${profiles.length} 个`;
    empty.hidden = profiles.length > 0;
    profiles.forEach((profile) => {
      const row = document.createElement("div");
      row.className = "preset-item";
      row.setAttribute("role", "presentation");

      const select = document.createElement("button");
      select.type = "button";
      select.className = "preset-item-select";
      select.setAttribute("role", "option");
      select.title = `应用${profile.name}`;
      const name = document.createElement("span");
      name.className = "preset-item-name";
      name.textContent = profile.name;
      select.append(name);
      select.addEventListener("click", () => {
        applyDrawing(profile.drawing).then((ok) => {
          if (ok) toast("配置档已应用", `已切换到「${profile.name}」。`);
        });
      });

      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "preset-delete";
      remove.setAttribute("aria-label", `删除${profile.name}`);
      remove.title = `删除${profile.name}`;
      remove.innerHTML = '<span class="preset-delete-glyph" aria-hidden="true">×</span>';
      remove.addEventListener("click", () => {
        confirmDialog.open(profile.name, () => {
          const previous = profiles;
          profiles = profiles.filter((item) => item.id !== profile.id);
          if (!writeWorldProfiles(profiles)) {
            profiles = previous;
            return;
          }
          renderList();
        });
      });

      row.append(select, remove);
      list.appendChild(row);
    });
  }

  saveButton.addEventListener("click", () => {
    closeMenu();
    nameDialog.open(`配置档 ${profiles.length + 1}`, (name) => {
      const existing = profiles.find((profile) => profile.name === name);
      const next = {
        ...(existing || { id: `profile-${Date.now()}-${Math.random().toString(36).slice(2, 8)}` }),
        name,
        updatedAt: Date.now(),
        drawing: { ...(state.config?.drawing || {}) },
      };
      let nextProfiles;
      if (existing) nextProfiles = profiles.map((profile) => profile.id === existing.id ? next : profile);
      else {
        if (profiles.length >= 12) {
          toast("配置档数量已达上限", "最多保存 12 个配置档，请先删除不用的。", true);
          return;
        }
        nextProfiles = [next, ...profiles];
      }
      if (!writeWorldProfiles(nextProfiles)) return;
      profiles = nextProfiles;
      renderList();
      toast(existing ? "配置档已更新" : "配置档已保存", `「${name}」已保存当前绘制参数。`);
    }, { title: "保存配置档", subtitle: "为当前绘制参数命名（按世界/画笔保存）" });
  });

  renderList();
}

function bindHelpTips() {
  const tooltip = $("helpTooltip");
  const tips = [...document.querySelectorAll(".help-tip")];
  if (!tooltip || !tips.length) return;

  let hideTimer = null;
  let activeButton = null;
  let positionFrame = 0;

  function positionTooltip(button) {
    const rect = button.getBoundingClientRect();
    const tipRect = tooltip.getBoundingClientRect();
    const gap = 10;
    let left = rect.right + gap;
    if (left + tipRect.width > window.innerWidth - 12) {
      left = rect.left - tipRect.width - gap;
    }
    left = Math.max(12, Math.min(left, window.innerWidth - tipRect.width - 12));
    let top = rect.top + (rect.height - tipRect.height) / 2;
    top = Math.max(12, Math.min(top, window.innerHeight - tipRect.height - 12));
    tooltip.style.left = `${Math.round(left)}px`;
    tooltip.style.top = `${Math.round(top)}px`;
  }

  function hide(button = null) {
    if (button && activeButton && button !== activeButton) return;
    activeButton = null;
    cancelAnimationFrame(positionFrame);
    positionFrame = 0;
    clearTimeout(hideTimer);
    tooltip.classList.remove("show");
    tips.forEach((tip) => tip.removeAttribute("aria-describedby"));
    hideTimer = setTimeout(() => {
      if (!tooltip.classList.contains("show")) tooltip.hidden = true;
    }, 150);
  }

  function renderTooltip(text) {
    const segments = text
      .split(/(?=调高\s*[:：]|调低\s*[:：])/)
      .map((segment) => segment.trim())
      .filter(Boolean);
    const isRangeHelp = segments.length > 1 && segments.every((segment) => /^(调高|调低)\s*[:：]/.test(segment));
    if (!isRangeHelp) {
      tooltip.textContent = text;
      return;
    }
    tooltip.replaceChildren();
    segments.forEach((segment) => {
      const match = segment.match(/^(调高|调低)\s*[:：]\s*(.*)$/);
      if (!match) return;
      const block = document.createElement("div");
      block.className = "help-block";
      const label = document.createElement("span");
      label.className = "help-label";
      label.textContent = match[1];
      block.append(label, document.createTextNode(`：${match[2]}`));
      tooltip.appendChild(block);
    });
  }

  function show(button) {
    const text = button.dataset.help?.trim();
    if (!text) return;
    activeButton = button;
    clearTimeout(hideTimer);
    renderTooltip(text);
    tooltip.hidden = false;
    tips.forEach((tip) => tip.removeAttribute("aria-describedby"));
    button.setAttribute("aria-describedby", "helpTooltip");
    tooltip.classList.remove("show");
    positionTooltip(button);
    requestAnimationFrame(() => {
      if (activeButton === button) tooltip.classList.add("show");
    });
  }

  tips.forEach((button) => {
    const status = { hover: false, focus: false };
    const update = () => {
      if (status.hover || status.focus) show(button);
      else hide(button);
    };
    button.addEventListener("pointerenter", () => { status.hover = true; update(); });
    button.addEventListener("pointerleave", () => { status.hover = false; update(); });
    button.addEventListener("focus", () => {
      status.focus = true;
      update();
      // 浏览器可能在 focus 前后滚动侧栏；下一帧再确认一次，避免滚动事件
      // 刚好把键盘触发的提示关闭。
      requestAnimationFrame(() => {
        if (status.focus && document.activeElement === button) show(button);
      });
    });
    button.addEventListener("blur", () => { status.focus = false; update(); });
  });

  window.addEventListener("resize", () => hide());
  document.addEventListener("scroll", () => {
    // 键盘 Tab/程序化 focus 可能先把按钮滚入视区。此时保留提示并在滚动后
    // 重新定位；鼠标悬停产生的普通滚动仍关闭提示，避免浮层脱离触发按钮。
    const focusedButton = tips.find((tip) => document.activeElement === tip);
    if (focusedButton) {
      cancelAnimationFrame(positionFrame);
      positionFrame = requestAnimationFrame(() => {
        positionFrame = 0;
        if (document.activeElement === focusedButton) {
          if (activeButton === focusedButton && !tooltip.hidden) {
            positionTooltip(focusedButton);
          } else {
            show(focusedButton);
          }
        }
      });
      return;
    }
    hide();
  }, true);
}

async function saveTheme(dark) {
  if (state.processing) return;
  state.config.theme_dark = dark;
  if (!await persistConfig()) {
    return;
  }
  applyTheme(dark);
  setSeg(["themeDark", "themeLight"], dark ? "themeDark" : "themeLight");
  toast(dark ? "已切换深色主题" : "已切换浅色主题");
}

async function saveCanvasMode(mode) {
  if (state.processing) return;
  state.config.canvas_dark = mode === "dark";
  if (!await persistConfig()) {
    return;
  }
  applyCanvasBg();
  setSeg(["canvasLight", "canvasDark"], mode === "dark" ? "canvasDark" : "canvasLight");
}

const AI_FIELD_IDS = ["aiBase", "aiKey", "aiModel"];
const AI_FIELD_MAP = { aiBase: "api_base_url", aiKey: "api_key", aiModel: "model" };

async function saveAiInput(id) {
  const v = $(id).value.trim();
  // Key 输入框留空 = 不修改（避免误清空已有 Key）
  if (id === "aiKey" && !v) return;
  const key = AI_FIELD_MAP[id];
  if (key === "api_key") {
    pendingAiApiKey = v;
  } else {
    state.aiConfig[key] = v;
  }
  if (!await persistAiConfig()) {
    return;
  }
}

// 显式清除本地 API Key。普通的空输入仍表示“保持原值”，避免用户编辑
// URL/模型时意外擦除 Key；只有这个按钮会设置一次性 clear_api_key 标记。
async function clearApiKey() {
  if (!state.aiConfig || !state.aiConfig.api_key_set) return;
  pendingAiApiKey = "";
  state.aiConfig.clear_api_key = true;
  $("aiKey").value = "";
  if (!await persistAiConfig()) {
    return;
  }
  toast("API Key 已清除", "本地配置中的 Key 已删除。", false);
}

// 眼睛图标：切换 API Key 输入框的明文/密文显示
function toggleKeyVisibility() {
  const input = $("aiKey");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  $("aiKeyToggle").querySelector("i").className = show ? "iconoir-eye-closed" : "iconoir-eye";
}

// 把 AI 表单当前值（可能未失焦）同步到 state 并保存，供"获取模型/测试连接"使用
async function saveAiFormNow() {
  for (const id of AI_FIELD_IDS) {
    const v = $(id).value.trim();
    if (id === "aiKey" && !v) continue; // Key 留空 = 不修改
    const key = AI_FIELD_MAP[id];
    if (key === "api_key") {
      pendingAiApiKey = v;
    } else {
      state.aiConfig[key] = v;
    }
  }
  return await persistAiConfig();
}

// 获取接口可用模型列表，填充模型下拉；按钮图标旋转表示加载中
async function fetchAiModels() {
  if (state.fetchingAi) return;
  state.fetchingAi = true;
  const btn = $("fetchModelsBtn");
  const icon = $("fetchModelsIcon");
  btn.disabled = true;
  icon.classList.add("spin");
  try {
    // 先保存表单当前值（用户可能刚粘贴 URL/Key 未失焦），确保按新配置请求；
    // 保存失败已 toast 提示，不再重复弹"获取模型失败"
    const saved = await saveAiFormNow();
    if (!saved) return;
    const models = await invoke("fetch_ai_models");
    aiModelCombobox.setItems(models);
    // 弹窗若已关闭则不自动展开（避免面板悬浮主界面）
    if (!$("settingsModal").hidden) aiModelCombobox.open();
    const count = Array.isArray(models) ? models.length : 0;
    toast("已获取模型", `接口返回 ${count} 个模型，可直接选择。`);
  } catch (e) {
    toast("获取模型失败", String(e), true);
  } finally {
    state.fetchingAi = false;
    btn.disabled = false;
    icon.classList.remove("spin");
  }
}

async function testAi() {
  if (state.testingAi) return;
  state.testingAi = true;
  $("testAiBtn").disabled = true;
  $("testAiBtn").innerHTML = '<span class="spinner"></span><span>测试中</span>';
  $("aiTestResult").textContent = "";
  try {
    // 先保存表单当前值，确保按最新 URL/Key 测试
    const saved = await saveAiFormNow();
    if (!saved) return;
    const r = await invoke("test_ai_connection");
    $("aiTestResult").textContent = "✓ " + r;
    toast("连接成功", r);
  } catch (e) {
    $("aiTestResult").textContent = "✗ 失败";
    toast("连接失败", String(e), true);
  } finally {
    state.testingAi = false;
    $("testAiBtn").disabled = false;
    $("testAiBtn").innerHTML = '<i class="iconoir-eye"></i><span>测试连接</span>';
  }
}

// ===================== 配置映射 =====================
function applySliderToConfig(key, v) {
  const c = state.config;
  switch (key) {
    case "eps": c.contour.epsilon_ratio = v; break;
    case "blur": c.image.blur_size = Math.round(v); break;
    case "sens": c.drawing.sensitivity = v; break;
    case "speed": c.drawing.draw_speed = v / 1000; break;
    case "step": c.drawing.max_step_px = Math.round(v); break;
    case "lift": c.drawing.lift_pen_speed = v; break;
    case "stretch": c.drawing.vertical_stretch = v; break;
  }
}

// ===================== 启动 =====================
init();
