// 笔画坐标 DTO：相册缓存需要序列化能力，故加 serde 派生
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawingPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DrawingStroke {
    pub points: Vec<DrawingPoint>,
}
