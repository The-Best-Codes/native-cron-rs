//! Small escaping helpers for command lines that aren't covered by a
//! dedicated format crate (PowerShell argument quoting, the Win32
//! argument-quoting convention used by `CommandLineToArgvW`, and systemd's
//! unit-file quoting rules).

/// Quotes a string as a systemd unit-file value: wraps it in double quotes
/// and escapes backslashes, quotes, and `%` (a systemd specifier character).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// Like [`systemd_quote`], but also escapes `$` as `$$` since `ExecStart=`
/// and similar directives additionally expand environment variables.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_exec_quote(value: &str) -> String {
    systemd_quote(value).replace('$', "$$")
}

/// Quotes a string as a single-quoted PowerShell literal.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Quotes a string as a Win32 command-line argument, following the same
/// backslash/quote escaping rules as `CommandLineToArgvW`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_argument(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c == '"') {
        return value.to_string();
    }

    let mut result = String::from("\"");
    let mut backslashes = 0usize;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            result.push_str(&"\\".repeat(backslashes * 2 + 1));
            result.push('"');
            backslashes = 0;
        } else {
            result.push_str(&"\\".repeat(backslashes));
            result.push(character);
            backslashes = 0;
        }
    }
    result.push_str(&"\\".repeat(backslashes * 2));
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_powershell_strings() {
        assert_eq!(powershell_quote("it's secret"), "'it''s secret'");
    }

    #[test]
    fn quotes_windows_arguments() {
        assert_eq!(windows_argument("plain"), "plain");
        assert_eq!(windows_argument("has space"), "\"has space\"");
        assert_eq!(windows_argument("it's safe"), "\"it's safe\"");
    }

    #[test]
    fn quotes_systemd_values() {
        assert_eq!(systemd_quote("100%"), "\"100%%\"");
        assert_eq!(systemd_exec_quote("a$b%"), "\"a$$b%%\"");
    }
}
