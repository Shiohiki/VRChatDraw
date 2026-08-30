//! 阶段 2：线稿响应提取
//! 对应 C++ ExtractInkMask / Dilate3x3 / DilateSquare / RemoveSmallComponents /
//! ClearImageBorder / ExtractColorBoundaries / ResponsePercentile / BuildPsLineResponse

use super::basic::{gaussian_blur, otsu_threshold, to_color_planes};
use super::types::{DecodedImage, PsLineResponse};

/// 3×3 膨胀（对应 C++ Dilate3x3）
pub fn dilate3x3(input: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as i32;
    let h = height as i32;
    let mut output = vec![0u8; input.len()];
    for y in 0..h {
        for x in 0..w {
            let mut foreground = false;
            'outer: for oy in -1..=1 {
                for ox in -1..=1 {
                    let sx = x + ox;
                    let sy = y + oy;
                    if sx >= 0 && sy >= 0 && sx < w && sy < h && input[(sy * w + sx) as usize] != 0
                    {
                        foreground = true;
                        break 'outer;
                    }
                }
            }
            output[(y * w + x) as usize] = if foreground { 1 } else { 0 };
        }
    }
    output
}

/// 方形膨胀（对应 C++ DilateSquare）
pub fn dilate_square(input: &[u8], width: u32, height: u32, radius: i32) -> Vec<u8> {
    if radius <= 1 {
        return dilate3x3(input, width, height);
    }
    let w = width as i32;
    let h = height as i32;
    let mut output = vec![0u8; input.len()];
    for y in 0..h {
        for x in 0..w {
            let mut foreground = false;
            'outer: for oy in -radius..=radius {
                for ox in -radius..=radius {
                    let sx = x + ox;
                    let sy = y + oy;
                    if sx >= 0 && sy >= 0 && sx < w && sy < h && input[(sy * w + sx) as usize] != 0
                    {
                        foreground = true;
                        break 'outer;
                    }
                }
            }
            output[(y * w + x) as usize] = if foreground { 1 } else { 0 };
        }
    }
    output
}

/// 移除小连通域（对应 C++ RemoveSmallComponents）
pub fn remove_small_components(mask: &mut [u8], width: u32, height: u32, processing_scale: f32) {
    let scaled_minimum = (8.0f32 * processing_scale * processing_scale).ceil() as usize;
    let minimum_size = scaled_minimum.max(mask.len() / 180_000);
    let w = width as i32;
    let h = height as i32;
    let mut visited = vec![0u8; mask.len()];
    let mut component: Vec<usize> = Vec::new();
    let mut pending: std::collections::VecDeque<usize> = std::collections::VecDeque::new();

    for start in 0..mask.len() {
        if mask[start] == 0 || visited[start] != 0 {
            continue;
        }
        component.clear();
        visited[start] = 1;
        pending.push_back(start);
        while let Some(current) = pending.pop_front() {
            component.push(current);
            let x = (current % w as usize) as i32;
            let y = (current / w as usize) as i32;
            for oy in -1..=1 {
                for ox in -1..=1 {
                    if ox == 0 && oy == 0 {
                        continue;
                    }
                    let nx = x + ox;
                    let ny = y + oy;
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        continue;
                    }
                    let neighbor = (ny * w + nx) as usize;
                    if mask[neighbor] != 0 && visited[neighbor] == 0 {
                        visited[neighbor] = 1;
                        pending.push_back(neighbor);
                    }
                }
            }
        }
        if component.len() < minimum_size {
            for &index in &component {
                mask[index] = 0;
            }
        }
    }
}

/// 清空图像边缘（对应 C++ ClearImageBorder）
pub fn clear_image_border(mask: &mut [u8], width: u32, height: u32, border_width: u32) {
    for y in 0..height {
        for x in 0..width {
            if x < border_width
                || y < border_width
                || x + border_width >= width
                || y + border_width >= height
            {
                mask[(y * width + x) as usize] = 0;
            }
        }
    }
}

