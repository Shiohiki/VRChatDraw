//! AI 预处理模块：通过 OpenAI 兼容接口把彩色图片转为黑白线稿。
//! 配置保存在用户数据目录的 config.json 中，Base URL 可自定义（方便切换中转站）。

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;

/// AI 预处理配置
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    pub api_base_url: String, // OpenAI 兼容接口地址，如 https://api.openai.com/v1
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[cfg_attr(windows, serde(skip_serializing))]
    pub api_key: String,
    /// Windows builds persist this field with DPAPI and never serialize api_key.
    /// It remains empty in runtime state and in frontend payloads.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) api_key_protected: String,
    pub model: String, // 如 gpt-image-1
    /// 生图接口后缀：images/edits（默认）| chat/completions
    /// 不同中转站兼容的后缀不一致，由用户在设置中下拉选择
    pub api_endpoint: String,
    /// 仅用于前端保存请求，不写入配置文件。
    #[serde(default, skip_serializing)]
    pub clear_api_key: bool,
}

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_BASE64_BYTES: usize = 28 * 1024 * 1024;
const MAX_DECODED_PIXELS: u64 = 40_000_000;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_MODELS: usize = 200;
const MAX_MODEL_ID_BYTES: usize = 256;

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            api_base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            api_key_protected: String::new(),
            model: "gpt-image-1".to_string(),
            api_endpoint: "images/edits".to_string(),
            clear_api_key: false,
        }
    }
}

/// 返回给前端的 AI 配置视图。Key 本体只留在 Rust 状态中，前端仅知道是否已配置；
/// 用户若要替换可直接输入新值，留空表示保持原值。
#[derive(Clone, serde::Serialize)]
pub struct AiConfigView {
    pub api_base_url: String,
    pub api_key_set: bool,
    pub model: String,
    pub api_endpoint: String,
}

impl AiConfigView {
    pub fn from_config(cfg: &AiConfig) -> Self {
        Self {
            api_base_url: cfg.api_base_url.clone(),
            api_key_set: !cfg.api_key.trim().is_empty(),
            model: cfg.model.clone(),
            api_endpoint: cfg.api_endpoint.clone(),
        }
    }
}

/// 配置文件路径：用户数据目录中的 config.json
pub fn config_path() -> PathBuf {
    crate::storage::data_path("config.json")
}

/// 加载 AI 配置并返回错误信息：解析失败时返回默认值 + 错误说明，
/// 避免 API Key/URL 因格式错误被静默清空
pub fn load_config_with_error() -> (AiConfig, Option<String>) {
    let Some((path, raw_bytes)) = (match crate::storage::read_preferred("config.json") {
        Ok(value) => value,
        Err(error) => {
            return (
                AiConfig::default(),
                Some(format!(
                    "无法读取 AI 配置文件：{error}（已使用默认 AI 配置）"
                )),
            );
        }
    }) else {
        return (AiConfig::default(), None);
    };
    if raw_bytes.len() as u64 > MAX_CONFIG_BYTES {
        let backup = crate::storage::preserve_corrupt(&path)
            .map(|path| format!("损坏文件已保留为 {path:#?}"))
            .unwrap_or_else(|error| format!("无法备份损坏文件：{error}"));
        return (
            AiConfig::default(),
            Some(format!("{path:#?} 超过 1 MB，已使用默认 AI 配置；{backup}")),
        );
    }
    let raw = match String::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(e) => {
            let backup = crate::storage::preserve_corrupt(&path)
                .map(|path| format!("损坏文件已保留为 {path:#?}"))
                .unwrap_or_else(|backup_error| format!("无法备份损坏文件：{backup_error}"));
            return (
                AiConfig::default(),
                Some(format!(
                    "{path:#?} 不是有效 UTF-8，已使用默认 AI 配置：{e}；{backup}"
                )),
            );
        }
    };
    match serde_json::from_str::<AiConfig>(&raw) {
        Ok(mut cfg) => {
            #[cfg(windows)]
            if let Err(error) = restore_api_key(&mut cfg) {
                return (
                    AiConfig::default(),
                    Some(format!(
                        "AI 配置中的 API Key 无法解密，已使用默认 AI 配置：{error}"
                    )),
                );
            }
            #[cfg(not(windows))]
            if !cfg.api_key_protected.trim().is_empty() {
                return (
                    AiConfig::default(),
                    Some("AI 配置使用了 Windows DPAPI，当前平台无法解密 API Key".to_string()),
                );
            }
            cfg.api_key_protected.clear();
            normalize_config(&mut cfg);
            (cfg, None)
        }
        Err(error) => {
            let backup = crate::storage::preserve_corrupt(&path)
                .map(|path| format!("损坏文件已保留为 {path:#?}"))
                .unwrap_or_else(|backup_error| format!("无法备份损坏文件：{backup_error}"));
            (
                AiConfig::default(),
                Some(format!(
                    "{path:#?} 解析失败，已使用默认 AI 配置：{error}；{backup}"
                )),
            )
        }
    }
}

