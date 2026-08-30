//! 管线数据结构（映射 C++ PathTypes.hpp / ImageProcessor.cpp 内部结构）

/// 解码后的 BGRA 图像（含 alpha，合成在消费处完成）
#[derive(Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// 三通道平面（均已合成白底）
pub struct ColorPlanes {
    pub red: Vec<u8>,
    pub green: Vec<u8>,
    pub blue: Vec<u8>,
}

/// PS 式多尺度线响应（对应 C++ PsLineResponse）
pub struct PsLineResponse {
    pub fused: Vec<u8>,
    pub scales: [Vec<u8>; 3],
}

/// 像素级线稿证据
#[derive(Clone, Copy, Default)]
pub struct LinePixelEvidence {
    pub confidence: u8,
    pub scale_mask: u8,
    pub flags: u8,
}

/// 证据标志（对应 C++ LineEvidenceFlags）
pub const EVIDENCE_STRONG_CORE: u8 = 1 << 0;
pub const EVIDENCE_ORIGINAL_DARK: u8 = 1 << 1;
pub const EVIDENCE_RGB_BOUNDARY: u8 = 1 << 2;
pub const EVIDENCE_REPAIRED_GAP: u8 = 1 << 3;

/// 规范线稿提取结果：二值拓扑图 + 像素证据
pub struct CanonicalLineArtExtraction {
    pub topology: Vec<u8>,
    pub evidence: Vec<LinePixelEvidence>,
}

/// 二值图（0=背景，1=可绘制黑色线条像素）
pub struct BinaryImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// 矢量段类型（映射 C++ VectorSegmentType）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorSegmentType {
    Line,
    CubicBezier,
}

/// 三次贝塞尔曲线
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub p0: PointF,
    pub p1: PointF,
    pub p2: PointF,
    pub p3: PointF,
}

/// 矢量段
#[derive(Clone, Debug)]
pub struct VectorSegment {
    pub seg_type: VectorSegmentType,
    pub line_end: PointF,
    pub cubic: CubicBezier,
}

/// 拟合后的路由跨度
#[derive(Clone)]
pub struct FittedRouteSpan {
    pub start: PointF,
    pub segments: Vec<VectorSegment>,
    pub fallback_polyline: Stroke,
    pub region_type: LineRegionType,
    pub begin_anchor_flags: u8,
    pub end_anchor_flags: u8,
    pub fitted: bool,
}

/// 矢量笔画
pub struct VectorStroke {
    pub spans: Vec<FittedRouteSpan>,
    pub closed: bool,
}

/// 矢量绘制路径
pub struct VectorDrawingPath {
    pub width: u32,
    pub height: u32,
    pub strokes: Vec<VectorStroke>,
}

/// 浮点路径点
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointF {
    pub x: f32,
    pub y: f32,
}

/// 一条笔画
pub type Stroke = Vec<PointF>;

/// 区域分类类型（映射 C++ LineRegionType）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LineRegionType {
    ThinLine = 0,
    ElongatedThickStroke = 1,
    CompactFill = 2,
    Junction = 3,
    Ambiguous = 4,
}

impl LineRegionType {
    pub fn from_u8(value: u8) -> LineRegionType {
        match value {
            0 => LineRegionType::ThinLine,
            1 => LineRegionType::ElongatedThickStroke,
            2 => LineRegionType::CompactFill,
            3 => LineRegionType::Junction,
            _ => LineRegionType::Ambiguous,
        }
    }
}

/// 路由锚点标志（映射 C++ RouteAnchorFlags）
pub const ANCHOR_NONE: u8 = 0;
pub const ANCHOR_GRAPH_ENDPOINT: u8 = 1 << 0;
pub const ANCHOR_JUNCTION_PORT: u8 = 1 << 1;
pub const ANCHOR_REGION_BOUNDARY: u8 = 1 << 2;
pub const ANCHOR_CLOSED_SEAM: u8 = 1 << 3;
/// 拐角拆分产生的受保护锚点（vector_fit.rs protected_corners 使用）
pub const ANCHOR_PROTECTED_CORNER: u8 = 1 << 4;

/// 路由跨度（连续点索引，相邻跨度共享边界点）
#[derive(Clone, Copy)]
pub struct RouteSpan {
    pub point_begin: usize,
    pub point_end: usize,
    pub region_type: LineRegionType,
    pub begin_anchor_flags: u8,
    pub end_anchor_flags: u8,
}

/// 笔画路由元数据
#[derive(Clone)]
pub struct StrokeRouteMetadata {
    pub spans: Vec<RouteSpan>,
    pub closed: bool,
}

impl StrokeRouteMetadata {
    /// 默认元数据：整笔单跨度（Ambiguous + 端点锚点），闭笔时加 CLOSED_SEAM。
    /// 供 optimize / vector_fit 在元数据缺失或不可信时回退使用
    pub fn default_for(stroke: &Stroke) -> Self {
        let closed = stroke.len() >= 3 && stroke[0] == *stroke.last().unwrap();
        let mut metadata = StrokeRouteMetadata {
            spans: Vec::new(),
            closed,
        };
        if stroke.len() >= 2 {
            let mut begin = ANCHOR_GRAPH_ENDPOINT;
            let mut end = ANCHOR_GRAPH_ENDPOINT;
            if closed {
                begin |= ANCHOR_CLOSED_SEAM;
                end |= ANCHOR_CLOSED_SEAM;
            }
            metadata.spans.push(RouteSpan {
                point_begin: 0,
                point_end: stroke.len() - 1,
                region_type: LineRegionType::Ambiguous,
                begin_anchor_flags: begin,
                end_anchor_flags: end,
            });
        }
        metadata
    }
}

/// 绘制路径
pub struct DrawingPath {
    pub width: u32,
    pub height: u32,
    pub strokes: Vec<Stroke>,
    pub route_metadata: Vec<StrokeRouteMetadata>,
}

/// 路由骨架数据（阶段 4/5 中间结构）
pub struct RouteSkeletonData {
    pub skeleton: Vec<u8>,
    pub region_types: Vec<u8>,
    pub ink_distance: Vec<f32>,
}
