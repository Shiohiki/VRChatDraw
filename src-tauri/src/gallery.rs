//! 笔画相册缓存：F9 有效发起绘制时按图片内容哈希保存笔画与缩略图，
//! 重新导入同一图片时可一键恢复；同时提供列表/删除管理能力。
//! 存储位置：%APPDATA%\VRChatDraw\stroke_cache\<sha256>.json
//! （每图一条、单条目覆盖，最多 MAX_ENTRIES 条，超出淘汰最旧）

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::types::DrawingStroke;

const MAX_ENTRIES: usize = 30;
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_THUMB_SIDE: u32 = 320;

/// 裁剪区域（根来源图坐标系，像素）
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// 完整相册条目（磁盘格式）
#[derive(Clone, Serialize, Deserialize)]
pub struct GalleryEntry {
    /// 图片文件内容的 SHA-256（hex）；裁剪/剪贴板按各自缓存文件计算
    pub image_hash: String,
    /// 根来源图内容哈希（hex）：文件导入=原文件字节；剪贴板=原始像素；裁剪时继承。
    /// 旧格式无此字段（反序列化为空串），读取时归一化为 image_hash（视为自身来源的全图版）
    #[serde(default)]
    pub source_hash: String,
    /// 生成时的裁剪区域（根来源图坐标系）；None = 全图
    #[serde(default)]
    pub crop: Option<CropRect>,
    pub image_name: String,
    pub image_size: String, // "宽×高"
    /// ≤320px JPEG data URL（生成失败为空串，前端用底色占位）
    pub thumbnail: String,
    pub strokes: Vec<DrawingStroke>,
    pub stroke_count: usize,
    pub point_count: usize,
    /// Unix 纳秒
    pub saved_at: u128,
    pub estimate_seconds: f64,
    // 生成参数快照（仅展示用途，绘制期参数在绘制时另读当前配置）
    pub blur_size: u32,
    pub epsilon_ratio: f64,
    pub use_ai: bool,
    pub ai_fallback: bool,
    pub ai_model: String,
    pub ai_endpoint: String,
}

/// 列表/提示用视图（不含笔画本体，避免把全部笔画搬进前端）。
/// list() 直接反序列化本视图：serde 对未声明的 strokes 字段做令牌扫描而不分配
/// Vec，打开相册不再为每条目构建完整笔画数据
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct GalleryView {
    pub image_hash: String,
    /// 根来源图内容哈希（旧格式归一化后 = 自身内容哈希）
    #[serde(default)]
    pub source_hash: String,
    /// 生成时的裁剪区域；None = 全图
    #[serde(default)]
    pub crop: Option<CropRect>,
    pub image_name: String,
    pub image_size: String,
    pub thumbnail: String,
    pub stroke_count: usize,
    pub point_count: usize,
    pub saved_at: u128,
    pub estimate_seconds: f64,
    pub use_ai: bool,
    pub ai_fallback: bool,
}

impl GalleryView {
    /// 旧格式条目（source_hash 为空）归一化为自身内容哈希（视为自身来源的全图版）。
    /// 所有 GalleryView 反序列化路径（list / lookup_by_source / lookup_view）都必须
    /// 经过这里，否则来源过滤会漏掉旧格式条目、前端来源匹配会拿到空串。
    fn normalized(mut self) -> Self {
        if self.source_hash.is_empty() {
            self.source_hash = self.image_hash.clone();
        }
        self
    }
}

/// 哈希标识格式校验（64 位 hex）：拒绝后同时防目录穿越
pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

fn cache_dir() -> PathBuf {
    crate::storage::data_path("stroke_cache")
}

fn entry_path(hash: &str) -> PathBuf {
    cache_dir().join(format!("{hash}.json"))
}

pub fn lookup(hash: &str) -> Option<GalleryEntry> {
    if !valid_hash(hash) {
        return None;
    }
    let path = entry_path(hash);
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        eprintln!("相册条目过大，忽略：{path:#?}");
        return None;
    }
    match serde_json::from_slice::<GalleryEntry>(&bytes) {
        Ok(entry) => Some(entry),
        Err(e) => {
            eprintln!("相册条目解析失败：{path:#?}: {e}");
            None
        }
    }
}

/// 按内容哈希读取条目视图（不含笔画本体的令牌扫描）：
/// 供 gallery_check 精确命中与缩略图复用等只需要少量字段的路径，
/// 避免为读取缩略图/元数据完整反序列化可达数十 MB 的 strokes。
/// 旧格式条目归一化与 lookup 一致（source_hash 为空 → 自身内容哈希）。
pub fn lookup_view(hash: &str) -> Option<GalleryView> {
    if !valid_hash(hash) {
        return None;
    }
    let path = entry_path(hash);
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return None;
    }
    match serde_json::from_slice::<GalleryView>(&bytes) {
        Ok(view) => Some(view.normalized()),
        Err(e) => {
            eprintln!("相册条目解析失败：{path:#?}: {e}");
            None
        }
    }
}

