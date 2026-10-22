//! Opaque invocation-local provider identity. Equality is UTF16, not a lossy
//! display string. No generated request/attempt identity or ambient registry.
use super::{NativeValue, entry_json::Json};
use serde::{Serialize, Serializer};
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct CallId {
    units: Vec<u16>,
    display: String,
}
impl std::fmt::Debug for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallId")
            .field("utf16_units", &self.units.len())
            .finish_non_exhaustive()
    }
}
impl CallId {
    pub(crate) fn from_json(value: Option<&Json>) -> Option<Self> {
        let Some(Json::String(units)) = value else {
            return None;
        };
        Some(Self {
            display: String::from_utf16_lossy(units),
            units: units.clone(),
        })
    }
    pub(crate) fn native(&self) -> NativeValue {
        NativeValue::new(Json::String(self.units.clone()))
    }
    /// Preserve existing delivery identities for scalar IDs. For non-scalar
    /// IDs use a separate, length-framed UTF16 domain; display is not authority.
    /// The invocation scope is the harness-generated UUID, never tool input.
    pub(crate) fn scoped_preimage(&self, scope: &str) -> Vec<u8> {
        if self.units.iter().copied().eq(self.display.encode_utf16()) {
            return format!("{scope}:{}", self.display).into_bytes();
        }
        let mut bytes = b"pantheon-call-id-utf16-v1\0".to_vec();
        bytes.extend_from_slice(&(scope.len() as u64).to_le_bytes());
        bytes.extend_from_slice(scope.as_bytes());
        bytes.extend_from_slice(&(self.units.len() as u64).to_le_bytes());
        for unit in &self.units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }
    pub fn as_str(&self) -> &str {
        &self.display
    }
}
impl From<String> for CallId {
    fn from(value: String) -> Self {
        Self {
            units: value.encode_utf16().collect(),
            display: value,
        }
    }
}
impl From<&str> for CallId {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}
impl std::ops::Deref for CallId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.display
    }
}
impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}
// Safe host diagnostic projection only. Native Back IDs use native() above;
// never route opaque non-scalar IDs through json!(CallId).
impl Serialize for CallId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.display.serialize(s)
    }
}
impl PartialEq<str> for CallId {
    fn eq(&self, other: &str) -> bool {
        self.units.iter().copied().eq(other.encode_utf16())
    }
}
impl PartialEq<&str> for CallId {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}
impl PartialEq<String> for CallId {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}
impl PartialEq<CallId> for serde_json::Value {
    fn eq(&self, other: &CallId) -> bool {
        self.as_str().is_some_and(|s| other == s)
    }
}
