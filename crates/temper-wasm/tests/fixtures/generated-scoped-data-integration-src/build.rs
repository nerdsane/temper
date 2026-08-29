use std::collections::BTreeSet;

use temper_spec::bundle::{IoaSourceInput, scoped_module_data_closure_digest};
use temper_wasm_sdk::data::{DataOperationKind, EntityDataGrant, ModuleDataGrant};

const CSDL: &str = include_str!("scoped.csdl.xml");
const CUSTOMER_IOA: &str = include_str!("customer.ioa.toml");
const WORKER_IOA: &str = include_str!("worker.ioa.toml");

fn main() {
    println!("cargo:rerun-if-changed=scoped.csdl.xml");
    println!("cargo:rerun-if-changed=customer.ioa.toml");
    println!("cargo:rerun-if-changed=worker.ioa.toml");
    let csdl = temper_spec::parse_csdl(CSDL).expect("fixture CSDL parses");
    let ioa = vec![
        IoaSourceInput {
            entity_type: "Temper.Scoped.Customer".into(),
            source: CUSTOMER_IOA.into(),
        },
        IoaSourceInput {
            entity_type: "Temper.Scoped.Worker".into(),
            source: WORKER_IOA.into(),
        },
    ];
    let closure = scoped_module_data_closure_digest(CSDL, ioa.clone())
        .expect("fixture closure is canonical");
    let generated = temper_codegen::generate_module_sdk(
        &csdl,
        &ioa,
        "scoped_client",
        &closure,
        &closure,
        "",
        ModuleDataGrant {
            operations: BTreeSet::from([
                DataOperationKind::EntityCreate,
                DataOperationKind::EntityGet,
            ]),
            entities: vec![EntityDataGrant {
                entity_type: "Temper.Scoped.Customer".into(),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        },
    )
    .expect("fixture generated client builds");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("generated.rs");
    std::fs::write(output, generated.source).expect("write generated fixture client");
}
