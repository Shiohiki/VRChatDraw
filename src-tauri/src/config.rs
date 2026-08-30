#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    pub blur_size: u32,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            blur_size: 1, // 默认不模糊（输入通常是干净的黑白线稿）
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ContourConfig {
    pub epsilon_ratio: f64,
}

impl Default for ContourConfig {
    fn default() -> Self {
        Self { epsilon_ratio: 1.5 }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DrawingConfig {
    pub draw_speed: f64, // 每点耗时（秒）
    pub max_step_px: i32,
    pub lift_pen_delay: f64,
    pub start_delay: f64,
    pub sensitivity: f64,
    pub vertical_stretch: f64,
    pub lift_pen_speed: f64,
}

impl Default for DrawingConfig {
    fn default() -> Self {
        Self {
            // VRChat 按游戏帧消费鼠标移动；低于约一帧的间隔会把多次移动累计成跳跃，
            // 从而让连续线看起来像一排分离的点。
            draw_speed: 0.016,
            max_step_px: 4,
            lift_pen_delay: 0.05, // 抬笔延迟 50ms（太短会导致拖墨）
            start_delay: 1.5,
            sensitivity: 1.2,
            vertical_stretch: 1.4,
            lift_pen_speed: 100.0, // 抬笔瞬间移动
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub image: ImageConfig,
    pub contour: ContourConfig,
    pub drawing: DrawingConfig,
    pub theme_dark: bool, // 界面主题：true=深色（黑主白辅），false=浅色（白主黑辅）
    pub show_grid: bool,  // 预览区显示网格
    pub canvas_dark: bool, // 预览画布底色：true=深色，false=白色
    pub use_ai: bool,     // 启用 AI 预处理（彩色图转线稿），与界面开关同步持久化
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            image: ImageConfig::default(),
            contour: ContourConfig::default(),
            drawing: DrawingConfig::default(),
            theme_dark: true,
            show_grid: true,
            canvas_dark: false,
            use_ai: false,
        }
    }
}

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

impl AppConfig {
    /// 界面配置路径（用户数据目录中的 ui_config.json，与 AI 的 config.json 分开）
    pub fn config_path() -> std::path::PathBuf {
        crate::storage::data_path("ui_config.json")
    }

    /// 加载配置并返回错误信息：文件不存在 = 正常（首次运行）；
    /// 解析失败 = 返回默认值 + 错误说明（避免配置（含用户参数）静默丢失无提示）
    pub fn load_with_error() -> (Self, Option<String>) {
        let Some((path, raw_bytes)) = (match crate::storage::read_preferred("ui_config.json") {
            Ok(value) => value,
            Err(error) => {
                return (
                    Self::default(),
                    Some(format!("无法读取配置文件：{error}（已使用默认配置）")),
                );
            }
        }) else {
            return (Self::default(), None);
        };
        if raw_bytes.len() > MAX_CONFIG_BYTES {
            let backup = crate::storage::preserve_corrupt(&path)
                .map(|path| format!("损坏文件已保留为 {path:#?}"))
                .unwrap_or_else(|error| format!("无法备份损坏文件：{error}"));
            return (
                Self::default(),
                Some(format!("{path:#?} 超过 1 MB，已使用默认配置；{backup}")),
            );
        }
        let raw = match String::from_utf8(raw_bytes) {
            Ok(value) => value,
            Err(error) => {
                let backup = crate::storage::preserve_corrupt(&path)
                    .map(|path| format!("损坏文件已保留为 {path:#?}"))
                    .unwrap_or_else(|backup_error| format!("无法备份损坏文件：{backup_error}"));
                return (
                    Self::default(),
                    Some(format!("{path:#?} 不是有效 UTF-8：{error}；{backup}")),
                );
            }
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(mut cfg) => {
                cfg.sanitize();
                (cfg, None)
            }
            Err(error) => {
                let backup = crate::storage::preserve_corrupt(&path)
                    .map(|path| format!("损坏文件已保留为 {path:#?}"))
                    .unwrap_or_else(|backup_error| format!("无法备份损坏文件：{backup_error}"));
                (
                    Self::default(),
                    Some(format!(
                        "{path:#?} 解析失败，已使用默认配置：{error}；{backup}"
                    )),
                )
            }
        }
    }

    /// 数据合法性兜底：用户手改配置文件可能出现负值/NaN/超大值。
    /// - 负值会让延时参数产生错误的等待行为（绘制卡死或节奏异常）
    /// - NaN 经 f64::max 会归零、经 clamp 会保留 NaN，必须统一走 is_finite 分支
    /// - 无上界时 draw_speed=1e300 会饱和成 ~58 万年 sleep，F10 无法中断
    pub fn sanitize(&mut self) {
        fn sane(v: f64, def: f64, min: f64, max: f64) -> f64 {
            if !v.is_finite() {
                def
            } else {
                v.clamp(min, max)
            }
        }
        let d = &mut self.drawing;
        // These limits mirror the controls exposed by the frontend.  Keeping the
        // same bounds here prevents hand-edited or stale config files from
        // bypassing the safe UI ranges.
        d.draw_speed = sane(d.draw_speed, 0.016, 0.016, 0.030);
        d.lift_pen_delay = sane(d.lift_pen_delay, 0.05, 0.0, 10.0);
        d.start_delay = sane(d.start_delay, 1.5, 0.0, 600.0);
        d.sensitivity = sane(d.sensitivity, 1.2, 0.1, 3.0);
        d.vertical_stretch = sane(d.vertical_stretch, 1.4, 0.5, 2.5);
        d.lift_pen_speed = sane(d.lift_pen_speed, 100.0, 10.0, 100.0);
        // 与 16ms 移动节奏配套；更大的单步仍可能跨过 VRChat 的笔刷采样范围。
        d.max_step_px = d.max_step_px.clamp(1, 6);
        self.image.blur_size = self.image.blur_size.clamp(1, 15);
        self.contour.epsilon_ratio = sane(self.contour.epsilon_ratio, 1.5, 0.1, 10.0);
    }

    /// Return a normalized copy suitable for both persistence and runtime use.
    pub fn normalized(&self) -> Self {
        let mut cfg = self.clone();
        cfg.sanitize();
        cfg
    }

    /// 保存界面配置（先写临时文件再 rename 原子替换，崩溃时不会留下半截损坏的配置）
    pub fn save(&self) -> Result<(), String> {
        let normalized = self.normalized();
        normalized.save_normalized()
    }

    pub(crate) fn save_normalized(&self) -> Result<(), String> {
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let path = Self::config_path();
        // 每次写入使用进程号 + 单调时间戳，避免异步保存互相覆盖临时文件。
        crate::storage::atomic_write(&path, s.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_all_runtime_limits() {
        let mut cfg = AppConfig::default();
        cfg.image.blur_size = u32::MAX;
        cfg.contour.epsilon_ratio = f64::NAN;
        cfg.drawing.draw_speed = f64::INFINITY;
        cfg.drawing.max_step_px = i32::MIN;
        cfg.drawing.sensitivity = -100.0;
        cfg.drawing.vertical_stretch = 100.0;
        cfg.drawing.lift_pen_speed = 1000.0;
        cfg.sanitize();
        assert_eq!(cfg.image.blur_size, 15);
        assert_eq!(cfg.contour.epsilon_ratio, 1.5);
        assert_eq!(cfg.drawing.draw_speed, 0.016);
        assert_eq!(cfg.drawing.max_step_px, 1);
        assert_eq!(cfg.drawing.sensitivity, 0.1);
        assert_eq!(cfg.drawing.vertical_stretch, 2.5);
        assert_eq!(cfg.drawing.lift_pen_speed, 100.0);
    }

    #[test]
    fn sanitize_migrates_unsafe_legacy_drawing_timing() {
        let mut cfg = AppConfig::default();
        cfg.drawing.draw_speed = 0.005;
        cfg.drawing.max_step_px = 20;
        cfg.sanitize();
        assert_eq!(cfg.drawing.draw_speed, 0.016);
        assert_eq!(cfg.drawing.max_step_px, 6);
    }
}
