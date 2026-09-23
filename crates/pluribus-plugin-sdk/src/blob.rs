//! JSON blob references shared by plugin boundaries.
//! Callers validate digest contents, media types, and access.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobRef {
    pub algorithm: String,
    pub digest: String,
    pub size: u64,
    #[serde(rename = "media-type", alias = "media_type", alias = "mediaType")]
    pub media_type: String,
}

impl BlobRef {
    pub fn from_value_strict(value: serde_json::Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "blob reference must be an object".to_owned())?;
        if let Some(key) = object.keys().find(|key| {
            !matches!(
                key.as_str(),
                "algorithm" | "digest" | "size" | "media_type" | "mediaType" | "media-type"
            )
        }) {
            return Err(format!("unknown blob reference field {key}"));
        }
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    pub fn to_json(&self, spelling: MediaTypeSpelling) -> serde_json::Value {
        let mut value = serde_json::json!({
            "algorithm": self.algorithm,
            "digest": self.digest,
            "size": self.size,
        });
        let key = match spelling {
            MediaTypeSpelling::Snake => "media_type",
            MediaTypeSpelling::Camel => "mediaType",
            MediaTypeSpelling::Kebab => "media-type",
        };
        value[key] = serde_json::Value::String(self.media_type.clone());
        value
    }
}

impl From<BlobRef> for crate::pluribus::plugin::types::BlobRef {
    fn from(value: BlobRef) -> Self {
        Self {
            algorithm: value.algorithm,
            digest: value.digest,
            size: value.size,
            media_type: value.media_type,
        }
    }
}

impl From<crate::pluribus::plugin::types::BlobRef> for BlobRef {
    fn from(value: crate::pluribus::plugin::types::BlobRef) -> Self {
        Self {
            algorithm: value.algorithm,
            digest: value.digest,
            size: value.size,
            media_type: value.media_type,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaTypeSpelling {
    Snake,
    Camel,
    Kebab,
}

#[cfg(test)]
mod tests {
    use super::{BlobRef, MediaTypeSpelling};

    #[test]
    fn accepts_and_emits_supported_media_type_spellings() {
        for key in ["media_type", "mediaType", "media-type"] {
            let value = serde_json::json!({
                "algorithm": "sha256",
                "digest": "abc",
                "size": 3,
                key: "image/png"
            });
            let blob: BlobRef = serde_json::from_value(value).unwrap();
            assert_eq!(blob.media_type, "image/png");
            assert_eq!(
                serde_json::to_value(blob).unwrap()["media-type"],
                "image/png"
            );
        }
    }

    #[test]
    fn emits_each_boundary_spelling() {
        let blob = BlobRef {
            algorithm: "sha256".into(),
            digest: "abc".into(),
            size: 3,
            media_type: "image/png".into(),
        };
        for (spelling, key) in [
            (MediaTypeSpelling::Snake, "media_type"),
            (MediaTypeSpelling::Camel, "mediaType"),
            (MediaTypeSpelling::Kebab, "media-type"),
        ] {
            assert_eq!(blob.to_json(spelling)[key], "image/png");
        }
    }

    #[test]
    fn rejects_missing_or_conflicting_fields() {
        let missing = serde_json::json!({"algorithm":"sha256","digest":"abc","size":3});
        assert!(serde_json::from_value::<BlobRef>(missing).is_err());
        let conflicting = serde_json::json!({
            "algorithm":"sha256", "digest":"abc", "size":3,
            "media_type":"image/png", "mediaType":"image/jpeg"
        });
        assert!(serde_json::from_value::<BlobRef>(conflicting).is_err());
    }

    #[test]
    fn strict_parser_rejects_unknown_fields() {
        let value = serde_json::json!({
            "algorithm":"sha256", "digest":"abc", "size":3,
            "mediaType":"image/png", "transport":"extra"
        });
        assert!(BlobRef::from_value_strict(value).is_err());
    }
}
