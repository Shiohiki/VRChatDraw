//! 阶段 3：规范线稿提取
//! 对应 C++ SkeletonNeighborsForRepair / EndpointOutwardDirection /
//! RemoveWeakOnePixelBridges / RepairDirectionalGaps / ExtractCanonicalLineArt

use super::basic::{
    gaussian_blur, is_likely_flat_color_illustration, is_likely_monochrome_line_drawing,
    otsu_threshold,
};
use super::response::{
    build_ps_line_response, clear_image_border, dilate_square, extract_color_boundaries,
    remove_small_components, response_percentile,
};
use super::skeleton::{pixel_neighbors, thin_zhang_suen};
use super::types::{
    CanonicalLineArtExtraction, DecodedImage, LinePixelEvidence, PointF, EVIDENCE_ORIGINAL_DARK,
    EVIDENCE_REPAIRED_GAP, EVIDENCE_RGB_BOUNDARY, EVIDENCE_STRONG_CORE,
};

/// 端点朝外方向（对应 C++ EndpointOutwardDirection）：沿骨架追踪取反向
fn endpoint_outward_direction(
    skeleton: &[u8],
    width: u32,
    height: u32,
    endpoint: usize,
    sample_length: usize,
) -> PointF {
    let mut previous = usize::MAX;
    let mut current = endpoint;
    let mut farthest = endpoint;
    for _ in 0..sample_length {
        let mut neighbors = pixel_neighbors(skeleton, width, height, current);
        if let Some(pos) = neighbors.iter().position(|&n| n == previous) {
            neighbors.remove(pos);
        }
        if neighbors.len() != 1 {
            break;
        }
        previous = current;
        current = neighbors[0];
        farthest = current;
    }
    let direction = PointF {
        x: (endpoint % width as usize) as f32 - (farthest % width as usize) as f32,
        y: (endpoint / width as usize) as f32 - (farthest / width as usize) as f32,
    };
    let length = (direction.x * direction.x + direction.y * direction.y).sqrt();
    if length >= 1.5 {
        PointF {
            x: direction.x / length,
            y: direction.y / length,
        }
    } else {
        PointF { x: 0.0, y: 0.0 }
    }
}

/// 移除弱单像素桥（对应 C++ RemoveWeakOnePixelBridges）
fn remove_weak_one_pixel_bridges(mask: &mut [u8], response: &[u8], width: u32, height: u32) {
    let support = dilate_square(mask, width, height, 1);
    let low_response = response_percentile(response, &support, 0.08);
    let maximum_bridge_response = 3.max(low_response as i32 / 2);
    const RING: [(i32, i32); 8] = [
        (0, -1),
        (1, -1),
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
        (-1, 0),
        (-1, -1),
    ];
    let w = width as i32;
    let h = height as i32;
    for _ in 0..2 {
        let mut remove: Vec<usize> = Vec::new();
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let index = (y * w + x) as usize;
                if mask[index] == 0 || response[index] as i32 > maximum_bridge_response {
                    continue;
                }
                let mut occupied = [0u8; 8];
                let mut maximum_neighbor_response = 0i32;
                for (neighbor, &(ox, oy)) in RING.iter().enumerate() {
                    let sample = ((y + oy) * w + (x + ox)) as usize;
                    occupied[neighbor] = if mask[sample] != 0 { 1 } else { 0 };
                    maximum_neighbor_response =
                        maximum_neighbor_response.max(response[sample] as i32);
                }
                let mut groups = 0;
                let mut neighbors = 0;
                for n in 0..8 {
                    neighbors += occupied[n] as i32;
                    groups += if occupied[n] != 0 && occupied[(n + 7) % 8] == 0 {
                        1
                    } else {
                        0
                    };
                }
                if neighbors >= 2
                    && groups >= 2
                    && maximum_neighbor_response >= response[index] as i32 + 16
                {
                    remove.push(index);
                }
            }
        }
        if remove.is_empty() {
            break;
        }
        for &index in &remove {
            mask[index] = 0;
        }
    }
}