/// 全部条目视图，按保存时间倒序；损坏条目跳过并记录日志
pub fn list() -> Vec<GalleryView> {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return Vec::new();
    };
    let mut views = Vec::new();
    for item in entries.flatten() {
        let path = item.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            continue;
        }
        match serde_json::from_slice::<GalleryView>(&bytes) {
            Ok(view) => views.push(view.normalized()),
            Err(e) => eprintln!("相册条目解析失败（跳过）：{path:#?}: {e}"),
        }
    }
    views.sort_by_key(|view| std::cmp::Reverse(view.saved_at));
    views
}

/// 按根来源图哈希查询全部变体：全图版优先，其余按保存时间倒序。
/// 视图化解析（不含笔画本体的令牌扫描）：目录扫描只为按来源过滤，
/// 无需为每个条目反序列化完整 strokes（单条目上限 64MB）。
/// 旧格式条目经 normalized 归一化（source_hash = 自身内容哈希），天然只归入自身名下
pub fn lookup_by_source(source_hash: &str) -> Vec<GalleryView> {
    if !valid_hash(source_hash) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return Vec::new();
    };
    let mut views = Vec::new();
    for item in entries.flatten() {
        let path = item.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            continue;
        }
        match serde_json::from_slice::<GalleryView>(&bytes) {
            Ok(view) => {
                let view = view.normalized();
                if view.source_hash == source_hash {
                    views.push(view);
                }
            }
            Err(e) => eprintln!("相册条目解析失败（跳过）：{path:#?}: {e}"),
        }
    }
    // Reverse 元组：is_none()=true（全图版）排在最前，其后 saved_at 倒序
    views.sort_by_key(|view| std::cmp::Reverse((view.crop.is_none(), view.saved_at)));
    views
}

pub fn delete(hash: &str) -> Result<(), String> {
    if !valid_hash(hash) {
        return Err("相册条目标识无效".to_string());
    }
    std::fs::remove_file(entry_path(hash)).map_err(|e| format!("删除相册条目失败：{e}"))
}

/// 原子写入条目并淘汰超出上限的最旧条目（按 saved_at）
pub fn save(entry: &GalleryEntry) -> Result<(), String> {
    if !valid_hash(&entry.image_hash) {
        return Err("相册条目标识无效".to_string());
    }
    crate::storage::ensure_app_data_dir()?;
    std::fs::create_dir_all(cache_dir()).map_err(|e| format!("创建相册目录失败：{e}"))?;
    let json = serde_json::to_string(entry).map_err(|e| format!("序列化失败：{e}"))?;
    crate::storage::atomic_write(&entry_path(&entry.image_hash), json.as_bytes())?;
    prune();
    Ok(())
}

fn prune() {
    // 常规路径零解析：条目数未超上限时排序后 skip(MAX_ENTRIES) 恒为空（不删任何条目），
    // 直接返回即可，避免每次保存都全量反序列化全部条目（单条目上限 64MB）。
    // 目录项计数 ≥ json 条目数，故"计数 ≤ 上限"时行为与全量排序严格等价。
    let file_count = std::fs::read_dir(cache_dir())
        .map(|entries| entries.count())
        .unwrap_or(0);
    if file_count <= MAX_ENTRIES {
        return;
    }
    let mut entries: Vec<(u128, PathBuf)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(cache_dir()) {
        for item in dir.flatten() {
            let path = item.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") {
                continue;
            }
            let saved_at = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<GalleryEntry>(&bytes).ok())
                .map(|entry| entry.saved_at)
                .unwrap_or(0);
            entries.push((saved_at, path));
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_ts, path) in entries.into_iter().skip(MAX_ENTRIES) {
        let _ = std::fs::remove_file(path);
    }
}