/// RGB 结构张量单边界提取（对应 C++ ExtractColorBoundaries）
/// 返回边界掩码（方向场仅在内部用于非极大值抑制，不外传）
pub fn extract_color_boundaries(
    image: &DecodedImage,
    width: u32,
    height: u32,
    flat_color_illustration: bool,
    processing_scale: f32,
) -> Vec<u8> {
    let planes = to_color_planes(image);
    let red = gaussian_blur(&planes.red, width, height, 1.0);
    let green = gaussian_blur(&planes.green, width, height, 1.0);
    let blue = gaussian_blur(&planes.blue, width, height, 1.0);
    let size = planes.red.len();
    let mut response = vec![0u8; size];
    let mut direction_x = vec![0i8; size];
    let mut direction_y = vec![0i8; size];

    for y in 1..height as i64 - 1 {
        for x in 1..width as i64 - 1 {
            let index = (y * width as i64 + x) as usize;
            let gx = |ch: &Vec<f32>| ch[index + 1] - ch[index - 1];
            let gy = |ch: &Vec<f32>| ch[index + width as usize] - ch[index - width as usize];
            let (red_x, red_y) = (gx(&red), gy(&red));
            let (green_x, green_y) = (gx(&green), gy(&green));
            let (blue_x, blue_y) = (gx(&blue), gy(&blue));
            let xx = red_x * red_x + green_x * green_x + blue_x * blue_x;
            let yy = red_y * red_y + green_y * green_y + blue_y * blue_y;
            let xy = red_x * red_y + green_x * green_y + blue_x * blue_y;
            let discriminant = (((xx - yy) * (xx - yy) + 4.0 * xy * xy).max(0.0)).sqrt();
            let dominant = (((xx + yy + discriminant) / 6.0).max(0.0)).sqrt();
            response[index] = dominant.round().clamp(0.0, 255.0) as u8;
            let angle = 0.5 * (2.0 * xy).atan2(xx - yy);
            direction_x[index] = angle.cos().round() as i8;
            direction_y[index] = angle.sin().round() as i8;
        }
    }

    let threshold = if flat_color_illustration {
        otsu_threshold(&response).clamp(7, 20)
    } else {
        otsu_threshold(&response).clamp(14, 36)
    };
    let continuation_threshold = 4.max(threshold / 2);
    let boundary_margin = (4.0f32 * processing_scale).round() as i64;
    let w = width as i64;
    let h = height as i64;
    let mut local_maxima = vec![0u8; size];
    for y in boundary_margin..(h - boundary_margin) {
        for x in boundary_margin..(w - boundary_margin) {
            let index = (y * w + x) as usize;
            if response[index] < continuation_threshold as u8 {
                continue;
            }
            let step_x = direction_x[index] as i64;
            let step_y = direction_y[index] as i64;
            if step_x == 0 && step_y == 0 {
                continue;
            }
            let forward = (index as i64 + step_y * w + step_x) as usize;
            let backward = (index as i64 - step_y * w - step_x) as usize;
            local_maxima[index] =
                if response[index] >= response[forward] && response[index] > response[backward] {
                    1
                } else {
                    0
                };
        }
    }

    let mut boundaries = vec![0u8; size];
    let mut pending: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for (index, &lm) in local_maxima.iter().enumerate() {
        if lm != 0 && response[index] >= threshold as u8 {
            boundaries[index] = 1;
            pending.push_back(index);
        }
    }
    while let Some(current) = pending.pop_front() {
        let x = (current % w as usize) as i64;
        let y = (current / w as usize) as i64;
        for oy in -1..=1 {
            for ox in -1..=1 {
                let nx = x + ox;
                let ny = y + oy;
                if (ox == 0 && oy == 0) || nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let neighbor = (ny * w + nx) as usize;
                if local_maxima[neighbor] != 0 && boundaries[neighbor] == 0 {
                    boundaries[neighbor] = 1;
                    pending.push_back(neighbor);
                }
            }
        }
    }
    boundaries
}

/// 响应百分位（对应 C++ ResponsePercentile）
pub fn response_percentile(response: &[u8], support: &[u8], percentile: f64) -> u8 {
    let mut histogram = [0u64; 256];
    let mut count = 0u64;
    for i in 0..response.len() {
        if support[i] != 0 && response[i] != 0 {
            histogram[response[i] as usize] += 1;
            count += 1;
        }
    }
    if count == 0 {
        return 0;
    }
    let target = ((count as f64 * percentile).ceil() as u64).clamp(1, count);
    let mut cumulative = 0u64;
    for (value, &count) in histogram.iter().enumerate().skip(1) {
        cumulative += count;
        if cumulative >= target {
            return value as u8;
        }
    }
    255
}

/// PS 式多尺度线响应（对应 C++ BuildPsLineResponse）
/// 三尺度 color-dodge 响应融合：细线多尺度支持
pub fn build_ps_line_response(
    grayscale: &[u8],
    width: u32,
    height: u32,
    processing_scale: f32,
) -> PsLineResponse {
    let denoised = gaussian_blur(
        grayscale,
        width,
        height,
        (0.55 * processing_scale).max(0.55),
    );
    let base_sigmas = [0.7f32, 1.4, 2.8];
    let weights = [1.0f32, 1.0, 0.72];
    let mut fused = vec![0u8; grayscale.len()];
    let mut scales = [Vec::new(), Vec::new(), Vec::new()];
    for (scale_index, &base_sigma) in base_sigmas.iter().enumerate() {
        let mut scale_response = vec![0u8; grayscale.len()];
        let local_mean = gaussian_blur(
            grayscale,
            width,
            height,
            (base_sigma * processing_scale).max(0.65),
        );
        for i in 0..grayscale.len() {
            let denominator = local_mean[i].max(1.0);
            let color_dodge_ink = (1.0 - denoised[i] / denominator).max(0.0);
            let value = (color_dodge_ink * 255.0 * 1.35 * weights[scale_index])
                .round()
                .clamp(0.0, 255.0) as u8;
            scale_response[i] = value;
            fused[i] = fused[i].max(value);
        }
        scales[scale_index] = scale_response;
    }
    PsLineResponse { fused, scales }
}