/// 方向约束补线（对应 C++ RepairDirectionalGaps）
/// 端点近距离 + 切线对齐（≥0.82）+ 响应走廊支持时用直线桥接
fn repair_directional_gaps(
    mask: &mut [u8],
    response: &[u8],
    width: u32,
    height: u32,
    processing_scale: f32,
) {
    let mut skeleton = mask.to_vec();
    thin_zhang_suen(&mut skeleton, width, height);
    let mut endpoints: Vec<usize> = Vec::new();
    for (index, &s) in skeleton.iter().enumerate() {
        if s != 0 && pixel_neighbors(&skeleton, width, height, index).len() == 1 {
            endpoints.push(index);
        }
    }
    if endpoints.len() < 2 {
        return;
    }

    let w = width as i32;
    let mut endpoint_by_pixel = vec![-1i32; mask.len()];
    let mut outward = vec![PointF { x: 0.0, y: 0.0 }; endpoints.len()];
    let tangent_samples = ((6.0f32 * processing_scale).round() as usize).max(4);
    for (endpoint, &pixel) in endpoints.iter().enumerate() {
        endpoint_by_pixel[pixel] = endpoint as i32;
        outward[endpoint] =
            endpoint_outward_direction(&skeleton, width, height, pixel, tangent_samples);
    }

    let maximum_gap = ((2.5f32 * processing_scale).round() as i32).max(2);
    let response_threshold = 3.max(otsu_threshold(response) / 4);
    let mut used = vec![0u8; endpoints.len()];
    for left in 0..endpoints.len() {
        if used[left] != 0 || (outward[left].x == 0.0 && outward[left].y == 0.0) {
            continue;
        }
        let left_x = (endpoints[left] % w as usize) as i32;
        let left_y = (endpoints[left] / w as usize) as i32;
        let mut best = endpoints.len();
        let mut best_distance = f32::INFINITY;
        for oy in -maximum_gap..=maximum_gap {
            for ox in -maximum_gap..=maximum_gap {
                let x = left_x + ox;
                let y = left_y + oy;
                if x < 0 || y < 0 || x >= w || y >= height as i32 {
                    continue;
                }
                let candidate = endpoint_by_pixel[(y * w + x) as usize];
                if candidate < 0 {
                    continue;
                }
                let right = candidate as usize;
                if right <= left
                    || used[right] != 0
                    || (outward[right].x == 0.0 && outward[right].y == 0.0)
                {
                    continue;
                }
                let delta_x = (x - left_x) as f32;
                let delta_y = (y - left_y) as f32;
                let distance = (delta_x * delta_x + delta_y * delta_y).sqrt();
                if distance < 1.5 || distance > maximum_gap as f32 {
                    continue;
                }
                let toward = PointF {
                    x: delta_x / distance,
                    y: delta_y / distance,
                };
                let left_alignment = outward[left].x * toward.x + outward[left].y * toward.y;
                let right_alignment = -(outward[right].x * toward.x + outward[right].y * toward.y);
                if left_alignment < 0.82 || right_alignment < 0.82 {
                    continue;
                }
                let steps = 2.max((distance * 2.0).ceil() as i32);
                let mut supported = true;
                for step in 1..steps {
                    let ratio = step as f32 / steps as f32;
                    let sample_x = (left_x as f32 + delta_x * ratio).round() as i32;
                    let sample_y = (left_y as f32 + delta_y * ratio).round() as i32;
                    let sample = (sample_y * w + sample_x) as usize;
                    if response[sample] < response_threshold as u8 {
                        supported = false;
                        break;
                    }
                }
                if supported && distance < best_distance {
                    best = right;
                    best_distance = distance;
                }
            }
        }
        if best == endpoints.len() {
            continue;
        }
        let right_x = (endpoints[best] % w as usize) as i32;
        let right_y = (endpoints[best] / w as usize) as i32;
        let steps = (right_x - left_x).abs().max((right_y - left_y).abs());
        for step in 0..=steps {
            let ratio = if steps == 0 {
                0.0
            } else {
                step as f32 / steps as f32
            };
            let x = (left_x as f32 + (right_x - left_x) as f32 * ratio).round() as i32;
            let y = (left_y as f32 + (right_y - left_y) as f32 * ratio).round() as i32;
            mask[(y * w + x) as usize] = 1;
        }
        used[left] = 1;
        used[best] = 1;
    }
}

