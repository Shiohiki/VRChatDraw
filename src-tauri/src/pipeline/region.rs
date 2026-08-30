//! 阶段 4：墨迹距离场 + 局部区域分类
//! 对应 C++ InsideInkDistance / RegionBoundary / BuildRouteSkeleton
//! 质量关键：粗笔画/填充区域识别为 ElongatedThickStroke/CompactFill，
//! 生成时用区域轮廓替代杂乱骨架中轴

use super::skeleton::{masked_pixel_neighbors, pixel_neighbor_mask, thin_zhang_suen};
use super::types::{
    LinePixelEvidence, LineRegionType, RouteSkeletonData, EVIDENCE_ORIGINAL_DARK,
    EVIDENCE_RGB_BOUNDARY, EVIDENCE_STRONG_CORE,
};

/// 墨迹内距离场（对应 C++ InsideInkDistance，双 pass 倒角距离）
fn inside_ink_distance(mask: &[u8], width: u32, height: u32) -> Vec<f32> {
    const DIAGONAL: f32 = std::f32::consts::SQRT_2;
    const INFINITY: f32 = 1.0e6;
    let w = width as usize;
    let mut distance = vec![INFINITY; mask.len()];
    for y in 0..height as usize {
        for x in 0..w {
            let index = y * w + x;
            if mask[index] == 0 {
                distance[index] = 0.0;
            } else if x == 0 || y == 0 || x + 1 == w || y + 1 == height as usize {
                distance[index] = 1.0;
            }
        }
    }
    for y in 0..height as usize {
        for x in 0..w {
            let index = y * w + x;
            if x > 0 {
                distance[index] = distance[index].min(distance[index - 1] + 1.0);
            }
            if y > 0 {
                distance[index] = distance[index].min(distance[index - w] + 1.0);
                if x > 0 {
                    distance[index] = distance[index].min(distance[index - w - 1] + DIAGONAL);
                }
                if x + 1 < w {
                    distance[index] = distance[index].min(distance[index - w + 1] + DIAGONAL);
                }
            }
        }
    }
    for y in (0..height as usize).rev() {
        for x in (0..w).rev() {
            let index = y * w + x;
            if x + 1 < w {
                distance[index] = distance[index].min(distance[index + 1] + 1.0);
            }
            if y + 1 < height as usize {
                distance[index] = distance[index].min(distance[index + w] + 1.0);
                if x > 0 {
                    distance[index] = distance[index].min(distance[index + w - 1] + DIAGONAL);
                }
                if x + 1 < w {
                    distance[index] = distance[index].min(distance[index + w + 1] + DIAGONAL);
                }
            }
        }
    }
    distance
}

/// 区域边界（对应 C++ RegionBoundary）
fn region_boundary(region: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as i32;
    let h = height as i32;
    let mut boundary = vec![0u8; region.len()];
    for y in 0..h {
        for x in 0..w {
            let index = (y * w + x) as usize;
            if region[index] == 0 {
                continue;
            }
            let mut edge = x == 0 || y == 0 || x + 1 == w || y + 1 == h;
            if !edge {
                'outer: for oy in -1..=1 {
                    for ox in -1..=1 {
                        let nx = x + ox;
                        let ny = y + oy;
                        if nx < 0
                            || ny < 0
                            || nx >= w
                            || ny >= h
                            || region[(ny * w + nx) as usize] == 0
                        {
                            edge = true;
                            break 'outer;
                        }
                    }
                }
            }
            boundary[index] = if edge { 1 } else { 0 };
        }
    }
    boundary
}

