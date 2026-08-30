#[cfg(windows)]
use windows_sys::Win32::Foundation::POINT;
#[cfg(windows)]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEINPUT,
};
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetCursorPos, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::DrawingConfig;
use crate::pipeline::optimize::{optimize_drawing_path_lossless, strokes_to_drawing_path};
use crate::pipeline::types::{PointF, StrokeRouteMetadata};
use crate::probe;
use crate::types::DrawingStroke;

const MIN_PEN_MOVE_INTERVAL_SECS: f64 = 0.016;
const MIN_BUTTON_DOWN_INTERVAL: Duration = Duration::from_millis(600);
const MAX_EXACT_F32_INTEGER: i64 = 16_777_216;
/// 预演时在每个角停留的时长（让用户看清边界位置）
const REHEARSE_CORNER_HOLD_MS: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawResult {
    Completed,
    Cancelled,
    VrchatNotFound,
    CursorUnavailable,
    InputInjectionFailed,
    InvalidStrokes,
    /// 绘制过程中目标窗口失去前台焦点（用户切换到其他窗口/最小化）
    TargetLost,
    /// 暂停进度与当前参数/选区不匹配（参数或选区已变化），禁止错误续画
    StaleCheckpoint,
    Panicked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveResult {
    Completed,
    Cancelled,
    InputInjectionFailed,
    TargetLost,
}

/// 绘制目标窗口的前台守卫：按进程 ID 比对前台窗口。
/// 比窗口句柄比对更稳（VRChat 可能重建窗口），与 VRC-Draw 的 TargetStillForeground 一致。
pub struct TargetGuard {
    hwnd: isize,
    pid: u32,
}

impl TargetGuard {
    pub fn still_foreground(&self) -> bool {
        #[cfg(windows)]
        {
            unsafe {
                let foreground = GetForegroundWindow();
                if foreground.is_null() {
                    // 无法确认目标窗口时必须 fail-closed，不能继续注入鼠标。
                    return false;
                }
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(foreground, &mut pid);
                if foreground == self.hwnd as _ {
                    return pid != 0 && pid == self.pid;
                }
                // VRChat 可能重建窗口句柄；仅在同 PID 且窗口标题仍为 VRChat 时兼容，
                // 避免把同一进程的隐藏窗口/弹窗误当作绘制目标。
                if pid == 0 || pid != self.pid {
                    return false;
                }
                let mut title_buf = [0u16; 128];
                let len =
                    GetWindowTextW(foreground, title_buf.as_mut_ptr(), title_buf.len() as i32);
                len > 0 && String::from_utf16_lossy(&title_buf[..len as usize]) == "VRChat"
            }
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// 尝试把目标窗口拉回前台（Windows 无边框游戏下任务栏"搜索"等系统 UI
    /// 会抢走前台，VRChat 因 RawInput 全局捕获仍响应鼠标，用户无法察觉）。
    ///
    /// 解锁方式：按住 Alt（解除"仅前台进程可设置前台"的锁）→
    /// AttachThreadInput 附加到前台线程（使 SetForegroundWindow 被视为前台调用）→
    /// SetForegroundWindow → 松开 Alt。
    #[cfg(windows)]
    pub fn try_restore_foreground(&self, active: &AtomicBool) -> bool {
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            keybd_event, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_MENU,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let hwnd = self.hwnd as HWND;
        unsafe {
            // 标准顺序：先按住 Alt 再切前台，最后松开
            keybd_event(VK_MENU as u8, 0, KEYEVENTF_EXTENDEDKEY, 0);
            let foreground = GetForegroundWindow();
            let mut foreground_tid: u32 = 0;
            if !foreground.is_null() {
                foreground_tid = GetWindowThreadProcessId(foreground, std::ptr::null_mut::<u32>());
            }
            let our_tid = GetCurrentThreadId();
            let attached = foreground_tid != 0
                && foreground_tid != our_tid
                && AttachThreadInput(our_tid, foreground_tid, 1) != 0;
            SetForegroundWindow(hwnd);
            keybd_event(VK_MENU as u8, 0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, 0);
            if attached {
                AttachThreadInput(our_tid, foreground_tid, 0);
            }
        }
        interruptible_sleep(active, Duration::from_millis(100)) && self.still_foreground()
    }

    #[cfg(not(windows))]
    pub fn try_restore_foreground(&self, active: &AtomicBool) -> bool {
        let _ = active;
        true
    }
}

/// 前台是否为 Windows 11 系统浮层（搜索/开始菜单等"偷"走前台的浮窗）：
/// 这类前台不代表用户真的切走了（常因鼠标悬停任务栏触发），拉回时给予更长重试。
///
/// 注意：不要用 GetWindowModuleFileNameW 判断跨进程窗口——该 API 对非本进程
/// 窗口通常返回空/不可靠，会导致识别恒为 false。这里改用
/// GetWindowThreadProcessId + OpenProcess + QueryFullProcessImageNameW。
#[cfg(windows)]
fn foreground_is_system_ui_float() -> bool {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return false;
        }
        let mut pid: u32 = 0;
        if GetWindowThreadProcessId(foreground, &mut pid) == 0 || pid == 0 {
            return false;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut buffer = [0u16; 64];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length);
        windows_sys::Win32::Foundation::CloseHandle(process);
        if ok == 0 {
            return false;
        }
        let path = String::from_utf16_lossy(&buffer[..length as usize]).to_lowercase();
        let file = std::path::Path::new(&path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_float = matches!(
            file.as_str(),
            "searchhost.exe" | "startmenuexperiencehost.exe" | "shellexperiencehost.exe"
        );
        if is_float {
            eprintln!("诊断：前台为系统浮层 {file}，拉回重试次数放宽为 12 次");
        }
        is_float
    }
}

#[cfg(not(windows))]
fn foreground_is_system_ui_float() -> bool {
    false
}

/// 前台检查（带拉回重试）：前台不是目标时尝试把目标拉回前台再查。
/// 有边框 VRChat 下任务栏可见，鼠标扫到搜索/开始菜单图标会触发系统浮层抢前台；
/// 前台是这类浮层时最多拉回 12 次（约 1.3s），普通失焦拉回 5 次（约 0.6s）；
/// 拉回失败（用户确实切走）才判定失焦。F10 可随时中断。
/// 返回 true = 前台是目标（或已恢复）；false = 确认失焦。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundFailure {
    Cancelled,
    Lost,
}

fn foreground_with_retry(
    guard: &TargetGuard,
    active: &AtomicBool,
) -> Result<(), ForegroundFailure> {
    if guard.still_foreground() {
        return Ok(());
    }
    let retries = if foreground_is_system_ui_float() {
        12
    } else {
        5
    };
    for _ in 0..retries {
        if !active.load(Ordering::SeqCst) {
            return Err(ForegroundFailure::Cancelled);
        }
        if guard.try_restore_foreground(active) {
            return Ok(());
        }
    }
    if active.load(Ordering::SeqCst) {
        Err(ForegroundFailure::Lost)
    } else {
        Err(ForegroundFailure::Cancelled)
    }
}

/// 前台窗口描述（标题 + 进程 ID），用于失焦/聚焦失败时的诊断提示
#[cfg(windows)]
fn foreground_window_desc() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return "（无前台窗口）".to_string();
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let mut title_buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), 256);
        let title = String::from_utf16_lossy(&title_buf[..len.max(0) as usize]);
        format!("「{title}」（pid {pid}）")
    }
}

#[cfg(not(windows))]
fn foreground_window_desc() -> String {
    String::new()
}

