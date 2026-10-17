//! The local roster, used before sign-in (C1): one private room holding only this device.

use super::{
    EndpointId, MAX_USERNAME, Me, Roster, RosterError, is_valid_name, private_room,
    this_device_entry,
};

/// Version string of every local roster.
pub const LOCAL_VERSION: &str = "local";

/// `Me.username` for the local provider: settings `username`, else the OS user name slugged, else `local`.
pub fn local_username(
    settings_username: Option<&str>,
    os_user: Option<&str>,
) -> Result<String, RosterError> {
    if let Some(name) = settings_username.filter(|n| !n.is_empty()) {
        if !is_valid_name(name, MAX_USERNAME) {
            return Err(RosterError(format!(
                "settings username {name:?} must be 1 to {MAX_USERNAME} ASCII letters, digits, '.', '_' or '-'"
            )));
        }
        return Ok(name.to_string());
    }
    let slugged = os_user.map(slug).unwrap_or_default();
    Ok(if slugged.is_empty() {
        "local".to_string()
    } else {
        slugged
    })
}

/// The OS user name from `USER`, else `LOGNAME`.
pub fn os_user_name() -> Option<String> {
    ["USER", "LOGNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
}

/// Lowercase ASCII letters, digits and single hyphens, at most [`MAX_USERNAME`] characters.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(MAX_USERNAME);
    out.trim_end_matches('-').to_string()
}

/// The local roster: one private room with `me` and only this device. It admits nothing from the network.
pub fn local_roster(
    me: &Me,
    this_device: &EndpointId,
    device_name: &str,
    now_ms: u64,
) -> Result<Roster, RosterError> {
    let mut roster = Roster {
        version: LOCAL_VERSION.to_string(),
        fetched_at: now_ms,
        rooms: vec![private_room(
            me,
            vec![this_device_entry(this_device, device_name)],
        )],
    };
    roster.validate(me)?;
    Ok(roster)
}

#[cfg(test)]
mod tests {
    use super::super::Admission;
    use super::*;

    #[test]
    fn username_order() {
        assert_eq!(local_username(Some("sam"), Some("x")).unwrap(), "sam");
        assert_eq!(local_username(None, Some("Sam Doe")).unwrap(), "sam-doe");
        assert_eq!(local_username(Some(""), Some("maya")).unwrap(), "maya");
        assert_eq!(local_username(None, Some("!!!")).unwrap(), "local");
        assert_eq!(local_username(None, None).unwrap(), "local");
        assert!(local_username(Some("sam/x"), None).is_err());
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("--Ab__c  d--"), "ab-c-d");
        assert_eq!(slug(&"a".repeat(60)).len(), MAX_USERNAME);
    }

    #[test]
    fn local_admits_nothing() {
        let key = iroh::SecretKey::from_bytes(&[7; 32]).public();
        let me = Me {
            username: "sam".into(),
            display: "sam".into(),
        };
        let roster = local_roster(&me, &key, "sam-linux", 5).unwrap();
        assert_eq!(roster.rooms.len(), 1);
        assert_eq!(roster.rooms[0].members[0].devices.len(), 1);
        assert!(!Admission::compute(&me, &key, &roster).admits_others());
    }
}
