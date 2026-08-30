//! 阶段 7：矢量拟合（对应 C++ VectorPath.cpp）
//! 对验证后的折线跨度做保守直线/三次贝塞尔拟合，逐跨度质量门禁：
//! 双向距离 / 墨迹支持 / 自交 / 曲率尖峰 / 包围盒，任一失败回退该跨度的折线
//! 受保护尖角把跨度在拐角处拆分后再拟合

use super::path_math::point_segment_distance;
use super::types::{
    BinaryImage, CubicBezier, DrawingPath, FittedRouteSpan, LineRegionType, PointF, RouteSpan,
    Stroke, StrokeRouteMetadata, VectorDrawingPath, VectorSegment, VectorSegmentType, VectorStroke,
    ANCHOR_PROTECTED_CORNER,
};

/// 矢量拟合选项（对应 C++ VectorFittingOptions）
#[derive(Clone, Copy)]
pub struct VectorFittingOptions {
    pub line_tolerance: f32,
    pub curve_tolerance: f32,
    pub flattening_tolerance: f32,
    pub corner_arm_length: f32,
    pub corner_cosine_threshold: f32,
}

impl Default for VectorFittingOptions {
    fn default() -> Self {
        Self {
            line_tolerance: 0.45,
            curve_tolerance: 0.85,
            flattening_tolerance: 0.25,
            corner_arm_length: 4.0,
            corner_cosine_threshold: -0.45,
        }
    }
}

const GEOMETRY_EPSILON: f32 = 1.0e-5;

#[inline]
fn add(left: PointF, right: PointF) -> PointF {
    PointF {
        x: left.x + right.x,
        y: left.y + right.y,
    }
}

#[inline]
fn subtract(left: PointF, right: PointF) -> PointF {
    PointF {
        x: left.x - right.x,
        y: left.y - right.y,
    }
}

#[inline]
fn multiply(point: PointF, value: f32) -> PointF {
    PointF {
        x: point.x * value,
        y: point.y * value,
    }
}

#[inline]
fn dot(left: PointF, right: PointF) -> f32 {
    left.x * right.x + left.y * right.y
}

#[inline]
fn length(point: PointF) -> f32 {
    point.x.hypot(point.y)
}

fn normalize(point: PointF) -> PointF {
    let l = length(point);
    if l > GEOMETRY_EPSILON {
        multiply(point, 1.0 / l)
    } else {
        PointF { x: 0.0, y: 0.0 }
    }
}

fn lexicographically_less(left: PointF, right: PointF) -> bool {
    (left.x, left.y) < (right.x, right.y)
}

fn evaluate(curve: &CubicBezier, t: f32) -> PointF {
    let u = 1.0 - t;
    let b0 = u * u * u;
    let b1 = 3.0 * u * u * t;
    let b2 = 3.0 * u * t * t;
    let b3 = t * t * t;
    PointF {
        x: curve.p0.x * b0 + curve.p1.x * b1 + curve.p2.x * b2 + curve.p3.x * b3,
        y: curve.p0.y * b0 + curve.p1.y * b1 + curve.p2.y * b2 + curve.p3.y * b3,
    }
}

fn split(curve: &CubicBezier) -> (CubicBezier, CubicBezier) {
    let p01 = multiply(add(curve.p0, curve.p1), 0.5);
    let p12 = multiply(add(curve.p1, curve.p2), 0.5);
    let p23 = multiply(add(curve.p2, curve.p3), 0.5);
    let p012 = multiply(add(p01, p12), 0.5);
    let p123 = multiply(add(p12, p23), 0.5);
    let center = multiply(add(p012, p123), 0.5);
    (
        CubicBezier {
            p0: curve.p0,
            p1: p01,
            p2: p012,
            p3: center,
        },
        CubicBezier {
            p0: center,
            p1: p123,
            p2: p23,
            p3: curve.p3,
        },
    )
}

