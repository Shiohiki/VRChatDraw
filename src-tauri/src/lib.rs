mod ai;
mod config;
mod drawer;
mod gallery;
mod history;
mod pipeline;
mod probe;
mod storage;
mod types;

use base64::Engine as _;
use config::AppConfig;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

// ===================== 前端数据 DTO =====================
#[derive(Clone, serde::Serialize)]
pub struct StrokePointDto {
    x: f64,
    y: f64,
}

#[derive(Clone, serde::Serialize)]
pub struct StrokeDto {
    points: Vec<StrokePointDto>,
}

#[derive(Clone, serde::Serialize)]
pub struct ProcessOutcome {
    strokes: Vec<StrokeDto>,
    stroke_count: usize,
    point_count: usize,
    /// 生成结果对应的工作区版本；前端据此拒绝过期结果。
    revision: u64,
    /// 启用 AI 但 AI 线稿化失败、已回退普通管线（供前端提示用户）
    ai_fallback: bool,
    /// 预计绘制耗时（秒，按绘制参数与步数模型估算；前端提示与历史记录共用）
    estimate_seconds: f64,
}

impl ProcessOutcome {
    fn from_strokes(
        strokes: &[types::DrawingStroke],
        ai_fallback: bool,
        revision: u64,
        estimate_seconds: f64,
    ) -> Self {
        let point_count: usize = strokes.iter().map(|s| s.points.len()).sum();
        Self {
            strokes: strokes
                .iter()
                .map(|s| StrokeDto {
                    points: s
                        .points
                        .iter()
                        .map(|p| StrokePointDto { x: p.x, y: p.y })
                        .collect(),
                })
                .collect(),
            stroke_count: strokes.len(),
            point_count,
            revision,
            ai_fallback,
            estimate_seconds,
        }
    }
}

#[derive(Clone, serde::Serialize)]
pub struct ImageInfo {
    path: String,
    file_name: String,
    data_url: String, // 原图缩略图（最长边 ≤1600px 的 data URL）
    width: u32,
    height: u32,
    /// 图片进入工作区时的版本号。
    revision: u64,
    /// 当前工作区图片内容的 SHA-256（hex）——相册精确匹配键（内容一致才可载入）。
    /// 裁剪/剪贴板导入按各自缓存文件计算。
    content_hash: String,
    /// 根来源图内容哈希（hex）：文件导入=原文件字节；剪贴板=原始像素；裁剪时继承。
    /// 相册按此归组，导入原图即可命中全部裁剪变体。
    source_hash: String,
    /// 生成当前图所用的裁剪区域（根来源图坐标系）；None = 未裁剪（全图）
    crop_rect: Option<gallery::CropRect>,
}

// ===================== 全局状态 =====================
/// 局部区域补画：仅绘制与选区相交的笔画（原图内容坐标，含边界）
#[derive(Debug, Clone, Copy, PartialEq)]
struct RegionFilter {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

pub struct AppState {
    config: Mutex<AppConfig>,
    /// 已落盘配置（磁盘真相）：save_config 据此判断是否需要写盘，
    /// sync_config 只改 config 不动本字段，避免实时同步后保存被误判为"未变化"。
    persisted_config: Mutex<AppConfig>,
    ai_config: Mutex<ai::AiConfig>,
    image: Mutex<Option<ImageInfo>>,
    strokes: Mutex<Vec<types::DrawingStroke>>,
    drawer: drawer::Drawer,
    processing: std::sync::atomic::AtomicBool, // 图像/AI 处理进行中（供热键路径检查）
    workspace_revision: AtomicU64,
    config_revision: AtomicU64,
    strokes_revision: Mutex<Option<u64>>,
    /// 局部区域补画选区（Some = F9 仅绘制与选区相交的笔画）
    region_filter: Mutex<Option<RegionFilter>>,
    crop_cache_paths: Mutex<Vec<std::path::PathBuf>>,
    /// 最近一次成功生成的笔画是否走了 AI 回退（相册条目参数快照用）
    last_ai_fallback: Mutex<bool>,
    /// 串行化“检查版本 + 提交图片/笔画”的临界区。
    commit_lock: Mutex<()>,
    /// 文件对话框打开期间置位：系统对话框为模态，用户在背后按全局热键
    /// 不应触发绘制/预演（否则对话框尚在、绘制却已开始，选完图只会被拒绝）。
    dialog_open: std::sync::atomic::AtomicBool,
    startup_errors: Mutex<Vec<String>>, // setup 阶段错误队列（前端 listen 前 emit 会丢失）
}

impl Default for AppState {
    fn default() -> Self {
        // 配置解析失败信息收集进 startup_errors，前端 init 后拉取提示
        // （避免 API Key 因配置格式错误被静默回退默认值而丢失）
        let (config, config_err) = AppConfig::load_with_error();
        let (ai_config, ai_err) = ai::load_config_with_error();
        let mut startup_errors = Vec::new();
        if let Some(e) = config_err {
            startup_errors.push(e);
        }
        if let Some(e) = ai_err {
            startup_errors.push(e);
        }
        Self {
            config: Mutex::new(config.clone()),
            persisted_config: Mutex::new(config),
            ai_config: Mutex::new(ai_config),
            image: Mutex::new(None),
            strokes: Mutex::new(Vec::new()),
            drawer: drawer::Drawer::new(),
            processing: std::sync::atomic::AtomicBool::new(false),
            workspace_revision: AtomicU64::new(0),
            config_revision: AtomicU64::new(0),
            strokes_revision: Mutex::new(None),
            region_filter: Mutex::new(None),
            crop_cache_paths: Mutex::new(Vec::new()),
            last_ai_fallback: Mutex::new(false),
            commit_lock: Mutex::new(()),
            dialog_open: std::sync::atomic::AtomicBool::new(false),
            startup_errors: Mutex::new(startup_errors),
        }
    }
}

/// 文件对话框打开标志的 RAII 守卫：pick_image 的全部退出路径（取消/选择/错误）
/// 都自动复位，避免标志残留导致热键被永久拒绝。
struct DialogOpenGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for DialogOpenGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 处理中标志 RAII 守卫：进入时原子抢锁（防止并发处理），析构时无条件复位。
/// 避免 async 任务被取消/panic 时 processing 永驻 true 导致后续绘制被永久拒绝。
struct ProcessingGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl<'a> ProcessingGuard<'a> {
    /// 尝试抢锁；失败说明已有处理在进行
    fn acquire(flag: &'a std::sync::atomic::AtomicBool) -> Option<Self> {
        if flag.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(Self(flag))
        }
    }
}

impl Drop for ProcessingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

// ===================== 配置 =====================
#[tauri::command]
fn get_config(state: State<'_, AppState>) -> AppConfig {
    state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

#[tauri::command]
fn save_config(
    app: AppHandle,
    state: State<'_, AppState>,
    cfg: AppConfig,
) -> Result<AppConfig, String> {
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = cfg.normalized();
    let previous = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    // 配置与"磁盘已落盘版本"一致时才跳过写盘与 revision 递增：
    // 不能用内存状态比较——sync_config 会让内存领先于磁盘，若按内存比较，
    // 实时同步后的真正保存会被误判为"未变化"而永不落盘。
    let persisted = state
        .persisted_config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if cfg == persisted {
        // 与磁盘一致时也收敛内存：sync_config 可能让内存领先于磁盘，若用户"拖回原值"
        // 恰好等于磁盘内容，此处不收敛会留下"内存旧值 / UI 新值"的短暂不一致窗口
        // （该窗口内 F9 会按旧值绘制）。
        *state.config.lock().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
        return Ok(cfg);
    }
    // 先写盘成功后再改内存（避免磁盘失败时内存被新值覆盖导致不一致）
    cfg.save_normalized()
        .map_err(|e| format!("保存配置失败：{e}"))?;
    // 主题切换：同步 Tauri 窗口原生主题（标题栏/系统按钮与前端 data-theme 一致）
    if cfg.theme_dark != previous.theme_dark {
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.set_theme(Some(if cfg.theme_dark {
                tauri::Theme::Dark
            } else {
                tauri::Theme::Light
            }));
        }
        sync_window_background(&app, cfg.theme_dark);
    }
    *state.config.lock().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
    *state
        .persisted_config
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = cfg.clone();
    state.config_revision.fetch_add(1, Ordering::SeqCst);
    // 参数只影响下一次处理或下一次绘制，不改变当前工作区图片和已生成笔画的有效性。
    // 这样调整灵敏度、速度等绘制参数后可以直接 F9 使用新参数；处理参数也不会让
    // 前端无条件进入“重新生成笔画”状态，用户仍可按主按钮主动重新处理。
    Ok(cfg)
}

/// 参数实时同步：只更新内存配置、不写盘，供前端滑块/数值输入在拖动、键入过程中
/// 节流调用，保证 F9/Shift+F9 等全局热键立即读到最新参数（磁盘持久化仍由
/// save_config 在 change/blur 时负责）。
#[tauri::command]
fn sync_config(
    app: AppHandle,
    state: State<'_, AppState>,
    cfg: AppConfig,
) -> Result<AppConfig, String> {
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = cfg.normalized();
    let previous = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if cfg == previous {
        return Ok(cfg);
    }
    // 主题同步与 save_config 一致（滑块同步快照可能包含主题变化）
    if cfg.theme_dark != previous.theme_dark {
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.set_theme(Some(if cfg.theme_dark {
                tauri::Theme::Dark
            } else {
                tauri::Theme::Light
            }));
        }
        sync_window_background(&app, cfg.theme_dark);
    }
    *state.config.lock().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
    state.config_revision.fetch_add(1, Ordering::SeqCst);
    Ok(cfg)
}

#[tauri::command]
fn get_ai_config(state: State<'_, AppState>) -> ai::AiConfigView {
    // 只返回是否已配置，Key 本体留在 Rust 侧，避免无必要地复制到 WebView。
    ai::AiConfigView::from_config(&state.ai_config.lock().unwrap_or_else(|p| p.into_inner()))
}

#[tauri::command]
fn save_ai_config(state: State<'_, AppState>, mut cfg: ai::AiConfig) -> Result<(), String> {
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    // 接口后缀白名单校验：非法值在保存时拒绝，避免手改配置后静默回落 images/edits
    const VALID_ENDPOINTS: [&str; 2] = ["images/edits", "chat/completions"];
    let endpoint = cfg.api_endpoint.trim().trim_start_matches('/');
    if !VALID_ENDPOINTS.contains(&endpoint) {
        return Err(format!(
            "接口后缀无效：{endpoint}（可选：images/edits | chat/completions）"
        ));
    }
    cfg.api_endpoint = endpoint.to_string();
    cfg.api_base_url = cfg.api_base_url.trim().trim_end_matches('/').to_string();
    ai::validate_api_base_url(&cfg.api_base_url)?;
    cfg.model = cfg.model.trim().to_string();
    if cfg.model.is_empty() {
        return Err("模型名称不能为空".to_string());
    }
    let previous = state
        .ai_config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    // 空值表示“未修改”，而显式 clear_api_key 才会真正删除本地 Key。
    // clear_api_key 不会写入 config.json，避免把一次性控制字段持久化。
    if cfg.clear_api_key {
        cfg.api_key.clear();
    } else if cfg.api_key.trim().is_empty() {
        cfg.api_key = previous.api_key.clone();
    }
    cfg.clear_api_key = false;
    ai::save_config(&cfg).map_err(|e| format!("保存 AI 配置失败：{e}"))?;
    *state.ai_config.lock().unwrap_or_else(|p| p.into_inner()) = cfg;
    state.config_revision.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// 重置处理参数并写回磁盘；界面偏好（主题、网格、画布底色）保持不变。
#[tauri::command]
fn reset_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再重置参数".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    // 检查与加锁之间可能已经启动了处理/绘制，提交前再确认一次。
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再重置参数".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let (theme, show_grid, canvas_dark) = {
        let current = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (current.theme_dark, current.show_grid, current.canvas_dark)
    };
    let cfg = AppConfig {
        theme_dark: theme,
        show_grid,
        canvas_dark,
        ..AppConfig::default()
    };
    cfg.save().map_err(|e| format!("重置保存失败：{e}"))?; // 先写盘
    *state.config.lock().unwrap_or_else(|p| p.into_inner()) = cfg.clone(); // 成功后再改内存
    *state
        .persisted_config
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = cfg.clone();
    state.config_revision.fetch_add(1, Ordering::SeqCst);
    // 重置的是参数，不是工作区图片；已有笔画仍可使用新的绘制参数。
    Ok(cfg)
}