pub fn save_config(cfg: &AiConfig) -> Result<(), String> {
    let mut normalized = cfg.clone();
    normalize_config(&mut normalized);
    #[cfg(windows)]
    {
        normalized.api_key_protected = protect_api_key(&normalized.api_key)?;
        normalized.api_key.clear();
    }
    #[cfg(not(windows))]
    normalized.api_key_protected.clear();
    let json = serde_json::to_string_pretty(&normalized).map_err(|e| e.to_string())?;
    let path = config_path();
    // tmp 带进程号唯一后缀：避免多写入路径同时写同一 tmp 互相覆盖
    crate::storage::atomic_write(&path, json.as_bytes())
}

#[cfg(windows)]
fn protect_api_key(value: &str) -> Result<String, String> {
    use std::ptr::null;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

    if value.is_empty() {
        return Ok(String::new());
    }
    let length = u32::try_from(value.len())
        .map_err(|_| "API Key 长度超出 Windows DPAPI 限制".to_string())?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: value.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let result = unsafe {
        CryptProtectData(
            &input,
            null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if result == 0 || output.pbData.is_null() {
        return Err(format!("Windows DPAPI 加密失败（错误码 {}）", unsafe {
            GetLastError()
        }));
    }
    let protected =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(format!(
        "dpapi:{}",
        base64::engine::general_purpose::STANDARD.encode(protected)
    ))
}

#[cfg(windows)]
fn restore_api_key(cfg: &mut AiConfig) -> Result<(), String> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    if cfg.api_key_protected.trim().is_empty() {
        return Ok(());
    }
    let encoded = cfg
        .api_key_protected
        .strip_prefix("dpapi:")
        .ok_or_else(|| "API Key 的加密格式无法识别".to_string())?;
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "API Key 的加密数据已损坏".to_string())?;
    let length = u32::try_from(encrypted.len())
        .map_err(|_| "API Key 的加密数据超出 Windows DPAPI 限制".to_string())?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let result =
        unsafe { CryptUnprotectData(&input, null_mut(), null(), null(), null(), 0, &mut output) };
    if result == 0 || output.pbData.is_null() {
        return Err(format!("Windows DPAPI 解密失败（错误码 {}）", unsafe {
            GetLastError()
        }));
    }
    let decrypted =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    cfg.api_key =
        String::from_utf8(decrypted).map_err(|_| "API Key 的解密结果不是有效文本".to_string())?;
    Ok(())
}

fn normalize_config(cfg: &mut AiConfig) {
    cfg.api_base_url = cfg.api_base_url.trim().trim_end_matches('/').to_string();
    if validate_api_base_url(&cfg.api_base_url).is_err() {
        cfg.api_base_url = AiConfig::default().api_base_url;
    }
    cfg.model = cfg.model.trim().to_string();
    if cfg.model.is_empty() {
        cfg.model = AiConfig::default().model;
    }
    let endpoint = cfg.api_endpoint.trim().trim_start_matches('/');
    cfg.api_endpoint = match endpoint {
        "chat/completions" | "images/edits" => endpoint.to_string(),
        _ => "images/edits".to_string(),
    };
    cfg.clear_api_key = false;
}