fn flatten_cubic(curve: &CubicBezier, tolerance: f32, depth: i32, output: &mut Stroke) {
    let flatness = point_segment_distance(curve.p1, curve.p0, curve.p3)
        .max(point_segment_distance(curve.p2, curve.p0, curve.p3));
    if flatness <= tolerance || depth >= 16 {
        if output.is_empty() || *output.last().unwrap() != curve.p3 {
            output.push(curve.p3);
        }
        return;
    }
    let (left, right) = split(curve);
    flatten_cubic(&left, tolerance, depth + 1, output);
    flatten_cubic(&right, tolerance, depth + 1, output);
}

fn point_polyline_distance(point: PointF, polyline: &Stroke) -> f32 {
    if polyline.is_empty() {
        return f32::INFINITY;
    }
    if polyline.len() == 1 {
        return length(subtract(point, polyline[0]));
    }
    let mut distance = f32::INFINITY;
    for i in 1..polyline.len() {
        distance = distance.min(point_segment_distance(point, polyline[i - 1], polyline[i]));
    }
    distance
}

fn bidirectional_distance(left: &Stroke, right: &Stroke) -> f32 {
    let mut distance = 0.0f32;
    for &p in left {
        distance = distance.max(point_polyline_distance(p, right));
    }
    for &p in right {
        distance = distance.max(point_polyline_distance(p, left));
    }
    distance
}

fn orientation(a: PointF, b: PointF, c: PointF) -> i32 {
    let value = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if value.abs() <= 1.0e-4 {
        0
    } else if value > 0.0 {
        1
    } else {
        -1
    }
}

fn properly_intersects(a: PointF, b: PointF, c: PointF, d: PointF) -> bool {
    orientation(a, b, c) * orientation(a, b, d) < 0
        && orientation(c, d, a) * orientation(c, d, b) < 0
}

fn segment_bounding_boxes_overlap(a: PointF, b: PointF, c: PointF, d: PointF) -> bool {
    let left_min_x = a.x.min(b.x);
    let left_max_x = a.x.max(b.x);
    let left_min_y = a.y.min(b.y);
    let left_max_y = a.y.max(b.y);
    let right_min_x = c.x.min(d.x);
    let right_max_x = c.x.max(d.x);
    let right_min_y = c.y.min(d.y);
    let right_max_y = c.y.max(d.y);
    left_min_x <= right_max_x
        && right_min_x <= left_max_x
        && left_min_y <= right_max_y
        && right_min_y <= left_max_y
}

fn self_intersection_count(points: &Stroke) -> usize {
    if points.len() < 4 {
        return 0;
    }
    let closed = points[0] == *points.last().unwrap();
    let mut count = 0usize;
    for left in 1..points.len() {
        for right in left + 2..points.len() {
            if closed && left == 1 && right + 1 == points.len() {
                continue;
            }
            let first_start = points[left - 1];
            let first_end = points[left];
            let second_start = points[right - 1];
            let second_end = points[right];
            if segment_bounding_boxes_overlap(first_start, first_end, second_start, second_end) {
                count +=
                    properly_intersects(first_start, first_end, second_start, second_end) as usize;
            }
        }
    }
    count
}

fn curvature_spike_count(points: &Stroke) -> usize {
    let mut count = 0usize;
    for index in 2..points.len() {
        let incoming = subtract(points[index - 1], points[index - 2]);
        let outgoing = subtract(points[index], points[index - 1]);
        let denominator = length(incoming) * length(outgoing);
        if denominator > GEOMETRY_EPSILON && dot(incoming, outgoing) / denominator < 0.35 {
            count += 1;
        }
    }
    count
}