#[tauri::command]
async fn test_ai_connection(state: State<'_, AppState>) -> Result<String, String> {
    let cfg = state
        .ai_config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    tauri::async_runtime::spawn_blocking(move || ai::test_connection(&cfg))
        .await
        .map_err(|e| format!("任务异常：{e}"))?
}

/// 获取接口可用模型列表（前端"获取模型"按钮）
#[tauri::command]
async fn fetch_ai_models(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let cfg = state
        .ai_config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    tauri::async_runtime::spawn_blocking(move || ai::fetch_models(&cfg))
        .await
        .map_err(|e| format!("任务异常：{e}"))?
}

// ===================== 图片 =====================
#[tauri::command]
async fn pick_image(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ImageInfo>, String> {
    // 处理/绘制中禁止换图：处理完成会回写 strokes（属于旧图），绘制线程持有笔画副本
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再选择图片".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let start_revision = state.revision();
    use tauri_plugin_dialog::DialogExt;
    // 对话框打开期间置位：全局热键（F9/F8/Shift+F9）据此拒绝在对话框背后启动绘制
    state.dialog_open.store(true, Ordering::SeqCst);
    let _dialog_guard = DialogOpenGuard(&state.dialog_open);
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("图片", &["png", "jpg", "jpeg", "bmp", "webp"])
            .blocking_pick_file()
    })
    .await
    .map_err(|e| format!("对话框异常：{e}"))?;

    let Some(path) = picked else {
        return Ok(None); // 用户取消
    };

    let path_str = match path {
        tauri_plugin_dialog::FilePath::Path(p) => p.to_string_lossy().to_string(),
        tauri_plugin_dialog::FilePath::Url(_) => {
            return Err("暂不支持 URL 路径，请选择本地文件".to_string());
        }
    };
    // 解码 + 缩略图生成（Lanczos3 + PNG 编码 + base64）是重 CPU/IO 操作，
    // 必须在 spawn_blocking 内执行，避免阻塞 async reactor
    let info = tauri::async_runtime::spawn_blocking(move || build_image_info(&path_str))
        .await
        .map_err(|e| format!("任务异常：{e}"))??;
    commit_image(&state, info, start_revision).map(Some)
}

/// 按路径导入图片（拖拽导入用；与 pick_image 共用解码与提交逻辑）
#[tauri::command]
async fn import_image(state: State<'_, AppState>, path: String) -> Result<ImageInfo, String> {
    // 处理/绘制中禁止换图：处理完成会回写 strokes（属于旧图），绘制线程持有笔画副本
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再导入图片".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let start_revision = state.revision();
    let info = tauri::async_runtime::spawn_blocking(move || build_image_info(&path))
        .await
        .map_err(|e| format!("任务异常：{e}"))??;
    commit_image(&state, info, start_revision)
}

/// 从系统剪贴板读取图片并导入工作区（Ctrl+V；剪贴板无图片时返回 None）
#[tauri::command]
async fn import_clipboard_image(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ImageInfo>, String> {
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再导入图片".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let start_revision = state.revision();
    // 剪贴板读取与解码是阻塞 IO/CPU 操作，放到阻塞线程避免卡 async reactor
    let decoded = tauri::async_runtime::spawn_blocking(
        move || -> Result<Option<image::DynamicImage>, String> {
            use tauri_plugin_clipboard_manager::ClipboardExt;
            // 插件在剪贴板无图片数据时返回错误；统一视为"剪贴板没有图片"
            let image = match app.clipboard().read_image() {
                Ok(image) => image,
                Err(_) => return Ok(None),
            };
            let (width, height) = (image.width(), image.height());
            if width == 0 || height == 0 {
                return Err("剪贴板图片尺寸无效".to_string());
            }
            let rgba = image.rgba().to_vec();
            let Some(rgb) = image::RgbaImage::from_raw(width, height, rgba) else {
                return Err("剪贴板图片数据无效".to_string());
            };
            Ok(Some(image::DynamicImage::ImageRgba8(rgb)))
        },
    )
    .await
    .map_err(|e| format!("任务异常：{e}"))??;
    let Some(img) = decoded else {
        return Ok(None); // 剪贴板没有图片
    };
    // 剪贴板图片没有文件路径：写入裁剪缓存文件（唯一文件名，随生命周期清理），
    // 后续生成笔画/再次裁剪都基于该缓存文件
    let cache_info = tauri::async_runtime::spawn_blocking(
        move || -> Result<(ImageInfo, std::path::PathBuf), String> {
            let cache = save_crop_cache(&img)?;
            let content_hash = hash_file_sha256(&cache.to_string_lossy())?;
            // 剪贴板来源身份 = 原始像素哈希：同内容重复粘贴天然同源
            let source_hash =
                hash_pixels_sha256(img.width(), img.height(), img.to_rgba8().as_raw());
            let mut info = match build_image_info_from_img_with_hash(
                &img,
                &cache.to_string_lossy(),
                content_hash,
                source_hash,
                None,
            ) {
                Ok(info) => info,
                Err(error) => {
                    remove_crop_cache(&cache);
                    return Err(error);
                }
            };
            info.file_name = "剪贴板图片".to_string();
            Ok((info, cache))
        },
    )
    .await
    .map_err(|e| format!("任务异常：{e}"))??;
    let (info, cache) = cache_info;
    // 提交成功后再登记缓存路径（提交失败时由 RAII 清理该缓存文件）
    let mut pending_cache = PendingCropCache(Some(cache.clone()));
    match commit_image(&state, info, start_revision) {
        Ok(committed_info) => {
            track_crop_cache(&state, &cache);
            pending_cache.0 = None;
            Ok(Some(committed_info))
        }
        Err(e) => Err(e),
    }
}

/// 提交新图片到工作区（对话框/拖拽/剪贴板共用）：
/// 在 commit_lock 临界区内复查版本与绘制/处理状态，一次性更新图片与笔画。
fn commit_image(
    state: &AppState,
    mut info: ImageInfo,
    start_revision: u64,
) -> Result<ImageInfo, String> {
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    // 对话框/解码期间可能已经开始处理/绘制，提交前必须再次确认。
    if state.processing.load(Ordering::SeqCst) {
        return Err("处理已在图片导入期间开始，请稍后重试".to_string());
    }
    if state.drawer.is_busy() {
        return Err("绘制已在图片导入期间开始，请先按 F10 停止".to_string());
    }
    if state.revision() != start_revision {
        return Err("工作区已发生变化，请重新选择图片".to_string());
    }
    let revision = state.bump_revision();
    info.revision = revision;
    // 清空旧笔画，加载新图（统一先 image 后 strokes 的锁顺序）
    *state.image.lock().unwrap_or_else(|p| p.into_inner()) = Some(info.clone());
    state
        .strokes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 新图意味着旧断点失效（续画进度必须重新开始）
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 新图意味着区域筛选失效
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    cleanup_crop_caches(state, Some(std::path::Path::new(&info.path)));
    Ok(info)
}

/// 清空预览画板（图片与笔画）
#[tauri::command]
fn clear_workspace(state: State<'_, AppState>) -> Result<(), String> {
    // 绘制中不允许清空：drawer 线程克隆了笔画副本，清空 state 无法停止其 SendInput
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再清空".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再清空".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    state.bump_revision();
    // 统一锁顺序：先 image 后 strokes（与 pick_image/crop_image 一致）
    *state.image.lock().unwrap_or_else(|p| p.into_inner()) = None;
    *state.strokes.lock().unwrap_or_else(|p| p.into_inner()) = Vec::new();
    *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 清空画布：绘制断点一并失效
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 清空画布：区域筛选一并失效
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    cleanup_crop_caches(&state, None);
    Ok(())
}

/// 图片像素上限（防止超大图解码耗尽内存）
const MAX_PIXELS: u64 = 40_000_000;
const MAX_IMAGE_FILE_BYTES: u64 = 128 * 1024 * 1024;
static CROP_CACHE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// 为每次裁剪分配独立的缓存文件。
///
/// WebView2 可能仍在读取上一次裁剪结果。Windows 下删除/替换正在使用的
/// `crop_cache.png` 会失败，因此不能复用固定文件名。
fn crop_cache_path() -> std::path::PathBuf {
    let sequence = CROP_CACHE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = format!(
        "crop_cache.{}.{}.{}.png",
        std::process::id(),
        temp_suffix(),
        sequence
    );
    crate::storage::data_path(&file_name)
}

/// 裁剪缓存采用无损 PNG；超大图改用 JPEG 92，
/// 避免 PNG 编码产生数百 MB 临时文件与长时间写入。
/// 管线对两种格式无差别：load_image_any 按内容嗅探解码。
const CROP_CACHE_JPEG_PIXELS: u64 = 16_000_000;

fn save_crop_cache(img: &image::DynamicImage) -> Result<std::path::PathBuf, String> {
    crate::storage::ensure_app_data_dir()?;
    let large = img.width() as u64 * img.height() as u64 > CROP_CACHE_JPEG_PIXELS;
    let cache = if large {
        crop_cache_path().with_extension("jpg")
    } else {
        crop_cache_path()
    };
    // 原子写：先写独立 .tmp 再 rename，避免并发的 process_image 读到半截文件。
    // 目标文件每次都唯一，不需要删除/替换 WebView2 仍在使用的旧缓存。
    let ext = if large { "jpg" } else { "png" };
    let tmp = cache.with_extension(format!(
        "{ext}.tmp.{}.{}",
        std::process::id(),
        temp_suffix()
    ));
    let write_result = if large {
        let file =
            std::fs::File::create(&tmp).map_err(|error| format!("保存裁剪结果失败：{error}"))?;
        // 白底合成后再编码：to_rgb8 会把透明像素（RGB 通常为 0）直接编码为黑色，
        // 管线按内容解码后会把透明区误判为黑色墨迹（PNG 分支保留 alpha、无此问题）
        let rgb = crate::pipeline::basic::to_rgb_on_white(img);
        image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 92)
            .encode(&rgb, rgb.width(), rgb.height(), image::ColorType::Rgb8)
            .map_err(|error| format!("保存裁剪结果失败：{error}"))
    } else {
        img.save_with_format(&tmp, image::ImageFormat::Png)
            .map_err(|error| format!("保存裁剪结果失败：{error}"))
    };
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp, &cache) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("保存裁剪结果失败：{error}"));
    }
    Ok(cache)
}

fn remove_crop_cache(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

struct PendingCropCache(Option<std::path::PathBuf>);

impl Drop for PendingCropCache {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            remove_crop_cache(&path);
        }
    }
}

