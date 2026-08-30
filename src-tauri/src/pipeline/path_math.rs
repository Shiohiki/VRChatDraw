//! 路径数学（对应 C++ PathMath.cpp）：点到线段距离 / RDP 简化

use super::types::PointF;

/// 点到线段距离（对应 C++ PointSegmentDistance）
pub fn point_segment_distance(point: PointF, start: PointF, end: PointF) -> f32 {
    let delta_x = end.x - start.x;
    let delta_y = end.y - start.y;
    let length_squared = delta_x * delta_x + delta_y * delta_y;
    if length_squared <= 1.0e-8 {
        return ((point.x - start.x).powi(2) + (point.y - start.y).powi(2)).sqrt();
    }
    let projection = ((((point.x - start.x) * delta_x) + ((point.y - start.y) * delta_y))
        / length_squared)
        .clamp(0.0, 1.0);
    let closest = PointF {
        x: start.x + projection * delta_x,
        y: start.y + projection * delta_y,
    };
    ((point.x - closest.x).powi(2) + (point.y - closest.y).powi(2)).sqrt()
}

/// RDP 简化（对应 C++ SimplifyRdp）
pub fn simplify_rdp(points: &[PointF], epsilon: f32) -> Vec<PointF> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    fn simplify_range(
        points: &[PointF],
        first: usize,
        last: usize,
        epsilon: f32,
        depth: i32,
        keep: &mut [bool],
    ) {
        if last <= first + 1 || depth >= 24 {
            // depth 上限：退化输入（逐点剥离）时递归深度可达 O(n)，防止长笔画栈溢出
            return;
        }
        let mut maximum_distance = 0.0f32;
        let mut maximum_index = first;
        for index in first + 1..last {
            let distance = point_segment_distance(points[index], points[first], points[last]);
            if distance > maximum_distance {
                maximum_distance = distance;
                maximum_index = index;
            }
        }
        if maximum_distance <= epsilon || maximum_index == first {
            // maximum_index == first：区间内无更优切点（共线/epsilon<0 退化），
            // 必须返回，否则 simplify_range(maximum_index, last) 与原调用相同 → 无限递归
            return;
        }
        keep[maximum_index] = true;
        simplify_range(points, first, maximum_index, epsilon, depth + 1, keep);
        simplify_range(points, maximum_index, last, epsilon, depth + 1, keep);
    }

    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    simplify_range(points, 0, points.len() - 1, epsilon, 0, &mut keep);
    points
        .iter()
        .enumerate()
        .filter_map(|(index, &p)| if keep[index] { Some(p) } else { None })
        .collect()
}

/// 笔画长度（对应 C++ StrokeLength）
pub fn stroke_length(points: &[PointF]) -> f32 {
    let mut length = 0.0f32;
    for i in 1..points.len() {
        length += ((points[i].x - points[i - 1].x).powi(2)
            + (points[i].y - points[i - 1].y).powi(2))
        .sqrt();
    }
    length
}