/// Validate the user-configured API endpoint before it is persisted or used.
/// Plain HTTP is allowed for loopback and private/local networks only, so an
/// API key is not sent over the public internet without encryption.
pub fn validate_api_base_url(value: &str) -> Result<(), String> {
    let value = value.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(value).map_err(|error| format!("API 地址无效：{error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("API 地址只能使用 http:// 或 https://".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("API 地址不能包含用户名或密码".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "API 地址缺少主机名".to_string())?;
    if url.scheme() == "http" {
        // url crate 的 host_str() 对 IPv6 返回带方括号的序列化形式（如 "[::1]"），
        // 直接 parse::<IpAddr> 必然失败，会把本机 IPv6 端点误判为公网地址；
        // 仅当两侧方括号成对时剥除后再判定
        let host_ip = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        let allowed = host.eq_ignore_ascii_case("localhost")
            || host_ip.parse::<IpAddr>().is_ok_and(is_private_or_local_ip);
        if !allowed {
            return Err(
                "非本机/内网 API 必须使用 HTTPS，以保护 API Key（局域网中转可填 http://内网IP:端口）"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn is_private_or_local_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && (18..=19).contains(&octets[1]))
        }
        IpAddr::V6(ip) => {
            // IPv4-mapped 字面量（::ffff:a.b.c.d）按还原后的 IPv4 判定：
            // 否则 ::ffff:192.168.1.1 这类内网地址会绕过全部 V6 判据
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_private_or_local_ip(IpAddr::V4(v4));
            }
            ip.is_unique_local()
                || ip.is_loopback()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
    }
}

/// 已知残余风险：DNS 重绑定。校验时 `to_socket_addrs` 解析出的地址与
/// reqwest 随后按主机名重连时解析的结果可能不同，理论上可绕过内网地址拒绝。
/// 本工具面向个人桌面使用，攻击者需先控制目标主机名的 DNS 解析，超出威胁模型，
/// 故只做解析校验而不固定 IP 直连（后者会破坏 TLS SNI 校验）。
fn validate_remote_image_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|error| format!("图片 URL 无效：{error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("接口返回的图片链接只能使用 HTTP(S)".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("接口返回的图片链接不能包含用户名或密码".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "接口返回的图片链接缺少主机名".to_string())?;
    let explicit_localhost = host.eq_ignore_ascii_case("localhost");
    if url.scheme() == "http" && !explicit_localhost {
        return Err("接口返回的远程图片必须使用 HTTPS（本机地址除外）".to_string());
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| "接口返回的图片链接缺少有效端口".to_string())?;
    let addresses: Vec<IpAddr> = if let Ok(address) = host.parse::<IpAddr>() {
        vec![address]
    } else {
        (host, port)
            .to_socket_addrs()
            .map_err(|error| format!("无法解析图片链接主机：{error}"))?
            .map(|address| address.ip())
            .collect()
    };
    if addresses.is_empty() {
        return Err("图片链接主机没有可用地址".to_string());
    }
    if !explicit_localhost
        && addresses
            .iter()
            .any(|address| is_private_or_local_ip(*address))
    {
        return Err("已拒绝指向内网或本机地址的远程图片链接".to_string());
    }
    Ok(())
}

/// 纯黑白线稿转换提示词：白底、黑色线条、无阴影、无渐变、高对比
const LINEART_PROMPT: &str = "Convert this image to a clean black and white line art drawing: \
pure white background, black strokes only, no shading, no gradients, no colors, \
high contrast, clear outlines. Keep the composition and main shapes unchanged.";

/// 构造带超时的阻塞 HTTP 客户端
/// connect_timeout 单独限制 TCP 建连（错 IP/防火墙挂起时快速失败，而非等满整体 timeout）
fn build_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败：{e}"))
}

/// 公共：GET /models 并返回 data 数组（校验 Base URL + API Key + 解析错误）
fn fetch_models_data(cfg: &AiConfig, timeout_secs: u64) -> Result<serde_json::Value, String> {
    validate_api_base_url(&cfg.api_base_url)?;
    let base = cfg.api_base_url.trim().trim_end_matches('/');
    let client = build_client(timeout_secs)?;
    let resp = client
        .get(format!("{base}/models"))
        .bearer_auth(cfg.api_key.trim())
        .send()
        .map_err(|e| format!("连接失败：{e}"))?;
    let status = resp.status();
    let body = read_response_limited(resp, MAX_RESPONSE_BYTES)?;
    let body = String::from_utf8_lossy(&body);
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| body.chars().take(200).collect());
        return Err(format!("API 错误（HTTP {status}）：{msg}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("响应解析失败：{e}"))?;
    Ok(v["data"].clone())
}

/// 从 JSON 响应中提取图像字节：兼容 images/* 的 data[0].b64_json、
/// data[0].url（托管链接，仅 http/https 二次下载）、
/// chat/completions 的 data[0].content[0].image_url.url（data URL 或裸 b64）
fn try_image_candidate(
    value: &str,
    client: &reqwest::blocking::Client,
    allow_raw_base64: bool,
    last_error: &mut Option<String>,
) -> Option<Vec<u8>> {
    let bytes = if value.starts_with("http://") || value.starts_with("https://") {
        match fetch_remote_image(client, value) {
            Ok(bytes) => bytes,
            Err(error) => {
                *last_error = Some(error);
                return None;
            }
        }
    } else {
        let is_data_url = value.starts_with("data:");
        if !is_data_url && !allow_raw_base64 {
            return None;
        }
        if allow_raw_base64 && !is_data_url && value.len() < 64 {
            return None;
        }
        let encoded = value
            .strip_prefix("data:")
            .and_then(|rest| rest.find(',').map(|index| &rest[index + 1..]))
            .unwrap_or(value);
        match decode_base64_limited(encoded) {
            Ok(bytes) => bytes,
            Err(error) => {
                *last_error = Some(error);
                return None;
            }
        }
    };

    match image::load_from_memory(&bytes) {
        Ok(image) if image.width() as u64 * image.height() as u64 <= MAX_DECODED_PIXELS => {
            Some(bytes)
        }
        Ok(_) => {
            *last_error = Some("接口返回的图片像素数过大".to_string());
            None
        }
        Err(_) => None,
    }
}

fn inspect_content(
    content: &serde_json::Value,
    client: &reqwest::blocking::Client,
    last_error: &mut Option<String>,
) -> Option<Vec<u8>> {
    let items = content
        .as_array()
        .map(|items| items.as_slice())
        .unwrap_or_else(|| std::slice::from_ref(content));
    for item in items {
        if let Some(url) = item["image_url"]["url"].as_str() {
            if let Some(bytes) = try_image_candidate(url, client, false, last_error) {
                return Some(bytes);
            }
        }
        if let Some(url) = item["image_url"].as_str() {
            if let Some(bytes) = try_image_candidate(url, client, false, last_error) {
                return Some(bytes);
            }
        }
        if let Some(text) = item["text"].as_str() {
            if let Some(bytes) = try_image_candidate(text, client, true, last_error) {
                return Some(bytes);
            }
        }
    }
    None
}

fn find_image_candidate(
    v: &serde_json::Value,
    client: &reqwest::blocking::Client,
) -> Result<Option<Vec<u8>>, String> {
    let mut last_error = None;
    let mut inspect_item = |item: &serde_json::Value| -> Option<Vec<u8>> {
        if let Some(b64) = item["b64_json"].as_str() {
            if let Some(bytes) = try_image_candidate(b64, client, true, &mut last_error) {
                return Some(bytes);
            }
        }
        if let Some(url) = item["url"].as_str() {
            if let Some(bytes) = try_image_candidate(url, client, false, &mut last_error) {
                return Some(bytes);
            }
        }
        if let Some(bytes) = inspect_content(&item["content"], client, &mut last_error) {
            return Some(bytes);
        }
        if let Some(bytes) = inspect_content(&item["images"], client, &mut last_error) {
            return Some(bytes);
        }
        None
    };

    if let Some(items) = v["data"].as_array() {
        for item in items {
            if let Some(bytes) = inspect_item(item) {
                return Ok(Some(bytes));
            }
        }
    }
    if let Some(choices) = v["choices"].as_array() {
        for choice in choices {
            if let Some(bytes) = inspect_item(&choice["message"]) {
                return Ok(Some(bytes));
            }
            if let Some(bytes) = inspect_item(&choice["delta"]) {
                return Ok(Some(bytes));
            }
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(None)
}

fn extract_image_bytes(
    v: &serde_json::Value,
    client: &reqwest::blocking::Client,
) -> Result<Vec<u8>, String> {
    if let Some(bytes) = find_image_candidate(v, client)? {
        return Ok(bytes);
    }
    if let Some(b64) = v["data"][0]["b64_json"].as_str() {
        return decode_base64_limited(b64);
    }
    if let Some(url) = v["data"][0]["content"][0]["image_url"]["url"].as_str() {
        if url.starts_with("http://") || url.starts_with("https://") {
            return fetch_remote_image(client, url);
        }
        let b64 = url
            .strip_prefix("data:")
            .and_then(|rest| rest.find(',').map(|i| &rest[i + 1..]))
            .unwrap_or(url);
        return decode_base64_limited(b64);
    }
    if let Some(url) = v["data"][0]["url"].as_str() {
        // 托管图片链接：仅允许 http/https，复用同一 client（含超时），避免 SSRF 面
        return fetch_remote_image(client, url);
    }
    if let Some(text) = v["data"][0]["content"][0]["text"].as_str() {
        if !text.starts_with("data:") && text.len() < 64 {
            return Err("响应中的文本不是有效图片数据".to_string());
        }
        // 部分兼容层把 base64 放在 content[0].text
        let b64 = text
            .strip_prefix("data:")
            .and_then(|rest| rest.find(',').map(|i| &rest[i + 1..]))
            .unwrap_or(text);
        return decode_base64_limited(b64);
    }
    Err("响应中缺少图像数据（未找到 b64_json / image_url / url 字段）".to_string())
}

/// 读取响应体并做统一错误处理，返回 JSON Value
fn parse_response(resp: reqwest::blocking::Response) -> Result<serde_json::Value, String> {
    let status = resp.status();
    let body = read_response_limited(resp, MAX_RESPONSE_BYTES)?;
    let body = String::from_utf8_lossy(&body);
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| body.chars().take(200).collect());
        return Err(format!("API 错误（HTTP {status}）：{msg}"));
    }
    serde_json::from_str(&body).map_err(|e| format!("响应解析失败：{e}"))
}

fn fetch_remote_image(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>, String> {
    validate_remote_image_url(url)?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("下载图片失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载图片失败（HTTP {}）", resp.status()));
    }
    read_response_limited(resp, MAX_IMAGE_BYTES)
}

fn read_response_limited(resp: reqwest::blocking::Response, limit: u64) -> Result<Vec<u8>, String> {
    if resp.content_length().is_some_and(|len| len > limit) {
        return Err(format!("接口响应超过 {} MB", limit / (1024 * 1024)));
    }
    let mut bytes = Vec::new();
    resp.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取响应失败：{e}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("接口响应超过 {} MB", limit / (1024 * 1024)));
    }
    Ok(bytes)
}

