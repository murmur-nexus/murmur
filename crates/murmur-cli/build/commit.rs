//! Where the commit `mur version` reports comes from. Shared by `build.rs`, which fixes the
//! commit at compile time, and by the `mur` binary's unit tests.

/// The commit a crates.io source package was cut from: `git.sha1` in its `.cargo_vcs_info.json`.
pub fn commit_from_vcs_info(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let sha1 = value.get("git")?.get("sha1")?.as_str()?;
    full_commit(sha1)
}

/// The commit `git rev-parse HEAD` printed.
pub fn commit_from_rev_parse(stdout: &str) -> Option<String> {
    full_commit(stdout.trim())
}

/// `candidate` when it is a full commit hash: 40 lowercase hex characters.
fn full_commit(candidate: &str) -> Option<String> {
    let is_full = candidate.len() == 40
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    is_full.then(|| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "321640ff1c0d2e3f4a5b6c7d8e9f0a1b2c3d4e5f";

    #[test]
    fn vcs_info_gives_its_sha1() {
        let json = format!(
            "{{\n  \"git\": {{\n    \"sha1\": \"{SHA}\"\n  }},\n  \"path_in_vcs\": \"crates/murmur-cli\"\n}}\n"
        );
        assert_eq!(commit_from_vcs_info(&json).as_deref(), Some(SHA));
    }

    #[test]
    fn vcs_info_without_git_sha1_gives_none() {
        assert_eq!(commit_from_vcs_info(r#"{"path_in_vcs": ""}"#), None);
        assert_eq!(commit_from_vcs_info(r#"{"git": {"dirty": true}}"#), None);
    }

    #[test]
    fn malformed_vcs_info_gives_none() {
        assert_eq!(
            commit_from_vcs_info(&format!(r#"{{"git": {{"sha1": "{SHA}""#)),
            None
        );
        assert_eq!(commit_from_vcs_info(""), None);
    }

    #[test]
    fn a_sha1_that_is_not_forty_lowercase_hex_gives_none() {
        for sha1 in [
            &SHA[..39],
            &format!("{SHA}0"),
            &SHA.to_uppercase(),
            "g21640ff1c0d2e3f4a5b6c7d8e9f0a1b2c3d4e5f",
            "",
        ] {
            let json = format!(r#"{{"git": {{"sha1": "{sha1}"}}}}"#);
            assert_eq!(commit_from_vcs_info(&json), None, "{sha1}");
        }
        assert_eq!(commit_from_vcs_info(r#"{"git": {"sha1": 7}}"#), None);
    }

    #[test]
    fn rev_parse_output_is_trimmed() {
        assert_eq!(
            commit_from_rev_parse(&format!("{SHA}\n")).as_deref(),
            Some(SHA)
        );
    }

    #[test]
    fn rev_parse_output_that_is_not_a_full_commit_gives_none() {
        assert_eq!(commit_from_rev_parse(""), None);
        assert_eq!(commit_from_rev_parse("HEAD\n"), None);
        assert_eq!(commit_from_rev_parse(&SHA[..12]), None);
    }
}
