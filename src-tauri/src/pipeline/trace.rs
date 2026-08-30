//! 阶段 5：拓扑图追踪（对应 C++ TraceSkeleton）
//! 节点（交叉区/端点）→ 图边追踪 → 闭环 → 毛刺判定 → 交叉端口配对 → 连接器（射线交点/测地线）→ 墨迹约束 RDP → 组件覆盖补漏 → 最近端点排序 → 跨度元数据
use std::collections::{HashSet, VecDeque};

use super::path_math::{simplify_rdp, stroke_length};
use super::skeleton::{masked_pixel_neighbors, pixel_neighbor_mask};
use super::types::{
    LinePixelEvidence, LineRegionType, PointF, RouteSpan, Stroke, StrokeRouteMetadata,
    ANCHOR_CLOSED_SEAM, ANCHOR_GRAPH_ENDPOINT, ANCHOR_JUNCTION_PORT, ANCHOR_NONE,
    ANCHOR_REGION_BOUNDARY, EVIDENCE_ORIGINAL_DARK, EVIDENCE_REPAIRED_GAP, EVIDENCE_RGB_BOUNDARY,
};

struct SkeletonNode {
    pixels: Vec<usize>,
    point: PointF,
    junction: bool,
}

struct GraphEdge {
    left: usize,
    right: usize,
    points: Stroke,
    length: f32,
    confidence: f32,
    low_confidence_ratio: f32,
    endpoint_confidence: f32,
    multi_scale_support: f32,
    original_support: f32,
    repaired_support: f32,
}

struct JunctionPort {
    point: PointF,
    outward: PointF,
}

#[inline]
fn edge_key(left: usize, right: usize) -> (usize, usize) {
    // 用元组做 key：避免 u64 拼接时的 as u32 截断（大图索引超 2^32 时哈希碰撞）
    (left.min(right), left.max(right))
}

#[inline]
fn point_pixel(point: PointF, width: u32, height: u32) -> usize {
    let x = (point.x.round() as i32).clamp(0, width as i32 - 1);
    let y = (point.y.round() as i32).clamp(0, height as i32 - 1);
    (y as usize) * width as usize + x as usize
}

/// 节点像素集合到候选点的最近欧氏距离。
/// 注：早期版本用 `min(hypot(min(prev, dx), dy), prev)` 链式写法，
/// 经推演与随机验证它与直接 `min(prev, hypot(dx, dy))` 数学等价
/// （dx > prev 时 hypot(prev, dy) ≥ prev 使结果保持 prev），此处改为
/// 清晰实现，行为不变。
fn nearest_node_pixel_distance(pixels: &[usize], width: u32, candidate: PointF) -> f32 {
    pixels
        .iter()
        .map(|&pixel| {
            let pixel_x = (pixel % width as usize) as f32;
            let pixel_y = (pixel / width as usize) as f32;
            (pixel_x - candidate.x).hypot(pixel_y - candidate.y)
        })
        .fold(f32::INFINITY, f32::min)
}

