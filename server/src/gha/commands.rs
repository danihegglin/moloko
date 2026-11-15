//! Step → runner communication: environment files (`$GITHUB_ENV`, `$GITHUB_OUTPUT`, ...) and
//! workflow commands printed to stdout (`::add-mask::`, `::group::`, ...).

/// Parse a `$GITHUB_ENV` / `$GITHUB_OUTPUT` / `$GITHUB_STATE` file: `NAME=value` lines and
/// multiline `NAME<<DELIM` ... `DELIM` blocks.
pub fn parse_kv_file(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let heredoc = line.find("<<");
        let eq = line.find('=');
        match (heredoc, eq) {
            (Some(h), e) if e.is_none_or(|e| h < e) => {
                let (name, delim) = (&line[..h], &line[h + 2..]);
                let mut value = Vec::new();
                for l in lines.by_ref() {
                    if l == delim {
                        break;
                    }
                    value.push(l);
                }
                out.push((name.to_string(), value.join("\n")));
            }
            (_, Some(e)) => out.push((line[..e].to_string(), line[e + 1..].to_string())),
            _ => {}
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum Command {
    SetOutput { name: String, value: String },
    SaveState { name: String, value: String },
    AddMask(String),
    Group(String),
    EndGroup,
    Error(String),
    Warning(String),
    Notice(String),
    Debug(String),
    StopCommands(String),
    /// Recognised but not shown, e.g. problem matchers.
    Ignore,
}

/// Parse `::name key=v,key2=v2::message`. Returns None for ordinary output lines.
pub fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim_start();
    // Legacy form: `##[name key=v]message`.
    let (head, msg) = if let Some(rest) = line.strip_prefix("##[") {
        rest.split_once(']')?
    } else {
        line.strip_prefix("::")?.split_once("::")?
    };
    let (name, props) = head.split_once(' ').unwrap_or((head, ""));
    let prop = |key: &str| {
        props
            .split(',')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| unescape_prop(v))
    };
    let msg = unescape_data(msg);
    let with_location = |msg: String| match (prop("file"), prop("line")) {
        (Some(f), Some(l)) => format!("{f}:{l}: {msg}"),
        (Some(f), None) => format!("{f}: {msg}"),
        _ => msg,
    };
    Some(match name {
        "set-output" => Command::SetOutput {
            name: prop("name")?,
            value: msg,
        },
        "save-state" => Command::SaveState {
            name: prop("name")?,
            value: msg,
        },
        "add-mask" => Command::AddMask(msg),
        "group" => Command::Group(msg),
        "endgroup" => Command::EndGroup,
        "error" => Command::Error(with_location(msg)),
        "warning" => Command::Warning(with_location(msg)),
        "notice" => Command::Notice(with_location(msg)),
        "debug" => Command::Debug(msg),
        "stop-commands" => Command::StopCommands(msg),
        "add-matcher" | "remove-matcher" | "echo" => Command::Ignore,
        _ => return None,
    })
}

fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0A", "\n").replace("%25", "%")
}

fn unescape_prop(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%3A", ":")
        .replace("%2C", ",")
        .replace("%25", "%")
}

/// `INPUT_<NAME>`: GitHub uppercases the input name and replaces spaces with underscores.
pub fn input_env_name(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files() {
        let f = "A=1\nB=x=y\n\nMULTI<<EOF\nline 1\nline 2\nEOF\nC<<ghadelimiter_1\n\nghadelimiter_1\n";
        assert_eq!(
            parse_kv_file(f),
            [
                ("A".into(), "1".into()),
                ("B".into(), "x=y".into()),
                ("MULTI".into(), "line 1\nline 2".into()),
                ("C".into(), "".into()),
            ]
        );
    }

    #[test]
    fn workflow_commands() {
        assert_eq!(
            parse_command("::set-output name=v::hello%0Aworld"),
            Some(Command::SetOutput {
                name: "v".into(),
                value: "hello\nworld".into()
            })
        );
        assert_eq!(parse_command("::add-mask::s3cret"), Some(Command::AddMask("s3cret".into())));
        assert_eq!(parse_command("::group::Install"), Some(Command::Group("Install".into())));
        assert_eq!(parse_command("::endgroup::"), Some(Command::EndGroup));
        assert_eq!(
            parse_command("::error file=a.rs,line=3::boom"),
            Some(Command::Error("a.rs:3: boom".into()))
        );
        assert_eq!(parse_command("plain output"), None);
        assert_eq!(parse_command("::unknown::x"), None);
        assert_eq!(parse_command("##[add-matcher]/x/tsc.json"), Some(Command::Ignore));
        assert_eq!(parse_command("##[group]Run x"), Some(Command::Group("Run x".into())));
        assert_eq!(input_env_name("node version"), "INPUT_NODE_VERSION");
        assert_eq!(input_env_name("fetch-depth"), "INPUT_FETCH-DEPTH");
    }
}
