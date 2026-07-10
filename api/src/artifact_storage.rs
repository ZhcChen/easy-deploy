use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use chrono::SecondsFormat;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use url::Url;

pub const STORAGE_PROVIDER_LOCAL: &str = "local";
pub const STORAGE_PROVIDER_ALIYUN_OSS: &str = "aliyun_oss";
pub const DEFAULT_ALIYUN_OSS_REGION: &str = "oss-cn-hangzhou";
pub const DEFAULT_ALIYUN_OSS_ENDPOINT: &str = "https://oss-cn-hangzhou.aliyuncs.com";
pub const DEFAULT_ALIYUN_OSS_OBJECT_PREFIX: &str = "easy-deploy/releases";
pub const DEFAULT_ALIYUN_OSS_UPLOAD_TTL_SECONDS: i64 = 900;
pub const DEFAULT_ALIYUN_OSS_DOWNLOAD_TTL_SECONDS: i64 = 600;
pub const ARTIFACT_UPLOAD_CONTENT_TYPE: &str = "application/octet-stream";
pub const ARTIFACT_UPLOAD_FORBID_OVERWRITE_HEADER: &str = "x-oss-forbid-overwrite";
pub const ARTIFACT_UPLOAD_FORBID_OVERWRITE_VALUE: &str = "true";
pub const MAX_ARTIFACT_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

const ARTIFACT_OBJECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ARTIFACT_OBJECT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const OSS_LIST_OBJECT_VERSIONS_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const OSS_LIST_OBJECT_VERSIONS_MAX_PAGES: usize = 1_000;
const OSS_LIST_OBJECT_VERSIONS_MAX_KEYS: usize = 999;