/// 规范线稿提取（对应 C++ ExtractCanonicalLineArt）
/// 输出：灰度覆盖线稿 + 二值拓扑图 + 像素证据
pub fn extract_canonical_line_art(
    image: &DecodedImage,
    grayscale: &[u8],
    width: u32,
    height: u32,
    processing_scale: f32,
) -> CanonicalLineArtExtraction {
    let mut mask = vec![0u8; grayscale.len()];
    let ps_response = build_ps_line_response(grayscale, width, height, processing_scale);
    let mut response = ps_response.fused.clone();
    let mut evidence = vec![LinePixelEvidence::default(); grayscale.len()];
    for (index, ev) in evidence.iter_mut().enumerate() {
        let support_threshold = 4.max((response[index] as f32 * 0.40).round() as i32);
        for scale in 0..3 {
            if ps_response.scales[scale][index] >= support_threshold as u8 {
                ev.scale_mask |= 1 << scale;
            }
        }
    }
    let fine = gaussian_blur(grayscale, width, height, 0.65);
    let monochrome_line_drawing = is_likely_monochrome_line_drawing(image, grayscale);
    let flat_color_illustration =
        !monochrome_line_drawing && is_likely_flat_color_illustration(image, grayscale);
    if monochrome_line_drawing {
        let local_mean = gaussian_blur(grayscale, width, height, 4.5);
        let global_threshold = otsu_threshold(grayscale).clamp(96, 236);
        for (index, _) in grayscale.iter().enumerate() {
            let globally_dark = fine[index] <= global_threshold as f32;
            let locally_dark = fine[index] < 249.0 && local_mean[index] - fine[index] >= 3.5;
            mask[index] = if globally_dark || locally_dark { 1 } else { 0 };
            if mask[index] != 0 {
                evidence[index].flags |= EVIDENCE_ORIGINAL_DARK;
            }
            let original_ink = ((255.0 - fine[index]) * 1.15).round().clamp(0.0, 255.0) as u8;
            response[index] = response[index].max(original_ink);
        }
    } else {
        let local_mean = gaussian_blur(
            grayscale,
            width,
            height,
            if flat_color_illustration { 4.0 } else { 3.2 },
        );
        let mut ridge_response = vec![0u8; grayscale.len()];
        for index in 0..grayscale.len() {
            let dark_ridge = (local_mean[index] - fine[index]).max(0.0);
            ridge_response[index] = (dark_ridge * 6.0).round().clamp(0.0, 255.0) as u8;
            response[index] = response[index].max(ridge_response[index]);
        }
        let ridge_threshold = if flat_color_illustration {
            otsu_threshold(&ridge_response).clamp(8, 32)
        } else {
            otsu_threshold(&ridge_response).clamp(12, 44)
        };
        for index in 0..grayscale.len() {
            mask[index] = if ridge_response[index] >= ridge_threshold as u8 && fine[index] < 249.0 {
                1
            } else {
                0
            };
            if mask[index] != 0 {
                evidence[index].flags |= EVIDENCE_ORIGINAL_DARK;
            }
        }

        let protected_ink = dilate_square(&mask, width, height, 2);
        let color_boundaries = extract_color_boundaries(
            image,
            width,
            height,
            flat_color_illustration,
            processing_scale,
        );
        for index in 0..mask.len() {
            if color_boundaries[index] != 0 && protected_ink[index] == 0 {
                mask[index] = 1;
            }
            if color_boundaries[index] != 0 {
                response[index] = response[index].max(176);
                evidence[index].flags |= EVIDENCE_RGB_BOUNDARY;
            }
        }
    }

    remove_weak_one_pixel_bridges(&mut mask, &response, width, height);
    let mask_before_repair = mask.clone();
    repair_directional_gaps(&mut mask, &response, width, height, processing_scale);
    for index in 0..mask.len() {
        if mask[index] != 0 && mask_before_repair[index] == 0 {
            evidence[index].flags |= EVIDENCE_REPAIRED_GAP;
        }
    }
    remove_small_components(&mut mask, width, height, processing_scale);
    clear_image_border(
        &mut mask,
        width,
        height,
        ((8.0f32 * processing_scale).round() as u32).max(8),
    );

    let strong_threshold = response_percentile(&response, &mask, 0.58);
    for (index, ev) in evidence.iter_mut().enumerate() {
        ev.confidence = response[index];
        if mask[index] != 0 && response[index] >= strong_threshold {
            ev.flags |= EVIDENCE_STRONG_CORE;
        }
    }

    CanonicalLineArtExtraction {
        topology: mask,
        evidence,
    }
}
