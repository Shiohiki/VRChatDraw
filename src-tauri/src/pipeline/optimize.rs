//! 阶段 6：无损路径优化（对应 C++ PathOptimizer.cpp）
//! 欧拉迹分解合并共享端点笔画（无重复描线）+ 空笔代价 2-opt + 零差异质量门禁
//! 门禁简化说明：C++ 用 ExecutionPlan 校验预计时间/落笔移动数；线段多重集一致
//! 已保证落笔移动线段完全相同（欧拉迹只是排列），故此处以
//! 线段多重集一致 + 笔画数不增 + 元数据一致作为等价门禁。

use std::collections::{HashMap, VecDeque};

use super::types::{
    DrawingPath, PointF, RouteSpan, Stroke, StrokeRouteMetadata, ANCHOR_CLOSED_SEAM,
};

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct PointKey {
    x: u32,
    y: u32,
}

fn to_key(point: PointF) -> PointKey {
    // NaN/inf 归一化到 0：避免 NaN 经 to_bits() 固定位模式被误判为同一节点
    let x = if point.x.is_finite() {
        if point.x == 0.0 {
            0.0
        } else {
            point.x
        }
    } else {
        0.0
    };
    let y = if point.y.is_finite() {
        if point.y == 0.0 {
            0.0
        } else {
            point.y
        }
    } else {
        0.0
    };
    PointKey {
        x: x.to_bits(),
        y: y.to_bits(),
    }
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct SegmentKey {
    first: PointKey,
    second: PointKey,
}

fn segment_key(first: PointF, second: PointF) -> SegmentKey {
    let left = to_key(first);
    let right = to_key(second);
    let (a, b) = if (right.x, right.y) < (left.x, left.y) {
        (right, left)
    } else {
        (left, right)
    };
    SegmentKey {
        first: a,
        second: b,
    }
}

fn segment_count(path: &DrawingPath) -> usize {
    path.strokes
        .iter()
        .map(|s| if s.len() > 1 { s.len() - 1 } else { 0 })
        .sum()
}

fn count_segments(path: &DrawingPath) -> HashMap<SegmentKey, usize> {
    let mut counts = HashMap::new();
    for stroke in &path.strokes {
        for i in 1..stroke.len() {
            *counts
                .entry(segment_key(stroke[i - 1], stroke[i]))
                .or_insert(0) += 1;
        }
    }
    counts
}

fn pen_up_distance(path: &DrawingPath) -> f32 {
    let mut current = PointF {
        x: path.width as f32 * 0.5,
        y: path.height as f32 * 0.5,
    };
    let mut distance = 0.0f32;
    for stroke in &path.strokes {
        if stroke.len() < 2 {
            continue;
        }
        distance += (stroke[0].x - current.x).hypot(stroke[0].y - current.y);
        current = *stroke.last().unwrap();
    }
    distance
}

struct GraphEdge {
    stroke: usize,
    left: usize,
    right: usize,
    virtual_edge: bool,
}

#[derive(Clone, Copy)]
struct EdgeVisit {
    edge: usize,
    from: usize,
}

struct Trail {
    points: Stroke,
    source_edge_ends: Vec<usize>,
    metadata: StrokeRouteMetadata,
}

fn metadata_for_stroke(path: &DrawingPath, stroke_index: usize) -> StrokeRouteMetadata {
    if stroke_index < path.route_metadata.len()
        && !path.route_metadata[stroke_index].spans.is_empty()
    {
        path.route_metadata[stroke_index].clone()
    } else {
        StrokeRouteMetadata::default_for(&path.strokes[stroke_index])
    }
}

fn append_oriented_metadata(
    trail: &mut Trail,
    source: &StrokeRouteMetadata,
    source_point_count: usize,
    forward: bool,
    destination_offset: usize,
) {
    if source_point_count < 2 || source.spans.is_empty() {
        return;
    }
    if forward {
        for span in &source.spans {
            trail.metadata.spans.push(RouteSpan {
                point_begin: destination_offset + span.point_begin,
                point_end: destination_offset + span.point_end,
                region_type: span.region_type,
                begin_anchor_flags: span.begin_anchor_flags,
                end_anchor_flags: span.end_anchor_flags,
            });
        }
    } else {
        for span in source.spans.iter().rev() {
            trail.metadata.spans.push(RouteSpan {
                point_begin: destination_offset + source_point_count - 1 - span.point_end,
                point_end: destination_offset + source_point_count - 1 - span.point_begin,
                region_type: span.region_type,
                begin_anchor_flags: span.end_anchor_flags,
                end_anchor_flags: span.begin_anchor_flags,
            });
        }
    }
}

fn outgoing_tangent(edge: &GraphEdge, node: usize, strokes: &[Stroke]) -> PointF {
    if edge.virtual_edge {
        return PointF { x: 0.0, y: 0.0 };
    }
    let stroke = &strokes[edge.stroke];
    if edge.left == node {
        for i in 1..stroke.len() {
            let x = stroke[i].x - stroke[0].x;
            let y = stroke[i].y - stroke[0].y;
            let length = x.hypot(y);
            if length > 1.0e-5 {
                return PointF {
                    x: x / length,
                    y: y / length,
                };
            }
        }
    } else {
        let mut i = stroke.len() - 1;
        loop {
            let x = stroke[i].x - stroke.last().unwrap().x;
            let y = stroke[i].y - stroke.last().unwrap().y;
            let length = x.hypot(y);
            if length > 1.0e-5 {
                return PointF {
                    x: x / length,
                    y: y / length,
                };
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
    }
    PointF { x: 0.0, y: 0.0 }
}

fn other_node(edge: &GraphEdge, node: usize) -> usize {
    if edge.left == node {
        edge.right
    } else {
        edge.left
    }
}

fn select_next_edge(
    node: usize,
    incoming_edge: usize,
    edges: &[GraphEdge],
    adjacency: &[Vec<usize>],
    used: &[u8],
    strokes: &[Stroke],
) -> usize {
    let missing = usize::MAX;
    let mut best = missing;
    let mut best_score = f32::INFINITY;
    let mut incoming = PointF { x: 0.0, y: 0.0 };
    let mut has_incoming_tangent = false;
    if incoming_edge != missing && !edges[incoming_edge].virtual_edge {
        let outward = outgoing_tangent(&edges[incoming_edge], node, strokes);
        incoming = PointF {
            x: -outward.x,
            y: -outward.y,
        };
        has_incoming_tangent = incoming.x.hypot(incoming.y) > 0.5;
    }
    for &edge_index in &adjacency[node] {
        if used[edge_index] != 0 {
            continue;
        }
        let edge = &edges[edge_index];
        let mut score = if edge.virtual_edge { 100.0 } else { 0.0 };
        if !edge.virtual_edge && has_incoming_tangent {
            let outgoing = outgoing_tangent(edge, node, strokes);
            score = 1.0 - (incoming.x * outgoing.x + incoming.y * outgoing.y);
        }
        if score < best_score || (score == best_score && edge_index < best) {
            best = edge_index;
            best_score = score;
        }
    }
    best
}

fn euler_circuit(
    start: usize,
    edges: &[GraphEdge],
    adjacency: &[Vec<usize>],
    used: &mut [u8],
    strokes: &[Stroke],
) -> Vec<EdgeVisit> {
    let missing = usize::MAX;
    #[derive(Clone, Copy)]
    struct StackItem {
        node: usize,
        incoming_edge: usize,
        from: usize,
    }
    let mut stack = vec![StackItem {
        node: start,
        incoming_edge: missing,
        from: start,
    }];
    let mut reversed: Vec<EdgeVisit> = Vec::new();
    while let Some(current) = stack.last().copied() {
        let next_edge = select_next_edge(
            current.node,
            current.incoming_edge,
            edges,
            adjacency,
            used,
            strokes,
        );
        if next_edge != missing {
            used[next_edge] = 1;
            let next_node = other_node(&edges[next_edge], current.node);
            stack.push(StackItem {
                node: next_node,
                incoming_edge: next_edge,
                from: current.node,
            });
            continue;
        }
        stack.pop();
        if current.incoming_edge != missing {
            reversed.push(EdgeVisit {
                edge: current.incoming_edge,
                from: current.from,
            });
        }
    }
    reversed.reverse();
    reversed
}

fn append_visit(
    trail: &mut Trail,
    visit: EdgeVisit,
    edges: &[GraphEdge],
    baseline: &DrawingPath,
) -> bool {
    let edge = &edges[visit.edge];
    if edge.virtual_edge {
        return false;
    }
    let source = &baseline.strokes[edge.stroke];
    let forward = visit.from == edge.left;
    let first = if forward {
        source[0]
    } else {
        *source.last().unwrap()
    };
    if !trail.points.is_empty() && *trail.points.last().unwrap() != first {
        return false;
    }
    let destination_offset = if trail.points.is_empty() {
        0
    } else {
        trail.points.len() - 1
    };
    let source_metadata = metadata_for_stroke(baseline, edge.stroke);
    if forward {
        let start = if trail.points.is_empty() { 0 } else { 1 };
        trail.points.extend_from_slice(&source[start..]);
    } else {
        let start = if trail.points.is_empty() { 0 } else { 1 };
        let mut rev: Stroke = source.iter().rev().skip(start).copied().collect();
        trail.points.append(&mut rev);
    }
    append_oriented_metadata(
        trail,
        &source_metadata,
        source.len(),
        forward,
        destination_offset,
    );
    trail.source_edge_ends.push(trail.points.len() - 1);
    trail.metadata.closed =
        trail.points.len() >= 3 && trail.points[0] == *trail.points.last().unwrap();
    true
}

fn append_circuit_trails(
    circuit: &[EdgeVisit],
    edges: &[GraphEdge],
    baseline: &DrawingPath,
    output: &mut Vec<Trail>,
) -> bool {
    let virtual_pos = circuit.iter().position(|v| edges[v.edge].virtual_edge);
    let first = match virtual_pos {
        Some(p) => (p + 1) % circuit.len(),
        None => 0,
    };
    let mut trail = Trail {
        points: Vec::new(),
        source_edge_ends: Vec::new(),
        metadata: StrokeRouteMetadata {
            spans: Vec::new(),
            closed: false,
        },
    };
    for offset in 0..circuit.len() {
        let visit = circuit[(first + offset) % circuit.len()];
        if edges[visit.edge].virtual_edge {
            if trail.points.len() >= 2 {
                output.push(std::mem::replace(
                    &mut trail,
                    Trail {
                        points: Vec::new(),
                        source_edge_ends: Vec::new(),
                        metadata: StrokeRouteMetadata {
                            spans: Vec::new(),
                            closed: false,
                        },
                    },
                ));
            }
            continue;
        }
        if !append_visit(&mut trail, visit, edges, baseline) {
            return false;
        }
    }
    if trail.points.len() >= 2 {
        output.push(trail);
    }
    true
}

fn reverse_trail(trail: &mut Trail) {
    let mut edge_lengths: Vec<usize> = Vec::new();
    let mut previous_end = 0usize;
    for &end in &trail.source_edge_ends {
        edge_lengths.push(end - previous_end);
        previous_end = end;
    }
    let point_count = trail.points.len();
    let mut reversed_spans: Vec<RouteSpan> = Vec::new();
    for span in trail.metadata.spans.iter().rev() {
        reversed_spans.push(RouteSpan {
            point_begin: point_count - 1 - span.point_end,
            point_end: point_count - 1 - span.point_begin,
            region_type: span.region_type,
            begin_anchor_flags: span.end_anchor_flags,
            end_anchor_flags: span.begin_anchor_flags,
        });
    }
    trail.points.reverse();
    trail.metadata.spans = reversed_spans;
    edge_lengths.reverse();
    trail.source_edge_ends.clear();
    let mut cumulative = 0usize;
    for length in edge_lengths {
        cumulative += length;
        trail.source_edge_ends.push(cumulative);
    }
}

fn rotate_closed_trail(trail: &mut Trail, first_edge: usize) {
    if first_edge == 0
        || trail.points.len() < 3
        || trail.points[0] != *trail.points.last().unwrap()
        || first_edge >= trail.source_edge_ends.len()
    {
        return;
    }
    let mut edge_lengths: Vec<usize> = Vec::new();
    let mut previous_end = 0usize;
    for &end in &trail.source_edge_ends {
        edge_lengths.push(end - previous_end);
        previous_end = end;
    }
    let first_point = trail.source_edge_ends[first_edge - 1];
    let unique_points = trail.points.len() - 1;
    let mut rotated: Stroke = Vec::with_capacity(trail.points.len());
    for offset in 0..unique_points {
        rotated.push(trail.points[(first_point + offset) % unique_points]);
    }
    rotated.push(rotated[0]);
    trail.points = rotated;

    if !trail.metadata.spans.is_empty() {
        if let Some(first_span_index) = trail
            .metadata
            .spans
            .iter()
            .position(|s| s.point_begin == first_point)
        {
            let mut spans = trail.metadata.spans.clone();
            spans.rotate_left(first_span_index);
            let mut cumulative = 0usize;
            for span in spans.iter_mut() {
                let length = span.point_end - span.point_begin;
                span.point_begin = cumulative;
                span.point_end = cumulative + length;
                span.begin_anchor_flags &= !ANCHOR_CLOSED_SEAM;
                span.end_anchor_flags &= !ANCHOR_CLOSED_SEAM;
                cumulative += length;
            }
            spans[0].begin_anchor_flags |= ANCHOR_CLOSED_SEAM;
            spans.last_mut().unwrap().end_anchor_flags |= ANCHOR_CLOSED_SEAM;
            trail.metadata.spans = spans;
        } else {
            // 旋转点落在跨度内部（该源边端点处区域类型未变化）：无法按源边界
            // 重建跨度，退化为默认单跨度元数据，避免跨度语义与旋转后的点序列
            // 错位（结构门禁只查连续性，不会拦截这种错位）
            trail.metadata = StrokeRouteMetadata::default_for(&trail.points);
        }
    }
    edge_lengths.rotate_left(first_edge);
    trail.source_edge_ends.clear();
    let mut cumulative = 0usize;
    for length in edge_lengths {
        cumulative += length;
        trail.source_edge_ends.push(cumulative);
    }
}

fn pen_up_move_cost(from: PointF, to: PointF, width: u32, height: u32) -> i32 {
    let maximum_dimension = width.max(height) as i32;
    let scale = if maximum_dimension > 0 {
        (768.0 / maximum_dimension as f32).min(1.0)
    } else {
        1.0
    };
    let center_x = width as f32 * 0.5;
    let center_y = height as f32 * 0.5;
    let convert = |p: PointF| -> (i32, i32) {
        (
            ((p.x - center_x) * scale).round() as i32,
            ((p.y - center_y) * scale).round() as i32,
        )
    };
    let left = convert(from);
    let right = convert(to);
    let distance = (right.0 - left.0).abs().max((right.1 - left.1).abs());
    (distance + 5) / 6
}

fn has_mouse_movement(stroke: &Stroke, width: u32, height: u32) -> bool {
    if stroke.len() < 2 {
        return false;
    }
    let maximum_dimension = width.max(height) as i32;
    let scale = if maximum_dimension > 0 {
        (768.0 / maximum_dimension as f32).min(1.0)
    } else {
        1.0
    };
    let center_x = width as f32 * 0.5;
    let center_y = height as f32 * 0.5;
    let convert = |p: PointF| -> (i32, i32) {
        (
            ((p.x - center_x) * scale).round() as i32,
            ((p.y - center_y) * scale).round() as i32,
        )
    };
    let mut previous = convert(stroke[0]);
    for &p in &stroke[1..] {
        let current = convert(p);
        if current != previous {
            return true;
        }
        previous = current;
    }
    false
}

fn improve_trail_order(trails: &mut [Trail], width: u32, height: u32) {
    const SEARCH_WINDOW: usize = 48;
    let center = PointF {
        x: width as f32 * 0.5,
        y: height as f32 * 0.5,
    };
    for _ in 0..2 {
        let mut improved = false;
        for first in 0..trails.len() {
            if trails[first].points.len() < 2 {
                continue; // 防御：空/单点 trail 不参与 2-opt
            }
            let previous = if first == 0 {
                center
            } else {
                *trails[first - 1].points.last().unwrap()
            };
            let limit = trails.len().min(first + SEARCH_WINDOW + 1);
            let mut swapped = false;
            for last in first + 1..limit {
                let old_cost = pen_up_move_cost(previous, trails[first].points[0], width, height)
                    + if last + 1 < trails.len() {
                        pen_up_move_cost(
                            *trails[last].points.last().unwrap(),
                            trails[last + 1].points[0],
                            width,
                            height,
                        )
                    } else {
                        0
                    };
                let new_cost = pen_up_move_cost(
                    previous,
                    *trails[last].points.last().unwrap(),
                    width,
                    height,
                ) + if last + 1 < trails.len() {
                    pen_up_move_cost(
                        trails[first].points[0],
                        trails[last + 1].points[0],
                        width,
                        height,
                    )
                } else {
                    0
                };
                if new_cost >= old_cost {
                    continue;
                }
                trails[first..=last].reverse();
                for trail in &mut trails[first..=last] {
                    reverse_trail(trail);
                }
                improved = true;
                swapped = true;
                break;
            }
            if swapped {
                break;
            }
        }
        if !improved {
            break;
        }
    }
}

fn order_trails(mut trails: Vec<Trail>, width: u32, height: u32) -> Vec<Trail> {
    let mut ordered: Vec<Trail> = Vec::with_capacity(trails.len());
    let mut current = PointF {
        x: width as f32 * 0.5,
        y: height as f32 * 0.5,
    };
    while !trails.is_empty() {
        let mut best_stroke = 0usize;
        let mut best_loop_edge = 0usize;
        let mut reverse_best = false;
        let mut best_distance = f32::INFINITY;
        for (index, trail_entry) in trails.iter().enumerate() {
            let stroke = &trail_entry.points;
            if stroke.len() < 2 {
                continue;
            }
            if stroke[0] == *stroke.last().unwrap() && !trails[index].source_edge_ends.is_empty() {
                for edge in 0..trails[index].source_edge_ends.len() {
                    let point = if edge == 0 {
                        0
                    } else {
                        trails[index].source_edge_ends[edge - 1]
                    };
                    let distance = (stroke[point].x - current.x).hypot(stroke[point].y - current.y);
                    if distance < best_distance {
                        best_distance = distance;
                        best_stroke = index;
                        best_loop_edge = edge;
                        reverse_best = false;
                    }
                }
                continue;
            }
            let front_distance = (stroke[0].x - current.x).hypot(stroke[0].y - current.y);
            let back_distance =
                (stroke.last().unwrap().x - current.x).hypot(stroke.last().unwrap().y - current.y);
            let nearest = front_distance.min(back_distance);
            if nearest < best_distance {
                best_distance = nearest;
                best_stroke = index;
                best_loop_edge = 0;
                reverse_best = back_distance < front_distance;
            }
        }
        // 防御：全部 trail 均不足 2 点（正常管线不可达，pub 入口防误用）时停止，
        // 避免 remove(0) 后对空 Vec 索引 panic
        if best_distance == f32::INFINITY {
            break;
        }
        let mut selected = trails.remove(best_stroke);
        if selected.points.len() < 2 {
            continue; // 极端防御：空/单点 trail 不产出笔画
        }
        if selected.points[0] == *selected.points.last().unwrap() {
            rotate_closed_trail(&mut selected, best_loop_edge);
        } else if reverse_best {
            reverse_trail(&mut selected);
        }
        current = *selected.points.last().unwrap();
        ordered.push(selected);
    }
    improve_trail_order(&mut ordered, width, height);
    ordered
}

fn build_trails(baseline: &DrawingPath, trails: &mut Vec<Trail>) -> bool {
    let missing = usize::MAX;
    let mut node_by_point: HashMap<PointKey, usize> = HashMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut adjacency: Vec<Vec<usize>> = Vec::new();
    let mut non_drawable: Vec<usize> = Vec::new();

    let mut node_for = |point: PointF, adjacency: &mut Vec<Vec<usize>>| -> usize {
        let key = to_key(point);
        if let Some(&node) = node_by_point.get(&key) {
            return node;
        }
        let node = adjacency.len();
        node_by_point.insert(key, node);
        adjacency.push(Vec::new());
        node
    };

    edges.reserve(baseline.strokes.len());
    for (stroke_index, stroke) in baseline.strokes.iter().enumerate() {
        if stroke.len() < 2 || !has_mouse_movement(stroke, baseline.width, baseline.height) {
            non_drawable.push(stroke_index);
            continue;
        }
        let left = node_for(stroke[0], &mut adjacency);
        let right = node_for(*stroke.last().unwrap(), &mut adjacency);
        let edge_index = edges.len();
        edges.push(GraphEdge {
            stroke: stroke_index,
            left,
            right,
            virtual_edge: false,
        });
        adjacency[left].push(edge_index);
        adjacency[right].push(edge_index);
    }

    let real_edge_count = edges.len();
    let mut visited_node = vec![0u8; adjacency.len()];
    let mut components: Vec<Vec<usize>> = Vec::new();
    for start in 0..adjacency.len() {
        if visited_node[start] != 0 || adjacency[start].is_empty() {
            continue;
        }
        let mut component: Vec<usize> = Vec::new();
        let mut pending: VecDeque<usize> = VecDeque::new();
        visited_node[start] = 1;
        pending.push_back(start);
        while let Some(node) = pending.pop_front() {
            component.push(node);
            for &edge_index in &adjacency[node] {
                let neighbor = other_node(&edges[edge_index], node);
                if visited_node[neighbor] == 0 {
                    visited_node[neighbor] = 1;
                    pending.push_back(neighbor);
                }
            }
        }
        components.push(component);
    }

    for component in &components {
        let mut odd_nodes: Vec<usize> = Vec::new();
        for &node in component {
            if !adjacency[node].len().is_multiple_of(2) {
                odd_nodes.push(node);
            }
        }
        for pair in odd_nodes.chunks(2) {
            if pair.len() < 2 {
                break;
            }
            let edge_index = edges.len();
            edges.push(GraphEdge {
                stroke: missing,
                left: pair[0],
                right: pair[1],
                virtual_edge: true,
            });
            adjacency[pair[0]].push(edge_index);
            adjacency[pair[1]].push(edge_index);
        }
    }

    let mut used = vec![0u8; edges.len()];
    for component in &components {
        let circuit = euler_circuit(
            component[0],
            &edges,
            &adjacency,
            &mut used,
            &baseline.strokes,
        );
        if circuit.is_empty() || !append_circuit_trails(&circuit, &edges, baseline, trails) {
            return false;
        }
    }
    for &stroke_index in &non_drawable {
        trails.push(Trail {
            points: baseline.strokes[stroke_index].clone(),
            source_edge_ends: Vec::new(),
            metadata: metadata_for_stroke(baseline, stroke_index),
        });
    }
    (0..real_edge_count).all(|edge| used[edge] != 0)
}

fn have_identical_path_segments(baseline: &DrawingPath, candidate: &DrawingPath) -> bool {
    baseline.width == candidate.width
        && baseline.height == candidate.height
        && segment_count(baseline) == segment_count(candidate)
        && count_segments(baseline) == count_segments(candidate)
}

fn have_consistent_route_metadata(path: &DrawingPath) -> bool {
    if path.route_metadata.len() != path.strokes.len() {
        return path.strokes.is_empty() && path.route_metadata.is_empty();
    }
    for (stroke_index, stroke) in path.strokes.iter().enumerate() {
        let metadata = &path.route_metadata[stroke_index];
        if stroke.len() < 2 {
            if !metadata.spans.is_empty() {
                return false;
            }
            continue;
        }
        if metadata.spans.is_empty()
            || metadata.spans[0].point_begin != 0
            || metadata.spans.last().unwrap().point_end != stroke.len() - 1
            || metadata.closed != (stroke.len() >= 3 && stroke[0] == *stroke.last().unwrap())
        {
            return false;
        }
        let mut previous_end = 0usize;
        for span in &metadata.spans {
            if span.point_begin != previous_end
                || span.point_end <= span.point_begin
                || span.point_end >= stroke.len()
            {
                return false;
            }
            previous_end = span.point_end;
        }
    }
    true
}

/// 无损优化（对应 C++ OptimizeDrawingPathLossless）
pub fn optimize_drawing_path_lossless(mut baseline: DrawingPath) -> DrawingPath {
    if !have_consistent_route_metadata(&baseline) {
        baseline.route_metadata.clear();
        for stroke in &baseline.strokes {
            baseline
                .route_metadata
                .push(StrokeRouteMetadata::default_for(stroke));
        }
    }
    let strokes_before = baseline.strokes.len();
    let pen_up_distance_before = pen_up_distance(&baseline);

    let mut candidate = DrawingPath {
        width: baseline.width,
        height: baseline.height,
        strokes: Vec::new(),
        route_metadata: Vec::new(),
    };
    let mut trails: Vec<Trail> = Vec::new();
    if !build_trails(&baseline, &mut trails) {
        return baseline;
    }
    trails = order_trails(trails, candidate.width, candidate.height);
    candidate.strokes.reserve(trails.len());
    candidate.route_metadata.reserve(trails.len());
    for trail in trails {
        candidate.strokes.push(trail.points);
        candidate.route_metadata.push(trail.metadata);
    }
    let strokes_after = candidate.strokes.len();
    let pen_up_distance_after = pen_up_distance(&candidate);
    let exact_segment_match = have_identical_path_segments(&baseline, &candidate);
    let metadata_consistent = have_consistent_route_metadata(&candidate);
    let applied = exact_segment_match
        && metadata_consistent
        && strokes_after <= strokes_before
        && pen_up_distance_after <= pen_up_distance_before + 1.0e-3;

    if !applied {
        return baseline;
    }
    candidate
}

/// 供阶段 8 集成使用：从 Strokes + 元数据构建 DrawingPath
pub fn strokes_to_drawing_path(
    width: u32,
    height: u32,
    strokes: Vec<Stroke>,
    metadata: Vec<StrokeRouteMetadata>,
) -> DrawingPath {
    DrawingPath {
        width,
        height,
        strokes,
        route_metadata: metadata,
    }
}