fn point_supported_by_ink(point: PointF, ink: &BinaryImage) -> bool {
    if ink.width == 0 || ink.height == 0 {
        return false;
    }
    let center_x = point.x.round() as i32;
    let center_y = point.y.round() as i32;
    for oy in -1..=1 {
        for ox in -1..=1 {
            let x = center_x + ox;
            let y = center_y + oy;
            if x >= 0
                && y >= 0
                && x < ink.width as i32
                && y < ink.height as i32
                && ink.pixels[(y as usize) * ink.width as usize + x as usize] != 0
            {
                return true;
            }
        }
    }
    false
}

fn polyline_supported_by_ink(points: &Stroke, ink: &BinaryImage) -> bool {
    if points.len() < 2 {
        return false;
    }
    for i in 1..points.len() {
        let start = points[i - 1];
        let end = points[i];
        let distance = length(subtract(end, start));
        if !distance.is_finite() {
            return false;
        }
        // 防御：异常大距离的 `as i32` 饱和会形成约 21 亿次迭代（正常管线坐标有界，不可达）
        let steps = ((distance * 2.0).ceil() as i64).clamp(1, 20_000) as i32;
        for step in 0..=steps {
            let ratio = step as f32 / steps as f32;
            if !point_supported_by_ink(add(start, multiply(subtract(end, start), ratio)), ink) {
                return false;
            }
        }
    }
    true
}

fn fit_single_cubic(
    points: &[PointF],
    tolerance: f32,
    worst_point: &mut usize,
) -> Option<CubicBezier> {
    if points.len() < 4 {
        return None;
    }
    let p0 = points[0];
    let p3 = *points.last().unwrap();
    let chord = length(subtract(p3, p0));
    if chord <= GEOMETRY_EPSILON {
        return None;
    }
    let start_tangent = normalize(subtract(points[1], p0));
    let end_tangent = normalize(subtract(points[points.len() - 2], p3));
    if length(start_tangent) < 0.5 || length(end_tangent) < 0.5 {
        return None;
    }

    let mut parameters = vec![0.0f32; points.len()];
    let mut total_length = 0.0f32;
    for index in 1..points.len() {
        total_length += length(subtract(points[index], points[index - 1]));
        parameters[index] = total_length;
    }
    if total_length <= GEOMETRY_EPSILON {
        return None;
    }
    for p in parameters.iter_mut() {
        *p /= total_length;
    }

    let mut c00 = 0.0f32;
    let mut c01 = 0.0f32;
    let mut c11 = 0.0f32;
    let mut x0 = 0.0f32;
    let mut x1 = 0.0f32;
    for index in 0..points.len() {
        let t = parameters[index];
        let u = 1.0 - t;
        let b0 = u * u * u;
        let b1 = 3.0 * u * u * t;
        let b2 = 3.0 * u * t * t;
        let b3 = t * t * t;
        let a0 = multiply(start_tangent, b1);
        let a1 = multiply(end_tangent, b2);
        let base = add(multiply(p0, b0 + b1), multiply(p3, b2 + b3));
        let residual = subtract(points[index], base);
        c00 += dot(a0, a0);
        c01 += dot(a0, a1);
        c11 += dot(a1, a1);
        x0 += dot(a0, residual);
        x1 += dot(a1, residual);
    }
    let determinant = c00 * c11 - c01 * c01;
    let mut alpha0 = chord / 3.0;
    let mut alpha1 = chord / 3.0;
    if determinant.abs() > 1.0e-6 {
        alpha0 = (x0 * c11 - x1 * c01) / determinant;
        alpha1 = (c00 * x1 - c01 * x0) / determinant;
    }
    let minimum_handle = chord * 0.02;
    let maximum_handle = chord * 1.25;
    if alpha0 < minimum_handle
        || alpha1 < minimum_handle
        || alpha0 > maximum_handle
        || alpha1 > maximum_handle
    {
        alpha0 = chord / 3.0;
        alpha1 = chord / 3.0;
    }
    let curve = CubicBezier {
        p0,
        p1: add(p0, multiply(start_tangent, alpha0)),
        p2: add(p3, multiply(end_tangent, alpha1)),
        p3,
    };
    if dot(subtract(curve.p1, p0), subtract(p3, p0)) < 0.0
        || dot(subtract(curve.p2, p3), subtract(p0, p3)) < 0.0
    {
        return None;
    }

    let mut maximum_error = 0.0f32;
    *worst_point = points.len() / 2;
    for index in 1..points.len() - 1 {
        let error = length(subtract(evaluate(&curve, parameters[index]), points[index]));
        if error > maximum_error {
            maximum_error = error;
            *worst_point = index;
        }
    }
    if maximum_error <= tolerance {
        Some(curve)
    } else {
        None
    }
}

