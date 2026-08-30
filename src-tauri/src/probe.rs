//! 输入环境诊断与模式探测（移植自 VRC-Draw 的 MouseInput.cpp / DrawingEngine.cpp）。
//! 第一阶段只做诊断、记录与警告，正式绘制仍固定使用已验证的相对模式；
//! 不自动切换绝对模式（VRChat 会拉回光标，绝对模式导致视角抽搐）。

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::drawer::{get_cursor, interruptible_sleep, move_cursor_relative};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Relative,
    DesktopAbsolute,
    Undetermined,
}

impl InputMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputMode::Relative => "Relative",
            InputMode::DesktopAbsolute => "DesktopAbsolute",
            InputMode::Undetermined => "Undetermined",
        }
    }
}

/// 一次探测的结果（供前端展示与历史记录）
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub mode: InputMode,
    pub probe_distance: i32,
    pub samples: usize,
    pub note: String,
}

/// 探测移动的恢复守卫：只要正向注入成功，任何提前返回路径都必须尝试反向注入。
struct ProbeRestoreGuard {
    reverse: (i32, i32),
    armed: bool,
}

impl ProbeRestoreGuard {
    fn new(reverse: (i32, i32)) -> Self {
        Self {
            reverse,
            armed: true,
        }
    }

    fn restore(&mut self) -> bool {
        if !self.armed {
            return true;
        }
        self.armed = false;
        move_cursor_relative(self.reverse.0, self.reverse.1)
    }
}

impl Drop for ProbeRestoreGuard {
    fn drop(&mut self) {
        if self.armed && !self.restore() {
            eprintln!("输入探测结束时无法恢复鼠标/视角位置");
        }
    }
}

/// 根据系统鼠标速度计算安全探测距离（MouseInput.cpp RecommendedProbeDistance）
pub fn recommended_probe_distance(system_speed: i32) -> i32 {
    let safe_speed = system_speed.clamp(1, 20);
    ((96 + safe_speed - 1) / safe_speed).clamp(8, 96)
}

/// 分类光标行为（MouseInput.cpp ClassifyCursorBehavior）：
/// - 光标被拉回原点附近 → Relative（游戏捕获鼠标，消耗相对增量）
/// - 光标稳定停留在注入位置 → DesktopAbsolute（鼠标自由移动）
/// - 行为不稳定/样本不足 → Undetermined
pub fn classify_cursor_behavior(
    origin: (i32, i32),
    injected: (i32, i32),
    samples: &[(i32, i32)],
) -> InputMode {
    const RETURN_TOLERANCE: i32 = 2;
    const STABLE_TOLERANCE: i32 = 2;
    const MINIMUM_MOVEMENT: i32 = 3;
    if samples.len() < 2 {
        return InputMode::Undetermined;
    }
    let last = samples[samples.len() - 1];
    let previous = samples[samples.len() - 2];
    let displacement_x = last.0 - origin.0;
    let displacement_y = last.1 - origin.1;
    if displacement_x.abs().max(displacement_y.abs()) <= RETURN_TOLERANCE {
        return InputMode::Relative;
    }
    if displacement_x.abs().max(displacement_y.abs()) < MINIMUM_MOVEMENT
        || (last.0 - previous.0).abs().max((last.1 - previous.1).abs()) > STABLE_TOLERANCE
    {
        return InputMode::Undetermined;
    }
    let alignment = i64::from(displacement_x) * i64::from(injected.0)
        + i64::from(displacement_y) * i64::from(injected.1);
    if alignment > 0 {
        InputMode::DesktopAbsolute
    } else {
        InputMode::Relative
    }
}

