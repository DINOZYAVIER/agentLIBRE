use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessPolicy {
    #[serde(default)]
    pub allowed_rooms: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessDecision {
    Allowed,
    Denied { reason: &'static str },
}

impl AccessPolicy {
    pub fn evaluate(&self, room_id: &str, user_id: &str) -> AccessDecision {
        if self.allowed_rooms.is_empty() && self.allowed_users.is_empty() {
            return AccessDecision::Denied {
                reason: "no access policy configured",
            };
        }

        if !self.allowed_rooms.is_empty() && !self.allowed_rooms.iter().any(|room| room == room_id)
        {
            return AccessDecision::Denied {
                reason: "room is not allowed",
            };
        }

        if !self.allowed_users.is_empty() && !self.allowed_users.iter().any(|user| user == user_id)
        {
            return AccessDecision::Denied {
                reason: "user is not allowed",
            };
        }

        AccessDecision::Allowed
    }

    pub fn allows(&self, room_id: &str, user_id: &str) -> bool {
        matches!(self.evaluate(room_id, user_id), AccessDecision::Allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_denies_every_room_and_user() {
        let policy = AccessPolicy::default();

        assert_eq!(
            policy.evaluate("!room:example", "@user:example"),
            AccessDecision::Denied {
                reason: "no access policy configured"
            }
        );
        assert!(!policy.allows("!room:example", "@user:example"));
    }

    #[test]
    fn policy_denies_unknown_room_before_user() {
        let policy = AccessPolicy {
            allowed_rooms: vec!["!allowed:example".to_owned()],
            allowed_users: vec!["@allowed:example".to_owned()],
        };

        assert_eq!(
            policy.evaluate("!other:example", "@other:example"),
            AccessDecision::Denied {
                reason: "room is not allowed"
            }
        );
    }
}