/// 从图片文件生成 ≤320px 的 JPEG 缩略图 data URL。
/// 失败返回空串（不阻断保存），前端用底色占位。
pub fn make_thumbnail(image_path: &str) -> String {
    let Ok(img) = crate::load_image_any(image_path) else {
        return String::new();
    };
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return String::new();
    }
    let scale = (MAX_THUMB_SIDE as f32 / w.max(h) as f32).min(1.0);
    let thumb = if scale < 1.0 {
        image::imageops::resize(
            &crate::pipeline::basic::to_rgb_on_white(&img),
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        crate::pipeline::basic::to_rgb_on_white(&img)
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
    if encoder
        .encode(
            &thumb,
            thumb.width(),
            thumb.height(),
            image::ColorType::Rgb8,
        )
        .is_err()
    {
        return String::new();
    }
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(buf.into_inner())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DrawingPoint;
    use std::sync::Mutex;

    // 三个相册测试共享同一缓存目录（cfg(test) 的 app_data_dir 按进程唯一），
    // 并行执行会互相污染（prune 甚至会把另一测试刚写入的条目淘汰），
    // 用互斥锁串行化并各自清空目录保证隔离。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn clear_cache() {
        let dir = cache_dir();
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn test_entry(hash: String, saved_at: u128) -> GalleryEntry {
        GalleryEntry {
            image_hash: hash.clone(),
            source_hash: hash,
            crop: None,
            image_name: "测试.png".to_string(),
            image_size: "8×6".to_string(),
            thumbnail: String::new(),
            strokes: vec![DrawingStroke {
                points: vec![
                    DrawingPoint { x: 1.0, y: 2.0 },
                    DrawingPoint { x: 3.0, y: 4.0 },
                ],
            }],
            stroke_count: 1,
            point_count: 2,
            saved_at,
            estimate_seconds: 0.5,
            blur_size: 1,
            epsilon_ratio: 1.5,
            use_ai: false,
            ai_fallback: false,
            ai_model: String::new(),
            ai_endpoint: "images/edits".to_string(),
        }
    }

    #[test]
    fn save_lookup_delete_round_trip() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache();
        let entry = test_entry("a".repeat(64), 1);
        save(&entry).unwrap();
        let got = lookup(&entry.image_hash).expect("lookup 应命中");
        assert_eq!(got.strokes.len(), 1);
        assert_eq!(got.strokes[0].points.len(), 2);
        assert_eq!(list().len(), 1);
        delete(&entry.image_hash).unwrap();
        assert!(lookup(&entry.image_hash).is_none());
        clear_cache();
    }

    #[test]
    fn invalid_hash_rejected_everywhere() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        assert!(!valid_hash("abc"));
        assert!(!valid_hash("../etc/passwd"));
        assert!(!valid_hash(&"g".repeat(64)));
        assert!(valid_hash(&"f".repeat(64)));
        assert!(lookup("../../x").is_none());
        assert!(delete("../x").is_err());
        assert!(save(&test_entry("../x".to_string(), 1)).is_err());
    }

    #[test]
    fn source_grouping_and_legacy_normalization() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache();
        let source = "a".repeat(64);
        let full = {
            let mut entry = test_entry("b".repeat(64), 1); // 来源 a 的全图版
            entry.source_hash = source.clone();
            entry
        };
        let cropped = {
            let mut entry = test_entry("c".repeat(64), 2); // 来源 a 的裁剪版
            entry.source_hash = source.clone();
            entry.crop = Some(CropRect {
                x: 10,
                y: 20,
                w: 300,
                h: 200,
            });
            entry
        };
        let legacy = {
            // 旧格式条目：无 source_hash 字段（反序列化为空串）
            let mut entry = test_entry("d".repeat(64), 3);
            entry.source_hash = String::new();
            entry
        };
        for entry in [&full, &cropped, &legacy] {
            save(entry).unwrap();
        }
        // 来源 a 命中 2 条：全图版优先，其余按时间倒序
        let variants = lookup_by_source(&source);
        assert_eq!(variants.len(), 2, "来源 a 应命中 2 条");
        assert!(variants[0].crop.is_none(), "全图版应排在最前");
        assert_eq!(variants[0].image_hash, full.image_hash);
        assert_eq!(variants[1].image_hash, cropped.image_hash);
        // 旧格式归一化：source_hash 为空 → 视为自身内容哈希（只归入自身名下）
        let own = lookup_by_source(&legacy.image_hash);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].source_hash, legacy.image_hash);
        assert!(
            lookup_by_source(&full.image_hash)
                .iter()
                .all(|v| v.image_hash != cropped.image_hash),
            "裁剪版不得归入全图版条目的来源名下"
        );
        // lookup_view 视图路径与 lookup 同源归一化（旧格式 source_hash → 自身哈希）
        let legacy_view = lookup_view(&legacy.image_hash).expect("lookup_view 应命中旧格式条目");
        assert_eq!(legacy_view.source_hash, legacy.image_hash);
        assert!(
            lookup_view(&"e".repeat(64)).is_none(),
            "不存在的条目应返回 None"
        );
        clear_cache();
    }

    #[test]
    fn prune_keeps_newest_thirty() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache();
        // 写 31 条，最旧的 1 条应被淘汰
        for i in 0..=MAX_ENTRIES {
            let hash = format!("{i:064x}");
            save(&test_entry(hash, i as u128)).unwrap();
        }
        let views = list();
        assert_eq!(views.len(), MAX_ENTRIES);
        assert!(lookup(&format!("{:064x}", 0)).is_none(), "最旧条目应被淘汰");
        assert!(
            lookup(&format!("{:064x}", MAX_ENTRIES)).is_some(),
            "最新条目应保留"
        );
        clear_cache();
    }
}
