//! Hooks for version-control systems that have none of their own.
//!
//! git and Mercurial will run a command of your choosing after a checkout or
//! a commit. Perforce, Subversion and most others will not. For those the
//! only place to stand is the shell: a function with the command's name that
//! runs the real command and then `cindex-rs --hook`. This module writes that
//! function for each shell it knows, and decides -- from the command line the
//! function passes along -- whether a refresh is worth starting at all.
//!
//! `cmd.exe` is deliberately absent. A doskey macro has to mention `$*`
//! twice, and `$*` is everything after the command name, so `tool sync &&
//! build` would run `build` twice; a batch-file shim mangles `%` in
//! arguments. Neither is safe to hand to anyone.

use std::ffi::OsString;
use std::path::Path;

/// A shell to write a wrapper for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// sh, bash, zsh, ksh, dash: a POSIX function.
    Posix,
    Fish,
    /// csh and tcsh: an alias, since they have no functions.
    Csh,
    /// Windows PowerShell 5 and PowerShell 7.
    PowerShell,
}

impl Shell {
    /// The names `--shell` accepts, for messages.
    pub const NAMES: &'static str = "sh, bash, zsh, ksh, dash, fish, csh, tcsh, powershell, pwsh";

    pub fn from_name(name: &str) -> Option<Shell> {
        // A path is fine (`/usr/bin/zsh`, `C:\...\pwsh.exe`): take the stem.
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let base = base.strip_suffix(".exe").unwrap_or(base);
        match base.to_ascii_lowercase().as_str() {
            "sh" | "bash" | "zsh" | "ksh" | "dash" => Some(Shell::Posix),
            "fish" => Some(Shell::Fish),
            "csh" | "tcsh" => Some(Shell::Csh),
            "powershell" | "pwsh" => Some(Shell::PowerShell),
            _ => None,
        }
    }

    /// The shell `$SHELL` names, if it is one of ours. Unset on Windows
    /// outside an MSYS or Cygwin shell, where the caller has to say.
    pub fn from_env() -> Option<Shell> {
        Shell::from_name(std::env::var("SHELL").ok()?.as_str())
    }

    /// A name `--shell` accepts for this shell.
    pub fn canonical_name(self) -> &'static str {
        match self {
            Shell::Posix => "sh",
            Shell::Fish => "fish",
            Shell::Csh => "csh",
            Shell::PowerShell => "powershell",
        }
    }
}

/// A command name that can be written into shell source as it stands: letters,
/// digits, `.`, `_` and `-`, not starting with punctuation. Anything else is
/// refused rather than quoted -- it is going to become a function name.
pub fn valid_tool(tool: &str) -> bool {
    let mut chars = tool.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Why a wrapper could not be written.
#[derive(Debug, PartialEq, Eq)]
pub enum WrapError {
    /// The tool's name is not a plain command name.
    BadTool,
    /// This program's own path contains a character that cannot be written
    /// safely into that shell's source.
    Unquotable(char),
}

impl std::fmt::Display for WrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WrapError::BadTool => write!(
                f,
                "not a plain command name (letters, digits, `.`, `_`, `-`)"
            ),
            WrapError::Unquotable(c) => write!(
                f,
                "this program's path contains {c:?}, which cannot be written safely into \
                 that shell's source -- install it somewhere with a plainer path"
            ),
        }
    }
}

impl std::error::Error for WrapError {}