type HmacSha1 = Hmac<Sha1>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactStorageConfig {
    pub provider: String,
    pub aliyun_oss: AliyunOssConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AliyunOssConfig {
    pub region: String,
    pub endpoint: String,
    pub bucket: String,
    pub object_prefix: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub upload_url_ttl_seconds: i64,
    pub download_url_ttl_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresignedUpload {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresignedDownload {
    pub url: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedArtifactObject {
    pub checksum_sha256: String,
    pub size_bytes: u64,
    pub version_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactObjectVersion {
    pub version_id: Option<String>,
    pub is_delete_marker: bool,
}

#[async_trait]
pub trait ArtifactObjectVerifier: Send + Sync {
    async fn verify(
        &self,
        config: &AliyunOssConfig,
        object_key: &str,
    ) -> Result<VerifiedArtifactObject, ArtifactStorageError>;

    async fn delete(
        &self,
        config: &AliyunOssConfig,
        object_key: &str,
        version_id: Option<&str>,
    ) -> Result<(), ArtifactStorageError>;

    async fn list_versions(
        &self,
        _config: &AliyunOssConfig,
        _object_key: &str,
    ) -> Result<Vec<ArtifactObjectVersion>, ArtifactStorageError> {
        Err(ArtifactStorageError::Unsupported(
            "当前制品对象校验器不支持枚举 OSS 对象版本，无法确认清理完成".to_owned(),
        ))
    }
}

#[derive(Clone)]
pub struct AliyunOssObjectVerifier {
    client: reqwest::Client,
}

impl Default for AliyunOssObjectVerifier {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(ARTIFACT_OBJECT_CONNECT_TIMEOUT)
            .timeout(ARTIFACT_OBJECT_REQUEST_TIMEOUT)
            .build()
            .expect("build OSS artifact HTTP client");
        Self { client }
    }
}

#[derive(Debug)]
pub enum ArtifactStorageError {
    InvalidInput(String),
    Unsupported(String),
    Internal(String),
}

impl ArtifactStorageError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Unsupported(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for ArtifactStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ArtifactStorageError {}

impl Default for ArtifactStorageConfig {
    fn default() -> Self {
        Self {
            provider: STORAGE_PROVIDER_LOCAL.to_owned(),
            aliyun_oss: AliyunOssConfig::default(),
        }
    }
}

impl Default for AliyunOssConfig {
    fn default() -> Self {
        Self {
            region: DEFAULT_ALIYUN_OSS_REGION.to_owned(),
            endpoint: DEFAULT_ALIYUN_OSS_ENDPOINT.to_owned(),
            bucket: String::new(),
            object_prefix: DEFAULT_ALIYUN_OSS_OBJECT_PREFIX.to_owned(),
            access_key_id: String::new(),
            access_key_secret: String::new(),
            upload_url_ttl_seconds: DEFAULT_ALIYUN_OSS_UPLOAD_TTL_SECONDS,
            download_url_ttl_seconds: DEFAULT_ALIYUN_OSS_DOWNLOAD_TTL_SECONDS,
        }
    }
}

impl ArtifactStorageConfig {
    pub fn is_aliyun_oss(&self) -> bool {
        self.provider == STORAGE_PROVIDER_ALIYUN_OSS
    }

    pub fn normalize(mut self) -> Result<Self, ArtifactStorageError> {
        self.provider = normalize_storage_provider(&self.provider)?;
        self.aliyun_oss = self.aliyun_oss.normalize()?;
        if self.is_aliyun_oss() {
            self.aliyun_oss.validate_required()?;
        }
        Ok(self)
    }
}

impl AliyunOssConfig {
    pub fn normalize(mut self) -> Result<Self, ArtifactStorageError> {
        self.region = normalize_region(&self.region);
        self.endpoint = normalize_endpoint(&self.endpoint, &self.region)?;
        self.bucket = normalize_bucket(&self.bucket)?;
        self.object_prefix = normalize_object_prefix(&self.object_prefix)?;
        self.access_key_id = self.access_key_id.trim().to_owned();
        self.access_key_secret = self.access_key_secret.trim().to_owned();
        self.upload_url_ttl_seconds = normalize_ttl(
            self.upload_url_ttl_seconds,
            DEFAULT_ALIYUN_OSS_UPLOAD_TTL_SECONDS,
        )?;
        self.download_url_ttl_seconds = normalize_ttl(
            self.download_url_ttl_seconds,
            DEFAULT_ALIYUN_OSS_DOWNLOAD_TTL_SECONDS,
        )?;
        Ok(self)
    }

    pub fn validate_required(&self) -> Result<(), ArtifactStorageError> {
        if self.bucket.trim().is_empty() {
            return Err(ArtifactStorageError::InvalidInput(
                "阿里云 OSS Bucket 不能为空".to_owned(),
            ));
        }
        if self.access_key_id.trim().is_empty() || self.access_key_secret.trim().is_empty() {
            return Err(ArtifactStorageError::InvalidInput(
                "阿里云 OSS AccessKey ID 和 Secret 必须配置完整".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn object_key(&self, app_key: &str, release_version: &str, file_name: &str) -> String {
        let prefix = self.object_prefix.trim_matches('/');
        let app_key = safe_object_segment(app_key);
        let release_version = safe_object_segment(release_version);
        let file_name = safe_object_segment(file_name);
        if prefix.is_empty() {
            format!("{app_key}/{release_version}/{file_name}")
        } else {
            format!("{prefix}/{app_key}/{release_version}/{file_name}")
        }
    }

    pub fn upload_object_key(
        &self,
        app_key: &str,
        release_version: &str,
        upload_id: &str,
        file_name: &str,
    ) -> String {
        let prefix = self.object_prefix.trim_matches('/');
        let app_key = safe_object_segment(app_key);
        let release_version = safe_object_segment(release_version);
        let upload_id = safe_object_segment(upload_id);
        let file_name = safe_object_segment(file_name);
        if prefix.is_empty() {
            format!("{app_key}/{release_version}/uploads/{upload_id}/{file_name}")
        } else {
            format!("{prefix}/{app_key}/{release_version}/uploads/{upload_id}/{file_name}")
        }
    }

    pub fn presign_upload(
        &self,
        object_key: &str,
    ) -> Result<PresignedUpload, ArtifactStorageError> {
        self.validate_required()?;
        let expires = unix_timestamp_now() + self.upload_url_ttl_seconds;
        let headers = vec![
            (
                "Content-Type".to_owned(),
                ARTIFACT_UPLOAD_CONTENT_TYPE.to_owned(),
            ),
            (
                ARTIFACT_UPLOAD_FORBID_OVERWRITE_HEADER.to_owned(),
                ARTIFACT_UPLOAD_FORBID_OVERWRITE_VALUE.to_owned(),
            ),
        ];
        let url = self.presign(
            "PUT",
            object_key,
            ARTIFACT_UPLOAD_CONTENT_TYPE,
            expires,
            &headers,
            None,
        )?;
        Ok(PresignedUpload {
            method: "PUT",
            url,
            headers,
            expires_at: timestamp_text(expires),
        })
    }

    pub fn presign_download(
        &self,
        object_key: &str,
    ) -> Result<PresignedDownload, ArtifactStorageError> {
        self.presign_download_version(object_key, None)
    }

    pub fn presign_download_version(
        &self,
        object_key: &str,
        version_id: Option<&str>,
    ) -> Result<PresignedDownload, ArtifactStorageError> {
        self.validate_required()?;
        let expires = unix_timestamp_now() + self.download_url_ttl_seconds;
        Ok(PresignedDownload {
            url: self.presign("GET", object_key, "", expires, &[], version_id)?,
            expires_at: timestamp_text(expires),
        })
    }

    pub fn presign_delete(
        &self,
        object_key: &str,
        version_id: Option<&str>,
    ) -> Result<String, ArtifactStorageError> {
        self.validate_required()?;
        let expires = unix_timestamp_now() + self.download_url_ttl_seconds;
        let object_key = normalize_object_key(object_key)?;
        let version_id = version_id
            .filter(|value| !value.trim().is_empty())
            .map(normalize_deletable_object_version_id)
            .transpose()?;
        self.presign_normalized(
            "DELETE",
            &object_key,
            "",
            expires,
            &[],
            version_id.as_deref(),
        )
    }

    pub fn presign_list_object_versions(
        &self,
        object_key: &str,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
    ) -> Result<String, ArtifactStorageError> {
        self.validate_required()?;
        let object_key = normalize_object_key(object_key)?;
        let key_marker = key_marker
            .filter(|value| !value.trim().is_empty())
            .map(normalize_object_key)
            .transpose()?;
        let version_id_marker = version_id_marker
            .filter(|value| !value.trim().is_empty())
            .map(normalize_deletable_object_version_id)
            .transpose()?;
        if version_id_marker.is_some() && key_marker.is_none() {
            return Err(ArtifactStorageError::InvalidInput(
                "枚举 OSS 对象版本时 version-id-marker 必须与 key-marker 一起使用".to_owned(),
            ));
        }

        let expires = unix_timestamp_now() + self.download_url_ttl_seconds;
        let resource = canonicalized_oss_bucket_resource(&self.bucket, "versions");
        let signature = self.presign_signature("GET", "", expires, &[], &resource)?;
        let mut parameters = vec![
            "versions".to_owned(),
            format!("prefix={}", encode_query_component(&object_key)),
            format!("max-keys={OSS_LIST_OBJECT_VERSIONS_MAX_KEYS}"),
        ];
        if let Some(key_marker) = key_marker {
            parameters.push(format!(
                "key-marker={}",
                encode_query_component(&key_marker)
            ));
        }
        if let Some(version_id_marker) = version_id_marker {
            parameters.push(format!(
                "version-id-marker={}",
                encode_query_component(&version_id_marker)
            ));
        }
        parameters.push(format!(
            "OSSAccessKeyId={}",
            encode_query_component(&self.access_key_id)
        ));
        parameters.push(format!("Expires={expires}"));
        parameters.push(format!("Signature={}", encode_query_component(&signature)));

        let mut url = bucket_url(&self.endpoint, &self.bucket)?;
        url.set_query(Some(&parameters.join("&")));
        Ok(url.to_string())
    }

    fn presign(
        &self,
        method: &str,
        object_key: &str,
        content_type: &str,
        expires: i64,
        headers: &[(String, String)],
        version_id: Option<&str>,
    ) -> Result<String, ArtifactStorageError> {
        let object_key = normalize_object_key(object_key)?;
        let version_id = version_id
            .filter(|value| !value.trim().is_empty())
            .map(normalize_object_version_id)
            .transpose()?;
        self.presign_normalized(
            method,
            &object_key,
            content_type,
            expires,
            headers,
            version_id.as_deref(),
        )
    }

    fn presign_normalized(
        &self,
        method: &str,
        object_key: &str,
        content_type: &str,
        expires: i64,
        headers: &[(String, String)],
        version_id: Option<&str>,
    ) -> Result<String, ArtifactStorageError> {
        let resource = canonicalized_oss_resource(&self.bucket, object_key, version_id);
        let signature =
            self.presign_signature(method, content_type, expires, headers, &resource)?;
        let mut url = object_url(&self.endpoint, &self.bucket, object_key)?;
        if let Some(version_id) = version_id {
            url.query_pairs_mut().append_pair("versionId", version_id);
        }
        url.query_pairs_mut()
            .append_pair("OSSAccessKeyId", &self.access_key_id)
            .append_pair("Expires", &expires.to_string())
            .append_pair("Signature", &signature);
        Ok(url.to_string())
    }

    fn presign_signature(
        &self,
        method: &str,
        content_type: &str,
        expires: i64,
        headers: &[(String, String)],
        resource: &str,
    ) -> Result<String, ArtifactStorageError> {
        let canonicalized_headers = canonicalized_oss_headers(headers);
        let string_to_sign =
            format!("{method}\n\n{content_type}\n{expires}\n{canonicalized_headers}{resource}");
        let mut mac =
            HmacSha1::new_from_slice(self.access_key_secret.as_bytes()).map_err(|_| {
                ArtifactStorageError::InvalidInput("阿里云 OSS AccessKey Secret 无效".to_owned())
            })?;
        mac.update(string_to_sign.as_bytes());
        Ok(general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
    }
}

#[async_trait]
impl ArtifactObjectVerifier for AliyunOssObjectVerifier {
    async fn verify(
        &self,
        config: &AliyunOssConfig,
        object_key: &str,
    ) -> Result<VerifiedArtifactObject, ArtifactStorageError> {
        let signed = config.presign_download(object_key)?;
        let response = self.client.get(signed.url).send().await.map_err(|err| {
            ArtifactStorageError::Internal(format!("读取 OSS 上传对象失败: {err}"))
        })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(ArtifactStorageError::InvalidInput(
                "OSS 上传对象不存在，请先完成文件上传".to_owned(),
            ));
        }
        if !response.status().is_success() {
            return Err(ArtifactStorageError::Internal(format!(
                "读取 OSS 上传对象失败，响应状态为 {}",
                response.status()
            )));
        }

        if let Some(content_length) = response.content_length()
            && content_length > MAX_ARTIFACT_OBJECT_BYTES
        {
            return Err(ArtifactStorageError::InvalidInput(format!(
                "OSS 上传对象超过最大允许大小 {} 字节",
                MAX_ARTIFACT_OBJECT_BYTES
            )));
        }
        let version_id = response
            .headers()
            .get("x-oss-version-id")
            .map(|value| {
                value.to_str().map_err(|_| {
                    ArtifactStorageError::Internal("OSS 返回的对象版本号格式无效".to_owned())
                })
            })
            .transpose()?
            .map(normalize_object_version_id)
            .transpose()?;
        let mut response = response;
        let mut checksum = Sha256::new();
        let mut size_bytes = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(|err| {
            ArtifactStorageError::Internal(format!("读取 OSS 上传对象内容失败: {err}"))
        })? {
            size_bytes = size_bytes.checked_add(chunk.len() as u64).ok_or_else(|| {
                ArtifactStorageError::Internal("OSS 上传对象大小超出支持范围".to_owned())
            })?;
            if size_bytes > MAX_ARTIFACT_OBJECT_BYTES {
                return Err(ArtifactStorageError::InvalidInput(format!(
                    "OSS 上传对象超过最大允许大小 {} 字节",
                    MAX_ARTIFACT_OBJECT_BYTES
                )));
            }
            checksum.update(&chunk);
        }
        Ok(VerifiedArtifactObject {
            checksum_sha256: format!("{:x}", checksum.finalize()),
            size_bytes,
            version_id,
        })
    }

    async fn delete(
        &self,
        config: &AliyunOssConfig,
        object_key: &str,
        version_id: Option<&str>,
    ) -> Result<(), ArtifactStorageError> {
        let signed = config.presign_delete(object_key, version_id)?;
        let response = self.client.delete(signed).send().await.map_err(|err| {
            ArtifactStorageError::Internal(format!("删除 OSS 上传对象失败: {err}"))
        })?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(ArtifactStorageError::Internal(format!(
            "删除 OSS 上传对象失败，响应状态为 {}",
            response.status()
        )))
    }

    async fn list_versions(
        &self,
        config: &AliyunOssConfig,
        object_key: &str,
    ) -> Result<Vec<ArtifactObjectVersion>, ArtifactStorageError> {
        let object_key = normalize_object_key(object_key)?;
        let mut key_marker = None;
        let mut version_id_marker = None;
        let mut seen_markers = BTreeSet::new();
        let mut listed_versions = Vec::new();

        for _ in 0..OSS_LIST_OBJECT_VERSIONS_MAX_PAGES {
            let signed = config.presign_list_object_versions(
                &object_key,
                key_marker.as_deref(),
                version_id_marker.as_deref(),
            )?;
            let response = self.client.get(signed).send().await.map_err(|err| {
                ArtifactStorageError::Internal(format!("枚举 OSS 上传对象版本失败: {err}"))
            })?;
            if !response.status().is_success() {
                return Err(ArtifactStorageError::Internal(format!(
                    "枚举 OSS 上传对象版本失败，响应状态为 {}；请确认 AccessKey 具备 oss:ListObjectVersions 权限",
                    response.status()
                )));
            }
            let response_text = read_oss_list_versions_response(response).await?;
            let page = parse_oss_object_versions_page(&response_text)?;
            listed_versions.extend(
                page.versions
                    .into_iter()
                    .filter(|item| item.key == object_key)
                    .map(|item| artifact_object_version_from_list_item(item, false))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            listed_versions.extend(
                page.delete_markers
                    .into_iter()
                    .filter(|item| item.key == object_key)
                    .map(|item| artifact_object_version_from_list_item(item, true))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if !page.is_truncated {
                return validate_listed_object_versions(listed_versions);
            }

            let next_key_marker = page
                .next_key_marker
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    ArtifactStorageError::Internal(
                        "OSS 对象版本列表缺少下一页 key-marker".to_owned(),
                    )
                })?;
            let next_version_id_marker = page
                .next_version_id_marker
                .filter(|value| !value.trim().is_empty());
            let marker = (
                next_key_marker.clone(),
                next_version_id_marker.clone().unwrap_or_default(),
            );
            if !seen_markers.insert(marker) {
                return Err(ArtifactStorageError::Internal(
                    "OSS 对象版本列表分页标记重复，拒绝将对象标记为已清理".to_owned(),
                ));
            }
            key_marker = Some(next_key_marker);
            version_id_marker = next_version_id_marker;
        }

        Err(ArtifactStorageError::Internal(
            "OSS 对象版本数量超过清理上限，拒绝将对象标记为已清理".to_owned(),
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename = "ListVersionsResult")]
struct OssObjectVersionsPageResponse {
    #[serde(rename = "IsTruncated")]
    is_truncated: Option<String>,
    #[serde(rename = "NextKeyMarker")]
    next_key_marker: Option<String>,
    #[serde(rename = "NextVersionIdMarker")]
    next_version_id_marker: Option<String>,
    #[serde(rename = "Version", default)]
    versions: Vec<OssListedObjectVersion>,
    #[serde(rename = "DeleteMarker", default)]
    delete_markers: Vec<OssListedObjectVersion>,
}

#[derive(Deserialize)]
struct OssListedObjectVersion {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "VersionId")]
    version_id: Option<String>,
}

struct OssObjectVersionsPage {
    is_truncated: bool,
    next_key_marker: Option<String>,
    next_version_id_marker: Option<String>,
    versions: Vec<OssListedObjectVersion>,
    delete_markers: Vec<OssListedObjectVersion>,
}

fn parse_oss_object_versions_page(
    value: &str,
) -> Result<OssObjectVersionsPage, ArtifactStorageError> {
    let response: OssObjectVersionsPageResponse =
        quick_xml::de::from_str(value).map_err(|err| {
            ArtifactStorageError::Internal(format!("解析 OSS 对象版本列表响应失败: {err}"))
        })?;
    let is_truncated = match response.is_truncated.as_deref().map(str::trim) {
        Some("true") => true,
        Some("false") => false,
        _ => {
            return Err(ArtifactStorageError::Internal(
                "OSS 对象版本列表缺少或包含无效的 IsTruncated".to_owned(),
            ));
        }
    };
    Ok(OssObjectVersionsPage {
        is_truncated,
        next_key_marker: response.next_key_marker,
        next_version_id_marker: response.next_version_id_marker,
        versions: response.versions,
        delete_markers: response.delete_markers,
    })
}

async fn read_oss_list_versions_response(
    mut response: reqwest::Response,
) -> Result<String, ArtifactStorageError> {
    if response
        .content_length()
        .is_some_and(|length| length > OSS_LIST_OBJECT_VERSIONS_MAX_RESPONSE_BYTES as u64)
    {
        return Err(ArtifactStorageError::Internal(
            "OSS 对象版本列表响应超过 8 MiB 上限".to_owned(),
        ));
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(OSS_LIST_OBJECT_VERSIONS_MAX_RESPONSE_BYTES);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        ArtifactStorageError::Internal(format!("读取 OSS 对象版本列表响应失败: {err}"))
    })? {
        append_bounded_oss_response_chunk(
            &mut body,
            &chunk,
            OSS_LIST_OBJECT_VERSIONS_MAX_RESPONSE_BYTES,
        )?;
    }
    String::from_utf8(body).map_err(|_| {
        ArtifactStorageError::Internal("OSS 对象版本列表响应不是有效 UTF-8".to_owned())
    })
}

fn append_bounded_oss_response_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
) -> Result<(), ArtifactStorageError> {
    let next_size = body
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| ArtifactStorageError::Internal("OSS 对象版本列表响应大小溢出".to_owned()))?;
    if next_size > max_bytes {
        return Err(ArtifactStorageError::Internal(format!(
            "OSS 对象版本列表响应超过 {} 字节上限",
            max_bytes
        )));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn artifact_object_version_from_list_item(
    item: OssListedObjectVersion,
    is_delete_marker: bool,
) -> Result<ArtifactObjectVersion, ArtifactStorageError> {
    let version_id = item
        .version_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(normalize_deletable_object_version_id)
        .transpose()?;
    Ok(ArtifactObjectVersion {
        version_id,
        is_delete_marker,
    })
}

fn validate_listed_object_versions(
    listed_versions: Vec<ArtifactObjectVersion>,
) -> Result<Vec<ArtifactObjectVersion>, ArtifactStorageError> {
    if listed_versions
        .iter()
        .any(|item| item.is_delete_marker && item.version_id.is_none())
    {
        return Err(ArtifactStorageError::Internal(
            "OSS 删除标记缺少版本号，拒绝将对象标记为已清理".to_owned(),
        ));
    }
    let versioning_detected = listed_versions
        .iter()
        .any(|item| item.is_delete_marker || item.version_id.is_some());
    if versioning_detected && listed_versions.iter().any(|item| item.version_id.is_none()) {
        return Err(ArtifactStorageError::Internal(
            "OSS 对象版本列表混入未标识版本，拒绝将对象标记为已清理".to_owned(),
        ));
    }

    let mut seen_version_ids = BTreeSet::new();
    Ok(listed_versions
        .into_iter()
        .filter(|item| seen_version_ids.insert(item.version_id.clone()))
        .collect())
}

pub fn normalize_storage_provider(value: &str) -> Result<String, ArtifactStorageError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | STORAGE_PROVIDER_LOCAL => Ok(STORAGE_PROVIDER_LOCAL.to_owned()),
        STORAGE_PROVIDER_ALIYUN_OSS => Ok(STORAGE_PROVIDER_ALIYUN_OSS.to_owned()),
        _ => Err(ArtifactStorageError::Unsupported(
            "制品存储后端暂只支持 local 和 aliyun_oss".to_owned(),
        )),
    }
}

pub fn normalize_checksum_sha256(value: &str) -> Result<String, ArtifactStorageError> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ArtifactStorageError::InvalidInput(
            "checksum_sha256 必须是 64 位十六进制 SHA-256".to_owned(),
        ));
    }
    Ok(value)
}

pub fn normalize_object_key(value: &str) -> Result<String, ArtifactStorageError> {
    let value = value.trim().replace('\\', "/");
    if value.is_empty()
        || value.starts_with('/')
        || value.contains("//")
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '@'))
    {
        return Err(ArtifactStorageError::InvalidInput(
            "OSS ObjectKey 仅支持字母、数字、斜线、点、短横线、下划线和 @，且不能包含上级目录"
                .to_owned(),
        ));
    }
    Ok(value)
}

fn normalize_region(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        DEFAULT_ALIYUN_OSS_REGION.to_owned()
    } else {
        value.to_owned()
    }
}

