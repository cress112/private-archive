use crate::PreparationError;
use std::ffi::OsString;
pub(crate) struct Target {
    pub uri: OsString,
    pub recursive: bool,
    pub depth: Option<u32>,
}
pub(crate) fn parse(args: &[OsString]) -> Result<Vec<Target>, PreparationError> {
    let mut targets: Vec<Target> = vec![];
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        let text = token.to_str();
        if matches!(text, Some("-r" | "--recursive")) {
            let target = targets
                .last_mut()
                .ok_or("--recursive must follow its URI")?;
            if target.recursive {
                return Err("duplicate --recursive for one URI".into());
            }
            target.recursive = true;
        } else if text == Some("--recursive-depth")
            || text.is_some_and(|v| v.starts_with("--recursive-depth="))
        {
            let value = match text.unwrap().split_once('=') {
                Some((_, value)) => value,
                None => tokens
                    .next()
                    .and_then(|v| v.to_str())
                    .ok_or("--recursive-depth requires a positive integer")?,
            };
            let target = targets
                .last_mut()
                .ok_or("--recursive-depth must follow its URI")?;
            if target.depth.is_some() {
                return Err("duplicate depth for one URI".into());
            }
            target.depth = Some(
                value
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("--recursive-depth requires a positive integer")?,
            );
        } else if text.is_some_and(|v| v.starts_with('-')) {
            return Err("only --recursive and --recursive-depth may follow a restore URI; put global options before the first URI".into());
        } else {
            targets.push(Target {
                uri: token.clone(),
                recursive: false,
                depth: None,
            });
        }
    }
    if targets.is_empty() {
        return Err("restore requires at least one URI".into());
    }
    if targets.iter().any(|t| t.depth.is_some() && !t.recursive) {
        return Err("--recursive-depth requires --recursive for the same URI".into());
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;
    #[test]
    fn restore_grammar_keeps_globals_before_uri_and_resets_each_uri_scope() {
        let cli = Cli::try_parse_from([
            "private-archive",
            "restore",
            "--restore-duration",
            "1w",
            "--restore-tier",
            "Standard",
            "s3://bucket/p",
            "--recursive-depth=2",
            "-r",
            "s3://bucket/exact",
        ])
        .unwrap();
        assert_eq!(cli.restore_tier.as_deref(), Some("Standard"));
        let Command::Restore(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.restore_duration.get(), 7);
        let targets = parse(&args.targets).unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets[0].recursive);
        assert_eq!(targets[0].depth, Some(2));
        assert!(!targets[1].recursive);
        assert_eq!(targets[1].depth, None);
        for tokens in [
            vec!["-r", "s3://bucket/p"],
            vec!["s3://bucket/p", "--recursive-depth", "0"],
            vec![
                "s3://bucket/p",
                "--recursive-depth",
                "2",
                "s3://bucket/q",
                "-r",
            ],
            vec!["s3://bucket/p", "--ignore", "a"],
            vec!["s3://bucket/p", "--restore-duration", "7d"],
            vec!["s3://bucket/p", "-r", "-r"],
        ] {
            assert!(parse(&tokens.into_iter().map(OsString::from).collect::<Vec<_>>()).is_err());
        }
    }
    #[test]
    fn restore_duration_is_explicit_positive_and_does_not_round_hours() {
        use archive_sync::RestoreDays;
        for (s, n) in [("7d", 7), ("1w", 7), ("2w", 14), ("01d", 1)] {
            assert_eq!(s.parse::<RestoreDays>().unwrap().get(), n);
        }
        for s in [
            "",
            "0d",
            "-1d",
            "+1d",
            "1.5d",
            "32h",
            "1D",
            "1",
            " 7d",
            "4294967295w",
            "2147483648d",
        ] {
            assert!(s.parse::<RestoreDays>().is_err(), "{s}");
        }
        assert!(Cli::try_parse_from(["private-archive", "restore", "s3://bucket/p"]).is_err());
    }
}
