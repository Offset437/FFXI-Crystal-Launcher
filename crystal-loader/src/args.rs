//! Command-line parsing. Only our own flags are recognised; everything else
//! (Windower's `--lang US`, Ashita's additions, a stray `/game eAZcFcB`) is ignored.

use std::fmt;

pub const PIPE_PREFIX: &str = r"\\.\pipe\";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    /// Full local pipe path, `\\.\pipe\<name>`.
    pub pipe: String,
    pub launcher_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgError {
    MissingPipe,
    MissingLauncherPid,
    MissingValue(&'static str),
    BadPipeName,
    BadPid,
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgError::MissingPipe => write!(f, "missing --crystal-pipe"),
            ArgError::MissingLauncherPid => write!(f, "missing --crystal-launcher-pid"),
            ArgError::MissingValue(flag) => write!(f, "{flag} needs a value"),
            ArgError::BadPipeName => write!(f, "--crystal-pipe is not a local pipe name"),
            ArgError::BadPid => write!(f, "--crystal-launcher-pid is not a process id"),
        }
    }
}

/// Parses the arguments after the program name.
pub fn parse<I, S>(args: I) -> Result<Args, ArgError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut pipe: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let arg = arg.as_ref();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        match flag {
            "--crystal-pipe" => {
                let v = take_value(inline, &mut it, "--crystal-pipe")?;
                pipe = Some(normalize_pipe(&v)?);
            }
            "--crystal-launcher-pid" => {
                let v = take_value(inline, &mut it, "--crystal-launcher-pid")?;
                let n: u32 = v.trim().parse().map_err(|_| ArgError::BadPid)?;
                if n == 0 {
                    return Err(ArgError::BadPid);
                }
                pid = Some(n);
            }
            _ => {} // unknown: ignored on purpose
        }
    }
    Ok(Args {
        pipe: pipe.ok_or(ArgError::MissingPipe)?,
        launcher_pid: pid.ok_or(ArgError::MissingLauncherPid)?,
    })
}

fn take_value<I, S>(inline: Option<String>, it: &mut I, flag: &'static str) -> Result<String, ArgError>
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    if let Some(v) = inline {
        return Ok(v);
    }
    match it.next() {
        Some(v) if !v.as_ref().starts_with("--") => Ok(v.as_ref().to_string()),
        _ => Err(ArgError::MissingValue(flag)),
    }
}

/// Accepts a bare name or `\\.\pipe\<name>`; anything that could point at
/// another machine or path is refused.
pub fn normalize_pipe(v: &str) -> Result<String, ArgError> {
    let name = if v.get(..PIPE_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(PIPE_PREFIX)) {
        &v[PIPE_PREFIX.len()..]
    } else {
        v
    };
    let ok = !name.is_empty()
        && name.len() <= 200
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && name != "."
        && name != "..";
    if !ok {
        return Err(ArgError::BadPipeName);
    }
    Ok(format!("{PIPE_PREFIX}{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_contract_args_and_ignores_windower_lang() {
        let a = parse(["--crystal-pipe", "crystal-loader-0123abcd", "--crystal-launcher-pid", "4242", "--lang", "US"]).unwrap();
        assert_eq!(a.pipe, r"\\.\pipe\crystal-loader-0123abcd");
        assert_eq!(a.launcher_pid, 4242);
    }

    #[test]
    fn accepts_full_pipe_path_and_equals_form() {
        let a = parse([r"--crystal-pipe=\\.\pipe\crystal-loader-ff", "--crystal-launcher-pid=7"]).unwrap();
        assert_eq!(a.pipe, r"\\.\pipe\crystal-loader-ff");
        assert_eq!(a.launcher_pid, 7);
    }

    #[test]
    fn ignores_unknown_args_anywhere() {
        let a = parse(["/game", "eAZcFcB", "--ashita-thing", "x", "--crystal-pipe", "p", "-net", "3", "--crystal-launcher-pid", "9", "--selftest-offline"]).unwrap();
        assert_eq!(a.pipe, r"\\.\pipe\p");
        assert_eq!(a.launcher_pid, 9, "the retired --selftest-offline is an ignored unknown flag");
    }

    #[test]
    fn rejects_missing_or_bad_values() {
        assert_eq!(parse(["--crystal-launcher-pid", "1"]), Err(ArgError::MissingPipe));
        assert_eq!(parse(["--crystal-pipe", "p"]), Err(ArgError::MissingLauncherPid));
        assert_eq!(parse(["--crystal-pipe"]), Err(ArgError::MissingValue("--crystal-pipe")));
        assert_eq!(parse(["--crystal-pipe", "--crystal-launcher-pid", "1"]), Err(ArgError::MissingValue("--crystal-pipe")));
        assert_eq!(parse(["--crystal-pipe", "p", "--crystal-launcher-pid", "abc"]), Err(ArgError::BadPid));
        assert_eq!(parse(["--crystal-pipe", "p", "--crystal-launcher-pid", "0"]), Err(ArgError::BadPid));
        assert_eq!(parse(["--crystal-pipe", "p", "--crystal-launcher-pid", "-5"]), Err(ArgError::BadPid));
    }

    #[test]
    fn refuses_remote_or_odd_pipe_names() {
        for bad in [r"\\server\pipe\x", r"\\.\pipe\a\b", "a/b", "", "..", "na me", r"\\.\pipe\", "C:\\x"] {
            assert_eq!(normalize_pipe(bad), Err(ArgError::BadPipeName), "{bad}");
        }
    }
}
