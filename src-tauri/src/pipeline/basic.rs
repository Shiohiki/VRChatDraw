//! 阶段 1：图像基础层
//! 对应 C++ ImageProcessor.cpp 的 DecodeAndResize / ToGrayscale / ToColorPlanes /
//! GaussianBlur / OtsuThreshold / 类型判别 / MergeInteriorDarkInk

use image::DynamicImage;

use super::types::{ColorPlanes, DecodedImage};

/// 图像处理上限（对应 C++ ImageProcessingOptions 默认值）
pub const MAXIMUM_DIMENSION: u32 = 1536;
pub const MAXIMUM_PIXELS: u64 = 2_400_000;

/// 从 DynamicImage 解码并缩放（对应 C++ DecodeAndResize；WIC 由 image crate 替代）
pub fn decode_and_resize(img: &DynamicImage) -> DecodedImage {
    let (source_width, source_height) = (img.width(), img.height());
    let source_pixels = source_width as f64 * source_height as f64;

    let dimension_scale = MAXIMUM_DIMENSION as f64 / source_width.max(source_height).max(1) as f64;
    let pixel_scale = (MAXIMUM_PIXELS as f64 / source_pixels.max(1.0)).sqrt();
    let scale = dimension_scale.min(pixel_scale).min(1.0);

    let target_width = ((source_width as f64 * scale).round() as u32).max(1);
    let target_height = ((source_height as f64 * scale).round() as u32).max(1);

    let resized = if target_width != source_width || target_height != source_height {
        img.resize(
            target_width,
            target_height,
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        img.clone()
    };
    // 注意：image crate 的 resize 可能按内部比例约束实际输出尺寸（如 1536 → 1535），
    // 必须以实际输出尺寸为准，否则下游按声明尺寸访问 bgra 会越界
    let (actual_width, actual_height) = (resized.width(), resized.height());

    // 转为 BGRA（含 alpha）
    let rgba = resized.to_rgba8();
    let mut bgra = Vec::with_capacity(rgba.len());
    for p in rgba.pixels() {
        bgra.push(p[2]); // B
        bgra.push(p[1]); // G
        bgra.push(p[0]); // R
        bgra.push(p[3]); // A
    }

    DecodedImage {
        width: actual_width,
        height: actual_height,
        bgra,
    }
}

/// 透明像素合成白底（对应 C++ CompositeOnWhite）
#[inline]
pub fn composite_on_white(value: u8, alpha: u8) -> u8 {
    ((value as u32 * alpha as u32 + 255u32 * (255u32 - alpha as u32)) / 255u32) as u8
}

/// 把任意图像转换为**白底合成**的 RGB 图像（透明/半透明像素按 alpha 与白色背景混合）。
/// 供"编码回写"路径使用：image crate 的 `DynamicImage::to_rgb8()` 会直接丢弃
/// alpha 而不做任何合成——透明像素的 RGB 分量（通常为黑）会被原样编码，
/// 导致缩略图/缓存显示黑底、甚至让管线把透明区误判为黑色墨迹。
/// 不透明像素（alpha=255）经本函数逐位等于 to_rgb8 的结果，行为无差异。
pub fn to_rgb_on_white(img: &image::DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (pixel, output) in rgba.pixels().zip(rgb.pixels_mut()) {
        let r = composite_on_white(pixel[0], pixel[3]);
        let g = composite_on_white(pixel[1], pixel[3]);
        let b = composite_on_white(pixel[2], pixel[3]);
        *output = image::Rgb([r, g, b]);
    }
    rgb
}

/// 感知灰度（对应 C++ ToGrayscale，BT.601 权重）
pub fn to_grayscale(image: &DecodedImage) -> Vec<u8> {
    let size = image.width as usize * image.height as usize;
    let mut grayscale = vec![0u8; size];
    for (pixel, out) in grayscale.iter_mut().enumerate() {
        let offset = pixel * 4;
        let alpha = image.bgra[offset + 3];
        let blue = composite_on_white(image.bgra[offset], alpha);
        let green = composite_on_white(image.bgra[offset + 1], alpha);
        let red = composite_on_white(image.bgra[offset + 2], alpha);
        *out = ((77u32 * red as u32 + 150u32 * green as u32 + 29u32 * blue as u32) >> 8) as u8;
    }
    grayscale
}

/// 三通道平面（对应 C++ ToColorPlanes）
pub fn to_color_planes(image: &DecodedImage) -> ColorPlanes {
    let size = image.width as usize * image.height as usize;
    let mut planes = ColorPlanes {
        red: vec![0u8; size],
        green: vec![0u8; size],
        blue: vec![0u8; size],
    };
    for (pixel, i) in (0..size).enumerate() {
        let offset = pixel * 4;
        let alpha = image.bgra[offset + 3];
        planes.blue[i] = composite_on_white(image.bgra[offset], alpha);
        planes.green[i] = composite_on_white(image.bgra[offset + 1], alpha);
        planes.red[i] = composite_on_white(image.bgra[offset + 2], alpha);
    }
    planes
}

/// 可分离高斯模糊（对应 C++ GaussianBlur，clamp 边界）
pub fn gaussian_blur(input: &[u8], width: u32, height: u32, sigma: f32) -> Vec<f32> {
    // 防御：sigma 非法（0/负/NaN）时返回原值拷贝，避免除零产生 NaN 核
    if !sigma.is_finite() || sigma <= 0.0 || width == 0 || height == 0 {
        return input.iter().map(|&v| v as f32).collect();
    }
    let radius = ((sigma * 2.5).ceil() as i32).max(1);
    let mut kernel = vec![0.0f32; (radius * 2 + 1) as usize];
    let mut kernel_sum = 0.0f32;
    for offset in -radius..=radius {
        let value = (-(offset * offset) as f32 / (2.0 * sigma * sigma)).exp();
        kernel[(offset + radius) as usize] = value;
        kernel_sum += value;
    }
    for v in kernel.iter_mut() {
        *v /= kernel_sum;
    }

    let w = width as usize;
    let h = height as usize;
    let mut horizontal = vec![0.0f32; input.len()];
    let mut output = vec![0.0f32; input.len()];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0f32;
            for offset in -radius..=radius {
                let sample_x = (x as i32 + offset).clamp(0, w as i32 - 1) as usize;
                sum += input[y * w + sample_x] as f32 * kernel[(offset + radius) as usize];
            }
            horizontal[y * w + x] = sum;
        }
    }
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0f32;
            for offset in -radius..=radius {
                let sample_y = (y as i32 + offset).clamp(0, h as i32 - 1) as usize;
                sum += horizontal[sample_y * w + x] * kernel[(offset + radius) as usize];
            }
            output[y * w + x] = sum;
        }
    }
    output
}

