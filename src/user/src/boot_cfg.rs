//! Minimal parser for board flash boot configs (`boards/<board>/init.conf`)
//! and optional runtime init files (e.g. `/sd/etc/init`).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoardInit<'a> {
    pub config_path: Option<&'a [u8]>,
    pub exec_path: Option<&'a [u8]>,
    pub exec_arg: &'a [u8],
}

/// Parse a board flash boot config consisting of `config <path>` and `exec <path> [arg]` lines.
pub fn parse_board_config(bytes: &[u8]) -> BoardInit<'_> {
    let mut spec = BoardInit {
        config_path: None,
        exec_path: None,
        exec_arg: b"",
    };
    for raw in bytes.split(|&b| b == b'\n') {
        let line = raw.trim_ascii();
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"config ") {
            let path = rest.trim_ascii();
            if path.starts_with(b"/") {
                spec.config_path = Some(path);
            }
        } else if let Some(rest) = line.strip_prefix(b"exec ") {
            let (path, arg) = split_command(rest.trim_ascii());
            if path.starts_with(b"/") {
                spec.exec_path = Some(path);
                spec.exec_arg = arg;
            }
        }
    }
    spec
}

/// Parse a runtime init file (such as `/sd/etc/init`), taking the first non-empty,
/// non-comment line as either `exec </path> [arg]` or `</path> [arg]`.
pub fn parse_runtime_init(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    for raw in bytes.split(|&b| b == b'\n') {
        let line = raw.trim_ascii();
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let body = line.strip_prefix(b"exec ").unwrap_or(line).trim_ascii();
        let (path, arg) = split_command(body);
        if path.starts_with(b"/") {
            return Some((path, arg));
        }
        return None;
    }
    None
}

fn split_command(line: &[u8]) -> (&[u8], &[u8]) {
    let mut parts = line.splitn(2, |&b| b == b' ' || b == b'\t');
    let path = parts.next().unwrap_or_default();
    let arg = parts.next().unwrap_or_default().trim_ascii();
    (path, arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tab5_board_config() {
        let cfg = b"# Tab5 boot\n\nconfig /sd/etc/init\nexec /sd/bin/shell.elf\n";
        assert_eq!(
            parse_board_config(cfg),
            BoardInit {
                config_path: Some(b"/sd/etc/init"),
                exec_path: Some(b"/sd/bin/shell.elf"),
                exec_arg: b"",
            }
        );
    }

    #[test]
    fn parses_minimal_rom_board_config_without_sd() {
        let cfg = b"# ESP32-C3 minimal\nexec /rom/blink.elf 500\r\n";
        assert_eq!(
            parse_board_config(cfg),
            BoardInit {
                config_path: None,
                exec_path: Some(b"/rom/blink.elf"),
                exec_arg: b"500",
            }
        );
    }

    #[test]
    fn parses_runtime_init_with_or_without_exec_prefix() {
        assert_eq!(
            parse_runtime_init(b"# custom\n/sd/bin/demo.elf --fast\n"),
            Some((b"/sd/bin/demo.elf".as_slice(), b"--fast".as_slice()))
        );
        assert_eq!(
            parse_runtime_init(b"exec /sd/bin/shell.elf\n"),
            Some((b"/sd/bin/shell.elf".as_slice(), b"".as_slice()))
        );
        assert_eq!(parse_runtime_init(b"# empty\n   \n"), None);
        assert_eq!(parse_runtime_init(b"relative/path.elf\n"), None);
    }
}