fn track_crop_cache(state: &AppState, current: &std::path::Path) {
    let previous = {
        let mut paths = state
            .crop_cache_paths
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous = std::mem::take(&mut *paths);
        paths.push(current.to_path_buf());
        previous
    };

    for path in previous {
        if path != current {
            remove_crop_cache(&path);
        }
    }
}

fn cleanup_crop_caches(state: &AppState, keep: Option<&std::path::Path>) {
    let mut paths = state
        .crop_cache_paths
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let previous = std::mem::take(&mut *paths);
    for path in previous {
        if keep.is_some_and(|keep| path == keep) {
            paths.push(path);
        } else {
            remove_crop_cache(&path);
        }
    }
}

fn cleanup_orphaned_crop_caches() {
    let directory = crate::storage::app_data_dir();

    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("crop_cache.")
            && (name.ends_with(".png")
                || name.ends_with(".jpg")
                || name.contains(".png.tmp.")
                || name.contains(".jpg.tmp."))
        {
            remove_crop_cache(&path);
        }
    }
}

/// 按文件内容嗅探格式解码图片（不依赖扩展名——很多下载图片扩展名是 .png 但内容实为 WebP/JPEG）
/// 先按内容嗅探格式读取尺寸头检查像素上限，避免超大图先全量解码再被拒
pub(crate) fn load_image_any(path: &str) -> Result<image::DynamicImage, String> {
    let (bytes, format) = load_image_bytes(path)?;
    decode_image_bytes(&bytes, format)
}

/// 读取图片文件字节并按内容嗅探格式（不做解码）。
/// 解码与内容哈希可共用同一份字节，避免大文件被读盘两次。
fn load_image_bytes(path: &str) -> Result<(Vec<u8>, image::ImageFormat), String> {
    // 整个检查和解码都基于同一份 bytes，避免“先检查后重新读取”造成 TOCTOU。
    if std::fs::metadata(path)
        .map_err(|e| format!("无法读取图片：{e}"))?
        .len()
        > MAX_IMAGE_FILE_BYTES
    {
        return Err("图片文件超过 128 MB 限制".to_string());
    }
    let file = std::fs::File::open(path).map_err(|e| format!("无法读取图片：{e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("无法读取图片：{e}"))?;
    if bytes.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return Err("图片文件超过 128 MB 限制".to_string());
    }
    let format = image::guess_format(&bytes)
        .map_err(|_| "无法识别的图片格式（请确认为 PNG/JPEG/BMP/WebP）".to_string())?;
    Ok((bytes, format))
}

/// 解码已嗅探格式的图片字节（含解码前后两次像素上限检查）
fn decode_image_bytes(
    bytes: &[u8],
    format: image::ImageFormat,
) -> Result<image::DynamicImage, String> {
    let mut reader = image::io::Reader::new(std::io::Cursor::new(bytes));
    reader.set_format(format);
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| format!("无法读取图片尺寸（文件可能损坏）：{e}"))?;
    check_image_size(w, h)?;
    let img = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| format!("图片解码失败：{e}"))?;
    check_image_size(img.width(), img.height())?;
    Ok(img)
}

fn check_image_size(w: u32, h: u32) -> Result<(), String> {
    if w as u64 * h as u64 > MAX_PIXELS {
        Err(format!(
            "图片过大（{w}×{h}，超过 4000 万像素），请先缩小图片再使用"
        ))
    } else {
        Ok(())
    }
}

/// 计算文件内容的 SHA-256（hex）——笔画相册的图片匹配键
pub(crate) fn hash_file_sha256(path: &str) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| format!("无法读取图片：{e}"))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("图片哈希计算失败：{e}"))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// 计算内存字节的 SHA-256（hex）——导入路径解码与哈希共用同一份字节，免二次读盘
fn hash_bytes_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 计算剪贴板像素的 SHA-256（hex，宽高前缀混合）——剪贴板图片的来源身份：
/// 同一内容重复复制/粘贴得到同一来源，天然命中相册归组
fn hash_pixels_sha256(width: u32, height: u32, pixels: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    hasher.update(pixels);
    format!("{:x}", hasher.finalize())
}