/// 路由骨架构建（对应 C++ BuildRouteSkeleton）
/// 粗墨迹种子 → 区域扩张 → 端口聚类 → 协方差/包围盒长宽比分类
pub fn build_route_skeleton(
    source_ink: &[u8],
    evidence: &[LinePixelEvidence],
    width: u32,
    height: u32,
    processing_scale: f32,
) -> RouteSkeletonData {
    let mut skeleton = source_ink.to_vec();
    thin_zhang_suen(&mut skeleton, width, height);
    let ink_distance = inside_ink_distance(source_ink, width, height);
    let mut region_types = vec![LineRegionType::ThinLine as u8; source_ink.len()];
    let mut thick_core = vec![0u8; source_ink.len()];
    let seed_radius = (2.75f32 * processing_scale).max(3.5);
    for index in 0..source_ink.len() {
        let reliable = index < evidence.len()
            && ((evidence[index].flags
                & (EVIDENCE_STRONG_CORE | EVIDENCE_ORIGINAL_DARK | EVIDENCE_RGB_BOUNDARY))
                != 0
                || evidence[index].confidence >= 48);
        thick_core[index] =
            if source_ink[index] != 0 && reliable && ink_distance[index] >= seed_radius {
                1
            } else {
                0
            };
    }

    let mut visited_core = vec![0u8; source_ink.len()];
    let mut expansion_distance = vec![-1i32; source_ink.len()];
    let mut region_membership = vec![0u8; source_ink.len()];
    let mut pending: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut core_component: Vec<usize> = Vec::new();
    let mut region: Vec<usize> = Vec::new();
    let mut touched: Vec<usize> = Vec::new();
    let minimum_core_size = (seed_radius * seed_radius * 0.70).ceil() as usize;

    for start in 0..thick_core.len() {
        if thick_core[start] == 0 || visited_core[start] != 0 {
            continue;
        }
        core_component.clear();
        visited_core[start] = 1;
        pending.push_back(start);
        let mut maximum_radius = ink_distance[start];
        while let Some(current) = pending.pop_front() {
            core_component.push(current);
            maximum_radius = maximum_radius.max(ink_distance[current]);
            let x = (current % width as usize) as i32;
            let y = (current / width as usize) as i32;
            for oy in -1..=1 {
                for ox in -1..=1 {
                    let nx = x + ox;
                    let ny = y + oy;
                    if (ox == 0 && oy == 0)
                        || nx < 0
                        || ny < 0
                        || nx >= width as i32
                        || ny >= height as i32
                    {
                        continue;
                    }
                    let neighbor = (ny * width as i32 + nx) as usize;
                    if thick_core[neighbor] != 0 && visited_core[neighbor] == 0 {
                        visited_core[neighbor] = 1;
                        pending.push_back(neighbor);
                    }
                }
            }
        }
        if core_component.len() < minimum_core_size {
            continue;
        }

        region.clear();
        touched.clear();
        let maximum_expansion = (maximum_radius + 2.0 * processing_scale).ceil() as i32;
        for &pixel in &core_component {
            expansion_distance[pixel] = 0;
            touched.push(pixel);
            pending.push_back(pixel);
        }
        while let Some(current) = pending.pop_front() {
            region.push(current);
            if expansion_distance[current] >= maximum_expansion {
                continue;
            }
            let x = (current % width as usize) as i32;
            let y = (current / width as usize) as i32;
            for oy in -1..=1 {
                for ox in -1..=1 {
                    let nx = x + ox;
                    let ny = y + oy;
                    if (ox == 0 && oy == 0)
                        || nx < 0
                        || ny < 0
                        || nx >= width as i32
                        || ny >= height as i32
                    {
                        continue;
                    }
                    let neighbor = (ny * width as i32 + nx) as usize;
                    if source_ink[neighbor] != 0 && expansion_distance[neighbor] < 0 {
                        expansion_distance[neighbor] = expansion_distance[current] + 1;
                        touched.push(neighbor);
                        pending.push_back(neighbor);
                    }
                }
            }
        }
        for &pixel in &touched {
            expansion_distance[pixel] = -1;
        }
        if region.is_empty() {
            continue;
        }
        for &pixel in &region {
            region_membership[pixel] = 1;
        }

        let mut mean_x = 0.0f64;
        let mut mean_y = 0.0f64;
        let mut min_x = width as i32;
        let mut max_x = 0i32;
        let mut min_y = height as i32;
        let mut max_y = 0i32;
        let mut skeleton_pixels = 0usize;
        let mut port_pixels: Vec<usize> = Vec::new();
        for &pixel in &region {
            let x = (pixel % width as usize) as i32;
            let y = (pixel / width as usize) as i32;
            mean_x += x as f64;
            mean_y += y as f64;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            skeleton_pixels += (skeleton[pixel] != 0) as usize;
            if skeleton[pixel] == 0 {
                continue;
            }
            // 热路径：用掩码迭代器避免逐像素分配 Vec（与 pixel_neighbors 语义一致）
            let mask = pixel_neighbor_mask(&skeleton, width, height, pixel);
            for neighbor in masked_pixel_neighbors(mask, width, pixel) {
                if region_membership[neighbor] == 0 {
                    port_pixels.push(neighbor);
                }
            }
        }
        port_pixels.sort_unstable();
        port_pixels.dedup();
        let mut visited_ports = vec![0u8; port_pixels.len()];
        let mut port_count = 0usize;
        let mut port_centroids: Vec<(f32, f32)> = Vec::new();
        let port_merge_distance = ((2.0f32 * processing_scale).round() as i32).max(2);
        for port in 0..port_pixels.len() {
            if visited_ports[port] != 0 {
                continue;
            }
            port_count += 1;
            visited_ports[port] = 1;
            let mut port_queue: std::collections::VecDeque<usize> =
                std::collections::VecDeque::new();
            port_queue.push_back(port);
            let mut port_sum_x = 0.0f64;
            let mut port_sum_y = 0.0f64;
            let mut port_pixel_count = 0usize;
            while let Some(current) = port_queue.pop_front() {
                let current_x = (port_pixels[current] % width as usize) as i32;
                let current_y = (port_pixels[current] / width as usize) as i32;
                port_sum_x += current_x as f64;
                port_sum_y += current_y as f64;
                port_pixel_count += 1;
                for candidate in 0..port_pixels.len() {
                    if visited_ports[candidate] != 0 {
                        continue;
                    }
                    let candidate_x = (port_pixels[candidate] % width as usize) as i32;
                    let candidate_y = (port_pixels[candidate] / width as usize) as i32;
                    if (candidate_x - current_x)
                        .abs()
                        .max((candidate_y - current_y).abs())
                        <= port_merge_distance
                    {
                        visited_ports[candidate] = 1;
                        port_queue.push_back(candidate);
                    }
                }
            }
            port_centroids.push((
                (port_sum_x / port_pixel_count as f64) as f32,
                (port_sum_y / port_pixel_count as f64) as f32,
            ));
        }
        mean_x /= region.len() as f64;
        mean_y /= region.len() as f64;
        let mut xx = 0.0f64;
        let mut yy = 0.0f64;
        let mut xy = 0.0f64;
        for &pixel in &region {
            let dx = (pixel % width as usize) as f64 - mean_x;
            let dy = (pixel / width as usize) as f64 - mean_y;
            xx += dx * dx;
            yy += dy * dy;
            xy += dx * dy;
        }
        let discriminant = (((xx - yy) * (xx - yy) + 4.0 * xy * xy).max(0.0)).sqrt();
        let major = ((xx + yy + discriminant) * 0.5).max(1.0);
        let minor = ((xx + yy - discriminant) * 0.5).max(1.0);
        let covariance_aspect = (major / minor).sqrt();
        let box_long = ((max_x - min_x + 1).max(max_y - min_y + 1)) as f64;
        let box_short = ((max_x - min_x + 1).min(max_y - min_y + 1)).max(1) as f64;
        let box_aspect = box_long / box_short;
        let loop_like_stroke =
            port_count <= 2 && skeleton_pixels as f64 >= (region.len() as f64).sqrt() * 2.25;
        let mut two_port_continuation = false;
        if port_centroids.len() == 2 {
            let (lx, ly) = (
                port_centroids[0].0 - mean_x as f32,
                port_centroids[0].1 - mean_y as f32,
            );
            let (rx, ry) = (
                port_centroids[1].0 - mean_x as f32,
                port_centroids[1].1 - mean_y as f32,
            );
            let denominator = (lx * lx + ly * ly).sqrt() * (rx * rx + ry * ry).sqrt();
            two_port_continuation =
                denominator > 1.0e-5 && (lx * rx + ly * ry) / denominator <= -0.35;
        }
        let elongated = covariance_aspect >= 4.0
            || box_aspect >= 5.0
            || loop_like_stroke
            || two_port_continuation;
        let region_type = if port_count >= 3 {
            LineRegionType::Junction
        } else if elongated {
            LineRegionType::ElongatedThickStroke
        } else {
            LineRegionType::CompactFill
        };
        for &pixel in &region {
            let existing = LineRegionType::from_u8(region_types[pixel]);
            let replace = region_type == LineRegionType::Junction
                || (region_type == LineRegionType::ElongatedThickStroke
                    && (existing == LineRegionType::ThinLine
                        || existing == LineRegionType::CompactFill))
                || (region_type == LineRegionType::CompactFill
                    && existing == LineRegionType::ThinLine);
            if replace {
                region_types[pixel] = region_type as u8;
            }
            region_membership[pixel] = 0;
        }
    }

    let mut compact_region = vec![0u8; source_ink.len()];
    for (pixel, &t) in region_types.iter().enumerate() {
        compact_region[pixel] = if t == LineRegionType::CompactFill as u8 {
            1
        } else {
            0
        };
    }
    let mut compact_boundary = region_boundary(&compact_region, width, height);
    thin_zhang_suen(&mut compact_boundary, width, height);
    for index in 0..skeleton.len() {
        if compact_region[index] != 0 {
            skeleton[index] = 0;
        }
        skeleton[index] = if skeleton[index] != 0 || compact_boundary[index] != 0 {
            1
        } else {
            0
        };
    }

    RouteSkeletonData {
        skeleton,
        region_types,
        ink_distance,
    }
}
