//! Regressions for checks previously duplicated in the control-plane CLI.
use super::*;
use tempfile::TempDir;

const IOA: &str = r##"[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Increment"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["count += 1"]

[[invariant]]
name = "Nonnegative"
assert = "count >= 0"
"##;
const CSDL: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CounterExample" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Counter">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="count" Type="Edm.Int64" Nullable="false" DefaultValue="0"/>
      </EntityType>
      <Action Name="Increment" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.CounterExample.Counter"/>
        <ReturnType Type="Temper.CounterExample.Counter"/>
      </Action>
      <EntityContainer Name="CounterService">
        <EntitySet Name="Counters" EntityType="Temper.CounterExample.Counter"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"##;

fn application() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let specs = dir.path().join("specs");
    fs::create_dir_all(specs.join("policies")).unwrap();
    fs::write(specs.join("model.csdl.xml"), CSDL).unwrap();
    fs::write(specs.join("counter.ioa.toml"), IOA).unwrap();
    fs::write(
        specs.join("policies/counter.cedar"),
        "permit(principal is Agent, action == Action::\"Increment\", resource is Counter);",
    )
    .unwrap();
    dir
}

fn verify_specs(app: &TempDir) -> Result<()> {
    run(app.path().join("specs").to_str().unwrap())
}

/// The policy warnings `temper verify` reports for the app's `specs/`.
fn policy_warnings(specs: &Path) -> Vec<String> {
    let files = AppFiles::find(app_dir_of(specs));
    let sources = input::read_ioa_files(&files.ioa).unwrap();
    package::validate(&sources, &files.modules, &files.policies)
        .unwrap()
        .1
}

#[test]
fn complete_application_passes() {
    let app = application();
    verify_specs(&app).unwrap();
}

#[test]
fn malformed_empty_or_unrelated_csdl_fails() {
    for xml in [
        "<other/>",
        "<Edmx>",
        "<Edmx/>",
        "<Edmx></Edmx><Edmx/>",
        "<!DOCTYPE Edmx><Edmx/>",
    ] {
        let app = application();
        fs::write(app.path().join("specs/model.csdl.xml"), xml).unwrap();
        assert!(verify_specs(&app).is_err(), "{xml}");
    }
}

#[test]
fn ioa_requires_matching_csdl_entity_and_set() {
    for (xml, message) in [
        (
            CSDL.replace("Name=\"Counter\"", "Name=\"Other\""),
            "missing from CSDL",
        ),
        (
            CSDL.replace(
                "<EntitySet Name=\"Counters\" EntityType=\"Temper.CounterExample.Counter\"/>",
                "",
            ),
            "no CSDL entity set",
        ),
    ] {
        let app = application();
        fs::write(app.path().join("specs/model.csdl.xml"), xml).unwrap();
        assert!(
            verify_specs(&app)
                .unwrap_err()
                .to_string()
                .contains(message)
        );
    }
}

#[test]
fn declared_entity_name_is_used_and_duplicates_fail() {
    let app = application();
    let specs = app.path().join("specs");
    fs::rename(
        specs.join("counter.ioa.toml"),
        specs.join("different_filename.ioa.toml"),
    )
    .unwrap();
    fs::rename(
        specs.join("policies/counter.cedar"),
        specs.join("policies/different_filename.cedar"),
    )
    .unwrap();
    verify_specs(&app).unwrap();
    fs::write(specs.join("counter.ioa.toml"), IOA).unwrap();
    assert!(
        verify_specs(&app)
            .unwrap_err()
            .to_string()
            .contains("duplicate entity Counter")
    );
}