/// 裁剪区域合成：当前图（可能已是某次裁剪的结果）内的子裁剪坐标换算回
/// 根来源图坐标系（纯平移）。parent 为 None 表示当前已是根来源图全图
fn compose_crop_rect(
    parent: Option<gallery::CropRect>,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> gallery::CropRect {
    match parent {
        Some(p) => gallery::CropRect {
            x: p.x.saturating_add(x),
            y: p.y.saturating_add(y),
            w,
            h,
        },
        None => gallery::CropRect { x, y, w, h },
    }
}

/// 从已解码图像构建 ImageInfo（缩略图 + 元数据），文件名为指定路径的文件名。
/// 哈希与来源由调用方提供：导入路径传已读字节哈希，剪贴板/裁剪路径传缓存文件哈希
/// 与各自的来源身份（来源哈希/裁剪区域）
fn build_image_info_from_img_with_hash(
    img: &image::DynamicImage,
    path: &str,
    content_hash: String,
    source_hash: String,
    crop_rect: Option<gallery::CropRect>,
) -> Result<ImageInfo, String> {
    let (w, h) = (img.width(), img.height());
    check_image_size(w, h)?;
    let max_side = 1600.0;
    let scale = (max_side / w.max(h) as f32).min(1.0);
    let rgb = if scale < 1.0 {
        // .max(1) 兜底：极端长宽比图（如 40000×1）缩放后短边可能被截为 0，
        // image crate 对 0 维度 resize 会 panic
        image::imageops::resize(
            &crate::pipeline::basic::to_rgb_on_white(img),
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        crate::pipeline::basic::to_rgb_on_white(img)
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    rgb.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| format!("图片编码失败：{e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
    Ok(ImageInfo {
        path: path.to_string(),
        file_name: std::path::Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        data_url: format!("data:image/png;base64,{b64}"),
        width: w,
        height: h,
        revision: 0,
        content_hash,
        source_hash,
        crop_rect,
    })
}

/// 读取图片并生成长边 ≤1600px 的 data URL 缩略图。
/// 字节只读一次：解码与内容哈希共用同一份数据（大文件免二次读盘；
/// 哈希与实际解码内容严格同源，无"读盘间隙文件被替换"的不一致窗口）
fn build_image_info(path: &str) -> Result<ImageInfo, String> {
    let (bytes, format) = load_image_bytes(path)?;
    // 文件直导即根来源：来源哈希与内容哈希同源，无裁剪
    let source_hash = hash_bytes_sha256(&bytes);
    let img = decode_image_bytes(&bytes, format)?;
    build_image_info_from_img_with_hash(&img, path, source_hash.clone(), source_hash, None)
}

/// 按区域裁剪图片并返回新的 ImageInfo（前端裁剪原图用）
/// 裁剪结果写入 exe 旁的独立缓存文件，path 指向该缓存文件，
/// 后续生成笔画/再次裁剪均基于裁剪后的图
#[tauri::command]
async fn crop_image(
    state: State<'_, AppState>,
    path: String,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<ImageInfo, String> {
    let Some(_guard) = ProcessingGuard::acquire(&state.processing) else {
        return Err("正在处理图像，请稍候".to_string());
    };
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    // 前置检查并捕获父图来源信息（来源哈希 + 已有裁剪区域 + 用户可见文件名）：
    // 结果 ImageInfo 据此继承来源并合成根坐标裁剪区域；
    // 文件名为继承而非从 worker 路径取 basename——剪贴板来源的 worker 是
    // crop_cache.<pid>.<ts>.<seq>.png 内部缓存文件，取 basename 会把用户可见的
    // "剪贴板图片"退化成缓存文件名（同源再裁剪同理）
    let (parent_name, parent_source, parent_crop) = {
        let image = state.image.lock().unwrap_or_else(|p| p.into_inner());
        let Some(info) = image.as_ref() else {
            return Err("裁剪目标不是当前工作区图片".to_string());
        };
        if info.path != path {
            return Err("裁剪目标不是当前工作区图片".to_string());
        }
        (
            info.file_name.clone(),
            info.source_hash.clone(),
            info.crop_rect,
        )
    };
    let worker_path = path.clone();
    let result = tauri::async_runtime::spawn_blocking(move || -> Result<ImageInfo, String> {
        let mut img = load_image_any(&worker_path)?;
        let (iw, ih) = (img.width(), img.height());
        let x = x.min(iw);
        let y = y.min(ih);
        let w = w.min(iw - x);
        let h = h.min(ih - y);
        if w < 1 || h < 1 {
            return Err("裁剪区域无效".to_string());
        }
        let img = img.crop(x, y, w, h);
        let cache = save_crop_cache(&img)?;
        let content_hash = hash_file_sha256(&cache.to_string_lossy())?;
        let crop_rect = Some(compose_crop_rect(parent_crop, x, y, w, h));
        let mut info = match build_image_info_from_img_with_hash(
            &img,
            &cache.to_string_lossy(),
            content_hash,
            parent_source,
            crop_rect,
        ) {
            Ok(info) => info,
            Err(error) => {
                remove_crop_cache(&cache);
                return Err(error);
            }
        };
        info.file_name = parent_name;
        Ok(info)
    })
    .await
    .map_err(|e| format!("任务异常：{e}"))?;

    let mut info = result?;
    let mut pending_cache = PendingCropCache(Some(std::path::PathBuf::from(&info.path)));
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if state.drawer.is_busy() {
        return Err("绘制已开始，裁剪结果未提交".to_string());
    }
    let current_path = state
        .image
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|image| image.path.clone());
    if current_path.as_deref() != Some(path.as_str()) {
        return Err("工作区图片已变化，裁剪结果已丢弃".to_string());
    }
    let revision = state.bump_revision();
    info.revision = revision;
    // 同步 Rust 侧状态：后续 process_image 读取裁剪后的图（path 指向缓存）
    *state.image.lock().unwrap_or_else(|p| p.into_inner()) = Some(info.clone());
    state
        .strokes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 裁剪改变原图：绘制断点一并失效
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 裁剪改变原图：区域筛选一并失效
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    if let Some(cache_path) = pending_cache.0.as_deref() {
        track_crop_cache(&state, cache_path);
    }
    pending_cache.0 = None;
    Ok(info)
}

// ===================== 处理 =====================
/// 线段（含端点）是否与矩形相交（slab 法参数区间测试）
fn segment_intersects_rect(
    a: &types::DrawingPoint,
    b: &types::DrawingPoint,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> bool {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    for (p, d, lo, hi) in [(a.x, dx, x0, x1), (a.y, dy, y0, y1)] {
        if d.abs() < 1e-12 {
            if p < lo || p > hi {
                return false;
            }
        } else {
            let mut t_lo = (lo - p) / d;
            let mut t_hi = (hi - p) / d;
            if t_lo > t_hi {
                std::mem::swap(&mut t_lo, &mut t_hi);
            }
            t0 = t0.max(t_lo);
            t1 = t1.min(t_hi);
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

/// 笔画是否与选区相交：任意点在矩形内（含边界），或任意线段穿过矩形
fn stroke_intersects_rect(stroke: &types::DrawingStroke, rect: &RegionFilter) -> bool {
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.x + rect.w, rect.y + rect.h);
    for point in &stroke.points {
        if point.x >= x0 && point.x <= x1 && point.y >= y0 && point.y <= y1 {
            return true;
        }
    }
    stroke
        .points
        .windows(2)
        .any(|pair| segment_intersects_rect(&pair[0], &pair[1], x0, y0, x1, y1))
}

/// 保留与选区相交的笔画（保持原笔画方向与点顺序）
fn filter_strokes_for_draw(
    strokes: &[types::DrawingStroke],
    rect: &RegionFilter,
) -> Vec<types::DrawingStroke> {
    strokes
        .iter()
        .filter(|stroke| stroke_intersects_rect(stroke, rect))
        .cloned()
        .collect()
}

/// 区域补画筛选结果（前端显示命中数与估算用）
#[derive(Clone, serde::Serialize)]
struct RegionFilterView {
    stroke_count: usize,
    point_count: usize,
    total_count: usize,
    /// 命中笔画的预计绘制耗时（秒，与正式绘制同一估算模型）
    estimate_seconds: f64,
}

/// 设置局部区域补画选区：之后 F9/Shift+F9 仅绘制与选区相交的笔画。
/// 原始笔画结果保留，随时可通过 clear_strokes_filter 退出。
#[tauri::command]
fn filter_strokes(
    state: State<'_, AppState>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<RegionFilterView, String> {
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    // 只读尺寸即可（避免克隆整份 ImageInfo，其 data_url 缩略图可达数 MB）
    let (iw, ih) = {
        let image = state.image.lock().unwrap_or_else(|p| p.into_inner());
        let Some(image) = image.as_ref() else {
            return Err("请先选择图片".to_string());
        };
        (f64::from(image.width), f64::from(image.height))
    };
    // 防御：笔画必须确实由当前工作区图片生成（当前状态机下不可达，防止未来改动破坏不变量）
    let strokes_revision = *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if strokes_revision != Some(state.revision()) {
        return Err("笔画已过期，请重新生成笔画后再框选区域".to_string());
    }
    if !x.is_finite() || !y.is_finite() || !w.is_finite() || !h.is_finite() {
        return Err("选区坐标无效".to_string());
    }
    let x = x.clamp(0.0, iw);
    let y = y.clamp(0.0, ih);
    let w = w.min(iw - x);
    let h = h.min(ih - y);
    if w < 1.0 || h < 1.0 {
        return Err("选区过小，请扩大区域范围".to_string());
    }
    let rect = RegionFilter { x, y, w, h };
    let drawing_cfg = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drawing
        .clone();
    let strokes = state.strokes.lock().unwrap_or_else(|p| p.into_inner());
    let filtered = filter_strokes_for_draw(&strokes, &rect);
    if filtered.is_empty() {
        return Err("选区内没有笔画，请重新框选区域".to_string());
    }
    let estimate_seconds = estimate_draw_seconds(&drawing_cfg, &filtered);
    // 选区变化使旧断点失效（续画进度基于旧的笔画子集）
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(rect);
    Ok(RegionFilterView {
        stroke_count: filtered.len(),
        point_count: filtered.iter().map(|s| s.points.len()).sum(),
        total_count: strokes.len(),
        estimate_seconds,
    })
}

/// 退出局部区域补画：恢复绘制全部笔画
#[tauri::command]
fn clear_strokes_filter(state: State<'_, AppState>) -> Result<(), String> {
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    Ok(())
}

// ===================== 处理 =====================
/// 管线处理核心（含 AI 备选与坐标放大）
/// 返回 (笔画, 结果)；AI 失败降级状态由 ProcessOutcome.ai_fallback 携带
fn process_image_inner(
    path: &str,
    cfg: &AppConfig,
    ai_cfg: &ai::AiConfig,
) -> Result<(Vec<types::DrawingStroke>, ProcessOutcome), String> {
    let img = load_image_any(path)?; // 内部已按内容嗅探尺寸并检查像素上限
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let mut ai_fallback = false;
    // AI 线稿单独持有（Option）：AI 调用失败直接回退原图，避免克隆原图
    let mut ai_lineart: Option<image::DynamicImage> = None;
    if cfg.use_ai {
        match ai::ai_to_lineart(ai_cfg, &img) {
            Ok(lineart) => ai_lineart = Some(lineart),
            Err(e) => {
                eprintln!("AI 线稿化失败，回退普通管线：{e}");
                ai_fallback = true;
            }
        }
    }
    let source = ai_lineart.as_ref().unwrap_or(&img);
    // VRC-Draw 移植管线（PS 响应 + 区域分类 + 拓扑追踪 + 欧拉迹优化 + 矢量拟合）
    let strokes_f32 =
        match pipeline::process_image(source, cfg.image.blur_size, cfg.contour.epsilon_ratio) {
            Ok(strokes) => strokes,
            // AI 输出异常（全白/比例错乱等）导致管线失败时，回退原图再处理一次
            Err(error) if cfg.use_ai && !ai_fallback => {
                eprintln!("管线处理 AI 线稿失败，回退普通管线：{error}");
                ai_fallback = true;
                pipeline::process_image(&img, cfg.image.blur_size, cfg.contour.epsilon_ratio)?
            }
            Err(error) => return Err(error),
        };
    // 坐标映射：AI 输出图按 contain 等比缩放居中（可能带白边），需减去白边偏移再除以缩放系数；
    // 非 AI 或已回退时 source == 原图（s=1.0、pad=0），公式退化为恒等映射，统一处理
    let source = if ai_fallback {
        &img
    } else {
        ai_lineart.as_ref().unwrap_or(&img)
    };
    let lw = source.width() as f32;
    let lh = source.height() as f32;
    let s = (lw / iw).min(lh / ih);
    let pad_x = (lw - iw * s) / 2.0;
    let pad_y = (lh - ih * s) / 2.0;
    if !s.is_finite() || s <= 0.0 {
        return Err("处理结果的坐标比例无效".to_string());
    }
    let strokes: Vec<types::DrawingStroke> = strokes_f32
        .into_iter()
        .filter_map(|stroke| {
            let points: Vec<types::DrawingPoint> = stroke
                .into_iter()
                .filter_map(|p| {
                    let x = ((p.x - pad_x) / s) as f64;
                    let y = ((p.y - pad_y) / s) as f64;
                    if !x.is_finite() || !y.is_finite() {
                        return None;
                    }
                    Some(types::DrawingPoint {
                        x: x.clamp(0.0, (iw - 1.0).max(0.0) as f64),
                        y: y.clamp(0.0, (ih - 1.0).max(0.0) as f64),
                    })
                })
                .collect();
            (points.len() >= 2).then_some(types::DrawingStroke { points })
        })
        .collect();
    if strokes.is_empty() {
        return Err("处理结果没有有效笔画".to_string());
    }
    let estimate_seconds = estimate_draw_seconds(&cfg.drawing, &strokes);
    let outcome = ProcessOutcome::from_strokes(&strokes, ai_fallback, 0, estimate_seconds);
    Ok((strokes, outcome))
}

#[tauri::command]
async fn process_image(state: State<'_, AppState>) -> Result<ProcessOutcome, String> {
    let Some(_guard) = ProcessingGuard::acquire(&state.processing) else {
        return Err("正在处理图像，请稍候".to_string());
    };
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let revision = state.revision();
    let config_revision = state.config_revision();
    let ai_cfg = state
        .ai_config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let cfg = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .normalized();
    let img_path = state
        .image
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|i| i.path.clone());
    let Some(path) = img_path else {
        return Err("请先选择图片".to_string());
    };
    let worker_path = path.clone();
    let result = tauri::async_runtime::spawn_blocking(
        move || -> Result<(Vec<types::DrawingStroke>, ProcessOutcome), String> {
            process_image_inner(&worker_path, &cfg, &ai_cfg)
        },
    )
    .await;
    let (strokes, mut outcome) = result.map_err(|e| format!("任务异常：{e}"))??;
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if state.drawer.is_busy() {
        return Err("绘制已开始，处理结果已丢弃".to_string());
    }
    if state.revision() != revision {
        return Err("工作区或参数已变化，处理结果已过期，请重新生成".to_string());
    }
    if state.config_revision() != config_revision {
        return Err("配置已变化，处理结果已过期，请重新生成".to_string());
    }
    let current_path = state
        .image
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|image| image.path.clone());
    if current_path.as_deref() != Some(path.as_str()) {
        return Err("工作区图片已变化，处理结果已过期".to_string());
    }
    outcome.revision = revision;
    // 记录本次生成的 AI 回退状态（相册条目参数快照用）
    *state
        .last_ai_fallback
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = outcome.ai_fallback;
    *state.strokes.lock().unwrap_or_else(|p| p.into_inner()) = strokes;
    *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(revision);
    // 笔画已重新生成：旧绘制断点失效（续画进度必须重新开始）
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    // 笔画已重新生成：区域筛选失效（选区基于旧笔画）
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    Ok(outcome)
}

impl AppState {
    fn revision(&self) -> u64 {
        self.workspace_revision.load(Ordering::SeqCst)
    }

    fn config_revision(&self) -> u64 {
        self.config_revision.load(Ordering::SeqCst)
    }

    fn bump_revision(&self) -> u64 {
        self.workspace_revision.fetch_add(1, Ordering::SeqCst) + 1
    }
}

// ===================== 绘制 =====================
/// DrawResult → 错误提示文案（None = 无需提示）
fn draw_result_error_message(result: drawer::DrawResult) -> Option<&'static str> {
    match result {
        drawer::DrawResult::Completed | drawer::DrawResult::Cancelled => None,
        drawer::DrawResult::VrchatNotFound => {
            Some("未找到 VRChat 窗口，已取消绘制。请先启动 VRChat 再按 F9")
        }
        drawer::DrawResult::CursorUnavailable => Some("无法读取鼠标位置，已取消绘制"),
        drawer::DrawResult::InputInjectionFailed => Some("鼠标输入注入失败，已取消绘制"),
        drawer::DrawResult::InvalidStrokes => Some("绘制数据无效，已取消绘制"),
        drawer::DrawResult::TargetLost => {
            Some("VRChat 失去前台焦点，绘制已停止（绘制中请勿切换窗口）")
        }
        drawer::DrawResult::StaleCheckpoint => {
            Some("参数或选区已变化，暂停进度已失效，请重新生成笔画或按 F9 从头开始")
        }
        drawer::DrawResult::Panicked => Some("绘制线程异常终止，已取消绘制"),
    }
}

/// 画作尺寸超出屏幕的警告（不拦截，仅提示画板可能不够大）。
/// 返回 None 表示本次画作在屏幕范围内。
fn outside_desktop_warning(app: &AppHandle) -> Option<String> {
    app.state::<AppState>()
        .drawer
        .last_outside_desktop
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// 绘制结果错误提示（带诊断信息）：失焦/聚焦失败时附上具体原因，
/// 便于用户（和开发者）判断失败时的真实前台窗口。
fn draw_result_error_with_diag(app: &AppHandle, result: drawer::DrawResult) -> Option<String> {
    match result {
        drawer::DrawResult::VrchatNotFound => {
            let diag = app
                .state::<AppState>()
                .drawer
                .last_focus_failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_default();
            Some(format!(
                "未找到 VRChat 窗口或无法获得前台焦点，已取消绘制。{diag}"
            ))
        }
        drawer::DrawResult::TargetLost => {
            let diag = app
                .state::<AppState>()
                .drawer
                .last_target_lost
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_default();
            Some(format!("VRChat 失去前台焦点，绘制已停止（{diag}）"))
        }
        other => draw_result_error_message(other).map(str::to_string),
    }
}

/// 按当前绘制参数估算整幅画耗时（秒，历史记录用；前端展示用同一模型）。
/// 实际绘制节奏是"按屏幕坐标细分线段、每步一个 draw_delay"（见 drawer::move_relatively），
/// 因此按段距离/步长估算步数，而非按点数——长直线段点数远少于步数，按点估算会系统性低估。
/// 与 drawer 逐项对齐：
/// - 坐标缩放（x × sensitivity，y × sensitivity × vertical_stretch）先行，步数按屏幕距离折算；
/// - 落笔步长按 drawer 的 adaptive_pen_down_step 规则降档（转弯处收窄步长、步数增多）；
/// - 抬笔行程耗时（lift_pen_speed < 100 时每步 (100-speed)×0.5ms）；
/// - 短笔画动态延时（整笔撑到 60ms）与单点笔画固定开销；
/// - 相邻两次落笔间隔不足 600ms 时按 drawer 的双击防护补等。
fn estimate_draw_seconds(cfg: &config::DrawingConfig, strokes: &[types::DrawingStroke]) -> f64 {
    let delay = cfg.draw_speed.max(0.016);
    let lift = cfg.lift_pen_delay.max(0.040);
    let max_step = f64::from(cfg.max_step_px.clamp(1, 6));
    // 与 drawer 相同的屏幕坐标映射；sanitize 已保证下界，此处防御性兜底
    let sensitivity = cfg.sensitivity.max(0.1);
    let stretch = cfg.vertical_stretch.max(0.5);
    // 抬笔移动延时（与 drawer::move_relatively 一致：100% = 瞬移，每低 1% 增加 0.5ms/步）
    let lift_step_delay = (100.0 - cfg.lift_pen_speed).max(0.0) * 0.0005;

    // drawer curvature_step 的 f64 版：方向余弦与坐标取整无关，步长降档规则一致
    fn step_at(screen: &[(f64, f64)], index: usize, maximum_step: f64) -> f64 {
        let minimum_step = maximum_step.min(2.0);
        if index == 0 || index + 1 >= screen.len() {
            return maximum_step;
        }
        let (px, py) = screen[index - 1];
        let (cx, cy) = screen[index];
        let (nx, ny) = screen[index + 1];
        let ix = cx - px;
        let iy = cy - py;
        let ox = nx - cx;
        let oy = ny - cy;
        let denominator = ix.hypot(iy) * ox.hypot(oy);
        if denominator <= 1.0e-9 {
            return minimum_step;
        }
        let direction_cosine = (ix * ox + iy * oy) / denominator;
        if direction_cosine < 0.55 {
            minimum_step
        } else if direction_cosine < 0.9 {
            (maximum_step - 1.0).max(minimum_step)
        } else {
            maximum_step
        }
    }

    let mut seconds = 0.0;
    // 模拟时钟：仅用于复现双击防护（两次按下真实间隔 < 600ms 时补齐）。
    // 分量与 drawer 时序一一对应：抬笔行程 → 到达同步 60ms → 按下前 delay →
    // （防护补等）→ 按下 → dynamic → 步进 × dynamic → 硬同步 30ms → 抬笔延迟 → 稳定 20ms。
    let mut clock = 0.0;
    let mut last_press_clock: Option<f64> = None;
    let mut previous_end: Option<(f64, f64)> = None;
    for stroke in strokes {
        if stroke.points.is_empty() {
            continue;
        }
        // 抬笔行程：上一笔终点 → 本笔起点（屏幕坐标），首笔从光标锚点出发的距离未知，不估算
        if let Some((px, py)) = previous_end {
            let sx = stroke.points[0].x * sensitivity;
            let sy = stroke.points[0].y * sensitivity * stretch;
            let lift_steps = ((sx - px).hypot(sy - py) / max_step).ceil().max(0.0);
            let travel = lift_steps * lift_step_delay;
            seconds += travel;
            clock += travel;
        }
        let screen: Vec<(f64, f64)> = stroke
            .points
            .iter()
            .map(|p| (p.x * sensitivity, p.y * sensitivity * stretch))
            .collect();
        // 落笔段数：段 j（目标顶点 j）的步长 = min(curv(j-1), curv(j))，
        // 与 drawer 落笔循环的 adaptive_pen_down_step(stroke, point_index) 一致
        let mut steps = 0.0;
        for j in 1..screen.len() {
            let step = step_at(&screen, j - 1, max_step).min(step_at(&screen, j, max_step));
            let dx = screen[j].0 - screen[j - 1].0;
            let dy = screen[j].1 - screen[j - 1].1;
            steps += (dx.hypot(dy) / step).ceil().max(1.0);
        }
        // 动态延时：笔画过短时 VRChat 会因轮询率丢弃，引擎把整笔撑到 60ms
        // （draw_strokes_thread 的 dynamic_delay），估算按同式取值；单点笔画
        // 的"按下 + 保持"也由该项覆盖（按下前 delay + 按下后 dynamic）。
        let points = stroke.points.len().max(1) as f64;
        let dynamic = if points * delay < 0.060 {
            0.060 / points
        } else {
            delay
        };
        // 双击防护：候选按下时刻距上次按下不足 600ms 时补等（补等推迟本次按下时刻，
        // 与 drawer 在按下前 delay 之后执行防护的时序一致）
        if let Some(previous) = last_press_clock {
            let elapsed = clock + 0.060 + delay - previous;
            if elapsed < 0.600 {
                let wait = 0.600 - elapsed;
                seconds += wait;
                clock += wait;
            }
        }
        let press_clock = clock + 0.060 + delay;
        last_press_clock = Some(press_clock);
        // 该笔按下之后的剩余：dynamic + 步进 + 硬同步 30ms + 抬笔延迟 + 稳定停顿 20ms
        // （与旧模型每笔 0.110 + lift + delay + dynamic + steps × dynamic 的分量总和一致）
        let rest = dynamic + steps * dynamic + 0.030 + lift + 0.020;
        seconds += 0.060 + delay + rest;
        clock += 0.060 + delay + rest;
        if let Some(last) = stroke.points.last() {
            // 与本笔起点同一坐标系（屏幕坐标）：抬笔行程两端点必须同为缩放后坐标，
            // 否则行程随几何方向被高估/低估（sensitivity/stretch 不为 1 时）
            previous_end = Some((last.x * sensitivity, last.y * sensitivity * stretch));
        }
    }
    seconds
}

/// 一次绘制请求的前置状态（F9 从头开始 / F8 断点续画共用）
struct DrawRequest {
    cfg: config::DrawingConfig,
    strokes: Vec<types::DrawingStroke>,
    revision: u64,
    filter: Option<RegionFilter>,
    filter_active: bool,
    filter_total: usize,
    request_fingerprint: u64,
}

fn draw_request_fingerprint(cfg: &config::DrawingConfig, filter: Option<RegionFilter>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cfg.sensitivity.to_bits().hash(&mut hasher);
    cfg.vertical_stretch.to_bits().hash(&mut hasher);
    cfg.max_step_px.hash(&mut hasher);
    if let Some(rect) = filter {
        rect.x.to_bits().hash(&mut hasher);
        rect.y.to_bits().hash(&mut hasher);
        rect.w.to_bits().hash(&mut hasher);
        rect.h.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

fn draw_request_is_current(state: &AppState, req: &DrawRequest) -> bool {
    if state.processing.load(Ordering::SeqCst) || state.drawer.is_busy() {
        return false;
    }
    if state.revision() != req.revision {
        return false;
    }
    let strokes_revision = *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if strokes_revision != Some(req.revision) {
        return false;
    }
    let current_cfg = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drawing
        .clone();
    if current_cfg != req.cfg {
        return false;
    }
    let current_filter = *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    current_filter == req.filter
        && draw_request_fingerprint(&current_cfg, current_filter) == req.request_fingerprint
}

/// 绘制/续画/预演共用的前置检查：临界区、处理中、笔画与版本校验、区域筛选。
/// `busy_action` 用于处理中提示文案（"开始绘制"/"续画"/"预演"）。
/// 返回 None 表示已提示错误并应中止。
fn prepare_draw_request(app: &AppHandle, busy_action: &str) -> Option<DrawRequest> {
    let state = app.state::<AppState>();
    // 文件对话框为系统模态：背后触发的绘制无法被用户看见，先拒绝（F10 不受限）
    if state.dialog_open.load(Ordering::SeqCst) {
        let _ = app.emit(
            "toast",
            (
                "正在选择图片，请稍候再开始绘制".to_string(),
                "info".to_string(),
            ),
        );
        return None;
    }
    // 将“检查版本 + 克隆笔画 + 启动线程”放在同一临界区，避免参数/图片
    // 刚在检查后变化，绘制线程仍拿到旧笔画副本。
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if state.processing.load(Ordering::SeqCst) {
        let _ = app.emit(
            "toast",
            (
                format!("正在处理图像，请稍候再{busy_action}"),
                "info".to_string(),
            ),
        );
        return None;
    }
    if state.drawer.is_busy() {
        let _ = app.emit(
            "toast",
            (
                format!("正在绘制中，请先按 F10 停止后再{busy_action}"),
                "info".to_string(),
            ),
        );
        return None;
    }
    let revision = state.revision();
    let (cfg, strokes, strokes_revision, region_filter) = {
        let cfg = state
            .config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drawing
            .clone();
        let strokes = state
            .strokes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let strokes_revision = *state
            .strokes_revision
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let region_filter = *state
            .region_filter
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        (cfg, strokes, strokes_revision, region_filter)
    };
    // 局部区域补画：仅绘制与选区相交的笔画（选区可能已变化，这里每次重新筛选）
    let (strokes, filter_active, filter_total) = match &region_filter {
        Some(rect) => {
            let filtered = filter_strokes_for_draw(&strokes, rect);
            let total = strokes.len();
            if filtered.is_empty() {
                let _ = app.emit(
                    "toast",
                    (
                        "选区内没有笔画，请重新框选区域".to_string(),
                        "info".to_string(),
                    ),
                );
                return None;
            }
            (filtered, true, total)
        }
        None => (strokes, false, 0),
    };
    if strokes.is_empty() {
        let _ = app.emit(
            "toast",
            ("请先选择图片并处理生成笔画".to_string(), "info".to_string()),
        );
        return None;
    }
    if strokes_revision != Some(revision) {
        let _ = app.emit(
            "toast",
            (
                "参数或图片已变化，请重新生成笔画".to_string(),
                "info".to_string(),
            ),
        );
        return None;
    }
    let request_fingerprint = draw_request_fingerprint(&cfg, region_filter);
    Some(DrawRequest {
        cfg,
        strokes,
        revision,
        filter: region_filter,
        filter_active,
        filter_total,
        request_fingerprint,
    })
}

#[derive(Clone, serde::Serialize)]
struct DrawingStateEvent {
    generation: u64,
    active: bool,
    rehearsal: bool,
}

#[derive(Clone, serde::Serialize)]
struct DrawingFinishedEvent {
    generation: u64,
    finished: bool,
    rehearsal: bool,
}

fn emit_stale_draw_request(app: &AppHandle) {
    let _ = app.emit(
        "toast",
        (
            "工作区或绘制参数已变化，请重新发起绘制".to_string(),
            "info".to_string(),
        ),
    );
}

/// 启动绘制线程并挂接结束回调（历史记录/暂停提示/探测警告共用）
fn start_draw(app: &AppHandle, req: DrawRequest, resume: Option<drawer::DrawCheckpoint>) {
    let state = app.state::<AppState>();
    // 准备请求后再次持有提交锁并校验，确保图片、筛选区域、绘制参数和断点
    // 在真正占用输入设备前仍然是同一份状态。
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if !draw_request_is_current(&state, &req) {
        emit_stale_draw_request(app);
        return;
    }
    if let Some(checkpoint) = &resume {
        let current = state
            .drawer
            .progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if current.as_ref() != Some(checkpoint) {
            emit_stale_draw_request(app);
            return;
        }
    }
    // 区域补画模式提示
    if req.filter_active {
        let _ = app.emit(
            "toast",
            (
                format!(
                    "区域补画：仅绘制与选区相交的 {} 笔（共 {} 笔）",
                    req.strokes.len(),
                    req.filter_total
                ),
                "info".to_string(),
            ),
        );
    }
    // 历史记录快照（绘制线程持有笔画副本后这里不能再读 strokes）
    let history_image = state
        .image
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|i| (i.file_name.clone(), format!("{}×{}", i.width, i.height)));
    let history_strokes = req.strokes.len();
    let history_points = req.strokes.iter().map(|s| s.points.len()).sum::<usize>();
    let history_estimate = estimate_draw_seconds(&req.cfg, &req.strokes);
    let history_resumed = resume.is_some();
    if let Some((generation, handle)) = state.drawer.start_drawing(
        req.strokes,
        req.cfg,
        resume,
        req.revision,
        req.request_fingerprint,
    ) {
        let _ = app.emit(
            "drawing-state",
            DrawingStateEvent {
                generation,
                active: true,
                rehearsal: false,
            },
        );
        let app2 = app.clone();
        // 阻塞等待绘制线程结束应走 spawn_blocking，避免占用 tokio async worker
        tauri::async_runtime::spawn_blocking(move || {
            let result = handle.join().unwrap_or(crate::drawer::DrawResult::Panicked);
            let finished = matches!(result, crate::drawer::DrawResult::Completed);
            let _ = app2.emit(
                "drawing-finished",
                DrawingFinishedEvent {
                    generation,
                    finished,
                    rehearsal: false,
                },
            );
            // F10 暂停（已有进度）时提示续画方式
            let paused_with_progress = matches!(result, drawer::DrawResult::Cancelled)
                && app2
                    .state::<AppState>()
                    .drawer
                    .progress
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some();
            if paused_with_progress {
                let checkpoint = app2
                    .state::<AppState>()
                    .drawer
                    .progress
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                let progress_text = checkpoint
                    .map(|cp| {
                        format!(
                            "（已完成 {} 笔，正在第 {} 笔 / 共 {} 笔）",
                            cp.stroke_index,
                            cp.stroke_index + 1,
                            cp.total
                        )
                    })
                    .unwrap_or_default();
                let _ = app2.emit(
                    "toast",
                    (
                        format!(
                            "绘制已暂停{progress_text}，按 F8 从断点继续；暂停期间请勿移动鼠标。按 F9 可从头开始绘制"
                        ),
                        "info".to_string(),
                    ),
                );
            }
            // 输入环境探测结果：非相对模式时给出警告（绘制仍按相对模式进行）
            let probe = app2
                .state::<AppState>()
                .drawer
                .last_probe
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(probe) = &probe {
                let message = match probe.mode {
                    probe::InputMode::DesktopAbsolute => Some(format!(
                        "检测到鼠标未锁定环境（{}），本次已按相对模式继续，线条位置可能不准",
                        probe.mode.as_str()
                    )),
                    probe::InputMode::Undetermined => Some(format!(
                        "输入环境未能确定（{}），已按相对模式继续",
                        probe.mode.as_str()
                    )),
                    probe::InputMode::Relative => None,
                };
                if let Some(message) = message {
                    let _ = app2.emit("toast", (message, "warning".to_string()));
                }
            }
            if let Some(message) = draw_result_error_with_diag(&app2, result) {
                let _ = app2.emit("toast", (message, "error".to_string()));
            }
            // 画作尺寸超出屏幕（不拦截，仅提示画板可能不够大）
            if let Some(message) = outside_desktop_warning(&app2) {
                let _ = app2.emit("toast", (message, "warning".to_string()));
            }
            // 历史记录（追加到 exe 旁 draw_history.json，失败不影响主流程）
            let (image_name, image_size) = history_image
                .as_ref()
                .map(|(name, size)| (name.clone(), size.clone()))
                .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
            history::append(history::HistoryEntry {
                ts: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default(),
                image: image_name,
                image_size,
                strokes: history_strokes,
                points: history_points,
                estimate_seconds: history_estimate,
                result: format!("{result:?}"),
                resumed: history_resumed,
                probe_mode: probe
                    .as_ref()
                    .map(|p| p.mode.as_str().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                probe_note: probe.map(|p| p.note).unwrap_or_default(),
            });
        });
    } else {
        let _ = app.emit(
            "toast",
            (
                "正在绘制中，请先按 F10 停止".to_string(),
                "info".to_string(),
            ),
        );
    }
}

/// F9：从头开始绘制。即使存在暂停断点也忽略（start_drawing 会清空断点），
/// 断点续画走专门的 F8。
fn handle_start(app: &AppHandle) {
    let Some(req) = prepare_draw_request(app, "开始绘制") else {
        return;
    };
    // F9 有效发起：把当前完整笔画保存进相册（后台线程执行，失败不阻断绘制）
    save_gallery_entry(app);
    // 存在暂停断点时提示已放弃，避免用户误以为会续画
    let had_checkpoint = app
        .state::<AppState>()
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_some();
    if had_checkpoint {
        let _ = app.emit(
            "toast",
            (
                "已从头开始绘制，原有暂停进度已放弃（断点续画请按 F8）".to_string(),
                "info".to_string(),
            ),
        );
    }
    start_draw(app, req, None);
}

/// F8：从断点继续绘制。无有效断点（未暂停过/进度已失效）时提示并中止。
fn handle_resume(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(req) = prepare_draw_request(app, "续画") else {
        return;
    };
    let checkpoint = state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let resume = match &checkpoint {
        Some(cp)
            if cp.revision == req.revision
                && cp.request_fingerprint == req.request_fingerprint
                && cp.stroke_index < cp.total
                && cp.total <= req.strokes.len() =>
        {
            Some(cp.clone())
        }
        _ => None,
    };
    if resume.is_none() {
        if checkpoint.is_some() {
            // 断点保留不删除：参数/选区变化导致的失效是"可逆"的，用户调回参数后
            // 仍可按 F8 续画（换图/重新生成等不可逆路径已在上游清除断点）。
            let _ = app.emit(
                "toast",
                (
                    "暂停进度暂不可用：绘制参数或选区已变化。调回参数后可按 F8 续画，或按 F9 从头开始绘制"
                        .to_string(),
                    "info".to_string(),
                ),
            );
        } else {
            let _ = app.emit(
                "toast",
                (
                    "没有可续画的进度，请按 F9 从头开始绘制".to_string(),
                    "info".to_string(),
                ),
            );
        }
        return;
    }
    if let Some(cp) = &resume {
        let _ = app.emit(
            "toast",
            (
                format!(
                    "从断点继续绘制（第 {} 笔 / 共 {} 笔），暂停期间请勿移动鼠标",
                    cp.stroke_index + 1,
                    cp.total
                ),
                "info".to_string(),
            ),
        );
    }
    start_draw(app, req, resume);
}

/// 边界预演（Shift+F9）：只移动鼠标到画作四角，不按下左键
fn handle_rehearse(app: &AppHandle) {
    let state = app.state::<AppState>();
    // 前置检查与 F9/F8 共用（处理中/笔画/版本/区域筛选）
    let Some(req) = prepare_draw_request(app, "预演") else {
        return;
    };
    let started = {
        let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
        if !draw_request_is_current(&state, &req) {
            emit_stale_draw_request(app);
            None
        } else {
            // 区域补画模式下预演同样只走命中笔画，明确提示避免误以为在预演全图
            if req.filter_active {
                let _ = app.emit(
                    "toast",
                    (
                        format!(
                            "区域补画预演：仅预演与选区相交的 {} 笔（共 {} 笔）",
                            req.strokes.len(),
                            req.filter_total
                        ),
                        "info".to_string(),
                    ),
                );
            }
            state.drawer.rehearse(req.strokes, req.cfg)
        }
    };
    if let Some((generation, handle)) = started {
        let _ = app.emit(
            "drawing-state",
            DrawingStateEvent {
                generation,
                active: true,
                rehearsal: true,
            },
        );
        let app2 = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let result = handle.join().unwrap_or(crate::drawer::DrawResult::Panicked);
            let _ = app2.emit(
                "drawing-finished",
                DrawingFinishedEvent {
                    generation,
                    finished: matches!(result, drawer::DrawResult::Completed),
                    rehearsal: true,
                },
            );
            match result {
                drawer::DrawResult::Completed => {
                    let _ = app2.emit(
                        "toast",
                        (
                            "边界预演完成，位置确认后按 F9 开始绘制（Shift+F9 再次预演）"
                                .to_string(),
                            "info".to_string(),
                        ),
                    );
                }
                drawer::DrawResult::Cancelled => {
                    let _ = app2.emit("toast", ("预演已取消".to_string(), "info".to_string()));
                }
                other => {
                    if let Some(message) = draw_result_error_with_diag(&app2, other) {
                        let _ = app2.emit("toast", (message, "error".to_string()));
                    }
                }
            }
            // 画作尺寸超出屏幕（不拦截，仅提示画板可能不够大）
            if let Some(message) = outside_desktop_warning(&app2) {
                let _ = app2.emit("toast", (message, "warning".to_string()));
            }
        });
    } else {
        let _ = app.emit(
            "toast",
            (
                "正在绘制中，请先按 F10 停止".to_string(),
                "info".to_string(),
            ),
        );
    }
}

/// 当前绘制断点（供前端展示续画进度）
#[derive(Clone, serde::Serialize)]
struct DrawProgressView {
    stroke_index: usize,
    point_index: usize,
    total: usize,
    revision: u64,
    updated_at: u128,
}

#[tauri::command]
fn get_draw_progress(state: State<'_, AppState>) -> Option<DrawProgressView> {
    state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|cp| DrawProgressView {
            stroke_index: cp.stroke_index,
            point_index: cp.point_index,
            total: cp.total,
            revision: cp.revision,
            updated_at: cp.updated_at,
        })
}

/// 放弃断点：下一次 F9 从第一笔重新开始
#[tauri::command]
fn reset_draw_progress(state: State<'_, AppState>) -> Result<(), String> {
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    Ok(())
}

/// 最近一次输入环境探测结果（供前端展示/诊断）
#[derive(Clone, serde::Serialize)]
struct ProbeOutcomeView {
    mode: String,
    probe_distance: i32,
    samples: usize,
    note: String,
}

#[tauri::command]
fn get_input_diagnosis(state: State<'_, AppState>) -> Option<ProbeOutcomeView> {
    state
        .drawer
        .last_probe
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|outcome| ProbeOutcomeView {
            mode: outcome.mode.as_str().to_string(),
            probe_distance: outcome.probe_distance,
            samples: outcome.samples,
            note: outcome.note.clone(),
        })
}

fn handle_stop(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.drawer.stop_drawing();
}

// ===================== 笔画相册 =====================
/// F9 有效发起时把当前完整笔画快照保存进相册（内容哈希 = 当前图片文件）。
/// 保存的是 state.strokes 全量（非区域筛选子集）；后台线程执行，
/// 失败仅发 warning toast，不影响绘制。
fn save_gallery_entry(app: &AppHandle) {
    let state = app.state::<AppState>();
    let snapshot = {
        let image = state
            .image
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(image) = image else { return };
        let strokes = state
            .strokes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if strokes.is_empty() {
            return;
        }
        let cfg = state
            .config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let ai = state
            .ai_config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let ai_fallback = *state
            .last_ai_fallback
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        (image, strokes, cfg, ai, ai_fallback)
    };
    let (image, strokes, cfg, ai, ai_fallback) = snapshot;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let stroke_count = strokes.len();
        let point_count: usize = strokes.iter().map(|s| s.points.len()).sum();
        let estimate = estimate_draw_seconds(&cfg.drawing, &strokes);
        // 缩略图按内容哈希复用：同一张图的缩略图每次生成结果相同，
        // 已有非空缩略图时直接复用，避免每次 F9 全量解码原图（最大 4000 万像素）。
        // 内容哈希相同 => 图像字节相同 => 缩略图逐位一致，行为与重新生成完全等价。
        // 复用查询走视图解析（不含 strokes），避免为读缩略图反序列化整份笔画。
        let thumbnail = match gallery::lookup_view(&image.content_hash) {
            Some(previous) if !previous.thumbnail.is_empty() => previous.thumbnail,
            _ => gallery::make_thumbnail(&image.path),
        };
        let entry = gallery::GalleryEntry {
            image_hash: image.content_hash,
            source_hash: image.source_hash.clone(),
            crop: image.crop_rect,
            image_name: image.file_name,
            image_size: format!("{}×{}", image.width, image.height),
            thumbnail,
            strokes,
            stroke_count,
            point_count,
            saved_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
            estimate_seconds: estimate,
            blur_size: cfg.image.blur_size,
            epsilon_ratio: cfg.contour.epsilon_ratio,
            use_ai: cfg.use_ai,
            ai_fallback,
            ai_model: ai.model,
            ai_endpoint: ai.api_endpoint,
        };
        if let Err(e) = gallery::save(&entry) {
            eprintln!("笔画相册保存失败：{e}");
            let _ = app2.emit(
                "toast",
                (format!("相册保存失败：{e}"), "warning".to_string()),
            );
        }
    });
}

/// 相册命中查询（导入/裁剪后由前端调用）：exact = 当前内容哈希的精确条目
/// （内容一致才可安全载入）；variants = 同一来源图的全部其他变体
/// （全图版优先，其余按保存时间倒序；不含 exact）。
/// async 标记：目录扫描 + 条目令牌扫描（不含笔画本体）不阻塞主线程
#[derive(Clone, serde::Serialize)]
struct GalleryMatchView {
    exact: Option<gallery::GalleryView>,
    variants: Vec<gallery::GalleryView>,
}

#[tauri::command(async)]
fn gallery_check(source_hash: String, content_hash: String) -> Option<GalleryMatchView> {
    if !gallery::valid_hash(&source_hash) || !gallery::valid_hash(&content_hash) {
        return None;
    }
    // 精确命中与来源扫描都走视图解析（不含 strokes 的令牌扫描）：
    // 每次导入/裁剪都会调用本命令，无需为读取元数据反序列化完整笔画
    let exact = gallery::lookup_view(&content_hash);
    let variants: Vec<gallery::GalleryView> = gallery::lookup_by_source(&source_hash)
        .into_iter()
        .filter(|view| view.image_hash != content_hash)
        .collect();
    if exact.is_none() && variants.is_empty() {
        return None;
    }
    Some(GalleryMatchView { exact, variants })
}

/// 相册条目列表（按保存时间倒序；不含笔画本体）。
/// async 标记：全量条目反序列化不阻塞主线程
#[tauri::command(async)]
fn gallery_list() -> Vec<gallery::GalleryView> {
    gallery::list()
}

/// 删除相册条目
#[tauri::command]
fn gallery_delete(hash: String) -> Result<(), String> {
    gallery::delete(&hash)
}

/// 从相册恢复笔画到当前工作区（要求当前图片与条目的内容哈希一致）。
/// 恢复后的笔画与"刚生成"完全等价：strokes_revision 对齐当前版本、
/// 断点与区域筛选清空，前端据此进入 ready 状态。
/// async 标记：条目反序列化 + 笔画克隆不阻塞主线程
#[tauri::command(async)]
fn gallery_restore(state: State<'_, AppState>, hash: String) -> Result<ProcessOutcome, String> {
    if !gallery::valid_hash(&hash) {
        return Err("相册条目标识无效".to_string());
    }
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再载入笔画".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let _commit = state.commit_lock.lock().unwrap_or_else(|p| p.into_inner());
    if state.processing.load(Ordering::SeqCst) {
        return Err("正在处理图像，请稍候再载入笔画".to_string());
    }
    if state.drawer.is_busy() {
        return Err("正在绘制中，请先按 F10 停止".to_string());
    }
    let current_hash = state
        .image
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|i| i.content_hash.clone())
        .ok_or("请先选择图片")?;
    if current_hash != hash {
        return Err("当前工作区图片与相册条目不匹配".to_string());
    }
    let entry = gallery::lookup(&hash).ok_or("相册条目不存在或已损坏")?;
    let revision = state.revision();
    let estimate = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        estimate_draw_seconds(&cfg.drawing, &entry.strokes)
    };
    // 先在借用的 entry.strokes 上构建结果视图，再把笔画本体 move 进状态：
    // 避免对可能达数十 MB 的笔画数据做第二次完整克隆
    let outcome =
        ProcessOutcome::from_strokes(&entry.strokes, entry.ai_fallback, revision, estimate);
    *state.strokes.lock().unwrap_or_else(|p| p.into_inner()) = entry.strokes;
    *state
        .strokes_revision
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(revision);
    *state
        .drawer
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    *state
        .region_filter
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
    Ok(outcome)
}