/// Shell source that makes `tool` run the real `tool` and then the hook.
///
/// `exe` is this program. It is written into the wrapper as an absolute path,
/// so the wrapper does not depend on `PATH` still containing it later.
pub fn wrapper(tool: &str, shell: Shell, exe: &Path) -> Result<String, WrapError> {
    if !valid_tool(tool) {
        return Err(WrapError::BadTool);
    }
    let native = exe.to_string_lossy().into_owned();
    // The Unix-family shells take forward slashes everywhere, including on
    // Windows under MSYS, and a backslash would need quoting in each of them.
    let slashed = native.replace('\\', "/");
    if let Some(c) = native.chars().find(|c| c.is_control()) {
        return Err(WrapError::Unquotable(c));
    }
    Ok(match shell {
        Shell::Posix => {
            let exe = format!("'{}'", slashed.replace('\'', r"'\''"));
            format!(
                "# csearch-rs hook: run {tool}, then refresh the code index if files changed.\n\
                 {tool}() {{\n\
                 \x20   command {tool} \"$@\"\n\
                 \x20   csearch_rs_status=$?\n\
                 \x20   {exe} --hook --after {tool} -- \"$@\" </dev/null >/dev/null 2>&1\n\
                 \x20   set -- \"$csearch_rs_status\"\n\
                 \x20   unset csearch_rs_status\n\
                 \x20   return \"$1\"\n\
                 }}\n"
            )
        }
        Shell::Fish => {
            let exe = format!("'{}'", slashed.replace('\\', r"\\").replace('\'', r"\'"));
            format!(
                "# csearch-rs hook: run {tool}, then refresh the code index if files changed.\n\
                 function {tool} --wraps {tool}\n\
                 \x20   command {tool} $argv\n\
                 \x20   set -l csearch_rs_status $status\n\
                 \x20   {exe} --hook --after {tool} -- $argv </dev/null >/dev/null 2>&1\n\
                 \x20   return $csearch_rs_status\n\
                 end\n"
            )
        }
        Shell::Csh => {
            // The whole alias is one single-quoted word, so the path cannot
            // be single-quoted inside it; double quotes are re-read when the
            // alias runs, and these characters would mean something then.
            if let Some(c) = slashed.chars().find(|c| "'\"$`!\\".contains(*c)) {
                return Err(WrapError::Unquotable(c));
            }
            // One line and no comment: csh's `eval` joins lines. The
            // parentheses make it a single command, so that a pipe or `&&`
            // written after the alias applies to all of it; `sh -c exit` is
            // how a csh alias ends with a status of its choosing.
            //
            // An alias is text, not a function: `\!*` is the words typed
            // after the command, redirections among them, and they are read
            // again when the alias runs. The command gets them as typed. The
            // hook gets them quoted (`:q`), or `p4 sync > log` would give
            // it a second `>` -- "Ambiguous output redirect", and csh runs
            // none of the line -- and a backquoted argument would run twice.
            format!(
                "alias {tool} '( \\{tool} \\!* ; set csearch_rs_status = $status ; \
                 \"{slashed}\" --hook --after {tool} -- \\!*:q >& /dev/null < /dev/null ; \
                 sh -c \"exit $csearch_rs_status\" )'\n"
            )
        }
        Shell::PowerShell => {
            let exe = format!("'{}'", native.replace('\'', "''"));
            format!(
                "# csearch-rs hook: run {tool}, then refresh the code index if files changed.\n\
                 function {tool} {{\n\
                 \x20   $csearchRsTool = @(Get-Command -Name '{tool}' -CommandType Application -ErrorAction Stop)[0]\n\
                 \x20   if ($MyInvocation.ExpectingInput) {{ $input | & $csearchRsTool @args }} else {{ & $csearchRsTool @args }}\n\
                 \x20   $csearchRsStatus = $LASTEXITCODE\n\
                 \x20   & {exe} --hook --after {tool} '--' @args *> $null\n\
                 \x20   $global:LASTEXITCODE = $csearchRsStatus\n\
                 }}\n"
            )
        }
    })
}

/// The line that switches the wrapper on from a shell's startup file, shown
/// beside the wrapper when `--print-hook` is run at a terminal. `name` is
/// what the shell was called on the command line, so the line reads back the
/// way it was asked for.
pub fn activation(tool: &str, shell: Shell, name: &str, program: &str) -> String {
    let print = format!("{program} --print-hook {tool} --shell {name}");
    match shell {
        Shell::Posix => format!("eval \"$({print})\""),
        Shell::Fish => format!("{print} | source"),
        Shell::Csh => format!("eval \"`{print}`\""),
        Shell::PowerShell => format!("{print} | Out-String | Invoke-Expression"),
    }
}