fn fit_recursive(
    points: &[PointF],
    options: &VectorFittingOptions,
    output: &mut Vec<VectorSegment>,
    depth: i32,
) {
    if points.len() < 2 {
        return;
    }
    // 深度上限：病态输入（worst_point 始终贴边）时递归深度可达 O(n)，防止长笔画栈溢出。
    // 达限时退化为逐点折线输出（语义等价于逐段 Line，与 points.len()<=3 分支一致）
    if depth >= 24 {
        for &point in &points[1..] {
            output.push(VectorSegment {
                seg_type: VectorSegmentType::Line,
                line_end: point,
                cubic: CubicBezier {
                    p0: PointF { x: 0.0, y: 0.0 },
                    p1: PointF { x: 0.0, y: 0.0 },
                    p2: PointF { x: 0.0, y: 0.0 },
                    p3: PointF { x: 0.0, y: 0.0 },
                },
            });
        }
        return;
    }
    let mut maximum_line_distance = 0.0f32;
    let mut line_split = points.len() / 2;
    for index in 1..points.len() - 1 {
        let distance = point_segment_distance(points[index], points[0], *points.last().unwrap());
        if distance > maximum_line_distance {
            maximum_line_distance = distance;
            line_split = index;
        }
    }
    if maximum_line_distance <= options.line_tolerance {
        output.push(VectorSegment {
            seg_type: VectorSegmentType::Line,
            line_end: *points.last().unwrap(),
            cubic: CubicBezier {
                p0: PointF { x: 0.0, y: 0.0 },
                p1: PointF { x: 0.0, y: 0.0 },
                p2: PointF { x: 0.0, y: 0.0 },
                p3: PointF { x: 0.0, y: 0.0 },
            },
        });
        return;
    }

    let mut worst_point = line_split;
    if let Some(curve) = fit_single_cubic(points, options.curve_tolerance, &mut worst_point) {
        output.push(VectorSegment {
            seg_type: VectorSegmentType::CubicBezier,
            line_end: curve.p3,
            cubic: curve,
        });
        return;
    }
    if points.len() <= 3 {
        for &point in &points[1..] {
            output.push(VectorSegment {
                seg_type: VectorSegmentType::Line,
                line_end: point,
                cubic: CubicBezier {
                    p0: PointF { x: 0.0, y: 0.0 },
                    p1: PointF { x: 0.0, y: 0.0 },
                    p2: PointF { x: 0.0, y: 0.0 },
                    p3: PointF { x: 0.0, y: 0.0 },
                },
            });
        }
        return;
    }
    worst_point = worst_point.clamp(1, points.len() - 2);
    let left = &points[..=worst_point];
    let right = &points[worst_point..];
    fit_recursive(left, options, output, depth + 1);
    fit_recursive(right, options, output, depth + 1);
}

fn flatten_segments(start: PointF, segments: &[VectorSegment], tolerance: f32) -> Stroke {
    let mut result: Stroke = vec![start];
    let mut current = start;
    for segment in segments {
        if segment.seg_type == VectorSegmentType::CubicBezier {
            let mut curve = segment.cubic;
            curve.p0 = current;
            flatten_cubic(&curve, 0.01f32.max(tolerance), 0, &mut result);
            current = curve.p3;
        } else {
            if *result.last().unwrap() != segment.line_end {
                result.push(segment.line_end);
            }
            current = segment.line_end;
        }
    }
    result
}