#[tauri::command]
fn drawing_active(state: State<'_, AppState>) -> bool {
    state.drawer.is_busy()
}

/// 取走 setup 阶段累积的错误消息（取走即清空）
#[tauri::command]
fn get_startup_errors(state: State<'_, AppState>) -> Vec<String> {
    std::mem::take(
        &mut *state
            .startup_errors
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    )
}

/// 窗口原生背景色与主题同步（窗口层 + WebView2 层双路）：
/// tauri.conf 的 backgroundColor 是静态值（深色），而 WebView2 在隐藏窗口下尚未
/// 上屏，show() 瞬间会先露一帧原生背景——浅色主题用户若不提前同步，启动会有一帧
/// 深色背景（"深→浅"闪变）。主题切换（save_config / sync_config）同样调用本函数。
/// 除 tauri 的双路 set_background_color 外，另用 GDI 直接填充客户区内存表面：
/// tao 的 WM_ERASEBKGND（FillRect）依赖 EraseBkgnd 未触发时的表面是"未初始化黑"，
/// 直接预填可让 DWM 首次合成帧即主题色（填充与 show 前的调用顺序无关、幂等）。
fn sync_window_background(app: &AppHandle, dark: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let color = if dark {
            tauri::window::Color(8, 8, 9, 255) // #080809 = styles.css 深色主题 --bg
        } else {
            tauri::window::Color(244, 244, 245, 255) // #f4f4f5 = styles.css 浅色主题 --bg
        };
        let _ = window.set_background_color(Some(color));
        #[cfg(windows)]
        if let Ok(hwnd) = window.hwnd() {
            fill_client_surface(hwnd.0 as isize, color);
        }
    }
}

