//! Origins, their stored labels, and history filters.

use super::*;

/// Who caused a change. Stamped by the adapter that connects the client to core, never
/// taken from the client.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Origin {
    Ui,
    MediaControls,
    Remote {
        /// A stable identifier of the paired device, chosen by the adapter (not a display
        /// name): it is stored with history rows and journal actions.
        device: String,
    },
    /// An agent.
    Agent {
        /// A stable identifier of the agent, chosen by the adapter (not a display name): it
        /// is stored with history rows and journal actions. A registered agent's name
        /// (`RegisterAgent`: letters, digits, `-`, `_`, `.`, never `:`); an agent the
        /// adapter names itself, without a token (on mobile, say), is `<platform>:<name>`
        /// (`ios:shortcuts`), so the two never meet (guarantee 16).
        name: String,
        /// The agent's session (one connection, for example), an identifier the adapter
        /// chooses, unique and never reused (random, such as a UUID); core only compares it
        /// (grants for one session). Not part of the stored origin (`agent:<name>`) nor of
        /// idempotency keys (guarantee 13). Left out when the adapter gives none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    System,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl Origin {
    /// An agent origin without a session.
    pub fn agent(name: impl Into<String>) -> Self {
        Origin::Agent {
            name: name.into(),
            session: None,
        }
    }

    /// The origin's kind as in the schema's `x-origins`: `ui`, `media_controls`, `remote`,
    /// `agent`, `system`.
    pub fn kind(&self) -> &'static str {
        match self {
            Origin::Ui => "ui",
            Origin::MediaControls => "media_controls",
            Origin::Remote { .. } => "remote",
            Origin::Agent { .. } => "agent",
            Origin::System => "system",
            Origin::Unknown => "unknown",
        }
    }

    /// The origin as one string, as core stores it with history rows and journal actions:
    /// `ui`, `media_controls`, `system`, `agent:<name>`, `remote:<device>`, `unknown`. An
    /// agent's session is not part of it. [`Origin::from_label`] reads it back;
    /// [`OriginFilter`] matches it.
    pub fn label(&self) -> String {
        match self {
            Origin::Remote { device } => format!("remote:{device}"),
            Origin::Agent { name, .. } => format!("agent:{name}"),
            other => other.kind().to_owned(),
        }
    }

    /// The origin a [`Origin::label`] stands for; `Unknown` for anything else.
    pub fn from_label(label: &str) -> Origin {
        match label.split_once(':') {
            Some(("agent", name)) => Origin::agent(name),
            Some(("remote", device)) => Origin::Remote {
                device: device.into(),
            },
            Some(_) => Origin::Unknown,
            None => match label {
                "ui" => Origin::Ui,
                "media_controls" => Origin::MediaControls,
                "system" => Origin::System,
                _ => Origin::Unknown,
            },
        }
    }
}

/// The origins the schema's `x-origins` lists, with a representative of each.
pub fn origin_kinds() -> [Origin; 5] {
    [
        Origin::Ui,
        Origin::MediaControls,
        Origin::Remote {
            device: String::new(),
        },
        Origin::agent(""),
        Origin::System,
    ]
}

/// Which actions the `actions` query lists: one origin, or every origin of a kind
/// (`{"type":"agent"}`: every agent; `{"type":"agent","name":"x"}`: that one).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OriginFilter {
    Ui,
    MediaControls,
    System,
    /// Every remote device, or the one named.
    Remote {
        #[serde(default)]
        device: Option<String>,
    },
    /// Every agent, or the one named.
    Agent {
        #[serde(default)]
        name: Option<String>,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl OriginFilter {
    /// The label (or, ending in `:`, label prefix) of the origins it keeps; `None` for
    /// `Unknown`.
    pub fn label_prefix(&self) -> Option<String> {
        Some(match self {
            OriginFilter::Ui => Origin::Ui.label(),
            OriginFilter::MediaControls => Origin::MediaControls.label(),
            OriginFilter::System => Origin::System.label(),
            OriginFilter::Remote { device: None } => "remote:".into(),
            OriginFilter::Remote {
                device: Some(device),
            } => Origin::Remote {
                device: device.clone(),
            }
            .label(),
            OriginFilter::Agent { name: None } => "agent:".into(),
            OriginFilter::Agent { name: Some(name) } => Origin::agent(name.clone()).label(),
            OriginFilter::Unknown => return None,
        })
    }
}
