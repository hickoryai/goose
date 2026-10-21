//! The active ACP session's title and busy state, as the downstream agent
//! reports them through `session_info_update` (router-acp: `_meta.routerAcp.busy`).
//! The CLI reads it for the terminal title.

use std::sync::{LazyLock, Mutex};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionActivity {
    pub title: Option<String>,
    pub busy: bool,
}

static ACTIVITY: LazyLock<Mutex<SessionActivity>> = LazyLock::new(Default::default);

pub fn current() -> SessionActivity {
    ACTIVITY.lock().map(|a| a.clone()).unwrap_or_default()
}

pub(crate) fn update(title: Option<String>, busy: Option<bool>) {
    if let Ok(mut activity) = ACTIVITY.lock() {
        if let Some(title) = title {
            activity.title = Some(title);
        }
        if let Some(busy) = busy {
            activity.busy = busy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_updates_keep_the_other_field() {
        update(Some("Chordzy [opus]".into()), Some(true));
        update(None, Some(false));
        assert_eq!(
            current(),
            SessionActivity {
                title: Some("Chordzy [opus]".into()),
                busy: false
            }
        );
        update(Some("Chordzy [sonnet]".into()), None);
        assert_eq!(current().title.as_deref(), Some("Chordzy [sonnet]"));
        assert!(!current().busy);
    }
}
