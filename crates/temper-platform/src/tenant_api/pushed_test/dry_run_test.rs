//! `"dry_run": true`: a push that only checks. It answers what the install of
//! the same files would answer, refusing with the same words, and leaves the
//! tenant as it found it.

use super::*;

/// What the tenant serves and keeps for the app, as a caller and the server see it.
#[derive(Debug, PartialEq)]
struct Kept {
    record: Option<String>,
    listed: Option<Listed>,
    versions: Vec<String>,
    registered: Option<String>,
    resolved: Option<String>,
    policy: Option<String>,
    csdl: Option<String>,
}

async fn kept(server: &Server, tenant: &str) -> Kept {
    let id = TenantId::new(tenant);
    let (registered, csdl) = {
        let registry = server.state.registry.read().expect("registry");
        (
            registry
                .get_spec(&id, "SentinelMonitor")
                .map(|spec| spec.ioa_source.clone()),
            registry
                .get_tenant(&id)
                .map(|config| config.csdl_xml.to_string()),
        )
    };
    Kept {
        record: record(&server.ps, tenant)
            .await
            .map(|record| format!("{record:?}")),
        listed: listed_for(&server.state, tenant).await,
        versions: pushed_versions(server.data_dir.path()),
        registered,
        resolved: tenant_os_app(tenant, APP).map(|bundle| {
            bundle
                .specs
                .into_iter()
                .map(|(entity_type, source)| format!("{entity_type}\n{source}"))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        policy: server.state.server.authz.get_tenant_policy_text(tenant),
        csdl,
    }
}

fn dry(files: &BTreeMap<String, String>) -> serde_json::Value {
    serde_json::json!({ "files": files, "dry_run": true })
}

/// The app's files changed one way each, every one a version the install
/// refuses, by what it checks.
fn refused_versions() -> Vec<(&'static str, BTreeMap<String, String>)> {
    let mut cases = Vec::new();

    let mut cedar = with_comment("cedar");
    cedar.insert(
        "policies/evolution.cedar".to_string(),
        "permit(principal, action, resource) when { this is not cedar };".to_string(),
    );
    cases.push(("Cedar that does not compile", cedar));

    // NoAlerts takes has_alerts, a flag the move itself writes, and reads it nowhere.
    let mut lost = with_comment("lost param");
    let spec = lost.get_mut(SPEC).expect("sentinel spec");
    let unread = "effect = [\"has_alerts = false\"]\n";
    assert_eq!(spec.matches(unread).count(), 1, "NoAlerts' effect");
    *spec = spec.replace(unread, &format!("{unread}params = [\"has_alerts\"]\n"));
    cases.push(("a param the move never stores", lost));

    let mut unparsed = with_comment("unparsed");
    unparsed.insert(SPEC.to_string(), "[automaton]\nname = \n".to_string());
    cases.push(("a spec that does not parse", unparsed));

    let mut csdl = with_comment("csdl");
    csdl.insert("model.csdl.xml".to_string(), "<edmx:Edmx".to_string());
    cases.push(("a CSDL that does not parse", csdl));

    let mut stray = with_comment("stray");
    stray.insert(
        "stray.ioa.toml".to_string(),
        app_files()[SPEC].replace("SentinelMonitor", "StrayMonitor"),
    );
    cases.push(("a spec with no entity type in the CSDL", stray));

    let mut cross = with_comment("cross");
    cross.insert(
        "specs/cross-invariants.toml".to_string(),
        "this is not toml =".to_string(),
    );
    cases.push(("cross-invariants that do not parse", cross));

    let mut reactions = with_comment("reactions");
    reactions.insert(
        "specs/reactions.toml".to_string(),
        "[[reaction]]\n".to_string(),
    );
    cases.push(("a reactions.toml", reactions));

    let mut module = with_comment("module");
    module.get_mut("app.toml").expect("app.toml").push_str(
        "\n[[wasm_modules]]\nname = \"absent\"\ntarget = \"wasm32-unknown-unknown\"\ncriticality = \"app-required\"\nstartup_loading = \"lazy\"\nprovenance = \"bundled-artifact\"\nimport_class = \"temper-host\"\n",
    );
    cases.push(("a required module with no binary", module));

    let mut unnamed = with_comment("unnamed");
    unnamed.remove("APP.md");
    cases.push(("files that are not the app", unnamed));

    cases
}

/// An answer with the folders it names left out: a check loads the version
/// from a folder of its own, the install from its cache, and the words around
/// them are what the two must share.
fn without_paths(answer: &str) -> String {
    let mut kept = String::new();
    let mut rest = answer;
    while let Some(at) = rest.find(" at '") {
        let (before, after) = rest.split_at(at + " at '".len());
        kept.push_str(before);
        match after.find('\'') {
            Some(end) => rest = &after[end..],
            None => {
                rest = after;
                break;
            }
        }
    }
    kept.push_str(rest);
    kept
}

/// Every version the install refuses, checked dry on a tenant with nothing
/// installed and on one serving the app: the check refuses it with the words
/// the install then refuses it with, and the tenant is as it was.
#[tokio::test]
async fn a_dry_run_refuses_what_the_install_refuses_and_changes_nothing() {
    let mut wrong = Vec::new();
    for installed_before in [false, true] {
        for (case, files) in refused_versions() {
            let case = format!("{case} (installed before: {installed_before})");
            let tenant = unique_tenant("dry-refused");
            let server = server(&tenant);
            if installed_before {
                let (status, body) = send(
                    &server.state,
                    push(&tenant, serde_json::json!({ "files": app_files() })),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
            let before = kept(&server, &tenant).await;

            let (checked, check) = send(&server.state, push(&tenant, dry(&files))).await;
            if checked == StatusCode::OK {
                wrong.push(format!("{case}: the check took it: {check}"));
            }
            if kept(&server, &tenant).await != before {
                wrong.push(format!("{case}: the check changed the tenant"));
            }

            let (installed, install) = send(
                &server.state,
                push(&tenant, serde_json::json!({ "files": files })),
            )
            .await;
            let said = without_paths(check["error"].as_str().unwrap_or("(no error)"));
            let agrees = installed == checked
                && install["error"]
                    .as_str()
                    .is_some_and(|error| without_paths(error).starts_with(&said));
            if !agrees {
                wrong.push(format!(
                    "{case}: the install answered {installed} {install}, the check {checked} {check}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A version the check passes is the version the install then installs, by
/// the same digest, and the check before it left nothing behind.
#[tokio::test]
async fn a_dry_run_passes_what_the_install_takes_and_changes_nothing() {
    let tenant = unique_tenant("dry-passed");
    let server = server(&tenant);
    for files in [app_files(), with_comment("a second version")] {
        let before = kept(&server, &tenant).await;
        let (status, check) = send(&server.state, push(&tenant, dry(&files))).await;
        assert_eq!(status, StatusCode::OK, "{check}");
        assert_eq!(check["checked"], APP, "{check}");
        assert!(check.get("installed").is_none(), "{check}");
        assert_eq!(
            kept(&server, &tenant).await,
            before,
            "the check changed the tenant"
        );

        let (status, install) = send(
            &server.state,
            push(&tenant, serde_json::json!({ "files": files })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{install}");
        assert_eq!(install["digest"], check["digest"]);
        assert_eq!(
            install["result"]["added"], check["result"]["added"],
            "{install} vs {check}"
        );
        assert_eq!(install["result"]["updated"], check["result"]["updated"]);
    }
}

/// A check takes no install turn: it answers while another install on this
/// host holds the turn, and says it waited for none.
#[tokio::test]
async fn a_dry_run_answers_while_the_host_install_turn_is_held() {
    let tenant = unique_tenant("dry-turn");
    let server = server(&tenant);
    let path = std::env::temp_dir().join("temper-installs.lock");
    let held = tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .expect("lock file");
        file.lock().expect("take the turn");
        file
    })
    .await
    .expect("lock thread");

    let answered = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        send(&server.state, push(&tenant, dry(&app_files()))),
    )
    .await;
    drop(held);
    let (status, check) = answered.expect("the check waited for the held turn");
    assert_eq!(status, StatusCode::OK, "{check}");
    assert_eq!(check["checked"], APP, "{check}");
    assert_eq!(check["waited_ms"], 0, "{check}");
}

/// An app that needs another the tenant's catalog holds but has not installed:
/// its install installs that one first, so a check cannot foresee the app
/// beside it. The check names the app to install first and changes nothing;
/// the install then takes both.
#[tokio::test]
async fn a_dry_run_names_the_dependency_its_install_installs_first() {
    use super::source_and_cache::{DEP, dependency_files};
    use crate::os_apps::{AppFileSource, add_tenant_written_apps};

    let tenant = unique_tenant("dry-dependency");
    let server = server(&tenant);
    let catalog = tempfile::tempdir().expect("catalog dir");
    let dir = catalog.path().join(DEP);
    let files = dependency_files();
    for (path, text) in &files {
        let dest = dir.join(path);
        std::fs::create_dir_all(dest.parent().expect("inside the app")).expect("app dir");
        std::fs::write(dest, text).expect("app file");
    }
    add_tenant_written_apps(
        &tenant,
        catalog.path().to_path_buf(),
        vec![(DEP.into(), AppFileSource::written(dir, files.into_keys()))],
    );
    let mut root = app_files();
    root.get_mut("app.toml")
        .expect("app.toml")
        .push_str(&format!("\ndependencies = [{DEP:?}]\n"));
    let before = kept(&server, &tenant).await;

    let (status, check) = send(&server.state, push(&tenant, dry(&root))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{check}");
    assert!(
        check["error"]
            .as_str()
            .is_some_and(|error| error.contains(&format!("'{DEP}' is installed"))),
        "{check}"
    );
    assert_eq!(
        kept(&server, &tenant).await,
        before,
        "the check changed the tenant"
    );
    assert!(
        server
            .ps
            .get_installed_app(&tenant, DEP)
            .await
            .expect("install records")
            .is_none(),
        "the check installed '{DEP}'"
    );

    let (status, install) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": root })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{install}");
}
