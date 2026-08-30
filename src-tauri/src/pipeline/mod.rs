//! VRC-Draw 线稿管线 Rust 移植（原项目 FlyPig01/VRC-Draw，MIT）
//! 已移植阶段：basic → response → canonical → skeleton → region → trace → optimize → vector_fit
//! 管线：解码缩放 → 预处理模糊(可选) → 规范线稿（PS 响应+边界+补线）→ 区域分类 → 拓扑追踪 → 欧拉迹优化 → 矢量拟合

pub mod basic;
pub mod canonical;
pub mod optimize;
pub mod path_math;
pub mod region;
pub mod response;
pub mod skeleton;
pub mod trace;
pub mod types;
pub mod vector_fit;

use image::DynamicImage;
use types::PointF;

/// 管线入口：DynamicImage → 笔画列表（对应 C++ ProcessImage 核心流程）
/// 输出坐标单位为**输入图尺寸**（lib.rs 传入的是 lineart：非 AI 时即原图，
/// AI 时即 AI 输出图尺寸），绘制引擎按 sensitivity/vertical_stretch 缩放
/// `blur_size`：预处理高斯模糊强度（1 = 不模糊，>1 时按该值模糊灰度图）
/// `epsilon_ratio`：轮廓简化容差倍率（1.5 保留原有默认行为）
pub fn process_image(
    img: &DynamicImage,
    blur_size: u32,
    epsilon_ratio: f64,
) -> Result<Vec<Vec<PointF>>, String> {
    let (input_w, input_h) = (img.width(), img.height());
    let decoded = basic::decode_and_resize(img);
    let (width, height) = (decoded.width, decoded.height);
    let grayscale = if blur_size > 1 {
        // 预处理模糊（前端"模糊大小"滑块）：值 1 保持原行为，>1 时模糊灰度图
        basic::gaussian_blur(
            &basic::to_grayscale(&decoded),
            width,
            height,
            blur_size as f32,
        )
        .into_iter()
        .map(|v| v.round().clamp(0.0, 255.0) as u8)
        .collect()
    } else {
        basic::to_grayscale(&decoded)
    };
    let processing_scale = (width.max(height) as f32 / 768.0).max(1.0);

    let canonical = canonical::extract_canonical_line_art(
        &decoded,
        &grayscale,
        width,
        height,
        processing_scale,
    );
    if !canonical.topology.iter().any(|&v| v != 0) {
        return Err("没有从图片中提取到可绘制线稿".to_string());
    }

    let route = region::build_route_skeleton(
        &canonical.topology,
        &canonical.evidence,
        width,
        height,
        processing_scale,
    );
    if !route.skeleton.iter().any(|&v| v != 0) {
        return Err("没有从图片中提取到可绘制线稿".to_string());
    }

    let (strokes, metadata) = trace::trace_skeleton(
        &route.skeleton,
        width,
        height,
        &canonical.topology,
        &canonical.evidence,
        &route.region_types,
        &route.ink_distance,
        processing_scale,
        epsilon_ratio as f32,
    );
    if strokes.is_empty() {
        return Err("没有从图片中提取到可绘制线稿".to_string());
    }

    let drawing_path = optimize::strokes_to_drawing_path(width, height, strokes, metadata);
    let optimized = optimize::optimize_drawing_path_lossless(drawing_path);

    // 阶段 7：矢量拟合（直线/贝塞尔），门禁失败逐跨度回退折线
    let allowed_ink = types::BinaryImage {
        width,
        height,
        pixels: canonical.topology,
    };
    let vector_path =
        vector_fit::fit_vector_drawing_path(&optimized, &allowed_ink, &Default::default());
    let flattened = vector_fit::flatten_vector_drawing_path(&vector_path, 0.25);

    // 坐标放大回输入图尺寸（处理尺寸 ≤1536，前端按原图比例渲染需全尺寸坐标）
    let ratio_x = input_w as f32 / width as f32;
    let ratio_y = input_h as f32 / height as f32;
    Ok(flattened
        .strokes
        .into_iter()
        .map(|stroke| {
            stroke
                .into_iter()
                .map(|p| PointF {
                    x: p.x * ratio_x,
                    y: p.y * ratio_y,
                })
                .collect()
        })
        .collect())
}
