//! Small formatting helpers shared by the CLI tables and the dashboard.

/// Compact relative age: `4m`, `3h`, `6d`, `5w`, `2y`. Deliberately short,
/// because it lives in a narrow column next to 500 other rows.
pub fn age(then: i64, now: i64) -> String {
    if then <= 0 {
        return "-".into();
    }
    let secs = (now - then).max(0);
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s if s < 2_592_000 => format!("{}w", s / 604_800),
        s if s < 31_536_000 => format!("{}mo", s / 2_592_000),
        s => format!("{}y", s / 31_536_000),
    }
}

/// Render the change counts the way git users read them: `+2 ~5 ?1`, with
/// staged, unstaged and untracked. Empty stays visually quiet.
pub fn changes(staged: u32, unstaged: u32, untracked: u32, conflicts: u32) -> String {
    let mut parts = Vec::new();
    if conflicts > 0 {
        parts.push(format!("!{conflicts}"));
    }
    if staged > 0 {
        parts.push(format!("+{staged}"));
    }
    if unstaged > 0 {
        parts.push(format!("~{unstaged}"));
    }
    if untracked > 0 {
        parts.push(format!("?{untracked}"));
    }
    if parts.is_empty() {
        "·".into()
    } else {
        parts.join(" ")
    }
}

/// Truncate to a display width, with an ellipsis when it doesn't fit.
pub fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".into();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// A count, or a quiet marker when it's zero.
pub fn count(n: u32) -> String {
    if n == 0 {
        "·".into()
    } else {
        n.to_string()
    }
}

pub fn duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_compact() {
        let now = 1_000_000_000;
        assert_eq!(age(now - 30, now), "now");
        assert_eq!(age(now - 240, now), "4m");
        assert_eq!(age(now - 7_200, now), "2h");
        assert_eq!(age(now - 259_200, now), "3d");
        assert_eq!(age(now - 1_209_600, now), "2w");
        assert_eq!(age(0, now), "-");
    }

    #[test]
    fn change_summaries() {
        assert_eq!(changes(0, 0, 0, 0), "·");
        assert_eq!(changes(2, 5, 1, 0), "+2 ~5 ?1");
        assert_eq!(changes(0, 0, 0, 3), "!3");
    }

    #[test]
    fn truncation_keeps_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
    }
}