fn decode_base64_limited(value: &str) -> Result<Vec<u8>, String> {
    if value.len() > MAX_BASE64_BYTES {
        return Err("接口返回的 Base64 图像数据过大".to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| format!("图像数据解码失败：{e}"))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("接口返回的图片数据过大".to_string());
    }
    Ok(bytes)
}

/// 调用 OpenAI 兼容接口把图片转为黑白线稿，返回转换后的图像
pub fn ai_to_lineart(
    cfg: &AiConfig,
    img: &image::DynamicImage,
) -> Result<image::DynamicImage, String> {
    validate_api_base_url(&cfg.api_base_url)?;
    if cfg.api_base_url.trim().is_empty() || cfg.api_key.trim().is_empty() {
        return Err("请先在设置中配置 AI 接口（API 地址与 Key）".to_string());
    }

    // 缩放：最长边不超过 1536，保持宽高比（gpt-image 系列输入上限）
    let (w, h) = (img.width(), img.height());
    let max_side = 1536u32;
    let scale = if w.max(h) > max_side {
        max_side as f32 / w.max(h) as f32
    } else {
        1.0
    };
    // 仅缩放时持有缩放副本；无需缩放时直接借用原图，避免整图克隆
    let resized;
    let img = if scale < 1.0 {
        // .max(1) 兜底：极端长宽比图缩放后短边可能截为 0，image crate 对 0 维度会 panic
        resized = img.resize(
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        );
        &resized
    } else {
        img
    };

    // 输出尺寸按原图宽高比选择，尽量接近原图比例、减少裁剪
    // 用浮点比较避免整数除法截断（h * 12 / 10 在边界值有 ±1 误差）
    let size = if w as f32 > h as f32 * 1.2 {
        "1536x1024"
    } else if h as f32 > w as f32 * 1.2 {
        "1024x1536"
    } else {
        "1024x1024"
    };

    // 编码 PNG 到内存
    let png_bytes = {
        let mut png_buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png_buf, image::ImageFormat::Png)
            .map_err(|e| format!("图片编码失败：{e}"))?;
        png_buf.into_inner()
    };
    if png_bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("上传图片编码后超过 20 MB 限制".to_string());
    }

    let base = cfg.api_base_url.trim().trim_end_matches('/');
    let client = build_client(120)?;
    let endpoint = cfg.api_endpoint.trim().trim_start_matches('/');

    let v = match endpoint {
        "chat/completions" => {
            // 对话式生图：图片以 data URL 作为 image_url 内容。
            // 注意：官方 chat 接口图片输出要求 response_format={"type":"image"}（返回
            // content[0].image_url.url data URL）；部分中转站接受 b64_json 写法，
            // 这里默认用官方 "image" 格式，extract_image_bytes 同时兼容两种返回。
            let data_url = format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&png_bytes)
            );
            let payload = serde_json::json!({
                "model": cfg.model,
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": LINEART_PROMPT },
                        { "type": "image_url", "image_url": { "url": data_url } }
                    ]
                }],
                "response_format": { "type": "image" }
            });
            let resp = client
                .post(format!("{base}/chat/completions"))
                .bearer_auth(cfg.api_key.trim())
                .json(&payload)
                .send()
                .map_err(|e| format!("请求失败（网络/连接）：{e}"))?;
            parse_response(resp)?
        }
        _ => {
            // images/edits（默认）：multipart 上传 image。
            let form = reqwest::blocking::multipart::Form::new()
                .part(
                    "image",
                    reqwest::blocking::multipart::Part::bytes(png_bytes)
                        .file_name("input.png")
                        .mime_str("image/png")
                        .map_err(|e| e.to_string())?,
                )
                .text("prompt", LINEART_PROMPT.to_string())
                .text("model", cfg.model.clone())
                .text("size", size.to_string())
                .text("response_format", "b64_json".to_string());
            let resp = client
                .post(format!("{base}/images/edits"))
                .bearer_auth(cfg.api_key.trim())
                .multipart(form)
                .send()
                .map_err(|e| format!("请求失败（网络/连接）：{e}"))?;
            parse_response(resp)?
        }
    };

    let bytes = extract_image_bytes(&v, &client)?;
    let output = image::load_from_memory(&bytes).map_err(|e| format!("图像解析失败：{e}"))?;
    if output.width() as u64 * output.height() as u64 > MAX_DECODED_PIXELS {
        return Err("接口返回的图片像素数过大".to_string());
    }
    Ok(output)
}