/// Otsu 全局阈值（对应 C++ OtsuThreshold）
pub fn otsu_threshold(values: &[u8]) -> i32 {
    let mut histogram = [0u64; 256];
    for &v in values {
        histogram[v as usize] += 1;
    }
    let total = values.len() as u64;
    let mut weighted_total = 0u64;
    for (value, &count) in histogram.iter().enumerate() {
        weighted_total += value as u64 * count;
    }

    let mut background_weight = 0u64;
    let mut background_sum = 0u64;
    let mut best_variance = -1.0f64;
    let mut best_threshold = 128i32;
    for threshold in 0..255 {
        background_weight += histogram[threshold as usize];
        background_sum += threshold as u64 * histogram[threshold as usize];
        if background_weight == 0 || background_weight == total {
            continue;
        }
        let foreground_weight = total - background_weight;
        let background_mean = background_sum as f64 / background_weight as f64;
        let foreground_mean = (weighted_total - background_sum) as f64 / foreground_weight as f64;
        let difference = background_mean - foreground_mean;
        let variance =
            background_weight as f64 * foreground_weight as f64 * difference * difference;
        if variance > best_variance {
            best_variance = variance;
            best_threshold = threshold;
        }
    }
    best_threshold
}

/// 线稿判别（对应 C++ IsLikelyLineDrawing）
pub fn is_likely_line_drawing(grayscale: &[u8]) -> bool {
    let size = grayscale.len().max(1) as f64;
    let white = grayscale.iter().filter(|&&v| v >= 238).count() as f64;
    let dark = grayscale.iter().filter(|&&v| v <= 96).count() as f64;
    white / size >= 0.48 && dark / size <= 0.32
}

/// 单色线稿判别（对应 C++ IsLikelyMonochromeLineDrawing）
pub fn is_likely_monochrome_line_drawing(image: &DecodedImage, grayscale: &[u8]) -> bool {
    if !is_likely_line_drawing(grayscale) {
        return false;
    }
    let mut chromatic = 0usize;
    for (pixel, _) in grayscale.iter().enumerate() {
        let offset = pixel * 4;
        let alpha = image.bgra[offset + 3];
        let blue = composite_on_white(image.bgra[offset], alpha) as i32;
        let green = composite_on_white(image.bgra[offset + 1], alpha) as i32;
        let red = composite_on_white(image.bgra[offset + 2], alpha) as i32;
        let minimum = red.min(green).min(blue);
        let maximum = red.max(green).max(blue);
        if maximum - minimum >= 12 {
            chromatic += 1;
        }
    }
    let size = grayscale.len().max(1) as f64;
    chromatic as f64 / size <= 0.16
}