fn normalize_endpoint(value: &str, region: &str) -> Result<String, ArtifactStorageError> {
    let value = value.trim();
    let value = if value.is_empty() {
        if region.trim().is_empty() {
            DEFAULT_ALIYUN_OSS_ENDPOINT.to_owned()
        } else {
            format!("https://{}.aliyuncs.com", region.trim())
        }
    } else if value.starts_with("http://") || value.starts_with("https://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let parsed = Url::parse(&value).map_err(|_| {
        ArtifactStorageError::InvalidInput("阿里云 OSS Endpoint 不是有效 URL".to_owned())
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().unwrap_or_default().trim().is_empty()
    {
        return Err(ArtifactStorageError::InvalidInput(
            "阿里云 OSS Endpoint 必须是 http/https URL".to_owned(),
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn normalize_bucket(value: &str) -> Result<String, ArtifactStorageError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Ok(value);
    }
    if !(3..=63).contains(&value.len())
        || !value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        || value.starts_with('-')
        || value.ends_with('-')
    {
        return Err(ArtifactStorageError::InvalidInput(
            "阿里云 OSS Bucket 只能包含小写字母、数字和短横线，长度 3 到 63".to_owned(),
        ));
    }
    Ok(value)
}

fn normalize_object_prefix(value: &str) -> Result<String, ArtifactStorageError> {
    let value = value.trim().replace('\\', "/").trim_matches('/').to_owned();
    if value.is_empty() {
        return Ok(String::new());
    }
    normalize_object_key(&value)
}

fn normalize_ttl(value: i64, fallback: i64) -> Result<i64, ArtifactStorageError> {
    let value = if value > 0 { value } else { fallback };
    if !(60..=86_400).contains(&value) {
        return Err(ArtifactStorageError::InvalidInput(
            "OSS 签名 URL TTL 必须在 60 到 86400 秒之间".to_owned(),
        ));
    }
    Ok(value)
}

fn object_url(endpoint: &str, bucket: &str, object_key: &str) -> Result<Url, ArtifactStorageError> {
    let mut url = bucket_url(endpoint, bucket)?;
    url.set_path(&format!("/{}", encode_object_path(object_key)));
    Ok(url)
}

fn bucket_url(endpoint: &str, bucket: &str) -> Result<Url, ArtifactStorageError> {
    let mut url = Url::parse(endpoint).map_err(|_| {
        ArtifactStorageError::InvalidInput("阿里云 OSS Endpoint 不是有效 URL".to_owned())
    })?;
    let host = url.host_str().unwrap_or_default().to_owned();
    url.set_host(Some(&format!("{bucket}.{host}")))
        .map_err(|_| {
            ArtifactStorageError::InvalidInput("无法构造阿里云 OSS Bucket 访问域名".to_owned())
        })?;
    url.set_path("/");
    url.set_query(None);
    Ok(url)
}

fn canonicalized_oss_resource(bucket: &str, object_key: &str, version_id: Option<&str>) -> String {
    let mut resource = format!("/{bucket}/{object_key}");
    if let Some(version_id) = version_id {
        resource.push_str("?versionId=");
        resource.push_str(version_id);
    }
    resource
}

fn canonicalized_oss_bucket_resource(bucket: &str, subresource: &str) -> String {
    format!("/{bucket}/?{subresource}")
}

pub fn normalize_object_version_id(value: &str) -> Result<String, ArtifactStorageError> {
    let value = normalize_deletable_object_version_id(value)?;
    if value == "null" {
        return Err(ArtifactStorageError::InvalidInput(
            "OSS 返回 null 对象版本号，Bucket 可能处于暂停版本控制状态，无法固定已校验制品"
                .to_owned(),
        ));
    }
    Ok(value)
}

fn normalize_deletable_object_version_id(value: &str) -> Result<String, ArtifactStorageError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 1_024 || value.chars().any(|ch| ch.is_ascii_control()) {
        return Err(ArtifactStorageError::InvalidInput(
            "OSS 对象版本号格式无效".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn encode_query_component(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn encode_object_path(value: &str) -> String {
    value
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_path_segment(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char);
            }
            other => output.push_str(&format!("%{other:02X}")),
        }
    }
    output
}

fn canonicalized_oss_headers(headers: &[(String, String)]) -> String {
    let mut headers = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.trim().to_ascii_lowercase();
            name.starts_with("x-oss-")
                .then(|| (name, value.split_whitespace().collect::<Vec<_>>().join(" ")))
        })
        .collect::<Vec<_>>();
    headers.sort_unstable();
    headers
        .into_iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect()
}

fn safe_object_segment(value: &str) -> String {
    value
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@'))
        .collect::<String>()
}

fn timestamp_text(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("unix epoch"))
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn unix_timestamp_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oss_config_builds_stable_object_keys() {
        let config = AliyunOssConfig {
            bucket: "easy-deploy-test".to_owned(),
            object_prefix: " easy-deploy/releases/ ".to_owned(),
            access_key_id: "id".to_owned(),
            access_key_secret: "secret".to_owned(),
            ..AliyunOssConfig::default()
        }
        .normalize()
        .expect("normalize");

        assert_eq!(
            config.object_key("Orders API", "v1.2.3", "orders-api_version_1_2_3.tar.gz"),
            "easy-deploy/releases/OrdersAPI/v1.2.3/orders-api_version_1_2_3.tar.gz"
        );
        assert_eq!(
            config.upload_object_key(
                "Orders API",
                "v1.2.3",
                "upload-123",
                "orders-api_version_1_2_3.tar.gz"
            ),
            "easy-deploy/releases/OrdersAPI/v1.2.3/uploads/upload-123/orders-api_version_1_2_3.tar.gz"
        );
    }

    #[test]
    fn aliyun_presign_masks_nothing_but_returns_required_headers() {
        let config = AliyunOssConfig {
            bucket: "easy-deploy-test".to_owned(),
            access_key_id: "test-key".to_owned(),
            access_key_secret: "test-secret".to_owned(),
            ..AliyunOssConfig::default()
        }
        .normalize()
        .expect("normalize");

        let upload = config
            .presign_upload("easy-deploy/releases/orders/v1.2.3/pkg.tar.gz")
            .expect("presign upload");
        assert_eq!(upload.method, "PUT");
        assert!(
            upload
                .url
                .contains("easy-deploy-test.oss-cn-hangzhou.aliyuncs.com")
        );
        assert!(upload.url.contains("OSSAccessKeyId=test-key"));
        assert_eq!(
            upload.headers,
            vec![
                (
                    "Content-Type".to_owned(),
                    ARTIFACT_UPLOAD_CONTENT_TYPE.to_owned()
                ),
                (
                    ARTIFACT_UPLOAD_FORBID_OVERWRITE_HEADER.to_owned(),
                    ARTIFACT_UPLOAD_FORBID_OVERWRITE_VALUE.to_owned()
                )
            ]
        );

        let download = config
            .presign_download("easy-deploy/releases/orders/v1.2.3/pkg.tar.gz")
            .expect("presign download");
        assert!(download.url.contains("Signature="));

        let versioned_download = config
            .presign_download_version(
                "easy-deploy/releases/orders/v1.2.3/pkg.tar.gz",
                Some("version-20260710"),
            )
            .expect("presign versioned download");
        assert!(
            versioned_download
                .url
                .contains("versionId=version-20260710")
        );
        assert!(versioned_download.url.contains("Signature="));

        let versioned_delete = config
            .presign_delete(
                "easy-deploy/releases/orders/v1.2.3/pkg.tar.gz",
                Some("version-20260710"),
            )
            .expect("presign versioned delete");
        assert!(versioned_delete.contains("versionId=version-20260710"));
        assert!(versioned_delete.contains("Signature="));
    }

    #[test]
    fn oss_version_id_uses_raw_value_for_signature_and_encoded_value_for_url() {
        let config = AliyunOssConfig {
            bucket: "easy-deploy-test".to_owned(),
            access_key_id: "test-key".to_owned(),
            access_key_secret: "test-secret".to_owned(),
            ..AliyunOssConfig::default()
        }
        .normalize()
        .expect("normalize");
        let object_key = "easy-deploy/releases/orders/v1.2.3/pkg.tar.gz";
        let version_id = "version+/%=";

        assert_eq!(
            canonicalized_oss_resource(&config.bucket, object_key, Some(version_id)),
            "/easy-deploy-test/easy-deploy/releases/orders/v1.2.3/pkg.tar.gz?versionId=version+/%="
        );
        let signed = config
            .presign("GET", object_key, "", 1_700_000_000, &[], Some(version_id))
            .expect("presign fixed expiration");
        assert!(signed.contains("versionId=version%2B%2F%25%3D"));
        assert!(signed.contains("Signature=eoOfBYtSeWXWb18QkrA1YBrXBwE%3D"));
    }

    #[test]
    fn oss_null_version_is_rejected_for_download_but_allowed_for_precise_delete() {
        let config = AliyunOssConfig {
            bucket: "easy-deploy-test".to_owned(),
            access_key_id: "test-key".to_owned(),
            access_key_secret: "test-secret".to_owned(),
            ..AliyunOssConfig::default()
        }
        .normalize()
        .expect("normalize");
        let object_key = "easy-deploy/releases/orders/v1.2.3/pkg.tar.gz";

        assert!(normalize_object_version_id("null").is_err());
        assert!(
            config
                .presign_download_version(object_key, Some("null"))
                .is_err()
        );
        assert!(
            config
                .presign_delete(object_key, Some("null"))
                .expect("presign null-version delete")
                .contains("versionId=null")
        );
    }

    #[test]
    fn oss_list_versions_presign_signs_versions_subresource_and_filters_exact_key() {
        let config = AliyunOssConfig {
            bucket: "easy-deploy-test".to_owned(),
            access_key_id: "test-key".to_owned(),
            access_key_secret: "test-secret".to_owned(),
            ..AliyunOssConfig::default()
        }
        .normalize()
        .expect("normalize");
        let object_key = "easy-deploy/releases/orders/v1.2.3/uploads/upload-1/pkg.tar.gz";

        assert_eq!(
            canonicalized_oss_bucket_resource(&config.bucket, "versions"),
            "/easy-deploy-test/?versions"
        );
        let signed = config
            .presign_list_object_versions(object_key, None, None)
            .expect("presign version listing");
        assert!(signed.contains("?versions&prefix=easy-deploy%2Freleases%2Forders%2Fv1.2.3%2Fuploads%2Fupload-1%2Fpkg.tar.gz"));
        assert!(signed.contains("max-keys=999"));
        assert!(signed.contains("Signature="));
    }

    #[test]
    fn parses_oss_object_versions_and_delete_markers() {
        let page = parse_oss_object_versions_page(
            r#"
            <ListVersionsResult xmlns="http://doc.oss-cn-hangzhou.aliyuncs.com">
              <IsTruncated>false</IsTruncated>
              <Version>
                <Key>easy-deploy/releases/orders/v1.2.3/uploads/upload-1/pkg.tar.gz</Key>
                <VersionId>version-new</VersionId>
              </Version>
              <Version>
                <Key>easy-deploy/releases/orders/v1.2.3/uploads/upload-1/pkg.tar.gz</Key>
                <VersionId>version-old</VersionId>
              </Version>
              <DeleteMarker>
                <Key>easy-deploy/releases/orders/v1.2.3/uploads/upload-1/pkg.tar.gz</Key>
                <VersionId>marker-1</VersionId>
              </DeleteMarker>
            </ListVersionsResult>
            "#,
        )
        .expect("parse object versions");
        assert!(!page.is_truncated);
        assert_eq!(page.versions.len(), 2);
        assert_eq!(page.delete_markers.len(), 1);
        let versions = validate_listed_object_versions(
            page.versions
                .into_iter()
                .map(|item| artifact_object_version_from_list_item(item, false))
                .chain(
                    page.delete_markers
                        .into_iter()
                        .map(|item| artifact_object_version_from_list_item(item, true)),
                )
                .collect::<Result<Vec<_>, _>>()
                .expect("normalize listed versions"),
        )
        .expect("validate listed versions");
        assert_eq!(
            versions,
            vec![
                ArtifactObjectVersion {
                    version_id: Some("version-new".to_owned()),
                    is_delete_marker: false,
                },
                ArtifactObjectVersion {
                    version_id: Some("version-old".to_owned()),
                    is_delete_marker: false,
                },
                ArtifactObjectVersion {
                    version_id: Some("marker-1".to_owned()),
                    is_delete_marker: true,
                },
            ]
        );
    }

    #[test]
    fn oss_version_list_response_enforces_streaming_size_limit() {
        let mut body = b"abc".to_vec();
        append_bounded_oss_response_chunk(&mut body, b"d", 4)
            .expect("response remains within limit");
        assert_eq!(body, b"abcd");

        let err = append_bounded_oss_response_chunk(&mut body, b"e", 4)
            .expect_err("response exceeding limit must fail");
        assert!(err.message().contains("4"));
        assert_eq!(body, b"abcd");
    }

    #[test]
    fn checksum_requires_sha256_hex() {
        assert!(normalize_checksum_sha256(&"a".repeat(64)).is_ok());
        assert!(normalize_checksum_sha256("abc").is_err());
        assert!(normalize_checksum_sha256(&"z".repeat(64)).is_err());
    }
}
