//! Placeholder substitution for greeting messages.

pub const DEFAULT_TEMPLATE: &str =
    "Welcome to **{server}**, {user}! You are member #{member_count}.";
pub const MAX_TEMPLATE_LEN: usize = 1500;
/// Discord message content limit.
pub const MAX_MESSAGE_LEN: usize = 2000;

/// Placeholders shown in the dashboard help box.
pub const PLACEHOLDERS: &[(&str, &str)] = &[
    ("{user}", "Mention of the new member (pings them)"),
    ("{display_name}", "Server nickname, or global display name"),
    ("{username}", "Discord username"),
    ("{user_id}", "Discord user ID"),
    ("{server}", "Server name"),
    ("{member_count}", "Member count after the join"),
];

pub struct Vars {
    pub user_id: u64,
    pub username: String,
    pub display_name: String,
    pub server: String,
    pub member_count: u64,
}

/// Replaces known `{placeholder}` tokens. Unknown tokens are left untouched.
/// Names are markdown escaped so odd nicknames cannot break the message formatting.
pub fn render(template: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let end = start + len;
        out.push_str(&rest[..start]);
        match replacement(&rest[start + 1..end], vars) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    truncate(out, MAX_MESSAGE_LEN)
}

fn replacement(token: &str, vars: &Vars) -> Option<String> {
    Some(match token {
        "user" => format!("<@{}>", vars.user_id),
        "username" => escape_markdown(&vars.username),
        "display_name" => escape_markdown(&vars.display_name),
        "user_id" => vars.user_id.to_string(),
        "server" => escape_markdown(&vars.server),
        "member_count" => vars.member_count.to_string(),
        _ => return None,
    })
}

pub fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']' | '(' | ')'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn truncate(mut s: String, max_chars: usize) -> String {
    if let Some((idx, _)) = s.char_indices().nth(max_chars) {
        s.truncate(idx);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars {
            user_id: 42,
            username: "new_guy".into(),
            display_name: "New Guy".into(),
            server: "Scorpio".into(),
            member_count: 7,
        }
    }

    #[test]
    fn default_template() {
        assert_eq!(
            render(DEFAULT_TEMPLATE, &vars()),
            "Welcome to **Scorpio**, <@42>! You are member #7."
        );
    }

    #[test]
    fn all_placeholders() {
        assert_eq!(
            render(
                "{user}|{username}|{display_name}|{user_id}|{server}|{member_count}",
                &vars()
            ),
            "<@42>|new\\_guy|New Guy|42|Scorpio|7"
        );
    }

    #[test]
    fn unknown_and_broken_tokens_kept() {
        assert_eq!(render("{nope} {user", &vars()), "{nope} {user");
        assert_eq!(render("{{user}}", &vars()), "{{user}}");
        assert_eq!(render("}{user}{", &vars()), "}<@42>{");
    }

    #[test]
    fn names_cannot_inject_markdown() {
        let mut v = vars();
        v.display_name = "**bold** `code`".into();
        assert_eq!(render("{display_name}", &v), "\\*\\*bold\\*\\* \\`code\\`");
    }

    #[test]
    fn output_truncated_to_discord_limit() {
        let mut v = vars();
        v.server = "x".repeat(100);
        let out = render(&"{server}".repeat(100), &v);
        assert_eq!(out.chars().count(), MAX_MESSAGE_LEN);
    }

    #[test]
    fn multibyte_safe() {
        let mut v = vars();
        v.display_name = "Zażółć 🦂".into();
        assert_eq!(render("hi {display_name}!", &v), "hi Zażółć 🦂!");
    }
}
