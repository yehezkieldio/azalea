//! Strongly typed Discord identifiers used across module boundaries.

use serde::Deserialize;
use twilight_model::id::{
    Id,
    marker::{ApplicationMarker, ChannelMarker, InteractionMarker, MessageMarker, UserMarker},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(transparent))]
pub struct ApplicationId(u64);

impl ApplicationId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn twilight(self) -> Id<ApplicationMarker> {
        Id::new(self.0)
    }
}

impl<'de> Deserialize<'de> for ApplicationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = u64::deserialize(deserializer)?;
        if raw == 0 {
            return Err(serde::de::Error::custom("application_id must be non-zero"));
        }
        Ok(Self::new(raw))
    }
}

impl From<ApplicationId> for Id<ApplicationMarker> {
    fn from(value: ApplicationId) -> Self {
        value.twilight()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(u64);

impl ChannelId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn twilight(self) -> Id<ChannelMarker> {
        Id::new(self.0)
    }
}

impl From<Id<ChannelMarker>> for ChannelId {
    fn from(value: Id<ChannelMarker>) -> Self {
        Self::new(value.get())
    }
}

impl From<ChannelId> for Id<ChannelMarker> {
    fn from(value: ChannelId) -> Self {
        value.twilight()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(u64);

impl MessageId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn twilight(self) -> Id<MessageMarker> {
        Id::new(self.0)
    }
}

impl From<Id<MessageMarker>> for MessageId {
    fn from(value: Id<MessageMarker>) -> Self {
        Self::new(value.get())
    }
}

impl From<MessageId> for Id<MessageMarker> {
    fn from(value: MessageId) -> Self {
        value.twilight()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId(u64);

impl UserId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn twilight(self) -> Id<UserMarker> {
        Id::new(self.0)
    }
}

impl From<Id<UserMarker>> for UserId {
    fn from(value: Id<UserMarker>) -> Self {
        Self::new(value.get())
    }
}

impl From<UserId> for Id<UserMarker> {
    fn from(value: UserId) -> Self {
        value.twilight()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InteractionId(u64);

impl InteractionId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn twilight(self) -> Id<InteractionMarker> {
        Id::new(self.0)
    }
}

impl From<Id<InteractionMarker>> for InteractionId {
    fn from(value: Id<InteractionMarker>) -> Self {
        Self::new(value.get())
    }
}

impl From<InteractionId> for Id<InteractionMarker> {
    fn from(value: InteractionId) -> Self {
        value.twilight()
    }
}
