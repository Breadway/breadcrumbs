//! Guards against two release workflows publishing the same tag. Forgejo's
//! `.forgejo/workflows/release.yml` is the authoritative publisher (GitHub
//! auto-mirrors from origin), so the GitHub copy must never run on a tag
//! push: it would write the same `/srv/breadway-dl` path and race it.

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `on:` block of a workflow, as the lines between `on:` and the next
/// top-level key. Enough to check trigger shape without a YAML dependency.
fn on_block(yml: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in yml.lines() {
        if line.starts_with("on:") {
            inside = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if inside {
            if !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#') {
                break;
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[test]
fn github_release_workflow_does_not_trigger_on_tag_push() {
    let on = on_block(&read(".github/workflows/release.yml"));
    assert!(!on.is_empty(), "no `on:` block found");
    assert!(
        !on.contains("tags"),
        "the GitHub release workflow must not run on tag pushes; Forgejo publishes:\n{on}"
    );
    assert!(
        on.contains("workflow_dispatch"),
        "manual dispatch is the only remaining trigger:\n{on}"
    );
}

#[test]
fn forgejo_release_workflow_still_triggers_on_version_tags() {
    let on = on_block(&read(".forgejo/workflows/release.yml"));
    assert!(
        on.contains("v*"),
        "Forgejo must stay the tag publisher:\n{on}"
    );
}

/// CRUMB-49: a manual dispatch must name an existing stable tag. The dispatch
/// ref (main, a feature branch) must never become the published version or
/// reach a build/publish step.
#[test]
fn github_release_dispatch_requires_an_existing_tag() {
    let yml = read(".github/workflows/release.yml");
    for line in yml.lines().filter(|l| !l.trim_start().starts_with('#')) {
        assert!(
            !line.contains("GITHUB_REF_NAME"),
            "the dispatch ref must never be used as the published version: {line}"
        );
    }
    assert!(
        yml.contains("  version:") && yml.contains("required: true"),
        "workflow_dispatch must require an explicit version input"
    );
    assert!(
        yml.contains("git ls-remote --exit-code --tags origin"),
        "the version input must be checked against an existing tag"
    );
    assert!(
        yml.contains("needs: validate"),
        "the publishing job must depend on the validation job"
    );
    assert!(
        yml.contains("needs.validate.outputs.version != ''"),
        "the publishing job must be gated on the validated version"
    );
    assert!(
        yml.contains("refs/tags/${{ needs.validate.outputs.version }}"),
        "the build must check out the validated tag, not the dispatch ref"
    );
}