/// 拓扑追踪（对应 C++ TraceSkeleton）
#[allow(clippy::too_many_arguments)]
pub fn trace_skeleton(
    skeleton: &[u8],
    width: u32,
    height: u32,
    source_ink: &[u8],
    evidence: &[LinePixelEvidence],
    region_types: &[u8],
    ink_distance: &[f32],
    processing_scale: f32,
    epsilon_ratio: f32,
) -> (Vec<Stroke>, Vec<StrokeRouteMetadata>) {
    let w = width as usize;
    let missing = usize::MAX;
    let mut pixel_adjacency = vec![0u8; skeleton.len()];
    let mut pixel_edge_count = 0usize;
    for index in 0..skeleton.len() {
        if skeleton[index] == 0 {
            continue;
        }
        pixel_adjacency[index] = pixel_neighbor_mask(skeleton, width, height, index);
        pixel_edge_count += pixel_adjacency[index].count_ones() as usize;
    }

    let mut node_by_pixel = vec![missing; skeleton.len()];
    let mut nodes: Vec<SkeletonNode> = Vec::new();
    let mut graph_adjacency: Vec<Vec<usize>> = Vec::new();
    let mut graph_edges: Vec<GraphEdge> = Vec::new();
    let mut visited_edges: HashSet<(usize, usize)> =
        HashSet::with_capacity(pixel_edge_count / 2 + 1);

    // 交叉核心区：高自由度像素 + 同 Junction 区域的二度像素（吸收分裂核心）
    let mut junction_core = vec![0u8; skeleton.len()];
    for index in 0..skeleton.len() {
        junction_core[index] = if pixel_adjacency[index].count_ones() >= 3 {
            1
        } else {
            0
        };
    }
    let mut junction_zone = junction_core.clone();
    for index in 0..skeleton.len() {
        if skeleton[index] != 0
            && index < region_types.len()
            && region_types[index] == LineRegionType::Junction as u8
            && pixel_adjacency[index].count_ones() >= 2
        {
            junction_zone[index] = 1;
        }
        if junction_core[index] == 0 {
            continue;
        }
        for neighbor in masked_pixel_neighbors(pixel_adjacency[index], width, index) {
            if pixel_adjacency[neighbor].count_ones() >= 2 {
                junction_zone[neighbor] = 1;
            }
        }
    }
    let mut visited_candidates = vec![0u8; skeleton.len()];
    for start in 0..skeleton.len() {
        if junction_zone[start] == 0 || visited_candidates[start] != 0 {
            continue;
        }
        let mut region: Vec<usize> = Vec::new();
        let mut pending: VecDeque<usize> = VecDeque::new();
        visited_candidates[start] = 1;
        pending.push_back(start);
        while let Some(current) = pending.pop_front() {
            region.push(current);
            for neighbor in masked_pixel_neighbors(pixel_adjacency[current], width, current) {
                if junction_zone[neighbor] != 0 && visited_candidates[neighbor] == 0 {
                    visited_candidates[neighbor] = 1;
                    pending.push_back(neighbor);
                }
            }
        }
        if !region.iter().any(|&p| junction_core[p] != 0) {
            continue;
        }
        let node_index = nodes.len();
        let mut sum_x = 0.0f64;
        let mut sum_y = 0.0f64;
        for &pixel in &region {
            node_by_pixel[pixel] = node_index;
            sum_x += (pixel % w) as f64;
            sum_y += (pixel / w) as f64;
        }
        let count = region.len().max(1) as f64;
        let centroid = PointF {
            x: (sum_x / count) as f32,
            y: (sum_y / count) as f32,
        };
        let representative = *region
            .iter()
            .min_by(|&&a, &&b| {
                let da = ((a % w) as f32 - centroid.x).hypot((a / w) as f32 - centroid.y);
                let db = ((b % w) as f32 - centroid.x).hypot((b / w) as f32 - centroid.y);
                da.total_cmp(&db)
            })
            .unwrap();
        nodes.push(SkeletonNode {
            pixels: region,
            point: PointF {
                x: (representative % w) as f32,
                y: (representative / w) as f32,
            },
            junction: true,
        });
        graph_adjacency.push(Vec::new());
    }

    // 端点/孤立像素节点
    for pixel in 0..skeleton.len() {
        if skeleton[pixel] == 0
            || node_by_pixel[pixel] != missing
            || pixel_adjacency[pixel].count_ones() == 2
        {
            continue;
        }
        let node_index = nodes.len();
        node_by_pixel[pixel] = node_index;
        nodes.push(SkeletonNode {
            pixels: vec![pixel],
            point: PointF {
                x: (pixel % w) as f32,
                y: (pixel / w) as f32,
            },
            junction: pixel_adjacency[pixel].count_ones() >= 3,
        });
        graph_adjacency.push(Vec::new());
    }

    // 节点内部边标记为已访问（几何噪声）
    for pixel in 0..skeleton.len() {
        if node_by_pixel[pixel] == missing {
            continue;
        }
        for neighbor in masked_pixel_neighbors(pixel_adjacency[pixel], width, pixel) {
            if node_by_pixel[neighbor] == node_by_pixel[pixel] {
                visited_edges.insert(edge_key(pixel, neighbor));
            }
        }
    }

    // 图边追踪（保留节点边界处的真实骨架像素，而非质心弧）
    let append_graph_edge = |left: usize,
                             start_pixel: usize,
                             first_neighbor: usize,
                             node_by_pixel: &[usize],
                             pixel_adjacency: &[u8],
                             visited_edges: &mut HashSet<(usize, usize)>,
                             graph_edges: &mut Vec<GraphEdge>,
                             graph_adjacency: &mut Vec<Vec<usize>>,
                             width: u32,
                             height: u32,
                             evidence: &[LinePixelEvidence]| {
        let mut points: Stroke = vec![PointF {
            x: (start_pixel % w) as f32,
            y: (start_pixel / w) as f32,
        }];
        let mut previous = start_pixel;
        let mut current = first_neighbor;
        visited_edges.insert(edge_key(previous, current));
        let mut right = missing;
        loop {
            if node_by_pixel[current] != missing {
                right = node_by_pixel[current];
                points.push(PointF {
                    x: (current % w) as f32,
                    y: (current / w) as f32,
                });
                break;
            }
            points.push(PointF {
                x: (current % w) as f32,
                y: (current / w) as f32,
            });
            if pixel_adjacency[current].count_ones() != 2 {
                break;
            }
            let mut neighbors = masked_pixel_neighbors(pixel_adjacency[current], width, current);
            let first = neighbors.next().unwrap();
            let second = neighbors.next().unwrap();
            let next = if first == previous { second } else { first };
            if visited_edges.contains(&edge_key(current, next)) {
                break;
            }
            visited_edges.insert(edge_key(current, next));
            previous = current;
            current = next;
        }
        if right == missing || points.len() < 2 {
            return;
        }
        let edge_index = graph_edges.len();
        let mut edge = GraphEdge {
            left,
            right,
            points: points.clone(),
            length: 0.0,
            confidence: 0.0,
            low_confidence_ratio: 0.0,
            endpoint_confidence: 0.0,
            multi_scale_support: 0.0,
            original_support: 0.0,
            repaired_support: 0.0,
        };
        edge.length = stroke_length(&edge.points);
        let mut confidence_sum = 0.0f64;
        let mut multi_scale_points = 0usize;
        let mut original_points = 0usize;
        let mut repaired_points = 0usize;
        let mut low_confidence_points = 0usize;
        for &point in &edge.points {
            let pixel = point_pixel(point, width, height);
            if pixel < evidence.len() {
                confidence_sum += evidence[pixel].confidence as f64;
                low_confidence_points += (evidence[pixel].confidence < 64) as usize;
                multi_scale_points += (evidence[pixel].scale_mask.count_ones() >= 2) as usize;
                original_points += ((evidence[pixel].flags
                    & (EVIDENCE_ORIGINAL_DARK | EVIDENCE_RGB_BOUNDARY))
                    != 0) as usize;
                repaired_points += ((evidence[pixel].flags & EVIDENCE_REPAIRED_GAP) != 0) as usize;
            }
        }
        let point_count = edge.points.len().max(1) as f32;
        edge.confidence = (confidence_sum / (255.0 * point_count as f64)) as f32;
        edge.low_confidence_ratio = low_confidence_points as f32 / point_count;
        let endpoint_conf = |point: PointF| -> f32 {
            let pixel = point_pixel(point, width, height);
            if pixel < evidence.len() {
                evidence[pixel].confidence as f32 / 255.0
            } else {
                0.0
            }
        };
        edge.endpoint_confidence =
            endpoint_conf(edge.points[0]).min(endpoint_conf(*edge.points.last().unwrap()));
        edge.multi_scale_support = multi_scale_points as f32 / point_count;
        edge.original_support = original_points as f32 / point_count;
        edge.repaired_support = repaired_points as f32 / point_count;
        graph_adjacency[left].push(edge_index);
        if right != left {
            graph_adjacency[right].push(edge_index);
        }
        graph_edges.push(edge);
    };

    for (node_index, node) in nodes.iter().enumerate() {
        for &pixel in &node.pixels {
            for neighbor in masked_pixel_neighbors(pixel_adjacency[pixel], width, pixel) {
                if node_by_pixel[neighbor] != node_index
                    && !visited_edges.contains(&edge_key(pixel, neighbor))
                {
                    append_graph_edge(
                        node_index,
                        pixel,
                        neighbor,
                        &node_by_pixel,
                        &pixel_adjacency,
                        &mut visited_edges,
                        &mut graph_edges,
                        &mut graph_adjacency,
                        width,
                        height,
                        evidence,
                    );
                }
            }
        }
    }

    // 闭环：显式追踪剩余像素边（环形/封闭细节）
    let mut closed_strokes: Vec<Stroke> = Vec::new();
    for pixel in 0..skeleton.len() {
        if skeleton[pixel] == 0 {
            continue;
        }
        for neighbor in masked_pixel_neighbors(pixel_adjacency[pixel], width, pixel) {
            if visited_edges.contains(&edge_key(pixel, neighbor)) {
                continue;
            }
            let mut loop_stroke: Stroke = vec![PointF {
                x: (pixel % w) as f32,
                y: (pixel / w) as f32,
            }];
            let mut previous = pixel;
            let mut current = neighbor;
            visited_edges.insert(edge_key(previous, current));
            while current != pixel {
                loop_stroke.push(PointF {
                    x: (current % w) as f32,
                    y: (current / w) as f32,
                });
                if pixel_adjacency[current].count_ones() != 2 {
                    break;
                }
                let mut neighbors =
                    masked_pixel_neighbors(pixel_adjacency[current], width, current);
                let first = neighbors.next().unwrap();
                let second = neighbors.next().unwrap();
                let next = if first == previous { second } else { first };
                if visited_edges.contains(&edge_key(current, next)) {
                    break;
                }
                visited_edges.insert(edge_key(current, next));
                previous = current;
                current = next;
            }
            if current == pixel {
                loop_stroke.push(loop_stroke[0]);
            }
            if loop_stroke.len() >= 2 {
                closed_strokes.push(loop_stroke);
            }
        }
    }

    // ===== 墨迹支持 =====
    const INK_SUPPORT_RADIUS: i32 = 1;
    let supported_by_ink = |point: PointF| -> bool {
        let cx = point.x.round() as i32;
        let cy = point.y.round() as i32;
        for oy in -INK_SUPPORT_RADIUS..=INK_SUPPORT_RADIUS {
            for ox in -INK_SUPPORT_RADIUS..=INK_SUPPORT_RADIUS {
                let x = cx + ox;
                let y = cy + oy;
                if x >= 0
                    && y >= 0
                    && x < width as i32
                    && y < height as i32
                    && source_ink[(y as usize) * w + x as usize] != 0
                {
                    return true;
                }
            }
        }
        false
    };
    let segment_supported_by_ink = |start: PointF, end: PointF| -> bool {
        let distance = ((end.x - start.x).powi(2) + (end.y - start.y).powi(2)).sqrt();
        if !distance.is_finite() {
            return false;
        }
        // 防御：异常大距离的 `as i32` 饱和会形成约 21 亿次迭代（正常管线坐标有界，不可达）
        let steps = ((distance * 2.0).ceil() as i64).clamp(1, 20_000) as i32;
        for step in 0..=steps {
            let ratio = step as f32 / steps as f32;
            if !supported_by_ink(PointF {
                x: start.x + (end.x - start.x) * ratio,
                y: start.y + (end.y - start.y) * ratio,
            }) {
                return false;
            }
        }
        true
    };
    let stroke_supported_by_ink = |stroke: &Stroke| -> bool {
        stroke.len() >= 2
            && (1..stroke.len()).all(|i| segment_supported_by_ink(stroke[i - 1], stroke[i]))
    };
    let simplify_within_ink = |stroke: Stroke| -> Stroke {
        if stroke.len() <= 2 {
            return stroke;
        }
        // 1.5 is the existing UI/default value.  Normalize around it so that
        // wiring the setting into RDP does not change the default output.
        let epsilon_scale = (epsilon_ratio / 1.5).clamp(0.1 / 1.5, 10.0 / 1.5);
        let simplified = simplify_rdp(&stroke, (0.35 * epsilon_scale * processing_scale).max(0.01));
        if stroke_supported_by_ink(&simplified) {
            simplified
        } else {
            stroke
        }
    };
    let local_ink_radius = |point: PointF| -> f32 {
        let cx = point.x.round() as i32;
        let cy = point.y.round() as i32;
        if cx >= 0 && cy >= 0 && cx < width as i32 && cy < height as i32 {
            let index = cy as usize * w + cx as usize;
            if index < ink_distance.len() && ink_distance[index] < 1.0e5 {
                return ink_distance[index].max(1.0);
            }
        }
        let maximum_radius = ((16.0f32 * processing_scale).round() as i32).max(1);
        for radius in 1..=maximum_radius {
            for offset in -radius..=radius {
                for (x, y) in [
                    (cx + offset, cy - radius),
                    (cx + offset, cy + radius),
                    (cx - radius, cy + offset),
                    (cx + radius, cy + offset),
                ] {
                    if x < 0
                        || y < 0
                        || x >= width as i32
                        || y >= height as i32
                        || source_ink[(y as usize) * w + x as usize] == 0
                    {
                        return radius as f32;
                    }
                }
            }
        }
        maximum_radius as f32
    };

    // 孤立节点：在源墨迹内找最长的微型线段
    let mut isolated_strokes: Vec<Stroke> = Vec::new();
    for node in 0..nodes.len() {
        if !graph_adjacency[node].is_empty() || nodes[node].pixels.is_empty() {
            continue;
        }
        let start = nodes[node].point;
        let mut end = start;
        let mut best_length = 0.0f32;
        let cx = start.x.round() as i32;
        let cy = start.y.round() as i32;
        let search_radius = ((4.0f32 * processing_scale).round() as i32).max(1);
        for oy in -search_radius..=search_radius {
            for ox in -search_radius..=search_radius {
                let x = cx + ox;
                let y = cy + oy;
                if x < 0
                    || y < 0
                    || x >= width as i32
                    || y >= height as i32
                    || source_ink[(y as usize) * w + x as usize] == 0
                {
                    continue;
                }
                let candidate = PointF {
                    x: x as f32,
                    y: y as f32,
                };
                let length = (candidate.x - start.x).hypot(candidate.y - start.y);
                if length > best_length && segment_supported_by_ink(start, candidate) {
                    best_length = length;
                    end = candidate;
                }
            }
        }
        // 找不到有效候选（孤立噪点）时不产出零长度笔画；
        // 该墨迹组件若未被覆盖，会由下方"组件覆盖补漏"生成穿过组件的最长线段
        if end != start {
            isolated_strokes.push(vec![start, end]);
        }
    }

    // ===== 毛刺判定（几何 + 证据双重条件） =====
    let mut removed_edge = vec![0u8; graph_edges.len()];
    let direction_from_node = |edge: &GraphEdge, node: usize| -> PointF {
        let port = if edge.left == node {
            edge.points[0]
        } else {
            *edge.points.last().unwrap()
        };
        let sample_distance = ((8.0f32 * processing_scale).round() as usize).max(1);
        let last = edge.points.len() - 1;
        let sample = if edge.left == node {
            edge.points[last.min(sample_distance)]
        } else {
            edge.points[last - last.min(sample_distance)]
        };
        let x = sample.x - port.x;
        let y = sample.y - port.y;
        let length = x.hypot(y);
        if length > 1.0e-5 {
            PointF {
                x: x / length,
                y: y / length,
            }
        } else {
            PointF { x: 0.0, y: 0.0 }
        }
    };

    // 终端边仅在几何与证据都判定为骨架伪影时移除；强原始短线受保护
    for node in 0..nodes.len() {
        if !nodes[node].junction || graph_adjacency[node].len() < 3 {
            continue;
        }
        let maximum_artifact_length = (local_ink_radius(nodes[node].point) * 2.25
            + processing_scale)
            .clamp(3.0 * processing_scale, 12.0 * processing_scale);
        for &candidate_index in &graph_adjacency[node] {
            if removed_edge[candidate_index] != 0 {
                continue;
            }
            let candidate = &graph_edges[candidate_index];
            if candidate.left == candidate.right || candidate.length > maximum_artifact_length {
                continue;
            }
            let other_node = if candidate.left == node {
                candidate.right
            } else {
                candidate.left
            };
            if other_node == node
                || nodes[other_node].junction
                || graph_adjacency[other_node].len() != 1
            {
                continue;
            }
            let mut others: Vec<usize> = Vec::new();
            let mut other_lengths: Vec<f32> = Vec::new();
            for &edge_index in &graph_adjacency[node] {
                if edge_index != candidate_index && removed_edge[edge_index] == 0 {
                    others.push(edge_index);
                    other_lengths.push(graph_edges[edge_index].length);
                }
            }
            if others.len() < 2 {
                continue;
            }
            other_lengths.sort_by(f32::total_cmp);
            let median_length = other_lengths[other_lengths.len() / 2];
            if median_length < (maximum_artifact_length * 1.25).max(candidate.length * 2.25) {
                continue;
            }
            let weak_evidence = (candidate.low_confidence_ratio > 0.60
                && candidate.endpoint_confidence < 0.42
                && candidate.multi_scale_support < 0.24
                && candidate.original_support < 0.24
                && candidate.confidence < 0.42)
                || (candidate.repaired_support > 0.60 && candidate.original_support < 0.18);
            if !weak_evidence {
                continue;
            }
            let short_direction = direction_from_node(candidate, node);
            let mut matches_false_bisector = false;
            'outer: for left in 0..others.len() {
                let left_direction = direction_from_node(&graph_edges[others[left]], node);
                for right in left + 1..others.len() {
                    let right_direction = direction_from_node(&graph_edges[others[right]], node);
                    let pair_cosine =
                        left_direction.x * right_direction.x + left_direction.y * right_direction.y;
                    let sum_x = left_direction.x + right_direction.x;
                    let sum_y = left_direction.y + right_direction.y;
                    let sum_length = sum_x.hypot(sum_y);
                    if pair_cosine <= -0.9 || sum_length <= 0.25 {
                        continue;
                    }
                    let opposite_bisector =
                        -(short_direction.x * sum_x + short_direction.y * sum_y) / sum_length;
                    if opposite_bisector >= 0.72 {
                        matches_false_bisector = true;
                        break 'outer;
                    }
                }
            }
            if matches_false_bisector {
                removed_edge[candidate_index] = 1;
            }
        }
    }

    // ===== 交叉端口配对 + 连接器 =====
    let mut junction_connectors: Vec<Stroke> = Vec::new();
    let mut predecessor = vec![missing; skeleton.len()];
    let mut touched_predecessors: Vec<usize> = Vec::new();
    let shortest_node_path = |node: usize,
                              from: PointF,
                              to: PointF,
                              node_by_pixel: &[usize],
                              pixel_adjacency: &[u8],
                              width: u32,
                              predecessor: &mut Vec<usize>,
                              touched: &mut Vec<usize>|
     -> Stroke {
        let start = point_pixel(from, width, height);
        let target = point_pixel(to, width, height);
        let mut path: Stroke = Vec::new();
        if node_by_pixel[start] != node || node_by_pixel[target] != node {
            return path;
        }
        let mut pending: VecDeque<usize> = VecDeque::new();
        predecessor[start] = start;
        touched.push(start);
        pending.push_back(start);
        while !pending.is_empty() && predecessor[target] == missing {
            let current = pending.pop_front().unwrap();
            for neighbor in masked_pixel_neighbors(pixel_adjacency[current], width, current) {
                if node_by_pixel[neighbor] == node && predecessor[neighbor] == missing {
                    predecessor[neighbor] = current;
                    touched.push(neighbor);
                    pending.push_back(neighbor);
                }
            }
        }
        if predecessor[target] != missing {
            let mut current = target;
            loop {
                path.push(PointF {
                    x: (current % width as usize) as f32,
                    y: (current / width as usize) as f32,
                });
                if current == start {
                    break;
                }
                current = predecessor[current];
            }
            path.reverse();
        }
        for &pixel in touched.iter() {
            predecessor[pixel] = missing;
        }
        touched.clear();
        path
    };
    let valid_ray_intersection = |node: usize,
                                  left: &JunctionPort,
                                  right: &JunctionPort,
                                  nodes: &[SkeletonNode],
                                  width: u32,
                                  processing_scale: f32|
     -> Option<PointF> {
        let left_inward = PointF {
            x: -left.outward.x,
            y: -left.outward.y,
        };
        let right_inward = PointF {
            x: -right.outward.x,
            y: -right.outward.y,
        };
        let cross = left_inward.x * right_inward.y - left_inward.y * right_inward.x;
        if cross.abs() < 0.08 {
            return None;
        }
        let delta = PointF {
            x: right.point.x - left.point.x,
            y: right.point.y - left.point.y,
        };
        let left_distance = (delta.x * right_inward.y - delta.y * right_inward.x) / cross;
        let right_distance = (delta.x * left_inward.y - delta.y * left_inward.x) / cross;
        let maximum_travel =
            (local_ink_radius(nodes[node].point) * 3.0 + 2.0 * processing_scale).max(3.0);
        if left_distance < -0.25
            || right_distance < -0.25
            || left_distance > maximum_travel
            || right_distance > maximum_travel
        {
            return None;
        }
        let candidate = PointF {
            x: left.point.x + left_inward.x * left_distance,
            y: left.point.y + left_inward.y * left_distance,
        };
        let nearest_node_pixel = nearest_node_pixel_distance(&nodes[node].pixels, width, candidate);
        if nearest_node_pixel > (1.5f32).max(processing_scale)
            || !supported_by_ink(candidate)
            || !segment_supported_by_ink(left.point, candidate)
            || !segment_supported_by_ink(candidate, right.point)
        {
            return None;
        }
        Some(candidate)
    };

    for node in 0..nodes.len() {
        if !nodes[node].junction {
            continue;
        }
        let mut ports: Vec<JunctionPort> = Vec::new();
        for &edge_index in &graph_adjacency[node] {
            if removed_edge[edge_index] != 0 {
                continue;
            }
            let edge = &graph_edges[edge_index];
            if edge.left == edge.right || (edge.left != node && edge.right != node) {
                continue;
            }
            let at_left = edge.left == node;
            ports.push(JunctionPort {
                point: if at_left {
                    edge.points[0]
                } else {
                    *edge.points.last().unwrap()
                },
                outward: direction_from_node(edge, node),
            });
        }
        if ports.len() < 2 {
            continue;
        }
        let pairing_cost = |left: usize, right: usize| -> f32 {
            1.0 + ports[left].outward.x * ports[right].outward.x
                + ports[left].outward.y * ports[right].outward.y
        };
        if ports.len() == 3 {
            let mut left = 0usize;
            let mut right = 1usize;
            let mut best_cost = pairing_cost(left, right);
            for candidate_left in 0..ports.len() {
                for candidate_right in candidate_left + 1..ports.len() {
                    let cost = pairing_cost(candidate_left, candidate_right);
                    if cost < best_cost {
                        best_cost = cost;
                        left = candidate_left;
                        right = candidate_right;
                    }
                }
            }
            let branch = 3 - left - right;
            let mut through = shortest_node_path(
                node,
                ports[left].point,
                ports[right].point,
                &node_by_pixel,
                &pixel_adjacency,
                width,
                &mut predecessor,
                &mut touched_predecessors,
            );
            if through.len() < 2 {
                through = vec![ports[left].point, ports[right].point];
            }
            let attachment = through
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let da = (a.x - ports[branch].point.x).hypot(a.y - ports[branch].point.y);
                    let db = (b.x - ports[branch].point.x).hypot(b.y - ports[branch].point.y);
                    da.total_cmp(&db)
                })
                .map(|(i, _)| i)
                .unwrap_or(0);
            if attachment > 0 {
                junction_connectors.push(through[..=attachment].to_vec());
            }
            if attachment + 1 < through.len() {
                junction_connectors.push(through[attachment..].to_vec());
            }
            let branch_path = shortest_node_path(
                node,
                ports[branch].point,
                through[attachment],
                &node_by_pixel,
                &pixel_adjacency,
                width,
                &mut predecessor,
                &mut touched_predecessors,
            );
            if branch_path.len() >= 2 {
                junction_connectors.push(branch_path);
            }
            continue;
        }

        let mut paired = vec![0u8; ports.len()];
        while paired.iter().filter(|&&p| p == 0).count() >= 2 {
            let mut best_left = ports.len();
            let mut best_right = ports.len();
            let mut best_cost = f32::INFINITY;
            for left in 0..ports.len() {
                if paired[left] != 0 {
                    continue;
                }
                for (right, (pr, _)) in paired.iter().zip(&ports).enumerate().skip(left + 1) {
                    if *pr != 0 {
                        continue;
                    }
                    let cost = pairing_cost(left, right);
                    if cost < best_cost {
                        best_cost = cost;
                        best_left = left;
                        best_right = right;
                    }
                }
            }
            if best_left == ports.len() {
                break;
            }
            paired[best_left] = 1;
            paired[best_right] = 1;
            if let Some(intersection) = valid_ray_intersection(
                node,
                &ports[best_left],
                &ports[best_right],
                &nodes,
                width,
                processing_scale,
            ) {
                junction_connectors.push(vec![
                    ports[best_left].point,
                    intersection,
                    ports[best_right].point,
                ]);
            } else {
                let connector = shortest_node_path(
                    node,
                    ports[best_left].point,
                    ports[best_right].point,
                    &node_by_pixel,
                    &pixel_adjacency,
                    width,
                    &mut predecessor,
                    &mut touched_predecessors,
                );
                if connector.len() >= 2 {
                    junction_connectors.push(connector);
                }
            }
        }
        if let Some(unpaired) = paired.iter().position(|&p| p == 0) {
            if let Some(last) = junction_connectors.last() {
                let last_front = last[0];
                let last_back = *last.last().unwrap();
                let to_front = (last_front.x - ports[unpaired].point.x)
                    .hypot(last_front.y - ports[unpaired].point.y);
                let to_back = (last_back.x - ports[unpaired].point.x)
                    .hypot(last_back.y - ports[unpaired].point.y);
                let attachment = if to_back < to_front {
                    last_back
                } else {
                    last_front
                };
                let connector = shortest_node_path(
                    node,
                    ports[unpaired].point,
                    attachment,
                    &node_by_pixel,
                    &pixel_adjacency,
                    width,
                    &mut predecessor,
                    &mut touched_predecessors,
                );
                if connector.len() >= 2 {
                    junction_connectors.push(connector);
                }
            }
        }
    }

    // ===== 笔画组（墨迹约束 RDP） =====
    let mut strokes: Vec<Stroke> = Vec::new();
    for edge_index in 0..graph_edges.len() {
        if removed_edge[edge_index] != 0 {
            continue;
        }
        if graph_edges[edge_index].points.len() >= 2 {
            strokes.push(simplify_within_ink(graph_edges[edge_index].points.clone()));
        }
    }
    for connector in junction_connectors {
        if connector.len() >= 2 && stroke_supported_by_ink(&connector) {
            strokes.push(simplify_within_ink(connector));
        }
    }
    strokes.extend(closed_strokes);
    strokes.extend(isolated_strokes);
    for stroke in &mut strokes {
        *stroke = simplify_within_ink(std::mem::take(stroke));
    }

    // ===== 组件覆盖补漏（细化可能抹掉极小连通域） =====
    let mut route_support = vec![0u8; source_ink.len()];
    let mut mark_nearest_source_ink = |point: PointF| {
        let cx = point.x.round() as i32;
        let cy = point.y.round() as i32;
        let mut best_distance = f32::INFINITY;
        let mut best = source_ink.len();
        for oy in -1..=1 {
            for ox in -1..=1 {
                let x = cx + ox;
                let y = cy + oy;
                if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                    continue;
                }
                let index = y as usize * w + x as usize;
                let distance = (x as f32 - point.x).hypot(y as f32 - point.y);
                if source_ink[index] != 0 && distance < best_distance {
                    best_distance = distance;
                    best = index;
                }
            }
        }
        if best != source_ink.len() {
            route_support[best] = 1;
        }
    };
    for stroke in &strokes {
        for i in 1..stroke.len() {
            let start = stroke[i - 1];
            let end = stroke[i];
            let distance = (end.x - start.x).hypot(end.y - start.y);
            if !distance.is_finite() {
                continue;
            }
            // 防御：异常大距离的 `as i32` 饱和会形成约 21 亿次迭代（正常管线坐标有界，不可达）
            let steps = ((distance * 2.0).ceil() as i64).clamp(1, 20_000) as i32;
            for step in 0..=steps {
                let ratio = step as f32 / steps as f32;
                mark_nearest_source_ink(PointF {
                    x: start.x + (end.x - start.x) * ratio,
                    y: start.y + (end.y - start.y) * ratio,
                });
            }
        }
    }

    let mut visited_source = vec![0u8; source_ink.len()];
    let mut pending_source: VecDeque<usize> = VecDeque::new();
    let mut source_component: Vec<usize> = Vec::new();
    for component_start in 0..source_ink.len() {
        if source_ink[component_start] == 0 || visited_source[component_start] != 0 {
            continue;
        }
        source_component.clear();
        let mut represented = false;
        let mut sum_x = 0.0f64;
        let mut sum_y = 0.0f64;
        visited_source[component_start] = 1;
        pending_source.push_back(component_start);
        while let Some(current) = pending_source.pop_front() {
            source_component.push(current);
            represented = represented || route_support[current] != 0;
            let x = (current % w) as i32;
            let y = (current / w) as i32;
            sum_x += x as f64;
            sum_y += y as f64;
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
                    let neighbor = ny as usize * w + nx as usize;
                    if source_ink[neighbor] != 0 && visited_source[neighbor] == 0 {
                        visited_source[neighbor] = 1;
                        pending_source.push_back(neighbor);
                    }
                }
            }
        }
        if represented || source_component.is_empty() {
            continue;
        }
        let centroid = PointF {
            x: (sum_x / source_component.len() as f64) as f32,
            y: (sum_y / source_component.len() as f64) as f32,
        };
        let center_pixel = *source_component
            .iter()
            .min_by(|&&a, &&b| {
                let da = ((a % w) as f32 - centroid.x).hypot((a / w) as f32 - centroid.y);
                let db = ((b % w) as f32 - centroid.x).hypot((b / w) as f32 - centroid.y);
                da.total_cmp(&db)
            })
            .unwrap();
        let start = PointF {
            x: (center_pixel % w) as f32,
            y: (center_pixel / w) as f32,
        };
        let mut end = start;
        let mut best_length = 0.0f32;
        for &pixel in &source_component {
            let candidate = PointF {
                x: (pixel % w) as f32,
                y: (pixel / w) as f32,
            };
            let length = (candidate.x - start.x).hypot(candidate.y - start.y);
            if length > best_length && segment_supported_by_ink(start, candidate) {
                best_length = length;
                end = candidate;
            }
        }
        // 无有效候选时不产出零长度笔画（组件可能只是无法连线的孤立噪点）
        if end != start {
            strokes.push(vec![start, end]);
        }
    }

    // ===== 最近端点排序（只改变空笔行程，不改变落笔几何） =====
    let mut ordered: Vec<Stroke> = Vec::with_capacity(strokes.len());
    let mut current = PointF {
        x: width as f32 * 0.5,
        y: height as f32 * 0.5,
    };
    // 用槽位标记代替 Vec::remove，避免每次选择最近笔画都搬移剩余元素；
    // 扫描顺序保持不变，因此并列距离时结果也保持稳定。
    let mut remaining: Vec<Option<Stroke>> = strokes.into_iter().map(Some).collect();
    let mut remaining_count = remaining.len();
    while remaining_count > 0 {
        let mut best_index = 0usize;
        let mut reverse_best = false;
        let mut best_distance = f32::INFINITY;
        for (index, slot) in remaining.iter().enumerate() {
            let Some(stroke) = slot.as_ref() else {
                continue;
            };
            let front_distance = (stroke[0].x - current.x).hypot(stroke[0].y - current.y);
            let back_distance =
                (stroke.last().unwrap().x - current.x).hypot(stroke.last().unwrap().y - current.y);
            let nearest = front_distance.min(back_distance);
            if nearest < best_distance {
                best_distance = nearest;
                best_index = index;
                reverse_best = back_distance < front_distance;
            }
        }
        let mut stroke = remaining[best_index].take().unwrap();
        remaining_count -= 1;
        if reverse_best {
            stroke.reverse();
        }
        current = *stroke.last().unwrap();
        ordered.push(stroke);
    }

    // ===== 跨度元数据 =====
    let point_region = |point: PointF| -> LineRegionType {
        let pixel = point_pixel(point, width, height);
        if pixel < region_types.len()
            && (region_types[pixel] as usize) <= LineRegionType::Ambiguous as usize
        {
            LineRegionType::from_u8(region_types[pixel])
        } else {
            LineRegionType::Ambiguous
        }
    };
    let segment_region = |left: PointF, right: PointF| -> LineRegionType {
        let left_type = point_region(left);
        let right_type = point_region(right);
        if left_type == right_type {
            return left_type;
        }
        if left_type == LineRegionType::Junction || right_type == LineRegionType::Junction {
            return LineRegionType::Junction;
        }
        right_type
    };

    let mut metadata: Vec<StrokeRouteMetadata> = Vec::with_capacity(ordered.len());
    for stroke in &ordered {
        let mut stroke_metadata = StrokeRouteMetadata {
            spans: Vec::new(),
            closed: stroke.len() >= 3 && stroke[0] == *stroke.last().unwrap(),
        };
        if stroke.len() >= 2 {
            let mut span_begin = 0usize;
            let mut current_type = segment_region(stroke[0], stroke[1]);
            for segment in 1..stroke.len() - 1 {
                let next_type = segment_region(stroke[segment], stroke[segment + 1]);
                if next_type == current_type {
                    continue;
                }
                stroke_metadata.spans.push(RouteSpan {
                    point_begin: span_begin,
                    point_end: segment,
                    region_type: current_type,
                    begin_anchor_flags: if span_begin == 0 {
                        ANCHOR_GRAPH_ENDPOINT
                    } else {
                        ANCHOR_REGION_BOUNDARY
                    },
                    end_anchor_flags: ANCHOR_REGION_BOUNDARY,
                });
                span_begin = segment;
                current_type = next_type;
            }
            stroke_metadata.spans.push(RouteSpan {
                point_begin: span_begin,
                point_end: stroke.len() - 1,
                region_type: current_type,
                begin_anchor_flags: if span_begin == 0 {
                    ANCHOR_GRAPH_ENDPOINT
                } else {
                    ANCHOR_REGION_BOUNDARY
                },
                end_anchor_flags: ANCHOR_GRAPH_ENDPOINT,
            });

            let junction_flag = |point: PointF| -> u8 {
                let pixel = point_pixel(point, width, height);
                if pixel < junction_zone.len() && junction_zone[pixel] != 0 {
                    ANCHOR_JUNCTION_PORT
                } else {
                    ANCHOR_NONE
                }
            };
            if let Some(first) = stroke_metadata.spans.first_mut() {
                first.begin_anchor_flags |= junction_flag(stroke[0]);
                if stroke_metadata.closed {
                    first.begin_anchor_flags |= ANCHOR_CLOSED_SEAM;
                }
            }
            if let Some(last) = stroke_metadata.spans.last_mut() {
                last.end_anchor_flags |= junction_flag(*stroke.last().unwrap());
                if stroke_metadata.closed {
                    last.end_anchor_flags |= ANCHOR_CLOSED_SEAM;
                }
            }
        }
        metadata.push(stroke_metadata);
    }

    (ordered, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_node_distance_is_euclidean() {
        // 3x3 图像，节点像素 (1,1)（索引 1*3+1=4），候选点 (5,1)：距离应为 4
        let pixels = vec![4];
        let d = nearest_node_pixel_distance(&pixels, 3, PointF { x: 5.0, y: 1.0 });
        assert!((d - 4.0).abs() < 1e-5);

        // 多像素取最近（候选点恰在某个像素上）
        let pixels = vec![0, 4, 8]; // (0,0) (1,1) (2,2)
        let d = nearest_node_pixel_distance(&pixels, 3, PointF { x: 2.0, y: 2.0 });
        assert!(d < 1e-5);

        // 大距离与负向偏移
        let pixels = vec![0]; // (0,0)
        let d = nearest_node_pixel_distance(&pixels, 3, PointF { x: 100.0, y: 0.0 });
        assert!((d - 100.0).abs() < 1e-3);
        let d = nearest_node_pixel_distance(
            &pixels,
            3,
            PointF {
                x: -100.0,
                y: -100.0,
            },
        );
        assert!((d - 100.0 * 2f32.sqrt()).abs() < 1e-3);

        // 空集合回退 INFINITY（判定为"过远"，调用方拒绝该交点）
        assert_eq!(
            nearest_node_pixel_distance(&[], 3, PointF { x: 0.0, y: 0.0 }),
            f32::INFINITY
        );
    }
}