/// 把窗口客户区的 GDI 内存表面直接填充为指定颜色（不经过消息循环/ERASE 时序）。
/// 窗口未显示时 GetDC 得到的也是内存表面（存在性有效），填充后 show 的首个合成帧
/// 即此颜色。tauri::window::Color 为 (r,g,b,a) tuple。
#[cfg(windows)]
fn fill_client_surface(hwnd: isize, color: tauri::window::Color) {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    unsafe {
        let handle = hwnd as HWND;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetClientRect(handle, &mut rect) == 0 {
            return;
        }
        let hdc = GetDC(handle);
        if hdc.is_null() {
            return;
        }
        let brush = CreateSolidBrush(
            u32::from(color.2) | (u32::from(color.1) << 8) | (u32::from(color.0) << 16),
        );
        if !brush.is_null() {
            FillRect(hdc, &rect, brush);
            DeleteObject(brush);
        }
        ReleaseDC(handle, hdc);
    }
}

/// 启动时前端在窗口显示前调用：把原生背景同步为当前主题色
/// （前端渲染完成 → 同步背景 → show，避免 show 瞬间露错颜色背景帧）。
#[tauri::command]
fn apply_window_background(app: AppHandle, dark: bool) {
    sync_window_background(&app, dark);
}

// ===================== 窗口阴影 =====================
/// 禁用 DWM 系统阴影（SetWindowCompositionAttribute WCA_DROP_SHADOW）
/// 注意：tauri.conf.json 的 "shadow": false 在 Windows 上会导致窗口不可见（tao bug），
/// 所以必须在窗口创建后于 setup 中手动禁用
#[cfg(windows)]
fn disable_window_shadow(hwnd: isize) {
    use std::os::raw::c_void;
    #[repr(C)]
    struct CompositionAttrData {
        attribute: i32,
        data: *mut c_void,
        size: usize,
    }
    // 该 API 未包含在各版本 Windows SDK 的 user32.lib 中，
    // 用 raw-dylib 直接从 user32.dll 导入，避免依赖具体 SDK 版本
    #[link(name = "user32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn SetWindowCompositionAttribute(
            hwnd: *const c_void,
            data: *const CompositionAttrData,
        ) -> i32;
    }
    const WCA_DROP_SHADOW: i32 = 0x14;
    let mut value: i32 = 0; // FALSE：禁用阴影
    let data = CompositionAttrData {
        attribute: WCA_DROP_SHADOW,
        data: &mut value as *mut i32 as *mut c_void,
        size: std::mem::size_of::<i32>(),
    };
    unsafe {
        let ret = SetWindowCompositionAttribute(hwnd as *const c_void, &data);
        eprintln!("SetWindowCompositionAttribute(WCA_DROP_SHADOW=0) = {ret} (1=成功)");
    }
}