/// 测试连接：GET /models 验证 Base URL 与 API Key 是否有效
pub fn test_connection(cfg: &AiConfig) -> Result<String, String> {
    if cfg.api_base_url.trim().is_empty() {
        return Err("API 地址为空".to_string());
    }
    if cfg.api_key.trim().is_empty() {
        return Err("API Key 为空，请先在设置中填写".to_string());
    }
    let resp = fetch_models_data(cfg, 10)?;
    if resp.as_array().is_none_or(|a| a.is_empty()) {
        return Err("接口未返回任何模型".to_string());
    }
    Ok("连接成功，API Key 有效".to_string())
}

/// 获取接口可用的全部模型 ID 列表（GET /models，OpenAI 兼容格式）
pub fn fetch_models(cfg: &AiConfig) -> Result<Vec<String>, String> {
    if cfg.api_base_url.trim().is_empty() {
        return Err("API 地址为空".to_string());
    }
    if cfg.api_key.trim().is_empty() {
        return Err("API Key 为空，请先在设置中填写".to_string());
    }
    let data = fetch_models_data(cfg, 15)?;
    let mut seen = std::collections::HashSet::new();
    let models = data
        .as_array()
        .ok_or_else(|| "响应中缺少 data 数组".to_string())?
        .iter()
        .filter_map(|m| m["id"].as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= MAX_MODEL_ID_BYTES)
        .filter(|id| seen.insert(*id))
        .take(MAX_MODELS)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if models.is_empty() {
        Err("接口未返回任何模型".to_string())
    } else {
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_normalization_migrates_legacy_endpoint() {
        let mut cfg = AiConfig {
            api_base_url: "not-a-url".to_string(),
            api_key: "secret".to_string(),
            api_key_protected: String::new(),
            model: "  ".to_string(),
            api_endpoint: "images/generations".to_string(),
            clear_api_key: true,
        };
        normalize_config(&mut cfg);
        assert_eq!(cfg.api_base_url, AiConfig::default().api_base_url);
        assert_eq!(cfg.model, AiConfig::default().model);
        assert_eq!(cfg.api_endpoint, "images/edits");
        assert!(!cfg.clear_api_key);
    }

    #[test]
    fn config_view_does_not_expose_key() {
        let cfg = AiConfig {
            api_key: "secret".to_string(),
            ..AiConfig::default()
        };
        let view = AiConfigView::from_config(&cfg);
        assert!(view.api_key_set);
    }

    #[test]
    fn invalid_base64_is_rejected_without_panicking() {
        assert!(decode_base64_limited("not base64").is_err());
    }

    #[test]
    fn image_content_after_text_is_selected() {
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([0, 0, 0, 255]),
        ))
        .write_to(&mut buffer, image::ImageFormat::Png)
        .unwrap();
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(buffer.into_inner())
        );
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "content": [
                        {"type": "text", "text": "done"},
                        {"type": "image_url", "image_url": {"url": data_url}}
                    ]
                }
            }]
        });
        let client = build_client(1).unwrap();
        let bytes = extract_image_bytes(&response, &client).unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!((image.width(), image.height()), (2, 2));
    }

    #[test]
    fn ordinary_text_is_not_treated_as_image_data() {
        let response = serde_json::json!({
            "data": [{"content": [{"type": "text", "text": "done"}]}]
        });
        let client = build_client(1).unwrap();
        assert!(extract_image_bytes(&response, &client).is_err());
    }

    #[test]
    fn api_base_requires_https_except_for_loopback() {
        assert!(validate_api_base_url("https://api.example.com/v1").is_ok());
        assert!(validate_api_base_url("http://127.0.0.1:8080/v1").is_ok());
        assert!(validate_api_base_url("http://192.168.1.5:8080/v1").is_ok()); // 局域网中转
        assert!(validate_api_base_url("http://10.0.0.2:8000/v1").is_ok()); // 局域网中转
        assert!(validate_api_base_url("http://api.example.com/v1").is_err()); // 公网 http 拒绝
        assert!(validate_api_base_url("https://user:pass@example.com/v1").is_err());
    }

    #[test]
    fn api_base_accepts_ipv6_local_http() {
        // url crate 的 host_str() 对 IPv6 返回带方括号形式：本机/内网 IPv6 http 端点
        // 必须在剥括号后按还原地址判定，不得被误判为公网
        assert!(validate_api_base_url("http://[::1]:8080/v1").is_ok());
        assert!(validate_api_base_url("http://[::ffff:192.168.1.10]:8080/v1").is_ok());
        assert!(validate_api_base_url("http://[2001:db8::1]/v1").is_err()); // 公网 IPv6 http 拒绝
        assert!(validate_api_base_url("https://[::1]/v1").is_ok()); // https 不受影响
    }

    #[test]
    fn remote_image_validation_rejects_private_addresses() {
        assert!(validate_remote_image_url("https://127.0.0.1/image.png").is_err());
        assert!(validate_remote_image_url("http://192.168.1.10/image.png").is_err());
        assert!(validate_remote_image_url("http://localhost:8080/image.png").is_ok());
        // IPv4-mapped IPv6 字面量按还原后的 IPv4 判定，不得绕过私网拒绝
        assert!(validate_remote_image_url("https://[::ffff:192.168.1.10]/image.png").is_err());
        assert!(validate_remote_image_url("https://[::ffff:127.0.0.1]/image.png").is_err());
        // 纯 IPv6 公网地址不受影响
        assert!(validate_remote_image_url("https://[2001:db8::1]/image.png").is_ok());
    }

    #[cfg(windows)]
    #[ignore = "requires a loaded Windows user profile with DPAPI available"]
    #[test]
    fn dpapi_round_trip_does_not_serialize_plaintext_key() {
        let protected = protect_api_key("test-secret").unwrap();
        assert!(protected.starts_with("dpapi:"));
        let mut cfg = AiConfig {
            api_key_protected: protected,
            ..AiConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("test-secret"));
        restore_api_key(&mut cfg).unwrap();
        assert_eq!(cfg.api_key, "test-secret");
    }
}