/// 绘制进度断点：F10 暂停后保留，F8 从该位置继续（F9 从头开始并清空断点）。
/// 坐标沿用同一偏移（不依赖暂停后光标位置——VRChat 捕获鼠标时光标位置不可信）。
#[derive(Debug, Clone, PartialEq)]
pub struct DrawCheckpoint {
    /// 已完成的笔画数（继续时跳过这么多笔）
    pub stroke_index: usize,
    /// 当前笔画已绘制到的点下标（0 表示该笔尚未开始）
    pub point_index: usize,
    /// 最后落笔的屏幕坐标
    pub last_screen: (f64, f64),
    /// 本次绘制使用的坐标映射偏移（offset_x, offset_y）
    pub offset: (f64, f64),
    /// 排序后的笔画总数（前端显示进度用）
    pub total: usize,
    /// 对应工作区版本（图片/笔画变化后旧断点自动失效）
    pub revision: u64,
    /// 对应本次绘制的坐标映射与区域筛选指纹，参数变化后禁止错误续画。
    pub request_fingerprint: u64,
    /// 最近更新时间戳（纳秒）
    pub updated_at: u128,
}

#[cfg(windows)]
fn send_mouse_event(flags: u32, dx: i32, dy: i32) -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let injected = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    if injected == 0 {
        // SendInput 失败（UIPI/权限/会话切换）：由调用方累计计数决定是否中止
        return false;
    }
    true
}

/// 相对增量移动：发送纯相对 mickeys（MOUSEEVENTF_MOVE，不带 ABSOLUTE）。
/// VRChat 桌面模式会锁定/拉回光标，绝对模式（GetCursorPos + 归一化坐标）在每次
/// 注入后读到的是"被拉回的位置"，导致光标原地抖动 + 视角持续旋转（人物低头抽搐）。
/// 相对增量是 VRChat 期望的输入形式（参考项目 VRC-Draw 亦默认使用相对注入，
/// 并通过 ClassifyCursorBehavior 探测后才启用绝对模式）。
pub fn move_cursor_relative(dx: i32, dy: i32) -> bool {
    #[cfg(windows)]
    return send_mouse_event(MOUSEEVENTF_MOVE, dx, dy);
    #[cfg(not(windows))]
    {
        let _ = (dx, dy);
        false
    }
}