fn candidate_is_safe(
    fallback: &Stroke,
    segments: &[VectorSegment],
    ink: &BinaryImage,
    options: &VectorFittingOptions,
) -> bool {
    if fallback.len() < 2 || segments.is_empty() {
        return false;
    }
    let candidate = flatten_segments(fallback[0], segments, options.flattening_tolerance);
    // 防御：候选点数过多时下方自交/双向距离为 O(n²)，直接拒绝拟合回退折线
    if candidate.len() < 2
        || candidate.len() > 4096
        || candidate[0] != fallback[0]
        || *candidate.last().unwrap() != *fallback.last().unwrap()
        || bidirectional_distance(&candidate, fallback) > options.curve_tolerance
        || !polyline_supported_by_ink(&candidate, ink)
        || self_intersection_count(&candidate) > self_intersection_count(fallback)
        || curvature_spike_count(&candidate) > curvature_spike_count(fallback)
    {
        return false;
    }
    let mut min_x = fallback[0].x;
    let mut max_x = fallback[0].x;
    let mut min_y = fallback[0].y;
    let mut max_y = fallback[0].y;
    for &p in fallback {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    candidate.iter().all(|&p| {
        p.x >= min_x - options.curve_tolerance
            && p.x <= max_x + options.curve_tolerance
            && p.y >= min_y - options.curve_tolerance
            && p.y <= max_y + options.curve_tolerance
    })
}

fn protected_corners(points: &Stroke, options: &VectorFittingOptions) -> Vec<usize> {
    let mut candidates: Vec<(usize, f32)> = Vec::new();
    for index in 1..points.len() - 1 {
        let mut left = index;
        let mut left_length = 0.0f32;
        while left > 0 && left_length < options.corner_arm_length {
            left_length += length(subtract(points[left], points[left - 1]));
            left -= 1;
        }
        let mut right = index;
        let mut right_length = 0.0f32;
        while right + 1 < points.len() && right_length < options.corner_arm_length {
            right_length += length(subtract(points[right + 1], points[right]));
            right += 1;
        }
        if left_length < 1.5 || right_length < 1.5 {
            continue;
        }
        let left_arm = normalize(subtract(points[left], points[index]));
        let right_arm = normalize(subtract(points[right], points[index]));
        let cosine = dot(left_arm, right_arm);
        if cosine > options.corner_cosine_threshold {
            candidates.push((index, cosine));
        }
    }
    candidates.sort_by_key(|&(index, _)| index);
    let mut result: Vec<usize> = Vec::new();
    for &(index, cosine) in &candidates {
        if let Some(&last) = result.last() {
            if index - last <= 2 {
                if let Some(prev) = candidates.iter().find(|&&(i, _)| i == last) {
                    if cosine > prev.1 {
                        *result.last_mut().unwrap() = index;
                    }
                }
                continue;
            }
        }
        result.push(index);
    }
    result
}

fn reverse_fitted_span(mut canonical: FittedRouteSpan, fallback: Stroke) -> FittedRouteSpan {
    let mut source: Vec<(PointF, VectorSegment)> = Vec::with_capacity(canonical.segments.len());
    let mut current = canonical.start;
    for segment in &canonical.segments {
        source.push((current, segment.clone()));
        current = if segment.seg_type == VectorSegmentType::CubicBezier {
            segment.cubic.p3
        } else {
            segment.line_end
        };
    }
    let mut reversed: Vec<VectorSegment> = Vec::with_capacity(source.len());
    for (old_start, old) in source.iter().rev() {
        if old.seg_type == VectorSegmentType::CubicBezier {
            reversed.push(VectorSegment {
                seg_type: VectorSegmentType::CubicBezier,
                line_end: *old_start,
                cubic: CubicBezier {
                    p0: old.cubic.p3,
                    p1: old.cubic.p2,
                    p2: old.cubic.p1,
                    p3: old.cubic.p0,
                },
            });
        } else {
            reversed.push(VectorSegment {
                seg_type: VectorSegmentType::Line,
                line_end: *old_start,
                cubic: CubicBezier {
                    p0: PointF { x: 0.0, y: 0.0 },
                    p1: PointF { x: 0.0, y: 0.0 },
                    p2: PointF { x: 0.0, y: 0.0 },
                    p3: PointF { x: 0.0, y: 0.0 },
                },
            });
        }
    }
    canonical.start = fallback[0];
    canonical.segments = reversed;
    canonical.fallback_polyline = fallback;
    std::mem::swap(
        &mut canonical.begin_anchor_flags,
        &mut canonical.end_anchor_flags,
    );
    canonical
}

fn fit_span(
    fallback: Stroke,
    region_type: LineRegionType,
    begin_flags: u8,
    end_flags: u8,
    ink: &BinaryImage,
    options: &VectorFittingOptions,
) -> FittedRouteSpan {
    let result = FittedRouteSpan {
        start: if fallback.is_empty() {
            PointF { x: 0.0, y: 0.0 }
        } else {
            fallback[0]
        },
        segments: Vec::new(),
        fallback_polyline: fallback.clone(),
        region_type,
        begin_anchor_flags: begin_flags,
        end_anchor_flags: end_flags,
        fitted: false,
    };
    if fallback.len() < 3
        || region_type == LineRegionType::Junction
        || region_type == LineRegionType::Ambiguous
    {
        return result;
    }
    let reverse = lexicographically_less(*fallback.last().unwrap(), fallback[0]);
    let mut canonical = fallback.clone();
    let mut canonical_begin = begin_flags;
    let mut canonical_end = end_flags;
    if reverse {
        canonical.reverse();
        std::mem::swap(&mut canonical_begin, &mut canonical_end);
    }

    let mut segments: Vec<VectorSegment> = Vec::new();
    fit_recursive(&canonical, options, &mut segments, 0);
    if segments.len() >= canonical.len() - 1
        || !candidate_is_safe(&canonical, &segments, ink, options)
    {
        return result;
    }
    let canonical_result = FittedRouteSpan {
        start: canonical[0],
        segments,
        fallback_polyline: canonical.clone(),
        region_type,
        begin_anchor_flags: canonical_begin,
        end_anchor_flags: canonical_end,
        fitted: true,
    };
    if reverse {
        reverse_fitted_span(canonical_result, fallback)
    } else {
        canonical_result
    }
}

/// 展平单个拟合跨度（对应 C++ FlattenFittedRouteSpan）
pub fn flatten_fitted_route_span(span: &FittedRouteSpan, tolerance: f32) -> Stroke {
    if !span.fitted || span.segments.is_empty() {
        return span.fallback_polyline.clone();
    }
    flatten_segments(span.start, &span.segments, tolerance)
}

/// 矢量拟合主入口（对应 C++ FitVectorDrawingPath）
pub fn fit_vector_drawing_path(
    path: &DrawingPath,
    allowed_ink: &BinaryImage,
    options: &VectorFittingOptions,
) -> VectorDrawingPath {
    let mut result = VectorDrawingPath {
        width: path.width,
        height: path.height,
        strokes: Vec::new(),
    };
    result.strokes.reserve(path.strokes.len());
    for (stroke_index, stroke) in path.strokes.iter().enumerate() {
        let metadata = if stroke_index < path.route_metadata.len()
            && !path.route_metadata[stroke_index].spans.is_empty()
        {
            path.route_metadata[stroke_index].clone()
        } else {
            StrokeRouteMetadata::default_for(stroke)
        };
        let mut vector_stroke = VectorStroke {
            spans: Vec::new(),
            closed: metadata.closed,
        };
        for span in &metadata.spans {
            // 防御：point_begin 也可能越界（外源/损坏 metadata），一并检查再切片
            if span.point_end >= stroke.len()
                || span.point_end <= span.point_begin
                || span.point_begin >= stroke.len()
            {
                continue;
            }
            let fallback: Stroke = stroke[span.point_begin..=span.point_end].to_vec();
            let mut boundaries: Vec<usize> = vec![0];
            let corners = protected_corners(&fallback, options);
            boundaries.extend(corners);
            boundaries.push(fallback.len() - 1);
            boundaries.sort_unstable();
            boundaries.dedup();
            for part in 1..boundaries.len() {
                let begin = boundaries[part - 1];
                let end = boundaries[part];
                if end <= begin {
                    continue;
                }
                let begin_flags = if part == 1 {
                    span.begin_anchor_flags
                } else {
                    ANCHOR_PROTECTED_CORNER
                };
                let end_flags = if part + 1 == boundaries.len() {
                    span.end_anchor_flags
                } else {
                    ANCHOR_PROTECTED_CORNER
                };
                let part_fallback: Stroke = fallback[begin..=end].to_vec();
                let fitted = fit_span(
                    part_fallback,
                    span.region_type,
                    begin_flags,
                    end_flags,
                    allowed_ink,
                    options,
                );
                vector_stroke.spans.push(fitted);
            }
        }
        result.strokes.push(vector_stroke);
    }
    result
}

/// 展平矢量路径为折线（对应 C++ FlattenVectorDrawingPath）
pub fn flatten_vector_drawing_path(path: &VectorDrawingPath, tolerance: f32) -> DrawingPath {
    let mut result = DrawingPath {
        width: path.width,
        height: path.height,
        strokes: Vec::new(),
        route_metadata: Vec::new(),
    };
    result.strokes.reserve(path.strokes.len());
    result.route_metadata.reserve(path.strokes.len());
    for vector_stroke in &path.strokes {
        let mut stroke: Stroke = Vec::new();
        let mut metadata = super::types::StrokeRouteMetadata {
            spans: Vec::new(),
            closed: vector_stroke.closed,
        };
        for span in &vector_stroke.spans {
            let flattened = flatten_fitted_route_span(span, tolerance);
            if flattened.len() < 2 {
                continue;
            }
            let offset = if stroke.is_empty() {
                0
            } else {
                stroke.len() - 1
            };
            if stroke.is_empty() {
                stroke = flattened;
            } else if *stroke.last().unwrap() == flattened[0] {
                stroke.extend_from_slice(&flattened[1..]);
            } else {
                // 已验证的矢量笔画必须连续；断裂时回退当前跨度无法修复结构边界
                stroke.clear();
                metadata.spans.clear();
                break;
            }
            metadata.spans.push(RouteSpan {
                point_begin: offset,
                point_end: stroke.len() - 1,
                region_type: span.region_type,
                begin_anchor_flags: span.begin_anchor_flags,
                end_anchor_flags: span.end_anchor_flags,
            });
        }
        if stroke.len() >= 2 {
            result.strokes.push(stroke);
            result.route_metadata.push(metadata);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_intersection_bbox_culling_preserves_crossing_count() {
        let crossing = vec![
            PointF { x: 0.0, y: 0.0 },
            PointF { x: 10.0, y: 10.0 },
            PointF { x: 0.0, y: 10.0 },
            PointF { x: 10.0, y: 0.0 },
        ];
        let disjoint = vec![
            PointF { x: 0.0, y: 0.0 },
            PointF { x: 1.0, y: 0.0 },
            PointF { x: 1.0, y: 1.0 },
            PointF { x: 2.0, y: 1.0 },
        ];
        assert_eq!(self_intersection_count(&crossing), 1);
        assert_eq!(self_intersection_count(&disjoint), 0);
    }
}