/// 平涂插画判别（对应 C++ IsLikelyFlatColorIllustration）
pub fn is_likely_flat_color_illustration(image: &DecodedImage, grayscale: &[u8]) -> bool {
    let mut quantized = [0u8; 4096];
    let mut color_count = 0usize;
    let mut background_like = 0usize;
    let mut chromatic = 0usize;

    for (pixel, &g) in grayscale.iter().enumerate() {
        let offset = pixel * 4;
        let alpha = image.bgra[offset + 3];
        let blue = composite_on_white(image.bgra[offset], alpha);
        let green = composite_on_white(image.bgra[offset + 1], alpha);
        let red = composite_on_white(image.bgra[offset + 2], alpha);
        let bin = ((red >> 4) as usize) << 8 | ((green >> 4) as usize) << 4 | (blue >> 4) as usize;
        if quantized[bin] == 0 {
            quantized[bin] = 1;
            color_count += 1;
        }
        if g <= 32 || g >= 238 {
            background_like += 1;
        }
        let minimum = red.min(green).min(blue) as i32;
        let maximum = red.max(green).max(blue) as i32;
        if maximum - minimum >= 12 {
            chromatic += 1;
        }
    }

    let size = grayscale.len().max(1) as f64;
    color_count <= 512 && background_like as f64 / size >= 0.18 && chromatic as f64 / size >= 0.08
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不透明灰底图（alpha=255，避免合成白底混淆灰度断言）
    fn test_image(w: u32, h: u32, fill: u8) -> DecodedImage {
        DecodedImage {
            width: w,
            height: h,
            bgra: [fill, fill, fill, 255].repeat((w * h) as usize),
        }
    }

    #[test]
    fn to_grayscale_dims_match() {
        let img = test_image(64, 48, 200);
        let g = to_grayscale(&img);
        assert_eq!(g.len(), 64 * 48);
        // 不透明灰底(200,200,200) → 感知灰度约 200
        assert!((g[0] as i32 - 200).abs() <= 2);
    }

    #[test]
    fn gaussian_blur_preserves_flat_region() {
        let w = 32u32;
        let h = 32u32;
        let img = test_image(w, h, 128);
        let g = to_grayscale(&img);
        let blurred = gaussian_blur(&g, w, h, 1.5);
        // 均匀区域模糊后均值应保持不变
        let mean = blurred.iter().sum::<f32>() / blurred.len() as f32;
        assert!((mean - 128.0).abs() < 1.0);
        assert_eq!(blurred.len(), g.len());
    }

    #[test]
    fn gaussian_blur_guards_bad_sigma() {
        let w = 16u32;
        let h = 16u32;
        let img = test_image(w, h, 100);
        let g = to_grayscale(&img);
        // sigma=0 / NaN 应返回原值拷贝而非 NaN
        let out = gaussian_blur(&g, w, h, 0.0);
        assert!(out.iter().all(|v| v.is_finite()));
        let out2 = gaussian_blur(&g, w, h, f32::NAN);
        assert!(out2.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn otsu_separates_bimodal() {
        // 一半 20 一半 230：Otsu 最优分割点落在两峰之间的任一阈值，
        // 实现取遍历到的第一个最优（threshold=20）
        let mut values = vec![0u8; 1000];
        values[..500].fill(20);
        values[500..].fill(230);
        let t = otsu_threshold(&values);
        assert!((15..40).contains(&t));
    }

    #[test]
    fn otsu_uniform_falls_back() {
        let values = vec![128u8; 100];
        let t = otsu_threshold(&values);
        assert!((0..=255).contains(&t));
    }

    #[test]
    fn to_rgb_on_white_composites_transparent_pixels() {
        // 4 组典型像素：全透明黑 → 白；不透明红 → 原色；50% 黑 → 127；50% 自定义 → 加权混合
        let src = image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(
                2,
                2,
                vec![
                    0, 0, 0, 0, // (0,0) alpha=0
                    255, 0, 0, 255, // (1,0) alpha=255
                    0, 0, 0, 128, // (0,1) 半透明黑
                    0, 128, 128, 128, // (1,1) 半透明自定义色
                ],
            )
            .unwrap(),
        );
        let out = to_rgb_on_white(&src);
        assert_eq!(out.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(out.get_pixel(1, 0).0, [255, 0, 0]);
        // (0*128 + 255*127) / 255 = 127
        assert_eq!(out.get_pixel(0, 1).0, [127, 127, 127]);
        // R=(0*128+255*127)/255=127；G=B=(128*128+255*127)/255=191（向下取整）
        assert_eq!(out.get_pixel(1, 1).0, [127, 191, 191]);
    }

    #[test]
    fn decode_and_resize_scales_down() {
        // 2048x2048 超过 MAXIMUM_DIMENSION(1536) → 缩放后长边 ≤1536
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2048,
            2048,
            image::Rgb([10, 10, 10]),
        ));
        let decoded = decode_and_resize(&img);
        assert!(decoded.width.max(decoded.height) <= 1536);
        assert_eq!(
            decoded.bgra.len() as u64,
            decoded.width as u64 * decoded.height as u64 * 4
        );
    }
}