pub fn press_left() -> bool {
    #[cfg(windows)]
    {
        send_mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn release_left() -> bool {
    #[cfg(windows)]
    {
        send_mouse_event(MOUSEEVENTF_LEFTUP, 0, 0)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 释放鼠标左键的最后一道安全措施。UIPI/权限瞬态失败时进行有限重试，
/// 仍失败则记录明确诊断，由绘制路径决定是否中止。
fn release_left_checked(context: &str) -> bool {
    for _ in 0..3 {
        if release_left() {
            return true;
        }
        thread::yield_now();
    }
    eprintln!("{context}：鼠标左键释放注入失败（可能是权限/UIPI/会话切换问题）");
    false
}

#[allow(unreachable_code)]
pub fn get_cursor() -> Option<(i32, i32)> {
    #[cfg(windows)]
    {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } == 0 {
            return None;
        }
        Some((pt.x, pt.y))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[allow(unreachable_code)]
/// 聚焦 VRChat 窗口；成功时返回 (窗口句柄, 目标进程 ID)（供绘制期间的
/// 前台守卫与失焦拉回使用）。返回 None 表示未找到窗口或聚焦失败
/// （调用方根据 active 区分取消与未找到）。
///
/// 后台进程调用 SetForegroundWindow 常被 Windows 前台锁规则拒绝，因此这里
/// 轮询最多 1 秒等待前台切换完成（也覆盖用户手动切换窗口的场景）；
/// 失败原因写入 `focus_diag` 供前端展示。
pub fn focus_vrchat_window(
    active: &AtomicBool,
    focus_diag: &Mutex<Option<String>>,
) -> Option<(isize, u32)> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let name: Vec<u16> = std::ffi::OsStr::new("VRChat")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), name.as_ptr());
            if hwnd.is_null() {
                *focus_diag.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some("未找到 VRChat 窗口".to_string());
                return None;
            }
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
            // 轮询等待前台切换完成（含用户手动切换），F10 可中断
            let deadline = Instant::now() + Duration::from_millis(1000);
            while active.load(Ordering::SeqCst) && Instant::now() < deadline {
                if GetForegroundWindow() == hwnd {
                    let mut pid: u32 = 0;
                    GetWindowThreadProcessId(hwnd, &mut pid);
                    if pid != 0 {
                        return Some((hwnd as isize, pid));
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !active.load(Ordering::SeqCst) {
                return None;
            }
            // 裸 SetForegroundWindow 被前台锁拒绝时，回退 Alt 解锁技巧
            // （与 try_restore_foreground 同序：按住 Alt → 切前台 → 松开 Alt）
            {
                use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
                    keybd_event, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_MENU,
                };
                keybd_event(VK_MENU as u8, 0, KEYEVENTF_EXTENDEDKEY, 0);
                SetForegroundWindow(hwnd);
                keybd_event(VK_MENU as u8, 0, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, 0);
            }
            let fallback_deadline = Instant::now() + Duration::from_millis(300);
            while active.load(Ordering::SeqCst) && Instant::now() < fallback_deadline {
                if GetForegroundWindow() == hwnd {
                    let mut pid: u32 = 0;
                    GetWindowThreadProcessId(hwnd, &mut pid);
                    if pid != 0 {
                        return Some((hwnd as isize, pid));
                    }
                }
                thread::sleep(Duration::from_millis(30));
            }
            *focus_diag.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                "已找到 VRChat 窗口但无法获得前台焦点（当前前台：{}）",
                foreground_window_desc()
            ));
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = (active, focus_diag);
        None
    }
}

/// 可中断等待，避免 F10 或窗口关闭被长时间 sleep 阻塞。
pub(crate) fn interruptible_sleep(active: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while active.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
    false
}

fn stroke_bounds(strokes: &[DrawingStroke]) -> Option<(f64, f64, f64, f64)> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for s in strokes {
        for p in &s.points {
            if p.x.is_finite() && p.y.is_finite() {
                if p.x < min_x {
                    min_x = p.x;
                }
                if p.x > max_x {
                    max_x = p.x;
                }
                if p.y < min_y {
                    min_y = p.y;
                }
                if p.y > max_y {
                    max_y = p.y;
                }
            }
        }
    }
    if min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite() {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

/// 在最终整数鼠标坐标上再次执行无损图边合并。
///
/// 源路径优化发生在浮点图像坐标中；缩放取整后，原先略有差异的端点可能落到同一
/// 鼠标坐标。这里只合并完全相同的整数端点，并复用管线优化器的线段多重集门禁，
/// 因此不会跨空白补线。完全没有移动的笔画作为真实点笔画单独保留。
fn optimize_screen_strokes(strokes: Vec<Vec<(i32, i32)>>) -> Vec<Vec<(i32, i32)>> {
    let mut drawable = Vec::new();
    let mut point_strokes = Vec::new();

    for mut stroke in strokes {
        stroke.dedup();
        if stroke.is_empty() {
            continue;
        }
        if stroke.len() == 1 {
            point_strokes.push(stroke);
        } else {
            drawable.push(stroke);
        }
    }

    if drawable.is_empty() {
        return point_strokes;
    }

    // f32 只能在此范围内精确表示每一个整数。正常桌面坐标远小于该范围；
    // 极端钳位坐标直接跳过合并，避免浮点转换把不同端点误判为相同。
    let exactly_representable = drawable.iter().flatten().all(|&(x, y)| {
        i64::from(x).abs() <= MAX_EXACT_F32_INTEGER && i64::from(y).abs() <= MAX_EXACT_F32_INTEGER
    });
    if !exactly_representable {
        drawable.extend(point_strokes);
        return drawable;
    }

    let screen_strokes = drawable
        .iter()
        .map(|stroke| {
            stroke
                .iter()
                .map(|&(x, y)| PointF {
                    x: x as f32,
                    y: y as f32,
                })
                .collect()
        })
        .collect::<Vec<_>>();
    let metadata = screen_strokes
        .iter()
        .map(StrokeRouteMetadata::default_for)
        .collect();
    let path = strokes_to_drawing_path(1, 1, screen_strokes, metadata);
    let optimized = optimize_drawing_path_lossless(path);

    let mut result = optimized
        .strokes
        .into_iter()
        .map(|stroke| {
            stroke
                .into_iter()
                .map(|point| (point.x.round() as i32, point.y.round() as i32))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    result.extend(point_strokes);
    result
}

fn order_strokes_by_proximity(strokes: Vec<Vec<(i32, i32)>>) -> Vec<Vec<(i32, i32)>> {
    let mut ordered = Vec::new();
    let mut remaining: Vec<Option<Vec<(i32, i32)>>> = strokes.into_iter().map(Some).collect();
    // 防御：剔除空笔画，避免首笔为空时 last().unwrap() panic（上游已保证非空，防未来误用）
    remaining.retain(|stroke| stroke.as_ref().is_some_and(|points| !points.is_empty()));
    let mut remaining_count = remaining.len();
    if remaining_count == 0 {
        return ordered;
    }

    ordered.push(remaining[0].take().unwrap());
    remaining_count -= 1;
    while remaining_count > 0 {
        let last_point = ordered.last().unwrap().last().unwrap();
        let mut best_index = 0;
        let mut best_distance = f64::INFINITY;
        let mut best_reversed = false;

        for (idx, slot) in remaining.iter().enumerate() {
            let Some(stroke) = slot.as_ref() else {
                continue;
            };
            let s = stroke[0];
            let e = stroke.last().unwrap();

            let d_start = ((s.0 - last_point.0) as f64).hypot((s.1 - last_point.1) as f64);
            let d_end = ((e.0 - last_point.0) as f64).hypot((e.1 - last_point.1) as f64);

            if d_start < best_distance {
                best_distance = d_start;
                best_index = idx;
                best_reversed = false;
            }
            if d_end < best_distance {
                best_distance = d_end;
                best_index = idx;
                best_reversed = true;
            }
        }

        let mut next = remaining[best_index].take().unwrap();
        remaining_count -= 1;
        if best_reversed {
            next.reverse();
        }
        ordered.push(next);
    }
    ordered
}

#[allow(clippy::too_many_arguments)]
fn move_relatively(
    current_x: i32,
    current_y: i32,
    target: (i32, i32),
    max_step_px: i32,
    active: &Arc<AtomicBool>,
    delay: bool,
    draw_delay_sec: f64,
    lift_pen_speed: f64,
    guard: &TargetGuard,
) -> ((i32, i32), MoveResult) {
    let dx = target.0 as i64 - current_x as i64;
    let dy = target.1 as i64 - current_y as i64;
    let distance = (dx as f64).hypot(dy as f64);
    if !distance.is_finite() || distance == 0.0 {
        return ((current_x, current_y), MoveResult::Completed);
    }

    let max_step = max_step_px.max(1) as f64;
    // 防御：极端目标距离下限制步数上限，防止 i32 溢出 / 天文数字步数
    // 即使图片尺寸接近像素上限，也不允许一次移动展开成百万级输入事件。
    // 上限只影响极端长距离抬笔，落笔笔画仍由路径点和 max_step 控制。
    let steps = ((distance / max_step).ceil() as i64).clamp(1, 100_000) as i32;
    let step_dx = dx as f64 / steps as f64;
    let step_dy = dy as f64 / steps as f64;

    let mut accum_x = current_x as f64;
    let mut accum_y = current_y as f64;
    let mut last_int_x = current_x;
    let mut last_int_y = current_y;
    // 连续注入失败计数：UIPI/权限问题导致 SendInput 持续失败时中止绘制，
    // 避免"看似绘制实则空转"（release 版无控制台，eprintln 不可见）
    let mut consecutive_failures = 0u32;

    for _ in 0..steps {
        if !active.load(Ordering::SeqCst) {
            return ((last_int_x, last_int_y), MoveResult::Cancelled);
        }
        // 落笔移动逐帧检查前台，切换窗口立即中止；抬笔快速移动由调用方在落笔前复查。
        if !guard.still_foreground() {
            return ((last_int_x, last_int_y), MoveResult::TargetLost);
        }
        accum_x += step_dx;
        accum_y += step_dy;
        let next_int_x = accum_x.round() as i32;
        let next_int_y = accum_y.round() as i32;

        let rel_dx = next_int_x - last_int_x;
        let rel_dy = next_int_y - last_int_y;

        if rel_dx != 0 || rel_dy != 0 {
            let mut injected = false;
            for _ in 0..3 {
                if move_cursor_relative(rel_dx, rel_dy) {
                    injected = true;
                    break;
                }
                thread::yield_now();
            }

            if injected {
                consecutive_failures = 0;
                last_int_x = next_int_x;
                last_int_y = next_int_y;
            } else {
                // 不让失败的逻辑步累积成下一次的大跳，避免一次短暂的
                // SendInput 失败被放大为错误轨迹。
                accum_x = last_int_x as f64;
                accum_y = last_int_y as f64;
                consecutive_failures += 1;
                if consecutive_failures >= 50 {
                    eprintln!("SendInput 连续失败 50 次，中止绘制（可能是权限/UIPI 问题）");
                    active.store(false, Ordering::SeqCst);
                    return ((last_int_x, last_int_y), MoveResult::InputInjectionFailed);
                }
            }

            // 仅在实际移动时休眠；零移动步
            // （浮点累加器未跨整数边界时）可跳过
            if delay {
                if !interruptible_sleep(active, Duration::from_secs_f64(draw_delay_sec.max(0.0))) {
                    return ((last_int_x, last_int_y), MoveResult::Cancelled);
                }
            } else if lift_pen_speed < 100.0 {
                // 抬笔移动速度：100% = 瞬移；每低 1% 增加 0.5ms/步 延时
                let step_delay_s = (100.0 - lift_pen_speed) * 0.0005;
                if !interruptible_sleep(active, Duration::from_secs_f64(step_delay_s.max(0.0))) {
                    return ((last_int_x, last_int_y), MoveResult::Cancelled);
                }
            }
        }
    }

    // 极长的抬笔移动可能触及步数上限；不能静默停在错误位置。
    if last_int_x != target.0 || last_int_y != target.1 {
        if delay {
            eprintln!("落笔移动达到安全步数上限，未到达目标位置，中止绘制");
            active.store(false, Ordering::SeqCst);
            return ((last_int_x, last_int_y), MoveResult::InputInjectionFailed);
        }
        if !active.load(Ordering::SeqCst) {
            return ((last_int_x, last_int_y), MoveResult::Cancelled);
        }
        if !guard.still_foreground() {
            return ((last_int_x, last_int_y), MoveResult::TargetLost);
        }
        let residual_dx = target.0 - last_int_x;
        let residual_dy = target.1 - last_int_y;
        let mut injected = false;
        for _ in 0..3 {
            if move_cursor_relative(residual_dx, residual_dy) {
                injected = true;
                break;
            }
            thread::yield_now();
        }
        if !injected {
            eprintln!("抬笔移动的最终补偿注入失败，中止绘制");
            active.store(false, Ordering::SeqCst);
            return ((last_int_x, last_int_y), MoveResult::InputInjectionFailed);
        }
        last_int_x = target.0;
        last_int_y = target.1;
    }

    ((last_int_x, last_int_y), MoveResult::Completed)
}

fn curvature_step(stroke: &[(i32, i32)], point_index: usize, maximum_step: i32) -> i32 {
    let maximum_step = maximum_step.clamp(1, 6);
    let minimum_step = maximum_step.min(2);
    if point_index == 0 || point_index + 1 >= stroke.len() {
        return maximum_step;
    }

    let previous = stroke[point_index - 1];
    let current = stroke[point_index];
    let next = stroke[point_index + 1];
    let incoming = (
        f64::from(current.0) - f64::from(previous.0),
        f64::from(current.1) - f64::from(previous.1),
    );
    let outgoing = (
        f64::from(next.0) - f64::from(current.0),
        f64::from(next.1) - f64::from(current.1),
    );
    let denominator = incoming.0.hypot(incoming.1) * outgoing.0.hypot(outgoing.1);
    if denominator <= 1.0e-9 {
        return minimum_step;
    }
    let direction_cosine = (incoming.0 * outgoing.0 + incoming.1 * outgoing.1) / denominator;
    if direction_cosine < 0.55 {
        minimum_step
    } else if direction_cosine < 0.9 {
        (maximum_step - 1).max(minimum_step)
    } else {
        maximum_step
    }
}

fn adaptive_pen_down_step(stroke: &[(i32, i32)], target_index: usize, maximum_step: i32) -> i32 {
    curvature_step(stroke, target_index.saturating_sub(1), maximum_step).min(curvature_step(
        stroke,
        target_index,
        maximum_step,
    ))
}

fn button_guard_remaining(elapsed: Duration) -> Duration {
    MIN_BUTTON_DOWN_INTERVAL.saturating_sub(elapsed)
}

pub struct Drawer {
    pub active: Arc<AtomicBool>,
    thread_done: Arc<AtomicBool>,
    generation: AtomicU64,
    mouse_lock: Arc<Mutex<()>>,
    /// 左键是否处于按下状态：press_left 成功置位、每次释放清除。
    /// 释放路径先 swap 再注入，保证抬笔状态下绝不向当前前台应用注入多余 LEFTUP，
    /// 且同一时刻多个兜底释放（stop/DoneGuard/收尾）只有第一个真正注入。
    pen_down: Arc<AtomicBool>,
    /// 最近一次绘制进度断点（F10 暂停后保留，F8 续画；完成/失效后清空）
    pub progress: Arc<Mutex<Option<DrawCheckpoint>>>,
    /// 最近一次输入环境探测结果（供前端展示与历史记录）
    pub last_probe: Arc<Mutex<Option<probe::ProbeOutcome>>>,
    /// 失焦诊断：失败阶段 + 当时的前台窗口信息（供 toast 精确提示）
    pub last_target_lost: Arc<Mutex<Option<String>>>,
    /// 聚焦失败诊断（未找到窗口 / 无法获得前台焦点）
    pub last_focus_failure: Arc<Mutex<Option<String>>>,
    /// 边界拦截诊断：绘制范围 vs 桌面尺寸
    pub last_outside_desktop: Arc<Mutex<Option<String>>>,
}

impl Drawer {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            thread_done: Arc::new(AtomicBool::new(true)),
            generation: AtomicU64::new(0),
            mouse_lock: Arc::new(Mutex::new(())),
            pen_down: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(None)),
            last_probe: Arc::new(Mutex::new(None)),
            last_target_lost: Arc::new(Mutex::new(None)),
            last_focus_failure: Arc::new(Mutex::new(None)),
            last_outside_desktop: Arc::new(Mutex::new(None)),
        }
    }

    /// 启动绘制线程。`resume` 为 Some 时从断点继续（跳过已完成笔画、沿用原偏移）。
    /// 成功启动返回线程句柄；active 已被占用时返回 None。
    pub fn start_drawing(
        &self,
        strokes: Vec<DrawingStroke>,
        cfg: DrawingConfig,
        resume: Option<DrawCheckpoint>,
        revision: u64,
        request_fingerprint: u64,
    ) -> Option<(u64, thread::JoinHandle<DrawResult>)> {
        if strokes.is_empty() {
            return None;
        }

        // stop→start 竞态防护：等待旧绘制线程完全退出（等待可中断，最多等待 500ms）
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while !self.thread_done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !self.thread_done.load(Ordering::SeqCst) {
            return None; // 理论不可达：旧线程最坏路径远小于 500ms
        }

        // TOCTOU 防护：检查+置位一次完成，避免 F9 热键与 start_drawing 命令并发各起一个绘制线程
        if self
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        self.thread_done.store(false, Ordering::SeqCst);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let flag = self.active.clone();
        let done = self.thread_done.clone();
        let mouse_lock = self.mouse_lock.clone();
        let pen_down = self.pen_down.clone();
        let progress = self.progress.clone();
        let last_probe = self.last_probe.clone();
        let lost_diag = self.last_target_lost.clone();
        let focus_diag = self.last_focus_failure.clone();
        let outside_diag = self.last_outside_desktop.clone();

        // DropGuard：保证线程任何退出路径（自然结束 / panic / 栈展开）都复位 active 与 thread_done，
        // 避免 catch_unwind 分支内再次 panic 导致 thread_done 永驻 false（绘制永远无法重启）
        struct DoneGuard {
            flag: Arc<AtomicBool>,
            done: Arc<AtomicBool>,
            mouse_lock: Arc<Mutex<()>>,
            pen_down: Arc<AtomicBool>,
        }
        impl Drop for DoneGuard {
            fn drop(&mut self) {
                self.flag.store(false, Ordering::SeqCst);
                // 仅当左键确实按下时才补发抬起：抬笔状态下的兜底释放会向
                // 当前前台应用注入多余的 LEFTUP（打断用户拖拽/框选）
                if self.pen_down.swap(false, Ordering::SeqCst) {
                    let _mouse = self.mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
                    release_left_checked("绘制线程退出");
                }
                self.done.store(true, Ordering::SeqCst);
            }
        }

        Some((
            generation,
            thread::spawn(move || {
                let guard = DoneGuard {
                    flag: flag.clone(),
                    done: done.clone(),
                    mouse_lock: mouse_lock.clone(),
                    pen_down: pen_down.clone(),
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Self::draw_strokes_thread(
                        strokes,
                        cfg,
                        flag.clone(),
                        mouse_lock.clone(),
                        &pen_down,
                        progress,
                        last_probe,
                        lost_diag,
                        focus_diag,
                        outside_diag,
                        resume,
                        revision,
                        request_fingerprint,
                    )
                }));
                let result = result.unwrap_or_else(|_| {
                    eprintln!("绘制线程异常终止");
                    DrawResult::Panicked
                });
                drop(guard);
                result
            }),
        ))
    }

    /// 边界预演：只移动鼠标到画作四角（绝不按下左键），供用户确认位置与范围。
    /// 不触碰绘制断点（暂停后预演不应丢失续画进度）。
    pub fn rehearse(
        &self,
        strokes: Vec<DrawingStroke>,
        cfg: DrawingConfig,
    ) -> Option<(u64, thread::JoinHandle<DrawResult>)> {
        if strokes.is_empty() {
            return None;
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while !self.thread_done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !self.thread_done.load(Ordering::SeqCst) {
            return None;
        }
        if self
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        self.thread_done.store(false, Ordering::SeqCst);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let flag = self.active.clone();
        let done = self.thread_done.clone();
        let mouse_lock = self.mouse_lock.clone();
        let pen_down = self.pen_down.clone();
        let last_probe = self.last_probe.clone();
        let lost_diag = self.last_target_lost.clone();
        let focus_diag = self.last_focus_failure.clone();
        let outside_diag = self.last_outside_desktop.clone();

        struct DoneGuard {
            flag: Arc<AtomicBool>,
            done: Arc<AtomicBool>,
            mouse_lock: Arc<Mutex<()>>,
            pen_down: Arc<AtomicBool>,
        }
        impl Drop for DoneGuard {
            fn drop(&mut self) {
                self.flag.store(false, Ordering::SeqCst);
                // 预演全程未按下左键：仅当标志意外为真时才补发抬起
                if self.pen_down.swap(false, Ordering::SeqCst) {
                    let _mouse = self.mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
                    release_left_checked("预演线程退出");
                }
                self.done.store(true, Ordering::SeqCst);
            }
        }

        Some((
            generation,
            thread::spawn(move || {
                let guard = DoneGuard {
                    flag: flag.clone(),
                    done: done.clone(),
                    mouse_lock: mouse_lock.clone(),
                    pen_down: pen_down.clone(),
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Self::rehearse_thread(
                        strokes,
                        cfg,
                        flag.clone(),
                        mouse_lock.clone(),
                        &pen_down,
                        last_probe,
                        lost_diag,
                        focus_diag,
                        outside_diag,
                    )
                }));
                let result = result.unwrap_or_else(|_| {
                    eprintln!("预演线程异常终止");
                    DrawResult::Panicked
                });
                drop(guard);
                result
            }),
        ))
    }

    pub fn stop_drawing(&self) {
        // 空闲时直接返回：未在绘制时调用（如误按 F10）不应向当前前台应用
        // 注入多余的左键抬起事件——那会打断用户在其他应用里正按住左键的操作。
        if !self.is_busy() {
            return;
        }
        let _mouse = self.mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
        self.active.store(false, Ordering::SeqCst);
        // 同样只在左键确实按下时才补发抬起（抬笔间隙按 F10 不注入多余事件）；
        // 释放失败时交回绘制线程 DoneGuard 兜底重试
        if self.pen_down.swap(false, Ordering::SeqCst) && !release_left_checked("停止绘制") {
            self.pen_down.store(true, Ordering::SeqCst);
        }
    }

    /// 绘制或预演尚未完全退出时仍视为忙碌，禁止工作区修改。
    pub fn is_busy(&self) -> bool {
        self.active.load(Ordering::SeqCst) || !self.thread_done.load(Ordering::SeqCst)
    }

    /// 绘制线程是否已完全退出（用于关闭窗口前等待，避免 press/release 窗口被硬杀）
    pub fn is_idle(&self) -> bool {
        self.thread_done.load(Ordering::SeqCst)
    }

    /// 返回明确的绘制结束原因，供 UI 显示针对性的提示。
    #[allow(clippy::too_many_arguments)]
    fn draw_strokes_thread(
        strokes: Vec<DrawingStroke>,
        cfg: DrawingConfig,
        active: Arc<AtomicBool>,
        mouse_lock: Arc<Mutex<()>>,
        pen_down: &AtomicBool,
        progress: Arc<Mutex<Option<DrawCheckpoint>>>,
        last_probe: Arc<Mutex<Option<probe::ProbeOutcome>>>,
        lost_diag: Arc<Mutex<Option<String>>>,
        focus_diag: Arc<Mutex<Option<String>>>,
        outside_diag: Arc<Mutex<Option<String>>>,
        resume: Option<DrawCheckpoint>,
        revision: u64,
        request_fingerprint: u64,
    ) -> DrawResult {
        if strokes.is_empty() {
            return DrawResult::InvalidStrokes;
        }

        // 失焦诊断：记录失败阶段与当时的前台窗口，供前端精确提示
        let mark_lost = |stage: &str| -> DrawResult {
            *lost_diag.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(format!("{stage}：当前前台为 {}", foreground_window_desc()));
            DrawResult::TargetLost
        };
        // 清空上次的画作尺寸诊断（本次若未超界，结束时不重复提示）
        *outside_diag.lock().unwrap_or_else(|p| p.into_inner()) = None;

        // 聚焦 VRChat 窗口（轮询最多 1s 等待前台切换，放后台线程避免卡 UI）
        let Some((target_hwnd, target_pid)) = focus_vrchat_window(&active, &focus_diag) else {
            eprintln!("警告：无法聚焦 VRChat 窗口。");
            let cancelled = !active.load(Ordering::SeqCst);
            active.store(false, Ordering::SeqCst);
            return if cancelled {
                DrawResult::Cancelled
            } else {
                DrawResult::VrchatNotFound
            };
        };
        let guard = TargetGuard {
            hwnd: target_hwnd,
            pid: target_pid,
        };

        // 输入环境探测（抬笔状态小幅移动 + 采样分类；仅诊断，不改变绘制模式）
        let probe_outcome = probe::probe_input_mode(&active);
        *last_probe.lock().unwrap_or_else(|p| p.into_inner()) = Some(probe_outcome);
        if !active.load(Ordering::SeqCst) {
            return DrawResult::Cancelled;
        }

        let Some((min_x, min_y, max_x, max_y)) = stroke_bounds(&strokes) else {
            active.store(false, Ordering::SeqCst);
            return DrawResult::InvalidStrokes;
        };
        let center_x = (min_x + max_x) / 2.0;
        let center_y = (min_y + max_y) / 2.0;

        let scale = cfg.sensitivity;
        let stretch = cfg.vertical_stretch;

        // 先等待开始延迟，再决定锚点：给用户时间把鼠标移到画板起点。
        // 延迟期间不做前台判定（用户可能在手动切换窗口/调整位置），
        // 延迟结束后统一验证一次（带短宽限，避免切换动画瞬间误判）。
        let start_deadline = Instant::now() + Duration::from_secs_f64(cfg.start_delay.max(0.0));
        while active.load(Ordering::SeqCst) && Instant::now() < start_deadline {
            if !interruptible_sleep(&active, Duration::from_millis(25)) {
                break;
            }
        }
        if !active.load(Ordering::SeqCst) {
            return DrawResult::Cancelled;
        }
        // 前台验证（带拉回重试）：误触系统 UI（搜索/开始菜单）后用户点回游戏可自动恢复
        match foreground_with_retry(&guard, &active) {
            Ok(()) => {}
            Err(ForegroundFailure::Cancelled) => return DrawResult::Cancelled,
            Err(ForegroundFailure::Lost) => return mark_lost("开始延迟结束后"),
        }

        let resuming = resume.is_some();
        // 断点续画：沿用原坐标偏移，从最后落笔位置继续（不读取当前光标——
        // VRChat 捕获鼠标时光标被锁定，其位置不代表笔尖位置）
        let (offset_x, offset_y, mut current_x, mut current_y, mut skip_strokes, mut partial_point) =
            if let Some(cp) = resume.as_ref() {
                (
                    cp.offset.0,
                    cp.offset.1,
                    cp.last_screen.0.round() as i32,
                    cp.last_screen.1.round() as i32,
                    cp.stroke_index,
                    cp.point_index,
                )
            } else {
                let Some((start_x, start_y)) = get_cursor() else {
                    eprintln!("无法获取光标位置，已取消绘制。");
                    active.store(false, Ordering::SeqCst);
                    return DrawResult::CursorUnavailable;
                };
                let offset_x = start_x as f64 - center_x * scale;
                // 垂直方向隐式应用拉伸：
                let offset_y = start_y as f64 - center_y * scale * stretch;
                (offset_x, offset_y, start_x, start_y, 0, 0)
            };

        let mut lost_foreground = false;
        let mut lost_stage = "绘制中";
        let mut scaled_strokes = Vec::new();
        for stroke in strokes {
            if !active.load(Ordering::SeqCst) {
                break;
            }
            if !guard.still_foreground() {
                lost_foreground = true;
                lost_stage = "笔画缩放阶段";
                break;
            }
            if stroke.points.is_empty() {
                continue;
            }

            let mut screen_points = Vec::new();
            for p in &stroke.points {
                if !p.x.is_finite() || !p.y.is_finite() {
                    continue;
                }
                let screen_x = (p.x * scale + offset_x).round();
                let screen_y = (p.y * scale * stretch + offset_y).round();
                if !screen_x.is_finite() || !screen_y.is_finite() {
                    continue;
                }
                let screen_x = screen_x.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                let screen_y = screen_y.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                screen_points.push((screen_x, screen_y));
            }
            // 步长细分由 move_relatively 按 max_step_px 完成，此处无需预插值
            scaled_strokes.push(screen_points);
        }
        if !active.load(Ordering::SeqCst) {
            return DrawResult::Cancelled;
        }
        if lost_foreground {
            return mark_lost(lost_stage);
        }

        // 画作尺寸检查（仅诊断、不拦截）：VRChat 桌面绘制是相对模式，
        // 画作落点在游戏画板（可大于屏幕），屏幕大小不限制绘制；
        // 仅在画作远超屏幕时提示"画板可能不够大"。
        {
            let desktop = probe::virtual_desktop();
            let mut min_x = f64::INFINITY;
            let mut min_y = f64::INFINITY;
            let mut max_x = f64::NEG_INFINITY;
            let mut max_y = f64::NEG_INFINITY;
            for &(x, y) in scaled_strokes.iter().flatten() {
                min_x = min_x.min(f64::from(x));
                min_y = min_y.min(f64::from(y));
                max_x = max_x.max(f64::from(x));
                max_y = max_y.max(f64::from(y));
            }
            if !probe::rect_within_desktop((min_x, min_y), (max_x, max_y), desktop) {
                let (dw, dh) = (f64::from(desktop.2), f64::from(desktop.3));
                *outside_diag.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                    "画作尺寸约 {}×{}，超出桌面 {}×{}；若 VRChat 画板较小可能画不下，已继续绘制",
                    (max_x - min_x).round() as i64,
                    (max_y - min_y).round() as i64,
                    dw.round() as i64,
                    dh.round() as i64,
                ));
                eprintln!("提示：画作尺寸超出屏幕范围（不拦截，继续绘制）。");
            }
        }

        let optimized = optimize_screen_strokes(scaled_strokes);
        let ordered = order_strokes_by_proximity(optimized);

        let total = ordered.len();
        if let Some(cp) = resume.as_ref() {
            let checkpoint_valid = cp.revision == revision
                && cp.request_fingerprint == request_fingerprint
                && cp.total == total
                && cp.stroke_index < total
                && cp.point_index < ordered[cp.stroke_index].len()
                && cp.last_screen.0.is_finite()
                && cp.last_screen.1.is_finite()
                && cp.offset.0.is_finite()
                && cp.offset.1.is_finite();
            if !checkpoint_valid {
                eprintln!("拒绝使用与当前绘制路径不匹配的断点");
                return DrawResult::StaleCheckpoint;
            }
        }
        // 防御：断点数据异常（全部笔画已完成）时直接结束，不应 panic；
        // 完成后清除断点（与自然完成语义一致，避免 Completion 后仍提示可续画）。
        if skip_strokes >= total {
            *progress.lock().unwrap_or_else(|p| p.into_inner()) = None;
            return DrawResult::Completed;
        }
        // 断点位于某笔"最后一个点"：F10 暂停发生在该笔收尾的同步/抬笔延迟/稳定
        // 停顿期，该笔实际已画完。若按 partial 语义续画，F8 会在笔末做一次无移动的
        // 按下+释放，在画板上落下多余墨点；此处直接把该笔计为已完成。
        if partial_point > 0 && partial_point >= ordered[skip_strokes].len() - 1 {
            skip_strokes += 1;
            partial_point = 0;
            if skip_strokes >= total {
                *progress.lock().unwrap_or_else(|p| p.into_inner()) = None;
                return DrawResult::Completed;
            }
        }

        // F9 从头开始时，只有完成聚焦、探测、映射和路径整理后才放弃旧断点；
        // 预检失败仍保留它，避免用户失去可恢复性。
        if !resuming {
            *progress.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }

        // 即使旧配置尚未保存迁移结果，执行层也保证至少约一帧的移动间隔。
        let draw_delay = cfg.draw_speed.max(MIN_PEN_MOVE_INTERVAL_SECS);
        let lift_delay = cfg.lift_pen_delay;
        let mut previous_button_down_at: Option<Instant> = None;

        // 断点落盘：暂停后 F9 可从该位置继续
        let write_progress = |stroke_index: usize, point_index: usize, last_screen: (f64, f64)| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            *progress.lock().unwrap_or_else(|p| p.into_inner()) = Some(DrawCheckpoint {
                stroke_index,
                point_index,
                last_screen,
                offset: (offset_x, offset_y),
                total,
                revision,
                request_fingerprint,
                updated_at: now,
            });
        };

        for (stroke_index, stroke) in ordered.iter().enumerate().skip(skip_strokes) {
            // active 被打回 false 时中止；空笔画仅跳过当前（防御性，上游已保证无空 stroke）
            if !active.load(Ordering::SeqCst) {
                break;
            }
            // 笔画间前台检查（带拉回重试）：仅在失败时尝试恢复，正常绘制零开销
            match foreground_with_retry(&guard, &active) {
                Ok(()) => {}
                Err(ForegroundFailure::Cancelled) => break,
                Err(ForegroundFailure::Lost) => {
                    lost_foreground = true;
                    lost_stage = "笔画开始前";
                    break;
                }
            }
            if stroke.is_empty() {
                continue;
            }
            // 断点续画且当前笔尚未画完：笔尖已在 partial_point 处，无需抬笔移动
            let is_partial = stroke_index == skip_strokes && partial_point > 0;

            if !is_partial {
                // 无延时移动到起点（快速抬笔）
                let ((nx, ny), move_result) = move_relatively(
                    current_x,
                    current_y,
                    stroke[0],
                    cfg.max_step_px,
                    &active,
                    false,
                    draw_delay,
                    cfg.lift_pen_speed,
                    &guard,
                );
                current_x = nx;
                current_y = ny;

                if move_result != MoveResult::Completed {
                    // 抬笔移动被中断：断点必须落到"最后已注入的真实位置"，否则停留在
                    // 上一笔末，续画从旧基准注入整段增量 → 后续笔画整体平移（相对模式
                    // 没有绝对坐标可补偿）。记为本笔尚未开始（partial=0），F8 从该笔
                    // 起重画，起点即中断位置，笔尖基准与游戏内一致。
                    write_progress(stroke_index, 0, (nx as f64, ny as f64));
                    return match move_result {
                        MoveResult::Cancelled => DrawResult::Cancelled,
                        MoveResult::InputInjectionFailed => DrawResult::InputInjectionFailed,
                        MoveResult::TargetLost => mark_lost("抬笔移动中"),
                        MoveResult::Completed => unreachable!(),
                    };
                }

                // 抬笔移动成功完成：光标已在本笔起点，立即刷新断点。否则在"到达同步 /
                // 按下前等待"窗口内 F10/失焦时断点仍停留在上一笔末坐标，而光标实际已在
                // 本笔起点，F8 续画会重复注入本段抬笔增量 → 余下笔画整体平移。
                // 写 (stroke_index, 0)：续画时 partial=0，抬笔移动为同点零位移空操作。
                write_progress(stroke_index, 0, (current_x as f64, current_y as f64));

                if !active.load(Ordering::SeqCst) {
                    break;
                }
                if !guard.still_foreground() {
                    lost_foreground = true;
                    lost_stage = "抬笔移动后";
                    break;
                }

                // 到达同步：等待落点稳定后再按下，防止大距离跳转时"过早按下"
                // （旧版用 0 像素相对移动"刷新输入队列"，驱动会对零位移事件去重，改为纯延迟）
                if !interruptible_sleep(&active, Duration::from_millis(60)) {
                    break;
                }
            }

            // 动态定时：笔画点数过少时 VRChat 会因轮询率丢弃它。
            // 60ms 最短绘制时间确保游戏引擎处理到左键按下事件。
            let stroke_pts = stroke.len().max(1) as f64;
            let default_duration = stroke_pts * draw_delay;
            let target_min_duration = 0.060; // VRChat 的 60ms 经验下限

            let dynamic_delay = if default_duration < target_min_duration {
                target_min_duration / stroke_pts
            } else {
                draw_delay
            };

            if !interruptible_sleep(&active, Duration::from_secs_f64(draw_delay.max(0.0))) {
                break;
            }

            // VRChat 会把时间过近的两次短落笔识别成双击并召唤橡皮。
            // 计划等待之外再次按真实单调时间校验，避免线程调度或未来暂停/恢复绕过保护。
            if let Some(previous) = previous_button_down_at {
                let remaining = button_guard_remaining(previous.elapsed());
                if !remaining.is_zero() && !interruptible_sleep(&active, remaining) {
                    break;
                }
            }
            // 按下前最终关口（锁外拉回，不阻塞 F10 获取 mouse_lock）：
            // 系统 UI 抢焦点时在这里再给一次恢复机会
            match foreground_with_retry(&guard, &active) {
                Ok(()) => {}
                Err(ForegroundFailure::Cancelled) => break,
                Err(ForegroundFailure::Lost) => {
                    lost_foreground = true;
                    lost_stage = "按下左键前";
                    break;
                }
            }
            let pressed = {
                // F10 获取同一把锁后才会释放鼠标；若停止请求先到达，这里不会再发出按下事件。
                let _mouse = mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
                if !active.load(Ordering::SeqCst) {
                    false
                } else if !guard.still_foreground() {
                    lost_foreground = true;
                    lost_stage = "按下左键前";
                    false
                } else if press_left() {
                    pen_down.store(true, Ordering::SeqCst);
                    true
                } else {
                    false
                }
            };
            if !pressed {
                let was_active = active.load(Ordering::SeqCst);
                active.store(false, Ordering::SeqCst);
                return if lost_foreground {
                    mark_lost(lost_stage)
                } else if was_active {
                    DrawResult::InputInjectionFailed
                } else {
                    DrawResult::Cancelled
                };
            }
            previous_button_down_at = Some(Instant::now());
            if !interruptible_sleep(&active, Duration::from_secs_f64(dynamic_delay.max(0.0))) {
                let _mouse = mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
                if pen_down.swap(false, Ordering::SeqCst) && !release_left_checked("取消落笔等待")
                {
                    // 释放失败：交回 DoneGuard 兜底重试
                    pen_down.store(true, Ordering::SeqCst);
                    active.store(false, Ordering::SeqCst);
                    return DrawResult::InputInjectionFailed;
                }
                break;
            }

            // 落笔移动：普通笔画从点 1 开始（点 0 为按下位置）；
            // 断点续画从中断点 partial_point 之后继续（笔尖已在 partial_point 处按下）
            let start_at = if is_partial { partial_point + 1 } else { 1 };
            for (point_index, point) in stroke.iter().enumerate().skip(start_at) {
                if !active.load(Ordering::SeqCst) {
                    break;
                }
                if !guard.still_foreground() {
                    lost_foreground = true;
                    lost_stage = "落笔绘制中";
                    break;
                }
                let maximum_step = adaptive_pen_down_step(stroke, point_index, cfg.max_step_px);
                let ((nx, ny), move_result) = move_relatively(
                    current_x,
                    current_y,
                    *point,
                    maximum_step,
                    &active,
                    true,
                    dynamic_delay,
                    100.0,
                    &guard,
                );
                current_x = nx;
                current_y = ny;
                if move_result != MoveResult::Completed {
                    // 落笔移动被中断：断点先从"上一整点"修正为"最后已注入的真实位置"
                    // （move_relatively 的返回值即游戏内真实笔尖），否则 F8 从旧基准注入
                    // 整段增量 → 余下笔画整体平移。记 point_index-1（该目标点未完成），
                    // F8 从 point_index 继续，无漏画无重画。
                    write_progress(
                        stroke_index,
                        point_index.saturating_sub(1),
                        (nx as f64, ny as f64),
                    );
                    return match move_result {
                        MoveResult::Cancelled => DrawResult::Cancelled,
                        MoveResult::InputInjectionFailed => DrawResult::InputInjectionFailed,
                        MoveResult::TargetLost => mark_lost("落笔移动中"),
                        MoveResult::Completed => unreachable!(),
                    };
                }
                // 逐点记录断点：F10 在笔画中途暂停后可从该点精确续画
                write_progress(stroke_index, point_index, (nx as f64, ny as f64));
            }

            let _mouse = mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
            if pen_down.swap(false, Ordering::SeqCst) && !release_left_checked("笔画结束") {
                // 释放失败：交回 DoneGuard 兜底重试
                pen_down.store(true, Ordering::SeqCst);
                active.store(false, Ordering::SeqCst);
                return DrawResult::InputInjectionFailed;
            }

            // 硬同步：等待游戏处理完松开事件再移动
            if !interruptible_sleep(&active, Duration::from_millis(30)) {
                break;
            }

            // 等待设定的抬笔延迟（合计至少 40ms）
            let actual_lift_delay = lift_delay.max(0.040);
            if !interruptible_sleep(&active, Duration::from_secs_f64(actual_lift_delay.max(0.0))) {
                break;
            }

            // 跳转到下一笔前的稳定停顿，防止拖拽
            if !interruptible_sleep(&active, Duration::from_millis(20)) {
                break;
            }
            if !active.load(Ordering::SeqCst) || lost_foreground {
                break;
            }
            // 整笔已完成且未被中断：更新断点为"下一笔尚未开始"。
            // 末笔不写 (total, 0)——它不是合法续画目标，若 F10 恰落在收尾与完成判定
            // 之间的窗口，会把"已全部画完"滞留为 Cancelled 断点（F8 预检拒绝且文案
            // 误导）。不写则末笔中断断点停留在笔末 (total-1, len-1)，F8 走既有
            // "笔末视为完成"路径正确返回 Completed 并清除断点。
            if stroke_index + 1 < total {
                write_progress(stroke_index + 1, 0, (current_x as f64, current_y as f64));
            }
        }

        // 记录是否自然完成（外部 stop 会先把 active 置 false）
        let finished_naturally = active.load(Ordering::SeqCst) && !lost_foreground;
        active.store(false, Ordering::SeqCst);
        let _mouse = mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
        // 仅当左键仍按下时才补发抬起（抬笔间隙被取消/失焦时不注入多余 LEFTUP）
        let release_ok = if pen_down.swap(false, Ordering::SeqCst) {
            release_left_checked("绘制结束")
        } else {
            true
        };
        if !release_ok {
            // 释放失败：交回 DoneGuard 兜底重试
            pen_down.store(true, Ordering::SeqCst);
            DrawResult::InputInjectionFailed
        } else if lost_foreground {
            mark_lost(lost_stage)
        } else if finished_naturally {
            // 自然完成：清除断点（已完成绘制，不再需要续画）
            *progress.lock().unwrap_or_else(|p| p.into_inner()) = None;
            DrawResult::Completed
        } else {
            DrawResult::Cancelled
        }
    }

    /// 边界预演线程：沿画作四角移动光标（抬笔状态，绝不按下左键），
    /// 让用户确认绘制位置、比例与范围。
    #[allow(clippy::too_many_arguments)]
    fn rehearse_thread(
        strokes: Vec<DrawingStroke>,
        cfg: DrawingConfig,
        active: Arc<AtomicBool>,
        mouse_lock: Arc<Mutex<()>>,
        pen_down: &AtomicBool,
        last_probe: Arc<Mutex<Option<probe::ProbeOutcome>>>,
        lost_diag: Arc<Mutex<Option<String>>>,
        focus_diag: Arc<Mutex<Option<String>>>,
        outside_diag: Arc<Mutex<Option<String>>>,
    ) -> DrawResult {
        if strokes.is_empty() {
            return DrawResult::InvalidStrokes;
        }

        let mark_lost = |stage: &str| -> DrawResult {
            *lost_diag.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(format!("{stage}：当前前台为 {}", foreground_window_desc()));
            DrawResult::TargetLost
        };
        // 清空上次的画作尺寸诊断（本次若未超界，结束时不重复提示）
        *outside_diag.lock().unwrap_or_else(|p| p.into_inner()) = None;

        let Some((target_hwnd, target_pid)) = focus_vrchat_window(&active, &focus_diag) else {
            eprintln!("警告：无法聚焦 VRChat 窗口。");
            let cancelled = !active.load(Ordering::SeqCst);
            active.store(false, Ordering::SeqCst);
            return if cancelled {
                DrawResult::Cancelled
            } else {
                DrawResult::VrchatNotFound
            };
        };
        let guard = TargetGuard {
            hwnd: target_hwnd,
            pid: target_pid,
        };

        let probe_outcome = probe::probe_input_mode(&active);
        *last_probe.lock().unwrap_or_else(|p| p.into_inner()) = Some(probe_outcome);
        if !active.load(Ordering::SeqCst) {
            return DrawResult::Cancelled;
        }

        // 开始延迟：给用户时间把鼠标移到期望的画作中心
        // （延迟期间不做前台判定，结束后统一验证一次并带短宽限）
        let start_deadline = Instant::now() + Duration::from_secs_f64(cfg.start_delay.max(0.0));
        while active.load(Ordering::SeqCst) && Instant::now() < start_deadline {
            if !interruptible_sleep(&active, Duration::from_millis(25)) {
                break;
            }
        }
        if !active.load(Ordering::SeqCst) {
            return DrawResult::Cancelled;
        }
        match foreground_with_retry(&guard, &active) {
            Ok(()) => {}
            Err(ForegroundFailure::Cancelled) => return DrawResult::Cancelled,
            Err(ForegroundFailure::Lost) => return mark_lost("开始延迟结束后"),
        }

        let Some((start_x, start_y)) = get_cursor() else {
            eprintln!("无法获取光标位置，已取消预演。");
            active.store(false, Ordering::SeqCst);
            return DrawResult::CursorUnavailable;
        };
        let Some((min_x, min_y, max_x, max_y)) = stroke_bounds(&strokes) else {
            active.store(false, Ordering::SeqCst);
            return DrawResult::InvalidStrokes;
        };

        let scale = cfg.sensitivity;
        let stretch = cfg.vertical_stretch;
        let offset_x = start_x as f64 - ((min_x + max_x) / 2.0) * scale;
        let offset_y = start_y as f64 - ((min_y + max_y) / 2.0) * scale * stretch;

        // 四角屏幕坐标（与正式绘制完全相同的映射）
        let corners = [
            (min_x * scale + offset_x, min_y * scale * stretch + offset_y),
            (max_x * scale + offset_x, min_y * scale * stretch + offset_y),
            (max_x * scale + offset_x, max_y * scale * stretch + offset_y),
            (min_x * scale + offset_x, max_y * scale * stretch + offset_y),
        ];

        // 画作尺寸检查（仅诊断、不拦截，与正式绘制一致）
        {
            let desktop = probe::virtual_desktop();
            let min_x = corners
                .iter()
                .map(|&(x, _)| x)
                .fold(f64::INFINITY, f64::min);
            let min_y = corners
                .iter()
                .map(|&(_, y)| y)
                .fold(f64::INFINITY, f64::min);
            let max_x = corners
                .iter()
                .map(|&(x, _)| x)
                .fold(f64::NEG_INFINITY, f64::max);
            let max_y = corners
                .iter()
                .map(|&(_, y)| y)
                .fold(f64::NEG_INFINITY, f64::max);
            if !probe::rect_within_desktop((min_x, min_y), (max_x, max_y), desktop) {
                let (dw, dh) = (f64::from(desktop.2), f64::from(desktop.3));
                *outside_diag.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                    "画作尺寸约 {}×{}，超出桌面 {}×{}；若 VRChat 画板较小可能画不下，已继续预演",
                    (max_x - min_x).round() as i64,
                    (max_y - min_y).round() as i64,
                    dw.round() as i64,
                    dh.round() as i64,
                ));
                eprintln!("提示：预演画作尺寸超出屏幕范围（不拦截，继续预演）。");
            }
        }

        let mut current_x = start_x;
        let mut current_y = start_y;
        for corner in corners {
            if !active.load(Ordering::SeqCst) {
                return DrawResult::Cancelled;
            }
            match foreground_with_retry(&guard, &active) {
                Ok(()) => {}
                Err(ForegroundFailure::Cancelled) => return DrawResult::Cancelled,
                Err(ForegroundFailure::Lost) => return mark_lost("预演移动中"),
            }
            let ((nx, ny), move_result) = move_relatively(
                current_x,
                current_y,
                (corner.0.round() as i32, corner.1.round() as i32),
                cfg.max_step_px,
                &active,
                false,
                0.0,
                cfg.lift_pen_speed,
                &guard,
            );
            current_x = nx;
            current_y = ny;
            if move_result != MoveResult::Completed {
                return match move_result {
                    MoveResult::Cancelled => DrawResult::Cancelled,
                    MoveResult::InputInjectionFailed => DrawResult::InputInjectionFailed,
                    MoveResult::TargetLost => mark_lost("预演移动中"),
                    MoveResult::Completed => unreachable!(),
                };
            }
            // 每个角停留片刻，让用户看清边界位置
            if !interruptible_sleep(&active, Duration::from_millis(REHEARSE_CORNER_HOLD_MS)) {
                return DrawResult::Cancelled;
            }
        }

        let lost_foreground = !guard.still_foreground();
        let finished = active.load(Ordering::SeqCst) && !lost_foreground;
        active.store(false, Ordering::SeqCst);
        let _mouse = mouse_lock.lock().unwrap_or_else(|p| p.into_inner());
        // 预演全程未按下左键：仅当标志意外为真时才补发抬起，不再无条件注入
        if pen_down.swap(false, Ordering::SeqCst) {
            let _ = release_left_checked("预演结束");
        }
        if lost_foreground {
            mark_lost("预演结束")
        } else if finished {
            DrawResult::Completed
        } else {
            DrawResult::Cancelled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment_multiset(strokes: &[Vec<(i32, i32)>]) -> Vec<((i32, i32), (i32, i32))> {
        let mut segments = strokes
            .iter()
            .flat_map(|stroke| stroke.windows(2))
            .filter_map(|pair| {
                if pair[0] == pair[1] {
                    None
                } else if pair[0] < pair[1] {
                    Some((pair[0], pair[1]))
                } else {
                    Some((pair[1], pair[0]))
                }
            })
            .collect::<Vec<_>>();
        segments.sort_unstable();
        segments
    }

    #[test]
    fn screen_optimizer_merges_only_exact_endpoints_and_preserves_points() {
        let input = vec![
            vec![(0, 0), (1, 0), (1, 0)],
            vec![(2, 0), (1, 0)],
            vec![(10, 10), (10, 10)],
        ];
        let before = segment_multiset(&input);
        let optimized = optimize_screen_strokes(input);

        assert_eq!(segment_multiset(&optimized), before);
        assert_eq!(optimized.len(), 2);
        assert!(optimized
            .iter()
            .any(|stroke| stroke.as_slice() == [(10, 10)]));
    }

    #[test]
    fn screen_optimizer_does_not_bridge_nearby_endpoints() {
        let input = vec![vec![(0, 0), (1, 0)], vec![(2, 0), (3, 0)]];
        let optimized = optimize_screen_strokes(input.clone());
        assert_eq!(segment_multiset(&optimized), segment_multiset(&input));
        assert_eq!(optimized.len(), 2);
    }

    #[test]
    fn adaptive_step_slows_down_at_sharp_turns() {
        let straight = [(0, 0), (4, 0), (8, 0)];
        let corner = [(0, 0), (4, 0), (4, 4)];
        assert_eq!(adaptive_pen_down_step(&straight, 1, 4), 4);
        assert_eq!(adaptive_pen_down_step(&corner, 1, 4), 2);
    }

    #[test]
    fn button_guard_enforces_six_hundred_milliseconds() {
        assert_eq!(
            button_guard_remaining(Duration::from_millis(100)),
            Duration::from_millis(500)
        );
        assert!(button_guard_remaining(Duration::from_millis(600)).is_zero());
        assert!(button_guard_remaining(Duration::from_millis(900)).is_zero());
    }
}