/// 绝对屏幕坐标矩形是否完全位于虚拟桌面内（绘制边界预检用）
pub fn rect_within_desktop(
    minimum: (f64, f64),
    maximum: (f64, f64),
    desktop: (i32, i32, i32, i32),
) -> bool {
    let (left, top, width, height) = desktop;
    if width <= 0 || height <= 0 {
        return false;
    }
    let right = f64::from(left) + f64::from(width) - 1.0;
    let bottom = f64::from(top) + f64::from(height) - 1.0;
    minimum.0 >= f64::from(left)
        && minimum.1 >= f64::from(top)
        && maximum.0 <= right
        && maximum.1 <= bottom
}

/// Windows 系统鼠标速度（SPI_GETMOUSESPEED，1–20，默认 10）
#[cfg(windows)]
pub fn system_pointer_speed() -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETMOUSESPEED};
    let mut speed: i32 = 10;
    unsafe {
        if SystemParametersInfoW(SPI_GETMOUSESPEED, 0, (&mut speed as *mut i32).cast(), 0) == 0 {
            return 10;
        }
    }
    speed
}

#[cfg(not(windows))]
pub fn system_pointer_speed() -> i32 {
    10
}

/// 虚拟桌面边界（left, top, width, height；多显示器会扩展）
#[cfg(windows)]
pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

#[cfg(not(windows))]
pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    (0, 0, 0, 0)
}

