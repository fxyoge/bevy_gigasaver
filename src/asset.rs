//! Authored Bevy asset paths, resolved through the destination AssetServer.
use bevy_asset::{Asset, Handle};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer, A: Asset>(
    value: &Handle<A>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value
        .path()
        .ok_or_else(|| serde::ser::Error::custom("persistent asset has no authored path"))?
        .to_string()
        .serialize(serializer)
}
pub fn deserialize<'de, D: Deserializer<'de>, A: Asset>(
    deserializer: D,
) -> Result<Handle<A>, D::Error> {
    let path = String::deserialize(deserializer)?;
    crate::entity::assets()
        .map(|server| server.load(path))
        .ok_or_else(|| {
            serde::de::Error::custom(
                "asset deserialization requires AssetServer and a restore context",
            )
        })
}
pub mod optional {
    use super::*;
    pub fn serialize<S: Serializer, A: Asset>(
        value: &Option<Handle<A>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(|handle| {
                handle.path().map(ToString::to_string).ok_or_else(|| {
                    serde::ser::Error::custom("persistent asset has no authored path")
                })
            })
            .transpose()?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, A: Asset>(
        deserializer: D,
    ) -> Result<Option<Handle<A>>, D::Error> {
        let path = Option::<String>::deserialize(deserializer)?;
        match path {
            None => Ok(None),
            Some(path) => crate::entity::assets()
                .map(|server| Some(server.load(path)))
                .ok_or_else(|| {
                    serde::de::Error::custom(
                        "asset deserialization requires AssetServer and a restore context",
                    )
                }),
        }
    }
}