#[test]
fn exhausted_real_composite_verification_fails_the_command() {
    use temper_verify::composite::{Bound, Bytes, CompositeOutcome, Progress, verify_composite};
    let automaton = temper_spec::automaton::parse_automaton(IOA).unwrap();
    let (room, progress) = (Bytes::new(0), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let result = verify_composite(&[&automaton], "Counter", bound).unwrap();
    assert_eq!(result.outcome, CompositeOutcome::Incomplete);
    assert!(
        report_composite_results(&[result])
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
}

/// A permit counts for its entity from any file in policies/, whatever the
/// file is named.
#[test]
fn a_permit_in_any_policy_file_counts() {
    let app = application();
    let policies = app.path().join("specs/policies");
    fs::rename(
        policies.join("counter.cedar"),
        policies.join("shared.cedar"),
    )
    .unwrap();
    verify_specs(&app).unwrap();
    let warnings = policy_warnings(&app.path().join("specs"));
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// An entity no permit reaches is a warning: verification still passes.
#[test]
fn an_entity_no_permit_reaches_is_a_warning() {
    let app = application();
    fs::write(
        app.path().join("specs/policies/counter.cedar"),
        "forbid(principal, action, resource);",
    )
    .unwrap();
    verify_specs(&app).unwrap();
    let warnings = policy_warnings(&app.path().join("specs"));
    assert_eq!(warnings, ["no Cedar permit reaches Counter"]);
}

#[test]
fn invalid_cedar_fails() {
    let app = application();
    fs::write(
        app.path().join("specs/policies/counter.cedar"),
        "this is not Cedar",
    )
    .unwrap();
    assert!(
        verify_specs(&app)
            .unwrap_err()
            .to_string()
            .contains("invalid Cedar policy")
    );
}

#[test]
fn no_ioa_cannot_pass_application_verification() {
    let app = application();
    fs::remove_file(app.path().join("specs/counter.ioa.toml")).unwrap();
    assert!(
        verify_specs(&app)
            .unwrap_err()
            .to_string()
            .contains("no IOA specifications")
    );
}

#[test]
fn referenced_wasm_must_exist_and_cannot_escape_modules_directory() {
    for module in ["probe", "../probe"] {
        let app = application();
        let source = IOA.replace("[[invariant]]", &format!("[[action.triggers]]\nname=\"probe\"\nkind=\"wasm\"\nmodule=\"{module}\"\n\n[[invariant]]"));
        fs::write(app.path().join("specs/counter.ioa.toml"), source).unwrap();
        let error = verify_specs(&app).unwrap_err().to_string();
        assert!(
            error.contains(if module == "probe" {
                "missing compiled WASM module"
            } else {
                "invalid module name"
            }),
            "{error}"
        );
    }
}

#[test]
fn only_existing_verify_command_is_exposed() {
    use clap::Parser;
    let cli = crate::Cli::try_parse_from(["temper", "verify", "--specs-dir", "/tmp/example/specs"])
        .unwrap();
    assert!(
        matches!(cli.command, crate::Commands::Verify { specs_dir } if specs_dir == "/tmp/example/specs")
    );
    assert!(crate::Cli::try_parse_from(["temper", "verify-app"]).is_err());
    assert!(crate::Cli::try_parse_from(["temper", "verify", "--application"]).is_err());
}

#[test]
fn entity_sets_must_reference_declared_qualified_types() {
    for xml in [
        CSDL.replace(
            "EntityType=\"Temper.CounterExample.Counter\"",
            "EntityType=\"Other.Counter\"",
        ),
        CSDL.replace(
            "</EntityContainer>",
            "<EntitySet Name=\"Broken\" EntityType=\"Other.Counter\"/></EntityContainer>",
        ),
    ] {
        let app = application();
        fs::write(app.path().join("specs/model.csdl.xml"), xml).unwrap();
        assert!(
            verify_specs(&app).is_err(),
            "undeclared qualified type passed verification"
        );
    }
}

#[test]
fn shared_and_uppercase_cedar_files_are_validated() {
    for name in ["shared.cedar", "shared.CEDAR"] {
        let app = application();
        fs::write(
            app.path().join("specs/policies").join(name),
            "this is not Cedar",
        )
        .unwrap();
        let error = verify_specs(&app).expect_err("every loaded policy must be validated");
        assert!(
            error.to_string().contains("invalid Cedar policy"),
            "{error:#}"
        );
    }
}

#[test]
fn declared_schema_aliases_resolve_entity_sets() {
    let app = application();
    let xml = CSDL
        .replace(
            "Namespace=\"Temper.CounterExample\"",
            "Namespace=\"Temper.CounterExample\" Alias=\"CE\"",
        )
        .replace(
            "EntityType=\"Temper.CounterExample.Counter\"",
            "EntityType=\"CE.Counter\"",
        );
    fs::write(app.path().join("specs/model.csdl.xml"), &xml).unwrap();
    verify_specs(&app).expect("declared schema alias must resolve");
    fs::write(
        app.path().join("specs/model.csdl.xml"),
        xml.replace("CE.Counter", "Other.Counter"),
    )
    .unwrap();
    assert!(
        verify_specs(&app).is_err(),
        "undeclared alias must still fail"
    );
}

#[test]
fn schema_alias_can_be_used_from_another_schema() {
    let app = application();
    let xml = CSDL.replace(
        "Namespace=\"Temper.CounterExample\"",
        "Namespace=\"Temper.CounterExample\" Alias=\"CE\"",
    ).replace("EntityType=\"Temper.CounterExample.Counter\"", "EntityType=\"CE.Counter\"")
        .replace("<EntityContainer Name=\"CounterService\">", "</Schema><Schema Namespace=\"Collections\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\"><EntityContainer Name=\"CounterService\">");
    fs::write(app.path().join("specs/model.csdl.xml"), xml).unwrap();
    verify_specs(&app).expect("alias resolution must cover all schemas in the document");
}

#[test]
fn ambiguous_schema_aliases_are_rejected() {
    for conflicting in ["Namespace=\"Other\" Alias=\"CE\"", "Namespace=\"CE\""] {
        let app = application();
        let xml = CSDL.replace(
            "Namespace=\"Temper.CounterExample\"",
            "Namespace=\"Temper.CounterExample\" Alias=\"CE\"",
        ).replace("</edmx:DataServices>", &format!("<Schema {conflicting} xmlns=\"http://docs.oasis-open.org/odata/ns/edm\"></Schema></edmx:DataServices>"));
        fs::write(app.path().join("specs/model.csdl.xml"), xml).unwrap();
        let error =
            verify_specs(&app).expect_err("ambiguous qualifier must not pick a namespace silently");
        assert!(
            error
                .to_string()
                .contains("ambiguous CSDL namespace or alias"),
            "{error:#}"
        );
    }
}

/// Verify reads an app's files as its install does: given the app's specs/,
/// it reads specs from the app root as well, and policies from the app's
/// policies/, so an app that passes here is the app that installs.
#[test]
fn verify_reads_the_files_an_install_reads() {
    let app = application();
    let root = app.path();
    fs::write(
        root.join("app.toml"),
        "name = \"counter-app\"\ndescription = \"Verify layout test\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    // The spec at the app root, the policy in the app's policies/.
    fs::rename(
        root.join("specs/counter.ioa.toml"),
        root.join("counter.ioa.toml"),
    )
    .unwrap();
    fs::create_dir_all(root.join("policies")).unwrap();
    fs::rename(
        root.join("specs/policies/counter.cedar"),
        root.join("policies/counter.cedar"),
    )
    .unwrap();

    let specs = root.join("specs");
    assert_eq!(
        input::read_ioa_files(&AppFiles::find(app_dir_of(&specs)).ioa)
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["Counter"],
        "the root spec an install reads is verified"
    );
    assert!(
        policy_warnings(&specs).is_empty(),
        "the app's policies/ is read"
    );
    verify_specs(&app).unwrap();
}

/// A spec file another of the same name shadows is reported, naming both.
#[test]
fn a_shadowed_spec_file_is_named_with_the_file_read_instead() {
    let app = application();
    let root = app.path();
    fs::write(
        root.join("app.toml"),
        "name = \"counter-app\"\ndescription = \"Verify layout test\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::copy(
        root.join("specs/counter.ioa.toml"),
        root.join("counter.ioa.toml"),
    )
    .unwrap();
    fs::copy(
        root.join("specs/model.csdl.xml"),
        root.join("model.csdl.xml"),
    )
    .unwrap();

    assert_eq!(
        AppFiles::find(root).unread_warnings(root),
        [
            "specs/counter.ioa.toml is not read: counter.ioa.toml is read in its place",
            "specs/model.csdl.xml is not read: model.csdl.xml is read in its place",
        ]
    );
}

/// A module the app declares in app.toml, with its binary under wasm/, is
/// found by verify where its install finds it.
#[test]
fn verify_finds_a_module_where_its_install_does() {
    let app = application();
    let root = app.path();
    fs::write(
        root.join("app.toml"),
        "name = \"counter-app\"\ndescription = \"Verify layout test\"\nversion = \"0.1.0\"\n\n[[wasm_modules]]\nname = \"probe\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("wasm/probe")).unwrap();
    fs::write(root.join("wasm/probe/probe.wasm"), b"\0asm").unwrap();
    let source = IOA.replace(
        "[[invariant]]",
        "[[action.triggers]]\nname=\"probe\"\nkind=\"wasm\"\nmodule=\"probe\"\n\n[[invariant]]",
    );
    fs::write(root.join("specs/counter.ioa.toml"), source).unwrap();
    verify_specs(&app).unwrap();
}

/// A spec may call a module of an app its own app depends on: the install
/// installs that app first, so verify finds the module there, as the
/// install's closure does, and still refuses one no app in it declares.
#[test]
fn verify_finds_a_module_of_an_app_it_depends_on() {
    let apps = tempfile::tempdir().unwrap();
    let suffix: String = apps
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase();
    let dependency = format!("gate-{suffix}");
    let gate = apps.path().join(&dependency);
    fs::create_dir_all(gate.join("wasm/probe_gate")).unwrap();
    fs::write(
        gate.join("app.toml"),
        format!(
            "name = \"{dependency}\"\ndescription = \"Holds the module\"\nversion = \"0.1.0\"\n\n[[wasm_modules]]\nname = \"probe_gate\"\n"
        ),
    )
    .unwrap();
    fs::write(gate.join("APP.md"), "# Gate\n\nHolds the module.\n").unwrap();
    fs::write(gate.join("wasm/probe_gate/probe_gate.wasm"), b"\0asm").unwrap();

    let app = apps.path().join(format!("counter-{suffix}"));
    let specs = app.join("specs");
    fs::create_dir_all(specs.join("policies")).unwrap();
    fs::write(specs.join("model.csdl.xml"), CSDL).unwrap();
    fs::write(
        specs.join("policies/counter.cedar"),
        "permit(principal is Agent, action == Action::\"Increment\", resource is Counter);",
    )
    .unwrap();
    let calls = |module: &str| {
        IOA.replace(
            "[[invariant]]",
            &format!(
                "[[action.triggers]]\nname=\"gate\"\nkind=\"wasm\"\nmodule=\"{module}\"\n\n[[invariant]]"
            ),
        )
    };
    fs::write(
        app.join("app.toml"),
        format!(
            "name = \"counter-{suffix}\"\ndescription = \"Calls a module of its dependency\"\nversion = \"0.1.0\"\ndependencies = [\"{dependency}\"]\n"
        ),
    )
    .unwrap();
    fs::write(specs.join("counter.ioa.toml"), calls("probe_gate")).unwrap();
    run(specs.to_str().unwrap()).unwrap();

    fs::write(specs.join("counter.ioa.toml"), calls("absent_gate")).unwrap();
    let error = run(specs.to_str().unwrap()).unwrap_err().to_string();
    assert!(
        error.contains("missing compiled WASM module absent_gate"),
        "a module no app in the closure declares is still refused: {error}"
    );
}