// ===================== 入口 =====================
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    let f9 = Shortcut::new(None, Code::F9);
    let f10 = Shortcut::new(None, Code::F10);
    let f8 = Shortcut::new(None, Code::F8);
    let shift_f9 = Shortcut::new(Some(Modifiers::SHIFT), Code::F9);
    let f9_id = f9.id();
    let f10_id = f10.id();
    let f8_id = f8.id();
    let shift_f9_id = shift_f9.id();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 已有一个实例在运行：聚焦已有窗口
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let id = shortcut.id();
                        if id == f9_id {
                            handle_start(app);
                        } else if id == f10_id {
                            handle_stop(app);
                        } else if id == f8_id {
                            handle_resume(app);
                        } else if id == shift_f9_id {
                            handle_rehearse(app);
                        }
                    }
                })
                .build(),
        )
        .manage(AppState::default())
        // 窗口关闭时停止绘制线程：否则 OS 绘制线程仍在 SendInput，VRChat 收到幽灵鼠标事件
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                let state = window.app_handle().state::<AppState>();
                state.drawer.stop_drawing();
                // 等待绘制线程退出（最多 2 秒），避免恰在 press 后 release 前被进程
                // 终止而在 VRChat 里残留左键按住状态
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while !state.drawer.is_idle() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                cleanup_crop_caches(&state, None);
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            sync_config,
            reset_config,
            get_ai_config,
            save_ai_config,
            test_ai_connection,
            fetch_ai_models,
            pick_image,
            import_image,
            import_clipboard_image,
            crop_image,
            process_image,
            clear_workspace,
            filter_strokes,
            clear_strokes_filter,
            drawing_active,
            get_draw_progress,
            reset_draw_progress,
            get_input_diagnosis,
            gallery_check,
            gallery_list,
            gallery_delete,
            gallery_restore,
            get_startup_errors,
            apply_window_background,
        ])
        .setup(move |app| {
            cleanup_orphaned_crop_caches();
            // 窗口图标（任务栏显示）：用内嵌 PNG 显式设置，避免系统缓存旧 exe 图标
            #[cfg(desktop)]
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(img) =
                    tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))
                {
                    let _ = window.set_icon(img);
                }
                // 禁用 DWM 系统阴影（用户要求：窗口四周阴影过深）。
                // 仅此一次：tao 0.35 的 set_visible（window_state.rs apply_diff）只做
                // ShowWindow(SW_SHOW)，不会重应用阴影——曾有"show 时重新应用阴影、
                // 首次 Resized 后再禁一次"的冗余调用（每启动固定触发 SetWindowCompositionAttribute
                // 图层重建，是"启动黑帧"的高嫌疑来源），已删除。
                #[cfg(windows)]
                if let Ok(hwnd) = window.hwnd() {
                    disable_window_shadow(hwnd.0 as isize);
                }
            }
            // 注册全局热键（失败仅警告，不阻断启动；错误入队由前端 init 后拉取）
            {
                let register = |shortcut: Shortcut, label: &str, detail: &str| {
                    if let Err(error) = app.global_shortcut().register(shortcut) {
                        eprintln!("警告：{label} 全局热键注册失败：{error}");
                        app.state::<AppState>()
                            .startup_errors
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(format!("{label} 全局热键注册失败：{error}，{detail}"));
                    }
                };
                register(f9, "F9", "请重启应用后重试 F9");
                register(f10, "F10", "请重启应用后重试 F10");
                register(f8, "F8", "断点续画功能不可用");
                register(shift_f9, "Shift+F9", "预演功能不可用");
            }
            // 窗口延迟显示兜底（tauri.conf 的 visible:false 由前端 init 完成后 show）：
            // 若前端 show 因异常未执行，3 秒后强制显示，防止窗口永久隐藏。
            // 走 spawn_blocking：std::thread::sleep 不应占用异步 worker。
            {
                let app2 = app.handle().clone();
                tauri::async_runtime::spawn_blocking(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    if let Some(window) = app2.get_webview_window("main") {
                        if !window.is_visible().unwrap_or(false) {
                            let _ = window.show();
                        }
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_cache_paths_are_unique() {
        let first = crop_cache_path();
        let second = crop_cache_path();
        assert_ne!(first, second, "连续裁剪不能复用同一个缓存路径");
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert_eq!(second.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert!(first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("crop_cache.")));
    }

    #[test]
    fn crop_cache_keeps_previous_result_readable() {
        let first = save_crop_cache(&image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(8, 6, image::Rgb([255, 0, 0])),
        ))
        .expect("第一次裁剪缓存应能写入");
        let second = save_crop_cache(&image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(4, 3, image::Rgb([0, 255, 0])),
        ))
        .expect("第二次裁剪缓存应能写入");

        assert_ne!(first, second);
        let first_img = load_image_any(first.to_str().unwrap()).unwrap();
        let second_img = load_image_any(second.to_str().unwrap()).unwrap();
        assert_eq!((first_img.width(), first_img.height()), (8, 6));
        assert_eq!((second_img.width(), second_img.height()), (4, 3));
        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_file(second);
    }

    /// 下载图常见情况：扩展名 .png 但内容为 JPEG——load_image_any 必须按内容嗅探格式解码。
    /// 测试自包含：运行时生成 JPEG 字节写入 .png 扩展名的临时文件，不依赖机器特定路径
    #[test]
    fn load_image_any_sniffs_jpeg_with_png_extension() {
        // 生成一张 8x8 灰阶 JPEG
        let jpeg: Vec<u8> = {
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
                8,
                8,
                image::Luma([128u8]),
            ))
            .write_to(&mut buf, image::ImageFormat::Jpeg)
            .expect("JPEG 编码应成功");
            buf.into_inner()
        };
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "应为 JPEG 内容");
        let tmp = std::env::temp_dir().join("vrc_sniff_test.png");
        std::fs::write(&tmp, &jpeg).unwrap();
        let img = load_image_any(tmp.to_str().unwrap()).expect("应能按内容嗅探格式解码");
        assert!(img.width() > 0 && img.height() > 0);
        let _ = std::fs::remove_file(&tmp);
    }

    fn stroke(points: &[(f64, f64)]) -> types::DrawingStroke {
        types::DrawingStroke {
            points: points
                .iter()
                .map(|&(x, y)| types::DrawingPoint { x, y })
                .collect(),
        }
    }

    #[test]
    fn region_filter_keeps_strokes_inside_and_crossing_rect() {
        let rect = RegionFilter {
            x: 10.0,
            y: 10.0,
            w: 10.0,
            h: 10.0,
        };
        let inside = stroke(&[(12.0, 12.0), (15.0, 15.0)]);
        let crossing = stroke(&[(0.0, 15.0), (25.0, 15.0)]); // 长线段穿过选区
        let outside = stroke(&[(0.0, 0.0), (5.0, 5.0)]);
        let boundary = stroke(&[(10.0, 10.0), (10.0, 20.0)]); // 贴边（含边界应保留）
        let all = vec![inside, crossing, outside, boundary];
        let filtered = filter_strokes_for_draw(&all, &rect);
        assert_eq!(filtered.len(), 3);
        assert!(filtered.iter().all(|s| s.points.iter().all(|p| p.x >= 0.0))); // 方向与点序不变
        assert_eq!(filtered[0].points.len(), 2);
        assert_eq!(
            filtered[0].points[0],
            types::DrawingPoint { x: 12.0, y: 12.0 }
        );
    }

    #[test]
    fn region_filter_rejects_empty_selection() {
        let rect = RegionFilter {
            x: 100.0,
            y: 100.0,
            w: 5.0,
            h: 5.0,
        };
        let all = vec![stroke(&[(0.0, 0.0), (1.0, 1.0)]), stroke(&[(50.0, 50.0)])];
        assert!(filter_strokes_for_draw(&all, &rect).is_empty());
    }

    #[test]
    fn segment_rect_intersection_handles_edges() {
        let p = |x: f64, y: f64| types::DrawingPoint { x, y };
        // 完全在矩形内的线段
        assert!(segment_intersects_rect(
            &p(1.0, 1.0),
            &p(2.0, 2.0),
            0.0,
            0.0,
            10.0,
            10.0
        ));
        // 穿过矩形的斜线
        assert!(segment_intersects_rect(
            &p(-5.0, 5.0),
            &p(15.0, 5.0),
            0.0,
            0.0,
            10.0,
            10.0
        ));
        // 平行于 x 轴但完全在矩形上方
        assert!(!segment_intersects_rect(
            &p(0.0, -1.0),
            &p(10.0, -1.0),
            0.0,
            0.0,
            10.0,
            10.0
        ));
        // 端点贴边（含边界）
        assert!(segment_intersects_rect(
            &p(0.0, 0.0),
            &p(10.0, 0.0),
            0.0,
            0.0,
            10.0,
            10.0
        ));
    }

    #[test]
    fn compose_crop_rect_translates_nested_crops() {
        assert_eq!(
            compose_crop_rect(None, 10, 20, 30, 40),
            gallery::CropRect {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            }
        );
        // 裁中裁：子坐标相对父区域，根坐标 = 父原点 + 子原点
        assert_eq!(
            compose_crop_rect(
                Some(gallery::CropRect {
                    x: 100,
                    y: 80,
                    w: 400,
                    h: 320
                }),
                5,
                6,
                7,
                8
            ),
            gallery::CropRect {
                x: 105,
                y: 86,
                w: 7,
                h: 8
            }
        );
    }

    #[test]
    fn estimate_uses_step_distance_not_point_count() {
        // 3 点密集短线（2 段 × 1px）vs 2 点长直线（1 段 × 40px，按 max_step=4 需 10 步）：
        // 点数相近时，长线段的估算应明显更大（按步数模型），验证估算不再低估长直线。
        let cfg = config::DrawingConfig::default();
        let dense = stroke(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)]);
        let sparse = stroke(&[(0.0, 0.0), (40.0, 0.0)]);
        let dense_secs = estimate_draw_seconds(&cfg, std::slice::from_ref(&dense));
        let sparse_secs = estimate_draw_seconds(&cfg, std::slice::from_ref(&sparse));
        assert!(
            sparse_secs > dense_secs,
            "长直线段应按步数估算：{sparse_secs} 应大于 {dense_secs}"
        );
        // 单点笔画按固定开销 + 按下保持估算（绘制端真实执行按下/抬起，不再估算为 0）
        let dot = stroke(&[(5.0, 5.0)]);
        assert!(estimate_draw_seconds(&cfg, std::slice::from_ref(&dot)) > 0.0);
    }

    #[test]
    fn estimate_counts_lift_travel_and_speed() {
        // 抬笔速度更慢（50 速 = 25ms/步）时，同一批笔画的估算应更大
        let pair = vec![
            stroke(&[(0.0, 0.0), (40.0, 0.0)]),
            stroke(&[(100.0, 100.0), (140.0, 100.0)]),
        ];
        let slow = config::DrawingConfig {
            lift_pen_speed: 50.0,
            ..config::DrawingConfig::default()
        };
        let fast = config::DrawingConfig {
            lift_pen_speed: 100.0,
            ..config::DrawingConfig::default()
        };
        let slow_secs = estimate_draw_seconds(&slow, &pair);
        let fast_secs = estimate_draw_seconds(&fast, &pair);
        assert!(
            slow_secs > fast_secs,
            "抬笔速度更慢时估算应更大：{slow_secs} 应大于 {fast_secs}"
        );
    }

    #[test]
    fn estimate_counts_turn_slowdown() {
        // L 形（直角转弯）与等长等段数的直线对比：唯一差异是转弯处
        // adaptive 步长降档（drawer::adaptive_pen_down_step），估算应显著更大
        let cfg = config::DrawingConfig::default();
        let straight = stroke(&[(0.0, 0.0), (40.0, 0.0), (80.0, 0.0)]);
        let corner = stroke(&[(0.0, 0.0), (40.0, 0.0), (40.0, 40.0)]);
        let straight_secs = estimate_draw_seconds(&cfg, std::slice::from_ref(&straight));
        let corner_secs = estimate_draw_seconds(&cfg, std::slice::from_ref(&corner));
        assert!(
            corner_secs > straight_secs * 1.5,
            "直角转弯应因步长降档显著增大估算：{corner_secs} 应明显大于 {straight_secs}"
        );
    }

    #[test]
    fn estimate_counts_double_click_guard() {
        // 两个单点笔画各自约 0.236s（< 600ms 防护线）：
        // 第二笔按下前应补足 600ms 间隔（drawer::button_guard_remaining 同语义）
        let cfg = config::DrawingConfig::default();
        let dots = vec![stroke(&[(5.0, 5.0)]), stroke(&[(15.0, 15.0)])];
        let two = estimate_draw_seconds(&cfg, &dots);
        let one = estimate_draw_seconds(&cfg, std::slice::from_ref(&dots[0]));
        let guard_wait = two - one * 2.0;
        assert!(
            (0.30..=0.40).contains(&guard_wait),
            "第二笔应补足 600ms 双击防护：差值 {guard_wait:.3}s 应约为 0.364s"
        );
    }
}
