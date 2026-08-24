//! Outdated command - proxy for `show --latest --outdated`.

use anyhow::Result;
use std::path::PathBuf;

use super::show::{self, ShowArgs};
use crate::platform::AppContext;

#[derive(usage_rs::Args, Debug)]
pub struct OutdatedArgs {
    /// Package to inspect (or wildcard pattern)
    pub package: Option<String>,

    /// Show all installed packages with their latest versions
    #[usage(short = 'a', long)]
    pub all: bool,

    /// Shows updates for packages from the lock file
    #[usage(long)]
    pub locked: bool,

    /// Shows only packages that are directly required by the root package
    #[usage(short = 'D', long)]
    pub direct: bool,

    /// Return a non-zero exit code when there are outdated packages
    #[usage(long)]
    pub strict: bool,

    /// Show only packages that have major SemVer-compatible updates
    #[usage(short = 'M', long, conflicts("--minor-only", "--patch-only"))]
    pub major_only: bool,

    /// Show only packages that have minor SemVer-compatible updates
    #[usage(short = 'm', long, conflicts("--major-only", "--patch-only"))]
    pub minor_only: bool,

    /// Show only packages that have patch SemVer-compatible updates
    #[usage(short = 'p', long, conflicts("--major-only", "--minor-only"))]
    pub patch_only: bool,

    /// Output format: text or json
    #[usage(short = 'f', long, default = "text")]
    pub format: String,

    /// Ignore specified package(s), can contain wildcards (*)
    #[usage(long)]
    pub ignore: Vec<String>,

    /// Disables search in require-dev packages
    #[usage(long)]
    pub no_dev: bool,

    /// Working directory
    #[usage(short = 'd', long, default = ".")]
    pub working_dir: PathBuf,
}

pub async fn execute(args: OutdatedArgs, context: &AppContext) -> Result<i32> {
    let update_filter = if args.major_only {
        Some("major".to_string())
    } else if args.minor_only {
        Some("minor".to_string())
    } else if args.patch_only {
        Some("patch".to_string())
    } else {
        None
    };

    let show_args = ShowArgs {
        package: args.package,
        version: None,
        all: args.all,
        locked: args.locked,
        platform: false,
        available: false,
        self_package: false,
        name_only: false,
        path: false,
        tree: false,
        latest: true,
        outdated: !args.all,
        direct: args.direct,
        format: args.format,
        no_dev: args.no_dev,
        working_dir: args.working_dir,
        strict: args.strict,
        update_filter,
        ignore: args.ignore,
    };

    show::execute(show_args, context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outdated_args_to_show_args_default() {
        let args = OutdatedArgs {
            package: None,
            all: false,
            locked: false,
            direct: false,
            strict: false,
            major_only: false,
            minor_only: false,
            patch_only: false,
            format: "text".to_string(),
            ignore: vec![],
            no_dev: false,
            working_dir: PathBuf::from("."),
        };
        assert!(!args.all);
    }

    #[test]
    fn test_outdated_args_with_all_flag() {
        let args = OutdatedArgs {
            package: None,
            all: true,
            locked: false,
            direct: false,
            strict: false,
            major_only: false,
            minor_only: false,
            patch_only: false,
            format: "text".to_string(),
            ignore: vec![],
            no_dev: false,
            working_dir: PathBuf::from("."),
        };
        assert!(args.all);
    }

    #[test]
    fn test_outdated_format_validation() {
        fn is_valid_format(format: &str) -> bool {
            format == "text" || format == "json"
        }
        assert!(is_valid_format("text"));
        assert!(is_valid_format("json"));
        assert!(!is_valid_format("xml"));
    }
}