/// Whether `tool args...` is known to leave the working tree as it was, so
/// that a refresh after it would be wasted. Anything not positively known to
/// be read-only is assumed to have changed something.
///
/// No tool is known yet: every wrapped command triggers the (cheap) check.
pub fn is_read_only(_tool: &str, _args: &[OsString]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_recognised_by_name_or_path() {
        assert_eq!(Shell::from_name("bash"), Some(Shell::Posix));
        assert_eq!(Shell::from_name("/usr/bin/zsh"), Some(Shell::Posix));
        assert_eq!(Shell::from_name("/bin/tcsh"), Some(Shell::Csh));
        assert_eq!(Shell::from_name("fish"), Some(Shell::Fish));
        assert_eq!(
            Shell::from_name(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Some(Shell::PowerShell)
        );
        assert_eq!(Shell::from_name("PowerShell"), Some(Shell::PowerShell));
        assert_eq!(Shell::from_name("cmd"), None);
        assert_eq!(Shell::from_name("nu"), None);
        assert_eq!(Shell::from_name(""), None);
    }

    #[test]
    fn every_shell_answers_to_its_own_canonical_name() {
        for shell in [Shell::Posix, Shell::Fish, Shell::Csh, Shell::PowerShell] {
            assert_eq!(Shell::from_name(shell.canonical_name()), Some(shell));
            assert!(Shell::NAMES
                .split(", ")
                .any(|n| n == shell.canonical_name()));
        }
        // Everything NAMES advertises is accepted.
        for name in Shell::NAMES.split(", ") {
            assert!(Shell::from_name(name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_activation_line_names_the_shell_as_it_was_asked_for() {
        assert_eq!(
            activation("p4", Shell::Posix, "zsh", "cindex-rs"),
            "eval \"$(cindex-rs --print-hook p4 --shell zsh)\""
        );
        assert_eq!(
            activation("p4", Shell::Csh, "tcsh", "cindex-rs"),
            "eval \"`cindex-rs --print-hook p4 --shell tcsh`\""
        );
        assert_eq!(
            activation("svn", Shell::Fish, "fish", "cindex-rs"),
            "cindex-rs --print-hook svn --shell fish | source"
        );
        assert_eq!(
            activation("p4", Shell::PowerShell, "pwsh", "cindex-rs"),
            "cindex-rs --print-hook p4 --shell pwsh | Out-String | Invoke-Expression"
        );
    }

    #[test]
    fn only_plain_command_names_become_functions() {
        for ok in ["p4", "svn", "git-lfs", "hg", "a.b_c", "7z"] {
            assert!(valid_tool(ok), "{ok}");
        }
        for bad in [
            "", "p4 sync", "p4;rm", "$(x)", "-p4", ".p4", "p4()", "a/b", "p4\n", "é",
        ] {
            assert!(!valid_tool(bad), "{bad:?}");
            assert_eq!(
                wrapper(bad, Shell::Posix, Path::new("/bin/cindex-rs")),
                Err(WrapError::BadTool)
            );
        }
    }

    #[test]
    fn the_program_path_is_quoted_for_each_shell() {
        let awkward = Path::new("/opt/it's here/cindex-rs");
        let sh = wrapper("p4", Shell::Posix, awkward).unwrap();
        assert!(sh.contains(r"'/opt/it'\''s here/cindex-rs' --hook"), "{sh}");
        let fish = wrapper("p4", Shell::Fish, awkward).unwrap();
        assert!(
            fish.contains(r"'/opt/it\'s here/cindex-rs' --hook"),
            "{fish}"
        );
        let ps = wrapper("p4", Shell::PowerShell, awkward).unwrap();
        assert!(ps.contains("& '/opt/it''s here/cindex-rs' --hook"), "{ps}");
        // csh cannot carry a quote inside its alias; it says so.
        assert_eq!(
            wrapper("p4", Shell::Csh, awkward),
            Err(WrapError::Unquotable('\''))
        );
        let csh = wrapper("p4", Shell::Csh, Path::new("/opt/my tools/cindex-rs")).unwrap();
        assert!(csh.contains("\"/opt/my tools/cindex-rs\" --hook"), "{csh}");
        assert_eq!(csh.lines().count(), 1, "csh's eval needs a single line");
        // The command takes its words as typed, the hook takes them quoted.
        assert_eq!(csh.matches(r"\!*").count(), 2, "{csh}");
        assert_eq!(csh.matches(r"\!*:q").count(), 1, "{csh}");
        assert!(csh.contains(r"( \p4 \!* ;"), "{csh}");
        assert!(csh.contains(r"--after p4 -- \!*:q >& /dev/null"), "{csh}");

        // Windows paths: forward slashes for the Unix-family shells, the
        // native form for PowerShell.
        let win = Path::new(r"C:\Program Files\csearch-rs\cindex-rs.exe");
        let sh = wrapper("p4", Shell::Posix, win).unwrap();
        assert!(
            sh.contains("'C:/Program Files/csearch-rs/cindex-rs.exe'"),
            "{sh}"
        );
        let ps = wrapper("p4", Shell::PowerShell, win).unwrap();
        assert!(
            ps.contains(r"& 'C:\Program Files\csearch-rs\cindex-rs.exe'"),
            "{ps}"
        );
    }
}
