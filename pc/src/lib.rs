//! Pure parts of the PC side, unit-tested without building the elevated exe.
#[cfg(windows)]
pub mod image;
pub mod keymap;
pub mod switch;

/// `text` (an .ini file) with `key=value` replacing that key's line, or appended if absent.
/// Comments and other keys stay as they are.
pub fn set_ini_key(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key}={value}\n");
    let is_key = |l: &str| l.split_once('=').is_some_and(|(k, _)| k.trim() == key);
    let mut out: String = text.lines().map(|l| if is_key(l) { line.clone() } else { format!("{l}\n") }).collect();
    if !text.lines().any(is_key) {
        out += &line;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::set_ini_key;

    #[test]
    fn replaces_or_appends_only_that_key() {
        let ini = "# side=left is a comment\nside = right\nsidebar=1\n";
        assert_eq!(set_ini_key(ini, "side", "left"), "# side=left is a comment\nside=left\nsidebar=1\n");
        assert_eq!(set_ini_key("a=1", "side", "left"), "a=1\nside=left\n");
    }
}