/// 执行一次输入环境探测：在抬笔状态下注入小幅相对移动，连续采样光标位置分类。
/// 探测只移动鼠标、绝不按下左键；结束后把光标/视角还原到探测前。
/// 探测失败不中止绘制（诊断优先），结果通过 note 说明。
pub fn probe_input_mode(active: &AtomicBool) -> ProbeOutcome {
    let probe_distance = recommended_probe_distance(system_pointer_speed());
    let Some(origin) = get_cursor() else {
        return ProbeOutcome {
            mode: InputMode::Undetermined,
            probe_distance,
            samples: 0,
            note: "无法读取光标位置".to_string(),
        };
    };
    let desktop = virtual_desktop();
    if desktop.2 <= 0 || desktop.3 <= 0 {
        return ProbeOutcome {
            mode: InputMode::Undetermined,
            probe_distance,
            samples: 0,
            note: "无法读取虚拟桌面边界".to_string(),
        };
    }
    // 选择光标四周空间最大的方向（C++ DetectInputMode 兜底分支）
    let right = desktop.0 + desktop.2 - 1 - origin.0;
    let left = origin.0 - desktop.0;
    let bottom = desktop.1 + desktop.3 - 1 - origin.1;
    let top = origin.1 - desktop.1;
    let (best_space, direction): (i32, (i32, i32)) = {
        let mut best = (right, (1, 0));
        if left > best.0 {
            best = (left, (-1, 0));
        }
        if bottom > best.0 {
            best = (bottom, (0, 1));
        }
        if top > best.0 {
            best = (top, (0, -1));
        }
        best
    };
    if best_space < probe_distance + 4 {
        return ProbeOutcome {
            mode: InputMode::Undetermined,
            probe_distance,
            samples: 0,
            note: "光标附近桌面边界空间不足，跳过探测".to_string(),
        };
    }
    let probe = (direction.0 * probe_distance, direction.1 * probe_distance);
    if !active.load(std::sync::atomic::Ordering::SeqCst) {
        return ProbeOutcome {
            mode: InputMode::Undetermined,
            probe_distance,
            samples: 0,
            note: "探测被取消".to_string(),
        };
    }
    if !move_cursor_relative(probe.0, probe.1) {
        return ProbeOutcome {
            mode: InputMode::Undetermined,
            probe_distance,
            samples: 0,
            note: "探测移动注入失败（可能是权限/UIPI 问题）".to_string(),
        };
    }
    // 还原：绝对模式时光标实际移动了注入距离，反向注入即可恢复；
    // 相对模式（游戏捕获）下反向注入把视角转回原位。守卫覆盖所有提前返回路径。
    let mut restore = ProbeRestoreGuard::new((-probe.0, -probe.1));
    let mut outcome = (|| {
        let mut samples: Vec<(i32, i32)> = Vec::new();
        let mut mode = InputMode::Undetermined;
        for sample_index in 0..6 {
            if !interruptible_sleep(active, Duration::from_millis(16)) {
                return ProbeOutcome {
                    mode: InputMode::Undetermined,
                    probe_distance,
                    samples: samples.len(),
                    note: "探测被取消".to_string(),
                };
            }
            let Some(point) = get_cursor() else {
                return ProbeOutcome {
                    mode: InputMode::Undetermined,
                    probe_distance,
                    samples: samples.len(),
                    note: "采样时无法读取光标位置".to_string(),
                };
            };
            samples.push(point);
            mode = classify_cursor_behavior(origin, probe, &samples);
            if mode == InputMode::Relative && sample_index >= 2 {
                break;
            }
            if mode == InputMode::DesktopAbsolute && sample_index + 1 == 6 {
                break;
            }
        }
        let note = if mode == InputMode::Undetermined {
            "光标行为不稳定，无法确定输入模式".to_string()
        } else {
            String::new()
        };
        ProbeOutcome {
            mode,
            probe_distance,
            samples: samples.len(),
            note,
        }
    })();
    if !restore.restore() {
        outcome.mode = InputMode::Undetermined;
        if outcome.note.is_empty() {
            outcome.note = "探测结束时无法恢复鼠标/视角位置".to_string();
        } else {
            outcome.note.push_str("；探测结束时无法恢复鼠标/视角位置");
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_distance_scales_with_pointer_speed() {
        assert_eq!(recommended_probe_distance(10), 10);
        assert_eq!(recommended_probe_distance(1), 96);
        assert_eq!(recommended_probe_distance(20), 8);
        assert_eq!(recommended_probe_distance(0), 96); // 非法值收敛到边界
        assert_eq!(recommended_probe_distance(100), 8);
    }

    #[test]
    fn classify_pulled_back_cursor_is_relative() {
        let origin = (100, 100);
        let injected = (10, 0);
        // 光标被拉回原点附近
        let samples = [(100, 100), (101, 100), (100, 100)];
        assert_eq!(
            classify_cursor_behavior(origin, injected, &samples),
            InputMode::Relative
        );
    }

    #[test]
    fn classify_stable_aligned_cursor_is_desktop_absolute() {
        let origin = (100, 100);
        let injected = (10, 0);
        // 光标稳定停留在注入后的位置（方向与注入一致）
        let samples = [(110, 100), (110, 100), (110, 100), (110, 100)];
        assert_eq!(
            classify_cursor_behavior(origin, injected, &samples),
            InputMode::DesktopAbsolute
        );
    }

    #[test]
    fn classify_unstable_cursor_is_undetermined() {
        let origin = (100, 100);
        let injected = (10, 0);
        // 光标在采样之间大幅跳变（不稳定）：既非拉回原点也非稳定停留
        let samples = [(100, 100), (95, 100), (110, 100)];
        assert_eq!(
            classify_cursor_behavior(origin, injected, &samples),
            InputMode::Undetermined
        );
    }

    #[test]
    fn classify_needs_at_least_two_samples() {
        let origin = (100, 100);
        let samples = [(100, 100)];
        assert_eq!(
            classify_cursor_behavior(origin, (10, 0), &samples),
            InputMode::Undetermined
        );
    }

    #[test]
    fn desktop_rect_checks_boundaries() {
        let desktop = (0, 0, 1920, 1080);
        assert!(rect_within_desktop((0.0, 0.0), (1919.0, 1079.0), desktop));
        assert!(!rect_within_desktop((-1.0, 0.0), (100.0, 100.0), desktop));
        assert!(!rect_within_desktop((0.0, 0.0), (1920.0, 100.0), desktop));
        assert!(!rect_within_desktop((0.0, 0.0), (100.0, 1080.0), desktop));
    }
}
