use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose};
use chrono::SecondsFormat;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use url::Url;

pub const STORAGE_PROVIDER_LOCAL: &str = "local";
pub const STORAGE_PROVIDER_ALIYUN_OSS: &str = "aliyun_oss";
pub const DEFAULT_ALIYUN_OSS_REGION: &str = "oss-cn-hangzhou";
pub const DEFAULT_ALIYUN_OSS_ENDPOINT: &str = "https://oss-cn-hangzhou.aliyuncs.com";
pub const DEFAULT_ALIYUN_OSS_OBJECT_PREFIX: &str = "easy-deploy/releases";
pub const DEFAULT_ALIYUN_OSS_UPLOAD_TTL_SECONDS: i64 = 900;
pub const DEFAULT_ALIYUN_OSS_DOWNLOAD_TTL_SECONDS: i64 = 600;
pub const ARTIFACT_UPLOAD_CONTENT_TYPE: &str = "application/octet-stream";

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

#[derive(Debug)]
pub enum ArtifactStorageError {
    InvalidInput(String),
    Unsupported(String),
}

impl ArtifactStorageError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Unsupported(message) => message,
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

    pub fn presign_upload(
        &self,
        object_key: &str,
    ) -> Result<PresignedUpload, ArtifactStorageError> {
        self.validate_required()?;
        let expires = unix_timestamp_now() + self.upload_url_ttl_seconds;
        let url = self.presign("PUT", object_key, ARTIFACT_UPLOAD_CONTENT_TYPE, expires)?;
        Ok(PresignedUpload {
            method: "PUT",
            url,
            headers: vec![(
                "Content-Type".to_owned(),
                ARTIFACT_UPLOAD_CONTENT_TYPE.to_owned(),
            )],
            expires_at: timestamp_text(expires),
        })
    }

    pub fn presign_download(
        &self,
        object_key: &str,
    ) -> Result<PresignedDownload, ArtifactStorageError> {
        self.validate_required()?;
        let expires = unix_timestamp_now() + self.download_url_ttl_seconds;
        Ok(PresignedDownload {
            url: self.presign("GET", object_key, "", expires)?,
            expires_at: timestamp_text(expires),
        })
    }

    fn presign(
        &self,
        method: &str,
        object_key: &str,
        content_type: &str,
        expires: i64,
    ) -> Result<String, ArtifactStorageError> {
        let object_key = normalize_object_key(object_key)?;
        let resource = format!("/{}/{}", self.bucket, object_key);
        let string_to_sign = format!("{method}\n\n{content_type}\n{expires}\n{resource}");
        let mut mac =
            HmacSha1::new_from_slice(self.access_key_secret.as_bytes()).map_err(|_| {
                ArtifactStorageError::InvalidInput("阿里云 OSS AccessKey Secret 无效".to_owned())
            })?;
        mac.update(string_to_sign.as_bytes());
        let signature = general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let mut url = object_url(&self.endpoint, &self.bucket, &object_key)?;
        url.query_pairs_mut()
            .append_pair("OSSAccessKeyId", &self.access_key_id)
            .append_pair("Expires", &expires.to_string())
            .append_pair("Signature", &signature);
        Ok(url.to_string())
    }
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
    let mut url = Url::parse(endpoint).map_err(|_| {
        ArtifactStorageError::InvalidInput("阿里云 OSS Endpoint 不是有效 URL".to_owned())
    })?;
    let host = url.host_str().unwrap_or_default().to_owned();
    url.set_host(Some(&format!("{bucket}.{host}")))
        .map_err(|_| {
            ArtifactStorageError::InvalidInput("无法构造阿里云 OSS Bucket 访问域名".to_owned())
        })?;
    url.set_path(&format!("/{}", encode_object_path(object_key)));
    url.set_query(None);
    Ok(url)
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
            vec![(
                "Content-Type".to_owned(),
                ARTIFACT_UPLOAD_CONTENT_TYPE.to_owned()
            )]
        );

        let download = config
            .presign_download("easy-deploy/releases/orders/v1.2.3/pkg.tar.gz")
            .expect("presign download");
        assert!(download.url.contains("Signature="));
    }

    #[test]
    fn checksum_requires_sha256_hex() {
        assert!(normalize_checksum_sha256(&"a".repeat(64)).is_ok());
        assert!(normalize_checksum_sha256("abc").is_err());
        assert!(normalize_checksum_sha256(&"z".repeat(64)).is_err());
    }
}
